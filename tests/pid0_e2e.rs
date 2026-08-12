//! Kernel-independent PID0 lifecycle coverage.
//!
//! `shared_parity` launches the real `ww` binary and asserts the behavioral
//! contract that must hold for every PID0 implementation: boot/serving
//! identity, host-owned generation replacement with replacement-byte identity,
//! initialization failure, and TTY process lifetime. Each driver runs against
//! the embedded Rust PID0 and against the explicit `file:` Glia PID0
//! (`std/kernel-glia`). Glia-only observables live in `glia_specific`; tests
//! that ride the temporary `/ww/0.1.0` guest-membrane surface live in
//! `ww_protocol_compat`.
//!
//! CI must run `make std` before compiling this test. Missing artifacts are a
//! hard failure: silently skipping would recreate the false-green gap this
//! baseline exists to close.

mod support;

use std::io::Read;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, ExitStatus, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use ed25519_dalek::SigningKey;
use libp2p::{Multiaddr, PeerId};
use serde_json::Value;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::watch;

use axum::body::{to_bytes, Body};
use axum::extract::{Request, State};
use axum::http::{header, Response, StatusCode};
use axum::Router;

use support::atom::AtomFixture;
use support::terminal::{expect_stale_or_disconnected, TerminalSession};

const KERNEL_WASM_PATH: &str = "std/kernel/bin/main.wasm";
const KERNEL_GLIA_WASM_PATH: &str = "std/kernel-glia/bin/main.wasm";
const STATUS_WASM_PATH: &str = "std/status/bin/status.wasm";
const STATUS_LAYER: &str = "std/status";
const DEFAULT_KUBO_ADDR: &str = "127.0.0.1:5001";
const KUBO_ADDR_ENV: &str = "WW_TEST_KUBO_ADDR";
const BOOT_TIMEOUT: Duration = Duration::from_secs(60);
const EXIT_TIMEOUT: Duration = Duration::from_secs(30);
const INVALIDATION_TIMEOUT: Duration = Duration::from_secs(20);

static E2E_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

async fn e2e_lock() -> tokio::sync::MutexGuard<'static, ()> {
    E2E_LOCK.lock().await
}

fn ww_bin() -> PathBuf {
    PathBuf::from(std::env::var_os("CARGO_BIN_EXE_ww").expect("CARGO_BIN_EXE_ww missing"))
}

fn required_artifact(path: &str) -> Vec<u8> {
    let bytes = std::fs::read(path).unwrap_or_else(|error| {
        panic!(
            "required WASM artifact {path} is missing: {error}; run `make std` before `cargo test`"
        )
    });
    assert!(!bytes.is_empty(), "required WASM artifact {path} is empty");
    bytes
}

fn append_custom_section(component: &mut Vec<u8>, name: &[u8], payload: &[u8]) {
    fn push_uleb128(output: &mut Vec<u8>, mut value: usize) {
        loop {
            let mut byte = (value & 0x7f) as u8;
            value >>= 7;
            if value != 0 {
                byte |= 0x80;
            }
            output.push(byte);
            if value == 0 {
                return;
            }
        }
    }

    let mut contents = Vec::with_capacity(name.len() + payload.len() + 1);
    push_uleb128(&mut contents, name.len());
    contents.extend_from_slice(name);
    contents.extend_from_slice(payload);

    component.push(0); // Component-model custom section.
    push_uleb128(component, contents.len());
    component.extend_from_slice(&contents);
}

/// A byte-distinct build of the status component. The custom section changes
/// the runtime CID without changing behavior, so `X-Wetware-Cell` identifies
/// which deployment root's bytes are serving.
fn status_variant(base: &[u8], tag: &[u8]) -> (Vec<u8>, cid::Cid) {
    let mut bytes = base.to_vec();
    append_custom_section(&mut bytes, b"ww.test.epoch", tag);
    let cid = ww::kernel::runtime_cid(&bytes);
    (bytes, cid)
}

/// A PID0 implementation selected through the host's existing kernel-source
/// machinery. The shared drivers assert host-observable behavior only;
/// kernel-specific observables live in `glia_specific` and
/// `ww_protocol_compat`.
#[derive(Clone, Copy)]
struct KernelUnderTest {
    explicit_path: Option<&'static str>,
}

const EMBEDDED_RUST: KernelUnderTest = KernelUnderTest {
    explicit_path: None,
};
const EXPLICIT_GLIA: KernelUnderTest = KernelUnderTest {
    explicit_path: Some(KERNEL_GLIA_WASM_PATH),
};

impl KernelUnderTest {
    fn artifact_path(self) -> &'static str {
        self.explicit_path.unwrap_or(KERNEL_WASM_PATH)
    }

    /// Apply the selection to `options`; return the kernel bytes and the
    /// exact `kernel_source` string `/version` must report.
    fn select(self, options: &mut NodeOptions) -> (Vec<u8>, String) {
        let bytes = required_artifact(self.artifact_path());
        let source = match self.explicit_path {
            Some(path) => {
                let path = Path::new(path)
                    .canonicalize()
                    .unwrap_or_else(|error| panic!("canonicalize {path}: {error}"));
                let source = format!("file:{}", path.display());
                options.kernel_cli = Some(source.clone());
                source
            }
            None => "embedded:main".to_string(),
        };
        (bytes, source)
    }
}

async fn unused_addr() -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind ephemeral test listener");
    let address = listener.local_addr().expect("read ephemeral address");
    drop(listener);
    address
}

struct RunningNode {
    child: Child,
    stdin: Option<ChildStdin>,
    output: tempfile::NamedTempFile,
}

struct NodeOptions {
    mounts: Vec<String>,
    kernel_cli: Option<String>,
    kernel_env: Option<String>,
    http_addr: Option<SocketAddr>,
    listen_addr: Option<SocketAddr>,
    identity_path: Option<PathBuf>,
    insecure_ephemeral: bool,
    tty: bool,
    stem: Option<StemOptions>,
    pid0_result_race: Option<(u64, SocketAddr)>,
}

struct StemOptions {
    contract_address: String,
    rpc_url: String,
    ws_url: String,
}

impl NodeOptions {
    fn embedded(http_addr: Option<SocketAddr>) -> Self {
        Self {
            mounts: vec![STATUS_LAYER.to_string()],
            kernel_cli: None,
            kernel_env: None,
            http_addr,
            listen_addr: None,
            identity_path: None,
            insecure_ephemeral: true,
            tty: true,
            stem: None,
            pid0_result_race: None,
        }
    }
}

impl RunningNode {
    fn spawn(
        home: &Path,
        admin_addr: SocketAddr,
        kubo_addr: SocketAddr,
        options: &NodeOptions,
    ) -> Self {
        // Both host tracing and guest stderr must share one file description:
        // lifecycle ordering assertions are invalid if independently captured
        // streams are concatenated after the process exits.
        let output = tempfile::NamedTempFile::new().expect("create combined output capture");
        let mut command = Command::new(ww_bin());
        command.arg("run").args(&options.mounts);
        if options.insecure_ephemeral {
            command.arg("--insecure-ephemeral");
        }
        let listen = options
            .listen_addr
            .map(|address| format!("/ip4/127.0.0.1/tcp/{}", address.port()))
            .unwrap_or_else(|| "/ip4/127.0.0.1/tcp/0".to_string());
        command
            .arg("--listen")
            .arg(listen)
            .args(["--executor-threads", "1", "--with-http-admin"])
            .arg(admin_addr.to_string())
            .arg("--ipfs-url")
            .arg(format!("http://{kubo_addr}"));
        if let Some(http_addr) = options.http_addr {
            command.arg("--http-listen").arg(http_addr.to_string());
        }
        if let Some(kernel) = options.kernel_cli.as_ref() {
            command.arg("--kernel").arg(kernel);
        }
        if let Some(kernel) = options.kernel_env.as_ref() {
            command.env("WW_KERNEL", kernel);
        } else {
            command.env_remove("WW_KERNEL");
        }
        if let Some(identity_path) = options.identity_path.as_ref() {
            command.arg("--identity").arg(identity_path);
        }
        if let Some(stem) = options.stem.as_ref() {
            command
                .arg("--stem")
                .arg(&stem.contract_address)
                .arg("--rpc-url")
                .arg(&stem.rpc_url)
                .arg("--ws-url")
                .arg(&stem.ws_url)
                .args(["--confirmation-depth", "0", "--epoch-drain-secs", "0"]);
        }
        if let Some((seq, address)) = options.pid0_result_race {
            command.env("WW_TEST_PID0_RESULT_RACE", format!("{seq}@{address}"));
        } else {
            command.env_remove("WW_TEST_PID0_RESULT_RACE");
        }

        command
            .env("HOME", home)
            // Captured logs are asserted as plain text; CI may force ANSI colors.
            .env("NO_COLOR", "1")
            .env("WW_KUBO_WAIT_MAX_SECS", "30")
            .env("WW_CWASM_DIR", home.join("cwasm"))
            .env_remove("WW_IDENTITY")
            .env_remove("WW_HTTP_ADMIN")
            .env_remove("IPFS_API")
            .stdin(Stdio::piped())
            .stdout(Stdio::from(
                output.as_file().try_clone().expect("clone output file"),
            ))
            .stderr(Stdio::from(
                output.as_file().try_clone().expect("clone output file"),
            ));
        if options.tty {
            command.env("WW_TTY", "1");
        } else {
            command.env_remove("WW_TTY");
        }
        let mut child = command.spawn().expect("spawn real ww host binary");
        let stdin = child.stdin.take().expect("open child stdin");
        Self {
            child,
            stdin: Some(stdin),
            output,
        }
    }

    fn try_wait(&mut self) -> Option<ExitStatus> {
        self.child.try_wait().expect("poll ww child")
    }

    async fn wait(&mut self, timeout: Duration) -> ExitStatus {
        let deadline = Instant::now() + timeout;
        loop {
            if let Some(status) = self.try_wait() {
                return status;
            }
            assert!(
                Instant::now() < deadline,
                "ww did not exit within {timeout:?}\n{}",
                self.logs()
            );
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }

    fn close_stdin(&mut self) {
        drop(self.stdin.take());
    }

    fn logs(&self) -> String {
        read_capture(&self.output)
    }
}

async fn require_kubo() -> (SocketAddr, reqwest::Client) {
    let kubo_addr: SocketAddr = std::env::var(KUBO_ADDR_ENV)
        .unwrap_or_else(|_| DEFAULT_KUBO_ADDR.to_string())
        .parse()
        .expect("WW_TEST_KUBO_ADDR must be host:port");
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(2))
        .build()
        .expect("build HTTP client");
    let kubo_id = client
        .post(format!("http://{kubo_addr}/api/v0/id"))
        .send()
        .await
        .unwrap_or_else(|error| {
            panic!("Kubo is required at {kubo_addr} for pid0 E2E tests: {error}")
        });
    assert!(kubo_id.status().is_success(), "Kubo /api/v0/id failed");
    (kubo_addr, client)
}

async fn version(client: &reqwest::Client, admin_addr: SocketAddr) -> Value {
    client
        .get(format!("http://{admin_addr}/version"))
        .send()
        .await
        .expect("query /version")
        .error_for_status()
        .expect("/version should succeed")
        .json()
        .await
        .expect("parse /version JSON")
}

async fn assert_status_route(client: &reqwest::Client, http_addr: SocketAddr) {
    let status: Value = client
        .get(format!("http://{http_addr}/status"))
        .timeout(Duration::from_secs(20))
        .send()
        .await
        .expect("query real Glia-registered /status route")
        .error_for_status()
        .expect("/status should return HTTP 200")
        .json()
        .await
        .expect("parse /status JSON");
    assert_eq!(status["status"], "ok");
    assert!(
        status["peer_id"]
            .as_str()
            .is_some_and(|peer| !peer.is_empty()),
        "real status cell must receive the host grant: {status}"
    );
}

async fn assert_status_cell(
    client: &reqwest::Client,
    http_addr: SocketAddr,
    expected_cell_cid: &cid::Cid,
) {
    let response = client
        .get(format!("http://{http_addr}/status"))
        .timeout(Duration::from_secs(20))
        .send()
        .await
        .expect("query real Glia-registered /status route")
        .error_for_status()
        .expect("/status should return HTTP 200");
    let observed_cell_cid = response
        .headers()
        .get("X-Wetware-Cell")
        .expect("/status response omitted X-Wetware-Cell")
        .to_str()
        .expect("X-Wetware-Cell must be UTF-8");
    assert_eq!(observed_cell_cid, expected_cell_cid.to_string());
    let status: Value = response.json().await.expect("parse /status JSON");
    assert_eq!(status["status"], "ok");
    assert!(
        status["peer_id"]
            .as_str()
            .is_some_and(|peer| !peer.is_empty()),
        "real status cell must receive the host grant: {status}"
    );
}

impl Drop for RunningNode {
    fn drop(&mut self) {
        if matches!(self.child.try_wait(), Ok(None)) {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }
}

fn read_capture(file: &tempfile::NamedTempFile) -> String {
    let mut handle = file.reopen().expect("reopen output capture");
    let mut output = String::new();
    handle
        .read_to_string(&mut output)
        .expect("read output capture");
    output
}

async fn wait_for_http(client: &reqwest::Client, url: &str, node: &mut RunningNode) {
    let deadline = Instant::now() + BOOT_TIMEOUT;
    loop {
        if let Some(status) = node.try_wait() {
            panic!(
                "ww exited before {url} became reachable: {status}\n{}",
                node.logs()
            );
        }
        if client.get(url).send().await.is_ok() {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "timed out waiting for {url}\n{}",
            node.logs()
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

async fn wait_for_ready(client: &reqwest::Client, url: &str, node: &mut RunningNode) -> Value {
    let deadline = Instant::now() + BOOT_TIMEOUT;
    loop {
        if let Some(status) = node.try_wait() {
            panic!("ww exited before readiness: {status}\n{}", node.logs());
        }
        if let Ok(response) = client.get(url).send().await {
            let status = response.status();
            let body = response.text().await.expect("read /readyz body");
            let json: Value = serde_json::from_str(&body)
                .unwrap_or_else(|error| panic!("invalid /readyz JSON: {error}; body: {body}"));
            if status == reqwest::StatusCode::OK {
                return json;
            }
            assert_eq!(
                status,
                reqwest::StatusCode::SERVICE_UNAVAILABLE,
                "unexpected /readyz response: {body}"
            );
        }
        assert!(
            Instant::now() < deadline,
            "timed out waiting for ready\n{}",
            node.logs()
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

async fn wait_for_phase(
    client: &reqwest::Client,
    url: &str,
    phase: &str,
    node: &mut RunningNode,
) -> Value {
    let deadline = Instant::now() + BOOT_TIMEOUT;
    loop {
        if let Some(status) = node.try_wait() {
            panic!("ww exited before phase {phase}: {status}\n{}", node.logs());
        }
        if let Ok(response) = client.get(url).send().await {
            let status = response.status();
            let body = response.text().await.expect("read /readyz body");
            let json: Value = serde_json::from_str(&body)
                .unwrap_or_else(|error| panic!("invalid /readyz JSON: {error}; body: {body}"));
            assert_eq!(
                status,
                reqwest::StatusCode::SERVICE_UNAVAILABLE,
                "runtime became ready before the delayed Kubo gate opened: {body}"
            );
            if json["phase"] == phase {
                return json;
            }
        }
        assert!(
            Instant::now() < deadline,
            "timed out waiting for phase {phase}\n{}",
            node.logs()
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

fn start_kubo_proxy(listener: TcpListener, target: SocketAddr) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        loop {
            let Ok((mut inbound, _)) = listener.accept().await else {
                return;
            };
            tokio::spawn(async move {
                let Ok(mut outbound) = TcpStream::connect(target).await else {
                    return;
                };
                let _ = tokio::io::copy_bidirectional(&mut inbound, &mut outbound).await;
            });
        }
    })
}

#[derive(Clone)]
struct ControlledKuboProxy {
    client: reqwest::Client,
    target: SocketAddr,
    path_fragment: String,
    control: ProxyControl,
}

#[derive(Clone)]
enum ProxyControl {
    Delay,
    Gate {
        reached: tokio::sync::mpsc::Sender<()>,
        release: watch::Receiver<bool>,
    },
    FailMergeUntil {
        attempts: Arc<AtomicUsize>,
        release: watch::Receiver<bool>,
    },
}

async fn forward_controlled_kubo(
    State(mut proxy): State<ControlledKuboProxy>,
    request: Request,
) -> Response<Body> {
    let (parts, body) = request.into_parts();
    let uri = parts.uri.to_string();
    if uri.contains(&proxy.path_fragment) {
        match &mut proxy.control {
            ProxyControl::Delay => tokio::time::sleep(Duration::from_secs(2)).await,
            ProxyControl::Gate { reached, release } => {
                let _ = reached.send(()).await;
                while !*release.borrow() {
                    if release.changed().await.is_err() {
                        break;
                    }
                }
            }
            ProxyControl::FailMergeUntil { attempts, release }
                if uri.contains("/api/v0/files/cp") && !*release.borrow() =>
            {
                attempts.fetch_add(1, Ordering::SeqCst);
                return Response::builder()
                    .status(StatusCode::SERVICE_UNAVAILABLE)
                    .body(Body::from("content is not available yet"))
                    .expect("build transient Kubo response");
            }
            ProxyControl::FailMergeUntil { .. } => {}
        }
    }

    let body = match to_bytes(body, 64 * 1024 * 1024).await {
        Ok(body) => body,
        Err(error) => {
            return Response::builder()
                .status(StatusCode::BAD_REQUEST)
                .body(Body::from(error.to_string()))
                .expect("build proxy request-error response");
        }
    };
    let mut upstream = proxy
        .client
        .request(parts.method, format!("http://{}{}", proxy.target, uri));
    for (name, value) in &parts.headers {
        if name != header::HOST {
            upstream = upstream.header(name, value);
        }
    }
    let response = match upstream.body(body).send().await {
        Ok(response) => response,
        Err(error) => {
            return Response::builder()
                .status(StatusCode::BAD_GATEWAY)
                .body(Body::from(error.to_string()))
                .expect("build proxy upstream-error response");
        }
    };
    let status = response.status();
    let headers = response.headers().clone();
    let body = match response.bytes().await {
        Ok(body) => body,
        Err(error) => {
            return Response::builder()
                .status(StatusCode::BAD_GATEWAY)
                .body(Body::from(error.to_string()))
                .expect("build proxy body-error response");
        }
    };
    let mut output = Response::builder().status(status);
    for (name, value) in &headers {
        if name != header::TRANSFER_ENCODING && name != header::CONTENT_LENGTH {
            output = output.header(name, value);
        }
    }
    output
        .body(Body::from(body))
        .expect("build proxied Kubo response")
}

fn start_delayed_kubo_proxy(
    listener: TcpListener,
    target: SocketAddr,
    delayed_path_fragment: impl Into<String>,
) -> tokio::task::JoinHandle<()> {
    let proxy = ControlledKuboProxy {
        client: reqwest::Client::new(),
        target,
        path_fragment: delayed_path_fragment.into(),
        control: ProxyControl::Delay,
    };
    let app = Router::new()
        .fallback(forward_controlled_kubo)
        .with_state(proxy);
    tokio::spawn(async move {
        axum::serve(listener, app)
            .await
            .expect("serve delayed Kubo proxy");
    })
}

fn start_gated_kubo_proxy(
    listener: TcpListener,
    target: SocketAddr,
    path_fragment: impl Into<String>,
) -> (
    tokio::task::JoinHandle<()>,
    tokio::sync::mpsc::Receiver<()>,
    watch::Sender<bool>,
) {
    let (reached_tx, reached_rx) = tokio::sync::mpsc::channel(4);
    let (release_tx, release_rx) = watch::channel(false);
    let proxy = ControlledKuboProxy {
        client: reqwest::Client::new(),
        target,
        path_fragment: path_fragment.into(),
        control: ProxyControl::Gate {
            reached: reached_tx,
            release: release_rx,
        },
    };
    let app = Router::new()
        .fallback(forward_controlled_kubo)
        .with_state(proxy);
    let task = tokio::spawn(async move {
        axum::serve(listener, app)
            .await
            .expect("serve sentinel-gated Kubo proxy");
    });
    (task, reached_rx, release_tx)
}

fn start_failing_merge_kubo_proxy(
    listener: TcpListener,
    target: SocketAddr,
    head_cid: impl Into<String>,
) -> (
    tokio::task::JoinHandle<()>,
    Arc<AtomicUsize>,
    watch::Sender<bool>,
) {
    let attempts = Arc::new(AtomicUsize::new(0));
    let (release_tx, release_rx) = watch::channel(false);
    let proxy = ControlledKuboProxy {
        client: reqwest::Client::new(),
        target,
        path_fragment: head_cid.into(),
        control: ProxyControl::FailMergeUntil {
            attempts: Arc::clone(&attempts),
            release: release_rx,
        },
    };
    let app = Router::new()
        .fallback(forward_controlled_kubo)
        .with_state(proxy);
    let task = tokio::spawn(async move {
        axum::serve(listener, app)
            .await
            .expect("serve transient-failure Kubo proxy");
    });
    (task, attempts, release_tx)
}

struct EpochRoot {
    _directory: tempfile::TempDir,
    cid: cid::Cid,
    delay_cid: String,
    marker: Vec<u8>,
    route: String,
}

impl EpochRoot {
    async fn valid(ipfs: &ww::ipfs::HttpClient, status_wasm: &[u8], generation: u64) -> Self {
        let route = format!("/epoch-{generation}");
        let marker = format!("epoch-{generation}\n").into_bytes();
        let script = format!(
            r#"(perform :load "architecture.md")

(perform host :listen
  (cell (perform :load "bin/status.wasm")
    :grants {{:host host}})
  "/status")

(perform host :listen
  (cell (perform :load "bin/status.wasm")
    :grants {{:host host}})
  "{route}")
"#
        );
        Self::build(ipfs, status_wasm, route, marker, script).await
    }

    async fn failing(ipfs: &ww::ipfs::HttpClient, status_wasm: &[u8]) -> Self {
        let route = "/epoch-invalid".to_string();
        let marker = b"epoch-invalid\n".to_vec();
        let script = format!(
            r#"; The replacement is syntactically valid, but status points at a
; missing component. SysV best-effort continues to the route below; the
; replacement-generation policy must then fail and tear that route down.
(perform :load "architecture.md")

(perform host :listen
  (cell (perform :load "bin/missing-status.wasm")
    :grants {{:host host}})
  "/status")

(perform host :listen
  (cell (perform :load "bin/status.wasm")
    :grants {{:host host}})
  "{route}")

; The failed-replacement test delays only this Kubo load. While it is pending,
; the real route task can finish executor.cid preflight, but readiness must
; remain closed until the generation commit that this init will never reach.
(perform :load "delay.txt")
"#
        );
        Self::build(ipfs, status_wasm, route, marker, script).await
    }

    /// A deployment root whose production status policy requires
    /// `bin/status.wasm` while the tree does not contain it. Every PID0
    /// implementation must fail composition against this root.
    async fn missing_status(ipfs: &ww::ipfs::HttpClient, generation: u64) -> Self {
        let route = format!("/epoch-{generation}");
        let marker = format!("epoch-{generation}-missing-status\n").into_bytes();
        let directory = tempfile::tempdir().expect("create missing-status root");
        let init = directory.path().join("etc/init.d");
        std::fs::create_dir_all(&init).expect("create missing-status init.d");
        std::fs::copy(
            "std/status/etc/init.d/05-status.glia",
            init.join("05-status.glia"),
        )
        .expect("copy production status policy");
        std::fs::write(directory.path().join("delay.txt"), &marker)
            .expect("write missing-status delay marker");
        std::fs::write(directory.path().join("generation.txt"), &marker)
            .expect("write missing-status generation marker");
        let cid: cid::Cid = ipfs
            .add_dir(directory.path())
            .await
            .expect("add missing-status root to Kubo")
            .parse()
            .expect("Kubo returned a valid missing-status root CID");
        let delay_cid = ipfs
            .ls(&format!("/ipfs/{cid}"))
            .await
            .expect("list missing-status root in Kubo")
            .into_iter()
            .find(|entry| entry.name == "delay.txt")
            .expect("missing-status root omitted delay.txt")
            .hash;
        Self {
            _directory: directory,
            cid,
            delay_cid,
            marker,
            route,
        }
    }

    async fn build(
        ipfs: &ww::ipfs::HttpClient,
        status_wasm: &[u8],
        route: String,
        marker: Vec<u8>,
        script: String,
    ) -> Self {
        let directory = tempfile::tempdir().expect("create epoch content root");
        let bin = directory.path().join("bin");
        let init = directory.path().join("etc/init.d");
        std::fs::create_dir_all(&bin).expect("create epoch bin directory");
        std::fs::create_dir_all(&init).expect("create epoch init.d directory");
        std::fs::write(bin.join("status.wasm"), status_wasm).expect("write epoch status component");
        std::fs::write(init.join("05-status.glia"), script.as_bytes())
            .expect("write epoch init script");
        std::fs::write(directory.path().join("delay.txt"), &marker)
            .expect("write replacement delay marker");
        std::fs::write(directory.path().join("generation.txt"), &marker)
            .expect("write epoch generation marker");

        let cid: cid::Cid = ipfs
            .add_dir(directory.path())
            .await
            .expect("add epoch content root to Kubo")
            .parse()
            .expect("Kubo returned a valid epoch root CID");
        let root = format!("/ipfs/{cid}");
        let delay_cid = ipfs
            .ls(&root)
            .await
            .expect("list epoch root in Kubo")
            .into_iter()
            .find(|entry| entry.name == "delay.txt")
            .expect("epoch root omitted delay.txt")
            .hash;
        assert_eq!(
            ipfs.cat(&format!("{root}/bin/status.wasm"))
                .await
                .expect("verify /bin/status.wasm in Kubo"),
            status_wasm,
            "Kubo epoch root changed status.wasm"
        );
        assert_eq!(
            ipfs.cat(&format!("{root}/etc/init.d/05-status.glia"))
                .await
                .expect("verify init script in Kubo"),
            script.as_bytes(),
            "Kubo epoch root changed init script"
        );
        assert_eq!(
            ipfs.cat(&format!("{root}/generation.txt"))
                .await
                .expect("verify generation marker in Kubo"),
            marker,
            "Kubo epoch root changed generation marker"
        );

        Self {
            _directory: directory,
            cid,
            delay_cid,
            marker,
            route,
        }
    }

    fn mount(&self) -> String {
        format!("/ipfs/{}", self.cid)
    }

    fn marker_path(&self) -> String {
        format!("{}/generation.txt", self.mount())
    }
}

fn persistent_identity(home: &Path) -> (SigningKey, PathBuf, PeerId) {
    let signing_key = ww::keys::generate().expect("generate persistent test identity");
    let path = home.join(".ww/identity");
    ww::keys::save(&signing_key, &path).expect("save persistent test identity");
    let peer_id = ww::keys::to_libp2p(&signing_key)
        .expect("convert persistent test identity")
        .public()
        .to_peer_id();
    (signing_key, path, peer_id)
}

fn epoch_node_options(
    _initial: &EpochRoot,
    http_addr: SocketAddr,
    listen_addr: SocketAddr,
    identity_path: PathBuf,
    atom: &AtomFixture,
) -> NodeOptions {
    let mut options = NodeOptions::embedded(Some(http_addr));
    // This non-conflicting user layer must remain present after every epoch.
    options.mounts = vec!["doc".to_owned()];
    options.listen_addr = Some(listen_addr);
    options.identity_path = Some(identity_path);
    options.insecure_ephemeral = false;
    options.tty = false;
    options.stem = Some(StemOptions {
        contract_address: atom.contract_address.clone(),
        rpc_url: atom.rpc_url.clone(),
        ws_url: atom.ws_url.clone(),
    });
    options
}

fn terminal_address(listen_addr: SocketAddr) -> Multiaddr {
    format!("/ip4/127.0.0.1/tcp/{}", listen_addr.port())
        .parse()
        .expect("parse explicit Terminal multiaddr")
}

async fn wait_for_log(node: &mut RunningNode, first: &str, second: &str) {
    let deadline = Instant::now() + INVALIDATION_TIMEOUT;
    loop {
        let logs = node.logs();
        if logs
            .lines()
            .any(|line| line.contains(first) && line.contains(second))
        {
            return;
        }
        if let Some(status) = node.try_wait() {
            panic!("ww exited before log evidence {first:?}/{second:?}: {status}\n{logs}");
        }
        assert!(
            Instant::now() < deadline,
            "timed out waiting for log evidence {first:?}/{second:?}\n{logs}"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

async fn wait_for_not_ready(
    client: &reqwest::Client,
    admin_addr: SocketAddr,
    node: &mut RunningNode,
) -> Value {
    let deadline = Instant::now() + INVALIDATION_TIMEOUT;
    let url = format!("http://{admin_addr}/readyz");
    loop {
        if let Some(status) = node.try_wait() {
            panic!(
                "ww exited before replacement unready state: {status}\n{}",
                node.logs()
            );
        }
        if let Ok(response) = client.get(&url).send().await {
            if response.status() == reqwest::StatusCode::SERVICE_UNAVAILABLE {
                return response.json().await.expect("parse replacement /readyz");
            }
        }
        assert!(
            Instant::now() < deadline,
            "timed out waiting for replacement /readyz=503\n{}",
            node.logs()
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

async fn assert_route_available(client: &reqwest::Client, http_addr: SocketAddr, path: &str) {
    let response = client
        .get(format!("http://{http_addr}{path}"))
        .send()
        .await
        .unwrap_or_else(|error| panic!("query {path}: {error}"));
    assert_eq!(
        response.status(),
        reqwest::StatusCode::OK,
        "expected {path} to be live"
    );
}

async fn assert_route_unavailable(client: &reqwest::Client, http_addr: SocketAddr, path: &str) {
    if let Ok(response) = client.get(format!("http://{http_addr}{path}")).send().await {
        assert_ne!(
            response.status(),
            reqwest::StatusCode::OK,
            "stale or partial route {path} remained live"
        );
    }
}

struct FailedReplacementObservation {
    exit: ExitStatus,
    readiness_samples: usize,
    saw_partial_route_live: bool,
    saw_kernel_not_ready_with_live_route: bool,
    saw_partial_route_removed: bool,
}

async fn wait_for_exit_while_unready(
    client: &reqwest::Client,
    admin_addr: SocketAddr,
    http_addr: SocketAddr,
    stale_route: &str,
    partial_route: &str,
    node: &mut RunningNode,
) -> FailedReplacementObservation {
    let deadline = Instant::now() + EXIT_TIMEOUT;
    let ready_url = format!("http://{admin_addr}/readyz");
    let partial_url = format!("http://{http_addr}{partial_route}");
    let mut readiness_samples = 0;
    let mut saw_partial_route_live = false;
    let mut saw_kernel_not_ready_with_live_route = false;
    let mut saw_partial_route_removed = false;
    loop {
        if let Some(status) = node.try_wait() {
            assert_route_unavailable(client, http_addr, stale_route).await;
            assert_route_unavailable(client, http_addr, partial_route).await;
            if saw_partial_route_live {
                saw_partial_route_removed = true;
            }
            return FailedReplacementObservation {
                exit: status,
                readiness_samples,
                saw_partial_route_live,
                saw_kernel_not_ready_with_live_route,
                saw_partial_route_removed,
            };
        }
        let partial_is_live = client
            .get(&partial_url)
            .send()
            .await
            .is_ok_and(|response| response.status() == reqwest::StatusCode::OK);
        if partial_is_live {
            saw_partial_route_live = true;
        } else if saw_partial_route_live {
            saw_partial_route_removed = true;
        }

        if let Ok(response) = client.get(&ready_url).send().await {
            readiness_samples += 1;
            let status = response.status();
            let body = response
                .text()
                .await
                .expect("read failed-replacement /readyz");
            assert_eq!(
                status,
                reqwest::StatusCode::SERVICE_UNAVAILABLE,
                "readiness recovered during failed replacement: {body}"
            );
            let readiness: Value = serde_json::from_str(&body).unwrap_or_else(|error| {
                panic!("invalid failed-replacement /readyz: {error}; {body}")
            });
            if partial_is_live {
                assert_eq!(
                    readiness["phase"], "kernel-not-ready",
                    "a live partial route must not override the kernel gate: {readiness}"
                );
                saw_kernel_not_ready_with_live_route = true;
            }
        }
        assert_route_unavailable(client, http_addr, stale_route).await;
        assert!(
            Instant::now() < deadline,
            "ww did not exit after failed replacement\n{}",
            node.logs()
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

fn log_position(logs: &str, first: &str, second: &str) -> usize {
    logs.lines()
        .position(|line| line.contains(first) && line.contains(second))
        .unwrap_or_else(|| panic!("missing log evidence {first:?}/{second:?}\n{logs}"))
}

fn count_log_lines(logs: &str, needle: &str) -> usize {
    logs.lines().filter(|line| line.contains(needle)).count()
}

fn count_log_lines_with(logs: &str, first: &str, second: &str) -> usize {
    logs.lines()
        .filter(|line| line.contains(first) && line.contains(second))
        .count()
}

// "registered HTTP route" is a substring of "unregistered HTTP route", so
// route-lifecycle positions need explicit disambiguation.
fn registered_route_positions(logs: &str, route: &str) -> Vec<usize> {
    logs.lines()
        .enumerate()
        .filter(|(_, line)| {
            line.contains("registered HTTP route")
                && !line.contains("unregistered HTTP route")
                && line.contains(route)
        })
        .map(|(index, _)| index)
        .collect()
}

fn unregistered_route_positions(logs: &str, route: &str) -> Vec<usize> {
    logs.lines()
        .enumerate()
        .filter(|(_, line)| line.contains("unregistered HTTP route") && line.contains(route))
        .map(|(index, _)| index)
        .collect()
}

/// An image carrying the production status policy with a missing or corrupt
/// `bin/status.wasm`. Every PID0 implementation must fail initialization.
fn invalid_status_image(status_bytes: Option<&[u8]>) -> tempfile::TempDir {
    let image = tempfile::tempdir().expect("create invalid status image");
    let init = image.path().join("etc/init.d");
    std::fs::create_dir_all(&init).expect("create status policy directory");
    std::fs::copy(
        "std/status/etc/init.d/05-status.glia",
        init.join("05-status.glia"),
    )
    .expect("copy status boot policy");
    if let Some(status_bytes) = status_bytes {
        let bin = image.path().join("bin");
        std::fs::create_dir_all(&bin).expect("create status component directory");
        std::fs::write(bin.join("status.wasm"), status_bytes)
            .expect("write invalid status component");
    }
    image
}

/// Wait for the host to exit while asserting `/readyz` never reports ready.
async fn wait_for_exit_without_readiness(
    client: &reqwest::Client,
    admin_addr: SocketAddr,
    node: &mut RunningNode,
) -> (ExitStatus, usize) {
    let deadline = Instant::now() + EXIT_TIMEOUT;
    let ready_url = format!("http://{admin_addr}/readyz");
    let mut samples = 0;
    loop {
        if let Ok(response) = client.get(&ready_url).send().await {
            samples += 1;
            let status = response.status();
            let body = response.text().await.expect("read failing /readyz body");
            assert_eq!(
                status,
                reqwest::StatusCode::SERVICE_UNAVAILABLE,
                "readiness opened during failed initialization: {body}"
            );
        }
        if let Some(exit) = node.try_wait() {
            return (exit, samples);
        }
        assert!(
            Instant::now() < deadline,
            "ww did not exit after initialization failure\n{}",
            node.logs()
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

async fn assert_unready_and_old_generation_dead(
    client: &reqwest::Client,
    admin_addr: SocketAddr,
    http_addr: SocketAddr,
    node: &mut RunningNode,
) {
    assert!(
        node.try_wait().is_none(),
        "daemon exited during Host retry\n{}",
        node.logs()
    );
    let response = client
        .get(format!("http://{admin_addr}/readyz"))
        .send()
        .await
        .expect("read readiness during Host retry");
    assert_eq!(response.status(), reqwest::StatusCode::SERVICE_UNAVAILABLE);
    assert_route_unavailable(client, http_addr, "/status").await;
}

async fn kubo_pin_is_present(
    client: &reqwest::Client,
    kubo_addr: SocketAddr,
    cid: &cid::Cid,
) -> bool {
    client
        .post(format!(
            "http://{kubo_addr}/api/v0/pin/ls?arg={cid}&type=recursive"
        ))
        .send()
        .await
        .is_ok_and(|response| response.status().is_success())
}

async fn wait_for_pin_release(client: &reqwest::Client, kubo_addr: SocketAddr, cid: &cid::Cid) {
    let deadline = Instant::now() + INVALIDATION_TIMEOUT;
    while kubo_pin_is_present(client, kubo_addr, cid).await {
        assert!(Instant::now() < deadline, "pin remained present for {cid}");
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

/// The kernel-independent behavioral contract. Every driver runs against the
/// embedded Rust PID0 and the explicit `file:` Glia PID0 through named
/// wrappers. Drivers assert host-observable behavior (admin plane, data
/// plane, host lifecycle events, exit codes) plus the shared
/// `INITIAL_INIT_FAILED` failure token. They must not assert same-instance
/// re-graft behavior, guest generation counters, per-kernel log wording, or
/// anything reachable only through the temporary `/ww/0.1.0` surface.
mod shared_parity {
    use super::*;

    async fn boot_and_tty_driver(kernel: KernelUnderTest) {
        let _guard = e2e_lock().await;
        let status_wasm = required_artifact(STATUS_WASM_PATH);
        assert!(
            !Path::new(STATUS_LAYER).join("bin/main.wasm").exists(),
            "status layer must not shadow the selected pid0 artifact"
        );
        let (kubo_addr, client) = require_kubo().await;
        let admin_addr = unused_addr().await;
        let http_addr = unused_addr().await;
        let proxy_listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind delayed Kubo proxy");
        let proxy_addr = proxy_listener.local_addr().expect("read proxy address");
        let home = tempfile::tempdir().expect("create isolated HOME");
        let mut options = NodeOptions::embedded(Some(http_addr));
        let (kernel_wasm, kernel_source) = kernel.select(&mut options);
        let mut node = RunningNode::spawn(home.path(), admin_addr, proxy_addr, &options);

        let health_url = format!("http://{admin_addr}/healthz");
        let ready_url = format!("http://{admin_addr}/readyz");
        wait_for_http(&client, &health_url, &mut node).await;

        // The proxy is bound but deliberately not accepting yet, holding boot
        // at the existing Kubo gate while the already-running admin plane
        // exposes the real not-ready phase and the pending kernel identity.
        let initial = wait_for_phase(&client, &ready_url, "waiting-for-kubo", &mut node).await;
        assert_eq!(initial["ready"], false);
        assert_eq!(initial["phase"], "waiting-for-kubo");
        let pending = version(&client, admin_addr).await;
        assert_eq!(pending["kernel_cid"], Value::Null);
        assert_eq!(
            pending["kernel_source"],
            format!("<pending: {kernel_source}>")
        );

        let proxy_task = start_kubo_proxy(proxy_listener, kubo_addr);
        let ready = wait_for_ready(&client, &ready_url, &mut node).await;
        assert_eq!(ready["ready"], true);
        assert_eq!(ready["phase"], "ready");

        let identity = version(&client, admin_addr).await;
        assert_eq!(identity["kernel_source"], kernel_source);
        assert_eq!(
            identity["kernel_cid"],
            ww::kernel::runtime_cid(&kernel_wasm).to_string()
        );
        assert_eq!(
            identity["kernel_wasm_blake3"],
            blake3::hash(&kernel_wasm).to_hex().to_string(),
            "the host must report the exact selected pid0 artifact"
        );
        assert_status_cell(&client, http_addr, &ww::kernel::runtime_cid(&status_wasm)).await;

        // Stdin EOF ends the interactive PID0 with exit 0, and the host
        // propagates that exact code.
        node.close_stdin();
        let exit = node.wait(EXIT_TIMEOUT).await;
        proxy_task.abort();
        let logs = node.logs();
        assert_eq!(exit.code(), Some(0), "unexpected host exit\n{logs}");
        assert!(
            logs.contains("Kernel exited") && logs.contains("code=0"),
            "host did not report the propagated pid0 exit code\n{logs}"
        );
    }

    async fn initial_failure_driver(kernel: KernelUnderTest) {
        let _guard = e2e_lock().await;
        let (kubo_addr, client) = require_kubo().await;
        for status_bytes in [None, Some(b"corrupt status component".as_slice())] {
            let image = invalid_status_image(status_bytes);
            let admin_addr = unused_addr().await;
            let http_addr = unused_addr().await;
            let home = tempfile::tempdir().expect("create isolated HOME");
            let mut options = NodeOptions::embedded(Some(http_addr));
            options.mounts = vec![image.path().display().to_string()];
            kernel.select(&mut options);
            let mut node = RunningNode::spawn(home.path(), admin_addr, kubo_addr, &options);
            let (exit, _samples) =
                wait_for_exit_without_readiness(&client, admin_addr, &mut node).await;
            let logs = node.logs();
            assert_eq!(
                exit.code(),
                Some(1),
                "status init failure must fail the host\n{logs}"
            );
            assert!(
                logs.contains("event_code=1"),
                "the host must classify the initial failure as authoritative\n{logs}"
            );
            assert!(
                logs.contains("INITIAL_INIT_FAILED"),
                "the guest must fail initialization before committing readiness\n{logs}"
            );
            assert_route_unavailable(&client, http_addr, "/status").await;
        }
    }

    async fn epoch_replacement_driver(kernel: KernelUnderTest) {
        let _guard = e2e_lock().await;
        let status_wasm = required_artifact(STATUS_WASM_PATH);
        let (variant_a, cid_a) = status_variant(&status_wasm, b"variant-a");
        let (variant_b, cid_b) = status_variant(&status_wasm, b"variant-b");
        assert_ne!(cid_a, cid_b);
        let (kubo_addr, client) = require_kubo().await;
        let ipfs = ww::ipfs::HttpClient::new(format!("http://{kubo_addr}"));
        let epoch1 = EpochRoot::valid(&ipfs, &variant_a, 1).await;
        let epoch2 = EpochRoot::valid(&ipfs, &variant_b, 2).await;
        let atom = AtomFixture::start(Path::new(env!("CARGO_MANIFEST_DIR"))).await;
        let first = atom.set_head(&epoch1.cid).await;

        let admin_addr = unused_addr().await;
        let http_addr = unused_addr().await;
        let listen_addr = unused_addr().await;
        let home = tempfile::tempdir().expect("create isolated epoch HOME");
        let (_signing_key, identity_path, _peer_id) = persistent_identity(home.path());
        let mut options = epoch_node_options(&epoch1, http_addr, listen_addr, identity_path, &atom);
        kernel.select(&mut options);
        let mut node = RunningNode::spawn(home.path(), admin_addr, kubo_addr, &options);
        let ready_url = format!("http://{admin_addr}/readyz");

        wait_for_ready(&client, &ready_url, &mut node).await;
        assert_status_cell(&client, http_addr, &cid_a).await;

        let second = atom.set_head(&epoch2.cid).await;
        assert!(
            second.block_number > first.block_number,
            "Atom head updates must be mined in order"
        );
        wait_for_log(&mut node, "Advancing epoch", "seq=2").await;
        let replacing = wait_for_not_ready(&client, admin_addr, &mut node).await;
        assert_eq!(replacing["ready"], false);
        assert_eq!(
            replacing["phase"], "kernel-not-ready",
            "epoch advance must close readiness before replacement commit: {replacing}"
        );
        // Old serving state must be gone: while readiness is closed, `/status`
        // either has no live route or already serves the replacement bytes.
        if let Ok(response) = client
            .get(format!("http://{http_addr}/status"))
            .send()
            .await
        {
            if response.status() == reqwest::StatusCode::OK {
                let cell = response
                    .headers()
                    .get("X-Wetware-Cell")
                    .and_then(|value| value.to_str().ok())
                    .unwrap_or_default()
                    .to_string();
                assert_eq!(
                    cell,
                    cid_b.to_string(),
                    "old generation served during replacement"
                );
            }
        }

        let recovered = wait_for_ready(&client, &ready_url, &mut node).await;
        assert_eq!(recovered["phase"], "ready");
        assert_status_cell(&client, http_addr, &cid_b).await;
        wait_for_pin_release(&client, kubo_addr, &epoch1.cid).await;
        assert!(
            node.try_wait().is_none(),
            "daemon exited after successful replacement\n{}",
            node.logs()
        );

        let logs = node.logs();
        assert_eq!(
            count_log_lines(&logs, "event_code=3"),
            1,
            "one head update must replace exactly one PID0 generation\n{logs}"
        );
        let registered = registered_route_positions(&logs, "/status");
        let unregistered = unregistered_route_positions(&logs, "/status");
        assert_eq!(
            registered.len(),
            2,
            "boot and replacement must each register /status once\n{logs}"
        );
        assert_eq!(
            unregistered.len(),
            1,
            "only the old generation's /status must unregister\n{logs}"
        );
        assert!(
            registered[0] < unregistered[0] && unregistered[0] < registered[1],
            "old-generation teardown must precede replacement activation\n{logs}"
        );
    }

    async fn epoch_burst_driver(kernel: KernelUnderTest) {
        let _guard = e2e_lock().await;
        let status_wasm = required_artifact(STATUS_WASM_PATH);
        let (variant_a, cid_a) = status_variant(&status_wasm, b"variant-a");
        let (variant_b, _cid_b) = status_variant(&status_wasm, b"variant-b");
        let (variant_c, cid_c) = status_variant(&status_wasm, b"variant-c");
        let (kubo_addr, client) = require_kubo().await;
        let ipfs = ww::ipfs::HttpClient::new(format!("http://{kubo_addr}"));
        let epoch1 = EpochRoot::valid(&ipfs, &variant_a, 1).await;
        let epoch2 = EpochRoot::valid(&ipfs, &variant_b, 2).await;
        let epoch3 = EpochRoot::valid(&ipfs, &variant_c, 3).await;
        let atom = AtomFixture::start(Path::new(env!("CARGO_MANIFEST_DIR"))).await;
        let first = atom.set_head(&epoch1.cid).await;

        let admin_addr = unused_addr().await;
        let http_addr = unused_addr().await;
        let listen_addr = unused_addr().await;
        let home = tempfile::tempdir().expect("create isolated burst HOME");
        let (_signing_key, identity_path, _peer_id) = persistent_identity(home.path());
        let mut options = epoch_node_options(&epoch1, http_addr, listen_addr, identity_path, &atom);
        kernel.select(&mut options);
        let mut node = RunningNode::spawn(home.path(), admin_addr, kubo_addr, &options);
        let ready_url = format!("http://{admin_addr}/readyz");
        wait_for_ready(&client, &ready_url, &mut node).await;
        assert_status_cell(&client, http_addr, &cid_a).await;

        // Finalizer canonicality: update 2 must be observed before update 3 is
        // mined, or the contract's head would correctly supersede update 2.
        let second = atom.set_head(&epoch2.cid).await;
        wait_for_log(&mut node, "Advancing epoch", "seq=2").await;
        let third = atom.set_head(&epoch3.cid).await;
        wait_for_log(&mut node, "Advancing epoch", "seq=3").await;
        assert!(first.block_number < second.block_number);
        assert!(second.block_number < third.block_number);

        wait_for_not_ready(&client, admin_addr, &mut node).await;
        wait_for_ready(&client, &ready_url, &mut node).await;
        assert_status_cell(&client, http_addr, &cid_c).await;
        assert!(
            node.try_wait().is_none(),
            "daemon exited during rapid updates\n{}",
            node.logs()
        );

        let logs = node.logs();
        let seq2 = log_position(&logs, "Advancing epoch", "seq=2");
        let seq3 = log_position(&logs, "Advancing epoch", "seq=3");
        assert!(seq2 < seq3, "host advances must be ordered\n{logs}");
        // Convergence contract: the final generation serves. A coherent
        // superseded intermediate generation may transiently activate, so one
        // or two replacements are legitimate outcomes.
        let replacements = count_log_lines(&logs, "event_code=3");
        assert!(
            (1..=2).contains(&replacements),
            "rapid updates must converge in one or two coherent replacements\n{logs}"
        );
        // A superseded generation may be terminated before it registers, so
        // route registrations range from 2 (boot + final) up to one per
        // replacement plus boot. Every registration except the final one must
        // be torn down.
        let registered = registered_route_positions(&logs, "/status");
        let unregistered = unregistered_route_positions(&logs, "/status");
        assert!(
            (2..=1 + replacements).contains(&registered.len()),
            "boot and the final generation must each register /status\n{logs}"
        );
        assert_eq!(
            unregistered.len(),
            registered.len() - 1,
            "every superseded /status registration must be torn down exactly once\n{logs}"
        );
        assert!(
            unregistered.iter().max() < registered.iter().max(),
            "the final generation's route must outlive every teardown\n{logs}"
        );
    }

    async fn replacement_failure_driver(kernel: KernelUnderTest) {
        let _guard = e2e_lock().await;
        let status_wasm = required_artifact(STATUS_WASM_PATH);
        let (variant_a, cid_a) = status_variant(&status_wasm, b"variant-a");
        let (kubo_addr, client) = require_kubo().await;
        let ipfs = ww::ipfs::HttpClient::new(format!("http://{kubo_addr}"));
        let epoch1 = EpochRoot::valid(&ipfs, &variant_a, 1).await;
        let invalid = EpochRoot::missing_status(&ipfs, 2).await;
        let atom = AtomFixture::start(Path::new(env!("CARGO_MANIFEST_DIR"))).await;
        atom.set_head(&epoch1.cid).await;

        let admin_addr = unused_addr().await;
        let http_addr = unused_addr().await;
        let listen_addr = unused_addr().await;
        let home = tempfile::tempdir().expect("create isolated failure HOME");
        let (_signing_key, identity_path, _peer_id) = persistent_identity(home.path());
        let mut options = epoch_node_options(&epoch1, http_addr, listen_addr, identity_path, &atom);
        kernel.select(&mut options);
        // Delay host fetches of the invalid root so the closed-readiness
        // window between the epoch advance and the authoritative failure is
        // reliably observable.
        let proxy_listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind replacement-delay Kubo proxy");
        let proxy_addr = proxy_listener.local_addr().expect("read proxy address");
        let proxy_task =
            start_delayed_kubo_proxy(proxy_listener, kubo_addr, invalid.cid.to_string());
        let mut node = RunningNode::spawn(home.path(), admin_addr, proxy_addr, &options);
        let ready_url = format!("http://{admin_addr}/readyz");
        wait_for_ready(&client, &ready_url, &mut node).await;
        assert_status_cell(&client, http_addr, &cid_a).await;

        atom.set_head(&invalid.cid).await;
        wait_for_log(&mut node, "Advancing epoch", "seq=2").await;
        let unready = wait_for_not_ready(&client, admin_addr, &mut node).await;
        assert_eq!(unready["ready"], false);
        let (exit, samples) = wait_for_exit_without_readiness(&client, admin_addr, &mut node).await;
        proxy_task.abort();
        let logs = node.logs();
        assert_eq!(
            exit.code(),
            Some(1),
            "authoritative replacement failure must fail the daemon\n{logs}"
        );
        assert!(
            samples > 0,
            "failed replacement exited without any readiness observations\n{logs}"
        );
        assert!(
            logs.contains("event_code=2"),
            "the host must classify the replacement failure as authoritative\n{logs}"
        );
        assert!(
            logs.contains("INITIAL_INIT_FAILED"),
            "the fresh replacement instance must fail before committing readiness\n{logs}"
        );
        assert_route_unavailable(&client, http_addr, "/status").await;
        assert!(
            !logs
                .lines()
                .any(|line| line.contains("Kernel exited") && line.contains("code=0")),
            "failed replacement followed the accidental success path\n{logs}"
        );
    }

    async fn interactive_replacement_driver(kernel: KernelUnderTest) {
        let _guard = e2e_lock().await;
        let status_wasm = required_artifact(STATUS_WASM_PATH);
        let (kubo_addr, client) = require_kubo().await;
        let ipfs = ww::ipfs::HttpClient::new(format!("http://{kubo_addr}"));
        let epoch1 = EpochRoot::valid(&ipfs, &status_wasm, 1).await;
        let epoch2 = EpochRoot::valid(&ipfs, &status_wasm, 2).await;
        let atom = AtomFixture::start(Path::new(env!("CARGO_MANIFEST_DIR"))).await;
        atom.set_head(&epoch1.cid).await;

        let admin_addr = unused_addr().await;
        let http_addr = unused_addr().await;
        let listen_addr = unused_addr().await;
        let home = tempfile::tempdir().expect("create isolated interactive epoch HOME");
        let (_signing_key, identity_path, _peer_id) = persistent_identity(home.path());
        let mut options = epoch_node_options(&epoch1, http_addr, listen_addr, identity_path, &atom);
        options.tty = true;
        kernel.select(&mut options);
        let mut node = RunningNode::spawn(home.path(), admin_addr, kubo_addr, &options);
        wait_for_ready(&client, &format!("http://{admin_addr}/readyz"), &mut node).await;

        atom.set_head(&epoch2.cid).await;
        let exit = node.wait(EXIT_TIMEOUT).await;
        let logs = node.logs();
        assert_eq!(
            exit.code(),
            Some(0),
            "interactive replacement must end the invocation cleanly\n{logs}"
        );
        assert_eq!(count_log_lines(&logs, "event_code=5"), 1, "{logs}");
        assert_eq!(count_log_lines(&logs, "event_code=3"), 0, "{logs}");
        assert_eq!(count_log_lines(&logs, "event_code=2"), 0, "{logs}");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn glia_pid0_boot_and_tty_parity() {
        boot_and_tty_driver(EXPLICIT_GLIA).await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn rust_pid0_boot_and_tty_parity() {
        boot_and_tty_driver(EMBEDDED_RUST).await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn glia_pid0_initial_failure_parity() {
        initial_failure_driver(EXPLICIT_GLIA).await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn rust_pid0_initial_failure_parity() {
        initial_failure_driver(EMBEDDED_RUST).await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn glia_pid0_replacement_parity() {
        tokio::task::LocalSet::new()
            .run_until(epoch_replacement_driver(EXPLICIT_GLIA))
            .await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn rust_pid0_replacement_parity() {
        tokio::task::LocalSet::new()
            .run_until(epoch_replacement_driver(EMBEDDED_RUST))
            .await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn glia_pid0_rapid_replacement_parity() {
        tokio::task::LocalSet::new()
            .run_until(epoch_burst_driver(EXPLICIT_GLIA))
            .await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn rust_pid0_rapid_replacement_parity() {
        tokio::task::LocalSet::new()
            .run_until(epoch_burst_driver(EMBEDDED_RUST))
            .await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn glia_pid0_replacement_failure_parity() {
        tokio::task::LocalSet::new()
            .run_until(replacement_failure_driver(EXPLICIT_GLIA))
            .await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn rust_pid0_replacement_failure_parity() {
        tokio::task::LocalSet::new()
            .run_until(replacement_failure_driver(EMBEDDED_RUST))
            .await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn glia_pid0_interactive_replacement_parity() {
        tokio::task::LocalSet::new()
            .run_until(interactive_replacement_driver(EXPLICIT_GLIA))
            .await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn rust_pid0_interactive_replacement_parity() {
        tokio::task::LocalSet::new()
            .run_until(interactive_replacement_driver(EMBEDDED_RUST))
            .await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn local_kernel_path_reaches_ready_and_cli_overrides_env() {
    let _guard = e2e_lock().await;
    let embedded_kernel = required_artifact(KERNEL_WASM_PATH);
    required_artifact(STATUS_WASM_PATH);
    let embedded_cid = ww::kernel::runtime_cid(&embedded_kernel);
    let mut selected_kernel = embedded_kernel.clone();
    append_custom_section(
        &mut selected_kernel,
        b"ww.test.distinct-kernel",
        b"pr2-path-source",
    );
    let selected_cid = ww::kernel::runtime_cid(&selected_kernel);
    assert_ne!(
        selected_cid, embedded_cid,
        "test fixture must differ from the embedded kernel"
    );
    let selected_file = tempfile::NamedTempFile::new().expect("create distinct pid0 component");
    std::fs::write(selected_file.path(), &selected_kernel).expect("write distinct pid0 component");
    let kernel_path = selected_file
        .path()
        .canonicalize()
        .expect("canonicalize distinct pid0 component");
    let (kubo_addr, client) = require_kubo().await;
    let admin_addr = unused_addr().await;
    let http_addr = unused_addr().await;
    let home = tempfile::tempdir().expect("create isolated HOME");
    let mut options = NodeOptions::embedded(Some(http_addr));
    options.kernel_cli = Some(format!("file:{}", kernel_path.display()));
    options.kernel_env = Some("file:/definitely/missing/env-kernel.wasm".to_string());
    let mut node = RunningNode::spawn(home.path(), admin_addr, kubo_addr, &options);

    let ready = wait_for_ready(&client, &format!("http://{admin_addr}/readyz"), &mut node).await;
    assert_eq!(ready["ready"], true);
    let identity = version(&client, admin_addr).await;
    assert_eq!(
        identity["kernel_source"],
        format!("file:{}", kernel_path.display())
    );
    assert_eq!(identity["kernel_cid"], selected_cid.to_string());
    assert_ne!(identity["kernel_cid"], embedded_cid.to_string());
    assert_eq!(
        identity["kernel_wasm_blake3"],
        blake3::hash(&selected_kernel).to_hex().to_string()
    );
    assert_status_route(&client, http_addr).await;

    node.close_stdin();
    let exit = node.wait(EXIT_TIMEOUT).await;
    let logs = node.logs();
    assert_eq!(exit.code(), Some(0), "unexpected host exit\n{logs}");
    assert!(
        logs.contains("Kernel source resolved")
            && logs.contains(&kernel_path.display().to_string()),
        "kernel resolution logs must identify the selected path\n{logs}"
    );
    assert!(
        !logs.contains("kernel_source=embedded:main"),
        "explicit path selection must not fall back to embedded pid0\n{logs}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn env_kernel_path_overrides_embedded_and_no_http_reaches_ready() {
    let _guard = e2e_lock().await;
    let kernel_wasm = required_artifact(KERNEL_WASM_PATH);
    required_artifact(STATUS_WASM_PATH);
    let kernel_path = Path::new(KERNEL_WASM_PATH)
        .canonicalize()
        .expect("canonicalize kernel artifact");
    let (kubo_addr, client) = require_kubo().await;
    let admin_addr = unused_addr().await;
    let home = tempfile::tempdir().expect("create isolated HOME");
    let mut options = NodeOptions::embedded(None);
    options.kernel_env = Some(format!("file:{}", kernel_path.display()));
    let mut node = RunningNode::spawn(home.path(), admin_addr, kubo_addr, &options);

    let ready = wait_for_ready(&client, &format!("http://{admin_addr}/readyz"), &mut node).await;
    assert_eq!(ready["ready"], true);
    let identity = version(&client, admin_addr).await;
    assert_eq!(
        identity["kernel_source"],
        format!("file:{}", kernel_path.display())
    );
    assert_eq!(
        identity["kernel_cid"],
        ww::kernel::runtime_cid(&kernel_wasm).to_string()
    );

    node.close_stdin();
    let exit = node.wait(EXIT_TIMEOUT).await;
    assert_eq!(
        exit.code(),
        Some(0),
        "unexpected host exit\n{}",
        node.logs()
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn kubo_cid_kernel_reaches_ready_and_reports_source_and_runtime_cids() {
    let _guard = e2e_lock().await;
    let kernel_wasm = required_artifact(KERNEL_WASM_PATH);
    required_artifact(STATUS_WASM_PATH);
    let (kubo_addr, client) = require_kubo().await;
    let source_cid = ww::ipfs::HttpClient::new(format!("http://{kubo_addr}"))
        .add_bytes(&kernel_wasm)
        .await
        .expect("add pid0 bytes to the CI-pinned Kubo");
    let admin_addr = unused_addr().await;
    let http_addr = unused_addr().await;
    let home = tempfile::tempdir().expect("create isolated HOME");
    let mut options = NodeOptions::embedded(Some(http_addr));
    options.kernel_cli = Some(format!("cid:{source_cid}"));
    let mut node = RunningNode::spawn(home.path(), admin_addr, kubo_addr, &options);

    let ready = wait_for_ready(&client, &format!("http://{admin_addr}/readyz"), &mut node).await;
    assert_eq!(ready["ready"], true);
    let identity = version(&client, admin_addr).await;
    assert_eq!(identity["kernel_source"], format!("cid:{source_cid}"));
    assert_eq!(identity["kernel_source_cid"], source_cid);
    assert_eq!(
        identity["kernel_cid"],
        ww::kernel::runtime_cid(&kernel_wasm).to_string()
    );
    assert_status_route(&client, http_addr).await;

    node.close_stdin();
    let exit = node.wait(EXIT_TIMEOUT).await;
    assert_eq!(
        exit.code(),
        Some(0),
        "unexpected host exit\n{}",
        node.logs()
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn incompatible_path_selected_component_fails_clearly() {
    let _guard = e2e_lock().await;
    required_artifact(STATUS_WASM_PATH);
    let (kubo_addr, _client) = require_kubo().await;
    let bad_kernel = tempfile::NamedTempFile::new().expect("create invalid pid0 component");
    std::fs::write(bad_kernel.path(), b"not a WebAssembly component")
        .expect("write invalid pid0 component");
    let admin_addr = unused_addr().await;
    let home = tempfile::tempdir().expect("create isolated HOME");
    let mut options = NodeOptions::embedded(None);
    options.kernel_cli = Some(format!("file:{}", bad_kernel.path().display()));
    let mut node = RunningNode::spawn(home.path(), admin_addr, kubo_addr, &options);

    let exit = node.wait(EXIT_TIMEOUT).await;
    let logs = node.logs();
    assert_eq!(
        exit.code(),
        Some(1),
        "invalid pid0 must fail closed\n{logs}"
    );
    assert!(
        logs.contains("failed to parse WebAssembly module")
            || logs.contains("failed to compile")
            || logs.contains("magic header"),
        "failure must name component compilation/parsing\n{logs}"
    );
    assert!(
        !logs.contains("kernel_source=embedded:main"),
        "explicit invalid path must not fall back to embedded pid0\n{logs}"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn host_transient_epoch_preparation_recovers_without_restoring_old_generation() {
    tokio::task::LocalSet::new()
        .run_until(async {
            let _guard = e2e_lock().await;
            let status_wasm = required_artifact(STATUS_WASM_PATH);
            let (variant_a, cid_a) = status_variant(&status_wasm, b"transient-a");
            let (variant_b, cid_b) = status_variant(&status_wasm, b"transient-b");
            let (kubo_addr, client) = require_kubo().await;
            let ipfs = ww::ipfs::HttpClient::new(format!("http://{kubo_addr}"));
            let epoch1 = EpochRoot::valid(&ipfs, &variant_a, 1).await;
            let epoch2 = EpochRoot::valid(&ipfs, &variant_b, 2).await;
            let atom = AtomFixture::start(Path::new(env!("CARGO_MANIFEST_DIR"))).await;
            atom.set_head(&epoch1.cid).await;

            let proxy_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let proxy_addr = proxy_listener.local_addr().unwrap();
            let (proxy_task, attempts, release_tx) =
                start_failing_merge_kubo_proxy(proxy_listener, kubo_addr, epoch2.cid.to_string());
            let admin_addr = unused_addr().await;
            let http_addr = unused_addr().await;
            let listen_addr = unused_addr().await;
            let home = tempfile::tempdir().unwrap();
            let (signing_key, identity_path, peer_id) = persistent_identity(home.path());
            let options = epoch_node_options(&epoch1, http_addr, listen_addr, identity_path, &atom);
            let mut node = RunningNode::spawn(home.path(), admin_addr, proxy_addr, &options);
            let ready_url = format!("http://{admin_addr}/readyz");
            wait_for_ready(&client, &ready_url, &mut node).await;
            assert_status_cell(&client, http_addr, &cid_a).await;
            let terminal =
                TerminalSession::connect(peer_id, terminal_address(listen_addr), &signing_key)
                    .await;
            let retained_old_host = terminal.graft.host.clone();

            atom.set_head(&epoch2.cid).await;
            wait_for_log(&mut node, "Advancing epoch", "seq=2").await;
            expect_stale_or_disconnected(&retained_old_host).await;
            let deadline = Instant::now() + INVALIDATION_TIMEOUT;
            while attempts.load(Ordering::SeqCst) < 2 {
                assert_unready_and_old_generation_dead(&client, admin_addr, http_addr, &mut node)
                    .await;
                assert!(
                    Instant::now() < deadline,
                    "Host did not retry twice\n{}",
                    node.logs()
                );
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
            assert_unready_and_old_generation_dead(&client, admin_addr, http_addr, &mut node).await;

            release_tx.send(true).unwrap();
            wait_for_ready(&client, &ready_url, &mut node).await;
            assert_status_cell(&client, http_addr, &cid_b).await;
            let logs = node.logs();
            assert!(
                count_log_lines(&logs, "Transient epoch preparation failure") >= 2,
                "retry logs missing\n{logs}"
            );
            assert!(
                node.try_wait().is_none(),
                "daemon exited after recovery\n{logs}"
            );
            proxy_task.abort();
        })
        .await;
}

#[tokio::test(flavor = "current_thread")]
async fn host_epoch_preparation_supersession_activates_only_latest_target() {
    tokio::task::LocalSet::new()
        .run_until(async {
            let _guard = e2e_lock().await;
            let status_wasm = required_artifact(STATUS_WASM_PATH);
            let (variant_a, cid_a) = status_variant(&status_wasm, b"supersession-a");
            let (variant_b, _cid_b) = status_variant(&status_wasm, b"supersession-b");
            let (variant_c, cid_c) = status_variant(&status_wasm, b"supersession-c");
            let (kubo_addr, client) = require_kubo().await;
            let ipfs = ww::ipfs::HttpClient::new(format!("http://{kubo_addr}"));
            let epoch1 = EpochRoot::valid(&ipfs, &variant_a, 1).await;
            let epoch2 = EpochRoot::valid(&ipfs, &variant_b, 2).await;
            let epoch3 = EpochRoot::valid(&ipfs, &variant_c, 3).await;
            let atom = AtomFixture::start(Path::new(env!("CARGO_MANIFEST_DIR"))).await;
            atom.set_head(&epoch1.cid).await;

            let proxy_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let proxy_addr = proxy_listener.local_addr().unwrap();
            let (proxy_task, attempts, _release_tx) =
                start_failing_merge_kubo_proxy(proxy_listener, kubo_addr, epoch2.cid.to_string());
            let admin_addr = unused_addr().await;
            let http_addr = unused_addr().await;
            let listen_addr = unused_addr().await;
            let home = tempfile::tempdir().unwrap();
            let (_, identity_path, _) = persistent_identity(home.path());
            let options = epoch_node_options(&epoch1, http_addr, listen_addr, identity_path, &atom);
            let mut node = RunningNode::spawn(home.path(), admin_addr, proxy_addr, &options);
            let ready_url = format!("http://{admin_addr}/readyz");
            wait_for_ready(&client, &ready_url, &mut node).await;
            assert_status_cell(&client, http_addr, &cid_a).await;

            atom.set_head(&epoch2.cid).await;
            wait_for_log(&mut node, "Advancing epoch", "seq=2").await;
            let retry_deadline = Instant::now() + INVALIDATION_TIMEOUT;
            while attempts.load(Ordering::SeqCst) < 2 {
                assert_unready_and_old_generation_dead(&client, admin_addr, http_addr, &mut node)
                    .await;
                assert!(Instant::now() < retry_deadline, "Host did not enter retry");
                tokio::time::sleep(Duration::from_millis(50)).await;
            }

            atom.set_head(&epoch3.cid).await;
            wait_for_log(&mut node, "Advancing epoch", "seq=3").await;
            wait_for_ready(&client, &ready_url, &mut node).await;
            assert_status_cell(&client, http_addr, &cid_c).await;
            wait_for_pin_release(&client, kubo_addr, &epoch2.cid).await;
            let logs = node.logs();
            assert_eq!(
                count_log_lines_with(&logs, "Effective epoch root is ready", "seq=2"),
                0,
                "superseded epoch activated\n{logs}"
            );
            assert!(
                node.try_wait().is_none(),
                "daemon exited after supersession\n{logs}"
            );
            proxy_task.abort();
        })
        .await;
}

#[tokio::test(flavor = "current_thread")]
async fn superseded_pending_generation_converges_to_the_newer_epoch() {
    tokio::task::LocalSet::new()
        .run_until(async {
            let _guard = e2e_lock().await;
            let status_wasm = required_artifact(STATUS_WASM_PATH);
            let (variant_a, _cid_a) = status_variant(&status_wasm, b"result-race-a");
            let (variant_b, _cid_b) = status_variant(&status_wasm, b"result-race-b");
            let (variant_c, cid_c) = status_variant(&status_wasm, b"result-race-c");
            let (kubo_addr, client) = require_kubo().await;
            let ipfs = ww::ipfs::HttpClient::new(format!("http://{kubo_addr}"));
            let epoch1 = EpochRoot::valid(&ipfs, &variant_a, 1).await;
            let pending = EpochRoot::valid(&ipfs, &variant_b, 2).await;
            let epoch3 = EpochRoot::valid(&ipfs, &variant_c, 3).await;
            let pending_status_cid = ipfs
                .ls(&format!("/ipfs/{}/bin", pending.cid))
                .await
                .expect("list pending-generation bin directory")
                .into_iter()
                .find(|entry| entry.name == "status.wasm")
                .expect("pending generation omitted status.wasm")
                .hash;
            let atom = AtomFixture::start(Path::new(env!("CARGO_MANIFEST_DIR"))).await;
            atom.set_head(&epoch1.cid).await;

            let admin_addr = unused_addr().await;
            let http_addr = unused_addr().await;
            let listen_addr = unused_addr().await;
            let home = tempfile::tempdir().expect("create isolated supersede HOME");
            let (_signing_key, identity_path, _peer_id) = persistent_identity(home.path());
            let mut options =
                epoch_node_options(&epoch1, http_addr, listen_addr, identity_path, &atom);
            let proxy_listener = TcpListener::bind("127.0.0.1:0")
                .await
                .expect("bind sentinel-gated Kubo proxy");
            let proxy_addr = proxy_listener.local_addr().expect("read proxy address");
            let result_race_listener = TcpListener::bind("127.0.0.1:0")
                .await
                .expect("bind PID0 result race barrier");
            options.pid0_result_race = Some((2, result_race_listener.local_addr().unwrap()));
            let (proxy_task, mut reached_rx, release_tx) =
                start_gated_kubo_proxy(proxy_listener, kubo_addr, pending_status_cid);
            let mut node = RunningNode::spawn(home.path(), admin_addr, proxy_addr, &options);
            let ready_url = format!("http://{admin_addr}/readyz");
            wait_for_ready(&client, &ready_url, &mut node).await;

            atom.set_head(&pending.cid).await;
            wait_for_log(&mut node, "Advancing epoch", "seq=2").await;
            tokio::time::timeout(INVALIDATION_TIMEOUT, reached_rx.recv())
                .await
                .expect("pending generation did not reach its sentinel")
                .expect("sentinel channel closed before pending generation arrived");

            let convergence_started = Instant::now();
            atom.set_head(&epoch3.cid).await;
            let (mut epoch_barrier, _) =
                tokio::time::timeout(INVALIDATION_TIMEOUT, result_race_listener.accept())
                    .await
                    .expect("host did not observe the superseding epoch")
                    .expect("accept host epoch barrier");
            let mut marker = [0_u8; 1];
            epoch_barrier
                .read_exact(&mut marker)
                .await
                .expect("read host epoch marker");
            assert_eq!(marker, *b"E");
            wait_for_log(&mut node, "Advancing epoch", "seq=3").await;
            release_tx
                .send(true)
                .expect("release pending generation sentinel");
            let (mut result_ready, _) =
                tokio::time::timeout(INVALIDATION_TIMEOUT, result_race_listener.accept())
                    .await
                    .expect("superseded PID0 did not produce a result")
                    .expect("accept PID0 result-ready marker");
            result_ready
                .read_exact(&mut marker)
                .await
                .expect("read PID0 result-ready marker");
            assert_eq!(marker, *b"R");
            epoch_barrier
                .write_all(b"C")
                .await
                .expect("release host epoch barrier");
            wait_for_ready(&client, &ready_url, &mut node).await;
            assert!(
                convergence_started.elapsed() < BOOT_TIMEOUT,
                "superseded generation did not converge within the test bound\n{}",
                node.logs()
            );
            assert_status_cell(&client, http_addr, &cid_c).await;
            let logs = node.logs();
            assert!(
                !logs.contains("event_code=2"),
                "superseded generation emitted an authoritative failure\n{logs}"
            );
            assert!(
                logs.contains("event_code=4")
                    && logs.contains("Kernel result superseded by newer generation"),
                "superseded generation did not exit through result classification\n{logs}"
            );
            assert!(
                node.try_wait().is_none(),
                "daemon exited after supersede\n{logs}"
            );

            proxy_task.abort();
        })
        .await;
}

/// Coverage that rides the temporary `/ww/0.1.0` guest-membrane surface:
/// Terminal grafting, semantic probes, and stale-capability delivery to a
/// remote holder. This module shrinks or disappears when that surface is
/// retired with the Glia shell/MCP; deleting it must not weaken
/// `shared_parity`, which never touches `/ww/0.1.0`.
mod ww_protocol_compat {
    use super::*;

    #[tokio::test(flavor = "current_thread")]
    async fn real_atom_epoch_transition_replaces_explicit_glia_pid0() {
        let local = tokio::task::LocalSet::new();
        local
            .run_until(async {
                let _guard = e2e_lock().await;
                let status_wasm = required_artifact(STATUS_WASM_PATH);
                let (kubo_addr, client) = require_kubo().await;
                let ipfs = ww::ipfs::HttpClient::new(format!("http://{kubo_addr}"));
                let epoch1 = EpochRoot::valid(&ipfs, &status_wasm, 1).await;
                let epoch2 = EpochRoot::valid(&ipfs, &status_wasm, 2).await;
                let atom = AtomFixture::start(Path::new(env!("CARGO_MANIFEST_DIR"))).await;
                let initial_receipt = atom.set_head(&epoch1.cid).await;
                assert!(!initial_receipt.hash.is_empty());
                assert_eq!(initial_receipt.transaction_index, 0);

                let admin_addr = unused_addr().await;
                let http_addr = unused_addr().await;
                let listen_addr = unused_addr().await;
                let home = tempfile::tempdir().expect("create isolated epoch HOME");
                let (signing_key, identity_path, peer_id) = persistent_identity(home.path());
                let mut options =
                    epoch_node_options(&epoch1, http_addr, listen_addr, identity_path, &atom);
                let (kernel_wasm, kernel_source) = EXPLICIT_GLIA.select(&mut options);
                let mut node = RunningNode::spawn(home.path(), admin_addr, kubo_addr, &options);
                let ready_url = format!("http://{admin_addr}/readyz");
                let ready = wait_for_ready(&client, &ready_url, &mut node).await;
                assert_eq!(ready["phase"], "ready");
                let identity = version(&client, admin_addr).await;
                assert_eq!(identity["kernel_source"], kernel_source);
                assert_eq!(
                    identity["kernel_cid"],
                    ww::kernel::runtime_cid(&kernel_wasm).to_string()
                );
                assert_status_route(&client, http_addr).await;
                assert_route_available(&client, http_addr, &epoch1.route).await;
                assert_route_unavailable(&client, http_addr, &epoch2.route).await;

                let terminal =
                    TerminalSession::connect(peer_id, terminal_address(listen_addr), &signing_key)
                        .await;
                terminal
                    .graft
                    .semantic_probes(
                        &signing_key,
                        &status_wasm,
                        &epoch1.marker_path(),
                        &epoch1.marker,
                    )
                    .await;
                let retained_old_host = terminal.graft.host.clone();

                let replacement_receipt = atom.set_head(&epoch2.cid).await;
                assert!(
                    replacement_receipt.block_number > initial_receipt.block_number,
                    "Atom head updates must be mined in order"
                );
                wait_for_log(&mut node, "Advancing epoch", "seq=2").await;
                let replacing = wait_for_not_ready(&client, admin_addr, &mut node).await;
                assert_eq!(replacing["ready"], false);
                assert_eq!(
                    replacing["phase"], "kernel-not-ready",
                    "epoch advance must close readiness before replacement commit: {replacing}"
                );
                assert_route_unavailable(&client, http_addr, &epoch1.route).await;
                assert_route_unavailable(&client, http_addr, &epoch2.route).await;
                expect_stale_or_disconnected(&retained_old_host).await;

                let recovered = wait_for_ready(&client, &ready_url, &mut node).await;
                assert_eq!(recovered["phase"], "ready");
                assert_status_route(&client, http_addr).await;
                assert_route_unavailable(&client, http_addr, &epoch1.route).await;
                assert_route_available(&client, http_addr, &epoch2.route).await;

                let fresh_terminal =
                    TerminalSession::connect(peer_id, terminal_address(listen_addr), &signing_key)
                        .await;
                fresh_terminal
                    .graft
                    .semantic_probes(
                        &signing_key,
                        &status_wasm,
                        &epoch2.marker_path(),
                        &epoch2.marker,
                    )
                    .await;

                let logs = node.logs();
                assert_eq!(
                    count_log_lines(&logs, "event_code=3"),
                    1,
                    "one Atom epoch transition must replace one PID0 generation\n{logs}"
                );
                let old_unregistered =
                    log_position(&logs, "unregistered HTTP route", &epoch1.route);
                let replacement_registered =
                    log_position(&logs, "registered HTTP route", &epoch2.route);
                assert!(
                    old_unregistered < replacement_registered,
                    "old-generation teardown must precede replacement activation\n{logs}"
                );
                assert_eq!(
                    count_log_lines_with(&logs, "registered HTTP route", &epoch2.route),
                    1,
                    "exactly one replacement generation route must register\n{logs}"
                );
                assert_eq!(
                    count_log_lines_with(&logs, "unregistered HTTP route", &epoch2.route),
                    0,
                    "the committed replacement route must remain live\n{logs}"
                );
            })
            .await;
    }
}

/// Glia-implementation-specific behavior retained during coexistence: the
/// interactive REPL and SysV best-effort init.d choreography (including
/// partial-route teardown during a failed replacement, which requires a
/// multi-form init script only the Glia kernel can execute). This module is
/// deleted with the Glia kernel.
mod glia_specific {
    use super::*;

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn tty_enters_repl_before_eof() {
        let _guard = e2e_lock().await;
        let status_wasm = required_artifact(STATUS_WASM_PATH);
        let (kubo_addr, client) = require_kubo().await;
        let admin_addr = unused_addr().await;
        let http_addr = unused_addr().await;
        let home = tempfile::tempdir().expect("create isolated HOME");
        let mut options = NodeOptions::embedded(Some(http_addr));
        EXPLICIT_GLIA.select(&mut options);
        let mut node = RunningNode::spawn(home.path(), admin_addr, kubo_addr, &options);
        wait_for_ready(&client, &format!("http://{admin_addr}/readyz"), &mut node).await;
        assert_status_cell(&client, http_addr, &ww::kernel::runtime_cid(&status_wasm)).await;
        node.close_stdin();
        let exit = node.wait(EXIT_TIMEOUT).await;
        let logs = node.logs();
        assert_eq!(exit.code(), Some(0), "unexpected host exit\n{logs}");
        assert!(
            logs.contains("\u{276f}"),
            "Glia pid0 never entered its TTY REPL\n{logs}"
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn replacement_init_failure_exits_nonzero_without_stale_or_partial_routes() {
        let local = tokio::task::LocalSet::new();
        local
            .run_until(async {
                let _guard = e2e_lock().await;
                let status_wasm = required_artifact(STATUS_WASM_PATH);
                let (kubo_addr, client) = require_kubo().await;
                let ipfs = ww::ipfs::HttpClient::new(format!("http://{kubo_addr}"));
                let epoch1 = EpochRoot::valid(&ipfs, &status_wasm, 1).await;
                let invalid = EpochRoot::failing(&ipfs, &status_wasm).await;
                let atom = AtomFixture::start(Path::new(env!("CARGO_MANIFEST_DIR"))).await;
                atom.set_head(&epoch1.cid).await;

                let admin_addr = unused_addr().await;
                let http_addr = unused_addr().await;
                let listen_addr = unused_addr().await;
                let home = tempfile::tempdir().expect("create isolated init-failure HOME");
                let (signing_key, identity_path, peer_id) = persistent_identity(home.path());
                let mut options =
                    epoch_node_options(&epoch1, http_addr, listen_addr, identity_path, &atom);
                EXPLICIT_GLIA.select(&mut options);
                let proxy_listener = TcpListener::bind("127.0.0.1:0")
                    .await
                    .expect("bind replacement-delay Kubo proxy");
                let proxy_addr = proxy_listener.local_addr().expect("read proxy address");
                let proxy_task =
                    start_delayed_kubo_proxy(proxy_listener, kubo_addr, invalid.delay_cid.clone());
                let mut node = RunningNode::spawn(home.path(), admin_addr, proxy_addr, &options);
                wait_for_ready(&client, &format!("http://{admin_addr}/readyz"), &mut node).await;
                assert_status_route(&client, http_addr).await;
                assert_route_available(&client, http_addr, &epoch1.route).await;
                let terminal =
                    TerminalSession::connect(peer_id, terminal_address(listen_addr), &signing_key)
                        .await;
                let retained_old_host = terminal.graft.host.clone();

                atom.set_head(&invalid.cid).await;
                wait_for_log(&mut node, "Advancing epoch", "seq=2").await;
                let initial_unready = wait_for_not_ready(&client, admin_addr, &mut node).await;
                assert_eq!(initial_unready["ready"], false);
                let (observation, ()) = tokio::join!(
                    wait_for_exit_while_unready(
                        &client,
                        admin_addr,
                        http_addr,
                        &epoch1.route,
                        &invalid.route,
                        &mut node,
                    ),
                    expect_stale_or_disconnected(&retained_old_host),
                );
                let logs = node.logs();
                assert_eq!(
                    observation.exit.code(),
                    Some(1),
                    "replacement init failure must propagate nonzero\n{logs}"
                );
                assert!(
                    observation.readiness_samples > 0,
                    "failed replacement exited without any readiness observations\n{logs}"
                );
                assert!(
                    observation.saw_partial_route_live,
                    "failure fixture registered but never preflighted a live partial route\n{logs}"
                );
                assert!(
                    observation.saw_kernel_not_ready_with_live_route,
                    "a live partial route overrode authoritative kernel readiness\n{logs}"
                );
                assert!(
                    observation.saw_partial_route_removed,
                    "partial replacement route was not removed after failure\n{logs}"
                );
                assert!(
                    logs.contains("event_code=2"),
                    "replacement failure omitted the host lifecycle event\n{logs}"
                );
                assert!(
                    logs.lines().any(|line| {
                        line.contains("registered HTTP route") && line.contains(&invalid.route)
                    }),
                    "failure fixture never exercised partial replacement registration\n{logs}"
                );
                let old_unregistered =
                    log_position(&logs, "unregistered HTTP route", &epoch1.route);
                let partial_registered =
                    log_position(&logs, "registered HTTP route", &invalid.route);
                let partial_unregistered =
                    log_position(&logs, "unregistered HTTP route", &invalid.route);
                assert!(
                    old_unregistered < partial_registered
                        && partial_registered < partial_unregistered,
                    "old and partial registration teardown ordering changed\n{logs}"
                );
                assert!(
                    !logs
                        .lines()
                        .any(|line| line.contains("Kernel exited") && line.contains("code=0")),
                    "failed replacement followed the accidental success path\n{logs}"
                );
                assert!(
                    !logs.contains("Transient epoch preparation failure"),
                    "Host classified a PID0-owned failure for retry\n{logs}"
                );
                proxy_task.abort();
            })
            .await;
    }
}
