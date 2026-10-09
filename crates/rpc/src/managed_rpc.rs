//! RPC ownership ends with explicit Cap'n Proto disconnection.
//!
//! Dropping a raw `RpcSystem` does not release its exports when imported
//! clients retain the connection. A managed owner signals its private worker
//! instead of aborting it. The same worker cuts runtime-created transport
//! cycles before dropping the RPC system; application-created capability
//! cycles remain the application's responsibility.

use capnp_rpc::RpcSystem;
use futures::FutureExt;
use std::future::Future;
use std::panic::AssertUnwindSafe;
use std::pin::Pin;
use std::task::{Context, Poll};
use std::time::Duration;
use tokio::task::{JoinError, JoinHandle};
use tokio_util::sync::CancellationToken;

/// Maximum time to flush RPC shutdown after exported ownership is released.
pub const DISCONNECT_GRACE: Duration = Duration::from_secs(1);

/// One owner of a local RPC worker. Drop requests shutdown without blocking.
///
/// The private worker is deliberately not abortable through this handle:
/// cancellation of an owner task, including before its first poll, must leave
/// the worker available to run disconnect. As with any async cleanup, the
/// LocalSet must continue running until workers have joined before runtime
/// shutdown. Retained imported clients do not own this handle.
#[must_use = "dropping the RPC owner requests disconnection"]
pub struct ManagedRpc {
    shutdown: CancellationToken,
    task: JoinHandle<capnp::Result<()>>,
}

/// Build an RPC system whose local bootstrap can be released at disconnect.
/// The raw driver remains private so every spawned owner has both release paths.
pub struct ManagedRpcSystem<VatId: 'static> {
    rpc: RpcSystem<VatId>,
    bootstrap: Option<membrane::RpcBootstrap>,
}

impl<VatId: 'static> ManagedRpcSystem<VatId> {
    pub fn new(
        network: Box<dyn capnp_rpc::VatNetwork<VatId>>,
        bootstrap: Option<capnp::capability::Client>,
    ) -> Self {
        let bootstrap = bootstrap.map(membrane::RpcBootstrap::new);
        let rpc = membrane::rpc_system(
            network,
            bootstrap.as_ref().map(membrane::RpcBootstrap::client),
        );
        Self { rpc, bootstrap }
    }

    pub fn bootstrap<C: capnp::capability::FromClientHook>(&mut self, vat: VatId) -> C {
        self.rpc.bootstrap(vat)
    }
}

/// Record observations in execution order; one failure cannot erase its cause.
fn record_failure(
    first: &mut Option<capnp::Error>,
    stage: &str,
    result: Result<capnp::Result<()>, Box<dyn std::any::Any + Send>>,
) {
    let result = result.unwrap_or_else(|panic| {
        let detail = panic
            .downcast_ref::<String>()
            .map(String::as_str)
            .or_else(|| panic.downcast_ref::<&str>().copied())
            .unwrap_or("non-string panic payload");
        let error = capnp::Error::failed(format!("{stage} panicked: {detail}"));
        if let Err(secondary) = std::panic::catch_unwind(AssertUnwindSafe(|| drop(panic))) {
            // Recursively panicking payloads have no safe general destructor.
            // Keep the original failure and continue the terminalization path.
            std::mem::forget(secondary);
            tracing::warn!(stage, "RPC panic payload destructor also panicked");
        }
        Err(error)
    });
    if let Err(error) = result {
        if let Some(cause) = first.as_ref() {
            tracing::warn!(stage, %error, first_error = %cause, "secondary RPC terminalization failure");
        } else {
            *first = Some(error);
        }
    }
}

impl ManagedRpc {
    /// Start the existing RPC driver task inside a Tokio LocalSet.
    pub fn spawn<VatId: 'static>(system: ManagedRpcSystem<VatId>) -> Self {
        let ManagedRpcSystem { rpc, bootstrap } = system;
        // Capture before spawning, including before the worker's first poll.
        let disconnector = rpc.get_disconnector();
        let shutdown = crate::local_tasks::shutdown_token();
        let cancelled = shutdown.clone();
        let task = tokio::task::spawn_local(crate::local_tasks::track(async move {
            let rpc = AssertUnwindSafe(rpc).catch_unwind();
            tokio::pin!(rpc);
            let result = tokio::select! {
                // Initialize the network before processing a preexisting
                // shutdown, so accept cannot create a connection afterwards.
                biased;
                result = &mut rpc => Some(result),
                _ = cancelled.cancelled() => None,
            };

            let mut first_error = None;
            let driver_live = result.is_none();
            if let Some(result) = result {
                record_failure(&mut first_error, "RPC driver", result);
            }

            // Bootstrap destructors and vendored disconnect can report user
            // panics. Each boundary must finish before the worker returns its
            // first failure; later failures remain secondary diagnostics.
            record_failure(
                &mut first_error,
                "RPC bootstrap clear",
                std::panic::catch_unwind(AssertUnwindSafe(|| {
                    if let Some(bootstrap) = &bootstrap {
                        bootstrap.clear();
                    }
                    Ok(())
                })),
            );
            record_failure(
                &mut first_error,
                "RPC disconnect",
                AssertUnwindSafe(membrane::initiate_disconnect(disconnector))
                    .catch_unwind()
                    .await,
            );

            // Only cancellation leaves a driver that is safe to poll. Never
            // resume a completed or unwound RpcSystem to flush transport.
            if driver_live {
                let result = tokio::time::timeout(DISCONNECT_GRACE, &mut rpc)
                    .await
                    .unwrap_or_else(|_| {
                        Ok(Err(capnp::Error::failed(
                            "RPC disconnect grace expired".into(),
                        )))
                    });
                record_failure(&mut first_error, "RPC disconnect grace", result);
            }
            first_error.map_or(Ok(()), Err)
        }));
        Self { shutdown, task }
    }

    /// Request shutdown once. Export release is asynchronous, not a child
    /// cleanup acknowledgement. Await this owner to join its bounded teardown.
    pub fn shutdown(&self) {
        self.shutdown.cancel();
    }

    /// Outer success certifies terminalization, even if the inner RPC result
    /// failed. A JoinError means the worker did not prove terminalization.
    pub async fn shutdown_and_join(&mut self) -> Result<capnp::Result<()>, JoinError> {
        self.shutdown();
        self.await
    }
}

impl Drop for ManagedRpc {
    fn drop(&mut self) {
        self.shutdown();
    }
}

impl Future for ManagedRpc {
    type Output = Result<capnp::Result<()>, JoinError>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        Pin::new(&mut self.task).poll(cx)
    }
}

#[cfg(test)]
mod tests {
    use crate::{
        cleanup_channel, ByteStreamImpl, CleanupObserver, CleanupPublisher, ProcessImpl,
        StreamMode, TerminationHandle,
    };
    use authority::system_capnp;
    use capnp::capability::Promise;
    use capnp_rpc::{rpc_twoparty_capnp::Side, twoparty::VatNetwork};
    use futures::FutureExt;
    use std::{
        cell::RefCell,
        future::Future,
        pin::Pin,
        rc::Rc,
        task::{Context, Poll},
        time::Duration,
    };
    use tokio::{
        io,
        sync::{oneshot, watch},
    };
    use tokio_util::compat::{TokioAsyncReadCompatExt, TokioAsyncWriteCompatExt};

    use super::{ManagedRpc, ManagedRpcSystem as RpcSystem};

    struct Exports(RefCell<Vec<system_capnp::process::Client>>, bool);
    impl Drop for Exports {
        fn drop(&mut self) {
            assert!(!self.1, "injected bootstrap destructor panic");
        }
    }
    impl system_capnp::membrane::Server for Exports {
        fn graft(
            self: capnp::capability::Rc<Self>,
            _: system_capnp::membrane::GraftParams,
            mut results: system_capnp::membrane::GraftResults,
        ) -> impl Future<Output = capnp::Result<()>> + 'static {
            let processes = self.0.take();
            let mut exports = results.get().init_extras(processes.len() as u32);
            for (i, process) in processes.into_iter().enumerate() {
                let mut export = exports.reborrow().get(i as u32);
                export.set_name("process");
                export.init_cap().set_as_capability(process.client.hook);
            }
            Promise::ok(())
        }
    }

    struct DenyKill;
    impl membrane::Policy for DenyKill {
        fn check(&self, _: u64, method: u16) -> capnp::Result<()> {
            if method == 5 {
                Err(capnp::Error::failed("kill denied".into()))
            } else {
                Ok(())
            }
        }
    }

    fn process() -> (
        system_capnp::process::Client,
        watch::Receiver<bool>,
        CleanupPublisher,
        CleanupObserver,
    ) {
        let (stream, _) = io::duplex(16);
        let stream: system_capnp::byte_stream::Client =
            capnp_rpc::new_client(ByteStreamImpl::new(stream, StreamMode::ReadOnly));
        let (cleanup, observer) = cleanup_channel();
        let (kill_tx, kill_rx) = watch::channel(false);
        let process = capnp_rpc::new_client(ProcessImpl::new(
            stream.clone(),
            stream.clone(),
            stream,
            observer.clone(),
            TerminationHandle::new(kill_tx),
        ));
        (process, kill_rx, cleanup, observer)
    }

    #[derive(Clone, Copy, Debug)]
    enum NetworkOutcome {
        Success,
        Error,
        Disconnected,
        Panic,
        PayloadPanic,
    }

    struct PanickingPayload;
    impl Drop for PanickingPayload {
        fn drop(&mut self) {
            panic!("secondary payload destructor panic");
        }
    }

    /// Force the network task to finish before the RPC disconnect task. This
    /// reproduces the pinned task-set terminal race without transport timing.
    struct TerminalNetwork {
        inner: Box<dyn capnp_rpc::VatNetwork<Side>>,
        terminal: Option<oneshot::Receiver<NetworkOutcome>>,
    }
    impl capnp_rpc::VatNetwork<Side> for TerminalNetwork {
        fn connect(&mut self, side: Side) -> Option<Box<dyn capnp_rpc::Connection<Side>>> {
            self.inner.connect(side)
        }
        fn accept(&mut self) -> Promise<Box<dyn capnp_rpc::Connection<Side>>, capnp::Error> {
            self.inner.accept()
        }
        fn drive_until_shutdown(&mut self) -> Promise<(), capnp::Error> {
            let inner = self.inner.drive_until_shutdown();
            let terminal = self.terminal.take().unwrap();
            Promise::from_future(async move {
                tokio::select! {
                    result = inner => result,
                    result = terminal => {
                        match result.unwrap_or(NetworkOutcome::Error) {
                            NetworkOutcome::Success => Ok(()),
                            NetworkOutcome::Error => Err(capnp::Error::failed("injected transport reset".into())),
                            NetworkOutcome::Disconnected => Err(capnp::Error::disconnected("injected disconnected network".into())),
                            NetworkOutcome::Panic => panic!("injected driver panic"),
                            NetworkOutcome::PayloadPanic => std::panic::panic_any(PanickingPayload),
                        }
                    },
                }
            })
        }
    }

    // An actual RpcSystem with a controlled driver task isolates host branch
    // handling from DisconnectNetwork's earlier network-error containment.
    struct ControlledNetwork(Option<Pin<Box<dyn Future<Output = capnp::Result<()>>>>>);
    impl capnp_rpc::VatNetwork<Side> for ControlledNetwork {
        fn connect(&mut self, _: Side) -> Option<Box<dyn capnp_rpc::Connection<Side>>> {
            None
        }
        fn accept(&mut self) -> Promise<Box<dyn capnp_rpc::Connection<Side>>, capnp::Error> {
            Promise::from_future(std::future::pending())
        }
        fn drive_until_shutdown(&mut self) -> Promise<(), capnp::Error> {
            Promise::from_future(self.0.take().unwrap())
        }
    }

    struct OnClear {
        _process: system_capnp::process::Client,
        action: Option<Box<dyn FnOnce()>>,
    }
    impl system_capnp::membrane::Server for OnClear {}
    impl Drop for OnClear {
        fn drop(&mut self) {
            if let Some(action) = self.action.take() {
                action();
            }
        }
    }

    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn completed_or_unwound_driver_is_not_repolled() {
        tokio::task::LocalSet::new()
            .run_until(async {
                for (outcome, expected) in [
                    (0, None),
                    (1, Some("first driver error")),
                    (2, Some("first driver panic")),
                    (3, Some("non-string panic payload")),
                    (4, Some("non-string panic payload")),
                ] {
                    let (process, kill, _cleanup, _) = process();
                    let (started, start) = oneshot::channel();
                    let (finish, done) = oneshot::channel();
                    let network = ControlledNetwork(Some(Box::pin(async move {
                        started.send(()).unwrap();
                        done.await.unwrap();
                        match outcome {
                            0 => Ok(()),
                            1 => Err(capnp::Error::failed("first driver error".into())),
                            2 => std::panic::panic_any(String::from("first driver panic")),
                            3 => std::panic::panic_any(42_u32),
                            _ => std::panic::panic_any(PanickingPayload),
                        }
                    })));
                    let bootstrap: system_capnp::membrane::Client =
                        capnp_rpc::new_client(OnClear {
                            _process: process,
                            action: (matches!(outcome, 1..=3)).then(|| {
                                Box::new(|| panic!("later bootstrap panic")) as Box<dyn FnOnce()>
                            }),
                        });
                    let bootstrap = membrane::RpcBootstrap::new(bootstrap.client);
                    let rpc =
                        capnp_rpc::RpcSystem::new(Box::new(network), Some(bootstrap.client()));
                    let mut owner = ManagedRpc::spawn(RpcSystem {
                        rpc,
                        bootstrap: Some(bootstrap),
                    });
                    watchdog(start).await.unwrap();
                    finish.send(()).unwrap();
                    let start = tokio::time::Instant::now();
                    let result = watchdog(&mut owner)
                        .await
                        .expect("expected user panic is an inner error");
                    match expected {
                        None => result.unwrap(),
                        Some(expected) => assert!(result.unwrap_err().extra.contains(expected)),
                    }
                    assert_eq!(
                        start.elapsed(),
                        Duration::ZERO,
                        "terminal driver must not enter the grace poll branch"
                    );
                    terminated(kill).await;
                }
            })
            .await;
    }

    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn cancellation_flushes_only_live_driver_and_bounds_grace() {
        tokio::task::LocalSet::new()
            .run_until(async {
                for outcome in 0..4 {
                    let (process, kill, _cleanup, _) = process();
                    let (started, start) = oneshot::channel();
                    let (finish, done) = oneshot::channel();
                    let flushed = Rc::new(std::cell::Cell::new(false));
                    let observed = flushed.clone();
                    let network = ControlledNetwork(Some(Box::pin(async move {
                        started.send(()).unwrap();
                        done.await.unwrap();
                        observed.set(true);
                        match outcome {
                            0 => Ok(()),
                            1 => Err(capnp::Error::failed("grace driver error".into())),
                            2 => panic!("grace driver panic"),
                            _ => std::future::pending().await,
                        }
                    })));
                    let bootstrap: system_capnp::membrane::Client =
                        capnp_rpc::new_client(OnClear {
                            _process: process,
                            action: Some(Box::new(move || {
                                finish.send(()).unwrap();
                            })),
                        });
                    let bootstrap = membrane::RpcBootstrap::new(bootstrap.client);
                    let rpc =
                        capnp_rpc::RpcSystem::new(Box::new(network), Some(bootstrap.client()));
                    let mut owner = ManagedRpc::spawn(RpcSystem {
                        rpc,
                        bootstrap: Some(bootstrap),
                    });
                    watchdog(start).await.unwrap();
                    let start = tokio::time::Instant::now();
                    let result = watchdog(owner.shutdown_and_join())
                        .await
                        .expect("grace failure is contained");
                    assert!(
                        flushed.get(),
                        "live driver must be polled after bootstrap release"
                    );
                    match outcome {
                        0 => result.unwrap(),
                        1 => assert!(result.unwrap_err().extra.contains("grace driver error")),
                        2 => assert!(result.unwrap_err().extra.contains("grace driver panic")),
                        _ => assert!(result.unwrap_err().extra.contains("grace expired")),
                    }
                    assert_eq!(
                        start.elapsed(),
                        if outcome == 3 {
                            super::DISCONNECT_GRACE
                        } else {
                            Duration::ZERO
                        }
                    );
                    terminated(kill).await;
                }
            })
            .await;
    }

    struct ResetRead<R> {
        inner: R,
        reset: oneshot::Receiver<()>,
    }
    impl<R: futures::io::AsyncRead + Unpin> futures::io::AsyncRead for ResetRead<R> {
        fn poll_read(
            mut self: Pin<&mut Self>,
            cx: &mut Context<'_>,
            buffer: &mut [u8],
        ) -> Poll<std::io::Result<usize>> {
            if Pin::new(&mut self.reset).poll(cx).is_ready() {
                return Poll::Ready(Err(std::io::ErrorKind::ConnectionReset.into()));
            }
            Pin::new(&mut self.inner).poll_read(cx, buffer)
        }
    }

    struct Pair {
        server: ManagedRpc,
        _client: ManagedRpc,
        processes: Vec<system_capnp::process::Client>,
        retained_import: system_capnp::membrane::Client,
        reset: oneshot::Sender<NetworkOutcome>,
        transport_reset: oneshot::Sender<()>,
    }

    async fn pair(processes: Vec<system_capnp::process::Client>) -> Pair {
        let peer_bootstrap: system_capnp::membrane::Client =
            capnp_rpc::new_client(Exports(RefCell::new(vec![]), false));
        pair_with_bootstrap(processes, peer_bootstrap.client, false).await
    }

    async fn pair_with_bootstrap(
        processes: Vec<system_capnp::process::Client>,
        peer_bootstrap: capnp::capability::Client,
        panic_on_clear: bool,
    ) -> Pair {
        let (a, b) = io::duplex(8192);
        let (ar, aw) = io::split(a);
        let (br, bw) = io::split(b);
        let bootstrap: system_capnp::membrane::Client =
            capnp_rpc::new_client(Exports(RefCell::new(processes), panic_on_clear));
        let (reset, terminal) = oneshot::channel();
        let (transport_reset, reset_rx) = oneshot::channel();
        let network = TerminalNetwork {
            inner: Box::new(VatNetwork::new(
                ResetRead {
                    inner: ar.compat(),
                    reset: reset_rx,
                },
                aw.compat_write(),
                Side::Server,
                Default::default(),
            )),
            terminal: Some(terminal),
        };
        let mut server = RpcSystem::new(Box::new(network), Some(bootstrap.client));
        let retained_import = server.bootstrap(Side::Client);
        let mut client = RpcSystem::new(
            Box::new(VatNetwork::new(
                br.compat(),
                bw.compat_write(),
                Side::Client,
                Default::default(),
            )),
            Some(peer_bootstrap),
        );
        let export: system_capnp::membrane::Client = client.bootstrap(Side::Server);
        let server = ManagedRpc::spawn(server);
        let client = ManagedRpc::spawn(client);
        let response = export.graft_request().send().promise.await.unwrap();
        let processes = response
            .get()
            .unwrap()
            .get_extras()
            .unwrap()
            .iter()
            .map(|export| {
                export
                    .get_cap()
                    .get_as::<system_capnp::process::Client>()
                    .unwrap()
            })
            .collect();
        drop(response);
        drop(export);
        Pair {
            server,
            _client: client,
            processes,
            retained_import,
            reset,
            transport_reset,
        }
    }

    async fn terminated(mut kill: watch::Receiver<bool>) {
        tokio::time::timeout(Duration::from_secs(2), kill.wait_for(|k| *k))
            .await
            .expect("managed disconnect must release final exported Process ownership")
            .expect("termination controller");
    }

    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn explicit_disconnect_releases_exports_with_retained_import() {
        tokio::task::LocalSet::new()
            .run_until(async {
                let (process, kill, _cleanup, _) = process();
                let mut pair = pair(vec![process]).await;
                pair.server.shutdown();
                let _ = (&mut pair.server).await;
                terminated(kill).await;
                assert!(pair
                    .retained_import
                    .graft_request()
                    .send()
                    .promise
                    .await
                    .is_err());
                assert!(pair.processes[0]
                    .stdout_request()
                    .send()
                    .promise
                    .await
                    .is_err());
            })
            .await;
    }

    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn transport_reset_after_task_set_completion_releases_exports() {
        tokio::task::LocalSet::new()
            .run_until(async {
                let (process, kill, _cleanup, _) = process();
                let mut pair = pair(vec![process]).await;
                pair.reset.send(NetworkOutcome::Error).unwrap();
                let result = tokio::time::timeout(Duration::from_secs(2), &mut pair.server)
                    .await
                    .expect("disconnect after a terminal RPC task set must not hang")
                    .unwrap();
                assert!(result.is_err());
                terminated(kill).await;
            })
            .await;
    }

    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn clean_network_disconnection_remains_successful() {
        tokio::task::LocalSet::new()
            .run_until(async {
                let (process, kill, _cleanup, _) = process();
                let mut pair = pair(vec![process]).await;
                pair.reset.send(NetworkOutcome::Disconnected).unwrap();
                watchdog(&mut pair.server).await.unwrap().unwrap();
                terminated(kill).await;
                assert!(
                    watchdog(pair.retained_import.graft_request().send().promise)
                        .await
                        .is_err()
                );
                assert!(watchdog(pair.processes[0].stdout_request().send().promise)
                    .await
                    .is_err());
            })
            .await;
    }

    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn terminal_network_rejects_response_with_retained_pipeline() {
        tokio::task::LocalSet::new()
            .run_until(async {
                let (process, _kill, _cleanup, _) = process();
                let mut pair = pair_with_bootstrap(vec![], process.client, false).await;
                let remote = system_capnp::process::Client {
                    client: pair.retained_import.client.clone(),
                };
                let pending: Vec<_> = (0..256).map(|_| remote.wait_request().send()).collect();
                remote.stdout_request().send().promise.await.unwrap(); // dispatch barrier
                pair.reset.send(NetworkOutcome::Error).unwrap();
                assert!((&mut pair.server).await.unwrap().is_err());
                for pending in pending {
                    let result = tokio::time::timeout(Duration::from_secs(2), pending.promise)
                        .await
                        .expect("terminal disconnect must reject responses while their pipeline is retained");
                    assert!(result.is_err());
                    drop(pending.pipeline);
                }
            })
            .await;
    }

    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn bootstrap_clear_panic_still_releases_other_exports() {
        tokio::task::LocalSet::new()
            .run_until(async {
                let (process, kill, _cleanup, _) = process();
                let peer: system_capnp::membrane::Client =
                    capnp_rpc::new_client(Exports(RefCell::new(vec![]), false));
                let mut pair = pair_with_bootstrap(vec![process], peer.client, true).await;
                let result = pair.server.shutdown_and_join().await;
                terminated(kill).await;
                let error = result
                    .expect("terminalization completed despite bootstrap panic")
                    .unwrap_err();
                assert!(error.extra.contains("injected bootstrap destructor panic"));
                assert!(pair
                    .retained_import
                    .graft_request()
                    .send()
                    .promise
                    .await
                    .is_err());
                assert!(pair.processes[0]
                    .stdout_request()
                    .send()
                    .promise
                    .await
                    .is_err());
            })
            .await;
    }

    // Every case also checks export release; first-cause text alone cannot
    // certify a worker that returned before terminalizing its connection.
    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn first_network_failure_survives_bootstrap_clear_panic() {
        tokio::task::LocalSet::new()
            .run_until(async {
                for (outcome, expected) in [
                    (NetworkOutcome::Error, "injected transport reset"),
                    (
                        NetworkOutcome::Disconnected,
                        "injected bootstrap destructor panic",
                    ),
                    (NetworkOutcome::Panic, "injected driver panic"),
                    (NetworkOutcome::PayloadPanic, "non-string panic payload"),
                    (
                        NetworkOutcome::Success,
                        "injected bootstrap destructor panic",
                    ),
                ] {
                    let (process, kill, _cleanup, _) = process();
                    let peer: system_capnp::membrane::Client =
                        capnp_rpc::new_client(Exports(RefCell::new(vec![]), false));
                    let mut pair = pair_with_bootstrap(vec![process], peer.client, true).await;
                    pair.reset.send(outcome).unwrap();
                    let start = tokio::time::Instant::now();
                    let error = watchdog(&mut pair.server)
                        .await
                        .expect("contained failures must prove terminalization")
                        .unwrap_err();
                    assert!(error.extra.contains(expected), "{error}");
                    assert_eq!(
                        start.elapsed(),
                        Duration::ZERO,
                        "completed driver must not enter grace polling"
                    );
                    terminated(kill).await;
                    assert!(
                        watchdog(pair.retained_import.graft_request().send().promise)
                            .await
                            .is_err()
                    );
                    assert!(watchdog(pair.processes[0].stdout_request().send().promise)
                        .await
                        .is_err());
                }
            })
            .await;
    }

    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn disconnect_network_preserves_first_failure_and_reports_disconnect_panic() {
        tokio::task::LocalSet::new()
            .run_until(async {
                for (outcome, expected) in [
                    (NetworkOutcome::Error, "injected transport reset"),
                    (
                        NetworkOutcome::Disconnected,
                        "injected disconnected network",
                    ),
                    (NetworkOutcome::Panic, "injected driver panic"),
                    (NetworkOutcome::PayloadPanic, "non-string panic payload"),
                    (
                        NetworkOutcome::Success,
                        "injected non-bootstrap export destructor panic",
                    ),
                ] {
                    let (process, kill, _cleanup, _) = process();
                    let drops = Rc::new(std::cell::Cell::new(0));
                    let panic = capnp_rpc::new_client(DropExport {
                        drops: drops.clone(),
                        panic: true,
                        reenter: None,
                    });
                    let mut pair = pair(vec![panic, process]).await;
                    pair.reset.send(outcome).unwrap();
                    let error = watchdog(&mut pair.server)
                        .await
                        .expect("network disconnect completed bookkeeping")
                        .unwrap_err();
                    assert!(error.extra.contains(expected), "{error}");
                    assert_eq!(drops.get(), 1);
                    terminated(kill).await;
                    assert!(
                        watchdog(pair.retained_import.graft_request().send().promise)
                            .await
                            .is_err()
                    );
                    for process in pair.processes {
                        assert!(watchdog(process.stdout_request().send().promise)
                            .await
                            .is_err());
                    }
                }
            })
            .await;
    }

    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn driver_panic_precedes_disconnect_destructor_panic() {
        tokio::task::LocalSet::new()
            .run_until(async {
                let (process, kill, _cleanup, _) = process();
                let drops = Rc::new(std::cell::Cell::new(0));
                let panic = capnp_rpc::new_client(DropExport {
                    drops: drops.clone(),
                    panic: true,
                    reenter: None,
                });
                let mut pair = terminal_pair(vec![panic, process]).await;
                let pending = pair.retained.stdout_request().send();
                let promised = pending.pipeline.get_stream();
                watchdog(pair.retained.stdin_request().send().promise)
                    .await
                    .unwrap();
                pair.panic_armed.set(true);
                let _trigger = pair.remote_bootstrap.graft_request().send();
                let start = tokio::time::Instant::now();
                let error = watchdog(&mut pair.server)
                    .await
                    .expect("driver panic must be contained")
                    .unwrap_err();
                assert!(
                    error.extra.contains("injected capability dispatch panic"),
                    "{error}"
                );
                assert_eq!(
                    start.elapsed(),
                    Duration::ZERO,
                    "unwound driver must not enter grace polling"
                );
                assert_eq!(drops.get(), 1);
                terminated(kill).await;
                assert!(watchdog(pending.promise).await.is_err());
                let _ = watchdog(promised.client.when_resolved()).await;
                assert!(watchdog(promised.close_request().send().promise)
                    .await
                    .is_err());
                assert!(
                    watchdog(pending.pipeline.get_stream().close_request().send().promise)
                        .await
                        .is_err()
                );
            })
            .await;
    }

    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn owner_task_cancellation_releases_exports() {
        tokio::task::LocalSet::new()
            .run_until(async {
                let (process, kill, _cleanup, _) = process();
                let pair = pair(vec![process]).await;
                let server = pair.server;
                let owner = tokio::task::spawn_local(async move {
                    let _server = server;
                    std::future::pending::<()>().await;
                });
                owner.abort();
                let _ = owner.await;
                terminated(kill).await;
                assert!(pair
                    .retained_import
                    .graft_request()
                    .send()
                    .promise
                    .await
                    .is_err());
            })
            .await;
    }

    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn cancellation_before_first_driver_poll_releases_queued_export() {
        tokio::task::LocalSet::new()
            .run_until(async {
                let (process, kill, _cleanup, _) = process();
                let (stream, _peer) = io::duplex(8192);
                let (reader, writer) = io::split(stream);
                let mut rpc = RpcSystem::new(
                    Box::new(VatNetwork::new(
                        reader.compat(),
                        writer.compat_write(),
                        Side::Client,
                        Default::default(),
                    )),
                    None,
                );
                let remote: system_capnp::executor::Client = rpc.bootstrap(Side::Server);
                let mut request = remote.spawn_request();
                request.get().set_membrane(system_capnp::membrane::Client {
                    client: process.client,
                });
                let pending = request.send();
                let driver = ManagedRpc::spawn(rpc);
                drop(driver);
                terminated(kill).await;
                assert!(pending.promise.await.is_err());
            })
            .await;
    }

    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn disconnect_releases_multiple_exports_and_pending_wait() {
        tokio::task::LocalSet::new()
            .run_until(async {
                let (a, kill_a, cleanup_a, observer_a) = process();
                let (b, kill_b, _cleanup_b, _) = process();
                let mut pair = pair(vec![a, b]).await;
                let mut wait = Box::pin(pair.processes[0].wait_request().send().promise);
                assert!(wait.as_mut().now_or_never().is_none());
                pair.processes[0]
                    .stdout_request()
                    .send()
                    .promise
                    .await
                    .unwrap(); // dispatch barrier
                pair.server.shutdown();
                let _ = (&mut pair.server).await;
                terminated(kill_a).await;
                terminated(kill_b).await;
                assert!(wait.await.is_err());
                assert_eq!(observer_a.state(), crate::CleanupState::Running);
                cleanup_a.cleaned(137);
                assert_eq!(observer_a.wait().await, Ok(137));
            })
            .await;
    }

    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn independent_remote_owners_keep_execution_until_final_release() {
        tokio::task::LocalSet::new()
            .run_until(async {
                let scope = crate::local_tasks::LocalTaskScope::enter();
                let (process, kill, _cleanup, _) = process();
                let wrapped = membrane::membrane(process, Rc::new(DenyKill));
                let mut b = pair(vec![wrapped.clone()]).await;
                let mut c = pair(vec![wrapped]).await;
                b.server.shutdown();
                let _ = (&mut b.server).await;
                assert!(!*kill.borrow(), "remote C still owns execution");
                assert!(c.processes[0].stdout_request().send().promise.await.is_ok());
                c.processes.clear(); // final Release on a still-usable connection
                terminated(kill).await;
                let _ = c.server.shutdown_and_join().await;
                drop(b.server); // Already disconnected owner drop is harmless.
                scope.shutdown();
                scope.wait().await;
            })
            .await;
    }
    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn driver_panic_still_disconnects_retained_exports() {
        tokio::task::LocalSet::new()
            .run_until(async {
                let (process, kill, _cleanup, _) = process();
                let mut pair = pair(vec![process]).await;
                pair.reset.send(NetworkOutcome::Panic).unwrap();
                let _ = (&mut pair.server).await;
                terminated(kill).await;
            })
            .await;
    }

    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn membrane_clones_denied_kill_and_recursive_results_preserve_ownership() {
        tokio::task::LocalSet::new()
            .run_until(async {
                let (process, kill, _cleanup, _) = process();
                let a = membrane::membrane(process, Rc::new(DenyKill));
                let b = a.clone();
                assert!(a.kill_request().send().promise.await.is_err());
                assert!(!*kill.borrow(), "denied kill must not terminate execution");
                let stream = a
                    .stdout_request()
                    .send()
                    .promise
                    .await
                    .unwrap()
                    .get()
                    .unwrap()
                    .get_stream()
                    .unwrap();
                drop(a);
                assert!(
                    !*kill.borrow(),
                    "the remaining exterior clone owns execution"
                );
                drop(b);
                terminated(kill).await;
                // A recursive result wrapper and the weak membrane registry must
                // not retain the originating Process wrapper.
                assert!(stream.close_request().send().promise.await.is_ok());
            })
            .await;
    }

    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn detected_transport_reset_releases_process() {
        tokio::task::LocalSet::new()
            .run_until(async {
                let (process, kill, _cleanup, _) = process();
                let mut pair = pair(vec![process]).await;
                pair.transport_reset.send(()).unwrap();
                let _ = tokio::time::timeout(Duration::from_secs(2), &mut pair.server)
                    .await
                    .unwrap();
                terminated(kill).await;
                assert!(pair
                    .retained_import
                    .graft_request()
                    .send()
                    .promise
                    .await
                    .is_err());
                drop(pair.server);
            })
            .await;
    }

    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn peer_owner_loss_releases_host_exports_and_already_closed_owner_is_harmless() {
        tokio::task::LocalSet::new()
            .run_until(async {
                let (process, kill, _cleanup, _) = process();
                let mut pair = pair(vec![process]).await;
                drop(pair._client);
                let _ = tokio::time::timeout(Duration::from_secs(2), &mut pair.server)
                    .await
                    .unwrap();
                terminated(kill).await;
                drop(pair.server);
                assert!(pair.processes[0]
                    .stdout_request()
                    .send()
                    .promise
                    .await
                    .is_err());
            })
            .await;
    }

    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn disconnect_releases_process_bootstrap_with_retained_import() {
        tokio::task::LocalSet::new()
            .run_until(async {
                let (process, kill, _cleanup, _) = process();
                let (a, b) = io::duplex(8192);
                let (ar, aw) = io::split(a);
                let (br, bw) = io::split(b);
                let mut server = RpcSystem::new(
                    Box::new(VatNetwork::new(
                        ar.compat(),
                        aw.compat_write(),
                        Side::Server,
                        Default::default(),
                    )),
                    Some(process.client),
                );
                let retained: system_capnp::membrane::Client = server.bootstrap(Side::Client);
                let mut client = RpcSystem::new(
                    Box::new(VatNetwork::new(
                        br.compat(),
                        bw.compat_write(),
                        Side::Client,
                        Default::default(),
                    )),
                    None,
                );
                let remote: system_capnp::process::Client = client.bootstrap(Side::Server);
                let mut server = ManagedRpc::spawn(server);
                let _client = ManagedRpc::spawn(client);
                remote.stdout_request().send().promise.await.unwrap();
                let _ = server.shutdown_and_join().await;
                terminated(kill).await;
                assert!(remote.stdout_request().send().promise.await.is_err());
                assert!(retained.graft_request().send().promise.await.is_err());
            })
            .await;
    }

    // These regressions keep the failing driver's outbound work alive. Removing
    // direct question-to-pipeline rejection strands it after an RPC unwind.
    struct PendingPeer;
    impl system_capnp::process::Server for PendingPeer {
        fn stdout(
            self: capnp::capability::Rc<Self>,
            _: system_capnp::process::StdoutParams,
            _: system_capnp::process::StdoutResults,
        ) -> impl Future<Output = capnp::Result<()>> + 'static {
            std::future::pending()
        }
        fn stdin(
            self: capnp::capability::Rc<Self>,
            _: system_capnp::process::StdinParams,
            _: system_capnp::process::StdinResults,
        ) -> impl Future<Output = capnp::Result<()>> + 'static {
            Promise::ok(()) // An ordered dispatch barrier for preceding stdout calls.
        }
    }

    struct TerminalExports {
        exports: RefCell<Vec<system_capnp::process::Client>>,
        panic_armed: Rc<std::cell::Cell<bool>>,
    }
    impl system_capnp::membrane::Server for TerminalExports {
        fn graft(
            self: capnp::capability::Rc<Self>,
            _: system_capnp::membrane::GraftParams,
            mut results: system_capnp::membrane::GraftResults,
        ) -> impl Future<Output = capnp::Result<()>> + 'static {
            assert!(
                !self.panic_armed.get(),
                "injected capability dispatch panic"
            );
            let exports = self.exports.take();
            let mut output = results.get().init_extras(exports.len() as u32);
            for (i, export) in exports.into_iter().enumerate() {
                output
                    .reborrow()
                    .get(i as u32)
                    .init_cap()
                    .set_as_capability(export.client.hook);
            }
            Promise::ok(())
        }
    }

    struct PanicRead<R> {
        inner: R,
        panic: oneshot::Receiver<()>,
    }
    impl<R: futures::io::AsyncRead + Unpin> futures::io::AsyncRead for PanicRead<R> {
        fn poll_read(
            mut self: Pin<&mut Self>,
            cx: &mut Context<'_>,
            buffer: &mut [u8],
        ) -> Poll<std::io::Result<usize>> {
            assert!(
                !matches!(Pin::new(&mut self.panic).poll(cx), Poll::Ready(Ok(()))),
                "injected receive task panic"
            );
            Pin::new(&mut self.inner).poll_read(cx, buffer)
        }
    }

    struct TerminalPair {
        server: ManagedRpc,
        _peer: ManagedRpc,
        retained: system_capnp::process::Client,
        remote_bootstrap: system_capnp::membrane::Client,
        remote_exports: Vec<system_capnp::process::Client>,
        panic_armed: Rc<std::cell::Cell<bool>>,
        receive_panic: oneshot::Sender<()>,
    }

    async fn terminal_pair(exports: Vec<system_capnp::process::Client>) -> TerminalPair {
        let panic_armed = Rc::new(std::cell::Cell::new(false));
        let bootstrap: system_capnp::membrane::Client = capnp_rpc::new_client(TerminalExports {
            exports: RefCell::new(exports),
            panic_armed: panic_armed.clone(),
        });
        let peer: system_capnp::process::Client = capnp_rpc::new_client(PendingPeer);
        let (a, b) = io::duplex(8192);
        let (ar, aw) = io::split(a);
        let (br, bw) = io::split(b);
        let (receive_panic, receive) = oneshot::channel();
        let mut server = RpcSystem::new(
            Box::new(VatNetwork::new(
                PanicRead {
                    inner: ar.compat(),
                    panic: receive,
                },
                aw.compat_write(),
                Side::Server,
                Default::default(),
            )),
            Some(bootstrap.client),
        );
        let retained = server.bootstrap(Side::Client);
        let mut peer = RpcSystem::new(
            Box::new(VatNetwork::new(
                br.compat(),
                bw.compat_write(),
                Side::Client,
                Default::default(),
            )),
            Some(peer.client),
        );
        let remote_bootstrap: system_capnp::membrane::Client = peer.bootstrap(Side::Server);
        let server = ManagedRpc::spawn(server);
        let peer = ManagedRpc::spawn(peer);
        let response = watchdog(remote_bootstrap.graft_request().send().promise)
            .await
            .unwrap();
        let remote_exports = response
            .get()
            .unwrap()
            .get_extras()
            .unwrap()
            .iter()
            .map(|export| export.get_cap().get_as().unwrap())
            .collect();
        // The bootstrap relinquished these capabilities into the export table.
        drop(response);
        TerminalPair {
            server,
            _peer: peer,
            retained,
            remote_bootstrap,
            remote_exports,
            panic_armed,
            receive_panic,
        }
    }

    async fn watchdog<F: Future>(future: F) -> F::Output {
        tokio::time::timeout(Duration::from_secs(2), future)
            .await
            .expect(
                "connection-owned work must become terminal without repolling an unwound RpcSystem",
            )
    }

    #[derive(Clone, Copy)]
    enum PendingCheck {
        Ordinary,
        Retained,
        Promised,
        Both,
    }

    async fn panic_terminalization(receive_panic: bool, check: PendingCheck) {
        tokio::task::LocalSet::new()
            .run_until(async {
                let (export, kill, _cleanup, _) = process();
                let mut pair = terminal_pair(vec![export]).await;
                let pending = pair.retained.stdout_request().send();
                let promised = matches!(check, PendingCheck::Promised | PendingCheck::Both)
                    .then(|| pending.pipeline.get_stream());
                let pipeline = if matches!(check, PendingCheck::Ordinary) {
                    drop(pending.pipeline);
                    None
                } else {
                    Some(pending.pipeline)
                };
                watchdog(pair.retained.stdin_request().send().promise)
                    .await
                    .unwrap();
                if receive_panic {
                    pair.receive_panic.send(()).unwrap();
                } else {
                    pair.panic_armed.set(true);
                    // Retain the trigger until the failing dispatch has run.
                    let _trigger = pair.remote_bootstrap.graft_request().send();
                    let error = watchdog(&mut pair.server).await.unwrap().unwrap_err();
                    assert!(error.extra.contains("RPC driver panicked"));
                }
                if receive_panic {
                    assert!(watchdog(&mut pair.server).await.unwrap().is_err());
                }
                terminated(kill).await;
                assert!(watchdog(pair.retained.stdin_request().send().promise)
                    .await
                    .is_err());
                let mut response = Box::pin(pending.promise);
                if matches!(check, PendingCheck::Both) {
                    assert!(watchdog(&mut response).await.is_err());
                }
                if let Some(promised) = promised {
                    let _ = watchdog(promised.client.when_resolved()).await;
                    assert!(watchdog(promised.close_request().send().promise)
                        .await
                        .is_err());
                }
                if !matches!(check, PendingCheck::Both) {
                    assert!(watchdog(response).await.is_err());
                }
                if let Some(pipeline) = pipeline {
                    let after = pipeline.get_stream();
                    let _ = watchdog(after.client.when_resolved()).await;
                    assert!(watchdog(after.close_request().send().promise)
                        .await
                        .is_err());
                }
                assert!(
                    watchdog(pair.remote_exports[0].stdout_request().send().promise)
                        .await
                        .is_err()
                );
                pair.server.shutdown();
                pair.server.shutdown();
                drop(pair.server);
            })
            .await;
    }

    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn dispatch_panic_rejects_ordinary_response() {
        panic_terminalization(false, PendingCheck::Ordinary).await;
    }
    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn dispatch_panic_rejects_retained_response_pipeline() {
        panic_terminalization(false, PendingCheck::Retained).await;
    }
    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn dispatch_panic_breaks_promised_capability() {
        panic_terminalization(false, PendingCheck::Promised).await;
    }
    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn dispatch_panic_rejects_parent_response_with_promised_capability() {
        panic_terminalization(false, PendingCheck::Both).await;
    }
    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn receive_task_panic_terminalizes_pending_work() {
        panic_terminalization(true, PendingCheck::Both).await;
    }

    struct DropExport {
        drops: Rc<std::cell::Cell<usize>>,
        panic: bool,
        reenter: Option<Box<dyn Fn()>>,
    }
    impl system_capnp::process::Server for DropExport {}
    impl Drop for DropExport {
        fn drop(&mut self) {
            self.drops.set(self.drops.get() + 1);
            if let Some(reenter) = &self.reenter {
                reenter();
            }
            assert!(
                !self.panic,
                "injected non-bootstrap export destructor panic"
            );
        }
    }

    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn export_destructor_panic_terminalizes_work_and_releases_independent_exports() {
        tokio::task::LocalSet::new()
            .run_until(async {
                let (export, kill, _cleanup, _) = process();
                let drops: Vec<_> = (0..3).map(|_| Rc::new(std::cell::Cell::new(0))).collect();
                let reentry = Rc::new(RefCell::new(None::<Box<dyn Fn()>>));
                let callback = reentry.clone();
                let mut exports: Vec<system_capnp::process::Client> = drops
                    .iter()
                    .enumerate()
                    .map(|(i, drops)| {
                        capnp_rpc::new_client(DropExport {
                            drops: drops.clone(),
                            panic: i == 0,
                            reenter: None,
                        })
                    })
                    .collect();
                let reentrant_drops = Rc::new(std::cell::Cell::new(0));
                exports.push(capnp_rpc::new_client(DropExport {
                    drops: reentrant_drops.clone(),
                    panic: false,
                    reenter: Some(Box::new(move || (callback.borrow().as_ref().unwrap())())),
                }));
                exports.push(export);
                let mut pair = terminal_pair(exports).await;
                let reentrant_export_drops = Rc::new(std::cell::Cell::new(0));
                let new_drops = reentrant_export_drops.clone();
                let retained = pair.retained.clone();
                let rejected = Rc::new(std::cell::Cell::new(false));
                let rejection = rejected.clone();
                *reentry.borrow_mut() = Some(Box::new(move || {
                    let new_export: system_capnp::process::Client =
                        capnp_rpc::new_client(DropExport {
                            drops: new_drops.clone(),
                            panic: false,
                            reenter: None,
                        });
                    let executor = system_capnp::executor::Client {
                        client: retained.client.clone(),
                    };
                    let mut request = executor.spawn_request();
                    request.get().set_membrane(system_capnp::membrane::Client {
                        client: new_export.client,
                    });
                    rejection.set(matches!(
                        request.send().promise.now_or_never(),
                        Some(Err(_))
                    ));
                }));
                let pending = pair.retained.stdout_request().send();
                let promised = pending.pipeline.get_stream();
                watchdog(pair.retained.stdin_request().send().promise)
                    .await
                    .unwrap();
                let result = watchdog(pair.server.shutdown_and_join()).await;
                let error = result
                    .expect("destructor panic must not lose the worker")
                    .unwrap_err();
                assert!(error
                    .extra
                    .contains("injected non-bootstrap export destructor panic"));
                terminated(kill).await;
                assert_eq!(
                    drops.iter().map(|x| x.get()).collect::<Vec<_>>(),
                    vec![1, 1, 1]
                );
                assert_eq!(reentrant_drops.get(), 1);
                assert!(
                    rejected.get(),
                    "reentrant export-bearing call must observe terminal connection"
                );
                assert_eq!(
                    reentrant_export_drops.get(),
                    1,
                    "reentry must not create an owning export"
                );
                assert!(watchdog(pair.retained.stdin_request().send().promise)
                    .await
                    .is_err());
                let _ = watchdog(promised.client.when_resolved()).await;
                assert!(watchdog(promised.close_request().send().promise)
                    .await
                    .is_err());
                assert!(watchdog(pending.promise).await.is_err());
                assert!(
                    watchdog(pending.pipeline.get_stream().close_request().send().promise)
                        .await
                        .is_err()
                );
                for remote in &pair.remote_exports {
                    assert!(watchdog(remote.stdout_request().send().promise)
                        .await
                        .is_err());
                }
                pair.server.shutdown();
                pair.server.shutdown();
                drop(pair.server);
                assert_eq!(
                    drops.iter().map(|x| x.get()).collect::<Vec<_>>(),
                    vec![1, 1, 1]
                );
                assert_eq!(reentrant_drops.get(), 1);
                reentry.borrow_mut().take();
            })
            .await;
    }
}
