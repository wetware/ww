//! HttpListener capability: WAGI/CGI cells served over HTTP.
//!
//! The `HttpListener` capability lets a guest register an HTTP endpoint.
//! For each incoming request matching the path prefix, the host spawns a
//! cell process (via the guest-provided `Executor`) with CGI env vars
//! as environment, request body piped to stdin, and CGI response read from stdout.
//! This is intentionally a stateless WAGI request adapter; long-lived service
//! identity belongs on vat RPC, and long-lived HTTP-facing sessions belong on
//! the stream/WebSocket path.
//!
//! Route registrations are stored in a shared `RouteRegistry` that the
//! `WagiService` (axum HTTP server) reads on every request. Because Cap'n
//! Proto clients are `!Send`, we use a channel-based dispatch: the axum
//! handler sends requests through an mpsc channel, and a local task on the
//! RPC event loop spawns cells and sends responses back.

use authority::EpochGuard;
use capnp::capability::Promise;
use capnp_rpc::pry;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;
use std::{fmt, time::Duration};
use tokio::sync::mpsc;

use crate::dispatch::{self, CgiRequest, CgiResponse, RegistrationId, RouteEntry, RouteRegistry};
use crate::{decode_exports, encode_exports, NamedCapabilities};
use authority::system_capnp;

/// Maximum response size from a cell process (16 MiB).
const MAX_RESPONSE_BYTES: usize = 16 * 1024 * 1024;

/// Default wall-clock bound for one WAGI request.
const WAGI_REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

/// Best-effort cleanup bound for a kill RPC after the response decision is made.
const WAGI_KILL_TIMEOUT: Duration = Duration::from_millis(500);

pub struct HttpListenerImpl {
    guard: EpochGuard,
    registry: RouteRegistry,
    registration_scope: Option<tokio::sync::watch::Receiver<()>>,
}

impl HttpListenerImpl {
    pub fn new(guard: EpochGuard, registry: RouteRegistry) -> Self {
        Self {
            guard,
            registry,
            registration_scope: None,
        }
    }

    /// Bind registrations to the trusted pid0 execution generation that
    /// issued this listener. Closing the scope drops each owned lease even if
    /// its epoch is still current, such as when replacement init registers a
    /// route and then fails. This is host execution state, not a child-visible
    /// channel.
    pub(crate) fn new_scoped(
        guard: EpochGuard,
        registry: RouteRegistry,
        registration_scope: tokio::sync::watch::Receiver<()>,
    ) -> Self {
        Self {
            guard,
            registry,
            registration_scope: Some(registration_scope),
        }
    }
}

/// Ownership guard for one exact route-table entry.
///
/// The dispatch task is the registration owner. Whether it ends because its
/// epoch became stale, its route sender was dropped/replaced, or the task was
/// cancelled, dropping this lease removes only the entry it installed.
struct RouteRegistration {
    registry: RouteRegistry,
    prefix: String,
    id: RegistrationId,
}

impl RouteRegistration {
    fn install(
        registry: RouteRegistry,
        prefix: String,
        sender: tokio::sync::mpsc::Sender<CgiRequest>,
        epoch_guard: EpochGuard,
        registration_scope: Option<tokio::sync::watch::Receiver<()>>,
    ) -> Result<Self, capnp::Error> {
        Self::install_with_target_state(
            registry,
            prefix,
            sender,
            epoch_guard,
            registration_scope,
            true,
        )
    }

    #[cfg(test)]
    fn install_pending(
        registry: RouteRegistry,
        prefix: String,
        sender: tokio::sync::mpsc::Sender<CgiRequest>,
        epoch_guard: EpochGuard,
        registration_scope: Option<tokio::sync::watch::Receiver<()>>,
    ) -> Result<Self, capnp::Error> {
        Self::install_with_target_state(
            registry,
            prefix,
            sender,
            epoch_guard,
            registration_scope,
            false,
        )
    }

    fn install_with_target_state(
        registry: RouteRegistry,
        prefix: String,
        sender: tokio::sync::mpsc::Sender<CgiRequest>,
        epoch_guard: EpochGuard,
        registration_scope: Option<tokio::sync::watch::Receiver<()>>,
        target_ready: bool,
    ) -> Result<Self, capnp::Error> {
        let id = RegistrationId::next();
        let target_ready = Arc::new(AtomicBool::new(target_ready));
        {
            let mut routes = registry
                .write()
                .map_err(|_| capnp::Error::failed("route registry lock poisoned".into()))?;
            routes.insert(
                prefix.clone(),
                RouteEntry::new(
                    id,
                    sender,
                    epoch_guard,
                    registration_scope,
                    target_ready.clone(),
                ),
            );
        }
        Ok(Self {
            registry,
            prefix,
            id,
        })
    }

    fn remove_if_owned(&self) -> bool {
        let Ok(mut routes) = self.registry.write() else {
            tracing::error!(
                prefix = %self.prefix,
                registration_id = ?self.id,
                "route registry lock poisoned during registration cleanup"
            );
            return false;
        };

        let owns_current_entry = routes
            .get(&self.prefix)
            .is_some_and(|entry| entry.registration_id() == self.id);
        if owns_current_entry {
            routes.remove(&self.prefix);
            tracing::info!(
                prefix = %self.prefix,
                registration_id = ?self.id,
                "unregistered HTTP route"
            );
            true
        } else {
            false
        }
    }
}

impl Drop for RouteRegistration {
    fn drop(&mut self) {
        self.remove_if_owned();
    }
}

#[allow(refining_impl_trait)]
impl system_capnp::http_listener::Server for HttpListenerImpl {
    fn listen(
        self: capnp::capability::Rc<Self>,
        params: system_capnp::http_listener::ListenParams,
        _results: system_capnp::http_listener::ListenResults,
    ) -> Promise<(), capnp::Error> {
        pry!(self.guard.check());

        let reader = pry!(params.get());
        let executor = pry!(reader.get_executor());
        let prefix = pry!(pry!(reader.get_prefix()).to_str());

        // Normalize prefix: ensure it starts with /
        let prefix = if prefix.starts_with('/') {
            prefix.to_string()
        } else {
            format!("/{prefix}")
        };

        // Decode once at registration. `NamedCapabilities` is immutable, so
        // every request receives the same fixed grant template and the
        // listener cannot widen it after registration.
        let grant_template = pry!(reader.get_caps().and_then(decode_exports));

        let guard = self.guard.clone();
        let registry = self.registry.clone();
        let registration_scope = self.registration_scope.clone();
        Promise::from_future(async move {
            // PID0 must not call `kernel_ready()` until the target component is
            // valid. A failed preflight is an initialization failure, not a
            // transient unavailable route.
            let cid_response = executor.cid_request().send().promise.await?;
            let cell_cid = read_preflight_cid(&cid_response).map_err(capnp::Error::failed)?;
            guard.check()?;

            let (tx, rx) = mpsc::channel::<CgiRequest>(64);
            let registration = RouteRegistration::install(
                registry,
                prefix.clone(),
                tx,
                guard.clone(),
                registration_scope.clone(),
            )?;
            tracing::info!(
                prefix = %prefix,
                registration_id = ?registration.id,
                issued_epoch = guard.issued_seq,
                "registered HTTP route"
            );

            let epoch_rx = guard.receiver.clone();
            let issued_seq = guard.issued_seq;
            // The local task owns the registration lease. Its Drop path performs
            // compare-and-remove cleanup on epoch expiry, issuing-session loss,
            // and cancellation alike.
            tokio::task::spawn_local(dispatch_loop(
                registration,
                DispatchLoop {
                    issued_seq,
                    epoch_rx,
                    registration_scope,
                    executor,
                    caps: grant_template,
                    cell_cid,
                },
                rx,
            ));

            Ok(())
        })
    }
}

/// Receive HTTP requests from the channel, spawn cells, send responses back.
struct DispatchLoop {
    issued_seq: u64,
    epoch_rx: tokio::sync::watch::Receiver<authority::Epoch>,
    registration_scope: Option<tokio::sync::watch::Receiver<()>>,
    executor: system_capnp::executor::Client,
    caps: NamedCapabilities,
    cell_cid: String,
}

async fn dispatch_loop(
    _registration: RouteRegistration,
    mut dispatch: DispatchLoop,
    mut rx: mpsc::Receiver<CgiRequest>,
) {
    if dispatch.epoch_rx.borrow().seq != dispatch.issued_seq {
        return;
    }

    loop {
        let req = tokio::select! {
            biased;
            _ = wait_for_registration_expiry(
                &mut dispatch.epoch_rx,
                dispatch.issued_seq,
                &mut dispatch.registration_scope,
            ) => break,
            req = rx.recv() => match req {
                Some(req) => req,
                None => break,
            },
        };
        if dispatch.epoch_rx.borrow().seq != dispatch.issued_seq {
            break;
        }
        let executor = dispatch.executor.clone();
        let caps = dispatch.caps.clone();
        let cell_cid = dispatch.cell_cid.clone();
        // Handle each request concurrently.
        tokio::task::spawn_local(async move {
            let mut response = handle_one_request(&executor, &caps, &req).await;
            response
                .headers
                .push(("X-Wetware-Cell".to_string(), cell_cid));
            let _ = req.response_tx.send(response);
        });
    }
}

fn read_preflight_cid(
    response: &capnp::capability::Response<system_capnp::executor::cid_results::Owned>,
) -> Result<String, String> {
    let results = response
        .get()
        .map_err(|error| format!("failed to read cell CID response: {error}"))?;
    let reader = results
        .get_cid()
        .map_err(|error| format!("failed to read cell CID: {error}"))?;
    let value = reader
        .to_str()
        .map_err(|error| format!("failed to decode cell CID: {error}"))?;
    let cid = value
        .parse::<cid::Cid>()
        .map_err(|error| format!("failed to parse cell CID: {error}"))?;
    Ok(cid.to_string())
}

async fn wait_for_registration_expiry(
    epoch_rx: &mut tokio::sync::watch::Receiver<authority::Epoch>,
    issued_seq: u64,
    registration_scope: &mut Option<tokio::sync::watch::Receiver<()>>,
) {
    loop {
        if epoch_rx.borrow().seq != issued_seq {
            return;
        }
        if let Some(scope) = registration_scope {
            tokio::select! {
                changed = epoch_rx.changed() => {
                    if changed.is_err() {
                        return;
                    }
                }
                changed = scope.changed() => {
                    if changed.is_err() {
                        return;
                    }
                }
            }
        } else if epoch_rx.changed().await.is_err() {
            return;
        }
    }
}

/// Spawn a cell, pipe stdin/stdout, parse CGI response.
async fn handle_one_request(
    executor: &system_capnp::executor::Client,
    caps: &NamedCapabilities,
    req: &CgiRequest,
) -> CgiResponse {
    handle_one_request_with_timeout(executor, caps, req, WAGI_REQUEST_TIMEOUT).await
}

async fn handle_one_request_with_timeout(
    executor: &system_capnp::executor::Client,
    caps: &NamedCapabilities,
    req: &CgiRequest,
    timeout: Duration,
) -> CgiResponse {
    match spawn_and_run(executor, caps, req, timeout).await {
        Ok(stdout) => match crate::wagi::parse_cgi_response(&stdout) {
            Ok(cgi) => CgiResponse {
                status: cgi.status_code,
                headers: cgi.headers.into_iter().collect(),
                body: cgi.body,
            },
            Err(e) => CgiResponse {
                status: 502,
                headers: vec![("content-type".to_string(), "text/plain".to_string())],
                body: format!("CGI parse error: {e}").into_bytes(),
            },
        },
        Err(WagiRequestError::Timeout { timeout }) => CgiResponse {
            status: 504,
            headers: vec![("content-type".to_string(), "text/plain".to_string())],
            body: format!("cell timed out after {}s", timeout.as_secs()).into_bytes(),
        },
        Err(WagiRequestError::Cell(e)) => CgiResponse {
            status: 502,
            headers: vec![("content-type".to_string(), "text/plain".to_string())],
            body: format!("cell error: {e}").into_bytes(),
        },
    }
}

#[derive(Debug)]
enum WagiRequestError {
    Cell(capnp::Error),
    Timeout { timeout: Duration },
}

impl fmt::Display for WagiRequestError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Cell(err) => write!(f, "{err}"),
            Self::Timeout { timeout } => write!(f, "cell timed out after {timeout:?}"),
        }
    }
}

impl From<capnp::Error> for WagiRequestError {
    fn from(err: capnp::Error) -> Self {
        Self::Cell(err)
    }
}

/// Spawn a cell via Executor, write body to stdin, read stdout.
///
/// Per-request CGI env vars (REQUEST_METHOD, PATH_INFO, etc.) are passed via
/// `executor.spawn(args, env, caps, ...)` — this is the late-binding pattern that the
/// Runtime+Executor API was designed for. `caps` carries explicit registration
/// grants into the spawned cell's `InitialGrants` bootstrap, so a WAGI cell
/// receives only the registration-time grant template.
async fn spawn_and_run(
    executor: &system_capnp::executor::Client,
    caps: &NamedCapabilities,
    req: &CgiRequest,
    timeout: Duration,
) -> Result<Vec<u8>, WagiRequestError> {
    let (server_name, server_port) = dispatch::extract_server_info(&req.headers);
    let env = crate::wagi::build_cgi_env(
        &req.method,
        &req.path,
        &req.query,
        &req.headers,
        &server_name,
        server_port,
    );

    let mut spawn_req = executor.spawn_request();
    {
        let mut builder = spawn_req.get();
        let mut env_list = builder.reborrow().init_env(env.len() as u32);
        for (i, e) in env.iter().enumerate() {
            env_list.set(i as u32, e);
        }
    }
    if !caps.is_empty() {
        let caps_builder = spawn_req.get().init_caps(caps.len() as u32);
        encode_exports(caps, caps_builder)?;
    }
    let spawn_resp = spawn_req.send().promise.await?;
    let process = spawn_resp.get()?.get_process()?;

    match tokio::time::timeout(timeout, run_spawned_process(&process, req)).await {
        Ok(result) => result.map_err(WagiRequestError::Cell),
        Err(_) => {
            kill_process_best_effort(&process);
            Err(WagiRequestError::Timeout { timeout })
        }
    }
}

fn kill_process_best_effort(process: &system_capnp::process::Client) {
    let process = process.clone();
    tokio::task::spawn_local(async move {
        match tokio::time::timeout(WAGI_KILL_TIMEOUT, process.kill_request().send().promise).await {
            Ok(Ok(_)) => {}
            Ok(Err(err)) => tracing::warn!(error = %err, "process.kill failed during WAGI cleanup"),
            Err(_) => tracing::warn!("process.kill timed out during WAGI cleanup"),
        }
    });
}

async fn run_spawned_process(
    process: &system_capnp::process::Client,
    req: &CgiRequest,
) -> Result<Vec<u8>, capnp::Error> {
    // Write request body to stdin, then close.
    let stdin_resp = process.stdin_request().send().promise.await?;
    let stdin = stdin_resp.get()?.get_stream()?;
    if !req.body.is_empty() {
        let mut write_req = stdin.write_request();
        write_req.get().set_data(&req.body);
        write_req.send().promise.await?;
    }
    stdin.close_request().send().promise.await?;

    // Read stdout until EOF.
    let stdout_resp = process.stdout_request().send().promise.await?;
    let stdout = stdout_resp.get()?.get_stream()?;
    let mut response = Vec::new();
    loop {
        let mut read_req = stdout.read_request();
        read_req.get().set_max_bytes(64 * 1024);
        let read_resp = read_req.send().promise.await?;
        let chunk = read_resp.get()?.get_data()?;
        if chunk.is_empty() {
            break;
        }
        response.extend_from_slice(chunk);
        if response.len() > MAX_RESPONSE_BYTES {
            kill_process_best_effort(process);
            return Err(capnp::Error::failed(format!(
                "cell response exceeded {MAX_RESPONSE_BYTES} bytes"
            )));
        }
    }

    // Collect exit code for observability.
    let wait_resp = process.wait_request().send().promise.await?;
    let exit_code = wait_resp.get()?.get_exit_code();
    if exit_code != 0 {
        tracing::warn!(exit_code, "WAGI cell exited with non-zero code");
    }

    Ok(response)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dispatch::new_registry;
    use crate::{ByteStreamImpl, ProcessImpl, StreamMode};
    use capnp::private::capability::ResponseHook;
    use std::cell::RefCell;
    use std::rc::Rc;
    use tokio::io::{self, AsyncWriteExt};
    use tokio::sync::{oneshot, watch};

    const TEST_CELL_CID: &str = "bafkr4if3s6yv23hd3hgfvftj2g2uwdrqazv53p36p5lqyy7n77d5t5p54a";

    /// Build an EpochGuard at seq=1 paired with its sender.
    fn test_epoch_guard() -> (
        tokio::sync::watch::Sender<authority::Epoch>,
        authority::EpochGuard,
    ) {
        let epoch = authority::Epoch {
            seq: 1,
            head: vec![],
            root: None,
        };
        let (tx, rx) = tokio::sync::watch::channel(epoch);
        let guard = authority::EpochGuard {
            issued_seq: 1,
            receiver: rx,
        };
        (tx, guard)
    }

    fn guard_for(
        tx: &tokio::sync::watch::Sender<authority::Epoch>,
        issued_seq: u64,
    ) -> authority::EpochGuard {
        authority::EpochGuard {
            issued_seq,
            receiver: tx.subscribe(),
        }
    }

    fn advance_epoch(tx: &tokio::sync::watch::Sender<authority::Epoch>, seq: u64) {
        tx.send_replace(authority::Epoch {
            seq,
            head: vec![],
            root: None,
        });
    }

    async fn register_test_route(
        registry: &RouteRegistry,
        guard: authority::EpochGuard,
        prefix: &str,
    ) -> RegistrationId {
        let listener: system_capnp::http_listener::Client =
            capnp_rpc::new_client(HttpListenerImpl::new(guard, registry.clone()));
        let mut req = listener.listen_request();
        req.get().set_executor(stub_executor());
        req.get().set_prefix(prefix);
        req.send()
            .promise
            .await
            .expect("test route registration should succeed");
        registry
            .read()
            .expect("registry lock")
            .get(prefix)
            .expect("registered route")
            .registration_id()
    }

    async fn wait_for_route_count(registry: &RouteRegistry, expected: usize) {
        tokio::time::timeout(Duration::from_secs(1), async {
            loop {
                if registry.read().expect("registry lock").len() == expected {
                    return;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap_or_else(|_| {
            panic!(
                "route count did not reach {expected}; current count is {}",
                registry.read().expect("registry lock").len()
            )
        });
    }

    async fn wait_for_live_route_count(registry: &RouteRegistry, expected: usize) {
        tokio::time::timeout(Duration::from_secs(1), async {
            loop {
                if dispatch::live_route_count(registry) == Ok(expected) {
                    return;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap_or_else(|_| {
            panic!(
                "live route count did not reach {expected}; current count is {:?}",
                dispatch::live_route_count(registry)
            )
        });
    }

    /// Stub Executor that errors on spawn — fine for tests that only verify
    /// `listen` accepts caps + registers the route. Per-request cap propagation
    /// (caps reaching `executor.spawn`) needs the kernel/cell-builder integration
    /// path and is covered there, not here.
    struct StubExecutor;

    #[allow(refining_impl_trait)]
    impl system_capnp::executor::Server for StubExecutor {
        fn spawn(
            self: capnp::capability::Rc<Self>,
            _params: system_capnp::executor::SpawnParams,
            _results: system_capnp::executor::SpawnResults,
        ) -> Promise<(), capnp::Error> {
            Promise::err(capnp::Error::failed("stub executor".into()))
        }

        fn cid(
            self: capnp::capability::Rc<Self>,
            _params: system_capnp::executor::CidParams,
            mut results: system_capnp::executor::CidResults,
        ) -> Promise<(), capnp::Error> {
            results.get().set_cid(TEST_CELL_CID);
            Promise::ok(())
        }
    }

    fn stub_executor() -> system_capnp::executor::Client {
        capnp_rpc::new_client(StubExecutor)
    }

    struct RecordingExecutor {
        observed_grants: Rc<RefCell<Vec<Vec<String>>>>,
    }

    #[allow(refining_impl_trait)]
    impl system_capnp::executor::Server for RecordingExecutor {
        fn spawn(
            self: capnp::capability::Rc<Self>,
            params: system_capnp::executor::SpawnParams,
            mut results: system_capnp::executor::SpawnResults,
        ) -> Promise<(), capnp::Error> {
            let params = pry!(params.get());
            let grants = pry!(params.get_caps().and_then(decode_exports));
            self.observed_grants
                .borrow_mut()
                .push(grants.iter().map(|entry| entry.name().to_owned()).collect());

            let (stdout_stream, mut stdout_writer) = io::duplex(1024);
            tokio::task::spawn_local(async move {
                stdout_writer
                    .write_all(b"Content-Type: application/json\r\n\r\n{}")
                    .await
                    .expect("write CGI fixture");
                stdout_writer.shutdown().await.expect("close CGI fixture");
            });
            let (kill_tx, _kill_rx) = watch::channel(false);
            results
                .get()
                .set_process(process_with_stdout(stdout_stream, kill_tx));
            Promise::ok(())
        }

        fn cid(
            self: capnp::capability::Rc<Self>,
            _params: system_capnp::executor::CidParams,
            mut results: system_capnp::executor::CidResults,
        ) -> Promise<(), capnp::Error> {
            results.get().set_cid(TEST_CELL_CID);
            Promise::ok(())
        }
    }

    #[derive(Clone, Copy, Debug)]
    enum CidPreflightOutcome {
        RpcFailure,
        UnreadableField,
        InvalidUtf8,
        InvalidCid,
    }

    struct UnreadableCidFieldProcess;
    impl system_capnp::process::Server for UnreadableCidFieldProcess {}

    struct GatedCidExecutor {
        outcome: CidPreflightOutcome,
        gate: RefCell<Option<oneshot::Receiver<()>>>,
    }

    #[allow(refining_impl_trait)]
    impl system_capnp::executor::Server for GatedCidExecutor {
        fn cid(
            self: capnp::capability::Rc<Self>,
            _params: system_capnp::executor::CidParams,
            mut results: system_capnp::executor::CidResults,
        ) -> impl std::future::Future<Output = Result<(), capnp::Error>> + 'static {
            let gate = self.gate.borrow_mut().take().expect("single CID request");
            let outcome = self.outcome;
            async move {
                gate.await
                    .map_err(|_| capnp::Error::failed("CID test gate dropped".into()))?;
                match outcome {
                    CidPreflightOutcome::RpcFailure => {
                        Err(capnp::Error::failed("CID preflight failed".into()))
                    }
                    CidPreflightOutcome::UnreadableField => {
                        // Keep the result root readable while placing a capability
                        // pointer where the CID text pointer belongs.
                        let root = results.hook.get()?;
                        let mut malformed =
                            root.get_as::<system_capnp::executor::spawn_results::Builder<'_>>()?;
                        malformed.set_process(capnp_rpc::new_client(UnreadableCidFieldProcess));
                        Ok(())
                    }
                    CidPreflightOutcome::InvalidUtf8 => {
                        results.get().set_cid(capnp::text::Reader(&[0xff]));
                        Ok(())
                    }
                    CidPreflightOutcome::InvalidCid => {
                        results.get().set_cid("not-a-cid");
                        Ok(())
                    }
                }
            }
        }
    }

    struct UnreadableCidResponse;

    impl ResponseHook for UnreadableCidResponse {
        fn get(&self) -> capnp::Result<capnp::any_pointer::Reader<'_>> {
            Err(capnp::Error::failed("unreadable CID response".into()))
        }
    }

    async fn assert_failed_cid_preflight_rejects_listen(outcome: CidPreflightOutcome) {
        let (_epoch_tx, guard) = test_epoch_guard();
        let registry = new_registry();
        let listener: system_capnp::http_listener::Client =
            capnp_rpc::new_client(HttpListenerImpl::new(guard, registry.clone()));
        let (gate_tx, gate_rx) = oneshot::channel();
        let executor: system_capnp::executor::Client = capnp_rpc::new_client(GatedCidExecutor {
            outcome,
            gate: RefCell::new(Some(gate_rx)),
        });
        let mut request = listener.listen_request();
        request.get().set_executor(executor);
        request.get().set_prefix("/preflight");
        gate_tx.send(()).expect("release CID preflight");
        let error = match request.send().promise.await {
            Ok(_) => panic!("failed CID preflight must reject listen"),
            Err(error) => error,
        };
        assert!(error.to_string().contains("CID") || error.to_string().contains("cid"));
        wait_for_route_count(&registry, 0).await;
        assert_eq!(dispatch::live_route_count(&registry), Ok(0));
        assert!(
            registry.read().expect("registry lock").is_empty(),
            "failed {outcome:?} preflight must not install a route"
        );
    }

    #[tokio::test]
    async fn failed_cid_preflights_never_become_live_and_release_their_routes() {
        let local = tokio::task::LocalSet::new();
        local
            .run_until(async {
                for outcome in [
                    CidPreflightOutcome::RpcFailure,
                    CidPreflightOutcome::InvalidUtf8,
                    CidPreflightOutcome::InvalidCid,
                ] {
                    assert_failed_cid_preflight_rejects_listen(outcome).await;
                }
            })
            .await;
    }

    #[tokio::test]
    async fn unreadable_cid_field_never_becomes_live_and_releases_its_route() {
        let local = tokio::task::LocalSet::new();
        local
            .run_until(assert_failed_cid_preflight_rejects_listen(
                CidPreflightOutcome::UnreadableField,
            ))
            .await;
    }

    #[test]
    fn unreadable_cid_response_fails_without_marking_the_route_live() {
        let registry = new_registry();
        let (_epoch_tx, guard) = test_epoch_guard();
        let (tx, _rx) = mpsc::channel(1);
        let registration = RouteRegistration::install_pending(
            registry.clone(),
            "/unreadable".into(),
            tx,
            guard,
            None,
        )
        .expect("install pending route");
        let pending = registry
            .read()
            .expect("registry lock")
            .get("/unreadable")
            .cloned()
            .expect("pending route");
        let response =
            capnp::capability::Response::<system_capnp::executor::cid_results::Owned>::new(
                Box::new(UnreadableCidResponse),
            );

        let error = read_preflight_cid(&response).expect_err("response read must fail");
        assert!(
            error.contains("failed to read cell CID response"),
            "{error}"
        );
        assert!(!pending.is_live());
        drop(registration);
        assert!(registry.read().expect("registry lock").is_empty());
        assert!(!pending.is_live());
    }

    struct ProcessExecutor {
        process: system_capnp::process::Client,
    }

    #[allow(refining_impl_trait)]
    impl system_capnp::executor::Server for ProcessExecutor {
        fn spawn(
            self: capnp::capability::Rc<Self>,
            _params: system_capnp::executor::SpawnParams,
            mut results: system_capnp::executor::SpawnResults,
        ) -> Promise<(), capnp::Error> {
            results.get().set_process(self.process.clone());
            Promise::ok(())
        }
    }

    fn executor_for_process(
        process: system_capnp::process::Client,
    ) -> system_capnp::executor::Client {
        capnp_rpc::new_client(ProcessExecutor { process })
    }

    fn test_request() -> CgiRequest {
        let (response_tx, _response_rx) = oneshot::channel();
        CgiRequest {
            method: "GET".to_string(),
            path: "/status".to_string(),
            query: String::new(),
            headers: vec![("host".to_string(), "localhost:2080".to_string())],
            body: Vec::new(),
            response_tx,
        }
    }

    fn process_with_stdout(
        stdout_stream: io::DuplexStream,
        kill_tx: watch::Sender<bool>,
    ) -> system_capnp::process::Client {
        let (stdin_stream, _stdin_peer) = io::duplex(64 * 1024);
        let (stderr_stream, _stderr_peer) = io::duplex(1);
        let stdin = capnp_rpc::new_client(ByteStreamImpl::new(stdin_stream, StreamMode::WriteOnly));
        let stdout =
            capnp_rpc::new_client(ByteStreamImpl::new(stdout_stream, StreamMode::ReadOnly));
        let stderr =
            capnp_rpc::new_client(ByteStreamImpl::new(stderr_stream, StreamMode::ReadOnly));
        let (exit_tx, exit_rx) = oneshot::channel();
        let _ = exit_tx.send(0);
        capnp_rpc::new_client(ProcessImpl::new(stdin, stdout, stderr, exit_rx, kill_tx))
    }

    struct HangingKillProcess {
        stdin: system_capnp::byte_stream::Client,
        stdout: system_capnp::byte_stream::Client,
        stderr: system_capnp::byte_stream::Client,
    }

    #[allow(refining_impl_trait)]
    impl system_capnp::process::Server for HangingKillProcess {
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

        fn stderr(
            self: capnp::capability::Rc<Self>,
            _params: system_capnp::process::StderrParams,
            mut results: system_capnp::process::StderrResults,
        ) -> Promise<(), capnp::Error> {
            results.get().set_stream(self.stderr.clone());
            Promise::ok(())
        }

        fn wait(
            self: capnp::capability::Rc<Self>,
            _params: system_capnp::process::WaitParams,
            _results: system_capnp::process::WaitResults,
        ) -> impl std::future::Future<Output = Result<(), capnp::Error>> + 'static {
            std::future::pending()
        }

        fn bootstrap(
            self: capnp::capability::Rc<Self>,
            _params: system_capnp::process::BootstrapParams,
            _results: system_capnp::process::BootstrapResults,
        ) -> Promise<(), capnp::Error> {
            Promise::err(capnp::Error::failed("no bootstrap".into()))
        }

        fn kill(
            self: capnp::capability::Rc<Self>,
            _params: system_capnp::process::KillParams,
            _results: system_capnp::process::KillResults,
        ) -> impl std::future::Future<Output = Result<(), capnp::Error>> + 'static {
            std::future::pending()
        }
    }

    fn hanging_kill_process(stdout_stream: io::DuplexStream) -> system_capnp::process::Client {
        let (stdin_stream, _stdin_peer) = io::duplex(64 * 1024);
        let (stderr_stream, _stderr_peer) = io::duplex(1);
        let stdin = capnp_rpc::new_client(ByteStreamImpl::new(stdin_stream, StreamMode::WriteOnly));
        let stdout =
            capnp_rpc::new_client(ByteStreamImpl::new(stdout_stream, StreamMode::ReadOnly));
        let stderr =
            capnp_rpc::new_client(ByteStreamImpl::new(stderr_stream, StreamMode::ReadOnly));
        capnp_rpc::new_client(HangingKillProcess {
            stdin,
            stdout,
            stderr,
        })
    }

    async fn response_for_process(
        process: system_capnp::process::Client,
        timeout: Duration,
    ) -> CgiResponse {
        let executor = executor_for_process(process);
        let req = test_request();
        handle_one_request_with_timeout(&executor, &NamedCapabilities::default(), &req, timeout)
            .await
    }

    #[tokio::test]
    async fn wagi_request_completes_before_timeout() {
        let local = tokio::task::LocalSet::new();
        local
            .run_until(async {
                let (stdout_stream, mut stdout_writer) = io::duplex(64 * 1024);
                let (kill_tx, kill_rx) = watch::channel(false);
                let process = process_with_stdout(stdout_stream, kill_tx);
                tokio::task::spawn_local(async move {
                    stdout_writer
                        .write_all(b"Status: 201 Created\r\nContent-Type: text/plain\r\n\r\nok")
                        .await
                        .expect("write CGI response");
                    stdout_writer.shutdown().await.expect("close stdout");
                });

                let response = response_for_process(process, Duration::from_secs(1)).await;

                assert_eq!(response.status, 201);
                assert_eq!(response.body, b"ok");
                assert!(!*kill_rx.borrow(), "normal request should not be killed");
            })
            .await;
    }

    #[tokio::test]
    async fn wagi_request_timeout_kills_process_and_returns_504() {
        let local = tokio::task::LocalSet::new();
        local
            .run_until(async {
                let (stdout_stream, _stdout_writer) = io::duplex(64 * 1024);
                let (kill_tx, mut kill_rx) = watch::channel(false);
                let process = process_with_stdout(stdout_stream, kill_tx);

                let response = response_for_process(process, Duration::from_millis(20)).await;

                assert_eq!(response.status, 504);
                assert!(
                    String::from_utf8_lossy(&response.body).contains("timed out"),
                    "timeout response body should explain the failure"
                );
                assert!(
                    tokio::time::timeout(Duration::from_secs(1), kill_rx.changed())
                        .await
                        .expect("kill signal should arrive")
                        .is_ok(),
                    "kill watch should stay open"
                );
                assert!(
                    *kill_rx.borrow(),
                    "timeout path should call process.kill() best-effort"
                );
            })
            .await;
    }

    #[tokio::test]
    async fn wagi_timeout_returns_504_even_when_kill_rpc_hangs() {
        let local = tokio::task::LocalSet::new();
        local
            .run_until(async {
                let (stdout_stream, _stdout_writer) = io::duplex(64 * 1024);
                let process = hanging_kill_process(stdout_stream);

                let response = tokio::time::timeout(
                    Duration::from_millis(250),
                    response_for_process(process, Duration::from_millis(20)),
                )
                .await
                .expect("hung kill RPC should not delay timeout response");

                assert_eq!(response.status, 504);
            })
            .await;
    }

    #[tokio::test]
    async fn oversized_wagi_response_still_kills_and_returns_502() {
        let local = tokio::task::LocalSet::new();
        local
            .run_until(async {
                let (stdout_stream, mut stdout_writer) = io::duplex(64 * 1024);
                let (kill_tx, mut kill_rx) = watch::channel(false);
                let process = process_with_stdout(stdout_stream, kill_tx);
                tokio::task::spawn_local(async move {
                    let oversized = vec![b'x'; MAX_RESPONSE_BYTES + 1];
                    stdout_writer
                        .write_all(&oversized)
                        .await
                        .expect("write oversized response");
                    stdout_writer.shutdown().await.expect("close stdout");
                });

                let response = response_for_process(process, Duration::from_secs(5)).await;

                assert_eq!(response.status, 502);
                assert!(
                    String::from_utf8_lossy(&response.body).contains("exceeded"),
                    "oversized response should keep existing error mapping"
                );
                assert!(
                    tokio::time::timeout(Duration::from_secs(1), kill_rx.changed())
                        .await
                        .expect("kill signal should arrive")
                        .is_ok(),
                    "kill watch should stay open"
                );
                assert!(
                    *kill_rx.borrow(),
                    "oversized response path should still kill the process"
                );
            })
            .await;
    }

    /// `HttpListener.listen` should accept an empty caps list and register
    /// the route — the explicit zero-grant case
    /// (e.g. `(perform host :listen (cell image :grants {}) "/path")`).
    #[tokio::test]
    async fn test_http_listener_listen_with_empty_caps_registers_route() {
        let local = tokio::task::LocalSet::new();
        local
            .run_until(async {
                let (_tx, guard) = test_epoch_guard();
                let registry = new_registry();
                let listener_impl = HttpListenerImpl::new(guard, registry.clone());
                let listener: system_capnp::http_listener::Client =
                    capnp_rpc::new_client(listener_impl);

                let mut req = listener.listen_request();
                req.get().set_executor(stub_executor());
                req.get().set_prefix("/status");
                // No caps set — empty list (default).

                req.send()
                    .promise
                    .await
                    .expect("listen with empty caps should succeed");

                let routes = registry.read().expect("registry not poisoned");
                assert!(
                    routes.contains_key("/status"),
                    "route /status should be registered"
                );
            })
            .await;
    }

    /// `HttpListener.listen` should accept a non-empty caps list (the init.d
    /// explicit-grant case) and still register the route. This is the
    /// shape the kernel emits for
    /// `(perform host :listen (cell image :grants {:host host}) "/path")`.
    #[tokio::test]
    async fn test_http_listener_listen_with_caps_registers_route() {
        let local = tokio::task::LocalSet::new();
        local
            .run_until(async {
                let (_tx, guard) = test_epoch_guard();
                let registry = new_registry();
                let listener_impl = HttpListenerImpl::new(guard, registry.clone());
                let listener: system_capnp::http_listener::Client =
                    capnp_rpc::new_client(listener_impl);

                let mut req = listener.listen_request();
                req.get().set_executor(stub_executor());
                req.get().set_prefix("/status");
                {
                    let mut caps_builder = req.get().init_caps(1);
                    let mut entry = caps_builder.reborrow().get(0);
                    entry.set_name("host");
                    entry
                        .init_cap()
                        .set_as_capability(stub_executor().client.hook);
                }

                req.send()
                    .promise
                    .await
                    .expect("listen with non-empty caps should succeed");

                let routes = registry.read().expect("registry not poisoned");
                assert!(
                    routes.contains_key("/status"),
                    "route /status should be registered"
                );
            })
            .await;
    }

    #[tokio::test]
    async fn registration_caps_are_a_fixed_template_for_every_request_child() {
        let local = tokio::task::LocalSet::new();
        local
            .run_until(async {
                let (_tx, guard) = test_epoch_guard();
                let registry = new_registry();
                let listener: system_capnp::http_listener::Client =
                    capnp_rpc::new_client(HttpListenerImpl::new(guard, registry.clone()));
                let observed_grants = Rc::new(RefCell::new(Vec::new()));
                let executor: system_capnp::executor::Client =
                    capnp_rpc::new_client(RecordingExecutor {
                        observed_grants: observed_grants.clone(),
                    });

                let mut listen = listener.listen_request();
                listen.get().set_executor(executor);
                listen.get().set_prefix("/fixed");
                {
                    let mut entry = listen.get().init_caps(1).get(0);
                    entry.set_name("only-grant");
                    entry
                        .init_cap()
                        .set_as_capability(stub_executor().client.hook);
                }
                listen
                    .send()
                    .promise
                    .await
                    .expect("register fixed grant template");

                let route = registry
                    .read()
                    .expect("registry lock")
                    .get("/fixed")
                    .map(|entry| entry.sender())
                    .expect("fixed route");
                for _ in 0..2 {
                    let (response_tx, response_rx) = oneshot::channel();
                    route
                        .send(CgiRequest {
                            method: "GET".into(),
                            path: "/fixed".into(),
                            query: String::new(),
                            headers: Vec::new(),
                            body: Vec::new(),
                            response_tx,
                        })
                        .await
                        .expect("dispatch fixed-template request");
                    let response = response_rx.await.expect("CGI response");
                    assert_eq!(response.status, 200);
                    assert!(response.headers.iter().any(|(name, value)| {
                        name == "X-Wetware-Cell" && value == TEST_CELL_CID
                    }));
                }

                assert_eq!(
                    observed_grants.borrow().as_slice(),
                    &[vec!["only-grant".to_owned()], vec!["only-grant".to_owned()]],
                    "each request must instantiate the same immutable registration template"
                );
            })
            .await;
    }

    /// `HttpListener.listen` must fail when its `EpochGuard` is stale —
    /// matching the VatListener guard semantics.
    #[tokio::test]
    async fn test_http_listener_listen_errors_on_stale_epoch() {
        let local = tokio::task::LocalSet::new();
        local
            .run_until(async {
                let (tx, guard) = test_epoch_guard();
                let registry = new_registry();
                let listener_impl = HttpListenerImpl::new(guard, registry.clone());
                let listener: system_capnp::http_listener::Client =
                    capnp_rpc::new_client(listener_impl);

                // Advance the epoch past the issued seq.
                tx.send(authority::Epoch {
                    seq: 2,
                    head: vec![],
                    root: None,
                })
                .expect("epoch broadcast");

                let mut req = listener.listen_request();
                req.get().set_executor(stub_executor());
                req.get().set_prefix("/status");

                let result = req.send().promise.await;
                assert!(
                    result.is_err(),
                    "listen should fail after the epoch advances past issued seq"
                );
            })
            .await;
    }

    #[tokio::test]
    async fn old_registration_expires_after_epoch_advance() {
        let local = tokio::task::LocalSet::new();
        local
            .run_until(async {
                let (tx, guard) = test_epoch_guard();
                let registry = new_registry();
                register_test_route(&registry, guard, "/status").await;
                assert_eq!(registry.read().expect("registry lock").len(), 1);
                let stale_sender = registry
                    .read()
                    .expect("registry lock")
                    .get("/status")
                    .expect("old route")
                    .sender();

                advance_epoch(&tx, 2);
                wait_for_route_count(&registry, 0).await;
                assert!(
                    !registry
                        .read()
                        .expect("registry lock")
                        .contains_key("/status"),
                    "the stale registration must become unreachable"
                );
                assert!(
                    stale_sender.send(test_request()).await.is_err(),
                    "a sender cloned before expiry must no longer reach a handler"
                );
            })
            .await;
    }

    #[test]
    fn fresh_replacement_at_same_path_survives_old_cleanup() {
        let registry = new_registry();
        let (epoch_tx, old_guard) = test_epoch_guard();
        let (old_tx, _old_rx) = mpsc::channel(1);
        let old =
            RouteRegistration::install(registry.clone(), "/status".into(), old_tx, old_guard, None)
                .unwrap();
        let old_id = old.id;

        advance_epoch(&epoch_tx, 2);
        let (fresh_tx, _fresh_rx) = mpsc::channel(1);
        let fresh = RouteRegistration::install(
            registry.clone(),
            "/status".into(),
            fresh_tx,
            guard_for(&epoch_tx, 2),
            None,
        )
        .unwrap();
        let fresh_id = fresh.id;
        assert_ne!(old_id, fresh_id);

        drop(old);

        let routes = registry.read().expect("registry lock");
        assert_eq!(routes.len(), 1);
        assert_eq!(
            routes.get("/status").map(RouteEntry::registration_id),
            Some(fresh_id),
            "late cleanup from the old epoch must not remove its replacement"
        );
        drop(routes);
        drop(fresh);
    }

    #[test]
    fn old_and_fresh_cleanup_can_race_without_removing_the_wrong_entry() {
        use std::sync::{Arc, Barrier};

        let registry = new_registry();
        let (epoch_tx, old_guard) = test_epoch_guard();
        let (old_tx, _old_rx) = mpsc::channel(1);
        let old =
            RouteRegistration::install(registry.clone(), "/status".into(), old_tx, old_guard, None)
                .unwrap();
        advance_epoch(&epoch_tx, 2);
        let (fresh_tx, _fresh_rx) = mpsc::channel(1);
        let fresh = RouteRegistration::install(
            registry.clone(),
            "/status".into(),
            fresh_tx,
            guard_for(&epoch_tx, 2),
            None,
        )
        .unwrap();

        let barrier = Arc::new(Barrier::new(3));
        let (old_removed, fresh_removed) = std::thread::scope(|scope| {
            let old_barrier = barrier.clone();
            let old_cleanup = scope.spawn(move || {
                old_barrier.wait();
                old.remove_if_owned()
            });
            let fresh_barrier = barrier.clone();
            let fresh_cleanup = scope.spawn(move || {
                fresh_barrier.wait();
                fresh.remove_if_owned()
            });
            barrier.wait();
            (
                old_cleanup.join().expect("old cleanup thread"),
                fresh_cleanup.join().expect("fresh cleanup thread"),
            )
        });

        assert!(
            !old_removed,
            "the old cleanup must lose compare-and-remove after replacement"
        );
        assert!(
            fresh_removed,
            "the current registration must remove its own entry"
        );
        assert!(registry.read().expect("registry lock").is_empty());
    }

    #[tokio::test]
    async fn restart_leaves_exactly_one_live_replacement_route() {
        let local = tokio::task::LocalSet::new();
        local
            .run_until(async {
                let (tx, old_guard) = test_epoch_guard();
                let registry = new_registry();
                let old_id = register_test_route(&registry, old_guard, "/status").await;

                advance_epoch(&tx, 2);
                let fresh_id = register_test_route(&registry, guard_for(&tx, 2), "/status").await;
                assert_ne!(old_id, fresh_id);

                // Give the old registration's stale cleanup a chance to run
                // after the replacement has become live.
                tokio::task::yield_now().await;
                tokio::task::yield_now().await;

                let routes = registry.read().expect("registry lock");
                assert_eq!(routes.len(), 1);
                assert_eq!(
                    routes.get("/status").map(RouteEntry::registration_id),
                    Some(fresh_id)
                );
            })
            .await;
    }

    #[tokio::test]
    async fn failed_replacement_init_leaves_no_stale_live_route() {
        let local = tokio::task::LocalSet::new();
        local
            .run_until(async {
                let (tx, old_guard) = test_epoch_guard();
                let registry = new_registry();
                register_test_route(&registry, old_guard, "/status").await;

                // A failed replacement init never calls listen. Expiry of the
                // old registration must therefore leave no route that a
                // readiness check could mistake for the replacement.
                advance_epoch(&tx, 2);
                wait_for_route_count(&registry, 0).await;
                assert!(registry.read().expect("registry lock").is_empty());
            })
            .await;
    }

    #[tokio::test]
    async fn failed_session_after_registration_invalidates_and_removes_replacement_route() {
        let local = tokio::task::LocalSet::new();
        local
            .run_until(async {
                let (_epoch_tx, guard) = test_epoch_guard();
                let registry = new_registry();
                let (session_tx, session_rx) = watch::channel(());
                let listener: system_capnp::http_listener::Client = capnp_rpc::new_client(
                    HttpListenerImpl::new_scoped(guard, registry.clone(), session_rx),
                );
                let mut request = listener.listen_request();
                request.get().set_executor(stub_executor());
                request.get().set_prefix("/status");
                request
                    .send()
                    .promise
                    .await
                    .expect("replacement registers before later init failure");

                wait_for_live_route_count(&registry, 1).await;
                assert_eq!(dispatch::live_route_count(&registry), Ok(1));
                drop(session_tx);
                assert_eq!(
                    dispatch::live_route_count(&registry),
                    Ok(0),
                    "session failure must stop readiness before async lease cleanup"
                );
                wait_for_route_count(&registry, 0).await;
                assert!(
                    registry.read().expect("registry lock").is_empty(),
                    "the failed replacement session must release its owned route"
                );
            })
            .await;
    }

    #[test]
    fn dropping_registration_removes_only_that_registration() {
        let registry = new_registry();
        let (_epoch_tx, guard) = test_epoch_guard();
        let (status_tx, _status_rx) = mpsc::channel(1);
        let status = RouteRegistration::install(
            registry.clone(),
            "/status".into(),
            status_tx,
            guard.clone(),
            None,
        )
        .unwrap();
        let (other_tx, _other_rx) = mpsc::channel(1);
        let other =
            RouteRegistration::install(registry.clone(), "/other".into(), other_tx, guard, None)
                .unwrap();
        let other_id = other.id;

        drop(status);

        let routes = registry.read().expect("registry lock");
        assert!(!routes.contains_key("/status"));
        assert_eq!(routes.len(), 1);
        assert_eq!(
            routes.get("/other").map(RouteEntry::registration_id),
            Some(other_id)
        );
        drop(routes);
        drop(other);
    }

    #[tokio::test]
    async fn repeated_epoch_changes_do_not_accumulate_routes_or_tasks() {
        let local = tokio::task::LocalSet::new();
        local
            .run_until(async {
                let (tx, initial_guard) = test_epoch_guard();
                let registry = new_registry();
                let mut weak_senders = Vec::new();

                for seq in 1..=7 {
                    let guard = if seq == 1 {
                        initial_guard.clone()
                    } else {
                        guard_for(&tx, seq)
                    };
                    register_test_route(&registry, guard, "/status").await;
                    weak_senders.push(
                        registry
                            .read()
                            .expect("registry lock")
                            .get("/status")
                            .expect("live route")
                            .sender()
                            .downgrade(),
                    );

                    advance_epoch(&tx, seq + 1);
                    wait_for_route_count(&registry, 0).await;
                }

                tokio::task::yield_now().await;
                assert!(registry.read().expect("registry lock").is_empty());
                assert!(
                    weak_senders.iter().all(|sender| sender.upgrade().is_none()),
                    "every expired registration task must release its request channel"
                );
            })
            .await;
    }
}
