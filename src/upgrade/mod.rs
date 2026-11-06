//! Explicit, recoverable native Host replacement.
//!
//! The production command stays disabled. Only the authenticated release
//! selector may eventually construct [`PreverifiedCandidate`]; the constructor
//! presently exists only in this module's tests. A checksum or local receipt is
//! not upgrade authorization.

use anyhow::{ensure, Context, Result};
use serde::{Deserialize, Serialize};
use std::io::Read;
use std::path::Path;
use std::time::{Duration, Instant};

use crate::installation_state::{MutationLock, Receipt};
use service::{EnabledState, RestartAdmission, RunningState, ServiceControl, ServiceSnapshot};
use storage::{checkpoint, Paths};

pub mod service;
mod storage;

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
/// No public constructor or Deserialize implementation exists. Integrating #738
/// must add the single conversion from its authenticated platform selection;
/// it must not expose a constructor from URLs, checksums, or arbitrary metadata.
#[derive(Clone, Debug)]
pub struct PreverifiedCandidate {
    artifact: ArtifactIdentity,
    release: String,
    version: String,
    revision: String,
    platform: String,
}

/// Both selections are authenticated. The legacy receipt authenticates neither.
pub struct UpgradeRequest<R: Read> {
    pub installed: PreverifiedCandidate,
    pub candidate: PreverifiedCandidate,
    pub bytes: R,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Outcome {
    AlreadyInstalled,
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

#[derive(Clone, Debug, PartialEq, Eq)]
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
    admission: Option<RestartAdmission>,
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
        let lock = MutationLock::acquire(home)?;
        let paths = Paths::open(home)?;
        Ok(Self { _lock: lock, paths })
    }

    /// Resolve a pending transaction before calling the release selector.
    ///
    /// The deadline governs admission to executable replacement, serving
    /// readiness, and receipt commit. The caller must separately bound download
    /// I/O: this synchronous owner cannot cancel an arbitrary blocking `Read` or
    /// in-flight filesystem operation. Once a commit is admitted, required
    /// durability work and terminal cleanup complete even if the budget expires.
    pub fn operator_upgrade<R: Read>(
        &mut self,
        service: &mut impl ServiceControl,
        timeout: Duration,
        select: impl FnOnce() -> Result<UpgradeRequest<R>>,
    ) -> Result<Outcome> {
        let deadline = deadline(timeout)?;
        if storage::exists(&self.paths.journal)? {
            let result = self.recover_until(service, deadline);
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
    pub fn operator_rollback(
        &mut self,
        service: &mut impl ServiceControl,
        timeout: Duration,
    ) -> Result<Outcome> {
        let deadline = deadline(timeout)?;
        if storage::exists(&self.paths.journal)? {
            let result = self.recover_until(service, deadline);
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
    pub fn recover(
        &mut self,
        service: &mut impl ServiceControl,
        timeout: Duration,
    ) -> Result<Outcome> {
        let result = self.recover_until(service, deadline(timeout)?);
        self.with_recovery_context(result)
    }

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
            .snapshot()
            .context("observe managed service before replacement")?;
        validate_snapshot(&before, &self.paths.executable)?;
        check_deadline(deadline)?;
        let admission = if matches!(before.running, RunningState::Running { .. }) {
            let admitted = service.admit_restart(&before).context("running startup inputs are unavailable or pending; explicitly stop the managed service before upgrading")?;
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
        service.compatible(&journal.candidate.artifact, &self.paths.artifact(&journal.candidate.artifact))
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
            service.snapshot()? == journal.before,
            "managed service state changed before replacement; recover the pending transaction"
        );
        self.readmit(service, &journal)?;
        service.compatible(
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
            ensure!(service.snapshot()? == journal.before, "service state changed during stopped-service upgrade; installed candidate retained, recovery required");
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

    fn readmit(&self, service: &mut impl ServiceControl, journal: &Journal) -> Result<()> {
        if let Some(expected) = &journal.admission {
            service.revalidate_restart(expected).context(
                "startup inputs or publisher target changed; explicitly stop and activate separately")?;
        }
        Ok(())
    }

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
        let result = service.restart(admission, expected, deadline);
        checkpoint("service.restart.after")?;
        let verified = result.and_then(|evidence| {
            check_deadline(deadline)?;
            evidence.verify(admission, expected)?;
            storage::verify(&self.paths.executable, expected)?;
            let after = service.snapshot()?;
            validate_snapshot(&after, &self.paths.executable)?;
            same_configuration(&journal.before, &after)?;
            ensure!(
                after.running == (RunningState::Running { pid: evidence.pid }),
                "service manager PID differs from the verified running Host"
            );
            service
                .compatible(expected, &self.paths.artifact(expected))
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

    fn stop_and_confirm(&self, service: &mut impl ServiceControl, deadline: Instant) -> Result<()> {
        checkpoint("service.stop.before")?;
        service.stop(deadline)?;
        checkpoint("service.stop.after")?;
        check_deadline(deadline)?;
        ensure!(
            service.snapshot()?.running == RunningState::Stopped,
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

    fn recover_until(
        &mut self,
        service: &mut impl ServiceControl,
        deadline: Instant,
    ) -> Result<Outcome> {
        let Some(bytes) = storage::read_record(&self.paths.journal)? else {
            return Ok(Outcome::NoPendingTransaction);
        };
        let mut journal: Journal = serde_json::from_slice(&bytes).context(
            "invalid Host transaction journal; preserve recovery files for manual inspection",
        )?;
        self.validate_journal(&journal)?;
        self.validate_receipt_bindings(&journal)?;
        self.expect_receipt(&journal, true)?;
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
            .snapshot()
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
        service.compatible(&journal.previous.artifact, &self.paths.artifact(&journal.previous.artifact)).context("previous Host is incompatible with the current deployment; service remains stopped, recovery pending")?;
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
            original.version == 1 && original.complete,
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

    fn set_phase(&self, journal: &mut Journal, phase: Phase) -> Result<()> {
        checkpoint(&format!("phase.{phase:?}.before"))?;
        journal.phase = phase;
        self.save(journal)?;
        checkpoint(&format!("phase.{phase:?}.after"))?;
        Ok(())
    }

    fn save(&self, journal: &Journal) -> Result<()> {
        storage::write_record(&self.paths.journal, &serde_json::to_vec(journal)?)
    }

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
    Instant::now()
        .checked_add(timeout)
        .context("Host operation timeout is out of range")
}

fn check_deadline(deadline: Instant) -> Result<()> {
    let now = Instant::now();
    #[cfg(test)]
    let now = storage::faults::clock_now(now);
    ensure!(now < deadline, "Host operation/readiness deadline expired");
    Ok(())
}

#[cfg(test)]
mod tests;
