use super::*;
use anyhow::bail;
use service::{RunningEvidence, StartupIdentity};
use sha2::Digest;
use std::io::{Cursor, Error, ErrorKind};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;

const OLD: &[u8] = b"old native bytes";
const NEW: &[u8] = b"new native bytes";
const THIRD: &[u8] = b"third native bytes";
const TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Serialize, Deserialize)]
struct FakeService {
    snapshot: ServiceSnapshot,
    startup: StartupIdentity,
    incompatible: Option<ArtifactIdentity>,
    admission_unavailable: bool,
    pending_before_restart: Option<StartupIdentity>,
    pending_in_restart: bool,
    restart_failure: bool,
    readiness_timeout: bool,
    wrong_running_artifact: bool,
    not_ready: bool,
    stop_failure: bool,
    restarts: usize,
    stops: usize,
    effects: usize,
    next_pid: u32,
    make_incompatible_on_restart: bool,
    persistent_state: Option<PathBuf>,
    running_artifact: Option<ArtifactIdentity>,
    delay_final_compatibility: bool,
    #[serde(skip)]
    restart_deadline: Option<Instant>,
}

impl FakeService {
    fn absent() -> Self {
        Self {
            snapshot: ServiceSnapshot {
                registered: false,
                enabled: EnabledState::Disabled,
                running: RunningState::Stopped,
                failed: false,
                executable: None,
                configuration_identity: "absent".into(),
            },
            startup: StartupIdentity {
                configuration: "captured-config".into(),
                fhs_baseline: Some("captured-fhs".into()),
                frozen_sources: "captured-local-sources".into(),
                publisher_target: Some("captured-publisher-target".into()),
            },
            incompatible: None,
            admission_unavailable: false,
            pending_before_restart: None,
            pending_in_restart: false,
            restart_failure: false,
            readiness_timeout: false,
            wrong_running_artifact: false,
            not_ready: false,
            stop_failure: false,
            restarts: 0,
            stops: 0,
            effects: 0,
            next_pid: 322,
            make_incompatible_on_restart: false,
            persistent_state: None,
            running_artifact: None,
            delay_final_compatibility: false,
            restart_deadline: None,
        }
    }

    fn running(home: &Path) -> Self {
        let mut service = Self::absent();
        service.snapshot = ServiceSnapshot {
            registered: true,
            enabled: EnabledState::Enabled,
            running: RunningState::Running { pid: 321 },
            failed: false,
            executable: Some(home.join(".ww/bin/ww")),
            configuration_identity: "operator-custom-service-and-dropins".into(),
        };
        service.running_artifact = Some(selected(OLD).artifact);
        service
    }

    fn save_state(&self) -> Result<()> {
        if let Some(path) = &self.persistent_state {
            std::fs::write(path, serde_json::to_vec(self)?)?;
        }
        Ok(())
    }

    fn simulated_manager(home: &Path) -> Self {
        let path = home.join("simulated-service-manager.json");
        if path.exists() {
            return serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap();
        }
        let mut service = Self::running(home);
        service.persistent_state = Some(path);
        service.save_state().unwrap();
        service
    }
}

impl ServiceControl for FakeService {
    fn snapshot(&mut self) -> Result<ServiceSnapshot> {
        Ok(self.snapshot.clone())
    }

    fn admit_restart(&mut self, before: &ServiceSnapshot) -> Result<RestartAdmission> {
        if self.admission_unavailable {
            bail!("running Host has no captured startup evidence; explicitly stop service");
        }
        let RunningState::Running { pid } = before.running else {
            bail!("not running");
        };
        let admission = RestartAdmission {
            startup: self.startup.clone(),
            configuration_identity: before.configuration_identity.clone(),
            prior_pid: pid,
        };
        if let Some(pending) = self.pending_before_restart.take() {
            self.startup = pending;
        }
        Ok(admission)
    }

    fn revalidate_restart(&mut self, admission: &RestartAdmission) -> Result<()> {
        ensure!(
            admission.startup == self.startup,
            "prepared startup inputs changed"
        );
        Ok(())
    }

    fn restart(
        &mut self,
        admission: &RestartAdmission,
        expected: &ArtifactIdentity,
        deadline: Instant,
    ) -> Result<RunningEvidence> {
        self.restart_deadline = Some(deadline);
        self.restarts += 1;
        if self.pending_in_restart {
            self.startup.publisher_target = Some("pending-publisher-target".into());
        }
        // Model the required child-side check before publication or guest effects.
        self.revalidate_restart(admission)?;
        if self.restart_failure {
            bail!("service manager restart failed");
        }
        self.next_pid += 1;
        self.snapshot.running = RunningState::Running { pid: self.next_pid };
        self.effects += 1;
        self.running_artifact = Some(if self.wrong_running_artifact {
            selected(OLD).artifact
        } else {
            expected.clone()
        });
        if self.make_incompatible_on_restart {
            self.incompatible = Some(expected.clone());
        }
        self.save_state()?;
        if self.readiness_timeout {
            bail!("serving readiness deadline expired");
        }
        Ok(RunningEvidence {
            pid: self.next_pid,
            artifact: if self.wrong_running_artifact {
                selected(OLD).artifact
            } else {
                expected.clone()
            },
            startup: self.startup.clone(),
            serving_ready: !self.not_ready,
        })
    }

    fn stop(&mut self, _: Instant) -> Result<()> {
        self.stops += 1;
        if self.stop_failure {
            bail!("service manager stop failed");
        }
        self.snapshot.running = RunningState::Stopped;
        self.snapshot.failed = false;
        self.running_artifact = None;
        self.save_state()?;
        Ok(())
    }

    fn compatible(&mut self, artifact: &ArtifactIdentity, executable: &Path) -> Result<()> {
        storage::verify(executable, artifact)?;
        if self.delay_final_compatibility {
            if let Some(deadline) = self.restart_deadline {
                std::thread::sleep(
                    deadline.saturating_duration_since(Instant::now()) + Duration::from_millis(2),
                );
            }
        }
        ensure!(
            self.incompatible.as_ref() != Some(artifact),
            "Host/PID0 compatibility mismatch"
        );
        Ok(())
    }
}

fn selected(bytes: &[u8]) -> PreverifiedCandidate {
    PreverifiedCandidate {
        artifact: ArtifactIdentity {
            algorithm: DigestAlgorithm::Sha256,
            digest: hex::encode(sha2::Sha256::digest(bytes)),
            size: bytes.len() as u64,
        },
        release: "authenticated-test-release".into(),
        version: "same-version".into(),
        revision: "same-source-revision".into(),
        platform: "test-native".into(),
    }
}

fn request(old: &'static [u8], new: &'static [u8]) -> UpgradeRequest<&'static [u8]> {
    UpgradeRequest {
        installed: selected(old),
        candidate: selected(new),
        bytes: new,
    }
}

fn fixture() -> tempfile::TempDir {
    storage::faults::configure(None);
    let home = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(home.path().join(".ww/bin")).unwrap();
    std::fs::write(home.path().join(".ww/bin/ww"), OLD).unwrap();
    let receipt = Receipt {
        version: 1,
        hashes: [
            blake3::hash(OLD).to_hex().to_string(),
            "kernel".into(),
            "status".into(),
            "namespace".into(),
            "service".into(),
        ],
        previous: None,
        complete: true,
    };
    std::fs::write(
        home.path().join(".ww/installation.json"),
        receipt.encode().unwrap(),
    )
    .unwrap();
    for name in [
        "identity",
        "publisher-state",
        "follower-watermark",
        "authoritative-head",
        "fhs",
        "namespace",
        "custom-service",
        "custom-dropin",
    ] {
        std::fs::write(
            home.path().join(format!(".ww/{name}")),
            format!("operator-owned {name}"),
        )
        .unwrap();
    }
    home
}

fn unchanged_user_state(home: &Path) {
    for name in [
        "identity",
        "publisher-state",
        "follower-watermark",
        "authoritative-head",
        "fhs",
        "namespace",
        "custom-service",
        "custom-dropin",
    ] {
        assert_eq!(
            std::fs::read(home.join(format!(".ww/{name}"))).unwrap(),
            format!("operator-owned {name}").as_bytes()
        );
    }
    let receipt = Receipt::load(&home.join(".ww")).unwrap();
    assert_eq!(
        &receipt.hashes[1..],
        ["kernel", "status", "namespace", "service"]
    );
}

fn open(home: &Path) -> Engine {
    // Parallel process tests may briefly inherit another test thread's flock
    // descriptor until exec. Retry only after the intended owner was dropped.
    let start = Instant::now();
    loop {
        match Engine::open(home) {
            Ok(engine) => return engine,
            Err(error)
                if error
                    .to_string()
                    .contains("installation mutation already in progress")
                    && start.elapsed() < Duration::from_secs(2) =>
            {
                std::thread::sleep(Duration::from_millis(5))
            }
            Err(error) => panic!("open engine: {error:#}"),
        }
    }
}

fn install(
    home: &Path,
    service: &mut FakeService,
    old: &'static [u8],
    new: &'static [u8],
) -> Outcome {
    open(home)
        .operator_upgrade(service, TIMEOUT, || Ok(request(old, new)))
        .unwrap()
}

fn disk_matches_receipt(home: &Path) -> Vec<u8> {
    let bytes = std::fs::read(home.join(".ww/bin/ww")).unwrap();
    let receipt = Receipt::load(&home.join(".ww")).unwrap();
    assert_eq!(
        receipt.executable_digest(),
        blake3::hash(&bytes).to_hex().as_str()
    );
    bytes
}

#[test]
fn same_version_different_bytes_replaces_native_only() {
    let home = fixture();
    let mut service = FakeService::absent();
    assert_eq!(
        install(home.path(), &mut service, OLD, NEW),
        Outcome::Installed { live_ready: false }
    );
    assert_eq!(disk_matches_receipt(home.path()), NEW);
    let executable = std::fs::metadata(home.path().join(".ww/bin/ww")).unwrap();
    assert_eq!(executable.permissions().mode() & 0o777, 0o755);
    assert_eq!((service.restarts, service.stops), (0, 0));
    unchanged_user_state(home.path());
}

#[test]
fn exact_artifact_is_noop_without_reading_candidate_stream() {
    struct MustNotRead;
    impl Read for MustNotRead {
        fn read(&mut self, _: &mut [u8]) -> std::io::Result<usize> {
            panic!("no-op read candidate bytes");
        }
    }
    let home = fixture();
    let original = std::fs::read(home.path().join(".ww/installation.json")).unwrap();
    let result = open(home.path()).operator_upgrade(&mut FakeService::absent(), TIMEOUT, || {
        Ok(UpgradeRequest {
            installed: selected(OLD),
            candidate: selected(OLD),
            bytes: MustNotRead,
        })
    });
    assert_eq!(result.unwrap(), Outcome::AlreadyInstalled);
    assert_eq!(
        std::fs::read(home.path().join(".ww/installation.json")).unwrap(),
        original
    );
    assert!(!home.path().join(".ww/host-upgrade.json").exists());
}

#[test]
fn invalid_digest_algorithm_size_and_platform_leave_installed_bytes_unchanged() {
    for invalid in 0..5 {
        let home = fixture();
        let mut candidate = selected(NEW);
        match invalid {
            0 => candidate.artifact.digest = "0".repeat(64),
            1 => candidate.artifact.algorithm = DigestAlgorithm::Blake3,
            2 => candidate.artifact.size += 1,
            3 => candidate.artifact.size -= 1,
            4 => candidate.platform = "other-platform".into(),
            _ => unreachable!(),
        }
        let result =
            open(home.path()).operator_upgrade(&mut FakeService::absent(), TIMEOUT, || {
                Ok(UpgradeRequest {
                    installed: selected(OLD),
                    candidate,
                    bytes: NEW,
                })
            });
        assert!(result.is_err(), "invalid case {invalid}");
        assert_eq!(disk_matches_receipt(home.path()), OLD);
        open(home.path())
            .recover(&mut FakeService::absent(), TIMEOUT)
            .unwrap();
        unchanged_user_state(home.path());
    }
    assert!(serde_json::from_str::<ArtifactIdentity>(
        r#"{"algorithm":"inferred","digest":"00","size":1}"#
    )
    .is_err());
}

#[test]
fn explicitly_selected_blake3_is_verified_as_blake3() {
    let home = fixture();
    let mut candidate = selected(NEW);
    candidate.artifact.algorithm = DigestAlgorithm::Blake3;
    candidate.artifact.digest = blake3::hash(NEW).to_hex().to_string();
    open(home.path())
        .operator_upgrade(&mut FakeService::absent(), TIMEOUT, || {
            Ok(UpgradeRequest {
                installed: selected(OLD),
                candidate,
                bytes: NEW,
            })
        })
        .unwrap();
    assert_eq!(disk_matches_receipt(home.path()), NEW);
}

#[test]
fn partial_download_and_io_error_leave_installed_executable_unchanged() {
    struct Interrupted {
        first: bool,
    }
    impl Read for Interrupted {
        fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
            if self.first {
                self.first = false;
                buffer[..3].copy_from_slice(b"new");
                Ok(3)
            } else {
                Err(Error::new(
                    ErrorKind::ConnectionReset,
                    "download interrupted",
                ))
            }
        }
    }
    let home = fixture();
    let result = open(home.path()).operator_upgrade(&mut FakeService::absent(), TIMEOUT, || {
        Ok(UpgradeRequest {
            installed: selected(OLD),
            candidate: selected(NEW),
            bytes: Interrupted { first: true },
        })
    });
    assert!(format!("{:#}", result.unwrap_err()).contains("selected native artifact"));
    assert_eq!(disk_matches_receipt(home.path()), OLD);
    assert!(home.path().join(".ww/host-upgrade.json").exists());
    open(home.path())
        .recover(&mut FakeService::absent(), TIMEOUT)
        .unwrap();
    assert!(!home.path().join(".ww/bin/.ww-host-upgrade/stage").exists());
    unchanged_user_state(home.path());
}

#[test]
fn lock_contention_precedes_selection() {
    let home = fixture();
    let first = open(home.path());
    assert!(Engine::open(home.path()).is_err());
    drop(first);
    assert!(open(home.path())
        .operator_upgrade(&mut FakeService::absent(), TIMEOUT, || Ok(request(
            OLD, NEW
        )))
        .is_ok());
}

#[test]
fn malformed_incomplete_receipts_and_unauthenticated_old_bytes_refuse() {
    for invalid in 0..4 {
        let home = fixture();
        let mut receipt = Receipt::load(&home.path().join(".ww")).unwrap();
        match invalid {
            0 => receipt.version = 3,
            1 => receipt.complete = false,
            2 => receipt.hashes[0] = "unrecognized old bytes".into(),
            _ => (),
        }
        std::fs::write(
            home.path().join(".ww/installation.json"),
            receipt.encode().unwrap(),
        )
        .unwrap();
        let result =
            open(home.path()).operator_upgrade(&mut FakeService::absent(), TIMEOUT, || {
                let mut request = request(OLD, NEW);
                if invalid == 3 {
                    request.installed = selected(b"unrelated old bytes");
                }
                Ok(request)
            });
        assert!(result.is_err());
        assert_eq!(std::fs::read(home.path().join(".ww/bin/ww")).unwrap(), OLD);
        assert!(!home.path().join(".ww/host-upgrade.json").exists());
    }
}

#[test]
fn incompatible_candidate_and_unknown_service_state_refuse_before_replacement() {
    for invalid in 0..4 {
        let home = fixture();
        let mut service = FakeService::running(home.path());
        match invalid {
            0 => service.incompatible = Some(selected(NEW).artifact),
            1 => service.snapshot.running = RunningState::Unknown,
            2 => service.snapshot.enabled = EnabledState::Unknown,
            3 => service.admission_unavailable = true,
            _ => unreachable!(),
        }
        assert!(open(home.path())
            .operator_upgrade(&mut service, TIMEOUT, || Ok(request(OLD, NEW)))
            .is_err());
        assert_eq!(disk_matches_receipt(home.path()), OLD);
        if invalid == 0 {
            open(home.path()).recover(&mut service, TIMEOUT).unwrap();
        }
        assert!(!home.path().join(".ww/host-upgrade.json").exists());
        assert_eq!((service.restarts, service.stops), (0, 0));
    }
}

#[test]
fn running_service_restarts_with_exact_artifact_and_serving_readiness() {
    let home = fixture();
    let mut service = FakeService::running(home.path());
    service.snapshot.enabled = EnabledState::Disabled;
    let config = service.snapshot.configuration_identity.clone();
    assert_eq!(
        install(home.path(), &mut service, OLD, NEW),
        Outcome::Installed { live_ready: true }
    );
    assert_eq!(service.restarts, 1);
    assert_eq!(service.snapshot.enabled, EnabledState::Disabled);
    assert_eq!(service.snapshot.configuration_identity, config);
    assert_eq!(disk_matches_receipt(home.path()), NEW);
    unchanged_user_state(home.path());
}

#[test]
fn stopped_failed_and_disabled_services_stay_stopped() {
    for failed in [false, true] {
        for enabled in [EnabledState::Disabled, EnabledState::Enabled] {
            let home = fixture();
            let mut service = FakeService::running(home.path());
            service.snapshot.running = RunningState::Stopped;
            service.snapshot.enabled = enabled.clone();
            service.snapshot.failed = failed;
            assert_eq!(
                install(home.path(), &mut service, OLD, NEW),
                Outcome::Installed { live_ready: false }
            );
            assert_eq!(service.snapshot.running, RunningState::Stopped);
            assert_eq!(service.snapshot.enabled, enabled);
            assert_eq!(service.snapshot.failed, failed);
            assert_eq!((service.restarts, service.stops), (0, 0));
        }
    }
}

#[test]
fn pending_fhs_sources_namespace_or_publisher_inputs_refuse_before_replacement() {
    for field in 0..4 {
        let home = fixture();
        let mut service = FakeService::running(home.path());
        let mut pending = service.startup.clone();
        match field {
            0 => pending.fhs_baseline = Some("pending-fhs".into()),
            1 => pending.frozen_sources = "pending-local-mount".into(),
            2 => pending.configuration = "pending-namespace".into(),
            3 => pending.publisher_target = Some("pending-publisher".into()),
            _ => unreachable!(),
        }
        service.pending_before_restart = Some(pending);
        assert!(open(home.path())
            .operator_upgrade(&mut service, TIMEOUT, || Ok(request(OLD, NEW)))
            .is_err());
        assert_eq!(disk_matches_receipt(home.path()), OLD);
        assert_eq!((service.restarts, service.effects), (0, 0));
        open(home.path()).recover(&mut service, TIMEOUT).unwrap();
        unchanged_user_state(home.path());
    }
}

#[test]
fn changed_inputs_at_child_admission_have_no_publication_or_guest_effects() {
    let home = fixture();
    let mut service = FakeService::running(home.path());
    service.pending_in_restart = true;
    assert!(open(home.path())
        .operator_upgrade(&mut service, TIMEOUT, || Ok(request(OLD, NEW)))
        .is_err());
    assert_eq!(service.effects, 0);
    assert_eq!(service.snapshot.running, RunningState::Stopped);
    assert!(home.path().join(".ww/host-upgrade.json").exists());
    unchanged_user_state(home.path());
}

#[test]
fn restart_failure_timeout_wrong_running_binary_and_unready_keep_recovery() {
    for failure in 0..4 {
        let home = fixture();
        let mut service = FakeService::running(home.path());
        match failure {
            0 => service.restart_failure = true,
            1 => service.readiness_timeout = true,
            2 => service.wrong_running_artifact = true,
            3 => service.not_ready = true,
            _ => unreachable!(),
        }
        let error = open(home.path())
            .operator_upgrade(&mut service, TIMEOUT, || Ok(request(OLD, NEW)))
            .unwrap_err();
        assert!(format!("{error:#}").contains("previous bytes retained"));
        assert_eq!(service.snapshot.running, RunningState::Stopped);
        assert_eq!(std::fs::read(home.path().join(".ww/bin/ww")).unwrap(), NEW);
        let previous = open(home.path()).paths.artifact(&selected(OLD).artifact);
        assert_eq!(std::fs::read(previous).unwrap(), OLD);
        service.restart_failure = false;
        service.readiness_timeout = false;
        service.wrong_running_artifact = false;
        service.not_ready = false;
        assert_eq!(
            open(home.path()).recover(&mut service, TIMEOUT).unwrap(),
            Outcome::Recovered {
                artifact: selected(OLD).artifact,
                action: RecoveryAction::RestoredPrevious,
                live_ready: true
            }
        );
        assert_eq!(disk_matches_receipt(home.path()), OLD);
        unchanged_user_state(home.path());
    }
}

#[test]
fn failed_stop_reports_unconfirmed_residual_process_state() {
    let home = fixture();
    let mut service = FakeService::running(home.path());
    service.not_ready = true;
    service.stop_failure = true;
    let error = open(home.path())
        .operator_upgrade(&mut service, TIMEOUT, || Ok(request(OLD, NEW)))
        .unwrap_err();
    assert!(format!("{error:#}").contains("state is unconfirmed"));
    assert!(home.path().join(".ww/host-upgrade.json").exists());
    assert_eq!(std::fs::read(home.path().join(".ww/bin/ww")).unwrap(), NEW);
}

#[test]
fn incompatible_previous_host_stays_stopped_without_rewinding_deployment() {
    let home = fixture();
    let mut service = FakeService::running(home.path());
    service.restart_failure = true;
    assert!(open(home.path())
        .operator_upgrade(&mut service, TIMEOUT, || Ok(request(OLD, NEW)))
        .is_err());
    service.restart_failure = false;
    service.incompatible = Some(selected(OLD).artifact);
    let error = open(home.path())
        .recover(&mut service, TIMEOUT)
        .unwrap_err();
    assert!(format!("{error:#}").contains("previous Host is incompatible"));
    assert_eq!(service.snapshot.running, RunningState::Stopped);
    assert_eq!(std::fs::read(home.path().join(".ww/bin/ww")).unwrap(), NEW);
    unchanged_user_state(home.path());
}

#[test]
fn pending_recovery_runs_before_new_release_selection() {
    let home = fixture();
    let mut service = FakeService::running(home.path());
    service.restart_failure = true;
    assert!(open(home.path())
        .operator_upgrade(&mut service, TIMEOUT, || Ok(request(OLD, NEW)))
        .is_err());
    service.restart_failure = false;
    let result =
        open(home.path()).operator_upgrade::<Cursor<Vec<u8>>>(&mut service, TIMEOUT, || {
            panic!("selected another release before recovery")
        });
    assert_eq!(
        result.unwrap(),
        Outcome::Recovered {
            artifact: selected(OLD).artifact,
            action: RecoveryAction::RestoredPrevious,
            live_ready: true
        }
    );
    assert_eq!(disk_matches_receipt(home.path()), OLD);
}

#[test]
fn authenticated_recovery_accepts_only_exact_recorded_generations() {
    for executable in [OLD, NEW] {
        let home = fixture();
        let mut service = FakeService::absent();
        service.incompatible = Some(selected(NEW).artifact);
        let mut engine = open(home.path());
        assert!(engine
            .operator_upgrade(&mut service, TIMEOUT, || Ok(request(OLD, NEW)))
            .is_err());
        engine.require_pending_recovery().unwrap();
        assert_eq!(
            engine
                .recover_authenticated(&mut service, TIMEOUT, selected(executable))
                .unwrap(),
            Outcome::Recovered {
                artifact: selected(OLD).artifact,
                action: RecoveryAction::AbortedBeforeReplacement,
                live_ready: false,
            }
        );
        assert_eq!(disk_matches_receipt(home.path()), OLD);
        assert!(!home.path().join(".ww/host-upgrade.json").exists());
        assert_eq!((service.restarts, service.stops), (0, 0));
        unchanged_user_state(home.path());
    }
}

#[test]
fn authenticated_recovery_rejects_unrelated_artifact_or_provenance_without_changes() {
    for mismatch in 0..5 {
        let home = fixture();
        let mut service = FakeService::absent();
        service.incompatible = Some(selected(NEW).artifact);
        let mut engine = open(home.path());
        assert!(engine
            .operator_upgrade(&mut service, TIMEOUT, || Ok(request(OLD, NEW)))
            .is_err());
        let journal = std::fs::read(&engine.paths.journal).unwrap();
        let mut executable = selected(OLD);
        match mismatch {
            0 => executable = selected(THIRD),
            1 => executable.release = "another-authenticated-release".into(),
            2 => executable.version = "another-version".into(),
            3 => executable.revision = "another-revision".into(),
            4 => executable.platform = "another-platform".into(),
            _ => unreachable!(),
        }
        let error = engine
            .recover_authenticated(&mut service, TIMEOUT, executable)
            .unwrap_err();
        assert!(error.to_string().contains("not a recorded Host generation"));
        assert_eq!(std::fs::read(&engine.paths.journal).unwrap(), journal);
        assert_eq!(disk_matches_receipt(home.path()), OLD);
        assert_eq!((service.restarts, service.stops), (0, 0));
        unchanged_user_state(home.path());
    }
}

#[test]
fn authenticated_recovery_requires_pending_ownership_and_preserves_service_gates() {
    let home = fixture();
    {
        let mut engine = open(home.path());
        assert!(engine.require_pending_recovery().is_err());
        assert!(engine
            .recover_authenticated(&mut FakeService::absent(), TIMEOUT, selected(OLD))
            .is_err());
        assert_eq!(disk_matches_receipt(home.path()), OLD);
        assert!(!engine.paths.journal.exists());
    }
    prepare_recovery(home.path(), false);
    let mut engine = open(home.path());
    let mut service = FakeService::absent();
    service.incompatible = Some(selected(OLD).artifact);
    let error = engine
        .recover_authenticated(&mut service, TIMEOUT, selected(OLD))
        .unwrap_err();
    assert!(format!("{error:#}").contains("previous Host is incompatible"));
    assert_eq!(std::fs::read(&engine.paths.executable).unwrap(), NEW);
    assert!(engine.paths.journal.exists());
    assert_eq!((service.restarts, service.stops), (0, 0));
    unchanged_user_state(home.path());
}

#[test]
fn explicit_rollback_uses_retained_artifact_and_preserves_current_receipt_fields() {
    let home = fixture();
    let mut service = FakeService::absent();
    install(home.path(), &mut service, OLD, NEW);
    let mut receipt = Receipt::load(&home.path().join(".ww")).unwrap();
    receipt.hashes[1] = "independently-updated-current-kernel".into();
    std::fs::write(
        home.path().join(".ww/installation.json"),
        receipt.encode().unwrap(),
    )
    .unwrap();
    assert_eq!(
        open(home.path())
            .operator_rollback(&mut service, TIMEOUT)
            .unwrap(),
        Outcome::Installed { live_ready: false }
    );
    assert_eq!(disk_matches_receipt(home.path()), OLD);
    assert_eq!(
        Receipt::load(&home.path().join(".ww")).unwrap().hashes[1],
        "independently-updated-current-kernel"
    );
    assert_eq!(
        std::fs::read(home.path().join(".ww/authoritative-head")).unwrap(),
        b"operator-owned authoritative-head"
    );
}

#[test]
fn repeated_upgrades_retain_only_current_and_one_previous_host() {
    let home = fixture();
    let mut service = FakeService::absent();
    install(home.path(), &mut service, OLD, NEW);
    install(home.path(), &mut service, NEW, THIRD);
    let engine = open(home.path());
    assert!(!engine.paths.artifact(&selected(OLD).artifact).exists());
    assert_eq!(
        std::fs::read(engine.paths.artifact(&selected(NEW).artifact)).unwrap(),
        NEW
    );
    assert_eq!(
        std::fs::read(engine.paths.artifact(&selected(THIRD).artifact)).unwrap(),
        THIRD
    );
    assert_eq!(std::fs::read_dir(&engine.paths.store).unwrap().count(), 2);
}

#[test]
fn missing_retained_or_recovery_artifact_fails_closed() {
    let home = fixture();
    assert!(open(home.path())
        .operator_rollback(&mut FakeService::absent(), TIMEOUT)
        .is_err());
    install(home.path(), &mut FakeService::absent(), OLD, NEW);
    let engine = open(home.path());
    std::fs::remove_file(engine.paths.artifact(&selected(OLD).artifact)).unwrap();
    drop(engine);
    assert!(open(home.path())
        .operator_rollback(&mut FakeService::absent(), TIMEOUT)
        .is_err());
    assert_eq!(disk_matches_receipt(home.path()), NEW);

    let home = fixture();
    let mut service = FakeService::running(home.path());
    service.restart_failure = true;
    assert!(open(home.path())
        .operator_upgrade(&mut service, TIMEOUT, || Ok(request(OLD, NEW)))
        .is_err());
    let engine = open(home.path());
    std::fs::remove_file(engine.paths.artifact(&selected(OLD).artifact)).unwrap();
    drop(engine);
    service.restart_failure = false;
    assert!(open(home.path()).recover(&mut service, TIMEOUT).is_err());
    assert_eq!(service.snapshot.running, RunningState::Stopped);
    assert!(home.path().join(".ww/host-upgrade.json").exists());
}

#[test]
fn malformed_bounded_journal_and_symlink_targets_fail_closed() {
    for content in [
        b"{\"version\":2}".to_vec(),
        vec![b'x'; storage::MAX_RECORD + 1],
    ] {
        let home = fixture();
        std::fs::write(home.path().join(".ww/host-upgrade.json"), content).unwrap();
        assert!(open(home.path())
            .recover(&mut FakeService::absent(), TIMEOUT)
            .is_err());
        assert_eq!(disk_matches_receipt(home.path()), OLD);
    }
    let home = fixture();
    let mut engine = open(home.path());
    std::os::unix::fs::symlink(
        home.path().join(".ww/identity"),
        engine.paths.store.join("stage"),
    )
    .unwrap();
    assert!(engine
        .operator_upgrade(&mut FakeService::absent(), TIMEOUT, || Ok(request(
            OLD, NEW
        )))
        .is_err());
    unchanged_user_state(home.path());
    assert_eq!(disk_matches_receipt(home.path()), OLD);
}

#[test]
fn external_receipt_edit_is_never_overwritten_by_recovery() {
    let home = fixture();
    let mut service = FakeService::running(home.path());
    service.restart_failure = true;
    assert!(open(home.path())
        .operator_upgrade(&mut service, TIMEOUT, || Ok(request(OLD, NEW)))
        .is_err());
    let mut edited = Receipt::load(&home.path().join(".ww")).unwrap();
    edited.hashes[1] = "external-edit".into();
    let edited = edited.encode().unwrap();
    std::fs::write(home.path().join(".ww/installation.json"), &edited).unwrap();
    assert!(open(home.path()).recover(&mut service, TIMEOUT).is_err());
    assert_eq!(
        std::fs::read(home.path().join(".ww/installation.json")).unwrap(),
        edited
    );
}

#[test]
fn zero_deadline_never_calls_selection() {
    let home = fixture();
    let result = open(home.path()).operator_upgrade::<Cursor<Vec<u8>>>(
        &mut FakeService::absent(),
        Duration::ZERO,
        || panic!("selected with invalid deadline"),
    );
    assert!(result.is_err());
    assert_eq!(disk_matches_receipt(home.path()), OLD);
}

fn run_operation(home: &Path, action: &str) -> Result<Outcome> {
    let mut service = if action.starts_with("running-") {
        FakeService::simulated_manager(home)
    } else {
        FakeService::absent()
    };
    let mut engine = Engine::open(home)?;
    match action.strip_prefix("running-").unwrap_or(action) {
        "upgrade" => engine.operator_upgrade(&mut service, TIMEOUT, || Ok(request(OLD, NEW))),
        "second-upgrade" => {
            engine.operator_upgrade(&mut service, TIMEOUT, || Ok(request(NEW, THIRD)))
        }
        "rollback" => engine.operator_rollback(&mut service, TIMEOUT),
        "recover" => engine.recover(&mut service, TIMEOUT),
        _ => panic!("unknown crash operation"),
    }
}

#[test]
#[ignore = "subprocess worker; invoked by crash matrix"]
fn crash_worker() {
    let home = PathBuf::from(std::env::var_os("WW_UPGRADE_CRASH_HOME").expect("worker home"));
    let point = std::env::var("WW_UPGRADE_CRASH_POINT")
        .unwrap()
        .parse()
        .unwrap();
    let action = std::env::var("WW_UPGRADE_CRASH_ACTION").unwrap();
    storage::faults::exit_at(point);
    run_operation(&home, &action).unwrap();
    panic!("requested crash checkpoint was not reached");
}

fn prepare_recovery(home: &Path, running: bool) {
    let action = if running {
        "running-upgrade"
    } else {
        "upgrade"
    };
    let baseline = fixture();
    storage::faults::configure(None);
    run_operation(baseline.path(), action).unwrap();
    let trace = storage::faults::trace();
    let point = trace
        .iter()
        .position(|label| label == "phase.Installed.before")
        .unwrap()
        + 1;
    storage::faults::configure(Some(point));
    assert!(run_operation(home, action).is_err());
    storage::faults::configure(None);
    assert_eq!(std::fs::read(home.join(".ww/bin/ww")).unwrap(), NEW);
}

fn prepare_action(home: &Path, action: &str) {
    match action {
        "upgrade" | "running-upgrade" => (),
        "rollback" | "second-upgrade" => {
            install(home, &mut FakeService::absent(), OLD, NEW);
        }
        "running-rollback" => {
            run_operation(home, "running-upgrade").unwrap();
        }
        "recover" => prepare_recovery(home, false),
        "running-recover" => prepare_recovery(home, true),
        _ => unreachable!(),
    }
}

#[test]
fn abrupt_process_exit_at_every_upgrade_rollback_and_recovery_transition() {
    for action in [
        "upgrade",
        "second-upgrade",
        "rollback",
        "recover",
        "running-upgrade",
        "running-rollback",
        "running-recover",
    ] {
        let baseline = fixture();
        prepare_action(baseline.path(), action);
        storage::faults::configure(None);
        run_operation(baseline.path(), action).unwrap();
        let trace = storage::faults::trace();
        assert!(!trace.is_empty());
        eprintln!("{action}: {} abrupt-process checkpoints", trace.len());
        for (index, label) in trace.iter().enumerate() {
            let home = fixture();
            prepare_action(home.path(), action);
            let output = Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "upgrade::tests::crash_worker",
                    "--ignored",
                    "--nocapture",
                ])
                .env("WW_UPGRADE_CRASH_HOME", home.path())
                .env("WW_UPGRADE_CRASH_POINT", (index + 1).to_string())
                .env("WW_UPGRADE_CRASH_ACTION", action)
                .output()
                .unwrap();
            assert_eq!(
                output.status.code(),
                Some(86),
                "{action} {} {label}: {}",
                index + 1,
                String::from_utf8_lossy(&output.stderr)
            );
            storage::faults::configure(None);
            let mut engine = open(home.path());
            let mut service = if action.starts_with("running-") {
                FakeService::simulated_manager(home.path())
            } else {
                FakeService::absent()
            };
            engine
                .recover(&mut service, TIMEOUT)
                .unwrap_or_else(|error| {
                    panic!("recover {action} {} {label}: {error:#}", index + 1)
                });
            assert!(
                !engine.paths.journal.exists(),
                "{action} {} {label}",
                index + 1
            );
            let installed = disk_matches_receipt(home.path());
            let expected = if action == "second-upgrade" {
                [NEW, THIRD]
            } else {
                [OLD, NEW]
            };
            assert!(
                expected.contains(&installed.as_slice()),
                "partial executable after {action} {} {label}",
                index + 1
            );
            if let Some(retained) = storage::read_record(&engine.paths.retained).unwrap() {
                let retained = Retained::decode(&retained).unwrap();
                assert_eq!(retained.current.artifact, selected(&installed).artifact);
                storage::verify(
                    &engine.paths.artifact(&retained.previous.artifact),
                    &retained.previous.artifact,
                )
                .unwrap();
            }
            assert!(
                std::fs::read_dir(&engine.paths.store).unwrap().count() <= 2,
                "unbounded retained bytes after {action} {} {label}",
                index + 1
            );
            unchanged_user_state(home.path());
            if let Some(running) = &service.running_artifact {
                assert_eq!(
                    &selected(&installed).artifact,
                    running,
                    "disk/running mismatch after {action} {} {label}",
                    index + 1
                );
            }
        }
    }
}

#[test]
fn unregistered_but_defined_launchd_service_stays_stopped() {
    let home = fixture();
    let mut service = FakeService::running(home.path());
    service.snapshot.registered = false;
    service.snapshot.running = RunningState::Stopped;
    service.running_artifact = None;
    assert_eq!(
        install(home.path(), &mut service, OLD, NEW),
        Outcome::Installed { live_ready: false }
    );
    assert_eq!((service.restarts, service.stops), (0, 0));
    assert!(!service.snapshot.registered);
    assert_eq!(service.snapshot.enabled, EnabledState::Enabled);
}

#[test]
fn advancing_authoritative_head_is_rechecked_after_restart() {
    let home = fixture();
    let mut service = FakeService::running(home.path());
    service.make_incompatible_on_restart = true;
    let error = open(home.path())
        .operator_upgrade(&mut service, TIMEOUT, || Ok(request(OLD, NEW)))
        .unwrap_err();
    assert!(format!("{error:#}").contains("deployment became incompatible"));
    assert_eq!(service.snapshot.running, RunningState::Stopped);
    assert!(home.path().join(".ww/host-upgrade.json").exists());
    unchanged_user_state(home.path());
}

#[test]
fn corrupted_journal_receipt_digests_cannot_relabel_recovery_bytes() {
    let baseline = fixture();
    storage::faults::configure(None);
    run_operation(baseline.path(), "upgrade").unwrap();
    let point = storage::faults::trace()
        .iter()
        .position(|label| label == "phase.Committed.before")
        .unwrap()
        + 1;
    for corrupt_previous in [true, false] {
        let home = fixture();
        storage::faults::configure(Some(point));
        assert!(run_operation(home.path(), "upgrade").is_err());
        storage::faults::configure(None);
        let path = home.path().join(".ww/host-upgrade.json");
        let mut journal: Journal = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        let target = if corrupt_previous {
            &mut journal.receipt_before
        } else {
            journal.receipt_after.as_mut().unwrap()
        };
        let mut receipt: Receipt = serde_json::from_slice(target).unwrap();
        receipt.hashes[0] = "0".repeat(64);
        *target = receipt.encode().unwrap();
        std::fs::write(&path, serde_json::to_vec(&journal).unwrap()).unwrap();
        let before = std::fs::read(home.path().join(".ww/installation.json")).unwrap();
        let error = open(home.path())
            .recover(&mut FakeService::absent(), TIMEOUT)
            .unwrap_err();
        assert!(format!("{error:#}").contains("does not match verified"));
        assert_eq!(
            std::fs::read(home.path().join(".ww/installation.json")).unwrap(),
            before
        );
        assert_eq!(std::fs::read(home.path().join(".ww/bin/ww")).unwrap(), NEW);
    }
}

#[test]
fn compatibility_finishing_after_deadline_cannot_report_live_readiness() {
    let home = fixture();
    let mut service = FakeService::running(home.path());
    service.delay_final_compatibility = true;
    let error = open(home.path())
        .operator_upgrade(&mut service, Duration::from_secs(2), || {
            Ok(request(OLD, NEW))
        })
        .unwrap_err();
    assert!(format!("{error:#}").contains("deadline expired"));
    assert_eq!(
        service.restarts, 1,
        "deadline must expire in final compatibility, not staging"
    );
    assert_eq!(service.snapshot.running, RunningState::Stopped);
    assert!(home.path().join(".ww/host-upgrade.json").exists());
}

#[test]
fn aborted_upgrades_do_not_accumulate_unreferenced_hosts_across_reinstalls() {
    let baseline = fixture();
    let mut baseline_engine = open(baseline.path());
    storage::faults::configure(None);
    baseline_engine
        .operator_upgrade(&mut FakeService::absent(), TIMEOUT, || {
            Ok(request(OLD, NEW))
        })
        .unwrap();
    let point = storage::faults::trace()
        .iter()
        .position(|label| label == "phase.Replacing.before")
        .unwrap()
        + 1;
    let home = fixture();
    for current in [OLD, THIRD] {
        // Model a completed legacy reinstall between independent transactions.
        std::fs::write(home.path().join(".ww/bin/ww"), current).unwrap();
        let receipt = Receipt::load(&home.path().join(".ww"))
            .unwrap()
            .with_executable_digest(blake3::hash(current).to_hex().to_string());
        std::fs::write(
            home.path().join(".ww/installation.json"),
            receipt.encode().unwrap(),
        )
        .unwrap();
        let mut engine = open(home.path());
        storage::faults::configure(Some(point));
        assert!(engine
            .operator_upgrade(&mut FakeService::absent(), TIMEOUT, || Ok(request(
                current, NEW
            )))
            .is_err());
        drop(engine);
        storage::faults::configure(None);
        let mut engine = open(home.path());
        engine.recover(&mut FakeService::absent(), TIMEOUT).unwrap();
        assert_eq!(disk_matches_receipt(home.path()), current);
        assert_eq!(
            std::fs::read_dir(&engine.paths.store).unwrap().count(),
            0,
            "aborted first upgrade left unreferenced Host bytes"
        );
    }
}

fn assert_stopped_deadline_boundary(action: &str, after_copy: bool) {
    let baseline = fixture();
    prepare_action(baseline.path(), action);
    let mut baseline_engine = open(baseline.path());
    storage::faults::configure(None);
    match action {
        "upgrade" => baseline_engine.operator_upgrade(&mut FakeService::absent(), TIMEOUT, || {
            Ok(request(OLD, NEW))
        }),
        "recover" => baseline_engine.recover(&mut FakeService::absent(), TIMEOUT),
        _ => unreachable!(),
    }
    .unwrap();
    let trace = storage::faults::trace();
    let start_label = if action == "upgrade" {
        "phase.Replacing.after"
    } else {
        "phase.Restoring.after"
    };
    let start = trace.iter().position(|label| label == start_label).unwrap();
    let target = if after_copy {
        "stage.sync.after"
    } else if action == "upgrade" {
        "phase.Verified.after"
    } else {
        "rename.after"
    };
    let point = trace
        .iter()
        .enumerate()
        .skip(start + 1)
        .find_map(|(index, label)| (label == target).then_some(index + 1))
        .unwrap();

    let home = fixture();
    prepare_action(home.path(), action);
    let receipt_before = std::fs::read(home.path().join(".ww/installation.json")).unwrap();
    let installed_before = std::fs::read(home.path().join(".ww/bin/ww")).unwrap();
    let mut engine = open(home.path());
    // Advance only the test clock at a real copy/durability checkpoint. This
    // models elapsed I/O time deterministically without a wall-clock sleep.
    storage::faults::advance_clock_at(point);
    let result = match action {
        "upgrade" => engine.operator_upgrade(&mut FakeService::absent(), TIMEOUT, || {
            Ok(request(OLD, NEW))
        }),
        "recover" => engine.recover(&mut FakeService::absent(), TIMEOUT),
        _ => unreachable!(),
    };
    storage::faults::configure(None);
    let error = result.unwrap_err();
    assert!(format!("{error:#}").contains("deadline expired"));
    assert!(engine.paths.journal.exists());
    assert_eq!(
        std::fs::read(&engine.paths.receipt).unwrap(),
        receipt_before
    );
    let installed = std::fs::read(&engine.paths.executable).unwrap();
    if after_copy {
        assert_eq!(
            installed, installed_before,
            "expired copy budget admitted executable rename"
        );
    } else {
        assert_eq!(installed, if action == "upgrade" { NEW } else { OLD });
    }
    storage::verify(
        &engine.paths.artifact(&selected(OLD).artifact),
        &selected(OLD).artifact,
    )
    .unwrap();
    engine.recover(&mut FakeService::absent(), TIMEOUT).unwrap();
    assert_eq!(disk_matches_receipt(home.path()), OLD);
    unchanged_user_state(home.path());
}

#[test]
fn deadline_during_upgrade_copy_preserves_installed_executable() {
    assert_stopped_deadline_boundary("upgrade", true);
}

#[test]
fn deadline_during_recovery_copy_preserves_installed_executable() {
    assert_stopped_deadline_boundary("recover", true);
}

#[test]
fn deadline_before_stopped_upgrade_commit_preserves_pending_transaction() {
    assert_stopped_deadline_boundary("upgrade", false);
}

#[test]
fn deadline_before_stopped_recovery_commit_preserves_pending_transaction() {
    assert_stopped_deadline_boundary("recover", false);
}

#[test]
fn scoped_v2_receipt_survives_refusal_recovery_upgrade_and_rollback() {
    let home = fixture();
    let root = home.path().join(".ww");
    let mut receipt = Receipt::load(&root).unwrap();
    receipt.version = 2;
    storage::write_record(&root.join("installation.json"), &receipt.encode().unwrap()).unwrap();
    let mut service = FakeService::absent();
    service.incompatible = Some(selected(NEW).artifact);
    assert!(open(home.path())
        .operator_upgrade(&mut service, TIMEOUT, || Ok(request(OLD, NEW)))
        .is_err());
    assert!(root.join("host-upgrade.json").exists());
    let recovery = open(home.path())
        .operator_upgrade::<&[u8]>(&mut service, TIMEOUT, || {
            panic!("recovery must precede release selection")
        })
        .unwrap();
    assert!(matches!(
        recovery,
        Outcome::Recovered {
            action: RecoveryAction::AbortedBeforeReplacement,
            live_ready: false,
            ..
        }
    ));
    assert!(!root.join("host-upgrade.json").exists());
    assert_eq!(
        Receipt::load(&root).unwrap().encode().unwrap(),
        receipt.encode().unwrap()
    );
    service.incompatible = None;
    assert!(matches!(
        install(home.path(), &mut service, OLD, NEW),
        Outcome::Installed { live_ready: false }
    ));
    assert_eq!(Receipt::load(&root).unwrap().version, 2);
    open(home.path())
        .operator_rollback(&mut service, TIMEOUT)
        .unwrap();
    assert_eq!(std::fs::read(root.join("bin/ww")).unwrap(), OLD);
    assert_eq!(Receipt::load(&root).unwrap().version, 2);
    unchanged_user_state(home.path());
}
