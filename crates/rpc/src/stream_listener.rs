//! StreamListener capability: guest-exported subprotocols via process-per-connection.
//!
//! The `StreamListener` capability lets a guest register a libp2p subprotocol cell.
//! For each incoming stream on that subprotocol, the host spawns a fresh WASI
//! process (via the guest-provided `Executor`) with stdin/stdout wired to the
//! stream — the cell speaks whatever wire protocol it wants over stdio.

use authority::EpochGuard;
use capnp::capability::Promise;
use capnp_rpc::pry;
use futures::future::LocalBoxFuture;
use futures::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use futures::{FutureExt, StreamExt};
use std::future::pending;
use std::time::Duration;
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

use crate::{inbound_connection_budget, ConnectionBudget, ConnectionPermit};
use authority::system_capnp;

const ACCEPTED_STREAM_COMPLETION_GRACE: Duration = Duration::from_secs(30);
const ACCEPTED_STREAM_KILL_GRACE: Duration = Duration::from_millis(100);

pub struct StreamListenerImpl {
    stream_control: libp2p_stream::Control,
    guard: EpochGuard,
    budget: ConnectionBudget,
    completion_grace: Duration,
}

impl StreamListenerImpl {
    pub fn new(stream_control: libp2p_stream::Control, guard: EpochGuard) -> Self {
        Self {
            stream_control,
            guard,
            budget: inbound_connection_budget(),
            completion_grace: ACCEPTED_STREAM_COMPLETION_GRACE,
        }
    }

    pub fn with_budget(mut self, budget: ConnectionBudget) -> Self {
        self.budget = budget;
        self
    }
}

#[allow(refining_impl_trait)]
impl system_capnp::stream_listener::Server for StreamListenerImpl {
    fn listen(
        self: capnp::capability::Rc<Self>,
        params: system_capnp::stream_listener::ListenParams,
        _results: system_capnp::stream_listener::ListenResults,
    ) -> Promise<(), capnp::Error> {
        pry!(self.guard.check());

        let params = pry!(params.get());
        let executor: system_capnp::executor::Client = pry!(params.get_executor());
        let protocol_str = pry!(pry!(params.get_protocol())
            .to_str()
            .map_err(|e| capnp::Error::failed(e.to_string())));

        let protocol_suffix = protocol_str.to_string();
        let stream_protocol = pry!(super::stream_protocol(&protocol_suffix));

        if !params.has_membrane() {
            return Promise::err(capnp::Error::failed(
                "stream listener: membrane is required".into(),
            ));
        }
        let membrane = pry!(params.get_membrane());

        let mut control = self.stream_control.clone();
        let mut incoming = pry!(control
            .accept(stream_protocol.clone())
            .map_err(|e| capnp::Error::failed(format!("failed to register protocol cell: {e}"))));

        tracing::info!(protocol = %stream_protocol, "Registered stream subprotocol cell");

        // Accept loop: for each incoming connection, spawn a cell process.
        // Watches the epoch guard so we stop accepting when capabilities are revoked.
        let mut epoch_rx = self.guard.receiver.clone();
        let issued_seq = self.guard.issued_seq;
        let guard = self.guard.clone();
        let budget = self.budget.clone();
        let completion_grace = self.completion_grace;
        tokio::task::spawn_local(async move {
            let cancellation = CancellationToken::new();
            let _cancel_on_drop = cancellation.clone().drop_guard();
            let mut supervisors = futures::stream::FuturesUnordered::new();
            let mut epoch_watch_open = true;
            loop {
                tokio::select! {
                    conn = incoming.next() => {
                        let Some((peer_id, stream)) = conn else {
                            tracing::warn!(protocol = %stream_protocol, "Stream subprotocol accept loop ended unexpectedly");
                            break;
                        };
                        let _accept_span = tracing::info_span!(
                            "stream.accept",
                            peer = %peer_id,
                            protocol = %stream_protocol,
                        ).entered();
                        tracing::debug!("Incoming stream connection");
                        if let Err(error) = guard.check() {
                            tracing::debug!(%error, "rejecting stream connection from stale registration");
                            drop(stream);
                            continue;
                        }
                        let permit = match budget.try_acquire() {
                            Ok(permit) => permit,
                            Err(error) => {
                                tracing::warn!(
                                    capacity = error.capacity,
                                    active = budget.active(),
                                    "rejecting stream connection: service connection budget exhausted"
                                );
                                drop(stream);
                                continue;
                            }
                        };
                        let executor = executor.clone();
                        let protocol = protocol_suffix.clone();
                        let membrane = membrane.clone();
                        let connection_guard = guard.clone();
                        let connection_cancellation = cancellation.child_token();
                        supervisors.push(tokio::task::spawn_local(async move {
                            let _handle_span = tracing::info_span!(
                                "stream.handle",
                                protocol = protocol.as_str(),
                            ).entered();
                            let supervisor = AcceptedConnectionSupervisor {
                                executor,
                                membrane,
                                stream,
                                protocol,
                                guard: connection_guard,
                                completion_grace,
                                cancellation: connection_cancellation,
                                permit,
                            };
                            if let Err(e) = supervisor.run().await {
                                tracing::error!("Stream cell connection error: {e}");
                            }
                        }));
                    }
                    completed = supervisors.next(), if !supervisors.is_empty() => {
                        if let Some(Err(error)) = completed {
                            tracing::error!(%error, "accepted stream supervisor task failed");
                        }
                    }
                    changed = epoch_rx.changed(), if epoch_watch_open => {
                        if epoch_rx.borrow().seq != issued_seq {
                            tracing::warn!(
                                protocol = %stream_protocol,
                                "Epoch became stale, closing stream accept loop"
                            );
                            break;
                        }
                        if changed.is_err() {
                            epoch_watch_open = false;
                        }
                    }
                }
            }

            cancellation.cancel();
            while let Some(result) = supervisors.next().await {
                if let Err(error) = result {
                    tracing::error!(%error, "accepted stream supervisor cleanup task failed");
                }
            }
        });

        Promise::ok(())
    }
}

/// Owns gateway connection state and one admission permit. Its Process reference
/// shares execution ownership; backend lifecycle cleanup remains independent.
struct AcceptedConnectionSupervisor<S> {
    executor: system_capnp::executor::Client,
    membrane: system_capnp::membrane::Client,
    stream: S,
    protocol: String,
    guard: EpochGuard,
    completion_grace: Duration,
    cancellation: CancellationToken,
    permit: ConnectionPermit,
}

impl<S> AcceptedConnectionSupervisor<S>
where
    S: AsyncRead + AsyncWrite + 'static,
{
    async fn run(self) -> Result<(), capnp::Error> {
        let Self {
            executor,
            membrane,
            stream,
            protocol,
            guard,
            completion_grace,
            cancellation,
            permit,
        } = self;
        let result = supervise_connection(
            executor,
            membrane,
            stream,
            &protocol,
            guard,
            completion_grace,
            cancellation,
        )
        .await;
        drop(permit);
        result
    }
}

#[cfg(test)]
#[allow(clippy::too_many_arguments)]
async fn handle_connection<S>(
    executor: system_capnp::executor::Client,
    membrane: system_capnp::membrane::Client,
    stream: S,
    protocol: &str,
    guard: EpochGuard,
    completion_grace: Duration,
    cancellation: CancellationToken,
    permit: ConnectionPermit,
) -> Result<(), capnp::Error>
where
    S: AsyncRead + AsyncWrite + 'static,
{
    AcceptedConnectionSupervisor {
        executor,
        membrane,
        stream,
        protocol: protocol.to_string(),
        guard,
        completion_grace,
        cancellation,
        permit,
    }
    .run()
    .await
}

async fn spawn_connection_child(
    executor: &system_capnp::executor::Client,
    membrane: system_capnp::membrane::Client,
    guard: &EpochGuard,
) -> Result<system_capnp::process::Client, capnp::Error> {
    // This is the authoritative admission check. There must be no yield between
    // the successful check and dispatching Executor.spawn().
    guard.check()?;
    let mut spawn_req = executor.spawn_request();
    spawn_req.get().set_membrane(membrane);
    let spawn = spawn_req.send().promise;
    let response = spawn.await?;
    response.get()?.get_process()
}

fn observe_process_exit(
    process: &system_capnp::process::Client,
) -> LocalBoxFuture<'static, Result<i32, capnp::Error>> {
    let wait = process.wait_request().send().promise;
    async move {
        let response = wait.await?;
        Ok(response.get()?.get_exit_code())
    }
    .boxed_local()
}

async fn acquire_stdio(
    process: system_capnp::process::Client,
) -> Result<
    (
        system_capnp::byte_stream::Client,
        system_capnp::byte_stream::Client,
    ),
    capnp::Error,
> {
    let stdin_resp = process.stdin_request().send().promise.await?;
    let stdin = stdin_resp.get()?.get_stream()?;
    let stdout_resp = process.stdout_request().send().promise.await?;
    let stdout = stdout_resp.get()?.get_stream()?;
    Ok((stdin, stdout))
}

async fn next_optional<T>(future: &mut Option<LocalBoxFuture<'static, T>>) -> T {
    match future {
        Some(future) => future.await,
        None => pending().await,
    }
}

async fn wait_for_deadline(deadline: Option<Instant>) {
    match deadline {
        Some(deadline) => tokio::time::sleep_until(deadline).await,
        None => pending().await,
    }
}

fn arm_deadline(deadline: &mut Option<Instant>, completion_grace: Duration) {
    if deadline.is_none() {
        *deadline = Some(Instant::now() + completion_grace);
    }
}

async fn best_effort_kill(process: &system_capnp::process::Client, protocol: &str) {
    // A request acknowledgement is not backend cleanup. Never keep a gateway
    // connection slot indefinitely for an unreachable or unresponsive executor.
    match tokio::time::timeout(
        ACCEPTED_STREAM_KILL_GRACE,
        process.kill_request().send().promise,
    )
    .await
    {
        Ok(Ok(_)) => {}
        Ok(Err(error)) => {
            tracing::warn!(%error, protocol, "process.kill failed during accepted stream teardown");
        }
        Err(_) => {
            tracing::warn!(
                protocol,
                "process.kill timed out during accepted stream teardown"
            );
        }
    }
}

async fn supervise_connection<S>(
    executor: system_capnp::executor::Client,
    membrane: system_capnp::membrane::Client,
    stream: S,
    protocol: &str,
    guard: EpochGuard,
    completion_grace: Duration,
    cancellation: CancellationToken,
) -> Result<(), capnp::Error>
where
    S: AsyncRead + AsyncWrite + 'static,
{
    let mut spawn = spawn_connection_child(&executor, membrane, &guard).boxed_local();
    let process = tokio::select! {
        biased;
        result = &mut spawn => result?,
        _ = cancellation.cancelled() => {
            // Dropping a dispatched spawn promise cancels the launcher handoff.
            // The launcher retains ownership until it returns the Process, so
            // no child can detach before this supervisor acquires ownership.
            return Err(capnp::Error::failed(
                "accepted stream supervisor cancelled during spawn".into(),
            ));
        }
    };
    drop(spawn);

    // Observe completion while the connection is live. This replayable wait is
    // ordinary gateway work and is cancelled along with stdio on failure.
    let mut wait = Some(observe_process_exit(&process));
    let mut child_result: Option<Result<i32, capnp::Error>> = None;
    let mut stream = Some(stream);
    let mut setup = Some(acquire_stdio(process.clone()).boxed_local());
    let mut deadline = None;

    let stdio = loop {
        tokio::select! {
            biased;
            _ = cancellation.cancelled() => {
                drop(setup.take());
                drop(stream.take());
                drop(wait.take());
                if !matches!(child_result, Some(Ok(_))) {
                    best_effort_kill(&process, protocol).await;
                }
                return Err(capnp::Error::failed("accepted stream supervisor cancelled".into()));
            }
            _ = wait_for_deadline(deadline) => {
                drop(setup.take());
                drop(stream.take());
                drop(wait.take());
                if !matches!(child_result, Some(Ok(_))) {
                    best_effort_kill(&process, protocol).await;
                }
                return Err(capnp::Error::failed("accepted stream completion deadline expired during setup".into()));
            }
            result = next_optional(&mut wait) => {
                wait = None;
                match result {
                    Ok(exit_code) => {
                        tracing::debug!(exit_code, protocol, "Cell process exited during stream setup");
                        child_result = Some(Ok(exit_code));
                        arm_deadline(&mut deadline, completion_grace);
                    }
                    Err(error) => {
                        tracing::warn!(%error, protocol, "process.wait failed during stream setup");
                        drop(setup.take());
                        drop(stream.take());
                        best_effort_kill(&process, protocol).await;
                        return Err(error);
                    }
                }
            }
            result = next_optional(&mut setup) => {
                drop(setup.take());
                match result {
                    Ok(stdio) => break stdio,
                    Err(error) => {
                        drop(stream.take());
                        drop(wait.take());
                        if !matches!(child_result, Some(Ok(_))) {
                            best_effort_kill(&process, protocol).await;
                        }
                        return Err(error);
                    }
                }
            }
        }
    };

    let (stdin, stdout) = stdio;
    let (reader, writer) = Box::pin(stream.take().expect("connection stream owned")).split();
    let mut inbound = child_result
        .is_none()
        .then(|| pump_stream_to_stdin(reader, stdin).boxed_local());
    let mut stdin_close: Option<LocalBoxFuture<'static, Result<(), capnp::Error>>> = None;
    let mut outbound = Some(pump_stdout_to_stream(stdout, writer).boxed_local());
    let mut output_complete = false;
    let mut teardown_error = None;

    while child_result.is_none() || !output_complete {
        tokio::select! {
            biased;
            _ = cancellation.cancelled() => {
                teardown_error = Some(capnp::Error::failed("accepted stream supervisor cancelled".into()));
                break;
            }
            _ = wait_for_deadline(deadline) => {
                teardown_error = Some(capnp::Error::failed("accepted stream completion deadline expired".into()));
                break;
            }
            result = next_optional(&mut wait) => {
                wait = None;
                match result {
                    Ok(exit_code) => {
                        tracing::debug!(exit_code, protocol, "Cell process exited");
                        child_result = Some(Ok(exit_code));
                        inbound = None;
                        if !output_complete {
                            arm_deadline(&mut deadline, completion_grace);
                        }
                    }
                    Err(error) => {
                        tracing::warn!(%error, protocol, "process.wait failed");
                        child_result = Some(Err(error));
                        break;
                    }
                }
            }
            outcome = next_optional(&mut inbound) => {
                inbound = None;
                match outcome {
                    InputPumpOutcome::CleanEof(stdin) => {
                        arm_deadline(&mut deadline, completion_grace);
                        let close = stdin.close_request().send().promise;
                        stdin_close = Some(async move {
                            close.await.map(|_| ())
                        }.boxed_local());
                    }
                    failure => {
                        teardown_error = Some(failure.into_error());
                        break;
                    }
                }
            }
            result = next_optional(&mut stdin_close) => {
                stdin_close = None;
                if let Err(error) = result {
                    teardown_error = Some(capnp::Error::failed(format!(
                        "accepted stream child stdin close failed: {error}"
                    )));
                    break;
                }
            }
            outcome = next_optional(&mut outbound) => {
                outbound = None;
                match outcome {
                    OutputPumpOutcome::CleanEof => output_complete = true,
                    failure => {
                        teardown_error = Some(failure.into_error());
                        break;
                    }
                }
            }
        }
    }

    // Dropping these futures stops normal transport activity and releases both
    // connection-owned halves before the permit owner returns.
    drop(inbound.take());
    drop(stdin_close.take());
    drop(outbound.take());
    drop(wait.take());

    if (teardown_error.is_some() || matches!(child_result, Some(Err(_))))
        && !matches!(child_result, Some(Ok(_)))
    {
        best_effort_kill(&process, protocol).await;
    }
    // Returning drops Process and all remaining connection-owned RPC values.
    // Neither this path nor task cancellation waits for backend teardown.

    if let Some(error) = teardown_error {
        return Err(error);
    }

    if let Some(Err(error)) = child_result {
        return Err(error);
    }

    Ok(())
}

pub(crate) enum InputPumpOutcome {
    CleanEof(system_capnp::byte_stream::Client),
    NetworkRead(std::io::Error),
    StdinWrite(capnp::Error),
}

impl InputPumpOutcome {
    fn into_error(self) -> capnp::Error {
        match self {
            Self::CleanEof(_) => unreachable!("clean EOF is not a failure"),
            Self::NetworkRead(error) => {
                capnp::Error::failed(format!("accepted stream input read failed: {error}"))
            }
            Self::StdinWrite(error) => {
                capnp::Error::failed(format!("accepted stream child stdin write failed: {error}"))
            }
        }
    }
}

#[derive(Debug)]
pub(crate) enum OutputPumpOutcome {
    CleanEof,
    StdoutRead(capnp::Error),
    NetworkWrite(std::io::Error),
    NetworkFlush(std::io::Error),
    NetworkClose(std::io::Error),
}

impl OutputPumpOutcome {
    fn into_error(self) -> capnp::Error {
        match self {
            Self::CleanEof => unreachable!("clean EOF is not a failure"),
            Self::StdoutRead(error) => {
                capnp::Error::failed(format!("accepted stream child stdout read failed: {error}"))
            }
            Self::NetworkWrite(error) => {
                capnp::Error::failed(format!("accepted stream output write failed: {error}"))
            }
            Self::NetworkFlush(error) => {
                capnp::Error::failed(format!("accepted stream output flush failed: {error}"))
            }
            Self::NetworkClose(error) => {
                capnp::Error::failed(format!("accepted stream output close failed: {error}"))
            }
        }
    }
}

/// Read from the libp2p stream and write to the cell's stdin.
pub(crate) async fn pump_stream_to_stdin(
    mut reader: impl futures::io::AsyncRead + Unpin,
    stdin: system_capnp::byte_stream::Client,
) -> InputPumpOutcome {
    let _span = tracing::info_span!("stream.pump_in").entered();
    let mut buf = vec![0u8; 64 * 1024];
    loop {
        match reader.read(&mut buf).await {
            Ok(0) => {
                return InputPumpOutcome::CleanEof(stdin);
            }
            Ok(n) => {
                tracing::trace!(bytes = n, "pump_in: read chunk");
                let mut req = stdin.write_request();
                req.get().set_data(&buf[..n]);
                if let Err(e) = req.send().promise.await {
                    tracing::debug!("stdin write failed: {e}");
                    return InputPumpOutcome::StdinWrite(e);
                }
            }
            Err(e) => {
                tracing::debug!("stream read error: {e}");
                return InputPumpOutcome::NetworkRead(e);
            }
        }
    }
}

/// Read from the cell's stdout and write to the libp2p stream.
pub(crate) async fn pump_stdout_to_stream(
    stdout: system_capnp::byte_stream::Client,
    mut writer: impl futures::io::AsyncWrite + Unpin,
) -> OutputPumpOutcome {
    let _span = tracing::info_span!("stream.pump_out").entered();
    loop {
        let mut req = stdout.read_request();
        req.get().set_max_bytes(64 * 1024);
        let result: Result<Vec<u8>, capnp::Error> = req.send().promise.await.and_then(|response| {
            let data = response.get()?.get_data()?.to_vec();
            Ok(data)
        });
        match result {
            Ok(data) if data.is_empty() => {
                if let Err(error) = writer.flush().await {
                    return OutputPumpOutcome::NetworkFlush(error);
                }
                return match writer.close().await {
                    Ok(()) => OutputPumpOutcome::CleanEof,
                    Err(error) => OutputPumpOutcome::NetworkClose(error),
                };
            }
            Ok(data) => {
                tracing::trace!(bytes = data.len(), "pump_out: write chunk");
                if let Err(e) = writer.write_all(&data).await {
                    tracing::debug!("stream write error: {e}");
                    return OutputPumpOutcome::NetworkWrite(e);
                }
                if let Err(e) = writer.flush().await {
                    tracing::debug!("stream flush error: {e}");
                    return OutputPumpOutcome::NetworkFlush(e);
                }
            }
            Err(e) => {
                tracing::debug!("stdout read error: {e}");
                return OutputPumpOutcome::StdoutRead(e);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{ByteStreamImpl, ProcessImpl, StreamMode};
    use authority::{GraftBuilder, MembraneServer};
    use std::cell::{Cell, RefCell};
    use std::rc::Rc;
    use std::time::Duration;
    use tokio::io::{self, AsyncReadExt as _, AsyncWriteExt as _};
    use tokio::sync::{oneshot, watch};
    use tokio_util::compat::TokioAsyncReadCompatExt;
    use tokio_util::sync::CancellationToken;

    struct RecordingExecutor {
        observed_membranes: Rc<RefCell<Vec<system_capnp::membrane::Client>>>,
    }

    struct ProcessExecutor {
        processes: RefCell<std::collections::VecDeque<system_capnp::process::Client>>,
    }

    struct GatedExecutor {
        process: RefCell<Option<system_capnp::process::Client>>,
        dispatched: RefCell<Option<oneshot::Sender<()>>>,
        release: Rc<RefCell<Option<oneshot::Receiver<()>>>>,
        spawn_calls: Rc<Cell<u32>>,
    }

    #[allow(refining_impl_trait)]
    impl system_capnp::executor::Server for GatedExecutor {
        fn spawn(
            self: capnp::capability::Rc<Self>,
            _params: system_capnp::executor::SpawnParams,
            mut results: system_capnp::executor::SpawnResults,
        ) -> Promise<(), capnp::Error> {
            self.spawn_calls.set(self.spawn_calls.get() + 1);
            if let Some(dispatched) = self.dispatched.borrow_mut().take() {
                let _ = dispatched.send(());
            }
            let process = self.process.borrow_mut().take().expect("one gated spawn");
            let release = self
                .release
                .borrow_mut()
                .take()
                .expect("one gated spawn request");
            Promise::from_future(async move {
                release.await.expect("release gated spawn response");
                results.get().set_process(process);
                Ok(())
            })
        }
    }

    #[derive(Clone, Copy)]
    enum SetupBehavior {
        FailStdin,
        FailStdout,
        PendingStdin,
    }

    struct SetupFailureProcess {
        stdin: system_capnp::byte_stream::Client,
        behavior: SetupBehavior,
        cleanup: crate::CleanupObserver,
        terminate: crate::TerminationHandle,
        wait_calls: Rc<Cell<u32>>,
        kill_calls: Rc<Cell<u32>>,
        stdin_started: Rc<RefCell<Option<oneshot::Sender<()>>>>,
        dropped: Option<oneshot::Sender<()>>,
    }

    struct WaitFailureProcess {
        stdin: system_capnp::byte_stream::Client,
        stdout: system_capnp::byte_stream::Client,
        wait_release: watch::Receiver<bool>,
        killed: RefCell<Option<oneshot::Sender<()>>>,
        kill_release: watch::Receiver<bool>,
        wait_calls: Rc<Cell<u32>>,
        kill_calls: Rc<Cell<u32>>,
        wait_started: RefCell<Option<oneshot::Sender<()>>>,
        dropped: Option<oneshot::Sender<()>>,
        terminate: crate::TerminationHandle,
    }

    impl Drop for SetupFailureProcess {
        fn drop(&mut self) {
            self.terminate.request();
            if let Some(dropped) = self.dropped.take() {
                let _ = dropped.send(());
            }
        }
    }

    impl Drop for WaitFailureProcess {
        fn drop(&mut self) {
            self.terminate.request();
            if let Some(dropped) = self.dropped.take() {
                let _ = dropped.send(());
            }
        }
    }

    #[allow(refining_impl_trait)]
    impl system_capnp::process::Server for SetupFailureProcess {
        fn stdin(
            self: capnp::capability::Rc<Self>,
            _params: system_capnp::process::StdinParams,
            mut results: system_capnp::process::StdinResults,
        ) -> Promise<(), capnp::Error> {
            if let Some(started) = self.stdin_started.borrow_mut().take() {
                let _ = started.send(());
            }
            match self.behavior {
                SetupBehavior::FailStdin => Promise::err(capnp::Error::failed(
                    "injected stdin acquisition failure".into(),
                )),
                SetupBehavior::PendingStdin => Promise::from_future(pending()),
                SetupBehavior::FailStdout => {
                    results.get().set_stream(self.stdin.clone());
                    Promise::ok(())
                }
            }
        }

        fn stdout(
            self: capnp::capability::Rc<Self>,
            _params: system_capnp::process::StdoutParams,
            _results: system_capnp::process::StdoutResults,
        ) -> Promise<(), capnp::Error> {
            Promise::err(capnp::Error::failed(
                "injected stdout acquisition failure".into(),
            ))
        }

        fn wait(
            self: capnp::capability::Rc<Self>,
            _params: system_capnp::process::WaitParams,
            mut results: system_capnp::process::WaitResults,
        ) -> Promise<(), capnp::Error> {
            self.wait_calls.set(self.wait_calls.get() + 1);
            let cleanup = self.cleanup.clone();
            Promise::from_future(async move {
                let exit_code = cleanup
                    .wait()
                    .await
                    .map_err(|reason| capnp::Error::failed(reason.to_string()))?;
                results.get().set_exit_code(exit_code);
                Ok(())
            })
        }

        fn kill(
            self: capnp::capability::Rc<Self>,
            _params: system_capnp::process::KillParams,
            _results: system_capnp::process::KillResults,
        ) -> Promise<(), capnp::Error> {
            self.kill_calls.set(self.kill_calls.get() + 1);
            self.terminate.request();
            Promise::ok(())
        }
    }

    #[allow(refining_impl_trait)]
    impl system_capnp::process::Server for WaitFailureProcess {
        fn stdin(
            self: capnp::capability::Rc<Self>,
            _params: system_capnp::process::StdinParams,
            mut results: system_capnp::process::StdinResults,
        ) -> Promise<(), capnp::Error> {
            results.get().set_stream(self.stdin.clone());
            Promise::ok(())
        }

        fn stdout(
            self: capnp::capability::Rc<Self>,
            _params: system_capnp::process::StdoutParams,
            mut results: system_capnp::process::StdoutResults,
        ) -> Promise<(), capnp::Error> {
            results.get().set_stream(self.stdout.clone());
            Promise::ok(())
        }

        fn wait(
            self: capnp::capability::Rc<Self>,
            _params: system_capnp::process::WaitParams,
            _results: system_capnp::process::WaitResults,
        ) -> Promise<(), capnp::Error> {
            self.wait_calls.set(self.wait_calls.get() + 1);
            if let Some(started) = self.wait_started.borrow_mut().take() {
                let _ = started.send(());
            }
            let mut release = self.wait_release.clone();
            Promise::from_future(async move {
                let _ = release.wait_for(|ready| *ready).await;
                Err(capnp::Error::failed("injected wait failure".into()))
            })
        }

        fn kill(
            self: capnp::capability::Rc<Self>,
            _params: system_capnp::process::KillParams,
            _results: system_capnp::process::KillResults,
        ) -> Promise<(), capnp::Error> {
            self.kill_calls.set(self.kill_calls.get() + 1);
            self.terminate.request();
            if let Some(killed) = self.killed.borrow_mut().take() {
                let _ = killed.send(());
            }
            let mut release = self.kill_release.clone();
            Promise::from_future(async move {
                let _ = release.wait_for(|ready| *ready).await;
                Ok(())
            })
        }
    }

    #[allow(refining_impl_trait)]
    impl system_capnp::executor::Server for ProcessExecutor {
        fn spawn(
            self: capnp::capability::Rc<Self>,
            _params: system_capnp::executor::SpawnParams,
            mut results: system_capnp::executor::SpawnResults,
        ) -> Promise<(), capnp::Error> {
            results.get().set_process(
                self.processes
                    .borrow_mut()
                    .pop_front()
                    .expect("queued process"),
            );
            Promise::ok(())
        }
    }

    fn executor_for_process(
        process: system_capnp::process::Client,
    ) -> system_capnp::executor::Client {
        executor_for_processes([process])
    }

    fn executor_for_processes(
        processes: impl IntoIterator<Item = system_capnp::process::Client>,
    ) -> system_capnp::executor::Client {
        capnp_rpc::new_client(ProcessExecutor {
            processes: RefCell::new(processes.into_iter().collect()),
        })
    }

    struct SetupFailureControl {
        process: system_capnp::process::Client,
        stdin_started: oneshot::Receiver<()>,
        killed: oneshot::Receiver<()>,
        acknowledge_teardown: oneshot::Sender<()>,
        wait_calls: Rc<Cell<u32>>,
        kill_calls: Rc<Cell<u32>>,
        cleanup: crate::CleanupObserver,
        dropped: oneshot::Receiver<()>,
    }

    fn setup_failure_process(behavior: SetupBehavior) -> SetupFailureControl {
        let (stdin_stream, _stdin_peer) = io::duplex(1);
        let stdin = capnp_rpc::new_client(ByteStreamImpl::new(stdin_stream, StreamMode::WriteOnly));
        let (exit_tx, cleanup) = crate::cleanup_channel();
        let (kill_tx, mut kill_rx) = watch::channel(false);
        let (stdin_started_tx, stdin_started) = oneshot::channel();
        let (killed_tx, killed) = oneshot::channel();
        let (acknowledge_teardown, teardown_ack) = oneshot::channel();
        let wait_calls = Rc::new(Cell::new(0));
        let kill_calls = Rc::new(Cell::new(0));
        let (dropped_tx, dropped) = oneshot::channel();

        tokio::task::spawn_local(async move {
            kill_rx
                .changed()
                .await
                .expect("setup failure requested kill");
            let _ = killed_tx.send(());
            teardown_ack.await.expect("allow setup failure teardown");
            exit_tx.cleaned(137);
        });

        SetupFailureControl {
            process: capnp_rpc::new_client(SetupFailureProcess {
                stdin,
                behavior,
                cleanup: cleanup.clone(),
                terminate: crate::TerminationHandle::new(kill_tx),
                wait_calls: wait_calls.clone(),
                kill_calls: kill_calls.clone(),
                stdin_started: Rc::new(RefCell::new(Some(stdin_started_tx))),
                dropped: Some(dropped_tx),
            }),
            stdin_started,
            killed,
            acknowledge_teardown,
            wait_calls,
            kill_calls,
            cleanup,
            dropped,
        }
    }

    fn gated_response_process(
        response: &'static [u8],
    ) -> (
        system_capnp::process::Client,
        oneshot::Receiver<Vec<u8>>,
        oneshot::Sender<()>,
    ) {
        let (stdin_stream, mut child_stdin) = io::duplex(64 * 1024);
        let (stdout_stream, mut child_stdout) = io::duplex(64 * 1024);
        let (stderr_stream, _stderr_peer) = io::duplex(1);
        let stdin = capnp_rpc::new_client(ByteStreamImpl::new(stdin_stream, StreamMode::WriteOnly));
        let stdout =
            capnp_rpc::new_client(ByteStreamImpl::new(stdout_stream, StreamMode::ReadOnly));
        let stderr =
            capnp_rpc::new_client(ByteStreamImpl::new(stderr_stream, StreamMode::ReadOnly));
        let (exit_tx, exit_rx) = crate::cleanup_channel();
        let (kill_tx, mut kill_rx) = watch::channel(false);
        let (input_tx, input_rx) = oneshot::channel();
        let (release_tx, release_rx) = oneshot::channel();

        tokio::task::spawn_local(async move {
            let mut input = Vec::new();
            child_stdin
                .read_to_end(&mut input)
                .await
                .expect("read child stdin through EOF");
            let _ = input_tx.send(input);

            tokio::select! {
                release = release_rx => {
                    release.expect("release delayed response");
                    child_stdout
                        .write_all(response)
                        .await
                        .expect("write delayed child stdout");
                    child_stdout.shutdown().await.expect("close child stdout");
                    exit_tx.cleaned(0);
                }
                _ = kill_rx.changed() => {
                    exit_tx.cleaned(137);
                }
            }
        });

        (
            capnp_rpc::new_client(ProcessImpl::new(
                stdin,
                stdout,
                stderr,
                exit_rx,
                crate::TerminationHandle::new(kill_tx),
            )),
            input_rx,
            release_tx,
        )
    }

    #[allow(refining_impl_trait)]
    impl system_capnp::executor::Server for RecordingExecutor {
        fn spawn(
            self: capnp::capability::Rc<Self>,
            params: system_capnp::executor::SpawnParams,
            _results: system_capnp::executor::SpawnResults,
        ) -> Promise<(), capnp::Error> {
            let params = pry!(params.get());
            let membrane = pry!(params.get_membrane());
            self.observed_membranes.borrow_mut().push(membrane);
            Promise::err(capnp::Error::failed(
                "recording executor has no process".into(),
            ))
        }
    }

    struct ExtrasBuilder {
        capability: capnp::capability::Client,
        grafts: Rc<Cell<u32>>,
    }

    impl GraftBuilder for ExtrasBuilder {
        fn build(
            &self,
            _guard: &EpochGuard,
            mut builder: system_capnp::membrane::graft_results::Builder<'_>,
        ) -> Result<(), capnp::Error> {
            let graft = self.grafts.get() + 1;
            self.grafts.set(graft);
            builder.set_peer_id(&graft.to_be_bytes());
            let mut extra = builder.reborrow().init_extras(1).get(0);
            extra.set_name("application-extra");
            extra
                .init_cap()
                .set_as_capability(self.capability.clone().hook);
            Ok(())
        }
    }

    fn test_membrane(grafts: Rc<Cell<u32>>) -> system_capnp::membrane::Client {
        let epoch = authority::Epoch {
            seq: 1,
            head: Vec::new(),
            root: None,
        };
        let (_tx, rx) = tokio::sync::watch::channel(epoch);
        let capability: system_capnp::executor::Client = capnp_rpc::new_client(RecordingExecutor {
            observed_membranes: Rc::new(RefCell::new(Vec::new())),
        });
        capnp_rpc::new_client(MembraneServer::new(
            rx,
            ExtrasBuilder {
                capability: capability.client,
                grafts,
            },
        ))
    }

    fn test_guard() -> EpochGuard {
        let epoch = authority::Epoch {
            seq: 1,
            head: Vec::new(),
            root: None,
        };
        let (_tx, rx) = tokio::sync::watch::channel(epoch);
        EpochGuard {
            issued_seq: 1,
            receiver: rx,
        }
    }

    fn test_guard_with_sender() -> (watch::Sender<authority::Epoch>, EpochGuard) {
        let epoch = authority::Epoch {
            seq: 1,
            head: Vec::new(),
            root: None,
        };
        let (sender, receiver) = watch::channel(epoch);
        (
            sender,
            EpochGuard {
                issued_seq: 1,
                receiver,
            },
        )
    }

    fn ignores_eof_until_killed_process() -> (
        system_capnp::process::Client,
        oneshot::Receiver<()>,
        oneshot::Receiver<()>,
        oneshot::Sender<()>,
    ) {
        let (stdin_stream, mut child_stdin) = io::duplex(64 * 1024);
        let (stdout_stream, child_stdout) = io::duplex(64 * 1024);
        let (stderr_stream, _stderr_peer) = io::duplex(1);
        let stdin = capnp_rpc::new_client(ByteStreamImpl::new(stdin_stream, StreamMode::WriteOnly));
        let stdout =
            capnp_rpc::new_client(ByteStreamImpl::new(stdout_stream, StreamMode::ReadOnly));
        let stderr =
            capnp_rpc::new_client(ByteStreamImpl::new(stderr_stream, StreamMode::ReadOnly));
        let (exit_tx, exit_rx) = crate::cleanup_channel();
        let (kill_tx, mut kill_rx) = watch::channel(false);
        let (eof_tx, eof_rx) = oneshot::channel();
        let (killed_tx, killed_rx) = oneshot::channel();
        let (ack_tx, ack_rx) = oneshot::channel();

        tokio::task::spawn_local(async move {
            let mut sink = Vec::new();
            child_stdin
                .read_to_end(&mut sink)
                .await
                .expect("read ignored child input through EOF");
            let _ = eof_tx.send(());
            kill_rx.changed().await.expect("supervisor sends kill");
            assert!(*kill_rx.borrow(), "kill signal value");
            let _ = killed_tx.send(());
            ack_rx.await.expect("allow child teardown acknowledgement");
            drop(child_stdout);
            exit_tx.cleaned(137);
        });

        (
            capnp_rpc::new_client(ProcessImpl::new(
                stdin,
                stdout,
                stderr,
                exit_rx,
                crate::TerminationHandle::new(kill_tx),
            )),
            eof_rx,
            killed_rx,
            ack_tx,
        )
    }

    fn completed_response_process(response: &'static [u8]) -> system_capnp::process::Client {
        let (stdin_stream, child_stdin) = io::duplex(64 * 1024);
        let (stdout_stream, mut child_stdout) = io::duplex(64 * 1024);
        let (stderr_stream, _stderr_peer) = io::duplex(1);
        let stdin = capnp_rpc::new_client(ByteStreamImpl::new(stdin_stream, StreamMode::WriteOnly));
        let stdout =
            capnp_rpc::new_client(ByteStreamImpl::new(stdout_stream, StreamMode::ReadOnly));
        let stderr =
            capnp_rpc::new_client(ByteStreamImpl::new(stderr_stream, StreamMode::ReadOnly));
        let (exit_tx, exit_rx) = crate::cleanup_channel();
        let (kill_tx, _kill_rx) = watch::channel(false);

        tokio::task::spawn_local(async move {
            let _child_stdin = child_stdin;
            child_stdout
                .write_all(response)
                .await
                .expect("write completed child response");
            child_stdout
                .shutdown()
                .await
                .expect("close completed child stdout");
            exit_tx.cleaned(0);
        });

        capnp_rpc::new_client(ProcessImpl::new(
            stdin,
            stdout,
            stderr,
            exit_rx,
            crate::TerminationHandle::new(kill_tx),
        ))
    }

    struct OutputChildControl {
        process: system_capnp::process::Client,
        eof: oneshot::Receiver<()>,
        killed: oneshot::Receiver<()>,
        acknowledge_teardown: oneshot::Sender<()>,
        cleanup: crate::CleanupObserver,
    }

    fn output_after_eof_until_killed_process() -> OutputChildControl {
        let (stdin_stream, mut child_stdin) = io::duplex(64 * 1024);
        let (stdout_stream, mut child_stdout) = io::duplex(1);
        let (stderr_stream, _stderr_peer) = io::duplex(1);
        let stdin = capnp_rpc::new_client(ByteStreamImpl::new(stdin_stream, StreamMode::WriteOnly));
        let stdout =
            capnp_rpc::new_client(ByteStreamImpl::new(stdout_stream, StreamMode::ReadOnly));
        let stderr =
            capnp_rpc::new_client(ByteStreamImpl::new(stderr_stream, StreamMode::ReadOnly));
        let (exit_tx, exit_rx) = crate::cleanup_channel();
        let (kill_tx, mut kill_rx) = watch::channel(false);
        let (eof_tx, eof_rx) = oneshot::channel();
        let (killed_tx, killed_rx) = oneshot::channel();
        let (acknowledge_teardown, teardown_ack) = oneshot::channel();

        tokio::task::spawn_local(async move {
            let mut input = Vec::new();
            child_stdin
                .read_to_end(&mut input)
                .await
                .expect("read backpressured child input through EOF");
            let _ = eof_tx.send(());
            let payload = vec![b'x'; 256 * 1024];
            let exit_code = tokio::select! {
                result = child_stdout.write_all(&payload) => {
                    if result.is_ok() {
                        0
                    } else {
                        // This child deliberately ignores pipe failure until
                        // termination, keeping backend cleanup independently gated.
                        kill_rx.changed().await.expect("termination after pipe failure");
                        let _ = killed_tx.send(());
                        137
                    }
                }
                _ = kill_rx.changed() => {
                    let _ = killed_tx.send(());
                    137
                }
            };
            teardown_ack.await.expect("allow backend cleanup");
            exit_tx.cleaned(exit_code);
        });

        OutputChildControl {
            process: capnp_rpc::new_client(ProcessImpl::new(
                stdin,
                stdout,
                stderr,
                exit_rx.clone(),
                crate::TerminationHandle::new(kill_tx),
            )),
            eof: eof_rx,
            killed: killed_rx,
            acknowledge_teardown,
            cleanup: exit_rx,
        }
    }

    async fn assert_setup_failure_returns_gateway_permit(
        behavior: SetupBehavior,
        cancel_during_setup: bool,
    ) {
        let SetupFailureControl {
            process,
            stdin_started,
            killed,
            acknowledge_teardown,
            wait_calls,
            kill_calls,
            cleanup,
            dropped,
        } = setup_failure_process(behavior);
        let budget = ConnectionBudget::new(1).expect("one connection slot");
        let permit = budget.try_acquire().expect("acquire connection slot");
        let (network, _peer) = io::duplex(1);
        let cancellation = CancellationToken::new();
        let supervisor = tokio::task::spawn_local(handle_connection(
            executor_for_process(process),
            test_membrane(Rc::new(Cell::new(0))),
            network.compat(),
            "setup-failure",
            test_guard(),
            Duration::from_secs(30),
            cancellation.clone(),
            permit,
        ));

        stdin_started.await.expect("stdin setup started");
        if cancel_during_setup {
            cancellation.cancel();
        }
        killed.await.expect("post-spawn failure requested kill");
        assert_eq!(kill_calls.get(), 1, "one kill request");
        gateway_failure(supervisor, &budget).await;
        dropped.await.expect("gateway dropped every Process owner");
        assert_eq!(cleanup.state(), crate::CleanupState::Running);

        acknowledge_teardown
            .send(())
            .expect("acknowledge setup failure teardown");
        assert_eq!(
            cleanup.wait().await.expect("independent backend cleanup"),
            137
        );
        assert_eq!(wait_calls.get(), 1, "gateway observes completion once");
        assert_eq!(budget.active(), 0);
    }

    async fn gateway_failure(
        supervisor: tokio::task::JoinHandle<Result<(), capnp::Error>>,
        budget: &ConnectionBudget,
    ) -> capnp::Error {
        let error = tokio::time::timeout(Duration::from_secs(1), supervisor)
            .await
            .expect("gateway must finish while backend cleanup or kill response is gated")
            .expect("supervisor task")
            .expect_err("gateway failure");
        assert_eq!(budget.active(), 0, "gateway completion returns its permit");
        error
    }

    #[tokio::test(start_paused = true)]
    async fn task_abort_drops_connection_and_process_ownership() {
        tokio::task::LocalSet::new()
            .run_until(async {
                let (process, eof, killed, acknowledge_teardown) =
                    ignores_eof_until_killed_process();
                let budget = ConnectionBudget::new(1).expect("one slot");
                let (network, mut peer) = io::duplex(1);
                let supervisor = tokio::task::spawn_local(handle_connection(
                    executor_for_process(process),
                    test_membrane(Rc::new(Cell::new(0))),
                    network.compat(),
                    "task-abort",
                    test_guard(),
                    Duration::from_secs(30),
                    CancellationToken::new(),
                    budget.try_acquire().expect("permit"),
                ));
                peer.shutdown().await.expect("half-close input");
                eof.await.expect("child observed EOF");
                supervisor.abort();
                assert!(supervisor.await.expect_err("task aborted").is_cancelled());
                assert_eq!(budget.active(), 0, "aborted gateway releases permit");
                killed
                    .await
                    .expect("final Process owner loss requests termination");
                assert_eq!(
                    peer.read(&mut [0; 1]).await.expect("gateway stream closed"),
                    0
                );
                acknowledge_teardown
                    .send(())
                    .expect("allow backend cleanup");
            })
            .await;
    }

    #[tokio::test(start_paused = true)]
    async fn task_abort_during_setup_drops_pending_rpc_and_process_ownership() {
        tokio::task::LocalSet::new()
            .run_until(async {
                let SetupFailureControl {
                    process,
                    stdin_started,
                    killed,
                    acknowledge_teardown,
                    cleanup,
                    dropped,
                    ..
                } = setup_failure_process(SetupBehavior::PendingStdin);
                let budget = ConnectionBudget::new(1).expect("one slot");
                let (network, mut peer) = io::duplex(1);
                let supervisor = tokio::task::spawn_local(handle_connection(
                    executor_for_process(process),
                    test_membrane(Rc::new(Cell::new(0))),
                    network.compat(),
                    "setup-task-abort",
                    test_guard(),
                    Duration::from_secs(30),
                    CancellationToken::new(),
                    budget.try_acquire().expect("permit"),
                ));
                stdin_started.await.expect("setup request dispatched");
                supervisor.abort();
                assert!(supervisor.await.expect_err("task aborted").is_cancelled());
                assert_eq!(budget.active(), 0);
                dropped.await.expect("pending RPC does not retain Process");
                killed.await.expect("final-owner termination");
                assert_eq!(
                    peer.read(&mut [0; 1]).await.expect("gateway stream closed"),
                    0
                );
                assert_eq!(cleanup.state(), crate::CleanupState::Running);
                acknowledge_teardown
                    .send(())
                    .expect("allow backend cleanup");
                assert_eq!(cleanup.wait().await.expect("independent cleanup"), 137);
            })
            .await;
    }

    #[tokio::test(start_paused = true)]
    async fn spawn_rpc_failure_closes_stream_and_returns_permit() {
        tokio::task::LocalSet::new()
            .run_until(async {
                let budget = ConnectionBudget::new(1).expect("one slot");
                let (network, mut peer) = io::duplex(1);
                let executor = capnp_rpc::new_client(RecordingExecutor {
                    observed_membranes: Rc::new(RefCell::new(Vec::new())),
                });
                let result = handle_connection(
                    executor,
                    test_membrane(Rc::new(Cell::new(0))),
                    network.compat(),
                    "spawn-failure",
                    test_guard(),
                    Duration::from_secs(30),
                    CancellationToken::new(),
                    budget.try_acquire().expect("permit"),
                )
                .await;
                assert!(result
                    .expect_err("spawn fails")
                    .to_string()
                    .contains("recording executor"));
                assert_eq!(budget.active(), 0);
                assert_eq!(
                    peer.read(&mut [0; 1]).await.expect("gateway stream closed"),
                    0
                );
            })
            .await;
    }

    #[tokio::test(start_paused = true)]
    async fn child_completion_does_not_restart_input_eof_deadline() {
        tokio::task::LocalSet::new()
            .run_until(async {
                let (process, input, release) = gated_response_process(&[b'x'; 4096]);
                let wait = observe_process_exit(&process);
                let budget = ConnectionBudget::new(1).expect("one slot");
                let (network, mut peer) = io::duplex(1);
                let grace = Duration::from_secs(10);
                let supervisor = tokio::task::spawn_local(handle_connection(
                    executor_for_process(process),
                    test_membrane(Rc::new(Cell::new(0))),
                    network.compat(),
                    "deadline-phase-transition",
                    test_guard(),
                    grace,
                    CancellationToken::new(),
                    budget.try_acquire().expect("permit"),
                ));
                peer.shutdown().await.expect("half-close input");
                input.await.expect("child observed EOF");
                tokio::time::advance(grace / 2).await;
                release.send(()).expect("release response");
                assert_eq!(wait.await.expect("child completed"), 0);
                assert_eq!(budget.active(), 1, "blocked output retains gateway permit");
                tokio::time::advance(grace / 2).await;
                let error = gateway_failure(supervisor, &budget).await;
                assert!(error.to_string().contains("deadline expired"));
            })
            .await;
    }

    #[tokio::test]
    async fn listener_registration_does_not_graft_the_supplied_membrane() {
        let local = tokio::task::LocalSet::new();
        local
            .run_until(async {
                let grafts = Rc::new(Cell::new(0));
                let membrane = test_membrane(grafts.clone());
                let observed_membranes = Rc::new(RefCell::new(Vec::new()));
                let executor: system_capnp::executor::Client =
                    capnp_rpc::new_client(RecordingExecutor { observed_membranes });
                let listener: system_capnp::stream_listener::Client =
                    capnp_rpc::new_client(StreamListenerImpl::new(
                        libp2p_stream::Behaviour::new().new_control(),
                        test_guard(),
                    ));
                let mut request = listener.listen_request();
                request.get().set_executor(executor);
                request.get().set_protocol("stateful-forwarding-test");
                request.get().set_membrane(membrane);

                request
                    .send()
                    .promise
                    .await
                    .expect("register stream listener");

                assert_eq!(
                    grafts.get(),
                    0,
                    "stream registration must not inspect the supplied Membrane"
                );
            })
            .await;
    }

    #[tokio::test]
    async fn spawn_connection_child_forwards_stateful_membrane_for_each_connection() {
        let local = tokio::task::LocalSet::new();
        local
            .run_until(async {
                let observed_membranes = Rc::new(RefCell::new(Vec::new()));
                let executor: system_capnp::executor::Client =
                    capnp_rpc::new_client(RecordingExecutor {
                        observed_membranes: observed_membranes.clone(),
                    });
                let grafts = Rc::new(Cell::new(0));
                let membrane = test_membrane(grafts.clone());
                let guard = test_guard();

                for _ in 0..2 {
                    let result = spawn_connection_child(&executor, membrane.clone(), &guard).await;
                    assert!(result.is_err(), "recording executor intentionally rejects");
                }

                let observed = observed_membranes.take();
                assert_eq!(
                    observed.len(),
                    2,
                    "each connection must receive the registration-time Membrane"
                );
                assert_eq!(
                    grafts.get(),
                    0,
                    "stream child spawning must forward without grafting"
                );
                for (index, membrane) in observed.into_iter().enumerate() {
                    let response = membrane
                        .graft_request()
                        .send()
                        .promise
                        .await
                        .expect("delegated Membrane graft");
                    let extras = response
                        .get()
                        .expect("graft results")
                        .get_extras()
                        .expect("extras");
                    assert_eq!(extras.len(), 1);
                    assert_eq!(
                        response
                            .get()
                            .expect("graft results")
                            .get_peer_id()
                            .expect("stateful peerId"),
                        &((index + 1) as u32).to_be_bytes()
                    );
                    assert_eq!(
                        extras
                            .get(0)
                            .get_name()
                            .expect("extra name")
                            .to_str()
                            .expect("UTF-8 extra"),
                        "application-extra"
                    );
                }
                assert_eq!(
                    grafts.get(),
                    2,
                    "each stream child must reach the supplied Membrane server"
                );
            })
            .await;
    }

    #[tokio::test]
    async fn clean_input_eof_preserves_delayed_output_and_permit() {
        tokio::task::LocalSet::new()
            .run_until(async {
                let response = b"response after input EOF";
                let (process, input, release_response) = gated_response_process(response);
                let executor = executor_for_process(process);
                let membrane = test_membrane(Rc::new(Cell::new(0)));
                let budget = ConnectionBudget::new(1).expect("one connection slot");
                let permit = budget.try_acquire().expect("acquire connection slot");
                let (network, mut peer) = io::duplex(64 * 1024);

                let supervisor = tokio::task::spawn_local(handle_connection(
                    executor,
                    membrane,
                    network.compat(),
                    "delayed-output",
                    test_guard(),
                    Duration::from_secs(30),
                    CancellationToken::new(),
                    permit,
                ));

                peer.write_all(b"request").await.expect("write request");
                peer.shutdown().await.expect("half-close peer input");
                assert_eq!(input.await.expect("child observed input EOF"), b"request");
                assert_eq!(budget.active(), 1, "input EOF must retain the permit");

                release_response
                    .send(())
                    .expect("release child response gate");
                let mut received = Vec::new();
                peer.read_to_end(&mut received)
                    .await
                    .expect("read delayed response and final EOF");
                assert_eq!(received, response);

                supervisor
                    .await
                    .expect("connection supervisor task")
                    .expect("connection supervisor result");
                assert_eq!(
                    budget.active(),
                    0,
                    "permit releases after child and transport completion"
                );
            })
            .await;
    }

    #[tokio::test]
    async fn stale_guard_at_final_admission_rejects_spawn_and_returns_permit() {
        tokio::task::LocalSet::new()
            .run_until(async {
                let observed_membranes = Rc::new(RefCell::new(Vec::new()));
                let executor: system_capnp::executor::Client =
                    capnp_rpc::new_client(RecordingExecutor {
                        observed_membranes: observed_membranes.clone(),
                    });
                let membrane = test_membrane(Rc::new(Cell::new(0)));
                let (epoch_tx, guard) = test_guard_with_sender();
                epoch_tx
                    .send(authority::Epoch {
                        seq: 2,
                        head: Vec::new(),
                        root: None,
                    })
                    .expect("advance epoch");
                let budget = ConnectionBudget::new(1).expect("one connection slot");
                let permit = budget.try_acquire().expect("acquire connection slot");
                let (network, _peer) = io::duplex(1);

                let result = handle_connection(
                    executor,
                    membrane,
                    network.compat(),
                    "stale-final-admission",
                    guard,
                    Duration::from_secs(30),
                    CancellationToken::new(),
                    permit,
                )
                .await;

                assert!(result.is_err(), "stale registration must be rejected");
                assert!(
                    observed_membranes.borrow().is_empty(),
                    "stale registration must not dispatch Executor.spawn"
                );
                assert_eq!(budget.active(), 0, "rejected admission returns permit");
            })
            .await;
    }

    #[tokio::test]
    async fn cancellation_while_spawn_is_pending_cancels_handoff_and_returns_permit() {
        tokio::task::LocalSet::new()
            .run_until(async {
                let (dispatched_tx, dispatched_rx) = oneshot::channel();
                let (release_tx, release_rx) = oneshot::channel();
                let spawn_calls = Rc::new(Cell::new(0));
                let executor: system_capnp::executor::Client =
                    capnp_rpc::new_client(GatedExecutor {
                        process: RefCell::new(Some(completed_response_process(b""))),
                        dispatched: RefCell::new(Some(dispatched_tx)),
                        release: Rc::new(RefCell::new(Some(release_rx))),
                        spawn_calls: spawn_calls.clone(),
                    });
                let budget = ConnectionBudget::new(1).expect("one connection slot");
                let permit = budget.try_acquire().expect("acquire connection slot");
                let cancellation = CancellationToken::new();
                let (network, _peer) = io::duplex(1);
                let supervisor = tokio::task::spawn_local(handle_connection(
                    executor,
                    test_membrane(Rc::new(Cell::new(0))),
                    network.compat(),
                    "pending-spawn-cancellation",
                    test_guard(),
                    Duration::from_secs(30),
                    cancellation.clone(),
                    permit,
                ));

                dispatched_rx.await.expect("spawn request dispatched");
                assert_eq!(budget.active(), 1, "pending spawn retains permit");
                cancellation.cancel();

                let result = supervisor.await.expect("supervisor task joined");
                assert!(result.is_err(), "cancellation terminates the supervisor");
                assert_eq!(spawn_calls.get(), 1, "spawn dispatch remains single-shot");
                assert_eq!(budget.active(), 0, "cancelled handoff returns permit");
                assert!(
                    release_tx.send(()).is_err(),
                    "dropping the spawn promise cancels the server-side handoff"
                );
            })
            .await;
    }

    #[tokio::test(start_paused = true)]
    async fn ready_spawn_handoff_wins_simultaneous_cancellation_and_returns_permit() {
        tokio::task::LocalSet::new()
            .run_until(async {
                let SetupFailureControl {
                    process,
                    stdin_started: _stdin_started,
                    killed,
                    acknowledge_teardown,
                    wait_calls,
                    kill_calls,
                    cleanup,
                    dropped,
                } = setup_failure_process(SetupBehavior::PendingStdin);
                let (dispatched_tx, dispatched_rx) = oneshot::channel();
                let (release_tx, release_rx) = oneshot::channel();
                let spawn_calls = Rc::new(Cell::new(0));
                let executor: system_capnp::executor::Client =
                    capnp_rpc::new_client(GatedExecutor {
                        process: RefCell::new(Some(process)),
                        dispatched: RefCell::new(Some(dispatched_tx)),
                        release: Rc::new(RefCell::new(Some(release_rx))),
                        spawn_calls: spawn_calls.clone(),
                    });
                let budget = ConnectionBudget::new(1).expect("one connection slot");
                let permit = budget.try_acquire().expect("acquire connection slot");
                let cancellation = CancellationToken::new();
                let (network, _peer) = io::duplex(1);
                let supervisor = tokio::task::spawn_local(handle_connection(
                    executor,
                    test_membrane(Rc::new(Cell::new(0))),
                    network.compat(),
                    "ready-spawn-cancellation",
                    test_guard(),
                    Duration::from_secs(30),
                    cancellation.clone(),
                    permit,
                ));

                dispatched_rx.await.expect("spawn request dispatched");
                release_tx.send(()).expect("make Process response ready");
                cancellation.cancel();

                killed
                    .await
                    .expect("ready Process handoff entered owned cleanup");
                assert_eq!(spawn_calls.get(), 1);
                assert!(
                    wait_calls.get() <= 1,
                    "cancelled completion observation is disposable"
                );
                assert_eq!(kill_calls.get(), 1, "cancellation requests one kill");
                gateway_failure(supervisor, &budget).await;
                dropped
                    .await
                    .expect("ready handoff releases Process ownership");
                assert_eq!(cleanup.state(), crate::CleanupState::Running);

                acknowledge_teardown
                    .send(())
                    .expect("acknowledge ready-handoff child teardown");
                assert_eq!(cleanup.wait().await.expect("backend cleanup"), 137);
            })
            .await;
    }

    #[tokio::test]
    async fn epoch_advance_after_spawn_dispatch_does_not_revoke_admission() {
        tokio::task::LocalSet::new()
            .run_until(async {
                let process = completed_response_process(b"");
                let (dispatched_tx, dispatched_rx) = oneshot::channel();
                let (release_tx, release_rx) = oneshot::channel();
                let spawn_calls = Rc::new(Cell::new(0));
                let executor: system_capnp::executor::Client =
                    capnp_rpc::new_client(GatedExecutor {
                        process: RefCell::new(Some(process)),
                        dispatched: RefCell::new(Some(dispatched_tx)),
                        release: Rc::new(RefCell::new(Some(release_rx))),
                        spawn_calls: spawn_calls.clone(),
                    });
                let (epoch_tx, guard) = test_guard_with_sender();
                let spawn = tokio::task::spawn_local(async move {
                    spawn_connection_child(&executor, test_membrane(Rc::new(Cell::new(0))), &guard)
                        .await
                });

                dispatched_rx
                    .await
                    .expect("spawn dispatched after final check");
                epoch_tx
                    .send(authority::Epoch {
                        seq: 2,
                        head: Vec::new(),
                        root: None,
                    })
                    .expect("advance epoch after dispatch");
                release_tx.send(()).expect("release spawn response");

                assert!(spawn.await.expect("spawn task").is_ok());
                assert_eq!(spawn_calls.get(), 1);
            })
            .await;
    }

    #[tokio::test(start_paused = true)]
    async fn completion_deadline_returns_permit_before_backend_cleanup() {
        tokio::task::LocalSet::new()
            .run_until(async {
                let (process, eof, killed, acknowledge_teardown) =
                    ignores_eof_until_killed_process();
                let budget = ConnectionBudget::new(1).expect("one connection slot");
                let permit = budget.try_acquire().expect("acquire connection slot");
                let (network, mut peer) = io::duplex(64 * 1024);
                let grace = Duration::from_secs(5);

                let supervisor = tokio::task::spawn_local(handle_connection(
                    executor_for_process(process),
                    test_membrane(Rc::new(Cell::new(0))),
                    network.compat(),
                    "deadline",
                    test_guard(),
                    grace,
                    CancellationToken::new(),
                    permit,
                ));

                peer.shutdown().await.expect("half-close peer input");
                eof.await.expect("child observed EOF");
                assert_eq!(budget.active(), 1);

                tokio::time::advance(grace).await;
                killed.await.expect("deadline requested child kill");
                let error = gateway_failure(supervisor, &budget).await;
                assert!(error.to_string().contains("deadline expired"));

                acknowledge_teardown
                    .send(())
                    .expect("acknowledge child teardown");
                assert_eq!(
                    budget.active(),
                    0,
                    "backend cleanup does not own the permit"
                );
            })
            .await;
    }

    #[tokio::test(start_paused = true)]
    async fn cancellation_returns_permit_before_backend_cleanup() {
        tokio::task::LocalSet::new()
            .run_until(async {
                let (process, eof, killed, acknowledge_teardown) =
                    ignores_eof_until_killed_process();
                let budget = ConnectionBudget::new(1).expect("one connection slot");
                let permit = budget.try_acquire().expect("acquire connection slot");
                let (network, mut peer) = io::duplex(64 * 1024);
                let cancellation = CancellationToken::new();

                let supervisor = tokio::task::spawn_local(handle_connection(
                    executor_for_process(process),
                    test_membrane(Rc::new(Cell::new(0))),
                    network.compat(),
                    "cancelled",
                    test_guard(),
                    Duration::from_secs(30),
                    cancellation.clone(),
                    permit,
                ));

                peer.shutdown().await.expect("half-close peer input");
                eof.await.expect("child observed EOF");
                cancellation.cancel();
                killed.await.expect("cancellation requested child kill");
                let error = gateway_failure(supervisor, &budget).await;
                assert!(error.to_string().contains("cancelled"));

                acknowledge_teardown
                    .send(())
                    .expect("acknowledge child teardown");
                assert_eq!(budget.active(), 0);
            })
            .await;
    }

    #[tokio::test]
    async fn child_completion_stops_input_and_drains_output_before_permit_release() {
        tokio::task::LocalSet::new()
            .run_until(async {
                let response = b"child completed first";
                let process = completed_response_process(response);
                let budget = ConnectionBudget::new(1).expect("one connection slot");
                let permit = budget.try_acquire().expect("acquire connection slot");
                let (network, mut peer) = io::duplex(64 * 1024);

                let supervisor = tokio::task::spawn_local(handle_connection(
                    executor_for_process(process),
                    test_membrane(Rc::new(Cell::new(0))),
                    network.compat(),
                    "child-first",
                    test_guard(),
                    Duration::from_secs(30),
                    CancellationToken::new(),
                    permit,
                ));

                let mut received = Vec::new();
                peer.read_to_end(&mut received)
                    .await
                    .expect("drain completed child output");
                assert_eq!(received, response);
                supervisor
                    .await
                    .expect("supervisor task")
                    .expect("normal child-first completion");
                assert_eq!(budget.active(), 0);
            })
            .await;
    }

    #[tokio::test(start_paused = true)]
    async fn post_spawn_setup_failures_return_permit_before_backend_cleanup() {
        tokio::task::LocalSet::new()
            .run_until(async {
                assert_setup_failure_returns_gateway_permit(SetupBehavior::FailStdin, false).await;
                assert_setup_failure_returns_gateway_permit(SetupBehavior::FailStdout, false).await;
                assert_setup_failure_returns_gateway_permit(SetupBehavior::PendingStdin, true)
                    .await;
            })
            .await;
    }

    #[tokio::test(start_paused = true)]
    async fn executor_rpc_failure_returns_permit_after_bounded_kill() {
        tokio::task::LocalSet::new()
            .run_until(async {
                let (stdin_stream, _stdin_peer) = io::duplex(1);
                let (stdout_stream, _stdout_peer) = io::duplex(1);
                let stdin =
                    capnp_rpc::new_client(ByteStreamImpl::new(stdin_stream, StreamMode::WriteOnly));
                let stdout =
                    capnp_rpc::new_client(ByteStreamImpl::new(stdout_stream, StreamMode::ReadOnly));
                let (killed_tx, killed_rx) = oneshot::channel();
                let (wait_release_tx, wait_release_rx) = watch::channel(false);
                let (kill_release_tx, kill_release_rx) = watch::channel(false);
                let wait_calls = Rc::new(Cell::new(0));
                let kill_calls = Rc::new(Cell::new(0));
                let (dropped_tx, dropped) = oneshot::channel();
                let (wait_started_tx, wait_started) = oneshot::channel();
                let (terminate_tx, termination) = watch::channel(false);
                let process: system_capnp::process::Client =
                    capnp_rpc::new_client(WaitFailureProcess {
                        stdin,
                        stdout,
                        wait_release: wait_release_rx,
                        killed: RefCell::new(Some(killed_tx)),
                        kill_release: kill_release_rx,
                        wait_calls: wait_calls.clone(),
                        kill_calls: kill_calls.clone(),
                        wait_started: RefCell::new(Some(wait_started_tx)),
                        dropped: Some(dropped_tx),
                        terminate: crate::TerminationHandle::new(terminate_tx),
                    });
                let budget = ConnectionBudget::new(1).expect("one connection slot");
                let permit = budget.try_acquire().expect("acquire connection slot");
                let (network, _peer) = io::duplex(1);
                let supervisor = tokio::task::spawn_local(handle_connection(
                    executor_for_process(process),
                    test_membrane(Rc::new(Cell::new(0))),
                    network.compat(),
                    "wait-contract-failure",
                    test_guard(),
                    Duration::from_secs(30),
                    CancellationToken::new(),
                    permit,
                ));

                wait_started.await.expect("wait request dispatched");
                wait_release_tx.send(true).expect("release wait failure");
                killed_rx.await.expect("wait failure requested kill");
                assert_eq!(wait_calls.get(), 1, "one completion observation");
                assert_eq!(kill_calls.get(), 1, "wait failure requests one kill");
                let error = gateway_failure(supervisor, &budget).await;
                assert!(error.to_string().contains("injected wait failure"));
                dropped.await.expect("failure drops every Process owner");
                assert!(
                    *termination.borrow(),
                    "termination requested before ownership release"
                );
                assert!(
                    kill_release_tx.send(true).is_err(),
                    "pending kill was cancelled"
                );
            })
            .await;
    }

    #[tokio::test(start_paused = true)]
    async fn cancellation_drops_pending_wait_and_bounds_pending_kill() {
        tokio::task::LocalSet::new()
            .run_until(async {
                let (stdin_stream, _stdin_peer) = io::duplex(1);
                let (stdout_stream, _stdout_peer) = io::duplex(1);
                let stdin =
                    capnp_rpc::new_client(ByteStreamImpl::new(stdin_stream, StreamMode::WriteOnly));
                let stdout =
                    capnp_rpc::new_client(ByteStreamImpl::new(stdout_stream, StreamMode::ReadOnly));
                let (wait_release_tx, wait_release_rx) = watch::channel(false);
                let (killed_tx, killed_rx) = oneshot::channel();
                let (kill_release_tx, kill_release_rx) = watch::channel(false);
                let wait_calls = Rc::new(Cell::new(0));
                let kill_calls = Rc::new(Cell::new(0));
                let (dropped_tx, dropped) = oneshot::channel();
                let (wait_started_tx, wait_started) = oneshot::channel();
                let (terminate_tx, termination) = watch::channel(false);
                let process: system_capnp::process::Client =
                    capnp_rpc::new_client(WaitFailureProcess {
                        stdin,
                        stdout,
                        wait_release: wait_release_rx,
                        killed: RefCell::new(Some(killed_tx)),
                        kill_release: kill_release_rx,
                        wait_calls: wait_calls.clone(),
                        kill_calls: kill_calls.clone(),
                        wait_started: RefCell::new(Some(wait_started_tx)),
                        dropped: Some(dropped_tx),
                        terminate: crate::TerminationHandle::new(terminate_tx),
                    });
                let budget = ConnectionBudget::new(1).expect("one connection slot");
                let permit = budget.try_acquire().expect("acquire connection slot");
                let cancellation = CancellationToken::new();
                let (network, _peer) = io::duplex(1);
                let supervisor = tokio::task::spawn_local(handle_connection(
                    executor_for_process(process),
                    test_membrane(Rc::new(Cell::new(0))),
                    network.compat(),
                    "forced-cleanup-wait-failure",
                    test_guard(),
                    Duration::from_secs(30),
                    cancellation.clone(),
                    permit,
                ));

                wait_started.await.expect("wait request dispatched");
                cancellation.cancel();
                killed_rx.await.expect("forced cleanup dispatched kill");
                let error = gateway_failure(supervisor, &budget).await;
                assert!(error.to_string().contains("cancelled"));
                assert_eq!(wait_calls.get(), 1, "one completion observation");
                assert_eq!(kill_calls.get(), 1, "forced cleanup sends one kill");
                dropped
                    .await
                    .expect("cancellation drops every Process owner");
                assert!(
                    *termination.borrow(),
                    "termination requested before ownership release"
                );
                assert!(
                    wait_release_tx.send(true).is_err(),
                    "pending wait was cancelled"
                );
                assert!(
                    kill_release_tx.send(true).is_err(),
                    "pending kill was cancelled"
                );
            })
            .await;
    }

    #[tokio::test(start_paused = true)]
    async fn output_backpressure_uses_the_original_completion_deadline() {
        tokio::task::LocalSet::new()
            .run_until(async {
                let OutputChildControl {
                    process,
                    eof,
                    killed,
                    acknowledge_teardown,
                    cleanup,
                } = output_after_eof_until_killed_process();
                let budget = ConnectionBudget::new(1).expect("one connection slot");
                let permit = budget.try_acquire().expect("acquire connection slot");
                let (network, mut peer) = io::duplex(1);
                let grace = Duration::from_secs(5);
                let supervisor = tokio::task::spawn_local(handle_connection(
                    executor_for_process(process),
                    test_membrane(Rc::new(Cell::new(0))),
                    network.compat(),
                    "backpressure",
                    test_guard(),
                    grace,
                    CancellationToken::new(),
                    permit,
                ));

                peer.shutdown().await.expect("half-close peer input");
                eof.await.expect("child observed EOF");
                tokio::time::advance(grace).await;
                killed.await.expect("deadline killed output-blocked child");
                gateway_failure(supervisor, &budget).await;
                assert_eq!(cleanup.state(), crate::CleanupState::Running);
                acknowledge_teardown
                    .send(())
                    .expect("allow backend cleanup");
                assert_eq!(cleanup.wait().await.expect("backend cleanup"), 137);
            })
            .await;
    }

    #[tokio::test(start_paused = true)]
    async fn peer_reset_returns_permit_before_backend_cleanup() {
        tokio::task::LocalSet::new()
            .run_until(async {
                let OutputChildControl {
                    process,
                    eof,
                    killed,
                    acknowledge_teardown,
                    cleanup,
                } = output_after_eof_until_killed_process();
                let budget = ConnectionBudget::new(1).expect("one connection slot");
                let permit = budget.try_acquire().expect("acquire connection slot");
                let (network, mut peer) = io::duplex(1);
                let supervisor = tokio::task::spawn_local(handle_connection(
                    executor_for_process(process),
                    test_membrane(Rc::new(Cell::new(0))),
                    network.compat(),
                    "remote-reset",
                    test_guard(),
                    Duration::from_secs(30),
                    CancellationToken::new(),
                    permit,
                ));

                peer.shutdown().await.expect("half-close peer input");
                eof.await.expect("child observed EOF");
                drop(peer);
                killed.await.expect("peer reset killed pending child");
                gateway_failure(supervisor, &budget).await;
                assert_eq!(cleanup.state(), crate::CleanupState::Running);
                acknowledge_teardown
                    .send(())
                    .expect("allow backend cleanup");
                assert_eq!(cleanup.wait().await.expect("backend cleanup"), 137);
            })
            .await;
    }

    #[tokio::test(start_paused = true)]
    async fn cancelling_one_sibling_does_not_affect_the_other() {
        tokio::task::LocalSet::new()
            .run_until(async {
                let (process_a, eof_a, killed_a, acknowledge_a) =
                    ignores_eof_until_killed_process();
                let response_b = b"sibling B response";
                let (process_b, input_b, release_b) = gated_response_process(response_b);
                let executor = executor_for_processes([process_a, process_b]);
                let budget = ConnectionBudget::new(2).expect("two connection slots");
                let cancel_a = CancellationToken::new();
                let (network_a, mut peer_a) = io::duplex(64 * 1024);
                let (network_b, mut peer_b) = io::duplex(64 * 1024);

                let supervisor_a = tokio::task::spawn_local(handle_connection(
                    executor.clone(),
                    test_membrane(Rc::new(Cell::new(0))),
                    network_a.compat(),
                    "sibling-a",
                    test_guard(),
                    Duration::from_secs(30),
                    cancel_a.clone(),
                    budget.try_acquire().expect("permit A"),
                ));
                let supervisor_b = tokio::task::spawn_local(handle_connection(
                    executor,
                    test_membrane(Rc::new(Cell::new(0))),
                    network_b.compat(),
                    "sibling-b",
                    test_guard(),
                    Duration::from_secs(30),
                    CancellationToken::new(),
                    budget.try_acquire().expect("permit B"),
                ));

                peer_a.shutdown().await.expect("half-close A");
                peer_b.write_all(b"request B").await.expect("write B");
                peer_b.shutdown().await.expect("half-close B");
                eof_a.await.expect("A observed EOF");
                assert_eq!(input_b.await.expect("B observed EOF"), b"request B");

                cancel_a.cancel();
                killed_a.await.expect("A killed");
                assert!(tokio::time::timeout(Duration::from_secs(1), supervisor_a)
                    .await
                    .expect("A completes before backend cleanup")
                    .expect("A supervisor")
                    .is_err());
                assert_eq!(budget.active(), 1, "only B retains its gateway permit");

                release_b.send(()).expect("release B response");
                let mut received_b = Vec::new();
                peer_b.read_to_end(&mut received_b).await.expect("read B");
                assert_eq!(received_b, response_b);
                supervisor_b
                    .await
                    .expect("B supervisor task")
                    .expect("B completes normally");
                assert_eq!(budget.active(), 0, "both gateway connections ended");

                acknowledge_a.send(()).expect("acknowledge A teardown");
                assert_eq!(budget.active(), 0);
            })
            .await;
    }

    #[tokio::test]
    async fn retained_old_handles_cannot_operate_on_replacement_child() {
        tokio::task::LocalSet::new()
            .run_until(async {
                let budget = ConnectionBudget::new(1).expect("one connection slot");
                let (process_a, input_a, release_a) = gated_response_process(b"response A");
                let retained_process_a = process_a.clone();
                let retained_stdin_a = retained_process_a
                    .stdin_request()
                    .send()
                    .promise
                    .await
                    .expect("retain A stdin response")
                    .get()
                    .expect("retain A stdin results")
                    .get_stream()
                    .expect("retain A stdin");
                let retained_stdout_a = retained_process_a
                    .stdout_request()
                    .send()
                    .promise
                    .await
                    .expect("retain A stdout response")
                    .get()
                    .expect("retain A stdout results")
                    .get_stream()
                    .expect("retain A stdout");
                let (network_a, mut peer_a) = io::duplex(64 * 1024);
                let supervisor_a = tokio::task::spawn_local(handle_connection(
                    executor_for_process(process_a),
                    test_membrane(Rc::new(Cell::new(0))),
                    network_a.compat(),
                    "old-a",
                    test_guard(),
                    Duration::from_secs(30),
                    CancellationToken::new(),
                    budget.try_acquire().expect("permit A"),
                ));
                peer_a.write_all(b"request A").await.expect("write A");
                peer_a.shutdown().await.expect("half-close A");
                assert_eq!(input_a.await.expect("A observed EOF"), b"request A");
                release_a.send(()).expect("release A response");
                let mut response_a = Vec::new();
                peer_a.read_to_end(&mut response_a).await.expect("read A");
                assert_eq!(response_a, b"response A");
                supervisor_a
                    .await
                    .expect("A supervisor task")
                    .expect("A supervisor result");
                assert_eq!(budget.active(), 0);

                let (process_c, input_c, release_c) = gated_response_process(b"response C");
                let (network_c, mut peer_c) = io::duplex(64 * 1024);
                let supervisor_c = tokio::task::spawn_local(handle_connection(
                    executor_for_process(process_c),
                    test_membrane(Rc::new(Cell::new(0))),
                    network_c.compat(),
                    "replacement-c",
                    test_guard(),
                    Duration::from_secs(30),
                    CancellationToken::new(),
                    budget.try_acquire().expect("permit C"),
                ));
                peer_c.write_all(b"request C").await.expect("write C");
                peer_c.shutdown().await.expect("half-close C");
                assert_eq!(input_c.await.expect("C observed EOF"), b"request C");

                let mut stale_write = retained_stdin_a.write_request();
                stale_write.get().set_data(b"must not reach C");
                assert!(
                    stale_write.send().promise.await.is_err(),
                    "A stdin stays closed"
                );
                let mut stale_read = retained_stdout_a.read_request();
                stale_read.get().set_max_bytes(64 * 1024);
                if let Ok(response) = stale_read.send().promise.await {
                    assert!(
                        response
                            .get()
                            .expect("stale A read results")
                            .get_data()
                            .expect("stale A bytes")
                            .is_empty(),
                        "A stdout exposes no C bytes"
                    );
                }
                retained_process_a
                    .kill_request()
                    .send()
                    .promise
                    .await
                    .expect("old A kill request remains isolated");

                release_c.send(()).expect("release C response");
                let mut response_c = Vec::new();
                peer_c.read_to_end(&mut response_c).await.expect("read C");
                assert_eq!(response_c, b"response C");
                supervisor_c
                    .await
                    .expect("C supervisor task")
                    .expect("C supervisor result");
                assert_eq!(budget.active(), 0);
            })
            .await;
    }
}
