use anyhow::{Context, Result};
use libp2p::multiaddr::Protocol;
use libp2p::{Multiaddr, PeerId};
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

const HOST_STATE_ENV: &str = "WW_HOST_STATE_PATH";

#[derive(Debug, Clone)]
pub struct HostState {
    pub peer_id: PeerId,
    pub addrs: Vec<Multiaddr>,
    pub pid: u32,
}

#[derive(Debug, Serialize, Deserialize)]
struct HostStateFile {
    pid: u32,
    peer_id: String,
    addrs: Vec<String>,
    updated_at_unix_ms: u64,
}

pub fn state_path() -> Result<PathBuf> {
    if let Some(path) = std::env::var_os(HOST_STATE_ENV) {
        return Ok(PathBuf::from(path));
    }

    if let Some(runtime_dir) = dirs::runtime_dir() {
        return Ok(runtime_dir.join("ww/host.json"));
    }

    let home = dirs::home_dir().context("cannot determine home directory")?;
    Ok(home.join(".ww/run/host.json"))
}

pub fn write_from_snapshot(snapshot: &rpc::NetworkSnapshot) -> Result<bool> {
    let state = match snapshot_to_state_file(snapshot)? {
        Some(state) => state,
        None => return Ok(false),
    };

    let path = state_path()?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("failed to create {}", parent.display()))?;
    }

    let json = serde_json::to_vec_pretty(&state)?;
    let tmp = path.with_extension(format!("tmp-{}", std::process::id()));
    std::fs::write(&tmp, json)
        .with_context(|| format!("failed to write temporary host state at {}", tmp.display()))?;

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600));
    }

    std::fs::rename(&tmp, &path)
        .with_context(|| format!("failed to install host state at {}", path.display()))?;

    Ok(true)
}

pub fn read_live_host_state() -> Result<Option<HostState>> {
    let path = state_path()?;
    if !path.exists() {
        return Ok(None);
    }

    let raw = std::fs::read_to_string(&path)
        .with_context(|| format!("failed to read host state {}", path.display()))?;
    let state_file: HostStateFile = serde_json::from_str(&raw)
        .with_context(|| format!("failed to parse host state {}", path.display()))?;

    if !pid_is_live(state_file.pid) {
        return Ok(None);
    }

    let peer_id: PeerId = state_file
        .peer_id
        .parse()
        .with_context(|| format!("invalid peer_id in host state {}", path.display()))?;

    let mut addrs = Vec::with_capacity(state_file.addrs.len());
    for addr in state_file.addrs {
        let parsed: Multiaddr = addr
            .parse()
            .with_context(|| format!("invalid multiaddr in host state {}", path.display()))?;
        addrs.push(parsed);
    }

    if addrs.is_empty() {
        return Ok(None);
    }

    Ok(Some(HostState {
        peer_id,
        addrs,
        pid: state_file.pid,
    }))
}

pub fn remove_state_file() -> Result<()> {
    let path = state_path()?;
    match std::fs::remove_file(&path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e).with_context(|| format!("failed to remove {}", path.display())),
    }
}

fn snapshot_to_state_file(snapshot: &rpc::NetworkSnapshot) -> Result<Option<HostStateFile>> {
    let peer_id = PeerId::from_bytes(&snapshot.local_peer_id)
        .context("invalid local peer id in network snapshot")?
        .to_string();

    let mut addrs = Vec::new();
    let mut seen = HashSet::new();

    for raw in &snapshot.listen_addrs {
        let parsed = match Multiaddr::try_from(raw.clone()) {
            Ok(addr) => addr,
            Err(_) => continue,
        };

        let normalized = normalize_dial_addr(parsed);
        if !is_local_dial_addr(&normalized) {
            continue;
        }
        let key = normalized.to_string();
        if seen.insert(key.clone()) {
            addrs.push(key);
        }
    }

    if addrs.is_empty() {
        return Ok(None);
    }

    let updated_at_unix_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64;

    Ok(Some(HostStateFile {
        pid: std::process::id(),
        peer_id,
        addrs,
        updated_at_unix_ms,
    }))
}

fn normalize_dial_addr(addr: Multiaddr) -> Multiaddr {
    let mut out = addr;
    let components: Vec<_> = out.iter().collect();

    for (idx, protocol) in components.iter().enumerate() {
        match protocol {
            Protocol::Ip4(ip) if ip.is_unspecified() => {
                if let Some(next) =
                    out.replace(idx, |_| Some(Protocol::Ip4(std::net::Ipv4Addr::LOCALHOST)))
                {
                    out = next;
                }
                break;
            }
            Protocol::Ip6(ip) if ip.is_unspecified() => {
                if let Some(next) =
                    out.replace(idx, |_| Some(Protocol::Ip6(std::net::Ipv6Addr::LOCALHOST)))
                {
                    out = next;
                }
                break;
            }
            _ => {}
        }
    }

    out
}

fn is_local_dial_addr(addr: &Multiaddr) -> bool {
    let mut ip: Option<std::net::IpAddr> = None;
    let mut relay = false;

    for protocol in addr.iter() {
        match protocol {
            Protocol::Ip4(v4) => ip = Some(std::net::IpAddr::V4(v4)),
            Protocol::Ip6(v6) => ip = Some(std::net::IpAddr::V6(v6)),
            Protocol::P2pCircuit => relay = true,
            _ => {}
        }
    }

    if relay {
        return false;
    }

    match ip {
        Some(std::net::IpAddr::V4(v4)) => v4.is_loopback() || v4.is_private() || v4.is_link_local(),
        Some(std::net::IpAddr::V6(v6)) => {
            if v6.is_loopback() {
                return true;
            }
            let seg0 = v6.segments()[0];
            let is_link_local = (seg0 & 0xffc0) == 0xfe80;
            let is_ula = (seg0 & 0xfe00) == 0xfc00;
            is_link_local || is_ula
        }
        None => false,
    }
}

fn pid_is_live(pid: u32) -> bool {
    if pid == 0 {
        return false;
    }

    let rc = unsafe { libc::kill(pid as i32, 0) };
    if rc == 0 {
        return true;
    }

    std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_unspecified_ipv4_to_loopback() {
        let addr: Multiaddr = "/ip4/0.0.0.0/tcp/2025".parse().unwrap();
        let normalized = normalize_dial_addr(addr);
        assert_eq!(normalized.to_string(), "/ip4/127.0.0.1/tcp/2025");
    }

    #[test]
    fn normalize_unspecified_ipv6_to_loopback() {
        let addr: Multiaddr = "/ip6/::/tcp/2025".parse().unwrap();
        let normalized = normalize_dial_addr(addr);
        assert_eq!(normalized.to_string(), "/ip6/::1/tcp/2025");
    }

    #[test]
    fn local_filter_accepts_private_v4_and_rejects_public_v4() {
        let private: Multiaddr = "/ip4/192.168.1.10/tcp/2025".parse().unwrap();
        let public: Multiaddr = "/ip4/8.8.8.8/tcp/2025".parse().unwrap();
        assert!(is_local_dial_addr(&private));
        assert!(!is_local_dial_addr(&public));
    }

    #[test]
    fn local_filter_rejects_relay_addr() {
        let relay: Multiaddr = "/ip4/23.94.2.155/tcp/4001/p2p/12D3KooWGXKWybkyuWY5pdsYE6iX6vQQ7dwraPcqwf2VrHkqxKMw/p2p-circuit/p2p/12D3KooWQ5YPLoJZncftpsSa8cUBLRqHuQh3BoN1TAU6ZpwDrvNt".parse().unwrap();
        assert!(!is_local_dial_addr(&relay));
    }
}
