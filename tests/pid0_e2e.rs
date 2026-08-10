//! Baseline lifecycle coverage for the current embedded Glia pid0.
//!
//! This intentionally launches the real `ww` binary against the `std/status`
//! image layer. That layer contains the production `/status` init.d policy but
//! no `bin/main.wasm`, so pid0 can only resolve from the host binary's embedded
//! `std/kernel/bin/main.wasm` artifact.
//!
//! CI must run `make std` before compiling this test. Missing artifacts are a
//! hard failure: silently skipping would recreate the false-green gap this
//! baseline exists to close.

mod support;

use std::io::Read;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, ExitStatus, Stdio};
use std::time::{Duration, Instant};

use ed25519_dalek::SigningKey;
use libp2p::{Multiaddr, PeerId};
use serde_json::Value;
use tokio::net::{TcpListener, TcpStream};

use axum::body::{to_bytes, Body};
use axum::extract::{Request, State};
use axum::http::{header, Response, StatusCode};
use axum::Router;

use support::atom::AtomFixture;
use support::terminal::{expect_stale_host, TerminalSession};

const KERNEL_WASM_PATH: &str = "std/kernel/bin/main.wasm";
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

fn append_test_custom_section(component: &mut Vec<u8>) {
    append_custom_section(component, b"ww.test.distinct-kernel", b"pr2-path-source");
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
struct DelayedKuboProxy {
    client: reqwest::Client,
    target: SocketAddr,
    delayed_path_fragment: String,
}

fn start_delayed_kubo_proxy(
    listener: TcpListener,
    target: SocketAddr,
    delayed_path_fragment: impl Into<String>,
) -> tokio::task::JoinHandle<()> {
    async fn forward(State(proxy): State<DelayedKuboProxy>, request: Request) -> Response<Body> {
        let (parts, body) = request.into_parts();
        let uri = parts.uri.to_string();
        if uri.contains(&proxy.delayed_path_fragment) {
            tokio::time::sleep(Duration::from_secs(2)).await;
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

    let proxy = DelayedKuboProxy {
        client: reqwest::Client::new(),
        target,
        delayed_path_fragment: delayed_path_fragment.into(),
    };
    let app = Router::new().fallback(forward).with_state(proxy);
    tokio::spawn(async move {
        axum::serve(listener, app)
            .await
            .expect("serve delayed Kubo proxy");
    })
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
            r#"(perform host :listen
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
        std::fs::write(
            directory.path().join("delay.txt"),
            b"delay replacement init\n",
        )
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
    initial: &EpochRoot,
    http_addr: SocketAddr,
    listen_addr: SocketAddr,
    identity_path: PathBuf,
    atom: &AtomFixture,
) -> NodeOptions {
    let mut options = NodeOptions::embedded(Some(http_addr));
    options.mounts = vec![initial.mount()];
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
        if let Some(status) = node.try_wait() {
            panic!(
                "ww exited before log evidence {first:?}/{second:?}: {status}\n{}",
                node.logs()
            );
        }
        let logs = node.logs();
        if logs
            .lines()
            .any(|line| line.contains(first) && line.contains(second))
        {
            return;
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

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn current_embedded_glia_pid0_lifecycle_baseline() {
    let _guard = e2e_lock().await;
    let kernel_wasm = required_artifact(KERNEL_WASM_PATH);
    required_artifact(STATUS_WASM_PATH);
    assert!(
        !Path::new(STATUS_LAYER).join("bin/main.wasm").exists(),
        "status layer must not shadow the embedded pid0 artifact"
    );

    let (kubo_addr, client) = require_kubo().await;

    let admin_addr = unused_addr().await;
    let http_addr = unused_addr().await;
    let proxy_listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind delayed Kubo proxy");
    let proxy_addr = proxy_listener.local_addr().expect("read proxy address");
    let home = tempfile::tempdir().expect("create isolated HOME");
    let options = NodeOptions::embedded(Some(http_addr));
    let mut node = RunningNode::spawn(home.path(), admin_addr, proxy_addr, &options);

    let health_url = format!("http://{admin_addr}/healthz");
    let ready_url = format!("http://{admin_addr}/readyz");
    wait_for_http(&client, &health_url, &mut node).await;

    // The proxy is bound but deliberately not accepting yet, holding boot at
    // the existing Kubo gate while the already-running admin plane exposes the
    // real not-ready phase.
    let initial_ready = wait_for_phase(&client, &ready_url, "waiting-for-kubo", &mut node).await;
    assert_eq!(initial_ready["ready"], false);
    assert_eq!(initial_ready["phase"], "waiting-for-kubo");
    let pending_version = version(&client, admin_addr).await;
    assert_eq!(pending_version["kernel_cid"], Value::Null);
    assert_eq!(pending_version["kernel_source"], "<pending: embedded:main>");

    let proxy_task = start_kubo_proxy(proxy_listener, kubo_addr);
    let final_ready = wait_for_ready(&client, &ready_url, &mut node).await;
    assert_eq!(final_ready["ready"], true);
    assert_eq!(final_ready["phase"], "ready");

    let version = version(&client, admin_addr).await;
    assert_eq!(version["kernel_source"], "embedded:main");
    assert_eq!(
        version["kernel_cid"],
        ww::kernel::runtime_cid(&kernel_wasm).to_string()
    );
    assert_eq!(
        version["kernel_wasm_blake3"],
        blake3::hash(&kernel_wasm).to_hex().to_string(),
        "host binary must embed the exact current std/kernel artifact"
    );

    assert_status_route(&client, http_addr).await;

    // WW_TTY pins today's truthiness-based TTY mode. EOF closes the kernel
    // shell, the WASM command returns 0, and the host propagates that exact
    // pid0 exit code.
    node.close_stdin();
    let exit = node.wait(EXIT_TIMEOUT).await;
    proxy_task.abort();
    let logs = node.logs();
    assert_eq!(exit.code(), Some(0), "unexpected host exit\n{logs}");
    assert!(
        logs.contains("❯"),
        "pid0 never entered its TTY shell\n{logs}"
    );
    assert!(
        logs.contains("Kernel exited") && logs.contains("code=0"),
        "host did not report the propagated pid0 exit code\n{logs}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn local_kernel_path_reaches_ready_and_cli_overrides_env() {
    let _guard = e2e_lock().await;
    let embedded_kernel = required_artifact(KERNEL_WASM_PATH);
    required_artifact(STATUS_WASM_PATH);
    let embedded_cid = ww::kernel::runtime_cid(&embedded_kernel);
    let mut selected_kernel = embedded_kernel.clone();
    append_test_custom_section(&mut selected_kernel);
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

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn missing_and_corrupt_status_artifacts_fail_before_readiness_commit() {
    let _guard = e2e_lock().await;
    required_artifact(KERNEL_WASM_PATH);
    let (kubo_addr, _client) = require_kubo().await;

    for corrupt in [false, true] {
        let image = tempfile::tempdir().expect("create isolated image");
        let init_dir = image.path().join("etc/init.d");
        std::fs::create_dir_all(&init_dir).expect("create init.d");
        std::fs::copy(
            "std/status/etc/init.d/05-status.glia",
            init_dir.join("05-status.glia"),
        )
        .expect("copy status boot policy");
        if corrupt {
            let bin_dir = image.path().join("bin");
            std::fs::create_dir_all(&bin_dir).expect("create image bin");
            std::fs::write(bin_dir.join("status.wasm"), b"corrupt status component")
                .expect("write corrupt status component");
        }

        let admin_addr = unused_addr().await;
        let http_addr = unused_addr().await;
        let home = tempfile::tempdir().expect("create isolated HOME");
        let mut options = NodeOptions::embedded(Some(http_addr));
        options.mounts = vec![image.path().display().to_string()];
        let mut node = RunningNode::spawn(home.path(), admin_addr, kubo_addr, &options);
        let exit = node.wait(EXIT_TIMEOUT).await;
        let logs = node.logs();
        assert_eq!(
            exit.code(),
            Some(1),
            "status init failure must fail the host\n{logs}"
        );
        assert!(
            logs.contains("INITIAL_INIT_FAILED"),
            "the guest must fail initialization before committing readiness\n{logs}"
        );
    }
}

#[tokio::test(flavor = "current_thread")]
async fn embedded_glia_ww_root_tracks_active_epoch_status_bytes() {
    tokio::task::LocalSet::new()
        .run_until(async {
            let _guard = e2e_lock().await;
            let status_wasm = required_artifact(STATUS_WASM_PATH);
            let mut status_a = status_wasm.clone();
            let mut status_b = status_wasm;
            append_custom_section(&mut status_a, b"ww.test.epoch", b"variant-a");
            append_custom_section(&mut status_b, b"ww.test.epoch", b"variant-b");
            let status_a_cid = ww::kernel::runtime_cid(&status_a);
            let status_b_cid = ww::kernel::runtime_cid(&status_b);
            assert_ne!(status_a_cid, status_b_cid);

            let (kubo_addr, client) = require_kubo().await;
            let ipfs = ww::ipfs::HttpClient::new(format!("http://{kubo_addr}"));
            let epoch1 = EpochRoot::valid(&ipfs, &status_a, 1).await;
            let epoch2 = EpochRoot::valid(&ipfs, &status_b, 2).await;
            let atom = AtomFixture::start(Path::new(env!("CARGO_MANIFEST_DIR"))).await;
            let initial_receipt = atom.set_head(&epoch1.cid).await;

            let admin_addr = unused_addr().await;
            let http_addr = unused_addr().await;
            let listen_addr = unused_addr().await;
            let home = tempfile::tempdir().expect("create isolated epoch HOME");
            let (_signing_key, identity_path, _peer_id) = persistent_identity(home.path());
            let options = epoch_node_options(&epoch1, http_addr, listen_addr, identity_path, &atom);
            let mut node = RunningNode::spawn(home.path(), admin_addr, kubo_addr, &options);
            let ready_url = format!("http://{admin_addr}/readyz");

            let initial_ready = wait_for_ready(&client, &ready_url, &mut node).await;
            assert_eq!(initial_ready["phase"], "ready");
            assert_status_cell(&client, http_addr, &status_a_cid).await;

            let replacement_receipt = atom.set_head(&epoch2.cid).await;
            assert!(replacement_receipt.block_number > initial_receipt.block_number);
            wait_for_log(&mut node, "Advancing epoch", "seq=2").await;
            let replacing = wait_for_not_ready(&client, admin_addr, &mut node).await;
            assert_eq!(replacing["phase"], "kernel-not-ready");

            let recovered = wait_for_ready(&client, &ready_url, &mut node).await;
            assert_eq!(recovered["phase"], "ready");
            assert_status_cell(&client, http_addr, &status_b_cid).await;
            assert!(
                node.try_wait().is_none(),
                "replacement exited\n{}",
                node.logs()
            );
        })
        .await;
}

#[tokio::test(flavor = "current_thread")]
async fn real_atom_epoch_transition_regrafts_current_embedded_glia_pid0() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let _guard = e2e_lock().await;
            let kernel_wasm = required_artifact(KERNEL_WASM_PATH);
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
            let options = epoch_node_options(&epoch1, http_addr, listen_addr, identity_path, &atom);
            let mut node = RunningNode::spawn(home.path(), admin_addr, kubo_addr, &options);
            let ready_url = format!("http://{admin_addr}/readyz");
            let ready = wait_for_ready(&client, &ready_url, &mut node).await;
            assert_eq!(ready["phase"], "ready");
            let identity = version(&client, admin_addr).await;
            assert_eq!(identity["kernel_source"], "embedded:main");
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
            expect_stale_host(&retained_old_host).await;

            let recovered = wait_for_ready(&client, &ready_url, &mut node).await;
            assert_eq!(recovered["phase"], "ready");
            assert_status_route(&client, http_addr).await;
            assert_route_unavailable(&client, http_addr, &epoch1.route).await;
            assert_route_available(&client, http_addr, &epoch2.route).await;

            // The authenticated bootstrap membrane is intentionally stable:
            // it may be used to obtain the current generation's exact graft.
            let stable_regraft = terminal.regraft().await;
            stable_regraft
                .semantic_probes(
                    &signing_key,
                    &status_wasm,
                    &epoch2.marker_path(),
                    &epoch2.marker,
                )
                .await;
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
                count_log_lines(
                    &logs,
                    "pid0 host authority became stale; re-grafting and rerunning init.d"
                ),
                1,
                "one Atom epoch transition must cause exactly one re-graft\n{logs}"
            );
            let old_unregistered = log_position(&logs, "unregistered HTTP route", &epoch1.route);
            let regraft = log_position(
                &logs,
                "pid0 host authority became stale; re-grafting and rerunning init.d",
                "pid0",
            );
            let replacement_registered =
                log_position(&logs, "registered HTTP route", &epoch2.route);
            assert!(
                old_unregistered < regraft && regraft < replacement_registered,
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

#[tokio::test(flavor = "current_thread")]
async fn rapid_atom_updates_are_serialized_and_only_final_generation_activates() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let _guard = e2e_lock().await;
            required_artifact(KERNEL_WASM_PATH);
            let status_wasm = required_artifact(STATUS_WASM_PATH);
            let (kubo_addr, client) = require_kubo().await;
            let ipfs = ww::ipfs::HttpClient::new(format!("http://{kubo_addr}"));
            let epoch1 = EpochRoot::valid(&ipfs, &status_wasm, 1).await;
            let epoch2 = EpochRoot::valid(&ipfs, &status_wasm, 2).await;
            let epoch3 = EpochRoot::valid(&ipfs, &status_wasm, 3).await;
            let atom = AtomFixture::start(Path::new(env!("CARGO_MANIFEST_DIR"))).await;
            let first = atom.set_head(&epoch1.cid).await;

            let admin_addr = unused_addr().await;
            let http_addr = unused_addr().await;
            let listen_addr = unused_addr().await;
            let home = tempfile::tempdir().expect("create isolated repeated-event HOME");
            let (signing_key, identity_path, peer_id) = persistent_identity(home.path());
            let options = epoch_node_options(&epoch1, http_addr, listen_addr, identity_path, &atom);
            let mut node = RunningNode::spawn(home.path(), admin_addr, kubo_addr, &options);
            let ready_url = format!("http://{admin_addr}/readyz");
            wait_for_ready(&client, &ready_url, &mut node).await;
            assert_route_available(&client, http_addr, &epoch1.route).await;
            let terminal =
                TerminalSession::connect(peer_id, terminal_address(listen_addr), &signing_key)
                    .await;
            let retained_old_host = terminal.graft.host.clone();

            // Finalizer canonicality means update 2 must be observed before
            // update 3 is mined; otherwise the contract's current head would
            // correctly supersede update 2. The second write follows the real
            // host's seq=2 advance immediately, within the Glia probe window.
            let second = atom.set_head(&epoch2.cid).await;
            wait_for_log(&mut node, "Advancing epoch", "seq=2").await;
            let third = atom.set_head(&epoch3.cid).await;
            wait_for_log(&mut node, "Advancing epoch", "seq=3").await;
            assert!(first.block_number < second.block_number);
            assert!(second.block_number < third.block_number);

            wait_for_not_ready(&client, admin_addr, &mut node).await;
            assert_route_unavailable(&client, http_addr, &epoch1.route).await;
            assert_route_unavailable(&client, http_addr, &epoch2.route).await;
            assert_route_unavailable(&client, http_addr, &epoch3.route).await;
            expect_stale_host(&retained_old_host).await;
            wait_for_ready(&client, &ready_url, &mut node).await;
            assert_status_route(&client, http_addr).await;
            assert_route_unavailable(&client, http_addr, &epoch1.route).await;
            assert_route_unavailable(&client, http_addr, &epoch2.route).await;
            assert_route_available(&client, http_addr, &epoch3.route).await;

            let fresh_terminal =
                TerminalSession::connect(peer_id, terminal_address(listen_addr), &signing_key)
                    .await;
            fresh_terminal
                .graft
                .semantic_probes(
                    &signing_key,
                    &status_wasm,
                    &epoch3.marker_path(),
                    &epoch3.marker,
                )
                .await;

            let logs = node.logs();
            let seq2 = log_position(&logs, "Advancing epoch", "seq=2");
            let seq3 = log_position(&logs, "Advancing epoch", "seq=3");
            let old_unregistered = log_position(&logs, "unregistered HTTP route", &epoch1.route);
            let regraft = log_position(
                &logs,
                "pid0 host authority became stale; re-grafting and rerunning init.d",
                "pid0",
            );
            let final_registered = log_position(&logs, "registered HTTP route", &epoch3.route);
            assert!(
                seq2 < seq3
                    && seq2 < old_unregistered
                    && old_unregistered < regraft
                    && regraft < final_registered,
                "host advances and the final activation must be strictly ordered\n{logs}"
            );
            assert_eq!(
                count_log_lines(
                    &logs,
                    "pid0 host authority became stale; re-grafting and rerunning init.d"
                ),
                1,
                "a burst inside one stale-probe window must coalesce to one serial re-graft\n{logs}"
            );
            assert!(
                !logs.lines().any(|line| {
                    line.contains("registered HTTP route") && line.contains(&epoch2.route)
                }),
                "superseded epoch 2 must never activate a registration\n{logs}"
            );
        })
        .await;
}

#[tokio::test(flavor = "current_thread")]
async fn replacement_init_failure_exits_nonzero_without_stale_or_partial_routes() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let _guard = e2e_lock().await;
            required_artifact(KERNEL_WASM_PATH);
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
            let options = epoch_node_options(&epoch1, http_addr, listen_addr, identity_path, &atom);
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
                expect_stale_host(&retained_old_host),
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
                logs.contains("EPOCH_RESTART_INIT_FAILED"),
                "replacement failure omitted the stable named error\n{logs}"
            );
            assert!(
                logs.lines().any(|line| {
                    line.contains("registered HTTP route") && line.contains(&invalid.route)
                }),
                "failure fixture never exercised partial replacement registration\n{logs}"
            );
            let old_unregistered = log_position(&logs, "unregistered HTTP route", &epoch1.route);
            let partial_registered = log_position(&logs, "registered HTTP route", &invalid.route);
            let partial_unregistered =
                log_position(&logs, "unregistered HTTP route", &invalid.route);
            assert!(
                old_unregistered < partial_registered && partial_registered < partial_unregistered,
                "old and partial registration teardown ordering changed\n{logs}"
            );
            assert!(
                !logs
                    .lines()
                    .any(|line| line.contains("Kernel exited") && line.contains("code=0")),
                "failed replacement followed the accidental success path\n{logs}"
            );
            proxy_task.abort();
        })
        .await;
}
