use std::path::{Path, PathBuf};
use std::process::{Command, Output};

#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

fn ww_bin() -> PathBuf {
    PathBuf::from(std::env::var_os("CARGO_BIN_EXE_ww").expect("CARGO_BIN_EXE_ww missing"))
}

fn ww_command(home: &Path, args: &[&str]) -> Command {
    let mut command = Command::new(ww_bin());
    command
        .args(args)
        .env("HOME", home)
        .env_remove("WW_IDENTITY");
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

fn service_arguments(service: &str) -> Vec<String> {
    if cfg!(target_os = "macos") {
        let program_arguments = service
            .split("<key>ProgramArguments</key>")
            .nth(1)
            .and_then(|tail| tail.split("</array>").next())
            .expect("launchd service must contain ProgramArguments");
        program_arguments
            .lines()
            .filter_map(|line| {
                line.trim()
                    .strip_prefix("<string>")
                    .and_then(|value| value.strip_suffix("</string>"))
                    .map(str::to_owned)
            })
            .collect()
    } else {
        service
            .lines()
            .find_map(|line| line.strip_prefix("ExecStart="))
            .expect("systemd service must contain ExecStart")
            .split_whitespace()
            .map(str::to_owned)
            .collect()
    }
}

fn image_arguments(arguments: &[String]) -> Vec<&str> {
    let mut images = Vec::new();
    let mut index = arguments
        .iter()
        .position(|argument| argument == "run")
        .expect("service command must invoke ww run")
        + 1;

    while index < arguments.len() {
        match arguments[index].as_str() {
            "--listen" | "--identity" | "--http-listen" | "--namespace-root" => {
                index += 2;
            }
            argument if argument.starts_with('-') => index += 1,
            image => {
                images.push(image);
                index += 1;
            }
        }
    }

    images
}

#[cfg(unix)]
fn embedded_images_hash() -> String {
    let mut hasher = blake3::Hasher::new();
    for path in ["std/kernel/bin/main.wasm", "std/status/bin/status.wasm"] {
        let path = Path::new(env!("CARGO_MANIFEST_DIR")).join(path);
        hasher.update(&std::fs::read(path).unwrap_or_default());
    }
    hasher.finalize().to_hex().to_string()
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

#[test]
fn daemon_install_writes_service_with_listen_args() {
    if !cfg!(target_os = "macos") && !cfg!(target_os = "linux") {
        eprintln!("SKIP: daemon service writer only supports macOS/Linux");
        return;
    }

    let home = tempfile::tempdir().expect("temp home");
    let output = run_ww(
        home.path(),
        &[
            "daemon",
            "install",
            "--listen",
            "/ip4/127.0.0.1/tcp/23025",
            "--listen",
            "/ip4/127.0.0.1/udp/23025/quic-v1",
        ],
    );

    assert!(
        output.status.success(),
        "daemon install failed: {}",
        stderr_text(&output)
    );

    let identity = home.path().join(".ww/identity");
    assert!(
        identity.exists(),
        "identity not created at {}",
        identity.display()
    );

    let service_path = if cfg!(target_os = "macos") {
        home.path().join("Library/LaunchAgents/io.wetware.ww.plist")
    } else {
        home.path().join(".config/systemd/user/ww.service")
    };

    let service = std::fs::read_to_string(&service_path)
        .unwrap_or_else(|e| panic!("failed reading {}: {e}", service_path.display()));

    assert!(service.contains("/ip4/127.0.0.1/tcp/23025"), "{service}");
    assert!(
        service.contains("/ip4/127.0.0.1/udp/23025/quic-v1"),
        "{service}"
    );
    assert!(service.contains("--identity"), "{service}");
}

#[cfg(unix)]
#[test]
fn perform_update_restarts_for_service_definition_change_only() {
    if !cfg!(target_os = "macos") && !cfg!(target_os = "linux") {
        eprintln!("SKIP: daemon service writer only supports macOS/Linux");
        return;
    }

    let home = tempfile::tempdir().expect("temp home");
    let install = run_ww(home.path(), &["daemon", "install"]);
    assert!(
        install.status.success(),
        "daemon install failed: {}",
        stderr_text(&install)
    );

    let ww_dir = home.path().join(".ww");
    std::fs::write(ww_dir.join(".last-std-cid"), embedded_images_hash())
        .expect("write current image marker");

    let service_path = if cfg!(target_os = "macos") {
        home.path().join("Library/LaunchAgents/io.wetware.ww.plist")
    } else {
        home.path().join(".config/systemd/user/ww.service")
    };
    let expected_service = std::fs::read(&service_path).expect("read safe service definition");
    std::fs::write(
        &service_path,
        b"stale service mounted the entire .ww directory\n",
    )
    .expect("write stale service definition");

    let tools_dir = home.path().join("test-tools");
    std::fs::create_dir(&tools_dir).expect("create service-manager stub directory");
    let service_manager = if cfg!(target_os = "macos") {
        "launchctl"
    } else {
        "systemctl"
    };
    let service_manager_path = tools_dir.join(service_manager);
    std::fs::write(
        &service_manager_path,
        "#!/bin/sh\nprintf '%s\\n' \"$*\" >> \"$WW_TEST_SERVICE_COMMAND_LOG\"\n",
    )
    .expect("write service-manager stub");
    let mut permissions = std::fs::metadata(&service_manager_path)
        .expect("read service-manager stub metadata")
        .permissions();
    permissions.set_mode(0o755);
    std::fs::set_permissions(&service_manager_path, permissions)
        .expect("make service-manager stub executable");

    let existing_path = std::env::var_os("PATH").unwrap_or_default();
    let path = std::env::join_paths(
        std::iter::once(tools_dir).chain(std::env::split_paths(&existing_path)),
    )
    .expect("build test PATH");
    let service_command_log = home.path().join("service-manager.log");

    let stale_update = ww_command(home.path(), &["perform", "update"])
        .env("PATH", &path)
        .env("WW_TEST_SERVICE_COMMAND_LOG", &service_command_log)
        .output()
        .expect("run update with stale service definition");
    assert!(
        stale_update.status.success(),
        "perform update failed: {}",
        stderr_text(&stale_update)
    );
    assert_eq!(
        std::fs::read(&service_path).expect("read rewritten service definition"),
        expected_service,
        "perform update should replace the stale service definition"
    );

    let restart_commands = std::fs::read_to_string(&service_command_log)
        .expect("service restart should invoke the service-manager stub");
    if cfg!(target_os = "macos") {
        assert!(restart_commands
            .lines()
            .any(|line| line.starts_with("load ")));
    } else {
        assert!(restart_commands
            .lines()
            .any(|line| line == "--user restart ww"));
    }

    std::fs::write(&service_command_log, "").expect("clear service-manager log");
    let unchanged_update = ww_command(home.path(), &["perform", "update"])
        .env("PATH", &path)
        .env("WW_TEST_SERVICE_COMMAND_LOG", &service_command_log)
        .output()
        .expect("run update with unchanged service definition");
    assert!(
        unchanged_update.status.success(),
        "unchanged perform update failed: {}",
        stderr_text(&unchanged_update)
    );
    assert!(
        String::from_utf8_lossy(&unchanged_update.stdout)
            .contains("Daemon restart (nothing changed)"),
        "unchanged update should skip the daemon restart: {}",
        String::from_utf8_lossy(&unchanged_update.stdout)
    );
    assert!(
        std::fs::read_to_string(&service_command_log)
            .expect("read service-manager log")
            .is_empty(),
        "unchanged update should not invoke the service manager"
    );
}

#[tokio::test]
async fn default_daemon_import_excludes_private_host_state() {
    if !cfg!(target_os = "macos") && !cfg!(target_os = "linux") {
        eprintln!("SKIP: daemon service writer only supports macOS/Linux");
        return;
    }

    let home = tempfile::tempdir().expect("temp home");
    let ww_dir = home.path().join(".ww");
    let fhs_dir = ww_dir.join("fhs");
    std::fs::create_dir_all(fhs_dir.join("svc")).expect("publishable FHS tree");
    std::fs::write(ww_dir.join("identity"), b"PRIVATE_HOST_IDENTITY_SENTINEL")
        .expect("identity fixture");
    std::fs::write(
        ww_dir.join("durable.ipns-record"),
        b"PRIVATE_MUTABLE_STATE_SENTINEL",
    )
    .expect("private state fixture");
    std::fs::write(
        fhs_dir.join("svc/deployable.txt"),
        b"INTENDED_DEPLOYABLE_CONTENT",
    )
    .expect("deployable fixture");

    let output = run_ww(home.path(), &["daemon", "install"]);
    assert!(
        output.status.success(),
        "daemon install failed: {}",
        stderr_text(&output)
    );

    let service_path = if cfg!(target_os = "macos") {
        home.path().join("Library/LaunchAgents/io.wetware.ww.plist")
    } else {
        home.path().join(".config/systemd/user/ww.service")
    };
    let service = std::fs::read_to_string(&service_path)
        .unwrap_or_else(|error| panic!("failed reading {}: {error}", service_path.display()));
    let arguments = service_arguments(&service);
    let images = image_arguments(&arguments);
    assert_eq!(images, [fhs_dir.to_str().expect("UTF-8 test path")]);
    assert!(
        arguments.windows(2).any(|pair| {
            pair[0] == "--namespace-root" && pair[1] == ww_dir.to_str().expect("UTF-8 test path")
        }),
        "service must read namespace state without mounting it: {arguments:?}"
    );

    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind fake Kubo");
    let address = listener.local_addr().expect("fake Kubo address");
    let server = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.expect("accept add request");
        let request = read_http_request(&mut stream).await;
        let body = b"{\"Name\":\"\",\"Hash\":\"test-root\",\"Size\":\"0\"}\n";
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
        .add_dir(Path::new(images[0]))
        .await
        .expect("import daemon FHS root");
    assert_eq!(root, "test-root");
    let request = server.await.expect("fake Kubo task");
    let request = String::from_utf8_lossy(&request);
    assert!(
        request.contains("filename=\"svc/deployable.txt\"")
            && request.contains("INTENDED_DEPLOYABLE_CONTENT"),
        "intended deployable file missing from import request: {request}"
    );
    assert!(!request.contains("filename=\"identity\""), "{request}");
    assert!(
        !request.contains("PRIVATE_HOST_IDENTITY_SENTINEL"),
        "{request}"
    );
    assert!(
        !request.contains("filename=\"durable.ipns-record\""),
        "{request}"
    );
    assert!(
        !request.contains("PRIVATE_MUTABLE_STATE_SENTINEL"),
        "{request}"
    );
}
