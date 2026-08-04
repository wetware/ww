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

use std::io::Read;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, ExitStatus, Stdio};
use std::time::{Duration, Instant};

use serde_json::Value;
use tokio::net::{TcpListener, TcpStream};

const KERNEL_WASM_PATH: &str = "std/kernel/bin/main.wasm";
const STATUS_WASM_PATH: &str = "std/status/bin/status.wasm";
const STATUS_LAYER: &str = "std/status";
const DEFAULT_KUBO_ADDR: &str = "127.0.0.1:5001";
const KUBO_ADDR_ENV: &str = "WW_TEST_KUBO_ADDR";
const BOOT_TIMEOUT: Duration = Duration::from_secs(60);
const EXIT_TIMEOUT: Duration = Duration::from_secs(30);

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
    stdout: tempfile::NamedTempFile,
    stderr: tempfile::NamedTempFile,
}

struct NodeOptions {
    image: PathBuf,
    kernel_cli: Option<String>,
    kernel_env: Option<String>,
    http_addr: Option<SocketAddr>,
    route_ready_timeout_secs: u64,
}

impl NodeOptions {
    fn embedded(http_addr: Option<SocketAddr>) -> Self {
        Self {
            image: PathBuf::from(STATUS_LAYER),
            kernel_cli: None,
            kernel_env: None,
            http_addr,
            route_ready_timeout_secs: 30,
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
        let stdout = tempfile::NamedTempFile::new().expect("create stdout capture");
        let stderr = tempfile::NamedTempFile::new().expect("create stderr capture");
        let mut command = Command::new(ww_bin());
        command
            .arg("run")
            .arg(&options.image)
            .args([
                "--insecure-ephemeral",
                "--listen",
                "/ip4/127.0.0.1/tcp/0",
                "--executor-threads",
                "1",
                "--with-http-admin",
            ])
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

        let mut child = command
            .env("HOME", home)
            // Captured logs are asserted as plain text; CI may force ANSI colors.
            .env("NO_COLOR", "1")
            .env("WW_TTY", "1")
            .env("WW_KUBO_WAIT_MAX_SECS", "30")
            .env(
                "WW_KERNEL_ROUTE_READY_TIMEOUT_SECS",
                options.route_ready_timeout_secs.to_string(),
            )
            .env("WW_CWASM_DIR", home.join("cwasm"))
            .env_remove("WW_IDENTITY")
            .env_remove("WW_HTTP_ADMIN")
            .env_remove("IPFS_API")
            .stdin(Stdio::piped())
            .stdout(Stdio::from(
                stdout.as_file().try_clone().expect("clone stdout file"),
            ))
            .stderr(Stdio::from(
                stderr.as_file().try_clone().expect("clone stderr file"),
            ))
            .spawn()
            .expect("spawn real ww host binary");
        let stdin = child.stdin.take().expect("open child stdin");
        Self {
            child,
            stdin: Some(stdin),
            stdout,
            stderr,
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
        format!(
            "--- stdout ---\n{}\n--- stderr ---\n{}",
            read_capture(&self.stdout),
            read_capture(&self.stderr)
        )
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
    let kernel_wasm = required_artifact(KERNEL_WASM_PATH);
    required_artifact(STATUS_WASM_PATH);
    let kernel_path = Path::new(KERNEL_WASM_PATH)
        .canonicalize()
        .expect("canonicalize kernel artifact");
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
    assert_eq!(
        identity["kernel_cid"],
        ww::kernel::runtime_cid(&kernel_wasm).to_string()
    );
    assert_eq!(
        identity["kernel_wasm_blake3"],
        blake3::hash(&kernel_wasm).to_hex().to_string()
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
async fn missing_and_corrupt_status_artifacts_fail_within_route_timeout() {
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
        options.image = image.path().to_owned();
        options.route_ready_timeout_secs = 5;
        let mut node = RunningNode::spawn(home.path(), admin_addr, kubo_addr, &options);
        let started = Instant::now();
        let exit = node.wait(EXIT_TIMEOUT).await;
        let logs = node.logs();
        assert_eq!(
            exit.code(),
            Some(1),
            "status failure must fail host\n{logs}"
        );
        assert!(
            started.elapsed() < Duration::from_secs(20),
            "route readiness did not honor its bound: {:?}\n{logs}",
            started.elapsed()
        );
        assert!(
            logs.contains(
                "kernel did not complete reverse graft and register an HTTP route within 5s"
            ),
            "failure must name the bounded route gate\n{logs}"
        );
        assert!(
            logs.contains("$WW_ROOT/bin/status.wasm"),
            "failure must name the required image artifact\n{logs}"
        );
    }
}
