//! Wetware host runtime: libp2p host + Wasmtime host.
#![cfg(not(target_arch = "wasm32"))]

use std::collections::{HashMap, HashSet, VecDeque};
use std::net::IpAddr;
use std::num::{NonZeroU8, NonZeroUsize};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use futures::stream::FuturesUnordered;
use futures::StreamExt;
use libp2p::kad;
use libp2p::kad::store::RecordStore;
use libp2p::swarm::dial_opts::DialOpts;
use libp2p::swarm::SwarmEvent;
use libp2p::{Multiaddr, PeerId, SwarmBuilder};
use tokio::sync::{mpsc, oneshot, watch};

use rpc::{NatReachability, NetworkState, PeerInfo};

// ---------------------------------------------------------------------------
// NAT traversal constants
// ---------------------------------------------------------------------------

/// Maximum number of concurrent relay reservations to maintain.
const MAX_RELAY_RESERVATIONS: usize = 2;

/// Each DHT query contacts this many peers. The same bound caps selected
/// provider results so an untrusted `count` cannot scale host memory or
/// address-resolution fan-out without limit.
const KAD_REPLICATION_FACTOR: usize = 16;
const MAX_FIND_PROVIDER_RESULTS: u32 = KAD_REPLICATION_FACTOR as u32;

fn find_provider_limit(requested: u32) -> u32 {
    requested.min(MAX_FIND_PROVIDER_RESULTS)
}

/// The relay v2 hop protocol advertised by peers that can serve as relays.
const RELAY_HOP_PROTOCOL: &str = "/libp2p/circuit/relay/0.2.0/hop";

/// AutoNAT v2 hysteresis thresholds for node-level reachability decisions.
///
/// We intentionally require multiple consecutive outcomes to avoid flapping on
/// transient network blips.
const NAT_SUCCESS_THRESHOLD: u8 = 2;
const NAT_FAILURE_THRESHOLD: u8 = 2;

// ---------------------------------------------------------------------------
// Dual DHT types
// ---------------------------------------------------------------------------

/// Identifies which Kademlia DHT instance produced or should receive a query.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum DhtSource {
    Wan,
    Lan,
}

/// Compound key for pending query maps.  Prevents collision between QueryId
/// values from the two independent `kad::Behaviour` instances.
type DhtQueryKey = (DhtSource, kad::QueryId);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct NatTransition {
    from: NatReachability,
    to: NatReachability,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct NatPolicyState {
    status: NatReachability,
    consecutive_successes: u8,
    consecutive_failures: u8,
}

impl NatPolicyState {
    fn new() -> Self {
        Self {
            status: NatReachability::Unknown,
            consecutive_successes: 0,
            consecutive_failures: 0,
        }
    }

    fn status(&self) -> NatReachability {
        self.status
    }

    fn record_probe_result(&mut self, success: bool) -> Option<NatTransition> {
        let from = self.status;
        if success {
            self.consecutive_successes = self.consecutive_successes.saturating_add(1);
            self.consecutive_failures = 0;
            if self.status != NatReachability::Public
                && self.consecutive_successes >= NAT_SUCCESS_THRESHOLD
            {
                self.status = NatReachability::Public;
            }
        } else {
            self.consecutive_failures = self.consecutive_failures.saturating_add(1);
            self.consecutive_successes = 0;
            if self.status != NatReachability::Private
                && self.consecutive_failures >= NAT_FAILURE_THRESHOLD
            {
                self.status = NatReachability::Private;
            }
        }
        if from != self.status {
            Some(NatTransition {
                from,
                to: self.status,
            })
        } else {
            None
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
struct NatTransitionActions {
    set_status: NatReachability,
    kad_mode: kad::Mode,
    try_reserve_relay: bool,
}

fn actions_for_nat_transition(transition: NatTransition) -> NatTransitionActions {
    match transition.to {
        NatReachability::Public => NatTransitionActions {
            set_status: NatReachability::Public,
            kad_mode: kad::Mode::Server,
            try_reserve_relay: false,
        },
        NatReachability::Private => NatTransitionActions {
            set_status: NatReachability::Private,
            kad_mode: kad::Mode::Client,
            try_reserve_relay: true,
        },
        NatReachability::Unknown => NatTransitionActions {
            set_status: NatReachability::Unknown,
            kad_mode: if transition.from == NatReachability::Public {
                kad::Mode::Client
            } else {
                kad::Mode::Server
            },
            try_reserve_relay: false,
        },
    }
}

/// Shared state for a logical `find_providers` request dispatched to both DHTs.
///
/// Both WAN and LAN queries feed providers into the same `sender`. `seen`
/// deduplicates discovery results. `queued_peers` also deduplicates the two
/// address-resolution queries for an addressless provider. `pending` retains
/// selected results until the single-slot sender accepts them. Its length plus
/// `delivered` cannot exceed the caller's `limit`. `remaining` tracks how many
/// provider queries are still active.
struct FindRequest {
    sender: mpsc::Sender<PeerInfo>,
    cancellation: watch::Receiver<bool>,
    seen: HashSet<PeerId>,
    queued_peers: HashSet<PeerId>,
    pending: VecDeque<PendingProvider>,
    remaining: u8,
    limit: u32,
    delivered: u32,
}

#[derive(Clone)]
struct PendingProvider {
    peer_id: PeerId,
    info: PeerInfo,
}

type PendingFindDelivery = (u64, mpsc::Sender<PeerInfo>, PendingProvider);
type PendingFindCancellation = (u64, watch::Receiver<bool>);

fn pending_find_deliveries(requests: &HashMap<u64, FindRequest>) -> Vec<PendingFindDelivery> {
    requests
        .iter()
        .filter_map(|(&request_id, request)| {
            request
                .pending
                .front()
                .cloned()
                .map(|provider| (request_id, request.sender.clone(), provider))
        })
        .collect()
}

async fn send_next_find_provider(
    candidates: Vec<PendingFindDelivery>,
) -> Option<(u64, PeerId, bool)> {
    if candidates.is_empty() {
        return std::future::pending().await;
    }

    let mut deliveries = FuturesUnordered::new();
    for (request_id, sender, provider) in candidates {
        deliveries.push(async move {
            let peer_id = provider.peer_id;
            let sent = sender.send(provider.info).await.is_ok();
            (request_id, peer_id, sent)
        });
    }
    deliveries.next().await
}

fn pending_find_cancellations(
    requests: &HashMap<u64, FindRequest>,
) -> Vec<PendingFindCancellation> {
    requests
        .iter()
        .map(|(&request_id, request)| (request_id, request.cancellation.clone()))
        .collect()
}

async fn wait_for_next_find_cancellation(candidates: Vec<PendingFindCancellation>) -> Option<u64> {
    if candidates.is_empty() {
        return std::future::pending().await;
    }

    let mut cancellations = FuturesUnordered::new();
    for (request_id, mut cancellation) in candidates {
        cancellations.push(async move {
            let already_canceled = *cancellation.borrow();
            if !already_canceled {
                let _ = cancellation.changed().await;
            }
            request_id
        });
    }
    cancellations.next().await
}

impl FindRequest {
    fn can_select_provider(&self) -> bool {
        self.seen.len() < usize::try_from(self.limit).unwrap_or(usize::MAX)
    }

    fn select_provider(&mut self, peer_id: PeerId) -> bool {
        self.can_select_provider() && self.seen.insert(peer_id)
    }

    fn selection_complete(&self) -> bool {
        !self.can_select_provider()
    }

    fn queue_resolved_provider(&mut self, peer_id: PeerId, addrs: &[Multiaddr]) {
        if self.queued_peers.insert(peer_id) {
            self.pending.push_back(PendingProvider {
                peer_id,
                info: PeerInfo {
                    peer_id: peer_id.to_bytes(),
                    addrs: addrs.iter().map(|addr| addr.to_vec()).collect(),
                },
            });
        }
    }
}

/// Shared state for a logical `provide` request dispatched to both DHTs.
///
/// The first WAN or LAN success activates the registration and replies to all
/// owners. If both DHTs fail, the request reports the WAN error when available.
struct ProvideRequest {
    key: Vec<u8>,
    owners: HashSet<rpc::ProviderOwnerId>,
    replies: Vec<(rpc::ProviderOwnerId, oneshot::Sender<Result<(), String>>)>,
    wan_done: bool,
    lan_done: bool,
    wan_err: Option<String>,
    succeeded: bool,
}

impl ProvideRequest {
    fn new(
        owner: rpc::ProviderOwnerId,
        key: Vec<u8>,
        reply: oneshot::Sender<Result<(), String>>,
    ) -> Self {
        Self {
            key,
            owners: HashSet::from([owner]),
            replies: vec![(owner, reply)],
            wan_done: false,
            lan_done: false,
            wan_err: None,
            succeeded: false,
        }
    }

    fn add_owner(
        &mut self,
        owner: rpc::ProviderOwnerId,
        reply: oneshot::Sender<Result<(), String>>,
    ) {
        self.owners.insert(owner);
        if self.succeeded {
            let _ = reply.send(Ok(()));
        } else {
            self.replies.push((owner, reply));
        }
    }

    fn release_owner(&mut self, owner: rpc::ProviderOwnerId, reason: &str) {
        self.owners.remove(&owner);
        let mut retained = Vec::with_capacity(self.replies.len());
        for (reply_owner, reply) in self.replies.drain(..) {
            if reply_owner == owner {
                let _ = reply.send(Err(reason.to_string()));
            } else {
                retained.push((reply_owner, reply));
            }
        }
        self.replies = retained;
    }

    /// Record a DHT result.  Returns true if the request is fully resolved.
    fn record(&mut self, source: DhtSource, result: Result<(), String>) -> bool {
        match source {
            DhtSource::Wan => self.wan_done = true,
            DhtSource::Lan => self.lan_done = true,
        }
        match result {
            Ok(()) => {
                self.succeeded = true;
                // First success wins. All duplicate callers observe the same
                // registration result.
                for (_, reply) in self.replies.drain(..) {
                    let _ = reply.send(Ok(()));
                }
            }
            Err(e) => {
                if source == DhtSource::Wan {
                    self.wan_err = Some(e);
                }
            }
        }
        self.wan_done && self.lan_done
    }

    /// Finalize: if nobody got a success, send the WAN error.
    fn finalize(mut self) -> ProvideOutcome {
        if !self.succeeded {
            let err = self
                .wan_err
                .unwrap_or_else(|| "both DHTs failed".to_string());
            for (_, reply) in self.replies.drain(..) {
                let _ = reply.send(Err(err.clone()));
            }
            ProvideOutcome::Failed {
                owners: self.owners.into_iter().collect(),
                key: self.key,
            }
        } else {
            ProvideOutcome::Active
        }
    }
}

enum ProvideOutcome {
    Active,
    Failed {
        owners: Vec<rpc::ProviderOwnerId>,
        key: Vec<u8>,
    },
}

/// Reference ownership for local provider registration and republication.
#[derive(Default)]
struct ProviderOwnership {
    by_key: HashMap<Vec<u8>, HashSet<rpc::ProviderOwnerId>>,
    by_owner: HashMap<rpc::ProviderOwnerId, HashSet<Vec<u8>>>,
}

impl ProviderOwnership {
    /// Claim one key. Returns true only for the first local owner.
    fn claim(&mut self, owner: rpc::ProviderOwnerId, key: Vec<u8>) -> bool {
        let owners = self.by_key.entry(key.clone()).or_default();
        if !owners.insert(owner) {
            return false;
        }
        self.by_owner.entry(owner).or_default().insert(key);
        owners.len() == 1
    }

    fn contains(&self, owner: rpc::ProviderOwnerId, key: &[u8]) -> bool {
        self.by_key
            .get(key)
            .is_some_and(|owners| owners.contains(&owner))
    }

    /// Release one claim. Returns true when the key lost its final owner.
    fn release_key(&mut self, owner: rpc::ProviderOwnerId, key: &[u8]) -> bool {
        let mut final_owner = false;
        if let Some(owners) = self.by_key.get_mut(key) {
            owners.remove(&owner);
            final_owner = owners.is_empty();
        }
        if final_owner {
            self.by_key.remove(key);
        }
        if let Some(keys) = self.by_owner.get_mut(&owner) {
            keys.remove(key);
            if keys.is_empty() {
                self.by_owner.remove(&owner);
            }
        }
        final_owner
    }

    /// Release all claims for an owner and return keys that lost their final owner.
    fn release_owner(&mut self, owner: rpc::ProviderOwnerId) -> Vec<Vec<u8>> {
        let keys = self.by_owner.remove(&owner).unwrap_or_default();
        let mut final_keys = Vec::new();
        for key in keys {
            if let Some(owners) = self.by_key.get_mut(&key) {
                owners.remove(&owner);
                if owners.is_empty() {
                    self.by_key.remove(&key);
                    final_keys.push(key);
                }
            }
        }
        final_keys
    }
}

// ---------------------------------------------------------------------------
// Address classification
// ---------------------------------------------------------------------------

/// Returns true if the multiaddr's first IP component is a private, loopback,
/// or link-local address.  Defaults to false (WAN) when no IP is present.
fn is_lan_addr(addr: &Multiaddr) -> bool {
    use libp2p::multiaddr::Protocol;
    for proto in addr.iter() {
        match proto {
            Protocol::Ip4(ip) => return is_lan_ip(IpAddr::V4(ip)),
            Protocol::Ip6(ip) => return is_lan_ip(IpAddr::V6(ip)),
            _ => continue,
        }
    }
    false
}

/// Classify an IP address as LAN (private, loopback, or link-local).
fn is_lan_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => {
            v4.is_private() || v4.is_loopback() || (v4.octets()[0] == 169 && v4.octets()[1] == 254)
            // link-local
        }
        IpAddr::V6(v6) => {
            v6.is_loopback()
                || (v6.segments()[0] & 0xffc0) == 0xfe80 // fe80::/10 link-local
                || (v6.segments()[0] & 0xfe00) == 0xfc00 // fc00::/7 ULA (private IPv6)
        }
    }
}

/// Returns true if the multiaddr contains an unspecified IP (0.0.0.0 or ::).
/// These should not be promoted as external addresses.
fn is_unspecified_addr(addr: &Multiaddr) -> bool {
    use libp2p::multiaddr::Protocol;
    for proto in addr.iter() {
        match proto {
            Protocol::Ip4(ip) => return ip.is_unspecified(),
            Protocol::Ip6(ip) => return ip.is_unspecified(),
            _ => continue,
        }
    }
    false
}

/// Returns true if the peer's protocol list includes the relay v2 hop protocol.
fn is_relay_capable(protocols: &[libp2p::StreamProtocol]) -> bool {
    protocols.iter().any(|p| p.as_ref() == RELAY_HOP_PROTOCOL)
}

/// Bootstrap info for the in-process Kad client.
///
/// Obtained by calling [`crate::ipfs::HttpClient::kubo_info`] and parsing the
/// returned peer ID + swarm address. Passed to [`Net::new`] so the Kad client
/// can bootstrap against the local Kubo node.
pub struct KuboBootstrapInfo {
    pub peer_id: PeerId,
    pub addr: Multiaddr,
}

pub use rpc::SwarmCommand;

/// Network behavior for Wetware hosts.
#[derive(libp2p::swarm::NetworkBehaviour)]
pub struct Behaviour {
    pub identify: libp2p::identify::Behaviour,
    pub stream: libp2p_stream::Behaviour,
    /// WAN Kademlia DHT client (Amino protocol `/ipfs/kad/1.0.0`).
    /// Runs in client mode initially.  Promoted to server when AutoNAT confirms
    /// public reachability.
    pub kad: kad::Behaviour<kad::store::MemoryStore>,
    /// LAN Kademlia DHT server (`/ipfs/lan/kad/1.0.0`).
    /// Runs in server mode.  Bootstrapped against Kubo's private/loopback peers.
    pub kad_lan: kad::Behaviour<kad::store::MemoryStore>,
    /// AutoNAT v2 client -- supplementary NAT probes for newer peers.
    pub autonat_v2: libp2p::autonat::v2::client::Behaviour,
    /// Relay client -- enables relayed connections and circuit addresses.
    pub relay_client: libp2p::relay::client::Behaviour,
    /// DCUtR -- upgrades relayed connections to direct via hole-punching.
    pub dcutr: libp2p::dcutr::Behaviour,
    /// Caps concurrent dials/connections to relieve QUIC TLS handshake load
    /// on the single-threaded swarm task (Ed25519 verification is the hotspot).
    pub connection_limits: libp2p::connection_limits::Behaviour,
}

/// Host-side P2P networking subsystem.
pub struct Net {
    swarm: libp2p::swarm::Swarm<Behaviour>,
    local_peer_id: PeerId,
    stream_control: libp2p_stream::Control,
}

impl Net {
    /// Create a new libp2p host and start listening on the given multiaddrs.
    ///
    /// `listen` is the set of multiaddrs to bind. Every entry must succeed —
    /// any bind failure (port in use, IPv6 disabled, etc.) is a hard error.
    /// Callers wanting a subset (e.g. IPv4 only) should pass only those addrs.
    ///
    /// `keypair` is the node's identity — load it with [`keys::to_libp2p`]
    /// or supply an ephemeral key for dev/test use.
    ///
    /// `kubo_bootstrap` is optional Kubo node info for bootstrapping the Kad
    /// client.  When `None`, the Kad client starts without any seed peers.
    pub fn new(
        listen: Vec<Multiaddr>,
        keypair: libp2p::identity::Keypair,
        kubo_bootstrap: Option<KuboBootstrapInfo>,
        kubo_peers: Vec<(PeerId, Multiaddr)>,
    ) -> Result<Self> {
        let peer_id = keypair.public().to_peer_id();

        let stream_behaviour = libp2p_stream::Behaviour::new();
        let stream_control = stream_behaviour.new_control();

        // PeerID-derived jitter for the WAN periodic bootstrap interval.
        //
        // Why jitter: synchronized starts in fleet deployments (e.g. rolling
        // restart, mass deploy) cause bootstrap storms on shared upstream peers
        // (Kubo, public Amino seeds). Spreading the period across [300, 600]s
        // smears the load.
        //
        // Why PeerID-seeded (not RNG): the value is deterministic per host —
        // easy to debug ("why does node X bootstrap every 437s?") — yet
        // uncorrelated across hosts, because peer IDs are themselves random.
        // Same desync benefit as wall-clock jitter, with reproducible behaviour.
        //
        // Why startup-only (not re-jittered per cycle): different peer_ids
        // already produce divergent intervals, so fleet-scale correlation is
        // unlikely to persist past the first bootstrap. Re-jittering would
        // require driving bootstrap manually instead of using libp2p's
        // built-in periodic timer, which is more code for marginal gain.
        let bootstrap_secs = {
            let bytes = peer_id.to_bytes();
            let mut buf = [0u8; 8];
            buf.copy_from_slice(&bytes[bytes.len() - 8..]);
            300 + (u64::from_le_bytes(buf) % 301) // uniform in [300, 600]
        };
        tracing::info!(
            bootstrap_secs,
            "kad wan bootstrap interval (peer-id-jittered)"
        );

        // ---- WAN Kademlia (Amino DHT, client mode) ----
        let kad_store = kad::store::MemoryStore::new(peer_id);
        let mut kad_config = kad::Config::new(kad::PROTOCOL_NAME);
        kad_config.set_periodic_bootstrap_interval(Some(Duration::from_secs(bootstrap_secs)));
        kad_config.set_replication_factor(NonZeroUsize::new(KAD_REPLICATION_FACTOR).unwrap());
        // NOTE: we'd like to call `kad_config.set_automatic_bootstrap_throttle`
        // here to rate-limit identify-triggered bootstrap fan-out (kad
        // auto-bootstraps whenever a new peer is inserted into the routing
        // table; without a throttle, identify storms cascade into bootstrap
        // storms that hammer the swarm task). But in libp2p-kad 0.47 the
        // setter is `pub(crate) #[cfg(test)]` (see behaviour.rs:443) and not
        // reachable from downstream crates. The internal default is 500ms;
        // we rely on that for now and let the random-walk timer in the
        // event loop do the heavy lifting for ambient refresh.
        let mut kad_wan = kad::Behaviour::with_config(peer_id, kad_store, kad_config);
        kad_wan.set_mode(Some(kad::Mode::Client));

        // ---- LAN Kademlia (server mode) ----
        let kad_lan_store = kad::store::MemoryStore::new(peer_id);
        let lan_proto = libp2p::StreamProtocol::new("/ipfs/lan/kad/1.0.0");
        let mut kad_lan_config = kad::Config::new(lan_proto);
        kad_lan_config.set_periodic_bootstrap_interval(None);
        kad_lan_config.set_replication_factor(NonZeroUsize::new(KAD_REPLICATION_FACTOR).unwrap());
        // Same throttle limitation applies here — see WAN comment above.
        let mut kad_lan = kad::Behaviour::with_config(peer_id, kad_lan_store, kad_lan_config);
        kad_lan.set_mode(Some(kad::Mode::Server));

        // Classify Kubo's connected peers by address and add to the
        // appropriate DHT routing table.
        let mut has_wan_peers = false;
        let mut has_lan_peers = false;
        for (pid, addr) in &kubo_peers {
            if is_lan_addr(addr) {
                kad_lan.add_address(pid, addr.clone());
                has_lan_peers = true;
            } else {
                kad_wan.add_address(pid, addr.clone());
                has_wan_peers = true;
            }
        }
        // Kubo itself gets added to both tables (it typically has both
        // private and public addresses).
        if let Some(ref bootstrap) = kubo_bootstrap {
            kad_wan.add_address(&bootstrap.peer_id, bootstrap.addr.clone());
            kad_lan.add_address(&bootstrap.peer_id, bootstrap.addr.clone());
            has_wan_peers = true;
            has_lan_peers = true;
        }

        // One-time bootstrap walks for each DHT that has seed peers.
        if has_wan_peers {
            match kad_wan.bootstrap() {
                Ok(_) => tracing::debug!("WAN Kad bootstrap walk started"),
                Err(e) => tracing::warn!("WAN Kad bootstrap failed to start: {e:?}"),
            }
        }
        if has_lan_peers {
            match kad_lan.bootstrap() {
                Ok(_) => tracing::debug!("LAN Kad bootstrap walk started"),
                Err(e) => tracing::warn!("LAN Kad bootstrap failed to start: {e:?}"),
            }
        }

        let identify_config =
            libp2p::identify::Config::new("wetware/0.1.0".to_string(), keypair.public());
        let local_peer_id = peer_id;

        let mut swarm = SwarmBuilder::with_existing_identity(keypair)
            .with_tokio()
            .with_tcp(
                Default::default(),
                libp2p::noise::Config::new,
                libp2p::yamux::Config::default,
            )?
            .with_quic()
            .with_relay_client(libp2p::noise::Config::new, libp2p::yamux::Config::default)?
            .with_behaviour(|_keypair, relay_client| {
                let conn_limits = libp2p::connection_limits::ConnectionLimits::default()
                    .with_max_pending_incoming(Some(16))
                    .with_max_pending_outgoing(Some(16))
                    .with_max_established_incoming(Some(64))
                    .with_max_established_outgoing(Some(64));
                Ok(Behaviour {
                    identify: libp2p::identify::Behaviour::new(identify_config),
                    stream: stream_behaviour,
                    kad: kad_wan,
                    kad_lan,
                    autonat_v2: libp2p::autonat::v2::client::Behaviour::default(),
                    relay_client,
                    dcutr: libp2p::dcutr::Behaviour::new(local_peer_id),
                    connection_limits: libp2p::connection_limits::Behaviour::new(conn_limits),
                })
            })?
            .with_swarm_config(|c: libp2p::swarm::Config| {
                c.with_idle_connection_timeout(Duration::from_secs(60))
                    .with_dial_concurrency_factor(NonZeroU8::new(1).unwrap())
            })
            .build();

        // Every requested listen addr must bind. Surfacing failure here is
        // intentional: if the user asked for IPv6 or QUIC and the OS can't
        // provide it, they should fix the config (host OS or --listen) rather
        // than discover the silent degradation later.
        for addr in &listen {
            swarm
                .listen_on(addr.clone())
                .with_context(|| format!("listen on {addr}"))?;
        }

        Ok(Self {
            swarm,
            local_peer_id: peer_id,
            stream_control,
        })
    }

    pub fn local_peer_id(&self) -> PeerId {
        self.local_peer_id
    }

    pub fn stream_control(&self) -> libp2p_stream::Control {
        self.stream_control.clone()
    }

    pub async fn run(
        mut self,
        network_state: NetworkState,
        mut cmd_rx: mpsc::Receiver<SwarmCommand>,
    ) -> Result<()> {
        let mut connected_peers: HashSet<PeerId> = HashSet::new();
        let mut pending_connects: HashMap<PeerId, Vec<oneshot::Sender<Result<(), String>>>> =
            HashMap::new();

        // --- Dual DHT pending query maps (compound-keyed) ---

        // Logical request ID → ProvideRequest.  Both DHT queries map here.
        let mut next_request_id: u64 = 0;
        let mut pending_provides: HashMap<u64, ProvideRequest> = HashMap::new();
        // Compound (source, query_id) → logical request_id for provide.
        let mut provide_query_to_req: HashMap<DhtQueryKey, u64> = HashMap::new();
        // One in-flight provide per owner/key pair. Duplicate calls join it.
        let mut pending_provider_claims: HashMap<(rpc::ProviderOwnerId, Vec<u8>), u64> =
            HashMap::new();
        let mut provider_ownership = ProviderOwnership::default();

        // Logical request ID → FindRequest.  Both DHT queries map here.
        let mut pending_finds: HashMap<u64, FindRequest> = HashMap::new();
        // Compound (source, query_id) → logical request_id for find_providers.
        let mut find_query_to_req: HashMap<DhtQueryKey, u64> = HashMap::new();

        // Peer address book populated from swarm events and peer routing results.
        let mut peer_addr_book: HashMap<PeerId, Vec<Multiaddr>> = HashMap::new();
        // Pending peer routing (RoutedHost-style): compound key → (target PeerId, owning request ID).
        let mut pending_peer_routing: HashMap<DhtQueryKey, (PeerId, Option<u64>)> = HashMap::new();
        // Peers already routed, scoped per logical find request.
        let mut routed_peers: HashMap<u64, HashSet<PeerId>> = HashMap::new();

        // --- NAT traversal state ---
        let mut nat_policy = NatPolicyState::new();
        let mut active_relay_reservations: usize = 0;
        let mut inflight_relay_requests: usize = 0;
        // Relay reservation attempts not yet confirmed by ReservationReqAccepted.
        // Keyed by relay peer id so we can unwind inflight counters on terminal
        // failures (e.g. outgoing dial error / connection close before accept).
        let mut pending_relay_attempts: HashMap<PeerId, usize> = HashMap::new();
        // Relay-capable peers discovered via Identify but not yet reserved.
        let mut relay_candidates: Vec<(PeerId, Multiaddr)> = Vec::new();
        // Peers already seen as relay candidates (dedup).
        let mut seen_relay_peers: HashSet<PeerId> = HashSet::new();

        // Local UDS admin discovery has been removed. Runtime discovery now
        // relies on libp2p mechanisms and direct multiaddr dialing paths.

        // Self-announcement on both DHTs.
        let beh = self.swarm.behaviour_mut();
        beh.kad.get_closest_peers(self.local_peer_id);
        beh.kad_lan.get_closest_peers(self.local_peer_id);
        tracing::debug!("Kad self-announcement walks started (WAN + LAN)");

        // Advertise the node's discovery record on the LAN DHT.
        let discovery_key = crate::discovery::discovery_record_key();
        match self
            .swarm
            .behaviour_mut()
            .kad_lan
            .start_providing(discovery_key)
        {
            Ok(_) => tracing::debug!("LAN discovery provide started"),
            Err(e) => tracing::warn!("LAN discovery provide failed: {e:?}"),
        }

        // Ambient kad refresh — Forest-inspired (ChainSafe/forest runs a
        // similar loop in their discovery service). Periodic random
        // `get_closest_peers` queries keep buckets warm by exploring fresh
        // points in keyspace, complementing the forced periodic bootstrap
        // (a brief synchronized burst against seed peers) with a continuous,
        // cheap, naturally desynchronized refresh source. Exponential
        // backoff 1s -> 60s gives fast warm-up after startup, then settles.
        // Inline in the swarm loop's select! rather than a spawned task:
        // DHT maintenance is internal to the swarm, not an external command,
        // so it doesn't belong on the `SwarmCommand` channel.
        let mut walk_interval = Duration::from_secs(1);
        let walk_timer = tokio::time::sleep(walk_interval);
        tokio::pin!(walk_timer);

        loop {
            let find_delivery = send_next_find_provider(pending_find_deliveries(&pending_finds));
            let find_cancellation =
                wait_for_next_find_cancellation(pending_find_cancellations(&pending_finds));

            tokio::select! {
                request_id = find_cancellation => {
                    cancel_find_request(
                        request_id.expect("find cancellation future requires a request"),
                        &mut self.swarm,
                        &mut find_query_to_req,
                        &mut pending_finds,
                        &mut pending_peer_routing,
                        &mut routed_peers,
                    );
                }
                delivery = find_delivery => {
                    let (request_id, peer_id, sent) =
                        delivery.expect("find delivery future only completes with a provider");
                    let mut cancel = !sent;
                    if sent {
                        if let Some(request) = pending_finds.get_mut(&request_id) {
                            let delivered = request
                                .pending
                                .pop_front()
                                .expect("selected find delivery disappeared");
                            debug_assert_eq!(delivered.peer_id, peer_id);
                            request.delivered = request.delivered.saturating_add(1);
                            cancel = request.delivered >= request.limit;
                        }
                    }

                    if cancel {
                        cancel_find_request(
                            request_id,
                            &mut self.swarm,
                            &mut find_query_to_req,
                            &mut pending_finds,
                            &mut pending_peer_routing,
                            &mut routed_peers,
                        );
                    } else if find_request_work_complete(
                        request_id,
                        &pending_finds,
                        &pending_peer_routing,
                    ) {
                        pending_finds.remove(&request_id);
                        routed_peers.remove(&request_id);
                    }
                }
                event = self.swarm.select_next_some() => {
                    match event {
                        SwarmEvent::NewListenAddr { address, .. } => {
                            if !is_unspecified_addr(&address) {
                                tracing::debug!(%address, "Promoting listen address to external");
                                self.swarm.add_external_address(address.clone());
                            } else {
                                tracing::debug!(%address, "Skipping unspecified listen address");
                            }
                            network_state.add_listen_addr(address.to_vec()).await;
                        }
                        SwarmEvent::ExpiredListenAddr { address, .. } => {
                            self.swarm.remove_external_address(&address);
                            network_state.remove_listen_addr(&address.to_vec()).await;
                            // Track relay reservation expiry.
                            if is_circuit_addr(&address) {
                                active_relay_reservations =
                                    active_relay_reservations.saturating_sub(1);
                                tracing::info!(
                                    %address,
                                    active = active_relay_reservations,
                                    "Relay reservation expired"
                                );
                                // Try to replace the expired reservation.
                                try_reserve_relay(
                                    &mut relay_candidates,
                                    &mut active_relay_reservations,
                                    &mut inflight_relay_requests,
                                    &mut pending_relay_attempts,
                                    &mut self.swarm,
                                );
                            }
                        }
                        SwarmEvent::ConnectionEstablished { peer_id, .. } => {
                            connected_peers.insert(peer_id);
                            network_state
                                .set_connected_peer_count(connected_peers.len())
                                .await;

                            if let Some(senders) = pending_connects.remove(&peer_id) {
                                for sender in senders {
                                    let _ = sender.send(Ok(()));
                                }
                            }
                        }
                        SwarmEvent::ConnectionClosed {
                            peer_id,
                            num_established,
                            ..
                        } => {
                            if num_established == 0 {
                                connected_peers.remove(&peer_id);
                            }
                            network_state
                                .set_connected_peer_count(connected_peers.len())
                                .await;
                            clear_pending_relay_attempts(
                                peer_id,
                                &mut inflight_relay_requests,
                                &mut pending_relay_attempts,
                            );
                        }
                        SwarmEvent::OutgoingConnectionError {
                            peer_id: Some(peer_id),
                            error,
                            ..
                        } => {
                            if let Some(senders) = pending_connects.remove(&peer_id) {
                                for sender in senders {
                                    let _ = sender.send(Err(error.to_string()));
                                }
                            }
                            clear_pending_relay_attempts(
                                peer_id,
                                &mut inflight_relay_requests,
                                &mut pending_relay_attempts,
                            );
                        }
                        // Classify new peer addresses into the correct DHT.
                        SwarmEvent::NewExternalAddrOfPeer { peer_id, address } => {
                            peer_addr_book.entry(peer_id).or_default().push(address.clone());
                            if is_lan_addr(&address) {
                                self.swarm.behaviour_mut().kad_lan.add_address(&peer_id, address);
                            } else {
                                self.swarm.behaviour_mut().kad.add_address(&peer_id, address);
                            }
                        }
                        // WAN Kad events
                        SwarmEvent::Behaviour(BehaviourEvent::Kad(
                            kad::Event::OutboundQueryProgressed { id, result, step, .. },
                        )) => {
                            handle_kad_event(
                                DhtSource::Wan, id, result, &step,
                                &mut self.swarm,
                                &mut pending_provides, &mut provide_query_to_req,
                                &mut pending_provider_claims, &mut provider_ownership,
                                &mut pending_finds, &mut find_query_to_req,
                                &mut peer_addr_book, &mut pending_peer_routing,
                                &mut routed_peers,
                            );
                            if step.last {
                                let cleanup = cleanup_query(
                                    DhtSource::Wan, id,
                                    &mut provide_query_to_req, &mut pending_provides,
                                    &mut pending_provider_claims,
                                    &mut find_query_to_req, &mut pending_finds,
                                    &mut pending_peer_routing, &mut routed_peers,
                                );
                                if let Some(outcome) = cleanup.provide {
                                    apply_provide_outcome(
                                        outcome,
                                        &mut provider_ownership,
                                        &mut self.swarm,
                                    );
                                }
                                if let Some(request_id) = cleanup.completed_find {
                                    cancel_find_request(
                                        request_id,
                                        &mut self.swarm,
                                        &mut find_query_to_req,
                                        &mut pending_finds,
                                        &mut pending_peer_routing,
                                        &mut routed_peers,
                                    );
                                }
                            }
                        }
                        SwarmEvent::Behaviour(BehaviourEvent::Kad(ref ev)) => {
                            tracing::debug!("WAN Kad event: {ev:?}");
                        }
                        // LAN Kad events
                        SwarmEvent::Behaviour(BehaviourEvent::KadLan(
                            kad::Event::OutboundQueryProgressed { id, result, step, .. },
                        )) => {
                            handle_kad_event(
                                DhtSource::Lan, id, result, &step,
                                &mut self.swarm,
                                &mut pending_provides, &mut provide_query_to_req,
                                &mut pending_provider_claims, &mut provider_ownership,
                                &mut pending_finds, &mut find_query_to_req,
                                &mut peer_addr_book, &mut pending_peer_routing,
                                &mut routed_peers,
                            );
                            if step.last {
                                let cleanup = cleanup_query(
                                    DhtSource::Lan, id,
                                    &mut provide_query_to_req, &mut pending_provides,
                                    &mut pending_provider_claims,
                                    &mut find_query_to_req, &mut pending_finds,
                                    &mut pending_peer_routing, &mut routed_peers,
                                );
                                if let Some(outcome) = cleanup.provide {
                                    apply_provide_outcome(
                                        outcome,
                                        &mut provider_ownership,
                                        &mut self.swarm,
                                    );
                                }
                                if let Some(request_id) = cleanup.completed_find {
                                    cancel_find_request(
                                        request_id,
                                        &mut self.swarm,
                                        &mut find_query_to_req,
                                        &mut pending_finds,
                                        &mut pending_peer_routing,
                                        &mut routed_peers,
                                    );
                                }
                            }
                        }
                        SwarmEvent::Behaviour(BehaviourEvent::KadLan(ref ev)) => {
                            tracing::debug!("LAN Kad event: {ev:?}");
                        }
                        // --- Identify: relay discovery ---
                        SwarmEvent::Behaviour(BehaviourEvent::Identify(
                            libp2p::identify::Event::Received { peer_id, info, .. },
                        )) => {
                            handle_identify_received(
                                peer_id, &info,
                                nat_policy.status(),
                                &mut active_relay_reservations,
                                &mut inflight_relay_requests,
                                &mut pending_relay_attempts,
                                &mut relay_candidates,
                                &mut seen_relay_peers,
                                &mut self.swarm,
                            );
                        }
                        SwarmEvent::Behaviour(BehaviourEvent::Identify(_)) => {}
                        // --- AutoNAT v2: supplementary probes ---
                        SwarmEvent::Behaviour(BehaviourEvent::AutonatV2(ev)) => {
                            let success = ev.result.is_ok();
                            tracing::debug!(
                                addr = %ev.tested_addr,
                                server = %ev.server,
                                success,
                                "AutoNAT v2 probe"
                            );
                            network_state
                                .record_nat_probe_event(rpc::NatProbeEvent {
                                    tested_addr: ev.tested_addr.to_string(),
                                    server_peer_id: ev.server.to_string(),
                                    success,
                                    timestamp_unix_ms: SystemTime::now()
                                        .duration_since(UNIX_EPOCH)
                                        .unwrap_or_default()
                                        .as_millis() as u64,
                                })
                                .await;
                            if let Some(transition) = nat_policy.record_probe_result(success) {
                                let actions = actions_for_nat_transition(transition);
                                tracing::info!(
                                    from = ?transition.from,
                                    to = ?transition.to,
                                    "AutoNAT v2 node reachability transition"
                                );
                                network_state.set_nat_status(actions.set_status).await;
                                self.swarm.behaviour_mut().kad.set_mode(Some(actions.kad_mode));
                                if actions.try_reserve_relay {
                                    try_reserve_relay(
                                        &mut relay_candidates,
                                        &mut active_relay_reservations,
                                        &mut inflight_relay_requests,
                                        &mut pending_relay_attempts,
                                        &mut self.swarm,
                                    );
                                }
                            }
                        }
                        // --- Relay client ---
                        SwarmEvent::Behaviour(BehaviourEvent::RelayClient(
                            libp2p::relay::client::Event::ReservationReqAccepted {
                                relay_peer_id,
                                renewal,
                                ..
                            },
                        )) => {
                            clear_pending_relay_attempts(
                                relay_peer_id,
                                &mut inflight_relay_requests,
                                &mut pending_relay_attempts,
                            );
                            if !renewal {
                                active_relay_reservations =
                                    active_relay_reservations.saturating_add(1);
                            }
                            tracing::info!(
                                relay = %relay_peer_id,
                                renewal,
                                active = active_relay_reservations,
                                "Relay reservation accepted"
                            );
                        }
                        SwarmEvent::Behaviour(BehaviourEvent::RelayClient(ev)) => {
                            tracing::debug!("Relay client event: {ev:?}");
                        }
                        // --- DCUtR: hole-punch results ---
                        SwarmEvent::Behaviour(BehaviourEvent::Dcutr(ev)) => {
                            match &ev.result {
                                Ok(conn_id) => {
                                    tracing::info!(
                                        peer = %ev.remote_peer_id,
                                        connection = ?conn_id,
                                        "DCUtR hole-punch succeeded"
                                    );
                                }
                                Err(e) => {
                                    tracing::debug!(
                                        peer = %ev.remote_peer_id,
                                        error = %e,
                                        "DCUtR hole-punch failed (relayed connection remains)"
                                    );
                                }
                            }
                        }
                        // --- Stream behaviour has no events ---
                        SwarmEvent::Behaviour(BehaviourEvent::Stream(_)) => {}
                        _ => {}
                    }
                }
                cmd = cmd_rx.recv() => {
                    match cmd {
                        Some(SwarmCommand::Connect { peer_id, addrs, reply }) => {
                            if self.swarm.is_connected(&peer_id) {
                                let _ = reply.send(Ok(()));
                                continue;
                            }
                            let dial = DialOpts::peer_id(peer_id).addresses(addrs).build();
                            match self.swarm.dial(dial) {
                                Ok(()) => {
                                    pending_connects.entry(peer_id).or_default().push(reply);
                                }
                                Err(e) => {
                                    let _ = reply.send(Err(e.to_string()));
                                }
                            }
                        }
                        Some(SwarmCommand::KadProvide { owner, key, reply }) => {
                            if let Some(req_id) = pending_provider_claims.get(&(owner, key.clone())) {
                                if let Some(request) = pending_provides.get_mut(req_id) {
                                    request.add_owner(owner, reply);
                                    continue;
                                }
                            }

                            if let Some(req_id) = pending_provides
                                .iter()
                                .find_map(|(id, request)| (request.key == key).then_some(*id))
                            {
                                provider_ownership.claim(owner, key.clone());
                                pending_provider_claims.insert((owner, key.clone()), req_id);
                                pending_provides
                                    .get_mut(&req_id)
                                    .expect("pending provider request disappeared")
                                    .add_owner(owner, reply);
                                continue;
                            }

                            if provider_ownership.contains(owner, &key) {
                                let _ = reply.send(Ok(()));
                                continue;
                            }

                            let first_owner = provider_ownership.claim(owner, key.clone());
                            if !first_owner {
                                // Another live owner already keeps both local
                                // DHT registrations eligible for republication.
                                let _ = reply.send(Ok(()));
                                continue;
                            }

                            let req_id = next_request_id;
                            next_request_id += 1;

                            let record_key = kad::RecordKey::new(&key);
                            let beh = self.swarm.behaviour_mut();

                            let mut req = ProvideRequest::new(owner, key.clone(), reply);

                            // WAN provide
                            match start_owned_providing(&mut beh.kad, record_key.clone()) {
                                Ok(qid) => {
                                    provide_query_to_req.insert((DhtSource::Wan, qid), req_id);
                                }
                                Err(e) => {
                                    tracing::warn!("WAN provide failed to start: {e:?}");
                                    req.record(DhtSource::Wan, Err(format!("{e:?}")));
                                }
                            }

                            // LAN registration has the same owner and lifetime.
                            match start_owned_providing(&mut beh.kad_lan, record_key) {
                                Ok(qid) => {
                                    provide_query_to_req.insert((DhtSource::Lan, qid), req_id);
                                }
                                Err(e) => {
                                    tracing::debug!("LAN provide failed to start: {e:?}");
                                    req.record(DhtSource::Lan, Err(format!("{e:?}")));
                                }
                            }

                            if req.wan_done && req.lan_done {
                                apply_provide_outcome(
                                    req.finalize(),
                                    &mut provider_ownership,
                                    &mut self.swarm,
                                );
                            } else {
                                pending_provider_claims.insert((owner, key), req_id);
                                pending_provides.insert(req_id, req);
                            }
                        }
                        Some(SwarmCommand::KadReleaseProviderOwner { owner }) => {
                            let final_keys = provider_ownership.release_owner(owner);
                            for key in final_keys {
                                stop_local_providing(&mut self.swarm, &key);
                            }

                            let pending_ids: Vec<u64> = pending_provides
                                .iter()
                                .filter_map(|(id, request)| {
                                    request.owners.contains(&owner).then_some(*id)
                                })
                                .collect();
                            for request_id in pending_ids {
                                let remove_request = if let Some(request) = pending_provides.get_mut(&request_id) {
                                    pending_provider_claims.remove(&(owner, request.key.clone()));
                                    request.release_owner(
                                        owner,
                                        "provider owner epoch ended during provide",
                                    );
                                    request.owners.is_empty()
                                } else {
                                    false
                                };
                                if remove_request {
                                    pending_provides.remove(&request_id);
                                }
                            }
                        }
                        Some(SwarmCommand::KadFindProviders {
                            request,
                            key,
                            count,
                            reply,
                            cancel,
                        }) => {
                            if count == 0 {
                                drop(reply);
                                continue;
                            }
                            if *cancel.borrow() {
                                drop(reply);
                                continue;
                            }
                            let record_key = kad::RecordKey::new(&key);
                            let beh = self.swarm.behaviour_mut();

                            let mut remaining = 0u8;

                            // WAN query
                            let wan_qid = beh.kad.get_providers(record_key.clone());
                            find_query_to_req.insert((DhtSource::Wan, wan_qid), request.0);
                            remaining += 1;

                            // LAN query
                            let lan_qid = beh.kad_lan.get_providers(record_key);
                            find_query_to_req.insert((DhtSource::Lan, lan_qid), request.0);
                            remaining += 1;

                            pending_finds.insert(request.0, FindRequest {
                                sender: reply,
                                cancellation: cancel,
                                seen: HashSet::new(),
                                queued_peers: HashSet::new(),
                                pending: VecDeque::new(),
                                remaining,
                                limit: find_provider_limit(count),
                                delivered: 0,
                            });
                            routed_peers.insert(request.0, HashSet::new());
                        }
                        None => {
                            break;
                        }
                    }
                }
                _ = &mut walk_timer => {
                    let key = PeerId::random();
                    let beh = self.swarm.behaviour_mut();
                    beh.kad.get_closest_peers(key);
                    beh.kad_lan.get_closest_peers(key);
                    walk_interval = (walk_interval * 2).min(Duration::from_secs(60));
                    walk_timer
                        .as_mut()
                        .reset(tokio::time::Instant::now() + walk_interval);
                    tracing::debug!(%key, ?walk_interval, "kad random walk dispatched");
                }
            }
        }

        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Extracted Kad event handler (shared by WAN and LAN)
// ---------------------------------------------------------------------------

fn start_owned_providing(
    behaviour: &mut kad::Behaviour<kad::store::MemoryStore>,
    key: kad::RecordKey,
) -> Result<kad::QueryId, String> {
    let query = behaviour
        .start_providing(key.clone())
        .map_err(|error| format!("{error:?}"))?;
    let stored = behaviour
        .store_mut()
        .provided()
        .any(|record| record.key == key);
    if !stored {
        if let Some(mut query) = behaviour.query_mut(&query) {
            query.finish();
        }
        return Err("local provider store rejected the Wetware host record".into());
    }
    Ok(query)
}

fn stop_local_providing(swarm: &mut libp2p::swarm::Swarm<Behaviour>, key: &[u8]) {
    let record_key = kad::RecordKey::new(&key);
    let behaviour = swarm.behaviour_mut();
    behaviour.kad.stop_providing(&record_key);
    behaviour.kad_lan.stop_providing(&record_key);
}

fn apply_provide_outcome(
    outcome: ProvideOutcome,
    ownership: &mut ProviderOwnership,
    swarm: &mut libp2p::swarm::Swarm<Behaviour>,
) {
    if let ProvideOutcome::Failed { owners, key } = outcome {
        let mut final_owner = false;
        for owner in owners {
            final_owner |= ownership.release_key(owner, &key);
        }
        if final_owner {
            stop_local_providing(swarm, &key);
        }
    }
}

fn finish_kad_query(
    source: DhtSource,
    query: kad::QueryId,
    swarm: &mut libp2p::swarm::Swarm<Behaviour>,
) {
    let query = match source {
        DhtSource::Wan => swarm.behaviour_mut().kad.query_mut(&query),
        DhtSource::Lan => swarm.behaviour_mut().kad_lan.query_mut(&query),
    };
    if let Some(mut query) = query {
        query.finish();
    }
}

fn cancel_find_request(
    request_id: u64,
    swarm: &mut libp2p::swarm::Swarm<Behaviour>,
    find_query_to_req: &mut HashMap<DhtQueryKey, u64>,
    pending_finds: &mut HashMap<u64, FindRequest>,
    pending_peer_routing: &mut HashMap<DhtQueryKey, (PeerId, Option<u64>)>,
    routed_peers: &mut HashMap<u64, HashSet<PeerId>>,
) {
    let provider_queries: Vec<DhtQueryKey> = find_query_to_req
        .iter()
        .filter_map(|(query, owner)| (*owner == request_id).then_some(*query))
        .collect();
    for (source, query) in provider_queries {
        find_query_to_req.remove(&(source, query));
        finish_kad_query(source, query, swarm);
    }

    let route_queries: Vec<DhtQueryKey> = pending_peer_routing
        .iter()
        .filter_map(|(query, (_, owner))| (*owner == Some(request_id)).then_some(*query))
        .collect();
    for (source, query) in route_queries {
        pending_peer_routing.remove(&(source, query));
        finish_kad_query(source, query, swarm);
    }

    pending_finds.remove(&request_id);
    routed_peers.remove(&request_id);
}

fn finish_find_provider_queries(
    request_id: u64,
    swarm: &mut libp2p::swarm::Swarm<Behaviour>,
    find_query_to_req: &mut HashMap<DhtQueryKey, u64>,
    pending_finds: &mut HashMap<u64, FindRequest>,
) {
    let provider_queries: Vec<DhtQueryKey> = find_query_to_req
        .iter()
        .filter_map(|(query, owner)| (*owner == request_id).then_some(*query))
        .collect();
    for (source, query) in provider_queries {
        find_query_to_req.remove(&(source, query));
        finish_kad_query(source, query, swarm);
    }
    if let Some(request) = pending_finds.get_mut(&request_id) {
        request.remaining = 0;
    }
}

fn find_request_work_complete(
    request_id: u64,
    pending_finds: &HashMap<u64, FindRequest>,
    pending_peer_routing: &HashMap<DhtQueryKey, (PeerId, Option<u64>)>,
) -> bool {
    pending_finds
        .get(&request_id)
        .is_some_and(|request| request.remaining == 0 && request.pending.is_empty())
        && !pending_peer_routing
            .values()
            .any(|(_, owner)| *owner == Some(request_id))
}

#[allow(clippy::too_many_arguments)]
fn handle_kad_event(
    source: DhtSource,
    id: kad::QueryId,
    result: kad::QueryResult,
    step: &kad::ProgressStep,
    swarm: &mut libp2p::swarm::Swarm<Behaviour>,
    pending_provides: &mut HashMap<u64, ProvideRequest>,
    provide_query_to_req: &mut HashMap<DhtQueryKey, u64>,
    pending_provider_claims: &mut HashMap<(rpc::ProviderOwnerId, Vec<u8>), u64>,
    provider_ownership: &mut ProviderOwnership,
    pending_finds: &mut HashMap<u64, FindRequest>,
    find_query_to_req: &mut HashMap<DhtQueryKey, u64>,
    peer_addr_book: &mut HashMap<PeerId, Vec<Multiaddr>>,
    pending_peer_routing: &mut HashMap<DhtQueryKey, (PeerId, Option<u64>)>,
    routed_peers: &mut HashMap<u64, HashSet<PeerId>>,
) {
    let key = (source, id);
    let mut cancel_find = None;
    let mut finish_provider_find = None;
    let label = match source {
        DhtSource::Wan => "WAN",
        DhtSource::Lan => "LAN",
    };

    match result {
        kad::QueryResult::Bootstrap(Ok(ok)) => {
            tracing::debug!(
                dht = label,
                peer = %ok.peer,
                remaining = ok.num_remaining,
                "Kad bootstrap progress"
            );
        }
        kad::QueryResult::Bootstrap(Err(e)) => {
            tracing::warn!(dht = label, "Kad bootstrap error: {e:?}");
        }
        kad::QueryResult::StartProviding(Ok(_)) => {
            tracing::debug!(dht = label, "Kad provide succeeded");
            if let Some(&req_id) = provide_query_to_req.get(&key) {
                if let Some(req) = pending_provides.get_mut(&req_id) {
                    if req.record(source, Ok(())) {
                        if let Some(req) = pending_provides.remove(&req_id) {
                            for owner in &req.owners {
                                pending_provider_claims.remove(&(*owner, req.key.clone()));
                            }
                            apply_provide_outcome(req.finalize(), provider_ownership, swarm);
                        }
                    }
                }
            }
        }
        kad::QueryResult::StartProviding(Err(e)) => {
            tracing::warn!(dht = label, "Kad provide FAILED: {e:?}");
            if let Some(&req_id) = provide_query_to_req.get(&key) {
                if let Some(req) = pending_provides.get_mut(&req_id) {
                    if req.record(source, Err(format!("{e:?}"))) {
                        if let Some(req) = pending_provides.remove(&req_id) {
                            for owner in &req.owners {
                                pending_provider_claims.remove(&(*owner, req.key.clone()));
                            }
                            apply_provide_outcome(req.finalize(), provider_ownership, swarm);
                        }
                    }
                }
            }
        }
        kad::QueryResult::GetProviders(Ok(kad::GetProvidersOk::FoundProviders {
            providers,
            ..
        })) => {
            tracing::debug!(
                dht = label,
                count = providers.len(),
                "Kad found providers batch"
            );
            if let Some(&req_id) = find_query_to_req.get(&key) {
                if let Some(find_req) = pending_finds.get_mut(&req_id) {
                    for provider in &providers {
                        if !find_req.select_provider(*provider) {
                            continue;
                        }

                        let addrs: Vec<Multiaddr> =
                            peer_addr_book.get(provider).cloned().unwrap_or_default();

                        for addr in &addrs {
                            swarm.add_peer_address(*provider, addr.clone());
                        }

                        if !addrs.is_empty() {
                            tracing::debug!(
                                dht = label,
                                peer = %provider,
                                addr_count = addrs.len(),
                                "Provider discovered with addresses"
                            );
                            find_req.queue_resolved_provider(*provider, &addrs);
                        } else {
                            tracing::debug!(
                                dht = label,
                                peer = %provider,
                                "No addresses for provider; issuing peer routing query"
                            );
                            // Query both DHTs for peer routing, scoped to this request.
                            let beh = swarm.behaviour_mut();
                            let wan_qid = beh.kad.get_closest_peers(*provider);
                            pending_peer_routing
                                .insert((DhtSource::Wan, wan_qid), (*provider, Some(req_id)));
                            let lan_qid = beh.kad_lan.get_closest_peers(*provider);
                            pending_peer_routing
                                .insert((DhtSource::Lan, lan_qid), (*provider, Some(req_id)));
                        }

                        if find_req.selection_complete() {
                            finish_provider_find = Some(req_id);
                            break;
                        }
                    }
                }
            }
        }
        kad::QueryResult::GetProviders(Ok(
            kad::GetProvidersOk::FinishedWithNoAdditionalRecord { closest_peers, .. },
        )) => {
            tracing::debug!(
                dht = label,
                closest = closest_peers.len(),
                "Kad find_providers finished (no more records)"
            );
        }
        kad::QueryResult::GetProviders(Err(e)) => {
            tracing::warn!(dht = label, "Kad find_providers FAILED: {e:?}");
        }
        kad::QueryResult::GetClosestPeers(Ok(kad::GetClosestPeersOk { ref peers, .. })) => {
            if let Some((target, owner_req)) = pending_peer_routing.remove(&key) {
                // Mark peer as routed only in the owning request's set.
                if let Some(req_id) = owner_req {
                    if let Some(routed) = routed_peers.get_mut(&req_id) {
                        routed.insert(target);
                    }
                }
                if let Some(info) = peers.iter().find(|p| p.peer_id == target) {
                    tracing::debug!(
                        dht = label,
                        peer = %target,
                        addr_count = info.addrs.len(),
                        "Peer routing resolved addresses"
                    );
                    for addr in &info.addrs {
                        swarm.add_peer_address(target, addr.clone());
                    }
                    peer_addr_book
                        .entry(target)
                        .or_default()
                        .extend(info.addrs.iter().cloned());
                    // Deliver the now-addressable provider to the owning find request.
                    if let Some(req_id) = owner_req {
                        if let Some(find_req) = pending_finds.get_mut(&req_id) {
                            if !info.addrs.is_empty() {
                                find_req.queue_resolved_provider(target, &info.addrs);
                            }
                        }
                    }
                } else {
                    tracing::debug!(
                        dht = label,
                        peer = %target,
                        closest_returned = peers.len(),
                        "Peer routing: target not found in closest peers"
                    );
                }
                if let Some(req_id) = owner_req {
                    if find_request_work_complete(req_id, pending_finds, pending_peer_routing) {
                        cancel_find = Some(req_id);
                    }
                }
            }
        }
        kad::QueryResult::GetClosestPeers(Err(ref e)) => {
            if let Some((target, owner_req)) = pending_peer_routing.remove(&key) {
                if let Some(req_id) = owner_req {
                    if let Some(routed) = routed_peers.get_mut(&req_id) {
                        routed.insert(target);
                    }
                }
                tracing::warn!(dht = label, peer = %target, "Peer routing query failed: {e:?}");
                if let Some(req_id) = owner_req {
                    if find_request_work_complete(req_id, pending_finds, pending_peer_routing) {
                        cancel_find = Some(req_id);
                    }
                }
            }
        }
        _ => {
            tracing::debug!(dht = label, "Kad query progress (other): {result:?}");
        }
    }
    if let Some(request_id) = finish_provider_find {
        finish_find_provider_queries(request_id, swarm, find_query_to_req, pending_finds);
    }
    if let Some(request_id) = cancel_find {
        cancel_find_request(
            request_id,
            swarm,
            find_query_to_req,
            pending_finds,
            pending_peer_routing,
            routed_peers,
        );
    }
    tracing::debug!(
        dht = label,
        query_id = ?id,
        step_count = step.count,
        last = step.last,
        "Kad query step"
    );
}

/// Clean up maps when a DHT query finishes (`step.last == true`).
/// For find_providers, decrement `remaining` and finish after both provider
/// queries and any bounded provider address-resolution queries are done.
#[derive(Default)]
struct QueryCleanup {
    provide: Option<ProvideOutcome>,
    completed_find: Option<u64>,
}

#[allow(clippy::too_many_arguments)]
fn cleanup_query(
    source: DhtSource,
    id: kad::QueryId,
    provide_query_to_req: &mut HashMap<DhtQueryKey, u64>,
    pending_provides: &mut HashMap<u64, ProvideRequest>,
    pending_provider_claims: &mut HashMap<(rpc::ProviderOwnerId, Vec<u8>), u64>,
    find_query_to_req: &mut HashMap<DhtQueryKey, u64>,
    pending_finds: &mut HashMap<u64, FindRequest>,
    pending_peer_routing: &mut HashMap<DhtQueryKey, (PeerId, Option<u64>)>,
    routed_peers: &mut HashMap<u64, HashSet<PeerId>>,
) -> QueryCleanup {
    let key = (source, id);
    let mut cleanup = QueryCleanup::default();

    // Provide cleanup
    if let Some(req_id) = provide_query_to_req.remove(&key) {
        // If no more queries reference this request, finalize it.
        if !provide_query_to_req.values().any(|&r| r == req_id) {
            if let Some(req) = pending_provides.remove(&req_id) {
                for owner in &req.owners {
                    pending_provider_claims.remove(&(*owner, req.key.clone()));
                }
                cleanup.provide = Some(req.finalize());
            }
        }
    }

    // FindProviders cleanup
    if let Some(req_id) = find_query_to_req.remove(&key) {
        if let Some(find_req) = pending_finds.get_mut(&req_id) {
            find_req.remaining = find_req.remaining.saturating_sub(1);
        }
        if find_request_work_complete(req_id, pending_finds, pending_peer_routing) {
            // Provider discovery and its bounded address resolution are done.
            pending_finds.remove(&req_id);
            routed_peers.remove(&req_id);
            cleanup.completed_find = Some(req_id);
        }
    }

    // Peer routing cleanup (compound key removes the specific entry).
    pending_peer_routing.remove(&key);
    cleanup
}

// ---------------------------------------------------------------------------
// NAT traversal helpers
// ---------------------------------------------------------------------------

/// Returns true if the multiaddr contains a `/p2p-circuit` component.
fn is_circuit_addr(addr: &Multiaddr) -> bool {
    use libp2p::multiaddr::Protocol;
    addr.iter().any(|p| matches!(p, Protocol::P2pCircuit))
}

/// Handle Identify Received events: discover relay-capable peers.
#[allow(clippy::too_many_arguments)]
fn handle_identify_received(
    peer_id: PeerId,
    info: &libp2p::identify::Info,
    nat_status: NatReachability,
    active_relay_reservations: &mut usize,
    inflight_relay_requests: &mut usize,
    pending_relay_attempts: &mut HashMap<PeerId, usize>,
    relay_candidates: &mut Vec<(PeerId, Multiaddr)>,
    seen_relay_peers: &mut HashSet<PeerId>,
    swarm: &mut libp2p::swarm::Swarm<Behaviour>,
) {
    // Check if this peer can serve as a relay.
    if !is_relay_capable(&info.protocols) {
        return;
    }

    // Deduplicate: skip peers we've already seen.
    if !seen_relay_peers.insert(peer_id) {
        return;
    }

    tracing::debug!(peer = %peer_id, "Discovered relay-capable peer");

    // Build the relay address from the peer's listen addresses.
    // Pick the first non-LAN address (or any address as fallback).
    let relay_addr = info
        .listen_addrs
        .iter()
        .find(|a| !is_lan_addr(a))
        .or_else(|| info.listen_addrs.first());

    let Some(base_addr) = relay_addr else {
        tracing::debug!(peer = %peer_id, "Relay-capable peer has no addresses");
        return;
    };

    let circuit_addr = base_addr
        .clone()
        .with(libp2p::multiaddr::Protocol::P2p(peer_id))
        .with(libp2p::multiaddr::Protocol::P2pCircuit);

    // If we're known-public, just track the candidate for later.
    if nat_status == NatReachability::Public {
        relay_candidates.push((peer_id, circuit_addr));
        return;
    }

    // Try to reserve if we're NATted or status is unknown.
    // Count both active and in-flight to avoid overshooting the cap.
    if *active_relay_reservations + *inflight_relay_requests < MAX_RELAY_RESERVATIONS {
        request_relay_reservation(
            peer_id,
            circuit_addr.clone(),
            inflight_relay_requests,
            pending_relay_attempts,
            swarm,
        );
    } else {
        // Save for later in case a reservation expires.
        relay_candidates.push((peer_id, circuit_addr));
    }
}

/// Try to reserve relay slots from accumulated candidates.
fn try_reserve_relay(
    relay_candidates: &mut Vec<(PeerId, Multiaddr)>,
    active_relay_reservations: &mut usize,
    inflight_relay_requests: &mut usize,
    pending_relay_attempts: &mut HashMap<PeerId, usize>,
    swarm: &mut libp2p::swarm::Swarm<Behaviour>,
) {
    while *active_relay_reservations + *inflight_relay_requests < MAX_RELAY_RESERVATIONS {
        let Some((peer_id, circuit_addr)) = relay_candidates.pop() else {
            break;
        };
        request_relay_reservation(
            peer_id,
            circuit_addr,
            inflight_relay_requests,
            pending_relay_attempts,
            swarm,
        );
    }
}

fn request_relay_reservation(
    peer_id: PeerId,
    circuit_addr: Multiaddr,
    inflight_relay_requests: &mut usize,
    pending_relay_attempts: &mut HashMap<PeerId, usize>,
    swarm: &mut libp2p::swarm::Swarm<Behaviour>,
) {
    tracing::info!(
        relay = %peer_id,
        addr = %circuit_addr,
        "Requesting relay reservation"
    );
    if let Err(e) = swarm.listen_on(circuit_addr) {
        tracing::warn!(
            relay = %peer_id,
            error = %e,
            "Failed to request relay reservation"
        );
    } else {
        *inflight_relay_requests += 1;
        *pending_relay_attempts.entry(peer_id).or_insert(0) += 1;
    }
}

fn clear_pending_relay_attempts(
    peer_id: PeerId,
    inflight_relay_requests: &mut usize,
    pending_relay_attempts: &mut HashMap<PeerId, usize>,
) {
    if let Some(count) = pending_relay_attempts.remove(&peer_id) {
        *inflight_relay_requests = inflight_relay_requests.saturating_sub(count);
        tracing::info!(
            relay = %peer_id,
            cleared = count,
            inflight = *inflight_relay_requests,
            "Cleared pending relay attempts"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use tokio::sync::oneshot;

    fn find_cancellation_receiver() -> watch::Receiver<bool> {
        let (_sender, receiver) = watch::channel(false);
        receiver
    }

    // -------------------------------------------------------------------
    // is_lan_addr / is_lan_ip
    // -------------------------------------------------------------------

    #[test]
    fn test_is_lan_addr_private_ipv4() {
        let cases = [
            "/ip4/10.0.0.1/tcp/4001",
            "/ip4/172.16.0.1/tcp/4001",
            "/ip4/172.31.255.255/tcp/4001",
            "/ip4/192.168.1.1/tcp/4001",
        ];
        for addr_str in &cases {
            let addr: Multiaddr = addr_str.parse().unwrap();
            assert!(is_lan_addr(&addr), "{addr_str} should be LAN");
        }
    }

    #[test]
    fn test_is_lan_addr_ipv6_ula() {
        let addr: Multiaddr = "/ip6/fd12:3456:789a::1/tcp/4001".parse().unwrap();
        assert!(is_lan_addr(&addr), "IPv6 ULA (fd00::/8) should be LAN");
    }

    #[test]
    fn test_is_lan_addr_loopback() {
        let v4: Multiaddr = "/ip4/127.0.0.1/tcp/4001".parse().unwrap();
        assert!(is_lan_addr(&v4));

        let v6: Multiaddr = "/ip6/::1/tcp/4001".parse().unwrap();
        assert!(is_lan_addr(&v6));
    }

    #[test]
    fn test_is_lan_addr_link_local() {
        let v4: Multiaddr = "/ip4/169.254.1.1/tcp/4001".parse().unwrap();
        assert!(is_lan_addr(&v4));

        let v6: Multiaddr = "/ip6/fe80::1/tcp/4001".parse().unwrap();
        assert!(is_lan_addr(&v6));
    }

    #[test]
    fn test_is_lan_addr_public() {
        let cases = [
            "/ip4/8.8.8.8/tcp/4001",
            "/ip4/1.1.1.1/tcp/4001",
            "/ip6/2001:db8::1/tcp/4001",
        ];
        for addr_str in &cases {
            let addr: Multiaddr = addr_str.parse().unwrap();
            assert!(!is_lan_addr(&addr), "{addr_str} should be WAN");
        }
    }

    #[test]
    fn test_is_lan_addr_no_ip() {
        let addr: Multiaddr = "/memory/1234".parse().unwrap();
        assert!(!is_lan_addr(&addr), "no IP should default to WAN");
    }

    // -------------------------------------------------------------------
    // DhtSource compound keys
    // -------------------------------------------------------------------

    #[test]
    fn test_compound_keys_no_collision() {
        let mut map: HashMap<DhtQueryKey, &str> = HashMap::new();
        // Simulate QueryId(0) from both DHTs — they must not collide.
        // We can't construct real QueryIds, so test the key logic directly.
        let wan_key = (DhtSource::Wan, unsafe {
            std::mem::transmute::<u64, kad::QueryId>(0)
        });
        let lan_key = (DhtSource::Lan, unsafe {
            std::mem::transmute::<u64, kad::QueryId>(0)
        });
        map.insert(wan_key, "wan");
        map.insert(lan_key, "lan");
        assert_eq!(map.len(), 2);
        assert_eq!(map[&wan_key], "wan");
        assert_eq!(map[&lan_key], "lan");
    }

    // -------------------------------------------------------------------
    // ProvideRequest
    // -------------------------------------------------------------------

    #[test]
    fn test_provide_request_first_success_wins() {
        let (tx, mut rx) = oneshot::channel();
        let mut req = ProvideRequest::new(rpc::ProviderOwnerId(1), b"key".to_vec(), tx);

        // WAN succeeds first
        assert!(!req.record(DhtSource::Wan, Ok(())));
        // Reply already sent
        assert!(req.replies.is_empty());
        // LAN result comes later
        assert!(req.record(DhtSource::Lan, Err("no peers".into())));

        // The receiver got Ok
        assert!(rx.try_recv().unwrap().is_ok());
    }

    #[test]
    fn test_provide_request_both_fail_sends_wan_error() {
        let (tx, mut rx) = oneshot::channel();
        let mut req = ProvideRequest::new(rpc::ProviderOwnerId(1), b"key".to_vec(), tx);

        assert!(!req.record(DhtSource::Lan, Err("lan fail".into())));
        assert!(req.record(DhtSource::Wan, Err("wan fail".into())));
        assert!(matches!(req.finalize(), ProvideOutcome::Failed { .. }));

        let result = rx.try_recv().unwrap();
        assert!(result.is_err());
        assert_eq!(result.unwrap_err(), "wan fail");
    }

    #[test]
    fn provide_request_lan_success_survives_wan_failure() {
        let (tx, mut rx) = oneshot::channel();
        let mut request = ProvideRequest::new(rpc::ProviderOwnerId(1), b"key".to_vec(), tx);

        assert!(!request.record(DhtSource::Lan, Ok(())));
        assert!(request.record(DhtSource::Wan, Err("wan fail".into())));
        assert!(matches!(request.finalize(), ProvideOutcome::Active));
        assert!(rx.try_recv().expect("provide response").is_ok());
    }

    #[test]
    fn failed_pending_provide_rolls_back_every_joined_owner() {
        let first = rpc::ProviderOwnerId(1);
        let second = rpc::ProviderOwnerId(2);
        let (first_tx, mut first_rx) = oneshot::channel();
        let (second_tx, mut second_rx) = oneshot::channel();
        let mut request = ProvideRequest::new(first, b"key".to_vec(), first_tx);
        request.add_owner(second, second_tx);

        assert!(!request.record(DhtSource::Wan, Err("wan fail".into())));
        assert!(request.record(DhtSource::Lan, Err("lan fail".into())));
        match request.finalize() {
            ProvideOutcome::Failed { owners, key } => {
                assert_eq!(
                    HashSet::<_>::from_iter(owners),
                    HashSet::from([first, second])
                );
                assert_eq!(key, b"key");
            }
            ProvideOutcome::Active => panic!("failed dual-DHT request became active"),
        }
        assert!(first_rx.try_recv().expect("first reply").is_err());
        assert!(second_rx.try_recv().expect("second reply").is_err());
    }

    // -------------------------------------------------------------------
    // FindRequest dedup
    // -------------------------------------------------------------------

    #[test]
    fn test_find_request_dedup_across_dhts() {
        let (tx, mut rx) = mpsc::channel(1);
        let mut find = FindRequest {
            sender: tx,
            cancellation: find_cancellation_receiver(),
            seen: HashSet::new(),
            queued_peers: HashSet::new(),
            pending: VecDeque::new(),
            remaining: 2,
            limit: 1,
            delivered: 0,
        };

        let peer_bytes = vec![0u8; 32]; // dummy peer ID bytes

        // First insert succeeds
        let peer_id: PeerId = PeerId::random();
        assert!(find.seen.insert(peer_id));

        // Same peer from other DHT is a duplicate
        assert!(!find.seen.insert(peer_id));

        // Send one provider through
        find.sender
            .try_send(PeerInfo {
                peer_id: peer_bytes.clone(),
                addrs: vec![],
            })
            .expect("single-slot channel has capacity");

        assert!(rx.try_recv().is_ok());

        // Decrement remaining
        find.remaining -= 1;
        assert_eq!(find.remaining, 1);
        find.remaining -= 1;
        assert_eq!(find.remaining, 0);
    }

    #[tokio::test]
    async fn find_request_retains_multi_provider_batch_while_handoff_is_busy() {
        let (tx, mut rx) = mpsc::channel(1);
        tx.send(PeerInfo {
            peer_id: b"occupied".to_vec(),
            addrs: Vec::new(),
        })
        .await
        .expect("occupy the single-slot handoff");

        let mut request = FindRequest {
            sender: tx,
            cancellation: find_cancellation_receiver(),
            seen: HashSet::new(),
            queued_peers: HashSet::new(),
            pending: VecDeque::new(),
            remaining: 2,
            limit: 3,
            delivered: 0,
        };
        let address: Multiaddr = "/ip4/127.0.0.1/tcp/4001".parse().expect("address");
        let peers = [PeerId::random(), PeerId::random(), PeerId::random()];
        for peer in peers {
            assert!(request.select_provider(peer));
            request.queue_resolved_provider(peer, std::slice::from_ref(&address));
        }

        assert!(request.selection_complete());
        assert_eq!(request.pending.len(), 3);
        assert_eq!(
            rx.recv().await.expect("occupied result").peer_id,
            b"occupied"
        );

        for expected in peers {
            let provider = request.pending.front().expect("pending provider").clone();
            request
                .sender
                .send(provider.info)
                .await
                .expect("handoff accepts pending provider");
            let delivered = request
                .pending
                .pop_front()
                .expect("remove delivered provider");
            assert_eq!(delivered.peer_id, expected);
            assert_eq!(
                rx.recv().await.expect("receive pending provider").peer_id,
                expected.to_bytes()
            );
        }
        assert!(request.pending.is_empty());
    }

    #[test]
    fn find_request_caps_untrusted_count_at_the_kad_replication_bound() {
        let (sender, _receiver) = mpsc::channel(1);
        let mut request = FindRequest {
            sender,
            cancellation: find_cancellation_receiver(),
            seen: HashSet::new(),
            queued_peers: HashSet::new(),
            pending: VecDeque::new(),
            remaining: 2,
            limit: find_provider_limit(u32::MAX),
            delivered: 0,
        };
        let address: Multiaddr = "/ip4/127.0.0.1/tcp/4001".parse().expect("address");

        for _ in 0..KAD_REPLICATION_FACTOR {
            let peer = PeerId::random();
            assert!(request.select_provider(peer));
            request.queue_resolved_provider(peer, std::slice::from_ref(&address));
        }
        assert!(!request.select_provider(PeerId::random()));
        assert_eq!(request.pending.len(), KAD_REPLICATION_FACTOR);
        assert_eq!(request.seen.len(), KAD_REPLICATION_FACTOR);
    }

    #[tokio::test]
    async fn slow_find_handoff_does_not_block_another_request() {
        let (slow_sender, mut slow_receiver) = mpsc::channel(1);
        slow_sender
            .send(PeerInfo {
                peer_id: b"occupied".to_vec(),
                addrs: Vec::new(),
            })
            .await
            .expect("occupy slow handoff");
        let (fast_sender, mut fast_receiver) = mpsc::channel(1);
        let slow_peer = PeerId::random();
        let fast_peer = PeerId::random();

        let requests = HashMap::from([
            (
                1,
                FindRequest {
                    sender: slow_sender,
                    cancellation: find_cancellation_receiver(),
                    seen: HashSet::from([slow_peer]),
                    queued_peers: HashSet::from([slow_peer]),
                    pending: VecDeque::from([PendingProvider {
                        peer_id: slow_peer,
                        info: PeerInfo {
                            peer_id: slow_peer.to_bytes(),
                            addrs: Vec::new(),
                        },
                    }]),
                    remaining: 2,
                    limit: 1,
                    delivered: 0,
                },
            ),
            (
                2,
                FindRequest {
                    sender: fast_sender,
                    cancellation: find_cancellation_receiver(),
                    seen: HashSet::from([fast_peer]),
                    queued_peers: HashSet::from([fast_peer]),
                    pending: VecDeque::from([PendingProvider {
                        peer_id: fast_peer,
                        info: PeerInfo {
                            peer_id: fast_peer.to_bytes(),
                            addrs: Vec::new(),
                        },
                    }]),
                    remaining: 2,
                    limit: 1,
                    delivered: 0,
                },
            ),
        ]);

        let delivery = tokio::time::timeout(
            Duration::from_secs(1),
            send_next_find_provider(pending_find_deliveries(&requests)),
        )
        .await
        .expect("writable Finder request was blocked")
        .expect("pending delivery");
        assert_eq!(delivery, (2, fast_peer, true));
        assert_eq!(
            fast_receiver
                .recv()
                .await
                .expect("fast provider delivery")
                .peer_id,
            fast_peer.to_bytes()
        );
        assert_eq!(
            slow_receiver
                .recv()
                .await
                .expect("occupied slow handoff")
                .peer_id,
            b"occupied"
        );
    }

    #[tokio::test]
    async fn find_cancellation_token_identifies_the_request_without_a_command() {
        let (cancel_sender, cancel_receiver) = watch::channel(false);
        let (sender, _receiver) = mpsc::channel(1);
        let requests = HashMap::from([(
            17,
            FindRequest {
                sender,
                cancellation: cancel_receiver,
                seen: HashSet::new(),
                queued_peers: HashSet::new(),
                pending: VecDeque::new(),
                remaining: 2,
                limit: 1,
                delivered: 0,
            },
        )]);

        cancel_sender.send_replace(true);
        let canceled = tokio::time::timeout(
            Duration::from_secs(1),
            wait_for_next_find_cancellation(pending_find_cancellations(&requests)),
        )
        .await
        .expect("Finder cancellation token was not observed");
        assert_eq!(canceled, Some(17));
    }

    // -------------------------------------------------------------------
    // cleanup_query
    // -------------------------------------------------------------------

    #[test]
    fn test_cleanup_find_providers_closes_on_both_done() {
        let (tx, mut rx) = mpsc::channel::<PeerInfo>(1);

        let mut pending_finds: HashMap<u64, FindRequest> = HashMap::new();
        let mut find_query_to_req: HashMap<DhtQueryKey, u64> = HashMap::new();
        let mut provide_query_to_req: HashMap<DhtQueryKey, u64> = HashMap::new();
        let mut pending_provides: HashMap<u64, ProvideRequest> = HashMap::new();
        let mut pending_provider_claims = HashMap::new();
        let mut pending_peer_routing: HashMap<DhtQueryKey, (PeerId, Option<u64>)> = HashMap::new();
        let mut routed: HashMap<u64, HashSet<PeerId>> = HashMap::new();

        let req_id = 0u64;
        let wan_qid: kad::QueryId = unsafe { std::mem::transmute(1u64) };
        let lan_qid: kad::QueryId = unsafe { std::mem::transmute(2u64) };

        find_query_to_req.insert((DhtSource::Wan, wan_qid), req_id);
        find_query_to_req.insert((DhtSource::Lan, lan_qid), req_id);
        pending_finds.insert(
            req_id,
            FindRequest {
                sender: tx,
                cancellation: find_cancellation_receiver(),
                seen: HashSet::new(),
                queued_peers: HashSet::new(),
                pending: VecDeque::new(),
                remaining: 2,
                limit: 10,
                delivered: 0,
            },
        );
        routed.insert(req_id, HashSet::new());

        // WAN finishes first — channel should stay open.
        let _ = cleanup_query(
            DhtSource::Wan,
            wan_qid,
            &mut provide_query_to_req,
            &mut pending_provides,
            &mut pending_provider_claims,
            &mut find_query_to_req,
            &mut pending_finds,
            &mut pending_peer_routing,
            &mut routed,
        );
        assert!(pending_finds.contains_key(&req_id));
        assert!(rx.try_recv().is_err()); // not closed yet

        // LAN finishes — channel should close.
        let _ = cleanup_query(
            DhtSource::Lan,
            lan_qid,
            &mut provide_query_to_req,
            &mut pending_provides,
            &mut pending_provider_claims,
            &mut find_query_to_req,
            &mut pending_finds,
            &mut pending_peer_routing,
            &mut routed,
        );
        assert!(!pending_finds.contains_key(&req_id));
        assert!(!routed.contains_key(&req_id));
        // Channel is closed now — recv returns None
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn find_completion_waits_for_address_resolution() {
        let request_id = 7;
        let (sender, _receiver) = mpsc::channel(1);
        let mut pending_finds = HashMap::from([(
            request_id,
            FindRequest {
                sender,
                cancellation: find_cancellation_receiver(),
                seen: HashSet::new(),
                queued_peers: HashSet::new(),
                pending: VecDeque::new(),
                remaining: 0,
                limit: 1,
                delivered: 0,
            },
        )]);
        let query: kad::QueryId = unsafe { std::mem::transmute(3u64) };
        let mut pending_peer_routing = HashMap::from([(
            (DhtSource::Wan, query),
            (PeerId::random(), Some(request_id)),
        )]);

        assert!(!find_request_work_complete(
            request_id,
            &pending_finds,
            &pending_peer_routing,
        ));
        pending_peer_routing.clear();
        assert!(find_request_work_complete(
            request_id,
            &pending_finds,
            &pending_peer_routing,
        ));
        pending_finds.remove(&request_id);
    }

    #[test]
    fn provider_ownership_stops_only_after_final_owner() {
        let key = b"cid-multihash".to_vec();
        let first = rpc::ProviderOwnerId(1);
        let second = rpc::ProviderOwnerId(2);
        let mut ownership = ProviderOwnership::default();

        assert!(ownership.claim(first, key.clone()));
        assert!(
            !ownership.claim(first, key.clone()),
            "duplicate claim is idempotent"
        );
        assert!(
            !ownership.claim(second, key.clone()),
            "second owner shares provision"
        );
        assert!(!ownership.release_key(first, &key));
        assert!(ownership.contains(second, &key));
        assert!(ownership.release_key(second, &key));
        assert!(ownership.by_key.is_empty());
        assert!(ownership.by_owner.is_empty());
    }

    #[test]
    fn provider_owner_release_cleans_all_keys_without_affecting_other_owner() {
        let first = rpc::ProviderOwnerId(1);
        let second = rpc::ProviderOwnerId(2);
        let shared = b"shared".to_vec();
        let exclusive = b"exclusive".to_vec();
        let mut ownership = ProviderOwnership::default();
        ownership.claim(first, shared.clone());
        ownership.claim(second, shared.clone());
        ownership.claim(first, exclusive.clone());

        assert_eq!(ownership.release_owner(first), vec![exclusive]);
        assert!(ownership.contains(second, &shared));
        assert_eq!(ownership.release_owner(second), vec![shared]);
    }

    #[test]
    fn libp2p_stop_providing_removes_local_republication_source() {
        let peer = PeerId::random();
        let store = kad::store::MemoryStore::new(peer);
        let config = kad::Config::new(kad::PROTOCOL_NAME);
        let mut behaviour = kad::Behaviour::with_config(peer, store, config);
        let key = kad::RecordKey::new(b"owned-provider");

        start_owned_providing(&mut behaviour, key.clone()).expect("start local provision");
        assert_eq!(behaviour.store_mut().provided().count(), 1);

        behaviour.stop_providing(&key);
        assert_eq!(
            behaviour.store_mut().provided().count(),
            0,
            "removed local records cannot enter a later republication cycle"
        );
    }

    // -------------------------------------------------------------------
    // NAT traversal helpers
    // -------------------------------------------------------------------

    #[test]
    fn test_is_unspecified_addr_ipv4() {
        let addr: Multiaddr = "/ip4/0.0.0.0/tcp/2025".parse().unwrap();
        assert!(is_unspecified_addr(&addr));
    }

    #[test]
    fn test_is_unspecified_addr_ipv6() {
        let addr: Multiaddr = "/ip6/::/tcp/2025".parse().unwrap();
        assert!(is_unspecified_addr(&addr));
    }

    #[test]
    fn test_is_unspecified_addr_real_ip() {
        let cases = [
            "/ip4/192.168.1.1/tcp/2025",
            "/ip4/8.8.8.8/tcp/2025",
            "/ip6/2001:db8::1/tcp/2025",
            "/ip4/127.0.0.1/tcp/2025",
        ];
        for addr_str in &cases {
            let addr: Multiaddr = addr_str.parse().unwrap();
            assert!(
                !is_unspecified_addr(&addr),
                "{addr_str} should not be unspecified"
            );
        }
    }

    #[test]
    fn test_is_unspecified_addr_no_ip() {
        let addr: Multiaddr = "/memory/1234".parse().unwrap();
        assert!(!is_unspecified_addr(&addr));
    }

    #[test]
    fn test_is_circuit_addr() {
        let relay: Multiaddr = "/ip4/1.2.3.4/tcp/4001/p2p/12D3KooWDpJ7As7BWAwRMfu1VU2WCqNjvq387JEYKDBj4kx6nXTN/p2p-circuit"
            .parse()
            .unwrap();
        assert!(is_circuit_addr(&relay));

        let direct: Multiaddr = "/ip4/1.2.3.4/tcp/4001".parse().unwrap();
        assert!(!is_circuit_addr(&direct));
    }

    #[test]
    fn test_is_relay_capable() {
        let with_relay = vec![
            libp2p::StreamProtocol::new("/ipfs/kad/1.0.0"),
            libp2p::StreamProtocol::new(RELAY_HOP_PROTOCOL),
        ];
        assert!(is_relay_capable(&with_relay));

        let without_relay = vec![
            libp2p::StreamProtocol::new("/ipfs/kad/1.0.0"),
            libp2p::StreamProtocol::new("/ipfs/id/1.0.0"),
        ];
        assert!(!is_relay_capable(&without_relay));

        assert!(!is_relay_capable(&[]));
    }

    #[test]
    fn test_nat_policy_starts_unknown() {
        let policy = NatPolicyState::new();
        assert_eq!(policy.status(), NatReachability::Unknown);
    }

    #[test]
    fn test_nat_policy_promotes_after_success_threshold() {
        let mut policy = NatPolicyState::new();
        assert_eq!(policy.record_probe_result(true), None);
        let transition = policy.record_probe_result(true).expect("must transition");
        assert_eq!(
            transition,
            NatTransition {
                from: NatReachability::Unknown,
                to: NatReachability::Public
            }
        );
        assert_eq!(policy.status(), NatReachability::Public);
    }

    #[test]
    fn test_nat_policy_demotes_after_failure_threshold() {
        let mut policy = NatPolicyState::new();
        policy.record_probe_result(true);
        policy.record_probe_result(true);
        assert_eq!(policy.status(), NatReachability::Public);

        assert_eq!(policy.record_probe_result(false), None);
        let transition = policy
            .record_probe_result(false)
            .expect("must transition after threshold");
        assert_eq!(
            transition,
            NatTransition {
                from: NatReachability::Public,
                to: NatReachability::Private
            }
        );
        assert_eq!(policy.status(), NatReachability::Private);
    }

    #[test]
    fn test_nat_policy_hysteresis_prevents_flapping() {
        let mut policy = NatPolicyState::new();
        policy.record_probe_result(true);
        policy.record_probe_result(true);
        assert_eq!(policy.status(), NatReachability::Public);

        // Single failure should not demote.
        assert_eq!(policy.record_probe_result(false), None);
        assert_eq!(policy.status(), NatReachability::Public);

        // A success after one failure resets failure streak.
        assert_eq!(policy.record_probe_result(true), None);
        assert_eq!(policy.status(), NatReachability::Public);
    }

    #[test]
    fn test_actions_for_public_transition() {
        let actions = actions_for_nat_transition(NatTransition {
            from: NatReachability::Unknown,
            to: NatReachability::Public,
        });
        assert_eq!(
            actions,
            NatTransitionActions {
                set_status: NatReachability::Public,
                kad_mode: kad::Mode::Server,
                try_reserve_relay: false,
            }
        );
    }

    #[test]
    fn test_actions_for_private_transition() {
        let actions = actions_for_nat_transition(NatTransition {
            from: NatReachability::Public,
            to: NatReachability::Private,
        });
        assert_eq!(
            actions,
            NatTransitionActions {
                set_status: NatReachability::Private,
                kad_mode: kad::Mode::Client,
                try_reserve_relay: true,
            }
        );
    }

    #[test]
    fn test_clear_pending_relay_attempts_unwinds_inflight() {
        let peer: PeerId = "12D3KooWJ3qM19qUUj8JdT9kPEg6VZLoes6eexfUYd6Xn7SPrf8n"
            .parse()
            .unwrap();
        let mut inflight = 3usize;
        let mut pending = HashMap::new();
        pending.insert(peer, 2usize);

        clear_pending_relay_attempts(peer, &mut inflight, &mut pending);
        assert_eq!(inflight, 1);
        assert!(!pending.contains_key(&peer));
    }
}
