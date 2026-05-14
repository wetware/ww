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

/// Canonical per-user run directory for socket and metadata files.
///
/// This is intentionally user-scoped (`~/.ww/run/`) rather than system-scoped.
/// File ownership and directory permissions are the local auth boundary.
pub fn run_dir() -> PathBuf {
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".ww/run")
}

/// Primary directory for *writing* the socket and metadata files.
///
/// Attempts to create `~/.ww/run/` and returns it regardless of whether
/// creation succeeds (bind/write calls will surface concrete errors).
fn writable_run_dir() -> PathBuf {
    let dir = run_dir();
    let _ = std::fs::create_dir_all(&dir);
    dir
}

/// Path to the admin UDS socket file for the given peer.
///
/// Joined under the writable per-user run directory. The socket
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

/// List all locally running wetware daemons by scanning [`run_dir()`]
/// for `<peer-id>.sock` entries.
///
/// Does not validate liveness — the caller should attempt `connect()`
/// and treat `ECONNREFUSED` as "stale socket, ignore this node".
pub fn list_local_nodes() -> Vec<LocalNode> {
    let mut nodes = Vec::new();
    let entries = match std::fs::read_dir(run_dir()) {
        Ok(entries) => entries,
        Err(_) => return nodes,
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
        nodes.push(LocalNode {
            peer_id,
            socket_path: path,
        });
    }

    nodes.sort_by(|a, b| a.peer_id.cmp(&b.peer_id));
    nodes
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn run_dir_is_user_scoped() {
        let dir = run_dir();
        let s = dir.to_string_lossy();
        assert!(
            s.contains(".ww/run"),
            "run_dir should point at ~/.ww/run, got: {s}"
        );
    }
}
