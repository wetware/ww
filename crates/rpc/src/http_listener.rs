//! HttpListener capability: WAGI/CGI cells served over HTTP.
//!
//! The `HttpListener` capability lets a guest register an HTTP endpoint.
//! For each incoming request matching the path prefix, the host spawns a
//! cell process (via the guest-provided `Executor`) with CGI env vars
//! as environment, request body piped to stdin, and CGI response read from stdout.
//!
//! Route registrations are stored in a shared `RouteRegistry` that the
//! `WagiService` (axum HTTP server) reads on every request. Because Cap'n
//! Proto clients are `!Send`, we use a channel-based dispatch: the axum
//! handler sends requests through an mpsc channel, and a local task on the
//! RPC event loop spawns cells and sends responses back.

use capnp::capability::Promise;
use capnp_rpc::pry;
use membrane::EpochGuard;
use std::{fmt, time::Duration};
use tokio::sync::mpsc;

use crate::dispatch::{self, CgiRequest, CgiResponse, RouteRegistry};
use crate::synapse_abi::{read_owned_synapse, write_owned_synapse, OwnedSynapse};
use membrane::system_capnp;

/// Maximum response size from a cell process (16 MiB).
const MAX_RESPONSE_BYTES: usize = 16 * 1024 * 1024;

/// Default wall-clock bound for one WAGI request.
const WAGI_REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

/// Best-effort cleanup bound for a kill RPC after the response decision is made.
const WAGI_KILL_TIMEOUT: Duration = Duration::from_millis(500);

pub struct HttpListenerImpl {
    guard: EpochGuard,
    registry: RouteRegistry,
}

impl HttpListenerImpl {
    pub fn new(guard: EpochGuard, registry: RouteRegistry) -> Self {
        Self { guard, registry }
    }
}

/// A captured init.d `with`-block grant. Cloned per request and re-emitted on
/// each spawned cell's graft.
type ExtraCap = (String, OwnedSynapse);

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

        // Read optional caps from the listen request (init.d `with` block grants).
        // Same pattern as VatListenerImpl::listen.
        let extra_caps: Vec<ExtraCap> = {
            let mut caps_vec = Vec::new();
            if let Ok(caps_reader) = reader.get_caps() {
                for entry in caps_reader.iter() {
                    if let Ok(name) = entry.get_name().map(|n| n.to_string().unwrap_or_default()) {
                        if let Ok(synapse) = entry.get_synapse().and_then(read_owned_synapse) {
                            caps_vec.push((name, synapse));
                        }
                    }
                }
            }
            caps_vec
        };

        // Create a channel for the axum handler to send requests through.
        let (tx, rx) = mpsc::channel::<CgiRequest>(64);

        // Spawn a local task that receives HTTP requests from the channel,
        // spawns cells via Executor, and sends CGI responses back.
        tokio::task::spawn_local(dispatch_loop(executor, extra_caps, rx));

        // Register the route with its request sender.
        match self.registry.write() {
            Ok(mut routes) => {
                tracing::info!(prefix = %prefix, "registered HTTP route");
                routes.insert(prefix, tx);
                Promise::ok(())
            }
            Err(_) => Promise::err(capnp::Error::failed("route registry lock poisoned".into())),
        }
    }
}

/// Receive HTTP requests from the channel, spawn cells, send responses back.
async fn dispatch_loop(
    executor: system_capnp::executor::Client,
    caps: Vec<ExtraCap>,
    mut rx: mpsc::Receiver<CgiRequest>,
) {
    // Fetch the cell's CID for provenance headers.
    let cell_cid = match executor.cid_request().send().promise.await {
        Ok(resp) => {
            if let Ok(reader) = resp.get().unwrap().get_cid() {
                reader.to_str().unwrap_or("unknown").to_string()
            } else {
                "unknown".to_string()
            }
        }
        Err(e) => {
            tracing::warn!("failed to fetch cell CID: {e}");
            "unknown".to_string()
        }
    };

    while let Some(req) = rx.recv().await {
        let executor = executor.clone();
        let caps = caps.clone();
        let cell_cid = cell_cid.clone();
        // Handle each request concurrently.
        tokio::task::spawn_local(async move {
            let mut response = handle_one_request(&executor, &caps, &req).await;
            response.headers.push(("X-Wetware-Cell".to_string(), cell_cid));
            let _ = req.response_tx.send(response);
        });
    }
}

/// Spawn a cell, pipe stdin/stdout, parse CGI response.
async fn handle_one_request(
    executor: &system_capnp::executor::Client,
    caps: &[ExtraCap],
    req: &CgiRequest,
) -> CgiResponse {
    handle_one_request_with_timeout(executor, caps, req, WAGI_REQUEST_TIMEOUT).await
}

async fn handle_one_request_with_timeout(
    executor: &system_capnp::executor::Client,
    caps: &[ExtraCap],
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
/// Runtime+Executor API was designed for. `caps` carries init.d `with`-block grants
/// (name + capnp client + canonical Schema.Node bytes) into the spawned cell's
/// membrane graft, so a WAGI cell only sees what the init.d author handed it.
async fn spawn_and_run(
    executor: &system_capnp::executor::Client,
    caps: &[ExtraCap],
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
        let mut caps_builder = spawn_req.get().init_caps(caps.len() as u32);
        for (i, (name, synapse)) in caps.iter().enumerate() {
            let mut entry = caps_builder.reborrow().get(i as u32);
            entry.set_name(name);
            write_owned_synapse(entry.init_synapse(), synapse);
        }
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
    use tokio::io::{self, AsyncWriteExt};
    use tokio::sync::{oneshot, watch};

    /// Build an EpochGuard at seq=1 paired with its sender.
    fn test_epoch_guard() -> (
        tokio::sync::watch::Sender<membrane::Epoch>,
        membrane::EpochGuard,
    ) {
        let epoch = membrane::Epoch {
            seq: 1,
            head: vec![],
            provenance: membrane::Provenance::Block(0),
        };
        let (tx, rx) = tokio::sync::watch::channel(epoch);
        let guard = membrane::EpochGuard {
            issued_seq: 1,
            receiver: rx,
        };
        (tx, guard)
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
    }

    fn stub_executor() -> system_capnp::executor::Client {
        capnp_rpc::new_client(StubExecutor)
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
        handle_one_request_with_timeout(&executor, &[], &req, timeout).await
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
    /// the route — the no-with-block case (e.g. `(perform host :listen cell "/path")`).
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
    /// `with`-block grant case) and still register the route. This is the
    /// shape the kernel emits for `(with [(host (perform host :host))]
    /// (perform host :listen cell "/path"))`.
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
                    crate::synapse_abi::write_placeholder_synapse(entry.init_synapse(), "host");
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
                tx.send(membrane::Epoch {
                    seq: 2,
                    head: vec![],
                    provenance: membrane::Provenance::Block(0),
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
}
