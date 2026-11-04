//! Qualification against the pinned, unmodified libp2p handler and memory transport.
//! The wrappers witness handler events and actual multistream header writes,
//! and gate transport/result delivery.
//! Watchdog timeouts detect hangs; channels and handler events establish ordering.

use std::{
    cell::Cell,
    future::Future,
    pin::Pin,
    rc::Rc,
    sync::{
        atomic::{AtomicBool, AtomicUsize, Ordering},
        Arc,
    },
    task::{Context, Poll},
    time::Duration,
};

use authority::system_capnp::process;
use futures::{
    future::poll_fn,
    io::{AsyncRead, AsyncReadExt, AsyncWrite},
    task::AtomicWaker,
    StreamExt,
};
use libp2p::{
    core::{
        muxing::{StreamMuxer, StreamMuxerBox, StreamMuxerEvent},
        transport::PortUse,
        Endpoint,
    },
    swarm::{
        handler::{ConnectionEvent, FullyNegotiatedOutbound},
        ConnectionDenied, ConnectionHandler, ConnectionHandlerEvent, ConnectionId, FromSwarm,
        NetworkBehaviour, SubstreamProtocol, THandler, THandlerInEvent, THandlerOutEvent, ToSwarm,
    },
    Multiaddr, PeerId, Stream, StreamProtocol, Swarm, Transport,
};
use rpc::{
    managed_rpc::{ManagedRpc, ManagedRpcSystem},
    outbound::{CellEffects, EffectLimits, OutboundRegistry},
    vat_dial::{connect_libp2p_owned, connect_owned, OwnedDial},
};
use tokio::sync::{mpsc, oneshot, Notify};

const PROTOCOL: StreamProtocol = StreamProtocol::new("/pending-negotiation-test/1");
const WATCHDOG: Duration = Duration::from_secs(20);

async fn bounded<T>(future: impl Future<Output = T>) -> T {
    tokio::time::timeout(WATCHDOG, future)
        .await
        .expect("real-swarm test watchdog")
}

#[derive(Default)]
struct Observation {
    emitted: AtomicUsize,
    handshake_started: AtomicUsize,
    negotiated: AtomicUsize,
    failed: AtomicUsize,
    dropped: AtomicUsize,
    changed: Notify,
    hold_result: AtomicBool,
    result_waker: AtomicWaker,
    // Zero preserves the dependency's default. Nonzero uses the public handler API.
    timeout_millis: AtomicUsize,
    substream_gate: Arc<TransportGate>,
    substreams_dropped: AtomicUsize,
    timed_out: AtomicUsize,
    hold_dequeue: AtomicBool,
    hold_emission: AtomicBool,
    held_requests: AtomicUsize,
    submission_waker: AtomicWaker,
}

impl Observation {
    fn record(&self, counter: &AtomicUsize) {
        counter.fetch_add(1, Ordering::SeqCst);
        self.changed.notify_waiters();
    }

    async fn wait_for(&self, counter: &AtomicUsize, value: usize) {
        bounded(async {
            loop {
                let notified = self.changed.notified();
                tokio::pin!(notified);
                notified.as_mut().enable();
                if counter.load(Ordering::SeqCst) >= value {
                    return;
                }
                notified.await;
            }
        })
        .await;
    }

    fn release_result(&self) {
        self.hold_result.store(false, Ordering::SeqCst);
        self.result_waker.wake();
    }
}

#[derive(Default)]
struct TransportGate {
    held: AtomicBool,
    blocked_writes: AtomicUsize,
    read: AtomicWaker,
    write: AtomicWaker,
    changed: Notify,
}

impl TransportGate {
    fn hold(&self) {
        self.held.store(true, Ordering::SeqCst);
    }

    fn release(&self) {
        self.held.store(false, Ordering::SeqCst);
        self.read.wake();
        self.write.wake();
    }

    async fn wait_for_blocked_write(&self) {
        bounded(async {
            loop {
                let notified = self.changed.notified();
                tokio::pin!(notified);
                notified.as_mut().enable();
                if self.blocked_writes.load(Ordering::SeqCst) != 0 {
                    return;
                }
                notified.await;
            }
        })
        .await;
    }
}

struct GatedTransport<S> {
    inner: S,
    gate: Arc<TransportGate>,
}

impl<S: AsyncRead + Unpin> AsyncRead for GatedTransport<S> {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buffer: &mut [u8],
    ) -> Poll<std::io::Result<usize>> {
        self.gate.read.register(cx.waker());
        if self.gate.held.load(Ordering::SeqCst) {
            return Poll::Pending;
        }
        Pin::new(&mut self.inner).poll_read(cx, buffer)
    }
}

impl<S: AsyncWrite + Unpin> AsyncWrite for GatedTransport<S> {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buffer: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        self.gate.write.register(cx.waker());
        if self.gate.held.load(Ordering::SeqCst) {
            self.gate.blocked_writes.fetch_add(1, Ordering::SeqCst);
            self.gate.changed.notify_waiters();
            return Poll::Pending;
        }
        Pin::new(&mut self.inner).poll_write(cx, buffer)
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        self.gate.write.register(cx.waker());
        if self.gate.held.load(Ordering::SeqCst) {
            return Poll::Pending;
        }
        Pin::new(&mut self.inner).poll_flush(cx)
    }

    fn poll_close(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.inner).poll_close(cx)
    }
}

/// Witness the unencrypted outbound substream after Yamux allocation. The first
/// write comes from StreamUpgrade::new_outbound -> dialer_select_proto, before
/// libp2p-stream's Upgrade can return a stream or RPC can write application bytes.
struct WitnessSubstream<S> {
    inner: S,
    observation: Arc<Observation>,
    first_outbound_write: bool,
    gate: Option<Arc<TransportGate>>,
}

impl<S> Drop for WitnessSubstream<S> {
    fn drop(&mut self) {
        self.observation
            .record(&self.observation.substreams_dropped);
    }
}

impl<S: AsyncRead + Unpin> AsyncRead for WitnessSubstream<S> {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buffer: &mut [u8],
    ) -> Poll<std::io::Result<usize>> {
        if let Some(gate) = &self.gate {
            gate.read.register(cx.waker());
            if gate.held.load(Ordering::SeqCst) {
                return Poll::Pending;
            }
        }
        Pin::new(&mut self.inner).poll_read(cx, buffer)
    }
}

impl<S: AsyncWrite + Unpin> AsyncWrite for WitnessSubstream<S> {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buffer: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        if self.first_outbound_write && !buffer.is_empty() {
            // Pinned multistream-select 0.14.0 LengthDelimited::poll_write_buffer
            // sends the complete buffered V1 header in its first scalar write.
            assert!(buffer.starts_with(b"\x13/multistream/1.0.0\n"));
            self.first_outbound_write = false;
            self.observation.record(&self.observation.handshake_started);
        }
        if let Some(gate) = &self.gate {
            gate.write.register(cx.waker());
            if gate.held.load(Ordering::SeqCst) {
                gate.blocked_writes.fetch_add(1, Ordering::SeqCst);
                gate.changed.notify_waiters();
                return Poll::Pending;
            }
        }
        Pin::new(&mut self.inner).poll_write(cx, buffer)
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.inner).poll_flush(cx)
    }

    fn poll_close(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.inner).poll_close(cx)
    }
}

/// Delegate every muxer operation to the real Yamux instance. Only substream
/// I/O is wrapped; the witness does not synthesize handler or negotiation events.
struct WitnessMuxer<M> {
    inner: M,
    observation: Arc<Observation>,
}

impl<M: StreamMuxer + Unpin> StreamMuxer for WitnessMuxer<M>
where
    M::Substream: Unpin,
{
    type Substream = WitnessSubstream<M::Substream>;
    type Error = M::Error;

    fn poll_inbound(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Result<Self::Substream, Self::Error>> {
        Pin::new(&mut self.inner)
            .poll_inbound(cx)
            .map_ok(|inner| WitnessSubstream {
                inner,
                observation: self.observation.clone(),
                first_outbound_write: false,
                gate: None,
            })
    }

    fn poll_outbound(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Result<Self::Substream, Self::Error>> {
        Pin::new(&mut self.inner)
            .poll_outbound(cx)
            .map_ok(|inner| WitnessSubstream {
                inner,
                observation: self.observation.clone(),
                first_outbound_write: true,
                gate: self
                    .observation
                    .substream_gate
                    .held
                    .load(Ordering::SeqCst)
                    .then(|| self.observation.substream_gate.clone()),
            })
    }

    fn poll_close(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Pin::new(&mut self.inner).poll_close(cx)
    }

    fn poll(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Result<StreamMuxerEvent, Self::Error>> {
        Pin::new(&mut self.inner).poll(cx)
    }
}

type InnerHandler = <libp2p_stream::Behaviour as NetworkBehaviour>::ConnectionHandler;
type OutboundProtocol = <InnerHandler as ConnectionHandler>::OutboundProtocol;
type OutboundInfo = <InnerHandler as ConnectionHandler>::OutboundOpenInfo;

struct WitnessHandler {
    inner: InnerHandler,
    observation: Arc<Observation>,
    pending_result: Option<FullyNegotiatedOutbound<OutboundProtocol, OutboundInfo>>,
    pending_request: Option<SubstreamProtocol<OutboundProtocol, OutboundInfo>>,
}

impl Drop for WitnessHandler {
    fn drop(&mut self) {
        self.observation.record(&self.observation.dropped);
    }
}

impl ConnectionHandler for WitnessHandler {
    type FromBehaviour = <InnerHandler as ConnectionHandler>::FromBehaviour;
    type ToBehaviour = <InnerHandler as ConnectionHandler>::ToBehaviour;
    type InboundProtocol = <InnerHandler as ConnectionHandler>::InboundProtocol;
    type OutboundProtocol = OutboundProtocol;
    type InboundOpenInfo = <InnerHandler as ConnectionHandler>::InboundOpenInfo;
    type OutboundOpenInfo = OutboundInfo;

    fn listen_protocol(&self) -> SubstreamProtocol<Self::InboundProtocol, Self::InboundOpenInfo> {
        self.inner.listen_protocol()
    }

    fn connection_keep_alive(&self) -> bool {
        self.inner.connection_keep_alive()
    }

    fn poll(
        &mut self,
        cx: &mut Context<'_>,
    ) -> Poll<
        ConnectionHandlerEvent<Self::OutboundProtocol, Self::OutboundOpenInfo, Self::ToBehaviour>,
    > {
        self.observation.submission_waker.register(cx.waker());
        if self.pending_request.is_some() {
            if self.observation.hold_emission.load(Ordering::SeqCst) {
                return Poll::Pending;
            }
            let protocol = self.pending_request.take().unwrap();
            self.observation.record(&self.observation.emitted);
            return Poll::Ready(ConnectionHandlerEvent::OutboundSubstreamRequest { protocol });
        }
        if self.observation.hold_dequeue.load(Ordering::SeqCst) {
            return Poll::Pending;
        }
        self.observation.result_waker.register(cx.waker());
        if self.pending_result.is_some() {
            if self.observation.hold_result.load(Ordering::SeqCst) {
                return Poll::Pending;
            }
            self.inner
                .on_connection_event(ConnectionEvent::FullyNegotiatedOutbound(
                    self.pending_result.take().unwrap(),
                ));
        }
        let event = self.inner.poll(cx);
        if let Poll::Ready(ConnectionHandlerEvent::OutboundSubstreamRequest { protocol }) = event {
            let millis = self.observation.timeout_millis.load(Ordering::SeqCst);
            let protocol = if millis == 0 {
                protocol
            } else {
                protocol.with_timeout(Duration::from_millis(millis as u64))
            };
            if self.observation.hold_emission.load(Ordering::SeqCst) {
                self.pending_request = Some(protocol);
                self.observation.record(&self.observation.held_requests);
                return Poll::Pending;
            }
            self.observation.record(&self.observation.emitted);
            return Poll::Ready(ConnectionHandlerEvent::OutboundSubstreamRequest { protocol });
        }
        event
    }

    fn poll_close(&mut self, cx: &mut Context<'_>) -> Poll<Option<Self::ToBehaviour>> {
        self.inner.poll_close(cx)
    }

    fn on_behaviour_event(&mut self, event: Self::FromBehaviour) {
        self.inner.on_behaviour_event(event);
    }

    fn on_connection_event(
        &mut self,
        event: ConnectionEvent<
            Self::InboundProtocol,
            Self::OutboundProtocol,
            Self::InboundOpenInfo,
            Self::OutboundOpenInfo,
        >,
    ) {
        match event {
            ConnectionEvent::FullyNegotiatedOutbound(event) => {
                self.observation.record(&self.observation.negotiated);
                if self.observation.hold_result.load(Ordering::SeqCst) {
                    assert!(self.pending_result.replace(event).is_none());
                } else {
                    self.inner
                        .on_connection_event(ConnectionEvent::FullyNegotiatedOutbound(event));
                }
            }
            ConnectionEvent::DialUpgradeError(event) => {
                if matches!(event.error, libp2p::swarm::StreamUpgradeError::Timeout) {
                    self.observation.record(&self.observation.timed_out);
                }
                self.observation.record(&self.observation.failed);
                self.inner
                    .on_connection_event(ConnectionEvent::DialUpgradeError(event));
            }
            event => self.inner.on_connection_event(event),
        }
    }
}

struct WitnessBehaviour {
    inner: libp2p_stream::Behaviour,
    observation: Arc<Observation>,
}

impl WitnessBehaviour {
    fn wrap(&self, inner: InnerHandler) -> WitnessHandler {
        WitnessHandler {
            inner,
            observation: self.observation.clone(),
            pending_result: None,
            pending_request: None,
        }
    }
}

impl NetworkBehaviour for WitnessBehaviour {
    type ConnectionHandler = WitnessHandler;
    type ToSwarm = <libp2p_stream::Behaviour as NetworkBehaviour>::ToSwarm;

    fn handle_pending_inbound_connection(
        &mut self,
        id: ConnectionId,
        local: &Multiaddr,
        remote: &Multiaddr,
    ) -> Result<(), ConnectionDenied> {
        self.inner
            .handle_pending_inbound_connection(id, local, remote)
    }

    fn handle_established_inbound_connection(
        &mut self,
        id: ConnectionId,
        peer: PeerId,
        local: &Multiaddr,
        remote: &Multiaddr,
    ) -> Result<THandler<Self>, ConnectionDenied> {
        let inner = self
            .inner
            .handle_established_inbound_connection(id, peer, local, remote)?;
        Ok(self.wrap(inner))
    }

    fn handle_pending_outbound_connection(
        &mut self,
        id: ConnectionId,
        peer: Option<PeerId>,
        addresses: &[Multiaddr],
        role: Endpoint,
    ) -> Result<Vec<Multiaddr>, ConnectionDenied> {
        self.inner
            .handle_pending_outbound_connection(id, peer, addresses, role)
    }

    fn handle_established_outbound_connection(
        &mut self,
        id: ConnectionId,
        peer: PeerId,
        address: &Multiaddr,
        role: Endpoint,
        port: PortUse,
    ) -> Result<THandler<Self>, ConnectionDenied> {
        let inner = self
            .inner
            .handle_established_outbound_connection(id, peer, address, role, port)?;
        Ok(self.wrap(inner))
    }

    fn on_swarm_event(&mut self, event: FromSwarm) {
        self.inner.on_swarm_event(event);
    }

    fn on_connection_handler_event(
        &mut self,
        peer: PeerId,
        connection: ConnectionId,
        event: THandlerOutEvent<Self>,
    ) {
        self.inner
            .on_connection_handler_event(peer, connection, event);
    }

    fn poll(
        &mut self,
        cx: &mut Context<'_>,
    ) -> Poll<ToSwarm<Self::ToSwarm, THandlerInEvent<Self>>> {
        self.inner.poll(cx)
    }
}

fn swarm(gate: Arc<TransportGate>, observation: Arc<Observation>) -> Swarm<WitnessBehaviour> {
    let key = libp2p::identity::Keypair::generate_ed25519();
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
                StreamMuxerBox::new(WitnessMuxer {
                    inner,
                    observation: muxer_observation.clone(),
                }),
            )
        })
        .boxed();
    Swarm::new(
        transport,
        WitnessBehaviour {
            inner: libp2p_stream::Behaviour::new(),
            observation,
        },
        key.public().to_peer_id(),
        libp2p::swarm::Config::with_tokio_executor()
            .with_idle_connection_timeout(Duration::from_secs(120)),
    )
}

enum Command {
    Disconnect(oneshot::Sender<()>),
}

struct Network {
    peer: PeerId,
    control: libp2p_stream::Control,
    server_control: libp2p_stream::Control,
    incoming: libp2p_stream::IncomingStreams,
    gate: Arc<TransportGate>,
    observation: Arc<Observation>,
    commands: mpsc::Sender<Command>,
    client_task: tokio::task::JoinHandle<()>,
    server_task: tokio::task::JoinHandle<()>,
}

impl Drop for Network {
    fn drop(&mut self) {
        self.client_task.abort();
        self.server_task.abort();
    }
}

impl Network {
    async fn new() -> Self {
        let mut server = swarm(Arc::default(), Arc::default());
        let peer = *server.local_peer_id();
        let mut server_control = server.behaviour().inner.new_control();
        let incoming = server_control.accept(PROTOCOL).unwrap();
        server.listen_on("/memory/0".parse().unwrap()).unwrap();
        let address = bounded(async {
            loop {
                if let libp2p::swarm::SwarmEvent::NewListenAddr { address, .. } =
                    server.select_next_some().await
                {
                    break address;
                }
            }
        })
        .await;
        let gate = Arc::new(TransportGate::default());
        let observation = Arc::new(Observation::default());
        let mut client = swarm(gate.clone(), observation.clone());
        let control = client.behaviour().inner.new_control();
        client.dial(address).unwrap();
        let server_task =
            tokio::task::spawn_local(async move { while server.next().await.is_some() {} });
        bounded(async {
            while !matches!(
                client.select_next_some().await,
                libp2p::swarm::SwarmEvent::ConnectionEstablished { .. }
            ) {}
        })
        .await;
        let (commands, mut command_rx) = mpsc::channel(1);
        let client_task = tokio::task::spawn_local(async move {
            loop {
                tokio::select! {
                    biased;
                    Some(Command::Disconnect(ack)) = command_rx.recv() => {
                        client.disconnect_peer_id(peer).unwrap();
                        let _ = ack.send(());
                    }
                    event = client.next() => if event.is_none() { break; },
                }
            }
        });
        Self {
            peer,
            control,
            server_control,
            incoming,
            gate,
            observation,
            commands,
            client_task,
            server_task,
        }
    }

    fn dial(&self, registry: &OutboundRegistry, owner: &CellEffects) -> OwnedDial<process::Client> {
        connect_libp2p_owned(
            registry,
            Some(owner.origin()),
            self.control.clone(),
            self.peer,
            PROTOCOL,
        )
        .unwrap()
    }

    async fn active_negotiation(&self, count: usize) {
        self.observation
            .wait_for(&self.observation.emitted, count)
            .await;
        self.observation
            .wait_for(&self.observation.handshake_started, count)
            .await;
        self.gate.wait_for_blocked_write().await;
        assert!(self.gate.held.load(Ordering::SeqCst));
        assert_eq!(self.observation.negotiated.load(Ordering::SeqCst), 0);
        assert_eq!(self.observation.failed.load(Ordering::SeqCst), 0);
        assert_eq!(self.observation.dropped.load(Ordering::SeqCst), 0);
    }

    async fn next_stream(&mut self) -> Stream {
        bounded(self.incoming.next()).await.unwrap().1
    }

    async fn disconnect(&self) {
        let (send, receive) = oneshot::channel();
        self.commands.send(Command::Disconnect(send)).await.unwrap();
        bounded(receive).await.unwrap();
        self.observation
            .wait_for(&self.observation.dropped, 1)
            .await;
    }
}

fn registry(total: usize) -> OutboundRegistry {
    OutboundRegistry::new(EffectLimits {
        max_owners: 4,
        max_effects_per_cell: total,
        max_total_effects: total,
    })
}

async fn assert_pending(future: impl Future) {
    tokio::pin!(future);
    poll_fn(|cx| {
        assert!(future.as_mut().poll(cx).is_pending());
        Poll::Ready(())
    })
    .await;
}

async fn assert_closed_without_bytes(mut stream: Stream) {
    assert_eq!(
        bounded(stream.read(&mut [0u8; 1])).await.unwrap(),
        0,
        "cancelled negotiation must not emit RPC Bootstrap bytes"
    );
}

struct Probe(Rc<Cell<usize>>);
impl process::Server for Probe {
    async fn stdin(
        self: capnp::capability::Rc<Self>,
        _: process::StdinParams,
        _: process::StdinResults,
    ) -> capnp::Result<()> {
        self.0.set(self.0.get() + 1);
        Ok(())
    }
}

fn remote(stream: Stream) -> (ManagedRpc, Rc<Cell<usize>>) {
    let calls = Rc::new(Cell::new(0));
    let bootstrap: process::Client = capnp_rpc::new_client(Probe(calls.clone()));
    let (reader, writer) = stream.split();
    let driver = ManagedRpc::spawn(ManagedRpcSystem::new(
        Box::new(capnp_rpc::twoparty::VatNetwork::new(
            reader,
            writer,
            capnp_rpc::rpc_twoparty_capnp::Side::Server,
            Default::default(),
        )),
        Some(bootstrap.client),
    ));
    (driver, calls)
}

#[tokio::test(flavor = "current_thread")]
async fn cancellation_before_worker_poll_submits_no_handler_request() {
    tokio::task::LocalSet::new()
        .run_until(async {
            let network = Network::new().await;
            let registry = registry(1);
            let owner = registry.register().unwrap();
            let dial = network.dial(&registry, &owner);
            assert_eq!(registry.total_active(), 1);
            owner.close();
            drop(dial);
            let done = bounded(owner.completion().wait()).await.unwrap();
            assert_eq!(
                (done.active, done.committed, registry.total_active()),
                (0, 0, 0)
            );
            assert_eq!(network.observation.emitted.load(Ordering::SeqCst), 0);
            assert_eq!(
                network.observation.handshake_started.load(Ordering::SeqCst),
                0
            );
        })
        .await;
}

#[tokio::test(flavor = "current_thread")]
async fn closure_before_worker_poll_with_live_caller_submits_no_native_request() {
    tokio::task::LocalSet::new()
        .run_until(async {
            let network = Network::new().await;
            let registry = registry(1);
            let owner = registry.register().unwrap();
            let dial = network.dial(&registry, &owner);
            owner.close();
            // Keep the caller alive: only Cell closure can prevent this first poll.
            assert!(bounded(dial).await.is_err());
            bounded(owner.completion().wait()).await.unwrap();
            assert_eq!(network.observation.emitted.load(Ordering::SeqCst), 0);
            assert_eq!(
                network.observation.handshake_started.load(Ordering::SeqCst),
                0
            );
            assert_eq!(registry.total_active(), 0);
        })
        .await;
}

#[tokio::test(flavor = "current_thread")]
async fn original_dropped_open_future_still_negotiates_a_late_stream() {
    tokio::task::LocalSet::new()
        .run_until(async {
            let mut network = Network::new().await;
            network.gate.hold();
            let mut control = network.control.clone();
            let mut open = Box::pin(control.open_stream(network.peer, PROTOCOL));
            assert_pending(open.as_mut()).await;
            network.active_negotiation(1).await;
            drop(open);
            assert_eq!(network.observation.negotiated.load(Ordering::SeqCst), 0);
            network.gate.release();
            let late = network.next_stream().await;
            network
                .observation
                .wait_for(&network.observation.negotiated, 1)
                .await;
            assert_closed_without_bytes(late).await;
        })
        .await;
}

#[tokio::test(flavor = "current_thread")]
async fn active_cancellation_retains_capacity_until_late_stream_is_disposed() {
    tokio::task::LocalSet::new()
        .run_until(async {
            let mut network = Network::new().await;
            let registry = registry(1);
            let owner = registry.register().unwrap();
            let other = registry.register().unwrap();
            network.gate.hold();
            let dial = network.dial(&registry, &owner);
            network.active_negotiation(1).await;
            owner.close();
            drop(dial);
            assert_pending(owner.completion().wait()).await;
            assert_eq!(registry.total_active(), 1);
            assert!(connect_libp2p_owned::<process::Client>(
                &registry,
                Some(other.origin()),
                network.control.clone(),
                network.peer,
                PROTOCOL,
            )
            .is_err());
            network.gate.release();
            let late = network.next_stream().await;
            let done = bounded(owner.completion().wait()).await.unwrap();
            assert_eq!((done.active, done.committed, done.joined), (0, 0, 0));
            assert_eq!(network.observation.negotiated.load(Ordering::SeqCst), 1);
            assert_eq!(registry.total_active(), 0);
            assert_closed_without_bytes(late).await;
            let next = network.dial(&registry, &other);
            let (mut remote, calls) = remote(network.next_stream().await);
            let client = bounded(next).await.unwrap();
            bounded(client.stdin_request().send().promise)
                .await
                .unwrap();
            assert_eq!(calls.get(), 1);
            bounded(other.close_and_join()).await.unwrap();
            assert!(bounded(remote.shutdown_and_join()).await.is_ok());
        })
        .await;
}

#[tokio::test(flavor = "current_thread")]
async fn dropped_caller_rejects_late_success_while_cell_remains_open() {
    tokio::task::LocalSet::new()
        .run_until(async {
            let mut network = Network::new().await;
            let registry = registry(1);
            let owner = registry.register().unwrap();
            network.gate.hold();
            let dial = network.dial(&registry, &owner);
            network.active_negotiation(1).await;
            drop(dial);
            assert!(!owner.completion().snapshot().closed);
            assert_eq!(registry.total_active(), 1);
            network.gate.release();
            let late = network.next_stream().await;
            assert_closed_without_bytes(late).await;
            // This predicate waits for the independently retained join monitor.
            // Cell closure must not supply the cancellation being tested.
            bounded(async {
                while registry.total_active() != 0 {
                    tokio::task::yield_now().await;
                }
            })
            .await;
            let state = owner.completion().snapshot();
            assert!(!state.closed);
            assert_eq!((state.active, state.committed, state.joined), (0, 0, 0));
            let next = network.dial(&registry, &owner);
            let (mut remote, calls) = remote(network.next_stream().await);
            let client = bounded(next).await.unwrap();
            bounded(client.stdin_request().send().promise)
                .await
                .unwrap();
            assert_eq!(calls.get(), 1);
            bounded(owner.close_and_join()).await.unwrap();
            assert!(bounded(remote.shutdown_and_join()).await.is_ok());
        })
        .await;
}

#[tokio::test(flavor = "current_thread")]
async fn cancellation_of_a_queued_request_does_not_release_its_permit() {
    tokio::task::LocalSet::new()
        .run_until(async {
            let mut network = Network::new().await;
            let registry = registry(2);
            let a = registry.register().unwrap();
            let b = registry.register().unwrap();
            // Separate inbound slots avoid overflowing IncomingStreams' zero-buffer
            // channel while both negotiations complete in one scheduler turn.
            let queued_protocol = StreamProtocol::new("/pending-negotiation-test/queued");
            let mut queued_incoming = network
                .server_control
                .accept(queued_protocol.clone())
                .unwrap();
            network.gate.hold();
            let first = network.dial(&registry, &a);
            network.active_negotiation(1).await;

            // The first poll of pinned Control::open_stream enqueues NewStream before
            // yielding. Witness that poll's return, while A still occupies pending_upgrade.
            // This successful backend path uses the same retained supervisor with an
            // explicit first-poll witness; error-classification cases use the public facade.
            let (polled, observed) = oneshot::channel();
            let mut control = network.control.clone();
            let peer = network.peer;
            let queued =
                connect_owned::<_, process::Client, _>(&registry, Some(b.origin()), async move {
                    let mut signal = Some(polled);
                    let mut open = Box::pin(control.open_stream(peer, queued_protocol));
                    let stream = poll_fn(|cx| {
                        let result = open.as_mut().poll(cx);
                        if let Some(signal) = signal.take() {
                            signal.send(()).unwrap();
                        }
                        result
                    })
                    .await
                    .expect("this queued success case requires an actual negotiated stream");
                    Ok(stream)
                })
                .unwrap();
            bounded(observed).await.unwrap();
            assert_eq!(network.observation.emitted.load(Ordering::SeqCst), 1);
            assert_eq!(
                network.observation.handshake_started.load(Ordering::SeqCst),
                1
            );
            b.close();
            drop(queued);
            assert_pending(b.completion().wait()).await;
            assert_eq!(registry.total_active(), 2);

            network.gate.release();
            let (mut remote_a, calls) = remote(network.next_stream().await);
            let client_a = bounded(first).await.unwrap();
            let late_b = bounded(queued_incoming.next()).await.unwrap().1;
            let done_b = bounded(b.completion().wait()).await.unwrap();
            assert_eq!((done_b.active, done_b.committed), (0, 0));
            assert_eq!(network.observation.emitted.load(Ordering::SeqCst), 2);
            assert_eq!(
                network.observation.handshake_started.load(Ordering::SeqCst),
                2
            );
            assert_eq!(registry.total_active(), 1);
            assert_closed_without_bytes(late_b).await;
            bounded(client_a.stdin_request().send().promise)
                .await
                .unwrap();
            assert_eq!(calls.get(), 1);
            bounded(a.close_and_join()).await.unwrap();
            assert!(bounded(remote_a.shutdown_and_join()).await.is_ok());
        })
        .await;
}

#[tokio::test(flavor = "current_thread")]
async fn completed_negotiation_losing_commit_race_drops_stream_before_cleanup() {
    tokio::task::LocalSet::new()
        .run_until(async {
            let mut network = Network::new().await;
            let registry = registry(1);
            let owner = registry.register().unwrap();
            network
                .observation
                .hold_result
                .store(true, Ordering::SeqCst);
            let dial = network.dial(&registry, &owner);
            let stream = network.next_stream().await;
            network
                .observation
                .wait_for(&network.observation.negotiated, 1)
                .await;
            owner.close();
            drop(dial);
            assert_pending(owner.completion().wait()).await;
            assert_eq!(registry.total_active(), 1);
            network.observation.release_result();
            let done = bounded(owner.completion().wait()).await.unwrap();
            assert_eq!((done.active, done.committed, done.joined), (0, 0, 0));
            assert_closed_without_bytes(stream).await;
        })
        .await;
}

#[tokio::test(flavor = "current_thread")]
async fn unsupported_protocol_is_a_terminal_handler_failure_and_recovers_capacity() {
    tokio::task::LocalSet::new()
        .run_until(async {
            let network = Network::new().await;
            let registry = registry(1);
            let owner = registry.register().unwrap();
            let dial = connect_libp2p_owned::<process::Client>(
                &registry,
                Some(owner.origin()),
                network.control.clone(),
                network.peer,
                StreamProtocol::new("/unsupported-pending-test/1"),
            )
            .unwrap();
            assert!(bounded(dial).await.is_err());
            let done = bounded(owner.close_and_join()).await.unwrap();
            assert_eq!(network.observation.failed.load(Ordering::SeqCst), 1);
            assert_eq!(done.active, 0);
            assert!(!done.failed);
            assert_eq!(registry.total_active(), 0);
        })
        .await;
}

#[tokio::test(flavor = "current_thread")]
async fn explicit_no_address_dial_failure_recovers_capacity() {
    tokio::task::LocalSet::new()
        .run_until(async {
            let network = Network::new().await;
            let registry = registry(1);
            let owner = registry.register().unwrap();
            let dial = connect_libp2p_owned::<process::Client>(
                &registry,
                Some(owner.origin()),
                network.control.clone(),
                PeerId::random(),
                PROTOCOL,
            )
            .unwrap();
            assert!(bounded(dial).await.is_err());
            let state = bounded(owner.close_and_join()).await.unwrap();
            assert_eq!((state.active, registry.total_active()), (0, 0));
            assert!(!state.failed);
            assert_eq!(network.observation.emitted.load(Ordering::SeqCst), 0);
        })
        .await;
}

#[tokio::test(flavor = "current_thread")]
async fn peer_disconnect_during_negotiation_quarantines_sender_loss() {
    tokio::task::LocalSet::new()
        .run_until(async {
            let network = Network::new().await;
            let registry = registry(1);
            let owner = registry.register().unwrap();
            network.gate.hold();
            let dial = network.dial(&registry, &owner);
            network.active_negotiation(1).await;
            network.disconnect().await;
            assert!(bounded(dial).await.is_err());
            assert!(bounded(owner.close_and_join()).await.is_err());
            let done = owner.completion().snapshot();
            assert!(done.failed);
            assert_eq!((done.active, registry.total_active()), (1, 1));
            assert_eq!(network.observation.negotiated.load(Ordering::SeqCst), 0);
        })
        .await;
}

#[tokio::test(flavor = "current_thread")]
async fn a_terminates_while_b_retains_its_session_on_the_same_connection() {
    tokio::task::LocalSet::new()
        .run_until(async {
            let mut network = Network::new().await;
            let registry = registry(2);
            let a = registry.register().unwrap();
            let b = registry.register().unwrap();
            let dial_a = network.dial(&registry, &a);
            let (mut remote_a, calls_a) = remote(network.next_stream().await);
            let client_a = bounded(dial_a).await.unwrap();
            let dial_b = network.dial(&registry, &b);
            let (mut remote_b, calls_b) = remote(network.next_stream().await);
            let client_b = bounded(dial_b).await.unwrap();
            bounded(client_a.stdin_request().send().promise)
                .await
                .unwrap();
            bounded(client_b.stdin_request().send().promise)
                .await
                .unwrap();
            let done_a = bounded(a.close_and_join()).await.unwrap();
            assert_eq!((done_a.active, done_a.committed, done_a.joined), (0, 1, 1));
            assert!(bounded(client_a.stdin_request().send().promise)
                .await
                .is_err());
            bounded(client_b.stdin_request().send().promise)
                .await
                .unwrap();
            assert_eq!((calls_a.get(), calls_b.get()), (1, 2));
            assert_eq!(network.observation.dropped.load(Ordering::SeqCst), 0);
            assert_eq!(registry.total_active(), 1);
            let done_b = bounded(b.close_and_join()).await.unwrap();
            assert_eq!((done_b.active, done_b.committed, done_b.joined), (0, 1, 1));
            assert!(bounded(remote_a.shutdown_and_join()).await.is_ok());
            assert!(bounded(remote_b.shutdown_and_join()).await.is_ok());
        })
        .await;
}

#[tokio::test(flavor = "current_thread")]
async fn lost_completion_observer_does_not_abandon_negotiation() {
    tokio::task::LocalSet::new()
        .run_until(async {
            let mut network = Network::new().await;
            let registry = registry(1);
            let owner = registry.register().unwrap();
            let observer = owner.completion();
            network.gate.hold();
            let dial = network.dial(&registry, &owner);
            network.active_negotiation(1).await;
            owner.close();
            drop(observer);
            drop(dial);
            assert_eq!(registry.total_active(), 1);
            network.gate.release();
            let stream = network.next_stream().await;
            let done = bounded(owner.completion().wait()).await.unwrap();
            assert_eq!((done.active, registry.total_active()), (0, 0));
            assert_closed_without_bytes(stream).await;
        })
        .await;
}

#[tokio::test(flavor = "current_thread")]
async fn swarm_shutdown_during_negotiation_never_certifies_cleanup() {
    tokio::task::LocalSet::new()
        .run_until(async {
            let network = Network::new().await;
            let registry = registry(1);
            let owner = registry.register().unwrap();
            network.gate.hold();
            let dial = network.dial(&registry, &owner);
            network.active_negotiation(1).await;
            network.client_task.abort();
            network
                .observation
                .wait_for(&network.observation.dropped, 1)
                .await;
            assert!(bounded(dial).await.is_err());
            assert!(bounded(owner.close_and_join()).await.is_err());
            assert_eq!(registry.total_active(), 1);
            assert!(owner.completion().snapshot().failed);
        })
        .await;
}

#[tokio::test(flavor = "current_thread")]
async fn repeated_terminal_negotiation_failures_recover_bounded_admission() {
    tokio::task::LocalSet::new()
        .run_until(async {
            let network = Network::new().await;
            let registry = registry(1);
            let started = std::time::Instant::now();
            for _ in 0..128 {
                let owner = registry.register().unwrap();
                let dial = connect_libp2p_owned::<process::Client>(
                    &registry,
                    Some(owner.origin()),
                    network.control.clone(),
                    network.peer,
                    StreamProtocol::new("/unsupported-pending-smoke/1"),
                )
                .unwrap();
                assert!(bounded(dial).await.is_err());
                let done = bounded(owner.close_and_join()).await.unwrap();
                assert_eq!((done.active, registry.total_active()), (0, 0));
                assert!(!done.failed);
            }
            assert_eq!(network.observation.failed.load(Ordering::SeqCst), 128);
            assert_eq!(network.observation.dropped.load(Ordering::SeqCst), 0);
            eprintln!(
                "128 sequential owned unsupported-protocol negotiations: {:?}",
                started.elapsed()
            );
        })
        .await;
}

// These cases qualify previously untested assumptions about the real dependency.
// Removing native timeout or releasing admission before its event breaks them.
#[tokio::test(flavor = "current_thread")]
async fn native_default_timeout_terminalizes_cancelled_active_request() {
    tokio::task::LocalSet::new()
        .run_until(async {
            let network = Network::new().await;
            let registry = registry(1);
            let owner = registry.register().unwrap();
            network.observation.substream_gate.hold();
            let started = std::time::Instant::now();
            let dial = network.dial(&registry, &owner);
            network
                .observation
                .wait_for(&network.observation.handshake_started, 1)
                .await;
            network
                .observation
                .substream_gate
                .wait_for_blocked_write()
                .await;
            owner.close();
            drop(dial);
            assert_pending(owner.completion().wait()).await;
            assert_eq!(registry.total_active(), 1);
            let done = bounded(owner.completion().wait()).await.unwrap();
            assert!(network
                .observation
                .substream_gate
                .held
                .load(Ordering::SeqCst));
            assert_eq!(network.observation.timed_out.load(Ordering::SeqCst), 1);
            assert_eq!(
                network
                    .observation
                    .substreams_dropped
                    .load(Ordering::SeqCst),
                1
            );
            assert_eq!(network.observation.negotiated.load(Ordering::SeqCst), 0);
            assert_eq!((done.active, registry.total_active()), (0, 0));
            assert!(!done.failed);
            eprintln!(
                "native default timeout observed after {:?}",
                started.elapsed()
            );
        })
        .await;
}

#[tokio::test(flavor = "current_thread")]
async fn configured_timeout_preserves_b_session_and_b_pending_request() {
    tokio::task::LocalSet::new()
        .run_until(async {
            let mut network = Network::new().await;
            let registry = registry(3);
            let a = registry.register().unwrap();
            let b = registry.register().unwrap();
            let b_first = network.dial(&registry, &b);
            let (mut remote_b, calls_b) = remote(network.next_stream().await);
            let client_b = bounded(b_first).await.unwrap();
            bounded(client_b.stdin_request().send().promise)
                .await
                .unwrap();
            network
                .observation
                .timeout_millis
                .store(300, Ordering::SeqCst);
            network.observation.substream_gate.hold();
            let a_dial = network.dial(&registry, &a);
            network
                .observation
                .wait_for(&network.observation.handshake_started, 2)
                .await;
            network
                .observation
                .substream_gate
                .wait_for_blocked_write()
                .await;
            let (b_second, submitted) = witnessed_dial(&network, &registry, &b);
            bounded(submitted).await.unwrap();
            a.close();
            drop(a_dial);
            assert_eq!(registry.total_active(), 3);
            bounded(client_b.stdin_request().send().promise)
                .await
                .unwrap();
            assert_eq!(calls_b.get(), 2);
            assert_eq!(network.observation.timed_out.load(Ordering::SeqCst), 0);
            assert_pending(a.completion().wait()).await;
            let a_done = bounded(a.completion().wait()).await.unwrap();
            assert_eq!(a_done.active, 0);
            assert_eq!(network.observation.timed_out.load(Ordering::SeqCst), 1);
            assert_eq!(
                network
                    .observation
                    .substreams_dropped
                    .load(Ordering::SeqCst),
                1
            );
            assert_eq!(registry.total_active(), 2);
            network.observation.substream_gate.release();
            let (mut remote_b2, calls_b2) = remote(network.next_stream().await);
            let client_b2 = bounded(b_second).await.unwrap();
            bounded(client_b2.stdin_request().send().promise)
                .await
                .unwrap();
            bounded(client_b.stdin_request().send().promise)
                .await
                .unwrap();
            assert_eq!((calls_b.get(), calls_b2.get()), (3, 1));
            assert_eq!(network.observation.dropped.load(Ordering::SeqCst), 0);
            let done = bounded(b.close_and_join()).await.unwrap();
            assert_eq!(
                (done.committed, done.joined, registry.total_active()),
                (2, 2, 0)
            );
            assert!(bounded(remote_b.shutdown_and_join()).await.is_ok());
            assert!(bounded(remote_b2.shutdown_and_join()).await.is_ok());
        })
        .await;
}

#[tokio::test(flavor = "current_thread")]
async fn cancelled_queued_request_starts_then_waits_for_its_own_native_timeout() {
    tokio::task::LocalSet::new()
        .run_until(async {
            let network = Network::new().await;
            network
                .observation
                .timeout_millis
                .store(250, Ordering::SeqCst);
            let registry = registry(2);
            let a = registry.register().unwrap();
            let b = registry.register().unwrap();
            network.observation.substream_gate.hold();
            let first = network.dial(&registry, &a);
            network
                .observation
                .wait_for(&network.observation.handshake_started, 1)
                .await;
            let (queued, submitted) = witnessed_dial(&network, &registry, &b);
            bounded(submitted).await.unwrap();
            a.close();
            b.close();
            drop(first);
            drop(queued);
            assert_eq!(network.observation.emitted.load(Ordering::SeqCst), 1);
            bounded(a.completion().wait()).await.unwrap();
            assert_eq!(network.observation.timed_out.load(Ordering::SeqCst), 1);
            assert_eq!(registry.total_active(), 1);
            // Closing B does not remove its libp2p queue entry: it starts after A ends.
            network
                .observation
                .wait_for(&network.observation.handshake_started, 2)
                .await;
            assert_pending(b.completion().wait()).await;
            bounded(b.completion().wait()).await.unwrap();
            assert_eq!(network.observation.timed_out.load(Ordering::SeqCst), 2);
            assert_eq!(
                network
                    .observation
                    .substreams_dropped
                    .load(Ordering::SeqCst),
                2
            );
            assert_eq!(registry.total_active(), 0);
        })
        .await;
}

#[tokio::test(flavor = "current_thread")]
async fn second_disconnected_peer_requires_another_dial_notification() {
    tokio::task::LocalSet::new()
        .run_until(async {
            let mut swarm = swarm(Arc::default(), Arc::default());
            let mut control1 = swarm.behaviour().inner.new_control();
            let mut control2 = control1.clone();
            let mut retry_control = control1.clone();
            let peer1 = PeerId::random();
            let peer2 = PeerId::random();
            let mut first = Box::pin(control1.open_stream(peer1, PROTOCOL));
            let mut second = Box::pin(control2.open_stream(peer2, PROTOCOL));
            assert_pending(first.as_mut()).await;
            assert_pending(second.as_mut()).await;
            // Drain the real Swarm until it has no immediate events or work.
            poll_fn(|cx| {
                while swarm.poll_next_unpin(cx).is_ready() {}
                Poll::Ready(())
            })
            .await;
            assert!(bounded(first).await.is_err());
            assert_pending(second.as_mut()).await;
            let mut retry = Box::pin(retry_control.open_stream(peer2, PROTOCOL));
            assert_pending(retry.as_mut()).await;
            poll_fn(|cx| {
                while swarm.poll_next_unpin(cx).is_ready() {}
                Poll::Ready(())
            })
            .await;
            assert!(bounded(second).await.is_err());
            assert!(bounded(retry).await.is_err());
        })
        .await;
}

#[tokio::test(flavor = "current_thread")]
async fn preconnection_open_remains_pending_after_swarm_drop() {
    tokio::task::LocalSet::new()
        .run_until(async {
            let swarm = swarm(Arc::default(), Arc::default());
            let mut control = swarm.behaviour().inner.new_control();
            let mut open = Box::pin(control.open_stream(PeerId::random(), PROTOCOL));
            assert_pending(open.as_mut()).await;
            drop(swarm);
            // Control retains Shared, which retains both queue ends. No driver or timer exists.
            assert_pending(open.as_mut()).await;
            assert!(tokio::time::timeout(Duration::from_millis(100), open)
                .await
                .is_err());
        })
        .await;
}

// Exact first-poll witness for tests that must establish queue admission.
// The fixed scenarios permit only explicit handler results; channel loss fails.
fn witnessed_dial(
    network: &Network,
    registry: &OutboundRegistry,
    owner: &CellEffects,
) -> (OwnedDial<process::Client>, oneshot::Receiver<()>) {
    let (send, receive) = oneshot::channel();
    let mut control = network.control.clone();
    let peer = network.peer;
    let dial = connect_owned(registry, Some(owner.origin()), async move {
        let mut open = Box::pin(control.open_stream(peer, PROTOCOL));
        let mut first = Some(send);
        poll_fn(|cx| {
            let polled = open.as_mut().poll(cx);
            if let Some(first) = first.take() {
                first.send(()).unwrap();
            }
            polled
        })
        .await
        .map_err(|error| {
            match &error {
                libp2p_stream::OpenStreamError::UnsupportedProtocol(_) => {}
                libp2p_stream::OpenStreamError::Io(error) => {
                    assert_eq!(error.kind(), std::io::ErrorKind::TimedOut);
                }
                _ => panic!("test received unknown native completion"),
            }
            capnp::Error::failed(error.to_string())
        })
    })
    .unwrap();
    (dial, receive)
}

async fn closure_at_native_queue_boundary(after_handler_dequeue: bool) {
    let mut network = Network::new().await;
    let registry = registry(2);
    let a = registry.register().unwrap();
    let b = registry.register().unwrap();
    let b_dial = network.dial(&registry, &b);
    let (mut b_remote, calls) = remote(network.next_stream().await);
    let b_client = bounded(b_dial).await.unwrap();
    bounded(b_client.stdin_request().send().promise)
        .await
        .unwrap();

    if after_handler_dequeue {
        network
            .observation
            .hold_emission
            .store(true, Ordering::SeqCst);
    } else {
        network
            .observation
            .hold_dequeue
            .store(true, Ordering::SeqCst);
    }
    let (a_dial, submitted) = witnessed_dial(&network, &registry, &a);
    bounded(submitted).await.unwrap();
    if after_handler_dequeue {
        network
            .observation
            .wait_for(&network.observation.held_requests, 1)
            .await;
    }
    assert_eq!(network.observation.emitted.load(Ordering::SeqCst), 1);
    assert_eq!(
        network.observation.handshake_started.load(Ordering::SeqCst),
        1
    );
    a.close();
    drop(a_dial);
    assert_pending(a.completion().wait()).await;
    assert_eq!(registry.total_active(), 2);
    bounded(b_client.stdin_request().send().promise)
        .await
        .unwrap();
    assert_eq!(calls.get(), 2);
    assert_eq!(network.observation.emitted.load(Ordering::SeqCst), 1);

    network
        .observation
        .hold_dequeue
        .store(false, Ordering::SeqCst);
    network
        .observation
        .hold_emission
        .store(false, Ordering::SeqCst);
    network.observation.submission_waker.wake();
    let late = network.next_stream().await;
    let done = bounded(a.completion().wait()).await.unwrap();
    assert_eq!((done.active, done.committed), (0, 0));
    assert_eq!(network.observation.emitted.load(Ordering::SeqCst), 2);
    assert_eq!(network.observation.negotiated.load(Ordering::SeqCst), 2);
    assert_eq!(registry.total_active(), 1);
    assert_closed_without_bytes(late).await;
    assert!(connect_libp2p_owned::<process::Client>(
        &registry,
        Some(a.origin()),
        network.control.clone(),
        network.peer,
        PROTOCOL,
    )
    .is_err());
    bounded(b_client.stdin_request().send().promise)
        .await
        .unwrap();
    assert_eq!(calls.get(), 3);
    assert_eq!(network.observation.dropped.load(Ordering::SeqCst), 0);
    bounded(b.close_and_join()).await.unwrap();
    assert!(bounded(b_remote.shutdown_and_join()).await.is_ok());
}

#[tokio::test(flavor = "current_thread")]
async fn closed_native_queue_is_drained_before_cleanup_and_preserves_b() {
    tokio::task::LocalSet::new()
        .run_until(closure_at_native_queue_boundary(false))
        .await;
}

#[tokio::test(flavor = "current_thread")]
async fn closure_immediately_before_swarm_emission_retains_the_obligation() {
    tokio::task::LocalSet::new()
        .run_until(closure_at_native_queue_boundary(true))
        .await;
}

#[test]
fn closure_during_first_backend_poll_cannot_deadlock_or_lose_the_native_attempt() {
    let (done, receive) = std::sync::mpsc::channel();
    let thread = std::thread::spawn(move || {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        runtime.block_on(tokio::task::LocalSet::new().run_until(async {
            let mut network = Network::new().await;
            let registry = registry(1);
            let owner = registry.register().unwrap();
            let close = owner.close_handle();
            network.observation.substream_gate.hold();
            let mut control = network.control.clone();
            let peer = network.peer;
            let (polled, witness) = oneshot::channel();
            let dial = connect_owned::<_, process::Client, _>(
                &registry,
                Some(owner.origin()),
                async move {
                    let mut native = Box::pin(control.open_stream(peer, PROTOCOL));
                    let mut close = Some(close);
                    let mut polled = Some(polled);
                    let stream = poll_fn(|cx| {
                        if let Some(close) = close.take() {
                            // Authorization has committed. Close on a separate
                            // thread while this first backend poll is executing.
                            // Holding the gate across poll would deadlock here.
                            std::thread::spawn(move || close.close()).join().unwrap();
                        }
                        let result = native.as_mut().poll(cx);
                        if let Some(polled) = polled.take() {
                            polled.send(()).unwrap();
                        }
                        result
                    })
                    .await
                    .expect("this gated success case requires an actual negotiated stream");
                    Ok(stream)
                },
            )
            .unwrap();
            bounded(witness).await.unwrap();
            owner.close();
            assert_eq!(registry.total_active(), 1);
            assert_pending(owner.completion().wait()).await;
            network
                .observation
                .wait_for(&network.observation.handshake_started, 1)
                .await;
            network.observation.substream_gate.release();
            let late = network.next_stream().await;
            assert!(bounded(dial).await.is_err());
            bounded(owner.completion().wait()).await.unwrap();
            assert_eq!(registry.total_active(), 0);
            assert_closed_without_bytes(late).await;
        }));
        done.send(()).unwrap();
    });
    receive
        .recv_timeout(Duration::from_secs(25))
        .expect("first-poll closure/worker join watchdog");
    thread.join().unwrap();
}
