//! Fixed-path installation ownership. Private state never enters the FHS.
use anyhow::{ensure, Context, Result};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

// Exactly five artifacts: executable, kernel, status, default namespace, service.
// During replacement, either recorded digest is recoverable by rerunning install.
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Ownership {
    version: u8,
    hashes: [String; 5],
    previous: Option<[String; 5]>,
    complete: bool,
}

fn incompatible(path: &Path) -> anyhow::Error {
    anyhow::anyhow!(
        "Existing custom installation is incompatible at {}. Back up your configuration and reinstall using the current installer. Custom --kernel/WW_KERNEL service settings must be reviewed manually.",
        path.display()
    )
}

/// Reject symlinks, non-directory ancestors, and unreadable metadata.
fn directory(path: &Path) -> Result<()> {
    match std::fs::symlink_metadata(path) {
        Ok(meta) => ensure!(meta.is_dir(), "{}", incompatible(path)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => (),
        Err(e) => return Err(e).with_context(|| format!("inspect {}", path.display())),
    }
    Ok(())
}

fn parents(home: &Path, path: &Path) -> Result<()> {
    ensure!(
        path.starts_with(home),
        "installation path escapes HOME: {}",
        path.display()
    );
    let mut parent = path.parent();
    while let Some(p) = parent {
        directory(p)?;
        if p == home {
            break;
        }
        parent = p.parent();
    }
    Ok(())
}

fn read_regular(path: &Path) -> Result<Option<Vec<u8>>> {
    match std::fs::symlink_metadata(path) {
        Ok(meta) => {
            ensure!(meta.is_file(), "{}", incompatible(path));
            Ok(Some(
                std::fs::read(path).with_context(|| format!("read {}", path.display()))?,
            ))
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e).with_context(|| format!("inspect {}", path.display())),
    }
}

fn identity(path: &Path) -> Result<Option<ed25519_dalek::SigningKey>> {
    if read_regular(path)?.is_some() {
        let key = ww::keys::load(path.to_str().context("identity path is non-UTF-8")?)
            .context("existing identity is malformed or unreadable; refusing to regenerate it")?;
        Ok(Some(key))
    } else {
        Ok(None)
    }
}

pub(super) fn ensure_identity(path: &Path) -> Result<bool> {
    if identity(path)?.is_some() {
        return Ok(false);
    }
    let key = ww::keys::generate()?;
    // Persist without replacing an identity created by another installer.
    persist_identity(path, &key)?;
    Ok(true)
}

fn persist_identity(path: &Path, key: &ed25519_dalek::SigningKey) -> Result<()> {
    use std::io::Write;
    let parent = path.parent().context("identity parent")?;
    std::fs::create_dir_all(parent)?;
    let mut temp = tempfile::NamedTempFile::new_in(parent)?;
    temp.write_all(ww::keys::encode(key).as_bytes())?;
    temp.as_file().sync_all()?;
    temp.persist_noclobber(path).with_context(|| {
        format!(
            "create identity {}; existing identity was preserved",
            path.display()
        )
    })?;
    Ok(())
}

fn ownership(ww_dir: &Path) -> Result<Option<Ownership>> {
    read_regular(&ww_dir.join("installation.json"))?
        .map(|bytes| {
            let state: Ownership = serde_json::from_slice(&bytes)
                .context("invalid installation ownership metadata; back up and reinstall")?;
            ensure!(
                state.version == 1,
                "unsupported installation ownership metadata; use the current installer"
            );
            Ok(state)
        })
        .transpose()
}

fn digest(bytes: &[u8]) -> String {
    // Private ownership evidence only; published Composer identities stay SHA-256.
    blake3::hash(bytes).to_hex().to_string()
}

fn recognized(state: &Ownership, index: usize, bytes: &[u8]) -> bool {
    // Every managed artifact is nonempty. An absent previous file's empty
    // digest must not authorize replacement of a new, unknown empty file.
    if bytes.is_empty() {
        return false;
    }
    let hash = digest(bytes);
    state.hashes[index] == hash
        || state
            .previous
            .as_ref()
            .is_some_and(|old| old[index] == hash)
}

pub(super) fn installed_fhs(ww_dir: &Path) -> Result<()> {
    let home = ww_dir.parent().context("installation HOME")?;
    parents(home, &ww_dir.join("installation.json"))?;
    let state =
        ownership(ww_dir)?.context("canonical FHS is not installed; run the current installer")?;
    ensure!(
        state.complete,
        "installation is incomplete; rerun the current installer"
    );
    for (index, name) in [(1, "boot/kernel.wasm"), (2, "bin/status.wasm")] {
        let path = ww_dir.join("fhs").join(name);
        parents(home, &path)?;
        let bytes = read_regular(&path)?
            .with_context(|| format!("missing {}; rerun the current installer", path.display()))?;
        ensure!(
            !bytes.is_empty() && state.hashes[index] == digest(&bytes),
            "{}",
            incompatible(&path)
        );
    }
    Ok(())
}

fn atomic_write(path: &Path, bytes: &[u8], executable: bool) -> Result<()> {
    use std::io::Write;
    use std::os::unix::fs::PermissionsExt;
    let parent = path.parent().context("installation file parent")?;
    std::fs::create_dir_all(parent)?;
    let mut file = tempfile::NamedTempFile::new_in(parent)?;
    file.write_all(bytes)?;
    file.as_file()
        .set_permissions(std::fs::Permissions::from_mode(if executable {
            0o755
        } else {
            0o600
        }))?;
    file.as_file().sync_all()?;
    file.persist(path)
        .with_context(|| format!("replace {}", path.display()))?;
    Ok(())
}

/// Validate everything before changing any managed file or creating identity.
/// Individual replacements are atomic; the five-file installation is not.
pub(super) async fn install(
    payload: Option<PathBuf>,
    executable: Option<PathBuf>,
    check: bool,
) -> Result<bool> {
    let home = dirs::home_dir().context("cannot determine HOME")?;
    directory(&home)?;
    ensure!(home.is_absolute(), "HOME must be an absolute path");
    service_overrides(&home)?;
    let ww_dir = home.join(".ww");
    let final_exe = executable.unwrap_or_else(|| ww_dir.join("bin/ww"));
    ensure!(
        final_exe == ww_dir.join("bin/ww"),
        "--executable must name {}",
        ww_dir.join("bin/ww").display()
    );
    let current_exe = std::env::current_exe()
        .context("locate running executable")?
        .canonicalize()?;
    let metadata_path = ww_dir.join("installation.json");
    let identity_path = ww_dir.join("identity");
    parents(&home, &metadata_path)?;
    parents(&home, &identity_path)?;
    let state = ownership(&ww_dir)?;
    let existing_key = identity(&identity_path)?;
    ensure!(state.is_none() || existing_key.is_some(), "managed installation identity is missing; restore the original identity from backup before reinstalling");
    let new_identity = existing_key.is_none();
    let key = match existing_key {
        Some(key) => key,
        None => ww::keys::generate()?,
    };

    let explicit_payload = payload.is_some();
    let (kernel, status) = if let Some(payload) = payload {
        let kernel = ww::packaging::read_payload(&payload.join("boot/kernel.wasm"))?;
        let status = ww::packaging::read_payload(&payload.join("bin/status.wasm"))?;
        ww::packaging::verify_fhs(&payload, &kernel, &status)?;
        (kernel, status)
    } else if state.is_some() {
        // A rerun can recover a pending installation only with explicit payload.
        installed_fhs(&ww_dir)?;
        (
            std::fs::read(ww_dir.join("fhs/boot/kernel.wasm"))?,
            std::fs::read(ww_dir.join("fhs/bin/status.wasm"))?,
        )
    } else {
        ensure!(
            !super::EMBEDDED_KERNEL.is_empty() && !super::EMBEDDED_STATUS.is_empty(),
            "canonical FHS payload required; use the current installer or --payload <fhs>"
        );
        (
            super::EMBEDDED_KERNEL.to_vec(),
            super::EMBEDDED_STATUS.to_vec(),
        )
    };
    if !explicit_payload {
        ww::packaging::validate_payload(&kernel, &status)?;
    }
    let peer_id = ww::keys::to_libp2p(&key)?.public().to_peer_id();
    let namespace_path = ww_dir.join("etc/ns/ww");
    parents(&home, &namespace_path)?;
    let namespace = match read_regular(&namespace_path)? {
        Some(bytes) => bytes,
        None => format!(
            "# Wetware namespace: ww\nipns={}\nbootstrap={}\n",
            ww::ipns::canonical_name(peer_id),
            ww::namespace::WW_STD_CID
        )
        .into_bytes(),
    };
    let config = super::daemon_cmd::DaemonServiceConfig {
        listen: super::daemon_cmd::default_listen(),
        identity: identity_path.clone(),
        namespace_roots: vec![ww_dir.clone()],
        images: vec![ww_dir.join("fhs")],
        http_listen: Some("127.0.0.1:2080".into()),
        ipfs_url: std::env::var("IPFS_API").unwrap_or_else(|_| "http://localhost:5001".into()),
        ipns_routing_url: std::env::var("IPFS_ROUTING_API")
            .unwrap_or_else(|_| "http://localhost:8080".into()),
    };
    let (service_path, service) = super::daemon_cmd::render_service(&final_exe, &config, &home)?;
    let mut files = [
        (final_exe, std::fs::read(&current_exe)?),
        (ww_dir.join("fhs/boot/kernel.wasm"), kernel),
        (ww_dir.join("fhs/bin/status.wasm"), status),
        (namespace_path, namespace),
        (service_path, service.into_bytes()),
    ];
    // Check every collision before any destination write. Merely matching the
    // new payload does not establish ownership of an unrecorded existing file.
    for (index, (path, _)) in files.iter().enumerate() {
        parents(&home, path)?;
        if let Some(bytes) = read_regular(path)? {
            let same_running_exe =
                index == 0 && path.canonicalize().is_ok_and(|p| p == current_exe);
            ensure!(
                same_running_exe || state.as_ref().is_some_and(|s| recognized(s, index, &bytes)),
                "{}",
                incompatible(path)
            );
        }
    }
    parents(&home, &ww_dir.join("logs/ww.log"))?;
    if check {
        println!("Installation preflight passed");
        return Ok(false);
    }

    let payload_changed = state.as_ref().is_none_or(|previous| {
        !previous.complete
            || previous.hashes[1] != digest(&files[1].1)
            || previous.hashes[2] != digest(&files[2].1)
    });
    let namespace_config = ww::ns::NamespaceConfig::parse(
        "ww",
        std::str::from_utf8(&files[3].1).context("namespace configuration is not UTF-8")?,
    );
    let service_changed = read_regular(&files[4].0)?.as_deref() != Some(files[4].1.as_slice());
    if payload_changed || namespace_config.bootstrap.is_empty() || service_changed {
        ww::ipns::RoutingClient::new(config.ipns_routing_url.clone())?
            .probe().await.context("Kubo HTTP Routing V1 is required; set Gateway.ExposeRoutingAPI=true, restart Kubo, and set IPFS_ROUTING_API to its Gateway listener")?;
    }
    if payload_changed || namespace_config.bootstrap.is_empty() {
        // Retain the existing flat standard namespace. In particular /boot is
        // absent, so this higher-priority namespace cannot shadow Stem kernels.
        let tree = tempfile::tempdir()?;
        std::fs::create_dir(tree.path().join("bin"))?;
        std::fs::write(tree.path().join("bin/main.wasm"), &files[1].1)?;
        std::fs::write(tree.path().join("bin/status.wasm"), &files[2].1)?;
        let client = ww::ipfs::HttpClient::new(config.ipfs_url.clone());
        let cid = client.add_dir(tree.path()).await.context("index canonical standard namespace; start local Kubo and set IPFS_API to its HTTP API before installing")?;
        files[3].1 = format!(
            "# Wetware namespace: ww\nipns={}\nbootstrap=/ipfs/{cid}\n",
            ww::ipns::canonical_name(peer_id)
        )
        .into_bytes();
    }
    let mut changed = new_identity;
    for (path, bytes) in &files {
        changed |= read_regular(path)?.as_deref() != Some(bytes.as_slice());
    }
    std::fs::create_dir_all(&ww_dir)?;
    if new_identity {
        persist_identity(&identity_path, &key)?;
    }
    let mut next = Ownership {
        version: 1,
        hashes: std::array::from_fn(|index| digest(&files[index].1)),
        previous: Some([
            digest(&read_regular(&files[0].0)?.unwrap_or_default()),
            digest(&read_regular(&files[1].0)?.unwrap_or_default()),
            digest(&read_regular(&files[2].0)?.unwrap_or_default()),
            digest(&read_regular(&files[3].0)?.unwrap_or_default()),
            digest(&read_regular(&files[4].0)?.unwrap_or_default()),
        ]),
        complete: false,
    };
    // Record both allowed generations first. A failure leaves an explicit
    // incomplete marker, which the installer can repair with the same payload.
    ww::keys::atomic_write_private(&metadata_path, &serde_json::to_vec_pretty(&next)?)?;
    for (index, (path, bytes)) in files.iter().enumerate() {
        if read_regular(path)?.as_deref() != Some(bytes.as_slice()) {
            atomic_write(path, bytes, index == 0)?;
        }
    }
    std::fs::create_dir_all(ww_dir.join("logs"))?;
    next.complete = true;
    next.previous = None;
    ww::keys::atomic_write_private(&metadata_path, &serde_json::to_vec_pretty(&next)?)?;
    println!("Installed {}", files[0].0.display());
    if cfg!(target_os = "macos") {
        println!(
            r#"Start the daemon: launchctl load "$HOME/Library/LaunchAgents/io.wetware.ww.plist""#
        );
    } else {
        println!(
            "Start the daemon: systemctl --user daemon-reload && systemctl --user enable --now ww"
        );
    }
    Ok(changed)
}

fn service_overrides(home: &Path) -> Result<()> {
    let dropins = home.join(".config/systemd/user/ww.service.d");
    parents(home, &dropins)?;
    match std::fs::symlink_metadata(&dropins) {
        Ok(_) => Err(incompatible(&dropins)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error).context("inspect custom systemd service overrides"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pending_absent_file_does_not_own_an_unknown_empty_collision() {
        let state = Ownership {
            version: 1,
            hashes: std::array::from_fn(|_| "new artifact".into()),
            previous: Some(std::array::from_fn(|_| digest(b""))),
            complete: false,
        };
        assert!(!recognized(&state, 2, b""));
    }

    #[test]
    fn custom_systemd_dropins_refuse_without_modification() {
        let home = tempfile::tempdir().unwrap();
        let custom = home.path().join(".config/systemd/user/ww.service.d");
        std::fs::create_dir_all(&custom).unwrap();
        std::fs::write(custom.join("kernel.conf"), b"Environment=WW_KERNEL=custom").unwrap();
        assert!(service_overrides(home.path()).is_err());
        assert_eq!(
            std::fs::read(custom.join("kernel.conf")).unwrap(),
            b"Environment=WW_KERNEL=custom"
        );
    }
}
