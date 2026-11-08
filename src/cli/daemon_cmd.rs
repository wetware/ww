//! Explicit per-user service registration. Software, identity, and FHS ownership
//! remain separate from the registration receipt and retained service definition.
use anyhow::{bail, ensure, Context, Result};
use serde::{Deserialize, Serialize};
use std::io::{Read, Seek, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};
use ww::installation_state::{MutationLock, Receipt};
use ww::upgrade::service::{
    EnabledState, NativeService, RunningState, ServiceControl, ServiceSnapshot,
};

const MAX_BYTES: u64 = 65536;
const LABEL: &str = "io.wetware.ww";

pub(super) fn default_listen() -> Vec<String> {
    vec![
        "/ip4/0.0.0.0/tcp/2025".into(),
        "/ip6/::/tcp/2025".into(),
        "/ip4/0.0.0.0/udp/2025/quic-v1".into(),
        "/ip6/::/udp/2025/quic-v1".into(),
    ]
}

impl super::RunArgs {
    /// Persist the same arguments accepted by `run`, with invocation-local paths
    /// frozen. This validates metadata and identity, but imports no content.
    pub(super) fn service_argv(mut self, home: &Path) -> Result<Vec<String>> {
        ensure!(!self.insecure_ephemeral, "daemon registration requires an existing persistent identity; use ww keygen --output PATH first");
        let cwd = std::env::current_dir().context("read daemon registration CWD")?;
        let identity = absolute(
            self.identity
                .as_deref()
                .map(Path::new)
                .unwrap_or(&home.join(".ww/identity")),
            home,
            &cwd,
        )?;
        ensure!(
            std::fs::symlink_metadata(&identity)
                .with_context(|| format!(
                    "read existing identity {}; use ww keygen --output PATH first",
                    identity.display()
                ))?
                .is_file(),
            "identity must be an existing regular file: {}",
            identity.display()
        );
        super::Commands::resolve_identity(Some(&identity), false)?;
        for root in &mut self.namespace_root {
            *root = absolute(root, home, &cwd)?;
        }
        let authoritative = self.stem.is_some() || self.ipns_stem.is_some();
        let (baseline, mounts) = super::Commands::startup_mounts(
            &self.mounts,
            self.fhs_root,
            &mut self.namespace_root,
            authoritative,
        )?;
        super::Commands::startup_namespaces(
            &mounts,
            &self.namespace_root,
            authoritative,
            baseline.is_some(),
        )?;
        // Use the shared parser to reject invalid input before any service write.
        if let Some(name) = &self.ipns_stem {
            ww::ipns::parse_name(name)?;
        }
        if let Some(stem) = &self.stem {
            ensure!(
                stem.strip_prefix("0x").is_some_and(
                    |value| value.len() == 40 && value.bytes().all(|c| c.is_ascii_hexdigit())
                ),
                "--stem must be a 0x-prefixed 20-byte address"
            );
        }
        ensure!(
            matches!(self.runtime_cache_policy.as_str(), "shared" | "isolated"),
            "invalid runtime cache policy"
        );
        let mut argv = vec!["run".to_owned()];
        let mut pair = |flag: &str, value: String| {
            argv.extend([flag.to_owned(), value]);
        };
        pair("--identity", utf8(&identity)?.to_owned());
        for addr in if self.listen.is_empty() {
            default_listen()
        } else {
            self.listen.iter().map(ToString::to_string).collect()
        } {
            pair("--listen", addr);
        }
        if let Some(baseline) = baseline {
            pair("--fhs-root", baseline.source);
        }
        for root in self.namespace_root {
            pair("--namespace-root", utf8(&root)?.to_owned());
        }
        if let Some(stem) = self.stem {
            pair("--stem", stem);
        }
        if let Some(stem) = self.ipns_stem {
            pair("--ipns-stem", stem);
        }
        pair("--ipns-routing-url", self.ipns_routing_url);
        pair("--rpc-url", self.rpc_url);
        pair("--ws-url", self.ws_url);
        pair("--confirmation-depth", self.confirmation_depth.to_string());
        pair("--executor-threads", self.executor_threads.to_string());
        if let Some(listener) = self.http_listen {
            pair("--http-listen", listener);
        }
        for dial in self.http_dial {
            pair("--http-dial", dial);
        }
        pair("--runtime-cache-policy", self.runtime_cache_policy);
        pair("--with-http-admin", self.with_http_admin);
        pair("--ipfs-url", self.ipfs_url);
        if self.wasm_debug {
            argv.push("--wasm-debug".into());
        }
        argv.extend(mounts.into_iter().map(|mount| {
            if mount.is_root() {
                mount.source
            } else {
                format!("{}:{}", mount.source, mount.target)
            }
        }));
        ensure!(
            argv.iter().all(|arg| !arg.contains('\0')),
            "service arguments contain NUL"
        );
        Ok(argv)
    }
}

fn absolute(path: &Path, home: &Path, cwd: &Path) -> Result<PathBuf> {
    let path = if let Ok(relative) = path.strip_prefix("~") {
        home.join(relative)
    } else if path.is_absolute() {
        path.to_owned()
    } else {
        cwd.join(path)
    };
    ensure!(
        !path
            .components()
            .any(|part| matches!(part, std::path::Component::ParentDir)),
        "service paths must not contain ..: {}",
        path.display()
    );
    Ok(path)
}
fn utf8(path: &Path) -> Result<&str> {
    path.to_str().context("service path is not UTF-8")
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Platform {
    Linux,
    Macos,
}
impl Platform {
    fn current() -> Result<Self> {
        if cfg!(target_os = "linux") {
            Ok(Self::Linux)
        } else if cfg!(target_os = "macos") {
            Ok(Self::Macos)
        } else {
            bail!("daemon registration supports only per-user systemd and launchd")
        }
    }
    fn path(self, home: &Path) -> PathBuf {
        home.join(match self {
            Self::Linux => ".config/systemd/user/ww.service",
            Self::Macos => "Library/LaunchAgents/io.wetware.ww.plist",
        })
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
enum Phase {
    Installing,
    Registered,
    Removing,
    Removed,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Registration {
    version: u8,
    digest: String,
    phase: Phase,
    prior_enabled: EnabledState,
}
impl Registration {
    fn read(home: &Path) -> Result<Option<Self>> {
        let Some(bytes) = read_file(home, &home.join(".ww/daemon.json"))? else {
            return Ok(None);
        };
        let record: Self =
            serde_json::from_slice(&bytes).context("invalid daemon registration receipt")?;
        ensure!(
            record.version == 1
                && record.digest.len() == 64
                && record
                    .digest
                    .bytes()
                    .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)),
            "unsupported daemon registration receipt"
        );
        Ok(Some(record))
    }
    fn write(&self, home: &Path) -> Result<()> {
        write_file(
            home,
            &home.join(".ww/daemon.json"),
            &serde_json::to_vec_pretty(self)?,
            false,
        )
    }
}
fn digest(bytes: &[u8]) -> String {
    blake3::hash(bytes).to_hex().to_string()
}
fn owned(home: &Path, bytes: &[u8], record: Option<&Registration>) -> Result<bool> {
    let hash = digest(bytes);
    if let Some(record) = record {
        return Ok(record.phase != Phase::Removed && record.digest == hash);
    }
    Ok(Receipt::read(&home.join(".ww"))?
        .is_some_and(|receipt| receipt.complete && receipt.hashes[4] == hash))
}

#[derive(Debug, Serialize)]
pub(super) struct RemovalReport {
    pub changed: bool,
    pub checked: bool,
    pub path: PathBuf,
    pub backup: Option<PathBuf>,
    pub registered: bool,
    pub running: bool,
    pub enabled: bool,
}

/// Validate software and this plan before invoking `remove_locked`.
pub(super) struct RemovalPlan {
    platform: Platform,
    bytes: Option<Vec<u8>>,
    before: ServiceSnapshot,
}

trait Manager {
    fn snapshot(&mut self) -> Result<ServiceSnapshot>;
    fn disable(&mut self, lock: &MutationLock) -> Result<()>;
    fn stop(&mut self, lock: &MutationLock, loaded: bool) -> Result<()>;
    fn reload(&mut self, lock: &MutationLock) -> Result<()>;
    fn disabled(&mut self) -> Result<bool>;
}
struct NativeManager {
    platform: Platform,
    home: PathBuf,
}
impl NativeManager {
    fn target(&self) -> String {
        format!("gui/{}/{LABEL}", unsafe { libc::geteuid() })
    }
    fn domain(&self) -> String {
        format!("gui/{}", unsafe { libc::geteuid() })
    }
}
impl Manager for NativeManager {
    fn snapshot(&mut self) -> Result<ServiceSnapshot> {
        let snapshot =
            NativeService::new(&self.home)?.snapshot(Instant::now() + Duration::from_secs(30))?;
        if self.platform == Platform::Linux {
            let output = command_output(
                "/usr/bin/systemctl",
                &[
                    "--user",
                    "show",
                    "ww.service",
                    "--property=FragmentPath,DropInPaths,UnitFileState",
                    "--no-pager",
                ],
                None,
            )?;
            let fields: Vec<_> = output.lines().collect();
            let fragment = if snapshot.registered {
                format!("FragmentPath={}", self.platform.path(&self.home).display())
            } else {
                "FragmentPath=".to_owned()
            };
            ensure!(fields.len() == 3 && fields.contains(&fragment.as_str()) && fields.contains(&"DropInPaths="), "systemd service uses an external definition or drop-in; preserve configuration and remove it explicitly");
            if !snapshot.registered {
                ensure!(fields.contains(&"UnitFileState=") || fields.contains(&"UnitFileState=not-found"), "systemd has stale or conflicting enablement; remove it explicitly before registration");
            }
        }
        Ok(snapshot)
    }
    fn disable(&mut self, lock: &MutationLock) -> Result<()> {
        match self.platform {
            Platform::Macos => {
                command_output("/bin/launchctl", &["disable", &self.target()], Some(lock))?;
            }
            Platform::Linux => {
                command_output(
                    "/usr/bin/systemctl",
                    &["--user", "disable", "ww.service"],
                    Some(lock),
                )?;
            }
        }
        ensure!(
            self.disabled()?,
            "service disablement could not be verified; definition was preserved"
        );
        Ok(())
    }
    fn stop(&mut self, lock: &MutationLock, loaded: bool) -> Result<()> {
        match self.platform {
            Platform::Macos if loaded => {
                command_output("/bin/launchctl", &["bootout", &self.target()], Some(lock))?;
            }
            Platform::Linux => {
                command_output(
                    "/usr/bin/systemctl",
                    &["--user", "stop", "ww.service"],
                    Some(lock),
                )?;
            }
            _ => (),
        }
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            if self.snapshot()?.running == RunningState::Stopped {
                return Ok(());
            }
            ensure!(
                Instant::now() < deadline,
                "service did not stop before deadline; registration was preserved"
            );
            std::thread::sleep(Duration::from_millis(50));
        }
    }
    fn reload(&mut self, lock: &MutationLock) -> Result<()> {
        if self.platform == Platform::Linux {
            command_output(
                "/usr/bin/systemctl",
                &["--user", "daemon-reload"],
                Some(lock),
            )?;
        }
        Ok(())
    }
    fn disabled(&mut self) -> Result<bool> {
        match self.platform {
            Platform::Linux => Ok(self.snapshot()?.enabled == EnabledState::Disabled),
            Platform::Macos => {
                let output =
                    command_output("/bin/launchctl", &["print-disabled", &self.domain()], None)?;
                let matches: Vec<_> = output
                    .lines()
                    .map(str::trim)
                    .filter(|line| line.starts_with("\"io.wetware.ww\" => "))
                    .collect();
                Ok(matches.len() == 1
                    && matches!(
                        matches[0],
                        "\"io.wetware.ww\" => true" | "\"io.wetware.ww\" => disabled"
                    ))
            }
        }
    }
}

/// Bound both runtime and output; lifecycle children retain the mutation lock.
fn command_output(program: &str, args: &[&str], lock: Option<&MutationLock>) -> Result<String> {
    let mut stdout = tempfile::tempfile()?;
    let mut stderr = tempfile::tempfile()?;
    let mut command = Command::new(program);
    command
        .args(args)
        .env("LC_ALL", "C")
        .stdin(Stdio::null())
        .stdout(stdout.try_clone()?)
        .stderr(stderr.try_clone()?);
    if let Some(lock) = lock {
        lock.inherit_to(&mut command)?;
    }
    let mut child = command
        .spawn()
        .with_context(|| format!("start {program} {}", args.join(" ")))?;
    let deadline = Instant::now() + Duration::from_secs(30);
    let status = loop {
        if stdout.metadata()?.len() > MAX_BYTES
            || stderr.metadata()?.len() > MAX_BYTES
            || Instant::now() >= deadline
        {
            let _ = child.kill();
            let _ = child.wait();
            bail!(
                "service command exceeded time/output bound: {program} {}",
                args.join(" ")
            );
        }
        if let Some(status) = child.try_wait()? {
            break status;
        }
        std::thread::sleep(Duration::from_millis(10));
    };
    stdout.rewind()?;
    stderr.rewind()?;
    let mut out = String::new();
    let mut err = String::new();
    stdout.take(MAX_BYTES + 1).read_to_string(&mut out)?;
    stderr.take(MAX_BYTES + 1).read_to_string(&mut err)?;
    ensure!(
        out.len() as u64 <= MAX_BYTES && err.len() as u64 <= MAX_BYTES,
        "service command output exceeds bound"
    );
    ensure!(
        status.success(),
        "service command failed ({status}): {program} {}: {}",
        args.join(" "),
        err.trim()
    );
    Ok(out)
}

pub(super) async fn daemon_install(run: super::RunArgs, check: bool, json: bool) -> Result<()> {
    let home = dirs::home_dir().context("cannot determine HOME")?;
    let lock = MutationLock::acquire(&home)?;
    lock.reject_pending()?;
    let argv = run.service_argv(&home)?;
    let binary = std::env::current_exe().context("locate ww executable")?;
    let platform = Platform::current()?;
    let definition = render_service(platform, &binary, &argv, &home);
    let mut manager = NativeManager {
        platform,
        home: home.clone(),
    };
    let changed = install_locked(&lock, platform, definition.as_bytes(), check, &mut manager)?;
    let observed = manager.snapshot()?;
    let report = serde_json::json!({"version":1,"operation":"daemon install","changed":changed,"checked":check,"path":platform.path(&home),"activation":false,"definition_present":read_file(&home, &platform.path(&home))?.is_some(),"loaded":observed.registered,"running":observed.running,"enabled":observed.enabled});
    if json {
        println!("{}", serde_json::to_string(&report)?);
    } else {
        eprintln!(
            "{} service definition: {}",
            if check {
                "Checked"
            } else if changed {
                "Registered disabled"
            } else {
                "Preserved existing"
            },
            platform.path(&home).display()
        );
        eprintln!("Service start and login activation require an explicit native service command.");
    }
    Ok(())
}

fn install_locked(
    lock: &MutationLock,
    platform: Platform,
    definition: &[u8],
    check: bool,
    manager: &mut impl Manager,
) -> Result<bool> {
    let home = lock.home();
    validate_paths(home, platform)?;
    let path = platform.path(home);
    let existing = read_file(home, &path)?;
    let record = Registration::read(home)?;
    if let Some(bytes) = &existing {
        ensure!(
            owned(home, bytes, record.as_ref())?,
            "unrecognized service definition {}; remove or migrate it explicitly",
            path.display()
        );
        ensure!(bytes == definition, "existing service definition differs at {}; use ww daemon uninstall before reconfiguration", path.display());
        ensure!(
            !record
                .as_ref()
                .is_some_and(|record| record.phase == Phase::Removing),
            "daemon removal is incomplete; rerun ww daemon uninstall"
        );
        if record
            .as_ref()
            .is_none_or(|record| record.phase == Phase::Registered)
        {
            manager.snapshot()?;
            return Ok(false);
        }
    }
    if let Some(record) = &record {
        ensure!(record.phase == Phase::Removed || record.digest == digest(definition), "incomplete daemon transaction has a different definition; recover the original configuration first");
    }
    let before = manager.snapshot()?;
    ensure!(
        platform != Platform::Linux
            || before.enabled == EnabledState::Disabled
            || existing.is_some(),
        "systemd has stale enablement; registration made no changes"
    );
    let resuming_linux = platform == Platform::Linux
        && existing.is_some()
        && record
            .as_ref()
            .is_some_and(|record| record.phase == Phase::Installing)
        && before.enabled == EnabledState::Disabled;
    ensure!((!before.registered || resuming_linux) && before.running == RunningState::Stopped, "a service is already loaded without the matching owned definition; unregister it explicitly first");
    if check {
        return Ok(false);
    }
    let mut registration = Registration {
        version: 1,
        digest: digest(definition),
        phase: Phase::Installing,
        prior_enabled: record
            .as_ref()
            .filter(|record| record.phase == Phase::Installing)
            .map(|record| record.prior_enabled.clone())
            .unwrap_or(before.enabled),
    };
    registration.write(home)?;
    if platform == Platform::Macos {
        manager.disable(lock).context(
            "disable launchd registration before exposing plist; rerun registration to recover",
        )?;
        let log_dir = home.join(".ww/logs");
        parents(home, &log_dir.join("ww.log"))?;
        std::fs::create_dir_all(log_dir).context("create daemon log directory")?;
    }
    if existing.is_none() {
        write_file(home, &path, definition, true)?;
    }
    manager.reload(lock).context("service definition was written but manager reload failed; reload the user manager and rerun registration")?;
    ensure!(
        manager.disabled()?,
        "registered service is not disabled; registration remains incomplete"
    );
    ensure!(
        manager.snapshot()?.running == RunningState::Stopped,
        "service started during registration; registration remains incomplete"
    );
    registration.phase = Phase::Registered;
    registration.write(home)?;
    Ok(true)
}

pub(super) fn preflight_removal(lock: &MutationLock) -> Result<RemovalPlan> {
    let platform = Platform::current()?;
    let mut manager = NativeManager {
        platform,
        home: lock.home().to_owned(),
    };
    prepare_removal(lock, platform, &mut manager)
}
fn prepare_removal(
    lock: &MutationLock,
    platform: Platform,
    manager: &mut impl Manager,
) -> Result<RemovalPlan> {
    lock.reject_pending()?;
    let home = lock.home();
    validate_paths(home, platform)?;
    let bytes = read_file(home, &platform.path(home))?;
    let record = Registration::read(home)?;
    if let Some(bytes) = &bytes {
        ensure!(
            owned(home, bytes, record.as_ref())?,
            "unrecognized service definition {}; preserve it and remove it explicitly",
            platform.path(home).display()
        );
    }
    let before = manager.snapshot()?;
    ensure!(
        before.running != RunningState::Unknown && before.enabled != EnabledState::Unknown,
        "service state is unknown; removal made no changes"
    );
    ensure!(
        bytes.is_some()
            || (before.running == RunningState::Stopped
                && (!before.registered
                    || (record.as_ref().is_some_and(|r| r.phase == Phase::Removing)
                        && before.enabled == EnabledState::Disabled))),
        "loaded service has no owned definition; removal made no changes"
    );
    Ok(RemovalPlan {
        platform,
        bytes,
        before,
    })
}

pub(super) fn remove_locked(
    lock: &MutationLock,
    plan: RemovalPlan,
    check: bool,
) -> Result<RemovalReport> {
    let mut manager = NativeManager {
        platform: plan.platform,
        home: lock.home().to_owned(),
    };
    execute_removal(lock, plan, check, &mut manager)
}
fn execute_removal(
    lock: &MutationLock,
    plan: RemovalPlan,
    check: bool,
    manager: &mut impl Manager,
) -> Result<RemovalReport> {
    let home = lock.home();
    let path = plan.platform.path(home);
    let mut report = RemovalReport {
        changed: false,
        checked: check,
        path: path.clone(),
        backup: None,
        registered: plan.before.registered,
        running: matches!(plan.before.running, RunningState::Running { .. }),
        enabled: plan.before.enabled == EnabledState::Enabled,
    };
    let Some(bytes) = plan.bytes else {
        if let Some(mut record) = Registration::read(home)? {
            if record.phase == Phase::Removing {
                let backup = home
                    .join(".ww/daemon-backups")
                    .join(format!("{}.service", record.digest));
                let saved = read_file(home, &backup)?
                    .context("pending service removal backup is missing")?;
                ensure!(
                    digest(&saved) == record.digest,
                    "pending service removal backup changed"
                );
                report.backup = Some(backup);
                if !check {
                    manager
                        .reload(lock)
                        .context("finish interrupted service removal reload")?;
                    let after = manager.snapshot()?;
                    ensure!(
                        !after.registered
                            && after.running == RunningState::Stopped
                            && manager.disabled()?,
                        "interrupted service removal is not stopped, disabled, and unregistered"
                    );
                    record.phase = Phase::Removed;
                    record.write(home)?;
                    report.changed = true;
                    report.registered = false;
                    report.running = false;
                    report.enabled = false;
                }
            }
        }
        return Ok(report);
    };
    ensure!(
        read_file(home, &path)?.as_deref() == Some(bytes.as_slice()),
        "service definition changed after removal preflight"
    );
    ensure!(
        manager.snapshot()? == plan.before,
        "service state changed after removal preflight; retry removal"
    );
    let backup = home
        .join(".ww/daemon-backups")
        .join(format!("{}.service", digest(&bytes)));
    report.backup = Some(backup.clone());
    if check {
        return Ok(report);
    }
    match read_file(home, &backup)? {
        Some(saved) => ensure!(
            saved == bytes,
            "service backup collision: {}",
            backup.display()
        ),
        None => write_file(home, &backup, &bytes, true)?,
    }
    let mut record = Registration {
        version: 1,
        digest: digest(&bytes),
        phase: Phase::Removing,
        prior_enabled: plan.before.enabled,
    };
    record.write(home)?;
    manager
        .stop(lock, plan.before.registered)
        .context("service stop failed; registration and backup were preserved")?;
    manager
        .disable(lock)
        .context("service disable failed; registration and backup were preserved")?;
    ensure!(
        manager.snapshot()?.running == RunningState::Stopped && manager.disabled()?,
        "service is not stopped and disabled; registration and backup were preserved"
    );
    ensure!(
        read_file(home, &path)?.as_deref() == Some(bytes.as_slice()),
        "service definition changed during removal; definition was preserved"
    );
    std::fs::remove_file(&path).with_context(|| {
        format!(
            "remove stopped service {}; backup: {}",
            path.display(),
            backup.display()
        )
    })?;
    sync_parent(&path)?;
    manager.reload(lock).context("service is stopped and definition removed, but manager reload failed; retained backup permits recovery")?;
    let after = manager.snapshot()?;
    ensure!(!after.registered && after.running == RunningState::Stopped, "service definition removed but manager still reports registration; reload the manager before retrying");
    record.phase = Phase::Removed;
    record.write(home)?;
    report.changed = true;
    report.registered = false;
    report.running = false;
    report.enabled = false;
    Ok(report)
}

pub(super) async fn daemon_uninstall(check: bool, json: bool) -> Result<()> {
    let home = dirs::home_dir().context("cannot determine HOME")?;
    let lock = MutationLock::acquire(&home)?;
    let plan = preflight_removal(&lock)?;
    let report = remove_locked(&lock, plan, check)?;
    if json {
        println!(
            "{}",
            serde_json::json!({"version":1,"operation":"daemon uninstall","result":report})
        );
    } else {
        eprintln!(
            "{} service: {}",
            if check {
                "Checked removal of"
            } else if report.changed {
                "Removed"
            } else {
                "No registered"
            },
            report.path.display()
        );
        if let Some(backup) = report.backup {
            eprintln!("Service configuration backup: {}", backup.display());
        }
    }
    Ok(())
}

fn validate_paths(home: &Path, platform: Platform) -> Result<()> {
    parents(home, &platform.path(home))?;
    parents(home, &home.join(".ww/daemon.json"))?;
    let dropins = home.join(".config/systemd/user/ww.service.d");
    if platform == Platform::Linux {
        parents(home, &dropins)?;
        match std::fs::symlink_metadata(&dropins) {
            Ok(_) => bail!(
                "custom systemd drop-ins at {}; preserve and remove them explicitly",
                dropins.display()
            ),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => (),
            Err(error) => return Err(error).context("inspect service drop-ins"),
        }
    }
    Ok(())
}
fn parents(home: &Path, path: &Path) -> Result<()> {
    ensure!(path.starts_with(home), "service path escapes HOME");
    for parent in path.ancestors().skip(1) {
        match std::fs::symlink_metadata(parent) {
            Ok(metadata) => ensure!(
                metadata.is_dir(),
                "service ancestor is not a directory: {}",
                parent.display()
            ),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => (),
            Err(error) => return Err(error).context("inspect service ancestor"),
        }
        if parent == home {
            return Ok(());
        }
    }
    bail!("service path does not have HOME ancestor")
}
fn read_file(home: &Path, path: &Path) -> Result<Option<Vec<u8>>> {
    use std::os::unix::fs::OpenOptionsExt;
    parents(home, path)?;
    let file = match std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC)
        .open(path)
    {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(error).with_context(|| format!("read service file {}", path.display()))
        }
    };
    ensure!(
        file.metadata()?.is_file(),
        "service file is not regular: {}",
        path.display()
    );
    let mut bytes = Vec::new();
    file.take(MAX_BYTES + 1).read_to_end(&mut bytes)?;
    ensure!(
        bytes.len() as u64 <= MAX_BYTES,
        "service file exceeds 64 KiB: {}",
        path.display()
    );
    Ok(Some(bytes))
}
fn write_file(home: &Path, path: &Path, bytes: &[u8], new: bool) -> Result<()> {
    parents(home, path)?;
    ensure!(
        bytes.len() as u64 <= MAX_BYTES,
        "service file exceeds 64 KiB"
    );
    if !new {
        read_file(home, path)?;
    }
    let parent = path.parent().context("service file parent")?;
    std::fs::create_dir_all(parent)?;
    let mut temp = tempfile::NamedTempFile::new_in(parent)?;
    temp.write_all(bytes)?;
    temp.as_file().sync_all()?;
    if new {
        temp.persist_noclobber(path)
            .with_context(|| format!("create service file {}", path.display()))?;
    } else {
        temp.persist(path)
            .with_context(|| format!("replace service receipt {}", path.display()))?;
    }
    sync_parent(path)
}
fn sync_parent(path: &Path) -> Result<()> {
    std::fs::File::open(path.parent().context("service parent")?)?
        .sync_all()
        .context("sync service directory")
}

fn render_service(platform: Platform, binary: &Path, argv: &[String], home: &Path) -> String {
    let args: Vec<_> = std::iter::once(binary.display().to_string())
        .chain(argv.iter().cloned())
        .collect();
    match platform {
        Platform::Macos => format!("<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n<plist version=\"1.0\">\n<dict>\n    <key>Label</key>\n    <string>{LABEL}</string>\n    <key>Disabled</key>\n    <true/>\n    <key>ProgramArguments</key>\n    <array>\n{}\n    </array>\n    <key>StandardOutPath</key>\n    <string>{}</string>\n    <key>StandardErrorPath</key>\n    <string>{}</string>\n    <key>KeepAlive</key>\n    <true/>\n    <key>SoftResourceLimits</key>\n    <dict>\n        <key>NumberOfFiles</key>\n        <integer>4096</integer>\n    </dict>\n</dict>\n</plist>\n", args.iter().map(|arg| format!("        <string>{}</string>", xml_text(arg))).collect::<Vec<_>>().join("\n"), xml_text(&home.join(".ww/logs/ww.log").display().to_string()), xml_text(&home.join(".ww/logs/ww.log").display().to_string())),
        Platform::Linux => format!("[Unit]\nDescription=Wetware daemon\nAfter=network-online.target\nWants=network-online.target\n\n[Service]\nType=simple\nExecStart={}\nRestart=on-failure\nRestartSec=5\n\n[Install]\nWantedBy=default.target\n", args.iter().map(|arg| systemd_argument(arg)).collect::<Vec<_>>().join(" ")),
    }
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
#[path = "daemon_cmd_tests.rs"]
mod tests;
