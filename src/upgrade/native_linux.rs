//! Bounded observation of effective systemd configuration, manager-owned stop,
//! executable file observation, and existing loopback /readyz readiness.

use std::collections::BTreeMap;
use std::ffi::CStr;
use std::fs::OpenOptions;
use std::io::{Read, Write};
use std::os::unix::fs::{FileTypeExt, MetadataExt, OpenOptionsExt};
use std::path::{Component, Path, PathBuf};
use std::time::{Duration, Instant};

use crate::installation_state::MutationLock;
use anyhow::{ensure, Context, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use super::service::{observe_until, EnabledState, RunningState, ServiceSnapshot};

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
/// persisted evidence; the service snapshot contains only a configuration digest.
struct ServiceInvocation {
    pub uid: u32,
    pub environment: BTreeMap<String, String>,
    pub argv: Vec<String>,
}

struct NativeObservation {
    pub invocation: ServiceInvocation,
    pub snapshot: ServiceSnapshot,
    pub invocation_id: [u8; 16],
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

fn observe(home: &Path, deadline: Instant) -> Result<NativeObservation> {
    observe_state(home, deadline, false)
}
fn observe_stop(home: &Path, deadline: Instant) -> Result<NativeObservation> {
    observe_state(home, deadline, true)
}
fn observe_state(home: &Path, deadline: Instant, stopping: bool) -> Result<NativeObservation> {
    let (uid, address, object) = observation_target(home, deadline)?;
    let before = capture_properties(&address, &object, deadline)?;
    let observation = if stopping {
        derive_stop(home, uid, &before, deadline)?
    } else {
        derive(home, uid, &before, deadline)?
    };
    let after = capture_properties(&address, &object, deadline)?;
    ensure!(
        stopping || before == after,
        "systemd state changed during effective startup observation"
    );
    // Re-read the owned definitions and environment files after manager queries.
    // Equality rejects edits even when the manager has not reported reload yet.
    let final_observation = if stopping {
        derive_stop(home, uid, &after, deadline)?
    } else {
        derive(home, uid, &after, deadline)?
    };
    ensure!(
        observation.snapshot.configuration_identity
            == final_observation.snapshot.configuration_identity,
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

fn derive_stop(
    home: &Path,
    uid: u32,
    capture: &PropertyCapture,
    deadline: Instant,
) -> Result<NativeObservation> {
    derive_state(home, uid, capture, deadline, true)
}

fn derive(
    home: &Path,
    uid: u32,
    capture: &PropertyCapture,
    deadline: Instant,
) -> Result<NativeObservation> {
    derive_state(home, uid, capture, deadline, false)
}
fn derive_state(
    home: &Path,
    uid: u32,
    capture: &PropertyCapture,
    deadline: Instant,
    stopping: bool,
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
        job.len() == 2 && job[0].as_u64().is_some_and(|job| stopping || job == 0),
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
        stopping || unsigned(service, "ControlPID")? == 0,
        "systemd control operation is active"
    );
    let pid = unsigned(service, "MainPID")?;
    let failed = string(service, "Result")? != "success";
    let running = match (string(unit, "ActiveState")?, string(unit, "SubState")?, pid) {
        ("active", "running", 1..) if !failed => RunningState::Running { pid },
        ("inactive", "dead", 0) | ("failed", "failed", 0) => RunningState::Stopped,
        _ if stopping && pid > 0 => RunningState::Running { pid },
        _ if stopping => RunningState::Unknown,
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
        actual_home == home,
        "systemd effective HOME must match the installation"
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
    let configuration_identity = hex::encode(hash(
        b"wetware.systemd.configuration.v3",
        &(
            &definition,
            &stable_unit,
            &stable_service,
            &cwd,
            &drop_ins,
            &environment_files,
            &argv,
            &environment,
        ),
    )?);
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
        !matches!(running, RunningState::Running { .. }) || invocation_id != [0; 16],
        "running systemd process has no InvocationID"
    );
    let snapshot = ServiceSnapshot {
        registered: true,
        enabled,
        running,
        failed,
        executable: Some(PathBuf::from(executable)),
        configuration_identity,
    };
    Ok(NativeObservation {
        invocation: ServiceInvocation {
            uid,
            environment,
            argv,
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
            && (metadata.mode() & 0o022 == 0 || (metadata.uid()==0 && u64::from(metadata.mode()) & u64::from(libc::S_ISVTX) != 0)),
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

fn readiness_address(invocation: &ServiceInvocation) -> Result<std::net::SocketAddr> {
    let mut value = invocation
        .environment
        .get("WW_HTTP_ADMIN")
        .map(String::as_str)
        .unwrap_or("127.0.0.1:2026");
    let mut args = invocation.argv.iter().skip(2);
    let mut explicit = false;
    while let Some(arg) = args.next() {
        if arg == "--" {
            break;
        }
        if arg == "--with-http-admin" || arg.starts_with("--with-http-admin=") {
            ensure!(
                !explicit,
                "multiple admin endpoint arguments are unsupported"
            );
            explicit = true;
            value = if arg == "--with-http-admin" {
                args.next()
                    .context("admin endpoint value missing")?
                    .as_str()
            } else {
                &arg[18..]
            };
        }
    }
    let address: std::net::SocketAddr = value.parse().context("managed restart requires a determinate loopback /readyz endpoint; admin off is unsupported")?;
    ensure!(
        address.ip().is_loopback() && address.port() != 0,
        "managed restart requires a fixed loopback /readyz endpoint"
    );
    Ok(address)
}

pub fn preflight(home: &Path, before: &ServiceSnapshot, deadline: Instant) -> Result<()> {
    ensure!(
        before.registered,
        "unmanaged automatic native replacement is unsupported"
    );
    let current = observe(home, deadline)?;
    ensure!(
        &current.snapshot == before,
        "service changed during configuration preflight"
    );
    if matches!(before.running, RunningState::Running { .. }) {
        readiness_address(&current.invocation)?;
        observe_executable(&current, home)?;
    }
    Ok(())
}

fn manager_action(
    home: &Path,
    before: &ServiceSnapshot,
    action: &str,
    mutation: &MutationLock,
    deadline: Instant,
) -> Result<()> {
    ensure!(
        mutation.home() == home,
        "manager command lock belongs to another installation"
    );
    let current = if action == "StopUnit" {
        observe_stop(home, deadline)?
    } else {
        observe(home, deadline)?
    };
    super::same_configuration(before, &current.snapshot)?;
    let address = format!(
        "--address=unix:path=/run/user/{}/bus",
        current.invocation.uid
    );
    let result = super::service::action_until(
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
        mutation,
        deadline,
    )?;
    ensure!(
        result.status.success(),
        "systemd {action} failed: {}",
        result.stderr
    );
    check_deadline(deadline)
}

#[cfg(target_os = "linux")]
fn process_handle(pid: u32) -> Result<Option<std::os::fd::OwnedFd>> {
    use std::os::fd::FromRawFd;
    // SAFETY: pidfd_open takes scalar arguments and returns an owned descriptor.
    let fd = unsafe { libc::syscall(libc::SYS_pidfd_open, pid, 0) };
    if fd < 0 {
        let error = std::io::Error::last_os_error();
        if error.raw_os_error() == Some(libc::ESRCH) {
            return Ok(None);
        }
        return Err(error).context("observe old process exit with pidfd");
    }
    Ok(Some(unsafe {
        std::os::fd::OwnedFd::from_raw_fd(fd as i32)
    }))
}
#[cfg(not(target_os = "linux"))]
fn process_handle(_: u32) -> Result<Option<std::os::fd::OwnedFd>> {
    anyhow::bail!("process exit observation requires Linux")
}

fn exited(handle: &std::os::fd::OwnedFd) -> Result<bool> {
    use std::os::fd::AsRawFd;
    let mut poll = libc::pollfd {
        fd: handle.as_raw_fd(),
        events: libc::POLLIN,
        revents: 0,
    };
    // SAFETY: poll has a valid single-element array, with no retained pointers.
    let status = unsafe { libc::poll(&mut poll, 1, 0) };
    ensure!(
        status >= 0,
        "observe process exit: {}",
        std::io::Error::last_os_error()
    );
    Ok(poll.revents & (libc::POLLIN | libc::POLLHUP) != 0)
}

pub fn quiesce(
    home: &Path,
    before: &ServiceSnapshot,
    mutation: &MutationLock,
    deadline: Instant,
) -> Result<ServiceSnapshot> {
    let current = observe_stop(home, deadline)?;
    super::same_configuration(before, &current.snapshot)?;
    let handle = match current.snapshot.running {
        RunningState::Running { pid } => process_handle(pid)?,
        _ => None,
    };
    // StopUnit is sent even if MainPID is zero: it also cancels manager restart intent.
    manager_action(home, before, "StopUnit", mutation, deadline)?;
    loop {
        check_deadline(deadline)?;
        if let Ok(now) = observe(home, deadline) {
            super::same_configuration(before, &now.snapshot)?;
            if now.snapshot.running == RunningState::Stopped
                && handle.as_ref().map(exited).transpose()?.unwrap_or(true)
            {
                return Ok(now.snapshot);
            }
        }
        std::thread::sleep(
            Duration::from_millis(10).min(deadline.saturating_duration_since(Instant::now())),
        );
    }
}

fn observe_executable(current: &NativeObservation, home: &Path) -> Result<()> {
    let RunningState::Running { pid } = current.snapshot.running else {
        anyhow::bail!("service is not running")
    };
    let actual =
        std::fs::metadata(format!("/proc/{pid}/exe")).context("observe managed executable")?;
    let installed = std::fs::metadata(home.join(".ww/bin/ww"))?;
    ensure!(
        actual.is_file() && actual.dev() == installed.dev() && actual.ino() == installed.ino(),
        "managed process is not executing the installed Host file"
    );
    Ok(())
}

fn probe_ready(address: std::net::SocketAddr, deadline: Instant) -> Result<()> {
    let remaining = || -> Result<Duration> {
        check_deadline(deadline)?;
        Ok(deadline
            .saturating_duration_since(Instant::now())
            .min(Duration::from_millis(250)))
    };
    let mut stream = std::net::TcpStream::connect_timeout(&address, remaining()?)?;
    stream.set_write_timeout(Some(remaining()?))?;
    write!(
        stream,
        "GET /readyz HTTP/1.1\r\nHost: {address}\r\nConnection: close\r\n\r\n"
    )?;
    let mut response = Vec::new();
    loop {
        stream.set_read_timeout(Some(remaining()?))?;
        let mut bytes = [0; 1024];
        let count = stream.read(&mut bytes)?;
        ensure!(count > 0, "readiness endpoint closed without HTTP headers");
        response.extend_from_slice(&bytes[..count]);
        ensure!(response.len() <= LIMIT, "readiness response exceeds bound");
        if response.windows(4).any(|part| part == b"\r\n\r\n") {
            break;
        }
    }
    let line = response
        .split(|b| *b == b'\n')
        .next()
        .context("readiness status absent")?;
    ensure!(
        line.starts_with(b"HTTP/1.1 200 ") || line.starts_with(b"HTTP/1.0 200 "),
        "Host /readyz does not report serving readiness"
    );
    Ok(())
}

/// Verify an existing process without starting or resetting the service.
pub fn verify_ready(
    home: &Path,
    before: &ServiceSnapshot,
    deadline: Instant,
) -> Result<ServiceSnapshot> {
    let current = observe(home, deadline)?;
    super::same_configuration(before, &current.snapshot)?;
    ensure!(!current.snapshot.failed, "managed Host has failed");
    observe_executable(&current, home)?;
    probe_ready(readiness_address(&current.invocation)?, deadline)?;
    let after = observe(home, deadline)?;
    super::same_configuration(before, &after.snapshot)?;
    ensure!(
        !after.snapshot.failed
            && current.snapshot.running == after.snapshot.running
            && current.invocation_id == after.invocation_id,
        "managed Host changed during readiness verification"
    );
    observe_executable(&after, home)?;
    Ok(after.snapshot)
}

pub fn start_ready(
    home: &Path,
    before: &ServiceSnapshot,
    mutation: &MutationLock,
    deadline: Instant,
) -> Result<ServiceSnapshot> {
    let current = observe(home, deadline)?;
    super::same_configuration(before, &current.snapshot)?;
    ensure!(
        current.snapshot.running == RunningState::Stopped,
        "service must be stopped before managed restart"
    );
    let address = readiness_address(&current.invocation)?;
    manager_action(home, before, "StartUnit", mutation, deadline)?;
    loop {
        check_deadline(deadline)?;
        if let Ok(now) = observe(home, deadline) {
            super::same_configuration(before, &now.snapshot)?;
            if now.snapshot.failed {
                anyhow::bail!("managed Host failed to start; explicit recovery is required");
            }
            if matches!(now.snapshot.running, RunningState::Running { .. }) {
                observe_executable(&now, home)?;
                if probe_ready(address, deadline).is_ok() {
                    let after = observe(home, deadline)?;
                    super::same_configuration(before, &after.snapshot)?;
                    if now.snapshot.running == after.snapshot.running
                        && now.invocation_id == after.invocation_id
                    {
                        observe_executable(&after, home)?;
                        return Ok(after.snapshot);
                    }
                }
            }
        }
        std::thread::sleep(
            Duration::from_millis(20).min(deadline.saturating_duration_since(Instant::now())),
        );
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
    fn stop_observation_accepts_transient_jobs_without_relaxing_configuration() {
        let (_temp, home, capture) = fixture();
        let stable = derived(&home, &capture).unwrap();
        for (active, sub) in [
            ("activating", "auto-restart"),
            ("deactivating", "stop-sigterm"),
        ] {
            let mut changed = capture.clone();
            changed.unit.get_mut("ActiveState").unwrap().data = serde_json::json!(active);
            changed.unit.get_mut("SubState").unwrap().data = serde_json::json!(sub);
            changed.unit.get_mut("Job").unwrap().data =
                serde_json::json!([37, "/org/freedesktop/systemd1/job/37"]);
            changed.service.get_mut("ControlPID").unwrap().data = serde_json::json!(123);
            assert!(derived(&home, &changed).is_err());
            let stopping = derive_stop(
                &home,
                unsafe { libc::getuid() },
                &changed,
                Instant::now() + Duration::from_secs(2),
            )
            .unwrap();
            assert_eq!(
                stopping.snapshot.configuration_identity,
                stable.snapshot.configuration_identity
            );
            changed.unit.get_mut("NeedDaemonReload").unwrap().data = serde_json::json!(true);
            assert!(derive_stop(
                &home,
                unsafe { libc::getuid() },
                &changed,
                Instant::now() + Duration::from_secs(2)
            )
            .is_err());
        }
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
        assert_ne!(
            before.snapshot.configuration_identity,
            after.snapshot.configuration_identity
        );
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

#[cfg(test)]
mod readiness_tests {
    use super::*;
    fn invocation(args: &[&str], env: &[(&str, &str)]) -> ServiceInvocation {
        ServiceInvocation {
            uid: 1,
            argv: std::iter::once("ww")
                .chain(std::iter::once("run"))
                .chain(args.iter().copied())
                .map(str::to_owned)
                .collect(),
            environment: env
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
        }
    }
    #[test]
    fn readiness_uses_effective_argv_and_environment_and_refuses_off_external_dynamic() {
        assert_eq!(
            readiness_address(&invocation(&[], &[]))
                .unwrap()
                .to_string(),
            "127.0.0.1:2026"
        );
        assert_eq!(
            readiness_address(&invocation(&[], &[("WW_HTTP_ADMIN", "127.0.0.1:1234")]))
                .unwrap()
                .port(),
            1234
        );
        assert_eq!(
            readiness_address(&invocation(
                &["--with-http-admin=[::1]:4567"],
                &[("WW_HTTP_ADMIN", "off")]
            ))
            .unwrap()
            .port(),
            4567
        );
        for value in ["off", "0.0.0.0:1234", "127.0.0.1:0", "localhost:1234", ""] {
            assert!(
                readiness_address(&invocation(&["--with-http-admin", value], &[])).is_err(),
                "{value}"
            );
        }
    }
    #[test]
    fn readiness_requires_actual_readyz_http_success() {
        for (status, want) in [
            ("200 OK", true),
            ("503 Service Unavailable", false),
            ("404 Not Found", false),
        ] {
            let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            let address = listener.local_addr().unwrap();
            let worker = std::thread::spawn(move || {
                let (mut stream, _) = listener.accept().unwrap();
                stream
                    .set_read_timeout(Some(Duration::from_secs(2)))
                    .unwrap();
                let mut request = Vec::new();
                loop {
                    let mut bytes = [0; 1024];
                    let size = stream.read(&mut bytes).unwrap();
                    assert!(size > 0);
                    request.extend_from_slice(&bytes[..size]);
                    if request.windows(4).any(|part| part == b"\r\n\r\n") {
                        break;
                    }
                }
                assert!(request.starts_with(b"GET /readyz HTTP/1.1\r\n"));
                write!(stream, "HTTP/1.1 {status}\r\nContent-Length: 0\r\n\r\n").unwrap();
            });
            assert_eq!(
                probe_ready(address, Instant::now() + Duration::from_secs(2)).is_ok(),
                want
            );
            worker.join().unwrap();
        }
    }
    #[test]
    fn readiness_silent_socket_is_bounded() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let worker = std::thread::spawn(move || {
            let (_stream, _) = listener.accept().unwrap();
            std::thread::sleep(Duration::from_millis(100));
        });
        let start = Instant::now();
        assert!(probe_ready(address, start + Duration::from_millis(30)).is_err());
        assert!(start.elapsed() < Duration::from_millis(500));
        worker.join().unwrap();
    }
}

fn observation_deadline(deadline: Instant) -> Instant {
    deadline.min(Instant::now() + Duration::from_secs(2))
}

fn reset_request(
    home: &Path,
    mutation: &MutationLock,
    deadline: Instant,
) -> Result<super::service::Observation> {
    ensure!(
        mutation.home() == home,
        "configuration command lock belongs to another installation"
    );
    let (_, address, _) = observation_target(home, deadline)?;
    let args = vec![
        address.as_str(),
        "--auto-start=no",
        "--json=short",
        "call",
        DESTINATION,
        MANAGER,
        "org.freedesktop.systemd1.Manager",
        "ResetFailedUnit",
        "s",
        UNIT,
    ];
    let result = super::service::action_until("/usr/bin/busctl", &args, mutation, deadline)?;
    check_deadline(deadline)?;
    Ok(result)
}
pub fn reset_failed(
    home: &Path,
    before: &ServiceSnapshot,
    mutation: &MutationLock,
    deadline: Instant,
) -> Result<()> {
    let current = observe(home, deadline)?;
    super::same_configuration(before, &current.snapshot)?;
    ensure!(
        current.snapshot.running == RunningState::Stopped,
        "reset requires stopped restored service"
    );
    let result = reset_request(home, mutation, deadline)?;
    if result.status.success() {
        return Ok(());
    }
    // Disabled stopped units may be garbage-collected between LoadUnit and
    // ResetFailedUnit. Unloading clears their start counters. Accept only this
    // exact unit-scoped response, then recheck configuration and clean stop.
    ensure!(
        result.stdout.is_empty()
            && result.stderr.trim() == "Call failed: Unit ww.service not loaded.",
        "systemd ResetFailedUnit failed: {}",
        result.stderr
    );
    let after = observe(home, deadline)?;
    super::same_configuration(before, &after.snapshot)?;
    ensure!(
        after.snapshot.running == RunningState::Stopped && !after.snapshot.failed,
        "unloaded unit did not return with cleared failure state"
    );
    check_deadline(deadline)
}
