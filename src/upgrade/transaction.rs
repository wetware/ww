//! Version-two transitions on the existing Engine and native journal path.
use super::attestation::VerifiedExecutable;
use super::inputs::PreflightInputs;
use super::provenance::{AuthenticatedGeneration, GenerationRecord};
use super::service::{RestartAdmission, ServiceDisposition};
use super::startup::{
    InstanceLease, InvocationBinding, PermitTarget, StartupContractV1, StartupDisposition,
    StartupLease, StartupPermitPayload, StartupStateV1, TargetTupleDigests,
};
use super::*;
use anyhow::bail;
use sha2::{Digest, Sha256};

pub struct UpgradeRequest<R: Read> {
    pub installed: AuthenticatedGeneration,
    pub candidate: AuthenticatedGeneration,
    pub bytes: R,
    pub inputs: PreflightInputs,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct NativeTuple {
    generation: GenerationRecord,
    receipt: Vec<u8>,
    locator: Vec<u8>,
    contract: StartupContractV1,
}
impl NativeTuple {
    fn validate(&self) -> Result<()> {
        self.generation.validate()?;
        self.contract.validate()?;
        ensure!(
            self.contract.artifact == self.generation.artifact,
            "tuple contract differs from native artifact"
        );
        let receipt = Receipt::decode(&self.receipt)?;
        ensure!(
            receipt.complete && receipt.encode()? == self.receipt,
            "tuple receipt is incomplete or noncanonical"
        );
        let locator =
            provenance::InstalledRelease::from_root(self.generation.transport_root.parse()?)?;
        ensure!(
            serde_json::to_vec(&locator)? == self.locator,
            "tuple locator differs from authenticated root"
        );
        Ok(())
    }
    fn digests(&self) -> Result<TargetTupleDigests> {
        self.validate()?;
        Ok(TargetTupleDigests {
            executable: self.generation.artifact.clone(),
            native_receipt: Sha256::digest(&self.receipt).into(),
            locator: Sha256::digest(&self.locator).into(),
            proof: hex::decode(&self.generation.proof.sha256)?
                .try_into()
                .map_err(|_| anyhow::anyhow!("proof digest size"))?,
            contract: self.contract.identity.digest,
        })
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
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
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct NativeJournal {
    version: u32,
    transaction_id: [u8; 32],
    phase: NativePhase,
    before: NativeTuple,
    after: NativeTuple,
    state_before: StartupStateV1,
    retained_before: Vec<u8>,
    retained_after: Vec<u8>,
    disposition: ServiceDisposition,
    admission: Option<RestartAdmission>,
    candidate_permit: Option<StartupPermitPayload>,
    restored_permit: Option<StartupPermitPayload>,
}

impl NativeJournal {
    pub(super) fn validate(&self) -> Result<()> {
        ensure!(
            self.version == 2 && self.transaction_id != [0; 32],
            "invalid native transaction identity/version"
        );
        self.before.validate()?;
        self.after.validate()?;
        self.state_before.validate()?;
        self.state_before.validate_contract(&self.before.contract)?;
        ensure!(
            matches!(
                self.state_before.disposition,
                StartupDisposition::Committed { .. }
            ) && self.state_before.initializing_receipt.is_none()
                && self.state_before.native == self.before.generation,
            "native transaction requires exact committed before-state"
        );
        ensure!(
            self.before.generation.artifact != self.after.generation.artifact
                && self.before.generation.target == self.after.generation.target,
            "native transaction must change bytes on the same target"
        );
        ensure!(
            self.after.contract.identity.generation
                == self.before.contract.identity.next_generation()?
                && self.after.contract.installation_id == self.before.contract.installation_id
                && self.after.contract.startup == self.before.contract.startup
                && self.after.contract.invocation == self.before.contract.invocation
                && self.after.contract.trusted_release_keys
                    == self.before.contract.trusted_release_keys,
            "native transaction changes startup authorization beyond native artifact"
        );
        let before = RetainedV2::decode(&self.retained_before)?;
        ensure!(
            before.current == self.before.generation
                && before.installation_id == self.state_before.installation_id
                && before.contract.as_ref() == Some(&self.before.contract.identity),
            "native before retention differs from committed tuple"
        );
        let expected = RetainedV2 {
            version: 2,
            installation_id: before.installation_id,
            current: self.after.generation.clone(),
            previous: Some(before.current),
            contract: Some(self.after.contract.identity.clone()),
        };
        ensure!(
            expected.encode()? == self.retained_after,
            "native after retention differs from target tuple"
        );
        self.disposition.validate()?;
        match (
            &self.disposition.quiescence_contract,
            &self.disposition.quiescence_permit,
        ) {
            (None, None) => (),
            (Some(contract), Some(permit)) => {
                ensure!(
                    self.admission.is_some(),
                    "stopped transaction cannot own a launched quiescence target"
                );
                let role = if contract == &self.after.contract {
                    PermitTarget::Candidate
                } else if contract == &self.before.contract {
                    PermitTarget::RestoredBefore
                } else {
                    bail!("quiescence contract is not an exact transaction target")
                };
                ensure!(
                    permit == &self.make_payload(role)?,
                    "quiescence permit differs from exact target"
                );
            }
            _ => bail!("quiescence target requires both contract and permit"),
        }
        match &self.admission {
            Some(admission) => {
                admission.validate(&self.disposition.original)?;
                ensure!(
                    admission.contract == self.before.contract
                        && admission.transaction_id == self.transaction_id
                        && self.disposition.original_process.as_ref()
                            == Some(&admission.original_process)
                        && self.disposition.identity.as_ref() == Some(&admission.service)
                        && self.disposition.original_contract.as_ref()
                            == Some(&self.before.contract),
                    "restart admission differs from original tuple/process"
                );
                ensure!(
                    self.candidate_permit.as_ref()
                        == Some(&self.make_payload(PermitTarget::Candidate)?)
                        && self.restored_permit.as_ref()
                            == Some(&self.make_payload(PermitTarget::RestoredBefore)?),
                    "native permit payload changed"
                );
            }
            None => ensure!(
                self.disposition.original.running == RunningState::Stopped
                    && self.candidate_permit.is_none()
                    && self.restored_permit.is_none(),
                "stopped native operation cannot grant a launch permit"
            ),
        }
        Ok(())
    }
    fn make_payload(&self, role: PermitTarget) -> Result<StartupPermitPayload> {
        let tuple = if role == PermitTarget::Candidate {
            &self.after
        } else {
            &self.before
        };
        Ok(StartupPermitPayload {
            version: 1,
            transaction_id: self.transaction_id,
            installation_id: self.state_before.installation_id,
            role,
            target: tuple.digests()?,
            contract: tuple.contract.identity.clone(),
            invocation: tuple.contract.invocation.clone(),
        })
    }
    fn state(&self, tuple: &NativeTuple, disposition: StartupDisposition) -> StartupStateV1 {
        let mut state = self.state_before.clone();
        state.native = tuple.generation.clone();
        state.trusted_release_keys = tuple.contract.trusted_release_keys.clone();
        state.disposition = disposition;
        state
    }
    fn blocked(&self) -> StartupStateV1 {
        self.state(&self.before, StartupDisposition::Blocked)
    }
    fn steady(&self, tuple: &NativeTuple) -> StartupStateV1 {
        self.state(
            tuple,
            StartupDisposition::Committed {
                contract: tuple.contract.identity.clone(),
            },
        )
    }
    fn runnable(&self, role: PermitTarget) -> Result<StartupStateV1> {
        let payload = if role == PermitTarget::Candidate {
            self.candidate_permit.as_ref()
        } else {
            self.restored_permit.as_ref()
        }
        .context("original service was not admitted to restart")?;
        let tuple = if role == PermitTarget::Candidate {
            &self.after
        } else {
            &self.before
        };
        Ok(self.state(
            tuple,
            StartupDisposition::Transaction {
                transaction_id: self.transaction_id,
                permit_digest: payload.digest()?,
                contract: tuple.contract.identity.clone(),
            },
        ))
    }
    fn save(&self, paths: &Paths, rollback: bool) -> Result<()> {
        let journal = if rollback {
            JournalV2::Rollback(self.clone())
        } else {
            JournalV2::Upgrade(self.clone())
        };
        save_v2(paths, &journal)
    }
    fn phase(&mut self, paths: &Paths, rollback: bool, phase: NativePhase) -> Result<()> {
        self.phase = phase;
        self.save(paths, rollback)?;
        checkpoint(&format!("native.phase.{phase:?}"))
    }
}

fn read_native(paths: &Paths) -> Result<Option<(NativeJournal, bool)>> {
    let Some(bytes) = storage::read_private_record(&paths.journal)? else {
        return Ok(None);
    };
    let decoded: JournalV2 =
        serde_json::from_slice(&bytes).context("native journal requires supported v2 recovery")?;
    let (journal, rollback) = match decoded {
        JournalV2::Upgrade(j) => (j, false),
        JournalV2::Rollback(j) => (j, true),
        JournalV2::Admit(_) => {
            bail!("pending startup admission must recover through startup admit")
        }
    };
    journal.validate()?;
    if rollback {
        ensure!(
            RetainedV2::decode(&journal.retained_before)?
                .previous
                .as_ref()
                == Some(&journal.after.generation),
            "rollback target is not exact committed previous generation"
        );
    }
    Ok(Some((journal, rollback)))
}

fn proof_generation(paths: &Paths, record: &GenerationRecord) -> Result<AuthenticatedGeneration> {
    let proof = record
        .proof
        .verify(&storage::read_proof(&paths.proof(&record.proof)?)?)?;
    let generation =
        AuthenticatedGeneration::from_proof(proof, &record.verification_keys, &record.target)?;
    ensure!(
        generation.record() == record,
        "proof identifies another recorded generation"
    );
    Ok(generation)
}
fn verify_tuple(paths: &Paths, tuple: &NativeTuple) -> Result<()> {
    tuple.validate()?;
    proof_generation(paths, &tuple.generation)?;
    ensure!(
        storage::read_private_record(&paths.receipt)?.as_ref() == Some(&tuple.receipt)
            && storage::read_private_record(&paths.locator)?.as_ref() == Some(&tuple.locator),
        "installed receipt/locator differ from exact native tuple"
    );
    let receipt = Receipt::decode(&tuple.receipt)?;
    ensure!(
        storage::verify(&paths.executable, &tuple.generation.artifact)?
            == receipt.executable_digest(),
        "installed native bytes differ from exact tuple"
    );
    Ok(())
}
fn validate_owned_mixed(paths: &Paths, lease: &StartupLease, j: &NativeJournal) -> Result<()> {
    ensure!(
        lease.marker()?.installation_id == j.state_before.installation_id,
        "native journal identifies another installation"
    );
    let state = lease.read_state()?.context("startup state missing")?;
    let mut states = vec![j.blocked(), j.steady(&j.before), j.steady(&j.after)];
    if j.admission.is_some() {
        states.push(j.runnable(PermitTarget::Candidate)?);
        states.push(j.runnable(PermitTarget::RestoredBefore)?);
    }
    ensure!(
        states.contains(&state),
        "startup state is not owned by native transaction"
    );
    for (path, before, after) in [
        (&paths.receipt, &j.before.receipt, &j.after.receipt),
        (&paths.locator, &j.before.locator, &j.after.locator),
        (&paths.retained, &j.retained_before, &j.retained_after),
    ] {
        let actual =
            storage::read_private_record(path)?.context("native tuple metadata missing")?;
        ensure!(
            &actual == before || &actual == after,
            "native tuple metadata changed outside transaction"
        );
    }
    let contract = lease
        .read_contract()?
        .context("native startup contract missing")?;
    ensure!(
        contract == j.before.contract || contract == j.after.contract,
        "startup contract changed outside native transaction"
    );
    proof_generation(paths, &j.before.generation)?;
    Ok(())
}
fn write_tuple(
    paths: &Paths,
    lease: &StartupLease,
    tuple: &NativeTuple,
    retained: &[u8],
) -> Result<()> {
    storage::write_record(&paths.receipt, &tuple.receipt)?;
    checkpoint("native.receipt.after")?;
    storage::write_record(&paths.locator, &tuple.locator)?;
    checkpoint("native.locator.after")?;
    storage::write_record(&paths.retained, retained)?;
    checkpoint("native.retained.after")?;
    lease.write_contract(&tuple.contract)?;
    checkpoint("native.contract.after")?;
    verify_tuple(paths, tuple)
}
fn verify_preflight(
    service: &mut impl ServiceControl,
    executable: &VerifiedExecutable,
    inputs: &PreflightInputs,
    deadline: Instant,
) -> Result<()> {
    check_deadline(deadline)?;
    inputs.validate()?;
    service
        .compatible(executable, inputs, deadline)?
        .verify(executable, inputs)?;
    executable.revalidate(deadline)
}
fn existing_inputs(
    contract: &StartupContractV1,
    inputs: &PreflightInputs,
    home: &Path,
) -> Result<()> {
    inputs.validate()?;
    ensure!(inputs.startup_identity()?==contract.startup && inputs.config.home==home && !inputs.config.insecure_ephemeral,
        "effective startup inputs changed; stop and explicitly admit the new inputs before native replacement");
    Ok(())
}

impl Engine {
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
        deadline: Instant,
        select: impl FnOnce() -> Result<UpgradeRequest<R>>,
    ) -> Result<Outcome> {
        check_deadline(deadline)?;
        ensure!(
            !storage::exists(&self.paths.journal)?,
            "pending native work must recover before selecting a release"
        );
        let request = select()?;
        self.start_native(service, deadline, request, false)
    }
    pub fn check_upgrade_until<R: Read>(
        &mut self,
        service: &mut impl ServiceControl,
        deadline: Instant,
        request: UpgradeRequest<R>,
    ) -> Result<Outcome> {
        check_deadline(deadline)?;
        self._lock.reject_pending()?;
        let state = self.startup_state()?;
        let contract = self
            .startup_contract()?
            .context("startup contract is unbound")?;
        ensure!(
            same_generation_identity(&state.native, request.installed.record()),
            "installed release differs from committed generation"
        );
        if state.native.artifact == *request.candidate.artifact() {
            return Ok(Outcome::AlreadyInstalled);
        }
        existing_inputs(&contract, &request.inputs, self._lock.home())?;
        let before = service.snapshot(deadline)?;
        validate_snapshot(&before, &self.paths.executable)?;
        let installed =
            VerifiedExecutable::open(&self.paths.executable, &state.native.artifact, deadline)?;
        authenticate_candidate(&request.candidate, &contract.trusted_release_keys)?;
        let temporary = tempfile::tempdir()?;
        let path = temporary.path().join("candidate");
        storage::stage(request.bytes, &path, request.candidate.artifact())?;
        let candidate = VerifiedExecutable::open(&path, request.candidate.artifact(), deadline)?;
        verify_preflight(service, &installed, &request.inputs, deadline)?;
        verify_preflight(service, &candidate, &request.inputs, deadline)?;
        ensure!(
            service.snapshot(deadline)? == before,
            "managed service changed during check"
        );
        Ok(Outcome::Checked)
    }
    pub fn operator_rollback_prepared_until(
        &mut self,
        service: &mut impl ServiceControl,
        deadline: Instant,
        inputs: &PreflightInputs,
    ) -> Result<Outcome> {
        check_deadline(deadline)?;
        self._lock.reject_pending()?;
        let state = self.startup_state()?;
        let retained = read_retained_v2(&self.paths)?;
        let previous = retained
            .previous
            .as_ref()
            .context("no exact committed previous generation is retained")?;
        let current = authenticate_retained(&self.paths, &retained, &state.native)?;
        let candidate = authenticate_retained(&self.paths, &retained, previous)?;
        let bytes = storage::open_read(&self.paths.artifact(candidate.artifact()))?;
        self.start_native(
            service,
            deadline,
            UpgradeRequest {
                installed: current,
                candidate,
                bytes,
                inputs: inputs.clone(),
            },
            true,
        )
    }
    pub fn preparation_contract(&self) -> Result<StartupContractV1> {
        if let Some((j, _)) = read_native(&self.paths)? {
            let lease = StartupLease::shared(self._lock.home())?;
            validate_owned_mixed(&self.paths, &lease, &j)?;
            Ok(j.before.contract)
        } else {
            self.startup_contract()?
                .context("startup contract is unbound; explicitly admit startup first")
        }
    }
    pub fn validate_recovery_executable(&self, generation: &AuthenticatedGeneration) -> Result<()> {
        let bytes = storage::read_private_record(&self.paths.journal)?
            .context("no pending native recovery")?;
        let header: serde_json::Value =
            serde_json::from_slice(&bytes).context("invalid Host transaction journal")?;
        if header.get("version").and_then(|v| v.as_u64()) == Some(1) {
            let journal = self.pending_recovery()?.context("legacy journal missing")?;
            ensure!(
                legacy_generation_matches(generation.record(), &journal.previous)
                    || legacy_generation_matches(generation.record(), &journal.candidate),
                "authenticated recovery executable is not an exact recorded legacy generation"
            );
        } else {
            let (journal, _) = read_native(&self.paths)?.context("no pending native recovery")?;
            ensure!(
                same_generation_identity(generation.record(), &journal.before.generation)
                    || same_generation_identity(generation.record(), &journal.after.generation),
                "authenticated recovery executable is not an exact recorded generation"
            );
        }
        Ok(())
    }

    /// Legacy recovery never invents a transport-root/proof association or
    /// grants guarded startup authority. Explicit signed evidence authorizes
    /// only the exact recorded physical generation selected for recovery.
    pub fn recover_authenticated_without_preparation_until(
        &mut self,
        service: &mut impl ServiceControl,
        deadline: Instant,
        generation: &AuthenticatedGeneration,
    ) -> Result<Option<Outcome>> {
        check_deadline(deadline)?;
        self._lock.reject_pending_image()?;
        self.validate_recovery_executable(generation)?;
        let bytes =
            storage::read_private_record(&self.paths.journal)?.context("journal missing")?;
        let header: serde_json::Value = serde_json::from_slice(&bytes)?;
        if header.get("version").and_then(|v| v.as_u64()) != Some(1) {
            return self.recover_without_preparation_until(service, deadline);
        }
        ensure!(
            !startup::metadata_present(self._lock.home())?,
            "legacy recovery cannot alter an initialized guarded installation"
        );
        let mut journal = self.pending_recovery()?.context("legacy journal missing")?;
        if matches!(journal.phase, Phase::Staging | Phase::Ready) {
            return self.recover_without_preparation_until(service, deadline);
        }
        let current = service.snapshot(deadline)?;
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
            return Ok(Some(Outcome::Recovered {
                artifact: journal.candidate.artifact,
                action: RecoveryAction::CompletedCommit,
                live_ready: false,
            }));
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
            return Ok(Some(Outcome::Recovered {
                artifact: journal.previous.artifact,
                action: RecoveryAction::RestoredPrevious,
                live_ready: false,
            }));
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
            service.snapshot(deadline)? == current,
            "legacy managed service changed during restoration"
        );
        storage::verify(&self.paths.executable, generation.artifact())?;
        journal.phase = Phase::Restored;
        storage::write_record(&self.paths.journal, &serde_json::to_vec(&journal)?)?;
        self.finish_aborted(&journal)?;
        Ok(Some(Outcome::Recovered {
            artifact: journal.previous.artifact,
            action: RecoveryAction::RestoredPrevious,
            live_ready: false,
        }))
    }

    fn start_native<R: Read>(
        &mut self,
        service: &mut impl ServiceControl,
        deadline: Instant,
        request: UpgradeRequest<R>,
        rollback: bool,
    ) -> Result<Outcome> {
        check_deadline(deadline)?;
        self._lock.reject_pending()?;
        let state = self.startup_state()?;
        let contract = self
            .startup_contract()?
            .context("startup contract is unbound; explicitly admit startup first")?;
        let retained_before =
            storage::read_private_record(&self.paths.retained)?.context("retention missing")?;
        let retained = RetainedV2::decode(&retained_before)?;
        authenticate_retained(&self.paths, &retained, &state.native)?;
        ensure!(
            same_generation_identity(&state.native, request.installed.record()),
            "installed release differs from exact committed generation"
        );
        if state.native.artifact == *request.candidate.artifact() {
            return Ok(Outcome::AlreadyInstalled);
        }
        existing_inputs(&contract, &request.inputs, self._lock.home())?;
        if rollback {
            ensure!(
                retained.previous.as_ref() == Some(request.candidate.record()),
                "rollback target differs from exact committed previous generation"
            );
        } else {
            authenticate_candidate(&request.candidate, &contract.trusted_release_keys)?;
        }
        let installed =
            VerifiedExecutable::open(&self.paths.executable, &state.native.artifact, deadline)?;
        let snapshot = service.snapshot(deadline)?;
        validate_snapshot(&snapshot, &self.paths.executable)?;
        let transaction_id = rand::random();
        let admission = if matches!(snapshot.running, RunningState::Running { .. }) {
            let mut admission =
                service.admit_restart(&snapshot, &contract, &installed, deadline)?;
            admission.transaction_id = transaction_id;
            admission.validate(&snapshot)?;
            Some(admission)
        } else {
            None
        };
        let before = NativeTuple {
            generation: state.native.clone(),
            receipt: storage::read_private_record(&self.paths.receipt)?
                .context("receipt missing")?,
            locator: storage::read_private_record(&self.paths.locator)?
                .context("locator missing")?,
            contract: contract.clone(),
        };
        verify_tuple(&self.paths, &before)?;
        // Staging records intended receipt bytes before candidate writes. The
        // candidate BLAKE3 is computed from the authenticated input as it stages.
        let mut candidate_bytes = Vec::new();
        request
            .bytes
            .take(request.candidate.artifact().size + 1)
            .read_to_end(&mut candidate_bytes)?;
        check_deadline(deadline)?;
        ensure!(
            candidate_bytes.len() as u64 == request.candidate.artifact().size,
            "candidate byte count differs from signed artifact"
        );
        let after_contract = StartupContractV1::new(
            state.installation_id,
            contract.identity.next_generation()?,
            request.candidate.artifact().clone(),
            contract.startup.clone(),
            contract.invocation.clone(),
            contract.trusted_release_keys.clone(),
        )?;
        let after = NativeTuple {
            generation: request.candidate.record().clone(),
            receipt: Receipt::decode(&before.receipt)?
                .with_executable_digest(blake3::hash(&candidate_bytes).to_hex().to_string())
                .encode()?,
            locator: serde_json::to_vec(&provenance::InstalledRelease::from_root(
                request.candidate.record().transport_root.parse()?,
            )?)?,
            contract: after_contract,
        };
        let retained_after = RetainedV2 {
            version: 2,
            installation_id: state.installation_id,
            current: after.generation.clone(),
            previous: Some(before.generation.clone()),
            contract: Some(after.contract.identity.clone()),
        }
        .encode()?;
        let identity = match &contract.invocation {
            InvocationBinding::Managed { service } => Some(service.clone()),
            InvocationBinding::Standalone => None,
        };
        let mut j = NativeJournal {
            version: 2,
            transaction_id,
            phase: NativePhase::Staging,
            before,
            after,
            state_before: state,
            retained_before,
            retained_after,
            disposition: ServiceDisposition {
                original: snapshot,
                identity,
                original_process: admission.as_ref().map(|a| a.original_process.clone()),
                original_contract: admission.as_ref().map(|_| contract),
                quiescence_contract: None,
                quiescence_permit: None,
                stop_intent: false,
                quiesced: None,
            },
            admission,
            candidate_permit: None,
            restored_permit: None,
        };
        if j.admission.is_some() {
            j.candidate_permit = Some(j.make_payload(PermitTarget::Candidate)?);
            j.restored_permit = Some(j.make_payload(PermitTarget::RestoredBefore)?);
        }
        j.save(&self.paths, rollback)?;
        checkpoint("native.staging")?;
        {
            let lease = StartupLease::exclusive(self._lock.home())?;
            lease.write_state(&j.blocked())?;
        }
        checkpoint("native.blocked")?;
        self.paths.create_proof_directory()?;
        storage::write_proof(
            &self.paths.proof(&j.after.generation.proof)?,
            request.candidate.proof_bytes(),
        )?;
        let stage = self.paths.store.join("stage");
        storage::stage(
            std::io::Cursor::new(candidate_bytes),
            &stage,
            &j.after.generation.artifact,
        )?;
        storage::rename(&stage, &self.paths.artifact(&j.after.generation.artifact))?;
        storage::stage(
            storage::open_read(&self.paths.executable)?,
            &stage,
            &j.before.generation.artifact,
        )?;
        storage::rename(&stage, &self.paths.artifact(&j.before.generation.artifact))?;
        let candidate = VerifiedExecutable::open(
            &self.paths.artifact(&j.after.generation.artifact),
            &j.after.generation.artifact,
            deadline,
        )?;
        verify_preflight(service, &installed, &request.inputs, deadline)?;
        verify_preflight(service, &candidate, &request.inputs, deadline)?;
        j.phase(&self.paths, rollback, NativePhase::Ready)?;
        ensure!(
            service.snapshot(deadline)? == j.disposition.original,
            "managed service changed before quiescence"
        );
        if let Some(admission) = &j.admission {
            service.revalidate_restart(admission, deadline)?;
        }
        verify_preflight(service, &candidate, &request.inputs, deadline)?;
        j.disposition.stop_intent = true;
        j.save(&self.paths, rollback)?;
        checkpoint("native.stop-intent")?;
        let quiesced = service.quiesce(&j.disposition, deadline)?;
        ensure!(
            quiesced.original == j.disposition.original
                && quiesced.identity == j.disposition.identity
                && quiesced.original_process == j.disposition.original_process
                && quiesced.original_contract == j.disposition.original_contract
                && quiesced.quiescence_contract == j.disposition.quiescence_contract
                && quiesced.quiescence_permit == j.disposition.quiescence_permit
                && quiesced.stop_intent
                && quiesced.quiesced.is_some(),
            "quiescence changed original transaction disposition"
        );
        quiesced.validate()?;
        j.disposition = quiesced;
        j.save(&self.paths, rollback)?;
        checkpoint("native.quiesced")?;
        let instance = InstanceLease::exclusive(self._lock.home())?;
        let lease = StartupLease::exclusive(self._lock.home())?;
        validate_owned_mixed(&self.paths, &lease, &j)?;
        verify_tuple(&self.paths, &j.before)?;
        installed.revalidate(deadline)?;
        check_deadline(deadline)?;
        j.phase(&self.paths, rollback, NativePhase::Replacing)?;
        self.install_cached(&j.after.generation.artifact, deadline)?;
        j.phase(&self.paths, rollback, NativePhase::Installed)?;
        let live = j.admission.is_some();
        let live_evidence = if live {
            j.phase(&self.paths, rollback, NativePhase::Starting)?;
            lease.write_state(&j.runnable(PermitTarget::Candidate)?)?;
            drop(lease);
            drop(instance);
            Some(self.restart_native(
                service,
                &j,
                PermitTarget::Candidate,
                &request.inputs,
                deadline,
            )?)
        } else {
            ensure!(
                service.snapshot(deadline)?.running == RunningState::Stopped,
                "service started during stopped native replacement"
            );
            drop(lease);
            drop(instance);
            None
        };
        j.phase(&self.paths, rollback, NativePhase::Verified)?;
        let _instance = if live {
            None
        } else {
            Some(InstanceLease::exclusive(self._lock.home())?)
        };
        let lease = StartupLease::exclusive(self._lock.home())?;
        validate_owned_mixed(&self.paths, &lease, &j)?;
        if !rollback {
            current_independent_generation(
                &self.paths,
                &j.after.generation,
                &j.after.contract.trusted_release_keys,
            )?;
        }
        check_deadline(deadline)?;
        if let Some(evidence) = &live_evidence {
            self.revalidate_native_evidence(
                service,
                &j,
                PermitTarget::Candidate,
                evidence,
                deadline,
            )?;
        }
        write_tuple(&self.paths, &lease, &j.after, &j.retained_after)?;
        if let Some(evidence) = &live_evidence {
            self.revalidate_native_evidence(
                service,
                &j,
                PermitTarget::Candidate,
                evidence,
                deadline,
            )?;
        }
        j.phase(&self.paths, rollback, NativePhase::Committed)?;
        lease.write_state(&j.steady(&j.after))?;
        self.finish_native(&j, true)?;
        Ok(Outcome::Installed { live_ready: live })
    }

    fn restart_native(
        &self,
        service: &mut impl ServiceControl,
        j: &NativeJournal,
        role: PermitTarget,
        inputs: &PreflightInputs,
        deadline: Instant,
    ) -> Result<service::RunningEvidence> {
        let admission = j.admission.as_ref().context("restart not admitted")?;
        let tuple = if role == PermitTarget::Candidate {
            &j.after
        } else {
            &j.before
        };
        let executable =
            VerifiedExecutable::open(&self.paths.executable, &tuple.generation.artifact, deadline)?;
        let permit = {
            let lease = StartupLease::shared(self._lock.home())?;
            validate_transaction_start(&lease, &tuple.contract.invocation, deadline)?
        };
        let evidence = service.restart(admission, &permit, &executable, deadline)?;
        evidence.verify(admission, &permit, &executable)?;
        executable.revalidate(deadline)?;
        let snapshot = service.snapshot(deadline)?;
        validate_snapshot(&snapshot, &self.paths.executable)?;
        same_configuration(&j.disposition.original, &snapshot)?;
        ensure!(
            snapshot.running
                == RunningState::Running {
                    pid: evidence.process().pid
                },
            "manager process differs from independently attested ready process"
        );
        verify_preflight(service, &executable, inputs, deadline)?;
        service.revalidate_running(&evidence, &permit, &executable, deadline)?;
        check_deadline(deadline)?;
        Ok(evidence)
    }
    fn revalidate_native_evidence(
        &self,
        service: &mut impl ServiceControl,
        j: &NativeJournal,
        role: PermitTarget,
        evidence: &service::RunningEvidence,
        deadline: Instant,
    ) -> Result<()> {
        let tuple = if role == PermitTarget::Candidate {
            &j.after
        } else {
            &j.before
        };
        let executable =
            VerifiedExecutable::open(&self.paths.executable, &tuple.generation.artifact, deadline)?;
        let permit = StartupPermit {
            payload: j.make_payload(role)?,
            contract: tuple.contract.clone(),
        };
        evidence.verify(
            j.admission.as_ref().context("missing restart admission")?,
            &permit,
            &executable,
        )?;
        service.revalidate_running(evidence, &permit, &executable, deadline)?;
        executable.revalidate(deadline)
    }

    fn finish_native(&self, j: &NativeJournal, committed: bool) -> Result<()> {
        let retained = RetainedV2::decode(if committed {
            &j.retained_after
        } else {
            &j.retained_before
        })?;
        let referenced = |record: &GenerationRecord| {
            &retained.current == record || retained.previous.as_ref() == Some(record)
        };
        let old = RetainedV2::decode(&j.retained_before)?;
        for record in [&j.after.generation, &j.before.generation]
            .into_iter()
            .chain(old.previous.as_ref())
        {
            // A crash can leave a partial envelope before its final rename.
            // This filename is derived only from a journal-owned proof reference.
            let proof = self.paths.proof(&record.proof)?;
            let pending = proof.with_file_name(format!(
                ".{}.pending",
                proof
                    .file_name()
                    .context("proof filename")?
                    .to_string_lossy()
            ));
            if storage::exists(&pending)? {
                storage::read_proof(&pending)?;
                storage::remove(&pending)?;
            }
            if !referenced(record) {
                let proof = self.paths.proof(&record.proof)?;
                if storage::exists(&proof)? {
                    record.proof.verify(&storage::read_proof(&proof)?)?;
                    storage::remove(&proof)?;
                }
                let artifact = self.paths.artifact(&record.artifact);
                if storage::exists(&artifact)? {
                    storage::verify(&artifact, &record.artifact)?;
                    storage::remove(&artifact)?;
                }
            }
        }
        self.remove_working_files()?;
        storage::remove(&self.paths.journal)
    }
}
fn authenticate_candidate(generation: &AuthenticatedGeneration, trust_path: &Path) -> Result<()> {
    let trust = provenance::IndependentTrustSource::read_bound(trust_path)?;
    let proof = crate::release::descriptor::SignedReleaseProof::decode(generation.proof_bytes())?;
    let current =
        AuthenticatedGeneration::from_proof(proof, trust.keyring(), &generation.record().target)?;
    ensure!(
        same_generation_identity(current.record(), generation.record()),
        "current independent trust differs from candidate generation"
    );
    Ok(())
}

/// Validate a runnable journal view without acquiring the parent's mutation
/// lock. The caller holds shared instance/startup leases until effects begin.
pub fn validate_transaction_start(
    lease: &StartupLease,
    actual_invocation: &InvocationBinding,
    deadline: Instant,
) -> Result<StartupPermit> {
    check_deadline(deadline)?;
    let paths = Paths::existing(lease.home())?;
    ensure!(
        !storage::exists(&lease.home().join(".ww/image-update.json"))?,
        "pending FHS work cannot launch a native transaction child"
    );
    let (j, rollback) = read_native(&paths)?.context("no native transaction launch is recorded")?;
    validate_owned_mixed(&paths, lease, &j)?;
    ensure!(
        j.admission.is_some(),
        "stopped operation cannot admit transaction launches"
    );
    let state = lease.read_state()?.context("startup state missing")?;
    let (tuple, payload, terminal) = match j.phase {
        NativePhase::Starting | NativePhase::Verified | NativePhase::Committed => (
            &j.after,
            j.candidate_permit
                .as_ref()
                .context("candidate permit missing")?,
            j.phase == NativePhase::Committed,
        ),
        NativePhase::Restoring | NativePhase::Restored => (
            &j.before,
            j.restored_permit
                .as_ref()
                .context("restored permit missing")?,
            j.phase == NativePhase::Restored,
        ),
        _ => bail!("native transaction has no runnable phase"),
    };
    ensure!(
        &tuple.contract.invocation == actual_invocation,
        "transaction child does not match the exact managed invocation"
    );
    let runnable = j.runnable(payload.role)?;
    ensure!(
        state == runnable || (terminal && state == j.steady(tuple)),
        "startup state does not publish this transaction permit"
    );
    if terminal {
        verify_tuple(&paths, tuple)?;
        let expected = if payload.role == PermitTarget::Candidate {
            &j.retained_after
        } else {
            &j.retained_before
        };
        ensure!(
            storage::read_private_record(&paths.retained)?.as_ref() == Some(expected),
            "terminal native retention changed"
        );
    }
    if payload.role == PermitTarget::Candidate && !rollback && !terminal {
        current_independent_generation(
            &paths,
            &tuple.generation,
            &tuple.contract.trusted_release_keys,
        )?;
    } else {
        proof_generation(&paths, &tuple.generation)?;
    }
    VerifiedExecutable::open(&paths.executable, &tuple.generation.artifact, deadline)?
        .revalidate(deadline)?;
    payload.validate_binding(
        &runnable,
        &tuple.contract,
        &tuple.digests()?,
        actual_invocation,
    )?;
    Ok(StartupPermit {
        payload: payload.clone(),
        contract: tuple.contract.clone(),
    })
}

impl Engine {
    pub fn recover_without_preparation_until(
        &mut self,
        service: &mut impl ServiceControl,
        deadline: Instant,
    ) -> Result<Option<Outcome>> {
        check_deadline(deadline)?;
        self._lock.reject_pending_image()?;
        let Some(bytes) = storage::read_private_record(&self.paths.journal)? else {
            return Ok(Some(Outcome::NoPendingTransaction));
        };
        let value: serde_json::Value = serde_json::from_slice(&bytes)?;
        if value.get("version").and_then(|v| v.as_u64()) == Some(1) {
            let j = self.pending_recovery()?.context("legacy journal missing")?;
            ensure!(matches!(j.phase,Phase::Staging|Phase::Ready),"post-replacement v1 recovery requires independently authenticated exact generation; no root/proof may be inferred from the descriptor CID");
            storage::verify(&self.paths.executable, &j.previous.artifact)?;
            ensure!(
                storage::read_record(&self.paths.receipt)?.as_ref() == Some(&j.receipt_before),
                "legacy before receipt changed"
            );
            self.finish_aborted(&j)?;
            return Ok(Some(Outcome::Recovered {
                artifact: j.previous.artifact,
                action: RecoveryAction::AbortedBeforeReplacement,
                live_ready: false,
            }));
        }
        let (j, _) = read_native(&self.paths)?.context("journal missing")?;
        if matches!(j.phase, NativePhase::Committed | NativePhase::Restored) {
            let lease = StartupLease::exclusive(self._lock.home())?;
            validate_owned_mixed(&self.paths, &lease, &j)?;
            let committed = j.phase == NativePhase::Committed;
            let tuple = if committed { &j.after } else { &j.before };
            let retained = if committed {
                &j.retained_after
            } else {
                &j.retained_before
            };
            verify_tuple(&self.paths, tuple)?;
            ensure!(
                storage::read_private_record(&self.paths.retained)?.as_ref() == Some(retained)
                    && lease.read_contract()?.as_ref() == Some(&tuple.contract),
                "terminal native tuple is incomplete"
            );
            lease.write_state(&j.steady(tuple))?;
            self.finish_native(&j, committed)?;
            return Ok(Some(Outcome::Recovered {
                artifact: tuple.generation.artifact.clone(),
                action: if committed {
                    RecoveryAction::CompletedCommit
                } else {
                    RecoveryAction::RestoredPrevious
                },
                live_ready: false,
            }));
        }
        // Before any stop intent, an unchanged before tuple can abort without
        // preflight or restarting a service. StopIntent always needs reconciliation.
        if matches!(j.phase, NativePhase::Staging | NativePhase::Ready)
            && !j.disposition.stop_intent
        {
            let now = service.snapshot(deadline)?;
            ensure!(
                now == j.disposition.original,
                "managed disposition changed before early abort"
            );
            let lease = StartupLease::exclusive(self._lock.home())?;
            validate_owned_mixed(&self.paths, &lease, &j)?;
            verify_tuple(&self.paths, &j.before)?;
            ensure!(
                storage::read_private_record(&self.paths.retained)?.as_ref()
                    == Some(&j.retained_before)
                    && lease.read_contract()?.as_ref() == Some(&j.before.contract),
                "before tuple changed prior to replacement"
            );
            lease.write_state(&j.steady(&j.before))?;
            self.finish_native(&j, false)?;
            return Ok(Some(Outcome::Recovered {
                artifact: j.before.generation.artifact,
                action: RecoveryAction::AbortedBeforeReplacement,
                live_ready: false,
            }));
        }
        Ok(None)
    }

    pub fn recover_before(
        &mut self,
        service: &mut impl ServiceControl,
        deadline: Instant,
    ) -> Result<Outcome> {
        self.recover_without_preparation_until(service, deadline)?
            .context("native recovery requires exact captured startup inputs")
    }
    pub fn recover(
        &mut self,
        service: &mut impl ServiceControl,
        timeout: Duration,
    ) -> Result<Outcome> {
        self.recover_before(service, deadline(timeout)?)
    }
    pub fn recover_authenticated_until(
        &mut self,
        service: &mut impl ServiceControl,
        deadline: Instant,
        executable: AuthenticatedGeneration,
    ) -> Result<Outcome> {
        self.validate_recovery_executable(&executable)?;
        self.recover_before(service, deadline)
    }
    pub fn recover_prepared_until(
        &mut self,
        service: &mut impl ServiceControl,
        deadline: Instant,
        inputs: &PreflightInputs,
    ) -> Result<Outcome> {
        if let Some(outcome) = self.recover_without_preparation_until(service, deadline)? {
            return Ok(outcome);
        }
        let (mut j, rollback) =
            read_native(&self.paths)?.context("no pending native transaction")?;
        existing_inputs(&j.before.contract, inputs, self._lock.home())?;
        {
            let lease = StartupLease::exclusive(self._lock.home())?;
            validate_owned_mixed(&self.paths, &lease, &j)?;
            lease.write_state(&j.blocked())?;
        }
        if j.admission.is_some() {
            let role = match j.phase {
                NativePhase::Starting | NativePhase::Verified => Some(PermitTarget::Candidate),
                NativePhase::Restoring => Some(PermitTarget::RestoredBefore),
                _ => None,
            };
            if let Some(role) = role {
                let tuple = if role == PermitTarget::Candidate {
                    &j.after
                } else {
                    &j.before
                };
                j.disposition.quiescence_contract = Some(tuple.contract.clone());
                j.disposition.quiescence_permit = Some(j.make_payload(role)?);
                j.disposition.quiesced = None;
                j.disposition.stop_intent = true;
                j.save(&self.paths, rollback)?;
            }
        }
        // Record stop intent before a recovery manager command as well. A prior
        // timeout does not prove that the original job is still registered.
        if !j.disposition.stop_intent {
            j.disposition.stop_intent = true;
            j.save(&self.paths, rollback)?;
        }
        let quiesced = service.quiesce(&j.disposition, deadline)?;
        ensure!(
            quiesced.original == j.disposition.original
                && quiesced.identity == j.disposition.identity
                && quiesced.original_process == j.disposition.original_process
                && quiesced.original_contract == j.disposition.original_contract
                && quiesced.quiescence_contract == j.disposition.quiescence_contract
                && quiesced.quiescence_permit == j.disposition.quiescence_permit
                && quiesced.stop_intent
                && quiesced.quiesced.is_some(),
            "recovery quiescence changed original ownership"
        );
        quiesced.validate()?;
        j.disposition = quiesced;
        j.save(&self.paths, rollback)?;
        let instance = InstanceLease::exclusive(self._lock.home())?;
        let lease = StartupLease::exclusive(self._lock.home())?;
        validate_owned_mixed(&self.paths, &lease, &j)?;
        ensure!(
            storage::verify(&self.paths.executable, &j.before.generation.artifact).is_ok()
                || storage::verify(&self.paths.executable, &j.after.generation.artifact).is_ok(),
            "installed executable matches neither recorded transaction target"
        );
        proof_generation(&self.paths, &j.before.generation)?;
        let previous = VerifiedExecutable::open(
            &self.paths.artifact(&j.before.generation.artifact),
            &j.before.generation.artifact,
            deadline,
        )?;
        verify_preflight(service, &previous, inputs, deadline)?;
        if let Some(admission) = &j.admission {
            service.revalidate_restart(admission, deadline)?;
        }
        j.phase(&self.paths, rollback, NativePhase::Restoring)?;
        self.install_cached(&j.before.generation.artifact, deadline)?;
        write_tuple(&self.paths, &lease, &j.before, &j.retained_before)?;
        let live = j.admission.is_some();
        let live_evidence = if live {
            lease.write_state(&j.runnable(PermitTarget::RestoredBefore)?)?;
            drop(lease);
            drop(instance);
            Some(self.restart_native(
                service,
                &j,
                PermitTarget::RestoredBefore,
                inputs,
                deadline,
            )?)
        } else {
            ensure!(
                service.snapshot(deadline)?.running == RunningState::Stopped,
                "service started during recovery"
            );
            drop(lease);
            drop(instance);
            None
        };
        let _instance = if live {
            None
        } else {
            Some(InstanceLease::exclusive(self._lock.home())?)
        };
        let lease = StartupLease::exclusive(self._lock.home())?;
        validate_owned_mixed(&self.paths, &lease, &j)?;
        verify_tuple(&self.paths, &j.before)?;
        check_deadline(deadline)?;
        if let Some(evidence) = &live_evidence {
            self.revalidate_native_evidence(
                service,
                &j,
                PermitTarget::RestoredBefore,
                evidence,
                deadline,
            )?;
        }
        j.phase(&self.paths, rollback, NativePhase::Restored)?;
        lease.write_state(&j.steady(&j.before))?;
        self.finish_native(&j, false)?;
        Ok(Outcome::Recovered {
            artifact: j.before.generation.artifact,
            action: RecoveryAction::RestoredPrevious,
            live_ready: live,
        })
    }
}

fn legacy_generation_matches(generation: &GenerationRecord, record: &ArtifactRecord) -> bool {
    generation.artifact == record.artifact
        && generation.descriptor == record.release
        && generation.target == record.platform
        && generation.release_version == record.version
        && generation.revision == record.revision
}
