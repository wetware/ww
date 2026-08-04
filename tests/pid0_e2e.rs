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

impl RunningNode {
    fn spawn(
        home: &Path,
        admin_addr: SocketAddr,
        http_addr: SocketAddr,
        proxy_addr: SocketAddr,
    ) -> Self {
        let stdout = tempfile::NamedTempFile::new().expect("create stdout capture");
        let stderr = tempfile::NamedTempFile::new().expect("create stderr capture");
        let mut child = Command::new(ww_bin())
            .args([
                "run",
                STATUS_LAYER,
                "--insecure-ephemeral",
                "--listen",
                "/ip4/127.0.0.1/tcp/0",
                "--executor-threads",
                "1",
                "--http-listen",
                &http_addr.to_string(),
                "--with-http-admin",
                &admin_addr.to_string(),
                "--ipfs-url",
                &format!("http://{proxy_addr}"),
            ])
            .env("HOME", home)
            // Captured logs are asserted as plain text; CI may force ANSI colors.
            .env("NO_COLOR", "1")
            .env("WW_TTY", "1")
            .env("WW_KUBO_WAIT_MAX_SECS", "30")
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
    let kernel_wasm = required_artifact(KERNEL_WASM_PATH);
    required_artifact(STATUS_WASM_PATH);
    assert!(
        !Path::new(STATUS_LAYER).join("bin/main.wasm").exists(),
        "status layer must not shadow the embedded pid0 artifact"
    );

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
            panic!("Kubo is required at {kubo_addr} for the real pid0 baseline: {error}")
        });
    assert!(kubo_id.status().is_success(), "Kubo /api/v0/id failed");

    let admin_addr = unused_addr().await;
    let http_addr = unused_addr().await;
    let proxy_listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind delayed Kubo proxy");
    let proxy_addr = proxy_listener.local_addr().expect("read proxy address");
    let home = tempfile::tempdir().expect("create isolated HOME");
    let mut node = RunningNode::spawn(home.path(), admin_addr, http_addr, proxy_addr);

    let health_url = format!("http://{admin_addr}/healthz");
    let ready_url = format!("http://{admin_addr}/readyz");
    wait_for_http(&client, &health_url, &mut node).await;

    // The proxy is bound but deliberately not accepting yet, holding boot at
    // the existing Kubo gate while the already-running admin plane exposes the
    // real not-ready phase.
    let initial_ready = wait_for_phase(&client, &ready_url, "waiting-for-kubo", &mut node).await;
    assert_eq!(initial_ready["ready"], false);
    assert_eq!(initial_ready["phase"], "waiting-for-kubo");

    let proxy_task = start_kubo_proxy(proxy_listener, kubo_addr);
    let final_ready = wait_for_ready(&client, &ready_url, &mut node).await;
    assert_eq!(final_ready["ready"], true);
    assert_eq!(final_ready["phase"], "ready");

    let version: Value = client
        .get(format!("http://{admin_addr}/version"))
        .send()
        .await
        .expect("query /version")
        .error_for_status()
        .expect("/version should succeed")
        .json()
        .await
        .expect("parse /version JSON");
    assert_eq!(
        version["kernel_wasm_blake3"],
        blake3::hash(&kernel_wasm).to_hex().to_string(),
        "host binary must embed the exact current std/kernel artifact"
    );

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
