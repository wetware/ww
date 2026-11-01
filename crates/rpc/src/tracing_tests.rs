//! Span attribution tests use a thread-local subscriber on a current-thread runtime.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use authority::{system_capnp, Epoch, EpochGuard};
use futures::StreamExt;
use tokio::sync::{mpsc, oneshot, watch};
use tokio_util::compat::{TokioAsyncReadCompatExt, TokioAsyncWriteCompatExt};
use tracing::field::{Field, Visit};
use tracing::span::{Attributes, Id, Record};
use tracing::{Event, Instrument, Metadata, Subscriber};

#[derive(Clone, Debug)]
struct SpanRecord {
    name: &'static str,
    metadata: &'static Metadata<'static>,
    parent: Option<Id>,
    fields: Fields,
}

#[derive(Clone, Debug, Default)]
struct Fields(BTreeMap<String, String>);

impl Visit for Fields {
    fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
        self.0.insert(field.name().into(), format!("{value:?}"));
    }

    fn record_str(&mut self, field: &Field, value: &str) {
        self.0.insert(field.name().into(), value.into());
    }
}

#[derive(Clone, Debug)]
struct EventRecord {
    message: String,
    fields: Fields,
    spans: Vec<SpanRecord>,
}

#[derive(Default)]
struct TraceState {
    spans: Vec<SpanRecord>,
    entered: Vec<Id>,
    events: Vec<EventRecord>,
    follows: Vec<(Id, Id)>,
}

#[derive(Clone)]
struct Capture {
    state: Arc<Mutex<TraceState>>,
    events: mpsc::UnboundedSender<EventRecord>,
    filter_handlers: bool,
}

impl Capture {
    fn new() -> (Self, mpsc::UnboundedReceiver<EventRecord>) {
        let (events, receiver) = mpsc::unbounded_channel();
        (
            Self {
                state: Arc::default(),
                events,
                filter_handlers: false,
            },
            receiver,
        )
    }

    fn assert_path(&self, message: &str, expected: &[&str]) {
        let state = self.state.lock().unwrap();
        let matching: Vec<_> = state
            .events
            .iter()
            .filter(|e| e.message == message)
            .collect();
        assert!(!matching.is_empty(), "missing event {message}");
        for event in matching {
            let path: Vec<_> = event.spans.iter().map(|span| span.name).collect();
            assert_eq!(path, expected, "event {message}");
        }
    }
}

impl Subscriber for Capture {
    fn enabled(&self, metadata: &Metadata<'_>) -> bool {
        metadata.target().starts_with("rpc")
            && !(self.filter_handlers && metadata.is_span() && metadata.name().ends_with(".handle"))
    }

    fn new_span(&self, attributes: &Attributes<'_>) -> Id {
        let mut state = self.state.lock().unwrap();
        let parent = attributes.parent().cloned().or_else(|| {
            attributes
                .is_contextual()
                .then(|| state.entered.last().cloned())
                .flatten()
        });
        let mut fields = Fields::default();
        attributes.record(&mut fields);
        state.spans.push(SpanRecord {
            name: attributes.metadata().name(),
            metadata: attributes.metadata(),
            parent,
            fields,
        });
        Id::from_u64(state.spans.len() as u64)
    }

    fn current_span(&self) -> tracing_core::span::Current {
        let state = self.state.lock().unwrap();
        match state.entered.last() {
            Some(id) => tracing_core::span::Current::new(
                id.clone(),
                state.spans[id.into_u64() as usize - 1].metadata,
            ),
            None => tracing_core::span::Current::none(),
        }
    }

    fn record(&self, span: &Id, values: &Record<'_>) {
        values.record(&mut self.state.lock().unwrap().spans[span.into_u64() as usize - 1].fields);
    }

    fn record_follows_from(&self, span: &Id, follows: &Id) {
        self.state
            .lock()
            .unwrap()
            .follows
            .push((span.clone(), follows.clone()));
    }

    fn event(&self, event: &Event<'_>) {
        let mut state = self.state.lock().unwrap();
        let mut parent = event.parent().cloned().or_else(|| {
            event
                .is_contextual()
                .then(|| state.entered.last().cloned())
                .flatten()
        });
        let mut spans = Vec::new();
        while let Some(id) = parent {
            let span = state.spans[id.into_u64() as usize - 1].clone();
            parent = span.parent.clone();
            spans.push(span);
        }
        spans.reverse();
        let mut fields = Fields::default();
        event.record(&mut fields);
        let record = EventRecord {
            message: fields.0.remove("message").unwrap_or_default(),
            fields,
            spans,
        };
        state.events.push(record.clone());
        let _ = self.events.send(record);
    }

    fn enter(&self, span: &Id) {
        self.state.lock().unwrap().entered.push(span.clone());
    }

    fn exit(&self, span: &Id) {
        let mut state = self.state.lock().unwrap();
        let position = state
            .entered
            .iter()
            .rposition(|entered| entered == span)
            .expect("entered span");
        state.entered.remove(position);
    }
}

async fn observed(events: &mut mpsc::UnboundedReceiver<EventRecord>, message: &str) -> EventRecord {
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let event = events.recv().await.expect("capture remains active");
            if event.message == message {
                return event;
            }
        }
    })
    .await
    .expect("expected tracing event")
}

struct DropEvent(&'static str);

impl Drop for DropEvent {
    fn drop(&mut self) {
        tracing::debug!("{}", self.0);
    }
}

// Signal only after the wrapped future has actually suspended; no sleep or
// scheduler ordering assumptions are needed to interleave the two tasks.
async fn first_pending<F: std::future::Future>(
    future: F,
    signal: oneshot::Sender<()>,
) -> F::Output {
    tokio::pin!(future);
    let mut signal = Some(signal);
    futures::future::poll_fn(|cx| {
        let result = future.as_mut().poll(cx);
        if result.is_pending() {
            if let Some(signal) = signal.take() {
                let _ = signal.send(());
            }
        }
        result
    })
    .await
}

#[tokio::test]
async fn tracing_interleaved_pumps_keep_task_context_through_cancellation() {
    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

    let (capture, mut events) = Capture::new();
    let _subscriber = tracing::subscriber::set_default(capture.clone());
    tokio::task::LocalSet::new()
        .run_until(async {
            let (network_in, mut input_peer) = tokio::io::duplex(64);
            let (stdin, mut child_stdin) = tokio::io::duplex(64);
            let stdin = capnp_rpc::new_client(crate::ByteStreamImpl::new(
                stdin,
                crate::StreamMode::WriteOnly,
            ));
            let (stdout, mut child_stdout) = tokio::io::duplex(64);
            let stdout = capnp_rpc::new_client(crate::ByteStreamImpl::new(
                stdout,
                crate::StreamMode::ReadOnly,
            ));
            let (network_out, mut output_peer) = tokio::io::duplex(64);
            let (resume_a, resumed_a) = oneshot::channel();
            let (resume_b, resumed_b) = oneshot::channel();
            let (pending_a, suspended_a) = oneshot::channel();
            let (pending_b, suspended_b) = oneshot::channel();
            let span_a = tracing::info_span!(parent: None, "task.a");
            let span_b = tracing::info_span!(parent: None, "task.b");
            let a = tokio::task::spawn_local(
                async move {
                    let _drop = DropEvent("a.drop");
                    tracing::debug!("a.before");
                    resumed_a.await.unwrap();
                    tracing::debug!("a.after");
                    first_pending(
                        crate::stream_listener::pump_stream_to_stdin(network_in.compat(), stdin, 1),
                        pending_a,
                    )
                    .await;
                }
                .instrument(span_a),
            );
            observed(&mut events, "a.before").await;
            let b = tokio::task::spawn_local(
                async move {
                    let _drop = DropEvent("b.drop");
                    tracing::debug!("b.before");
                    resumed_b.await.unwrap();
                    tracing::debug!("b.after");
                    first_pending(
                        crate::stream_listener::pump_stdout_to_stream(
                            stdout,
                            network_out.compat_write(),
                            2,
                        ),
                        pending_b,
                    )
                    .await;
                    tracing::debug!("b.done");
                }
                .instrument(span_b),
            );
            observed(&mut events, "b.before").await;
            resume_a.send(()).unwrap();
            suspended_a.await.unwrap();
            resume_b.send(()).unwrap();
            suspended_b.await.unwrap();
            tokio::task::spawn_local(async {
                tracing::debug!("unrelated.suspended");
            })
            .await
            .unwrap();

            input_peer.write_all(b"a").await.unwrap();
            let mut byte = [0];
            child_stdin.read_exact(&mut byte).await.unwrap();
            assert_eq!(byte, *b"a");
            child_stdout.write_all(b"b").await.unwrap();
            output_peer.read_exact(&mut byte).await.unwrap();
            assert_eq!(byte, *b"b");
            a.abort();
            assert!(a.await.unwrap_err().is_cancelled());
            tokio::task::spawn_local(async {
                tracing::debug!("unrelated.cancelled");
            })
            .await
            .unwrap();
            child_stdout.shutdown().await.unwrap();
            b.await.unwrap();
            tracing::debug!("unrelated.complete");
        })
        .await;

    for event in [
        "unrelated.suspended",
        "unrelated.cancelled",
        "unrelated.complete",
    ] {
        capture.assert_path(event, &[]);
    }
    for event in ["a.before", "a.after", "a.drop"] {
        capture.assert_path(event, &["task.a"]);
    }
    for event in ["b.before", "b.after", "b.done", "b.drop"] {
        capture.assert_path(event, &["task.b"]);
    }
    capture.assert_path("pump_in: read chunk", &["task.a", "stream.pump_in"]);
    capture.assert_path("pump_out: write chunk", &["task.b", "stream.pump_out"]);
    let state = capture.state.lock().unwrap();
    for (message, connection_id) in [("pump_in: read chunk", "1"), ("pump_out: write chunk", "2")] {
        let event = state
            .events
            .iter()
            .find(|event| event.message == message)
            .unwrap();
        assert_eq!(
            event.spans.last().unwrap().fields.0["connection_id"],
            connection_id
        );
    }
    assert!(state.entered.is_empty());
}

fn memory_swarm() -> libp2p::Swarm<libp2p_stream::Behaviour> {
    use libp2p::Transport;
    let identity = libp2p::identity::Keypair::generate_ed25519();
    let transport = libp2p::core::transport::MemoryTransport::default()
        .upgrade(libp2p::core::upgrade::Version::V1)
        .authenticate(libp2p::noise::Config::new(&identity).unwrap())
        .multiplex(libp2p::yamux::Config::default())
        .boxed();
    libp2p::Swarm::new(
        transport,
        libp2p_stream::Behaviour::new(),
        identity.public().to_peer_id(),
        libp2p::swarm::Config::with_tokio_executor()
            .with_idle_connection_timeout(Duration::from_secs(60)),
    )
}

#[tokio::test]
async fn tracing_outgoing_vat_failures_keep_managed_rpc_identity() {
    use futures::io::{AsyncReadExt as _, AsyncWriteExt as _};

    let (capture, mut events) = Capture::new();
    let _subscriber = tracing::subscriber::set_default(capture.clone());
    let local = tokio::task::LocalSet::new();
    tokio::time::timeout(
        Duration::from_secs(15),
        local.run_until(async {
            let mut server = memory_swarm();
            let server_peer = *server.local_peer_id();
            let mut server_control = server.behaviour().new_control();
            let mut incoming = server_control
                .accept(crate::vat_protocol("trace-outgoing").unwrap())
                .unwrap();
            server.listen_on("/memory/0".parse().unwrap()).unwrap();
            let address = loop {
                if let libp2p::swarm::SwarmEvent::NewListenAddr { address, .. } =
                    server.select_next_some().await
                {
                    break address;
                }
            };
            let mut client = memory_swarm();
            let client_control = client.behaviour().new_control();
            client.dial(address).unwrap();
            let server_task =
                tokio::task::spawn_local(async move { while server.next().await.is_some() {} });
            while !matches!(
                client.select_next_some().await,
                libp2p::swarm::SwarmEvent::ConnectionEstablished { .. }
            ) {}
            let client_task =
                tokio::task::spawn_local(async move { while client.next().await.is_some() {} });
            let (_epoch, receiver) = watch::channel(Epoch {
                seq: 1,
                head: vec![],
                root: None,
            });
            let vat: system_capnp::vat_client::Client =
                capnp_rpc::new_client(crate::vat_client::VatClientImpl::new(
                    client_control,
                    EpochGuard {
                        issued_seq: 1,
                        receiver,
                    },
                ));

            let mut connections = Vec::new();
            for _ in 0..2 {
                let mut request = vat.dial_request();
                request.get().set_peer(&server_peer.to_bytes());
                request.get().set_protocol("trace-outgoing");
                let response = request.send().promise.await.unwrap();
                let (_, mut stream) = incoming.next().await.unwrap();
                // Observe actual driver progress before allowing either peer
                // to fail. Both RPC workers remain live across suspension.
                let mut byte = [0];
                stream.read_exact(&mut byte).await.unwrap();
                let state = capture.state.lock().unwrap();
                let root = state
                    .spans
                    .iter()
                    .rev()
                    .find(|span| span.name == "rpc")
                    .unwrap();
                assert!(root.parent.is_none());
                connections.push((stream, response, root.fields.0["rpc_id"].clone()));
            }
            assert_ne!(connections[0].2, connections[1].2);

            let mut message = capnp::message::Builder::new_default();
            let mut abort = message
                .init_root::<capnp_rpc::rpc_capnp::message::Builder>()
                .init_abort();
            abort.set_type(capnp_rpc::rpc_capnp::exception::Type::Failed);
            abort.set_reason("outgoing tracing fixture failure");
            let mut bytes = Vec::new();
            capnp::serialize::write_message(&mut bytes, &message).unwrap();
            for (mut stream, _response, rpc_id) in connections {
                stream.write_all(&bytes).await.unwrap();
                stream.flush().await.unwrap();
                let event = tokio::time::timeout(Duration::from_secs(5), async {
                    loop {
                        let event = events.recv().await.unwrap();
                        if event
                            .message
                            .starts_with("vat dial session ended with error:")
                        {
                            break event;
                        }
                    }
                })
                .await
                .expect("outgoing failure warning");
                assert_eq!(event.fields.0["event_code"], "400");
                assert_eq!(event.fields.0["rpc_id"], rpc_id);
                assert!(
                    event.spans.is_empty(),
                    "completion must not reparent the worker"
                );
            }
            server_task.abort();
            client_task.abort();
            let _ = server_task.await;
            let _ = client_task.await;
        }),
    )
    .await
    .expect("outgoing VAT fixture completed");
    assert!(capture.state.lock().unwrap().entered.is_empty());
}

struct RejectSpawn;

impl system_capnp::executor::Server for RejectSpawn {
    async fn spawn(
        self: capnp::capability::Rc<Self>,
        _: system_capnp::executor::SpawnParams,
        _: system_capnp::executor::SpawnResults,
    ) -> Result<(), capnp::Error> {
        Err(capnp::Error::failed(
            "tracing fixture rejected spawn".into(),
        ))
    }
}

#[tokio::test]
async fn tracing_listener_handlers_keep_accept_parents_and_fields() {
    listener_handlers_keep_accept_context(false).await;
}

#[tokio::test]
async fn tracing_filtered_listener_handlers_keep_enabled_accept_context() {
    listener_handlers_keep_accept_context(true).await;
}

async fn listener_handlers_keep_accept_context(filter_handlers: bool) {
    use futures::io::AsyncWriteExt as _;

    let (mut capture, mut events) = Capture::new();
    capture.filter_handlers = filter_handlers;
    let _subscriber = tracing::subscriber::set_default(capture.clone());
    tokio::task::LocalSet::new()
        .run_until(async {
            let mut server = memory_swarm();
            let server_peer = *server.local_peer_id();
            let server_control = server.behaviour().new_control();
            server.listen_on("/memory/0".parse().unwrap()).unwrap();
            let address = loop {
                if let libp2p::swarm::SwarmEvent::NewListenAddr { address, .. } =
                    server.select_next_some().await
                {
                    break address;
                }
            };
            let mut client = memory_swarm();
            let client_peer = *client.local_peer_id();
            let mut client_control = client.behaviour().new_control();
            client.dial(address).unwrap();
            let server_task =
                tokio::task::spawn_local(async move { while server.next().await.is_some() {} });
            while !matches!(
                client.select_next_some().await,
                libp2p::swarm::SwarmEvent::ConnectionEstablished { .. }
            ) {}
            let client_task =
                tokio::task::spawn_local(async move { while client.next().await.is_some() {} });

            let (epoch, receiver) = watch::channel(Epoch {
                seq: 1,
                head: vec![],
                root: None,
            });
            let guard = EpochGuard {
                issued_seq: 1,
                receiver: receiver.clone(),
            };
            let membrane = authority::membrane_client(receiver, b"trace-test");
            let vat: system_capnp::vat_listener::Client = capnp_rpc::new_client(
                crate::vat_listener::VatListenerImpl::new(server_control.clone(), guard.clone()),
            );
            let mut raw = vat.serve_raw_request();
            raw.get().set_protocol("trace-raw");
            raw.get()
                .init_cap()
                .set_as_capability(membrane.client.clone().hook);
            raw.send().promise.await.unwrap();
            let mut authenticated = vat.serve_authenticated_request();
            authenticated.get().set_protocol("trace-auth");
            authenticated
                .get()
                .init_cap()
                .set_as_capability(membrane.client.clone().hook);
            let mut policy = authenticated.get().init_policy();
            let mut profile = policy.reborrow().init_profiles(1).get(0);
            profile.set_name("test");
            let mut method = profile.init_methods(1).get(0);
            method.set_interface_id(1);
            method.set_ordinal(0);
            let mut recipient = policy.init_recipients(1).get(0);
            recipient.set_profile("test");
            recipient.set_verifying_key(&[1; 32]);
            authenticated.send().promise.await.unwrap();
            let stream: system_capnp::stream_listener::Client = capnp_rpc::new_client(
                crate::stream_listener::StreamListenerImpl::new(server_control, guard),
            );
            let mut listen = stream.listen_request();
            listen.get().set_protocol("trace-stream");
            listen.get().set_membrane(membrane);
            listen
                .get()
                .set_executor(capnp_rpc::new_client(RejectSpawn));
            listen.send().promise.await.unwrap();

            for (protocol, message, expected) in [
                (
                    crate::vat_protocol("trace-raw").unwrap(),
                    "vat peer disconnected",
                    ["vat.accept", "vat.handle"],
                ),
                (
                    crate::vat_protocol("trace-auth").unwrap(),
                    "authenticated vat peer disconnected",
                    ["vat.authenticated.accept", "vat.authenticated.handle"],
                ),
                (
                    crate::stream_protocol("trace-stream").unwrap(),
                    "Stream cell connection error",
                    ["stream.accept", "stream.handle"],
                ),
            ] {
                let mut connection = client_control
                    .open_stream(server_peer, protocol.clone())
                    .await
                    .unwrap();
                connection.close().await.unwrap();
                drop(connection);
                let event = observed(&mut events, message).await;
                capture.assert_path(
                    message,
                    if filter_handlers {
                        &expected[..1]
                    } else {
                        &expected
                    },
                );
                assert_eq!(event.spans[0].fields.0["peer"], client_peer.to_string());
                assert_eq!(event.spans[0].fields.0["protocol"], protocol.to_string());
                let connection_id = &event.spans[0].fields.0["connection_id"];
                assert_ne!(connection_id.parse::<u64>().unwrap(), 0);
                let field = if expected[1] == "stream.handle" {
                    "protocol"
                } else {
                    "service"
                };
                if !filter_handlers {
                    assert_eq!(&event.spans[1].fields.0["connection_id"], connection_id);
                    assert_eq!(
                        event.spans[1].fields.0[field],
                        protocol.as_ref().rsplit('/').next().unwrap()
                    );
                }
                tokio::task::spawn_local(async {
                    tracing::debug!("unrelated.listener");
                })
                .await
                .unwrap();
            }
            epoch
                .send(Epoch {
                    seq: 2,
                    head: vec![],
                    root: None,
                })
                .unwrap();
            server_task.abort();
            client_task.abort();
            let _ = server_task.await;
            let _ = client_task.await;
        })
        .await;
    capture.assert_path("unrelated.listener", &[]);
    assert!(capture.state.lock().unwrap().entered.is_empty());
}

// The real RpcSystem polls this network driver. Explicit channels keep two
// independent workers suspended before either can fail or be cancelled.
struct TraceNetwork(Option<oneshot::Receiver<bool>>);

impl capnp_rpc::VatNetwork<capnp_rpc::rpc_twoparty_capnp::Side> for TraceNetwork {
    fn connect(
        &mut self,
        _: capnp_rpc::rpc_twoparty_capnp::Side,
    ) -> Option<Box<dyn capnp_rpc::Connection<capnp_rpc::rpc_twoparty_capnp::Side>>> {
        None
    }

    fn accept(
        &mut self,
    ) -> capnp::capability::Promise<
        Box<dyn capnp_rpc::Connection<capnp_rpc::rpc_twoparty_capnp::Side>>,
        capnp::Error,
    > {
        capnp::capability::Promise::from_future(std::future::pending())
    }

    fn drive_until_shutdown(&mut self) -> capnp::capability::Promise<(), capnp::Error> {
        let release = self.0.take().unwrap();
        capnp::capability::Promise::from_future(async move {
            let _drop = DropEvent("rpc.driver.drop");
            tracing::debug!("rpc.driver.before");
            let panic = release.await.unwrap();
            tracing::debug!("rpc.driver.after");
            assert!(!panic, "injected tracing driver panic");
            Err(capnp::Error::failed("injected tracing driver error".into()))
        })
    }
}

#[tokio::test]
async fn tracing_rpc_workers_have_independent_roots_through_error_and_panic() {
    use crate::managed_rpc::{ManagedRpc, ManagedRpcSystem};

    let (capture, mut events) = Capture::new();
    let _subscriber = tracing::subscriber::set_default(capture.clone());
    let mut callers = Vec::new();
    tokio::task::LocalSet::new()
        .run_until(async {
            let mut workers = Vec::new();
            for panic in [false, true] {
                let (release, resume) = oneshot::channel();
                let caller = tracing::info_span!(parent: None, "short.caller");
                callers.push(caller.id().unwrap());
                let worker = caller.in_scope(|| {
                    ManagedRpc::spawn(ManagedRpcSystem::new(
                        Box::new(TraceNetwork(Some(resume))),
                        None,
                    ))
                });
                drop(caller);
                let event = observed(&mut events, "rpc.driver.before").await;
                assert_eq!(
                    event.spans.len(),
                    1,
                    "worker requires its own root: {event:?}"
                );
                assert_eq!(event.spans[0].name, "rpc");
                assert_eq!(
                    worker.rpc_id().to_string(),
                    event.spans[0].fields.0["rpc_id"]
                );
                workers.push((
                    worker,
                    release,
                    panic,
                    event.spans[0].fields.0["rpc_id"].clone(),
                ));
            }
            assert_ne!(workers[0].3, workers[1].3);
            tokio::task::spawn_local(async {
                tracing::debug!("rpc.unrelated");
            })
            .await
            .unwrap();
            for (worker, release, panic, id) in workers {
                release.send(panic).unwrap();
                assert!(worker.await.unwrap().is_err());
                for message in ["rpc.driver.after", "rpc.driver.drop"] {
                    let event = observed(&mut events, message).await;
                    assert_eq!(event.spans.len(), 1);
                    assert_eq!(event.spans[0].fields.0["rpc_id"], id);
                }
            }
            tracing::debug!("rpc.unrelated");
        })
        .await;
    capture.assert_path("rpc.unrelated", &[]);
    let state = capture.state.lock().unwrap();
    assert!(state.entered.is_empty());
    // Value: protects=causal ownership; fails_when=follows_from is missing or
    // targets another caller; why_new=JSON omits causal edges; seam=none.
    assert_eq!(state.follows.len(), callers.len());
    for ((span, follows), caller) in state.follows.iter().zip(callers) {
        let source = &state.spans[span.into_u64() as usize - 1];
        assert_eq!(source.name, "rpc");
        assert!(source.parent.is_none());
        assert_eq!(*follows, caller);
    }
    assert_ne!(state.follows[0].0, state.follows[1].0);
}
