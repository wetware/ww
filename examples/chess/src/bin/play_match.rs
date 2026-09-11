//! Generate a chess match PGN by running two real Wetware nodes.
//!
//! Each node runs the chess guest in "serve" mode. They discover each
//! other via DHT, connect over libp2p, and play a game over Cap'n Proto
//! RPC. The resulting PGN is extracted from the node logs.
//!
//! Usage:
//!   cargo run -p chess --bin play_match              # PGN to stdout
//!   cargo run -p chess --bin play_match -- game.pgn  # PGN to file

use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::thread;

/// Build and locate the ww binary used to run the demo nodes.
///
/// Cargo does not rebuild this separate binary while compiling `play_match`,
/// so refresh it here before selecting the debug artifact.
fn find_ww_binary() -> PathBuf {
    let status = Command::new("cargo")
        .args(["build", "-p", "ww", "--bin", "ww"])
        .stdout(Stdio::null())
        .status()
        .unwrap_or_else(|e| panic!("failed to build ww binary: {e}"));
    if !status.success() {
        panic!("failed to build ww binary");
    }

    for candidate in ["target/debug/ww", "target/release/ww"] {
        let p = PathBuf::from(candidate);
        if p.exists() {
            return p;
        }
    }
    PathBuf::from("ww")
}

/// Set up a node directory with the chess binary at boot/main.wasm.
/// Returns a temp directory that must stay alive for the duration.
fn setup_node_dir(chess_wasm: &Path, node_name: &str) -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap_or_else(|e| {
        panic!("failed to create temp dir for {node_name}: {e}");
    });
    let boot_dir = dir.path().join("boot");
    std::fs::create_dir_all(&boot_dir).expect("create boot dir");
    std::fs::copy(chess_wasm, boot_dir.join("main.wasm")).expect("copy chess wasm");
    dir
}

fn create_identity(identity_dir: &Path, node_name: &str) -> (PathBuf, String) {
    let path = identity_dir.join("identity");
    let key = ww::keys::generate()
        .unwrap_or_else(|e| panic!("failed to generate identity for node {node_name}: {e}"));
    ww::keys::save(&key, &path)
        .unwrap_or_else(|e| panic!("failed to save identity for node {node_name}: {e}"));
    let peer_id = ww::keys::to_libp2p(&key)
        .unwrap_or_else(|e| panic!("failed to derive peer ID for node {node_name}: {e}"))
        .public()
        .to_peer_id();
    (path, peer_id.to_string())
}

fn spawn_node(
    ww_bin: &Path,
    port: u16,
    node_dir: &Path,
    identity: &Path,
    bootstrap: &str,
) -> Child {
    Command::new(ww_bin)
        .args([
            "run",
            "--listen",
            &format!("/ip4/127.0.0.1/tcp/{port}"),
            "--identity",
            &identity.to_string_lossy(),
            "--bootstrap",
            bootstrap,
            "--with-http-admin=off",
            "std/kernel",
            "std/status",
            &node_dir.to_string_lossy(),
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap_or_else(|e| panic!("failed to start ww on port {port}: {e}"))
}

fn main() {
    let output_path = std::env::args().nth(1);
    let ww_bin = find_ww_binary();

    // Locate the chess WASM binary.
    let chess_wasm = PathBuf::from("examples/chess/bin/chess-demo.wasm");
    if !chess_wasm.exists() {
        eprintln!(
            "Chess binary not found at {}. Run `make chess` first.",
            chess_wasm.display()
        );
        std::process::exit(1);
    }

    eprintln!("Using ww binary: {}", ww_bin.display());

    // Set up node directories with chess binary at boot/main.wasm.
    let node_a_dir = setup_node_dir(&chess_wasm, "A");
    let node_b_dir = setup_node_dir(&chess_wasm, "B");
    let identity_a_dir = tempfile::tempdir().expect("create identity directory for A");
    let identity_b_dir = tempfile::tempdir().expect("create identity directory for B");
    let (identity_a, peer_a) = create_identity(identity_a_dir.path(), "A");
    let (identity_b, peer_b) = create_identity(identity_b_dir.path(), "B");
    let bootstrap_a = format!("/ip4/127.0.0.1/tcp/2041/p2p/{peer_b}");
    let bootstrap_b = format!("/ip4/127.0.0.1/tcp/2040/p2p/{peer_a}");

    eprintln!("Starting two Wetware nodes...");
    let mut node_a = spawn_node(&ww_bin, 2040, node_a_dir.path(), &identity_a, &bootstrap_a);
    let mut node_b = spawn_node(&ww_bin, 2041, node_b_dir.path(), &identity_b, &bootstrap_b);
    eprintln!("Node A (pid {}): port 2040", node_a.id());
    eprintln!("Node B (pid {}): port 2041", node_b.id());
    eprintln!("Waiting for discovery and game...");

    let (pgn_tx, pgn_rx) = mpsc::channel::<String>();

    let stderr_a = node_a.stderr.take().expect("stderr A");
    let tx_a = pgn_tx.clone();
    thread::spawn(move || monitor_stderr("A", stderr_a, tx_a));
    let stdout_a = node_a.stdout.take().expect("stdout A");
    let tx_a = pgn_tx.clone();
    thread::spawn(move || monitor_stderr("A", stdout_a, tx_a));

    let stderr_b = node_b.stderr.take().expect("stderr B");
    let tx_b = pgn_tx.clone();
    thread::spawn(move || monitor_stderr("B", stderr_b, tx_b));
    let stdout_b = node_b.stdout.take().expect("stdout B");
    thread::spawn(move || monitor_stderr("B", stdout_b, pgn_tx));

    let pgn = match pgn_rx.recv_timeout(std::time::Duration::from_secs(180)) {
        Ok(pgn) => pgn,
        Err(_) => {
            eprintln!("Timeout: no game completed within 180 seconds.");
            let _ = node_a.kill();
            let _ = node_b.kill();
            std::process::exit(1);
        }
    };

    eprintln!("Game complete! Shutting down...");
    let _ = node_a.kill();
    let _ = node_b.kill();
    let _ = node_a.wait();
    let _ = node_b.wait();

    match output_path {
        Some(path) => {
            std::fs::write(&path, &pgn).expect("write PGN");
            eprintln!("PGN written to {path}");
        }
        None => {
            std::io::stdout().lock().write_all(pgn.as_bytes()).unwrap();
        }
    }
}

fn monitor_stderr(label: &str, stderr: impl std::io::Read, tx: mpsc::Sender<String>) {
    let reader = BufReader::new(stderr);
    let mut in_pgn = false;
    let mut pgn_buf = String::new();

    for line in reader.lines() {
        let line = match line {
            Ok(l) => l,
            Err(_) => break,
        };

        eprintln!("[Node {label}] {line}");

        if line.contains("---PGN_START---") {
            in_pgn = true;
            pgn_buf.clear();
            continue;
        }
        if in_pgn && line.contains("---PGN_END---") {
            let _ = tx.send(clean_pgn(&pgn_buf));
            // Keep draining the node's output until it exits. Returning here
            // closes the pipe while the guest may still emit completion logs.
            in_pgn = false;
            pgn_buf.clear();
            continue;
        }
        if in_pgn {
            let content = strip_log_prefix(&line);
            pgn_buf.push_str(&content);
            pgn_buf.push('\n');
        }
    }
}

/// Strip host tracing metadata and guest log prefixes from a PGN line.
fn strip_log_prefix(line: &str) -> String {
    let plain = strip_ansi(line);
    if let Some((_, message)) = plain.rsplit_once("ww::launcher: ") {
        return message.to_string();
    }
    if let Some((_, message)) = plain.rsplit_once("[INFO] ") {
        return message.to_string();
    }
    plain.trim().to_string()
}

/// Remove ANSI control sequences emitted by the tracing formatter.
fn strip_ansi(text: &str) -> String {
    let mut plain = String::with_capacity(text.len());
    let bytes = text.as_bytes();
    let mut index = 0;

    while index < bytes.len() {
        if bytes[index] == b'\x1b' && bytes.get(index + 1) == Some(&b'[') {
            index += 2;
            while index < bytes.len() && !(b'@'..=b'~').contains(&bytes[index]) {
                index += 1;
            }
            index += usize::from(index < bytes.len());
        } else {
            plain.push(bytes[index] as char);
            index += 1;
        }
    }

    plain
}

/// Trim trailing whitespace from every line and strip any fully empty
/// leading/trailing lines so the PGN is clean.
fn clean_pgn(raw: &str) -> String {
    let trimmed: Vec<&str> = raw.lines().map(|l| l.trim_end()).collect();
    let start = trimmed.iter().position(|l| !l.is_empty()).unwrap_or(0);
    let end = trimmed
        .iter()
        .rposition(|l| !l.is_empty())
        .map(|i| i + 1)
        .unwrap_or(0);
    let mut out: String = trimmed[start..end].join("\n");
    out.push('\n');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strips_tracing_metadata_from_pgn_lines() {
        let line = "\x1b[2m2026-09-09T20:53:37Z\x1b[0m \x1b[32m INFO\x1b[0m \
                    \x1b[2mww::launcher\x1b[0m\x1b[2m:\x1b[0m [Event \"Wetware Chess\"]";
        assert_eq!(strip_log_prefix(line), "[Event \"Wetware Chess\"]");
    }

    #[test]
    fn cleans_a_complete_traced_pgn() {
        let raw = "[Event \"Wetware Chess\"]\n\n1. e4 e5 1/2-1/2\n";
        assert_eq!(
            clean_pgn(raw),
            "[Event \"Wetware Chess\"]\n\n1. e4 e5 1/2-1/2\n"
        );
    }
}
