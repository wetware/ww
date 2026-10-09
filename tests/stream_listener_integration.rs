//! Actual StreamListener registration, loopback libp2p, and a compiled P3 child.
//!
//! Build with `make stream-lifecycle-probe`. Set `WW_REQUIRE_P3_FIXTURES=1` in
//! P3 lanes so a missing artifact is a failure, not a host-only lane skip.

#[path = "support/ticked_executor.rs"]
mod ticked_executor;

use std::cell::Cell;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use capnp::capability::Promise;
use capnp_rpc::{pry, rpc_twoparty_capnp::Side, twoparty::VatNetwork};
use futures::io::{AsyncReadExt, AsyncWriteExt};
use futures::FutureExt;
use libp2p::identity::Keypair;
use tokio::sync::{mpsc, oneshot, watch};
use tokio_util::compat::{TokioAsyncReadCompatExt, TokioAsyncWriteCompatExt};
use tracing_subscriber::prelude::*;

use ticked_executor::TickedExecutor;
use ww::host::Net;
use ww::launcher::create_runtime_client;
use ww::rpc::managed_rpc::{ManagedRpc, ManagedRpcSystem};
use ww::rpc::stream_listener::StreamListenerImpl;
use ww::rpc::{CachePolicy, ConnectionBudget, NetworkState, SwarmCommand};
use ww::system_capnp;

struct LocalSwarms {
    server_peer: libp2p::PeerId,
    server_control: libp2p_stream::Control,
    client_control: libp2p_stream::Control,
    _server_commands: mpsc::Sender<SwarmCommand>,
    _client_commands: mpsc::Sender<SwarmCommand>,
    server_task: tokio::task::JoinHandle<anyhow::Result<()>>,
    client_task: tokio::task::JoinHandle<anyhow::Result<()>>,
}

impl Drop for LocalSwarms {
    fn drop(&mut self) {
        self.server_task.abort();
        self.client_task.abort();
    }
}

async fn local_swarms() -> LocalSwarms {
    let server_key = Keypair::generate_ed25519();
    let server = Net::new(
        vec!["/ip4/127.0.0.1/tcp/0".parse().expect("listen address")],
        server_key,
        None,
        Vec::new(),
    )
    .expect("create server swarm");
    let server_peer = server.local_peer_id();
    let server_control = server.stream_control();
    let server_state = NetworkState::from_peer_id(server_peer.to_bytes());
    let (server_commands, server_cmd_rx) = mpsc::channel(8);
    let server_task = tokio::task::spawn_local(server.run(server_state.clone(), server_cmd_rx));
    let server_addr = server_state.wait_for_listen_addr().await;
    let server_addr =
        libp2p::Multiaddr::try_from(server_addr).expect("published server listen address");

    let client_key = Keypair::generate_ed25519();
    let client = Net::new(Vec::new(), client_key, None, Vec::new()).expect("create client swarm");
    let client_peer = client.local_peer_id();
    let client_control = client.stream_control();
    let client_state = NetworkState::from_peer_id(client_peer.to_bytes());
    let (client_commands, client_cmd_rx) = mpsc::channel(8);
    let client_task = tokio::task::spawn_local(client.run(client_state, client_cmd_rx));

    let (connected_tx, connected_rx) = oneshot::channel();
    client_commands
        .send(SwarmCommand::Connect {
            peer_id: server_peer,
            addrs: vec![server_addr],
            reply: connected_tx,
        })
        .await
        .expect("queue local swarm connection");
    connected_rx
        .await
        .expect("local connection reply")
        .expect("connect local swarms");

    LocalSwarms {
        server_peer,
        server_control,
        client_control,
        _server_commands: server_commands,
        _client_commands: client_commands,
        server_task,
        client_task,
    }
}

struct Child {
    exit: oneshot::Receiver<Result<i32, capnp::Error>>,
}

/// Forward through the public Executor API. The only retained child observation
/// is a dispatched wait, which must not itself keep Process ownership alive.
struct ObservedExecutor {
    inner: system_capnp::executor::Client,
    spawned: mpsc::UnboundedSender<Child>,
    calls: Arc<AtomicUsize>,
    fail_spawn: bool,
}

struct DenyKill(Rc<Cell<usize>>);

impl membrane::Policy for DenyKill {
    fn check(&self, interface_id: u64, method_id: u16) -> Result<(), capnp::Error> {
        use capnp::traits::HasTypeId;
        if interface_id == system_capnp::process::Client::TYPE_ID && method_id == 5 {
            self.0.set(self.0.get() + 1);
            return Err(membrane::denied_error(
                interface_id,
                method_id,
                "test denies explicit kill",
            ));
        }
        Ok(())
    }
}

#[allow(refining_impl_trait)]
impl system_capnp::executor::Server for ObservedExecutor {
    fn cid(
        self: capnp::capability::Rc<Self>,
        _params: system_capnp::executor::CidParams,
        mut results: system_capnp::executor::CidResults,
    ) -> Promise<(), capnp::Error> {
        let cid = self.inner.cid_request().send().promise;
        Promise::from_future(async move {
            let response = cid.await?;
            results.get().set_cid(response.get()?.get_cid()?);
            Ok(())
        })
    }

    fn spawn(
        self: capnp::capability::Rc<Self>,
        params: system_capnp::executor::SpawnParams,
        mut results: system_capnp::executor::SpawnResults,
    ) -> Promise<(), capnp::Error> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        if self.fail_spawn {
            return Promise::err(capnp::Error::disconnected(
                "injected executor failure".into(),
            ));
        }
        let mut request = self.inner.spawn_request();
        request
            .get()
            .set_membrane(pry!(pry!(params.get()).get_membrane()));
        let spawn = request.send().promise;
        Promise::from_future(async move {
            let response = spawn.await?;
            let process = response.get()?.get_process()?;
            let wait = process.wait_request().send().promise;
            let (exit_tx, exit) = oneshot::channel();
            tokio::task::spawn_local(async move {
                let result = wait
                    .await
                    .and_then(|response| Ok(response.get()?.get_exit_code()));
                let _ = exit_tx.send(result);
            });
            self.spawned
                .send(Child { exit })
                .expect("observe child spawn");
            results.get().set_process(process);
            Ok(())
        })
    }
}

/// One Executor connection carries both siblings and their Process exports.
/// Its ownership is independent of each accepted connection supervisor.
struct SharedExecutor {
    server: ManagedRpc,
    client: ManagedRpc,
    remote: system_capnp::executor::Client,
}

impl SharedExecutor {
    fn new(executor: system_capnp::executor::Client) -> Self {
        let (server, client) = tokio::io::duplex(64 * 1024);
        let (server_read, server_write) = tokio::io::split(server);
        let (client_read, client_write) = tokio::io::split(client);
        let server = ManagedRpcSystem::new(
            Box::new(VatNetwork::new(
                server_read.compat(),
                server_write.compat_write(),
                Side::Server,
                Default::default(),
            )),
            Some(executor.client),
        );
        let mut client = ManagedRpcSystem::new(
            Box::new(VatNetwork::new(
                client_read.compat(),
                client_write.compat_write(),
                Side::Client,
                Default::default(),
            )),
            None,
        );
        let remote = client.bootstrap(Side::Server);
        Self {
            server: ManagedRpc::spawn(server),
            client: ManagedRpc::spawn(client),
            remote,
        }
    }

    async fn disconnect(&mut self) {
        self.server
            .shutdown_and_join()
            .await
            .expect("join Executor server")
            .expect("disconnect Executor server");
        self.client
            .shutdown_and_join()
            .await
            .expect("join Executor client")
            .expect("disconnect Executor client");
    }
}

struct Gate {
    release: oneshot::Sender<Vec<u8>>,
    cancelled: oneshot::Receiver<()>,
}

struct PendingGraft(Option<oneshot::Sender<()>>);

impl Drop for PendingGraft {
    fn drop(&mut self) {
        if let Some(cancelled) = self.0.take() {
            let _ = cancelled.send(());
        }
    }
}

struct GatedMembrane(mpsc::UnboundedSender<Gate>);

#[allow(refining_impl_trait)]
impl system_capnp::membrane::Server for GatedMembrane {
    fn graft(
        self: capnp::capability::Rc<Self>,
        _params: system_capnp::membrane::GraftParams,
        mut results: system_capnp::membrane::GraftResults,
    ) -> Promise<(), capnp::Error> {
        let (release, released) = oneshot::channel();
        let (cancelled_tx, cancelled) = oneshot::channel();
        self.0
            .send(Gate { release, cancelled })
            .expect("observe supplied Membrane call");
        let mut pending = PendingGraft(Some(cancelled_tx));
        Promise::from_future(async move {
            let data = released
                .await
                .map_err(|_| capnp::Error::failed("gate abandoned".into()))?;
            results.get().set_peer_id(&data);
            pending.0.take();
            Ok(())
        })
    }
}

fn fixture_wasm() -> Option<Vec<u8>> {
    let path = std::env::var_os("WW_STREAM_LIFECYCLE_FIXTURE")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            Path::new(env!("CARGO_MANIFEST_DIR")).join(
                "target/stream-lifecycle-probe/wasm32-wasip3/release/stream_lifecycle_probe.wasm",
            )
        });
    if !path.is_file() {
        assert!(
            std::env::var_os("WW_REQUIRE_P3_FIXTURES").is_none(),
            "required P3 fixture missing: {}; run `make stream-lifecycle-probe`",
            path.display()
        );
        eprintln!(
            "SKIP: P3 fixture missing: {}; run `make stream-lifecycle-probe`",
            path.display()
        );
        return None;
    }
    Some(std::fs::read(path).expect("read native P3 lifecycle fixture"))
}

async fn bounded<T>(future: impl std::future::Future<Output = T>) -> T {
    tokio::time::timeout(Duration::from_secs(10), future)
        .await
        .expect("deterministic lifecycle event did not arrive")
}

async fn budget_released(budget: &ConnectionBudget, remaining: usize) {
    // This is a failure watchdog, not test ordering. It also bounds a failed
    // assertion while the test deliberately holds Tokio's clock fixed.
    let watchdog = std::time::Instant::now() + Duration::from_secs(10);
    while budget.active() != remaining {
        assert!(
            std::time::Instant::now() < watchdog,
            "gateway permit did not return"
        );
        tokio::task::yield_now().await;
    }
}

/// Hold a runtime started with paused time, including during real loopback I/O.
/// Starting paused also aligns the deadline with Tokio's millisecond timer ticks.
/// A parked blocking task inhibits Tokio's automatic clock advancement. Its
/// wall-clock watchdog only lets a stalled test's ordinary timeouts fail.
struct ControlledClock {
    release: Option<std::sync::mpsc::Sender<()>>,
    task: Option<tokio::task::JoinHandle<()>>,
}

impl ControlledClock {
    fn hold() -> Self {
        let (release, parked) = std::sync::mpsc::channel();
        let task = tokio::task::spawn_blocking(move || {
            let _ = parked.recv_timeout(Duration::from_secs(10));
        });
        Self {
            release: Some(release),
            task: Some(task),
        }
    }

    async fn resume(mut self) {
        tokio::time::resume();
        self.release.take();
        self.task
            .take()
            .unwrap()
            .await
            .expect("join test clock gate");
    }
}

impl Drop for ControlledClock {
    fn drop(&mut self) {
        if self.release.is_some() {
            tokio::time::resume();
        }
    }
}

struct Harness {
    swarms: LocalSwarms,
    epoch: watch::Sender<authority::Epoch>,
    budget: ConnectionBudget,
    children: mpsc::UnboundedReceiver<Child>,
    grafts: mpsc::UnboundedReceiver<Gate>,
    calls: Arc<AtomicUsize>,
    denied_kills: Rc<Cell<usize>>,
    executor: system_capnp::executor::Client,
    executor_rpc: SharedExecutor,
    // Retain host roots for exactly the test's lifetime, never a child Process.
    _runtime: system_capnp::runtime::Client,
    _engine: TickedExecutor,
}

impl Harness {
    async fn new(wasm: &[u8], fail_spawn: bool) -> Self {
        admission_trace();
        let swarms = local_swarms().await;
        let (epoch, epoch_rx) = watch::channel(authority::Epoch {
            seq: 1,
            head: b"p3-acceptance".to_vec(),
            root: None,
        });
        let guard = authority::EpochGuard {
            issued_seq: 1,
            receiver: epoch_rx,
        };
        let engine = TickedExecutor::new();
        let runtime = create_runtime_client(
            false,
            authority::EpochGuard::fixed(authority::Epoch::zero()),
            engine.runtime_engine(),
            None,
            CachePolicy::Isolated,
        );
        let mut request = runtime.load_request();
        request.get().set_wasm(wasm);
        let executor = bounded(request.send().promise)
            .await
            .expect("Runtime.load P3 fixture")
            .get()
            .expect("load results")
            .get_executor()
            .expect("compiled Executor");
        let (spawned, children) = mpsc::unbounded_channel();
        let calls = Arc::new(AtomicUsize::new(0));
        let executor: system_capnp::executor::Client = capnp_rpc::new_client(ObservedExecutor {
            inner: executor,
            spawned,
            calls: calls.clone(),
            fail_spawn,
        });
        let denied_kills = Rc::new(Cell::new(0));
        let executor = membrane::membrane(executor, Rc::new(DenyKill(denied_kills.clone())));
        let executor_rpc = SharedExecutor::new(executor);
        let executor = executor_rpc.remote.clone();
        let (grafts_tx, grafts) = mpsc::unbounded_channel();
        let membrane = capnp_rpc::new_client(GatedMembrane(grafts_tx));
        let budget = ConnectionBudget::new(2).expect("two gateway connection slots");
        let listener: system_capnp::stream_listener::Client = capnp_rpc::new_client(
            StreamListenerImpl::new(swarms.server_control.clone(), guard)
                .with_budget(budget.clone()),
        );
        let mut request = listener.listen_request();
        request.get().set_executor(executor.clone());
        request.get().set_protocol("p3-lifecycle");
        request.get().set_membrane(membrane);
        bounded(request.send().promise)
            .await
            .expect("register actual StreamListener");
        Self {
            swarms,
            epoch,
            budget,
            children,
            grafts,
            calls,
            denied_kills,
            executor,
            executor_rpc,
            _runtime: runtime,
            _engine: engine,
        }
    }

    async fn open(&self) -> libp2p::Stream {
        let mut control = self.swarms.client_control.clone();
        bounded(control.open_stream(
            self.swarms.server_peer,
            ww::rpc::stream_protocol("p3-lifecycle").expect("stream protocol"),
        ))
        .await
        .expect("open local accepted libp2p stream")
    }

    async fn child(&mut self) -> Child {
        bounded(self.children.recv())
            .await
            .expect("normal executor spawned P3 child")
    }

    async fn executor_cid(&self) -> String {
        bounded(self.executor.cid_request().send().promise)
            .await
            .expect("shared Executor connection remains callable")
            .get()
            .unwrap()
            .get_cid()
            .unwrap()
            .to_str()
            .unwrap()
            .to_owned()
    }

    async fn gate(&mut self) -> Gate {
        bounded(self.grafts.recv())
            .await
            .expect("compiled child called supplied Membrane")
    }

    fn cancel(&self) {
        self.epoch.send_modify(|epoch| epoch.seq += 1);
    }
}

impl Drop for Harness {
    fn drop(&mut self) {
        self.cancel();
    }
}

async fn assert_killed(child: Child, gate: Gate) {
    assert_eq!(
        bounded(child.exit)
            .await
            .expect("backend exit observation")
            .expect("backend completed cleanup"),
        137
    );
    bounded(gate.cancelled)
        .await
        .expect("managed child RPC cancelled pending Membrane call");
}

#[tokio::test]
async fn real_p3_delayed_output_uses_supplied_membrane_and_completes() {
    let Some(wasm) = fixture_wasm() else {
        return;
    };
    tokio::task::LocalSet::new()
        .run_until(async {
            let mut h = Harness::new(&wasm, false).await;
            let mut stream = h.open().await;
            stream.write_all(b"request").await.expect("peer input");
            stream.close().await.expect("peer input EOF");
            let mut child = h.child().await;
            let gate = h.gate().await; // The guest calls graft only after consuming stdin EOF.
            assert_eq!(h.budget.active(), 1);
            assert!(child.exit.try_recv().is_err(), "EOF must retain execution");
            assert!(
                stream.read(&mut [0; 1]).now_or_never().is_none(),
                "response remains gated"
            );
            gate.release
                .send(b"supplied-membrane:".to_vec())
                .expect("release delayed response");
            let mut response = vec![0; b"supplied-membrane:request".len()];
            bounded(stream.read_exact(&mut response))
                .await
                .expect("exact response");
            assert_eq!(response, b"supplied-membrane:request");
            let finish = h.gate().await;
            assert_eq!(
                h.budget.active(),
                1,
                "child still holds its completion gate"
            );
            finish
                .release
                .send(b"finish".to_vec())
                .expect("allow normal exit");
            let mut tail = Vec::new();
            bounded(stream.read_to_end(&mut tail))
                .await
                .expect("final network EOF");
            assert!(tail.is_empty());
            assert_eq!(bounded(child.exit).await.unwrap().unwrap(), 0);
            budget_released(&h.budget, 0).await;
        })
        .await;
}

#[tokio::test(start_paused = true)]
async fn real_p3_completion_timeout_releases_permit_and_execution() {
    let Some(wasm) = fixture_wasm() else {
        return;
    };
    tokio::task::LocalSet::new()
        .run_until(async {
            let clock = ControlledClock::hold();
            let mut h = Harness::new(&wasm, false).await;
            let mut stream = h.open().await;
            let child = h.child().await;
            let started = tokio::time::Instant::now();
            stream.close().await.expect("input EOF");
            let gate = h.gate().await;
            assert_eq!(
                tokio::time::Instant::now(),
                started,
                "freeze time before peer EOF can arm the deadline"
            );
            tokio::time::advance(Duration::from_secs(29)).await;
            h.executor_cid().await;
            assert_eq!(h.budget.active(), 1, "completion grace remains open");
            assert_eq!(
                tokio::time::Instant::now(),
                started + Duration::from_secs(29)
            );
            tokio::time::advance(Duration::from_secs(1)).await;
            budget_released(&h.budget, 0).await;
            assert_killed(child, gate).await;
            assert_eq!(
                tokio::time::Instant::now(),
                started + Duration::from_secs(30),
                "the original deadline expired without advancing time for cleanup"
            );
            clock.resume().await;
            assert_eq!(
                h.denied_kills.get(),
                1,
                "cleanup must survive denied explicit kill"
            );
        })
        .await;
}

#[tokio::test]
async fn real_p3_host_cancellation_releases_permit_and_execution() {
    let Some(wasm) = fixture_wasm() else {
        return;
    };
    tokio::task::LocalSet::new()
        .run_until(async {
            let mut h = Harness::new(&wasm, false).await;
            let mut stream = h.open().await;
            stream.close().await.expect("input EOF");
            let child = h.child().await;
            let gate = h.gate().await;
            h.cancel();
            budget_released(&h.budget, 0).await;
            assert_killed(child, gate).await;
            assert_eq!(
                h.denied_kills.get(),
                1,
                "cancellation must release Process ownership"
            );
        })
        .await;
}

#[tokio::test]
async fn real_p3_peer_reset_releases_permit_and_execution() {
    let Some(wasm) = fixture_wasm() else {
        return;
    };
    tokio::task::LocalSet::new()
        .run_until(async {
            let mut h = Harness::new(&wasm, false).await;
            let mut stream = h.open().await;
            stream.close().await.expect("input EOF");
            let child = h.child().await;
            let gate = h.gate().await;
            h.swarms.client_task.abort();
            let _ = (&mut h.swarms.client_task).await; // Drop the socket before releasing output.
            drop(stream);
            gate.release
                .send(vec![b'x'; 256 * 1024])
                .expect("release backpressured response");
            budget_released(&h.budget, 0).await;
            assert_eq!(
                bounded(child.exit).await.unwrap().unwrap(),
                137,
                "failed peer must not strand child at its completion gate"
            );
        })
        .await;
}

#[tokio::test(start_paused = true)]
async fn real_p3_timing_out_one_sibling_preserves_the_other() {
    let Some(wasm) = fixture_wasm() else {
        return;
    };
    tokio::task::LocalSet::new()
        .run_until(async {
            let clock = ControlledClock::hold();
            let mut h = Harness::new(&wasm, false).await;
            let executor_cid = h.executor_cid().await;
            assert!(!executor_cid.is_empty());
            let mut a = h.open().await;
            let child_a = h.child().await;
            let mut b = h.open().await;
            b.write_all(b"sibling B")
                .await
                .expect("B input remains open");
            let mut child_b = h.child().await;
            assert_eq!(h.budget.active(), 2);
            let started = tokio::time::Instant::now();
            a.close().await.expect("A input EOF");
            let gate_a = h.gate().await;
            assert_eq!(
                tokio::time::Instant::now(),
                started,
                "sibling setup must not consume A's completion grace"
            );
            tokio::time::advance(Duration::from_secs(29)).await;
            assert_eq!(h.executor_cid().await, executor_cid);
            assert_eq!(
                h.budget.active(),
                2,
                "both siblings remain admitted at 29 seconds"
            );
            tokio::time::advance(Duration::from_secs(1)).await;
            budget_released(&h.budget, 1).await;
            assert_killed(child_a, gate_a).await;
            assert_eq!(
                h.denied_kills.get(),
                1,
                "A cleanup must release its Process through the shared connection"
            );
            assert_eq!(
                tokio::time::Instant::now(),
                started + Duration::from_secs(30)
            );
            assert_eq!(
                h.executor_cid().await,
                executor_cid,
                "A timeout must preserve the shared Executor RPC connection"
            );
            clock.resume().await;
            assert!(
                child_b.exit.try_recv().is_err(),
                "B still executes through the same Executor"
            );
            b.close().await.expect("B input EOF");
            h.gate()
                .await
                .release
                .send(b"independent:".to_vec())
                .unwrap();
            let mut response = vec![0; b"independent:sibling B".len()];
            bounded(b.read_exact(&mut response)).await.unwrap();
            assert_eq!(response, b"independent:sibling B");
            h.gate().await.release.send(b"finish".to_vec()).unwrap();
            bounded(b.read_to_end(&mut Vec::new())).await.unwrap();
            assert_eq!(bounded(child_b.exit).await.unwrap().unwrap(), 0);
            budget_released(&h.budget, 0).await;
            h.executor_rpc.disconnect().await;
            assert!(
                bounded(h.executor.cid_request().send().promise)
                    .await
                    .is_err(),
                "listener must use the RPC import, not bypass it with the local Executor"
            );
        })
        .await;
}

/// The existing handle span is entered after admission in the spawned task and
/// before the authoritative check. This changes the epoch at that exact
/// boundary without adding a test-only scheduling hook to production code.
struct AdmissionTrigger {
    thread: std::thread::ThreadId,
    epoch: watch::Sender<authority::Epoch>,
    budget: ConnectionBudget,
    admitted: oneshot::Sender<usize>,
}

struct InvalidateBeforeDispatch(Arc<Mutex<Option<AdmissionTrigger>>>);

fn admission_trace() -> &'static Arc<Mutex<Option<AdmissionTrigger>>> {
    static TRIGGER: OnceLock<Arc<Mutex<Option<AdmissionTrigger>>>> = OnceLock::new();
    TRIGGER.get_or_init(|| {
        let trigger = Arc::new(Mutex::new(None));
        let subscriber =
            tracing_subscriber::registry().with(InvalidateBeforeDispatch(trigger.clone()));
        // Install before any test registers tracing callsites. A fixed global
        // subscriber avoids callsite-interest races with parallel test threads.
        tracing::subscriber::set_global_default(subscriber).expect("test trace subscriber");
        trigger
    })
}

impl<S: tracing::Subscriber> tracing_subscriber::Layer<S> for InvalidateBeforeDispatch {
    fn on_new_span(
        &self,
        attrs: &tracing::span::Attributes<'_>,
        _id: &tracing::Id,
        _ctx: tracing_subscriber::layer::Context<'_, S>,
    ) {
        if attrs.metadata().name() == "stream.handle" {
            let mut trigger = self.0.lock().unwrap();
            if trigger
                .as_ref()
                .is_some_and(|trigger| trigger.thread == std::thread::current().id())
            {
                let trigger = trigger.take().unwrap();
                trigger.epoch.send_modify(|epoch| epoch.seq += 1);
                let _ = trigger.admitted.send(trigger.budget.active());
            }
        }
    }
}

#[tokio::test]
async fn real_p3_stale_final_admission_never_dispatches_executor() {
    let Some(wasm) = fixture_wasm() else {
        return;
    };
    tokio::task::LocalSet::new()
        .run_until(async {
            let h = Harness::new(&wasm, false).await;
            let (admitted_tx, admitted) = oneshot::channel();
            *admission_trace().lock().unwrap() = Some(AdmissionTrigger {
                thread: std::thread::current().id(),
                epoch: h.epoch.clone(),
                budget: h.budget.clone(),
                admitted: admitted_tx,
            });
            let mut stream = h.open().await;
            assert_eq!(
                bounded(admitted).await.unwrap(),
                1,
                "accepted before epoch invalidation"
            );
            let _ = bounded(stream.read_to_end(&mut Vec::new())).await;
            budget_released(&h.budget, 0).await;
            assert_eq!(
                h.calls.load(Ordering::SeqCst),
                0,
                "stale registration must not spawn"
            );
        })
        .await;
}

#[tokio::test]
async fn real_p3_executor_path_failure_returns_gateway_permit() {
    let Some(wasm) = fixture_wasm() else {
        return;
    };
    tokio::task::LocalSet::new()
        .run_until(async {
            let h = Harness::new(&wasm, true).await;
            let mut stream = h.open().await;
            let _ = bounded(stream.read_to_end(&mut Vec::new())).await;
            budget_released(&h.budget, 0).await;
            assert_eq!(
                h.calls.load(Ordering::SeqCst),
                1,
                "exercise failing public spawn dispatch"
            );
        })
        .await;
}
