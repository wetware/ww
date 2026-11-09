//! Guarded software removal. The installation marker and signed proofs survive.

use anyhow::{ensure, Context, Result};

use super::startup::{InstanceLease, StartupDisposition, StartupLease, StartupStateV1};
use super::storage;
use crate::installation_state::MutationLock;

/// Records the exact blocked tuple while the caller stops its owned service.
/// Holding the mutation lock excludes another installation writer throughout.
pub struct Removal<'a> {
    mutation: &'a MutationLock,
    state: StartupStateV1,
    receipt: Vec<u8>,
    check: bool,
}

/// Retain both leases until the caller finishes deleting owned software.
pub struct QuiescentRemoval {
    _instance: InstanceLease,
    _startup: StartupLease,
}

/// A legacy installation returns None. Any surviving guarded metadata requires
/// the complete guarded removal checks, including when its marker is missing.
pub fn prepare(mutation: &MutationLock, check: bool) -> Result<Option<Removal<'_>>> {
    mutation.reject_pending()?;
    let home = mutation.home();
    if !super::startup::metadata_present(home)? {
        return Ok(None);
    }
    let lease = if check {
        StartupLease::shared(home)?
    } else {
        StartupLease::exclusive(home)?
    };
    let (mut state, receipt) = validate(mutation, &lease)?;
    if !check && state.disposition != StartupDisposition::Uninstalled {
        state.disposition = StartupDisposition::Blocked;
        lease.write_state(&state)?;
    }
    Ok(Some(Removal {
        mutation,
        state,
        receipt,
        check,
    }))
}

impl Removal<'_> {
    /// Call only after removing/stopping the owned service. A remaining manual
    /// Host retains its shared instance lease, so software removal then refuses.
    pub fn quiesce(self) -> Result<QuiescentRemoval> {
        ensure!(
            !self.check,
            "removal check does not authorize software deletion"
        );
        ensure!(
            matches!(
                self.state.disposition,
                StartupDisposition::Blocked | StartupDisposition::Uninstalled
            ),
            "removal check does not authorize software deletion"
        );
        self.mutation.reject_pending()?;
        // Never wait for instance exclusion while holding the startup lease.
        let instance = InstanceLease::exclusive(self.mutation.home())?;
        let lease = StartupLease::exclusive(self.mutation.home())?;
        let (mut state, receipt) = validate(self.mutation, &lease)?;
        ensure!(
            state == self.state && receipt == self.receipt,
            "guarded removal tuple changed while stopping the service"
        );
        state.disposition = StartupDisposition::Uninstalled;
        lease.write_state(&state)?;
        Ok(QuiescentRemoval {
            _instance: instance,
            _startup: lease,
        })
    }
}

fn validate(mutation: &MutationLock, lease: &StartupLease) -> Result<(StartupStateV1, Vec<u8>)> {
    let state = super::validate_owned_installation(mutation, lease)?;
    let root = mutation.home().join(".ww");
    let receipt = storage::read_private_record(&root.join("installation.json"))?
        .or(storage::read_private_record(
            &root.join("uninstalled-software.json"),
        )?)
        .context("removal ownership receipt is missing")?;
    Ok((state, receipt))
}

#[cfg(test)]
#[path = "removal_crash_tests.rs"]
mod crash_tests;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::upgrade::tests::completed_initialization;

    #[test]
    fn removal_check_preserves_admission_and_running_instance() {
        let (home, engine, _) = completed_initialization();
        let instance = InstanceLease::shared(home.path()).unwrap();
        let original = engine.startup_state().unwrap();
        let check = prepare(engine.mutation_lock(), true).unwrap().unwrap();
        assert_eq!(engine.startup_state().unwrap(), original);
        assert!(check.quiesce().is_err());
        assert_eq!(engine.startup_state().unwrap(), original);
        drop(instance);
    }

    #[test]
    fn removal_blocks_new_starts_and_requires_instance_exit_before_tombstone() {
        let (home, engine, generation) = completed_initialization();
        let instance = InstanceLease::shared(home.path()).unwrap();
        let removal = prepare(engine.mutation_lock(), false).unwrap().unwrap();
        assert!(removal.quiesce().is_err());
        let lease = StartupLease::shared(home.path()).unwrap();
        let blocked = lease.read_state().unwrap().unwrap();
        assert_eq!(blocked.disposition, StartupDisposition::Blocked);
        let marker = lease.marker().unwrap();
        drop(lease);
        assert!(home.path().join(".ww/bin/ww").is_file());
        drop(instance);
        let quiescent = prepare(engine.mutation_lock(), false)
            .unwrap()
            .unwrap()
            .quiesce()
            .unwrap();
        assert!(InstanceLease::shared(home.path()).is_err());
        assert!(StartupLease::shared(home.path()).is_err());
        drop(quiescent);
        let lease = StartupLease::shared(home.path()).unwrap();
        assert_eq!(lease.marker().unwrap(), marker);
        assert_eq!(
            lease.read_state().unwrap().unwrap().disposition,
            StartupDisposition::Uninstalled
        );
        assert_eq!(
            storage::read_proof(&engine.paths.proof(&generation.record().proof).unwrap()).unwrap(),
            generation.proof_bytes()
        );
    }

    #[test]
    fn interrupted_removal_reuses_tombstone_without_recreating_software() {
        let (home, engine, _) = completed_initialization();
        let receipt = std::fs::read(&engine.paths.receipt).unwrap();
        let quiescent = prepare(engine.mutation_lock(), false)
            .unwrap()
            .unwrap()
            .quiesce()
            .unwrap();
        storage::write_record(&home.path().join(".ww/uninstalled-software.json"), &receipt)
            .unwrap();
        for path in [
            &engine.paths.executable,
            &engine.paths.receipt,
            &engine.paths.locator,
        ] {
            storage::remove(path).unwrap();
        }
        drop(quiescent);
        let retry = prepare(engine.mutation_lock(), false)
            .unwrap()
            .unwrap()
            .quiesce()
            .unwrap();
        assert!(!engine.paths.executable.exists());
        assert!(!engine.paths.receipt.exists());
        assert!(!engine.paths.locator.exists());
        drop(retry);
        assert!(home.path().join(".ww-startup.lock").exists());
        assert!(home.path().join(".ww-host-instance.lock").exists());
        assert!(home.path().join(".ww/identity").exists());
    }

    #[test]
    fn missing_proof_refuses_before_blocking_and_conflicting_tombstone_refuses_retry() {
        let (home, engine, generation) = completed_initialization();
        let before = engine.startup_state().unwrap();
        let proof_path = engine.paths.proof(&generation.record().proof).unwrap();
        storage::remove(&proof_path).unwrap();
        assert!(prepare(engine.mutation_lock(), false).is_err());
        let lease = StartupLease::shared(home.path()).unwrap();
        assert_eq!(lease.read_state().unwrap().unwrap(), before);
        drop(lease);
        storage::write_proof(&proof_path, generation.proof_bytes()).unwrap();
        let quiescent = prepare(engine.mutation_lock(), false)
            .unwrap()
            .unwrap()
            .quiesce()
            .unwrap();
        storage::write_record(&home.path().join(".ww/uninstalled-software.json"), b"{}").unwrap();
        drop(quiescent);
        assert!(prepare(engine.mutation_lock(), false).is_err());
        assert!(engine.paths.executable.is_file());
    }
}
