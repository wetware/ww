//! Service evidence for native Host replacement.
//!
//! Native mutation is restricted to the Linux GNU ARM64 qualification candidate.
//! Successful service-manager calls alone never establish live-upgrade readiness.

use std::collections::BTreeMap;
use std::io::Read;
use std::os::fd::AsRawFd;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus, Stdio};
use std::time::{Duration, Instant};

use anyhow::{bail, ensure, Context, Result};
use serde::{Deserialize, Serialize};

use super::ArtifactIdentity;

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum EnabledState {
    Enabled,
    Disabled,
    Unknown,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum RunningState {
    Stopped,
    Running { pid: u32 },
    Unknown,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ServiceSnapshot {
    pub registered: bool,
    pub enabled: EnabledState,
    pub running: RunningState,
    pub failed: bool,
    pub executable: Option<PathBuf>,
    pub configuration_identity: String,
}

/// Canonical identities of the actual captured inputs consumed by startup.
/// These are observations, not authorization to alter deployment authority.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StartupIdentity {
    /// Effective startup configuration, including the configured authoritative
    /// source identity, but excluding that source's moving head value.
    pub configuration: String,
    /// Captured installed/explicit FHS baseline. None means no baseline.
    /// This never includes the followed authoritative deployment head.
    pub fhs_baseline: Option<String>,
    /// Ordered captured namespace and local-source identities.
    pub frozen_sources: String,
    pub publisher_target: Option<String>,
}

/// Evidence that child startup enforces the checked inputs before effects.
/// The native adapter cannot currently construct this admission.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LegacyRestartAdmission {
    pub(crate) startup: StartupIdentity,
    pub(crate) configuration_identity: String,
    pub(crate) prior_pid: u32,
}

impl LegacyRestartAdmission {
    pub fn validate(&self, before: &ServiceSnapshot) -> Result<()> {
        self.startup.validate()?;
        ensure!(
            before.registered,
            "restart requires a registered managed service"
        );
        ensure!(
            before.enabled != EnabledState::Unknown,
            "automatic-start state is unknown"
        );
        ensure!(
            !before.failed,
            "running service has failed state; stop it explicitly before upgrade"
        );
        ensure!(
            before.running
                == RunningState::Running {
                    pid: self.prior_pid
                }
                && self.prior_pid != 0,
            "restart admission does not match the previously running service"
        );
        ensure!(
            !self.configuration_identity.is_empty()
                && self.configuration_identity.len() <= MAX_IDENTITY_BYTES
                && before.configuration_identity == self.configuration_identity,
            "effective service configuration changed before restart"
        );
        Ok(())
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
#[cfg(test)]
pub struct LegacyRunningEvidence {
    pub pid: u32,
    pub artifact: ArtifactIdentity,
    pub startup: StartupIdentity,
    pub serving_ready: bool,
}

#[cfg(test)]
impl LegacyRunningEvidence {
    pub fn verify(
        &self,
        admission: &LegacyRestartAdmission,
        artifact: &ArtifactIdentity,
    ) -> Result<()> {
        admission.startup.validate()?;
        self.startup.validate()?;
        ensure!(
            self.pid != 0 && self.pid != admission.prior_pid,
            "service did not establish a new process identity"
        );
        ensure!(
            &self.artifact == artifact,
            "running native artifact differs from the installed candidate"
        );
        ensure!(
            self.startup == admission.startup,
            "restarted Host accepted different startup inputs"
        );
        ensure!(
            self.serving_ready,
            "restarted Host has not reached serving readiness"
        );
        Ok(())
    }
}

/// Verified preparation from an attested protocol child. Wire responses alone
/// cannot construct this evidence outside the crate's trusted native adapter.
#[derive(Clone, Debug)]
pub struct CompatibilityEvidence {
    pub(crate) artifact: ArtifactIdentity,
    pub(crate) capture_digest: [u8; 32],
    pub(crate) protocol_version: u32,
    pub(crate) prepared: super::inputs::PreparedHead,
}

impl CompatibilityEvidence {
    pub fn verify(
        &self,
        executable: &super::attestation::VerifiedExecutable,
        inputs: &super::inputs::PreflightInputs,
    ) -> Result<()> {
        ensure!(
            self.protocol_version == super::inputs::NATIVE_PROTOCOL_VERSION,
            "preflight protocol version changed"
        );
        ensure!(
            &self.artifact == executable.artifact()
                && self.capture_digest == inputs.capture_digest()?,
            "preflight evidence identifies another executable or captured inputs"
        );
        ensure!(
            !self.prepared.effective_root.is_empty()
                && !self.prepared.kernel_source.is_empty()
                && !self.prepared.kernel_runtime.is_empty(),
            "preflight did not prepare an effective root and kernel"
        );
        Ok(())
    }
}

/// A process birth plus its fresh admitted execution nonce. The nonce is
/// independently challenged through the process-owned CLOEXEC evidence socket.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProcessInstance {
    pub pid: u32,
    pub start: super::attestation::ProcessStartToken,
    pub launch_nonce: [u8; 32],
    pub manager_invocation_id: String,
}

impl ProcessInstance {
    pub fn validate(&self) -> Result<()> {
        ensure!(
            self.pid != 0 && self.launch_nonce != [0; 32],
            "missing process birth or execution nonce"
        );
        ensure!(
            self.manager_invocation_id.len() == 32
                && self
                    .manager_invocation_id
                    .bytes()
                    .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
                && self.manager_invocation_id.bytes().any(|byte| byte != b'0'),
            "invalid systemd manager invocation identity"
        );
        match &self.start {
            super::attestation::ProcessStartToken::Linux {
                boot_id,
                start_ticks,
            } => {
                ensure!(
                    boot_id.len() == 36 && *start_ticks != 0,
                    "invalid Linux process birth token"
                );
            }
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RestartAdmission {
    pub(crate) contract: super::startup::StartupContractV1,
    pub(crate) original_process: ProcessInstance,
    pub(crate) service: super::startup::ServiceIdentity,
    pub(crate) transaction_id: [u8; 32],
}

impl RestartAdmission {
    pub fn contract(&self) -> &super::startup::StartupContractV1 {
        &self.contract
    }
    pub fn original_process(&self) -> &ProcessInstance {
        &self.original_process
    }
    pub fn service(&self) -> &super::startup::ServiceIdentity {
        &self.service
    }
    pub fn validate(&self, before: &ServiceSnapshot) -> Result<()> {
        self.contract.validate()?;
        self.original_process.validate()?;
        ensure!(
            self.transaction_id != [0; 32],
            "restart admission has no transaction identity"
        );
        ensure!(
            before.running
                == RunningState::Running {
                    pid: self.original_process.pid
                }
                && before.registered
                && before.enabled == self.service.enabled
                && !before.failed,
            "restart admission differs from original service disposition"
        );
        ensure!(
            self.contract.invocation
                == super::startup::InvocationBinding::Managed {
                    service: self.service.clone()
                },
            "restart admission differs from the committed managed invocation"
        );
        Ok(())
    }
}

/// Durable stop intent is separate from observed completion. A command timeout
/// leaves stop_intent set and quiesced absent; recovery must reobserve the manager.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ServiceDisposition {
    pub original: ServiceSnapshot,
    pub identity: Option<super::startup::ServiceIdentity>,
    pub original_process: Option<ProcessInstance>,
    pub original_contract: Option<super::startup::StartupContractV1>,
    pub quiescence_contract: Option<super::startup::StartupContractV1>,
    pub quiescence_permit: Option<super::startup::StartupPermitPayload>,
    pub stop_intent: bool,
    pub quiesced: Option<ServiceSnapshot>,
}

impl ServiceDisposition {
    pub fn validate(&self) -> Result<()> {
        if let Some(process) = &self.original_process {
            process.validate()?;
        }
        ensure!(
            self.quiesced.is_none() || self.stop_intent,
            "quiescence has no durable stop intent"
        );
        if let Some(quiesced) = &self.quiesced {
            ensure!(
                quiesced.running == RunningState::Stopped
                    && quiesced.enabled == self.original.enabled,
                "quiescence changed automatic-start disposition"
            );
        }
        if let RunningState::Running { pid } = self.original.running {
            ensure!(
                self.original.registered
                    && self.identity.is_some()
                    && self.original_process.as_ref().is_some_and(|p| p.pid == pid),
                "running disposition lacks exact managed process identity"
            );
        }
        Ok(())
    }
}

#[derive(Clone, Debug)]
pub struct RunningEvidence {
    pub(crate) process: ProcessInstance,
    pub(crate) artifact: ArtifactIdentity,
    pub(crate) startup: StartupIdentity,
    pub(crate) service: super::startup::ServiceIdentity,
    pub(crate) contract: super::startup::ContractIdentity,
    pub(crate) epoch_seq: u64,
    pub(crate) transaction_id: Option<[u8; 32]>,
    pub(crate) permit_digest: Option<[u8; 32]>,
    pub(crate) effective_root: String,
    pub(crate) kernel_source: String,
    pub(crate) kernel_runtime: String,
}

impl RunningEvidence {
    pub fn process(&self) -> &ProcessInstance {
        &self.process
    }
    pub fn verify(
        &self,
        admission: &RestartAdmission,
        permit: &super::StartupPermit,
        executable: &super::attestation::VerifiedExecutable,
    ) -> Result<()> {
        self.process.validate()?;
        ensure!(
            self.process != admission.original_process
                && self.process.launch_nonce != admission.original_process.launch_nonce
                && self.process.manager_invocation_id
                    != admission.original_process.manager_invocation_id,
            "restart did not establish a fresh admitted execution"
        );
        ensure!(
            &self.artifact == executable.artifact()
                && self.artifact == permit.contract().artifact
                && self.contract == permit.contract().identity
                && self.startup == permit.contract().startup
                && self.service == admission.service,
            "running evidence differs from permitted executable, contract or managed invocation"
        );
        ensure!(
            self.transaction_id == Some(permit.payload().transaction_id)
                && self.permit_digest == Some(permit.payload().digest()?),
            "running process did not consume this transaction permit"
        );
        ensure!(
            !self.effective_root.is_empty()
                && !self.kernel_source.is_empty()
                && !self.kernel_runtime.is_empty(),
            "running evidence has no coherent serving deployment"
        );
        let _ = self.epoch_seq; // An initial coherent epoch may be zero.
        Ok(())
    }
}

/// Operations must preserve definitions, keys, user data, and authority heads.
/// `compatible` checks the current effective deployment, including rollback.
/// `restart` must enforce admission in the child before publication or guests.
pub trait ServiceControl {
    fn admit_restart(
        &mut self,
        _before: &ServiceSnapshot,
        _contract: &super::startup::StartupContractV1,
        _expected: &super::attestation::VerifiedExecutable,
        _deadline: Instant,
    ) -> Result<RestartAdmission> {
        bail!("native live admission is not qualified")
    }
    fn revalidate_restart(
        &mut self,
        _admission: &RestartAdmission,
        _deadline: Instant,
    ) -> Result<()> {
        bail!("native live admission revalidation is not qualified")
    }
    fn quiesce(
        &mut self,
        _intent: &ServiceDisposition,
        _deadline: Instant,
    ) -> Result<ServiceDisposition> {
        bail!("native service quiescence is not qualified")
    }
    fn restart(
        &mut self,
        _admission: &RestartAdmission,
        _permit: &super::StartupPermit,
        _expected: &super::attestation::VerifiedExecutable,
        _deadline: Instant,
    ) -> Result<RunningEvidence> {
        bail!("native guarded restart is not qualified")
    }
    fn revalidate_running(
        &mut self,
        _evidence: &RunningEvidence,
        _permit: &super::StartupPermit,
        _expected: &super::attestation::VerifiedExecutable,
        _deadline: Instant,
    ) -> Result<()> {
        bail!("native live evidence revalidation is not qualified")
    }
    fn stop(
        &mut self,
        _expected: &super::startup::ServiceIdentity,
        _deadline: Instant,
    ) -> Result<()> {
        bail!("native service containment is not qualified")
    }
    fn snapshot(&mut self, deadline: Instant) -> Result<ServiceSnapshot>;
    #[cfg(test)]
    fn legacy_admit_restart(&mut self, before: &ServiceSnapshot) -> Result<LegacyRestartAdmission>;
    /// Recheck prepared next-start inputs against the durable admission during
    /// recovery. The original process may already have stopped. Child-side
    /// enforcement remains required before publication or guest execution.
    #[cfg(test)]
    fn legacy_revalidate_restart(&mut self, admission: &LegacyRestartAdmission) -> Result<()>;
    #[cfg(test)]
    fn legacy_restart(
        &mut self,
        admission: &LegacyRestartAdmission,
        expected: &ArtifactIdentity,
        deadline: Instant,
    ) -> Result<LegacyRunningEvidence>;
    #[cfg(test)]
    fn legacy_stop(&mut self, deadline: Instant) -> Result<()>;
    /// Execute the descriptor-bound preparation protocol with these exact captures.
    fn compatible(
        &mut self,
        executable: &super::attestation::VerifiedExecutable,
        inputs: &super::inputs::PreflightInputs,
        deadline: Instant,
    ) -> Result<CompatibilityEvidence>;
    /// Version-one recovery only. Never grants guarded startup authority.
    #[cfg(test)]
    fn legacy_compatible(&mut self, artifact: &ArtifactIdentity, executable: &Path) -> Result<()>;
}

const MAX_OBSERVATION_BYTES: usize = 64 * 1024;
const MAX_IDENTITY_BYTES: usize = 4096;
#[cfg(test)]
const COMMAND_TIMEOUT: Duration = Duration::from_secs(10);

impl StartupIdentity {
    pub(crate) fn validate(&self) -> Result<()> {
        for value in [
            Some(self.configuration.as_str()),
            self.fhs_baseline.as_deref(),
            Some(self.frozen_sources.as_str()),
            self.publisher_target.as_deref(),
        ]
        .into_iter()
        .flatten()
        {
            ensure!(
                !value.trim().is_empty()
                    && value.len() <= MAX_IDENTITY_BYTES
                    && !value.contains('\0'),
                "missing or oversized captured startup identity"
            );
        }
        Ok(())
    }
}

fn fingerprint(parts: &[&[u8]]) -> String {
    let mut hash = blake3::Hasher::new();
    for part in parts {
        hash.update(&(part.len() as u64).to_le_bytes());
        hash.update(part);
    }
    hash.finalize().to_hex().to_string()
}

fn absent(configuration: &[u8]) -> ServiceSnapshot {
    ServiceSnapshot {
        registered: false,
        enabled: EnabledState::Disabled,
        running: RunningState::Stopped,
        failed: false,
        executable: None,
        configuration_identity: fingerprint(&[configuration]),
    }
}

fn systemd_snapshot(show: &str, configuration: &[u8]) -> Result<ServiceSnapshot> {
    ensure!(
        show.len() <= MAX_OBSERVATION_BYTES && configuration.len() <= MAX_OBSERVATION_BYTES,
        "service observation exceeds bound"
    );
    let mut properties = BTreeMap::new();
    for line in show.lines() {
        let (name, value) = line.split_once('=').context("malformed systemd property")?;
        ensure!(
            properties.insert(name, value).is_none(),
            "duplicate systemd property {name}"
        );
    }
    let property = |key| {
        properties
            .get(key)
            .copied()
            .with_context(|| format!("missing systemd property {key}"))
    };
    let load = property("LoadState")?;
    let active = property("ActiveState")?;
    let sub = property("SubState")?;
    let pid: u32 = property("MainPID")?
        .parse()
        .context("invalid systemd MainPID")?;
    ensure!(
        property("NeedDaemonReload")? == "no",
        "service has pending configuration; activate or revert it explicitly before upgrade"
    );
    if load == "not-found" {
        ensure!(
            active == "inactive"
                && sub == "dead"
                && pid == 0
                && property("FragmentPath")?.is_empty()
                // systemctl omits an empty ExecStart array for an absent unit,
                // including with --all. Nonempty command evidence still fails.
                && properties.get("ExecStart").is_none_or(|value| value.is_empty())
                && property("DropInPaths")?.is_empty()
                && configuration.is_empty(),
            "systemd absence conflicts with loaded service evidence"
        );
        return Ok(absent(show.as_bytes()));
    }
    ensure!(
        load == "loaded",
        "systemd service load state is unknown or unsupported: {load}"
    );
    let enabled = match property("UnitFileState")? {
        "enabled" | "enabled-runtime" => EnabledState::Enabled,
        "disabled" | "masked" | "masked-runtime" => EnabledState::Disabled,
        value => bail!("systemd automatic-start state is unknown or unsupported: {value}"),
    };
    let running = match (active, sub, pid) {
        ("active", "running", 1..) => RunningState::Running { pid },
        ("inactive", "dead", 0) | ("failed", "failed", 0) => RunningState::Stopped,
        _ => bail!("systemd service state is transitional or ambiguous: {active}/{sub}, PID {pid}"),
    };
    let start = property("ExecStart")?;
    let after_path = start
        .strip_prefix("{ path=")
        .context("unsupported systemd ExecStart")?;
    let (path, _) = after_path
        .split_once(" ; argv[]=")
        .context("unsupported systemd ExecStart arguments")?;
    ensure!(
        Path::new(path).is_absolute()
            && !path.contains(['\\', '\0', '\n'])
            && start.matches("{ path=").count() == 1,
        "ambiguous systemd executable path"
    );
    // ExecStart also contains process status and timestamps. Those are evidence
    // of activity, not part of the effective command configuration.
    let start_configuration = start
        .split(" ; start_time=")
        .next()
        .context("missing ExecStart configuration")?;
    let mut parts = vec![configuration, start_configuration.as_bytes()];
    for key in [
        "Environment",
        "EnvironmentFiles",
        "FragmentPath",
        "DropInPaths",
    ] {
        // systemctl omits the EnvironmentFiles array when no files are
        // configured. Keep its empty identity slot; configured files remain
        // part of the fingerprint.
        let value = if key == "EnvironmentFiles" {
            properties.get(key).copied().unwrap_or_default()
        } else {
            property(key)?
        };
        parts.push(value.as_bytes());
    }
    Ok(ServiceSnapshot {
        registered: true,
        enabled,
        running,
        failed: active == "failed" || property("Result")? != "success",
        executable: Some(PathBuf::from(path)),
        configuration_identity: fingerprint(&parts),
    })
}

/// Read top-level fields without confusing nested environment/argument values
/// with the service's state. Unknown output shapes fail closed.
fn launchd_fields(print: &str) -> Result<BTreeMap<String, String>> {
    ensure!(
        print.len() <= MAX_OBSERVATION_BYTES,
        "launchd observation exceeds bound"
    );
    let mut lines = print.lines();
    let header = lines.next().context("empty launchd observation")?;
    ensure!(
        header.starts_with("gui/") && header.ends_with("/io.wetware.ww = {"),
        "unexpected launchd service target"
    );
    let mut properties = BTreeMap::new();
    let mut closed = false;
    while let Some(line) = lines.next() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        if line == "}" {
            closed = true;
            ensure!(
                lines.all(|line| line.trim().is_empty()),
                "trailing launchd observation"
            );
            break;
        }
        let (name, value) = line.split_once(" = ").context("malformed launchd field")?;
        let mut value = value.to_owned();
        if value == "{" {
            let mut depth = 1usize;
            for nested in lines.by_ref() {
                value.push('\n');
                value.push_str(nested);
                if nested.trim().ends_with(" = {") {
                    depth += 1;
                }
                if nested.trim() == "}" {
                    depth -= 1;
                }
                if depth == 0 {
                    break;
                }
            }
            ensure!(depth == 0, "unterminated launchd field");
        }
        ensure!(
            properties.insert(name.to_owned(), value).is_none(),
            "duplicate launchd field {name}"
        );
    }
    ensure!(closed, "unterminated launchd observation");
    Ok(properties)
}

fn launchd_enabled(output: &str) -> Result<EnabledState> {
    ensure!(
        output.len() <= MAX_OBSERVATION_BYTES,
        "launchd disable observation exceeds bound"
    );
    let mut lines = output
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty());
    ensure!(
        lines.next() == Some("disabled services = {"),
        "invalid launchd disable observation"
    );
    let mut enabled = None;
    let mut closed = false;
    for line in lines.by_ref() {
        if line == "}" {
            closed = true;
            break;
        }
        let (label, value) = line
            .split_once(" => ")
            .context("malformed launchd disable entry")?;
        ensure!(
            label.starts_with('"')
                && label.ends_with('"')
                && matches!(value, "true" | "false" | "enabled" | "disabled"),
            "invalid launchd disable entry"
        );
        if label == "\"io.wetware.ww\"" {
            ensure!(enabled.is_none(), "duplicate launchd disable entry");
            enabled = Some(if matches!(value, "true" | "disabled") {
                EnabledState::Disabled
            } else {
                EnabledState::Enabled
            });
        }
    }
    ensure!(
        closed && lines.next().is_none(),
        "unterminated launchd disable observation"
    );
    // No override is not proof of the job's default Disabled property.
    Ok(enabled.unwrap_or(EnabledState::Unknown))
}

fn launchd_snapshot(print: &str, enabled: &str, definition: &[u8]) -> Result<ServiceSnapshot> {
    ensure!(
        definition.len() <= MAX_OBSERVATION_BYTES,
        "launchd definition exceeds bound"
    );
    let fields = launchd_fields(print)?;
    let state = fields.get("state").context("missing launchd state")?;
    let pid = fields
        .get("pid")
        .map(|pid| pid.parse::<u32>())
        .transpose()
        .context("invalid launchd PID")?
        .unwrap_or(0);
    let running = match (state.as_str(), pid) {
        ("running", 1..) => RunningState::Running { pid },
        ("not running", 0) => RunningState::Stopped,
        _ => bail!("launchd service state is transitional or ambiguous: {state}, PID {pid}"),
    };
    let program = fields.get("program").context("missing launchd program")?;
    ensure!(
        Path::new(program).is_absolute() && !program.contains('\0'),
        "invalid launchd executable path"
    );
    ensure!(
        fields.contains_key("arguments"),
        "missing effective launchd arguments"
    );
    let failed = fields
        .get("last exit code")
        .map(|value| value.parse::<i32>())
        .transpose()
        .context("invalid launchd last exit code")?
        .is_some_and(|value| value != 0)
        || fields.contains_key("last terminating signal");
    let mut parts = vec![definition];
    for (name, value) in &fields {
        if !matches!(
            name.as_str(),
            "active count"
                | "state"
                | "pid"
                | "runs"
                | "last exit code"
                | "last terminating signal"
        ) {
            parts.push(name.as_bytes());
            parts.push(value.as_bytes());
        }
    }
    Ok(ServiceSnapshot {
        registered: true,
        enabled: launchd_enabled(enabled)?,
        running,
        failed,
        executable: Some(PathBuf::from(program)),
        configuration_identity: fingerprint(&parts),
    })
}

// Owner-authorized Linux GNU ARM64 candidate for disposable qualification.
// Every admission, process, compatibility and recovery check remains required.
// Other targets remain refused; there is no runtime override.
const NATIVE_LINUX_QUALIFIED: bool = cfg!(all(
    target_os = "linux",
    target_arch = "aarch64",
    target_env = "gnu"
));

pub struct NativeService {
    home: PathBuf,
}

impl NativeService {
    pub fn new(home: impl Into<PathBuf>) -> Result<Self> {
        let home = home.into();
        ensure!(home.is_absolute(), "native service HOME must be absolute");
        Ok(Self { home })
    }
}

impl ServiceControl for NativeService {
    fn admit_restart(
        &mut self,
        before: &ServiceSnapshot,
        contract: &super::startup::StartupContractV1,
        expected: &super::attestation::VerifiedExecutable,
        deadline: Instant,
    ) -> Result<RestartAdmission> {
        ensure!(
            NATIVE_LINUX_QUALIFIED && cfg!(target_os = "linux"),
            "native live admission is not qualified"
        );
        super::native_linux::admit_restart(&self.home, before, contract, expected, deadline)
    }
    fn revalidate_restart(
        &mut self,
        admission: &RestartAdmission,
        deadline: Instant,
    ) -> Result<()> {
        ensure!(
            NATIVE_LINUX_QUALIFIED && cfg!(target_os = "linux"),
            "native live admission revalidation is not qualified"
        );
        super::native_linux::revalidate_restart(&self.home, admission, deadline)
    }
    fn quiesce(
        &mut self,
        intent: &ServiceDisposition,
        deadline: Instant,
    ) -> Result<ServiceDisposition> {
        ensure!(
            NATIVE_LINUX_QUALIFIED && cfg!(target_os = "linux"),
            "native quiescence is not qualified"
        );
        super::native_linux::quiesce(&self.home, intent, deadline)
    }
    fn restart(
        &mut self,
        admission: &RestartAdmission,
        permit: &super::StartupPermit,
        expected: &super::attestation::VerifiedExecutable,
        deadline: Instant,
    ) -> Result<RunningEvidence> {
        ensure!(
            NATIVE_LINUX_QUALIFIED && cfg!(target_os = "linux"),
            "native guarded restart is not qualified"
        );
        super::native_linux::restart(&self.home, admission, permit, expected, deadline)
    }
    fn revalidate_running(
        &mut self,
        evidence: &RunningEvidence,
        permit: &super::StartupPermit,
        expected: &super::attestation::VerifiedExecutable,
        deadline: Instant,
    ) -> Result<()> {
        ensure!(
            NATIVE_LINUX_QUALIFIED && cfg!(target_os = "linux"),
            "native live evidence revalidation is not qualified"
        );
        super::native_linux::revalidate_running(&self.home, evidence, permit, expected, deadline)
    }
    fn stop(
        &mut self,
        expected: &super::startup::ServiceIdentity,
        deadline: Instant,
    ) -> Result<()> {
        ensure!(
            NATIVE_LINUX_QUALIFIED && cfg!(target_os = "linux"),
            "native service containment is not qualified"
        );
        super::native_linux::stop(&self.home, expected, deadline)
    }
    fn snapshot(&mut self, deadline: Instant) -> Result<ServiceSnapshot> {
        if NATIVE_LINUX_QUALIFIED && cfg!(target_os = "linux") {
            return super::native_linux::snapshot(&self.home, deadline);
        }
        if cfg!(target_os = "linux") {
            let show = observe_until("/usr/bin/systemctl", &[
                "--user", "show", "ww.service", "--no-pager",
                "--property=LoadState,UnitFileState,ActiveState,SubState,MainPID,Result,ExecStart,Environment,EnvironmentFiles,FragmentPath,DropInPaths,NeedDaemonReload",
            ], deadline)?;
            ensure!(
                show.status.success(),
                "systemd query failed: {}",
                show.stderr.trim()
            );
            let initial = systemd_snapshot(&show.stdout, &[])?;
            if !initial.registered {
                return Ok(initial);
            }
            let configuration = observe_until(
                "/usr/bin/systemctl",
                &["--user", "cat", "ww.service", "--no-pager"],
                deadline,
            )?;
            ensure!(
                configuration.status.success(),
                "cannot read effective systemd configuration: {}",
                configuration.stderr.trim()
            );
            systemd_snapshot(&show.stdout, configuration.stdout.as_bytes())
        } else if cfg!(target_os = "macos") {
            // SAFETY: geteuid has no pointer arguments or preconditions.
            let domain = format!("gui/{}", unsafe { libc::geteuid() });
            let target = format!("{domain}/io.wetware.ww");
            let print = observe_until("/bin/launchctl", &["print", &target], deadline)?;
            let disabled = observe_until("/bin/launchctl", &["print-disabled", &domain], deadline)?;
            ensure!(
                disabled.status.success(),
                "launchd domain state is unavailable: {}",
                disabled.stderr.trim()
            );
            let enabled = launchd_enabled(&disabled.stdout)?;
            let definition_path = self.home.join("Library/LaunchAgents/io.wetware.ww.plist");
            let definition = read_definition(&definition_path)?;
            if !print.status.success() {
                // Missing domains and failed manager queries must never be
                // interpreted as proof that a service is absent.
                let uid = domain.strip_prefix("gui/").expect("constructed domain");
                let absent_message = format!("Bad request.\nCould not find service \"io.wetware.ww\" in domain for user gui: {uid}\n");
                ensure!(
                    print.status.code() == Some(113)
                        && print.stdout.is_empty()
                        && print.stderr == absent_message,
                    "launchd service state is unknown: {}",
                    print.stderr.trim()
                );
                let Some(definition) = definition else {
                    return Ok(absent(disabled.stdout.as_bytes()));
                };
                let (executable, default_enabled) = plist_settings(&definition_path, deadline)?;
                return Ok(ServiceSnapshot {
                    registered: false,
                    enabled: if enabled == EnabledState::Unknown {
                        default_enabled
                    } else {
                        enabled
                    },
                    running: RunningState::Stopped,
                    failed: false,
                    executable: Some(executable),
                    configuration_identity: fingerprint(&[&definition]),
                });
            }
            let definition = definition.context(
                "loaded launchd service has no recognized definition; service state is ambiguous",
            )?;
            let fields = launchd_fields(&print.stdout)?;
            ensure!(
                fields.get("path").map(Path::new) == Some(definition_path.as_path()),
                "loaded launchd service uses a different definition path"
            );
            let (executable, default_enabled) = plist_settings(&definition_path, deadline)?;
            let mut snapshot = launchd_snapshot(&print.stdout, &disabled.stdout, &definition)?;
            ensure!(
                snapshot.executable.as_ref() == Some(&executable),
                "launchd program differs from the pending service definition"
            );
            if snapshot.enabled == EnabledState::Unknown {
                snapshot.enabled = default_enabled;
            }
            Ok(snapshot)
        } else {
            bail!("native service observation supports only per-user systemd and launchd")
        }
    }

    #[cfg(test)]
    fn legacy_admit_restart(
        &mut self,
        _before: &ServiceSnapshot,
    ) -> Result<LegacyRestartAdmission> {
        bail!("live Host upgrade requires reliable captured startup inputs and child-side admission; explicitly stop the managed service before upgrading")
    }

    #[cfg(test)]
    fn legacy_revalidate_restart(&mut self, _admission: &LegacyRestartAdmission) -> Result<()> {
        bail!("native restart recovery requires reliable captured startup inputs and child-side admission; explicitly stop the managed service and resolve the pending transaction")
    }

    #[cfg(test)]
    fn legacy_restart(
        &mut self,
        _admission: &LegacyRestartAdmission,
        _expected: &ArtifactIdentity,
        _deadline: Instant,
    ) -> Result<LegacyRunningEvidence> {
        bail!(
            "native restart is gated on startup admission and exact running-artifact qualification"
        )
    }

    #[cfg(test)]
    fn legacy_stop(&mut self, _deadline: Instant) -> Result<()> {
        bail!("native service mutation is not qualified; explicitly stop the managed service and verify its state")
    }

    fn compatible(
        &mut self,
        executable: &super::attestation::VerifiedExecutable,
        inputs: &super::inputs::PreflightInputs,
        deadline: Instant,
    ) -> Result<CompatibilityEvidence> {
        ensure!(
            NATIVE_LINUX_QUALIFIED && cfg!(target_os = "linux"),
            "descriptor-bound native compatibility is not qualified"
        );
        super::native_linux::compatible(executable, inputs, deadline)
    }

    #[cfg(test)]
    fn legacy_compatible(
        &mut self,
        _artifact: &ArtifactIdentity,
        _executable: &Path,
    ) -> Result<()> {
        bail!("native Host upgrade requires the selected candidate's compatibility check against the current effective deployment; production integration is unavailable")
    }
}

#[derive(Debug)]
pub(super) struct Observation {
    pub(super) status: ExitStatus,
    pub(super) stdout: String,
    pub(super) stderr: String,
}

#[cfg(test)]
fn observe(program: &str, args: &[&str], timeout: Duration) -> Result<Observation> {
    ensure!(
        !timeout.is_zero(),
        "native observation deadline must be positive"
    );
    let deadline = Instant::now()
        .checked_add(timeout)
        .context("native observation deadline overflow")?;
    observe_until(program, args, deadline)
}

pub(super) fn observe_until(
    program: &str,
    args: &[&str],
    deadline: Instant,
) -> Result<Observation> {
    ensure!(
        Instant::now() < deadline,
        "native service query exceeded deadline before spawn"
    );
    let mut child = Command::new(program)
        .args(args)
        .env("LC_ALL", "C")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .with_context(|| format!("start native service query {program}"))?;
    let result = (|| {
        let mut stdout = child
            .stdout
            .take()
            .context("native query stdout unavailable")?;
        let mut stderr = child
            .stderr
            .take()
            .context("native query stderr unavailable")?;
        for descriptor in [stdout.as_raw_fd(), stderr.as_raw_fd()] {
            // SAFETY: descriptors are owned by the live ChildStdout/ChildStderr
            // values. fcntl reads/changes descriptor flags without pointers.
            let flags = unsafe { libc::fcntl(descriptor, libc::F_GETFL) };
            ensure!(flags >= 0, "read native query pipe flags");
            ensure!(
                unsafe { libc::fcntl(descriptor, libc::F_SETFL, flags | libc::O_NONBLOCK) } >= 0,
                "set native query pipe nonblocking"
            );
        }
        let mut out = Vec::new();
        let mut err = Vec::new();
        loop {
            ensure!(
                Instant::now() < deadline,
                "native service query exceeded deadline"
            );
            read_available(&mut stdout, &mut out, deadline)?;
            read_available(&mut stderr, &mut err, deadline)?;
            if let Some(status) = child.try_wait().context("inspect native query process")? {
                // The final kernel pipe buffers may have filled after the
                // previous read and before process exit.
                read_available(&mut stdout, &mut out, deadline)?;
                read_available(&mut stderr, &mut err, deadline)?;
                return Ok(Observation {
                    status,
                    stdout: String::from_utf8(out).context("native query stdout is not UTF-8")?,
                    stderr: String::from_utf8(err).context("native query stderr is not UTF-8")?,
                });
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

fn read_available(reader: &mut impl Read, bytes: &mut Vec<u8>, deadline: Instant) -> Result<()> {
    let mut buffer = [0u8; 4096];
    loop {
        ensure!(
            Instant::now() < deadline,
            "native service query exceeded deadline"
        );
        match reader.read(&mut buffer) {
            Ok(0) => return Ok(()),
            Ok(size) => {
                ensure!(
                    bytes.len() + size <= MAX_OBSERVATION_BYTES,
                    "native service query exceeded output bound"
                );
                bytes.extend_from_slice(&buffer[..size]);
            }
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => return Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => (),
            Err(error) => return Err(error).context("read native service query"),
        }
    }
}

fn read_definition(path: &Path) -> Result<Option<Vec<u8>>> {
    use std::os::unix::fs::OpenOptionsExt;
    let file = match std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)
    {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(error).context("read native service definition without following symlinks")
        }
    };
    ensure!(
        file.metadata()?.is_file(),
        "native service definition must be a regular file"
    );
    let mut bytes = Vec::new();
    file.take(MAX_OBSERVATION_BYTES as u64 + 1)
        .read_to_end(&mut bytes)?;
    ensure!(
        bytes.len() <= MAX_OBSERVATION_BYTES,
        "native service definition exceeds bound"
    );
    Ok(Some(bytes))
}

fn plist_settings(path: &Path, deadline: Instant) -> Result<(PathBuf, EnabledState)> {
    let path = path
        .to_str()
        .context("launchd definition path is not UTF-8")?;
    let output = observe_until(
        "/usr/bin/plutil",
        &["-convert", "json", "-o", "-", "--", path],
        deadline,
    )?;
    ensure!(
        output.status.success(),
        "cannot read launchd plist settings: {}",
        output.stderr.trim()
    );
    let value: serde_json::Value =
        serde_json::from_str(&output.stdout).context("invalid launchd plist JSON")?;
    let program = value
        .get("Program")
        .and_then(serde_json::Value::as_str)
        .or_else(|| {
            value
                .get("ProgramArguments")
                .and_then(serde_json::Value::as_array)
                .and_then(|arguments| arguments.first())
                .and_then(serde_json::Value::as_str)
        })
        .context("launchd definition has no executable")?;
    ensure!(
        Path::new(program).is_absolute(),
        "launchd executable must be absolute"
    );
    let disabled = value
        .get("Disabled")
        .map(|value| value.as_bool().context("invalid launchd Disabled setting"))
        .transpose()?
        .unwrap_or(false);
    Ok((
        PathBuf::from(program),
        if disabled {
            EnabledState::Disabled
        } else {
            EnabledState::Enabled
        },
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    const SYSTEMD: &str = "LoadState=loaded\nUnitFileState=enabled\nActiveState=active\nSubState=running\nMainPID=321\nResult=success\nExecStart={ path=/tmp/ww ; argv[]=/tmp/ww run --identity /private/key ; ignore_errors=no ; start_time=[n/a] ; stop_time=[n/a] ; pid=321 ; code=(null) ; status=0/0 }\nEnvironment=IPFS_API=http://localhost:5001\nEnvironmentFiles=\nFragmentPath=/tmp/ww.service\nDropInPaths=/tmp/custom.conf\nNeedDaemonReload=no\n";
    const LAUNCHD: &str = "gui/501/io.wetware.ww = {\n\tactive count = 1\n\tpath = /tmp/ww.plist\n\ttype = LaunchAgent\n\tstate = running\n\tprogram = /tmp/ww\n\targuments = {\n\t\t/tmp/ww\n\t\trun\n\t\t--identity\n\t\t/private/key\n\t}\n\tpid = 321\n\tlast exit code = 0\n}\n";
    const ENABLED: &str = "disabled services = {\n\t\"io.wetware.ww\" => false\n}\n";

    #[test]
    fn systemd_running_and_enabled_are_independent() {
        let running_disabled = SYSTEMD.replace("UnitFileState=enabled", "UnitFileState=disabled");
        let observed =
            systemd_snapshot(&running_disabled, b"operator-owned configuration").unwrap();
        assert_eq!(observed.running, RunningState::Running { pid: 321 });
        assert_eq!(observed.enabled, EnabledState::Disabled);
        assert_eq!(observed.executable, Some(PathBuf::from("/tmp/ww")));
        let stopped = SYSTEMD
            .replace("ActiveState=active", "ActiveState=inactive")
            .replace("SubState=running", "SubState=dead")
            .replace("MainPID=321", "MainPID=0");
        let observed = systemd_snapshot(&stopped, b"operator-owned configuration").unwrap();
        assert_eq!(observed.running, RunningState::Stopped);
        assert_eq!(observed.enabled, EnabledState::Enabled);
    }

    #[test]
    fn systemd_failed_without_pid_remains_stopped() {
        let failed = SYSTEMD
            .replace("ActiveState=active", "ActiveState=failed")
            .replace("SubState=running", "SubState=failed")
            .replace("MainPID=321", "MainPID=0")
            .replace("Result=success", "Result=exit-code");
        let observed = systemd_snapshot(&failed, b"custom").unwrap();
        assert_eq!(observed.running, RunningState::Stopped);
        assert!(observed.failed);
    }

    #[test]
    fn systemd_absent_requires_explicit_manager_evidence() {
        let absent = "LoadState=not-found\nUnitFileState=\nActiveState=inactive\nSubState=dead\nMainPID=0\nResult=success\nExecStart=\nEnvironment=\nEnvironmentFiles=\nFragmentPath=\nDropInPaths=\nNeedDaemonReload=no\n";
        let observed = systemd_snapshot(absent, b"").unwrap();
        assert!(!observed.registered);
        assert_eq!(observed.running, RunningState::Stopped);
        assert!(systemd_snapshot("", b"").is_err());
        assert!(systemd_snapshot(&absent.replace("MainPID=0", "MainPID=7"), b"").is_err());
    }

    #[test]
    fn systemd_absent_accepts_omitted_empty_execstart_but_rejects_command_evidence() {
        // Actual systemd 255 output for a nonexistent unit. --all also omits
        // the empty ExecStart and EnvironmentFiles array properties.
        let absent = "MainPID=0\nResult=success\nEnvironment=\nLoadState=not-found\nActiveState=inactive\nSubState=dead\nFragmentPath=\nDropInPaths=\nUnitFileState=\nNeedDaemonReload=no\n";
        let observed = systemd_snapshot(absent, b"").unwrap();
        assert!(!observed.registered);
        assert_eq!(observed.running, RunningState::Stopped);
        assert!(systemd_snapshot(&format!("{absent}ExecStart=/bin/sleep 300\n"), b"").is_err());
        assert!(systemd_snapshot(
            &absent.replace("ActiveState=inactive", "ActiveState=active"),
            b""
        )
        .is_err());
    }

    #[test]
    fn systemd_omitted_empty_environment_files_preserves_configuration_identity() {
        let omitted = SYSTEMD.replace("EnvironmentFiles=\n", "");
        let before = systemd_snapshot(SYSTEMD, b"unit bytes").unwrap();
        let after = systemd_snapshot(&omitted, b"unit bytes").unwrap();
        assert_eq!(before.configuration_identity, after.configuration_identity);
        let configured =
            format!("{omitted}EnvironmentFiles=/home/testww/probe.env (ignore_errors=yes)\n");
        let changed = systemd_snapshot(&configured, b"unit bytes").unwrap();
        assert_ne!(
            before.configuration_identity,
            changed.configuration_identity
        );
        let missing_command = omitted
            .lines()
            .filter(|line| !line.starts_with("ExecStart="))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(systemd_snapshot(&missing_command, b"unit bytes").is_err());
    }

    #[test]
    fn systemd_rejects_ambiguous_and_malformed_states() {
        for invalid in [
            SYSTEMD.replace("ActiveState=active", "ActiveState=activating"),
            SYSTEMD.replace("MainPID=321", "MainPID=0"),
            SYSTEMD.replace("UnitFileState=enabled", "UnitFileState=unexpected"),
            SYSTEMD.replace("LoadState=loaded", "LoadState=error"),
            SYSTEMD.replace("NeedDaemonReload=no", "NeedDaemonReload=yes"),
            format!("{SYSTEMD}MainPID=456\n"),
        ] {
            assert!(systemd_snapshot(&invalid, b"custom").is_err(), "{invalid}");
        }
    }

    #[test]
    fn configuration_evidence_detects_custom_dropin_changes() {
        let before = systemd_snapshot(SYSTEMD, b"[Service]\nEnvironment=CUSTOM=before").unwrap();
        let after = systemd_snapshot(SYSTEMD, b"[Service]\nEnvironment=CUSTOM=after").unwrap();
        assert_ne!(before.configuration_identity, after.configuration_identity);
    }

    #[test]
    fn launchd_running_disabled_and_stopped_enabled_are_distinct() {
        let disabled = ENABLED.replace("false", "true");
        let observed = launchd_snapshot(LAUNCHD, &disabled, b"custom plist").unwrap();
        assert_eq!(observed.running, RunningState::Running { pid: 321 });
        assert_eq!(observed.enabled, EnabledState::Disabled);
        let stopped = LAUNCHD
            .replace("state = running", "state = not running")
            .replace("\tpid = 321\n", "");
        let observed = launchd_snapshot(&stopped, ENABLED, b"custom plist").unwrap();
        assert_eq!(observed.running, RunningState::Stopped);
        assert_eq!(observed.enabled, EnabledState::Enabled);
    }

    #[test]
    fn launchd_rejects_missing_ambiguous_and_duplicate_evidence() {
        for invalid in [
            String::new(),
            LAUNCHD.replace("state = running", "state = waiting"),
            LAUNCHD.replace("\tpid = 321\n", ""),
            LAUNCHD.replace("\tpid = 321\n", "\tpid = 321\n\tpid = 456\n"),
        ] {
            assert!(launchd_snapshot(&invalid, ENABLED, b"custom").is_err());
        }
        assert!(launchd_snapshot(LAUNCHD, "manager error", b"custom").is_err());
    }

    fn before() -> ServiceSnapshot {
        ServiceSnapshot {
            registered: true,
            enabled: EnabledState::Enabled,
            running: RunningState::Running { pid: 321 },
            failed: false,
            executable: Some(PathBuf::from("/tmp/ww")),
            configuration_identity: "captured-service".into(),
        }
    }

    fn admission() -> LegacyRestartAdmission {
        LegacyRestartAdmission {
            prior_pid: 321,
            configuration_identity: "captured-service".into(),
            startup: StartupIdentity {
                configuration: "captured-configuration".into(),
                fhs_baseline: Some("captured-fhs-baseline".into()),
                frozen_sources: "captured-sources".into(),
                publisher_target: Some("captured-publisher-target".into()),
            },
        }
    }

    #[test]
    fn admission_requires_known_running_service_and_matching_inputs() {
        let admitted = admission();
        assert!(admitted.validate(&before()).is_ok());
        let mut changed = before();
        changed.configuration_identity.push_str("-pending");
        assert!(admitted.validate(&changed).is_err());
        changed = before();
        changed.running = RunningState::Stopped;
        assert!(admitted.validate(&changed).is_err());
        changed = before();
        changed.enabled = EnabledState::Unknown;
        assert!(admitted.validate(&changed).is_err());
    }
    fn artifact() -> ArtifactIdentity {
        ArtifactIdentity {
            algorithm: super::super::DigestAlgorithm::Sha256,
            digest: "11".repeat(32),
            size: 42,
        }
    }

    #[test]
    fn running_evidence_requires_exact_artifact_startup_and_readiness() {
        let admitted = admission();
        let expected = artifact();
        let good = LegacyRunningEvidence {
            pid: 322,
            artifact: expected.clone(),
            startup: admitted.startup.clone(),
            serving_ready: true,
        };
        assert!(good.verify(&admitted, &expected).is_ok());
        let mut wrong = good.clone();
        wrong.artifact.digest = "22".repeat(32);
        assert!(wrong.verify(&admitted, &expected).is_err());
        wrong = good.clone();
        wrong.pid = admitted.prior_pid;
        assert!(wrong.verify(&admitted, &expected).is_err());
        wrong = good.clone();
        wrong.serving_ready = false;
        assert!(wrong.verify(&admitted, &expected).is_err());
        for pending in [
            "configuration",
            "fhs_baseline",
            "frozen_sources",
            "publisher_target",
        ] {
            wrong = good.clone();
            match pending {
                "configuration" => wrong.startup.configuration.push_str("-pending"),
                "fhs_baseline" => wrong.startup.fhs_baseline = Some("pending-fhs-baseline".into()),
                "frozen_sources" => wrong.startup.frozen_sources.push_str("-pending"),
                _ => wrong.startup.publisher_target = Some("pending-publisher".into()),
            }
            assert!(wrong.verify(&admitted, &expected).is_err(), "{pending}");
        }
    }

    #[test]
    fn native_adapter_cannot_claim_compatibility_or_live_admission() {
        let home = tempfile::tempdir().unwrap();
        let mut service = NativeService::new(home.path()).unwrap();
        assert!(service.legacy_admit_restart(&before()).is_err());
        assert!(service.legacy_revalidate_restart(&admission()).is_err());
        assert!(service
            .legacy_compatible(&artifact(), &home.path().join("candidate"))
            .is_err());
        assert!(service
            .legacy_restart(&admission(), &artifact(), Instant::now())
            .is_err());
        assert!(service.legacy_stop(Instant::now()).is_err());
        assert_eq!(std::fs::read_dir(home.path()).unwrap().count(), 0);
    }

    #[test]
    fn observations_capture_output_and_bound_time_and_bytes() {
        let output = observe(
            "/bin/sh",
            &["-c", "printf observed"],
            Duration::from_secs(1),
        )
        .unwrap();
        assert!(output.status.success());
        assert_eq!(output.stdout, "observed");
        let started = Instant::now();
        let timeout = observe(
            "/bin/sh",
            &["-c", "exec sleep 5"],
            Duration::from_millis(25),
        )
        .unwrap_err();
        assert!(timeout.to_string().contains("deadline"));
        assert!(started.elapsed() < Duration::from_secs(2));
        let too_large =
            observe("/bin/sh", &["-c", "exec yes x"], Duration::from_secs(1)).unwrap_err();
        assert!(too_large.to_string().contains("bound"));
    }

    #[test]
    fn launchd_failed_stopped_and_unknown_enablement_remain_distinct() {
        let stopped = LAUNCHD
            .replace("state = running", "state = not running")
            .replace("\tpid = 321\n", "")
            .replace("last exit code = 0", "last exit code = 78");
        let observed = launchd_snapshot(&stopped, "disabled services = {\n}\n", b"custom").unwrap();
        assert_eq!(observed.running, RunningState::Stopped);
        assert_eq!(observed.enabled, EnabledState::Unknown);
        assert!(observed.failed);
    }

    #[test]
    fn service_definition_is_bounded_and_symlinks_are_refused() {
        use std::os::unix::fs::symlink;
        let root = tempfile::tempdir().unwrap();
        let file = root.path().join("custom.plist");
        let link = root.path().join("link.plist");
        std::fs::write(&file, b"operator-owned bytes").unwrap();
        symlink(&file, &link).unwrap();
        assert!(read_definition(&link).is_err());
        assert_eq!(
            read_definition(&file).unwrap().unwrap(),
            b"operator-owned bytes"
        );
        std::fs::write(&file, vec![b'x'; MAX_OBSERVATION_BYTES + 1]).unwrap();
        assert!(read_definition(&file).is_err());
        assert!(read_definition(&root.path().join("absent"))
            .unwrap()
            .is_none());
    }

    #[test]
    #[ignore = "reads the real per-user manager; run explicitly for platform evidence"]
    fn native_manager_observation() {
        let home = dirs::home_dir().unwrap();
        let mut service = NativeService::new(home).unwrap();
        let observed = service
            .snapshot(Instant::now() + COMMAND_TIMEOUT)
            .expect("native manager read-only observation");
        eprintln!(
            "registered={} enabled={:?} running={:?} failed={} executable={:?}",
            observed.registered,
            observed.enabled,
            observed.running,
            observed.failed,
            observed.executable
        );
    }
    #[test]
    fn launchd_accepts_current_manager_enablement_words() {
        assert_eq!(
            launchd_enabled("\n\tdisabled services = {\n\t\t\"io.wetware.ww\" => enabled\n\t}\n")
                .unwrap(),
            EnabledState::Enabled
        );
        assert_eq!(
            launchd_enabled("disabled services = {\n\"io.wetware.ww\" => disabled\n}\n").unwrap(),
            EnabledState::Disabled
        );
    }
    #[test]
    fn service_definition_refuses_fifo_without_waiting_for_a_writer() {
        let root = tempfile::tempdir().unwrap();
        let fifo = root.path().join("service.plist");
        let name = std::ffi::CString::new(fifo.as_os_str().as_encoded_bytes()).unwrap();
        // SAFETY: name is a live NUL-terminated pathname. The directory is
        // test-owned and the mode is a valid permission mask.
        assert_eq!(unsafe { libc::mkfifo(name.as_ptr(), 0o600) }, 0);
        assert!(read_definition(&fifo).is_err());
    }
}
