use anyhow::{bail, Context, Result};
use libp2p::Multiaddr;
use std::path::{Path, PathBuf};

/// Default libp2p listen multiaddrs: TCP and QUIC on both IPv4 and IPv6, port 2025.
pub(super) fn default_listen() -> Vec<String> {
    vec![
        "/ip4/0.0.0.0/tcp/2025".to_string(),
        "/ip6/::/tcp/2025".to_string(),
        "/ip4/0.0.0.0/udp/2025/quic-v1".to_string(),
        "/ip6/::/udp/2025/quic-v1".to_string(),
    ]
}

/// Host-side service configuration used to render launchd/systemd units.
#[derive(Debug, Clone)]
pub(super) struct DaemonServiceConfig {
    pub listen: Vec<String>,
    pub identity: PathBuf,
    /// Host-only roots whose `etc/ns` directories configure namespace layers.
    pub namespace_roots: Vec<PathBuf>,
    pub images: Vec<PathBuf>,
    /// Address (`host:port`) for the WAGI HTTP server. `None` disables WAGI.
    pub http_listen: Option<String>,
}

/// Register wetware as a user-level background service.
///
/// When `quiet` is true, suppresses status output (used by `perform install`
/// which prints its own summary). Returns whether the rendered service
/// definition differs from the definition that was on disk.
pub(super) async fn daemon_install(
    identity: Option<PathBuf>,
    listen: Vec<Multiaddr>,
    images: Vec<String>,
    quiet: bool,
) -> Result<bool> {
    let home = dirs::home_dir().context("cannot determine home directory")?;
    let ww_dir = home.join(".ww");

    // 1. Resolve identity path — default to ~/.ww/identity.
    let key_path = identity.unwrap_or_else(|| ww_dir.join("identity"));

    // Generate key if it doesn't exist.
    if !key_path.exists() {
        let sk = ww::keys::generate()?;
        ww::keys::save(&sk, &key_path)?;

        if !quiet {
            let kp = ww::keys::to_libp2p(&sk)?;
            eprintln!("Generated new identity: {}", key_path.display());
            eprintln!("  Peer ID:     {}", kp.public().to_peer_id());
        }
    } else if !quiet {
        eprintln!("Using existing identity: {}", key_path.display());
    }

    // 2. Build service config directly from CLI/defaults.
    let listen_addrs = if listen.is_empty() {
        default_listen()
    } else {
        listen.iter().map(|a| a.to_string()).collect()
    };

    let (namespace_roots, image_layers) = if images.is_empty() {
        let (fhs_dir, _) = refresh_default_fhs(&ww_dir)?;
        (vec![ww_dir.clone()], vec![fhs_dir])
    } else {
        (Vec::new(), images.iter().map(PathBuf::from).collect())
    };

    // Default WAGI HTTP listener so Rust PID0 responds to curl on first boot.
    let config = DaemonServiceConfig {
        listen: listen_addrs,
        identity: key_path.clone(),
        namespace_roots,
        images: image_layers,
        http_listen: Some("127.0.0.1:2080".to_string()),
    };

    // 3. Write platform service file.
    let ww_bin = std::env::current_exe().context("cannot determine ww binary path")?;
    write_service_file(&ww_bin, &config, &home, quiet)
}

/// Materialize the install-owned FHS layer outside the private host-state tree.
///
/// Release binaries carry the status component. Install-script layouts from
/// older versions provide the same bytes under `~/.ww/std/status`.
pub(super) fn refresh_default_fhs(ww_dir: &Path) -> Result<(PathBuf, bool)> {
    let fhs_dir = ww_dir.join("fhs");
    let bin_dir = fhs_dir.join("bin");
    let mut changed = !bin_dir.exists();
    std::fs::create_dir_all(&bin_dir).context("create ~/.ww/fhs/bin")?;

    let status_path = bin_dir.join("status.wasm");
    let legacy_status_path = ww_dir.join("std/status/bin/status.wasm");
    let status_bytes = if !super::EMBEDDED_STATUS.is_empty() {
        Some(super::EMBEDDED_STATUS.to_vec())
    } else {
        match std::fs::read(&legacy_status_path) {
            Ok(bytes) => Some(bytes),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
            Err(error) => {
                return Err(error)
                    .with_context(|| format!("read {}", legacy_status_path.display()));
            }
        }
    };
    if let Some(status_bytes) = status_bytes {
        let current = match std::fs::read(&status_path) {
            Ok(bytes) => Some(bytes),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
            Err(error) => {
                return Err(error).with_context(|| format!("read {}", status_path.display()));
            }
        };
        if current.as_deref() != Some(status_bytes.as_slice()) {
            std::fs::write(&status_path, status_bytes)
                .context("write ~/.ww/fhs/bin/status.wasm")?;
            changed = true;
        }
    }

    Ok((fhs_dir, changed))
}

/// Remove the platform service file.
pub(super) async fn daemon_uninstall() -> Result<()> {
    let home = dirs::home_dir().context("cannot determine home directory")?;

    if cfg!(target_os = "macos") {
        let plist_path = home.join("Library/LaunchAgents/io.wetware.ww.plist");
        if plist_path.exists() {
            std::fs::remove_file(&plist_path)
                .with_context(|| format!("remove {}", plist_path.display()))?;
            eprintln!("Removed: {}", plist_path.display());
            eprintln!();
            eprintln!("If the service is running, stop it with:");
            eprintln!("  launchctl unload {}", plist_path.display());
        } else {
            eprintln!("No service file found at: {}", plist_path.display());
        }
    } else if cfg!(target_os = "linux") {
        let unit_path = home.join(".config/systemd/user/ww.service");
        if unit_path.exists() {
            std::fs::remove_file(&unit_path)
                .with_context(|| format!("remove {}", unit_path.display()))?;
            eprintln!("Removed: {}", unit_path.display());
            eprintln!();
            eprintln!("If the service is running, stop it with:");
            eprintln!("  systemctl --user disable --now ww");
        } else {
            eprintln!("No service file found at: {}", unit_path.display());
        }
    } else {
        bail!("unsupported platform; only macOS and Linux are supported");
    }

    Ok(())
}

/// Write a platform-specific service file and print the activation command.
/// Returns whether the rendered definition differs from the prior file bytes.
pub(super) fn write_service_file(
    ww_bin: &Path,
    config: &DaemonServiceConfig,
    home: &Path,
    quiet: bool,
) -> Result<bool> {
    // Identity as a --identity CLI flag (NOT a :/etc/identity mount).
    // The host reads it to create the signing key; it never enters
    // the merged FHS tree visible to guests.
    let identity_path = config.identity.display().to_string();

    if cfg!(target_os = "macos") {
        write_launchd_plist(ww_bin, config, home, &identity_path, quiet)
    } else if cfg!(target_os = "linux") {
        write_systemd_unit(ww_bin, config, home, &identity_path, quiet)
    } else {
        bail!("unsupported platform; only macOS and Linux are supported")
    }
}

fn service_definition_differs(path: &Path, definition: &[u8]) -> Result<bool> {
    match std::fs::read(path) {
        Ok(existing) => Ok(existing != definition),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(true),
        Err(error) => Err(error).with_context(|| format!("read {}", path.display())),
    }
}

/// Write a macOS launchd plist.
pub(super) fn write_launchd_plist(
    ww_bin: &Path,
    config: &DaemonServiceConfig,
    home: &Path,
    identity_path: &str,
    quiet: bool,
) -> Result<bool> {
    let plist_dir = home.join("Library/LaunchAgents");
    std::fs::create_dir_all(&plist_dir).context("create ~/Library/LaunchAgents")?;

    let plist_path = plist_dir.join("io.wetware.ww.plist");

    let log_dir = home.join(".ww/logs");
    std::fs::create_dir_all(&log_dir).context("create ~/.ww/logs")?;
    let log_path = log_dir.join("ww.log");

    // Build ProgramArguments array entries.
    let mut args = vec![
        format!("        <string>{}</string>", ww_bin.display()),
        "        <string>run</string>".to_string(),
    ];
    for addr in &config.listen {
        args.push("        <string>--listen</string>".to_string());
        args.push(format!("        <string>{addr}</string>"));
    }
    // Identity as a --identity flag (host-side only, not a guest mount).
    args.push("        <string>--identity</string>".to_string());
    args.push(format!("        <string>{identity_path}</string>"));
    // WAGI HTTP listen addr (engagement starter kit: status endpoint on :2080).
    if let Some(ref addr) = config.http_listen {
        args.push("        <string>--http-listen</string>".to_string());
        args.push(format!("        <string>{addr}</string>"));
    }
    // Namespace configuration is host state, not an image mount.
    for root in &config.namespace_roots {
        args.push("        <string>--namespace-root</string>".to_string());
        args.push(format!("        <string>{}</string>", root.display()));
    }
    // Image layers (root mounts).
    for img in &config.images {
        args.push(format!("        <string>{}</string>", img.display()));
    }

    let plist = format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN"
  "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>Label</key>
    <string>io.wetware.ww</string>
    <key>ProgramArguments</key>
    <array>
{args}
    </array>
    <key>StandardOutPath</key>
    <string>{log}</string>
    <key>StandardErrorPath</key>
    <string>{log}</string>
    <key>KeepAlive</key>
    <true/>
    <key>RunAtLoad</key>
    <false/>
    <key>SoftResourceLimits</key>
    <dict>
        <key>NumberOfFiles</key>
        <integer>4096</integer>
    </dict>
</dict>
</plist>
"#,
        args = args.join("\n"),
        log = log_path.display(),
    );

    let changed = service_definition_differs(&plist_path, plist.as_bytes())?;
    std::fs::write(&plist_path, plist)
        .with_context(|| format!("write plist: {}", plist_path.display()))?;
    if !quiet {
        eprintln!("Wrote service: {}", plist_path.display());
        eprintln!();
        eprintln!("Activate with:");
        eprintln!("  launchctl load {}", plist_path.display());
    }

    Ok(changed)
}

/// Write a Linux systemd user unit.
pub(super) fn write_systemd_unit(
    ww_bin: &Path,
    config: &DaemonServiceConfig,
    home: &Path,
    identity_path: &str,
    quiet: bool,
) -> Result<bool> {
    let unit_dir = home.join(".config/systemd/user");
    std::fs::create_dir_all(&unit_dir).context("create ~/.config/systemd/user")?;

    let unit_path = unit_dir.join("ww.service");

    // Build positional args: image layers only (identity is a flag, not a mount).
    let mut positional = Vec::new();
    for img in &config.images {
        positional.push(img.display().to_string());
    }

    let listen_args: String = config
        .listen
        .iter()
        .map(|a| format!("--listen {a}"))
        .collect::<Vec<_>>()
        .join(" ");
    let http_listen_arg = match &config.http_listen {
        Some(addr) => format!(" --http-listen {addr}"),
        None => String::new(),
    };
    let namespace_args = config
        .namespace_roots
        .iter()
        .map(|root| format!("--namespace-root {}", root.display()))
        .collect::<Vec<_>>()
        .join(" ");
    let exec_start = format!(
        "{} run {} --identity {}{} {} {}",
        ww_bin.display(),
        listen_args,
        identity_path,
        http_listen_arg,
        namespace_args,
        positional.join(" "),
    );

    let unit = format!(
        "[Unit]\n\
         Description=Wetware daemon\n\
         After=network-online.target\n\
         Wants=network-online.target\n\
         \n\
         [Service]\n\
         Type=simple\n\
         ExecStart={exec_start}\n\
         Restart=on-failure\n\
         RestartSec=5\n\
         \n\
         [Install]\n\
         WantedBy=default.target\n"
    );

    let changed = service_definition_differs(&unit_path, unit.as_bytes())?;
    std::fs::write(&unit_path, unit)
        .with_context(|| format!("write unit: {}", unit_path.display()))?;
    if !quiet {
        eprintln!("Wrote service: {}", unit_path.display());
        eprintln!();
        eprintln!("Activate with:");
        eprintln!("  systemctl --user enable --now ww");
    }

    Ok(changed)
}
