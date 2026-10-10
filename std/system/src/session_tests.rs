use super::*;
use capnp::capability::{Promise, RemotePromise};
use futures::channel::mpsc;
use futures::{FutureExt, StreamExt, TryStreamExt};
use std::cell::RefCell;
use std::io;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

struct PendingRead;

impl futures::io::AsyncRead for PendingRead {
    fn poll_read(
        self: Pin<&mut Self>,
        _: &mut Context<'_>,
        _: &mut [u8],
    ) -> Poll<std::io::Result<usize>> {
        Poll::Pending
    }
}

struct Counted<F> {
    inner: Pin<Box<F>>,
    polls: Rc<Cell<usize>>,
}

impl<F> Counted<F> {
    fn new(inner: F, polls: Rc<Cell<usize>>) -> Self {
        Self {
            inner: Box::pin(inner),
            polls,
        }
    }
}

impl<F: Future> Future for Counted<F> {
    type Output = F::Output;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        self.polls.set(self.polls.get() + 1);
        self.inner.as_mut().poll(cx)
    }
}

struct OwnedExport(Rc<Cell<usize>>);

impl system_capnp::membrane::Server for OwnedExport {}

impl Drop for OwnedExport {
    fn drop(&mut self) {
        self.0.set(self.0.get() + 1);
    }
}

#[test]
fn cancel_before_first_rpc_poll_terminalizes_retained_connection() {
    let released = Rc::new(Cell::new(0));
    let owned: system_capnp::membrane::Client =
        capnp_rpc::new_client(OwnedExport(released.clone()));
    let network = VatNetwork::new(
        PendingRead,
        futures::io::sink(),
        Side::Client,
        Default::default(),
    );
    let mut rpc = membrane::rpc_system(Box::new(network), None);
    let retained: system_capnp::executor::Client = rpc.bootstrap(Side::Server);
    let mut request = retained.spawn_request();
    request.get().set_membrane(owned);
    let pending = request.send();
    let disconnect = rpc.get_disconnector();
    let rpc_polls = Rc::new(Cell::new(0));
    let application_polls = Rc::new(Cell::new(0));
    let session = select_session_with_disconnect(
        std::future::pending(),
        Counted::new(rpc, rpc_polls.clone()),
        Counted::new(std::future::pending(), application_polls.clone()),
        disconnect,
        None,
    );

    assert_eq!(released.get(), 0, "the queued call owns the export");
    drop(session);

    assert_eq!(rpc_polls.get(), 0, "cancellation must not poll RpcSystem");
    assert_eq!(application_polls.get(), 0, "application must not start");
    assert_eq!(
        released.get(),
        1,
        "cancellation before the first poll must release the queued export"
    );
    assert!(
        pending.promise.now_or_never().is_some_and(|r| r.is_err()),
        "the retained response must be terminal before cancellation returns"
    );
    assert!(
        retained
            .cid_request()
            .send()
            .promise
            .now_or_never()
            .is_some_and(|r| r.is_err()),
        "a retained import must already reject new calls"
    );
    drop(pending.pipeline);
    assert_eq!(released.get(), 1, "cleanup releases each owner once");
}

fn failed(message: &str) -> capnp::Error {
    capnp::Error::failed(message.to_owned())
}

fn assert_ready_error<T>(future: impl Future<Output = capnp::Result<T>>) {
    assert!(
        matches!(future.now_or_never(), Some(Err(_))),
        "connection-owned work must already be terminal without a driver poll"
    );
}

// Queued ownership exercises cleanup without relying on any peer execution.
struct QueuedConnection {
    rpc: Option<RpcSystem<Side>>,
    retained: system_capnp::executor::Client,
    pending: Option<RemotePromise<system_capnp::executor::spawn_results::Owned>>,
    released: Rc<Cell<usize>>,
}

impl QueuedConnection {
    fn new(bootstrap: Option<capnp::capability::Client>) -> Self {
        Self::with_reader(bootstrap, PendingRead)
    }

    fn with_reader(
        bootstrap: Option<capnp::capability::Client>,
        reader: impl futures::io::AsyncRead + Unpin + 'static,
    ) -> Self {
        Self::with_network(
            bootstrap,
            Box::new(VatNetwork::new(
                reader,
                futures::io::sink(),
                Side::Client,
                Default::default(),
            )),
        )
    }

    fn with_network(
        bootstrap: Option<capnp::capability::Client>,
        network: Box<dyn capnp_rpc::VatNetwork<Side>>,
    ) -> Self {
        let released = Rc::new(Cell::new(0));
        let owned: system_capnp::membrane::Client =
            capnp_rpc::new_client(OwnedExport(released.clone()));
        let mut rpc = membrane::rpc_system(network, bootstrap);
        let retained: system_capnp::executor::Client = rpc.bootstrap(Side::Server);
        let mut request = retained.spawn_request();
        request.get().set_membrane(owned);
        Self {
            rpc: Some(rpc),
            retained,
            pending: Some(request.send()),
            released,
        }
    }

    fn take_rpc(&mut self) -> (RpcSystem<Side>, capnp_rpc::Disconnector<Side>) {
        let rpc = self.rpc.take().unwrap();
        let disconnect = rpc.get_disconnector();
        (rpc, disconnect)
    }

    fn assert_terminal(&mut self) {
        assert_eq!(self.released.get(), 1, "connection must release its export");
        let pending = self.pending.take().unwrap();
        assert_ready_error(pending.promise);
        assert_ready_error(self.retained.cid_request().send().promise);
        drop(pending.pipeline);
        assert_eq!(
            self.released.get(),
            1,
            "cleanup must release ownership once"
        );
    }
}

// Native byte pipes use futures' buffering and wake registration. Both real RPC
// systems remain explicitly polled by the test; there is no RPC worker task.
type ByteReader = Box<dyn futures::io::AsyncRead + Unpin>;

struct ByteWriter(mpsc::UnboundedSender<Vec<u8>>);

impl futures::io::AsyncWrite for ByteWriter {
    fn poll_write(
        self: Pin<&mut Self>,
        _: &mut Context<'_>,
        bytes: &[u8],
    ) -> Poll<io::Result<usize>> {
        Poll::Ready(
            self.0
                .unbounded_send(bytes.to_vec())
                .map(|()| bytes.len())
                .map_err(|_| io::ErrorKind::BrokenPipe.into()),
        )
    }

    fn poll_flush(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(Ok(()))
    }

    fn poll_close(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<io::Result<()>> {
        self.0.close_channel();
        Poll::Ready(Ok(()))
    }
}

fn byte_pipe() -> (ByteReader, ByteWriter) {
    let (sender, receiver) = mpsc::unbounded::<Vec<u8>>();
    let reader = receiver.map(Ok::<_, io::Error>).into_async_read();
    (Box::new(reader), ByteWriter(sender))
}

struct WakeFlag(AtomicBool);

impl futures::task::ArcWake for WakeFlag {
    fn wake_by_ref(flag: &Arc<Self>) {
        flag.0.store(true, Ordering::Relaxed);
    }
}

// A single-threaded fixture has no clock or external source of readiness.
// Each pass polls both participants and the observed result. An unready pass
// without a wake is stranded; the bound diagnoses self-waking livelocks.
fn drive<F: Future>(future: F, mut progress: impl FnMut(&mut Context<'_>)) -> F::Output {
    let mut future = Box::pin(future);
    let flag = Arc::new(WakeFlag(AtomicBool::new(false)));
    let waker = futures::task::waker(flag.clone());
    let mut cx = Context::from_waker(&waker);
    for _ in 0..256 {
        flag.0.store(false, Ordering::Relaxed);
        progress(&mut cx);
        if let Poll::Ready(result) = future.as_mut().poll(&mut cx) {
            return result;
        }
        assert!(
            flag.0.load(Ordering::Relaxed),
            "RPC fixture stranded pending work without a registered wake"
        );
    }
    panic!("RPC fixture made no result progress after 256 wake-driven passes");
}

struct PendingPeer {
    calls: Rc<Cell<usize>>,
}

impl system_capnp::process::Server for PendingPeer {
    fn stdout(
        self: capnp::capability::Rc<Self>,
        _: system_capnp::process::StdoutParams,
        _: system_capnp::process::StdoutResults,
    ) -> impl Future<Output = capnp::Result<()>> + 'static {
        self.calls.set(self.calls.get() + 1);
        std::future::pending()
    }

    fn stdin(
        self: capnp::capability::Rc<Self>,
        _: system_capnp::process::StdinParams,
        _: system_capnp::process::StdinResults,
    ) -> impl Future<Output = capnp::Result<()>> + 'static {
        // Ordered dispatch on this same capability proves earlier stdout calls
        // reached the server before the test destroys the guest session.
        Promise::ok(())
    }
}

struct ExportedProcess(Rc<Cell<usize>>);

impl system_capnp::process::Server for ExportedProcess {}

impl Drop for ExportedProcess {
    fn drop(&mut self) {
        self.0.set(self.0.get() + 1);
    }
}

struct TransferBootstrap(RefCell<Option<system_capnp::process::Client>>);

impl system_capnp::membrane::Server for TransferBootstrap {
    fn graft(
        self: capnp::capability::Rc<Self>,
        _: system_capnp::membrane::GraftParams,
        mut results: system_capnp::membrane::GraftResults,
    ) -> impl Future<Output = capnp::Result<()>> + 'static {
        let exported = self.0.borrow_mut().take().unwrap();
        results
            .get()
            .init_extras(1)
            .get(0)
            .init_cap()
            .set_as_capability(exported.client.hook);
        Promise::ok(())
    }
}

struct PeerDriver {
    rpc: Pin<Box<RpcSystem<Side>>>,
    disconnect: Option<capnp_rpc::Disconnector<Side>>,
    completed: bool,
}

impl PeerDriver {
    fn new(rpc: RpcSystem<Side>) -> Self {
        let disconnect = rpc.get_disconnector();
        Self {
            rpc: Box::pin(rpc),
            disconnect: Some(disconnect),
            completed: false,
        }
    }

    fn advance(&mut self, cx: &mut Context<'_>) {
        if !self.completed && self.rpc.as_mut().poll(cx).is_ready() {
            self.completed = true;
        }
    }
}

impl Drop for PeerDriver {
    fn drop(&mut self) {
        if let Some(disconnect) = self.disconnect.take() {
            let _ = membrane::initiate_disconnect(disconnect).now_or_never();
        }
    }
}

type SessionFuture = Pin<Box<dyn Future<Output = capnp::Result<()>>>>;

struct RpcPair {
    session: Option<SessionFuture>,
    peer: PeerDriver,
    retained: system_capnp::process::Client,
    remote_export: system_capnp::process::Client,
    released: Rc<Cell<usize>>,
    dispatched: Rc<Cell<usize>>,
    polls: Rc<Cell<usize>>,
}

impl RpcPair {
    fn new() -> Self {
        let (guest_read, peer_write) = byte_pipe();
        let (peer_read, guest_write) = byte_pipe();
        let released = Rc::new(Cell::new(0));
        let exported: system_capnp::process::Client =
            capnp_rpc::new_client(ExportedProcess(released.clone()));
        let bootstrap: system_capnp::membrane::Client =
            capnp_rpc::new_client(TransferBootstrap(RefCell::new(Some(exported))));
        let bootstrap = membrane::RpcBootstrap::new(bootstrap.client);
        let mut guest = membrane::rpc_system(
            Box::new(VatNetwork::new(
                guest_read,
                guest_write,
                Side::Server,
                Default::default(),
            )),
            Some(bootstrap.client()),
        );
        let retained = guest.bootstrap(Side::Client);
        let dispatched = Rc::new(Cell::new(0));
        let peer_bootstrap: system_capnp::process::Client = capnp_rpc::new_client(PendingPeer {
            calls: dispatched.clone(),
        });
        let mut peer = membrane::rpc_system(
            Box::new(VatNetwork::new(
                peer_read,
                peer_write,
                Side::Client,
                Default::default(),
            )),
            Some(peer_bootstrap.client),
        );
        let remote_bootstrap: system_capnp::membrane::Client = peer.bootstrap(Side::Server);
        let disconnect = guest.get_disconnector();
        let polls = Rc::new(Cell::new(0));
        let mut session: SessionFuture = Box::pin(select_session_with_disconnect(
            std::future::pending(),
            Counted::new(guest, polls.clone()),
            std::future::pending(),
            disconnect,
            Some(bootstrap),
        ));
        let mut peer = PeerDriver::new(peer);
        let response = drive(remote_bootstrap.graft_request().send().promise, |cx| {
            assert!(session.as_mut().poll(cx).is_pending());
            peer.advance(cx);
        })
        .unwrap();
        let remote_export = response
            .get()
            .unwrap()
            .get_extras()
            .unwrap()
            .get(0)
            .get_cap()
            .get_as()
            .unwrap();
        drop(response);
        let mut pair = Self {
            session: Some(session),
            peer,
            retained,
            remote_export,
            released,
            dispatched,
            polls,
        };
        pair.barrier();
        assert_eq!(
            pair.released.get(),
            0,
            "peer retains a real exported capability"
        );
        pair
    }

    fn drive<F: Future>(&mut self, future: F) -> F::Output {
        drive(future, |cx| {
            if let Some(session) = &mut self.session {
                assert!(session.as_mut().poll(cx).is_pending());
            }
            self.peer.advance(cx);
        })
    }

    fn barrier(&mut self) {
        let response = self.retained.stdin_request().send().promise;
        self.drive(response).unwrap();
    }

    fn cancel(&mut self) {
        let polls = self.polls.get();
        drop(self.session.take().unwrap());
        assert_eq!(
            self.polls.get(),
            polls,
            "cleanup must never repoll RpcSystem"
        );
        assert_eq!(
            self.released.get(),
            1,
            "cancellation synchronously releases exports"
        );
        assert_ready_error(self.retained.stdin_request().send().promise);
    }
}

#[test]
fn cancellation_after_rpc_roundtrips_releases_exports_and_breaks_retained_import() {
    let mut pair = RpcPair::new();
    pair.barrier();
    pair.barrier();
    pair.cancel();
    let response = pair.remote_export.stdin_request().send().promise;
    assert!(pair.drive(response).is_err());
    assert_eq!(pair.released.get(), 1);
}

#[test]
fn cancellation_settles_retained_ordinary_response() {
    let mut pair = RpcPair::new();
    let pending = pair.retained.stdout_request().send();
    drop(pending.pipeline);
    pair.barrier();
    assert_eq!(pair.dispatched.get(), 1);
    pair.cancel();
    assert_ready_error(pending.promise);
}

#[test]
fn cancellation_settles_response_while_pipeline_is_retained() {
    let mut pair = RpcPair::new();
    let pending = pair.retained.stdout_request().send();
    pair.barrier();
    assert_eq!(pair.dispatched.get(), 1);
    pair.cancel();
    assert_ready_error(pending.promise);
    assert_ready_error(pending.pipeline.get_stream().close_request().send().promise);
}

#[test]
fn cancellation_breaks_promised_capability_and_its_pending_call() {
    let mut pair = RpcPair::new();
    let pending = pair.retained.stdout_request().send();
    let promised = pending.pipeline.get_stream();
    let promised_call = promised.close_request().send();
    pair.barrier();
    assert_eq!(pair.dispatched.get(), 1);
    pair.cancel();
    assert!(promised.client.when_resolved().now_or_never().is_some());
    assert_ready_error(promised_call.promise);
    assert_ready_error(promised.close_request().send().promise);
    assert_ready_error(pending.promise);
    drop(pending.pipeline);
}

#[test]
fn cancelling_one_session_preserves_sibling_rpc_and_new_session_work() {
    let mut victim = RpcPair::new();
    let mut sibling = RpcPair::new();
    let sibling_response = sibling.retained.stdin_request().send().promise;
    victim.cancel();
    sibling.drive(sibling_response).unwrap();
    sibling.barrier();
    assert_eq!(sibling.released.get(), 0);
    let mut later = RpcPair::new();
    later.barrier();
    sibling.cancel();
    later.barrier();
    later.cancel();
}

struct OnDrop<F> {
    inner: Pin<Box<F>>,
    action: Option<Box<dyn FnOnce()>>,
}

impl<F> OnDrop<F> {
    fn new(inner: F, action: impl FnOnce() + 'static) -> Self {
        Self {
            inner: Box::pin(inner),
            action: Some(Box::new(action)),
        }
    }
}

impl<F: Future> Future for OnDrop<F> {
    type Output = F::Output;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        self.inner.as_mut().poll(cx)
    }
}

impl<F> Drop for OnDrop<F> {
    fn drop(&mut self) {
        self.action.take().unwrap()();
    }
}

type DropObservations = Rc<RefCell<Vec<(&'static str, bool, usize)>>>;

fn observe_drop<F>(
    inner: F,
    stage: &'static str,
    connection: &QueuedConnection,
    observations: &DropObservations,
) -> OnDrop<F> {
    let retained = connection.retained.clone();
    let released = connection.released.clone();
    let observations = observations.clone();
    OnDrop::new(inner, move || {
        // Record instead of asserting inside Drop so a violated ordering cannot
        // abort the test process while another panic is already unwinding.
        let terminal = matches!(
            retained.cid_request().send().promise.now_or_never(),
            Some(Err(_))
        );
        observations
            .borrow_mut()
            .push((stage, terminal, released.get()));
    })
}

#[test]
fn unpolled_application_capture_drops_after_connection_terminalization() {
    let mut connection = QueuedConnection::new(None);
    let (rpc, disconnect) = connection.take_rpc();
    let observations = Rc::new(RefCell::new(Vec::new()));
    let application = observe_drop(
        std::future::pending(),
        "application",
        &connection,
        &observations,
    );
    let rpc = observe_drop(rpc, "rpc", &connection, &observations);
    let transport = observe_drop(
        std::future::pending(),
        "transport",
        &connection,
        &observations,
    );
    let factory_calls = Rc::new(Cell::new(0));
    let calls = factory_calls.clone();
    let session = select_session_with_cleanup(
        transport,
        rpc,
        move || {
            calls.set(calls.get() + 1);
            application
        },
        SessionCleanup::new(disconnect, None),
    );
    drop(session);
    assert_eq!(
        factory_calls.get(),
        0,
        "uncalled application must remain uncalled"
    );
    assert_eq!(
        *observations.borrow(),
        [
            ("application", true, 1),
            ("rpc", true, 1),
            ("transport", true, 1)
        ]
    );
    connection.assert_terminal();
}

#[test]
fn pending_application_drop_and_selected_error_observe_terminal_connection() {
    for cancel in [true, false] {
        let mut connection = QueuedConnection::new(None);
        let (rpc, disconnect) = connection.take_rpc();
        let observations = Rc::new(RefCell::new(Vec::new()));
        let application = observe_drop(
            futures::future::poll_fn(move |_| {
                if cancel {
                    Poll::Pending
                } else {
                    Poll::Ready(Err(failed("selected application error")))
                }
            }),
            "application",
            &connection,
            &observations,
        );
        let rpc = observe_drop(rpc, "rpc", &connection, &observations);
        let transport = observe_drop(
            std::future::pending(),
            "transport",
            &connection,
            &observations,
        );
        let mut session = Box::pin(select_session_with_disconnect(
            transport,
            rpc,
            application,
            disconnect,
            None,
        ));
        let result = session
            .as_mut()
            .poll(&mut Context::from_waker(futures::task::noop_waker_ref()));
        if cancel {
            assert!(result.is_pending());
            assert!(observations.borrow().is_empty());
        } else {
            assert!(
                matches!(result, Poll::Ready(Err(ref error)) if error.extra.contains("selected application error"))
            );
            // No extra poll or future destruction may be needed to certify a
            // result that the caller has already observed as Ready.
            assert_eq!(observations.borrow().len(), 3);
        }
        drop(session);
        assert_eq!(
            *observations.borrow(),
            [
                ("application", true, 1),
                ("rpc", true, 1),
                ("transport", true, 1)
            ]
        );
        connection.assert_terminal();
    }
}

#[test]
fn successful_application_terminalizes_before_waiting_for_transport_completion() {
    let mut connection = QueuedConnection::new(None);
    let (rpc, disconnect) = connection.take_rpc();
    let transport_ready = Rc::new(Cell::new(false));
    let ready = transport_ready.clone();
    let transport = futures::future::poll_fn(move |_| {
        if ready.get() {
            Poll::Ready(Ok(()))
        } else {
            Poll::Pending
        }
    });
    let polls = Rc::new(Cell::new(0));
    let mut session = Box::pin(select_session_with_disconnect(
        transport,
        Counted::new(rpc, polls.clone()),
        std::future::ready(Ok(())),
        disconnect,
        None,
    ));
    let mut cx = Context::from_waker(futures::task::noop_waker_ref());
    assert!(session.as_mut().poll(&mut cx).is_pending());
    connection.assert_terminal();
    let selected_polls = polls.get();
    transport_ready.set(true);
    assert!(matches!(
        session.as_mut().poll(&mut cx),
        Poll::Ready(Ok(()))
    ));
    assert_eq!(
        polls.get(),
        selected_polls,
        "completed selection must not resume RPC"
    );
    drop(session);
    assert_eq!(connection.released.get(), 1);
}

struct PanicBootstrap;

impl system_capnp::membrane::Server for PanicBootstrap {}

impl Drop for PanicBootstrap {
    fn drop(&mut self) {
        panic!("secondary bootstrap destructor panic");
    }
}

fn panicking_bootstrap() -> membrane::RpcBootstrap {
    let root: system_capnp::membrane::Client = capnp_rpc::new_client(PanicBootstrap);
    membrane::RpcBootstrap::new(root.client)
}

fn panic_message(panic: &(dyn std::any::Any + Send)) -> &str {
    panic
        .downcast_ref::<&str>()
        .copied()
        .or_else(|| panic.downcast_ref::<String>().map(String::as_str))
        .unwrap_or("non-string panic")
}

#[test]
fn application_factory_panic_preserves_primary_and_cleans_without_rpc_poll() {
    let bootstrap = panicking_bootstrap();
    let mut connection = QueuedConnection::new(Some(bootstrap.client()));
    let (rpc, disconnect) = connection.take_rpc();
    let polls = Rc::new(Cell::new(0));
    let session = select_session_with_cleanup(
        std::future::pending(),
        Counted::new(rpc, polls.clone()),
        || -> std::future::Pending<capnp::Result<()>> { panic!("primary factory panic") },
        SessionCleanup::new(disconnect, Some(bootstrap)),
    );
    let panic = catch_unwind(AssertUnwindSafe(|| session.now_or_never()))
        .expect_err("factory unwind must propagate");
    assert_eq!(panic_message(&*panic), "primary factory panic");
    assert_eq!(polls.get(), 0);
    connection.assert_terminal();
}

#[test]
fn application_poll_panic_preserves_primary_and_orders_reentrant_destructors() {
    let bootstrap = panicking_bootstrap();
    let mut connection = QueuedConnection::new(Some(bootstrap.client()));
    let (rpc, disconnect) = connection.take_rpc();
    let observations = Rc::new(RefCell::new(Vec::new()));
    let application = observe_drop(
        futures::future::poll_fn(|_| -> Poll<capnp::Result<()>> {
            panic!("primary application poll panic")
        }),
        "application",
        &connection,
        &observations,
    );
    let polls = Rc::new(Cell::new(0));
    let rpc = observe_drop(
        Counted::new(rpc, polls.clone()),
        "rpc",
        &connection,
        &observations,
    );
    let session = select_session_with_disconnect(
        std::future::pending(),
        rpc,
        application,
        disconnect,
        Some(bootstrap),
    );
    let panic = catch_unwind(AssertUnwindSafe(|| session.now_or_never()))
        .expect_err("application unwind must propagate");
    assert_eq!(panic_message(&*panic), "primary application poll panic");
    assert_eq!(polls.get(), 1, "cleanup must not repoll the RPC driver");
    assert_eq!(
        *observations.borrow(),
        [("application", true, 1), ("rpc", true, 1)]
    );
    connection.assert_terminal();
}

#[test]
fn successful_selection_reports_cleanup_failure_without_waiting_for_transport() {
    let bootstrap = panicking_bootstrap();
    let mut connection = QueuedConnection::new(Some(bootstrap.client()));
    let (rpc, disconnect) = connection.take_rpc();
    let result = select_session_with_disconnect(
        std::future::pending(),
        rpc,
        std::future::ready(Ok(())),
        disconnect,
        Some(bootstrap),
    )
    .now_or_never()
    .expect("cleanup failure must return without waiting for pending transport")
    .expect_err("cleanup failure cannot certify success");
    assert!(result
        .extra
        .contains("secondary bootstrap destructor panic"));
    connection.assert_terminal();
}

#[test]
fn selected_application_error_remains_primary_over_cleanup_failure() {
    let bootstrap = panicking_bootstrap();
    let mut connection = QueuedConnection::new(Some(bootstrap.client()));
    let (rpc, disconnect) = connection.take_rpc();
    let result = select_session_with_disconnect(
        std::future::pending(),
        rpc,
        std::future::ready(Err(failed("primary application error"))),
        disconnect,
        Some(bootstrap),
    )
    .now_or_never()
    .unwrap()
    .unwrap_err();
    assert_eq!(result.extra, "primary application error");
    connection.assert_terminal();
}

#[test]
fn application_destructor_panic_does_not_skip_rpc_or_transport_release() {
    let mut connection = QueuedConnection::new(None);
    let (rpc, disconnect) = connection.take_rpc();
    let observations = Rc::new(RefCell::new(Vec::new()));
    let application = OnDrop::new(std::future::ready(Ok(())), || {
        panic!("application destructor panic")
    });
    let rpc = observe_drop(rpc, "rpc", &connection, &observations);
    let transport = observe_drop(
        std::future::pending(),
        "transport",
        &connection,
        &observations,
    );
    let result = select_session_with_disconnect(transport, rpc, application, disconnect, None)
        .now_or_never()
        .expect("destructor failure cannot wait for transport")
        .expect_err("destructor panic must be reported as cleanup failure");
    assert!(result.extra.contains("application destructor panic"));
    assert_eq!(
        *observations.borrow(),
        [("rpc", true, 1), ("transport", true, 1)]
    );
    connection.assert_terminal();
}

struct PanicRead;

impl futures::io::AsyncRead for PanicRead {
    fn poll_read(
        self: Pin<&mut Self>,
        _: &mut Context<'_>,
        _: &mut [u8],
    ) -> Poll<io::Result<usize>> {
        panic!("primary RPC read panic")
    }
}

#[test]
fn rpc_poll_unwind_terminalizes_without_repolling_unwound_driver() {
    let mut connection = QueuedConnection::with_reader(None, PanicRead);
    let (rpc, disconnect) = connection.take_rpc();
    let polls = Rc::new(Cell::new(0));
    let application_polls = Rc::new(Cell::new(0));
    let session = select_session_with_disconnect(
        std::future::pending(),
        Counted::new(rpc, polls.clone()),
        Counted::new(std::future::pending(), application_polls.clone()),
        disconnect,
        None,
    );
    let panic = catch_unwind(AssertUnwindSafe(|| session.now_or_never()))
        .expect_err("RPC unwind must propagate");
    assert_eq!(panic_message(&*panic), "primary RPC read panic");
    assert_eq!(
        polls.get(),
        1,
        "an unwound RpcSystem must never be repolled"
    );
    assert_eq!(application_polls.get(), 0);
    connection.assert_terminal();
}

#[test]
fn already_disconnected_session_cleanup_is_idempotent() {
    let mut connection = QueuedConnection::new(None);
    let (rpc, disconnect) = connection.take_rpc();
    let again = rpc.get_disconnector();
    membrane::initiate_disconnect(rpc.get_disconnector())
        .now_or_never()
        .unwrap()
        .unwrap();
    let session = select_session_with_disconnect(
        std::future::pending(),
        rpc,
        std::future::pending(),
        disconnect,
        None,
    );
    assert_eq!(connection.released.get(), 1);
    drop(session);
    membrane::initiate_disconnect(again)
        .now_or_never()
        .unwrap()
        .unwrap();
    connection.assert_terminal();
}

struct TerminalNetwork(VatNetwork<PendingRead>);

impl capnp_rpc::VatNetwork<Side> for TerminalNetwork {
    fn connect(&mut self, side: Side) -> Option<Box<dyn capnp_rpc::Connection<Side>>> {
        self.0.connect(side)
    }

    fn accept(&mut self) -> Promise<Box<dyn capnp_rpc::Connection<Side>>, capnp::Error> {
        self.0.accept()
    }

    fn drive_until_shutdown(&mut self) -> Promise<(), capnp::Error> {
        Promise::err(failed("RPC selected"))
    }
}

#[test]
fn real_nested_selector_preserves_transport_then_rpc_then_application_precedence() {
    for (transport_ready, rpc_ready, application_ready, expected_polls) in [
        (true, true, true, vec!["transport"]),
        (false, true, true, vec!["transport", "rpc"]),
        (false, true, false, vec!["transport", "rpc"]),
        (false, false, true, vec!["transport", "rpc", "application"]),
    ] {
        let mut connection = if rpc_ready {
            QueuedConnection::with_network(
                None,
                Box::new(TerminalNetwork(VatNetwork::new(
                    PendingRead,
                    futures::io::sink(),
                    Side::Client,
                    Default::default(),
                ))),
            )
        } else {
            QueuedConnection::new(None)
        };
        let (rpc, disconnect) = connection.take_rpc();
        let polls = Rc::new(RefCell::new(Vec::new()));
        let transport_polls = polls.clone();
        let transport = futures::future::poll_fn(move |_| {
            transport_polls.borrow_mut().push("transport");
            if transport_ready {
                Poll::Ready(Err(failed("transport selected")))
            } else {
                Poll::Pending
            }
        });
        let rpc_polls = polls.clone();
        let mut driver = Box::pin(rpc);
        let mut rpc: SessionFuture = if rpc_ready {
            // A ready network future does not promise that RpcSystem's first
            // poll is ready: its task set may need initialization passes.
            // Observe actual completion, retain that completed driver, and
            // present its buffered result to exactly one selector observation.
            let result = drive(driver.as_mut(), |_| {});
            Box::pin(OnDrop::new(std::future::ready(result), move || {
                drop(driver)
            }))
        } else {
            driver
        };
        let rpc = futures::future::poll_fn(move |cx| {
            rpc_polls.borrow_mut().push("rpc");
            rpc.as_mut().poll(cx)
        });
        let application_polls = polls.clone();
        let application = futures::future::poll_fn(move |_| {
            application_polls.borrow_mut().push("application");
            if application_ready {
                Poll::Ready(Err(failed("application selected")))
            } else {
                Poll::Pending
            }
        });
        let mut session = Box::pin(select_session_with_disconnect(
            transport,
            rpc,
            application,
            disconnect,
            None,
        ));
        let result = session
            .as_mut()
            .poll(&mut Context::from_waker(futures::task::noop_waker_ref()));
        let Poll::Ready(Err(error)) = result else {
            panic!("the first selector observation must return the ready result");
        };
        let expected = if transport_ready {
            "transport selected"
        } else if rpc_ready {
            "RPC selected"
        } else {
            "application selected"
        };
        assert_eq!(error.extra, expected);
        assert_eq!(*polls.borrow(), expected_polls);
        connection.assert_terminal();
        drop(session);
    }
}

#[test]
fn reentrant_application_destructor_cannot_export_new_connection_ownership() {
    let bootstrap_released = Rc::new(Cell::new(0));
    let root: system_capnp::membrane::Client =
        capnp_rpc::new_client(OwnedExport(bootstrap_released.clone()));
    let bootstrap = membrane::RpcBootstrap::new(root.client);
    let mut connection = QueuedConnection::new(Some(bootstrap.client()));
    let (rpc, disconnect) = connection.take_rpc();
    let retained = connection.retained.clone();
    let reentrant_released = Rc::new(Cell::new(0));
    let new_released = reentrant_released.clone();
    let bootstrap_drops = bootstrap_released.clone();
    let observed = Rc::new(Cell::new((false, 0, 0)));
    let observation = observed.clone();
    let application = OnDrop::new(std::future::pending(), move || {
        let new_export: system_capnp::membrane::Client =
            capnp_rpc::new_client(OwnedExport(new_released.clone()));
        let mut request = retained.spawn_request();
        request.get().set_membrane(new_export);
        let result = request.send().promise.now_or_never();
        observation.set((
            matches!(result, Some(Err(_))),
            new_released.get(),
            bootstrap_drops.get(),
        ));
    });
    let session = select_session_with_disconnect(
        std::future::pending(),
        rpc,
        application,
        disconnect,
        Some(bootstrap),
    );
    drop(session);
    assert_eq!(observed.get(), (true, 1, 1));
    assert_eq!(reentrant_released.get(), 1);
    connection.assert_terminal();
}
