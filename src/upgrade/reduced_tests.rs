use super::tests::{signed_generation, NEW, OLD};
use super::*;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;

#[derive(Serialize, Deserialize)]
struct Managed {
    state: ServiceSnapshot,
    stop_fails: bool,
    ready_fails: bool,
    #[serde(default)]
    first_start_fails: bool,
    unsupported: bool,
    stop_unconfirmed: bool,
    stop_count: usize,
    start_count: usize,
    turnover_after_ready: bool,
    ready_observed: bool,
    #[serde(default)]
    persistent_path: Option<PathBuf>,
}
impl Managed {
    fn persist(&self) -> Result<()> {
        if let Some(path) = &self.persistent_path {
            std::fs::write(path, serde_json::to_vec(self)?)?;
        }
        Ok(())
    }
}
impl ServiceControl for Managed {
    fn snapshot(&mut self, _: Instant) -> Result<ServiceSnapshot> {
        if self.ready_observed && self.turnover_after_ready {
            self.state.running = RunningState::Running { pid: 999 };
            self.turnover_after_ready = false;
        }
        Ok(self.state.clone())
    }
    fn preflight(&mut self, _: &ServiceSnapshot, _: Instant) -> Result<()> {
        ensure!(!self.unsupported, "unsupported readiness configuration");
        Ok(())
    }
    fn quiesce(
        &mut self,
        before: &ServiceSnapshot,
        _: &MutationLock,
        _: Instant,
    ) -> Result<ServiceSnapshot> {
        same_configuration(before, &self.state)?;
        self.stop_count += 1;
        ensure!(!self.stop_fails, "stop failed");
        if !self.stop_unconfirmed {
            self.state.running = RunningState::Stopped;
        }
        self.persist()?;
        Ok(self.state.clone())
    }
    fn verify_ready(&mut self, before: &ServiceSnapshot, _: Instant) -> Result<ServiceSnapshot> {
        same_configuration(before, &self.state)?;
        ensure!(
            !self.state.failed && !self.ready_fails,
            "service is not ready"
        );
        ensure!(
            matches!(self.state.running, RunningState::Running { .. }),
            "service is not running"
        );
        Ok(self.state.clone())
    }
    fn start_ready(
        &mut self,
        before: &ServiceSnapshot,
        _: &MutationLock,
        _: Instant,
    ) -> Result<ServiceSnapshot> {
        same_configuration(before, &self.state)?;
        assert_eq!(self.state.running, RunningState::Stopped);
        self.start_count += 1;
        self.ready_observed = true;
        self.state.running = RunningState::Running {
            pid: 500 + self.start_count as u32,
        };
        self.persist()?;
        ensure!(
            !self.ready_fails && !(self.first_start_fails && self.start_count == 1),
            "readiness failed"
        );
        Ok(self.state.clone())
    }
}
fn setup(running: bool) -> (tempfile::TempDir, UpgradeCoordinator, Managed) {
    storage::faults::configure(None);
    let home = tempfile::tempdir().unwrap();
    for sub in [".ww/bin", ".ww/fhs/bin", ".ww/fhs/boot"] {
        std::fs::create_dir_all(home.path().join(sub)).unwrap();
    }
    for (file, bytes) in [
        ("bin/ww", OLD),
        ("fhs/bin/status.wasm", b"status".as_slice()),
        ("fhs/boot/kernel.wasm", b"kernel".as_slice()),
    ] {
        std::fs::write(home.path().join(".ww").join(file), bytes).unwrap();
    }
    std::fs::set_permissions(
        home.path().join(".ww/bin/ww"),
        std::fs::Permissions::from_mode(0o755),
    )
    .unwrap();
    let receipt = Receipt {
        version: 2,
        hashes: [
            blake3::hash(OLD).to_hex().to_string(),
            blake3::hash(b"kernel").to_hex().to_string(),
            blake3::hash(b"status").to_hex().to_string(),
            String::new(),
            String::new(),
        ],
        previous: None,
        complete: true,
    };
    storage::write_record(
        &home.path().join(".ww/installation.json"),
        &receipt.encode().unwrap(),
    )
    .unwrap();
    let mut owner = UpgradeCoordinator::open(home.path()).unwrap();
    owner
        .initialize_completed_installation(&signed_generation(OLD, 1))
        .unwrap();
    let service = Managed {
        state: ServiceSnapshot {
            registered: true,
            enabled: EnabledState::Disabled,
            running: if running {
                RunningState::Running { pid: 400 }
            } else {
                RunningState::Stopped
            },
            failed: false,
            executable: Some(home.path().join(".ww/bin/ww")),
            configuration_identity: "unchanged custom unit".into(),
        },
        stop_fails: false,
        ready_fails: false,
        first_start_fails: false,
        unsupported: false,
        stop_unconfirmed: false,
        stop_count: 0,
        start_count: 0,
        turnover_after_ready: false,
        ready_observed: false,
        persistent_path: None,
    };
    (home, owner, service)
}
fn request() -> UpgradeRequest<&'static [u8]> {
    UpgradeRequest {
        installed: signed_generation(OLD, 1),
        candidate: signed_generation(NEW, 2),
        bytes: NEW,
    }
}
fn end() -> Instant {
    Instant::now() + Duration::from_secs(30)
}
fn upgrade(owner: &mut UpgradeCoordinator, service: &mut Managed) -> Result<Outcome> {
    owner.operator_upgrade_until(service, end(), || Ok(request()))
}

#[test]
fn completed_installer_does_not_create_startup_authority() {
    let (_home, owner, _) = setup(false);
    assert!(!owner.paths.store.join("startup-state.json").exists());
    assert!(!owner.paths.store.join("startup-contract.json").exists());
    assert_eq!(
        owner.current_generation().unwrap().record(),
        signed_generation(OLD, 1).record()
    );
}
#[test]
fn reduced_forward_rollback_preserve_stopped_and_disabled_running_dispositions() {
    for running in [false, true] {
        let (_home, mut owner, mut service) = setup(running);
        assert_eq!(
            upgrade(&mut owner, &mut service).unwrap(),
            Outcome::Installed {
                live_ready: running
            }
        );
        assert_eq!(std::fs::read(&owner.paths.executable).unwrap(), NEW);
        assert_eq!(service.state.enabled, EnabledState::Disabled);
        assert_eq!(
            owner.operator_rollback_until(&mut service, end()).unwrap(),
            Outcome::Installed {
                live_ready: running
            }
        );
        assert_eq!(std::fs::read(&owner.paths.executable).unwrap(), OLD);
        assert_eq!(service.start_count, if running { 2 } else { 0 });
        assert!(!owner.paths.journal.exists());
    }
}
#[test]
fn failed_or_unconfirmed_stop_never_replaces_executable() {
    for failed in [false, true] {
        let (_home, mut owner, mut service) = setup(true);
        service.stop_fails = failed;
        service.stop_unconfirmed = !failed;
        assert!(upgrade(&mut owner, &mut service).is_err());
        assert_eq!(std::fs::read(&owner.paths.executable).unwrap(), OLD);
        assert!(owner.paths.journal.exists());
        service.stop_fails = false;
        service.stop_unconfirmed = false;
        assert!(matches!(
            owner.recover_until(&mut service, end()).unwrap(),
            Outcome::Recovered {
                action: RecoveryAction::RestoredPrevious,
                live_ready: true,
                ..
            }
        ));
    }
}
#[test]
fn unsupported_readiness_or_unmanaged_service_refuses_before_journal_or_candidate_stage() {
    for unmanaged in [false, true] {
        let (_home, mut owner, mut service) = setup(true);
        service.unsupported = !unmanaged;
        if unmanaged {
            service.state.registered = false;
            service.state.running = RunningState::Stopped;
            service.state.executable = None;
        }
        assert!(upgrade(&mut owner, &mut service).is_err());
        assert!(!owner.paths.journal.exists());
        assert!(!owner
            .paths
            .artifact(signed_generation(NEW, 2).artifact())
            .exists());
        assert_eq!(service.stop_count, 0);
    }
}
#[test]
fn failed_rollback_retains_journal_for_explicit_recovery() {
    let (_home, mut owner, mut service) = setup(true);
    service.ready_fails = true;
    assert!(upgrade(&mut owner, &mut service).is_err());
    assert_eq!(std::fs::read(&owner.paths.executable).unwrap(), OLD);
    assert_eq!(service.state.running, RunningState::Stopped);
    assert!(owner.recover_until(&mut service, end()).is_err());
    assert_eq!(std::fs::read(&owner.paths.executable).unwrap(), OLD);
    assert!(owner.paths.journal.exists());
    service.ready_fails = false;
    assert!(matches!(
        owner.recover_until(&mut service, end()).unwrap(),
        Outcome::Recovered {
            action: RecoveryAction::RestoredPrevious,
            live_ready: true,
            ..
        }
    ));
    assert!(!owner.paths.journal.exists());
}
#[test]
fn corrupt_retained_rollback_refuses_before_replacement() {
    let (_home, mut owner, mut service) = setup(false);
    upgrade(&mut owner, &mut service).unwrap();
    std::fs::write(
        owner.paths.artifact(signed_generation(OLD, 1).artifact()),
        b"corrupt",
    )
    .unwrap();
    assert!(owner.operator_rollback_until(&mut service, end()).is_err());
    assert_eq!(std::fs::read(&owner.paths.executable).unwrap(), NEW);
    assert!(!owner.paths.journal.exists());
}
#[test]
fn exact_artifact_noop_keeps_provenance_service_and_previous_generation() {
    let (_home, mut owner, mut service) = setup(true);
    let before = std::fs::read(&owner.paths.retained).unwrap();
    assert_eq!(
        owner
            .operator_upgrade_until(&mut service, end(), || Ok(UpgradeRequest {
                installed: signed_generation(OLD, 1),
                candidate: signed_generation(OLD, 9),
                bytes: OLD
            }))
            .unwrap(),
        Outcome::AlreadyInstalled
    );
    assert_eq!(std::fs::read(&owner.paths.retained).unwrap(), before);
    assert_eq!((service.start_count, service.stop_count), (0, 0));
}
#[test]
fn damaged_journal_repair_authenticates_existing_bytes_archives_evidence_and_requires_ack() {
    let (_home, mut owner, mut service) = setup(true);
    storage::write_record(&owner.paths.journal, b"{broken").unwrap();
    assert!(upgrade(&mut owner, &mut service).is_err());
    assert!(owner
        .repair_baseline_until(end(), &signed_generation(NEW, 2), true)
        .is_err());
    assert!(owner
        .repair_baseline_until(end(), &signed_generation(OLD, 1), false)
        .is_err());
    let repair = owner
        .repair_baseline_until(end(), &signed_generation(OLD, 1), true)
        .unwrap();
    assert_eq!(
        std::fs::read(repair.archive.join("journal")).unwrap(),
        b"{broken"
    );
    assert!(repair.archive.join("manifest.json").exists());
    assert!(!repair.historical_rollback_available && !repair.live_ready);
    assert_eq!(std::fs::read(&owner.paths.executable).unwrap(), OLD);
    assert_eq!((service.start_count, service.stop_count), (0, 0));
    upgrade(&mut owner, &mut service).unwrap();
    owner.operator_rollback_until(&mut service, end()).unwrap();
    assert_eq!(std::fs::read(&owner.paths.executable).unwrap(), OLD);
}
#[test]
fn repair_preserves_authenticated_previous_generation_and_all_other_artifacts() {
    let (_home, mut owner, mut service) = setup(false);
    upgrade(&mut owner, &mut service).unwrap();
    let unrelated = owner.paths.store.join("preserve-unknown-artifact");
    std::fs::write(&unrelated, b"historical evidence").unwrap();
    storage::write_record(&owner.paths.journal, b"{\"version\":99}").unwrap();
    let result = owner
        .repair_baseline_until(end(), &signed_generation(NEW, 2), false)
        .unwrap();
    assert!(result.historical_rollback_available);
    assert_eq!(std::fs::read(&unrelated).unwrap(), b"historical evidence");
    owner.operator_rollback_until(&mut service, end()).unwrap();
}
#[test]
fn candidate_copy_checks_size_digest_and_keeps_installed_bytes() {
    for bytes in [
        b"bad".as_slice(),
        b"new native bytes x".as_slice(),
        b"new native byteX".as_slice(),
    ] {
        let (_home, mut owner, mut service) = setup(false);
        assert!(owner
            .operator_upgrade_until(&mut service, end(), || Ok(UpgradeRequest {
                installed: signed_generation(OLD, 1),
                candidate: signed_generation(NEW, 2),
                bytes
            }))
            .is_err());
        assert_eq!(std::fs::read(&owner.paths.executable).unwrap(), OLD);
        assert!(!owner.paths.journal.exists());
    }
}
#[test]
fn native_faults_at_every_durable_boundary_recover_idempotently() {
    for running in [false, true] {
        let (_home, mut owner, mut service) = setup(running);
        storage::faults::configure(None);
        upgrade(&mut owner, &mut service).unwrap();
        let trace = storage::faults::trace();
        for (point, label) in trace.iter().enumerate() {
            let (_home, mut owner, mut service) = setup(running);
            storage::faults::configure(Some(point + 1));
            let _ = upgrade(&mut owner, &mut service);
            storage::faults::configure(None);
            owner
                .recover_until(&mut service, end())
                .unwrap_or_else(|e| panic!("running={running} point={point} {label}: {e:#}"));
            assert_eq!(
                owner.recover_until(&mut service, end()).unwrap(),
                Outcome::NoPendingTransaction
            );
            let bytes = std::fs::read(&owner.paths.executable).unwrap();
            assert!(bytes == OLD || bytes == NEW);
            owner.current_generation().unwrap();
        }
    }
}
#[test]
fn terminal_recovery_requires_recorded_disposition() {
    let (_home, mut owner, mut service) = setup(true);
    storage::faults::configure(None);
    upgrade(&mut owner, &mut service).unwrap();
    let point = storage::faults::trace()
        .iter()
        .position(|label| label == "native.phase.Committed")
        .unwrap()
        + 1;
    for running in [false, true] {
        let (_home, mut owner, mut service) = setup(running);
        // The stopped path has fewer checkpoints; locate its own commit point.
        storage::faults::configure(None);
        upgrade(&mut owner, &mut service).unwrap();
        let commit = if running {
            point
        } else {
            storage::faults::trace()
                .iter()
                .position(|label| label == "native.phase.Committed")
                .unwrap()
                + 1
        };
        let (_home, mut owner, mut service) = setup(running);
        storage::faults::configure(Some(commit));
        assert!(upgrade(&mut owner, &mut service).is_err());
        storage::faults::configure(None);
        let journal = std::fs::read(&owner.paths.journal).unwrap();
        if running {
            service.ready_fails = true;
        } else {
            service.state.running = RunningState::Running { pid: 987 };
        }
        assert!(owner.recover_until(&mut service, end()).is_err());
        assert_eq!(std::fs::read(&owner.paths.journal).unwrap(), journal);
        assert_eq!(std::fs::read(&owner.paths.executable).unwrap(), NEW);
    }
}
#[test]
fn baseline_repair_faults_preserve_damaged_evidence_and_resume() {
    let (_home, mut owner, _) = setup(false);
    storage::write_record(&owner.paths.journal, b"damaged").unwrap();
    storage::faults::configure(None);
    owner
        .repair_baseline_until(end(), &signed_generation(OLD, 1), true)
        .unwrap();
    let trace = storage::faults::trace();
    for (point, label) in trace.iter().enumerate() {
        let (_home, mut owner, _) = setup(false);
        storage::write_record(&owner.paths.journal, b"damaged").unwrap();
        storage::faults::configure(Some(point + 1));
        let _ = owner.repair_baseline_until(end(), &signed_generation(OLD, 1), true);
        storage::faults::configure(None);
        owner
            .repair_baseline_until(end(), &signed_generation(OLD, 1), true)
            .unwrap_or_else(|e| panic!("repair point={point} {label}: {e:#}"));
        assert_eq!(std::fs::read(&owner.paths.executable).unwrap(), OLD);
        owner.current_generation().unwrap();
        assert!(std::fs::read_dir(&owner.paths.store)
            .unwrap()
            .filter_map(Result::ok)
            .any(|e| std::fs::read(e.path().join("journal")).ok().as_deref()
                == Some(b"damaged".as_slice())));
    }
}

#[test]
fn baseline_repair_cannot_discard_a_known_recoverable_transaction() {
    let (_home, mut owner, mut service) = setup(true);
    service.ready_fails = true;
    assert!(upgrade(&mut owner, &mut service).is_err());
    let journal = std::fs::read(&owner.paths.journal).unwrap();
    assert!(owner
        .repair_baseline_until(end(), &signed_generation(NEW, 2), true)
        .is_err());
    assert_eq!(std::fs::read(&owner.paths.journal).unwrap(), journal);
}
#[test]
fn baseline_repair_preserves_oversized_damaged_records_without_parsing_them() {
    let (_home, mut owner, _) = setup(false);
    let damaged = vec![b'x'; storage::MAX_RECORD + 1];
    std::fs::write(&owner.paths.retained, &damaged).unwrap();
    let repair = owner
        .repair_baseline_until(end(), &signed_generation(OLD, 1), true)
        .unwrap();
    assert_eq!(
        std::fs::read(repair.archive.join("retained")).unwrap(),
        damaged
    );
}

fn crash_action(owner: &mut UpgradeCoordinator, service: &mut Managed, action: &str) -> Result<()> {
    match action {
        "upgrade" => {
            upgrade(owner, service)?;
        }
        "rollback" => {
            owner.operator_rollback_until(service, end())?;
        }
        "recover" => {
            owner.recover_until(service, end())?;
        }
        "repair" => {
            owner.repair_baseline_until(end(), &signed_generation(OLD, 1), true)?;
        }
        _ => panic!("unknown reduced crash action"),
    }
    Ok(())
}
fn crash_setup(action: &str) -> (tempfile::TempDir, UpgradeCoordinator, Managed) {
    let (home, mut owner, mut service) = setup(true);
    if action == "rollback" {
        upgrade(&mut owner, &mut service).unwrap();
    }
    if action == "recover" {
        service.ready_fails = true;
        assert!(upgrade(&mut owner, &mut service).is_err());
        service.ready_fails = false;
    }
    if action == "repair" {
        storage::write_record(&owner.paths.journal, b"damaged").unwrap();
    }
    std::fs::write(
        home.path().join("service-fixture.json"),
        serde_json::to_vec(&service).unwrap(),
    )
    .unwrap();
    (home, owner, service)
}
#[test]
#[ignore = "subprocess fixture used by reduced crash matrix"]
fn reduced_crash_worker() {
    let home = PathBuf::from(std::env::var_os("WW_REDUCED_CRASH_HOME").unwrap());
    let mut owner = UpgradeCoordinator::open(&home).unwrap();
    let mut service: Managed =
        serde_json::from_slice(&std::fs::read(home.join("service-fixture.json")).unwrap()).unwrap();
    service.persistent_path = Some(home.join("service-fixture.json"));
    let point: usize = std::env::var("WW_REDUCED_CRASH_POINT")
        .unwrap()
        .parse()
        .unwrap();
    storage::faults::exit_at(point);
    crash_action(
        &mut owner,
        &mut service,
        &std::env::var("WW_REDUCED_CRASH_ACTION").unwrap(),
    )
    .unwrap();
    panic!("crash point not reached");
}
#[test]
fn reduced_process_exit_covers_each_phase_and_durable_operation_kind() {
    super::tests::run_isolated_crash_matrix(
        "upgrade::reduced_tests::reduced_crash_matrix_worker",
        &["reduced crash matrix complete"],
        Duration::from_secs(300),
    );
}
#[test]
#[ignore = "isolated matrix worker bounds all subprocess diagnostics and execution"]
fn reduced_crash_matrix_worker() {
    for action in ["upgrade", "rollback", "recover", "repair"] {
        let (_home, mut owner, mut service) = crash_setup(action);
        storage::faults::configure(None);
        crash_action(&mut owner, &mut service, action).unwrap();
        let trace = storage::faults::trace();
        let mut labels = std::collections::BTreeSet::new();
        for (point, label) in trace.iter().enumerate() {
            // Every distinct durable filesystem boundary and every transaction
            // phase gets a real process exit. The fault matrix covers repeated
            // occurrences at each metadata destination as well.
            if !labels.insert(label.clone()) {
                continue;
            }
            let (home, owner, _service) = crash_setup(action);
            drop(owner);
            let output = std::process::Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "upgrade::reduced_tests::reduced_crash_worker",
                    "--ignored",
                    "--nocapture",
                ])
                .env("WW_REDUCED_CRASH_HOME", home.path())
                .env("WW_REDUCED_CRASH_POINT", (point + 1).to_string())
                .env("WW_REDUCED_CRASH_ACTION", action)
                .output()
                .unwrap();
            assert_eq!(
                output.status.code(),
                Some(86),
                "{action} {label}: {}",
                String::from_utf8_lossy(&output.stderr)
            );
            storage::faults::configure(None);
            let mut service: Managed = serde_json::from_slice(
                &std::fs::read(home.path().join("service-fixture.json")).unwrap(),
            )
            .unwrap();
            let mut owner = UpgradeCoordinator::open(home.path()).unwrap();
            if action == "repair" {
                owner
                    .repair_baseline_until(end(), &signed_generation(OLD, 1), true)
                    .unwrap_or_else(|e| panic!("{action} {label}: {e:#}"));
            } else {
                owner
                    .recover_until(&mut service, end())
                    .unwrap_or_else(|e| panic!("{action} {label}: {e:#}"));
            }
            assert!(!owner.paths.journal.exists(), "{action} {label}");
            owner
                .current_generation()
                .unwrap_or_else(|e| panic!("{action} {label}: {e:#}"));
            assert_eq!(
                owner.recover_until(&mut service, end()).unwrap(),
                Outcome::NoPendingTransaction
            );
        }
        eprintln!(
            "{action}: {} distinct real process-exit boundaries",
            labels.len()
        );
    }
    eprintln!("reduced crash matrix complete");
}

#[test]
fn process_turnover_after_ready_prevents_commit_and_rolls_back() {
    let (_home, mut owner, mut service) = setup(true);
    service.turnover_after_ready = true;
    assert!(upgrade(&mut owner, &mut service).is_err());
    assert!(!owner.paths.journal.exists());
    assert_eq!(std::fs::read(&owner.paths.executable).unwrap(), OLD);
    assert_eq!(
        owner.current_generation().unwrap().artifact(),
        signed_generation(OLD, 1).artifact()
    );
    assert!(matches!(
        service.state.running,
        RunningState::Running { .. }
    ));
}

#[test]
fn baseline_repair_archives_and_replaces_damaged_proof_from_independent_evidence() {
    let (_home, mut owner, _) = setup(false);
    let selected = signed_generation(OLD, 1);
    let proof = owner.paths.proof(&selected.record().proof).unwrap();
    storage::write_record(&proof, b"damaged signed proof").unwrap();
    let repaired = owner.repair_baseline_until(end(), &selected, true).unwrap();
    assert_eq!(
        std::fs::read(repaired.archive.join("proof")).unwrap(),
        b"damaged signed proof"
    );
    assert_eq!(
        owner.current_generation().unwrap().record(),
        selected.record()
    );
}

#[test]
fn repair_preserves_interrupted_metadata_temporary_files_before_reuse() {
    let (_home, mut owner, _) = setup(false);
    let pending = owner
        .paths
        .journal
        .with_file_name(".host-upgrade.json.pending");
    std::fs::write(&pending, b"interrupted damaged evidence").unwrap();
    let repaired = owner
        .repair_baseline_until(end(), &signed_generation(OLD, 1), true)
        .unwrap();
    assert_eq!(
        std::fs::read(repaired.archive.join("journal-pending")).unwrap(),
        b"interrupted damaged evidence"
    );
}

#[test]
fn live_upgrade_failure_attempts_rollback_before_returning_error() {
    let (_home, mut owner, mut service) = setup(true);
    service.ready_fails = true;
    assert!(upgrade(&mut owner, &mut service).is_err());
    assert_eq!(std::fs::read(&owner.paths.executable).unwrap(), OLD);
    assert!(owner.paths.journal.exists()); // Previous startup also fails in this fixture.
}

#[test]
fn live_upgrade_failure_restores_ready_previous_generation_but_returns_error() {
    let (_home, mut owner, mut service) = setup(true);
    service.first_start_fails = true;
    let error = upgrade(&mut owner, &mut service).unwrap_err();
    assert!(format!("{error:#}").contains("readiness failed"));
    assert_eq!(std::fs::read(&owner.paths.executable).unwrap(), OLD);
    assert!(matches!(
        service.state.running,
        RunningState::Running { .. }
    ));
    assert_eq!(service.start_count, 2);
    assert!(!owner.paths.journal.exists());
    assert_eq!(
        owner.current_generation().unwrap().artifact(),
        signed_generation(OLD, 1).artifact()
    );
}
