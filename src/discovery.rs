//! Local node discovery via Unix Domain Sockets and LAN Kademlia DHT.
//!
//! Running daemons open a UDS at `<run-dir>/<peer-id>.sock` (created by
//! [`AdminUdsService`](crate::admin_uds::AdminUdsService)) plus a metadata
//! JSON at `<run-dir>/<peer-id>.json`. Clients enumerate `*.sock` entries
//! to find live local daemons; the `.json` carries peer_id / multiaddrs /
//! pid / started_at / version for tooling consumers.
//!
//! The DHT discovery CID is also provided for LAN Kademlia advertisement.
//! That path is orthogonal to local discovery and unaffected by the
//! UDS migration.
#![cfg(not(target_arch = "wasm32"))]

use std::path::PathBuf;
use std::sync::LazyLock;

/// Well-known CID that wetware nodes provide on the LAN DHT.
///
/// Computed as `CIDv1(raw, BLAKE3(b"wetware"))`.  Any peer providing
/// this key is advertising itself as a wetware host.
pub static DISCOVERY_CID: LazyLock<cid::Cid> = LazyLock::new(|| {
    let digest = blake3::hash(b"wetware");
    let mh = cid::multihash::Multihash::<64>::wrap(0x1e, digest.as_bytes())
        .expect("blake3 digest always fits in 64-byte multihash");
    cid::Cid::new_v1(0x55, mh)
});

/// The discovery CID as a Kad record key (raw CID bytes).
pub fn discovery_record_key() -> libp2p::kad::RecordKey {
    libp2p::kad::RecordKey::new(&DISCOVERY_CID.to_bytes())
}

/// Candidate directories where running daemons may publish their UDS
/// sockets and metadata, in priority order:
///
/// 1. `/var/run/ww/` — system-wide on Linux (preferred when writable).
/// 2. `$HOME/.ww/run/` — per-user fallback (used in containers, on macOS
///    where `/var/run/` is SIP-protected, and anywhere `/var/run/` is
///    not writable for the daemon's user).
///
/// The writer picks the first writable directory. The reader scans all
/// candidates and merges results.
pub fn run_dirs() -> Vec<PathBuf> {
    let mut dirs = vec![PathBuf::from("/var/run/ww")];
    if let Some(home) = dirs::home_dir() {
        dirs.push(home.join(".ww/run"));
    }
    dirs
}

/// Primary directory for *writing* the socket and metadata files. Picks
/// the first candidate from [`run_dirs()`] that can be created and
/// written to. Falls back to the last candidate if none are writable
/// (the writer will then surface a bind error and exit).
fn writable_run_dir() -> PathBuf {
    let candidates = run_dirs();
    for dir in &candidates {
        if std::fs::create_dir_all(dir).is_ok() {
            // Probe write access by creating and removing a temp file.
            let probe = dir.join(".write-probe");
            if std::fs::write(&probe, b"").is_ok() {
                let _ = std::fs::remove_file(&probe);
                return dir.clone();
            }
        }
    }
    candidates
        .into_iter()
        .last()
        .unwrap_or_else(|| PathBuf::from("/var/run/ww"))
}

/// Path to the admin UDS socket file for the given peer.
///
/// Joined under the writable directory from [`run_dirs()`]. The socket
/// is created by `tokio::net::UnixListener::bind` at daemon startup;
/// clients connect via `UnixStream::connect(socket_path(...))`.
pub fn socket_path(peer_id: &str) -> PathBuf {
    writable_run_dir().join(format!("{peer_id}.sock"))
}

/// Path to the admin metadata JSON for the given peer.
///
/// Lives alongside the `.sock` file and carries peer_id, multiaddrs,
/// started_at, pid, and version. Consumed by `ww status` and external
/// tooling. Not load-bearing for shell connection — the `.sock` file
/// (and a successful `connect()` to it) is the authoritative liveness
/// signal.
pub fn metadata_path(peer_id: &str) -> PathBuf {
    writable_run_dir().join(format!("{peer_id}.json"))
}

/// A locally running wetware daemon discovered via UDS socket file.
#[derive(Debug, Clone)]
pub struct LocalNode {
    pub peer_id: String,
    pub socket_path: PathBuf,
}

/// List all locally running wetware daemons by scanning every candidate
/// directory in [`run_dirs()`] for `<peer-id>.sock` entries. Duplicate
/// peer IDs across directories are deduplicated (first occurrence wins).
///
/// Does not validate liveness — the caller should attempt `connect()`
/// and treat `ECONNREFUSED` as "stale socket, ignore this node".
pub fn list_local_nodes() -> Vec<LocalNode> {
    use std::collections::HashSet;

    let mut nodes = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();

    for dir in run_dirs() {
        let entries = match std::fs::read_dir(&dir) {
            Ok(entries) => entries,
            Err(_) => continue,
        };

        for entry in entries.flatten() {
            let path = entry.path();
            // Only consider `.sock` files. Skip the `.json` siblings and
            // any unrelated artifacts in the run dir.
            let name = match path.file_name().and_then(|n| n.to_str()) {
                Some(n) => n,
                None => continue,
            };
            let peer_id = match name.strip_suffix(".sock") {
                Some(p) if !p.is_empty() && !p.starts_with('.') => p.to_string(),
                _ => continue,
            };
            if !seen.insert(peer_id.clone()) {
                continue; // already saw this peer in a higher-priority dir
            }
            nodes.push(LocalNode {
                peer_id,
                socket_path: path,
            });
        }
    }

    nodes
}
