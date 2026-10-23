//! Kernel-independent PID0 lifecycle coverage.
//!
//! These tests launch the real `ww` binary and assert the Rust PID0 behavioral
//! contract: boot/serving identity, host-owned generation replacement with
//! replacement-byte identity, initialization failure, and TTY process lifetime.
//!
//! CI must run `make std` before compiling this test. Missing artifacts are a
//! hard failure: silently skipping would recreate the false-green gap this
//! baseline exists to close.

mod support;

use std::io::{Read, Write};
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, ExitStatus, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use ed25519_dalek::SigningKey;
use libp2p::PeerId;
use serde_json::Value;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::watch;

use axum::body::{to_bytes, Body};
use axum::extract::{Request, State};
use axum::http::{header, Response, StatusCode};
use axum::Router;

use support::atom::AtomFixture;

const KERNEL_WASM_PATH: &str = "std/kernel/bin/main.wasm";
const STATUS_WASM_PATH: &str = "std/status/bin/status.wasm";
const STATUS_LAYER: &str = "./std/status";
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
/// the bytecode CID without changing behavior, so `X-Wetware-Cell` distinguishes
/// the status bytecode served by each test generation, not its deployment root.
fn status_variant(base: &[u8], tag: &[u8]) -> (Vec<u8>, cid::Cid) {
    let mut bytes = base.to_vec();
    append_custom_section(&mut bytes, b"ww.test.epoch", tag);
    let cid = ww::kernel::runtime_cid(&bytes);
    (bytes, cid)
}

/// Change an executed guest log literal without changing component lengths.
/// A custom section changes identity but cannot prove which PID0 executed.
fn kernel_variant(tag: char) -> Vec<u8> {
    let mut bytes = required_artifact(KERNEL_WASM_PATH);
    let original = b"committed readiness";
    let marker = format!("kernel-{tag} readiness!");
    assert_eq!(marker.len(), original.len());
    let positions: Vec<_> = bytes
        .windows(original.len())
        .enumerate()
        .filter_map(|(index, value)| (value == original).then_some(index))
        .collect();
    assert_eq!(
        positions.len(),
        1,
        "kernel readiness literal must be unique"
    );
    bytes[positions[0]..positions[0] + original.len()].copy_from_slice(marker.as_bytes());
    bytes
}

fn seed_installed_kernel(home: &Path) {
    let boot = home.join(".ww/fhs/boot");
    std::fs::create_dir_all(&boot).expect("create installed boot directory");
    std::fs::write(
        boot.join("kernel.wasm"),
        required_artifact(KERNEL_WASM_PATH),
    )
    .expect("seed installed kernel");
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
    installed_baseline: bool,
    cwd: Option<PathBuf>,
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
    fn installed(http_addr: Option<SocketAddr>) -> Self {
        Self {
            mounts: vec![STATUS_LAYER.to_string()],
            installed_baseline: true,
            cwd: None,
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
        // macOS exposes /var through a symlink. The installed-source contract
        // accepts physical directories, so isolate HOME at its physical path.
        let home = home.canonicalize().expect("canonicalize isolated HOME");
        if options.installed_baseline {
            seed_installed_kernel(&home);
        }
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
        if let Some(cwd) = &options.cwd {
            command.current_dir(cwd);
        }
        command.env_remove("WW_FHS_ROOT");
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
                .args(["--confirmation-depth", "0"]);
        }
        if let Some((seq, address)) = options.pid0_result_race {
            command.env("WW_TEST_PID0_RESULT_RACE", format!("{seq}@{address}"));
        } else {
            command.env_remove("WW_TEST_PID0_RESULT_RACE");
        }

        command
            .env("HOME", &home)
            // Lifecycle assertions parse compact fields without ANSI escapes.
            .env("WW_LOG_FORMAT", "compact")
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

    fn write_stdin(&mut self, bytes: &[u8]) {
        let stdin = self.stdin.as_mut().expect("child stdin is open");
        stdin.write_all(bytes).expect("write child stdin");
        stdin.flush().expect("flush child stdin");
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

async fn assert_kernel_epoch(
    client: &reqwest::Client,
    admin_addr: SocketAddr,
    kubo_addr: SocketAddr,
    node: &RunningNode,
    seq: u64,
    kernel: &[u8],
) -> Value {
    let root = activated_root(node, seq);
    let source = immutable_path_cid(client, kubo_addr, &root, "boot/kernel.wasm").await;
    let identity = version(client, admin_addr).await;
    assert_eq!(identity["kernel_epoch_seq"], seq);
    assert_eq!(identity["kernel_effective_root"], root.to_string());
    // Kubo resolve canonicalizes DAG-PB CIDv0 links to CIDv1. The selector
    // retains the exact source link, so compare their codec and multihash.
    let selected: cid::Cid = identity["kernel_source_cid"]
        .as_str()
        .unwrap()
        .parse()
        .unwrap();
    assert_eq!(selected.codec(), source.codec());
    assert_eq!(selected.hash(), source.hash());
    assert_eq!(
        identity["kernel_source"],
        format!("/ipfs/{root}/boot/kernel.wasm")
    );
    assert_eq!(
        identity["kernel_cid"],
        ww::kernel::runtime_cid(kernel).to_string()
    );
    assert_eq!(
        identity["kernel_wasm_blake3"],
        blake3::hash(kernel).to_hex().to_string()
    );
    let bytes = kubo_call(
        client,
        kubo_addr,
        "cat",
        &[("arg", &format!("/ipfs/{source}"))],
    )
    .await
    .bytes()
    .await
    .unwrap();
    assert_eq!(
        bytes.as_ref(),
        kernel,
        "selected file must contain the executing kernel bytes"
    );
    identity
}

async fn assert_persistent_peer(client: &reqwest::Client, http_addr: SocketAddr, peer: PeerId) {
    let status: Value = client
        .get(format!("http://{http_addr}/status"))
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(
        status["peer_id"],
        peer.to_string(),
        "kernel replacement changed Host PeerId"
    );
}

async fn assert_status_route(client: &reqwest::Client, http_addr: SocketAddr, node: &RunningNode) {
    let response = client
        .get(format!("http://{http_addr}/status"))
        .timeout(Duration::from_secs(20))
        .send()
        .await
        .expect("query Rust PID0 /status route");
    let response_status = response.status();
    let body = response.text().await.expect("read /status body");
    assert_eq!(
        response_status,
        reqwest::StatusCode::OK,
        "/status failed; body={body:?}\n{}",
        node.logs()
    );
    let status: Value = serde_json::from_str(&body).unwrap_or_else(|error| {
        panic!(
            "parse /status JSON: {error}; body={body:?}\n{}",
            node.logs()
        )
    });
    assert_eq!(status["status"], "ok");
    assert!(
        status["peer_id"]
            .as_str()
            .is_some_and(|peer| !peer.is_empty()),
        "real status cell must receive its narrow Membrane: {status}"
    );
}

async fn assert_status_cell(
    client: &reqwest::Client,
    http_addr: SocketAddr,
    expected_cell_cid: &cid::Cid,
    node: &RunningNode,
) {
    let response = client
        .get(format!("http://{http_addr}/status"))
        .timeout(Duration::from_secs(20))
        .send()
        .await
        .expect("query Rust PID0 /status route");
    let response_status = response.status();
    let observed_cell_cid = response
        .headers()
        .get("X-Wetware-Cell")
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned);
    let body = response.text().await.expect("read /status body");
    assert_eq!(
        response_status,
        reqwest::StatusCode::OK,
        "/status failed; body={body:?}\n{}",
        node.logs()
    );
    let expected_cell_cid = expected_cell_cid.to_string();
    assert_eq!(
        observed_cell_cid.as_deref(),
        Some(expected_cell_cid.as_str()),
        "/status response had an invalid X-Wetware-Cell header; body={body:?}\n{}",
        node.logs()
    );
    let status: Value = serde_json::from_str(&body).unwrap_or_else(|error| {
        panic!(
            "parse /status JSON: {error}; body={body:?}\n{}",
            node.logs()
        )
    });
    assert_eq!(status["status"], "ok");
    assert!(
        status["peer_id"]
            .as_str()
            .is_some_and(|peer| !peer.is_empty()),
        "real status cell must receive its narrow Membrane: {status}"
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
        block: bool,
    },
    CorruptBlock {
        attempts: Arc<AtomicUsize>,
    },
    FailMergeUntil {
        attempts: Arc<AtomicUsize>,
        release: watch::Receiver<bool>,
        status: StatusCode,
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
            // Composition reads DAG-PB file headers through block/get before
            // PID0 starts. Gate the later content read to preserve this test's
            // PID0-result race boundary.
            ProxyControl::Gate {
                reached,
                release,
                block,
            } if uri.contains(if *block {
                "/api/v0/block/get"
            } else {
                "/api/v0/cat"
            }) =>
            {
                let _ = reached.send(()).await;
                while !*release.borrow() {
                    if release.changed().await.is_err() {
                        break;
                    }
                }
            }
            ProxyControl::Gate { .. } => {}
            ProxyControl::CorruptBlock { attempts } if uri.contains("/api/v0/block/get") => {
                attempts.fetch_add(1, Ordering::SeqCst);
                return Response::builder()
                    .status(StatusCode::OK)
                    .body(Body::from("corrupted selected kernel block"))
                    .unwrap();
            }
            ProxyControl::CorruptBlock { .. } => {}
            ProxyControl::FailMergeUntil {
                attempts,
                release,
                status,
            } if uri.contains("/api/v0/block/get") && !*release.borrow() => {
                attempts.fetch_add(1, Ordering::SeqCst);
                return Response::builder()
                    .status(*status)
                    .body(Body::from(if *status == StatusCode::INTERNAL_SERVER_ERROR {
                        r#"{"Message":"block was not found locally (offline)","Code":0,"Type":"error"}"#
                    } else {
                        "content is not available yet"
                    }))
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
    start_gated_kubo_proxy_for(listener, target, path_fragment, false)
}

fn start_gated_kubo_proxy_for(
    listener: TcpListener,
    target: SocketAddr,
    path_fragment: impl Into<String>,
    block: bool,
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
            block,
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

fn start_corrupt_block_kubo_proxy(
    listener: TcpListener,
    target: SocketAddr,
    path_fragment: String,
) -> (tokio::task::JoinHandle<()>, Arc<AtomicUsize>) {
    let attempts = Arc::new(AtomicUsize::new(0));
    let proxy = ControlledKuboProxy {
        client: reqwest::Client::new(),
        target,
        path_fragment,
        control: ProxyControl::CorruptBlock {
            attempts: Arc::clone(&attempts),
        },
    };
    let app = Router::new()
        .fallback(forward_controlled_kubo)
        .with_state(proxy);
    (
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        }),
        attempts,
    )
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
    start_failing_block_kubo_proxy(listener, target, head_cid, StatusCode::SERVICE_UNAVAILABLE)
}

fn start_failing_block_kubo_proxy(
    listener: TcpListener,
    target: SocketAddr,
    head_cid: impl Into<String>,
    status: StatusCode,
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
            status,
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
}

impl EpochRoot {
    async fn valid(ipfs: &ww::ipfs::HttpClient, status_wasm: &[u8], generation: u64) -> Self {
        Self::with_kernel(
            ipfs,
            status_wasm,
            &required_artifact(KERNEL_WASM_PATH),
            generation,
        )
        .await
    }

    async fn with_kernel(
        ipfs: &ww::ipfs::HttpClient,
        status_wasm: &[u8],
        kernel: &[u8],
        generation: u64,
    ) -> Self {
        let marker = format!("epoch-{generation}\n").into_bytes();
        Self::build(ipfs, status_wasm, kernel, marker).await
    }

    /// A deployment root whose production status policy requires
    /// `bin/status.wasm` while the tree does not contain it. Every PID0
    /// implementation must fail composition against this root.
    async fn missing_status(ipfs: &ww::ipfs::HttpClient, generation: u64) -> Self {
        let marker = format!("epoch-{generation}-missing-status\n").into_bytes();
        let directory = tempfile::tempdir().expect("create missing-status root");
        std::fs::create_dir(directory.path().join("boot")).unwrap();
        std::fs::write(
            directory.path().join("boot/kernel.wasm"),
            required_artifact(KERNEL_WASM_PATH),
        )
        .unwrap();
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
        Self {
            _directory: directory,
            cid,
        }
    }

    async fn build(
        ipfs: &ww::ipfs::HttpClient,
        status_wasm: &[u8],
        kernel: &[u8],
        marker: Vec<u8>,
    ) -> Self {
        let directory = tempfile::tempdir().expect("create epoch content root");
        std::fs::create_dir(directory.path().join("boot")).unwrap();
        std::fs::write(directory.path().join("boot/kernel.wasm"), kernel).unwrap();
        let bin = directory.path().join("bin");
        std::fs::create_dir_all(&bin).expect("create epoch bin directory");
        std::fs::write(bin.join("status.wasm"), status_wasm).expect("write epoch status component");
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
        assert_eq!(
            ipfs.cat(&format!("{root}/bin/status.wasm"))
                .await
                .expect("verify /bin/status.wasm in Kubo"),
            status_wasm,
            "Kubo epoch root changed status.wasm"
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
        }
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
    let mut options = NodeOptions::installed(Some(http_addr));
    // This non-conflicting user layer must remain present after every epoch.
    options.mounts = vec!["./doc".to_owned()];
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

async fn assert_route_unavailable(client: &reqwest::Client, http_addr: SocketAddr, path: &str) {
    if let Ok(response) = client.get(format!("http://{http_addr}{path}")).send().await {
        assert_ne!(
            response.status(),
            reqwest::StatusCode::OK,
            "stale or partial route {path} remained live"
        );
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

/// An image with a missing or corrupt `bin/status.wasm`.
fn invalid_status_image(status_bytes: Option<&[u8]>) -> tempfile::TempDir {
    let image = tempfile::tempdir().expect("create invalid status image");
    std::fs::write(
        image.path().join("invalid-status.txt"),
        b"invalid status fixture\n",
    )
    .expect("write invalid status fixture marker");
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
    let identity = version(client, admin_addr).await;
    for field in [
        "kernel_cid",
        "kernel_source_cid",
        "kernel_epoch_seq",
        "kernel_effective_root",
    ] {
        assert_eq!(
            identity[field],
            Value::Null,
            "unrooted epoch reported {field}"
        );
    }
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

/// Compute the local-file snapshot CID without creating an independent pin.
async fn unuploaded_file_cid(client: &reqwest::Client, kubo: SocketAddr, bytes: &[u8]) -> cid::Cid {
    let result: Value = client
        .post(format!("http://{kubo}/api/v0/add"))
        .query(&[
            ("only-hash", "true"),
            ("pin", "false"),
            ("cid-version", "1"),
            ("hash", "sha2-256"),
            ("raw-leaves", "true"),
            ("chunker", "size-262144"),
            ("trickle", "false"),
            ("wrap-with-directory", "false"),
            ("inline", "false"),
            ("preserve-mode", "false"),
            ("preserve-mtime", "false"),
        ])
        .multipart(reqwest::multipart::Form::new().part(
            "file",
            reqwest::multipart::Part::bytes(bytes.to_vec()).file_name("source"),
        ))
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    result["Hash"].as_str().unwrap().parse().unwrap()
}

async fn checked_gc(client: &reqwest::Client, kubo: SocketAddr) {
    let body = kubo_call(client, kubo, "repo/gc", &[])
        .await
        .text()
        .await
        .unwrap();
    for line in body.lines().filter(|line| !line.is_empty()) {
        let event: Value = serde_json::from_str(line).expect("valid Kubo GC event");
        assert!(event.get("Error").is_none(), "Kubo GC failed: {event}");
    }
}

async fn wait_for_pin_release(client: &reqwest::Client, kubo_addr: SocketAddr, cid: &cid::Cid) {
    let deadline = Instant::now() + INVALIDATION_TIMEOUT;
    while kubo_pin_is_present(client, kubo_addr, cid).await {
        assert!(Instant::now() < deadline, "pin remained present for {cid}");
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

async fn kubo_call(
    client: &reqwest::Client,
    kubo_addr: SocketAddr,
    method: &str,
    query: &[(&str, &str)],
) -> reqwest::Response {
    client
        .post(format!("http://{kubo_addr}/api/v0/{method}"))
        .query(query)
        .timeout(Duration::from_secs(30))
        .send()
        .await
        .unwrap_or_else(|error| panic!("Kubo {method}: {error}"))
        .error_for_status()
        .unwrap_or_else(|error| panic!("Kubo {method}: {error}"))
}

async fn immutable_path_cid(
    client: &reqwest::Client,
    kubo_addr: SocketAddr,
    root: &cid::Cid,
    path: &str,
) -> cid::Cid {
    let path = format!("/ipfs/{root}/{path}");
    let value: Value = kubo_call(client, kubo_addr, "resolve", &[("arg", &path)])
        .await
        .json()
        .await
        .expect("resolve immutable test path");
    value["Path"]
        .as_str()
        .expect("resolved path")
        .strip_prefix("/ipfs/")
        .expect("immutable resolved path")
        .parse()
        .expect("resolved CID")
}

/// Find the data block containing the executed marker. Composition can
/// inspect the file header first; only verified file reads need this leaf.
async fn selected_marker_block(
    client: &reqwest::Client,
    kubo: SocketAddr,
    root: &cid::Cid,
    marker: &str,
) -> cid::Cid {
    let file = immutable_path_cid(client, kubo, root, "boot/kernel.wasm").await;
    let mut pending = vec![file];
    while let Some(block) = pending.pop() {
        let bytes = kubo_call(client, kubo, "block/get", &[("arg", &block.to_string())])
            .await
            .bytes()
            .await
            .unwrap();
        if bytes
            .windows(marker.len())
            .any(|value| value == marker.as_bytes())
        {
            return block;
        }
        if block.codec() == 0x70 {
            let dag: Value = kubo_call(client, kubo, "dag/get", &[("arg", &block.to_string())])
                .await
                .json()
                .await
                .unwrap();
            for link in dag["Links"].as_array().unwrap() {
                pending.push(link["Hash"]["/"].as_str().unwrap().parse().unwrap());
            }
        }
    }
    panic!("kernel marker has no isolated data leaf: {marker}");
}

async fn directory_data(
    client: &reqwest::Client,
    kubo_addr: SocketAddr,
    root: &cid::Cid,
    path: &str,
) -> Value {
    let path = if path.is_empty() {
        format!("/ipfs/{root}")
    } else {
        format!("/ipfs/{root}/{path}")
    };
    let value: Value = kubo_call(client, kubo_addr, "dag/get", &[("arg", &path)])
        .await
        .json()
        .await
        .expect("read directory DAG data");
    assert!(!value["Data"].is_null(), "directory data missing: {value}");
    value["Data"].clone()
}

fn activated_root(node: &RunningNode, seq: u64) -> cid::Cid {
    let logs = node.logs();
    let sequence = format!("seq={seq}");
    let line = logs
        .lines()
        .find(|line| {
            (line.contains("deployment root activated")
                || line.contains("Deployment root activated"))
                && line.split_whitespace().any(|field| field == sequence)
        })
        .unwrap_or_else(|| panic!("missing activated epoch {seq}\n{logs}"));
    line.split_whitespace()
        .find_map(|field| field.strip_prefix("root="))
        .expect("activation must report the exact effective root CID")
        .parse()
        .expect("effective root CID")
}

/// Build independent authoritative roots with metadata that changes per head.
/// The MFS path exists only inside the isolated test backend; it is removed
/// after acquiring a recursive pin on the resulting immutable root.
async fn targeted_epoch_root(
    ipfs: &ww::ipfs::HttpClient,
    client: &reqwest::Client,
    kubo_addr: SocketAddr,
    status: &[u8],
    generation: u64,
    obstruct: bool,
) -> EpochRoot {
    let kernel = kernel_variant(if generation == 10 { 'A' } else { 'B' });
    let mut root = EpochRoot::with_kernel(ipfs, status, &kernel, generation).await;
    let directory = root._directory.path();
    if obstruct {
        std::fs::write(directory.join("etc"), b"strict ancestor obstruction").unwrap();
    } else {
        std::fs::create_dir_all(directory.join("etc/app")).unwrap();
        std::fs::write(
            directory.join("etc/app/config.toml"),
            b"authoritative conflict",
        )
        .unwrap();
        std::fs::write(directory.join("etc/app/unrelated"), generation.to_string()).unwrap();
        std::fs::create_dir_all(directory.join("etc/overlay")).unwrap();
        std::fs::write(directory.join("etc/overlay/overlap"), b"head overlap").unwrap();
        std::fs::write(
            directory.join("etc/overlay/unrelated"),
            generation.to_string(),
        )
        .unwrap();
    }
    let imported = ipfs.add_dir(directory).await.unwrap();
    let mfs = format!("/landing-c-{}-{generation}", std::process::id());
    let source = format!("/ipfs/{imported}");
    kubo_call(
        client,
        kubo_addr,
        "files/cp",
        &[("arg", &source), ("arg", &mfs)],
    )
    .await;
    if !obstruct {
        for (suffix, mode) in [
            ("/etc/overlay", "705"),
            ("/etc/app", "700"),
            ("/etc", "750"),
            ("", "755"),
        ] {
            let path = format!("{mfs}{suffix}");
            kubo_call(
                client,
                kubo_addr,
                "files/chmod",
                &[("arg", mode), ("arg", &path)],
            )
            .await;
            let mtime = (1000 + generation).to_string();
            kubo_call(
                client,
                kubo_addr,
                "files/touch",
                &[("arg", &path), ("mtime", &mtime)],
            )
            .await;
        }
    }
    let stat: Value = kubo_call(client, kubo_addr, "files/stat", &[("arg", &mfs)])
        .await
        .json()
        .await
        .unwrap();
    let cid = stat["Hash"].as_str().unwrap();
    kubo_call(client, kubo_addr, "pin/add", &[("arg", cid)]).await;
    root.cid = cid.parse().unwrap();
    kubo_call(
        client,
        kubo_addr,
        "files/rm",
        &[("arg", &mfs), ("recursive", "true")],
    )
    .await;
    root
}

#[tokio::test(flavor = "current_thread")]
async fn frozen_cli_mounts_survive_real_stem_transition_obstruction_recovery_and_gc() {
    tokio::task::LocalSet::new().run_until(async {
        let _guard = e2e_lock().await;
        let status = required_artifact(STATUS_WASM_PATH);
        let kernel = kernel_variant('X');
        let (overlay_status, overlay_status_cid) = status_variant(&status, b"frozen-cli-overlay");
        let (kubo_addr, client) = require_kubo().await;
        let backend_version: Value = kubo_call(&client, kubo_addr, "version", &[]).await.json().await.unwrap();
        assert_eq!(backend_version["Version"], "0.33.0", "required versioned backend");
        let ipfs = ww::ipfs::HttpClient::new(format!("http://{kubo_addr}"));
        let h0 = targeted_epoch_root(&ipfs, &client, kubo_addr, &status, 10, false).await;
        let h1 = targeted_epoch_root(&ipfs, &client, kubo_addr, &status, 11, false).await;
        let obstructed = targeted_epoch_root(&ipfs, &client, kubo_addr, &status, 12, true).await;
        let recovered = targeted_epoch_root(&ipfs, &client, kubo_addr, &status, 13, false).await;
        let atom = AtomFixture::start(Path::new(env!("CARGO_MANIFEST_DIR"))).await;
        atom.set_head(&h0.cid).await;
        let sources = tempfile::tempdir().unwrap();
        // macOS temporary paths can contain a symlink alias. CLI capture must
        // receive the explicit physical source path required by its contract.
        let source = sources.path().canonicalize().unwrap();
        let hidden_payload = format!("hidden frozen source remains retained: {}", source.display());
        let hidden_bytes = hidden_payload.as_bytes();
        std::fs::write(source.join("hidden"), hidden_bytes).unwrap();
        // Hash-only requests neither upload nor pin the hidden source. The
        // deployment is therefore the only owner that can retain this CID.
        let hidden_cid = unuploaded_file_cid(&client, kubo_addr, hidden_bytes).await;
        assert!(!kubo_pin_is_present(&client, kubo_addr, &hidden_cid).await, "hidden source must have no preexisting owner");
        std::fs::write(source.join("config"), b"original frozen config").unwrap();
        std::fs::write(source.join("status.wasm"), &overlay_status).unwrap();
        std::fs::write(source.join("kernel.wasm"), &kernel).unwrap();
        std::fs::create_dir_all(source.join("directory/empty")).unwrap();
        std::fs::write(source.join("directory/.gitignore"), b"targeted inclusion").unwrap();
        std::fs::write(source.join("directory/overlap"), b"frozen directory overlap").unwrap();
        // Local captures omit metadata. An explicit immutable directory mount
        // supplies distinguishable target mode/mtime for the Composer contract.
        let overlay_mfs = format!("/landing3-overlay-{}", std::process::id());
        kubo_call(&client, kubo_addr, "files/mkdir", &[("arg", &format!("{overlay_mfs}/empty")), ("parents", "true")]).await;
        for name in [".gitignore", "overlap"] {
            client.post(format!("http://{kubo_addr}/api/v0/files/write"))
                .query(&[("arg", format!("{overlay_mfs}/{name}")), ("create", "true".to_owned())])
                .multipart(reqwest::multipart::Form::new().part("file", reqwest::multipart::Part::bytes(std::fs::read(source.join("directory").join(name)).unwrap()).file_name(name.to_owned())))
                .send().await.unwrap().error_for_status().unwrap();
        }
        kubo_call(&client, kubo_addr, "files/chmod", &[("arg", "711"), ("arg", &overlay_mfs)]).await;
        kubo_call(&client, kubo_addr, "files/touch", &[("arg", &overlay_mfs), ("mtime", "4242")]).await;
        let overlay_stat: Value = kubo_call(&client, kubo_addr, "files/stat", &[("arg", &overlay_mfs)]).await.json().await.unwrap();
        let overlay_cid: cid::Cid = overlay_stat["Hash"].as_str().unwrap().parse().unwrap();
        kubo_call(&client, kubo_addr, "pin/add", &[("arg", &overlay_cid.to_string())]).await;
        kubo_call(&client, kubo_addr, "files/rm", &[("arg", &overlay_mfs), ("recursive", "true")]).await;
        let overlay_metadata = directory_data(&client, kubo_addr, &overlay_cid, "").await;
        assert_ne!(overlay_metadata, directory_data(&client, kubo_addr, &h0.cid, "etc/overlay").await);
        let proxy_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let proxy_addr = proxy_listener.local_addr().unwrap();
        let (proxy_task, mut replacement_reached, replacement_release) =
            start_gated_kubo_proxy_for(proxy_listener, kubo_addr, h1.cid.to_string(), true);
        let home = tempfile::tempdir().unwrap();
        let (_, identity_path, peer_id) = persistent_identity(home.path());
        std::fs::create_dir_all(home.path().join(".ww/fhs")).unwrap();
        std::fs::write(home.path().join(".ww/fhs/installed-marker"), b"frozen baseline").unwrap();
        let admin_addr = unused_addr().await;
        let http_addr = unused_addr().await;
        let listen_addr = unused_addr().await;
        let mut options = epoch_node_options(&h0, http_addr, listen_addr, identity_path, &atom);
        options.mounts = [
            ("hidden", "/etc/app/config.toml"),
            ("config", "/etc/app/config.toml"),
            ("status.wasm", "/bin/status.wasm"),
            ("kernel.wasm", "/boot/kernel.wasm"),
        ].map(|(name, target)| format!("{}:{target}", source.join(name).display())).to_vec();
        options.mounts.push(format!("/ipfs/{overlay_cid}:/etc/overlay"));
        let mut node = RunningNode::spawn(home.path(), admin_addr, proxy_addr, &options);
        let ready_url = format!("http://{admin_addr}/readyz");
        wait_for_ready(&client, &ready_url, &mut node).await;
        assert_status_cell(&client, http_addr, &overlay_status_cid, &node).await;
        let initial_root = activated_root(&node, 0);
        wait_for_log(&mut node, "kernel-X readiness!", "").await;
        assert_persistent_peer(&client, http_addr, peer_id).await;
        let initial_identity = assert_kernel_epoch(&client, admin_addr, kubo_addr, &node, 0, &kernel).await;
        assert_eq!(initial_identity["kernel_epoch_seq"], 0);
        assert_eq!(initial_identity["kernel_effective_root"], initial_root.to_string());
        let frozen_config = immutable_path_cid(&client, kubo_addr, &initial_root, "etc/app/config.toml").await;
        let frozen_directory_entry = immutable_path_cid(&client, kubo_addr, &initial_root, "etc/overlay/overlap").await;
        let frozen_status = immutable_path_cid(&client, kubo_addr, &initial_root, "bin/status.wasm").await;
        let frozen_kernel = immutable_path_cid(&client, kubo_addr, &initial_root, "boot/kernel.wasm").await;
        assert_eq!(directory_data(&client, kubo_addr, &initial_root, "etc/overlay").await, overlay_metadata);
        let initial_etc_data = directory_data(&client, kubo_addr, &h0.cid, "etc").await;
        // Mutation then deletion distinguishes frozen-byte replay from either
        // rereading changed bytes or relying on the source path still existing.
        std::fs::write(source.join("config"), b"changed after freeze").unwrap();
        std::fs::write(source.join("status.wasm"), b"invalid after freeze").unwrap();
        std::fs::write(source.join("kernel.wasm"), b"invalid after freeze").unwrap();
        sources.close().unwrap();
        std::fs::write(home.path().join(".ww/fhs/installed-marker"), b"changed after freeze").unwrap();
        std::fs::remove_dir_all(home.path().join(".ww/fhs")).unwrap();
        let mut evidence = Vec::new();
        for (seq, head) in [(1, &h1), (2, &obstructed), (3, &recovered)] {
            atom.set_head(&head.cid).await;
            wait_for_log(&mut node, "Advancing deployment epoch", &format!("seq={seq}")).await;
            if seq == 1 {
                tokio::time::timeout(INVALIDATION_TIMEOUT, replacement_reached.recv()).await
                    .expect("valid replacement did not reach its preparation gate").expect("replacement gate closed");
                wait_for_log(&mut node, "unregistered HTTP route", "/status").await;
                assert_unready_and_old_generation_dead(&client, admin_addr, http_addr, &mut node).await;
                assert_eq!(count_log_lines(&node.logs(), "kernel-X readiness!"), 1, "cached kernel reused prior readiness");
                assert_eq!(count_log_lines_with(&node.logs(), "Deployment root activated", "seq=1"), 0);
                println!("LANDING3_B_REVOKED={}", serde_json::json!({"epoch": seq, "blocked_head": h1.cid.to_string(), "identity": version(&client, admin_addr).await}));
                replacement_release.send(true).unwrap();
            }
            if seq == 2 {
                wait_for_log(&mut node, "Authoritative Stem update selected an invalid deployment head", "").await;
                assert_unready_and_old_generation_dead(&client, admin_addr, http_addr, &mut node).await;
                assert_eq!(count_log_lines_with(&node.logs(), "Deployment root activated", "seq=2"), 0);
                continue;
            }
            wait_for_log(&mut node, "Deployment root activated", &format!("seq={seq}")).await;
            wait_for_ready(&client, &ready_url, &mut node).await;
            assert_status_cell(&client, http_addr, &overlay_status_cid, &node).await;
            let effective = activated_root(&node, seq);
            assert_ne!(effective, initial_root);
            for (path, expected) in [("etc/app/config.toml", frozen_config), ("bin/status.wasm", frozen_status), ("boot/kernel.wasm", frozen_kernel)] {
                assert_eq!(immutable_path_cid(&client, kubo_addr, &effective, path).await, expected);
                assert!(kubo_pin_is_present(&client, kubo_addr, &expected).await, "frozen source pin lost: {expected}");
            }
            for path in ["", "etc", "etc/app"] {
                assert_eq!(directory_data(&client, kubo_addr, &effective, path).await, directory_data(&client, kubo_addr, &head.cid, path).await, "ancestor metadata at {path}");
            }
            assert_ne!(directory_data(&client, kubo_addr, &effective, "etc").await, initial_etc_data);
            assert_ne!(overlay_metadata, directory_data(&client, kubo_addr, &head.cid, "etc/overlay").await);
            assert_eq!(directory_data(&client, kubo_addr, &effective, "etc/overlay").await, overlay_metadata, "CLI target directory metadata must win wholesale");
            let root_path = format!("/ipfs/{effective}");
            assert_eq!(ipfs.cat(&format!("{root_path}/etc/app/config.toml")).await.unwrap(), b"original frozen config");
            assert_eq!(ipfs.cat(&format!("{root_path}/etc/overlay/.gitignore")).await.unwrap(), b"targeted inclusion");
            assert!(ipfs.ls(&format!("{root_path}/etc/overlay/empty")).await.unwrap().is_empty());
            assert_eq!(ipfs.cat(&format!("{root_path}/generation.txt")).await.unwrap(), format!("epoch-{}\n", if seq == 1 { 11 } else { 13 }).as_bytes());
            assert_eq!(immutable_path_cid(&client, kubo_addr, &effective, "etc/overlay/overlap").await, frozen_directory_entry);
            assert_eq!(ipfs.cat(&format!("{root_path}/etc/overlay/overlap")).await.unwrap(), b"frozen directory overlap");
            assert_eq!(ipfs.cat(&format!("{root_path}/etc/overlay/unrelated")).await.unwrap(), if seq == 1 { b"11" } else { b"13" });
            assert_eq!(ipfs.cat(&format!("{root_path}/installed-marker")).await.unwrap(), b"frozen baseline");
            assert!(ipfs.ls(&format!("{root_path}/.ww")).await.is_err(), "private state leaked into FHS");
            assert_persistent_peer(&client, http_addr, peer_id).await;
            let identity = assert_kernel_epoch(&client, admin_addr, kubo_addr, &node, seq, &kernel).await;
            assert_eq!(count_log_lines(&node.logs(), "kernel-X readiness!"), if seq == 1 { 2 } else { 3 });
            assert!(!node.logs().contains("kernel-A readiness!") && !node.logs().contains("kernel-B readiness!"));
            assert_eq!(identity["kernel_cid"], ww::kernel::runtime_cid(&kernel).to_string());
            assert_eq!(identity["kernel_epoch_seq"], seq);
            assert_eq!(identity["kernel_effective_root"], effective.to_string());
            assert_eq!(identity["kernel_cid"], initial_identity["kernel_cid"]);
            assert_eq!(ipfs.cat(&format!("{root_path}/boot/kernel.wasm")).await.unwrap(), kernel);
            assert!(kubo_pin_is_present(&client, kubo_addr, &effective).await);
            assert!(kubo_pin_is_present(&client, kubo_addr, &hidden_cid).await, "overwritten source must remain frozen");
            let gc = kubo_call(&client, kubo_addr, "repo/gc", &[]).await;
            let gc_body = gc.text().await.unwrap();
            assert!(!gc_body.contains("\"Error\""), "GC failed: {gc_body}");
            assert_eq!(ipfs.cat(&format!("{root_path}/etc/app/config.toml")).await.unwrap(), b"original frozen config");
            assert_eq!(ipfs.cat(&format!("/ipfs/{hidden_cid}")).await.unwrap(), hidden_bytes);
            assert_status_cell(&client, http_addr, &overlay_status_cid, &node).await;
            evidence.push(serde_json::json!({"seq": seq, "head": head.cid.to_string(), "effective": effective.to_string(), "frozen_config": frozen_config.to_string(), "hidden_source": hidden_cid.to_string(), "frozen_directory_entry": frozen_directory_entry.to_string(), "frozen_status": frozen_status.to_string(), "frozen_kernel": frozen_kernel.to_string()}));
        }
        println!("LANDING_C_EPOCH_EVIDENCE={}", serde_json::to_string(&evidence).unwrap());
        proxy_task.abort();
    }).await;
}

/// The Rust PID0 lifecycle contract. Drivers assert host-observable behavior:
/// admin and data planes, host lifecycle events, exit codes, and the
/// `INITIAL_INIT_FAILED` failure token.
mod rust_lifecycle {
    use super::*;

    async fn tty_stdin_and_status_route_driver() {
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
        let options = NodeOptions::installed(Some(http_addr));
        let kernel_wasm = required_artifact(KERNEL_WASM_PATH);
        let kernel_source = "/boot/kernel.wasm";
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
        assert_eq!(
            identity["kernel_source"],
            format!(
                "/ipfs/{}/boot/kernel.wasm",
                identity["kernel_effective_root"].as_str().unwrap()
            )
        );
        assert_eq!(
            identity["kernel_cid"],
            ww::kernel::runtime_cid(&kernel_wasm).to_string()
        );
        assert_eq!(
            identity["kernel_wasm_blake3"],
            blake3::hash(&kernel_wasm).to_hex().to_string(),
            "the host must report the exact selected pid0 artifact"
        );
        // `/status` exercises the host HttpListener registration and a
        // separate status Cell. Production PID0 exports no guest bootstrap,
        // so this request does not probe a PID0-served capability.
        assert_status_cell(
            &client,
            http_addr,
            &ww::kernel::runtime_cid(&status_wasm),
            &node,
        )
        .await;

        node.write_stdin(b"p3-input-resume-probe\n");
        wait_for_log(&mut node, "terminal input", "received").await;
        // The PID0 log above proves non-EOF P3 stdin resumed its application
        // branch. This second request separately proves route liveness.
        assert_status_cell(
            &client,
            http_addr,
            &ww::kernel::runtime_cid(&status_wasm),
            &node,
        )
        .await;

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

    async fn initial_failure_driver() {
        let _guard = e2e_lock().await;
        let (kubo_addr, client) = require_kubo().await;
        for status_bytes in [None, Some(b"corrupt status component".as_slice())] {
            let image = invalid_status_image(status_bytes);
            let admin_addr = unused_addr().await;
            let http_addr = unused_addr().await;
            let home = tempfile::tempdir().expect("create isolated HOME");
            let mut options = NodeOptions::installed(Some(http_addr));
            options.mounts = vec![image.path().canonicalize().unwrap().display().to_string()];
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

    async fn epoch_replacement_driver() {
        let _guard = e2e_lock().await;
        let status_wasm = required_artifact(STATUS_WASM_PATH);
        let (variant_a, cid_a) = status_variant(&status_wasm, b"variant-a");
        let (variant_b, cid_b) = status_variant(&status_wasm, b"variant-b");
        assert_ne!(cid_a, cid_b);
        let (kubo_addr, client) = require_kubo().await;
        let ipfs = ww::ipfs::HttpClient::new(format!("http://{kubo_addr}"));
        let kernel_a = kernel_variant('A');
        let kernel_b = kernel_variant('B');
        let epoch1 = EpochRoot::with_kernel(&ipfs, &variant_a, &kernel_a, 1).await;
        let epoch2 = EpochRoot::with_kernel(&ipfs, &variant_b, &kernel_b, 2).await;
        let atom = AtomFixture::start(Path::new(env!("CARGO_MANIFEST_DIR"))).await;
        let first = atom.set_head(&epoch1.cid).await;

        let admin_addr = unused_addr().await;
        let http_addr = unused_addr().await;
        let listen_addr = unused_addr().await;
        let home = tempfile::tempdir().expect("create isolated epoch HOME");
        let (_signing_key, identity_path, peer_id) = persistent_identity(home.path());
        let mut options = epoch_node_options(&epoch1, http_addr, listen_addr, identity_path, &atom);
        // An authoritative head is sufficient; no implicit CWD layer is needed.
        options.mounts.clear();
        let mut node = RunningNode::spawn(home.path(), admin_addr, kubo_addr, &options);
        let ready_url = format!("http://{admin_addr}/readyz");

        wait_for_ready(&client, &ready_url, &mut node).await;
        assert_status_cell(&client, http_addr, &cid_a, &node).await;
        wait_for_log(&mut node, "kernel-A readiness!", "").await;
        let initial_identity =
            assert_kernel_epoch(&client, admin_addr, kubo_addr, &node, 0, &kernel_a).await;
        assert_persistent_peer(&client, http_addr, peer_id).await;

        let second = atom.set_head(&epoch2.cid).await;
        assert!(
            second.block_number > first.block_number,
            "Atom head updates must be mined in order"
        );
        wait_for_log(&mut node, "Advancing deployment epoch", "seq=1").await;
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
        assert_status_cell(&client, http_addr, &cid_b, &node).await;
        wait_for_log(&mut node, "kernel-B readiness!", "").await;
        let final_identity =
            assert_kernel_epoch(&client, admin_addr, kubo_addr, &node, 1, &kernel_b).await;
        assert_ne!(initial_identity["kernel_cid"], final_identity["kernel_cid"]);
        assert_persistent_peer(&client, http_addr, peer_id).await;
        println!(
            "LANDING3_A={}",
            serde_json::json!({"before": initial_identity, "after": final_identity, "peer_id": peer_id.to_string()})
        );
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

    async fn epoch_burst_driver() {
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
        let options = epoch_node_options(&epoch1, http_addr, listen_addr, identity_path, &atom);
        let mut node = RunningNode::spawn(home.path(), admin_addr, kubo_addr, &options);
        let ready_url = format!("http://{admin_addr}/readyz");
        wait_for_ready(&client, &ready_url, &mut node).await;
        assert_status_cell(&client, http_addr, &cid_a, &node).await;

        // Each mined head is polled from finalized-depth chain state. Mine the
        // intermediate update before the final update to exercise both local
        // transitions.
        let second = atom.set_head(&epoch2.cid).await;
        wait_for_log(&mut node, "Advancing deployment epoch", "seq=1").await;
        let third = atom.set_head(&epoch3.cid).await;
        wait_for_log(&mut node, "Advancing deployment epoch", "seq=2").await;
        assert!(first.block_number < second.block_number);
        assert!(second.block_number < third.block_number);

        wait_for_not_ready(&client, admin_addr, &mut node).await;
        wait_for_ready(&client, &ready_url, &mut node).await;
        assert_status_cell(&client, http_addr, &cid_c, &node).await;
        assert!(
            node.try_wait().is_none(),
            "daemon exited during rapid updates\n{}",
            node.logs()
        );

        let logs = node.logs();
        let seq1 = log_position(&logs, "Advancing deployment epoch", "seq=1");
        let seq2 = log_position(&logs, "Advancing deployment epoch", "seq=2");
        assert!(seq1 < seq2, "host advances must be ordered\n{logs}");
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

    async fn replacement_failure_driver() {
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
        let options = epoch_node_options(&epoch1, http_addr, listen_addr, identity_path, &atom);
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
        assert_status_cell(&client, http_addr, &cid_a, &node).await;

        atom.set_head(&invalid.cid).await;
        wait_for_log(&mut node, "Advancing deployment epoch", "seq=1").await;
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

    async fn interactive_replacement_driver() {
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
    async fn rust_pid0_tty_input_resumes_guest_status_route_remains_live_and_eof_exits_zero() {
        tty_stdin_and_status_route_driver().await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn rust_pid0_initial_failure() {
        initial_failure_driver().await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn rust_pid0_replacement() {
        tokio::task::LocalSet::new()
            .run_until(epoch_replacement_driver())
            .await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn rust_pid0_rapid_replacement() {
        tokio::task::LocalSet::new()
            .run_until(epoch_burst_driver())
            .await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn rust_pid0_replacement_failure() {
        tokio::task::LocalSet::new()
            .run_until(replacement_failure_driver())
            .await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn rust_pid0_interactive_replacement() {
        tokio::task::LocalSet::new()
            .run_until(interactive_replacement_driver())
            .await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn local_targeted_kernel_mount_executes_selected_bytes() {
    let _guard = e2e_lock().await;
    let selected = kernel_variant('X');
    let selected_file = tempfile::NamedTempFile::new().unwrap();
    std::fs::write(selected_file.path(), &selected).unwrap();
    let kernel_path = selected_file.path().canonicalize().unwrap();
    let (kubo_addr, client) = require_kubo().await;
    let admin_addr = unused_addr().await;
    let http_addr = unused_addr().await;
    let home = tempfile::tempdir().unwrap();
    let mut options = NodeOptions::installed(Some(http_addr));
    options
        .mounts
        .push(format!("{}:/boot/kernel.wasm", kernel_path.display()));
    let mut node = RunningNode::spawn(home.path(), admin_addr, kubo_addr, &options);
    wait_for_ready(&client, &format!("http://{admin_addr}/readyz"), &mut node).await;
    wait_for_log(&mut node, "kernel-X readiness!", "").await;
    assert_kernel_epoch(&client, admin_addr, kubo_addr, &node, 0, &selected).await;
    assert_status_route(&client, http_addr, &node).await;
    node.close_stdin();
    assert_eq!(
        node.wait(EXIT_TIMEOUT).await.code(),
        Some(0),
        "{}",
        node.logs()
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn bare_installed_boot_from_unrelated_cwd_does_not_import_cwd_or_private_state() {
    let _guard = e2e_lock().await;
    let kernel = required_artifact(KERNEL_WASM_PATH);
    let (kubo_addr, client) = require_kubo().await;
    let admin_addr = unused_addr().await;
    let home = tempfile::tempdir().unwrap();
    let cwd = tempfile::tempdir().unwrap();
    std::fs::write(cwd.path().join("cwd-secret"), b"must stay local").unwrap();
    let (_, identity_path, _) = persistent_identity(home.path());
    let private_identity = std::fs::read(&identity_path).unwrap();
    let mut options = NodeOptions::installed(None);
    options.mounts.clear();
    options.cwd = Some(cwd.path().canonicalize().unwrap());
    let mut node = RunningNode::spawn(home.path(), admin_addr, kubo_addr, &options);
    wait_for_ready(&client, &format!("http://{admin_addr}/readyz"), &mut node).await;
    assert_kernel_epoch(&client, admin_addr, kubo_addr, &node, 0, &kernel).await;
    let root = activated_root(&node, 0);
    let ipfs = ww::ipfs::HttpClient::new(format!("http://{kubo_addr}"));
    for path in ["cwd-secret", ".ww", "identity", "bin"] {
        assert!(
            ipfs.ls(&format!("/ipfs/{root}/{path}")).await.is_err(),
            "unexpected imported {path}"
        );
    }
    assert_eq!(std::fs::read(identity_path).unwrap(), private_identity);
    node.close_stdin();
    assert_eq!(
        node.wait(EXIT_TIMEOUT).await.code(),
        Some(0),
        "{}",
        node.logs()
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn explicit_dot_boots_complete_source_without_installation() {
    let _guard = e2e_lock().await;
    let selected = kernel_variant('A');
    let (kubo_addr, client) = require_kubo().await;
    let image = tempfile::tempdir().unwrap();
    std::fs::create_dir(image.path().join("boot")).unwrap();
    std::fs::write(image.path().join("boot/kernel.wasm"), &selected).unwrap();
    std::fs::write(image.path().join("explicit-marker"), b"explicit source").unwrap();
    let home = tempfile::tempdir().unwrap();
    let admin_addr = unused_addr().await;
    let mut options = NodeOptions::installed(None);
    options.installed_baseline = false;
    options.mounts = vec![".".to_owned()];
    options.cwd = Some(image.path().canonicalize().unwrap());
    let mut node = RunningNode::spawn(home.path(), admin_addr, kubo_addr, &options);
    wait_for_ready(&client, &format!("http://{admin_addr}/readyz"), &mut node).await;
    wait_for_log(&mut node, "kernel-A readiness!", "").await;
    assert_kernel_epoch(&client, admin_addr, kubo_addr, &node, 0, &selected).await;
    let root = activated_root(&node, 0);
    assert_eq!(
        ww::ipfs::HttpClient::new(format!("http://{kubo_addr}"))
            .cat(&format!("/ipfs/{root}/explicit-marker"))
            .await
            .unwrap(),
        b"explicit source"
    );
    node.close_stdin();
    assert_eq!(
        node.wait(EXIT_TIMEOUT).await.code(),
        Some(0),
        "{}",
        node.logs()
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn kubo_targeted_kernel_mount_reports_verified_source_and_runtime_identity() {
    let _guard = e2e_lock().await;
    let selected = kernel_variant('B');
    let (kubo_addr, client) = require_kubo().await;
    let source_cid = ww::ipfs::HttpClient::new(format!("http://{kubo_addr}"))
        .add_bytes(&selected)
        .await
        .unwrap();
    let admin_addr = unused_addr().await;
    let http_addr = unused_addr().await;
    let home = tempfile::tempdir().unwrap();
    let mut options = NodeOptions::installed(Some(http_addr));
    options
        .mounts
        .push(format!("/ipfs/{source_cid}:/boot/kernel.wasm"));
    let mut node = RunningNode::spawn(home.path(), admin_addr, kubo_addr, &options);
    wait_for_ready(&client, &format!("http://{admin_addr}/readyz"), &mut node).await;
    wait_for_log(&mut node, "kernel-B readiness!", "").await;
    let identity = assert_kernel_epoch(&client, admin_addr, kubo_addr, &node, 0, &selected).await;
    assert_eq!(identity["kernel_source_cid"], source_cid);
    assert_status_route(&client, http_addr, &node).await;
    node.close_stdin();
    assert_eq!(
        node.wait(EXIT_TIMEOUT).await.code(),
        Some(0),
        "{}",
        node.logs()
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn invalid_selected_kernel_fails_without_installed_or_embedded_fallback() {
    let _guard = e2e_lock().await;
    let (kubo_addr, client) = require_kubo().await;
    let incompatible = wat::parse_str("(component)").unwrap();
    for (case, bytes) in [
        ("malformed", b"not a WebAssembly component".to_vec()),
        ("incompatible", incompatible),
        ("oversized", vec![0; 8 * 1024 * 1024 + 1]),
    ] {
        let bad_kernel = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(bad_kernel.path(), bytes).unwrap();
        let admin_addr = unused_addr().await;
        let home = tempfile::tempdir().unwrap();
        let mut options = NodeOptions::installed(None);
        options.mounts.push(format!(
            "{}:/boot/kernel.wasm",
            bad_kernel.path().canonicalize().unwrap().display()
        ));
        let mut node = RunningNode::spawn(home.path(), admin_addr, kubo_addr, &options);
        let (exit, _) = wait_for_exit_without_readiness(&client, admin_addr, &mut node).await;
        let logs = node.logs();
        assert!(!exit.success(), "{case} selected kernel booted\n{logs}");
        assert!(
            logs.contains("kernel"),
            "kernel failure not identified: {logs}"
        );
        assert!(
            !logs.contains("committed readiness"),
            "{case} fell back to installed kernel\n{logs}"
        );
        assert!(
            !logs.contains("Loaded selected kernel artifact"),
            "{case} reached PID0 generation construction\n{logs}"
        );
    }
}

#[tokio::test(flavor = "current_thread")]
async fn host_transient_epoch_preparation_recovers_without_restoring_old_generation() {
    selected_block_failure_driver(StatusCode::SERVICE_UNAVAILABLE).await;
}

#[tokio::test(flavor = "current_thread")]
async fn missing_selected_block_retries_without_fallback_and_later_recovers() {
    // Kubo v0.33.0 reports an absent offline block with HTTP 500, not 404.
    selected_block_failure_driver(StatusCode::INTERNAL_SERVER_ERROR).await;
}

async fn selected_block_failure_driver(status: StatusCode) {
    tokio::task::LocalSet::new()
        .run_until(async {
            let _guard = e2e_lock().await;
            let status_wasm = required_artifact(STATUS_WASM_PATH);
            let (variant_a, cid_a) = status_variant(&status_wasm, b"transient-a");
            let (variant_b, cid_b) = status_variant(&status_wasm, b"transient-b");
            let (kubo_addr, client) = require_kubo().await;
            let ipfs = ww::ipfs::HttpClient::new(format!("http://{kubo_addr}"));
            let kernel_a = kernel_variant('A');
            let kernel_b = kernel_variant('B');
            let epoch1 = EpochRoot::with_kernel(&ipfs, &variant_a, &kernel_a, 1).await;
            let epoch2 = EpochRoot::with_kernel(&ipfs, &variant_b, &kernel_b, 2).await;
            let selected_block =
                selected_marker_block(&client, kubo_addr, &epoch2.cid, "kernel-B readiness!").await;
            let atom = AtomFixture::start(Path::new(env!("CARGO_MANIFEST_DIR"))).await;
            atom.set_head(&epoch1.cid).await;

            let proxy_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let proxy_addr = proxy_listener.local_addr().unwrap();
            let (proxy_task, attempts, release_tx) = start_failing_block_kubo_proxy(
                proxy_listener,
                kubo_addr,
                selected_block.to_string(),
                status,
            );
            let admin_addr = unused_addr().await;
            let http_addr = unused_addr().await;
            let listen_addr = unused_addr().await;
            let home = tempfile::tempdir().unwrap();
            let (_, identity_path, _) = persistent_identity(home.path());
            let mut options =
                epoch_node_options(&epoch1, http_addr, listen_addr, identity_path, &atom);
            let hidden_file = tempfile::NamedTempFile::new().unwrap();
            let hidden_bytes = format!("hidden retained source during retry: {}", hidden_file.path().display()).into_bytes();
            std::fs::write(hidden_file.path(), &hidden_bytes).unwrap();
            let hidden_cid = unuploaded_file_cid(&client, kubo_addr, &hidden_bytes).await;
            assert!(!kubo_pin_is_present(&client, kubo_addr, &hidden_cid).await, "hidden source already had a pin owner");
            options.mounts.push(format!("{}:/frozen-marker", hidden_file.path().canonicalize().unwrap().display()));
            let frozen_file = tempfile::NamedTempFile::new().unwrap();
            std::fs::write(frozen_file.path(), b"frozen during retry and supersession").unwrap();
            options.mounts.push(format!(
                "{}:/frozen-marker",
                frozen_file.path().canonicalize().unwrap().display()
            ));
            let mut node = RunningNode::spawn(home.path(), admin_addr, proxy_addr, &options);
            let ready_url = format!("http://{admin_addr}/readyz");
            wait_for_ready(&client, &ready_url, &mut node).await;
            assert_status_cell(&client, http_addr, &cid_a, &node).await;
            let initial_root = activated_root(&node, 0);
            let frozen_cid =
                immutable_path_cid(&client, kubo_addr, &initial_root, "frozen-marker").await;
            assert_ne!(frozen_cid, hidden_cid, "hidden source must be overwritten in the initial root");
            assert!(kubo_pin_is_present(&client, kubo_addr, &hidden_cid).await);
            hidden_file.close().unwrap();
            frozen_file.close().unwrap();
            atom.set_head(&epoch2.cid).await;
            wait_for_log(&mut node, "Advancing deployment epoch", "seq=1").await;
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
            while count_log_lines(&node.logs(), "Transient deployment preparation failure") < 2 {
                assert_unready_and_old_generation_dead(&client, admin_addr, http_addr, &mut node)
                    .await;
                assert!(
                    Instant::now() < deadline,
                    "Host did not report two retries\n{}",
                    node.logs()
                );
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
            assert_unready_and_old_generation_dead(&client, admin_addr, http_addr, &mut node).await;

            // The hidden source is absent from every effective root. Its own
            // frozen-source pin must retain its blocks throughout unrooted retries.
            assert!(kubo_pin_is_present(&client, kubo_addr, &hidden_cid).await);
            checked_gc(&client, kubo_addr).await;
            assert_eq!(ipfs.cat(&format!("/ipfs/{hidden_cid}")).await.unwrap(), hidden_bytes);
            assert!(kubo_pin_is_present(&client, kubo_addr, &hidden_cid).await);
            assert_unready_and_old_generation_dead(&client, admin_addr, http_addr, &mut node).await;
            println!("LANDING3_H_RETRY_RETENTION={}", serde_json::json!({"failure_status": status.as_u16(), "epoch": 1, "hidden_source": hidden_cid.to_string(), "old_effective": initial_root.to_string(), "identity": version(&client, admin_addr).await}));
            assert!(!node.logs().contains("kernel-B readiness!"));
            assert_eq!(
                count_log_lines(&node.logs(), "kernel-A readiness!"),
                1,
                "previous kernel restarted during retry"
            );
            release_tx.send(true).unwrap();
            wait_for_ready(&client, &ready_url, &mut node).await;
            assert_status_cell(&client, http_addr, &cid_b, &node).await;
            wait_for_log(&mut node, "kernel-B readiness!", "").await;
            assert_kernel_epoch(&client, admin_addr, kubo_addr, &node, 1, &kernel_b).await;
            let final_root = activated_root(&node, 1);
            assert_eq!(
                immutable_path_cid(&client, kubo_addr, &final_root, "frozen-marker").await,
                frozen_cid
            );
            assert!(kubo_pin_is_present(&client, kubo_addr, &frozen_cid).await);
            checked_gc(&client, kubo_addr).await;
            assert_eq!(ipfs.cat(&format!("/ipfs/{hidden_cid}")).await.unwrap(), hidden_bytes);
            assert_eq!(
                ipfs.cat(&format!("/ipfs/{final_root}/frozen-marker"))
                    .await
                    .unwrap(),
                b"frozen during retry and supersession"
            );
            let logs = node.logs();
            assert!(
                count_log_lines(&logs, "Transient deployment preparation failure") >= 2,
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
            let mut options =
                epoch_node_options(&epoch1, http_addr, listen_addr, identity_path, &atom);
            let frozen_file = tempfile::NamedTempFile::new().unwrap();
            std::fs::write(frozen_file.path(), b"frozen during retry and supersession").unwrap();
            options.mounts.push(format!(
                "{}:/frozen-marker",
                frozen_file.path().canonicalize().unwrap().display()
            ));
            let mut node = RunningNode::spawn(home.path(), admin_addr, proxy_addr, &options);
            let ready_url = format!("http://{admin_addr}/readyz");
            wait_for_ready(&client, &ready_url, &mut node).await;
            assert_status_cell(&client, http_addr, &cid_a, &node).await;
            let initial_root = activated_root(&node, 0);
            let frozen_cid =
                immutable_path_cid(&client, kubo_addr, &initial_root, "frozen-marker").await;
            frozen_file.close().unwrap();

            atom.set_head(&epoch2.cid).await;
            wait_for_log(&mut node, "Advancing deployment epoch", "seq=1").await;
            let retry_deadline = Instant::now() + INVALIDATION_TIMEOUT;
            while attempts.load(Ordering::SeqCst) < 2 {
                assert_unready_and_old_generation_dead(&client, admin_addr, http_addr, &mut node)
                    .await;
                assert!(Instant::now() < retry_deadline, "Host did not enter retry");
                tokio::time::sleep(Duration::from_millis(50)).await;
            }

            atom.set_head(&epoch3.cid).await;
            wait_for_log(&mut node, "Advancing deployment epoch", "seq=2").await;
            wait_for_ready(&client, &ready_url, &mut node).await;
            assert_status_cell(&client, http_addr, &cid_c, &node).await;
            let final_root = activated_root(&node, 2);
            assert_eq!(
                immutable_path_cid(&client, kubo_addr, &final_root, "frozen-marker").await,
                frozen_cid
            );
            assert!(kubo_pin_is_present(&client, kubo_addr, &frozen_cid).await);
            kubo_call(&client, kubo_addr, "repo/gc", &[])
                .await
                .bytes()
                .await
                .unwrap();
            assert_eq!(
                ipfs.cat(&format!("/ipfs/{final_root}/frozen-marker"))
                    .await
                    .unwrap(),
                b"frozen during retry and supersession"
            );
            wait_for_pin_release(&client, kubo_addr, &epoch2.cid).await;
            let logs = node.logs();
            assert_eq!(
                count_log_lines_with(&logs, "Deployment root activated", "seq=1"),
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
            wait_for_log(&mut node, "Advancing deployment epoch", "seq=1").await;
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
            wait_for_log(&mut node, "Advancing deployment epoch", "seq=2").await;
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
            assert_status_cell(&client, http_addr, &cid_c, &node).await;
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

#[tokio::test(flavor = "current_thread")]
async fn invalid_and_corrupted_selected_kernels_close_identity_until_valid_head_recovers() {
    tokio::task::LocalSet::new().run_until(async {
        let _guard = e2e_lock().await;
        let status = required_artifact(STATUS_WASM_PATH);
        let kernel_a = kernel_variant('A');
        let kernel_b = kernel_variant('B');
        let kernel_x = kernel_variant('X');
        let (kubo, client) = require_kubo().await;
        let ipfs = ww::ipfs::HttpClient::new(format!("http://{kubo}"));
        let initial = EpochRoot::with_kernel(&ipfs, &status, &kernel_a, 40).await;
        let invalid = EpochRoot::with_kernel(&ipfs, &status, b"invalid selected kernel", 41).await;
        let valid = EpochRoot::with_kernel(&ipfs, &status, &kernel_b, 42).await;
        let corrupt = EpochRoot::with_kernel(&ipfs, &status, &kernel_x, 43).await;
        let final_head = EpochRoot::with_kernel(&ipfs, &status, &kernel_a, 44).await;
        let block = selected_marker_block(&client, kubo, &corrupt.cid, "kernel-X readiness!").await;
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let proxy_addr = listener.local_addr().unwrap();
        let (proxy, corrupt_attempts) = start_corrupt_block_kubo_proxy(listener, kubo, block.to_string());
        let atom = AtomFixture::start(Path::new(env!("CARGO_MANIFEST_DIR"))).await;
        atom.set_head(&initial.cid).await;
        let home = tempfile::tempdir().unwrap();
        let (_, identity_path, peer) = persistent_identity(home.path());
        let admin = unused_addr().await;
        let http = unused_addr().await;
        let options = epoch_node_options(&initial, http, unused_addr().await, identity_path, &atom);
        let mut node = RunningNode::spawn(home.path(), admin, proxy_addr, &options);
        let ready = format!("http://{admin}/readyz");
        wait_for_ready(&client, &ready, &mut node).await;
        assert_kernel_epoch(&client, admin, kubo, &node, 0, &kernel_a).await;
        for (seq, rejected, recovered, expected) in [(1, &invalid, &valid, &kernel_b), (3, &corrupt, &final_head, &kernel_a)] {
            atom.set_head(&rejected.cid).await;
            wait_for_log(&mut node, "Advancing deployment epoch", &format!("seq={seq}")).await;
            let deadline = Instant::now() + INVALIDATION_TIMEOUT;
            while count_log_lines(&node.logs(), "Authoritative Stem update selected an invalid deployment head") < if seq == 1 { 1 } else { 2 } {
                assert!(Instant::now() < deadline, "invalid selected kernel was not rejected\n{}", node.logs());
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
            for _ in 0..3 {
                assert_unready_and_old_generation_dead(&client, admin, http, &mut node).await;
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
            assert_eq!(count_log_lines_with(&node.logs(), "Deployment root activated", &format!("seq={seq}")), 0);
            assert!(!node.logs().contains("kernel-X readiness!"), "corrupted bytes launched PID0");
            assert!(!node.logs().contains("committed readiness"), "installed baseline became a fallback");
            atom.set_head(&recovered.cid).await;
            wait_for_log(&mut node, "Advancing deployment epoch", &format!("seq={}", seq + 1)).await;
            wait_for_ready(&client, &ready, &mut node).await;
            let identity = assert_kernel_epoch(&client, admin, kubo, &node, seq + 1, expected).await;
            assert_persistent_peer(&client, http, peer).await;
            println!("LANDING3_F_H={}", serde_json::json!({"rejected_epoch": seq, "rejected_head": rejected.cid.to_string(), "recovered": identity, "peer_id": peer.to_string()}));
        }
        assert!(corrupt_attempts.load(Ordering::SeqCst) > 0);
        assert_eq!(count_log_lines(&node.logs(), "kernel-A readiness!"), 2);
        assert_eq!(count_log_lines(&node.logs(), "kernel-B readiness!"), 1);
        proxy.abort();
    }).await;
}

#[tokio::test(flavor = "current_thread")]
async fn superseded_selected_kernel_read_never_launches_obsolete_pid0() {
    tokio::task::LocalSet::new().run_until(async {
        let _guard = e2e_lock().await;
        let status = required_artifact(STATUS_WASM_PATH);
        let kernel_a = kernel_variant('A');
        let kernel_b = kernel_variant('B');
        let kernel_x = kernel_variant('X');
        let (kubo, client) = require_kubo().await;
        let ipfs = ww::ipfs::HttpClient::new(format!("http://{kubo}"));
        let initial = EpochRoot::with_kernel(&ipfs, &status, &kernel_a, 50).await;
        let obsolete = EpochRoot::with_kernel(&ipfs, &status, &kernel_b, 51).await;
        let newest = EpochRoot::with_kernel(&ipfs, &status, &kernel_x, 52).await;
        let block = selected_marker_block(&client, kubo, &obsolete.cid, "kernel-B readiness!").await;
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let proxy_addr = listener.local_addr().unwrap();
        let (proxy, mut reached, release) = start_gated_kubo_proxy_for(listener, kubo, block.to_string(), true);
        let atom = AtomFixture::start(Path::new(env!("CARGO_MANIFEST_DIR"))).await;
        atom.set_head(&initial.cid).await;
        let home = tempfile::tempdir().unwrap();
        let (_, identity_path, peer) = persistent_identity(home.path());
        let admin = unused_addr().await;
        let http = unused_addr().await;
        let options = epoch_node_options(&initial, http, unused_addr().await, identity_path, &atom);
        let mut node = RunningNode::spawn(home.path(), admin, proxy_addr, &options);
        let ready = format!("http://{admin}/readyz");
        wait_for_ready(&client, &ready, &mut node).await;
        wait_for_log(&mut node, "kernel-A readiness!", "").await;
        atom.set_head(&obsolete.cid).await;
        wait_for_log(&mut node, "Advancing deployment epoch", "seq=1").await;
        tokio::time::timeout(INVALIDATION_TIMEOUT, reached.recv()).await
            .expect("obsolete preparation did not request selected leaf").expect("selected leaf gate closed");
        assert_unready_and_old_generation_dead(&client, admin, http, &mut node).await;
        atom.set_head(&newest.cid).await;
        wait_for_log(&mut node, "Advancing deployment epoch", "seq=2").await;
        // A blocked obsolete request must not prevent a newer epoch from becoming ready.
        wait_for_ready(&client, &ready, &mut node).await;
        wait_for_log(&mut node, "kernel-X readiness!", "").await;
        let identity = assert_kernel_epoch(&client, admin, kubo, &node, 2, &kernel_x).await;
        assert_persistent_peer(&client, http, peer).await;
        release.send(true).unwrap();
        tokio::time::sleep(Duration::from_millis(100)).await;
        let logs = node.logs();
        assert!(!logs.contains("kernel-B readiness!"), "obsolete guest executed\n{logs}");
        assert_eq!(count_log_lines_with(&logs, "Deployment root activated", "seq=1"), 0);
        assert_eq!(version(&client, admin).await["kernel_epoch_seq"], 2);
        println!("LANDING3_G={}", serde_json::json!({"blocked_leaf": block.to_string(), "obsolete": obsolete.cid.to_string(), "accepted": identity, "peer_id": peer.to_string()}));
        proxy.abort();
    }).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn missing_or_wrong_type_selected_kernel_has_no_implicit_fallback() {
    let _guard = e2e_lock().await;
    let (kubo, client) = require_kubo().await;
    for kind in ["missing", "directory", "symlink"] {
        let home = tempfile::tempdir().unwrap();
        let source = tempfile::tempdir().unwrap();
        std::fs::write(source.path().join("marker"), b"valid image").unwrap();
        if kind == "directory" {
            std::fs::create_dir_all(source.path().join("boot/kernel.wasm")).unwrap();
        } else if kind == "symlink" {
            std::fs::create_dir(source.path().join("boot")).unwrap();
            std::fs::write(
                source.path().join("boot/real-kernel"),
                required_artifact(KERNEL_WASM_PATH),
            )
            .unwrap();
            #[cfg(unix)]
            std::os::unix::fs::symlink("real-kernel", source.path().join("boot/kernel.wasm"))
                .unwrap();
        }
        let mut options = NodeOptions::installed(None);
        options.installed_baseline = false;
        options.mounts = vec![source.path().canonicalize().unwrap().display().to_string()];
        let admin = unused_addr().await;
        let mut node = RunningNode::spawn(home.path(), admin, kubo, &options);
        let (exit, _) = wait_for_exit_without_readiness(&client, admin, &mut node).await;
        assert!(
            !exit.success(),
            "incomplete source started\n{}",
            node.logs()
        );
        assert!(!node.logs().contains("committed readiness"));
    }
}
