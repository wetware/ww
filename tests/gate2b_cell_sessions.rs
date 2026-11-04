//! Cell-owned outbound RPC qualification through Runtime/Executor and real P3 Cells.
//! Removing authenticated ingress or the Cell close/join barrier must break
//! these tests: neither executor task-local scope nor a live imported cap owns
//! an outbound session.

mod support;
#[path = "support/ticked_executor.rs"]
mod ticked_executor;

use capnp::capability::{Client as AnyClient, FromClientHook};
use capnp::traits::HasTypeId;
use capnp_rpc::{rpc_twoparty_capnp::Side, twoparty::VatNetwork};
use futures::{io::AsyncReadExt, StreamExt};
use rpc::managed_rpc::{ManagedRpc, ManagedRpcSystem};
use std::{
    cell::{Cell, RefCell},
    path::Path,
    rc::Rc,
    time::Duration,
};
use ww::system_capnp::{self, membrane as membrane_schema, process, vat_client};

#[derive(Default)]
struct TransportGate {
    paused: std::sync::atomic::AtomicBool,
    read: futures::task::AtomicWaker,
    write: futures::task::AtomicWaker,
    negotiation: std::sync::Arc<P3NegotiationObservation>,
}
impl TransportGate {
    fn hold(&self) {
        self.paused.store(true, std::sync::atomic::Ordering::SeqCst);
    }
    fn release(&self) {
        self.paused
            .store(false, std::sync::atomic::Ordering::SeqCst);
        self.read.wake();
        self.write.wake();
    }
    fn blocks(&self, cx: &std::task::Context<'_>, waker: &futures::task::AtomicWaker) -> bool {
        waker.register(cx.waker());
        self.paused.load(std::sync::atomic::Ordering::SeqCst)
    }
}
struct GatedTransport<S> {
    inner: S,
    gate: std::sync::Arc<TransportGate>,
}
impl<S: futures::io::AsyncRead + Unpin> futures::io::AsyncRead for GatedTransport<S> {
    fn poll_read(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        bytes: &mut [u8],
    ) -> std::task::Poll<std::io::Result<usize>> {
        if self.gate.blocks(cx, &self.gate.read) {
            return std::task::Poll::Pending;
        }
        std::pin::Pin::new(&mut self.inner).poll_read(cx, bytes)
    }
}
impl<S: futures::io::AsyncWrite + Unpin> futures::io::AsyncWrite for GatedTransport<S> {
    fn poll_write(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        bytes: &[u8],
    ) -> std::task::Poll<std::io::Result<usize>> {
        if self.gate.blocks(cx, &self.gate.write) {
            return std::task::Poll::Pending;
        }
        std::pin::Pin::new(&mut self.inner).poll_write(cx, bytes)
    }
    fn poll_flush(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        if self.gate.blocks(cx, &self.gate.write) {
            return std::task::Poll::Pending;
        }
        std::pin::Pin::new(&mut self.inner).poll_flush(cx)
    }
    fn poll_close(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        if self.gate.blocks(cx, &self.gate.write) {
            return std::task::Poll::Pending;
        }
        std::pin::Pin::new(&mut self.inner).poll_close(cx)
    }
}

// Test-only delegation preserves the actual pinned handler and native timeout.
// The substream wrapper witnesses an allocated outbound Yamux stream attempting
// the V1 multistream header; the handler wrapper observes its real Timeout event.
#[derive(Default)]
struct P3NegotiationObservation {
    started: std::sync::atomic::AtomicUsize,
    timed_out: std::sync::atomic::AtomicUsize,
    negotiated: std::sync::atomic::AtomicUsize,
    timeout_event: tokio::sync::Notify,
}

struct P3Substream<S> {
    inner: S,
    observation: std::sync::Arc<P3NegotiationObservation>,
    first_outbound_write: bool,
}
impl<S: futures::io::AsyncRead + Unpin> futures::io::AsyncRead for P3Substream<S> {
    fn poll_read(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        bytes: &mut [u8],
    ) -> std::task::Poll<std::io::Result<usize>> {
        std::pin::Pin::new(&mut self.inner).poll_read(cx, bytes)
    }
}
impl<S: futures::io::AsyncWrite + Unpin> futures::io::AsyncWrite for P3Substream<S> {
    fn poll_write(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        bytes: &[u8],
    ) -> std::task::Poll<std::io::Result<usize>> {
        if self.first_outbound_write && !bytes.is_empty() {
            assert!(bytes.starts_with(b"\x13/multistream/1.0.0\n"));
            self.first_outbound_write = false;
            self.observation
                .started
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        }
        std::pin::Pin::new(&mut self.inner).poll_write(cx, bytes)
    }
    fn poll_flush(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        std::pin::Pin::new(&mut self.inner).poll_flush(cx)
    }
    fn poll_close(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        std::pin::Pin::new(&mut self.inner).poll_close(cx)
    }
}
struct P3Muxer<M> {
    inner: M,
    observation: std::sync::Arc<P3NegotiationObservation>,
}
impl<M: libp2p::core::muxing::StreamMuxer + Unpin> libp2p::core::muxing::StreamMuxer for P3Muxer<M>
where
    M::Substream: Unpin,
{
    type Substream = P3Substream<M::Substream>;
    type Error = M::Error;
    fn poll_inbound(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Result<Self::Substream, Self::Error>> {
        std::pin::Pin::new(&mut self.inner)
            .poll_inbound(cx)
            .map_ok(|inner| P3Substream {
                inner,
                observation: self.observation.clone(),
                first_outbound_write: false,
            })
    }
    fn poll_outbound(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Result<Self::Substream, Self::Error>> {
        std::pin::Pin::new(&mut self.inner)
            .poll_outbound(cx)
            .map_ok(|inner| P3Substream {
                inner,
                observation: self.observation.clone(),
                first_outbound_write: true,
            })
    }
    fn poll_close(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Result<(), Self::Error>> {
        std::pin::Pin::new(&mut self.inner).poll_close(cx)
    }
    fn poll(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Result<libp2p::core::muxing::StreamMuxerEvent, Self::Error>> {
        std::pin::Pin::new(&mut self.inner).poll(cx)
    }
}
type P3InnerHandler =
    <libp2p_stream::Behaviour as libp2p::swarm::NetworkBehaviour>::ConnectionHandler;
struct P3ObservedHandler {
    inner: P3InnerHandler,
    observation: std::sync::Arc<P3NegotiationObservation>,
}
impl libp2p::swarm::ConnectionHandler for P3ObservedHandler {
    type FromBehaviour = <P3InnerHandler as libp2p::swarm::ConnectionHandler>::FromBehaviour;
    type ToBehaviour = <P3InnerHandler as libp2p::swarm::ConnectionHandler>::ToBehaviour;
    type InboundProtocol = <P3InnerHandler as libp2p::swarm::ConnectionHandler>::InboundProtocol;
    type OutboundProtocol = <P3InnerHandler as libp2p::swarm::ConnectionHandler>::OutboundProtocol;
    type InboundOpenInfo = <P3InnerHandler as libp2p::swarm::ConnectionHandler>::InboundOpenInfo;
    type OutboundOpenInfo = <P3InnerHandler as libp2p::swarm::ConnectionHandler>::OutboundOpenInfo;
    fn listen_protocol(
        &self,
    ) -> libp2p::swarm::SubstreamProtocol<Self::InboundProtocol, Self::InboundOpenInfo> {
        self.inner.listen_protocol()
    }
    fn connection_keep_alive(&self) -> bool {
        self.inner.connection_keep_alive()
    }
    fn poll(
        &mut self,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<
        libp2p::swarm::ConnectionHandlerEvent<
            Self::OutboundProtocol,
            Self::OutboundOpenInfo,
            Self::ToBehaviour,
        >,
    > {
        self.inner.poll(cx)
    }
    fn poll_close(
        &mut self,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<Self::ToBehaviour>> {
        self.inner.poll_close(cx)
    }
    fn on_behaviour_event(&mut self, event: Self::FromBehaviour) {
        self.inner.on_behaviour_event(event);
    }
    fn on_connection_event(
        &mut self,
        event: libp2p::swarm::handler::ConnectionEvent<
            Self::InboundProtocol,
            Self::OutboundProtocol,
            Self::InboundOpenInfo,
            Self::OutboundOpenInfo,
        >,
    ) {
        use libp2p::swarm::handler::ConnectionEvent;
        match &event {
            ConnectionEvent::DialUpgradeError(event)
                if matches!(event.error, libp2p::swarm::StreamUpgradeError::Timeout) =>
            {
                self.observation
                    .timed_out
                    .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                self.observation.timeout_event.notify_waiters();
            }
            ConnectionEvent::FullyNegotiatedOutbound(_) => {
                self.observation
                    .negotiated
                    .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            }
            _ => {}
        }
        self.inner.on_connection_event(event);
    }
}
struct P3ObservedBehaviour {
    inner: libp2p_stream::Behaviour,
    observation: std::sync::Arc<P3NegotiationObservation>,
}
impl P3ObservedBehaviour {
    fn new_control(&self) -> libp2p_stream::Control {
        self.inner.new_control()
    }
    fn wrap(&self, inner: P3InnerHandler) -> P3ObservedHandler {
        P3ObservedHandler {
            inner,
            observation: self.observation.clone(),
        }
    }
}
impl libp2p::swarm::NetworkBehaviour for P3ObservedBehaviour {
    type ConnectionHandler = P3ObservedHandler;
    type ToSwarm = <libp2p_stream::Behaviour as libp2p::swarm::NetworkBehaviour>::ToSwarm;
    fn handle_pending_inbound_connection(
        &mut self,
        id: libp2p::swarm::ConnectionId,
        local: &libp2p::Multiaddr,
        remote: &libp2p::Multiaddr,
    ) -> Result<(), libp2p::swarm::ConnectionDenied> {
        self.inner
            .handle_pending_inbound_connection(id, local, remote)
    }
    fn handle_established_inbound_connection(
        &mut self,
        id: libp2p::swarm::ConnectionId,
        peer: libp2p::PeerId,
        local: &libp2p::Multiaddr,
        remote: &libp2p::Multiaddr,
    ) -> Result<libp2p::swarm::THandler<Self>, libp2p::swarm::ConnectionDenied> {
        let inner = self
            .inner
            .handle_established_inbound_connection(id, peer, local, remote)?;
        Ok(self.wrap(inner))
    }
    fn handle_pending_outbound_connection(
        &mut self,
        id: libp2p::swarm::ConnectionId,
        peer: Option<libp2p::PeerId>,
        addresses: &[libp2p::Multiaddr],
        role: libp2p::core::Endpoint,
    ) -> Result<Vec<libp2p::Multiaddr>, libp2p::swarm::ConnectionDenied> {
        self.inner
            .handle_pending_outbound_connection(id, peer, addresses, role)
    }
    fn handle_established_outbound_connection(
        &mut self,
        id: libp2p::swarm::ConnectionId,
        peer: libp2p::PeerId,
        address: &libp2p::Multiaddr,
        role: libp2p::core::Endpoint,
        port: libp2p::core::transport::PortUse,
    ) -> Result<libp2p::swarm::THandler<Self>, libp2p::swarm::ConnectionDenied> {
        let inner = self
            .inner
            .handle_established_outbound_connection(id, peer, address, role, port)?;
        Ok(self.wrap(inner))
    }
    fn on_swarm_event(&mut self, event: libp2p::swarm::FromSwarm) {
        self.inner.on_swarm_event(event);
    }
    fn on_connection_handler_event(
        &mut self,
        peer: libp2p::PeerId,
        id: libp2p::swarm::ConnectionId,
        event: libp2p::swarm::THandlerOutEvent<Self>,
    ) {
        self.inner.on_connection_handler_event(peer, id, event);
    }
    fn poll(
        &mut self,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<libp2p::swarm::ToSwarm<Self::ToSwarm, libp2p::swarm::THandlerInEvent<Self>>>
    {
        self.inner.poll(cx)
    }
}

fn swarm(gate: std::sync::Arc<TransportGate>) -> libp2p::Swarm<P3ObservedBehaviour> {
    use libp2p::Transport;
    let key = libp2p::identity::Keypair::generate_ed25519();
    let observation = gate.negotiation.clone();
    let muxer_observation = observation.clone();
    let transport = libp2p::core::transport::MemoryTransport::default()
        .map(move |inner, _| GatedTransport {
            inner,
            gate: gate.clone(),
        })
        .upgrade(libp2p::core::upgrade::Version::V1)
        .authenticate(libp2p::noise::Config::new(&key).unwrap())
        .multiplex(libp2p::yamux::Config::default())
        .map(move |(peer, inner), _| {
            (
                peer,
                libp2p::core::muxing::StreamMuxerBox::new(P3Muxer {
                    inner,
                    observation: muxer_observation.clone(),
                }),
            )
        })
        .boxed();
    libp2p::Swarm::new(
        transport,
        P3ObservedBehaviour {
            inner: libp2p_stream::Behaviour::new(),
            observation,
        },
        key.public().to_peer_id(),
        libp2p::swarm::Config::with_tokio_executor()
            .with_idle_connection_timeout(Duration::from_secs(60)),
    )
}

struct RemoteProbe {
    calls: Rc<Cell<usize>>,
    dropped: Rc<Cell<bool>>,
}
impl process::Server for RemoteProbe {
    async fn stdin(
        self: capnp::capability::Rc<Self>,
        _: process::StdinParams,
        _: process::StdinResults,
    ) -> capnp::Result<()> {
        self.calls.set(self.calls.get() + 1);
        Ok(())
    }
}
impl Drop for RemoteProbe {
    fn drop(&mut self) {
        self.dropped.set(true);
    }
}

#[derive(Clone)]
struct RemoteObservation {
    calls: Rc<Cell<usize>>,
    dropped: Rc<Cell<bool>>,
    joined: Rc<Cell<bool>>,
}
struct Network {
    peer: libp2p::PeerId,
    local_peer: libp2p::PeerId,
    control: libp2p_stream::Control,
    remotes: Rc<RefCell<Vec<RemoteObservation>>>,
    tasks: Vec<tokio::task::JoinHandle<()>>,
    gate: std::sync::Arc<TransportGate>,
}
impl Drop for Network {
    fn drop(&mut self) {
        for task in &self.tasks {
            task.abort();
        }
    }
}
impl Network {
    async fn new() -> Self {
        Self::with_paused_client(false).await
    }
    async fn with_paused_client(paused: bool) -> Self {
        let _ = open_events();
        let mut server = swarm(Default::default());
        let peer = *server.local_peer_id();
        let mut server_control = server.behaviour().new_control();
        let mut incoming = server_control
            .accept(rpc::vat_protocol("gate2b-sessions").unwrap())
            .unwrap();
        server.listen_on("/memory/0".parse().unwrap()).unwrap();
        let address = loop {
            if let libp2p::swarm::SwarmEvent::NewListenAddr { address, .. } =
                server.select_next_some().await
            {
                break address;
            }
        };
        let gate = std::sync::Arc::new(TransportGate::default());
        let mut client = swarm(gate.clone());
        let local_peer = *client.local_peer_id();
        let control = client.behaviour().new_control();
        client.dial(address).unwrap();
        let server_task =
            tokio::task::spawn_local(async move { while server.next().await.is_some() {} });
        while !matches!(
            client.select_next_some().await,
            libp2p::swarm::SwarmEvent::ConnectionEstablished { .. }
        ) {}
        if paused {
            gate.hold();
        }
        let client_task =
            tokio::task::spawn_local(async move { while client.next().await.is_some() {} });
        let remotes = Rc::new(RefCell::new(Vec::new()));
        let observed = remotes.clone();
        let incoming_task = tokio::task::spawn_local(async move {
            while let Some((_, stream)) = incoming.next().await {
                let state = RemoteObservation {
                    calls: Rc::new(Cell::new(0)),
                    dropped: Rc::new(Cell::new(false)),
                    joined: Rc::new(Cell::new(false)),
                };
                observed.borrow_mut().push(state.clone());
                let bootstrap: process::Client = capnp_rpc::new_client(RemoteProbe {
                    calls: state.calls.clone(),
                    dropped: state.dropped.clone(),
                });
                let (read, write) = stream.split();
                let driver = ManagedRpc::spawn(ManagedRpcSystem::new(
                    Box::new(VatNetwork::new(
                        read,
                        write,
                        Side::Server,
                        Default::default(),
                    )),
                    Some(bootstrap.client),
                ));
                tokio::task::spawn_local(async move {
                    let _ = driver.await;
                    state.joined.set(true);
                });
            }
        });
        Self {
            peer,
            local_peer,
            control,
            remotes,
            tasks: vec![server_task, client_task, incoming_task],
            gate,
        }
    }
}

struct Extras(Vec<(&'static str, AnyClient)>);
impl membrane_schema::Server for Extras {
    async fn graft(
        self: capnp::capability::Rc<Self>,
        _: membrane_schema::GraftParams,
        mut results: membrane_schema::GraftResults,
    ) -> capnp::Result<()> {
        let mut extras = results.get().init_extras(self.0.len() as u32);
        for (index, (name, cap)) in self.0.iter().enumerate() {
            let mut entry = extras.reborrow().get(index as u32);
            entry.set_name(*name);
            entry.init_cap().set_as_capability(cap.clone().hook);
        }
        Ok(())
    }
}

struct DialAndUse;
impl membrane::Policy for DialAndUse {
    fn check(&self, interface: u64, method: u16) -> capnp::Result<()> {
        if method == 0
            && (interface == vat_client::Client::TYPE_ID || interface == process::Client::TYPE_ID)
        {
            Ok(())
        } else {
            Err(membrane::denied_error(
                interface,
                method,
                "outside original Gate2B grant",
            ))
        }
    }
}

async fn spawn(
    executor: &system_capnp::executor::Client,
    peer: libp2p::PeerId,
    grants: Vec<(&'static str, AnyClient)>,
) -> process::Client {
    let mut request = executor.spawn_request();
    let mut args = request.get().init_args(2);
    args.set(0, "authority-probe");
    args.set(1, "gate2b-sessions");
    request.get().init_env(1).set(
        0,
        format!(
            "WW_GATE2B_PEER={}",
            peer.to_bytes()
                .iter()
                .map(u8::to_string)
                .collect::<Vec<_>>()
                .join(",")
        ),
    );
    request
        .get()
        .set_membrane(capnp_rpc::new_client(Extras(grants)));
    request
        .send()
        .promise
        .await
        .expect("spawn real P3 Cell")
        .get()
        .unwrap()
        .get_process()
        .unwrap()
}

async fn exports(process: &process::Client) -> membrane_schema::Client {
    process
        .bootstrap_request()
        .send()
        .promise
        .await
        .expect("guest bootstrap")
        .get()
        .unwrap()
        .get_cap()
        .get_as_capability()
        .unwrap()
}
async fn get_export<T: FromClientHook>(exports: &membrane_schema::Client, name: &str) -> T {
    loop {
        let response = exports
            .graft_request()
            .send()
            .promise
            .await
            .expect("guest exports");
        for extra in response.get().unwrap().get_extras().unwrap().iter() {
            if extra.get_name().unwrap().to_str().unwrap() == name {
                return extra.get_cap().get_as_capability().unwrap();
            }
        }
        tokio::time::sleep(Duration::from_millis(1)).await;
    }
}
async fn command(commands: &vat_client::Client, operation: &str, index: u8) -> capnp::Result<()> {
    let mut request = commands.dial_request();
    request.get().set_protocol(operation);
    request.get().set_peer(&[index]);
    request.send().promise.await.map(|_| ())
}
async fn kill(process: &process::Client) {
    process
        .kill_request()
        .send()
        .promise
        .await
        .expect("Process.kill");
    assert_eq!(
        process
            .wait_request()
            .send()
            .promise
            .await
            .expect("Process.wait")
            .get()
            .unwrap()
            .get_exit_code(),
        137
    );
}
async fn eventually(label: &str, mut predicate: impl FnMut() -> bool) {
    tokio::time::timeout(Duration::from_secs(5), async {
        while !predicate() {
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("timed out waiting for {label}"));
}

async fn executor(
    registry: rpc::outbound::OutboundRegistry,
) -> (
    system_capnp::executor::Client,
    ticked_executor::TickedExecutor,
) {
    let wasm = std::fs::read(
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("target/authority-probe/wasm32-wasip3/release/authority_probe.wasm"),
    )
    .expect("build the guest with make authority-probe before this suite");
    let ticked = ticked_executor::TickedExecutor::new();
    let runtime = ww::launcher::create_runtime_client_with_outbound_registry(
        false,
        authority::EpochGuard::fixed(authority::Epoch::zero()),
        ticked.runtime_engine(),
        None,
        rpc::CachePolicy::Isolated,
        None,
        support::filesystem_resources(),
        registry,
    );
    let mut request = runtime.load_request();
    request.get().set_wasm(&wasm);
    let executor = request
        .send()
        .promise
        .await
        .expect("Runtime.load native P3")
        .get()
        .unwrap()
        .get_executor()
        .unwrap();
    (executor, ticked)
}

async fn check_a_b_b_ownership(separate_registries: bool) {
    tokio::time::timeout(
        Duration::from_secs(120),
        tokio::task::LocalSet::new().run_until(async {
            let network = Network::new().await;
            let registry = rpc::outbound::OutboundRegistry::default();
            let b_registry = if separate_registries {
                rpc::outbound::OutboundRegistry::default()
            } else {
                registry.clone()
            };
            let (a_executor, _a_ticked) = executor(registry.clone()).await;
            let (b_executor, _b_ticked) = executor(b_registry.clone()).await;
            let root: vat_client::Client = capnp_rpc::new_client(
                rpc::vat_client::VatClientImpl::new(
                    network.control.clone(),
                    authority::EpochGuard::fixed(authority::Epoch::zero()),
                )
                .with_outbound_registry(registry.clone()),
            );
            let original: vat_client::Client =
                membrane::membrane(root.clone(), Rc::new(DialAndUse));
            let original_identity = (
                original.client.hook.get_ptr(),
                original.client.hook.get_brand(),
            );
            let a = spawn(&a_executor, network.peer, vec![("grant", original.client)]).await;
            let a_exports = exports(&a).await;
            let a_session: process::Client = get_export(&a_exports, "session-0").await;
            let a_owners = registry.live_snapshots();
            assert_eq!(a_owners.len(), 1);
            assert_eq!(a_owners[0].1.committed, 1);
            let a_origin = a_owners[0].0.clone();
            let delegated: vat_client::Client = get_export(&a_exports, "grant").await;
            let resolved = capnp::capability::get_resolved_cap(delegated.clone()).await;
            assert_eq!(
                (
                    resolved.client.hook.get_ptr(),
                    resolved.client.hook.get_brand()
                ),
                original_identity,
                "A must return the exact attenuated grant"
            );
            let b_root: vat_client::Client = capnp_rpc::new_client(
                rpc::vat_client::VatClientImpl::new(
                    network.control.clone(),
                    authority::EpochGuard::fixed(authority::Epoch::zero()),
                )
                .with_outbound_registry(b_registry.clone()),
            );
            let b = spawn(
                &b_executor,
                network.peer,
                vec![("grant", delegated.client), ("independent", b_root.client)],
            )
            .await;
            let b_exports = exports(&b).await;
            let b_session: process::Client = get_export(&b_exports, "session-1").await;
            let b_commands: vat_client::Client = get_export(&b_exports, "command").await;
            let mut owners = registry.live_snapshots();
            if separate_registries {
                owners.extend(b_registry.live_snapshots());
            }
            assert_eq!(owners.len(), 2);
            assert_eq!(
                owners
                    .iter()
                    .find(|(origin, _)| *origin == a_origin)
                    .unwrap()
                    .1
                    .committed,
                1
            );
            assert_eq!(
                owners
                    .iter()
                    .find(|(origin, _)| *origin != a_origin)
                    .unwrap()
                    .1
                    .committed,
                2,
                "literal owner distribution is [A, B, B]"
            );
            let total_active = || {
                registry.total_active()
                    + if separate_registries {
                        b_registry.total_active()
                    } else {
                        0
                    }
            };
            assert_eq!(total_active(), 3);
            if separate_registries {
                assert_eq!(registry.total_active(), 1, "A's registry owns only A");
                assert_eq!(
                    b_registry.total_active(),
                    2,
                    "B's registry owns both B sessions"
                );
            }
            let denied = command(&b_commands, "deny", 0)
                .await
                .expect_err("attenuation survives A to B delegation");
            assert!(
                denied.extra.contains(membrane::DENIED_MARKER),
                "denial must originate in original policy: {denied}"
            );

            kill(&a).await;
            assert_eq!(
                total_active(),
                2,
                "A wait includes outbound join and budget release"
            );
            assert!(a_session.stdin_request().send().promise.await.is_err());
            command(&b_commands, "touch", 0)
                .await
                .expect("B invokes its retained session after A dies");
            command(&b_commands, "touch", 1)
                .await
                .expect("B independent session remains live");
            eventually("A remote drop and join", || {
                network
                    .remotes
                    .borrow()
                    .first()
                    .is_some_and(|remote| remote.joined.get() && remote.dropped.get())
            })
            .await;
            assert_eq!(network.remotes.borrow().len(), 3);
            assert_eq!(network.remotes.borrow()[1].calls.get(), 2);
            assert_eq!(network.remotes.borrow()[2].calls.get(), 2);

            kill(&b).await;
            assert_eq!(
                total_active(),
                0,
                "both wait barriers recover the shared budget"
            );
            assert!(b_session.stdin_request().send().promise.await.is_err());
            eventually("all remotes drop and join", || {
                network
                    .remotes
                    .borrow()
                    .iter()
                    .all(|remote| remote.joined.get() && remote.dropped.get())
            })
            .await;
            assert!(registry.live_snapshots().is_empty());
            assert!(b_registry.live_snapshots().is_empty());
        }),
    )
    .await
    .expect("real Cell session watchdog");
}

#[tokio::test(flavor = "current_thread")]
async fn a_b_b_real_cell_sessions_follow_invoker_not_delegator() {
    check_a_b_b_ownership(false).await;
}

#[tokio::test(flavor = "current_thread")]
async fn a_b_b_real_cells_delegate_across_independent_registries() {
    check_a_b_b_ownership(true).await;
}

struct ObserveDial(Rc<Cell<usize>>);
impl membrane::Policy for ObserveDial {
    fn check(&self, interface: u64, method: u16) -> capnp::Result<()> {
        DialAndUse.check(interface, method)?;
        if interface == vat_client::Client::TYPE_ID {
            self.0.set(self.0.get() + 1);
        }
        Ok(())
    }
}

// This existing libp2p event executes inside Control::open_stream's first
// poll. Observing it on this single-thread LocalSet proves that cancellation
// cannot take the legal "never started" shortcut in connect_owned.
type OpenEvents = std::sync::Arc<std::sync::Mutex<std::collections::HashMap<String, usize>>>;
fn open_events() -> &'static OpenEvents {
    use tracing_subscriber::prelude::*;
    static EVENTS: std::sync::OnceLock<OpenEvents> = std::sync::OnceLock::new();
    EVENTS.get_or_init(|| {
        let events = OpenEvents::default();
        tracing::subscriber::set_global_default(
            tracing_subscriber::registry().with(OpenEntered(events.clone())),
        )
        .expect("install integration test backend observer");
        events
    })
}
struct OpenEntered(OpenEvents);
impl<S: tracing::Subscriber> tracing_subscriber::Layer<S> for OpenEntered {
    fn on_event(&self, event: &tracing::Event<'_>, _: tracing_subscriber::layer::Context<'_, S>) {
        if event.metadata().target() == "libp2p_stream::control" {
            struct Message {
                matched: bool,
                peer: String,
            }
            impl tracing::field::Visit for Message {
                fn record_debug(
                    &mut self,
                    field: &tracing::field::Field,
                    value: &dyn std::fmt::Debug,
                ) {
                    if field.name() == "message" && format!("{value:?}") == "Requesting new stream"
                    {
                        self.matched = true;
                    }
                    if field.name() == "peer" {
                        self.peer = format!("{value:?}");
                    }
                }
            }
            let mut message = Message {
                matched: false,
                peer: String::new(),
            };
            event.record(&mut message);
            if message.matched {
                *self.0.lock().unwrap().entry(message.peer).or_default() += 1;
            }
        }
    }
}

#[tokio::test(flavor = "current_thread")]
async fn killed_real_cell_queued_dial_cannot_admit_after_resolution() {
    tokio::time::timeout(
        Duration::from_secs(120),
        tokio::task::LocalSet::new().run_until(async {
            let network = Network::new().await;
            let registry = rpc::outbound::OutboundRegistry::default();
            let (executor, _ticked) = executor(registry.clone()).await;
            let root: vat_client::Client = capnp_rpc::new_client(
                rpc::vat_client::VatClientImpl::new(
                    network.control.clone(),
                    authority::EpochGuard::fixed(authority::Epoch::zero()),
                )
                .with_outbound_registry(registry.clone()),
            );
            let (resolve, resolution) = tokio::sync::oneshot::channel();
            let future: vat_client::Client = capnp_rpc::new_future_client(async move {
                resolution
                    .await
                    .map_err(|_| capnp::Error::failed("resolution lost".into()))
            });
            let entered = Rc::new(Cell::new(0));
            let grant: vat_client::Client =
                membrane::membrane(future.clone(), Rc::new(ObserveDial(entered.clone())));
            let a = spawn(&executor, network.peer, vec![("grant", grant.client)]).await;
            eventually("real guest call into unresolved capability", || {
                entered.get() == 1
            })
            .await;
            assert_eq!(
                registry.total_active(),
                0,
                "request is queued before real sink admission"
            );
            kill(&a).await;
            assert!(registry.live_snapshots().is_empty());
            resolve
                .send(root)
                .ok()
                .expect("resolve genuine pending grant after Cell death");
            // Await actual future-client resolution, rather than assuming a
            // fixed number of executor turns was enough to release the queue.
            let _ = capnp::capability::get_resolved_cap(future).await;
            assert_eq!(registry.total_active(), 0);
            assert!(network.remotes.borrow().is_empty());
        }),
    )
    .await
    .expect("queued real Cell watchdog");
}

#[tokio::test(flavor = "current_thread")]
async fn kill_during_real_transport_open_recovers_admission_before_restart() {
    tokio::time::timeout(
        Duration::from_secs(120),
        tokio::task::LocalSet::new().run_until(async {
            let network = Network::with_paused_client(true).await;
            let registry = rpc::outbound::OutboundRegistry::default();
            let (executor, _ticked) = executor(registry.clone()).await;
            let grant: vat_client::Client = capnp_rpc::new_client(
                rpc::vat_client::VatClientImpl::new(
                    network.control.clone(),
                    authority::EpochGuard::fixed(authority::Epoch::zero()),
                )
                .with_outbound_registry(registry.clone()),
            );
            let a = spawn(
                &executor,
                network.peer,
                vec![("grant", grant.client.clone())],
            )
            .await;
            eventually("libp2p open_stream entry event", || {
                open_events().lock().unwrap().get(&network.peer.to_string()).copied() == Some(1)
            }).await;
            assert_eq!(registry.total_active(), 1);
            let before = registry.live_snapshots();
            assert_eq!(before.len(), 1);
            assert_eq!(
                before[0].1.committed, 0,
                "real libp2p open is pending while its underlying transport is paused"
            );
            let old_origin = before[0].0.clone();
            a.kill_request()
                .send()
                .promise
                .await
                .expect("kill while actual libp2p open is pending");
            let wait = a.wait_request().send().promise;
            tokio::pin!(wait);
            let premature = tokio::time::timeout(Duration::from_millis(50), &mut wait).await;
            assert!(premature.is_err(),
                "Process.wait must not report Cleaned before the actual backend returns: result={:?}, active={}",
                premature.map(|result| result.map(|reply| reply.get().unwrap().get_exit_code())), registry.total_active());
            assert_eq!(
                registry.total_active(),
                1,
                "unfinished native attempt retains its permit after kill"
            );
            assert!(registry.live_snapshots().is_empty());
            network.gate.release();
            assert_eq!(
                wait.await
                    .expect("wait after releasing actual backend")
                    .get()
                    .unwrap()
                    .get_exit_code(),
                137
            );
            assert_eq!(registry.total_active(), 0);
            let b = spawn(&executor, network.peer, vec![("grant", grant.client)]).await;
            let b_exports = exports(&b).await;
            let _: process::Client = get_export(&b_exports, "session-0").await;
            let after = registry.live_snapshots();
            assert_eq!(after.len(), 1);
            assert_ne!(
                after[0].0, old_origin,
                "a new Cell generation cannot reuse the retired ingress identity"
            );
            assert_eq!(after[0].1.committed, 1);
            kill(&b).await;
            assert_eq!(registry.total_active(), 0);
            eventually("cancelled and fresh remote drop and join", || {
                network
                    .remotes
                    .borrow()
                    .iter()
                    .all(|remote| remote.joined.get() && remote.dropped.get())
            })
            .await;
        }),
    )
    .await
    .expect("negotiating real Cell watchdog");
}

async fn terminal_guest_session(operation: &str) {
    let network = Network::new().await;
    let registry = rpc::outbound::OutboundRegistry::default();
    let (executor, _ticked) = executor(registry.clone()).await;
    let grant: vat_client::Client = capnp_rpc::new_client(
        rpc::vat_client::VatClientImpl::new(
            network.control.clone(),
            authority::EpochGuard::fixed(authority::Epoch::zero()),
        )
        .with_outbound_registry(registry.clone()),
    );
    let child = spawn(&executor, network.peer, vec![("grant", grant.client)]).await;
    let child_exports = exports(&child).await;
    let session: process::Client = get_export(&child_exports, "session-0").await;
    let commands: vat_client::Client = get_export(&child_exports, "command").await;
    assert_eq!(registry.total_active(), 1);
    // The command's own reply may lose the race to main-RPC teardown.
    let _ = command(&commands, operation, 0).await;
    let exit = child
        .wait_request()
        .send()
        .promise
        .await
        .expect("terminal guest Process.wait")
        .get()
        .unwrap()
        .get_exit_code();
    if operation == "exit" {
        assert_eq!(exit, 0);
    } else {
        assert_ne!(exit, 0);
    }
    assert_eq!(
        registry.total_active(),
        0,
        "Process.wait includes native outbound teardown"
    );
    assert!(registry.live_snapshots().is_empty());
    assert!(session.stdin_request().send().promise.await.is_err());
    eventually("terminal guest remote drop and join", || {
        network
            .remotes
            .borrow()
            .iter()
            .all(|remote| remote.joined.get() && remote.dropped.get())
    })
    .await;
}

#[tokio::test(flavor = "current_thread")]
async fn normal_real_guest_exit_joins_committed_outbound_session() {
    tokio::time::timeout(
        Duration::from_secs(120),
        tokio::task::LocalSet::new().run_until(terminal_guest_session("exit")),
    )
    .await
    .expect("normal exit watchdog");
}

#[tokio::test(flavor = "current_thread")]
async fn real_guest_trap_joins_committed_outbound_session() {
    tokio::time::timeout(
        Duration::from_secs(120),
        tokio::task::LocalSet::new().run_until(terminal_guest_session("trap")),
    )
    .await
    .expect("trap watchdog");
}

fn fixture_wasm() -> Vec<u8> {
    std::fs::read(
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("target/gate2b-root/wasm32-wasip3/release/gate2b_root_probe.wasm"),
    )
    .expect("build the root guest with make gate2b-root-probe before this suite")
}

fn block_cid(codec: u64, bytes: &[u8]) -> cid::Cid {
    use sha2::Digest;
    cid::Cid::new_v1(
        codec,
        cid::multihash::Multihash::wrap(0x12, &sha2::Sha256::digest(bytes)).unwrap(),
    )
}

// A literal one-entry UnixFS directory, using the standard PBNode/PBLink
// layout. Selection and CID verification remain the production selector's job.
fn directory(name: &str, target: cid::Cid, size: u64) -> Vec<u8> {
    fn varint(mut value: u64, out: &mut Vec<u8>) {
        while value >= 128 {
            out.push((value as u8 & 127) | 128);
            value >>= 7;
        }
        out.push(value as u8);
    }
    fn field(tag: u8, bytes: &[u8], out: &mut Vec<u8>) {
        out.push(tag);
        varint(bytes.len() as u64, out);
        out.extend_from_slice(bytes);
    }
    let mut link = Vec::new();
    field(10, &target.to_bytes(), &mut link);
    field(18, name.as_bytes(), &mut link);
    link.push(24);
    varint(size, &mut link);
    let mut node = Vec::new();
    field(18, &link, &mut node);
    field(10, &[8, 1], &mut node);
    node
}

async fn root_artifact() -> (cid::Cid, ww::ipfs::HttpClient, tokio::task::JoinHandle<()>) {
    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
    let wasm = fixture_wasm();
    fn varint(mut value: u64, out: &mut Vec<u8>) {
        while value >= 128 {
            out.push((value as u8 & 127) | 128);
            value >>= 7;
        }
        out.push(value as u8);
    }
    fn field(tag: u8, bytes: &[u8], out: &mut Vec<u8>) {
        out.push(tag);
        varint(bytes.len() as u64, out);
        out.extend_from_slice(bytes);
    }
    // Production selection caps each block at 1 MiB. Use a standard chunked
    // UnixFS file rather than weakening the selector for this 1.5 MiB guest.
    let mut blocks = Vec::new();
    let mut file_node = Vec::new();
    let mut data = vec![8, 2, 24];
    varint(wasm.len() as u64, &mut data);
    for chunk in wasm.chunks(256 * 1024) {
        let cid = block_cid(0x55, chunk);
        let mut link = Vec::new();
        field(10, &cid.to_bytes(), &mut link);
        link.push(24);
        varint(chunk.len() as u64, &mut link);
        field(18, &link, &mut file_node);
        data.push(32);
        varint(chunk.len() as u64, &mut data);
        blocks.push((cid.to_string(), chunk.to_vec()));
    }
    field(10, &data, &mut file_node);
    let file = block_cid(0x70, &file_node);
    let file_size = wasm.len() + file_node.len();
    let boot = directory("kernel.wasm", file, file_size as u64);
    let boot_cid = block_cid(0x70, &boot);
    let root = directory("boot", boot_cid, (boot.len() + file_size) as u64);
    let root_cid = block_cid(0x70, &root);
    blocks.extend([
        (file.to_string(), file_node),
        (boot_cid.to_string(), boot),
        (root_cid.to_string(), root),
    ]);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let client = ww::ipfs::HttpClient::new(format!("http://{}", listener.local_addr().unwrap()));
    let task = tokio::task::spawn_local(async move {
        loop {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut request = vec![0; 8192];
            let count = socket.read(&mut request).await.unwrap();
            let request = String::from_utf8_lossy(&request[..count]);
            let (_, block) = blocks
                .iter()
                .find(|(cid, _)| request.contains(cid))
                .expect("only fixture block/get requests");
            let header = format!(
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                block.len()
            );
            socket.write_all(header.as_bytes()).await.unwrap();
            socket.write_all(block).await.unwrap();
        }
    });
    (root_cid, client, task)
}

#[tokio::test(flavor = "current_thread")]
async fn root_guest_generation_joins_real_outbound_session_before_retirement() {
    tokio::time::timeout(
        Duration::from_secs(120),
        tokio::task::LocalSet::new().run_until(async {
            use std::sync::Arc;
            use tokio::sync::{mpsc, oneshot, watch};
            use ww::kernel;

            let network = Network::new().await;
            let (root, backend, backend_task) = root_artifact().await;
            let boot_client =
                ww::ipfs::BootClient::one_attempt(backend.clone(), Duration::from_secs(10));
            let (_cancel_tx, mut cancel_rx) = watch::channel(false);
            let file = cell::image::select_kernel_file(root, &boot_client, &mut cancel_rx)
                .await
                .expect("select verified real guest /boot/kernel.wasm");
            let ticked = ticked_executor::TickedExecutor::new();
            let prepared = kernel::PreparedKernel::prepare(
                file,
                &kernel::Compilation::default(),
                ticked.runtime_engine(),
            )
            .await
            .expect("production kernel component validation");
            let resources = support::filesystem_resources();
            let (tree_scope, tree_owner, tree_finalizer, tree_completion) =
                resources.try_filesystem_scope().unwrap();
            let tree_finalizer = tokio::task::spawn_local(tree_finalizer.run());
            let staging = tempfile::tempdir().unwrap();
            let tree = Arc::new(cell::vfs::CidTree::new(
                root,
                backend.clone(),
                staging.path().to_owned(),
                resources.clone(),
                tree_scope,
            ));
            let pins = Arc::new(
                cache::PinsetCache::new(
                    Arc::new(ww::deployment::RetainedPinner::new(backend)),
                    1024,
                    resources.clone(),
                )
                .await
                .unwrap(),
            );
            let (_epoch_tx, epoch_rx) = watch::channel(authority::Epoch {
                seq: 1,
                head: Vec::new(),
                root: Some(root),
            });
            let identity = kernel::IdentityState::pending(epoch_rx.clone());
            let ready = Arc::new(authority::KernelReadyGate::new(epoch_rx.clone()));
            let (compile_tx, _compile_rx) = mpsc::channel(1);
            let (swarm_tx, mut swarm_rx) = mpsc::channel(1);
            let peer = network.peer;
            let provider = tokio::task::spawn_local(async move {
                match swarm_rx.recv().await.expect("root Finder request") {
                    rpc::SwarmCommand::KadFindProviders { reply, count, .. } => {
                        assert_eq!(count, 1);
                        reply
                            .send(rpc::PeerInfo {
                                peer_id: peer.to_bytes(),
                                addrs: Vec::new(),
                            })
                            .await
                            .unwrap();
                    }
                    _ => panic!("root guest must discover its test peer using the granted Finder"),
                }
            });
            let (terminate_tx, terminate_rx) = watch::channel(());
            let (constructed_tx, constructed_rx) = oneshot::channel();
            let (admit_tx, admit_rx) = oneshot::channel();
            let generation = kernel::Generation::new(
                prepared,
                kernel::Root::new(root, tree),
                authority::EpochGuard {
                    issued_seq: 1,
                    receiver: epoch_rx,
                },
                identity.clone(),
                ready.clone(),
                kernel::Bootstrap::new(
                    rpc::NetworkState::from_peer_id(network.local_peer.to_bytes()),
                    swarm_tx,
                    Arc::new(ed25519_dalek::SigningKey::from_bytes(&[7; 32])),
                    network.control.clone(),
                    Vec::new(),
                ),
                kernel::RuntimeInputs::new(
                    false,
                    ticked.runtime_engine(),
                    compile_tx,
                    rpc::CachePolicy::Shared,
                    pins,
                    resources,
                ),
                kernel::Stdio::Closed,
                terminate_rx,
                kernel::LaunchAdmission {
                    constructed: constructed_tx,
                    admitted: admit_rx,
                },
            )
            .unwrap();
            let effects = generation.effects_completion();
            let filesystem = generation.filesystem_completion();
            let rpc = generation.rpc_completion();
            let worker = tokio::task::spawn_local(generation.run());
            constructed_rx.await.unwrap();
            admit_tx.send(()).unwrap();
            eventually("root guest's real session method call", || {
                network
                    .remotes
                    .borrow()
                    .first()
                    .is_some_and(|remote| remote.calls.get() == 1)
            })
            .await;
            provider.await.unwrap();
            assert!(identity.get().is_some());
            assert_eq!(effects.snapshot().committed, 1);
            assert_eq!(effects.snapshot().active, 1);
            assert!(!effects.snapshot().closed);
            terminate_tx.send(()).unwrap();
            assert_eq!(
                worker
                    .await
                    .unwrap()
                    .expect("root Generation joins cleanup"),
                kernel::Outcome::Terminated
            );
            // Generation::run returning is itself the cleanup barrier. Do not
            // await a second receipt before testing that promise.
            let done = effects.snapshot();
            assert!(done.closed);
            assert!(!done.failed);
            assert_eq!(done.active, 0);
            assert_eq!(done.committed, 1);
            assert_eq!(done.joined, 1);
            assert_eq!(rpc.state(), rpc::CleanupState::Cleaned(0));
            assert_eq!(rpc.wait().await, Ok(0));
            filesystem.wait().await.unwrap();
            assert!(identity.get().is_none());
            eventually("root remote session drop and join", || {
                network
                    .remotes
                    .borrow()
                    .iter()
                    .all(|remote| remote.joined.get() && remote.dropped.get())
            })
            .await;
            drop(tree_owner);
            tree_completion.wait().await.unwrap();
            tree_finalizer.await.unwrap().unwrap();
            backend_task.abort();
        }),
    )
    .await
    .expect("root generation real session watchdog");
}

/// A real P3 Cell submits to an actual, deliberately unpolled Swarm. No handler
/// consumes its preconnection queue. The cleanup deadline is a failure result,
/// never a claim that the queued native request stopped.
#[tokio::test(flavor = "current_thread")]
async fn stalled_swarm_cleanup_fails_without_releasing_pending_cell_receipt() {
    tokio::time::timeout(
        Duration::from_secs(90),
        tokio::task::LocalSet::new().run_until(async {
            let _ = open_events();
            let mut stalled = swarm(Default::default());
            let peer = libp2p::PeerId::random();
            let registry = rpc::outbound::OutboundRegistry::default();
            let (executor, _ticked) = executor(registry.clone()).await;
            let grant: vat_client::Client = capnp_rpc::new_client(
                rpc::vat_client::VatClientImpl::new(
                    stalled.behaviour().new_control(),
                    authority::EpochGuard::fixed(authority::Epoch::zero()),
                )
                .with_outbound_registry(registry.clone()),
            );
            let a = spawn(&executor, peer, vec![("grant", grant.client)]).await;
            eventually("real libp2p request entered unpolled Swarm", || {
                open_events()
                    .lock()
                    .unwrap()
                    .get(&peer.to_string())
                    .copied()
                    == Some(1)
            })
            .await;
            assert_eq!(registry.total_active(), 1);
            a.kill_request().send().promise.await.unwrap();
            let result = a.wait_request().send().promise.await;
            assert!(
                result.is_err(),
                "unresolved native work must not certify Cleaned(exit)"
            );
            assert_eq!(
                registry.total_active(),
                1,
                "cleanup timeout must retain native receipt"
            );
            assert!(
                registry.live_snapshots().is_empty(),
                "closed Cell cannot admit again"
            );

            // Polling the same Swarm now produces the explicit no-address dial
            // failure. Only that actual result plus the worker join releases capacity.
            let driver =
                tokio::task::spawn_local(async move { while stalled.next().await.is_some() {} });
            eventually("explicit native dial failure discharged receipt", || {
                registry.total_active() == 0
            })
            .await;
            assert!(
                a.wait_request().send().promise.await.is_err(),
                "a previously failed Cell cleanup must not become Cleaned retroactively"
            );
            driver.abort();
            let _ = driver.await;
        }),
    )
    .await
    .expect("stalled Swarm P3 cleanup watchdog");
}

/// A real P3 Cell closes after an allocated outbound Yamux stream attempts its
/// actual V1 multistream header. The native handler's real Timeout event, not
/// elapsed caller time or a synthetic failed future, permits eventual cleanup.
#[tokio::test(flavor = "current_thread")]
async fn killed_real_cell_joins_native_timeout_with_transport_still_blocked() {
    tokio::time::timeout(
        Duration::from_secs(90),
        tokio::task::LocalSet::new().run_until(async {
            use std::sync::atomic::Ordering;
            let network = Network::with_paused_client(true).await;
            let registry = rpc::outbound::OutboundRegistry::default();
            let (executor, _ticked) = executor(registry.clone()).await;
            let grant: vat_client::Client = capnp_rpc::new_client(
                rpc::vat_client::VatClientImpl::new(
                    network.control.clone(),
                    authority::EpochGuard::fixed(authority::Epoch::zero()),
                )
                .with_outbound_registry(registry.clone()),
            );
            let a = spawn(&executor, network.peer, vec![("grant", grant.client)]).await;
            eventually("actual outbound multistream header attempt", || {
                network.gate.negotiation.started.load(Ordering::SeqCst) == 1
            })
            .await;
            let started = std::time::Instant::now();
            assert_eq!(registry.total_active(), 1);
            let before = registry.live_snapshots();
            assert_eq!(before.len(), 1);
            assert_eq!(before[0].1.committed, 0);
            assert_eq!(
                network.gate.negotiation.negotiated.load(Ordering::SeqCst),
                0
            );
            assert_eq!(network.gate.negotiation.timed_out.load(Ordering::SeqCst), 0);
            assert!(network.remotes.borrow().is_empty());
            let native_timeout = network.gate.negotiation.timeout_event.notified();
            tokio::pin!(native_timeout);
            native_timeout.as_mut().enable();
            a.kill_request()
                .send()
                .promise
                .await
                .expect("kill actual negotiating P3 Cell");
            eventually(
                "closed Cell origin retired while native work stays owned",
                || registry.live_snapshots().is_empty(),
            )
            .await;
            assert_eq!(registry.total_active(), 1);
            let wait = a.wait_request().send().promise;
            tokio::pin!(wait);
            // Poll Process.wait throughout the pending native operation. If a
            // lifecycle barrier is removed, an earlier result is an assertion
            // failure; task ordering after the actual timeout may select either arm.
            let result = tokio::select! {
                result = &mut wait => {
                    assert_eq!(network.gate.negotiation.timed_out.load(Ordering::SeqCst), 1,
                        "Process.wait completed before the actual native Timeout event");
                    result
                }
                _ = &mut native_timeout => wait.await,
            };
            assert_eq!(
                result
                    .expect("Cell cleanup after native terminal error and worker join")
                    .get()
                    .unwrap()
                    .get_exit_code(),
                137
            );
            assert!(
                network.gate.paused.load(Ordering::SeqCst),
                "transport gate was never released"
            );
            assert_eq!(network.gate.negotiation.timed_out.load(Ordering::SeqCst), 1);
            assert_eq!(
                network.gate.negotiation.negotiated.load(Ordering::SeqCst),
                0,
                "no successful negotiation exists from which a session could commit"
            );
            assert_eq!(
                registry.total_active(),
                0,
                "Process.wait must include actual attempt-worker join and admission release"
            );
            assert!(registry.live_snapshots().is_empty());
            assert!(
                network.remotes.borrow().is_empty(),
                "no accepted remote session or RPC Bootstrap traffic"
            );
            eprintln!(
                "real P3 native-timeout cleanup completed after {:?}; gate remained held",
                started.elapsed()
            );
        }),
    )
    .await
    .expect("real P3 native-timeout cleanup watchdog");
}
