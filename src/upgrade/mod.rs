//! Explicit, recoverable native Host replacement.
//!
//! Only an authenticated release can construct [`PreverifiedCandidate`].
//! A checksum or local receipt is not upgrade authorization. Native operational
//! compatibility and live restart admission remain separately enforced gates.

use anyhow::{ensure, Context, Result};
use serde::{Deserialize, Serialize};
use std::io::Read;
use std::path::Path;
use std::time::{Duration, Instant};

use crate::installation_state::{MutationLock, Receipt};
use service::{
    EnabledState, LegacyRestartAdmission, RunningState, ServiceControl, ServiceSnapshot,
};
use storage::{checkpoint, Paths};

pub mod attestation;
pub mod inputs;
pub mod native_linux;
pub mod provenance;
pub mod removal;
pub mod service;
pub mod startup;
#[doc(hidden)]
pub mod storage;
mod transaction;
pub use transaction::{validate_transaction_start, UpgradeRequest};

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum DigestAlgorithm {
    Sha256,
    Blake3,
}

/// Exact executable bytes; version and source revision are separate provenance.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ArtifactIdentity {
    pub algorithm: DigestAlgorithm,
    pub digest: String,
    pub size: u64,
}

impl ArtifactIdentity {
    fn validate(&self) -> Result<()> {
        ensure!(
            self.size > 0 && self.size <= storage::MAX_EXECUTABLE,
            "selected native artifact size must be between 1 byte and 1 GiB"
        );
        ensure!(
            self.digest.len() == 64
                && self
                    .digest
                    .bytes()
                    .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)),
            "selected native artifact requires a canonical digest and explicit algorithm"
        );
        Ok(())
    }
}

/// An authenticated selection, independent of downloaded bytes.
///
/// The only public constructor consumes an authenticated release and performs
/// platform/compatibility selection. There is no metadata-only constructor or
/// Deserialize implementation.
#[derive(Clone, Debug)]
pub struct PreverifiedCandidate {
    artifact: ArtifactIdentity,
    #[cfg(test)]
    release: String,
    version: String,
    revision: String,
    #[cfg(test)]
    platform: String,
}

impl PreverifiedCandidate {
    /// Exact authenticated native identity, without exposing a trusted constructor.
    pub fn artifact(&self) -> &ArtifactIdentity {
        &self.artifact
    }

    /// Bind #738's authenticated platform selection to #739's transaction input.
    /// This does not certify downloaded bytes or operational compatibility;
    /// staging and the service adapter perform those checks independently.
    pub fn from_release(
        release: &crate::release::descriptor::AuthenticatedRelease,
        target: &str,
    ) -> Result<Self> {
        let selection =
            release.select(target, crate::release::descriptor::COMPATIBILITY_PROFILE)?;
        let host = selection.host();
        let candidate = Self {
            artifact: ArtifactIdentity {
                algorithm: DigestAlgorithm::Sha256,
                digest: hex::encode(host.artifact.sha256),
                size: host.artifact.size,
            },
            #[cfg(test)]
            release: release.descriptor_cid().to_string(),
            version: release.descriptor().version.clone(),
            revision: host.provenance.revision.clone(),
            #[cfg(test)]
            platform: host.target.clone(),
        };
        candidate.artifact.validate()?;
        Ok(candidate)
    }
}

/// Both selections are authenticated. The legacy receipt authenticates neither.
#[cfg(test)]
struct LegacyUpgradeRequest<R: Read> {
    pub installed: PreverifiedCandidate,
    pub candidate: PreverifiedCandidate,
    pub bytes: R,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", rename_all = "kebab-case")]
pub enum Outcome {
    AlreadyInstalled,
    Checked,
    /// `live_ready` is false for absent/stopped services.
    Installed {
        live_ready: bool,
    },
    /// Recovery restored the previous artifact or completed an already committed
    /// transaction. False never claims that a running daemon is ready.
    Recovered {
        artifact: ArtifactIdentity,
        action: RecoveryAction,
        live_ready: bool,
    },
    NoPendingTransaction,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum RecoveryAction {
    AbortedBeforeReplacement,
    RestoredPrevious,
    CompletedCommit,
}

// Recovery records stay private: decoding local state cannot manufacture a
// public trusted selection. The private directory is within the operator's
// existing local authority boundary, not an independent authentication root.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct ArtifactRecord {
    artifact: ArtifactIdentity,
    release: String,
    version: String,
    revision: String,
    platform: String,
}

#[cfg(test)]
impl From<PreverifiedCandidate> for ArtifactRecord {
    fn from(selected: PreverifiedCandidate) -> Self {
        Self {
            artifact: selected.artifact,
            release: selected.release,
            version: selected.version,
            revision: selected.revision,
            platform: selected.platform,
        }
    }
}

impl ArtifactRecord {
    fn validate(&self) -> Result<()> {
        self.artifact.validate()?;
        for (name, value) in [
            ("release", &self.release),
            ("version", &self.version),
            ("revision", &self.revision),
            ("platform", &self.platform),
        ] {
            ensure!(
                !value.is_empty() && value.len() <= 1024 && !value.chars().any(char::is_control),
                "invalid authenticated artifact {name}"
            );
        }
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
enum Phase {
    Staging,
    Ready,
    Replacing,
    Installed,
    Restarting,
    Verified,
    Committed,
    Restoring,
    Restored,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
enum Operation {
    Upgrade,
    Rollback,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Journal {
    version: u8,
    operation: Operation,
    phase: Phase,
    previous: ArtifactRecord,
    candidate: ArtifactRecord,
    before: ServiceSnapshot,
    admission: Option<LegacyRestartAdmission>,
    receipt_before: Vec<u8>,
    receipt_after: Option<Vec<u8>>,
    retained_before: Option<Vec<u8>>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct Retained {
    version: u8,
    current: ArtifactRecord,
    previous: ArtifactRecord,
}

impl Retained {
    fn decode(bytes: &[u8]) -> Result<Self> {
        let record: Self = serde_json::from_slice(bytes).context("invalid retained Host record")?;
        ensure!(
            record.version == 1,
            "unsupported retained Host record version"
        );
        record.current.validate()?;
        record.previous.validate()?;
        Ok(record)
    }
}

/// Holds exclusive installation mutation authority for one operator operation.
/// The lock also excludes the existing installer and daemon-definition writers.
pub struct Engine {
    _lock: MutationLock,
    paths: Paths,
}

impl Engine {
    pub fn open(home: &Path) -> Result<Self> {
        Self::from_lock(MutationLock::acquire(home)?)
    }

    /// Transfer an existing installation mutation owner without reacquiring its
    /// stable lock inode. Installers must create private .ww/bin directories
    /// before this handoff; this method writes no executable bytes.
    pub fn from_lock(lock: MutationLock) -> Result<Self> {
        lock.reject_pending_image()?;
        let paths = Paths::open(lock.home())?;
        Ok(Self { _lock: lock, paths })
    }

    pub fn mutation_lock(&self) -> &MutationLock {
        &self._lock
    }

    /// Resolve a pending transaction before calling the release selector.
    ///
    /// The deadline governs admission to executable replacement, serving
    /// readiness, and receipt commit. The caller must separately bound download
    /// I/O: this synchronous owner cannot cancel an arbitrary blocking `Read` or
    /// in-flight filesystem operation. Once a commit is admitted, required
    /// durability work and terminal cleanup complete even if the budget expires.
    #[cfg(test)]
    fn legacy_operator_upgrade<R: Read>(
        &mut self,
        service: &mut impl ServiceControl,
        timeout: Duration,
        select: impl FnOnce() -> Result<LegacyUpgradeRequest<R>>,
    ) -> Result<Outcome> {
        self.legacy_operator_upgrade_until(service, deadline(timeout)?, select)
    }

    #[cfg(test)]
    fn legacy_operator_upgrade_until<R: Read>(
        &mut self,
        service: &mut impl ServiceControl,
        deadline: Instant,
        select: impl FnOnce() -> Result<LegacyUpgradeRequest<R>>,
    ) -> Result<Outcome> {
        check_deadline(deadline)?;
        if storage::exists(&self.paths.journal)? {
            let result = self.legacy_recover_until(service, deadline);
            return self.with_recovery_context(result);
        }
        let request = select()?;
        let result = self.start(
            service,
            deadline,
            Operation::Upgrade,
            request.installed.into(),
            request.candidate.into(),
            request.bytes,
        );
        self.with_recovery_context(result)
    }

    /// Select only the previous authenticated artifact retained by this owner.
    /// No release discovery, freshness watermark, or deployment head is changed.
    #[cfg(test)]
    fn legacy_operator_rollback(
        &mut self,
        service: &mut impl ServiceControl,
        timeout: Duration,
    ) -> Result<Outcome> {
        self.legacy_operator_rollback_until(service, deadline(timeout)?)
    }

    #[cfg(test)]
    fn legacy_operator_rollback_until(
        &mut self,
        service: &mut impl ServiceControl,
        deadline: Instant,
    ) -> Result<Outcome> {
        check_deadline(deadline)?;
        if storage::exists(&self.paths.journal)? {
            let result = self.legacy_recover_until(service, deadline);
            return self.with_recovery_context(result);
        }
        let bytes = storage::read_record(&self.paths.retained)?
            .context("no retained authenticated previous Host; rollback is unavailable")?;
        let retained = Retained::decode(&bytes)?;
        let source = self.paths.artifact(&retained.previous.artifact);
        storage::verify(&source, &retained.previous.artifact).context(
            "retained previous Host is missing or corrupt; restore authenticated recovery bytes",
        )?;
        let reader = storage::open_read(&source)?;
        let result = self.start(
            service,
            deadline,
            Operation::Rollback,
            retained.current,
            retained.previous,
            reader,
        );
        self.with_recovery_context(result)
    }

    /// Recover the recorded operation without selecting another release.
    #[cfg(test)]
    fn legacy_recover(
        &mut self,
        service: &mut impl ServiceControl,
        timeout: Duration,
    ) -> Result<Outcome> {
        self.legacy_recover_before(service, deadline(timeout)?)
    }

    #[cfg(test)]
    fn legacy_recover_before(
        &mut self,
        service: &mut impl ServiceControl,
        deadline: Instant,
    ) -> Result<Outcome> {
        check_deadline(deadline)?;
        let result = self.legacy_recover_until(service, deadline);
        self.with_recovery_context(result)
    }

    /// Check recorded recovery ownership before a bootstrap transport fetches
    /// an executable. This engine retains the mutation lock throughout selection.
    pub fn require_pending_recovery(&self) -> Result<()> {
        let bytes = storage::read_private_record(&self.paths.journal)?
            .context("no pending Host transaction; recovery-only cannot select a new release")?;
        let value: serde_json::Value =
            serde_json::from_slice(&bytes).context("invalid Host transaction journal")?;
        if value.get("version").and_then(|v| v.as_u64()) == Some(1) {
            self.pending_recovery()?.context("legacy journal missing")?;
        } else {
            match serde_json::from_slice::<JournalV2>(&bytes)
                .context("invalid Host transaction journal")?
            {
                JournalV2::Upgrade(j) | JournalV2::Rollback(j) => j.validate()?,
                JournalV2::Admit(_) => {
                    anyhow::bail!("pending startup admission must recover through startup admit")
                }
            }
        }
        Ok(())
    }

    /// Recover only through an independently authenticated recorded generation.
    /// The caller must verify that its running recovery executable has the exact
    /// selected bytes. A local journal supplies scope, never release authority.
    #[cfg(test)]
    fn legacy_recover_authenticated(
        &mut self,
        service: &mut impl ServiceControl,
        timeout: Duration,
        executable: PreverifiedCandidate,
    ) -> Result<Outcome> {
        self.legacy_recover_authenticated_until(service, deadline(timeout)?, executable)
    }

    #[cfg(test)]
    fn legacy_recover_authenticated_until(
        &mut self,
        service: &mut impl ServiceControl,
        deadline: Instant,
        executable: PreverifiedCandidate,
    ) -> Result<Outcome> {
        check_deadline(deadline)?;
        let journal = self.pending_recovery()?.context(
            "no pending Host transaction; recovery-only operation cannot select a new release",
        )?;
        let executable = ArtifactRecord::from(executable);
        executable.validate()?;
        ensure!(
            executable == journal.previous || executable == journal.candidate,
            "authenticated recovery executable is not a recorded Host generation; preserve the pending transaction and select its exact release"
        );
        self.legacy_recover_before(service, deadline)
    }

    fn pending_recovery(&self) -> Result<Option<Journal>> {
        let Some(bytes) = storage::read_record(&self.paths.journal)? else {
            return Ok(None);
        };
        let journal: Journal = serde_json::from_slice(&bytes).context(
            "invalid Host transaction journal; preserve recovery files for manual inspection",
        )?;
        self.validate_journal(&journal)?;
        self.validate_receipt_bindings(&journal)?;
        self.expect_receipt(&journal, true)?;
        Ok(Some(journal))
    }

    #[cfg(test)]
    fn start(
        &mut self,
        service: &mut impl ServiceControl,
        deadline: Instant,
        operation: Operation,
        previous: ArtifactRecord,
        candidate: ArtifactRecord,
        bytes: impl Read,
    ) -> Result<Outcome> {
        previous.validate()?;
        candidate.validate()?;
        ensure!(
            previous.platform == candidate.platform,
            "authenticated native platform changed"
        );
        let receipt_before =
            storage::read_record(&self.paths.receipt)?.context("installation receipt missing")?;
        let receipt = Receipt::decode(&receipt_before)?;
        ensure!(
            receipt.complete,
            "installation is incomplete; recover the original installation before upgrading"
        );
        let installed_blake3 = storage::verify(&self.paths.executable, &previous.artifact)
            .context("installed Host does not match authenticated installed selection")?;
        ensure!(
            receipt.executable_digest() == installed_blake3,
            "installed Host does not match ownership receipt"
        );
        // Exact artifact identity, not version or revision, determines no-op.
        if previous.artifact == candidate.artifact {
            return Ok(Outcome::AlreadyInstalled);
        }
        let before = service
            .snapshot(deadline)
            .context("observe managed service before replacement")?;
        validate_snapshot(&before, &self.paths.executable)?;
        check_deadline(deadline)?;
        let admission = if matches!(before.running, RunningState::Running { .. }) {
            let admitted = service.legacy_admit_restart(&before).context("running startup inputs are unavailable or pending; explicitly stop the managed service before upgrading")?;
            admitted.validate(&before)?;
            Some(admitted)
        } else {
            None
        };
        let retained_before = storage::read_record(&self.paths.retained)?;
        if let Some(bytes) = &retained_before {
            Retained::decode(bytes)?;
        }
        let mut journal = Journal {
            version: 1,
            operation,
            phase: Phase::Staging,
            previous,
            candidate,
            before,
            admission,
            receipt_before,
            receipt_after: None,
            retained_before,
        };
        self.save(&journal)?;

        let stage = self.paths.store.join("stage");
        let candidate_blake3 = storage::stage(bytes, &stage, &journal.candidate.artifact)?;
        storage::rename(&stage, &self.paths.artifact(&journal.candidate.artifact))?;
        service.legacy_compatible(&journal.candidate.artifact, &self.paths.artifact(&journal.candidate.artifact))
            .context("selected Host is incompatible with effective deployment; Host/FHS combined changes require separate authorization")?;
        storage::stage(
            storage::open_read(&self.paths.executable)?,
            &stage,
            &journal.previous.artifact,
        )?;
        storage::rename(&stage, &self.paths.artifact(&journal.previous.artifact))?;
        journal.receipt_after = Some(receipt.with_executable_digest(candidate_blake3).encode()?);
        self.set_phase(&mut journal, Phase::Ready)?;

        // Recheck immediately before replacement. For a live service, admission
        // additionally binds child consumption before publication/guest effects.
        ensure!(
            service.snapshot(deadline)? == journal.before,
            "managed service state changed before replacement; recover the pending transaction"
        );
        self.readmit(service, &journal)?;
        service.legacy_compatible(
            &journal.candidate.artifact,
            &self.paths.artifact(&journal.candidate.artifact),
        )?;
        check_deadline(deadline)?;
        self.expect_receipt(&journal, false)?;
        storage::verify(&self.paths.executable, &journal.previous.artifact)?;
        self.set_phase(&mut journal, Phase::Replacing)?;
        self.install_cached(&journal.candidate.artifact, deadline)?;
        self.set_phase(&mut journal, Phase::Installed)?;

        let live_ready = if journal.admission.is_some() {
            self.set_phase(&mut journal, Phase::Restarting)?;
            self.restart(service, &journal, &journal.candidate.artifact, deadline)?;
            true
        } else {
            ensure!(service.snapshot(deadline)? == journal.before, "service state changed during stopped-service upgrade; installed candidate retained, recovery required");
            false
        };
        storage::verify(&self.paths.executable, &journal.candidate.artifact)?;
        self.set_phase(&mut journal, Phase::Verified)?;
        self.expect_receipt(&journal, false)?;
        check_deadline(deadline)?;
        storage::write_record(
            &self.paths.receipt,
            journal
                .receipt_after
                .as_ref()
                .context("replacement receipt")?,
        )?;
        let retained = Retained {
            version: 1,
            current: journal.candidate.clone(),
            previous: journal.previous.clone(),
        };
        storage::write_record(&self.paths.retained, &serde_json::to_vec(&retained)?)?;
        self.set_phase(&mut journal, Phase::Committed)?;
        self.finish_committed(&journal)?;
        Ok(Outcome::Installed { live_ready })
    }

    #[cfg(test)]
    fn readmit(&self, service: &mut impl ServiceControl, journal: &Journal) -> Result<()> {
        if let Some(expected) = &journal.admission {
            service.legacy_revalidate_restart(expected).context(
                "startup inputs or publisher target changed; explicitly stop and activate separately")?;
        }
        Ok(())
    }

    #[cfg(test)]
    fn restart(
        &self,
        service: &mut impl ServiceControl,
        journal: &Journal,
        expected: &ArtifactIdentity,
        deadline: Instant,
    ) -> Result<()> {
        let admission = journal
            .admission
            .as_ref()
            .context("restart was not admitted")?;
        check_deadline(deadline)?;
        checkpoint("service.restart.before")?;
        let result = service.legacy_restart(admission, expected, deadline);
        checkpoint("service.restart.after")?;
        let verified = result.and_then(|evidence| {
            check_deadline(deadline)?;
            evidence.verify(admission, expected)?;
            storage::verify(&self.paths.executable, expected)?;
            let after = service.snapshot(deadline)?;
            validate_snapshot(&after, &self.paths.executable)?;
            same_configuration(&journal.before, &after)?;
            ensure!(
                after.running == (RunningState::Running { pid: evidence.pid }),
                "service manager PID differs from the verified running Host"
            );
            service
                .legacy_compatible(expected, &self.paths.artifact(expected))
                .context("current deployment became incompatible during restart")?;
            check_deadline(deadline)?;
            Ok(())
        });
        if let Err(error) = verified {
            // Use a separate bounded stop deadline even when readiness exhausted
            // the operation deadline. Thirty seconds matches the existing Host
            // lifecycle drain bound; this is not a readiness default. The caller
            // is told if stop also fails.
            let stop = self.stop_and_confirm(service, Instant::now() + Duration::from_secs(30));
            return match stop {
                Ok(()) => Err(error).context("Host restart/readiness failed; service is stopped, previous bytes retained; recover the pending transaction"),
                Err(stop_error) => Err(error).context(format!("Host restart/readiness failed and service stop failed ({stop_error:#}); service state is unconfirmed, previous bytes retained; recover the pending transaction")),
            };
        }
        Ok(())
    }

    #[cfg(test)]
    fn stop_and_confirm(&self, service: &mut impl ServiceControl, deadline: Instant) -> Result<()> {
        checkpoint("service.stop.before")?;
        service.legacy_stop(deadline)?;
        checkpoint("service.stop.after")?;
        check_deadline(deadline)?;
        ensure!(
            service.snapshot(deadline)?.running == RunningState::Stopped,
            "service manager did not confirm a stopped Host"
        );
        Ok(())
    }

    fn install_cached(&self, artifact: &ArtifactIdentity, deadline: Instant) -> Result<()> {
        let source = self.paths.artifact(artifact);
        storage::verify(&source, artifact).context(
            "recovery artifact is missing or corrupt; restore authenticated bytes before recovery",
        )?;
        let replacement = self.paths.store.join("replacement");
        storage::stage(storage::open_read(&source)?, &replacement, artifact)?;
        check_deadline(deadline)?;
        storage::rename(&replacement, &self.paths.executable)?;
        Ok(())
    }

    #[cfg(test)]
    fn legacy_recover_until(
        &mut self,
        service: &mut impl ServiceControl,
        deadline: Instant,
    ) -> Result<Outcome> {
        let Some(mut journal) = self.pending_recovery()? else {
            return Ok(Outcome::NoPendingTransaction);
        };
        if journal.phase == Phase::Committed {
            storage::verify(&self.paths.executable, &journal.candidate.artifact)?;
            ensure!(
                storage::read_record(&self.paths.receipt)? == journal.receipt_after,
                "committed Host receipt does not match recorded bytes"
            );
            let retained = storage::read_record(&self.paths.retained)?
                .context("committed Host retention record missing")?;
            ensure!(
                Retained::decode(&retained)?
                    == (Retained {
                        version: 1,
                        current: journal.candidate.clone(),
                        previous: journal.previous.clone()
                    }),
                "committed retained Host identity changed"
            );
            self.finish_committed(&journal)?;
            return Ok(Outcome::Recovered {
                artifact: journal.candidate.artifact.clone(),
                action: RecoveryAction::CompletedCommit,
                live_ready: false,
            });
        }
        if journal.phase == Phase::Restored {
            storage::verify(&self.paths.executable, &journal.previous.artifact)?;
            ensure!(
                storage::read_record(&self.paths.receipt)?.as_deref()
                    == Some(journal.receipt_before.as_slice()),
                "restored Host receipt differs from original recorded bytes"
            );
            ensure!(
                storage::read_record(&self.paths.retained)? == journal.retained_before,
                "restored Host retention differs from original recorded metadata"
            );
            self.finish_aborted(&journal)?;
            return Ok(Outcome::Recovered {
                artifact: journal.previous.artifact.clone(),
                action: RecoveryAction::RestoredPrevious,
                live_ready: false,
            });
        }
        if matches!(journal.phase, Phase::Staging | Phase::Ready) {
            storage::verify(&self.paths.executable, &journal.previous.artifact)?;
            ensure!(
                storage::read_record(&self.paths.receipt)?.as_deref()
                    == Some(journal.receipt_before.as_slice()),
                "receipt changed before replacement"
            );
            self.finish_aborted(&journal)?;
            return Ok(Outcome::Recovered {
                artifact: journal.previous.artifact.clone(),
                action: RecoveryAction::AbortedBeforeReplacement,
                live_ready: false,
            });
        }

        let current = service
            .snapshot(deadline)
            .context("cannot establish service state during recovery")?;
        validate_snapshot(&current, &self.paths.executable)?;
        same_configuration(&journal.before, &current)?;
        let previous_on_disk =
            storage::verify(&self.paths.executable, &journal.previous.artifact).is_ok();
        ensure!(previous_on_disk || storage::verify(&self.paths.executable, &journal.candidate.artifact).is_ok(),
            "installed Host matches neither recorded artifact; preserve journal and restore authenticated recovery bytes");
        self.set_phase(&mut journal, Phase::Restoring)?;
        if journal.admission.is_some() {
            self.stop_and_confirm(service, deadline)?;
        } else {
            ensure!(current.running == RunningState::Stopped, "service was started after stopped-service upgrade; explicitly stop it before recovery");
        }
        // Compatibility is checked again against the authoritative deployment.
        // An incompatible previous Host remains stopped; no head is rewound.
        service.legacy_compatible(&journal.previous.artifact, &self.paths.artifact(&journal.previous.artifact)).context("previous Host is incompatible with the current deployment; service remains stopped, recovery pending")?;
        self.readmit(service, &journal)?;
        check_deadline(deadline)?;
        self.install_cached(&journal.previous.artifact, deadline)?;
        let live_ready = if journal.admission.is_some() {
            self.restart(service, &journal, &journal.previous.artifact, deadline)?;
            true
        } else {
            false
        };
        storage::verify(&self.paths.executable, &journal.previous.artifact)?;
        self.expect_receipt(&journal, true)?;
        check_deadline(deadline)?;
        storage::write_record(&self.paths.receipt, &journal.receipt_before)?;
        match &journal.retained_before {
            Some(bytes) => storage::write_record(&self.paths.retained, bytes)?,
            None => storage::remove(&self.paths.retained)?,
        }
        self.set_phase(&mut journal, Phase::Restored)?;
        self.finish_aborted(&journal)?;
        Ok(Outcome::Recovered {
            artifact: journal.previous.artifact.clone(),
            action: RecoveryAction::RestoredPrevious,
            live_ready,
        })
    }

    fn validate_journal(&self, journal: &Journal) -> Result<()> {
        ensure!(
            journal.version == 1,
            "unsupported Host transaction journal version"
        );
        journal.previous.validate()?;
        journal.candidate.validate()?;
        ensure!(
            journal.previous.platform == journal.candidate.platform,
            "journal native platform changed"
        );
        validate_snapshot(&journal.before, &self.paths.executable)?;
        ensure!(
            journal.admission.is_some()
                == matches!(journal.before.running, RunningState::Running { .. }),
            "journal restart admission does not match original service state"
        );
        if let Some(admission) = &journal.admission {
            admission.validate(&journal.before)?;
        }
        let original = Receipt::decode(&journal.receipt_before)?;
        ensure!(
            original.complete,
            "journal contains an incomplete or unknown installation receipt"
        );
        if let Some(bytes) = &journal.receipt_after {
            let next = Receipt::decode(bytes)?;
            let expected = original.with_executable_digest(next.executable_digest().to_owned());
            ensure!(
                next.encode()? == expected.encode()?,
                "journal changes receipt fields outside native executable ownership"
            );
        } else {
            ensure!(
                journal.phase == Phase::Staging,
                "journal is missing the replacement receipt"
            );
        }
        if let Some(bytes) = &journal.retained_before {
            Retained::decode(bytes)?;
        }
        Ok(())
    }

    fn validate_receipt_bindings(&self, journal: &Journal) -> Result<()> {
        let original = Receipt::decode(&journal.receipt_before)?;
        let previous_path = if matches!(
            journal.phase,
            Phase::Staging | Phase::Ready | Phase::Restored
        ) {
            self.paths.executable.clone()
        } else {
            self.paths.artifact(&journal.previous.artifact)
        };
        let previous_blake3 = storage::verify(&previous_path, &journal.previous.artifact)?;
        ensure!(
            original.executable_digest() == previous_blake3,
            "recorded previous receipt does not match verified previous Host bytes"
        );
        if let Some(bytes) = &journal.receipt_after {
            let candidate_path = self.paths.artifact(&journal.candidate.artifact);
            if storage::exists(&candidate_path)? {
                let candidate_blake3 =
                    storage::verify(&candidate_path, &journal.candidate.artifact)?;
                let next = Receipt::decode(bytes)?;
                ensure!(
                    next.executable_digest() == candidate_blake3,
                    "recorded replacement receipt does not match verified candidate bytes"
                );
            } else {
                ensure!(
                    matches!(
                        journal.phase,
                        Phase::Ready | Phase::Restoring | Phase::Restored
                    ),
                    "recorded candidate recovery artifact is missing"
                );
            }
        }
        Ok(())
    }

    #[cfg(test)]
    fn with_recovery_context(&self, result: Result<Outcome>) -> Result<Outcome> {
        result.map_err(|error| {
            if storage::exists(&self.paths.journal).unwrap_or(true) {
                error.context(format!("Host transaction remains pending at {}; installed/running state must be inspected, not inferred from the journal phase; recover this transaction before selecting another release (recovery bytes: {})",
                    self.paths.journal.display(), self.paths.store.display()))
            } else { error }
        })
    }

    fn expect_receipt(&self, journal: &Journal, allow_after: bool) -> Result<()> {
        let current = storage::read_record(&self.paths.receipt)?
            .context("installation receipt is missing during Host transaction")?;
        ensure!(current == journal.receipt_before || (allow_after && journal.receipt_after.as_ref() == Some(&current)),
            "installation receipt changed outside the Host transaction; preserve journal and resolve ownership before recovery");
        Ok(())
    }

    #[cfg(test)]
    fn set_phase(&self, journal: &mut Journal, phase: Phase) -> Result<()> {
        checkpoint(&format!("phase.{phase:?}.before"))?;
        journal.phase = phase;
        self.save(journal)?;
        checkpoint(&format!("phase.{phase:?}.after"))?;
        Ok(())
    }

    #[cfg(test)]
    fn save(&self, journal: &Journal) -> Result<()> {
        storage::write_record(&self.paths.journal, &serde_json::to_vec(journal)?)
    }

    #[cfg(test)]
    fn finish_committed(&self, journal: &Journal) -> Result<()> {
        storage::verify(
            &self.paths.artifact(&journal.previous.artifact),
            &journal.previous.artifact,
        )?;
        if let Some(bytes) = &journal.retained_before {
            let retained = Retained::decode(bytes)?;
            for record in [&retained.current, &retained.previous] {
                if record.artifact != journal.previous.artifact
                    && record.artifact != journal.candidate.artifact
                {
                    storage::remove(&self.paths.artifact(&record.artifact))?;
                }
            }
        }
        self.remove_working_files()?;
        storage::remove(&self.paths.journal)?;
        Ok(())
    }

    fn finish_aborted(&self, journal: &Journal) -> Result<()> {
        // Delete the journal last. Restored is durable before deleting recovery
        // bytes: its recovery path verifies the restored installed executable.
        // Pre-replacement aborts likewise verify the unchanged installed file.
        ensure!(
            matches!(
                journal.phase,
                Phase::Staging | Phase::Ready | Phase::Restored
            ),
            "recovery cleanup requires unchanged or durably restored executable"
        );
        let retained = journal
            .retained_before
            .as_deref()
            .map(Retained::decode)
            .transpose()?;
        let candidate_needed = retained.as_ref().is_some_and(|retained| {
            retained.current.artifact == journal.candidate.artifact
                || retained.previous.artifact == journal.candidate.artifact
        });
        if !candidate_needed && journal.candidate.artifact != journal.previous.artifact {
            storage::remove(&self.paths.artifact(&journal.candidate.artifact))?;
        }
        let previous_needed = retained.as_ref().is_some_and(|retained| {
            retained.current.artifact == journal.previous.artifact
                || retained.previous.artifact == journal.previous.artifact
        });
        if !previous_needed {
            storage::remove(&self.paths.artifact(&journal.previous.artifact))?;
        }
        self.remove_working_files()?;
        storage::remove(&self.paths.journal)?;
        Ok(())
    }

    fn remove_working_files(&self) -> Result<()> {
        storage::remove(&self.paths.store.join("stage"))?;
        storage::remove(&self.paths.store.join("replacement"))?;
        Ok(())
    }
}

fn validate_snapshot(snapshot: &ServiceSnapshot, executable: &Path) -> Result<()> {
    ensure!(
        snapshot.enabled != EnabledState::Unknown && snapshot.running != RunningState::Unknown,
        "managed service state is unknown; no executable replacement was authorized"
    );
    ensure!(
        !snapshot.configuration_identity.is_empty()
            && snapshot.configuration_identity.len() <= 8192,
        "effective service configuration is unavailable or exceeds its bound"
    );
    if snapshot.registered {
        ensure!(
            snapshot.executable.as_deref() == Some(executable),
            "managed service executable differs from the installed Host path"
        );
        if let RunningState::Running { pid } = snapshot.running {
            ensure!(
                pid != 0 && !snapshot.failed,
                "managed service has ambiguous running/failed state"
            );
        }
    } else {
        ensure!(
            snapshot.running == RunningState::Stopped && !snapshot.failed,
            "unregistered service observation contains contradictory state"
        );
        if let Some(configured) = &snapshot.executable {
            ensure!(
                configured == executable,
                "unregistered service definition selects another executable"
            );
        } else {
            ensure!(
                snapshot.enabled == EnabledState::Disabled,
                "absent service observation contains contradictory enablement"
            );
        }
    }
    Ok(())
}

fn same_configuration(before: &ServiceSnapshot, now: &ServiceSnapshot) -> Result<()> {
    ensure!(before.registered == now.registered && before.enabled == now.enabled
        && before.executable == now.executable && before.configuration_identity == now.configuration_identity,
        "managed service configuration or enablement changed; stop and resolve the pending Host transaction explicitly");
    Ok(())
}

fn deadline(timeout: Duration) -> Result<Instant> {
    ensure!(
        !timeout.is_zero(),
        "Host operation timeout must be positive"
    );
    monotonic_now()
        .checked_add(timeout)
        .context("Host operation timeout is out of range")
}

fn check_deadline(deadline: Instant) -> Result<()> {
    ensure!(
        monotonic_now() < deadline,
        "Host operation/readiness deadline expired"
    );
    Ok(())
}

fn monotonic_now() -> Instant {
    let now = Instant::now();
    #[cfg(test)]
    let now = storage::faults::clock_now(now);
    now
}

#[cfg(test)]
mod tests;

/// Version-2 retention records accepted native generations, not release ordering.
/// Decoding this private record alone never authenticates its referenced proof.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct RetainedV2 {
    version: u32,
    installation_id: [u8; 32],
    current: provenance::GenerationRecord,
    previous: Option<provenance::GenerationRecord>,
    contract: Option<startup::ContractIdentity>,
}

impl RetainedV2 {
    fn validate(&self) -> Result<()> {
        ensure!(
            self.version == 2,
            "unsupported guarded native retention version"
        );
        ensure!(
            self.installation_id != [0; 32],
            "retention installation ID is zero"
        );
        self.current.validate()?;
        if let Some(previous) = &self.previous {
            previous.validate()?;
            ensure!(
                previous.target == self.current.target,
                "retained native target changed"
            );
            ensure!(
                previous.artifact != self.current.artifact,
                "retention must not rotate identical native bytes"
            );
        }
        if let Some(contract) = &self.contract {
            ensure!(
                contract.generation > 0,
                "invalid retained contract generation"
            );
        }
        Ok(())
    }

    fn encode(&self) -> Result<Vec<u8>> {
        self.validate()?;
        let bytes = serde_json::to_vec(self)?;
        ensure!(
            bytes.len() <= storage::MAX_RECORD,
            "native retention exceeds 64 KiB"
        );
        Ok(bytes)
    }

    fn decode(bytes: &[u8]) -> Result<Self> {
        ensure!(
            bytes.len() <= storage::MAX_RECORD,
            "native retention exceeds 64 KiB"
        );
        let value: Self =
            serde_json::from_slice(bytes).context("invalid guarded native retention")?;
        ensure!(
            value.encode()? == bytes,
            "noncanonical guarded native retention"
        );
        Ok(value)
    }
}

/// Validate the committed installed tuple under the caller's startup lease.
/// The caller must separately verify executing bytes and captured startup inputs.
pub fn validate_steady_start(
    lease: &startup::StartupLease,
    state: &startup::StartupStateV1,
    contract: &startup::StartupContractV1,
) -> Result<()> {
    ensure!(
        lease.marker()?.installation_id == state.installation_id,
        "startup marker identifies another installation"
    );
    ensure!(
        lease.read_state()?.as_ref() == Some(state)
            && lease.read_contract()?.as_ref() == Some(contract),
        "startup state or contract changed during admission"
    );
    ensure!(
        matches!(
            state.disposition,
            startup::StartupDisposition::Committed { .. }
        ) && state.initializing_receipt.is_none(),
        "startup requires a committed native tuple"
    );
    state.validate_contract(contract)?;
    let paths = Paths::existing(lease.home())?;
    ensure!(
        !storage::exists(&paths.journal)?,
        "pending Host transaction requires authenticated recovery"
    );
    ensure!(
        !storage::exists(&lease.home().join(".ww/image-update.json"))?,
        "pending FHS update requires recovery"
    );
    let retained = read_retained_v2(&paths)?;
    ensure!(
        retained.contract.as_ref() == Some(&contract.identity),
        "retention references another startup contract"
    );
    validate_committed_native(&paths, state, &retained)?;
    Ok(())
}

fn read_retained_v2(paths: &Paths) -> Result<RetainedV2> {
    RetainedV2::decode(
        &storage::read_private_record(&paths.retained)?
            .context("guarded native retention is missing")?,
    )
}

/// Historical keys are usable only after the caller binds this custody-checked
/// committed retention to the installation marker, current state and receipt.
fn authenticate_retained(
    paths: &Paths,
    retained: &RetainedV2,
    record: &provenance::GenerationRecord,
) -> Result<provenance::AuthenticatedGeneration> {
    retained.validate()?;
    ensure!(
        &retained.current == record || retained.previous.as_ref() == Some(record),
        "generation is not exact committed retention"
    );
    storage::directory(&paths.proofs)?;
    let proof = record
        .proof
        .verify(&storage::read_proof(&paths.proof(&record.proof)?)?)?;
    let generation = provenance::AuthenticatedGeneration::from_proof(
        proof,
        &record.verification_keys,
        &record.target,
    )?;
    ensure!(
        generation.record() == record,
        "retained proof authenticates another native generation"
    );
    Ok(generation)
}

fn validate_committed_native(
    paths: &Paths,
    state: &startup::StartupStateV1,
    retained: &RetainedV2,
) -> Result<Receipt> {
    state.validate()?;
    retained.validate()?;
    ensure!(
        state.installation_id == retained.installation_id && state.native == retained.current,
        "startup state and committed native retention disagree"
    );
    let receipt_bytes = storage::read_private_record(&paths.receipt)?
        .context("guarded installation receipt is missing")?;
    let receipt = Receipt::decode(&receipt_bytes)?;
    ensure!(
        receipt.encode()? == receipt_bytes,
        "guarded receipt is not canonical"
    );
    ensure!(receipt.complete, "guarded installation is incomplete");
    let locator =
        provenance::InstalledRelease::from_root(retained.current.transport_root.parse()?)?;
    ensure!(
        storage::read_private_record(&paths.locator)?.as_deref()
            == Some(serde_json::to_vec(&locator)?.as_slice()),
        "installed release locator differs from committed native provenance"
    );
    let generation = authenticate_retained(paths, retained, &retained.current)?;
    let native_blake3 = storage::verify(&paths.executable, generation.artifact())?;
    ensure!(
        native_blake3 == receipt.executable_digest(),
        "native ownership receipt differs from authenticated executable bytes"
    );
    Ok(receipt)
}

/// Owns instance and startup exclusion across the installer's software writes.
pub struct InitializationGuard<'a> {
    owner: &'a mut Engine,
    _instance: startup::InstanceLease,
    startup: startup::StartupLease,
    generation: &'a provenance::AuthenticatedGeneration,
    state: startup::StartupStateV1,
}

impl InitializationGuard<'_> {
    /// Only the first recorded fresh initialization can create a missing key.
    /// The installer must use exclusive creation and never replace an existing key.
    pub fn may_create_identity(&self) -> Result<bool> {
        Ok(self.state.initializing_identity
            == Some(startup::InitializationIdentityPolicy::CreateIfMissing)
            && !storage::exists(&self.owner._lock.home().join(".ww/identity"))?)
    }

    /// Finish only after all intended software and the preserved identity exist.
    /// Both native and FHS bytes are verified against the signed release again.
    pub fn finish(mut self, complete: &Receipt) -> Result<()> {
        ensure!(
            self.startup.read_state()?.as_ref() == Some(&self.state),
            "initialization intent changed"
        );
        let incomplete = Receipt::decode(
            self.state
                .initializing_receipt
                .as_deref()
                .context("initialization receipt missing")?,
        )?;
        let mut expected = incomplete;
        expected.complete = true;
        expected.previous = None;
        ensure!(
            complete.complete && complete.encode()? == expected.encode()?,
            "completed receipt differs from initialization intent"
        );
        let current_trust =
            provenance::IndependentTrustSource::read_bound(&self.state.trusted_release_keys)?;
        let proof = self
            .state
            .native
            .proof
            .verify(self.generation.proof_bytes())?;
        let release = proof.authenticate(&crate::release::auth::TrustedKeys::from_bytes(
            current_trust.keyring(),
        )?)?;
        let selected = release.select(
            &self.state.native.target,
            crate::release::descriptor::COMPATIBILITY_PROFILE,
        )?;
        let native_blake3 =
            storage::verify(&self.owner.paths.executable, &self.state.native.artifact)?;
        ensure!(
            native_blake3 == expected.executable_digest(),
            "installed native bytes differ from initialization receipt"
        );
        let root = self.owner._lock.home().join(".ww");
        for relative in ["fhs", "fhs/boot", "fhs/bin"] {
            storage::directory(&root.join(relative))?;
        }
        for (artifact, slot) in [
            (&selected.deployment().kernel, 1),
            (&selected.deployment().status, 2),
        ] {
            let identity = ArtifactIdentity {
                algorithm: DigestAlgorithm::Sha256,
                digest: hex::encode(artifact.sha256),
                size: artifact.size,
            };
            let digest = storage::verify(&root.join(&artifact.path), &identity)?;
            ensure!(
                digest == expected.hashes[slot],
                "installed FHS bytes differ from initialization receipt"
            );
        }
        let identity_file = storage::open_read(&root.join("identity"))
            .context("preserved installation identity missing")?;
        use std::os::unix::fs::MetadataExt;
        ensure!(
            identity_file.metadata()?.mode() & 0o177 == 0,
            "installation identity must remain private"
        );
        ensure!(
            same_generation_identity(&self.state.native, self.generation.record()),
            "initialization generation changed"
        );
        let retained_proof =
            storage::read_proof(&self.owner.paths.proof(&self.state.native.proof)?)?;
        self.state.native.proof.verify(&retained_proof)?;
        let locator =
            provenance::InstalledRelease::from_root(self.state.native.transport_root.parse()?)?;
        let locator_bytes = serde_json::to_vec(&locator)?;
        if let Some(existing) = storage::read_private_record(&self.owner.paths.locator)? {
            let old: provenance::InstalledRelease = serde_json::from_slice(&existing)?;
            old.root()?;
            ensure!(
                existing == locator_bytes
                    || self
                        .state
                        .initializing_previous_proofs
                        .iter()
                        .any(|p| p.root == old.release),
                "installed locator changed outside initialization"
            );
        }
        let retained = RetainedV2 {
            version: 2,
            installation_id: self.state.installation_id,
            current: self.state.native.clone(),
            previous: None,
            contract: None,
        };
        if let Some(existing) = storage::read_private_record(&self.owner.paths.retained)? {
            let old = RetainedV2::decode(&existing)?;
            ensure!(
                old.installation_id == self.state.installation_id
                    && (old == retained
                        || self
                            .state
                            .initializing_previous_proofs
                            .iter()
                            .any(|p| p == &old.current.proof)),
                "native retention changed outside initialization"
            );
        }
        storage::write_record(&self.owner.paths.locator, &locator_bytes)?;
        storage::write_record(&self.owner.paths.retained, &retained.encode()?)?;
        if self.startup.read_contract()?.is_some() {
            // Reinstall deliberately returns to unbound state. The lease has
            // checked this contract's installation ID against the stable marker.
            ensure!(
                !self.state.initializing_previous_proofs.is_empty(),
                "fresh initialization found an unrelated contract"
            );
            storage::remove(&self.owner.paths.store.join("startup-contract.json"))?;
        }
        storage::write_record(&self.owner.paths.receipt, &expected.encode()?)?;
        validate_committed_native(&self.owner.paths, &self.state, &retained)?;
        for reference in &self.state.initializing_previous_proofs {
            if reference.root != self.state.native.proof.root {
                let path = self.owner.paths.proof(reference)?;
                if storage::exists(&path)? {
                    reference.verify(&storage::read_proof(&path)?)?;
                    storage::remove(&path)?;
                }
            }
        }
        self.state.initializing_receipt = None;
        self.state.initializing_receipt_sha256 = None;
        self.state.initializing_identity = None;
        self.state.initializing_previous_proofs.clear();
        self.startup.write_state(&self.state)?;
        // Keep the complete receipt fence last among installation tuple writes;
        // the final state update only removes already completed recovery intent.
        Ok(())
    }
}

impl Engine {
    /// Read the original receipt intent without acquiring or creating startup
    /// leases. This is a retry hint, not installation authority: callers must
    /// still pass the exact release, trust source and receipt to initialization.
    pub fn initialization_receipt(&self) -> Result<Option<Receipt>> {
        self._lock.reject_pending()?;
        let Some(bytes) =
            storage::read_private_record(&self.paths.store.join("startup-state.json"))?
        else {
            return Ok(None);
        };
        let state = startup::StartupStateV1::decode(&bytes)?;
        state
            .initializing_receipt
            .as_deref()
            .map(Receipt::decode)
            .transpose()
    }

    pub fn prepare_initialization<'a>(
        &'a mut self,
        generation: &'a provenance::AuthenticatedGeneration,
        incomplete: &Receipt,
        trust: &provenance::IndependentTrustSource,
    ) -> Result<InitializationGuard<'a>> {
        use sha2::Digest;
        use startup::{InitializationIdentityPolicy, StartupDisposition, StartupStateV1};
        self._lock.reject_pending()?;
        ensure!(
            !incomplete.complete,
            "initialization requires an incomplete receipt"
        );
        let current_trust = provenance::IndependentTrustSource::read_bound(trust.path())?;
        let verified = provenance::AuthenticatedGeneration::from_proof(
            crate::release::descriptor::SignedReleaseProof::decode(generation.proof_bytes())?,
            current_trust.keyring(),
            &generation.record().target,
        )?;
        ensure!(
            same_generation_identity(verified.record(), generation.record()),
            "current independent trust changed since release selection"
        );
        let existing_receipt = storage::read_private_record(&self.paths.receipt)?;
        let instance = startup::InstanceLease::exclusive_create(&self._lock)?;
        let mut lease = startup::StartupLease::exclusive_create(&self._lock)?;
        let previous = lease.read_state()?;
        let receipt_bytes = incomplete.encode()?;
        let state = match previous {
            Some(state) if state.initializing_receipt.is_some() => {
                ensure!(same_generation_identity(&state.native, generation.record()) && state.trusted_release_keys == trust.path()
                    && state.initializing_receipt.as_ref() == Some(&receipt_bytes),
                    "interrupted initialization requires its exact authenticated release, trust source and receipt");
                state
            }
            previous => {
                let (installation_id, policy, old_proofs) = match previous {
                    Some(state) => {
                        ensure!(state.disposition == StartupDisposition::Uninstalled,
                            "guarded installation is already initialized; preserve admission and native provenance");
                        ensure!(
                            state.trusted_release_keys == trust.path(),
                            "reinstall must preserve the bound release trust path; rebind through explicit startup admission"
                        );
                        ensure!(
                            lease.marker()?.installation_id == state.installation_id,
                            "reinstall marker changed"
                        );
                        ensure!(
                            storage::exists(&self._lock.home().join(".ww/identity"))?,
                            "reinstall cannot replace a missing preserved identity"
                        );
                        let mut proofs = vec![state.native.proof.clone()];
                        if let Some(bytes) = storage::read_private_record(&self.paths.retained)? {
                            let retained = RetainedV2::decode(&bytes)?;
                            ensure!(
                                retained.installation_id == state.installation_id
                                    && retained.current == state.native,
                                "uninstalled retention changed"
                            );
                            if let Some(previous) = retained.previous {
                                proofs.push(previous.proof);
                            }
                        }
                        (
                            state.installation_id,
                            InitializationIdentityPolicy::PreserveExisting,
                            proofs,
                        )
                    }
                    None => {
                        ensure!(lease.marker_is_empty()? && existing_receipt.is_none(), "pre-existing installation cannot acquire guarded upgrade authority implicitly");
                        ensure!(
                            !storage::exists(&self.paths.locator)?
                                && !storage::exists(&self.paths.retained)?,
                            "fresh initialization found unrelated native provenance"
                        );
                        let policy = if storage::exists(&self._lock.home().join(".ww/identity"))? {
                            InitializationIdentityPolicy::PreserveExisting
                        } else {
                            InitializationIdentityPolicy::CreateIfMissing
                        };
                        (rand::random(), policy, Vec::new())
                    }
                };
                StartupStateV1 {
                    version: 1,
                    installation_id,
                    native: generation.record().clone(),
                    trusted_release_keys: trust.path().to_owned(),
                    disposition: StartupDisposition::RequiredUnbound,
                    initializing_receipt: Some(receipt_bytes.clone()),
                    initializing_receipt_sha256: Some(sha2::Sha256::digest(&receipt_bytes).into()),
                    initializing_identity: Some(policy),
                    initializing_previous_proofs: old_proofs,
                }
            }
        };
        let mut completed = incomplete.clone();
        completed.complete = true;
        completed.previous = None;
        if let Some(actual) = &existing_receipt {
            ensure!(
                actual == &receipt_bytes || actual == &completed.encode()?,
                "installation receipt differs from recorded initialization intent"
            );
        }
        lease.write_state(&state)?;
        if existing_receipt.as_deref() != Some(completed.encode()?.as_slice()) {
            storage::write_record(&self.paths.receipt, &receipt_bytes)?;
        }
        match lease.marker() {
            Ok(marker) => ensure!(
                marker.installation_id == state.installation_id,
                "initialization marker changed"
            ),
            Err(_) => {
                lease.initialize_marker(&state, generation)?;
            }
        }
        self.paths.create_proof_directory()?;
        storage::write_proof(
            &self.paths.proof(&state.native.proof)?,
            generation.proof_bytes(),
        )?;
        Ok(InitializationGuard {
            owner: self,
            _instance: instance,
            startup: lease,
            generation,
            state,
        })
    }

    /// Exact software repetition preserves the original native root and contract,
    /// even if the requested signed release carries identical bytes at a new root.
    pub fn verify_complete_installation(
        &self,
        generation: &provenance::AuthenticatedGeneration,
        proposed: &Receipt,
    ) -> Result<bool> {
        self._lock.reject_pending()?;
        let lease = startup::StartupLease::shared(self._lock.home())?;
        let state = lease
            .read_state()?
            .context("required startup state is missing")?;
        ensure!(
            lease.marker()?.installation_id == state.installation_id,
            "startup marker changed"
        );
        if state.initializing_receipt.is_some() {
            return Ok(false);
        }
        ensure!(
            matches!(
                state.disposition,
                startup::StartupDisposition::RequiredUnbound
                    | startup::StartupDisposition::Committed { .. }
            ),
            "startup state does not describe a complete installation"
        );
        let retained = read_retained_v2(&self.paths)?;
        let receipt = validate_committed_native(&self.paths, &state, &retained)?;
        if let Some(contract) = lease.read_contract()? {
            state.validate_contract(&contract)?;
            ensure!(
                retained.contract.as_ref() == Some(&contract.identity),
                "retained startup contract changed"
            );
        } else {
            ensure!(
                state.disposition == startup::StartupDisposition::RequiredUnbound
                    && retained.contract.is_none(),
                "required startup contract is missing"
            );
        }
        ensure!(
            generation.artifact() == &state.native.artifact
                && proposed.hashes[..3] == receipt.hashes[..3],
            "installation already contains different owned software"
        );
        verify_fhs_receipt(self._lock.home(), &receipt)?;
        Ok(true)
    }
}

fn verify_fhs_receipt(home: &Path, receipt: &Receipt) -> Result<()> {
    for (relative, slot) in [("fhs/boot/kernel.wasm", 1), ("fhs/bin/status.wasm", 2)] {
        let path = home.join(".ww").join(relative);
        let file = storage::open_read(&path)?;
        let size = file.metadata()?.len();
        ensure!(
            size > 0 && size <= 256 * 1024 * 1024,
            "installed FHS artifact exceeds bound"
        );
        storage::verify(
            &path,
            &ArtifactIdentity {
                algorithm: DigestAlgorithm::Blake3,
                digest: receipt.hashes[slot].clone(),
                size,
            },
        )?;
    }
    Ok(())
}

// Keyring contents are verification evidence, not release identity. Current
// independent key rotation can authenticate the same signed root/proof again.
fn same_generation_identity(
    a: &provenance::GenerationRecord,
    b: &provenance::GenerationRecord,
) -> bool {
    let mut comparable = a.clone();
    comparable.verification_keys = b.verification_keys.clone();
    comparable == *b
}

impl Engine {
    pub fn startup_state(&self) -> Result<startup::StartupStateV1> {
        self._lock.reject_pending()?;
        let lease = startup::StartupLease::shared(self._lock.home())?;
        let state = lease
            .read_state()?
            .context("required startup state is missing")?;
        ensure!(
            lease.marker()?.installation_id == state.installation_id,
            "startup marker identifies another installation"
        );
        ensure!(
            state.initializing_receipt.is_none(),
            "guarded installation initialization requires recovery"
        );
        ensure!(
            matches!(
                state.disposition,
                startup::StartupDisposition::RequiredUnbound
                    | startup::StartupDisposition::Committed { .. }
            ),
            "startup state is blocked or uninstalled"
        );
        validate_committed_native(&self.paths, &state, &read_retained_v2(&self.paths)?)?;
        Ok(state)
    }

    pub fn startup_contract(&self) -> Result<Option<startup::StartupContractV1>> {
        let state = self.startup_state()?;
        let lease = startup::StartupLease::shared(self._lock.home())?;
        let contract = lease.read_contract()?;
        let retained = read_retained_v2(&self.paths)?;
        match &contract {
            Some(contract) => {
                state.validate_contract(contract)?;
                ensure!(
                    retained.contract.as_ref() == Some(&contract.identity),
                    "retention references another contract"
                );
            }
            None => ensure!(
                state.disposition == startup::StartupDisposition::RequiredUnbound
                    && retained.contract.is_none(),
                "required startup contract is missing"
            ),
        }
        Ok(contract)
    }

    pub fn current_generation(&self) -> Result<provenance::AuthenticatedGeneration> {
        let state = self.startup_state()?;
        let retained = read_retained_v2(&self.paths)?;
        if state.disposition == startup::StartupDisposition::RequiredUnbound {
            let trust =
                provenance::IndependentTrustSource::read_bound(&state.trusted_release_keys)?;
            let proof = state.native.proof.verify(&storage::read_proof(
                &self.paths.proof(&state.native.proof)?,
            )?)?;
            let generation = provenance::AuthenticatedGeneration::from_proof(
                proof,
                trust.keyring(),
                &state.native.target,
            )?;
            ensure!(
                same_generation_identity(generation.record(), &state.native),
                "current independent trust authenticates another installed generation"
            );
            Ok(generation)
        } else {
            authenticate_retained(&self.paths, &retained, &state.native)
        }
    }
}

#[derive(Clone, Debug, Serialize)]
#[serde(tag = "kind", rename_all = "kebab-case")]
#[allow(clippy::large_enum_variant)] // Owned bounded contract is the public checked result.
pub enum AdmissionOutcome {
    Checked {
        contract: startup::StartupContractV1,
    },
    Admitted {
        contract: startup::ContractIdentity,
    },
    Recovered {
        contract: Option<startup::ContractIdentity>,
    },
}

/// This operation shares the existing native journal path and mutation owner.
/// Admission never grants a transaction permit or changes executable bytes.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "operation", rename_all = "kebab-case", deny_unknown_fields)]
enum JournalV2 {
    Admit(AdmissionJournal),
    Upgrade(transaction::NativeJournal),
    Rollback(transaction::NativeJournal),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
enum AdmissionPhase {
    Prepared,
    Committed,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct AdmissionJournal {
    version: u32,
    transaction_id: [u8; 32],
    phase: AdmissionPhase,
    service: ServiceSnapshot,
    state_before: startup::StartupStateV1,
    state_after: startup::StartupStateV1,
    contract_before: Option<Vec<u8>>,
    contract_after: Vec<u8>,
    retained_before: Vec<u8>,
    retained_after: Vec<u8>,
    receipt: Vec<u8>,
    locator: Vec<u8>,
}

impl AdmissionJournal {
    fn validate(&self) -> Result<()> {
        use startup::StartupDisposition;
        ensure!(
            self.version == 2 && self.transaction_id != [0; 32],
            "invalid admission journal version or transaction ID"
        );
        self.state_before.validate()?;
        self.state_after.validate()?;
        ensure!(
            self.state_before.initializing_receipt.is_none()
                && self.state_after.initializing_receipt.is_none(),
            "initialization cannot enter admission recovery"
        );
        ensure!(
            self.state_before.installation_id == self.state_after.installation_id
                && self.state_before.native == self.state_after.native,
            "admission journal changes native generation"
        );
        let after = startup::StartupContractV1::decode(&self.contract_after)?;
        self.state_after.validate_contract(&after)?;
        ensure!(
            matches!(
                self.state_after.disposition,
                StartupDisposition::Committed { .. }
            ),
            "admission target must be steady committed state"
        );
        let before = self
            .contract_before
            .as_deref()
            .map(startup::StartupContractV1::decode)
            .transpose()?;
        match &before {
            Some(before) => {
                self.state_before.validate_contract(before)?;
                ensure!(
                    after.identity.generation == before.identity.next_generation()?,
                    "admission contract generation must advance once"
                );
            }
            None => ensure!(
                self.state_before.disposition == StartupDisposition::RequiredUnbound
                    && after.identity.generation == 1,
                "first admission requires unbound state and generation one"
            ),
        }
        let mut retained = RetainedV2::decode(&self.retained_before)?;
        ensure!(
            retained.current == self.state_before.native
                && retained.installation_id == self.state_before.installation_id
                && retained.contract == before.map(|c| c.identity),
            "admission before retention disagrees with startup state"
        );
        retained.contract = Some(after.identity);
        ensure!(
            retained.encode()? == self.retained_after,
            "admission changes retained native provenance"
        );
        let receipt = Receipt::decode(&self.receipt)?;
        ensure!(
            receipt.complete,
            "admission requires a complete installation"
        );
        let locator =
            provenance::InstalledRelease::from_root(retained.current.transport_root.parse()?)?;
        ensure!(
            serde_json::to_vec(&locator)? == self.locator,
            "admission locator differs from native generation"
        );
        ensure!(
            self.service.running == RunningState::Stopped
                && self.service.enabled != EnabledState::Unknown,
            "admission journal requires known stopped service"
        );
        Ok(())
    }
    fn blocked(&self) -> startup::StartupStateV1 {
        let mut state = self.state_before.clone();
        state.disposition = startup::StartupDisposition::Blocked;
        state
    }
}

fn save_v2(paths: &Paths, journal: &JournalV2) -> Result<()> {
    match journal {
        JournalV2::Admit(j) => j.validate()?,
        JournalV2::Upgrade(j) | JournalV2::Rollback(j) => j.validate()?,
    }
    storage::write_record(&paths.journal, &serde_json::to_vec(journal)?)
}

/// Holds installation and instance exclusion before the caller captures inputs.
/// The startup lease also excludes ordinary launches throughout preparation.
pub struct AdmissionGuard<'a> {
    owner: &'a mut Engine,
    _instance: startup::InstanceLease,
    startup: startup::StartupLease,
    before: ServiceSnapshot,
    state: startup::StartupStateV1,
    contract: Option<startup::StartupContractV1>,
    deadline: Instant,
}

impl AdmissionGuard<'_> {
    pub fn startup_state(&self) -> Result<startup::StartupStateV1> {
        Ok(self.state.clone())
    }
    pub fn startup_contract(&self) -> Result<Option<startup::StartupContractV1>> {
        Ok(self.contract.clone())
    }
    pub fn current_generation(&self) -> Result<provenance::AuthenticatedGeneration> {
        let retained = read_retained_v2(&self.owner.paths)?;
        if self.state.disposition == startup::StartupDisposition::RequiredUnbound {
            current_independent_generation(
                &self.owner.paths,
                &self.state.native,
                &self.state.trusted_release_keys,
            )
        } else {
            authenticate_retained(&self.owner.paths, &retained, &self.state.native)
        }
    }

    pub fn finish(
        self,
        service: &mut impl ServiceControl,
        desired: startup::StartupContractV1,
        inputs: &inputs::PreflightInputs,
        check: bool,
    ) -> Result<AdmissionOutcome> {
        check_deadline(self.deadline)?;
        desired.validate()?;
        inputs.validate()?;
        ensure!(
            desired.installation_id == self.state.installation_id
                && desired.artifact == self.state.native.artifact
                && desired.startup == inputs.startup_identity()?,
            "proposed contract differs from installed executable or captured inputs"
        );
        let expected_generation = self
            .contract
            .as_ref()
            .map(|c| c.identity.next_generation())
            .transpose()?
            .unwrap_or(1);
        ensure!(
            desired.identity.generation == expected_generation,
            "proposed contract generation is not the next generation"
        );
        ensure!(
            inputs.config.home == self.owner._lock.home() && !inputs.config.insecure_ephemeral,
            "admission requires this installation and its persistent identity"
        );
        if let startup::InvocationBinding::Managed { service: identity } = &desired.invocation {
            ensure!(
                identity.registered
                    && self.before.registered
                    && identity.enabled == self.before.enabled,
                "managed admission differs from stopped service registration"
            );
        }
        // New admission and explicit trust-path changes always use current,
        // independent trust. Retained historical keys cannot grant a new contract.
        current_independent_generation(
            &self.owner.paths,
            &self.state.native,
            &desired.trusted_release_keys,
        )?;
        let executable = attestation::VerifiedExecutable::open(
            &self.owner.paths.executable,
            &self.state.native.artifact,
            self.deadline,
        )?;
        ensure!(
            service.snapshot(self.deadline)? == self.before,
            "managed service changed during input capture"
        );
        let retained_before = storage::read_private_record(&self.owner.paths.retained)?
            .context("native retention missing")?;
        let mut retained = RetainedV2::decode(&retained_before)?;
        validate_committed_native(&self.owner.paths, &self.state, &retained)?;
        let mut state_after = self.state.clone();
        state_after.trusted_release_keys = desired.trusted_release_keys.clone();
        state_after.disposition = startup::StartupDisposition::Committed {
            contract: desired.identity.clone(),
        };
        retained.contract = Some(desired.identity.clone());
        let mut journal = AdmissionJournal {
            version: 2,
            transaction_id: rand::random(),
            phase: AdmissionPhase::Prepared,
            service: self.before.clone(),
            state_before: self.state.clone(),
            state_after,
            contract_before: self
                .contract
                .as_ref()
                .map(startup::StartupContractV1::encode)
                .transpose()?,
            contract_after: desired.encode()?,
            retained_before,
            retained_after: retained.encode()?,
            receipt: storage::read_private_record(&self.owner.paths.receipt)?
                .context("installation receipt missing")?,
            locator: storage::read_private_record(&self.owner.paths.locator)?
                .context("installed release locator missing")?,
        };
        journal.validate()?;
        if !check {
            save_v2(&self.owner.paths, &JournalV2::Admit(journal.clone()))?;
            checkpoint("admit.journal.prepared")?;
            self.startup.write_state(&journal.blocked())?;
            checkpoint("admit.blocked")?;
        }
        let evidence = service.compatible(&executable, inputs, self.deadline)?;
        evidence.verify(&executable, inputs)?;
        executable.revalidate(self.deadline)?;
        ensure!(
            service.snapshot(self.deadline)? == self.before,
            "managed service changed during preflight"
        );
        current_independent_generation(
            &self.owner.paths,
            &self.state.native,
            &desired.trusted_release_keys,
        )?;
        check_deadline(self.deadline)?;
        if check {
            return Ok(AdmissionOutcome::Checked { contract: desired });
        }
        ensure!(
            self.startup.read_state()?.as_ref() == Some(&journal.blocked()),
            "admission block changed"
        );
        self.startup.write_contract(&desired)?;
        checkpoint("admit.contract.after")?;
        storage::write_record(&self.owner.paths.retained, &journal.retained_after)?;
        checkpoint("admit.retained.after")?;
        validate_committed_native(&self.owner.paths, &journal.state_after, &retained)?;
        ensure!(
            self.startup.read_contract()?.as_ref() == Some(&desired),
            "admission contract changed before commit"
        );
        check_deadline(self.deadline)?;
        journal.phase = AdmissionPhase::Committed;
        save_v2(&self.owner.paths, &JournalV2::Admit(journal.clone()))?;
        checkpoint("admit.committed")?;
        self.startup.write_state(&journal.state_after)?;
        checkpoint("admit.steady")?;
        storage::remove(&self.owner.paths.journal)?;
        Ok(AdmissionOutcome::Admitted {
            contract: desired.identity,
        })
    }
}

fn current_independent_generation(
    paths: &Paths,
    record: &provenance::GenerationRecord,
    trust_path: &Path,
) -> Result<provenance::AuthenticatedGeneration> {
    let trust = provenance::IndependentTrustSource::read_bound(trust_path)?;
    let proof = record
        .proof
        .verify(&storage::read_proof(&paths.proof(&record.proof)?)?)?;
    let authenticated =
        provenance::AuthenticatedGeneration::from_proof(proof, trust.keyring(), &record.target)?;
    ensure!(
        same_generation_identity(authenticated.record(), record),
        "current independent trust identifies another native generation"
    );
    Ok(authenticated)
}

impl Engine {
    pub fn begin_admission(
        &mut self,
        service: &mut impl ServiceControl,
        deadline: Instant,
    ) -> Result<AdmissionGuard<'_>> {
        check_deadline(deadline)?;
        self._lock.reject_pending()?;
        let before = service.snapshot(deadline)?;
        validate_snapshot(&before, &self.paths.executable)?;
        ensure!(
            before.running == RunningState::Stopped,
            "stop the managed service before startup admission"
        );
        let instance = startup::InstanceLease::exclusive(self._lock.home())?;
        let lease = startup::StartupLease::exclusive(self._lock.home())?;
        let state = validate_owned_installation(&self._lock, &lease)?;
        ensure!(
            matches!(
                state.disposition,
                startup::StartupDisposition::RequiredUnbound
                    | startup::StartupDisposition::Committed { .. }
            ),
            "startup admission requires an installed steady generation"
        );
        let contract = lease.read_contract()?;
        ensure!(
            service.snapshot(deadline)? == before,
            "managed service changed before input capture"
        );
        Ok(AdmissionGuard {
            owner: self,
            _instance: instance,
            startup: lease,
            before,
            state,
            contract,
            deadline,
        })
    }

    pub fn operator_admit(
        &mut self,
        service: &mut impl ServiceControl,
        timeout: Duration,
        desired: startup::StartupContractV1,
        inputs: &inputs::PreflightInputs,
        check: bool,
    ) -> Result<AdmissionOutcome> {
        let deadline = deadline(timeout)?;
        if let Some(outcome) = self.recover_admission(service, deadline, check)? {
            return Ok(outcome);
        }
        self.begin_admission(service, deadline)?
            .finish(service, desired, inputs, check)
    }

    /// Resolve only a recorded admission and return before selecting new inputs.
    pub fn recover_admission(
        &mut self,
        service: &mut impl ServiceControl,
        deadline: Instant,
        check: bool,
    ) -> Result<Option<AdmissionOutcome>> {
        check_deadline(deadline)?;
        self._lock.reject_pending_image()?;
        let Some(bytes) = storage::read_private_record(&self.paths.journal)? else {
            return Ok(None);
        };
        let decoded = serde_json::from_slice::<JournalV2>(&bytes).context(
            "pending transaction is not a supported startup admission; use native recovery",
        )?;
        let JournalV2::Admit(journal) = decoded else {
            anyhow::bail!("pending native transaction requires native recovery");
        };
        journal.validate()?;
        ensure!(!check,"pending startup admission requires recovery; rerun without --check before selecting new inputs");
        let before = service.snapshot(deadline)?;
        validate_snapshot(&before, &self.paths.executable)?;
        ensure!(before == journal.service, "managed service changed during admission recovery; stop and restore its recorded disposition");
        let _instance = startup::InstanceLease::exclusive(self._lock.home())?;
        let lease = startup::StartupLease::exclusive(self._lock.home())?;
        ensure!(
            lease.marker()?.installation_id == journal.state_before.installation_id,
            "admission journal identifies another installation"
        );
        let state = lease.read_state()?.context("startup state missing")?;
        ensure!(
            state == journal.state_before
                || state == journal.blocked()
                || state == journal.state_after,
            "startup state is not owned by the pending admission"
        );
        ensure!(
            storage::read_private_record(&self.paths.receipt)?.as_ref() == Some(&journal.receipt)
                && storage::read_private_record(&self.paths.locator)?.as_ref()
                    == Some(&journal.locator),
            "native receipt or locator changed during admission"
        );
        let actual_retained =
            storage::read_private_record(&self.paths.retained)?.context("retention missing")?;
        ensure!(
            actual_retained == journal.retained_before || actual_retained == journal.retained_after,
            "retention is not owned by pending admission"
        );
        let actual_contract = lease.read_contract()?.map(|c| c.encode()).transpose()?;
        ensure!(
            actual_contract == journal.contract_before
                || actual_contract.as_ref() == Some(&journal.contract_after),
            "contract is not owned by pending admission"
        );
        validate_committed_native(
            &self.paths,
            &journal.state_before,
            &RetainedV2::decode(&journal.retained_before)?,
        )?;
        lease.write_state(&journal.blocked())?;
        check_deadline(deadline)?;
        let (target_state, target_contract, target_retained) =
            if journal.phase == AdmissionPhase::Committed {
                (
                    &journal.state_after,
                    Some(journal.contract_after.as_slice()),
                    &journal.retained_after,
                )
            } else {
                (
                    &journal.state_before,
                    journal.contract_before.as_deref(),
                    &journal.retained_before,
                )
            };
        match target_contract {
            Some(bytes) => lease.write_contract(&startup::StartupContractV1::decode(bytes)?)?,
            None => storage::remove(&self.paths.store.join("startup-contract.json"))?,
        }
        storage::write_record(&self.paths.retained, target_retained)?;
        validate_committed_native(
            &self.paths,
            target_state,
            &RetainedV2::decode(target_retained)?,
        )?;
        lease.write_state(target_state)?;
        storage::remove(&self.paths.journal)?;
        let contract = target_contract
            .map(startup::StartupContractV1::decode)
            .transpose()?
            .map(|c| c.identity);
        Ok(Some(AdmissionOutcome::Recovered { contract }))
    }
}

/// Shared validation for software lifecycle operations under the original
/// mutation owner. An uninstalled tombstone permits only deletion/reinstallation.
pub fn validate_owned_installation(
    lock: &MutationLock,
    lease: &startup::StartupLease,
) -> Result<startup::StartupStateV1> {
    lock.reject_pending()?;
    ensure!(
        lock.home() == lease.home(),
        "startup lease belongs to another installation"
    );
    let paths = Paths::existing(lock.home())?;
    let state = lease
        .read_state()?
        .context("required startup state missing")?;
    ensure!(
        lease.marker()?.installation_id == state.installation_id
            && state.initializing_receipt.is_none(),
        "guarded installation is incomplete or has a different marker"
    );
    ensure!(
        matches!(
            state.disposition,
            startup::StartupDisposition::RequiredUnbound
                | startup::StartupDisposition::Committed { .. }
                | startup::StartupDisposition::Blocked
                | startup::StartupDisposition::Uninstalled
        ),
        "transaction startup state requires native recovery"
    );
    let retained = read_retained_v2(&paths)?;
    ensure!(
        retained.installation_id == state.installation_id && retained.current == state.native,
        "retained native generation changed"
    );
    authenticate_retained(&paths, &retained, &state.native)?;
    if let Some(previous) = &retained.previous {
        authenticate_retained(&paths, &retained, previous)?;
    }
    let contract = lease.read_contract()?;
    match &contract {
        Some(contract) => {
            ensure!(
                retained.contract.as_ref() == Some(&contract.identity)
                    && state.disposition != startup::StartupDisposition::RequiredUnbound
                    && contract.installation_id == state.installation_id
                    && contract.artifact == state.native.artifact
                    && contract.trusted_release_keys == state.trusted_release_keys,
                "retained startup contract changed"
            );
            if matches!(
                state.disposition,
                startup::StartupDisposition::Committed { .. }
            ) {
                state.validate_contract(contract)?;
            }
        }
        None => ensure!(
            retained.contract.is_none()
                && !matches!(
                    state.disposition,
                    startup::StartupDisposition::Committed { .. }
                ),
            "required startup contract missing"
        ),
    }
    if state.disposition == startup::StartupDisposition::Uninstalled {
        let current_receipt = storage::read_private_record(&paths.receipt)?;
        let tombstone =
            storage::read_private_record(&lock.home().join(".ww/uninstalled-software.json"))?;
        if let (Some(current), Some(tombstone)) = (&current_receipt, &tombstone) {
            ensure!(
                current == tombstone,
                "removal receipt and tombstone disagree"
            );
        }
        let receipt = current_receipt
            .or(tombstone)
            .context("uninstalled ownership receipt missing")?;
        ensure!(
            Receipt::decode(&receipt)?.complete && Receipt::decode(&receipt)?.encode()? == receipt,
            "uninstalled receipt is incomplete or noncanonical"
        );
        if storage::exists(&paths.executable)? {
            ensure!(
                storage::verify(&paths.executable, &state.native.artifact)?
                    == Receipt::decode(&receipt)?.executable_digest(),
                "remaining executable differs from removal ownership"
            );
        }
        if let Some(locator) = storage::read_private_record(&paths.locator)? {
            ensure!(
                locator
                    == serde_json::to_vec(&provenance::InstalledRelease::from_root(
                        state.native.transport_root.parse()?
                    )?)?,
                "remaining release locator differs from removal ownership"
            );
        }
    } else {
        validate_committed_native(&paths, &state, &retained)?;
    }
    Ok(state)
}

/// A transaction launch capability constructed only after validating the owner
/// journal, installed tuple and independent trust under the startup lease.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct StartupPermit {
    payload: startup::StartupPermitPayload,
    contract: startup::StartupContractV1,
}
impl StartupPermit {
    pub fn payload(&self) -> &startup::StartupPermitPayload {
        &self.payload
    }
    pub fn contract(&self) -> &startup::StartupContractV1 {
        &self.contract
    }
}
