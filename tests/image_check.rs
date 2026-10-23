//! Executable image-check contracts. Real backend cases require
//! WW_TEST_REQUIRE_KUBO=1, Kubo 0.33.0, and std/kernel/bin/main.wasm.

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::time::{Duration, Instant};

use cell::image::{inspect_source, FrozenMount, GuestTarget};
use cid::Cid;
use serde_json::Value;
use tokio::io::AsyncReadExt;

struct Kubo {
    child: Child,
    repository: tempfile::TempDir,
    api: String,
    url: String,
}

impl Drop for Kubo {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn success(output: &Output) {
    assert!(
        output.status.success(),
        "command failed:\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

impl Kubo {
    fn command(repository: &Path) -> Command {
        let mut command = Command::new("ipfs");
        command.env("IPFS_PATH", repository);
        command
    }

    fn start() -> Option<Self> {
        if std::env::var_os("WW_TEST_REQUIRE_KUBO").is_none() {
            eprintln!("Kubo image-check case not executed; set WW_TEST_REQUIRE_KUBO=1");
            return None;
        }
        let version = Command::new("ipfs")
            .args(["version", "--number"])
            .output()
            .expect("WW_TEST_REQUIRE_KUBO requires ipfs on PATH");
        success(&version);
        assert_eq!(String::from_utf8_lossy(&version.stdout).trim(), "0.33.0");
        let repository = tempfile::tempdir().unwrap();
        for args in [
            vec!["init", "--profile=test"],
            vec!["config", "Addresses.API", "/ip4/127.0.0.1/tcp/0"],
            vec!["config", "Addresses.Gateway", "/ip4/127.0.0.1/tcp/0"],
            vec!["config", "--json", "Addresses.Swarm", "[]"],
        ] {
            success(
                &Self::command(repository.path())
                    .args(args)
                    .output()
                    .unwrap(),
            );
        }
        let child = Self::command(repository.path())
            .args(["daemon", "--offline"])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let mut kubo = Self {
            child,
            repository,
            api: String::new(),
            url: String::new(),
        };
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            if let Ok(api) = std::fs::read_to_string(kubo.repository.path().join("api")) {
                kubo.api = api.trim().to_owned();
                if kubo
                    .command_api()
                    .arg("id")
                    .output()
                    .unwrap()
                    .status
                    .success()
                {
                    let parts: Vec<_> = kubo.api.split('/').collect();
                    kubo.url = format!("http://{}:{}", parts[2], parts[4]);
                    return Some(kubo);
                }
            }
            assert!(Instant::now() < deadline, "isolated Kubo startup timed out");
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    fn command_api(&self) -> Command {
        let mut command = Self::command(self.repository.path());
        command.arg(format!("--api={}", self.api));
        command
    }

    fn bytes(&self, args: &[&str]) -> Vec<u8> {
        let output = self.command_api().args(args).output().unwrap();
        success(&output);
        output.stdout
    }

    fn client(&self) -> ww::ipfs::BootClient {
        ww::ipfs::BootClient::one_attempt(
            ww::ipfs::HttpClient::new(self.url.clone()),
            Duration::from_secs(20),
        )
    }

    async fn add(&self, directory: &Path) -> Cid {
        String::from_utf8(self.bytes(&[
            "add",
            "-Qr",
            "--cid-version=1",
            "--raw-leaves=true",
            "--hash=sha2-256",
            "--chunker=size-262144",
            directory.to_str().unwrap(),
        ]))
        .unwrap()
        .trim()
        .parse()
        .unwrap()
    }
}

fn write(root: &Path, name: &str, bytes: &[u8]) {
    let path = root.join(name);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, bytes).unwrap();
}

// Resolve macOS /var once in fixture setup; production must reject symlink traversal.
fn real_path(directory: &tempfile::TempDir) -> PathBuf {
    directory.path().canonicalize().unwrap()
}

fn kernel() -> Vec<u8> {
    std::fs::read(Path::new(env!("CARGO_MANIFEST_DIR")).join("std/kernel/bin/main.wasm"))
        .expect("build std/kernel/bin/main.wasm before required image-check tests")
}

fn command(home: &Path, api: &str, baseline: Option<&Path>) -> tokio::process::Command {
    command_format(home, api, baseline, true)
}

fn command_format(
    home: &Path,
    api: &str,
    baseline: Option<&Path>,
    json: bool,
) -> tokio::process::Command {
    let mut command = tokio::process::Command::new(env!("CARGO_BIN_EXE_ww"));
    command
        .args(["image", "check", "--ipfs-url", api])
        .env("HOME", home)
        .env("NO_PROXY", "127.0.0.1,localhost")
        .env("RUST_LOG", "info")
        .env_remove("WW_FHS_ROOT")
        .env_remove("WW_KERNEL")
        .env_remove("WW_IDENTITY")
        .env_remove("IPFS_ROUTING_API")
        .stdin(Stdio::null())
        .kill_on_drop(true);
    if json {
        command.arg("--json");
    }
    if let Some(baseline) = baseline {
        command.env("WW_FHS_ROOT", baseline);
    }
    command
}

async fn run(command: &mut tokio::process::Command) -> Output {
    tokio::time::timeout(Duration::from_secs(90), command.output())
        .await
        .expect("image check timed out")
        .unwrap()
}

fn report(output: &Output) -> Value {
    success(output);
    let report: Value = serde_json::from_slice(&output.stdout).unwrap_or_else(|error| {
        panic!(
            "success stdout must contain only JSON: {error}\n{}",
            String::from_utf8_lossy(&output.stdout)
        )
    });
    assert_eq!(report["static_validation"], "PASS");
    assert_eq!(report["composer_profile"], "wetware-composer-v1");
    assert_eq!(report["hash_algorithm"], "sha2-256");
    assert_eq!(
        report["stages"],
        serde_json::json!([
            "source-resolution",
            "filesystem-composition",
            "kernel-selection",
            "verified-component-bytes",
            "production-p3-static-validation",
        ])
    );
    report
}

async fn frozen(kubo: &Kubo, root: Cid, target: &str) -> FrozenMount {
    let (_sender, mut cancel) = tokio::sync::watch::channel(false);
    FrozenMount::new(
        inspect_source(root, &kubo.client(), &mut cancel)
            .await
            .unwrap(),
        GuestTarget::new(target).unwrap(),
    )
    .unwrap()
}

async fn compare_preparation(
    kubo: &Kubo,
    report: &Value,
    baseline: Option<&FrozenMount>,
    mounts: &[FrozenMount],
    bytes: &[u8],
) {
    let (_sender, mut cancel) = tokio::sync::watch::channel(false);
    let root = ww::image::compose_root(None, baseline, mounts, &kubo.client(), &mut cancel)
        .await
        .unwrap();
    let (runtime, _clock) = cell::engine::runtime_engine().unwrap();
    let prepared = ww::image::prepare_kernel(
        root,
        &kubo.client(),
        runtime,
        &ww::kernel::Compilation::default(),
        &mut cancel,
    )
    .await
    .unwrap();
    assert_eq!(report["effective_root"], root.to_string());
    assert_eq!(report["kernel_source"], prepared.source_file().to_string());
    assert_eq!(report["kernel_runtime"], prepared.runtime_cid().to_string());
    assert_eq!(prepared.runtime_cid(), ww::kernel::runtime_cid(bytes));
    assert_ne!(prepared.source_file(), prepared.runtime_cid());
}

#[tokio::test]
async fn installed_local_cid_and_targeted_inputs_match_boot_preparation_and_preserve_pins() {
    let Some(kubo) = Kubo::start() else { return };
    let home = tempfile::tempdir().unwrap();
    let home = real_path(&home);
    let baseline = home.join(".ww/fhs");
    let bytes = kernel();
    write(&baseline, "boot/kernel.wasm", &bytes);
    write(&baseline, "etc/original", b"installed baseline");
    let baseline_cid = kubo.add(&baseline).await;
    let baseline_mount = frozen(&kubo, baseline_cid, "/").await;

    // Default discovery must select HOME/.ww/fhs, without explicit mounts.
    let installed = report(&run(&mut command(&home, &kubo.url, None)).await);
    compare_preparation(&kubo, &installed, Some(&baseline_mount), &[], &bytes).await;

    let overlay = home.join("overlay");
    write(&overlay, "etc/original", b"CLI override");
    let overlay_cid = kubo.add(&overlay).await;
    let overlay_mount = frozen(&kubo, overlay_cid, "/").await;
    let local = report(&run(command(&home, &kubo.url, Some(&baseline)).arg(&overlay)).await);
    // Check ownership before direct preparation can acquire the same root pin.
    kubo.bytes(&["repo", "gc"]);
    for root in [baseline_cid, overlay_cid] {
        kubo.bytes(&["pin", "ls", "--type=recursive", &root.to_string()]);
    }
    compare_preparation(
        &kubo,
        &local,
        Some(&baseline_mount),
        std::slice::from_ref(&overlay_mount),
        &bytes,
    )
    .await;
    let immutable = report(
        &run(command(&home, &kubo.url, Some(&baseline)).arg(format!("/ipfs/{overlay_cid}"))).await,
    );
    assert_eq!(local["effective_root"], immutable["effective_root"]);
    assert_eq!(local["kernel_source"], immutable["kernel_source"]);

    // The positional file overrides the bad baseline kernel at its exact target.
    let bad = home.join("bad-baseline");
    write(&bad, "boot/kernel.wasm", b"invalid baseline component");
    let replacement = home.join("replacement.wasm");
    std::fs::write(&replacement, &bytes).unwrap();
    let override_report = report(
        &run(command(&home, &kubo.url, Some(&bad))
            .arg(format!("{}:/boot/kernel.wasm", replacement.display())))
        .await,
    );
    assert_eq!(
        override_report["kernel_runtime"],
        installed["kernel_runtime"]
    );
    let human = run(&mut command_format(
        &home,
        &kubo.url,
        Some(&baseline),
        false,
    ))
    .await;
    success(&human);
    let text = String::from_utf8(human.stdout).unwrap();
    for expected in [
        "Effective root:",
        "Kernel source:",
        "Kernel runtime:",
        "Static validation: PASS",
        "does not establish runtime readiness",
    ] {
        assert!(text.contains(expected), "missing {expected}: {text}");
    }

    // Check existing pins before invoking the shared seam again or adding another owner.
    kubo.bytes(&["repo", "gc"]);
    for root in [
        baseline_cid.to_string(),
        overlay_cid.to_string(),
        local["effective_root"].as_str().unwrap().to_owned(),
    ] {
        kubo.bytes(&["pin", "ls", "--type=recursive", &root]);
        kubo.bytes(&["block", "get", &root]);
    }
    assert_eq!(
        kubo.bytes(&[
            "cat",
            &format!(
                "/ipfs/{}/etc/original",
                local["effective_root"].as_str().unwrap()
            )
        ]),
        b"CLI override"
    );
    assert!(!home.join(".ww/host.key").exists());
    assert!(!home.join(".ww/ipns").exists());
}

#[tokio::test]
async fn frozen_namespace_is_below_positional_override() {
    let Some(kubo) = Kubo::start() else { return };
    let home = tempfile::tempdir().unwrap();
    let home = real_path(&home);
    let baseline = home.join("baseline");
    let bytes = kernel();
    write(&baseline, "boot/kernel.wasm", &bytes);
    let namespace = home.join("namespace");
    write(&namespace, "etc/value", b"namespace");
    let namespace_cid = kubo.add(&namespace).await;
    let config = home.join("config");
    write(
        &config,
        "etc/ns/application",
        format!("bootstrap=/ipfs/{namespace_cid}\n").as_bytes(),
    );
    let override_file = home.join("override");
    std::fs::write(&override_file, b"positional").unwrap();
    let output = run(command(&home, &kubo.url, Some(&baseline))
        .arg("--namespace-root")
        .arg(&config)
        .arg(format!("{}:/etc/value", override_file.display())))
    .await;
    let checked = report(&output);
    let root = checked["effective_root"].as_str().unwrap();
    assert_eq!(
        kubo.bytes(&["cat", &format!("/ipfs/{root}/etc/value")]),
        b"positional"
    );
    assert_eq!(
        checked["kernel_runtime"],
        ww::kernel::runtime_cid(&bytes).to_string()
    );
}

#[tokio::test]
async fn static_check_does_not_execute_trapping_guest_start() {
    let Some(kubo) = Kubo::start() else { return };
    let home = tempfile::tempdir().unwrap();
    let home = real_path(&home);
    let image = home.join("image");
    let wat = include_str!("../crates/cell/tests/fixtures/kernel-command.wat").replace(
        "(func (export \"run\")",
        "(func $trap unreachable) (start $trap) (func (export \"run\")",
    );
    let bytes = wat::parse_str(wat).unwrap();
    write(&image, "boot/kernel.wasm", &bytes);
    let checked = report(&run(command(&home, &kubo.url, None).arg(&image)).await);
    assert_eq!(
        checked["kernel_runtime"],
        ww::kernel::runtime_cid(&bytes).to_string()
    );
}

#[tokio::test]
async fn invalid_images_fail_without_success_output() {
    let Some(kubo) = Kubo::start() else { return };
    let home = tempfile::tempdir().unwrap();
    let home = real_path(&home);
    for case in ["missing", "directory", "invalid", "ancestor", "symlink"] {
        let image = home.join(case);
        std::fs::create_dir_all(&image).unwrap();
        match case {
            "missing" => write(&image, "etc/config", b"no kernel"),
            "directory" => std::fs::create_dir_all(image.join("boot/kernel.wasm")).unwrap(),
            "invalid" => write(&image, "boot/kernel.wasm", b"not a wasm component"),
            "ancestor" => write(&image, "boot", b"obstruction"),
            "symlink" => {
                write(&image, "real-kernel", &kernel());
                std::fs::create_dir_all(image.join("boot")).unwrap();
                #[cfg(unix)]
                std::os::unix::fs::symlink("../real-kernel", image.join("boot/kernel.wasm"))
                    .unwrap();
            }
            _ => unreachable!(),
        }
        let output = run(command(&home, &kubo.url, None).arg(&image)).await;
        assert!(!output.status.success(), "{case} unexpectedly passed");
        assert!(output.stdout.is_empty(), "{case} printed success output");
        let diagnostic = String::from_utf8_lossy(&output.stderr);
        assert!(
            diagnostic.contains("kernel") || diagnostic.contains("symlink"),
            "{case}: {diagnostic}"
        );
        // Freeze the same invalid filesystem once, then compare immutable
        // CLI input with the actual boot preparation seam.
        let cid = kubo.add(&image).await;
        let immutable = run(command(&home, &kubo.url, None).arg(format!("/ipfs/{cid}"))).await;
        assert!(
            !immutable.status.success(),
            "immutable {case} unexpectedly passed"
        );
        assert!(immutable.stdout.is_empty());
        let (_sender, mut cancel) = tokio::sync::watch::channel(false);
        let root = ww::image::compose_root(Some(cid), None, &[], &kubo.client(), &mut cancel).await;
        if let Ok(root) = root {
            let (runtime, _clock) = cell::engine::runtime_engine().unwrap();
            assert!(
                ww::image::prepare_kernel(
                    root,
                    &kubo.client(),
                    runtime,
                    &ww::kernel::Compilation::default(),
                    &mut cancel,
                )
                .await
                .is_err(),
                "boot accepted immutable {case} rejected by check"
            );
        }
    }
}

#[tokio::test]
async fn invalid_authoritative_ipns_head_never_falls_back_to_valid_baseline() {
    let Some(kubo) = Kubo::start() else { return };
    let home = tempfile::tempdir().unwrap();
    let home = real_path(&home);
    let baseline = home.join("baseline");
    write(&baseline, "boot/kernel.wasm", &kernel());
    let key = libp2p::identity::Keypair::generate_ed25519();
    let name = key.public().to_peer_id();
    let raw = rust_ipns::Record::new(
        &key,
        "/ipns/not-a-deployment",
        chrono::Utc::now() + chrono::Duration::hours(1),
        3,
        ww::ipns::RECORD_TTL,
    )
    .unwrap()
    .encode()
    .unwrap();
    let watermark = ww::ipns::RecordStore::follower(&home.join(".ww"), name);
    watermark.persist(&raw).unwrap();
    let output = run(command(&home, &kubo.url, Some(&baseline)).args([
        "--ipns-stem",
        &name.to_string(),
        "--ipns-routing-url",
        "http://127.0.0.1:1",
    ]))
    .await;
    assert!(!output.status.success());
    assert!(output.stdout.is_empty());
    assert!(String::from_utf8_lossy(&output.stderr).contains("invalid authoritative head"));
    assert_eq!(std::fs::read(watermark.path()).unwrap(), raw);
}

#[cfg(unix)]
#[tokio::test]
async fn sigint_cancels_stalled_source_import_without_success_output() {
    let home = tempfile::tempdir().unwrap();
    let home = real_path(&home);
    let image = home.join("image");
    write(
        &image,
        "boot/kernel.wasm",
        b"input never reaches compilation",
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let (arrived, received) = tokio::sync::oneshot::channel();
    let server = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        let mut request = [0; 4096];
        let count = stream.read(&mut request).await.unwrap();
        assert!(String::from_utf8_lossy(&request[..count]).contains("/api/v0/add"));
        arrived.send(()).unwrap();
        std::future::pending::<()>().await;
    });
    let child = command(&home, &url, None)
        .arg(&image)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    tokio::time::timeout(Duration::from_secs(10), received)
        .await
        .unwrap()
        .unwrap();
    let status = Command::new("kill")
        .args(["-INT", &child.id().unwrap().to_string()])
        .status()
        .unwrap();
    assert!(status.success());
    let output = tokio::time::timeout(Duration::from_secs(10), child.wait_with_output())
        .await
        .expect("SIGINT must interrupt source import and finish local cleanup")
        .unwrap();
    server.abort();
    assert!(!output.status.success());
    assert!(output.stdout.is_empty());
    let diagnostic = String::from_utf8_lossy(&output.stderr);
    assert!(diagnostic.contains("cancel"));
    assert!(
        diagnostic.contains("publication outcome is uncertain"),
        "{diagnostic}"
    );
    assert!(
        diagnostic.contains("may have left blocks or pins"),
        "{diagnostic}"
    );
    assert!(!home.join(".ww/ipns").exists());
}

#[tokio::test]
async fn host_owned_namespace_mismatch_fails_without_publication() {
    let home = tempfile::tempdir().unwrap();
    let home = real_path(&home);
    let baseline = home.join("baseline");
    write(
        &baseline,
        "boot/kernel.wasm",
        b"must never reach compilation",
    );
    let key = ww::keys::generate().unwrap();
    let name = ww::keys::to_libp2p(&key).unwrap().public().to_peer_id();
    let identity = home.join("identity.key");
    ww::keys::save(&key, &identity).unwrap();
    let key_before = std::fs::read(&identity).unwrap();
    let expected = Cid::new_v1(
        0x55,
        cid::multihash::Multihash::<64>::wrap(0, b"expected").unwrap(),
    );
    let observed = Cid::new_v1(
        0x55,
        cid::multihash::Multihash::<64>::wrap(0, b"observed").unwrap(),
    );
    let config = home.join("config");
    write(
        &config,
        "etc/ns/ww",
        format!("ipns={name}\nbootstrap=/ipfs/{expected}\n").as_bytes(),
    );
    let requests = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let observed_requests = requests.clone();
    let app = axum::Router::new().fallback(move |uri: axum::http::Uri| {
        let requests = observed_requests.clone();
        async move {
            requests.lock().unwrap().push(uri.path().to_owned());
            axum::Json(serde_json::json!({ "Path": format!("/ipfs/{observed}") }))
        }
    });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let output = run(command(&home, &url, Some(&baseline))
        .arg("--namespace-root")
        .arg(&config)
        .arg("--identity")
        .arg(&identity))
    .await;
    server.abort();
    assert!(!output.status.success());
    assert!(output.stdout.is_empty());
    assert!(String::from_utf8_lossy(&output.stderr)
        .contains("host-owned ww namespace does not resolve"));
    assert_eq!(*requests.lock().unwrap(), ["/api/v0/name/resolve"]);
    assert_eq!(std::fs::read(identity).unwrap(), key_before);
    assert!(!home.join(".ww/ipns").exists());
}

#[tokio::test]
async fn unavailable_namespace_cannot_succeed_with_baseline_only() {
    let home = tempfile::tempdir().unwrap();
    let home = real_path(&home);
    let baseline = home.join("baseline");
    write(
        &baseline,
        "boot/kernel.wasm",
        b"must never reach compilation",
    );
    let name = libp2p::identity::Keypair::generate_ed25519()
        .public()
        .to_peer_id();
    let config = home.join("config");
    write(
        &config,
        "etc/ns/application",
        format!("ipns={name}\n").as_bytes(),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let app = axum::Router::new().fallback(|| async {
        (
            axum::http::StatusCode::SERVICE_UNAVAILABLE,
            "routing unavailable",
        )
    });
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let output = run(command(&home, &url, Some(&baseline))
        .arg("--namespace-root")
        .arg(&config))
    .await;
    server.abort();
    assert!(!output.status.success());
    assert!(output.stdout.is_empty());
    assert!(String::from_utf8_lossy(&output.stderr).contains("cannot validate a partial image"));
}

#[tokio::test]
async fn finalized_atom_head_matches_preparation_and_invalid_head_cannot_fall_back() {
    let Some(kubo) = Kubo::start() else { return };
    let home = tempfile::tempdir().unwrap();
    let home = real_path(&home);
    let baseline = home.join("baseline");
    let bytes = kernel();
    write(&baseline, "boot/kernel.wasm", &bytes);
    let baseline_cid = kubo.add(&baseline).await;
    let baseline = frozen(&kubo, baseline_cid, "/").await;
    let head = home.join("head");
    write(&head, "etc/selected", b"finalized Atom head");
    let head_cid = kubo.add(&head).await;
    for (selected, valid) in [(head_cid.to_bytes(), true), (b"not-a-cid".to_vec(), false)] {
        let mut encoded = vec![0_u8; 96 + selected.len().div_ceil(32) * 32];
        encoded[31] = 9; // Atom revision.
        encoded[63] = 64; // Dynamic CID bytes offset.
        encoded[88..96].copy_from_slice(&(selected.len() as u64).to_be_bytes());
        encoded[96..96 + selected.len()].copy_from_slice(&selected);
        let encoded = format!("0x{}", hex::encode(encoded));
        let requests = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let observed = requests.clone();
        let app = axum::Router::new().fallback(move |axum::Json(request): axum::Json<Value>| {
            let encoded = encoded.clone();
            let observed = observed.clone();
            async move {
                observed.lock().unwrap().push(request.clone());
                let result = match request["method"].as_str().unwrap() {
                    "eth_blockNumber" => "0xa".to_owned(),
                    "eth_call" => {
                        assert_eq!(request["params"][1], "0x4");
                        encoded
                    }
                    method => panic!("unexpected Atom request {method}"),
                };
                axum::Json(
                    serde_json::json!({"jsonrpc": "2.0", "id": request["id"], "result": result}),
                )
            }
        });
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let rpc = format!("http://{}", listener.local_addr().unwrap());
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let output = run(
            command(&home, &kubo.url, Some(&home.join("baseline"))).args([
                "--stem",
                "0x1111111111111111111111111111111111111111",
                "--rpc-url",
                &rpc,
            ]),
        )
        .await;
        server.abort();
        assert_eq!(
            requests.lock().unwrap().len(),
            2,
            "check must resolve only one finalized head"
        );
        if valid {
            let checked = report(&output);
            let (_sender, mut cancel) = tokio::sync::watch::channel(false);
            let expected = ww::image::compose_root(
                Some(head_cid),
                Some(&baseline),
                &[],
                &kubo.client(),
                &mut cancel,
            )
            .await
            .unwrap();
            assert_eq!(checked["effective_root"], expected.to_string());
            assert_eq!(
                checked["kernel_runtime"],
                ww::kernel::runtime_cid(&bytes).to_string()
            );
            assert_eq!(
                kubo.bytes(&["cat", &format!("/ipfs/{expected}/etc/selected")]),
                b"finalized Atom head"
            );
        } else {
            assert!(!output.status.success());
            assert!(output.stdout.is_empty());
            assert!(String::from_utf8_lossy(&output.stderr).contains("invalid authoritative head"));
        }
    }
}

#[tokio::test]
async fn uncertain_pin_and_car_acknowledgements_retain_real_pins_without_unpin_requests() {
    let Some(kubo) = Kubo::start() else { return };
    let home = tempfile::tempdir().unwrap();
    let home = real_path(&home);
    let image = home.join("image");
    write(
        &image,
        "boot/kernel.wasm",
        b"never reaches static validation",
    );
    let root = kubo.add(&image).await;
    for (failed_path, status, malformed) in [
        (
            "/api/v0/pin/add",
            axum::http::StatusCode::INTERNAL_SERVER_ERROR,
            false,
        ),
        (
            "/api/v0/dag/import",
            axum::http::StatusCode::INTERNAL_SERVER_ERROR,
            false,
        ),
        ("/api/v0/dag/import", axum::http::StatusCode::OK, true),
    ] {
        let requests = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let observed = requests.clone();
        let backend = kubo.url.clone();
        let client = reqwest::Client::builder().no_proxy().build().unwrap();
        let app = axum::Router::new().fallback(
            move |uri: axum::http::Uri, headers: axum::http::HeaderMap, body: axum::body::Bytes| {
                let observed = observed.clone();
                let client = client.clone();
                let backend = backend.clone();
                async move {
                    observed.lock().unwrap().push(uri.path().to_owned());
                    // Unexpected cleanup requests are recorded but never forwarded.
                    if !matches!(uri.path(), "/api/v0/pin/add" | "/api/v0/dag/import") {
                        return (
                            axum::http::StatusCode::BAD_REQUEST,
                            b"unexpected request".to_vec(),
                        );
                    }
                    let mut forward = client
                        .post(format!("{backend}{}", uri.path_and_query().unwrap()))
                        .body(body);
                    if let Some(content_type) = headers.get("content-type") {
                        forward = forward.header("content-type", content_type);
                    }
                    let response = forward.send().await.unwrap();
                    assert!(
                        response.status().is_success(),
                        "real Kubo rejected forwarded request"
                    );
                    let body = response.bytes().await.unwrap().to_vec();
                    if uri.path() == failed_path {
                        // Kubo completed the operation. The CLI receives either a
                        // failure status or an invalid acknowledgement for that pin.
                        (
                            status,
                            if malformed {
                                b"{}\n".to_vec()
                            } else {
                                b"acknowledgement lost after backend completion".to_vec()
                            },
                        )
                    } else {
                        (axum::http::StatusCode::OK, body)
                    }
                }
            },
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let output = run(command(&home, &url, None).arg(format!("/ipfs/{root}"))).await;
        server.abort();
        assert!(!output.status.success());
        assert!(output.stdout.is_empty());
        let diagnostic = String::from_utf8_lossy(&output.stderr);
        assert!(
            diagnostic.contains("publication outcome is uncertain"),
            "{diagnostic}"
        );
        assert!(
            diagnostic.contains("may have left blocks or pins"),
            "{diagnostic}"
        );
        let expected = if failed_path == "/api/v0/pin/add" {
            vec!["/api/v0/pin/add"]
        } else {
            vec!["/api/v0/pin/add", "/api/v0/dag/import"]
        };
        assert_eq!(
            *requests.lock().unwrap(),
            expected,
            "failed acknowledgement must not trigger pin/rm or retry"
        );
        kubo.bytes(&["repo", "gc"]);
        kubo.bytes(&["pin", "ls", "--type=recursive", &root.to_string()]);
        assert_eq!(
            kubo.bytes(&["cat", &format!("/ipfs/{root}/boot/kernel.wasm")]),
            b"never reaches static validation"
        );
    }
}

#[cfg(unix)]
#[tokio::test]
async fn sigint_cancels_stalled_head_and_namespace_resolution() {
    for stage in ["head", "namespace"] {
        let home = tempfile::tempdir().unwrap();
        let home = real_path(&home);
        let baseline = home.join("baseline");
        write(&baseline, "boot/kernel.wasm", b"never reaches capture");
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let (arrived, received) = tokio::sync::oneshot::channel();
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut request = [0; 4096];
            let count = stream.read(&mut request).await.unwrap();
            let request = String::from_utf8_lossy(&request[..count]);
            if stage == "head" {
                assert!(request.contains("eth_blockNumber"), "{request}");
            } else {
                assert!(request.contains("/api/v0/name/resolve"), "{request}");
            }
            arrived.send(()).unwrap();
            std::future::pending::<()>().await;
        });
        let mut check = command(&home, &url, Some(&baseline));
        if stage == "head" {
            check.args([
                "--stem",
                "0x1111111111111111111111111111111111111111",
                "--rpc-url",
                &url,
            ]);
        } else {
            let config = home.join("config");
            let name = libp2p::identity::Keypair::generate_ed25519()
                .public()
                .to_peer_id();
            write(
                &config,
                "etc/ns/application",
                format!("ipns={name}\n").as_bytes(),
            );
            check.arg("--namespace-root").arg(config);
        }
        let child = check
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        tokio::time::timeout(Duration::from_secs(10), received)
            .await
            .unwrap()
            .unwrap();
        assert!(Command::new("kill")
            .args(["-INT", &child.id().unwrap().to_string()])
            .status()
            .unwrap()
            .success());
        let output = tokio::time::timeout(Duration::from_secs(10), child.wait_with_output())
            .await
            .expect("SIGINT must interrupt mutable source resolution")
            .unwrap();
        server.abort();
        assert!(!output.status.success());
        assert!(output.stdout.is_empty());
        let diagnostic = String::from_utf8_lossy(&output.stderr);
        assert!(diagnostic.contains("cancel"), "{stage}: {diagnostic}");
        assert!(
            diagnostic.contains(if stage == "head" {
                "authoritative head resolution"
            } else {
                "namespace resolution"
            }),
            "{stage}: {diagnostic}"
        );
        assert!(
            !home.join(".ww").exists(),
            "resolution cancellation created follower state"
        );
    }
}
