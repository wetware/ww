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
use tokio::sync::mpsc;

use crate::dispatch::{self, CgiRequest, CgiResponse, RouteRegistry};
use membrane::system_capnp;

/// Maximum response size from a cell process (16 MiB).
const MAX_RESPONSE_BYTES: usize = 16 * 1024 * 1024;

pub struct HttpListenerImpl {
    guard: EpochGuard,
    registry: RouteRegistry,
}

impl HttpListenerImpl {
    pub fn new(guard: EpochGuard, registry: RouteRegistry) -> Self {
        Self { guard, registry }
    }
}

/// A captured init.d `with`-block grant: name + capnp client + canonical Schema.Node bytes.
/// Cloned per request and re-emitted on each spawned cell's graft so guests can
/// introspect each cap's interface end-to-end (`(schema cap)`).
type ExtraCap = (String, capnp::capability::Client, Vec<u8>);

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
                    if let (Ok(name), Ok(cap)) = (
                        entry.get_name().map(|n| n.to_string().unwrap_or_default()),
                        entry.get_cap().get_as_capability(),
                    ) {
                        let schema_bytes = match entry.get_schema() {
                            Ok(node) => super::canonicalize_schema_node(node).unwrap_or_default(),
                            Err(_) => Vec::new(),
                        };
                        caps_vec.push((name, cap, schema_bytes));
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
    while let Some(req) = rx.recv().await {
        let executor = executor.clone();
        let caps = caps.clone();
        // Handle each request concurrently.
        tokio::task::spawn_local(async move {
            let response = handle_one_request(&executor, &caps, &req).await;
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
    match spawn_and_run(executor, caps, req).await {
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
        Err(e) => CgiResponse {
            status: 502,
            headers: vec![("content-type".to_string(), "text/plain".to_string())],
            body: format!("cell error: {e}").into_bytes(),
        },
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
) -> Result<Vec<u8>, capnp::Error> {
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
        for (i, (name, client, schema_bytes)) in caps.iter().enumerate() {
            let mut entry = caps_builder.reborrow().get(i as u32);
            entry.set_name(name);
            if !schema_bytes.is_empty() {
                let aligned = crate::graft::bytes_to_aligned_words(schema_bytes);
                let segments: &[&[u8]] = &[capnp::Word::words_to_bytes(&aligned)];
                let segment_array = capnp::message::SegmentArray::new(segments);
                let reader = capnp::message::Reader::new(
                    segment_array,
                    capnp::message::ReaderOptions::new(),
                );
                let schema_node: capnp::schema_capnp::node::Reader = reader.get_root()?;
                entry.reborrow().set_schema(schema_node)?;
            }
            entry.init_cap().set_as_capability(client.hook.clone());
        }
    }
    let spawn_resp = spawn_req.send().promise.await?;
    let process = spawn_resp.get()?.get_process()?;

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
            let _ = process.kill_request().send().promise.await;
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
                    // Use the stub executor as a stand-in capability — any
                    // capnp client works here; this test verifies the listen
                    // request shape, not what the cap does.
                    let placeholder = stub_executor();
                    entry.init_cap().set_as_capability(placeholder.client.hook);
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
