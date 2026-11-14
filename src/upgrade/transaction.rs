//! One durable native transaction. Ordinary Host startup does not consult it.
use super::*;
use provenance::{AuthenticatedGeneration, GenerationRecord, InstalledRelease};
use std::path::PathBuf;

pub struct UpgradeRequest<R: Read> {
    pub installed: AuthenticatedGeneration,
    pub candidate: AuthenticatedGeneration,
    pub bytes: R,
}

#[derive(Clone, Debug, Serialize)]
pub struct RepairOutcome {
    pub archive: PathBuf,
    pub historical_rollback_available: bool,
    pub live_ready: bool,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct Retention {
    version: u32,
    current: GenerationRecord,
    previous: Option<GenerationRecord>,
}
impl Retention {
    fn validate(&self) -> Result<()> {
        ensure!(
            self.version == 3,
            "unsupported native retention; explicitly repair the authenticated baseline"
        );
        self.current.validate()?;
        if let Some(previous) = &self.previous {
            previous.validate()?;
            ensure!(
                previous.target == self.current.target
                    && previous.artifact != self.current.artifact,
                "invalid previous native generation"
            );
        }
        Ok(())
    }
    fn encode(&self) -> Result<Vec<u8>> {
        self.validate()?;
        Ok(serde_json::to_vec(self)?)
    }
    fn decode(bytes: &[u8]) -> Result<Self> {
        let record: Self = serde_json::from_slice(bytes).context("invalid native retention")?;
        ensure!(record.encode()? == bytes, "noncanonical native retention");
        Ok(record)
    }
}
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct NativeTuple {
    generation: GenerationRecord,
    receipt: Vec<u8>,
    locator: Vec<u8>,
}
impl NativeTuple {
    fn validate(&self) -> Result<()> {
        self.generation.validate()?;
        let receipt = Receipt::decode(&self.receipt)?;
        ensure!(
            receipt.complete && receipt.encode()? == self.receipt,
            "native tuple requires a canonical complete installation receipt"
        );
        ensure!(
            self.locator == locator(&self.generation)?,
            "native tuple locator differs from signed generation"
        );
        Ok(())
    }
}
#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
enum NativePhase {
    Staging,
    Ready,
    Replacing,
    Installed,
    Starting,
    Verified,
    Committed,
    Restoring,
    Restored,
    Containing,
}
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct NativeJournal {
    transaction_id: [u8; 32],
    operation: Operation,
    phase: NativePhase,
    before: NativeTuple,
    after: NativeTuple,
    retained_before: Vec<u8>,
    retained_after: Vec<u8>,
    service: ServiceSnapshot,
    stop_intent: bool,
    quiesced: Option<ServiceSnapshot>,
    cleanup_timeout_millis: u64,
}
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct EvidenceRecord {
    name: String,
    digest: ArtifactIdentity,
}
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct BaselineJournal {
    archive: String,
    evidence: Vec<EvidenceRecord>,
    after: NativeTuple,
    retained_after: Vec<u8>,
    historical_rollback_available: bool,
    committed: bool,
}
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(tag = "kind", content = "transaction", rename_all = "kebab-case")]
enum Work {
    Native(Box<NativeJournal>),
    Baseline(Box<BaselineJournal>),
}
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Record {
    version: u32,
    work: Work,
}
impl Record {
    fn save(&self, paths: &Paths) -> Result<()> {
        self.validate()?;
        storage::write_record(&paths.journal, &serde_json::to_vec(self)?)
    }
    fn validate(&self) -> Result<()> {
        ensure!(
            self.version == 3,
            "unknown native journal version; preserve evidence and explicitly repair baseline"
        );
        match &self.work {
            Work::Native(j) => j.validate(),
            Work::Baseline(j) => j.validate(),
        }
    }
}
impl NativeJournal {
    fn validate(&self) -> Result<()> {
        ensure!(
            self.transaction_id != [0; 32] && (1..=30_000).contains(&self.cleanup_timeout_millis),
            "invalid native transaction identity or cleanup bound"
        );
        self.before.validate()?;
        self.after.validate()?;
        ensure!(
            self.before.generation.target == self.after.generation.target
                && self.before.generation.artifact != self.after.generation.artifact,
            "native transaction target is inconsistent"
        );
        let before = Receipt::decode(&self.before.receipt)?;
        let after = Receipt::decode(&self.after.receipt)?;
        ensure!(
            before
                .with_executable_digest(after.executable_digest().into())
                .encode()?
                == self.after.receipt,
            "native transaction alters unrelated receipt fields"
        );
        let old = Retention::decode(&self.retained_before)?;
        let new = Retention::decode(&self.retained_after)?;
        ensure!(
            old.current == self.before.generation
                && new.current == self.after.generation
                && new.previous.as_ref() == Some(&self.before.generation),
            "native retention differs from transaction"
        );
        if self.operation == Operation::Rollback {
            ensure!(
                old.previous.as_ref() == Some(&self.after.generation),
                "rollback must select the exact retained previous generation"
            );
        }
        ensure!(
            self.service.registered,
            "unmanaged native replacement is unsupported"
        );
        if let Some(stopped) = &self.quiesced {
            ensure!(
                self.stop_intent && stopped.running == RunningState::Stopped,
                "native quiescence is not confirmed"
            );
            same_configuration(&self.service, stopped)?;
        }
        if !matches!(self.phase, NativePhase::Staging | NativePhase::Ready) {
            ensure!(
                self.stop_intent && self.quiesced.is_some(),
                "replacement phase lacks confirmed stop"
            );
        }
        Ok(())
    }
    fn save(&self, paths: &Paths) -> Result<()> {
        Record {
            version: 3,
            work: Work::Native(Box::new(self.clone())),
        }
        .save(paths)
    }
    fn phase(&mut self, paths: &Paths, phase: NativePhase) -> Result<()> {
        self.phase = phase;
        self.save(paths)?;
        checkpoint(&format!("native.phase.{phase:?}"))
    }
}
impl BaselineJournal {
    fn validate(&self) -> Result<()> {
        ensure!(
            self.archive.len() == 71
                && self.archive.starts_with("repair-")
                && self.archive[7..]
                    .bytes()
                    .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)),
            "invalid repair evidence archive"
        );
        self.after.validate()?;
        let retained = Retention::decode(&self.retained_after)?;
        ensure!(
            retained.current == self.after.generation
                && retained.previous.is_some() == self.historical_rollback_available,
            "baseline repair retention is inconsistent"
        );
        let mut names = std::collections::BTreeSet::new();
        for entry in &self.evidence {
            ensure!(
                matches!(
                    entry.name.strip_suffix("-pending").unwrap_or(&entry.name),
                    "journal" | "receipt" | "locator" | "retained" | "proof"
                ),
                "unknown repair evidence name"
            );
            ensure!(names.insert(&entry.name), "duplicate repair evidence");
            // Empty damaged records still need preservation; their archive identity is SHA-256.
            ensure!(
                entry.digest.algorithm == DigestAlgorithm::Sha256
                    && entry.digest.size <= storage::MAX_EXECUTABLE
                    && entry.digest.digest.len() == 64
                    && entry
                        .digest
                        .digest
                        .bytes()
                        .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase()),
                "invalid archived evidence identity"
            );
        }
        Ok(())
    }
    fn save(&self, paths: &Paths) -> Result<()> {
        Record {
            version: 3,
            work: Work::Baseline(Box::new(self.clone())),
        }
        .save(paths)
    }
}
fn read_record(paths: &Paths) -> Result<Option<Record>> {
    let Some(bytes) = storage::read_private_record(&paths.journal)? else {
        return Ok(None);
    };
    let record: Record = serde_json::from_slice(&bytes).context("unknown or damaged native journal; preserve evidence and use authenticated baseline repair")?;
    record.validate()?;
    Ok(Some(record))
}
fn locator(generation: &GenerationRecord) -> Result<Vec<u8>> {
    Ok(serde_json::to_vec(&InstalledRelease::from_root(
        generation.transport_root.parse()?,
    )?)?)
}
fn proof_generation(paths: &Paths, record: &GenerationRecord) -> Result<AuthenticatedGeneration> {
    let proof = record
        .proof
        .verify(&storage::read_proof(&paths.proof(&record.proof)?)?)?;
    let generation =
        AuthenticatedGeneration::from_proof(proof, &record.verification_keys, &record.target)?;
    ensure!(
        generation.record() == record,
        "retained signed proof identifies another generation"
    );
    Ok(generation)
}
fn retain_proof(paths: &Paths, generation: &AuthenticatedGeneration) -> Result<()> {
    paths.create_proof_directory()?;
    storage::write_proof(
        &paths.proof(&generation.record().proof)?,
        generation.proof_bytes(),
    )
}
fn verify_tuple(paths: &Paths, tuple: &NativeTuple) -> Result<()> {
    tuple.validate()?;
    proof_generation(paths, &tuple.generation)?;
    ensure!(
        storage::read_private_record(&paths.receipt)?.as_ref() == Some(&tuple.receipt)
            && storage::read_private_record(&paths.locator)?.as_ref() == Some(&tuple.locator),
        "installed native receipt/locator differs from transaction"
    );
    ensure!(
        storage::verify(&paths.executable, &tuple.generation.artifact)?
            == Receipt::decode(&tuple.receipt)?.executable_digest(),
        "installed bytes differ from receipt"
    );
    Ok(())
}
fn write_tuple(paths: &Paths, tuple: &NativeTuple, retained: &[u8]) -> Result<()> {
    storage::write_record(&paths.receipt, &tuple.receipt)?;
    checkpoint("native.receipt.after")?;
    storage::write_record(&paths.locator, &tuple.locator)?;
    checkpoint("native.locator.after")?;
    storage::write_record(&paths.retained, retained)?;
    checkpoint("native.retained.after")?;
    verify_tuple(paths, tuple)
}
fn validate_mixed(paths: &Paths, j: &NativeJournal) -> Result<()> {
    for (path, before, after) in [
        (&paths.receipt, &j.before.receipt, &j.after.receipt),
        (&paths.locator, &j.before.locator, &j.after.locator),
        (&paths.retained, &j.retained_before, &j.retained_after),
    ] {
        let actual = storage::read_private_record(path)?.context("transaction metadata missing")?;
        ensure!(
            &actual == before || &actual == after,
            "transaction metadata changed outside recorded operation"
        );
    }
    proof_generation(paths, &j.before.generation)?;
    Ok(())
}

impl UpgradeCoordinator {
    pub fn current_generation(&self) -> Result<AuthenticatedGeneration> {
        self._lock.reject_pending()?;
        let retained = self.retention()?;
        let tuple = self.current_tuple(&retained.current)?;
        verify_tuple(&self.paths, &tuple)?;
        proof_generation(&self.paths, &retained.current)
    }
    fn retention(&self) -> Result<Retention> {
        Retention::decode(&storage::read_private_record(&self.paths.retained)?.context("native provenance is uninitialized; explicitly reconcile an authenticated baseline")?)
    }
    fn current_tuple(&self, generation: &GenerationRecord) -> Result<NativeTuple> {
        Ok(NativeTuple {
            generation: generation.clone(),
            receipt: storage::read_private_record(&self.paths.receipt)?
                .context("installation receipt missing")?,
            locator: storage::read_private_record(&self.paths.locator)?
                .context("installed release locator missing")?,
        })
    }
    pub fn operator_upgrade<R: Read>(
        &mut self,
        service: &mut impl ServiceControl,
        timeout: Duration,
        select: impl FnOnce() -> Result<UpgradeRequest<R>>,
    ) -> Result<Outcome> {
        self.operator_upgrade_until(service, deadline(timeout)?, select)
    }
    pub fn operator_upgrade_until<R: Read>(
        &mut self,
        service: &mut impl ServiceControl,
        end: Instant,
        select: impl FnOnce() -> Result<UpgradeRequest<R>>,
    ) -> Result<Outcome> {
        check_deadline(end)?;
        self._lock.reject_pending()?;
        let request = select()?;
        let result = self.start_native(service, end, request, Operation::Upgrade);
        self.rollback_failed_upgrade(service, result)
    }
    pub fn check_upgrade_until<R: Read>(
        &mut self,
        service: &mut impl ServiceControl,
        end: Instant,
        request: UpgradeRequest<R>,
    ) -> Result<Outcome> {
        check_deadline(end)?;
        self._lock.reject_pending()?;
        let current = self.current_generation()?;
        ensure!(
            same_generation_identity(current.record(), request.installed.record()),
            "installed authentication differs from committed generation"
        );
        ensure!(
            current.record().target == request.candidate.record().target,
            "native target changed"
        );
        if current.artifact() == request.candidate.artifact() {
            return Ok(Outcome::AlreadyInstalled);
        }
        let before = service.snapshot(end)?;
        self.preflight(service, &before, end)?;
        let temporary = tempfile::tempdir()?;
        storage::stage(
            request.bytes,
            &temporary.path().join("candidate"),
            request.candidate.artifact(),
        )?;
        check_deadline(end)?;
        ensure!(
            service.snapshot(end)? == before,
            "service changed during check"
        );
        Ok(Outcome::Checked)
    }
    pub fn operator_rollback_until(
        &mut self,
        service: &mut impl ServiceControl,
        end: Instant,
    ) -> Result<Outcome> {
        check_deadline(end)?;
        self._lock.reject_pending()?;
        let installed = self.current_generation()?;
        let retention = self.retention()?;
        let previous = retention
            .previous
            .as_ref()
            .context("no authenticated historical rollback is available")?;
        let candidate = proof_generation(&self.paths, previous)?;
        let bytes = storage::open_read(&self.paths.artifact(candidate.artifact()))?;
        let result = self.start_native(
            service,
            end,
            UpgradeRequest {
                installed,
                candidate,
                bytes,
            },
            Operation::Rollback,
        );
        self.contain_failure(service, result)
    }
    fn preflight(
        &self,
        service: &mut impl ServiceControl,
        before: &ServiceSnapshot,
        end: Instant,
    ) -> Result<()> {
        validate_snapshot(before, &self.paths.executable)?;
        ensure!(
            before.registered,
            "unmanaged automatic native replacement is unsupported"
        );
        service.preflight(before, end)?;
        check_deadline(end)
    }
    fn start_native<R: Read>(
        &mut self,
        service: &mut impl ServiceControl,
        end: Instant,
        request: UpgradeRequest<R>,
        operation: Operation,
    ) -> Result<Outcome> {
        check_deadline(end)?;
        self._lock.reject_pending()?;
        let current = self.current_generation()?;
        ensure!(
            same_generation_identity(current.record(), request.installed.record()),
            "installed authentication differs from committed generation"
        );
        ensure!(
            current.record().target == request.candidate.record().target,
            "native target changed"
        );
        if current.artifact() == request.candidate.artifact() {
            return Ok(Outcome::AlreadyInstalled);
        }
        let before = self.current_tuple(current.record())?;
        let retained_before =
            storage::read_private_record(&self.paths.retained)?.context("retention missing")?;
        if operation == Operation::Rollback {
            ensure!(
                Retention::decode(&retained_before)?.previous.as_ref()
                    == Some(request.candidate.record()),
                "rollback target differs from committed previous generation"
            );
        }
        let snapshot = service.snapshot(end)?;
        self.preflight(service, &snapshot, end)?;
        // Stream and verify the candidate; no candidate buffering or execution.
        let stage = self.paths.store.join("stage");
        let digest = storage::stage(request.bytes, &stage, request.candidate.artifact())?;
        check_deadline(end)?;
        storage::rename(&stage, &self.paths.artifact(request.candidate.artifact()))?;
        retain_proof(&self.paths, &request.candidate)?;
        let after = NativeTuple {
            generation: request.candidate.record().clone(),
            receipt: Receipt::decode(&before.receipt)?
                .with_executable_digest(digest)
                .encode()?,
            locator: locator(request.candidate.record())?,
        };
        let retained_after = Retention {
            version: 3,
            current: after.generation.clone(),
            previous: Some(before.generation.clone()),
        }
        .encode()?;
        let mut j = NativeJournal {
            transaction_id: rand::random(),
            operation,
            phase: NativePhase::Staging,
            before,
            after,
            retained_before,
            retained_after,
            service: snapshot,
            stop_intent: false,
            quiesced: None,
            cleanup_timeout_millis: 30_000,
        };
        j.save(&self.paths)?;
        checkpoint("native.staging")?;
        storage::stage(
            storage::open_read(&self.paths.executable)?,
            &stage,
            &j.before.generation.artifact,
        )?;
        storage::rename(&stage, &self.paths.artifact(&j.before.generation.artifact))?;
        j.phase(&self.paths, NativePhase::Ready)?;
        ensure!(
            service.snapshot(end)? == j.service,
            "service changed before quiescence"
        );
        service.preflight(&j.service, end)?;
        self.stop_native(service, &mut j, end)?;
        validate_mixed(&self.paths, &j)?;
        j.phase(&self.paths, NativePhase::Replacing)?;
        self.install_cached(&j.after.generation.artifact, end)?;
        j.phase(&self.paths, NativePhase::Installed)?;
        if j.operation == Operation::Rollback {
            service.reset_failed(&j.service, &self._lock, end)?;
        }
        let live = self.start_if_running(service, &j.service.clone(), end, || {
            j.phase(&self.paths, NativePhase::Starting)
        })?;
        j.phase(&self.paths, NativePhase::Verified)?;
        check_deadline(end)?;
        self.confirm_disposition(service, &j.service, &live, end)?;
        write_tuple(&self.paths, &j.after, &j.retained_after)?;
        self.confirm_disposition(service, &j.service, &live, end)?;
        j.phase(&self.paths, NativePhase::Committed)?;
        self.finish_native()?;
        Ok(Outcome::Installed {
            live_ready: matches!(live, RunningState::Running { .. }),
        })
    }
    fn verify_terminal(
        &self,
        service: &mut impl ServiceControl,
        j: &NativeJournal,
        end: Instant,
    ) -> Result<()> {
        // Terminal metadata does not establish the recorded service disposition.
        let current = match j.service.running {
            RunningState::Running { .. } => service.verify_ready(&j.service, end)?,
            RunningState::Stopped => service.snapshot(end)?,
            RunningState::Unknown => anyhow::bail!("terminal service disposition is unknown"),
        };
        same_configuration(&j.service, &current)?;
        validate_snapshot(&current, &self.paths.executable)?;
        ensure!(!current.failed, "terminal service has failed");
        ensure!(
            matches!(
                (&j.service.running, &current.running),
                (RunningState::Stopped, RunningState::Stopped)
                    | (RunningState::Running { .. }, RunningState::Running { .. })
            ),
            "terminal service disposition changed"
        );
        check_deadline(end)
    }
    fn stop_native(
        &self,
        service: &mut impl ServiceControl,
        j: &mut NativeJournal,
        end: Instant,
    ) -> Result<()> {
        j.stop_intent = true;
        j.save(&self.paths)?;
        checkpoint("native.stop-intent")?;
        let stopped = service.quiesce(&j.service, &self._lock, end)?;
        same_configuration(&j.service, &stopped)?;
        validate_snapshot(&stopped, &self.paths.executable)?;
        ensure!(
            stopped.running == RunningState::Stopped,
            "service stop did not confirm quiescence"
        );
        check_deadline(end)?;
        j.quiesced = Some(stopped);
        j.save(&self.paths)?;
        checkpoint("native.quiesced")
    }
    fn start_if_running(
        &self,
        service: &mut impl ServiceControl,
        before: &ServiceSnapshot,
        end: Instant,
        starting: impl FnOnce() -> Result<()>,
    ) -> Result<RunningState> {
        let running = if matches!(before.running, RunningState::Running { .. }) {
            starting()?;
            let ready = service.start_ready(before, &self._lock, end)?;
            same_configuration(before, &ready)?;
            validate_snapshot(&ready, &self.paths.executable)?;
            ensure!(
                matches!(ready.running, RunningState::Running { .. }),
                "readiness lacks running service"
            );
            ready.running
        } else {
            RunningState::Stopped
        };
        self.confirm_disposition(service, before, &running, end)?;
        Ok(running)
    }
    fn confirm_disposition(
        &self,
        service: &mut impl ServiceControl,
        before: &ServiceSnapshot,
        expected: &RunningState,
        end: Instant,
    ) -> Result<()> {
        let current = service.snapshot(end)?;
        same_configuration(before, &current)?;
        validate_snapshot(&current, &self.paths.executable)?;
        ensure!(
            &current.running == expected,
            "managed process changed after readiness or confirmed stop"
        );
        check_deadline(end)
    }
    fn finish_native(&self) -> Result<()> {
        // Preserve retained files and signed proofs, including repair evidence.
        self.remove_working_files()?;
        storage::remove(&self.paths.journal)
    }
    fn rollback_failed_upgrade(
        &mut self,
        service: &mut impl ServiceControl,
        result: Result<Outcome>,
    ) -> Result<Outcome> {
        let Err(error) = result else {
            return result;
        };
        let rollback = (|| -> Result<bool> {
            let Some(Record {
                work: Work::Native(j),
                ..
            }) = read_record(&self.paths)?
            else {
                return Ok(false);
            };
            if !j.stop_intent || matches!(j.phase, NativePhase::Committed | NativePhase::Restored) {
                return Ok(false);
            }
            let end = deadline(Duration::from_millis(j.cleanup_timeout_millis))?;
            let result = self.recover_inner(service, end);
            self.contain_failure(service, result)?;
            Ok(true)
        })();
        match rollback {
            Ok(true) => Err(error).context("upgrade failed; authenticated previous generation restored"),
            Ok(false) => Err(error),
            Err(rollback) => Err(error).context(format!("upgrade failed; rollback incomplete ({rollback:#}); preserve journal and explicitly recover")),
        }
    }
    fn contain_failure(
        &self,
        service: &mut impl ServiceControl,
        result: Result<Outcome>,
    ) -> Result<Outcome> {
        let Err(error) = result else {
            return result;
        };
        let cleanup = (|| -> Result<bool> {
            let Some(Record {
                work: Work::Native(mut j),
                ..
            }) = read_record(&self.paths)?
            else {
                return Ok(false);
            };
            if !matches!(
                j.phase,
                NativePhase::Starting
                    | NativePhase::Verified
                    | NativePhase::Restoring
                    | NativePhase::Containing
            ) {
                return Ok(false);
            }
            let end = deadline(Duration::from_millis(j.cleanup_timeout_millis))?;
            j.phase(&self.paths, NativePhase::Containing)?;
            service.stop(&j.service, &self._lock, end)?;
            self.confirm_disposition(service, &j.service, &RunningState::Stopped, end)?;
            Ok(true)
        })();
        match cleanup {
            Ok(true)=>Err(error).context("native operation failed; service stopped, authenticated bytes retained; explicitly recover"),
            Ok(false)=>Err(error),
            Err(stop)=>Err(error).context(format!("native operation failed; service stop unconfirmed ({stop:#}); preserve pending journal and explicitly recover")),
        }
    }
    pub fn require_pending_recovery(&self) -> Result<()> {
        let bytes = storage::read_private_record(&self.paths.journal)?
            .context("no pending native transaction")?;
        let header: serde_json::Value =
            serde_json::from_slice(&bytes).context("invalid native transaction journal")?;
        if header.get("version").and_then(|v| v.as_u64()) == Some(1) {
            self.pending_recovery()?.context("legacy journal missing")?;
        } else {
            read_record(&self.paths)?.context("journal missing")?;
        }
        Ok(())
    }
    pub fn validate_recovery_executable(&self, generation: &AuthenticatedGeneration) -> Result<()> {
        let bytes = storage::read_private_record(&self.paths.journal)?
            .context("no pending native recovery")?;
        let header: serde_json::Value = serde_json::from_slice(&bytes)?;
        if header.get("version").and_then(|v| v.as_u64()) == Some(1) {
            let j = self.pending_recovery()?.context("legacy journal missing")?;
            ensure!(
                legacy_generation_matches(generation.record(), &j.previous)
                    || legacy_generation_matches(generation.record(), &j.candidate),
                "authenticated recovery executable is not an exact recorded legacy generation"
            );
        } else {
            let record = read_record(&self.paths)?.context("journal missing")?;
            let matches = match record.work {
                Work::Native(j) => {
                    same_generation_identity(generation.record(), &j.before.generation)
                        || same_generation_identity(generation.record(), &j.after.generation)
                }
                Work::Baseline(j) => {
                    same_generation_identity(generation.record(), &j.after.generation)
                }
            };
            ensure!(
                matches,
                "authenticated recovery executable differs from recorded generation"
            );
        }
        Ok(())
    }
    pub fn recover(
        &mut self,
        service: &mut impl ServiceControl,
        timeout: Duration,
    ) -> Result<Outcome> {
        self.recover_until(service, deadline(timeout)?)
    }
    pub fn recover_until(
        &mut self,
        service: &mut impl ServiceControl,
        end: Instant,
    ) -> Result<Outcome> {
        let result = self.recover_inner(service, end);
        self.contain_failure(service, result)
    }
    fn recover_inner(
        &mut self,
        service: &mut impl ServiceControl,
        end: Instant,
    ) -> Result<Outcome> {
        check_deadline(end)?;
        self._lock.reject_pending_image()?;
        let Some(bytes) = storage::read_private_record(&self.paths.journal)? else {
            return Ok(Outcome::NoPendingTransaction);
        };
        let header: serde_json::Value = serde_json::from_slice(&bytes)
            .context("damaged native journal; explicitly repair authenticated baseline")?;
        if header.get("version").and_then(|v| v.as_u64()) == Some(1) {
            return self.recover_legacy_early();
        }
        let record = read_record(&self.paths)?.context("journal missing")?;
        let mut j = match record.work {
            Work::Baseline(mut baseline) => {
                self.finish_baseline(&mut baseline, end)?;
                return Ok(Outcome::Recovered {
                    artifact: baseline.after.generation.artifact,
                    action: RecoveryAction::CompletedCommit,
                    live_ready: false,
                });
            }
            Work::Native(j) => j,
        };
        validate_mixed(&self.paths, &j)?;
        if matches!(j.phase, NativePhase::Committed | NativePhase::Restored) {
            let committed = j.phase == NativePhase::Committed;
            let (tuple, retained) = if committed {
                (&j.after, &j.retained_after)
            } else {
                (&j.before, &j.retained_before)
            };
            verify_tuple(&self.paths, tuple)?;
            ensure!(
                storage::read_private_record(&self.paths.retained)?.as_ref() == Some(retained),
                "terminal native retention changed"
            );
            self.verify_terminal(service, &j, end)?;
            self.finish_native()?;
            return Ok(Outcome::Recovered {
                artifact: tuple.generation.artifact.clone(),
                action: if committed {
                    RecoveryAction::CompletedCommit
                } else {
                    RecoveryAction::RestoredPrevious
                },
                live_ready: false,
            });
        }
        if matches!(j.phase, NativePhase::Staging | NativePhase::Ready) && !j.stop_intent {
            ensure!(
                service.snapshot(end)? == j.service,
                "service changed before early abort"
            );
            verify_tuple(&self.paths, &j.before)?;
            ensure!(
                storage::read_private_record(&self.paths.retained)?.as_ref()
                    == Some(&j.retained_before),
                "retention changed before stop intent"
            );
            self.finish_native()?;
            return Ok(Outcome::Recovered {
                artifact: j.before.generation.artifact,
                action: RecoveryAction::AbortedBeforeReplacement,
                live_ready: false,
            });
        }
        // Validate rollback bytes before stopping or replacing anything.
        storage::verify(
            &self.paths.artifact(&j.before.generation.artifact),
            &j.before.generation.artifact,
        )?;
        ensure!(
            storage::verify(&self.paths.executable, &j.before.generation.artifact).is_ok()
                || storage::verify(&self.paths.executable, &j.after.generation.artifact).is_ok(),
            "installed executable matches neither recorded generation"
        );
        self.stop_native(service, &mut j, end)?;
        j.phase(&self.paths, NativePhase::Restoring)?;
        self.install_cached(&j.before.generation.artifact, end)?;
        write_tuple(&self.paths, &j.before, &j.retained_before)?;
        service.reset_failed(&j.service, &self._lock, end)?;
        let live = self.start_if_running(service, &j.service, end, || Ok(()))?;
        check_deadline(end)?;
        j.phase(&self.paths, NativePhase::Restored)?;
        self.finish_native()?;
        Ok(Outcome::Recovered {
            artifact: j.before.generation.artifact,
            action: RecoveryAction::RestoredPrevious,
            live_ready: matches!(live, RunningState::Running { .. }),
        })
    }
    fn recover_legacy_early(&self) -> Result<Outcome> {
        let j = self.pending_recovery()?.context("legacy journal missing")?;
        ensure!(matches!(j.phase,Phase::Staging|Phase::Ready),"post-replacement V1 recovery requires independently authenticated exact generation; a descriptor CID is not a transport root or signed proof");
        storage::verify(&self.paths.executable, &j.previous.artifact)?;
        ensure!(
            storage::read_record(&self.paths.receipt)?.as_ref() == Some(&j.receipt_before),
            "legacy before receipt changed"
        );
        self.finish_aborted(&j)?;
        Ok(Outcome::Recovered {
            artifact: j.previous.artifact,
            action: RecoveryAction::AbortedBeforeReplacement,
            live_ready: false,
        })
    }
    /// Reconcile the installer's already-complete receipt and software. This does
    /// not replace executable bytes, create identity, or authorize Host startup.
    pub fn initialize_completed_installation(
        &mut self,
        generation: &AuthenticatedGeneration,
    ) -> Result<()> {
        self._lock.reject_pending()?;
        if storage::exists(&self.paths.retained)? {
            let current = self.current_generation()?;
            ensure!(same_generation_identity(current.record(),generation.record()),"installation already records another native generation; explicitly repair baseline");
            return Ok(());
        }
        let receipt = Receipt::decode(
            &storage::read_record(&self.paths.receipt)?.context("installation receipt missing")?,
        )?;
        ensure!(
            receipt.complete,
            "installer must complete its ownership receipt before native initialization"
        );
        verify_fhs_receipt(self._lock.home(), &receipt)?;
        ensure!(
            storage::verify(&self.paths.executable, generation.artifact())?
                == receipt.executable_digest(),
            "completed installation differs from authenticated executable"
        );
        self.begin_baseline(generation, None, true, deadline(Duration::from_secs(30))?)
            .map(|_| ())
    }
    /// An explicit metadata-only decision. Unknown journals are archived before
    /// being superseded. No old release, service intent, or readiness is invented.
    pub fn repair_baseline_until(
        &mut self,
        end: Instant,
        generation: &AuthenticatedGeneration,
        acknowledge_historical_rollback_unavailable: bool,
    ) -> Result<RepairOutcome> {
        check_deadline(end)?;
        self._lock.reject_pending_image()?;
        // Resume only this exact authenticated repair if a prior attempt reached
        // its journal. A different selection must first finish that repair.
        match read_record(&self.paths) {
            Ok(Some(Record { work: Work::Baseline(mut j), .. })) => {
                ensure!(same_generation_identity(&j.after.generation, generation.record()), "pending baseline repair selects another authenticated generation");
                self.finish_baseline(&mut j, end)?;
                return Ok(self.repair_outcome(&j));
            }
            Ok(Some(Record { work: Work::Native(_), .. })) => anyhow::bail!("known native transaction must recover its authenticated before/after generations; baseline repair cannot discard its rollback obligation"),
            _ => {}
        }
        if let Ok(Some(bytes)) = storage::read_record(&self.paths.journal) {
            if let Ok(journal) = serde_json::from_slice::<Journal>(&bytes) {
                ensure!(self.validate_journal(&journal).is_err(), "known V1 transaction requires explicit authenticated legacy recovery; baseline repair cannot discard it");
            }
        }
        let previous = (|| -> Result<Option<GenerationRecord>> {
            let retained = self.retention()?;
            ensure!(
                same_generation_identity(&retained.current, generation.record()),
                "prior current generation differs from selected baseline"
            );
            if let Some(previous) = &retained.previous {
                proof_generation(&self.paths, previous)?;
                storage::verify(&self.paths.artifact(&previous.artifact), &previous.artifact)?;
            }
            Ok(retained.previous)
        })()
        .unwrap_or(None);
        ensure!(previous.is_some()||acknowledge_historical_rollback_unavailable,"historical rollback cannot be authenticated; explicitly acknowledge its unavailability to repair baseline (all old evidence will be preserved)");
        self.begin_baseline(
            generation,
            previous,
            acknowledge_historical_rollback_unavailable,
            end,
        )
    }
    fn begin_baseline(
        &self,
        generation: &AuthenticatedGeneration,
        previous: Option<GenerationRecord>,
        acknowledged: bool,
        end: Instant,
    ) -> Result<RepairOutcome> {
        check_deadline(end)?;
        let receipt=Receipt::decode(&storage::read_record(&self.paths.receipt)?.context("baseline repair needs a readable complete installation receipt; restore its FHS ownership evidence first")?)?;
        ensure!(
            receipt.complete,
            "baseline repair cannot complete an interrupted installation"
        );
        verify_fhs_receipt(self._lock.home(), &receipt)?;
        let digest=storage::verify(&self.paths.executable,generation.artifact()).context("current executable is not the independently authenticated release; manual verified restoration is required")?;
        ensure!(
            previous.is_some() || acknowledged,
            "historical rollback acknowledgement missing"
        );
        let archive = format!("repair-{}", hex::encode(rand::random::<[u8; 32]>()));
        let directory = self.paths.store.join(&archive);
        use std::os::unix::fs::DirBuilderExt;
        std::fs::DirBuilder::new().mode(0o700).create(&directory)?;
        storage::sync_parent(&directory)?;
        let mut evidence = Vec::new();
        let proof_path = self.paths.proof(&generation.record().proof)?;
        for (name, path) in [
            ("journal", &self.paths.journal),
            ("receipt", &self.paths.receipt),
            ("locator", &self.paths.locator),
            ("retained", &self.paths.retained),
            ("proof", &proof_path),
        ] {
            let pending = path.with_file_name(format!(
                ".{}.pending",
                path.file_name()
                    .context("metadata filename")?
                    .to_string_lossy()
            ));
            for (name, path) in [
                (name.to_owned(), path.clone()),
                (format!("{name}-pending"), pending),
            ] {
                if storage::exists(&path)? {
                    let digest = storage::archive(&path, &directory.join(&name))?;
                    evidence.push(EvidenceRecord { name, digest });
                }
            }
        }
        checkpoint("repair.evidence.archived")?;
        let after = NativeTuple {
            generation: generation.record().clone(),
            receipt: receipt.with_executable_digest(digest).encode()?,
            locator: locator(generation.record())?,
        };
        let retained_after = Retention {
            version: 3,
            current: generation.record().clone(),
            previous: previous.clone(),
        }
        .encode()?;
        let mut j = BaselineJournal {
            archive,
            evidence,
            after,
            retained_after,
            historical_rollback_available: previous.is_some(),
            committed: false,
        };
        // Keep the independently authenticated selection separate from damaged
        // canonical proof bytes until durable repair intent owns the replacement.
        storage::write_proof(&directory.join("selected-proof"), generation.proof_bytes())?;
        j.save(&self.paths)?;
        checkpoint("repair.intent")?;
        self.finish_baseline(&mut j, end)?;
        Ok(self.repair_outcome(&j))
    }
    fn repair_outcome(&self, j: &BaselineJournal) -> RepairOutcome {
        RepairOutcome {
            archive: self.paths.store.join(&j.archive),
            historical_rollback_available: j.historical_rollback_available,
            live_ready: false,
        }
    }
    fn finish_baseline(&self, j: &mut BaselineJournal, end: Instant) -> Result<()> {
        j.validate()?;
        for entry in &j.evidence {
            storage::verify_archive(
                &self.paths.store.join(&j.archive).join(&entry.name),
                &entry.digest,
            )?;
        }
        let proof_bytes =
            storage::read_proof(&self.paths.store.join(&j.archive).join("selected-proof"))?;
        let proof = j.after.generation.proof.verify(&proof_bytes)?;
        let authenticated = AuthenticatedGeneration::from_proof(
            proof,
            &j.after.generation.verification_keys,
            &j.after.generation.target,
        )?;
        ensure!(
            authenticated.record() == &j.after.generation,
            "repair selection differs from independently authenticated proof"
        );
        let digest = storage::verify(&self.paths.executable, &j.after.generation.artifact)?;
        ensure!(
            digest == Receipt::decode(&j.after.receipt)?.executable_digest(),
            "baseline bytes changed during repair"
        );
        // During repair only exact archived-before or intended-after metadata is
        // owned. This prevents a resumed repair overwriting intervening edits.
        for (name, path, after) in [
            ("receipt", &self.paths.receipt, &j.after.receipt),
            ("locator", &self.paths.locator, &j.after.locator),
            ("retained", &self.paths.retained, &j.retained_after),
        ] {
            let after_matches = storage::read_record(path).ok().flatten().as_ref() == Some(after);
            let before_matches = match j.evidence.iter().find(|entry| entry.name == name) {
                Some(entry) => storage::verify_archive(path, &entry.digest).is_ok(),
                None => !storage::exists(path)?,
            };
            ensure!(
                before_matches || after_matches,
                "baseline metadata changed outside repair"
            );
        }
        verify_fhs_receipt(self._lock.home(), &Receipt::decode(&j.after.receipt)?)?;
        check_deadline(end)?;
        self.paths.create_proof_directory()?;
        let canonical = self.paths.proof(&j.after.generation.proof)?;
        if storage::exists(&canonical)?
            && storage::read_proof(&canonical).ok().as_ref() != Some(&proof_bytes)
        {
            let old = j
                .evidence
                .iter()
                .find(|entry| entry.name == "proof")
                .context("canonical proof changed outside repair")?;
            storage::verify_archive(&canonical, &old.digest)?;
            // The exact damaged bytes are already durably archived. A crash
            // here resumes from selected-proof without losing either evidence.
            storage::remove(&canonical)?;
        }
        storage::write_proof(&canonical, &proof_bytes)?;
        checkpoint("repair.proof.after")?;
        write_tuple(&self.paths, &j.after, &j.retained_after)?;
        j.committed = true;
        j.save(&self.paths)?;
        checkpoint("repair.committed")?;
        // Keep a durable manifest next to all damaged evidence after journal-last cleanup.
        storage::write_record(
            &self.paths.store.join(&j.archive).join("manifest.json"),
            &serde_json::to_vec(j)?,
        )?;
        storage::remove(&self.paths.journal)
    }
}

impl UpgradeCoordinator {
    pub fn recover_authenticated_legacy_until(
        &mut self,
        service: &mut impl ServiceControl,
        deadline: Instant,
        generation: &AuthenticatedGeneration,
    ) -> Result<Outcome> {
        check_deadline(deadline)?;
        self._lock.reject_pending_image()?;
        self.validate_recovery_executable(generation)?;
        let bytes =
            storage::read_private_record(&self.paths.journal)?.context("journal missing")?;
        let header: serde_json::Value = serde_json::from_slice(&bytes)?;
        if header.get("version").and_then(|v| v.as_u64()) != Some(1) {
            return self.recover_until(service, deadline);
        }
        let mut journal = self.pending_recovery()?.context("legacy journal missing")?;
        if matches!(journal.phase, Phase::Staging | Phase::Ready) {
            return self.recover_until(service, deadline);
        }
        let current = service.legacy_snapshot(deadline)?;
        validate_snapshot(&current, &self.paths.executable)?;
        same_configuration(&journal.before, &current)?;
        ensure!(
            current.running == RunningState::Stopped,
            "explicitly stop the legacy managed service before authenticated recovery"
        );
        if journal.phase == Phase::Committed {
            ensure!(
                legacy_generation_matches(generation.record(), &journal.candidate),
                "committed legacy cleanup requires the exact independently authenticated candidate"
            );
            storage::verify(&self.paths.executable, generation.artifact())?;
            ensure!(
                storage::read_record(&self.paths.receipt)?.as_ref()
                    == journal.receipt_after.as_ref(),
                "committed legacy receipt changed"
            );
            let expected = Retained {
                version: 1,
                current: journal.candidate.clone(),
                previous: journal.previous.clone(),
            };
            ensure!(
                storage::read_record(&self.paths.retained)?
                    .as_deref()
                    .map(Retained::decode)
                    .transpose()?
                    .as_ref()
                    == Some(&expected),
                "committed legacy retention changed"
            );
            self.remove_working_files()?;
            storage::remove(&self.paths.journal)?;
            return Ok(Outcome::Recovered {
                artifact: journal.candidate.artifact,
                action: RecoveryAction::CompletedCommit,
                live_ready: false,
            });
        }
        ensure!(legacy_generation_matches(generation.record(),&journal.previous),"legacy restoration requires independently authenticated exact previous generation; select its explicit release root");
        if journal.phase == Phase::Restored {
            storage::verify(&self.paths.executable, generation.artifact())?;
            ensure!(
                storage::read_record(&self.paths.receipt)?.as_ref()
                    == Some(&journal.receipt_before)
                    && storage::read_record(&self.paths.retained)? == journal.retained_before,
                "restored legacy tuple changed"
            );
            self.finish_aborted(&journal)?;
            return Ok(Outcome::Recovered {
                artifact: journal.previous.artifact,
                action: RecoveryAction::RestoredPrevious,
                live_ready: false,
            });
        }
        let retained_now = storage::read_record(&self.paths.retained)?;
        let after_retained = serde_json::to_vec(&Retained {
            version: 1,
            current: journal.candidate.clone(),
            previous: journal.previous.clone(),
        })?;
        ensure!(
            retained_now == journal.retained_before
                || retained_now.as_ref() == Some(&after_retained),
            "legacy retention changed outside transaction"
        );
        ensure!(
            storage::verify(&self.paths.executable, &journal.previous.artifact).is_ok()
                || storage::verify(&self.paths.executable, &journal.candidate.artifact).is_ok(),
            "legacy executable matches neither recorded generation"
        );
        storage::verify(
            &self.paths.artifact(&journal.previous.artifact),
            generation.artifact(),
        )?;
        check_deadline(deadline)?;
        journal.phase = Phase::Restoring;
        storage::write_record(&self.paths.journal, &serde_json::to_vec(&journal)?)?;
        self.install_cached(generation.artifact(), deadline)?;
        storage::write_record(&self.paths.receipt, &journal.receipt_before)?;
        match &journal.retained_before {
            Some(bytes) => storage::write_record(&self.paths.retained, bytes)?,
            None => storage::remove(&self.paths.retained)?,
        }
        ensure!(
            service.legacy_snapshot(deadline)? == current,
            "legacy managed service changed during restoration"
        );
        storage::verify(&self.paths.executable, generation.artifact())?;
        journal.phase = Phase::Restored;
        storage::write_record(&self.paths.journal, &serde_json::to_vec(&journal)?)?;
        self.finish_aborted(&journal)?;
        Ok(Outcome::Recovered {
            artifact: journal.previous.artifact,
            action: RecoveryAction::RestoredPrevious,
            live_ready: false,
        })
    }

    pub fn recover_authenticated_until(
        &mut self,
        service: &mut impl ServiceControl,
        end: Instant,
        generation: AuthenticatedGeneration,
    ) -> Result<Outcome> {
        self.validate_recovery_executable(&generation)?;
        self.recover_authenticated_legacy_until(service, end, &generation)
    }
}
fn legacy_generation_matches(generation: &GenerationRecord, record: &ArtifactRecord) -> bool {
    generation.artifact == record.artifact
        && generation.descriptor == record.release
        && generation.target == record.platform
        && generation.release_version == record.version
        && generation.revision == record.revision
}
