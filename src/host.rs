//! Wetware host runtime: libp2p host + Wasmtime host.
#![cfg(not(target_arch = "wasm32"))]

use std::collections::{HashMap, HashSet};
use std::net::IpAddr;
use std::num::{NonZeroU8, NonZeroUsize};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use futures::StreamExt;
use libp2p::core::connection::ConnectedPoint;
use libp2p::kad;
use libp2p::swarm::SwarmEvent;
use libp2p::{Multiaddr, PeerId, SwarmBuilder};
use tokio::sync::{mpsc, oneshot};
use wasmtime::{Config as WasmConfig, Engine};

use rpc::{NatReachability, NetworkState, PeerInfo};

// ---------------------------------------------------------------------------
// NAT traversal constants
// ---------------------------------------------------------------------------

/// Maximum number of concurrent relay reservations to maintain.
const MAX_RELAY_RESERVATIONS: usize = 2;

/// The relay v2 hop protocol advertised by peers that can serve as relays.
const RELAY_HOP_PROTOCOL: &str = "/libp2p/circuit/relay/0.2.0/hop";

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

/// Shared state for a logical `find_providers` request dispatched to both DHTs.
///
/// Both WAN and LAN queries feed providers into the same `sender`.  The `seen`
/// set deduplicates across DHTs.  `remaining` tracks how many DHT queries are
/// still active; the channel closes when it reaches 0.
struct FindRequest {
    sender: mpsc::UnboundedSender<PeerInfo>,
    seen: HashSet<PeerId>,
    remaining: u8,
}

/// Shared state for a logical `provide` request dispatched to both DHTs.
///
/// WAN is the source of truth.  We reply on first success.  If both fail,
/// reply with the WAN error.
struct ProvideRequest {
    reply: Option<oneshot::Sender<Result<(), String>>>,
    wan_done: bool,
    lan_done: bool,
    wan_err: Option<String>,
}

impl ProvideRequest {
    fn new(reply: oneshot::Sender<Result<(), String>>) -> Self {
        Self {
            reply: Some(reply),
            wan_done: false,
            lan_done: false,
            wan_err: None,
        }
    }

    /// Record a DHT result.  Returns true if the request is fully resolved.
    fn record(&mut self, source: DhtSource, result: Result<(), String>) -> bool {
        match source {
            DhtSource::Wan => self.wan_done = true,
            DhtSource::Lan => self.lan_done = true,
        }
        match result {
            Ok(()) => {
                // First success wins — reply immediately.
                if let Some(reply) = self.reply.take() {
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
    fn finalize(mut self) {
        if let Some(reply) = self.reply.take() {
            let err = self
                .wan_err
                .unwrap_or_else(|| "both DHTs failed".to_string());
            let _ = reply.send(Err(err));
        }
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
/// returned peer ID + swarm address.  Passed to [`WetwareHost::new`] so the
/// Kad client can bootstrap against the local Kubo node.
pub struct KuboBootstrapInfo {
    pub peer_id: PeerId,
    pub addr: Multiaddr,
}

pub use rpc::SwarmCommand;

/// Network behavior for Wetware hosts.
#[derive(libp2p::swarm::NetworkBehaviour)]
pub struct WetwareBehaviour {
    pub identify: libp2p::identify::Behaviour,
    pub stream: libp2p_stream::Behaviour,
    /// WAN Kademlia DHT client (Amino protocol `/ipfs/kad/1.0.0`).
    /// Runs in client mode initially.  Promoted to server when AutoNAT confirms
    /// public reachability.
    pub kad: kad::Behaviour<kad::store::MemoryStore>,
    /// LAN Kademlia DHT server (`/ipfs/lan/kad/1.0.0`).
    /// Runs in server mode.  Bootstrapped against Kubo's private/loopback peers.
    pub kad_lan: kad::Behaviour<kad::store::MemoryStore>,
    /// AutoNAT v1 client -- probes peers to determine NAT reachability.
    /// Authoritative source for NAT status (has built-in threshold logic).
    pub autonat: libp2p::autonat::v1::Behaviour,
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

/// Libp2p host wrapper for Wetware.
pub struct Libp2pHost {
    swarm: libp2p::swarm::Swarm<WetwareBehaviour>,
    local_peer_id: PeerId,
    stream_control: libp2p_stream::Control,
}

impl Libp2pHost {
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
        kad_config.set_replication_factor(NonZeroUsize::new(16).unwrap());
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
        kad_lan_config.set_replication_factor(NonZeroUsize::new(16).unwrap());
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
                Ok(WetwareBehaviour {
                    identify: libp2p::identify::Behaviour::new(identify_config),
                    stream: stream_behaviour,
                    kad: kad_wan,
                    kad_lan,
                    autonat: libp2p::autonat::v1::Behaviour::new(
                        local_peer_id,
                        libp2p::autonat::v1::Config::default(),
                    ),
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
        let mut known_peers: HashMap<PeerId, PeerInfo> = HashMap::new();
        let mut pending_connects: HashMap<PeerId, Vec<oneshot::Sender<Result<(), String>>>> =
            HashMap::new();

        // --- Dual DHT pending query maps (compound-keyed) ---

        // Logical request ID → ProvideRequest.  Both DHT queries map here.
        let mut next_request_id: u64 = 0;
        let mut pending_provides: HashMap<u64, ProvideRequest> = HashMap::new();
        // Compound (source, query_id) → logical request_id for provide.
        let mut provide_query_to_req: HashMap<DhtQueryKey, u64> = HashMap::new();

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
        let mut nat_status = NatReachability::Unknown;
        let mut active_relay_reservations: usize = 0;
        let mut inflight_relay_requests: usize = 0;
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

        // Advertise on the LAN DHT so `ww shell` can discover us via Kubo.
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
            tokio::select! {
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
                                    &mut self.swarm,
                                );
                            }
                        }
                        SwarmEvent::ConnectionEstablished {
                            peer_id,
                            endpoint,
                            ..
                        } => {
                            let addrs = match endpoint {
                                ConnectedPoint::Dialer { address, .. } => vec![address.to_vec()],
                                ConnectedPoint::Listener { send_back_addr, .. } => {
                                    vec![send_back_addr.to_vec()]
                                }
                            };
                            known_peers.insert(
                                peer_id,
                                PeerInfo {
                                    peer_id: peer_id.to_bytes(),
                                    addrs,
                                },
                            );
                            network_state
                                .set_known_peers(known_peers.values().cloned().collect())
                                .await;

                            if let Some(senders) = pending_connects.remove(&peer_id) {
                                for sender in senders {
                                    let _ = sender.send(Ok(()));
                                }
                            }
                        }
                        SwarmEvent::ConnectionClosed { peer_id, .. } => {
                            known_peers.remove(&peer_id);
                            network_state
                                .set_known_peers(known_peers.values().cloned().collect())
                                .await;
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
                        SwarmEvent::Behaviour(WetwareBehaviourEvent::Kad(
                            kad::Event::OutboundQueryProgressed { id, result, step, .. },
                        )) => {
                            handle_kad_event(
                                DhtSource::Wan, id, result, &step,
                                &mut self.swarm,
                                &mut pending_provides, &mut provide_query_to_req,
                                &mut pending_finds, &mut find_query_to_req,
                                &mut peer_addr_book, &mut pending_peer_routing,
                                &mut routed_peers,
                            );
                            if step.last {
                                cleanup_query(
                                    DhtSource::Wan, id,
                                    &mut provide_query_to_req, &mut pending_provides,
                                    &mut find_query_to_req, &mut pending_finds,
                                    &mut pending_peer_routing, &mut routed_peers,
                                );
                            }
                        }
                        SwarmEvent::Behaviour(WetwareBehaviourEvent::Kad(ref ev)) => {
                            tracing::debug!("WAN Kad event: {ev:?}");
                        }
                        // LAN Kad events
                        SwarmEvent::Behaviour(WetwareBehaviourEvent::KadLan(
                            kad::Event::OutboundQueryProgressed { id, result, step, .. },
                        )) => {
                            handle_kad_event(
                                DhtSource::Lan, id, result, &step,
                                &mut self.swarm,
                                &mut pending_provides, &mut provide_query_to_req,
                                &mut pending_finds, &mut find_query_to_req,
                                &mut peer_addr_book, &mut pending_peer_routing,
                                &mut routed_peers,
                            );
                            if step.last {
                                cleanup_query(
                                    DhtSource::Lan, id,
                                    &mut provide_query_to_req, &mut pending_provides,
                                    &mut find_query_to_req, &mut pending_finds,
                                    &mut pending_peer_routing, &mut routed_peers,
                                );
                            }
                        }
                        SwarmEvent::Behaviour(WetwareBehaviourEvent::KadLan(ref ev)) => {
                            tracing::debug!("LAN Kad event: {ev:?}");
                        }
                        // --- Identify: relay discovery ---
                        SwarmEvent::Behaviour(WetwareBehaviourEvent::Identify(
                            libp2p::identify::Event::Received { peer_id, info, .. },
                        )) => {
                            handle_identify_received(
                                peer_id, &info,
                                nat_status,
                                &mut active_relay_reservations,
                                &mut inflight_relay_requests,
                                &mut relay_candidates,
                                &mut seen_relay_peers,
                                &mut self.swarm,
                            );
                        }
                        SwarmEvent::Behaviour(WetwareBehaviourEvent::Identify(_)) => {}
                        // --- AutoNAT v1: authoritative NAT status ---
                        SwarmEvent::Behaviour(WetwareBehaviourEvent::Autonat(
                            libp2p::autonat::v1::Event::StatusChanged { old, new },
                        )) => {
                            handle_autonat_v1_status(
                                &old, &new,
                                &mut nat_status,
                                &mut self.swarm,
                                &network_state,
                                &mut active_relay_reservations,
                                &mut inflight_relay_requests,
                                &mut relay_candidates,
                            ).await;
                        }
                        SwarmEvent::Behaviour(WetwareBehaviourEvent::Autonat(_)) => {}
                        // --- AutoNAT v2: supplementary probes ---
                        SwarmEvent::Behaviour(WetwareBehaviourEvent::AutonatV2(ev)) => {
                            tracing::debug!("AutoNAT v2 probe: {ev:?}");
                        }
                        // --- Relay client ---
                        SwarmEvent::Behaviour(WetwareBehaviourEvent::RelayClient(
                            libp2p::relay::client::Event::ReservationReqAccepted {
                                relay_peer_id,
                                renewal,
                                ..
                            },
                        )) => {
                            inflight_relay_requests =
                                inflight_relay_requests.saturating_sub(1);
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
                        SwarmEvent::Behaviour(WetwareBehaviourEvent::RelayClient(ev)) => {
                            tracing::debug!("Relay client event: {ev:?}");
                        }
                        // --- DCUtR: hole-punch results ---
                        SwarmEvent::Behaviour(WetwareBehaviourEvent::Dcutr(ev)) => {
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
                        SwarmEvent::Behaviour(WetwareBehaviourEvent::Stream(_)) => {}
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
                            for addr in &addrs {
                                self.swarm.add_peer_address(peer_id, addr.clone());
                            }
                            match self.swarm.dial(peer_id) {
                                Ok(()) => {
                                    pending_connects.entry(peer_id).or_default().push(reply);
                                }
                                Err(e) => {
                                    let _ = reply.send(Err(e.to_string()));
                                }
                            }
                        }
                        Some(SwarmCommand::KadProvide { key, reply }) => {
                            let req_id = next_request_id;
                            next_request_id += 1;

                            let record_key = kad::RecordKey::new(&key);
                            let beh = self.swarm.behaviour_mut();

                            let mut req = ProvideRequest::new(reply);

                            // WAN provide
                            match beh.kad.start_providing(record_key.clone()) {
                                Ok(qid) => {
                                    provide_query_to_req.insert((DhtSource::Wan, qid), req_id);
                                }
                                Err(e) => {
                                    tracing::warn!("WAN provide failed to start: {e:?}");
                                    req.record(DhtSource::Wan, Err(format!("{e:?}")));
                                }
                            }

                            // LAN provide (fire-and-forget semantics, but tracked)
                            match beh.kad_lan.start_providing(record_key) {
                                Ok(qid) => {
                                    provide_query_to_req.insert((DhtSource::Lan, qid), req_id);
                                }
                                Err(e) => {
                                    tracing::debug!("LAN provide failed to start: {e:?}");
                                    req.record(DhtSource::Lan, Err(format!("{e:?}")));
                                }
                            }

                            if req.wan_done && req.lan_done {
                                req.finalize();
                            } else {
                                pending_provides.insert(req_id, req);
                            }
                        }
                        Some(SwarmCommand::KadFindProviders { key, reply }) => {
                            let req_id = next_request_id;
                            next_request_id += 1;

                            let record_key = kad::RecordKey::new(&key);
                            let beh = self.swarm.behaviour_mut();

                            let mut remaining = 0u8;

                            // WAN query
                            let wan_qid = beh.kad.get_providers(record_key.clone());
                            find_query_to_req.insert((DhtSource::Wan, wan_qid), req_id);
                            remaining += 1;

                            // LAN query
                            let lan_qid = beh.kad_lan.get_providers(record_key);
                            find_query_to_req.insert((DhtSource::Lan, lan_qid), req_id);
                            remaining += 1;

                            pending_finds.insert(req_id, FindRequest {
                                sender: reply,
                                seen: HashSet::new(),
                                remaining,
                            });
                            routed_peers.insert(req_id, HashSet::new());
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

#[allow(clippy::too_many_arguments)]
fn handle_kad_event(
    source: DhtSource,
    id: kad::QueryId,
    result: kad::QueryResult,
    step: &kad::ProgressStep,
    swarm: &mut libp2p::swarm::Swarm<WetwareBehaviour>,
    pending_provides: &mut HashMap<u64, ProvideRequest>,
    provide_query_to_req: &mut HashMap<DhtQueryKey, u64>,
    pending_finds: &mut HashMap<u64, FindRequest>,
    find_query_to_req: &mut HashMap<DhtQueryKey, u64>,
    peer_addr_book: &mut HashMap<PeerId, Vec<Multiaddr>>,
    pending_peer_routing: &mut HashMap<DhtQueryKey, (PeerId, Option<u64>)>,
    routed_peers: &mut HashMap<u64, HashSet<PeerId>>,
) {
    let key = (source, id);
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
                            req.finalize();
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
                            req.finalize();
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
                    let routed = routed_peers.entry(req_id).or_default();
                    for provider in &providers {
                        if !find_req.seen.insert(*provider) {
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
                            let _ = find_req.sender.send(PeerInfo {
                                peer_id: provider.to_bytes(),
                                addrs: addrs.iter().map(|a| a.to_vec()).collect(),
                            });
                        } else if !routed.contains(provider)
                            && !pending_peer_routing.values().any(|(p, _)| p == provider)
                        {
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
                        if let Some(find_req) = pending_finds.get(&req_id) {
                            if !info.addrs.is_empty() {
                                let _ = find_req.sender.send(PeerInfo {
                                    peer_id: target.to_bytes(),
                                    addrs: info.addrs.iter().map(|a| a.to_vec()).collect(),
                                });
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
            }
        }
        _ => {
            tracing::debug!(dht = label, "Kad query progress (other): {result:?}");
        }
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
/// For find_providers, decrement `remaining` and close the channel when both
/// DHT queries are done.
#[allow(clippy::too_many_arguments)]
fn cleanup_query(
    source: DhtSource,
    id: kad::QueryId,
    provide_query_to_req: &mut HashMap<DhtQueryKey, u64>,
    pending_provides: &mut HashMap<u64, ProvideRequest>,
    find_query_to_req: &mut HashMap<DhtQueryKey, u64>,
    pending_finds: &mut HashMap<u64, FindRequest>,
    pending_peer_routing: &mut HashMap<DhtQueryKey, (PeerId, Option<u64>)>,
    routed_peers: &mut HashMap<u64, HashSet<PeerId>>,
) {
    let key = (source, id);

    // Provide cleanup
    if let Some(req_id) = provide_query_to_req.remove(&key) {
        // If no more queries reference this request, finalize it.
        if !provide_query_to_req.values().any(|&r| r == req_id) {
            if let Some(req) = pending_provides.remove(&req_id) {
                req.finalize();
            }
        }
    }

    // FindProviders cleanup
    if let Some(req_id) = find_query_to_req.remove(&key) {
        if let Some(find_req) = pending_finds.get_mut(&req_id) {
            find_req.remaining = find_req.remaining.saturating_sub(1);
            if find_req.remaining == 0 {
                // Both DHTs done — drop the sender to close the channel.
                pending_finds.remove(&req_id);
                routed_peers.remove(&req_id);
            }
        }
    }

    // Peer routing cleanup (compound key removes the specific entry).
    pending_peer_routing.remove(&key);
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
    relay_candidates: &mut Vec<(PeerId, Multiaddr)>,
    seen_relay_peers: &mut HashSet<PeerId>,
    swarm: &mut libp2p::swarm::Swarm<WetwareBehaviour>,
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
        tracing::info!(
            relay = %peer_id,
            addr = %circuit_addr,
            "Requesting relay reservation"
        );
        if let Err(e) = swarm.listen_on(circuit_addr.clone()) {
            tracing::warn!(
                relay = %peer_id,
                error = %e,
                "Failed to request relay reservation"
            );
        } else {
            *inflight_relay_requests += 1;
        }
    } else {
        // Save for later in case a reservation expires.
        relay_candidates.push((peer_id, circuit_addr));
    }
}

/// Handle AutoNAT v1 status change.
#[allow(clippy::too_many_arguments)]
async fn handle_autonat_v1_status(
    _old: &libp2p::autonat::v1::NatStatus,
    new: &libp2p::autonat::v1::NatStatus,
    nat_status: &mut NatReachability,
    swarm: &mut libp2p::swarm::Swarm<WetwareBehaviour>,
    network_state: &NetworkState,
    active_relay_reservations: &mut usize,
    inflight_relay_requests: &mut usize,
    relay_candidates: &mut Vec<(PeerId, Multiaddr)>,
) {
    match new {
        libp2p::autonat::v1::NatStatus::Public(addr) => {
            if *nat_status != NatReachability::Public {
                tracing::info!(%addr, "AutoNAT: confirmed public reachability");
                *nat_status = NatReachability::Public;
                network_state.set_nat_status(NatReachability::Public).await;
                // Promote WAN Kad to server mode — we can serve DHT queries.
                swarm.behaviour_mut().kad.set_mode(Some(kad::Mode::Server));
                tracing::info!("WAN Kad promoted to server mode");
            }
        }
        libp2p::autonat::v1::NatStatus::Private => {
            if *nat_status != NatReachability::Private {
                let was_public = *nat_status == NatReachability::Public;
                tracing::info!(was_public, "AutoNAT: node is behind NAT, seeking relay");
                *nat_status = NatReachability::Private;
                network_state.set_nat_status(NatReachability::Private).await;
                // Demote Kad back to client mode if we were previously public.
                if was_public {
                    swarm.behaviour_mut().kad.set_mode(Some(kad::Mode::Client));
                    tracing::info!("WAN Kad demoted to client mode");
                }
                // Try to reserve from any candidates we've already seen.
                try_reserve_relay(
                    relay_candidates,
                    active_relay_reservations,
                    inflight_relay_requests,
                    swarm,
                );
            }
        }
        libp2p::autonat::v1::NatStatus::Unknown => {
            tracing::debug!("AutoNAT: status reset to Unknown");
        }
    }
}

/// Try to reserve relay slots from accumulated candidates.
fn try_reserve_relay(
    relay_candidates: &mut Vec<(PeerId, Multiaddr)>,
    active_relay_reservations: &mut usize,
    inflight_relay_requests: &mut usize,
    swarm: &mut libp2p::swarm::Swarm<WetwareBehaviour>,
) {
    while *active_relay_reservations + *inflight_relay_requests < MAX_RELAY_RESERVATIONS {
        let Some((peer_id, circuit_addr)) = relay_candidates.pop() else {
            break;
        };
        tracing::info!(
            relay = %peer_id,
            addr = %circuit_addr,
            "Requesting relay reservation (from candidate pool)"
        );
        if let Err(e) = swarm.listen_on(circuit_addr) {
            tracing::warn!(
                relay = %peer_id,
                error = %e,
                "Failed to request relay reservation"
            );
        } else {
            *inflight_relay_requests += 1;
        }
    }
}

/// Shared Wasmtime runtime state for Wetware hosts.
pub struct WasmtimeHost {
    engine: Arc<Engine>,
}

impl WasmtimeHost {
    pub fn new() -> Result<Self> {
        // No epoch_interruption here — WasmtimeHost is used by integration
        // tests, not by the ExecutorPool which has its own Engine + tick task.
        let mut config = WasmConfig::new();
        config.consume_fuel(true);
        let engine = Engine::new(&config)?;
        Ok(Self {
            engine: Arc::new(engine),
        })
    }

    pub fn engine(&self) -> Arc<Engine> {
        Arc::clone(&self.engine)
    }
}

/// Wetware host combines libp2p and Wasmtime runtimes.
pub struct WetwareHost {
    libp2p: Libp2pHost,
    wasmtime: WasmtimeHost,
    network_state: NetworkState,
    swarm_cmd_tx: mpsc::Sender<SwarmCommand>,
    swarm_cmd_rx: Option<mpsc::Receiver<SwarmCommand>>,
}

impl WetwareHost {
    pub fn new(
        listen: Vec<Multiaddr>,
        keypair: libp2p::identity::Keypair,
        kubo_bootstrap: Option<KuboBootstrapInfo>,
        kubo_peers: Vec<(PeerId, Multiaddr)>,
    ) -> Result<Self> {
        let libp2p = Libp2pHost::new(listen, keypair, kubo_bootstrap, kubo_peers)?;
        let wasmtime = WasmtimeHost::new()?;
        let network_state = NetworkState::from_peer_id(libp2p.local_peer_id().to_bytes());
        let (swarm_cmd_tx, swarm_cmd_rx) = mpsc::channel(64);
        Ok(Self {
            libp2p,
            wasmtime,
            network_state,
            swarm_cmd_tx,
            swarm_cmd_rx: Some(swarm_cmd_rx),
        })
    }

    pub fn network_state(&self) -> NetworkState {
        self.network_state.clone()
    }

    pub fn swarm_cmd_tx(&self) -> mpsc::Sender<SwarmCommand> {
        self.swarm_cmd_tx.clone()
    }

    pub fn wasmtime_engine(&self) -> Arc<Engine> {
        self.wasmtime.engine()
    }

    pub fn stream_control(&self) -> libp2p_stream::Control {
        self.libp2p.stream_control()
    }

    pub async fn run(mut self) -> Result<()> {
        let cmd_rx = self
            .swarm_cmd_rx
            .take()
            .expect("run() called more than once");
        self.libp2p.run(self.network_state, cmd_rx).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use tokio::sync::oneshot;

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
        let mut req = ProvideRequest::new(tx);

        // WAN succeeds first
        assert!(!req.record(DhtSource::Wan, Ok(())));
        // Reply already sent
        assert!(req.reply.is_none());
        // LAN result comes later
        assert!(req.record(DhtSource::Lan, Err("no peers".into())));

        // The receiver got Ok
        assert!(rx.try_recv().unwrap().is_ok());
    }

    #[test]
    fn test_provide_request_both_fail_sends_wan_error() {
        let (tx, mut rx) = oneshot::channel();
        let mut req = ProvideRequest::new(tx);

        assert!(!req.record(DhtSource::Lan, Err("lan fail".into())));
        assert!(req.record(DhtSource::Wan, Err("wan fail".into())));
        req.finalize();

        let result = rx.try_recv().unwrap();
        assert!(result.is_err());
        assert_eq!(result.unwrap_err(), "wan fail");
    }

    // -------------------------------------------------------------------
    // FindRequest dedup
    // -------------------------------------------------------------------

    #[test]
    fn test_find_request_dedup_across_dhts() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let mut find = FindRequest {
            sender: tx,
            seen: HashSet::new(),
            remaining: 2,
        };

        let peer_bytes = vec![0u8; 32]; // dummy peer ID bytes

        // First insert succeeds
        let peer_id: PeerId = PeerId::random();
        assert!(find.seen.insert(peer_id));

        // Same peer from other DHT is a duplicate
        assert!(!find.seen.insert(peer_id));

        // Send one provider through
        let _ = find.sender.send(PeerInfo {
            peer_id: peer_bytes.clone(),
            addrs: vec![],
        });

        assert!(rx.try_recv().is_ok());

        // Decrement remaining
        find.remaining -= 1;
        assert_eq!(find.remaining, 1);
        find.remaining -= 1;
        assert_eq!(find.remaining, 0);
    }

    // -------------------------------------------------------------------
    // cleanup_query
    // -------------------------------------------------------------------

    #[test]
    fn test_cleanup_find_providers_closes_on_both_done() {
        let (tx, mut rx) = mpsc::unbounded_channel::<PeerInfo>();

        let mut pending_finds: HashMap<u64, FindRequest> = HashMap::new();
        let mut find_query_to_req: HashMap<DhtQueryKey, u64> = HashMap::new();
        let mut provide_query_to_req: HashMap<DhtQueryKey, u64> = HashMap::new();
        let mut pending_provides: HashMap<u64, ProvideRequest> = HashMap::new();
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
                seen: HashSet::new(),
                remaining: 2,
            },
        );
        routed.insert(req_id, HashSet::new());

        // WAN finishes first — channel should stay open.
        cleanup_query(
            DhtSource::Wan,
            wan_qid,
            &mut provide_query_to_req,
            &mut pending_provides,
            &mut find_query_to_req,
            &mut pending_finds,
            &mut pending_peer_routing,
            &mut routed,
        );
        assert!(pending_finds.contains_key(&req_id));
        assert!(rx.try_recv().is_err()); // not closed yet

        // LAN finishes — channel should close.
        cleanup_query(
            DhtSource::Lan,
            lan_qid,
            &mut provide_query_to_req,
            &mut pending_provides,
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
}

// ---------------------------------------------------------------------------
// Client-mode swarm — minimal libp2p for dialing only (ww shell)
// ---------------------------------------------------------------------------

/// Minimal network behaviour for client-only operation.
/// Identify for peer info exchange + libp2p_stream for VatClient dialing.
/// Relay client for connecting to NATted nodes via relayed addresses.
/// No Kademlia and no listeners.
#[derive(libp2p::swarm::NetworkBehaviour)]
pub struct ClientBehaviour {
    identify: libp2p::identify::Behaviour,
    stream: libp2p_stream::Behaviour,
    relay_client: libp2p::relay::client::Behaviour,
}

/// A lightweight libp2p swarm for dialing peers and consuming vat services.
/// Used by `ww shell` to connect to a running node without booting a full host.
pub struct ClientSwarm {
    swarm: libp2p::swarm::Swarm<ClientBehaviour>,
    local_peer_id: PeerId,
    stream_control: libp2p_stream::Control,
}

impl ClientSwarm {
    /// Build a client-mode swarm with TCP + QUIC + Relay (same stack as host).
    /// No listeners are registered — this swarm only dials outgoing connections.
    /// Relay transport enables dialing NATted nodes via their relayed addresses.
    pub fn new(keypair: libp2p::identity::Keypair) -> Result<Self> {
        let peer_id = keypair.public().to_peer_id();

        let stream_behaviour = libp2p_stream::Behaviour::new();
        let stream_control = stream_behaviour.new_control();

        let identify_config =
            libp2p::identify::Config::new("wetware-shell/0.1.0".to_string(), keypair.public());

        let swarm = SwarmBuilder::with_existing_identity(keypair)
            .with_tokio()
            .with_tcp(
                Default::default(),
                libp2p::noise::Config::new,
                libp2p::yamux::Config::default,
            )?
            .with_quic()
            .with_relay_client(libp2p::noise::Config::new, libp2p::yamux::Config::default)?
            .with_behaviour(|_keypair, relay_client| {
                Ok(ClientBehaviour {
                    identify: libp2p::identify::Behaviour::new(identify_config),
                    stream: stream_behaviour,
                    relay_client,
                })
            })?
            .with_swarm_config(|c: libp2p::swarm::Config| {
                c.with_idle_connection_timeout(Duration::from_secs(60))
            })
            .build();

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

    /// Add a known address for a peer and initiate dialing.
    pub fn add_peer_addr(&mut self, peer_id: PeerId, addr: Multiaddr) {
        self.swarm.add_peer_address(peer_id, addr.clone());
        // Proactively dial so the connection is established before open_stream.
        if let Err(e) = self
            .swarm
            .dial(addr.with(libp2p::multiaddr::Protocol::P2p(peer_id)))
        {
            tracing::warn!(peer = %peer_id, "Failed to initiate dial: {e}");
        }
    }

    /// Dial a multiaddr directly.
    ///
    /// Use this when the peer ID is not yet known.
    pub fn dial(&mut self, addr: Multiaddr) -> Result<(), libp2p::swarm::DialError> {
        self.swarm.dial(addr)
    }

    /// Drive the swarm event loop. Spawn this as a background task.
    ///
    /// If `connected_tx` is provided, the peer ID of the first established
    /// connection is sent through it (useful when the
    /// peer ID is not known upfront).
    pub async fn run(
        mut self,
        connected_tx: Option<tokio::sync::oneshot::Sender<Result<PeerId, String>>>,
        discovered_tx: Option<tokio::sync::mpsc::UnboundedSender<(PeerId, Multiaddr)>>,
    ) {
        use libp2p::swarm::SwarmEvent;
        let mut connected_tx = connected_tx;
        let _discovered_tx = discovered_tx;
        loop {
            match self.swarm.next().await {
                Some(SwarmEvent::ConnectionEstablished { peer_id, .. }) => {
                    tracing::debug!(peer = %peer_id, "Client connection established");
                    if let Some(tx) = connected_tx.take() {
                        let _ = tx.send(Ok(peer_id));
                    }
                }
                Some(SwarmEvent::ConnectionClosed { peer_id, .. }) => {
                    tracing::debug!(peer = %peer_id, "Client connection closed");
                }
                Some(SwarmEvent::OutgoingConnectionError { peer_id, error, .. }) => {
                    tracing::warn!(
                        peer = ?peer_id,
                        "Client outgoing connection error: {error}"
                    );
                    if let Some(tx) = connected_tx.take() {
                        let _ = tx.send(Err(format!("{error}")));
                    }
                }
                Some(_) => {} // Ignore other events
                None => break,
            }
        }
    }
}
