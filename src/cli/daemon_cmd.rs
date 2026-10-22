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
    /// Kubo HTTP API endpoint resolved during service installation.
    pub ipfs_url: String,
    /// Kubo Gateway listener that exposes HTTP Routing V1.
    pub ipns_routing_url: String,
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
    ipns_routing_url: String,
    quiet: bool,
) -> Result<bool> {
    let home = dirs::home_dir().context("cannot determine home directory")?;
    let ww_dir = home.join(".ww");
    // Service startup has a different working directory. Freeze explicitly
    // selected local sources while the install invocation still owns its CWD.
    let mut image_mounts = ww::cell::mount::parse_args(&images)?;
    ww::cell::mount::freeze_local_paths(
        &mut image_mounts,
        &std::env::current_dir().context("read daemon install CWD")?,
        Some(&home),
    )?;

    // 1. Resolve identity path — default to ~/.ww/identity.
    let key_path = identity.unwrap_or_else(|| ww_dir.join("identity"));

    let generated = super::installation::ensure_identity(&key_path)?;
    if !quiet {
        eprintln!(
            "{} identity: {}",
            if generated {
                "Generated"
            } else {
                "Using existing"
            },
            key_path.display()
        );
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
        let images = image_mounts
            .into_iter()
            .map(|mount| {
                if mount.is_root() {
                    PathBuf::from(mount.source)
                } else {
                    PathBuf::from(format!("{}:{}", mount.source, mount.target))
                }
            })
            .collect();
        (Vec::new(), images)
    };

    // Default WAGI HTTP listener so Rust PID0 responds to curl on first boot.
    let config = DaemonServiceConfig {
        listen: listen_addrs,
        identity: key_path.clone(),
        namespace_roots,
        images: image_layers,
        http_listen: Some("127.0.0.1:2080".to_string()),
        ipfs_url: std::env::var("IPFS_API").unwrap_or_else(|_| "http://localhost:5001".into()),
        ipns_routing_url,
    };

    // 3. Write platform service file.
    let ww_bin = std::env::current_exe().context("cannot determine ww binary path")?;
    write_service_file(&ww_bin, &config, &home, quiet)
}

/// Reuse the explicitly installed payload; embedded seeds never replace it.
pub(super) fn refresh_default_fhs(ww_dir: &Path) -> Result<(PathBuf, bool)> {
    super::installation::installed_fhs(ww_dir)?;
    Ok((ww_dir.join("fhs"), false))
}

/// Render the service without modifying the installation.
pub(super) fn render_service(
    ww_bin: &Path,
    config: &DaemonServiceConfig,
    home: &Path,
) -> Result<(PathBuf, String)> {
    let identity = config
        .identity
        .to_str()
        .context("identity path is non-UTF-8")?;
    if cfg!(target_os = "macos") {
        Ok((
            home.join("Library/LaunchAgents/io.wetware.ww.plist"),
            render_launchd_plist(ww_bin, config, home, identity),
        ))
    } else if cfg!(target_os = "linux") {
        Ok((
            home.join(".config/systemd/user/ww.service"),
            render_systemd_unit(ww_bin, config, identity),
        ))
    } else {
        bail!("unsupported platform; only macOS and Linux are supported")
    }
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

    let plist = render_launchd_plist(ww_bin, config, home, identity_path);

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

    let unit = render_systemd_unit(ww_bin, config, identity_path);

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

fn render_launchd_plist(
    ww_bin: &Path,
    config: &DaemonServiceConfig,
    home: &Path,
    identity_path: &str,
) -> String {
    let log_path = home.join(".ww/logs/ww.log");
    let args = service_argv(ww_bin, config, identity_path)
        .iter()
        .map(|arg| format!("        <string>{}</string>", xml_text(arg)))
        .collect::<Vec<_>>();

    format!(
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
        log = xml_text(&log_path.display().to_string()),
    )
}

fn render_systemd_unit(ww_bin: &Path, config: &DaemonServiceConfig, identity_path: &str) -> String {
    let exec_start = service_argv(ww_bin, config, identity_path)
        .iter()
        .map(|arg| systemd_argument(arg))
        .collect::<Vec<_>>()
        .join(" ");

    format!(
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
    )
}

fn service_argv(ww_bin: &Path, config: &DaemonServiceConfig, identity_path: &str) -> Vec<String> {
    let mut args = vec![ww_bin.display().to_string(), "run".into()];
    for addr in &config.listen {
        args.extend(["--listen".into(), addr.clone()]);
    }
    args.extend([
        "--identity".into(),
        identity_path.into(),
        "--ipfs-url".into(),
        config.ipfs_url.clone(),
        "--ipns-routing-url".into(),
        config.ipns_routing_url.clone(),
    ]);
    if let Some(addr) = &config.http_listen {
        args.extend(["--http-listen".into(), addr.clone()]);
    }
    for root in &config.namespace_roots {
        args.extend(["--namespace-root".into(), root.display().to_string()]);
    }
    args.extend(config.images.iter().map(|path| path.display().to_string()));
    args
}

fn xml_text(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

fn systemd_argument(value: &str) -> String {
    if !value.is_empty()
        && value
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || b"/:._-".contains(&c))
    {
        return value.into();
    }
    let escaped = value
        .replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('\n', "\\n")
        .replace('\r', "\\r")
        .replace('\t', "\\t")
        .replace('%', "%%")
        .replace('$', "$$");
    format!("\"{escaped}\"")
}

#[cfg(test)]
mod rendering_tests {
    use super::*;

    fn config() -> DaemonServiceConfig {
        DaemonServiceConfig {
            listen: default_listen(),
            identity: PathBuf::from("/home/a b&c/identity"),
            namespace_roots: vec![PathBuf::from("/home/a b&c/.ww")],
            images: vec![PathBuf::from("/home/a b&c/.ww/fhs")],
            http_listen: Some("127.0.0.1:2080".into()),
            ipfs_url: "http://localhost:5001".into(),
            ipns_routing_url: "http://localhost:8080".into(),
        }
    }

    #[test]
    fn service_renderers_preserve_nondefault_api_and_escape_endpoint_characters() {
        let mut config = config();
        config.ipfs_url = "http://127.0.0.1:45001/api?x=%20&y=$value".into();
        let binary = Path::new("/home/user/.ww/bin/ww");
        let identity = "/home/user/.ww/identity";
        let argv = service_argv(binary, &config, identity);
        assert_eq!(argv.iter().filter(|arg| *arg == "--ipfs-url").count(), 1);
        let endpoint = argv.iter().position(|arg| arg == "--ipfs-url").unwrap() + 1;
        assert_eq!(argv[endpoint], "http://127.0.0.1:45001/api?x=%20&y=$value");
        let launchd = render_launchd_plist(binary, &config, Path::new("/home/user"), identity);
        assert!(launchd.contains("<string>http://127.0.0.1:45001/api?x=%20&amp;y=$value</string>"));
        let systemd = render_systemd_unit(binary, &config, identity);
        assert!(systemd.contains(r#"--ipfs-url "http://127.0.0.1:45001/api?x=%%20&y=$$value""#));
    }

    #[test]
    fn launchd_escapes_paths_without_changing_argument_boundaries() {
        let config = config();
        let rendered = render_launchd_plist(
            Path::new("/home/a b&c/.ww/bin/ww"),
            &config,
            Path::new("/home/a b&c"),
            "/home/a b&c/identity",
        );
        assert!(rendered.contains(
            "<string>--ipfs-url</string>\n        <string>http://localhost:5001</string>"
        ));
        assert!(rendered.contains("<string>/home/a b&amp;c/.ww/bin/ww</string>"));
        assert!(rendered.contains("<string>/home/a b&amp;c/.ww/fhs</string>"));
        assert!(!rendered.contains("a b&c"));
    }

    #[test]
    fn systemd_quotes_paths_and_escapes_expansion_characters() {
        let config = config();
        let rendered = render_systemd_unit(
            Path::new("/home/a b%$/.ww/bin/ww"),
            &config,
            "/home/a b&c/identity",
        );
        assert!(rendered.contains("ExecStart=\"/home/a b%%$$/.ww/bin/ww\" run"));
        assert!(rendered.contains("--identity \"/home/a b&c/identity\""));
        assert!(rendered.contains("--ipfs-url http://localhost:5001"));
        assert!(rendered.contains("--namespace-root \"/home/a b&c/.ww\""));
        assert!(rendered.contains("\"/home/a b&c/.ww/fhs\""));
    }
}
