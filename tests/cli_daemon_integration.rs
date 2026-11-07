//! CLI operation boundaries. Service lifecycle mutation uses fake managers in
//! src/cli/daemon_cmd_tests.rs; this suite never invokes a real service manager.
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

fn ww_bin() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_ww"))
}

fn ww_command(home: &Path, args: &[&str]) -> Command {
    let mut command = Command::new(ww_bin());
    for (key, _) in std::env::vars_os() {
        if key.to_string_lossy().starts_with("WW_") || key.to_string_lossy().starts_with("IPFS_") {
            command.env_remove(key);
        }
    }
    command
        .args(args)
        .env("HOME", home)
        .env("IPFS_API", "http://127.0.0.1:1")
        .env("IPFS_ROUTING_API", "http://127.0.0.1:1");
    command
}

fn run_ww(home: &Path, args: &[&str]) -> Output {
    ww_command(home, args)
        .output()
        .expect("failed to execute ww")
}

fn stderr_text(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).to_string()
}

fn prepare_fhs_fixture(home: &Path) {
    // The import boundary does not need an installation receipt. Stage public
    // payloads and a private identity without selecting a release or a service.
    for (source, target) in [
        ("std/kernel/bin/main.wasm", "fhs/boot/kernel.wasm"),
        ("std/status/bin/status.wasm", "fhs/bin/status.wasm"),
    ] {
        let destination = home.join(".ww").join(target);
        std::fs::create_dir_all(destination.parent().unwrap()).unwrap();
        std::fs::copy(
            Path::new(env!("CARGO_MANIFEST_DIR")).join(source),
            destination,
        )
        .expect("build canonical payloads with make fhs before CLI integration tests");
    }
    let identity = home.join(".ww/identity");
    let output = run_ww(home, &["keygen", "--output", identity.to_str().unwrap()]);
    assert!(output.status.success(), "{}", stderr_text(&output));
}

#[tokio::test]
async fn unsigned_install_refuses_without_contacting_kubo_or_registering_a_service() {
    let home = tempfile::tempdir().unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let command = ww_command(home.path(), &["install", "--json"]);
    let mut command = tokio::process::Command::from(command);
    command
        .env("IPFS_API", &url)
        .env("IPFS_ROUTING_API", &url)
        .kill_on_drop(true);
    let output = tokio::time::timeout(std::time::Duration::from_secs(30), command.output())
        .await
        .expect("offline installation must not wait for dependencies")
        .unwrap();
    assert!(
        !output.status.success(),
        "unsigned fresh installation must fail"
    );
    assert!(!home.path().join(".ww").exists());
    assert!(!home.path().join("Library").exists());
    assert!(!home.path().join(".config").exists());
    assert!(
        tokio::time::timeout(std::time::Duration::from_millis(20), listener.accept())
            .await
            .is_err()
    );
}

#[test]
fn daemon_registration_requires_existing_identity_and_fhs() {
    let home = tempfile::tempdir().unwrap();
    let output = run_ww(home.path(), &["daemon", "install", "--check"]);
    assert!(!output.status.success());
    assert!(
        stderr_text(&output).contains("identity"),
        "{}",
        stderr_text(&output)
    );
    assert!(!home.path().join(".ww").exists());

    let identity = home.path().join(".ww/identity");
    let output = run_ww(
        home.path(),
        &["keygen", "--output", identity.to_str().unwrap()],
    );
    assert!(output.status.success(), "{}", stderr_text(&output));
    let before = std::fs::read(&identity).unwrap();
    let output = run_ww(home.path(), &["daemon", "install", "--check"]);
    assert!(!output.status.success());
    assert!(
        stderr_text(&output).contains("FHS"),
        "{}",
        stderr_text(&output)
    );
    assert_eq!(std::fs::read(identity).unwrap(), before);
    assert!(!home.path().join(".ww/fhs").exists());
    assert!(!home.path().join("Library").exists());
    assert!(!home.path().join(".config").exists());
}

#[test]
fn daemon_registration_rejects_ephemeral_identity_before_manager_observation() {
    let home = tempfile::tempdir().unwrap();
    prepare_fhs_fixture(home.path());
    let identity = home.path().join(".ww/identity");
    let before = std::fs::read(&identity).unwrap();
    let output = run_ww(
        home.path(),
        &["daemon", "install", "--insecure-ephemeral", "--check"],
    );
    assert!(!output.status.success());
    assert!(
        stderr_text(&output).contains("persistent identity"),
        "{}",
        stderr_text(&output)
    );
    assert_eq!(std::fs::read(identity).unwrap(), before);
    assert!(!home.path().join(".ww/daemon.json").exists());
}

async fn read_http_request(stream: &mut TcpStream) -> Vec<u8> {
    let mut request = Vec::new();
    let mut expected_len = None;

    loop {
        let mut chunk = [0_u8; 4096];
        let read = stream.read(&mut chunk).await.expect("read request");
        assert_ne!(read, 0, "request ended before the complete body arrived");
        request.extend_from_slice(&chunk[..read]);

        if expected_len.is_none() {
            if let Some(header_end) = request.windows(4).position(|window| window == b"\r\n\r\n") {
                let headers = String::from_utf8_lossy(&request[..header_end]);
                let content_len = headers
                    .lines()
                    .find_map(|line| {
                        line.to_ascii_lowercase()
                            .strip_prefix("content-length:")
                            .map(str::trim)
                            .map(str::parse::<usize>)
                    })
                    .transpose()
                    .expect("valid Content-Length")
                    .expect("multipart request must include Content-Length");
                expected_len = Some(header_end + 4 + content_len);
            }
        }

        if expected_len.is_some_and(|length| request.len() >= length) {
            return request;
        }
    }
}

#[tokio::test]
async fn installed_fhs_import_excludes_private_host_state() {
    if !cfg!(target_os = "macos") && !cfg!(target_os = "linux") {
        eprintln!("SKIP: daemon service writer only supports macOS/Linux");
        return;
    }

    let home = tempfile::tempdir().expect("temp home");
    prepare_fhs_fixture(home.path());
    let ww_dir = home.path().join(".ww");
    let fhs_dir = ww_dir.join("fhs");
    std::fs::create_dir_all(fhs_dir.join("svc")).expect("publishable FHS tree");
    let identity_sentinel = std::fs::read_to_string(ww_dir.join("identity")).unwrap();
    std::fs::create_dir_all(ww_dir.join("ipns/follow")).expect("follower state directory");
    std::fs::create_dir_all(ww_dir.join("ipns/publish")).expect("publisher state directory");
    std::fs::write(
        ww_dir.join("ipns/follow/name.record"),
        b"PRIVATE_FOLLOWER_WATERMARK_SENTINEL",
    )
    .expect("follower watermark fixture");
    std::fs::write(
        ww_dir.join("ipns/publish/name.record"),
        b"PRIVATE_PUBLISHER_RECORD_SENTINEL",
    )
    .expect("publisher record fixture");
    std::fs::write(
        fhs_dir.join("svc/deployable.txt"),
        b"INTENDED_DEPLOYABLE_CONTENT",
    )
    .expect("deployable fixture");

    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind fake Kubo");
    let address = listener.local_addr().expect("fake Kubo address");
    let server = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.expect("accept add request");
        let request = read_http_request(&mut stream).await;
        let body = b"{\"Name\":\"\",\"Hash\":\"bafkreibm6jg3ux5quy7flfgn5gmxk5ubm6yur3apcu3to3d6tmjzptm2ye\",\"Size\":\"0\"}\n";
        let response = format!(
            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            body.len()
        );
        stream
            .write_all(response.as_bytes())
            .await
            .expect("write response headers");
        stream.write_all(body).await.expect("write response body");
        request
    });

    let client = ww::ipfs::HttpClient::new(format!("http://{address}"));
    let root = client
        .add_dir(&fhs_dir)
        .await
        .expect("import daemon FHS root");
    assert_eq!(
        root,
        "bafkreibm6jg3ux5quy7flfgn5gmxk5ubm6yur3apcu3to3d6tmjzptm2ye"
    );
    let request = server.await.expect("fake Kubo task");
    let request = String::from_utf8_lossy(&request);
    assert!(
        request.contains("filename=\"svc/deployable.txt\"")
            && request.contains("INTENDED_DEPLOYABLE_CONTENT"),
        "intended deployable file missing from import request: {request}"
    );
    assert!(!request.contains("filename=\"identity\""), "{request}");
    assert!(!request.contains(&identity_sentinel), "{request}");
    assert!(
        !request.contains("filename=\"ipns/follow/name.record\""),
        "{request}"
    );
    assert!(
        !request.contains("PRIVATE_FOLLOWER_WATERMARK_SENTINEL"),
        "{request}"
    );
    assert!(
        !request.contains("filename=\"ipns/publish/name.record\""),
        "{request}"
    );
    assert!(
        !request.contains("PRIVATE_PUBLISHER_RECORD_SENTINEL"),
        "{request}"
    );
}

#[tokio::test]
async fn bare_run_from_private_cwd_fails_without_contacting_kubo() {
    let home = tempfile::tempdir().unwrap();
    let private = home.path().join("private");
    std::fs::create_dir(&private).unwrap();
    std::fs::write(
        private.join("identity"),
        b"private material must stay local",
    )
    .unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let mut command = ww_command(home.path(), &["run", "--ipfs-url", &url]);
    command.current_dir(&private);
    let mut command = tokio::process::Command::from(command);
    command.kill_on_drop(true);
    let output = tokio::time::timeout(std::time::Duration::from_secs(5), command.output())
        .await
        .expect("bare startup must fail before waiting for dependencies")
        .unwrap();
    assert!(!output.status.success());
    assert!(stderr_text(&output).contains("No installed FHS"));
    assert!(
        tokio::time::timeout(std::time::Duration::from_millis(20), listener.accept())
            .await
            .is_err()
    );
    assert!(
        !home.path().join(".ww").exists(),
        "bare startup must not initialize Host state"
    );
}

#[test]
fn configured_fhs_rejects_private_installation_roots() {
    let home = tempfile::tempdir().unwrap();
    let private = home.path().join(".ww");
    std::fs::create_dir(&private).unwrap();
    for root in [home.path(), private.as_path()] {
        let output = run_ww(home.path(), &["run", "--fhs-root", root.to_str().unwrap()]);
        assert!(!output.status.success());
        let error = stderr_text(&output);
        assert!(error.contains("private installation state"), "{error}");
        assert!(!private.join("identity").exists());
    }
}
