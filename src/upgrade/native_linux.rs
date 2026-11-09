//! Bounded systemd observation and native evidence helpers.
//!
//! Observation does not authorize a restart. NativeService remains gated until
//! the engine, startup child, evidence protocol and disposable platform probes
//! establish the entire transaction contract.

#[cfg(all(test, target_os = "linux"))]
#[path = "native_linux_acceptance.rs"]
mod acceptance;

use std::collections::BTreeMap;
use std::ffi::CStr;
use std::fs::OpenOptions;
use std::io::{Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::fs::{FileTypeExt, MetadataExt, OpenOptionsExt};
use std::os::unix::net::UnixStream;
use std::path::{Component, Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{ensure, Context, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use super::attestation::VerifiedExecutable;
use super::inputs::{PreflightInputs, PreflightResponse};
use super::service::{observe_until, EnabledState, RunningState, ServiceSnapshot};
use super::startup::{ServiceIdentity, ServiceManager};

const LIMIT: usize = 64 * 1024;
const UNIT: &str = "ww.service";
const DESTINATION: &str = "org.freedesktop.systemd1";
const MANAGER: &str = "/org/freedesktop/systemd1";
const UNIT_PROPERTIES: &[&str] = &[
    "Id",
    "LoadState",
    "ActiveState",
    "SubState",
    "FragmentPath",
    "DropInPaths",
    "UnitFileState",
    "NeedDaemonReload",
    "InvocationID",
    "Job",
    "Transient",
    "JoinsNamespaceOf",
    "TriggeredBy",
];
const SERVICE_PROPERTIES: &[&str] = &[
    "Type",
    "MainPID",
    "ControlPID",
    "Result",
    "ExecStartEx",
    "ExecCondition",
    "ExecStartPre",
    "ExecStartPost",
    "ExecStop",
    "ExecStopPost",
    "Environment",
    "EnvironmentFiles",
    "PassEnvironment",
    "UnsetEnvironment",
    "WorkingDirectory",
    "RootDirectory",
    "RootImage",
    "User",
    "Group",
    "DynamicUser",
    "PAMName",
    "SetLoginEnvironment",
    "StandardInput",
    "PrivateTmp",
    "PrivateNetwork",
    "PrivateUsers",
    "PrivateMounts",
    "NetworkNamespacePath",
    "BindPaths",
    "BindReadOnlyPaths",
    "TemporaryFileSystem",
    "RuntimeDirectory",
    "StateDirectory",
    "CacheDirectory",
    "LogsDirectory",
    "ConfigurationDirectory",
    "SetCredential",
    "SetCredentialEncrypted",
    "LoadCredential",
    "LoadCredentialEncrypted",
    "ImportCredential",
    "LogNamespace",
    "PIDFile",
    "StandardOutput",
    "StandardError",
    "FileDescriptorStoreMax",
];

/// Ephemeral effective invocation. Environment values must not enter logs or
/// persisted evidence; ServiceIdentity contains digests instead.
pub struct ServiceInvocation {
    pub uid: u32,
    pub home: PathBuf,
    pub cwd: PathBuf,
    pub environment: BTreeMap<String, String>,
    pub argv: Vec<String>,
    pub identity: ServiceIdentity,
}

pub struct NativeObservation {
    pub invocation: ServiceInvocation,
    pub snapshot: ServiceSnapshot,
    pub invocation_id: [u8; 16],
}

/// Read the loaded owned unit without registering, enabling or starting it.
/// The caller decides whether the observed running/stopped state is admissible.
pub fn observe_invocation(home: &Path, deadline: Instant) -> Result<ServiceInvocation> {
    Ok(observe(home, deadline)?.invocation)
}

pub fn snapshot(home: &Path, deadline: Instant) -> Result<ServiceSnapshot> {
    let (uid, address, object) = observation_target(home, deadline)?;
    let before = capture_properties(&address, &object, deadline)?;
    if string(&before.unit, "LoadState")? == "loaded" {
        return Ok(observe(home, deadline)?.snapshot);
    }
    ensure!(
        string(&before.unit, "Id")? == UNIT
            && string(&before.unit, "LoadState")? == "not-found"
            && string(&before.unit, "ActiveState")? == "inactive"
            && string(&before.unit, "SubState")? == "dead"
            && string(&before.unit, "FragmentPath")?.is_empty()
            && unsigned(&before.service, "MainPID")? == 0
            && unsigned(&before.service, "ControlPID")? == 0,
        "systemd service absence is ambiguous"
    );
    ensure!(
        matches!(std::fs::symlink_metadata(home.join(".config/systemd/user/ww.service")),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound),
        "unloaded owned systemd definition exists"
    );
    let after = capture_properties(&address, &object, deadline)?;
    ensure!(
        before == after,
        "systemd state changed while establishing service absence"
    );
    Ok(ServiceSnapshot {
        registered: false,
        enabled: EnabledState::Disabled,
        running: RunningState::Stopped,
        failed: false,
        executable: None,
        configuration_identity: hex::encode(hash(
            b"wetware.systemd.absent.v1",
            &(uid, home, UNIT),
        )?),
    })
}

/// Match a managed launch to the manager's independently observed MainPID and
/// effective configuration. Executable-byte proof remains the caller's duty.
pub fn validate_managed_start(
    home: &Path,
    pid: u32,
    expected: &ServiceIdentity,
    deadline: Instant,
) -> Result<()> {
    let observed = observe(home, deadline)?;
    ensure!(
        observed.snapshot.running == RunningState::Running { pid }
            && pid != 0
            && observed.invocation_id != [0; 16],
        "systemd does not identify this process as the managed Host"
    );
    ensure!(
        &observed.invocation.identity == expected,
        "loaded systemd startup configuration changed; stop and explicitly admit the new inputs"
    );
    Ok(())
}

pub fn observe(home: &Path, deadline: Instant) -> Result<NativeObservation> {
    let (uid, address, object) = observation_target(home, deadline)?;
    let before = capture_properties(&address, &object, deadline)?;
    let observation = derive(home, uid, &before, deadline)?;
    let after = capture_properties(&address, &object, deadline)?;
    ensure!(
        before == after,
        "systemd state changed during effective startup observation"
    );
    // Re-read the owned definitions and environment files after manager queries.
    // Equality rejects edits even when the manager has not reported reload yet.
    let final_observation = derive(home, uid, &after, deadline)?;
    ensure!(
        observation.invocation.identity == final_observation.invocation.identity,
        "systemd definition or environment file changed during observation"
    );
    check_deadline(deadline)?;
    Ok(final_observation)
}

fn observation_target(home: &Path, deadline: Instant) -> Result<(u32, String, String)> {
    ensure!(
        cfg!(target_os = "linux"),
        "native systemd evidence is available only on Linux"
    );
    check_deadline(deadline)?;
    // SAFETY: these getters take no pointers.
    let uid = unsafe { libc::geteuid() };
    ensure!(
        unsafe { libc::getuid() } == uid,
        "setuid native service observation is unsupported"
    );
    directory_custody(home, uid)?;
    let runtime = PathBuf::from(format!("/run/user/{uid}"));
    directory_custody(&runtime, uid)?;
    let runtime_metadata = std::fs::symlink_metadata(&runtime)?;
    ensure!(
        runtime_metadata.uid() == uid && runtime_metadata.mode() & 0o077 == 0,
        "systemd user runtime directory is not private to the installation owner"
    );
    let bus = runtime.join("bus");
    let bus_metadata =
        std::fs::symlink_metadata(&bus).context("systemd user bus is unavailable")?;
    ensure!(
        bus_metadata.file_type().is_socket() && bus_metadata.uid() == uid,
        "systemd user bus must be owned by the installation user"
    );
    let address = format!("--address=unix:path={}", bus.display());
    // LoadUnit loads metadata without registering, enabling or starting the
    // service. Every query uses the installation owner's explicit private bus.
    let object = unit_object(&address, deadline)?;
    Ok((uid, address, object))
}

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct BusProperty {
    #[serde(rename = "type")]
    signature: String,
    data: serde_json::Value,
}
type Properties = BTreeMap<String, BusProperty>;

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
struct PropertyCapture {
    manager_environment: Vec<String>,
    unit: Properties,
    service: Properties,
}

fn capture_properties(address: &str, object: &str, deadline: Instant) -> Result<PropertyCapture> {
    let manager = query(
        address,
        MANAGER,
        "org.freedesktop.systemd1.Manager",
        &["Environment"],
        deadline,
    )?;
    Ok(PropertyCapture {
        manager_environment: strings(&manager, "Environment")?,
        unit: query(
            address,
            object,
            "org.freedesktop.systemd1.Unit",
            UNIT_PROPERTIES,
            deadline,
        )?,
        service: query(
            address,
            object,
            "org.freedesktop.systemd1.Service",
            SERVICE_PROPERTIES,
            deadline,
        )?,
    })
}

fn query(
    address: &str,
    object: &str,
    interface: &str,
    names: &[&str],
    deadline: Instant,
) -> Result<Properties> {
    let mut args = vec![
        address,
        "--auto-start=no",
        "--json=short",
        "get-property",
        DESTINATION,
        object,
        interface,
    ];
    args.extend_from_slice(names);
    let output = observe_until("/usr/bin/busctl", &args, observation_deadline(deadline))?;
    ensure!(
        output.status.success(),
        "cannot read typed properties from the owned systemd user manager"
    );
    properties(&output.stdout, names)
}

fn unit_object(address: &str, deadline: Instant) -> Result<String> {
    let output = observe_until(
        "/usr/bin/busctl",
        &[
            address,
            "--auto-start=no",
            "--json=short",
            "call",
            DESTINATION,
            MANAGER,
            "org.freedesktop.systemd1.Manager",
            "LoadUnit",
            "s",
            UNIT,
        ],
        observation_deadline(deadline),
    )?;
    ensure!(
        output.status.success(),
        "systemd does not expose the owned loaded unit"
    );
    let value: BusProperty =
        serde_json::from_str(&output.stdout).context("decode systemd unit object")?;
    ensure!(
        value.signature == "o",
        "unexpected systemd unit-object signature"
    );
    let values = value
        .data
        .as_array()
        .context("missing systemd unit-object result")?;
    ensure!(values.len() == 1, "ambiguous systemd unit-object result");
    let path = values[0]
        .as_str()
        .context("invalid systemd unit-object path")?;
    ensure!(
        path.starts_with("/org/freedesktop/systemd1/unit/")
            && path
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'/'),
        "invalid systemd unit-object namespace"
    );
    Ok(path.into())
}

fn properties(text: &str, names: &[&str]) -> Result<Properties> {
    ensure!(text.len() <= LIMIT, "systemd properties exceed 64 KiB");
    let mut values = serde_json::Deserializer::from_str(text).into_iter::<BusProperty>();
    let mut result = BTreeMap::new();
    for name in names {
        let property = values
            .next()
            .context("missing systemd property")?
            .context("decode typed systemd property")?;
        ensure!(
            result.insert((*name).to_owned(), property).is_none(),
            "duplicate systemd property request"
        );
    }
    ensure!(
        values.next().is_none(),
        "unexpected extra systemd property result"
    );
    Ok(result)
}

fn property<'a>(
    values: &'a Properties,
    name: &str,
    signature: &str,
) -> Result<&'a serde_json::Value> {
    let value = values
        .get(name)
        .with_context(|| format!("missing systemd property {name}"))?;
    ensure!(
        value.signature == signature,
        "unsupported systemd property signature for {name}"
    );
    Ok(&value.data)
}
fn string<'a>(values: &'a Properties, name: &str) -> Result<&'a str> {
    property(values, name, "s")?
        .as_str()
        .with_context(|| format!("invalid systemd string property {name}"))
}
fn boolean(values: &Properties, name: &str) -> Result<bool> {
    property(values, name, "b")?
        .as_bool()
        .with_context(|| format!("invalid systemd boolean property {name}"))
}
fn unsigned(values: &Properties, name: &str) -> Result<u32> {
    u32::try_from(
        property(values, name, "u")?
            .as_u64()
            .with_context(|| format!("invalid systemd integer property {name}"))?,
    )
    .context("systemd integer overflow")
}
fn strings(values: &Properties, name: &str) -> Result<Vec<String>> {
    string_array(property(values, name, "as")?)
}
fn string_array(value: &serde_json::Value) -> Result<Vec<String>> {
    value
        .as_array()
        .context("systemd value is not a string array")?
        .iter()
        .map(|value| {
            value
                .as_str()
                .map(str::to_owned)
                .context("systemd array contains a non-string")
        })
        .collect()
}
fn empty_array(values: &Properties, name: &str) -> Result<()> {
    ensure!(
        values
            .get(name)
            .context("missing systemd array")?
            .data
            .as_array()
            .is_some_and(Vec::is_empty),
        "unsupported nonempty systemd setting {name}"
    );
    Ok(())
}

fn derive(
    home: &Path,
    uid: u32,
    capture: &PropertyCapture,
    deadline: Instant,
) -> Result<NativeObservation> {
    let unit = &capture.unit;
    let service = &capture.service;
    ensure!(
        string(unit, "Id")? == UNIT && string(unit, "LoadState")? == "loaded",
        "systemd unit identity changed"
    );
    ensure!(
        !boolean(unit, "NeedDaemonReload")? && !boolean(unit, "Transient")?,
        "systemd reload or transient service is unsupported"
    );
    empty_array(unit, "JoinsNamespaceOf")?;
    empty_array(unit, "TriggeredBy")?;
    let job = property(unit, "Job", "(uo)")?
        .as_array()
        .context("invalid systemd pending job")?;
    ensure!(
        job.len() == 2 && job[0].as_u64() == Some(0),
        "systemd unit has a pending job"
    );
    ensure!(
        matches!(string(service, "Type")?, "simple" | "exec"),
        "native upgrade supports direct simple/exec systemd services"
    );
    for name in [
        "RootDirectory",
        "RootImage",
        "User",
        "Group",
        "PAMName",
        "NetworkNamespacePath",
        "LogNamespace",
        "PIDFile",
    ] {
        ensure!(
            string(service, name)?.is_empty(),
            "unsupported systemd startup setting {name}"
        );
    }
    for name in [
        "DynamicUser",
        "SetLoginEnvironment",
        "PrivateTmp",
        "PrivateNetwork",
        "PrivateUsers",
        "PrivateMounts",
    ] {
        ensure!(
            !boolean(service, name)?,
            "unsupported systemd startup setting {name}"
        );
    }
    ensure!(
        string(service, "StandardInput")? == "null",
        "systemd stdin activation is unsupported"
    );
    for name in ["StandardOutput", "StandardError"] {
        ensure!(
            matches!(string(service, name)?, "journal" | "null" | "inherit"),
            "unsupported systemd output setting {name}"
        );
    }
    ensure!(
        unsigned(service, "FileDescriptorStoreMax")? == 0,
        "systemd descriptor-store activation is unsupported"
    );
    for name in [
        "ExecCondition",
        "ExecStartPre",
        "ExecStartPost",
        "ExecStop",
        "ExecStopPost",
        "BindPaths",
        "BindReadOnlyPaths",
        "TemporaryFileSystem",
        "RuntimeDirectory",
        "StateDirectory",
        "CacheDirectory",
        "LogsDirectory",
        "ConfigurationDirectory",
        "SetCredential",
        "SetCredentialEncrypted",
        "LoadCredential",
        "LoadCredentialEncrypted",
        "ImportCredential",
    ] {
        empty_array(service, name)?;
    }
    ensure!(
        unsigned(service, "ControlPID")? == 0,
        "systemd control operation is active"
    );
    let pid = unsigned(service, "MainPID")?;
    let failed = string(service, "Result")? != "success";
    let running = match (string(unit, "ActiveState")?, string(unit, "SubState")?, pid) {
        ("active", "running", 1..) if !failed => RunningState::Running { pid },
        ("inactive", "dead", 0) | ("failed", "failed", 0) => RunningState::Stopped,
        _ => anyhow::bail!("systemd service state is transitioning or ambiguous"),
    };
    let enabled = match string(unit, "UnitFileState")? {
        "enabled" | "enabled-runtime" => EnabledState::Enabled,
        "disabled" | "static" => EnabledState::Disabled,
        _ => anyhow::bail!("systemd enablement state is unsupported"),
    };
    let fragment = PathBuf::from(string(unit, "FragmentPath")?);
    ensure!(
        fragment == home.join(".config/systemd/user/ww.service"),
        "loaded systemd fragment is outside the owned user-service path"
    );
    let definition = read_configuration(&fragment, uid, deadline)?;
    let mut drop_ins = Vec::new();
    for path in strings(unit, "DropInPaths")? {
        let bytes = read_configuration(Path::new(&path), uid, deadline)?;
        drop_ins.push((path, bytes));
    }
    let mut environment = assignments(&capture.manager_environment)?;
    for name in ["PATH", "USER", "XDG_RUNTIME_DIR"] {
        ensure!(
            environment.contains_key(name),
            "systemd generated environment fallback for {name} is unsupported"
        );
    }
    overlay(&mut environment, &strings(service, "Environment")?)?;
    let files = property(service, "EnvironmentFiles", "a(sb)")?
        .as_array()
        .context("invalid EnvironmentFiles")?;
    let mut environment_files = Vec::new();
    for file in files {
        let parts = file.as_array().context("invalid EnvironmentFile entry")?;
        ensure!(parts.len() == 2, "invalid EnvironmentFile tuple");
        let name = parts[0].as_str().context("invalid EnvironmentFile path")?;
        let optional = parts[1]
            .as_bool()
            .context("invalid EnvironmentFile optional flag")?;
        ensure!(
            !name.contains(['*', '?', '[', ']']),
            "wildcard EnvironmentFile paths are unsupported"
        );
        let path = Path::new(name);
        let bytes = match std::fs::symlink_metadata(path) {
            Err(error) if optional && error.kind() == std::io::ErrorKind::NotFound => {
                directory_custody(
                    path.parent().context("EnvironmentFile parent missing")?,
                    uid,
                )?;
                None
            }
            result => {
                result.context("inspect EnvironmentFile")?;
                Some(read_configuration(path, uid, deadline)?)
            }
        };
        if let Some(bytes) = &bytes {
            parse_environment_file(bytes, &mut environment)?;
        }
        environment_files.push((name.to_owned(), bytes));
    }
    // PassEnvironment has no filtering effect for a user manager. Its loaded
    // value remains covered by the stable property digest below.
    for name in strings(service, "PassEnvironment")? {
        valid_name(&name)?;
    }
    unset(&mut environment, &strings(service, "UnsetEnvironment")?)?;
    let actual_home = PathBuf::from(
        environment
            .get("HOME")
            .context("systemd effective HOME is unavailable")?,
    );
    ensure!(
        actual_home.is_absolute(),
        "systemd effective HOME must be absolute"
    );
    let working = string(service, "WorkingDirectory")?;
    let cwd = if working.is_empty() || working == "~" {
        passwd_home(uid)?
    } else {
        ensure!(
            !working.starts_with(['-', '!']),
            "optional systemd working directories are unsupported"
        );
        PathBuf::from(working)
    };
    directory_custody(&cwd, uid)?;
    let commands = property(service, "ExecStartEx", "a(sasasttttuii)")?
        .as_array()
        .context("invalid ExecStartEx")?;
    ensure!(
        commands.len() == 1,
        "systemd must execute one direct native Host command"
    );
    let command = commands[0]
        .as_array()
        .context("invalid ExecStartEx command")?;
    ensure!(command.len() == 10, "unsupported ExecStartEx tuple");
    let executable = command[0]
        .as_str()
        .context("invalid ExecStartEx executable")?;
    let argv = string_array(&command[1])?;
    let flags = string_array(&command[2])?;
    ensure!(
        flags.is_empty(),
        "systemd execution prefixes are unsupported"
    );
    ensure!(
        Path::new(executable) == home.join(".ww/bin/ww")
            && argv.first().map(String::as_str) == Some(executable)
            && argv.get(1).map(String::as_str) == Some("run"),
        "systemd must directly execute the owned Host run command"
    );
    ensure!(
        argv.iter().all(|arg| !arg.contains(['$', '\0'])),
        "systemd argv environment expansion is unsupported"
    );
    let mut stable_unit = unit.clone();
    for name in ["ActiveState", "SubState", "InvocationID", "Job"] {
        stable_unit.remove(name);
    }
    let mut stable_service = service.clone();
    for name in ["MainPID", "ControlPID", "Result", "ExecStartEx"] {
        stable_service.remove(name);
    }
    let identity = ServiceIdentity {
        manager: ServiceManager::Systemd,
        domain: format!("user/{uid}"),
        name: UNIT.into(),
        definition: hash(
            b"wetware.systemd.definition.v1",
            &(&definition, &stable_unit, &stable_service, &cwd),
        )?,
        drop_ins: hash(b"wetware.systemd.drop-ins.v1", &drop_ins)?,
        environment_files: hash(b"wetware.systemd.environment-files.v1", &environment_files)?,
        effective_argv: hash(b"wetware.systemd.argv.v1", &argv)?,
        effective_environment: hash(b"wetware.systemd.environment.v1", &environment)?,
        registered: true,
        enabled: enabled.clone(),
    };
    let invocation = property(unit, "InvocationID", "ay")?
        .as_array()
        .context("invalid systemd InvocationID")?;
    let invocation_id = if invocation.is_empty() {
        [0; 16]
    } else {
        ensure!(
            invocation.len() == 16,
            "invalid systemd InvocationID length"
        );
        let bytes: Vec<u8> = invocation
            .iter()
            .map(|value| {
                u8::try_from(value.as_u64().context("invalid InvocationID byte")?)
                    .context("InvocationID byte overflow")
            })
            .collect::<Result<_>>()?;
        bytes.try_into().expect("checked length")
    };
    ensure!(
        matches!(running, RunningState::Stopped) || invocation_id != [0; 16],
        "running systemd process has no InvocationID"
    );
    let snapshot = ServiceSnapshot {
        registered: true,
        enabled,
        running,
        failed,
        executable: Some(PathBuf::from(executable)),
        configuration_identity: hex::encode(hash(b"wetware.systemd.identity.v1", &identity)?),
    };
    Ok(NativeObservation {
        invocation: ServiceInvocation {
            uid,
            home: actual_home,
            cwd,
            environment,
            argv,
            identity,
        },
        snapshot,
        invocation_id,
    })
}

fn valid_name(name: &str) -> Result<()> {
    ensure!(
        !name.is_empty()
            && name.bytes().enumerate().all(|(i, b)| b == b'_'
                || b.is_ascii_alphabetic()
                || (i > 0 && b.is_ascii_digit())),
        "invalid systemd environment variable name"
    );
    Ok(())
}

fn assignment(value: &str) -> Result<(String, String)> {
    let (name, value) = value
        .split_once('=')
        .context("systemd environment assignment is missing =")?;
    valid_name(name)?;
    ensure!(
        !value.contains('\0'),
        "systemd environment value contains NUL"
    );
    // These are supplied per invocation by systemd. Explicit manager/unit/file
    // assignments are rejected rather than silently removed from the digest.
    ensure!(
        !matches!(
            name,
            "INVOCATION_ID"
                | "SYSTEMD_EXEC_PID"
                | "JOURNAL_STREAM"
                | "NOTIFY_SOCKET"
                | "LISTEN_PID"
                | "LISTEN_FDS"
                | "LISTEN_FDNAMES"
                | "WATCHDOG_PID"
                | "WATCHDOG_USEC"
                | "MEMORY_PRESSURE_WATCH"
                | "MEMORY_PRESSURE_WRITE"
                | "MANAGERPID"
                | "MAINPID"
                | "RUNTIME_DIRECTORY"
                | "STATE_DIRECTORY"
                | "CACHE_DIRECTORY"
                | "LOGS_DIRECTORY"
                | "CONFIGURATION_DIRECTORY"
                | "CREDENTIALS_DIRECTORY"
                | "LOG_NAMESPACE"
                | "PIDFILE"
                | "FDSTORE"
                | "REMOTE_ADDR"
                | "REMOTE_PORT"
                | "TRIGGER_UNIT"
                | "TRIGGER_PATH"
                | "TRIGGER_TIMER_REALTIME_USEC"
                | "TRIGGER_TIMER_MONOTONIC_USEC"
        ) && !name.starts_with("LD_")
            && !name.starts_with("DYLD_"),
        "explicit systemd-generated or loader environment assignment is unsupported"
    );
    Ok((name.into(), value.into()))
}
fn assignments(values: &[String]) -> Result<BTreeMap<String, String>> {
    let mut result = BTreeMap::new();
    overlay(&mut result, values)?;
    Ok(result)
}
fn overlay(values: &mut BTreeMap<String, String>, assignments: &[String]) -> Result<()> {
    for value in assignments {
        let (name, value) = assignment(value)?;
        values.insert(name, value);
    }
    Ok(())
}
fn unset(values: &mut BTreeMap<String, String>, removals: &[String]) -> Result<()> {
    for removal in removals {
        if let Some((name, value)) = removal.split_once('=') {
            valid_name(name)?;
            if values.get(name).is_some_and(|current| current == value) {
                values.remove(name);
            }
        } else {
            valid_name(removal)?;
            values.remove(removal);
        }
    }
    Ok(())
}

/// Deliberately bounded EnvironmentFile subset: single-line quoted/unquoted
/// assignments. Multiline quoting/continuation refuses rather than guessing.
fn parse_environment_file(
    bytes: &[u8],
    values: &mut BTreeMap<String, String>,
) -> Result<BTreeMap<String, String>> {
    ensure!(bytes.len() <= LIMIT, "EnvironmentFile exceeds 64 KiB");
    let text = std::str::from_utf8(bytes).context("EnvironmentFile is not UTF-8")?;
    ensure!(
        text.chars().all(|c| c != '\0'
            && c != '\u{feff}'
            && !(0xfdd0..=0xfdef).contains(&(c as u32))
            && (c as u32 & 0xffff) < 0xfffe),
        "EnvironmentFile contains a forbidden Unicode value"
    );
    for line in text.lines() {
        let line = line.trim_matches([' ', '\t', '\r']);
        if line.is_empty() || line.starts_with(['#', ';']) || !line.contains('=') {
            continue;
        }
        let (name, raw) = line.split_once('=').expect("checked separator");
        let name = name.trim_end_matches([' ', '\t', '\r']);
        valid_name(name)?;
        let raw = raw.trim_matches([' ', '\t', '\r']);
        let value = if let Some(inner) = raw.strip_prefix('\'') {
            let inner = inner
                .strip_suffix('\'')
                .context("multiline/suffixed single-quoted EnvironmentFile value is unsupported")?;
            ensure!(
                !inner.contains('\''),
                "multiple single-quoted EnvironmentFile segments are unsupported"
            );
            inner.to_owned()
        } else if let Some(inner) = raw.strip_prefix('"') {
            let inner = inner
                .strip_suffix('"')
                .context("multiline/suffixed double-quoted EnvironmentFile value is unsupported")?;
            let mut value = String::new();
            let mut chars = inner.chars();
            while let Some(c) = chars.next() {
                if c == '\\' {
                    let next = chars
                        .next()
                        .context("EnvironmentFile continuation is unsupported")?;
                    if !matches!(next, '"' | '\\' | '`' | '$') {
                        value.push('\\');
                    }
                    value.push(next);
                } else {
                    ensure!(
                        c != '"',
                        "multiple double-quoted EnvironmentFile segments are unsupported"
                    );
                    value.push(c);
                }
            }
            value
        } else {
            let mut value = String::new();
            let mut chars = raw.chars();
            while let Some(c) = chars.next() {
                if c == '\\' {
                    value.push(
                        chars
                            .next()
                            .context("EnvironmentFile continuation is unsupported")?,
                    );
                } else {
                    value.push(c);
                }
            }
            value
        };
        let (name, value) = assignment(&format!("{name}={value}"))?;
        values.insert(name, value);
    }
    Ok(values.clone())
}

fn directory_custody(path: &Path, uid: u32) -> Result<()> {
    ensure!(
        path.is_absolute()
            && path
                .components()
                .all(|c| matches!(c, Component::RootDir | Component::Normal(_))),
        "systemd input path must be absolute and normalized"
    );
    let mut prefix = PathBuf::new();
    for component in path.components() {
        prefix.push(component.as_os_str());
        let metadata =
            std::fs::symlink_metadata(&prefix).context("inspect systemd input directory")?;
        ensure!(metadata.is_dir() && (metadata.uid()==0 || metadata.uid()==uid)
            && (metadata.mode() & 0o022 == 0 || (metadata.uid()==0 && metadata.mode() & libc::S_ISVTX as u32 != 0)),
            "systemd input directory {} must be owned without symlinks or unprotected other-user writes", prefix.display());
    }
    Ok(())
}

fn read_configuration(path: &Path, uid: u32, deadline: Instant) -> Result<Vec<u8>> {
    check_deadline(deadline)?;
    directory_custody(path.parent().context("systemd input parent missing")?, uid)?;
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_NONBLOCK)
        .open(path)
        .context("open systemd input without following symlinks")?;
    let before = file.metadata()?;
    ensure!(
        before.is_file()
            && before.nlink() == 1
            && (before.uid() == uid || before.uid() == 0)
            && before.mode() & 0o022 == 0
            && before.len() <= LIMIT as u64,
        "systemd input custody or size is unsupported"
    );
    let mut bytes = Vec::new();
    (&file).take(LIMIT as u64 + 1).read_to_end(&mut bytes)?;
    let after = file.metadata()?;
    let named = std::fs::symlink_metadata(path)?;
    ensure!(
        bytes.len() <= LIMIT
            && before.len() == after.len()
            && before.dev() == after.dev()
            && before.ino() == after.ino()
            && before.mtime() == after.mtime()
            && before.mtime_nsec() == after.mtime_nsec()
            && before.ctime() == after.ctime()
            && before.ctime_nsec() == after.ctime_nsec()
            && named.is_file()
            && named.dev() == after.dev()
            && named.ino() == after.ino(),
        "systemd input changed during capture"
    );
    check_deadline(deadline)?;
    Ok(bytes)
}

fn passwd_home(uid: u32) -> Result<PathBuf> {
    let mut buffer = vec![0u8; LIMIT];
    // SAFETY: getpwuid_r initializes the supplied passwd and places its pointed
    // strings in the live bounded buffer. The returned pointer is checked.
    let mut pwd: libc::passwd = unsafe { std::mem::zeroed() };
    let mut result = std::ptr::null_mut();
    let status = unsafe {
        libc::getpwuid_r(
            uid,
            &mut pwd,
            buffer.as_mut_ptr().cast(),
            buffer.len(),
            &mut result,
        )
    };
    ensure!(
        status == 0 && !result.is_null() && !pwd.pw_dir.is_null(),
        "systemd user home lookup failed"
    );
    // SAFETY: successful getpwuid_r returns a NUL-terminated string in buffer.
    let home = unsafe { CStr::from_ptr(pwd.pw_dir) }
        .to_str()
        .context("systemd user home is not UTF-8")?;
    let home = PathBuf::from(home);
    ensure!(home.is_absolute(), "systemd passwd home is not absolute");
    Ok(home)
}

fn hash<T: Serialize>(domain: &[u8], value: &T) -> Result<[u8; 32]> {
    let bytes = serde_json::to_vec(value)?;
    ensure!(
        bytes.len() <= 1024 * 1024,
        "systemd configuration exceeds bound"
    );
    let mut hash = Sha256::new();
    hash.update((domain.len() as u64).to_le_bytes());
    hash.update(domain);
    hash.update((bytes.len() as u64).to_le_bytes());
    hash.update(bytes);
    Ok(hash.finalize().into())
}
fn check_deadline(deadline: Instant) -> Result<()> {
    ensure!(
        Instant::now() < deadline,
        "native systemd operation deadline expired"
    );
    Ok(())
}
fn observation_deadline(deadline: Instant) -> Instant {
    deadline.min(Instant::now() + Duration::from_secs(10))
}

pub fn verify_guard_capability(executable: &VerifiedExecutable, deadline: Instant) -> Result<()> {
    let response = launch_preflight(executable, None, deadline)?;
    ensure!(
        response.guard_capable && response.capture_digest.is_none() && response.prepared.is_none(),
        "candidate does not implement the required startup admission protocol"
    );
    Ok(())
}

pub fn prepare(
    executable: &VerifiedExecutable,
    inputs: &PreflightInputs,
    deadline: Instant,
) -> Result<PreflightResponse> {
    inputs.validate()?;
    let response = launch_preflight(executable, Some(inputs), deadline)?;
    ensure!(
        response.guard_capable
            && response.capture_digest == Some(inputs.capture_digest()?)
            && response.prepared.is_some(),
        "candidate preflight does not match captured startup inputs"
    );
    Ok(response)
}

pub(crate) fn compatible(
    executable: &VerifiedExecutable,
    inputs: &PreflightInputs,
    deadline: Instant,
) -> Result<super::service::CompatibilityEvidence> {
    let response = prepare(executable, inputs, deadline)?;
    let evidence = super::service::CompatibilityEvidence {
        artifact: response.artifact,
        capture_digest: response.capture_digest.context("missing capture digest")?,
        protocol_version: response.version,
        prepared: response.prepared.context("missing prepared head")?,
    };
    evidence.verify(executable, inputs)?;
    Ok(evidence)
}

pub struct LiveEvidence {
    pub process: super::attestation::PinnedProcess,
    pub response: super::inputs::ReadinessResponse,
    pub service: ServiceIdentity,
    pub invocation_id: [u8; 16],
}

fn process_instance(live: &LiveEvidence) -> super::service::ProcessInstance {
    super::service::ProcessInstance {
        pid: live.process.pid(),
        start: live.process.start_token().clone(),
        launch_nonce: live.response.launch_nonce,
        manager_invocation_id: hex::encode(live.invocation_id),
    }
}

pub(crate) fn admit_restart(
    home: &Path,
    before: &ServiceSnapshot,
    contract: &super::startup::StartupContractV1,
    executable: &VerifiedExecutable,
    deadline: Instant,
) -> Result<super::service::RestartAdmission> {
    ensure!(
        before.registered && !before.failed,
        "live restart requires a healthy registered service"
    );
    let observed = observe(home, deadline)?;
    ensure!(
        &observed.snapshot == before,
        "service changed before live admission"
    );
    let live = observe_ready(home, contract, executable, None, deadline)?;
    ensure!(
        before.running
            == RunningState::Running {
                pid: live.process.pid()
            },
        "service process changed during admission"
    );
    Ok(super::service::RestartAdmission {
        contract: contract.clone(),
        original_process: process_instance(&live),
        service: live.service,
        transaction_id: [0; 32],
    })
}

pub(crate) fn revalidate_restart(
    home: &Path,
    admission: &super::service::RestartAdmission,
    deadline: Instant,
) -> Result<()> {
    admission.contract.validate()?;
    admission.original_process.validate()?;
    let observed = observe(home, deadline)?;
    ensure!(
        observed.invocation.identity == admission.service,
        "admitted service configuration changed"
    );
    if let RunningState::Running { pid } = observed.snapshot.running {
        ensure!(
            pid == admission.original_process.pid,
            "admitted service has a different running process"
        );
        let process = super::attestation::PinnedProcess::capture(
            pid,
            &admission.contract.artifact,
            deadline,
        )?;
        let live = observe_ready(
            home,
            &admission.contract,
            process.executable(),
            Some(admission.original_process.launch_nonce),
            deadline,
        )?;
        ensure!(
            process_instance(&live) == admission.original_process,
            "admitted process birth or execution changed"
        );
    }
    Ok(())
}

pub(crate) fn quiesce(
    home: &Path,
    intent: &super::service::ServiceDisposition,
    deadline: Instant,
) -> Result<super::service::ServiceDisposition> {
    intent.validate()?;
    ensure!(
        intent.stop_intent,
        "quiescence requires durable stop intent"
    );
    if intent.identity.is_none() {
        let current = snapshot(home, deadline)?;
        ensure!(
            current == intent.original && current.running == RunningState::Stopped,
            "standalone service disposition changed before quiescence"
        );
        let mut completed = intent.clone();
        completed.quiesced = Some(current);
        completed.validate()?;
        return Ok(completed);
    }
    let identity = intent
        .identity
        .as_ref()
        .context("quiescence lacks owned service identity")?;
    let observed = observe(home, deadline)?;
    ensure!(
        &observed.invocation.identity == identity
            && observed.snapshot.enabled == intent.original.enabled,
        "service configuration or automatic-start disposition changed before quiescence"
    );
    if let RunningState::Running { pid } = observed.snapshot.running {
        let live = identify_quiescence_target(home, intent, pid, deadline)?;
        let confirmed = identify_quiescence_target(home, intent, pid, deadline)?;
        ensure!(
            process_instance(&live) == process_instance(&confirmed)
                && live.response.contract == confirmed.response.contract
                && live.response.transaction_id == confirmed.response.transaction_id
                && live.response.permit_digest == confirmed.response.permit_digest,
            "owned execution changed immediately before quiescence"
        );
        live.process.revalidate(deadline)?;
        manager_action(home, identity, "StopUnit", deadline)?;
    }
    let stopped = await_stopped(home, identity, deadline)?;
    let mut completed = intent.clone();
    completed.quiesced = Some(stopped);
    completed.validate()?;
    Ok(completed)
}

fn identify_quiescence_target(
    home: &Path,
    intent: &super::service::ServiceDisposition,
    pid: u32,
    deadline: Instant,
) -> Result<LiveEvidence> {
    if let (Some(contract), Some(permit)) = (&intent.quiescence_contract, &intent.quiescence_permit)
    {
        let candidate = (|| -> Result<LiveEvidence> {
            permit.validate()?;
            ensure!(
                permit.contract == contract.identity && permit.invocation == contract.invocation,
                "quiescence permit differs from the journal-selected contract"
            );
            let process =
                super::attestation::PinnedProcess::capture(pid, &contract.artifact, deadline)?;
            let live = observe_execution(home, contract, process.executable(), None, deadline)?;
            ensure!(
                live.response.transaction_id == Some(permit.transaction_id)
                    && live.response.permit_digest == Some(permit.digest()?),
                "quiescence execution identifies another transaction permit"
            );
            Ok(live)
        })();
        if let Ok(live) = candidate {
            return Ok(live);
        }
        check_deadline(deadline)?;
    }
    let original = intent
        .original_process
        .as_ref()
        .context("quiescence lacks original process identity")?;
    let contract = intent
        .original_contract
        .as_ref()
        .context("quiescence lacks original startup contract")?;
    ensure!(
        pid == original.pid,
        "service runs an unrelated process before quiescence"
    );
    let process = super::attestation::PinnedProcess::capture(pid, &contract.artifact, deadline)?;
    let live = observe_execution(
        home,
        contract,
        process.executable(),
        Some(original.launch_nonce),
        deadline,
    )?;
    ensure!(
        process_instance(&live) == *original,
        "original process changed before quiescence"
    );
    Ok(live)
}

pub(crate) fn stop(home: &Path, expected: &ServiceIdentity, deadline: Instant) -> Result<()> {
    let observed = observe(home, deadline)?;
    ensure!(
        &observed.invocation.identity == expected,
        "service containment identity changed"
    );
    if matches!(observed.snapshot.running, RunningState::Running { .. }) {
        manager_action(home, expected, "StopUnit", deadline)?;
    }
    await_stopped(home, expected, deadline)?;
    Ok(())
}

pub(crate) fn restart(
    home: &Path,
    admission: &super::service::RestartAdmission,
    permit: &super::StartupPermit,
    executable: &VerifiedExecutable,
    deadline: Instant,
) -> Result<super::service::RunningEvidence> {
    ensure!(
        admission.transaction_id != [0; 32]
            && permit.payload().transaction_id == admission.transaction_id,
        "restart permit identifies another transaction"
    );
    ensure!(
        permit.contract().invocation
            == super::startup::InvocationBinding::Managed {
                service: admission.service.clone()
            }
            && &permit.contract().artifact == executable.artifact(),
        "restart permit identifies another service or executable"
    );
    executable.revalidate(deadline)?;
    let observed = observe(home, deadline)?;
    ensure!(
        observed.invocation.identity == admission.service,
        "service changed before guarded restart"
    );
    if observed.snapshot.running == RunningState::Stopped {
        manager_action(home, &admission.service, "StartUnit", deadline)?;
    }
    let live = loop {
        check_deadline(deadline)?;
        match observe_ready(home, permit.contract(), executable, None, deadline) {
            Ok(live) => break live,
            Err(error) => {
                if let Ok(current) = observe(home, deadline) {
                    ensure!(
                        current.invocation.identity == admission.service,
                        "service changed while awaiting guarded readiness"
                    );
                    check_restart_progress(&current.snapshot)?;
                }
                if deadline.saturating_duration_since(Instant::now()) <= Duration::from_millis(10) {
                    return Err(error)
                        .context("guarded service did not establish readiness before deadline");
                }
                std::thread::sleep(Duration::from_millis(10));
            }
        }
    };
    let evidence = super::service::RunningEvidence {
        process: process_instance(&live),
        artifact: live.response.artifact,
        startup: live.response.startup,
        service: live.service,
        contract: live.response.contract,
        epoch_seq: live.response.epoch_seq,
        effective_root: live.response.effective_root,
        kernel_source: live.response.kernel_source,
        kernel_runtime: live.response.kernel_runtime,
        transaction_id: live.response.transaction_id,
        permit_digest: live.response.permit_digest,
    };
    evidence.verify(admission, permit, executable)?;
    Ok(evidence)
}

fn check_restart_progress(snapshot: &ServiceSnapshot) -> Result<()> {
    ensure!(
        !(snapshot.failed && snapshot.running == RunningState::Stopped),
        "managed Host exited before serving readiness"
    );
    Ok(())
}

fn manager_action(
    home: &Path,
    expected: &ServiceIdentity,
    action: &str,
    deadline: Instant,
) -> Result<()> {
    ensure!(
        matches!(action, "StartUnit" | "StopUnit"),
        "unsupported native manager action"
    );
    let current = observe(home, deadline)?;
    ensure!(
        &current.invocation.identity == expected,
        "service changed immediately before manager action"
    );
    let address = format!(
        "--address=unix:path=/run/user/{}/bus",
        current.invocation.uid
    );
    let result = observe_until(
        "/usr/bin/busctl",
        &[
            &address,
            "--auto-start=no",
            "--json=short",
            "call",
            DESTINATION,
            MANAGER,
            "org.freedesktop.systemd1.Manager",
            action,
            "ss",
            UNIT,
            "replace",
        ],
        deadline,
    )?;
    ensure!(
        result.status.success(),
        "native manager {action} request failed; reobserve durable service intent before retry"
    );
    check_deadline(deadline)
}

fn await_stopped(
    home: &Path,
    expected: &ServiceIdentity,
    deadline: Instant,
) -> Result<ServiceSnapshot> {
    loop {
        check_deadline(deadline)?;
        if let Ok(observed) = observe(home, deadline) {
            ensure!(
                &observed.invocation.identity == expected,
                "service configuration changed while stopping"
            );
            if observed.snapshot.running == RunningState::Stopped {
                return Ok(observed.snapshot);
            }
        }
        std::thread::sleep(
            Duration::from_millis(5).min(deadline.saturating_duration_since(Instant::now())),
        );
    }
}

/// Independently bind a ready response to the loaded manager configuration,
/// exact running executable, process birth, connected peer and launch nonce.
pub fn observe_ready(
    home: &Path,
    contract: &super::startup::StartupContractV1,
    executable: &VerifiedExecutable,
    launch_nonce: Option<[u8; 32]>,
    deadline: Instant,
) -> Result<LiveEvidence> {
    let live = observe_execution(home, contract, executable, launch_nonce, deadline)?;
    ensure!(
        live.response.serving_ready
            && !live.response.effective_root.is_empty()
            && !live.response.kernel_source.is_empty()
            && !live.response.kernel_runtime.is_empty(),
        "managed Host has not established coherent serving readiness"
    );
    Ok(live)
}

pub fn observe_execution(
    home: &Path,
    contract: &super::startup::StartupContractV1,
    executable: &VerifiedExecutable,
    launch_nonce: Option<[u8; 32]>,
    deadline: Instant,
) -> Result<LiveEvidence> {
    use super::inputs::{
        evidence_socket_path, ReadinessChallenge, ReadinessResponse, MAX_NATIVE_RESPONSE_BYTES,
        NATIVE_PROTOCOL_VERSION,
    };
    contract.validate()?;
    let expected = match &contract.invocation {
        super::startup::InvocationBinding::Managed { service } => service,
        _ => anyhow::bail!("managed readiness requires a managed startup contract"),
    };
    ensure!(
        &contract.artifact == executable.artifact(),
        "readiness contract identifies another executable"
    );
    let before = observe(home, deadline)?;
    ensure!(
        &before.invocation.identity == expected,
        "managed readiness configuration changed"
    );
    let pid = match before.snapshot.running {
        RunningState::Running { pid } => pid,
        _ => anyhow::bail!("managed Host is not running"),
    };
    let process = super::attestation::PinnedProcess::capture(pid, executable.artifact(), deadline)?;
    ensure!(
        process.executable().file_identity() == executable.file_identity(),
        "managed Host runs another executable inode"
    );
    let path = evidence_socket_path(home, pid, process.start_token())?;
    directory_custody(
        path.parent().context("readiness directory missing")?,
        before.invocation.uid,
    )?;
    let metadata =
        std::fs::symlink_metadata(&path).context("managed Host readiness socket is unavailable")?;
    ensure!(
        metadata.file_type().is_socket()
            && metadata.uid() == before.invocation.uid
            && metadata.mode() & 0o077 == 0,
        "managed Host readiness socket custody changed"
    );
    let stream = connect_stream(&path, deadline)?;
    process.verify_peer(&stream, deadline)?;
    let challenge = ReadinessChallenge {
        version: NATIVE_PROTOCOL_VERSION,
        nonce: fresh_nonce(),
        launch_nonce,
        timeout_millis: u64::try_from(
            deadline
                .saturating_duration_since(Instant::now())
                .as_millis(),
        )?
        .min(300_000),
    };
    ensure!(
        challenge.timeout_millis > 0,
        "readiness operation deadline expired"
    );
    let bytes = serde_json::to_vec(&challenge)?;
    write_stream(&stream, &(bytes.len() as u32).to_be_bytes(), deadline)?;
    write_stream(&stream, &bytes, deadline)?;
    let mut length = [0; 4];
    read_stream(&stream, &mut length, deadline)?;
    let length = u32::from_be_bytes(length) as usize;
    ensure!(
        length > 0 && length <= MAX_NATIVE_RESPONSE_BYTES,
        "readiness response exceeds bound"
    );
    let mut bytes = vec![0; length];
    read_stream(&stream, &mut bytes, deadline)?;
    let response: ReadinessResponse =
        serde_json::from_slice(&bytes).context("decode managed readiness evidence")?;
    ensure!(
        response.version == NATIVE_PROTOCOL_VERSION
            && response.nonce == challenge.nonce
            && response.launch_nonce != [0; 32]
            && launch_nonce.is_none_or(|nonce| nonce == response.launch_nonce)
            && response.contract == contract.identity
            && response.artifact == contract.artifact
            && response.startup == contract.startup,
        "managed readiness evidence binding mismatch"
    );
    process.verify_peer(&stream, deadline)?;
    executable.revalidate(deadline)?;
    let after = observe(home, deadline)?;
    ensure!(
        before.snapshot == after.snapshot
            && before.invocation.identity == after.invocation.identity
            && before.invocation_id == after.invocation_id,
        "managed Host changed during readiness exchange"
    );
    process.revalidate(deadline)?;
    require_live_stream(&stream, deadline)?;
    write_stream(&stream, &[0x06], deadline)?;
    Ok(LiveEvidence {
        process,
        response,
        service: after.invocation.identity,
        invocation_id: after.invocation_id,
    })
}

pub(crate) fn revalidate_running(
    home: &Path,
    evidence: &super::service::RunningEvidence,
    permit: &super::StartupPermit,
    executable: &VerifiedExecutable,
    deadline: Instant,
) -> Result<()> {
    let live = observe_ready(
        home,
        permit.contract(),
        executable,
        Some(evidence.process.launch_nonce),
        deadline,
    )?;
    ensure!(
        process_instance(&live) == evidence.process
            && live.service == evidence.service
            && live.response.contract == evidence.contract
            && live.response.artifact == evidence.artifact
            && live.response.startup == evidence.startup
            && live.response.transaction_id == Some(permit.payload().transaction_id)
            && live.response.permit_digest == Some(permit.payload().digest()?),
        "running process, startup contract or transaction changed before commit"
    );
    Ok(())
}

fn fresh_nonce() -> [u8; 32] {
    loop {
        let nonce = rand::random();
        if nonce != [0; 32] {
            return nonce;
        }
    }
}

fn connect_stream(path: &Path, deadline: Instant) -> Result<UnixStream> {
    use std::os::fd::FromRawFd;
    use std::os::unix::ffi::OsStrExt;
    check_deadline(deadline)?;
    // SAFETY: zero initializes the address before bounded path copying.
    let mut address: libc::sockaddr_un = unsafe { std::mem::zeroed() };
    address.sun_family = libc::AF_UNIX as libc::sa_family_t;
    let bytes = path.as_os_str().as_bytes();
    ensure!(
        bytes.len() < address.sun_path.len() && !bytes.contains(&0),
        "invalid readiness socket path"
    );
    for (target, byte) in address.sun_path.iter_mut().zip(bytes) {
        *target = *byte as libc::c_char;
    }
    // SAFETY: socket returns a new descriptor, owned immediately below.
    let fd = unsafe { libc::socket(libc::AF_UNIX, libc::SOCK_STREAM, 0) };
    ensure!(
        fd >= 0,
        "create readiness socket: {}",
        std::io::Error::last_os_error()
    );
    // SAFETY: the new descriptor is uniquely owned.
    let stream = unsafe { UnixStream::from_raw_fd(fd) };
    // SAFETY: fcntl operates on the live descriptor.
    ensure!(
        unsafe { libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC) } == 0,
        "seal readiness socket descriptor"
    );
    stream.set_nonblocking(true)?;
    // SAFETY: address points to a fully initialized sockaddr_un.
    let status = unsafe {
        libc::connect(
            fd,
            (&address as *const libc::sockaddr_un).cast(),
            std::mem::size_of_val(&address) as libc::socklen_t,
        )
    };
    if status != 0 {
        let error = std::io::Error::last_os_error();
        ensure!(
            matches!(
                error.raw_os_error(),
                Some(libc::EINPROGRESS) | Some(libc::EAGAIN)
            ),
            "connect readiness socket: {error}"
        );
        wait_stream(&stream, libc::POLLOUT, deadline)?;
        if let Some(error) = stream.take_error()? {
            return Err(error.into());
        }
    }
    check_deadline(deadline)?;
    Ok(stream)
}

fn wait_stream(stream: &UnixStream, events: libc::c_short, deadline: Instant) -> Result<()> {
    loop {
        let remaining = deadline
            .checked_duration_since(Instant::now())
            .context("native evidence deadline expired")?;
        let millis = remaining
            .as_millis()
            .saturating_add(1)
            .min(i32::MAX as u128) as i32;
        let mut descriptor = libc::pollfd {
            fd: stream.as_raw_fd(),
            events,
            revents: 0,
        };
        // SAFETY: poll receives one live descriptor and a bounded timeout.
        let result = unsafe { libc::poll(&mut descriptor, 1, millis) };
        if result < 0 {
            let error = std::io::Error::last_os_error();
            if error.kind() == std::io::ErrorKind::Interrupted {
                continue;
            }
            return Err(error.into());
        }
        check_deadline(deadline)?;
        if result > 0 {
            ensure!(
                descriptor.revents & libc::POLLNVAL == 0,
                "native evidence descriptor closed"
            );
            return Ok(());
        }
    }
}

fn require_live_stream(stream: &UnixStream, deadline: Instant) -> Result<()> {
    loop {
        check_deadline(deadline)?;
        let mut byte = 0u8;
        // SAFETY: recv peeks one byte into a live writable buffer without
        // blocking. The complete bounded response has already been consumed.
        let count = unsafe {
            libc::recv(
                stream.as_raw_fd(),
                (&mut byte as *mut u8).cast(),
                1,
                libc::MSG_PEEK | libc::MSG_DONTWAIT,
            )
        };
        if count < 0 {
            let error = std::io::Error::last_os_error();
            if error.kind() == std::io::ErrorKind::Interrupted {
                continue;
            }
            ensure!(
                error.kind() == std::io::ErrorKind::WouldBlock,
                "native evidence connection failed before acknowledgement: {error}"
            );
            return Ok(());
        }
        anyhow::bail!("native evidence peer closed or sent extra data before acknowledgement");
    }
}

fn write_stream(mut stream: &UnixStream, mut bytes: &[u8], deadline: Instant) -> Result<()> {
    while !bytes.is_empty() {
        check_deadline(deadline)?;
        match stream.write(bytes) {
            Ok(0) => anyhow::bail!("native evidence peer closed during write"),
            Ok(count) => bytes = &bytes[count..],
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                wait_stream(stream, libc::POLLOUT, deadline)?
            }
            Err(error) => return Err(error.into()),
        }
    }
    check_deadline(deadline)
}

fn read_stream(mut stream: &UnixStream, mut bytes: &mut [u8], deadline: Instant) -> Result<()> {
    while !bytes.is_empty() {
        check_deadline(deadline)?;
        match stream.read(bytes) {
            Ok(0) => anyhow::bail!("native evidence peer closed during read"),
            Ok(count) => bytes = &mut bytes[count..],
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                wait_stream(stream, libc::POLLIN, deadline)?
            }
            Err(error) => return Err(error.into()),
        }
    }
    check_deadline(deadline)
}

fn launch_preflight(
    executable: &VerifiedExecutable,
    inputs: Option<&PreflightInputs>,
    deadline: Instant,
) -> Result<PreflightResponse> {
    #[cfg(not(target_os = "linux"))]
    {
        let _ = (executable, inputs, deadline);
        anyhow::bail!(
            "native preflight exact-executable evidence is not qualified on this platform"
        )
    }
    #[cfg(target_os = "linux")]
    {
        use super::inputs::{
            PreflightRequest, MAX_NATIVE_RESPONSE_BYTES, MAX_PREFLIGHT_REQUEST_BYTES,
            NATIVE_PROTOCOL_VERSION,
        };
        use std::os::unix::process::CommandExt;
        use std::process::{Command, Stdio};
        executable.revalidate(deadline)?;
        let (parent, child_endpoint) = UnixStream::pair()?;
        parent.set_nonblocking(true)?;
        let control_fd = child_endpoint.as_raw_fd();
        let executable_fd = executable.descriptor().as_raw_fd();
        let request = PreflightRequest {
            version: NATIVE_PROTOCOL_VERSION,
            nonce: fresh_nonce(),
            artifact: executable.artifact().clone(),
            inputs: inputs.cloned(),
            timeout_millis: u64::try_from(
                deadline
                    .saturating_duration_since(Instant::now())
                    .as_millis()
                    .min(300_000),
            )?,
        };
        ensure!(
            request.timeout_millis > 0,
            "native preflight deadline expired"
        );
        let bytes = serde_json::to_vec(&request)?;
        ensure!(
            bytes.len() <= MAX_PREFLIGHT_REQUEST_BYTES,
            "native preflight request exceeds bound"
        );
        let mut command = Command::new(format!("/proc/self/fd/{executable_fd}"));
        command
            .args(["startup", "preflight", "--fd", &control_fd.to_string()])
            .env_clear()
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        if let Some(inputs) = inputs {
            command
                .current_dir(&inputs.config.cwd)
                .env("HOME", &inputs.config.home);
        }
        // SAFETY: only async-signal-safe fcntl runs after fork. Both descriptors
        // remain owned in the parent until spawn completes. Only the child copy
        // clears CLOEXEC; the candidate immediately restores it on its control FD.
        unsafe {
            command.pre_exec(move || {
                // ELF execve resolves /proc/self/fd/N before closing CLOEXEC
                // descriptors. The executable descriptor remains CLOEXEC.
                for fd in [control_fd] {
                    let flags = libc::fcntl(fd, libc::F_GETFD);
                    if flags < 0 || libc::fcntl(fd, libc::F_SETFD, flags & !libc::FD_CLOEXEC) < 0 {
                        return Err(std::io::Error::last_os_error());
                    }
                }
                Ok(())
            });
        }
        let mut child = command
            .spawn()
            .context("launch authenticated native preflight")?;
        drop(child_endpoint);
        let result = (|| {
            write_stream(&parent, &(bytes.len() as u32).to_be_bytes(), deadline)?;
            write_stream(&parent, &bytes, deadline)?;
            let mut length = [0u8; 4];
            read_stream(&parent, &mut length, deadline)
                .context("native preflight did not return protocol evidence")?;
            let length = u32::from_be_bytes(length) as usize;
            ensure!(
                length > 0 && length <= MAX_NATIVE_RESPONSE_BYTES,
                "native preflight response exceeds bound"
            );
            let mut response = vec![0; length];
            read_stream(&parent, &mut response, deadline)?;
            let response: PreflightResponse =
                serde_json::from_slice(&response).context("decode native preflight evidence")?;
            ensure!(
                response.version == NATIVE_PROTOCOL_VERSION
                    && response.nonce == request.nonce
                    && response.artifact == *executable.artifact(),
                "native preflight response identity mismatch"
            );
            // socketpair credentials identify its creator, not the exec child.
            // The exclusive inherited endpoint is correlated with the spawned
            // Child PID, whose complete executable is independently measured.
            let pinned = super::attestation::PinnedProcess::capture(
                child.id(),
                executable.artifact(),
                deadline,
            )?;
            ensure!(
                pinned.executable().file_identity() == executable.file_identity(),
                "native preflight executed a different inode"
            );
            executable.revalidate(deadline)?;
            pinned.revalidate(deadline)?;
            require_live_stream(&parent, deadline)?;
            write_stream(&parent, &[0x06], deadline)?;
            loop {
                check_deadline(deadline)?;
                if let Some(status) = child.try_wait()? {
                    ensure!(
                        status.success(),
                        "native preflight child failed after evidence exchange"
                    );
                    return Ok(response);
                }
                std::thread::sleep(Duration::from_millis(5));
            }
        })();
        if result.is_err() {
            let _ = child.kill();
            let _ = child.wait();
        }
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::{symlink, PermissionsExt};

    fn fixture() -> (tempfile::TempDir, PathBuf, PropertyCapture) {
        let temp = tempfile::tempdir().unwrap();
        let home = temp.path().canonicalize().unwrap();
        let mut capture: PropertyCapture =
            serde_json::from_str(include_str!("../../tests/fixtures/native_systemd_255.json"))
                .unwrap();
        let fragment = home.join(".config/systemd/user/ww.service");
        std::fs::create_dir_all(fragment.parent().unwrap()).unwrap();
        for directory in [
            home.clone(),
            home.join(".config"),
            home.join(".config/systemd"),
            home.join(".config/systemd/user"),
        ] {
            std::fs::set_permissions(directory, std::fs::Permissions::from_mode(0o700)).unwrap();
        }
        std::fs::write(&fragment, "[Service]\nExecStart=fixture\n").unwrap();
        std::fs::set_permissions(&fragment, std::fs::Permissions::from_mode(0o600)).unwrap();
        capture.unit.get_mut("FragmentPath").unwrap().data = serde_json::json!(fragment);
        capture.service.get_mut("WorkingDirectory").unwrap().data = serde_json::json!(home);
        let executable = home.join(".ww/bin/ww");
        capture.service.get_mut("ExecStartEx").unwrap().data =
            serde_json::json!([[executable, [executable, "run"], [], 0, 0, 0, 0, 0, 0, 0]]);
        capture
            .manager_environment
            .retain(|value| !value.starts_with("HOME="));
        capture
            .manager_environment
            .push(format!("HOME={}", home.display()));
        (temp, home, capture)
    }

    fn derived(home: &Path, capture: &PropertyCapture) -> Result<NativeObservation> {
        derive(
            home,
            unsafe { libc::getuid() },
            capture,
            Instant::now() + Duration::from_secs(2),
        )
    }

    #[test]
    fn actual_systemd_stopped_fixture_and_definition_drift() {
        let (_temp, home, capture) = fixture();
        let before = derived(&home, &capture).unwrap();
        assert_eq!(before.snapshot.running, RunningState::Stopped);
        assert_eq!(before.snapshot.enabled, EnabledState::Disabled);
        assert_eq!(before.invocation.environment["WW_LOG_LEVEL"], "info");
        std::fs::write(
            home.join(".config/systemd/user/ww.service"),
            "[Service]\nExecStart=changed\n",
        )
        .unwrap();
        let after = derived(&home, &capture).unwrap();
        assert_ne!(before.invocation.identity, after.invocation.identity);
    }

    #[test]
    fn terminated_candidate_does_not_consume_the_readiness_deadline() {
        let (_temp, home, mut capture) = fixture();
        capture.unit.get_mut("ActiveState").unwrap().data = serde_json::json!("failed");
        capture.unit.get_mut("SubState").unwrap().data = serde_json::json!("failed");
        capture.service.get_mut("Result").unwrap().data = serde_json::json!("exit-code");
        let failed = derived(&home, &capture).unwrap();
        assert!(check_restart_progress(&failed.snapshot).is_err());
        let mut uncertain = failed.snapshot.clone();
        uncertain.running = RunningState::Unknown;
        assert!(check_restart_progress(&uncertain).is_ok());
        capture.unit.get_mut("ActiveState").unwrap().data = serde_json::json!("activating");
        capture.unit.get_mut("SubState").unwrap().data = serde_json::json!("auto-restart");
        assert!(derived(&home, &capture).is_err());
    }

    #[test]
    fn refuse_loaded_hooks_pending_jobs_and_expanded_argv() {
        let (_temp, home, capture) = fixture();
        for working in [format!("!{}", home.display()), "!~".into()] {
            let mut changed = capture.clone();
            changed.service.get_mut("WorkingDirectory").unwrap().data = serde_json::json!(working);
            let error = derived(&home, &changed)
                .err()
                .expect("optional cwd must refuse");
            assert!(
                error
                    .to_string()
                    .contains("optional systemd working directories"),
                "{error:#}"
            );
        }
        let mut changed = capture.clone();
        changed.service.get_mut("ExecStartPre").unwrap().data = serde_json::json!([["/bin/true"]]);
        assert!(derived(&home, &changed).is_err());
        let mut changed = capture.clone();
        changed.unit.get_mut("Job").unwrap().data =
            serde_json::json!([37, "/org/freedesktop/systemd1/job/37"]);
        assert!(derived(&home, &changed).is_err());
        let mut changed = capture.clone();
        changed.service.get_mut("ExecStartEx").unwrap().data[0][1][1] = serde_json::json!("$MODE");
        assert!(derived(&home, &changed).is_err());
        let mut changed = capture.clone();
        changed.unit.get_mut("NeedDaemonReload").unwrap().data = serde_json::json!(true);
        assert!(derived(&home, &changed).is_err());
    }

    #[test]
    fn refuse_symlink_hardlink_and_writable_configuration() {
        let (_temp, home, capture) = fixture();
        let fragment = home.join(".config/systemd/user/ww.service");
        let renamed = home.join("unit-copy");
        std::fs::rename(&fragment, &renamed).unwrap();
        symlink(&renamed, &fragment).unwrap();
        assert!(derived(&home, &capture).is_err());
        std::fs::remove_file(&fragment).unwrap();
        std::fs::hard_link(&renamed, &fragment).unwrap();
        assert!(derived(&home, &capture).is_err());
        std::fs::remove_file(&renamed).unwrap();
        std::fs::set_permissions(&fragment, std::fs::Permissions::from_mode(0o666)).unwrap();
        assert!(derived(&home, &capture).is_err());
    }

    #[test]
    fn refuse_explicit_generated_environment_and_missing_defaults() {
        let (_temp, home, capture) = fixture();
        for assignment in [
            "INVOCATION_ID=forged",
            "SYSTEMD_EXEC_PID=1",
            "LD_PRELOAD=/tmp/x.so",
            "CREDENTIALS_DIRECTORY=/tmp/keys",
        ] {
            let mut changed = capture.clone();
            changed.service.get_mut("Environment").unwrap().data = serde_json::json!([assignment]);
            assert!(derived(&home, &changed).is_err(), "{assignment}");
        }
        let mut changed = capture.clone();
        changed
            .manager_environment
            .retain(|value| !value.starts_with("PATH="));
        assert!(derived(&home, &changed).is_err());
    }

    #[test]
    fn native_stream_uses_one_deadline_across_partial_reads() {
        let (parent, mut child) = UnixStream::pair().unwrap();
        parent.set_nonblocking(true).unwrap();
        let writer = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(20));
            let _ = child.write_all(&[1]);
            std::thread::sleep(Duration::from_millis(35));
            let _ = child.write_all(&[2]);
        });
        let start = Instant::now();
        assert!(read_stream(&parent, &mut [0; 2], start + Duration::from_millis(35)).is_err());
        writer.join().unwrap();
    }

    #[test]
    fn completed_response_requires_a_live_silent_peer_until_ack() {
        let (parent, mut child) = UnixStream::pair().unwrap();
        let deadline = Instant::now() + Duration::from_secs(1);
        assert!(require_live_stream(&parent, deadline).is_ok());
        child.write_all(&[1]).unwrap();
        assert!(require_live_stream(&parent, deadline).is_err());
        parent.set_nonblocking(true).unwrap();
        read_stream(&parent, &mut [0; 1], deadline).unwrap();
        drop(child);
        assert!(require_live_stream(&parent, deadline).is_err());
    }

    #[test]
    #[ignore = "requires the explicit disposable ww739 systemd container"]
    fn disposable_systemd_observation_matches_running_process() {
        assert_eq!(
            std::env::var("WW739_DISPOSABLE_SYSTEMD").as_deref(),
            Ok("1")
        );
        assert_eq!(unsafe { libc::geteuid() }, 1001);
        let deadline = Instant::now() + Duration::from_secs(20);
        let observed = observe(Path::new("/home/testww"), deadline).unwrap();
        let pid = match observed.snapshot.running {
            RunningState::Running { pid } => pid,
            _ => panic!("fixture is not running"),
        };
        assert_ne!(observed.invocation_id, [0; 16]);
        let bytes = std::fs::read(format!("/proc/{pid}/environ")).unwrap();
        let mut actual: BTreeMap<String, String> = bytes
            .split(|byte| *byte == 0)
            .filter(|part| !part.is_empty())
            .map(|part| {
                let value = std::str::from_utf8(part).unwrap();
                let (name, value) = value.split_once('=').unwrap();
                (name.to_owned(), value.to_owned())
            })
            .collect();
        assert_eq!(
            actual.remove("INVOCATION_ID").unwrap(),
            hex::encode(observed.invocation_id)
        );
        assert_eq!(actual.remove("SYSTEMD_EXEC_PID").unwrap(), pid.to_string());
        assert!(actual.remove("MANAGERPID").is_some());
        assert!(actual.remove("JOURNAL_STREAM").is_some());
        assert_eq!(actual, observed.invocation.environment);
        let again = observe(Path::new("/home/testww"), deadline).unwrap();
        assert_eq!(again.snapshot, observed.snapshot);
        assert_eq!(again.invocation.identity, observed.invocation.identity);
    }

    #[test]
    fn environment_files_preserve_values_without_shell_evaluation() {
        let result = parse_environment_file(b"# comment\nA=one two\nB=' literal $HOME '\nC=\"quoted \\\"value\\\" $HOME\"\nD=one\\ two\nE=pre\"quote\"\n", &mut BTreeMap::new()).unwrap();
        assert_eq!(result["A"], "one two");
        assert_eq!(result["B"], " literal $HOME ");
        assert_eq!(result["C"], "quoted \"value\" $HOME");
        assert_eq!(result["D"], "one two");
        assert_eq!(result["E"], "pre\"quote\"");
    }

    #[test]
    fn environment_files_refuse_unsupported_or_invalid_syntax() {
        for bytes in [
            b"A='multiline\nvalue'".as_slice(),
            b"A=continued\\\nline",
            b"A=zero\0",
            b"1INVALID=value",
            b"A=\"unterminated",
        ] {
            assert!(parse_environment_file(bytes, &mut BTreeMap::new()).is_err());
        }
    }

    #[test]
    fn unset_environment_applies_after_overrides() {
        let mut values = assignments(&["A=manager".into(), "B=manager".into()]).unwrap();
        overlay(&mut values, &["A=unit".into()]).unwrap();
        parse_environment_file(b"A=file\nC=file\n", &mut values).unwrap();
        unset(&mut values, &["A=unit".into(), "B".into(), "C=file".into()]).unwrap();
        assert_eq!(values, BTreeMap::from([("A".into(), "file".into())]));
    }

    #[test]
    fn bus_properties_preserve_typed_json_and_reject_extra_or_wrong_types() {
        let data = "{\"type\":\"s\",\"data\":\"loaded\"}\n{\"type\":\"b\",\"data\":false}\n";
        let values = properties(data, &["LoadState", "NeedDaemonReload"]).unwrap();
        assert_eq!(string(&values, "LoadState").unwrap(), "loaded");
        assert!(!boolean(&values, "NeedDaemonReload").unwrap());
        assert!(boolean(&values, "LoadState").is_err());
        assert!(properties(data, &["LoadState"]).is_err());
    }
}
