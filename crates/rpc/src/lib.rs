//! Cap'n Proto RPC for host-provided capabilities.
//!
//! The Host capability is served to each WASM guest over in-memory duplex
//! streams (no TCP listener). See [`build_peer_rpc`] for the entry point.
#![cfg(not(target_arch = "wasm32"))]

pub mod connection_budget;
pub mod dispatch;
pub mod graft;
pub mod http_client;
pub mod http_listener;
pub mod keys;
pub mod routing;
pub mod stream_dialer;
pub mod stream_listener;
pub mod vat_client;
pub mod vat_dial;
pub mod vat_listener;

pub use connection_budget::{
    ConnectionBudget, ConnectionLimitReached, ConnectionPermit, InvalidConnectionLimit,
    DEFAULT_MAX_INBOUND_CONNECTIONS,
};
pub mod wagi;

use std::sync::Arc;

use capnp::capability::Promise;
use capnp_rpc::pry;
use capnp_rpc::rpc_twoparty_capnp::Side;
use capnp_rpc::twoparty::VatNetwork;
use capnp_rpc::RpcSystem;
use tokio::io::{self, AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::sync::{mpsc, Mutex, Notify, RwLock};
use tokio_util::compat::{TokioAsyncReadCompatExt, TokioAsyncWriteCompatExt};

use authority::EpochGuard;

use libp2p::{Multiaddr, PeerId, StreamProtocol};
use tokio::sync::oneshot;

use authority::system_capnp;

/// Commands sent from vat cells to the swarm event loop.
pub enum SwarmCommand {
    Connect {
        peer_id: PeerId,
        addrs: Vec<Multiaddr>,
        reply: oneshot::Sender<Result<(), String>>,
    },
    /// Announce this Wetware node as a provider for the given DHT key
    /// (multihash bytes of a CID) on the Amino Kademlia DHT.
    KadProvide {
        key: Vec<u8>,
        reply: oneshot::Sender<Result<(), String>>,
    },
    /// Find providers for the given DHT key (multihash bytes of a CID).
    ///
    /// Providers are sent over the unbounded channel as they are discovered.
    /// The channel is closed when the query completes.
    KadFindProviders {
        key: Vec<u8>,
        reply: mpsc::UnboundedSender<PeerInfo>,
    },
}

fn validate_service_protocol_name(protocol: &str) -> Result<(), capnp::Error> {
    if protocol.is_empty() {
        return Err(capnp::Error::failed(
            "protocol name must not be empty".into(),
        ));
    }
    if protocol.contains('/') {
        return Err(capnp::Error::failed(
            "protocol name must not contain '/'".into(),
        ));
    }
    Ok(())
}

/// Build the libp2p protocol for a vat service-name locator.
///
/// The protocol string is not authority; it only selects the remote service.
pub fn vat_protocol(protocol: &str) -> Result<StreamProtocol, capnp::Error> {
    validate_service_protocol_name(protocol)?;
    StreamProtocol::try_from_owned(format!("/ww/0.1.0/vat/{protocol}"))
        .map_err(|e| capnp::Error::failed(format!("invalid vat protocol: {e}")))
}

/// Build the libp2p protocol for a byte-stream service-name locator.
pub fn stream_protocol(protocol: &str) -> Result<StreamProtocol, capnp::Error> {
    validate_service_protocol_name(protocol)?;
    StreamProtocol::try_from_owned(format!("/ww/0.1.0/stream/{protocol}"))
        .map_err(|e| capnp::Error::failed(format!("invalid stream protocol: {e}")))
}

/// Re-canonicalize a `Schema.Node` reader into raw single-segment bytes.
///
/// Mirrors `crates/schema-id::canonicalize_node` and the build-time
/// emission path so the bytes match what `crates/authority`'s
/// `schema_registry` exposes for core caps. Returns `None` if the message
/// produces an unexpected (non-single) segment count, which would indicate
/// a malformed input.
pub fn canonicalize_schema_node(node: capnp::schema_capnp::node::Reader<'_>) -> Option<Vec<u8>> {
    let mut msg = capnp::message::Builder::new_default();
    msg.set_root_canonical(node).ok()?;
    let segments = msg.get_segments_for_output();
    if segments.len() != 1 {
        return None;
    }
    Some(segments[0].to_vec())
}

/// Maximum bytes a single ByteStream read may allocate.
///
/// Guards against OOM from callers requesting u32::MAX bytes.
/// 64 KiB matches the RPC pipe buffer and the listener pump size.
const MAX_READ_BYTES: usize = 64 * 1024;

#[derive(Clone, Debug)]
pub struct PeerInfo {
    pub peer_id: Vec<u8>,
    pub addrs: Vec<Vec<u8>>,
}

/// Maximum number of recent AutoNAT v2 probe outcomes retained in memory.
pub const MAX_NAT_PROBE_EVENTS: usize = 64;

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
pub struct NatProbeEvent {
    pub tested_addr: String,
    pub server_peer_id: String,
    pub success: bool,
    pub timestamp_unix_ms: u64,
}

#[derive(Clone, Debug)]
pub struct NetworkSnapshot {
    pub local_peer_id: Vec<u8>,
    pub listen_addrs: Vec<Vec<u8>>,
    pub known_peers: Vec<PeerInfo>,
    pub nat_status: NatReachability,
    pub nat_probe_events: Vec<NatProbeEvent>,
}

/// NAT reachability status as determined by AutoNAT.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum NatReachability {
    Unknown,
    Public,
    Private,
}

#[derive(Clone, Debug)]
pub struct NetworkState {
    inner: Arc<RwLock<NetworkSnapshot>>,
    listen_addr_notify: Arc<Notify>,
}

impl Default for NetworkState {
    fn default() -> Self {
        Self::new()
    }
}

impl NetworkState {
    pub fn new() -> Self {
        use libp2p::identity::Keypair;
        use libp2p::PeerId;

        let keypair = Keypair::generate_ed25519();
        let peer_id = PeerId::from_public_key(&keypair.public());
        Self::from_peer_id(peer_id.to_bytes())
    }

    pub fn from_peer_id(peer_id: Vec<u8>) -> Self {
        let snapshot = NetworkSnapshot {
            local_peer_id: peer_id,
            listen_addrs: Vec::new(),
            known_peers: Vec::new(),
            nat_status: NatReachability::Unknown,
            nat_probe_events: Vec::new(),
        };
        Self {
            inner: Arc::new(RwLock::new(snapshot)),
            listen_addr_notify: Arc::new(Notify::new()),
        }
    }

    pub async fn snapshot(&self) -> NetworkSnapshot {
        self.inner.read().await.clone()
    }

    pub async fn set_local_peer_id(&self, peer_id: Vec<u8>) {
        let mut guard = self.inner.write().await;
        guard.local_peer_id = peer_id;
    }

    pub async fn add_listen_addr(&self, addr: Vec<u8>) {
        let mut guard = self.inner.write().await;
        if !guard.listen_addrs.contains(&addr) {
            guard.listen_addrs.push(addr);
            drop(guard);
            self.listen_addr_notify.notify_waiters();
        }
    }

    /// Wait until the host publishes its first bound listen address.
    ///
    /// Callers should own the deadline around this future. Registering the
    /// notification before checking state avoids missing an address event
    /// between the read and the await.
    pub async fn wait_for_listen_addr(&self) -> Vec<u8> {
        loop {
            let notified = self.listen_addr_notify.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            if let Some(addr) = self.inner.read().await.listen_addrs.first().cloned() {
                return addr;
            }
            notified.await;
        }
    }

    pub async fn remove_listen_addr(&self, addr: &[u8]) {
        let mut guard = self.inner.write().await;
        guard.listen_addrs.retain(|a| a != addr);
    }

    pub async fn set_known_peers(&self, peers: Vec<PeerInfo>) {
        let mut guard = self.inner.write().await;
        guard.known_peers = peers;
    }

    pub async fn set_nat_status(&self, status: NatReachability) {
        let mut guard = self.inner.write().await;
        guard.nat_status = status;
    }

    pub async fn nat_status(&self) -> NatReachability {
        self.inner.read().await.nat_status
    }

    pub async fn record_nat_probe_event(&self, event: NatProbeEvent) {
        let mut guard = self.inner.write().await;
        guard.nat_probe_events.push(event);
        if guard.nat_probe_events.len() > MAX_NAT_PROBE_EVENTS {
            let overflow = guard.nat_probe_events.len() - MAX_NAT_PROBE_EVENTS;
            guard.nat_probe_events.drain(0..overflow);
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub enum StreamMode {
    ReadOnly,
    WriteOnly,
    Bidirectional,
}

pub struct ByteStreamImpl {
    stream: Arc<Mutex<io::DuplexStream>>,
    mode: StreamMode,
}

impl ByteStreamImpl {
    pub fn new(stream: io::DuplexStream, mode: StreamMode) -> Self {
        Self {
            stream: Arc::new(Mutex::new(stream)),
            mode,
        }
    }

    async fn with_stream<'a>(
        stream: &'a Arc<Mutex<io::DuplexStream>>,
    ) -> tokio::sync::MutexGuard<'a, io::DuplexStream> {
        stream.lock().await
    }
}

impl system_capnp::byte_stream::Server for ByteStreamImpl {
    fn read(
        self: capnp::capability::Rc<Self>,
        params: system_capnp::byte_stream::ReadParams,
        mut results: system_capnp::byte_stream::ReadResults,
    ) -> impl std::future::Future<Output = Result<(), capnp::Error>> + 'static {
        if matches!(self.mode, StreamMode::WriteOnly) {
            return Promise::from_future(async {
                Err(capnp::Error::failed("stream is write-only".into()))
            });
        }
        // ReadOnly and Bidirectional both allow read

        let max_bytes = (pry!(params.get()).get_max_bytes() as usize).min(MAX_READ_BYTES);
        let stream = self.stream.clone();
        Promise::from_future(async move {
            if max_bytes == 0 {
                results.get().set_data(&[]);
                return Ok(());
            }
            let mut buffer = vec![0u8; max_bytes];
            let mut locked = ByteStreamImpl::with_stream(&stream).await;
            let read = locked
                .read(&mut buffer)
                .await
                .map_err(|err| capnp::Error::failed(err.to_string()))?;
            buffer.truncate(read);
            results.get().set_data(&buffer);
            Ok(())
        })
    }

    fn write(
        self: capnp::capability::Rc<Self>,
        params: system_capnp::byte_stream::WriteParams,
        _results: system_capnp::byte_stream::WriteResults,
    ) -> impl std::future::Future<Output = Result<(), capnp::Error>> + 'static {
        if matches!(self.mode, StreamMode::ReadOnly) {
            return Promise::from_future(async {
                Err(capnp::Error::failed("stream is read-only".into()))
            });
        }
        // WriteOnly and Bidirectional both allow write

        let data = pry!(params.get()).get_data().unwrap_or(&[]).to_vec();
        let stream = self.stream.clone();
        Promise::from_future(async move {
            let mut locked = ByteStreamImpl::with_stream(&stream).await;
            if !data.is_empty() {
                locked
                    .write_all(&data)
                    .await
                    .map_err(|err| capnp::Error::failed(err.to_string()))?;
                locked
                    .flush()
                    .await
                    .map_err(|err| capnp::Error::failed(err.to_string()))?;
            }
            Ok(())
        })
    }

    fn close(
        self: capnp::capability::Rc<Self>,
        _params: system_capnp::byte_stream::CloseParams,
        _results: system_capnp::byte_stream::CloseResults,
    ) -> impl std::future::Future<Output = Result<(), capnp::Error>> + 'static {
        let stream = self.stream.clone();
        Promise::from_future(async move {
            let mut locked = ByteStreamImpl::with_stream(&stream).await;
            let _ = locked.shutdown().await;
            Ok(())
        })
    }
}

pub struct ProcessImpl {
    stdin: system_capnp::byte_stream::Client,
    stdout: system_capnp::byte_stream::Client,
    stderr: system_capnp::byte_stream::Client,
    exit_rx: Arc<Mutex<Option<tokio::sync::oneshot::Receiver<i32>>>>,
    bootstrap_cap: Option<capnp::capability::Client>,
    kill_tx: Arc<tokio::sync::watch::Sender<bool>>,
}

impl ProcessImpl {
    pub fn new(
        stdin: system_capnp::byte_stream::Client,
        stdout: system_capnp::byte_stream::Client,
        stderr: system_capnp::byte_stream::Client,
        exit_rx: tokio::sync::oneshot::Receiver<i32>,
        kill_tx: tokio::sync::watch::Sender<bool>,
    ) -> Self {
        Self {
            stdin,
            stdout,
            stderr,
            exit_rx: Arc::new(Mutex::new(Some(exit_rx))),
            bootstrap_cap: None,
            kill_tx: Arc::new(kill_tx),
        }
    }

    pub fn with_bootstrap(
        stdin: system_capnp::byte_stream::Client,
        stdout: system_capnp::byte_stream::Client,
        stderr: system_capnp::byte_stream::Client,
        exit_rx: tokio::sync::oneshot::Receiver<i32>,
        bootstrap_cap: capnp::capability::Client,
        kill_tx: tokio::sync::watch::Sender<bool>,
    ) -> Self {
        Self {
            stdin,
            stdout,
            stderr,
            exit_rx: Arc::new(Mutex::new(Some(exit_rx))),
            bootstrap_cap: Some(bootstrap_cap),
            kill_tx: Arc::new(kill_tx),
        }
    }
}

impl system_capnp::process::Server for ProcessImpl {
    fn stdin(
        self: capnp::capability::Rc<Self>,
        _params: system_capnp::process::StdinParams,
        mut results: system_capnp::process::StdinResults,
    ) -> impl std::future::Future<Output = Result<(), capnp::Error>> + 'static {
        results.get().set_stream(self.stdin.clone());
        Promise::ok(())
    }

    fn stdout(
        self: capnp::capability::Rc<Self>,
        _params: system_capnp::process::StdoutParams,
        mut results: system_capnp::process::StdoutResults,
    ) -> impl std::future::Future<Output = Result<(), capnp::Error>> + 'static {
        results.get().set_stream(self.stdout.clone());
        Promise::ok(())
    }

    fn stderr(
        self: capnp::capability::Rc<Self>,
        _params: system_capnp::process::StderrParams,
        mut results: system_capnp::process::StderrResults,
    ) -> impl std::future::Future<Output = Result<(), capnp::Error>> + 'static {
        results.get().set_stream(self.stderr.clone());
        Promise::ok(())
    }

    fn wait(
        self: capnp::capability::Rc<Self>,
        _params: system_capnp::process::WaitParams,
        mut results: system_capnp::process::WaitResults,
    ) -> impl std::future::Future<Output = Result<(), capnp::Error>> + 'static {
        let exit_rx = Arc::clone(&self.exit_rx);
        Promise::from_future(async move {
            let mut guard = exit_rx.lock().await;
            let rx = guard.take().ok_or_else(|| {
                capnp::Error::failed("wait() already called for this process".into())
            })?;
            let code = rx.await.unwrap_or(1);
            results.get().set_exit_code(code);
            Ok(())
        })
    }

    fn bootstrap(
        self: capnp::capability::Rc<Self>,
        _params: system_capnp::process::BootstrapParams,
        mut results: system_capnp::process::BootstrapResults,
    ) -> impl std::future::Future<Output = Result<(), capnp::Error>> + 'static {
        let cap = self.bootstrap_cap.clone();
        Promise::from_future(async move {
            let cap = cap.ok_or_else(|| {
                capnp::Error::failed(
                    "process did not export a bootstrap capability via system::serve()".into(),
                )
            })?;
            results.get().init_cap().set_as_capability(cap.hook);
            Ok(())
        })
    }

    fn kill(
        self: capnp::capability::Rc<Self>,
        _params: system_capnp::process::KillParams,
        _results: system_capnp::process::KillResults,
    ) -> impl std::future::Future<Output = Result<(), capnp::Error>> + 'static {
        let _ = self.kill_tx.send(true);
        tracing::info!("process.kill: kill signal sent");
        Promise::ok(())
    }
}

#[allow(dead_code)] // swarm_cmd_tx and wasm_debug reserved for future Host methods
pub struct HostImpl {
    network_state: NetworkState,
    swarm_cmd_tx: mpsc::Sender<SwarmCommand>,
    wasm_debug: bool,
    guard: Option<EpochGuard>,
    stream_control: Option<libp2p_stream::Control>,
    route_registry: Option<crate::dispatch::RouteRegistry>,
}

impl HostImpl {
    pub fn new(
        network_state: NetworkState,
        swarm_cmd_tx: mpsc::Sender<SwarmCommand>,
        wasm_debug: bool,
        guard: Option<EpochGuard>,
        stream_control: Option<libp2p_stream::Control>,
    ) -> Self {
        Self {
            network_state,
            swarm_cmd_tx,
            wasm_debug,
            guard,
            stream_control,
            route_registry: None,
        }
    }

    /// Set the HTTP route registry for WAGI service integration.
    pub fn with_route_registry(mut self, registry: crate::dispatch::RouteRegistry) -> Self {
        self.route_registry = Some(registry);
        self
    }

    fn check_epoch(&self) -> Result<(), capnp::Error> {
        match self.guard {
            Some(ref g) => g.check(),
            None => Ok(()),
        }
    }
}

#[allow(refining_impl_trait)]
impl system_capnp::host::Server for HostImpl {
    fn id(
        self: capnp::capability::Rc<Self>,
        _params: system_capnp::host::IdParams,
        mut results: system_capnp::host::IdResults,
    ) -> Promise<(), capnp::Error> {
        pry!(self.check_epoch());
        let network_state = self.network_state.clone();
        Promise::from_future(async move {
            let snapshot = network_state.snapshot().await;
            results.get().set_peer_id(&snapshot.local_peer_id);
            Ok(())
        })
    }

    fn addrs(
        self: capnp::capability::Rc<Self>,
        _params: system_capnp::host::AddrsParams,
        mut results: system_capnp::host::AddrsResults,
    ) -> Promise<(), capnp::Error> {
        pry!(self.check_epoch());
        let network_state = self.network_state.clone();
        Promise::from_future(async move {
            let snapshot = network_state.snapshot().await;
            let mut list = results.get().init_addrs(snapshot.listen_addrs.len() as u32);
            for (i, addr) in snapshot.listen_addrs.iter().enumerate() {
                list.set(i as u32, addr);
            }
            Ok(())
        })
    }

    fn peers(
        self: capnp::capability::Rc<Self>,
        _params: system_capnp::host::PeersParams,
        mut results: system_capnp::host::PeersResults,
    ) -> Promise<(), capnp::Error> {
        pry!(self.check_epoch());
        let network_state = self.network_state.clone();
        Promise::from_future(async move {
            let snapshot = network_state.snapshot().await;
            let mut list = results.get().init_peers(snapshot.known_peers.len() as u32);
            for (i, peer) in snapshot.known_peers.iter().enumerate() {
                let mut entry = list.reborrow().get(i as u32);
                entry.set_peer_id(&peer.peer_id);
                let mut addrs = entry.init_addrs(peer.addrs.len() as u32);
                for (j, addr) in peer.addrs.iter().enumerate() {
                    addrs.set(j as u32, addr);
                }
            }
            Ok(())
        })
    }

    fn network(
        self: capnp::capability::Rc<Self>,
        _params: system_capnp::host::NetworkParams,
        mut results: system_capnp::host::NetworkResults,
    ) -> Promise<(), capnp::Error> {
        pry!(self.check_epoch());
        let guard = match &self.guard {
            Some(g) => g.clone(),
            None => {
                return Promise::err(capnp::Error::failed(
                    "network() requires an epoch-scoped Host".into(),
                ))
            }
        };
        let stream_control = match &self.stream_control {
            Some(c) => c.clone(),
            None => {
                return Promise::err(capnp::Error::failed(
                    "network() not available on this Host".into(),
                ))
            }
        };
        let stream_listener: system_capnp::stream_listener::Client = capnp_rpc::new_client(
            stream_listener::StreamListenerImpl::new(stream_control.clone(), guard.clone()),
        );
        let stream_dialer: system_capnp::stream_dialer::Client = capnp_rpc::new_client(
            stream_dialer::StreamDialerImpl::new(stream_control.clone(), guard.clone()),
        );
        let vat_listener: system_capnp::vat_listener::Client = capnp_rpc::new_client(
            vat_listener::VatListenerImpl::new(stream_control.clone(), guard.clone()),
        );
        let vat_client: system_capnp::vat_client::Client = capnp_rpc::new_client(
            vat_client::VatClientImpl::new(stream_control, guard.clone()),
        );
        let registry = self
            .route_registry
            .clone()
            .unwrap_or_else(crate::dispatch::new_registry);
        let http_listener: system_capnp::http_listener::Client =
            capnp_rpc::new_client(http_listener::HttpListenerImpl::new(guard, registry));
        results.get().set_stream_listener(stream_listener);
        results.get().set_stream_dialer(stream_dialer);
        results.get().set_vat_listener(vat_listener);
        results.get().set_vat_client(vat_client);
        results.get().set_http_listener(http_listener);
        Promise::ok(())
    }
}

// =========================================================================
// CachePolicy — operator-level runtime cache configuration
// =========================================================================

/// Runtime-wide cache policy for `Runtime.load()`.
///
/// Set by `--runtime-cache-policy` CLI flag or `WW_RUNTIME_CACHE_POLICY` env var.
/// Default is `Shared` — the common case for performance.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum CachePolicy {
    /// `load(same bytes)` → clone of cached Executor client (same server object).
    #[default]
    Shared,
    /// `load(same bytes)` → fresh Executor server every time.
    Isolated,
}

pub fn build_peer_rpc<R, W>(
    reader: R,
    writer: W,
    network_state: NetworkState,
    swarm_cmd_tx: mpsc::Sender<SwarmCommand>,
    wasm_debug: bool,
) -> RpcSystem<Side>
where
    R: AsyncRead + Unpin + 'static,
    W: AsyncWrite + Unpin + 'static,
{
    let host: system_capnp::host::Client = capnp_rpc::new_client(HostImpl::new(
        network_state,
        swarm_cmd_tx,
        wasm_debug,
        None,
        None,
    ));

    let rpc_network = VatNetwork::new(
        reader.compat(),
        writer.compat_write(),
        Side::Server,
        Default::default(),
    );
    RpcSystem::new(Box::new(rpc_network), Some(host.client))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio_util::compat::{TokioAsyncReadCompatExt, TokioAsyncWriteCompatExt};

    /// A throwaway capability for tests that exercise protocol/validation logic
    /// where the specific cap does not matter (Export now carries a bare cap).
    fn placeholder_cap() -> capnp::capability::Client {
        let (a, _b) = io::duplex(64);
        let bs: system_capnp::byte_stream::Client =
            capnp_rpc::new_client(ByteStreamImpl::new(a, StreamMode::WriteOnly));
        bs.client
    }

    /// Helper: spin up server + client over in-memory duplex, return Host client.
    fn setup_rpc() -> (
        system_capnp::host::Client,
        tokio::task::JoinHandle<()>,
        mpsc::Receiver<SwarmCommand>,
    ) {
        let (client_stream, server_stream) = io::duplex(8 * 1024);
        let (client_read, client_write) = io::split(client_stream);
        let (server_read, server_write) = io::split(server_stream);

        let peer_id = vec![1, 2, 3, 4];
        let network_state = NetworkState::from_peer_id(peer_id);
        let (swarm_tx, swarm_rx) = mpsc::channel(16);

        let server_rpc = build_peer_rpc(server_read, server_write, network_state, swarm_tx, false);

        let server_handle = tokio::task::spawn_local(async move {
            let _ = server_rpc.await;
        });

        let client_network = VatNetwork::new(
            client_read.compat(),
            client_write.compat_write(),
            Side::Client,
            Default::default(),
        );
        let mut client_rpc = RpcSystem::new(Box::new(client_network), None);
        let host: system_capnp::host::Client = client_rpc.bootstrap(Side::Server);
        tokio::task::spawn_local(async move {
            let _ = client_rpc.await;
        });

        (host, server_handle, swarm_rx)
    }

    #[tokio::test]
    async fn test_host_id_returns_peer_id() {
        let local = tokio::task::LocalSet::new();
        local
            .run_until(async {
                let (host, _server, _rx) = setup_rpc();

                let resp = host.id_request().send().promise.await.unwrap();
                let peer_id = resp.get().unwrap().get_peer_id().unwrap();
                assert_eq!(peer_id, &[1, 2, 3, 4]);
            })
            .await;
    }

    #[tokio::test]
    async fn test_host_addrs_initially_empty() {
        let local = tokio::task::LocalSet::new();
        local
            .run_until(async {
                let (host, _server, _rx) = setup_rpc();

                let resp = host.addrs_request().send().promise.await.unwrap();
                let addrs = resp.get().unwrap().get_addrs().unwrap();
                assert_eq!(addrs.len(), 0);
            })
            .await;
    }

    #[tokio::test]
    async fn test_host_peers_initially_empty() {
        let local = tokio::task::LocalSet::new();
        local
            .run_until(async {
                let (host, _server, _rx) = setup_rpc();

                let resp = host.peers_request().send().promise.await.unwrap();
                let peers = resp.get().unwrap().get_peers().unwrap();
                assert_eq!(peers.len(), 0);
            })
            .await;
    }

    // Echo tests removed — echo method deleted from API.
    // Runtime.load() and Executor.spawn() are tested via integration tests
    // that compile real WASM (not mockable in unit tests without a wasmtime Engine).

    #[tokio::test]
    async fn test_network_state_snapshot() {
        let state = NetworkState::from_peer_id(vec![42]);

        let snap = state.snapshot().await;
        assert_eq!(snap.local_peer_id, vec![42]);
        assert!(snap.listen_addrs.is_empty());
        assert!(snap.known_peers.is_empty());
    }

    #[tokio::test]
    async fn test_network_state_add_remove_addr() {
        let state = NetworkState::from_peer_id(vec![1]);

        state.add_listen_addr(vec![10, 20]).await;
        state.add_listen_addr(vec![30, 40]).await;

        let snap = state.snapshot().await;
        assert_eq!(snap.listen_addrs.len(), 2);

        // Duplicate add is a no-op
        state.add_listen_addr(vec![10, 20]).await;
        let snap = state.snapshot().await;
        assert_eq!(snap.listen_addrs.len(), 2);

        // Remove
        state.remove_listen_addr(&[10, 20]).await;
        let snap = state.snapshot().await;
        assert_eq!(snap.listen_addrs.len(), 1);
        assert_eq!(snap.listen_addrs[0], vec![30, 40]);
    }

    #[tokio::test]
    async fn test_network_state_waits_for_listen_addr_without_polling() {
        let state = NetworkState::from_peer_id(vec![1]);
        let waiter_state = state.clone();
        let waiter = tokio::spawn(async move { waiter_state.wait_for_listen_addr().await });

        state.add_listen_addr(vec![10, 20]).await;

        let addr = tokio::time::timeout(std::time::Duration::from_secs(1), waiter)
            .await
            .expect("listen-address waiter timed out")
            .expect("listen-address waiter task");
        assert_eq!(addr, vec![10, 20]);
        assert_eq!(state.wait_for_listen_addr().await, vec![10, 20]);
    }

    #[tokio::test]
    async fn test_network_state_set_known_peers() {
        let state = NetworkState::from_peer_id(vec![1]);

        let peers = vec![
            PeerInfo {
                peer_id: vec![2],
                addrs: vec![vec![10]],
            },
            PeerInfo {
                peer_id: vec![3],
                addrs: vec![vec![20], vec![30]],
            },
        ];
        state.set_known_peers(peers).await;

        let snap = state.snapshot().await;
        assert_eq!(snap.known_peers.len(), 2);
        assert_eq!(snap.known_peers[0].peer_id, vec![2]);
        assert_eq!(snap.known_peers[1].addrs.len(), 2);
    }

    #[tokio::test]
    async fn test_network_state_set_peer_id() {
        let state = NetworkState::from_peer_id(vec![1]);
        state.set_local_peer_id(vec![99]).await;

        let snap = state.snapshot().await;
        assert_eq!(snap.local_peer_id, vec![99]);
    }

    #[tokio::test]
    async fn test_network_state_clone_shares_state() {
        let state1 = NetworkState::from_peer_id(vec![1]);
        let state2 = state1.clone();

        state1.add_listen_addr(vec![10]).await;

        let snap = state2.snapshot().await;
        assert_eq!(snap.listen_addrs.len(), 1);
    }

    #[tokio::test]
    async fn test_network_state_records_nat_probe_events() {
        let state = NetworkState::from_peer_id(vec![1]);
        state
            .record_nat_probe_event(NatProbeEvent {
                tested_addr: "/ip4/127.0.0.1/tcp/2025".to_string(),
                server_peer_id: "12D3KooWTest".to_string(),
                success: true,
                timestamp_unix_ms: 123,
            })
            .await;

        let snap = state.snapshot().await;
        assert_eq!(snap.nat_probe_events.len(), 1);
        assert_eq!(
            snap.nat_probe_events[0].tested_addr,
            "/ip4/127.0.0.1/tcp/2025"
        );
        assert!(snap.nat_probe_events[0].success);
    }

    #[tokio::test]
    async fn test_network_state_nat_probe_ring_buffer_bounded() {
        let state = NetworkState::from_peer_id(vec![1]);
        for i in 0..(MAX_NAT_PROBE_EVENTS + 5) {
            state
                .record_nat_probe_event(NatProbeEvent {
                    tested_addr: format!("/ip4/127.0.0.1/tcp/{}", 2000 + i),
                    server_peer_id: format!("12D3KooW{i}"),
                    success: i % 2 == 0,
                    timestamp_unix_ms: i as u64,
                })
                .await;
        }

        let snap = state.snapshot().await;
        assert_eq!(snap.nat_probe_events.len(), MAX_NAT_PROBE_EVENTS);
        assert_eq!(snap.nat_probe_events[0].timestamp_unix_ms, 5);
        assert_eq!(
            snap.nat_probe_events[MAX_NAT_PROBE_EVENTS - 1].timestamp_unix_ms,
            (MAX_NAT_PROBE_EVENTS + 4) as u64
        );
    }

    // =========================================================================
    // vat_protocol tests
    // =========================================================================

    #[test]
    fn test_vat_protocol_builds_valid_protocol() {
        let protocol = super::vat_protocol("greeter");
        assert!(protocol.is_ok());
        let proto = protocol.unwrap();
        assert_eq!(proto.as_ref(), "/ww/0.1.0/vat/greeter");
    }

    #[test]
    fn test_vat_protocol_rejects_path_like_name() {
        let err = super::vat_protocol("foo/bar").unwrap_err();
        assert!(err.to_string().contains("must not contain '/'"));
    }

    #[test]
    fn test_stream_protocol_builds_valid_protocol() {
        let protocol = super::stream_protocol("echo");
        assert!(protocol.is_ok());
        let proto = protocol.unwrap();
        assert_eq!(proto.as_ref(), "/ww/0.1.0/stream/echo");
    }

    #[test]
    fn test_stream_protocol_rejects_path_like_name() {
        let err = super::stream_protocol("foo/bar").unwrap_err();
        assert!(err.to_string().contains("must not contain '/'"));
    }

    // =========================================================================
    // Process.bootstrap() tests
    // =========================================================================

    /// Helper: create an in-memory RPC pair for a Process capability.
    fn setup_process_rpc(process_impl: ProcessImpl) -> system_capnp::process::Client {
        let (client_stream, server_stream) = io::duplex(8 * 1024);
        let (client_read, client_write) = io::split(client_stream);
        let (server_read, server_write) = io::split(server_stream);

        let process_cap: system_capnp::process::Client = capnp_rpc::new_client(process_impl);

        let server_network = VatNetwork::new(
            server_read.compat(),
            server_write.compat_write(),
            Side::Server,
            Default::default(),
        );
        let server_rpc = RpcSystem::new(Box::new(server_network), Some(process_cap.client));
        tokio::task::spawn_local(async move {
            let _ = server_rpc.await;
        });

        let client_network = VatNetwork::new(
            client_read.compat(),
            client_write.compat_write(),
            Side::Client,
            Default::default(),
        );
        let mut client_rpc = RpcSystem::new(Box::new(client_network), None);
        let client: system_capnp::process::Client = client_rpc.bootstrap(Side::Server);
        tokio::task::spawn_local(async move {
            let _ = client_rpc.await;
        });

        client
    }

    /// Helper: create a dummy ByteStream + exit channel for ProcessImpl.
    fn dummy_process_parts() -> (
        system_capnp::byte_stream::Client,
        system_capnp::byte_stream::Client,
        system_capnp::byte_stream::Client,
        tokio::sync::oneshot::Receiver<i32>,
        tokio::sync::watch::Sender<bool>,
    ) {
        let (dummy_in, _) = io::duplex(1);
        let (dummy_out, _) = io::duplex(1);
        let (dummy_err, _) = io::duplex(1);
        let stdin = capnp_rpc::new_client(ByteStreamImpl::new(dummy_in, StreamMode::WriteOnly));
        let stdout = capnp_rpc::new_client(ByteStreamImpl::new(dummy_out, StreamMode::ReadOnly));
        let stderr = capnp_rpc::new_client(ByteStreamImpl::new(dummy_err, StreamMode::ReadOnly));
        let (_tx, rx) = tokio::sync::oneshot::channel();
        let (kill_tx, _kill_rx) = tokio::sync::watch::channel(false);
        (stdin, stdout, stderr, rx, kill_tx)
    }

    #[tokio::test]
    async fn test_process_bootstrap_returns_stored_cap() {
        let local = tokio::task::LocalSet::new();
        local
            .run_until(async {
                // Use the Host cap as the bootstrap capability.
                let (host, _server, _rx) = setup_rpc();

                let (stdin, stdout, stderr, exit_rx, kill_tx) = dummy_process_parts();
                let process_impl = ProcessImpl::with_bootstrap(
                    stdin,
                    stdout,
                    stderr,
                    exit_rx,
                    host.client.clone(),
                    kill_tx,
                );
                let process = setup_process_rpc(process_impl);

                // Call bootstrap() — returns the exported capability directly.
                let resp = process.bootstrap_request().send().promise.await.unwrap();
                // bootstrap now returns the exported capability directly.
                resp.get()
                    .unwrap()
                    .get_cap()
                    .get_as_capability::<capnp::capability::Client>()
                    .unwrap();
            })
            .await;
    }

    #[tokio::test]
    async fn test_process_bootstrap_errors_without_cap() {
        let local = tokio::task::LocalSet::new();
        local
            .run_until(async {
                let (stdin, stdout, stderr, exit_rx, kill_tx) = dummy_process_parts();
                let process_impl = ProcessImpl::new(stdin, stdout, stderr, exit_rx, kill_tx);
                let process = setup_process_rpc(process_impl);

                // Call bootstrap() without a stored cap — should error.
                let result = process.bootstrap_request().send().promise.await;
                assert!(
                    result.is_err() || {
                        let resp = result.unwrap();
                        // The error may come from a missing exported capability,
                        // or from the server returning an error in the response.
                        resp.get().is_err()
                    }
                );
            })
            .await;
    }

    #[tokio::test]
    async fn test_process_bootstrap_cap_survives_multiple_calls() {
        let local = tokio::task::LocalSet::new();
        local
            .run_until(async {
                let (host, _server, _rx) = setup_rpc();

                let (stdin, stdout, stderr, exit_rx, kill_tx) = dummy_process_parts();
                let process_impl = ProcessImpl::with_bootstrap(
                    stdin,
                    stdout,
                    stderr,
                    exit_rx,
                    host.client.clone(),
                    kill_tx,
                );
                let process = setup_process_rpc(process_impl);

                // Call bootstrap() twice — both should return working caps.
                for _ in 0..2 {
                    let resp = process.bootstrap_request().send().promise.await.unwrap();
                    resp.get()
                        .unwrap()
                        .get_cap()
                        .get_as_capability::<capnp::capability::Client>()
                        .unwrap();
                }
            })
            .await;
    }

    #[tokio::test]
    async fn test_bootstrap_cap_resolves_after_delay() {
        // Simulate the real scenario: build_membrane_rpc returns a pipelined
        // bootstrap cap immediately, but the cell hasn't called serve() yet.
        // Cap'n Proto promise pipelining should queue requests and resolve them
        // once the underlying cap becomes available.
        let local = tokio::task::LocalSet::new();
        local
            .run_until(async {
                let (host, _server, _rx) = setup_rpc();

                // Create a "delayed" host cap using new_future_client.
                // This simulates a pipelined cap that resolves after 200ms.
                let host_clone = host.clone();
                let delayed_host: system_capnp::host::Client =
                    capnp_rpc::new_future_client(async move {
                        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
                        Ok::<_, capnp::Error>(host_clone)
                    });

                // Store the delayed cap in ProcessImpl.
                let (stdin, stdout, stderr, exit_rx, kill_tx) = dummy_process_parts();
                let process_impl = ProcessImpl::with_bootstrap(
                    stdin,
                    stdout,
                    stderr,
                    exit_rx,
                    delayed_host.client.clone(),
                    kill_tx,
                );
                let process = setup_process_rpc(process_impl);

                // Call bootstrap() immediately — the cap hasn't resolved yet.
                let resp = process.bootstrap_request().send().promise.await.unwrap();
                resp.get()
                    .unwrap()
                    .get_cap()
                    .get_as_capability::<capnp::capability::Client>()
                    .unwrap();
            })
            .await;
    }

    // =========================================================================
    // Host.network() tests
    // =========================================================================

    #[tokio::test]
    async fn test_host_network_errors_without_epoch() {
        let local = tokio::task::LocalSet::new();
        local
            .run_until(async {
                // setup_rpc() creates a non-epoch-scoped Host (no guard, no stream_control).
                let (host, _server, _rx) = setup_rpc();

                let result = host.network_request().send().promise.await;
                assert!(
                    result.is_err(),
                    "network() should fail on non-epoch-scoped Host"
                );
            })
            .await;
    }

    // =========================================================================
    // Process.wait() tests
    // =========================================================================

    #[tokio::test]
    async fn test_process_wait_returns_exit_code() {
        let local = tokio::task::LocalSet::new();
        local
            .run_until(async {
                let (stdin, stdout, stderr, _, kill_tx) = dummy_process_parts();
                // Create our own channel so we control the sender.
                let (exit_tx, exit_rx) = tokio::sync::oneshot::channel();
                let process_impl = ProcessImpl::new(stdin, stdout, stderr, exit_rx, kill_tx);
                let process = setup_process_rpc(process_impl);

                // Send exit code from the "cell" side.
                exit_tx.send(42).unwrap();

                let resp = process.wait_request().send().promise.await.unwrap();
                let exit_code = resp.get().unwrap().get_exit_code();
                assert_eq!(exit_code, 42);
            })
            .await;
    }

    #[tokio::test]
    async fn test_process_wait_double_call_errors() {
        let local = tokio::task::LocalSet::new();
        local
            .run_until(async {
                let (stdin, stdout, stderr, _, kill_tx) = dummy_process_parts();
                let (exit_tx, exit_rx) = tokio::sync::oneshot::channel();
                let process_impl = ProcessImpl::new(stdin, stdout, stderr, exit_rx, kill_tx);
                let process = setup_process_rpc(process_impl);

                exit_tx.send(0).unwrap();

                // First call succeeds.
                let resp = process.wait_request().send().promise.await.unwrap();
                assert_eq!(resp.get().unwrap().get_exit_code(), 0);

                // Second call should error (receiver already consumed).
                let result = process.wait_request().send().promise.await;
                assert!(result.is_err(), "wait() called twice should fail");
            })
            .await;
    }

    // =========================================================================
    // RPC bridge integration tests
    // =========================================================================
    //
    // These test the full capability bridge pattern used by VatListener/VatClient
    // without requiring libp2p or WASM. We simulate the bridge with duplex streams:
    //
    //   Cell (Host cap)
    //       ↓ bootstrap cap
    //   Process.bootstrap()
    //       ↓ cap over duplex
    //   Host bridge (Side::Server, bootstrap = cell_cap)
    //       ↓ duplex stream
    //   Remote peer (Side::Client, bootstraps → gets cell_cap)
    //       ↓
    //   Uses the cap (id request)

    /// Simulate the host bridge: serve a bootstrap cap over a duplex stream,
    /// return the "remote peer" side client that bootstrapped from it.
    fn setup_bridge<T: capnp::capability::FromClientHook>(
        bootstrap_cap: capnp::capability::Client,
    ) -> (T, tokio::task::JoinHandle<()>) {
        let (peer_stream, bridge_stream) = io::duplex(8 * 1024);
        let (bridge_read, bridge_write) = io::split(bridge_stream);
        let (peer_read, peer_write) = io::split(peer_stream);

        // Host bridge side: serve the cell's cap.
        let bridge_network = VatNetwork::new(
            bridge_read.compat(),
            bridge_write.compat_write(),
            Side::Server,
            Default::default(),
        );
        let bridge_rpc = RpcSystem::new(Box::new(bridge_network), Some(bootstrap_cap));
        let bridge_handle = tokio::task::spawn_local(async move {
            let _ = bridge_rpc.await;
        });

        // Remote peer side: bootstrap to get the cell's cap.
        let peer_network = VatNetwork::new(
            peer_read.compat(),
            peer_write.compat_write(),
            Side::Client,
            Default::default(),
        );
        let mut peer_rpc = RpcSystem::new(Box::new(peer_network), None);
        let remote_cap: T = peer_rpc.bootstrap(Side::Server);
        tokio::task::spawn_local(async move {
            let _ = peer_rpc.await;
        });

        (remote_cap, bridge_handle)
    }

    #[tokio::test]
    async fn test_rpc_bridge_cap_flows_to_remote_peer() {
        // The golden path: cell exports a cap → Process.bootstrap() →
        // host serves it over a stream → remote peer bootstraps and uses it.
        let local = tokio::task::LocalSet::new();
        local
            .run_until(async {
                // 1. Create a Host cap (the "cell's exported cap").
                let (host, _server, _rx) = setup_rpc();

                // 2. Bridge the internal cap directly; public bootstrap carries the cap.
                let bootstrap_cap = host.client.clone();

                // 3. Bridge: serve it over a duplex (simulates the libp2p stream bridge).
                let (remote_host, _bridge): (system_capnp::host::Client, _) =
                    setup_bridge(bootstrap_cap);

                // 4. Remote peer uses the cap through the bridge.
                let id_resp = remote_host.id_request().send().promise.await.unwrap();
                let peer_id = id_resp.get().unwrap().get_peer_id().unwrap();
                assert_eq!(peer_id, &[1, 2, 3, 4]);
            })
            .await;
    }

    #[tokio::test]
    async fn test_rpc_bridge_multiple_calls_through_bridge() {
        // Verify the bridge handles multiple sequential RPC calls, not just one.
        let local = tokio::task::LocalSet::new();
        local
            .run_until(async {
                let (host, _server, _rx) = setup_rpc();

                let bootstrap_cap = host.client.clone();

                let (remote_host, _bridge): (system_capnp::host::Client, _) =
                    setup_bridge(bootstrap_cap);

                // Make 5 calls through the bridge.
                for _ in 0..5 {
                    let id_resp = remote_host.id_request().send().promise.await.unwrap();
                    let peer_id = id_resp.get().unwrap().get_peer_id().unwrap();
                    assert_eq!(peer_id, &[1, 2, 3, 4]);
                }
            })
            .await;
    }

    #[tokio::test]
    async fn test_rpc_bridge_concurrent_calls() {
        // Verify pipelined (concurrent) calls work through the bridge.
        let local = tokio::task::LocalSet::new();
        local
            .run_until(async {
                let (host, _server, _rx) = setup_rpc();

                let bootstrap_cap = host.client.clone();

                let (remote_host, _bridge): (system_capnp::host::Client, _) =
                    setup_bridge(bootstrap_cap);

                // Fire 5 calls concurrently (pipelined), then collect results.
                let mut futures = Vec::new();
                for _ in 0..5 {
                    futures.push(remote_host.id_request().send().promise);
                }

                for fut in futures {
                    let resp = fut.await.unwrap();
                    let peer_id = resp.get().unwrap().get_peer_id().unwrap();
                    assert_eq!(peer_id, &[1, 2, 3, 4]);
                }
            })
            .await;
    }

    #[tokio::test]
    async fn test_rpc_bridge_distinct_caps_stay_independent() {
        // Two separate bridges with different bootstrap caps don't interfere.
        // This validates that the bridge correctly isolates per-connection state.
        let local = tokio::task::LocalSet::new();
        local
            .run_until(async {
                let (host, _server, _rx) = setup_rpc();

                // Create two independent process+bridge chains.
                let mut remote_hosts = Vec::new();
                for _ in 0..2 {
                    let cap = host.client.clone();

                    let (remote, _bridge): (system_capnp::host::Client, _) = setup_bridge(cap);
                    remote_hosts.push(remote);
                }

                // Both bridges work independently.
                for remote in &remote_hosts {
                    let id_resp = remote.id_request().send().promise.await.unwrap();
                    let peer_id = id_resp.get().unwrap().get_peer_id().unwrap();
                    assert_eq!(peer_id, &[1, 2, 3, 4]);
                }
            })
            .await;
    }

    // =========================================================================
    // VatListener / VatClient validation tests
    // =========================================================================

    /// Helper: create an EpochGuard and its sender for test manipulation.
    fn test_epoch_guard(seq: u64) -> (tokio::sync::watch::Sender<authority::Epoch>, EpochGuard) {
        let epoch = authority::Epoch {
            seq,
            head: vec![],
            provenance: authority::Provenance::Block(0),
        };
        let (tx, rx) = tokio::sync::watch::channel(epoch);
        let guard = EpochGuard {
            issued_seq: seq,
            receiver: rx,
        };
        (tx, guard)
    }

    /// Helper: create a dummy stream_control for validation tests.
    /// The control won't be used for actual I/O in these tests.
    fn dummy_stream_control() -> libp2p_stream::Control {
        libp2p_stream::Behaviour::new().new_control()
    }

    #[tokio::test]
    async fn test_vat_listener_empty_protocol_errors() {
        let local = tokio::task::LocalSet::new();
        local
            .run_until(async {
                let (_host, _server, _rx) = setup_rpc();
                let (_tx, guard) = test_epoch_guard(1);
                let listener_impl =
                    vat_listener::VatListenerImpl::new(dummy_stream_control(), guard);
                let listener: system_capnp::vat_listener::Client =
                    capnp_rpc::new_client(listener_impl);

                let mut req = listener.serve_request();
                req.get()
                    .init_cap()
                    .set_as_capability(placeholder_cap().hook);
                req.get().set_protocol("");

                let result = req.send().promise.await;
                assert!(result.is_err(), "empty protocol should error");
            })
            .await;
    }

    #[tokio::test]
    async fn test_vat_client_empty_protocol_errors() {
        let local = tokio::task::LocalSet::new();
        local
            .run_until(async {
                let (_tx, guard) = test_epoch_guard(1);
                let dialer_impl = vat_client::VatClientImpl::new(dummy_stream_control(), guard);
                let dialer: system_capnp::vat_client::Client = capnp_rpc::new_client(dialer_impl);

                let mut req = dialer.dial_request();
                // Valid peer ID (Ed25519 public key)
                let keypair = libp2p::identity::Keypair::generate_ed25519();
                let peer_id = libp2p::PeerId::from_public_key(&keypair.public());
                req.get().set_peer(&peer_id.to_bytes());
                req.get().set_protocol("");

                let result = req.send().promise.await;
                assert!(result.is_err(), "empty protocol should error");
            })
            .await;
    }

    #[tokio::test]
    async fn test_vat_client_invalid_peer_id_errors() {
        let local = tokio::task::LocalSet::new();
        local
            .run_until(async {
                let (_tx, guard) = test_epoch_guard(1);
                let dialer_impl = vat_client::VatClientImpl::new(dummy_stream_control(), guard);
                let dialer: system_capnp::vat_client::Client = capnp_rpc::new_client(dialer_impl);

                let mut req = dialer.dial_request();
                req.get().set_peer(&[0xFF, 0xFF, 0xFF]); // garbage peer ID
                req.get().set_protocol("greeter");

                let result = req.send().promise.await;
                assert!(result.is_err(), "invalid peer ID should error");
            })
            .await;
    }

    #[tokio::test]
    async fn test_vat_listener_stale_epoch_errors() {
        let local = tokio::task::LocalSet::new();
        local
            .run_until(async {
                let (_host, _server, _rx) = setup_rpc();
                let (tx, guard) = test_epoch_guard(1);
                let listener_impl =
                    vat_listener::VatListenerImpl::new(dummy_stream_control(), guard);
                let listener: system_capnp::vat_listener::Client =
                    capnp_rpc::new_client(listener_impl);

                // Advance epoch to make guard stale.
                tx.send(authority::Epoch {
                    seq: 2,
                    head: vec![],
                    provenance: authority::Provenance::Block(0),
                })
                .unwrap();

                let mut req = listener.serve_request();
                req.get()
                    .init_cap()
                    .set_as_capability(placeholder_cap().hook);
                req.get().set_protocol("greeter");

                let result = req.send().promise.await;
                assert!(result.is_err(), "stale epoch should error");
            })
            .await;
    }

    #[tokio::test]
    async fn test_vat_client_stale_epoch_errors() {
        let local = tokio::task::LocalSet::new();
        local
            .run_until(async {
                let (tx, guard) = test_epoch_guard(1);
                let dialer_impl = vat_client::VatClientImpl::new(dummy_stream_control(), guard);
                let dialer: system_capnp::vat_client::Client = capnp_rpc::new_client(dialer_impl);

                // Advance epoch to make guard stale.
                tx.send(authority::Epoch {
                    seq: 2,
                    head: vec![],
                    provenance: authority::Provenance::Block(0),
                })
                .unwrap();

                let keypair = libp2p::identity::Keypair::generate_ed25519();
                let peer_id = libp2p::PeerId::from_public_key(&keypair.public());

                let mut req = dialer.dial_request();
                req.get().set_peer(&peer_id.to_bytes());
                req.get().set_protocol("greeter");

                let result = req.send().promise.await;
                assert!(result.is_err(), "stale epoch should error");
            })
            .await;
    }

    #[tokio::test]
    async fn test_vat_listener_protocol_collision_errors() {
        let local = tokio::task::LocalSet::new();
        local
            .run_until(async {
                let (_host, _server, _rx) = setup_rpc();
                let (_tx, guard) = test_epoch_guard(1);
                // Share the same Behaviour so both listeners see the same protocol registry.
                let behaviour = libp2p_stream::Behaviour::new();
                let control1 = behaviour.new_control();
                let control2 = behaviour.new_control();

                let listener1 = vat_listener::VatListenerImpl::new(control1, guard.clone());
                let client1: system_capnp::vat_listener::Client = capnp_rpc::new_client(listener1);

                let listener2 = vat_listener::VatListenerImpl::new(control2, guard);
                let client2: system_capnp::vat_listener::Client = capnp_rpc::new_client(listener2);

                // First registration should succeed.
                let mut req1 = client1.serve_request();
                req1.get()
                    .init_cap()
                    .set_as_capability(placeholder_cap().hook);
                req1.get().set_protocol("greeter");
                req1.send()
                    .promise
                    .await
                    .expect("first serve should succeed");

                // Second registration with the same service name should fail.
                let mut req2 = client2.serve_request();
                req2.get()
                    .init_cap()
                    .set_as_capability(placeholder_cap().hook);
                req2.get().set_protocol("greeter");
                let result = req2.send().promise.await;
                assert!(
                    result.is_err(),
                    "duplicate protocol registration should error"
                );
            })
            .await;
    }

    /// End-to-end: pass an existing cap to VatListener and verify it
    /// successfully registers a service-name protocol.
    #[tokio::test]
    async fn test_vat_listener_accepts_valid_cap_and_protocol() {
        let local = tokio::task::LocalSet::new();
        local
            .run_until(async {
                let (_host, _server, _rx) = setup_rpc();
                let (_tx, guard) = test_epoch_guard(1);
                let listener_impl =
                    vat_listener::VatListenerImpl::new(dummy_stream_control(), guard);
                let listener: system_capnp::vat_listener::Client =
                    capnp_rpc::new_client(listener_impl);

                let mut req = listener.serve_request();
                req.get()
                    .init_cap()
                    .set_as_capability(placeholder_cap().hook);
                req.get().set_protocol("greeter");

                let result = req.send().promise.await;
                assert!(
                    result.is_ok(),
                    "valid cap + protocol should be accepted: {:?}",
                    result.err()
                );
            })
            .await;
    }

    #[tokio::test]
    async fn test_rpc_bridge_dead_cell_returns_error() {
        // When the cell's RPC system dies (process exits), the cap served
        // through the bridge should break. We simulate this by creating a
        // cell-side RPC system we directly control, then abort() it.
        //
        // Topology:
        //   cell RPC (Side::Server, serves Host)
        //       ↓ bootstrap
        //   cell_cap (client ref to Host)
        //       ↓ bridged over
        //   bridge RPC (Side::Server, bootstrap = cell_cap)
        //       ↓ bootstrap
        //   remote_host (remote peer's view)
        //
        // We abort the cell RPC task, which drops the RPC system,
        // closes the duplex half, and disconnects the Host cap.
        let local = tokio::task::LocalSet::new();
        local
            .run_until(async {
                // Create a Host cap to serve as the cell's exported cap.
                let (host, _server, _rx) = setup_rpc();

                // Set up a cell-side RPC system we control.
                let (cell_stream, host_stream) = io::duplex(8 * 1024);
                let (h_read, h_write) = io::split(cell_stream);
                let (c_read, c_write) = io::split(host_stream);

                let cell_network = VatNetwork::new(
                    h_read.compat(),
                    h_write.compat_write(),
                    Side::Server,
                    Default::default(),
                );
                let cell_rpc = RpcSystem::new(Box::new(cell_network), Some(host.client));
                let cell_task = tokio::task::spawn_local(async move {
                    let _ = cell_rpc.await;
                });

                let client_network = VatNetwork::new(
                    c_read.compat(),
                    c_write.compat_write(),
                    Side::Client,
                    Default::default(),
                );
                let mut client_rpc = RpcSystem::new(Box::new(client_network), None);
                let cell_cap: system_capnp::host::Client = client_rpc.bootstrap(Side::Server);
                let cell_cap = cell_cap.client;
                tokio::task::spawn_local(async move {
                    let _ = client_rpc.await;
                });

                // Bridge the cell cap to a remote peer.
                let (remote_host, _bridge): (system_capnp::host::Client, _) =
                    setup_bridge(cell_cap);

                // Verify it works while alive.
                let id_resp = remote_host.id_request().send().promise.await.unwrap();
                let peer_id = id_resp.get().unwrap().get_peer_id().unwrap();
                assert_eq!(peer_id, &[1, 2, 3, 4]);

                // Kill the cell's RPC system.
                cell_task.abort();
                // Let the runtime propagate the disconnection.
                tokio::time::sleep(std::time::Duration::from_millis(50)).await;

                // Call through the bridge should now fail.
                let result = tokio::time::timeout(
                    std::time::Duration::from_millis(500),
                    remote_host.id_request().send().promise,
                )
                .await;

                // Either the inner call errors (disconnected) or the timeout fires
                // (cap is dead but stream hasn't noticed yet). Both prove the cell
                // death propagated — a live cap would return Ok instantly.
                let is_dead = match result {
                    Err(_) => true,     // timeout — stream stalled
                    Ok(Err(_)) => true, // RPC error — disconnected
                    Ok(Ok(resp)) => {
                        // If somehow a response came back, it should be an error.
                        resp.get().is_err()
                    }
                };
                assert!(is_dead, "call through bridge should fail after cell dies");
            })
            .await;
    }

    // =========================================================================
    // ByteStream tests
    // =========================================================================

    /// Helper: create a ByteStream client over in-memory RPC.
    fn setup_byte_stream_rpc(
        mode: StreamMode,
    ) -> (system_capnp::byte_stream::Client, io::DuplexStream) {
        let (host_side, guest_side) = io::duplex(4096);
        let stream_impl = ByteStreamImpl::new(guest_side, mode);
        let client: system_capnp::byte_stream::Client = capnp_rpc::new_client(stream_impl);
        (client, host_side)
    }

    #[tokio::test]
    async fn test_byte_stream_write_and_read_bidirectional() {
        let local = tokio::task::LocalSet::new();
        local
            .run_until(async {
                let (client, mut host) = setup_byte_stream_rpc(StreamMode::Bidirectional);

                // Write through the cap.
                let mut req = client.write_request();
                req.get().set_data(b"hello");
                req.send().promise.await.unwrap();

                // Read on the host side.
                let mut buf = [0u8; 16];
                let n = host.read(&mut buf).await.unwrap();
                assert_eq!(&buf[..n], b"hello");
            })
            .await;
    }

    #[tokio::test]
    async fn test_byte_stream_read_only_rejects_write() {
        let local = tokio::task::LocalSet::new();
        local
            .run_until(async {
                let (client, _host) = setup_byte_stream_rpc(StreamMode::ReadOnly);
                let mut req = client.write_request();
                req.get().set_data(b"nope");
                let result = req.send().promise.await;
                assert!(result.is_err(), "write to read-only stream should fail");
            })
            .await;
    }

    #[tokio::test]
    async fn test_byte_stream_write_only_rejects_read() {
        let local = tokio::task::LocalSet::new();
        local
            .run_until(async {
                let (client, _host) = setup_byte_stream_rpc(StreamMode::WriteOnly);
                let mut req = client.read_request();
                req.get().set_max_bytes(1024);
                let result = req.send().promise.await;
                assert!(result.is_err(), "read from write-only stream should fail");
            })
            .await;
    }

    #[tokio::test]
    async fn test_byte_stream_read_returns_host_data() {
        let local = tokio::task::LocalSet::new();
        local
            .run_until(async {
                let (client, mut host) = setup_byte_stream_rpc(StreamMode::Bidirectional);

                // Write from host side.
                use tokio::io::AsyncWriteExt;
                host.write_all(b"from host").await.unwrap();
                host.flush().await.unwrap();

                // Read through the cap.
                let mut req = client.read_request();
                req.get().set_max_bytes(1024);
                let resp = req.send().promise.await.unwrap();
                let data = resp.get().unwrap().get_data().unwrap();
                assert_eq!(data, b"from host");
            })
            .await;
    }

    #[tokio::test]
    async fn test_byte_stream_close() {
        let local = tokio::task::LocalSet::new();
        local
            .run_until(async {
                let (client, _host) = setup_byte_stream_rpc(StreamMode::Bidirectional);
                let result = client.close_request().send().promise.await;
                assert!(result.is_ok(), "close should succeed");
            })
            .await;
    }
}
