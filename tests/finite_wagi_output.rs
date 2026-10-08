//! Completion and exact-output tests for finite first-party WAGI artifacts.

#[path = "support/ticked_executor.rs"]
mod ticked_executor;

use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll, Waker};

use capnp::capability::Promise;
use tokio::io::AsyncWrite;
use tokio::sync::{watch, Notify};
use ww::cell::{Builder, Proc, Program};
use ww::launcher::create_runtime_client;
use ww::rpc::CachePolicy;
use ww::{http_capnp, system_capnp};

use ticked_executor::TickedExecutor;

const COUNTER_WASM: &str = "examples/counter/bin/counter.wasm";
const ORACLE_WASM: &str = "examples/oracle/bin/oracle.wasm";
const SNAP_WASM: &str = "examples/snap-hello-rs/bin/snap-hello-rs.wasm";

#[derive(Clone, Default)]
struct CaptureWriter {
    bytes: Arc<Mutex<Vec<u8>>>,
}

impl CaptureWriter {
    fn bytes(&self) -> Vec<u8> {
        self.bytes.lock().expect("capture lock").clone()
    }
}

impl AsyncWrite for CaptureWriter {
    fn poll_write(
        self: Pin<&mut Self>,
        _cx: &mut Context<'_>,
        bytes: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        self.bytes.lock().expect("capture lock").extend(bytes);
        Poll::Ready(Ok(bytes.len()))
    }

    fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Poll::Ready(Ok(()))
    }

    fn poll_shutdown(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Poll::Ready(Ok(()))
    }
}

#[derive(Clone, Copy)]
enum FlushRelease {
    Pending,
    Success,
    BrokenPipe,
}

struct FlushState {
    release: FlushRelease,
    waker: Option<Waker>,
}

#[derive(Clone)]
struct FlushControl {
    state: Arc<Mutex<FlushState>>,
    started: Arc<Notify>,
}

impl FlushControl {
    fn new() -> Self {
        Self {
            state: Arc::new(Mutex::new(FlushState {
                release: FlushRelease::Pending,
                waker: None,
            })),
            started: Arc::new(Notify::new()),
        }
    }

    fn release(&self, release: FlushRelease) {
        let waker = {
            let mut state = self.state.lock().expect("flush state lock");
            state.release = release;
            state.waker.take()
        };
        if let Some(waker) = waker {
            waker.wake();
        }
    }
}

struct GatedFlushWriter {
    capture: CaptureWriter,
    control: FlushControl,
}

impl GatedFlushWriter {
    fn poll_gate(&self, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        self.control.started.notify_one();
        let mut state = self.control.state.lock().expect("flush state lock");
        match state.release {
            FlushRelease::Pending => {
                state.waker = Some(cx.waker().clone());
                Poll::Pending
            }
            FlushRelease::Success => Poll::Ready(Ok(())),
            FlushRelease::BrokenPipe => Poll::Ready(Err(std::io::Error::new(
                std::io::ErrorKind::BrokenPipe,
                "injected final flush failure",
            ))),
        }
    }
}

impl AsyncWrite for GatedFlushWriter {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        bytes: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        Pin::new(&mut self.capture).poll_write(cx, bytes)
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        self.poll_gate(cx)
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        self.poll_gate(cx)
    }
}

struct ClosedWriter;

impl AsyncWrite for ClosedWriter {
    fn poll_write(
        self: Pin<&mut Self>,
        _cx: &mut Context<'_>,
        _bytes: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        Poll::Ready(Err(std::io::Error::new(
            std::io::ErrorKind::BrokenPipe,
            "injected closed stdout",
        )))
    }

    fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Poll::Ready(Err(std::io::Error::new(
            std::io::ErrorKind::BrokenPipe,
            "injected closed stdout",
        )))
    }

    fn poll_shutdown(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Poll::Ready(Ok(()))
    }
}

struct PartialThenFailWriter {
    bytes: Arc<Mutex<Vec<u8>>>,
    prefix_len: usize,
    accepted_prefix: bool,
}

impl AsyncWrite for PartialThenFailWriter {
    fn poll_write(
        mut self: Pin<&mut Self>,
        _cx: &mut Context<'_>,
        bytes: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        if self.accepted_prefix {
            return Poll::Ready(Err(std::io::Error::new(
                std::io::ErrorKind::BrokenPipe,
                "injected failure after response prefix",
            )));
        }
        let count = self.prefix_len.min(bytes.len());
        self.bytes
            .lock()
            .expect("partial output lock")
            .extend_from_slice(&bytes[..count]);
        self.accepted_prefix = true;
        Poll::Ready(Ok(count))
    }

    fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Poll::Ready(Ok(()))
    }

    fn poll_shutdown(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Poll::Ready(Ok(()))
    }
}

fn artifact(path: &str) -> Option<Vec<u8>> {
    match std::fs::read(path) {
        Ok(bytes) => Some(bytes),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            eprintln!("skipping: {path} not built (run `make test-wasm` first)");
            None
        }
        Err(error) => panic!("read {path}: {error}"),
    }
}

async fn build_proc<W>(wasm: Vec<u8>, env: Vec<String>, stdout: W) -> (Proc, TickedExecutor)
where
    W: AsyncWrite + Send + Sync + Unpin + 'static,
{
    let ticked = TickedExecutor::new();
    let (builder, _transport) = Builder::ordinary(
        Program::Bytes(wasm),
        tokio::io::empty(),
        stdout,
        tokio::io::sink(),
    );
    let proc = builder
        .with_env(env)
        .with_runtime_engine(ticked.runtime_engine())
        .build()
        .await
        .expect("build finite WAGI artifact");
    (proc, ticked)
}

fn cgi_env(method: &str, path: &str, query: &str, headers: &[(&str, &str)]) -> Vec<String> {
    let headers = headers
        .iter()
        .map(|(name, value)| ((*name).to_string(), (*value).to_string()))
        .collect::<Vec<_>>();
    ww::dispatcher::wagi::build_cgi_env(method, path, query, &headers, "example.test", 2080)
}

async fn exact_output(artifact_path: &str, env: Vec<String>, expected: &[u8]) -> Option<()> {
    let wasm = artifact(artifact_path)?;
    let capture = CaptureWriter::default();
    let (proc, _ticked) = build_proc(wasm, env, capture.clone()).await;
    proc.run().await.expect("finite WAGI artifact succeeds");
    assert_eq!(capture.bytes(), expected);
    Some(())
}

struct FixedHttpResponse {
    status: u16,
    body: Vec<u8>,
}

#[allow(refining_impl_trait)]
impl http_capnp::http_client::Server for FixedHttpResponse {
    fn get(
        self: capnp::capability::Rc<Self>,
        _params: http_capnp::http_client::GetParams,
        mut results: http_capnp::http_client::GetResults,
    ) -> Promise<(), capnp::Error> {
        let mut response = results.get();
        response.set_status(self.status);
        response.reborrow().init_headers(0);
        response.set_body(&self.body);
        Promise::ok(())
    }
}

#[derive(Clone)]
struct OracleGraft {
    http: http_capnp::http_client::Client,
}

impl authority::GraftBuilder for OracleGraft {
    fn build(
        &self,
        _guard: &authority::EpochGuard,
        mut builder: system_capnp::membrane::graft_results::Builder<'_>,
    ) -> Result<(), capnp::Error> {
        builder.set_peer_id(b"oracle-test-peer");
        builder
            .reborrow()
            .init_network()
            .init_http()
            .set_dialer(self.http.clone());
        Ok(())
    }
}

async fn run_system_wagi(wasm: Vec<u8>, env: Vec<String>, graft: OracleGraft) -> Vec<u8> {
    let epoch = authority::Epoch {
        seq: 1,
        head: vec![],
        root: None,
    };
    let (_epoch_tx, epoch_rx) = watch::channel(epoch);
    let guard = authority::EpochGuard {
        issued_seq: 1,
        receiver: epoch_rx.clone(),
    };
    let ticked = TickedExecutor::new();
    let runtime = create_runtime_client(
        false,
        guard,
        ticked.runtime_engine(),
        None,
        CachePolicy::Shared,
    );
    let membrane: system_capnp::membrane::Client =
        capnp_rpc::new_client(authority::MembraneServer::new(epoch_rx, graft));

    let mut load = runtime.load_request();
    load.get().set_wasm(&wasm);
    let executor = load
        .send()
        .promise
        .await
        .expect("runtime load")
        .get()
        .expect("load response")
        .get_executor()
        .expect("executor");

    let mut spawn = executor.spawn_request();
    {
        let mut request = spawn.get();
        let mut env_list = request.reborrow().init_env(env.len() as u32);
        for (index, value) in env.iter().enumerate() {
            env_list.set(index as u32, value);
        }
        request.set_membrane(membrane);
    }
    let process = spawn
        .send()
        .promise
        .await
        .expect("executor spawn")
        .get()
        .expect("spawn response")
        .get_process()
        .expect("process");

    let stdin = process
        .stdin_request()
        .send()
        .promise
        .await
        .expect("stdin request")
        .get()
        .expect("stdin response")
        .get_stream()
        .expect("stdin stream");
    stdin
        .close_request()
        .send()
        .promise
        .await
        .expect("close stdin");

    let stdout = process
        .stdout_request()
        .send()
        .promise
        .await
        .expect("stdout request")
        .get()
        .expect("stdout response")
        .get_stream()
        .expect("stdout stream");
    let mut response = Vec::new();
    loop {
        let mut read = stdout.read_request();
        read.get().set_max_bytes(64 * 1024);
        let reply = read.send().promise.await.expect("read stdout");
        let chunk = reply
            .get()
            .expect("read response")
            .get_data()
            .expect("stdout data");
        if chunk.is_empty() {
            break;
        }
        response.extend_from_slice(chunk);
    }

    let exit_code = process
        .wait_request()
        .send()
        .promise
        .await
        .expect("wait request")
        .get()
        .expect("wait response")
        .get_exit_code();
    assert_eq!(exit_code, 0);
    response
}

async fn oracle_output(status: u16, body: &[u8]) -> Option<Vec<u8>> {
    let wasm = artifact(ORACLE_WASM)?;
    let http = capnp_rpc::new_client(FixedHttpResponse {
        status,
        body: body.to_vec(),
    });
    Some(
        run_system_wagi(
            wasm,
            cgi_env("GET", "/oracle", "pair=ETH%2Fgas", &[]),
            OracleGraft { http },
        )
        .await,
    )
}

const COUNTER_GET_RESPONSE: &[u8] = b"Status: 200 OK\r\nContent-Type: text/plain\r\n\r\n0";

#[tokio::test(flavor = "current_thread")]
async fn counter_get_emits_exact_cgi_bytes() {
    exact_output(
        COUNTER_WASM,
        cgi_env("GET", "/counter", "", &[]),
        COUNTER_GET_RESPONSE,
    )
    .await;
}

#[tokio::test(flavor = "current_thread")]
async fn counter_post_emits_exact_cgi_bytes() {
    exact_output(
        COUNTER_WASM,
        cgi_env("POST", "/counter", "", &[]),
        b"Status: 200 OK\r\nContent-Type: text/plain\r\n\r\n1",
    )
    .await;
}

#[tokio::test(flavor = "current_thread")]
async fn counter_method_not_allowed_emits_exact_cgi_bytes() {
    exact_output(
        COUNTER_WASM,
        cgi_env("DELETE", "/counter", "", &[]),
        b"Status: 405 Method Not Allowed\r\nContent-Type: text/plain\r\n\r\nMethod Not Allowed",
    )
    .await;
}

#[tokio::test(flavor = "current_thread")]
async fn snap_json_emits_exact_cgi_bytes() {
    exact_output(
        SNAP_WASM,
        cgi_env(
            "GET",
            "/snaps/hello",
            "",
            &[
                ("Accept", "application/vnd.farcaster.snap+json"),
                ("Host", "example.test"),
            ],
        ),
        concat!(
            "Status: 200 OK\r\n",
            "Content-Type: application/vnd.farcaster.snap+json\r\n",
            "Vary: Accept\r\n",
            "Cache-Control: public, max-age=300\r\n",
            "Access-Control-Allow-Origin: *\r\n\r\n",
            "{\"ui\":{\"elements\":{\"greeting\":{\"props\":{\"content\":\"Hello, @stranger\"},\"type\":\"text\"},",
            "\"ping_button\":{\"on\":{\"press\":{\"action\":\"submit\",\"params\":{\"target\":\"https://example.test/snaps/hello\"}}},",
            "\"props\":{\"label\":\"Ping me\",\"variant\":\"primary\"},\"type\":\"button\"},",
            "\"root\":{\"children\":[\"greeting\",\"ping_button\"],\"props\":{\"direction\":\"vertical\",\"gap\":\"sm\"},\"type\":\"stack\"}},",
            "\"root\":\"root\"},\"version\":\"2.0\"}"
        )
        .as_bytes(),
    )
    .await;
}

#[tokio::test(flavor = "current_thread")]
async fn snap_html_emits_exact_cgi_bytes() {
    exact_output(
        SNAP_WASM,
        cgi_env("GET", "/snaps/hello", "", &[("Accept", "text/html")]),
        concat!(
            "Status: 200 OK\r\n",
            "Content-Type: text/html; charset=utf-8\r\n",
            "Vary: Accept\r\n",
            "Cache-Control: public, max-age=300\r\n",
            "Access-Control-Allow-Origin: *\r\n",
            "Link: <>; rel=\"alternate\"; type=\"application/vnd.farcaster.snap+json\"\r\n\r\n",
            "<!DOCTYPE html>\n<html>\n<head>\n  <meta charset=\"utf-8\">\n  <title>Hello from a wetware snap</title>\n",
            "  <link rel=\"alternate\" type=\"application/vnd.farcaster.snap+json\" href=\"\">\n",
            "  <meta property=\"og:title\" content=\"Hello from a wetware snap\">\n</head>\n<body>\n",
            "  <h1>Hello, @stranger</h1>\n  <p>This is a Farcaster Snap hosted on wetware.\n",
            "     Open this URL in a Farcaster client to render it (with a\n     \"Ping me\" button).</p>\n</body>\n</html>"
        )
        .as_bytes(),
    )
    .await;
}

#[tokio::test(flavor = "current_thread")]
async fn oracle_success_emits_exact_cgi_bytes() {
    let response = tokio::task::LocalSet::new()
        .run_until(oracle_output(200, br#"{"blockPrices":[]}"#))
        .await;
    let Some(response) = response else {
        return;
    };
    assert_eq!(
        response,
        concat!(
            "Status: 200 OK\r\nContent-Type: application/json\r\n\r\n",
            "{\n  \"pairs\": {\n    \"ETH/gas\": {\n      \"confidence\": 0.0,\n",
            "      \"price\": 0.0,\n      \"timestamp\": 0,\n      \"unit\": \"gwei\"\n",
            "    }\n  }\n}"
        )
        .as_bytes()
    );
}

#[tokio::test(flavor = "current_thread")]
async fn oracle_upstream_failure_emits_exact_cgi_bytes() {
    let response = tokio::task::LocalSet::new()
        .run_until(oracle_output(503, b"unavailable"))
        .await;
    let Some(response) = response else {
        return;
    };
    assert_eq!(
        response,
        concat!(
            "Status: 502 Bad Gateway\r\nContent-Type: text/plain\r\n\r\n",
            "price fetch failed: Failed: Blocknative API returned status 503"
        )
        .as_bytes()
    );
}

#[tokio::test(flavor = "current_thread")]
async fn finite_response_waits_for_final_flush() {
    let Some(wasm) = artifact(COUNTER_WASM) else {
        return;
    };
    let capture = CaptureWriter::default();
    let control = FlushControl::new();
    let stdout = GatedFlushWriter {
        capture: capture.clone(),
        control: control.clone(),
    };
    let (proc, _ticked) = build_proc(wasm, cgi_env("GET", "/counter", "", &[]), stdout).await;
    let mut run = Box::pin(proc.run());
    let flush_started = control.started.notified();
    tokio::pin!(flush_started);

    tokio::select! {
        result = &mut run => panic!("guest completed before final flush: {result:?}"),
        () = &mut flush_started => {}
    }
    let pending_bytes = capture.bytes();
    assert!(!pending_bytes.is_empty());
    assert!(COUNTER_GET_RESPONSE.starts_with(&pending_bytes));

    control.release(FlushRelease::Success);
    run.await.expect("guest succeeds after final flush");
    assert_eq!(capture.bytes(), COUNTER_GET_RESPONSE);
}

#[tokio::test(flavor = "current_thread")]
async fn final_flush_failure_reaches_the_guest_root() {
    let Some(wasm) = artifact(COUNTER_WASM) else {
        return;
    };
    let control = FlushControl::new();
    let stdout = GatedFlushWriter {
        capture: CaptureWriter::default(),
        control: control.clone(),
    };
    let (proc, _ticked) = build_proc(wasm, cgi_env("GET", "/counter", "", &[]), stdout).await;
    let mut run = Box::pin(proc.run());
    let flush_started = control.started.notified();
    tokio::pin!(flush_started);

    tokio::select! {
        result = &mut run => panic!("guest completed before final flush: {result:?}"),
        () = &mut flush_started => {}
    }
    control.release(FlushRelease::BrokenPipe);
    let error = run.await.expect_err("final flush failure reaches root");
    assert!(
        error
            .to_string()
            .contains("guest returned non-zero exit status"),
        "{error:#}"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn closed_writer_failure_reaches_the_guest_root() {
    let Some(wasm) = artifact(COUNTER_WASM) else {
        return;
    };
    let (proc, _ticked) = build_proc(wasm, cgi_env("GET", "/counter", "", &[]), ClosedWriter).await;
    let error = proc
        .run()
        .await
        .expect_err("closed stdout failure reaches root");
    assert!(
        error
            .to_string()
            .contains("guest returned non-zero exit status"),
        "{error:#}"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn partial_output_remains_observable_when_later_output_fails() {
    let Some(wasm) = artifact(COUNTER_WASM) else {
        return;
    };
    let bytes = Arc::new(Mutex::new(Vec::new()));
    let stdout = PartialThenFailWriter {
        bytes: bytes.clone(),
        prefix_len: 8,
        accepted_prefix: false,
    };
    let (proc, _ticked) = build_proc(wasm, cgi_env("GET", "/counter", "", &[]), stdout).await;
    let error = proc
        .run()
        .await
        .expect_err("partial output failure reaches root");
    assert!(
        error
            .to_string()
            .contains("guest returned non-zero exit status"),
        "{error:#}"
    );
    assert_eq!(
        bytes.lock().expect("partial output lock").as_slice(),
        b"Status: "
    );
}

#[tokio::test(flavor = "current_thread")]
async fn output_failure_is_request_local() {
    let Some(wasm) = artifact(COUNTER_WASM) else {
        return;
    };
    let env = cgi_env("GET", "/counter", "", &[]);
    let (failed_proc, _failed_ticked) = build_proc(wasm.clone(), env.clone(), ClosedWriter).await;
    failed_proc
        .run()
        .await
        .expect_err("request A must observe its closed writer");

    let capture = CaptureWriter::default();
    let (successful_proc, _successful_ticked) = build_proc(wasm, env, capture.clone()).await;
    successful_proc
        .run()
        .await
        .expect("request B must remain independent");
    assert_eq!(capture.bytes(), COUNTER_GET_RESPONSE);
}
