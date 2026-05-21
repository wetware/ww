use std::path::{Path, PathBuf};
use std::process::{Command, Output};

fn ww_bin() -> PathBuf {
    PathBuf::from(std::env::var_os("CARGO_BIN_EXE_ww").expect("CARGO_BIN_EXE_ww missing"))
}

fn run_ww(home: &Path, args: &[&str]) -> Output {
    Command::new(ww_bin())
        .args(args)
        .env("HOME", home)
        .env_remove("WW_IDENTITY")
        .output()
        .expect("failed to execute ww")
}

fn stderr_text(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).to_string()
}

#[test]
fn shell_rejects_select_with_explicit_addr() {
    let home = tempfile::tempdir().expect("temp home");
    let output = run_ww(
        home.path(),
        &[
            "shell",
            "/ip4/127.0.0.1/tcp/2025/p2p/12D3KooWJ3qM19qUUj8JdT9kPEg6VZLoes6eexfUYd6Xn7SPrf8n",
            "--select",
            "1",
        ],
    );

    assert!(!output.status.success(), "unexpected success");
    let stderr = stderr_text(&output);
    assert!(stderr.contains("cannot be used with"), "stderr: {stderr}");
    assert!(stderr.contains("--select"), "stderr: {stderr}");
}

#[test]
fn shell_requires_persistent_identity_even_with_select() {
    let home = tempfile::tempdir().expect("temp home");
    let output = run_ww(home.path(), &["shell", "--select", "1"]);

    assert!(!output.status.success(), "unexpected success");
    let stderr = stderr_text(&output);
    assert!(
        stderr.contains("requires a persistent identity to authenticate"),
        "stderr: {stderr}"
    );
    assert!(
        stderr.contains("ww keygen > ~/.ww/identity"),
        "stderr: {stderr}"
    );
}

#[test]
fn daemon_install_writes_service_with_listen_args_and_no_config_glia_dependency() {
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
    assert!(
        !service.contains("config.glia"),
        "service should not reference config.glia: {service}"
    );
}
