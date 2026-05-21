use std::path::{Path, PathBuf};
use std::process::{Command, Output};

fn ww_bin() -> PathBuf {
    PathBuf::from(std::env::var_os("CARGO_BIN_EXE_ww").expect("CARGO_BIN_EXE_ww missing"))
}

fn make_identity(home: &Path) {
    let ww_dir = home.join(".ww");
    std::fs::create_dir_all(&ww_dir).expect("create ~/.ww");
    let path = ww_dir.join("identity");
    let sk = ww::keys::generate().expect("generate identity");
    ww::keys::save(&sk, &path).expect("save identity");
}

fn run_ww(home: &Path, args: &[&str], extra_env: &[(&str, &str)]) -> Output {
    let mut cmd = Command::new(ww_bin());
    cmd.args(args)
        .env("HOME", home)
        .env_remove("WW_IDENTITY")
        .env_remove("WW_TEST_MDNS_CANDIDATES");

    for (k, v) in extra_env {
        cmd.env(k, v);
    }

    cmd.output().expect("failed to execute ww")
}

fn stderr_text(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).to_string()
}

#[test]
fn shell_nontty_ambiguous_discovery_suggests_select_or_addr() {
    let home = tempfile::tempdir().expect("temp home");
    make_identity(home.path());

    let discovery_json = r#"[
        {
            "peer_id": "12D3KooWJ3qM19qUUj8JdT9kPEg6VZLoes6eexfUYd6Xn7SPrf8n",
            "addrs": ["/ip4/127.0.0.1/tcp/2025"]
        },
        {
            "peer_id": "12D3KooWQdQnZYK7hX8Q2Yb8qXWQYvdr4jRWk6TUhSxvVmF5vU3P",
            "addrs": ["/ip4/127.0.0.1/tcp/2121"]
        }
    ]"#;

    let output = run_ww(
        home.path(),
        &["shell"],
        &[("WW_TEST_MDNS_CANDIDATES", discovery_json)],
    );

    assert!(!output.status.success(), "unexpected success");
    let stderr = stderr_text(&output);
    assert!(
        stderr.contains("Multiple wetware hosts discovered via mDNS; refusing to guess"),
        "stderr: {stderr}"
    );
    assert!(stderr.contains("--select <index|peer-id>"), "stderr: {stderr}");
    assert!(
        stderr.contains("ww shell <multiaddr>"),
        "stderr: {stderr}"
    );
}
