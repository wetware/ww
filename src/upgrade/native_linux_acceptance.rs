//! Explicit disposable component driver. This is not genuine-release or
//! installed-CLI acceptance. The production NativeService gate remains intact.

use super::*;
use crate::upgrade::{self, service::*, startup::*, Engine};

struct ComponentService {
    home: PathBuf,
}

impl ServiceControl for ComponentService {
    fn snapshot(&mut self, deadline: Instant) -> Result<ServiceSnapshot> {
        super::snapshot(&self.home, deadline)
    }
    fn compatible(
        &mut self,
        executable: &VerifiedExecutable,
        inputs: &PreflightInputs,
        deadline: Instant,
    ) -> Result<CompatibilityEvidence> {
        super::compatible(executable, inputs, deadline)
    }
    fn admit_restart(
        &mut self,
        before: &ServiceSnapshot,
        contract: &StartupContractV1,
        executable: &VerifiedExecutable,
        deadline: Instant,
    ) -> Result<RestartAdmission> {
        super::admit_restart(&self.home, before, contract, executable, deadline)
    }
    fn revalidate_restart(
        &mut self,
        admission: &RestartAdmission,
        deadline: Instant,
    ) -> Result<()> {
        super::revalidate_restart(&self.home, admission, deadline)
    }
    fn quiesce(
        &mut self,
        intent: &ServiceDisposition,
        deadline: Instant,
    ) -> Result<ServiceDisposition> {
        super::quiesce(&self.home, intent, deadline)
    }
    fn restart(
        &mut self,
        admission: &RestartAdmission,
        permit: &upgrade::StartupPermit,
        executable: &VerifiedExecutable,
        deadline: Instant,
    ) -> Result<RunningEvidence> {
        super::restart(&self.home, admission, permit, executable, deadline)
    }
    fn revalidate_running(
        &mut self,
        evidence: &RunningEvidence,
        permit: &upgrade::StartupPermit,
        executable: &VerifiedExecutable,
        deadline: Instant,
    ) -> Result<()> {
        super::revalidate_running(&self.home, evidence, permit, executable, deadline)
    }
    fn stop(&mut self, service: &ServiceIdentity, deadline: Instant) -> Result<()> {
        super::stop(&self.home, service, deadline)
    }
    fn legacy_admit_restart(&mut self, _: &ServiceSnapshot) -> Result<LegacyRestartAdmission> {
        anyhow::bail!("component driver forbids legacy admission")
    }
    fn legacy_revalidate_restart(&mut self, _: &LegacyRestartAdmission) -> Result<()> {
        anyhow::bail!("component driver forbids legacy admission")
    }
    fn legacy_restart(
        &mut self,
        _: &LegacyRestartAdmission,
        _: &upgrade::ArtifactIdentity,
        _: Instant,
    ) -> Result<LegacyRunningEvidence> {
        anyhow::bail!("component driver forbids legacy restart")
    }
    fn legacy_stop(&mut self, _: Instant) -> Result<()> {
        anyhow::bail!("component driver forbids legacy stop")
    }
    fn legacy_compatible(&mut self, _: &upgrade::ArtifactIdentity, _: &Path) -> Result<()> {
        anyhow::bail!("component driver forbids legacy preparation")
    }
}

fn bounded(path: &Path, limit: u64) -> Result<Vec<u8>> {
    let mut bytes = Vec::new();
    std::fs::File::open(path)?
        .take(limit + 1)
        .read_to_end(&mut bytes)?;
    ensure!(
        bytes.len() as u64 <= limit,
        "component fixture exceeds bound"
    );
    Ok(bytes)
}

/// Run one explicitly selected component operation against production Engine
/// state. All permits originate from Engine journal validation.
#[test]
#[ignore = "explicit disposable systemd component fixture only"]
fn source_fixture_component_operation() -> Result<()> {
    ensure!(
        std::env::var("WW739_DISPOSABLE_SYSTEMD").as_deref() == Ok("1")
            && Path::new("/.dockerenv").is_file()
            && unsafe { libc::geteuid() } == 1001
            && std::env::var("HOME").as_deref() == Ok("/home/testww"),
        "component driver requires the isolated ww739 fixture account"
    );
    let home = PathBuf::from("/home/testww");
    let inputs: PreflightInputs =
        serde_json::from_slice(&bounded(&home.join("ww739-inputs.json"), 1024 * 1024)?)?;
    inputs.validate()?;
    let mut service = ComponentService { home: home.clone() };
    let mut engine = Engine::open(&home)?;
    let deadline = Instant::now() + Duration::from_secs(300);
    match std::env::var("WW739_COMPONENT_OPERATION").as_deref() {
        Ok("admit") => {
            let observed = super::observe(&home, deadline)?;
            let guard = engine.begin_admission(&mut service, deadline)?;
            let state = guard.startup_state()?;
            let generation = guard.current_generation()?;
            let previous = guard.startup_contract()?;
            let counter = previous.map_or(Ok(1), |contract| {
                contract
                    .identity
                    .generation
                    .checked_add(1)
                    .context("contract generation overflow")
            })?;
            let contract = StartupContractV1::new(
                state.installation_id,
                counter,
                generation.artifact().clone(),
                inputs.startup_identity()?,
                InvocationBinding::Managed {
                    service: observed.invocation.identity,
                },
                state.trusted_release_keys,
            )?;
            eprintln!(
                "component admission outcome: {:?}",
                guard.finish(&mut service, contract, &inputs, false)?
            );
        }
        Ok("upgrade") => {
            let trust = upgrade::provenance::IndependentTrustSource::read_bound(
                &home.join("ww739-trusted-keys"),
            )?;
            let proof = crate::release::descriptor::SignedReleaseProof::decode(&bounded(
                &home.join("ww739-candidate.proof"),
                1024 * 1024,
            )?)?;
            let candidate = upgrade::provenance::AuthenticatedGeneration::from_proof(
                proof,
                trust.keyring(),
                "aarch64-unknown-linux-gnu",
            )?;
            let request = upgrade::UpgradeRequest {
                installed: engine.current_generation()?,
                candidate,
                bytes: std::fs::File::open(home.join("ww739-candidate"))?,
                inputs,
            };
            eprintln!(
                "component upgrade outcome: {:?}",
                engine.operator_upgrade_until(&mut service, deadline, || Ok(request))?
            );
        }
        Ok("rollback") => {
            eprintln!(
                "component rollback outcome: {:?}",
                engine.operator_rollback_prepared_until(&mut service, deadline, &inputs)?
            );
        }
        _ => anyhow::bail!("select admit, upgrade or rollback component operation"),
    }
    Ok(())
}
