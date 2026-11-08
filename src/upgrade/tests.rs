use super::*;
use anyhow::bail;
use service::{LegacyRunningEvidence, StartupIdentity};
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
    #[serde(skip)]
    interrupt_guarded_restart: bool,
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
    #[serde(skip)]
    service_deadlines: Vec<(&'static str, Instant)>,
    #[serde(skip)]
    expire_after_guarded_restart: bool,
    #[serde(skip)]
    capturing_child: Option<std::process::Child>,
    #[serde(skip)]
    capture_fixture: Option<(PathBuf, PathBuf)>,
    #[serde(skip)]
    expected_quiescence_permit: Option<startup::StartupPermitPayload>,
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
            interrupt_guarded_restart: false,
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
            service_deadlines: Vec::new(),
            expire_after_guarded_restart: false,
            capturing_child: None,
            capture_fixture: None,
            expected_quiescence_permit: None,
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
    fn snapshot(&mut self, deadline: Instant) -> Result<ServiceSnapshot> {
        self.service_deadlines.push(("snapshot", deadline));
        Ok(self.snapshot.clone())
    }

    fn admit_restart(
        &mut self,
        before: &ServiceSnapshot,
        contract: &startup::StartupContractV1,
        executable: &attestation::VerifiedExecutable,
        deadline: Instant,
    ) -> Result<service::RestartAdmission> {
        executable.revalidate(deadline)?;
        let RunningState::Running { pid } = before.running else {
            bail!("not running")
        };
        let startup::InvocationBinding::Managed { service } = &contract.invocation else {
            bail!("not managed")
        };
        ensure!(
            contract.startup == self.startup && !self.admission_unavailable,
            "startup admission unavailable"
        );
        Ok(service::RestartAdmission {
            contract: contract.clone(),
            original_process: fixture_process(pid),
            service: service.clone(),
            transaction_id: [0; 32],
        })
    }
    fn revalidate_restart(
        &mut self,
        admission: &service::RestartAdmission,
        _: Instant,
    ) -> Result<()> {
        ensure!(
            admission.contract.startup == self.startup,
            "captured startup inputs changed"
        );
        Ok(())
    }
    fn quiesce(
        &mut self,
        intent: &service::ServiceDisposition,
        _: Instant,
    ) -> Result<service::ServiceDisposition> {
        intent.validate()?;
        if let Some(expected) = &self.expected_quiescence_permit {
            ensure!(
                intent.quiescence_permit.as_ref() == Some(expected),
                "recovery changed the recorded containment target"
            );
        }
        ensure!(intent.stop_intent, "missing stop intent");
        ensure!(!self.stop_failure, "service stop failed");
        if matches!(self.snapshot.running, RunningState::Running { .. }) {
            self.stops += 1;
        }
        self.snapshot.running = RunningState::Stopped;
        self.snapshot.failed = false;
        self.running_artifact = None;
        self.save_state()?;
        let mut confirmed = intent.clone();
        confirmed.quiesced = Some(self.snapshot.clone());
        Ok(confirmed)
    }
    fn restart(
        &mut self,
        admission: &service::RestartAdmission,
        permit: &StartupPermit,
        executable: &attestation::VerifiedExecutable,
        deadline: Instant,
    ) -> Result<service::RunningEvidence> {
        self.service_deadlines.push(("restart", deadline));
        assert!(
            !self.interrupt_guarded_restart,
            "simulated interruption after runnable permit publication"
        );
        self.restarts += 1;
        self.revalidate_restart(admission, deadline)?;
        ensure!(!self.restart_failure, "manager restart failed");
        executable.revalidate(deadline)?;
        self.next_pid += 1;
        self.snapshot.running = RunningState::Running { pid: self.next_pid };
        self.running_artifact = Some(executable.artifact().clone());
        self.effects += 1;
        self.save_state()?;
        if let Some((home, executable)) = &self.capture_fixture {
            self.capturing_child = Some(
                Command::new(executable)
                    .args([
                        "--exact",
                        "upgrade::tests::native_v2_capturing_child_worker",
                        "--ignored",
                        "--nocapture",
                    ])
                    .env("WW_NATIVE_CAPTURE_HOME", home)
                    .stdout(std::process::Stdio::null())
                    .stderr(std::fs::File::create(home.join("capture-stderr"))?)
                    .spawn()?,
            );
            while !home.join("capture-ready").exists() {
                check_deadline(deadline)?;
                std::thread::sleep(Duration::from_millis(5));
            }
            // Exhaust the actual operation deadline while the independently
            // validated child retains its real shared startup lease.
            std::thread::sleep(
                deadline.saturating_duration_since(Instant::now()) + Duration::from_millis(5),
            );
            check_deadline(deadline)?;
        }
        if self.expire_after_guarded_restart {
            storage::faults::advance_clock_at(1);
            checkpoint("fixture.restart.exhausted-deadline")?;
            check_deadline(deadline)?;
        }
        ensure!(
            !self.readiness_timeout && !self.not_ready,
            "readiness unavailable"
        );
        Ok(service::RunningEvidence {
            process: fixture_process(self.next_pid),
            artifact: if self.wrong_running_artifact {
                signed_generation(THIRD, 3).artifact().clone()
            } else {
                executable.artifact().clone()
            },
            startup: self.startup.clone(),
            service: admission.service.clone(),
            contract: permit.contract().identity.clone(),
            transaction_id: Some(permit.payload().transaction_id),
            permit_digest: Some(permit.payload().digest()?),
            epoch_seq: 1,
            effective_root: "fixture-root".into(),
            kernel_source: "fixture-kernel".into(),
            kernel_runtime: "fixture-runtime".into(),
        })
    }
    fn revalidate_running(
        &mut self,
        evidence: &service::RunningEvidence,
        permit: &StartupPermit,
        executable: &attestation::VerifiedExecutable,
        deadline: Instant,
    ) -> Result<()> {
        executable.revalidate(deadline)?;
        ensure!(
            self.snapshot.running
                == RunningState::Running {
                    pid: evidence.process().pid
                }
                && self.running_artifact.as_ref() == Some(executable.artifact())
                && self.startup == permit.contract().startup
                && !self.not_ready,
            "live evidence changed"
        );
        Ok(())
    }
    fn stop(&mut self, _: &startup::ServiceIdentity, deadline: Instant) -> Result<()> {
        self.service_deadlines.push(("stop", deadline));
        check_deadline(deadline)?;
        ensure!(!self.stop_failure, "stop failed");
        self.stops += 1;
        self.snapshot.running = RunningState::Stopped;
        self.running_artifact = None;
        self.save_state()
    }

    fn legacy_admit_restart(&mut self, before: &ServiceSnapshot) -> Result<LegacyRestartAdmission> {
        if self.admission_unavailable {
            bail!("running Host has no captured startup evidence; explicitly stop service");
        }
        let RunningState::Running { pid } = before.running else {
            bail!("not running");
        };
        let admission = LegacyRestartAdmission {
            startup: self.startup.clone(),
            configuration_identity: before.configuration_identity.clone(),
            prior_pid: pid,
        };
        if let Some(pending) = self.pending_before_restart.take() {
            self.startup = pending;
        }
        Ok(admission)
    }

    fn legacy_revalidate_restart(&mut self, admission: &LegacyRestartAdmission) -> Result<()> {
        ensure!(
            admission.startup == self.startup,
            "prepared startup inputs changed"
        );
        Ok(())
    }

    fn legacy_restart(
        &mut self,
        admission: &LegacyRestartAdmission,
        expected: &ArtifactIdentity,
        deadline: Instant,
    ) -> Result<LegacyRunningEvidence> {
        self.restart_deadline = Some(deadline);
        self.restarts += 1;
        if self.pending_in_restart {
            self.startup.publisher_target = Some("pending-publisher-target".into());
        }
        // Model the required child-side check before publication or guest effects.
        self.legacy_revalidate_restart(admission)?;
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
        Ok(LegacyRunningEvidence {
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

    fn legacy_stop(&mut self, _: Instant) -> Result<()> {
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

    fn compatible(
        &mut self,
        executable: &attestation::VerifiedExecutable,
        inputs: &inputs::PreflightInputs,
        deadline: Instant,
    ) -> Result<service::CompatibilityEvidence> {
        executable.revalidate(deadline)?;
        ensure!(
            self.incompatible.as_ref() != Some(executable.artifact()),
            "incompatible fixture"
        );
        Ok(service::CompatibilityEvidence {
            artifact: executable.artifact().clone(),
            capture_digest: inputs.capture_digest()?,
            protocol_version: inputs::NATIVE_PROTOCOL_VERSION,
            prepared: inputs::PreparedHead {
                effective_root: "fixture-root".into(),
                kernel_source: "fixture-kernel".into(),
                kernel_runtime: "fixture-runtime".into(),
                kernel_source_cid: "fixture-source".into(),
                kernel_wasm_blake3: "00".repeat(32),
                kernel_size: 1,
                authoritative_head: None,
            },
        })
    }

    fn legacy_compatible(&mut self, artifact: &ArtifactIdentity, executable: &Path) -> Result<()> {
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

fn request(old: &'static [u8], new: &'static [u8]) -> LegacyUpgradeRequest<&'static [u8]> {
    LegacyUpgradeRequest {
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
        .legacy_operator_upgrade(service, TIMEOUT, || Ok(request(old, new)))
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
    let result =
        open(home.path()).legacy_operator_upgrade(&mut FakeService::absent(), TIMEOUT, || {
            Ok(LegacyUpgradeRequest {
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
            open(home.path()).legacy_operator_upgrade(&mut FakeService::absent(), TIMEOUT, || {
                Ok(LegacyUpgradeRequest {
                    installed: selected(OLD),
                    candidate,
                    bytes: NEW,
                })
            });
        assert!(result.is_err(), "invalid case {invalid}");
        assert_eq!(disk_matches_receipt(home.path()), OLD);
        open(home.path())
            .legacy_recover(&mut FakeService::absent(), TIMEOUT)
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
        .legacy_operator_upgrade(&mut FakeService::absent(), TIMEOUT, || {
            Ok(LegacyUpgradeRequest {
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
    let result =
        open(home.path()).legacy_operator_upgrade(&mut FakeService::absent(), TIMEOUT, || {
            Ok(LegacyUpgradeRequest {
                installed: selected(OLD),
                candidate: selected(NEW),
                bytes: Interrupted { first: true },
            })
        });
    assert!(format!("{:#}", result.unwrap_err()).contains("selected native artifact"));
    assert_eq!(disk_matches_receipt(home.path()), OLD);
    assert!(home.path().join(".ww/host-upgrade.json").exists());
    open(home.path())
        .legacy_recover(&mut FakeService::absent(), TIMEOUT)
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
        .legacy_operator_upgrade(&mut FakeService::absent(), TIMEOUT, || Ok(request(
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
            open(home.path()).legacy_operator_upgrade(&mut FakeService::absent(), TIMEOUT, || {
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
            .legacy_operator_upgrade(&mut service, TIMEOUT, || Ok(request(OLD, NEW)))
            .is_err());
        assert_eq!(disk_matches_receipt(home.path()), OLD);
        if invalid == 0 {
            open(home.path())
                .legacy_recover(&mut service, TIMEOUT)
                .unwrap();
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
            .legacy_operator_upgrade(&mut service, TIMEOUT, || Ok(request(OLD, NEW)))
            .is_err());
        assert_eq!(disk_matches_receipt(home.path()), OLD);
        assert_eq!((service.restarts, service.effects), (0, 0));
        open(home.path())
            .legacy_recover(&mut service, TIMEOUT)
            .unwrap();
        unchanged_user_state(home.path());
    }
}

#[test]
fn changed_inputs_at_child_admission_have_no_publication_or_guest_effects() {
    let home = fixture();
    let mut service = FakeService::running(home.path());
    service.pending_in_restart = true;
    assert!(open(home.path())
        .legacy_operator_upgrade(&mut service, TIMEOUT, || Ok(request(OLD, NEW)))
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
            .legacy_operator_upgrade(&mut service, TIMEOUT, || Ok(request(OLD, NEW)))
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
            open(home.path())
                .legacy_recover(&mut service, TIMEOUT)
                .unwrap(),
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
        .legacy_operator_upgrade(&mut service, TIMEOUT, || Ok(request(OLD, NEW)))
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
        .legacy_operator_upgrade(&mut service, TIMEOUT, || Ok(request(OLD, NEW)))
        .is_err());
    service.restart_failure = false;
    service.incompatible = Some(selected(OLD).artifact);
    let error = open(home.path())
        .legacy_recover(&mut service, TIMEOUT)
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
        .legacy_operator_upgrade(&mut service, TIMEOUT, || Ok(request(OLD, NEW)))
        .is_err());
    service.restart_failure = false;
    let result =
        open(home.path()).legacy_operator_upgrade::<Cursor<Vec<u8>>>(&mut service, TIMEOUT, || {
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
            .legacy_operator_upgrade(&mut service, TIMEOUT, || Ok(request(OLD, NEW)))
            .is_err());
        engine.require_pending_recovery().unwrap();
        assert_eq!(
            engine
                .legacy_recover_authenticated(&mut service, TIMEOUT, selected(executable))
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
            .legacy_operator_upgrade(&mut service, TIMEOUT, || Ok(request(OLD, NEW)))
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
            .legacy_recover_authenticated(&mut service, TIMEOUT, executable)
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
            .legacy_recover_authenticated(&mut FakeService::absent(), TIMEOUT, selected(OLD))
            .is_err());
        assert_eq!(disk_matches_receipt(home.path()), OLD);
        assert!(!engine.paths.journal.exists());
    }
    prepare_recovery(home.path(), false);
    let mut engine = open(home.path());
    let mut service = FakeService::absent();
    service.incompatible = Some(selected(OLD).artifact);
    let error = engine
        .legacy_recover_authenticated(&mut service, TIMEOUT, selected(OLD))
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
            .legacy_operator_rollback(&mut service, TIMEOUT)
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
        .legacy_operator_rollback(&mut FakeService::absent(), TIMEOUT)
        .is_err());
    install(home.path(), &mut FakeService::absent(), OLD, NEW);
    let engine = open(home.path());
    std::fs::remove_file(engine.paths.artifact(&selected(OLD).artifact)).unwrap();
    drop(engine);
    assert!(open(home.path())
        .legacy_operator_rollback(&mut FakeService::absent(), TIMEOUT)
        .is_err());
    assert_eq!(disk_matches_receipt(home.path()), NEW);

    let home = fixture();
    let mut service = FakeService::running(home.path());
    service.restart_failure = true;
    assert!(open(home.path())
        .legacy_operator_upgrade(&mut service, TIMEOUT, || Ok(request(OLD, NEW)))
        .is_err());
    let engine = open(home.path());
    std::fs::remove_file(engine.paths.artifact(&selected(OLD).artifact)).unwrap();
    drop(engine);
    service.restart_failure = false;
    assert!(open(home.path())
        .legacy_recover(&mut service, TIMEOUT)
        .is_err());
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
            .legacy_recover(&mut FakeService::absent(), TIMEOUT)
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
        .legacy_operator_upgrade(&mut FakeService::absent(), TIMEOUT, || Ok(request(
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
        .legacy_operator_upgrade(&mut service, TIMEOUT, || Ok(request(OLD, NEW)))
        .is_err());
    let mut edited = Receipt::load(&home.path().join(".ww")).unwrap();
    edited.hashes[1] = "external-edit".into();
    let edited = edited.encode().unwrap();
    std::fs::write(home.path().join(".ww/installation.json"), &edited).unwrap();
    assert!(open(home.path())
        .legacy_recover(&mut service, TIMEOUT)
        .is_err());
    assert_eq!(
        std::fs::read(home.path().join(".ww/installation.json")).unwrap(),
        edited
    );
}

#[test]
fn zero_deadline_never_calls_selection() {
    let home = fixture();
    let result = open(home.path()).legacy_operator_upgrade::<Cursor<Vec<u8>>>(
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
        "upgrade" => {
            engine.legacy_operator_upgrade(&mut service, TIMEOUT, || Ok(request(OLD, NEW)))
        }
        "second-upgrade" => {
            engine.legacy_operator_upgrade(&mut service, TIMEOUT, || Ok(request(NEW, THIRD)))
        }
        "rollback" => engine.legacy_operator_rollback(&mut service, TIMEOUT),
        "recover" => engine.legacy_recover(&mut service, TIMEOUT),
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
                .legacy_recover(&mut service, TIMEOUT)
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
        .legacy_operator_upgrade(&mut service, TIMEOUT, || Ok(request(OLD, NEW)))
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
            .legacy_recover(&mut FakeService::absent(), TIMEOUT)
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
        .legacy_operator_upgrade(&mut service, Duration::from_secs(2), || {
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
        .legacy_operator_upgrade(&mut FakeService::absent(), TIMEOUT, || {
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
            .legacy_operator_upgrade(&mut FakeService::absent(), TIMEOUT, || Ok(request(
                current, NEW
            )))
            .is_err());
        drop(engine);
        storage::faults::configure(None);
        let mut engine = open(home.path());
        engine
            .legacy_recover(&mut FakeService::absent(), TIMEOUT)
            .unwrap();
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
        "upgrade" => {
            baseline_engine.legacy_operator_upgrade(&mut FakeService::absent(), TIMEOUT, || {
                Ok(request(OLD, NEW))
            })
        }
        "recover" => baseline_engine.legacy_recover(&mut FakeService::absent(), TIMEOUT),
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
        "upgrade" => engine.legacy_operator_upgrade(&mut FakeService::absent(), TIMEOUT, || {
            Ok(request(OLD, NEW))
        }),
        "recover" => engine.legacy_recover(&mut FakeService::absent(), TIMEOUT),
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
    engine
        .legacy_recover(&mut FakeService::absent(), TIMEOUT)
        .unwrap();
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
        .legacy_operator_upgrade(&mut service, TIMEOUT, || Ok(request(OLD, NEW)))
        .is_err());
    assert!(root.join("host-upgrade.json").exists());
    let recovery = open(home.path())
        .legacy_operator_upgrade::<&[u8]>(&mut service, TIMEOUT, || {
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
        .legacy_operator_rollback(&mut service, TIMEOUT)
        .unwrap();
    assert_eq!(std::fs::read(root.join("bin/ww")).unwrap(), OLD);
    assert_eq!(Receipt::load(&root).unwrap().version, 2);
    unchanged_user_state(home.path());
}

// Genuine signatures over disposable protocol fixtures. These are not producer
// artifacts and never qualify native restart or the public release pipeline.
fn signed_generation(bytes: &[u8], sequence: u64) -> provenance::AuthenticatedGeneration {
    signed_generation_with_key(bytes, sequence, [7; 32])
}

fn signed_generation_with_key(
    bytes: &[u8],
    sequence: u64,
    seed: [u8; 32],
) -> provenance::AuthenticatedGeneration {
    use crate::release::auth::TrustedKeys;
    use crate::release::descriptor::{
        Artifact, AuthenticatedRelease, DeploymentArtifact, Descriptor, HostArtifact, Provenance,
        SignedReleaseProof, COMPATIBILITY_PROFILE, SOURCE_REPOSITORY,
    };
    use ed25519_dalek::{Signer, SigningKey};
    fn cid(codec: u64, bytes: &[u8]) -> cid::Cid {
        cid::Cid::new_v1(
            codec,
            cid::multihash::Multihash::wrap(0x12, &sha2::Sha256::digest(bytes)).unwrap(),
        )
    }
    fn file(path: &str, bytes: &[u8]) -> Artifact {
        Artifact {
            path: path.into(),
            root: cid(0x55, bytes),
            sha256: sha2::Sha256::digest(bytes).into(),
            size: bytes.len() as u64,
        }
    }
    let key = SigningKey::from_bytes(&seed);
    let mut der = vec![
        0x30, 0x2a, 0x30, 0x05, 0x06, 0x03, 0x2b, 0x65, 0x70, 0x03, 0x21, 0x00,
    ];
    der.extend_from_slice(key.verifying_key().as_bytes());
    let alphabet = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut keys = Vec::new();
    for block in der.chunks(3) {
        let value = (u32::from(block[0]) << 16)
            | (u32::from(*block.get(1).unwrap_or(&0)) << 8)
            | u32::from(*block.get(2).unwrap_or(&0));
        keys.extend([
            alphabet[(value >> 18) as usize],
            alphabet[((value >> 12) & 63) as usize],
            if block.len() > 1 {
                alphabet[((value >> 6) & 63) as usize]
            } else {
                b'='
            },
            if block.len() > 2 {
                alphabet[(value & 63) as usize]
            } else {
                b'='
            },
        ]);
    }
    keys.push(b'\n');
    let target = "aarch64-unknown-linux-gnu";
    let source = Provenance {
        repository: SOURCE_REPOSITORY.into(),
        revision: "a".repeat(40),
        builder: "disposable-unit-fixture".into(),
    };
    let host = file(
        crate::release::descriptor::platform_path(target).unwrap(),
        bytes,
    );
    let kernel = file("fhs/boot/kernel.wasm", b"kernel fixture");
    let status = file("fhs/bin/status.wasm", b"status fixture");
    let descriptor = Descriptor {
        channel: crate::ipns::canonical_name(
            libp2p::identity::Keypair::ed25519_from_bytes([42; 32])
                .unwrap()
                .public()
                .to_peer_id(),
        ),
        sequence,
        version: "0.1.1".into(),
        source: source.clone(),
        hosts: vec![HostArtifact {
            target: target.into(),
            artifact: host.clone(),
            provenance: source,
            compatibility: COMPATIBILITY_PROFILE.into(),
        }],
        deployment: DeploymentArtifact {
            root: cid(0x70, b"disposable FHS"),
            kernel: kernel.clone(),
            status: status.clone(),
            compatibility: COMPATIBILITY_PROFILE.into(),
        },
    };
    let binding = descriptor.binding().unwrap();
    let mut manifest = format!(
        "# sha256\n{}  RELEASE-DESCRIPTOR\n",
        hex::encode(sha2::Sha256::digest(&binding))
    );
    for artifact in [&host, &kernel, &status] {
        manifest.push_str(&format!(
            "{}  {}\n",
            hex::encode(artifact.sha256),
            artifact.path
        ));
    }
    let descriptor_cid = descriptor.cid().unwrap();
    let mut link = vec![0x0a, descriptor_cid.to_bytes().len() as u8];
    link.extend(descriptor_cid.to_bytes());
    link.extend([0x12, 18]);
    link.extend(b"release-descriptor");
    let mut root_bytes = vec![0x0a, 2, 8, 1, 0x12, link.len() as u8];
    root_bytes.extend(link);
    let release = AuthenticatedRelease::authenticate(
        &TrustedKeys::from_bytes(&keys).unwrap(),
        manifest.as_bytes(),
        &key.sign(manifest.as_bytes()).to_bytes(),
        &binding,
        descriptor_cid,
        &descriptor.encode().unwrap(),
    )
    .unwrap();
    let proof = SignedReleaseProof::capture(cid(0x70, &root_bytes), &root_bytes, &release).unwrap();
    provenance::AuthenticatedGeneration::from_proof(proof, &keys, target).unwrap()
}

#[test]
fn steady_start_requires_exact_committed_proof_locator_receipt_and_contract() {
    use startup::{
        InitializationIdentityPolicy, InstanceLease, InvocationBinding, StartupContractV1,
        StartupDisposition, StartupLease, StartupStateV1,
    };
    let home = fixture();
    let generation = signed_generation(OLD, 1);
    let engine = open(home.path());
    let _instance = InstanceLease::exclusive_create(engine.mutation_lock()).unwrap();
    let mut lease = StartupLease::exclusive_create(engine.mutation_lock()).unwrap();
    let mut receipt = Receipt::load(&home.path().join(".ww")).unwrap();
    receipt.complete = false;
    let receipt_bytes = receipt.encode().unwrap();
    let contract = StartupContractV1::new(
        [9; 32],
        1,
        generation.artifact().clone(),
        FakeService::absent().startup,
        InvocationBinding::Standalone,
        PathBuf::from("/etc/wetware/release-keys"),
    )
    .unwrap();
    let mut state = StartupStateV1 {
        version: 1,
        installation_id: [9; 32],
        native: generation.record().clone(),
        trusted_release_keys: contract.trusted_release_keys.clone(),
        disposition: StartupDisposition::RequiredUnbound,
        initializing_receipt: Some(receipt_bytes.clone()),
        initializing_receipt_sha256: Some(sha2::Sha256::digest(&receipt_bytes).into()),
        initializing_identity: Some(InitializationIdentityPolicy::PreserveExisting),
        initializing_previous_proofs: Vec::new(),
    };
    lease.write_state(&state).unwrap();
    storage::write_record(&engine.paths.receipt, &receipt_bytes).unwrap();
    lease.initialize_marker(&state, &generation).unwrap();
    engine.paths.create_proof_directory().unwrap();
    storage::write_proof(
        &engine.paths.proof(&generation.record().proof).unwrap(),
        generation.proof_bytes(),
    )
    .unwrap();
    let locator = serde_json::to_vec(
        &provenance::InstalledRelease::from_root(
            generation.record().transport_root.parse().unwrap(),
        )
        .unwrap(),
    )
    .unwrap();
    storage::write_record(&engine.paths.locator, &locator).unwrap();
    let retained = RetainedV2 {
        version: 2,
        installation_id: [9; 32],
        current: generation.record().clone(),
        previous: None,
        contract: Some(contract.identity.clone()),
    };
    storage::write_record(&engine.paths.retained, &retained.encode().unwrap()).unwrap();
    receipt.complete = true;
    storage::write_record(&engine.paths.receipt, &receipt.encode().unwrap()).unwrap();
    state.initializing_receipt = None;
    state.initializing_receipt_sha256 = None;
    state.initializing_identity = None;
    lease.write_state(&state).unwrap();
    lease.write_contract(&contract).unwrap();
    state.disposition = StartupDisposition::Committed {
        contract: contract.identity.clone(),
    };
    lease.write_state(&state).unwrap();
    validate_steady_start(&lease, &state, &contract).unwrap();
    storage::write_record(&engine.paths.locator, b"{}").unwrap();
    assert!(validate_steady_start(&lease, &state, &contract).is_err());
    storage::write_record(&engine.paths.locator, &locator).unwrap();
    storage::remove(&engine.paths.proof(&generation.record().proof).unwrap()).unwrap();
    assert!(validate_steady_start(&lease, &state, &contract).is_err());
}

fn initialization_fixture() -> (
    tempfile::TempDir,
    provenance::AuthenticatedGeneration,
    provenance::IndependentTrustSource,
    Receipt,
) {
    let home = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(home.path().join(".ww/bin")).unwrap();
    let generation = signed_generation(OLD, 1);
    let keys = home.path().join("release-keys");
    std::fs::write(&keys, &generation.record().verification_keys).unwrap();
    let trust = provenance::IndependentTrustSource::from_path(&keys).unwrap();
    let receipt = Receipt {
        version: 2,
        hashes: [
            blake3::hash(OLD).to_hex().to_string(),
            blake3::hash(b"kernel fixture").to_hex().to_string(),
            blake3::hash(b"status fixture").to_hex().to_string(),
            String::new(),
            String::new(),
        ],
        previous: None,
        complete: false,
    };
    (home, generation, trust, receipt)
}

fn write_initialization_software(home: &Path) {
    std::fs::write(home.join(".ww/bin/ww"), OLD).unwrap();
    std::fs::set_permissions(
        home.join(".ww/bin/ww"),
        std::fs::Permissions::from_mode(0o755),
    )
    .unwrap();
    std::fs::create_dir_all(home.join(".ww/fhs/boot")).unwrap();
    std::fs::create_dir_all(home.join(".ww/fhs/bin")).unwrap();
    std::fs::write(home.join(".ww/fhs/boot/kernel.wasm"), b"kernel fixture").unwrap();
    std::fs::write(home.join(".ww/fhs/bin/status.wasm"), b"status fixture").unwrap();
}

#[test]
fn initialization_holds_leases_and_completes_exact_authenticated_tuple_unbound() {
    storage::faults::configure(None);
    let (home, generation, trust, mut receipt) = initialization_fixture();
    let mut engine = open(home.path());
    let guard = engine
        .prepare_initialization(&generation, &receipt, &trust)
        .unwrap();
    assert!(guard.may_create_identity().unwrap());
    assert!(startup::InstanceLease::shared(home.path()).is_err());
    assert!(startup::StartupLease::shared(home.path()).is_err());
    assert!(
        !Receipt::decode(&std::fs::read(home.path().join(".ww/installation.json")).unwrap())
            .unwrap()
            .complete
    );
    write_initialization_software(home.path());
    std::fs::write(
        home.path().join(".ww/identity"),
        b"private key fixture, not release authority",
    )
    .unwrap();
    std::fs::set_permissions(
        home.path().join(".ww/identity"),
        std::fs::Permissions::from_mode(0o600),
    )
    .unwrap();
    receipt.complete = true;
    guard.finish(&receipt).unwrap();
    let lease = startup::StartupLease::shared(home.path()).unwrap();
    let state = lease.read_state().unwrap().unwrap();
    assert_eq!(
        state.disposition,
        startup::StartupDisposition::RequiredUnbound
    );
    assert!(state.initializing_receipt.is_none());
    assert!(lease.read_contract().unwrap().is_none());
    let retained = read_retained_v2(&engine.paths).unwrap();
    assert_eq!(retained.current, *generation.record());
    assert!(retained.previous.is_none());
    validate_committed_native(&engine.paths, &state, &retained).unwrap();
}

#[test]
fn initialization_retry_refuses_another_signed_root_with_identical_host_bytes() {
    storage::faults::configure(None);
    let (home, generation, trust, receipt) = initialization_fixture();
    let mut engine = open(home.path());
    drop(
        engine
            .prepare_initialization(&generation, &receipt, &trust)
            .unwrap(),
    );
    let other = signed_generation(OLD, 2);
    assert!(engine
        .prepare_initialization(&other, &receipt, &trust)
        .is_err());
    let guard = engine
        .prepare_initialization(&generation, &receipt, &trust)
        .unwrap();
    assert!(guard.may_create_identity().unwrap());
}

pub(super) fn completed_initialization() -> (
    tempfile::TempDir,
    Engine,
    provenance::AuthenticatedGeneration,
) {
    let (home, generation, trust, mut receipt) = initialization_fixture();
    let mut engine = open(home.path());
    let guard = engine
        .prepare_initialization(&generation, &receipt, &trust)
        .unwrap();
    write_initialization_software(home.path());
    std::fs::write(home.path().join(".ww/identity"), b"identity fixture").unwrap();
    std::fs::set_permissions(
        home.path().join(".ww/identity"),
        std::fs::Permissions::from_mode(0o600),
    )
    .unwrap();
    receipt.complete = true;
    guard.finish(&receipt).unwrap();
    (home, engine, generation)
}

fn admission_inputs(home: &Path) -> inputs::PreflightInputs {
    inputs::PreflightInputs {
        version: 1,
        config: inputs::EffectiveRunConfig {
            cwd: home.to_owned(), home: home.to_owned(), uid: unsafe { libc::geteuid() },
            mounts: Vec::new(), fhs_root: None, namespace_roots: Vec::new(), listen: Vec::new(),
            wasm_debug: false, identity_path: Some(home.join(".ww/identity")), insecure_ephemeral: false,
            stem: None, ipns_stem: None, ipns_routing_url: "http://127.0.0.1:8180".into(),
            rpc_url: "http://127.0.0.1:8545".into(), ws_url: "ws://127.0.0.1:8546".into(),
            confirmation_depth: 1, executor_threads: 1, http_listen: None, http_dial: Vec::new(),
            runtime_cache_policy: inputs::RuntimeCachePolicy::Isolated, http_admin: None,
            ipfs_url: "http://127.0.0.1:5001".into(),
            runtime: inputs::RuntimeSettings {
                kubo_wait_max_secs: 1, kubo_boot_retry_max_secs: 1, kubo_operation_timeout_secs: 1,
                compile_workers: 1, tracing: serde_json::from_value(serde_json::json!({"filter":"warn","json":false,"ansi":false,"invalid_format":false,"invalid_telemetry":false,"telemetry":null})).unwrap(),
                compilation_cache: crate::cell::engine::CapturedCacheSettings::from_values(None,None),
                interactive: false, oci_image_id: None, unsupported_environment: Vec::new(),
            },
        },
        namespaces: Vec::new(), identity_fingerprint: [12;32], operation_digests: Vec::new(),
        has_baseline: false, sources: Vec::new(), publisher: None,
    }
}

fn desired_contract(
    engine: &Engine,
    inputs: &inputs::PreflightInputs,
) -> startup::StartupContractV1 {
    let state = engine.startup_state().unwrap();
    let generation = engine
        .startup_contract()
        .unwrap()
        .map(|c| c.identity.next_generation().unwrap())
        .unwrap_or(1);
    startup::StartupContractV1::new(
        state.installation_id,
        generation,
        state.native.artifact,
        inputs.startup_identity().unwrap(),
        startup::InvocationBinding::Standalone,
        state.trusted_release_keys,
    )
    .unwrap()
}

#[test]
fn admission_checks_without_metadata_writes_then_commits_one_contract() {
    storage::faults::configure(None);
    let (home, mut engine, generation) = completed_initialization();
    let inputs = admission_inputs(home.path());
    let desired = desired_contract(&engine, &inputs);
    let state_before = std::fs::read(engine.paths.store.join("startup-state.json")).unwrap();
    let retained_before = std::fs::read(&engine.paths.retained).unwrap();
    let mut service = FakeService::absent();
    let guard = engine
        .begin_admission(&mut service, Instant::now() + TIMEOUT)
        .unwrap();
    assert!(startup::InstanceLease::shared(home.path()).is_err());
    assert!(matches!(
        guard
            .finish(&mut service, desired.clone(), &inputs, true)
            .unwrap(),
        AdmissionOutcome::Checked { .. }
    ));
    assert_eq!(
        std::fs::read(engine.paths.store.join("startup-state.json")).unwrap(),
        state_before
    );
    assert_eq!(
        std::fs::read(&engine.paths.retained).unwrap(),
        retained_before
    );
    assert!(!engine.paths.journal.exists());
    assert!(matches!(
        engine
            .operator_admit(&mut service, TIMEOUT, desired.clone(), &inputs, false)
            .unwrap(),
        AdmissionOutcome::Admitted { .. }
    ));
    assert_eq!(engine.startup_contract().unwrap(), Some(desired));
    assert_eq!(
        engine.current_generation().unwrap().record(),
        generation.record()
    );
    assert!(!engine.paths.journal.exists());
    assert_eq!(
        (service.restarts, service.stops, service.effects),
        (0, 0, 0)
    );
}

#[test]
fn admission_refuses_running_process_and_expired_deadline_before_capture() {
    storage::faults::configure(None);
    let (home, mut engine, _) = completed_initialization();
    let mut running = FakeService::running(home.path());
    assert!(engine
        .begin_admission(&mut running, Instant::now() + TIMEOUT)
        .is_err());
    let mut stopped = FakeService::absent();
    assert!(engine
        .begin_admission(&mut stopped, Instant::now())
        .is_err());
    let live = startup::InstanceLease::shared(home.path()).unwrap();
    assert!(engine
        .begin_admission(&mut stopped, Instant::now() + TIMEOUT)
        .is_err());
    drop(live);
    assert!(!engine.paths.journal.exists());
}

fn run_admission_fixture(home: &Path) -> Result<()> {
    let mut engine = Engine::open(home)?;
    let mut inputs = admission_inputs(home);
    inputs.config.rpc_url.push_str("/new-admission");
    let desired = desired_contract(&engine, &inputs);
    engine.operator_admit(&mut FakeService::absent(), TIMEOUT, desired, &inputs, false)?;
    Ok(())
}

#[test]
#[ignore = "subprocess worker; invoked by admission crash matrix"]
fn admission_crash_worker() {
    let home = PathBuf::from(std::env::var_os("WW_ADMISSION_CRASH_HOME").unwrap());
    let point: usize = std::env::var("WW_ADMISSION_CRASH_POINT")
        .unwrap()
        .parse()
        .unwrap();
    storage::faults::exit_at(point);
    run_admission_fixture(&home).unwrap();
    panic!("requested admission crash was not reached");
}

#[test]
fn admission_process_exit_at_every_durable_boundary_restores_or_commits_exact_contract() {
    for readmission in [false, true] {
        let prepare = || {
            storage::faults::configure(None);
            let (home, mut engine, generation) = completed_initialization();
            if readmission {
                let inputs = admission_inputs(home.path());
                let desired = desired_contract(&engine, &inputs);
                engine
                    .operator_admit(&mut FakeService::absent(), TIMEOUT, desired, &inputs, false)
                    .unwrap();
            }
            let original = engine.startup_contract().unwrap();
            drop(engine);
            (home, original, generation)
        };
        let (baseline, _, _) = prepare();
        storage::faults::configure(None);
        run_admission_fixture(baseline.path()).unwrap();
        let trace = storage::faults::trace();
        assert!(trace.iter().any(|s| s == "admit.committed"));
        for (index, label) in trace.iter().enumerate() {
            let (home, original, generation) = prepare();
            let receipt = std::fs::read(home.path().join(".ww/installation.json")).unwrap();
            let locator = std::fs::read(home.path().join(".ww/installed-release.json")).unwrap();
            let status = Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "upgrade::tests::admission_crash_worker",
                    "--ignored",
                    "--nocapture",
                ])
                .env("WW_ADMISSION_CRASH_HOME", home.path())
                .env("WW_ADMISSION_CRASH_POINT", (index + 1).to_string())
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .status()
                .unwrap();
            assert_eq!(
                status.code(),
                Some(86),
                "{readmission} {} {label}",
                index + 1
            );
            storage::faults::configure(None);
            let mut engine = open(home.path());
            engine
                .recover_admission(&mut FakeService::absent(), Instant::now() + TIMEOUT, false)
                .unwrap_or_else(|e| panic!("{readmission} {} {label}: {e:#}", index + 1));
            assert_eq!(
                engine.current_generation().unwrap().record(),
                generation.record()
            );
            assert_eq!(std::fs::read(&engine.paths.receipt).unwrap(), receipt);
            assert_eq!(std::fs::read(&engine.paths.locator).unwrap(), locator);
            assert_eq!(std::fs::read(&engine.paths.executable).unwrap(), OLD);
            assert!(!engine.paths.journal.exists());
            let resulting = engine.startup_contract().unwrap();
            if resulting != original {
                let resulting = resulting.unwrap();
                assert_eq!(
                    resulting.identity.generation,
                    if readmission { 2 } else { 1 }
                );
                let lease = startup::StartupLease::shared(home.path()).unwrap();
                validate_steady_start(&lease, &engine.startup_state().unwrap(), &resulting)
                    .unwrap();
            }
        }
        eprintln!(
            "admission readmission={readmission}: {} real process-exit checkpoints",
            trace.len()
        );
    }
}

fn fixture_process(pid: u32) -> service::ProcessInstance {
    service::ProcessInstance {
        pid,
        manager_invocation_id: format!("{pid:032x}"),
        start: attestation::ProcessStartToken::Linux {
            boot_id: "00000000-0000-0000-0000-000000000001".into(),
            start_ticks: u64::from(pid),
        },
        launch_nonce: [(pid % 255 + 1) as u8; 32],
    }
}

fn native_fixture(
    running: bool,
) -> (
    tempfile::TempDir,
    Engine,
    provenance::AuthenticatedGeneration,
    inputs::PreflightInputs,
    FakeService,
) {
    let (home, mut engine, current) = completed_initialization();
    let inputs = admission_inputs(home.path());
    let mut contract = desired_contract(&engine, &inputs);
    let mut service = FakeService::absent();
    if running {
        let identity = startup::ServiceIdentity {
            manager: startup::ServiceManager::Systemd,
            domain: "user/1000".into(),
            name: "ww.service".into(),
            definition: [1; 32],
            drop_ins: [2; 32],
            environment_files: [3; 32],
            effective_argv: [4; 32],
            effective_environment: [5; 32],
            registered: true,
            enabled: EnabledState::Enabled,
        };
        contract = startup::StartupContractV1::new(
            contract.installation_id,
            1,
            contract.artifact,
            contract.startup,
            startup::InvocationBinding::Managed { service: identity },
            contract.trusted_release_keys,
        )
        .unwrap();
        service = FakeService::running(home.path());
        service.snapshot.running = RunningState::Stopped;
        service.running_artifact = None;
    }
    service.startup = inputs.startup_identity().unwrap();
    engine
        .operator_admit(&mut service, TIMEOUT, contract, &inputs, false)
        .unwrap();
    if running {
        service.snapshot.running = RunningState::Running { pid: 321 };
        service.running_artifact = Some(current.artifact().clone());
    }
    (home, engine, current, inputs, service)
}

fn native_request(
    current: provenance::AuthenticatedGeneration,
    inputs: inputs::PreflightInputs,
    bytes: &[u8],
    sequence: u64,
) -> UpgradeRequest<Cursor<Vec<u8>>> {
    UpgradeRequest {
        installed: current,
        candidate: signed_generation(bytes, sequence),
        bytes: Cursor::new(bytes.to_vec()),
        inputs,
    }
}

#[test]
fn native_v2_stopped_upgrade_and_rollback_preserve_complete_tuple_and_inputs() {
    storage::faults::configure(None);
    let (home, mut engine, current, inputs, mut service) = native_fixture(false);
    let before = engine.startup_contract().unwrap().unwrap();
    assert_eq!(
        engine
            .operator_upgrade(&mut service, TIMEOUT, || Ok(native_request(
                current,
                inputs.clone(),
                NEW,
                2
            )))
            .unwrap(),
        Outcome::Installed { live_ready: false }
    );
    assert_eq!(std::fs::read(&engine.paths.executable).unwrap(), NEW);
    let retained = read_retained_v2(&engine.paths).unwrap();
    assert_eq!(retained.current, *signed_generation(NEW, 2).record());
    assert_eq!(
        retained.previous.as_ref(),
        Some(signed_generation(OLD, 1).record())
    );
    let after = engine.startup_contract().unwrap().unwrap();
    assert_eq!(after.startup, before.startup);
    assert_eq!(after.identity.generation, 2);
    assert_eq!(
        engine
            .operator_rollback_prepared_until(&mut service, Instant::now() + TIMEOUT, &inputs)
            .unwrap(),
        Outcome::Installed { live_ready: false }
    );
    assert_eq!(std::fs::read(home.path().join(".ww/bin/ww")).unwrap(), OLD);
    assert_eq!(
        engine
            .startup_contract()
            .unwrap()
            .unwrap()
            .identity
            .generation,
        3
    );
    assert_eq!(
        (service.restarts, service.stops, service.effects),
        (0, 0, 0)
    );
    assert!(!engine.paths.journal.exists());
}

#[test]
fn native_v2_running_upgrade_consumes_exact_permit_and_recovers_failed_restart() {
    storage::faults::configure(None);
    let (_home, mut engine, current, inputs, mut service) = native_fixture(true);
    service.restart_failure = true;
    assert!(engine
        .operator_upgrade(&mut service, TIMEOUT, || Ok(native_request(
            current,
            inputs.clone(),
            NEW,
            2
        )))
        .is_err());
    assert_eq!(std::fs::read(&engine.paths.executable).unwrap(), NEW);
    assert!(engine.paths.journal.exists());
    service.restart_failure = false;
    assert!(matches!(
        engine
            .recover_prepared_until(&mut service, Instant::now() + TIMEOUT, &inputs)
            .unwrap(),
        Outcome::Recovered {
            live_ready: true,
            ..
        }
    ));
    assert_eq!(std::fs::read(&engine.paths.executable).unwrap(), OLD);
    assert_eq!(
        engine
            .startup_contract()
            .unwrap()
            .unwrap()
            .identity
            .generation,
        1
    );
    assert_eq!(
        service.running_artifact.as_ref(),
        Some(signed_generation(OLD, 1).artifact())
    );
    assert!(!engine.paths.journal.exists());
}

fn run_native_fixture_operation(home: &Path, action: &str) -> Result<Outcome> {
    let mut engine = Engine::open(home)?;
    let mut service: FakeService =
        serde_json::from_slice(&std::fs::read(home.join("simulated-native-v2.json"))?)?;
    let inputs = admission_inputs(home);
    match action {
        "upgrade" => {
            let current = engine.current_generation()?;
            engine.operator_upgrade(&mut service, TIMEOUT, || {
                Ok(native_request(current, inputs, NEW, 2))
            })
        }
        "second" => {
            let current = engine.current_generation()?;
            engine.operator_upgrade(&mut service, TIMEOUT, || {
                Ok(native_request(current, inputs, THIRD, 3))
            })
        }
        "rollback" => {
            engine.operator_rollback_prepared_until(&mut service, Instant::now() + TIMEOUT, &inputs)
        }
        "recover" => engine.recover_prepared_until(&mut service, Instant::now() + TIMEOUT, &inputs),
        _ => bail!("unknown native crash action"),
    }
}

#[test]
#[ignore = "subprocess worker; invoked by native v2 crash matrix"]
fn native_v2_crash_worker() {
    let home = PathBuf::from(std::env::var_os("WW_NATIVE_V2_HOME").unwrap());
    let point: usize = std::env::var("WW_NATIVE_V2_POINT")
        .unwrap()
        .parse()
        .unwrap();
    let action = std::env::var("WW_NATIVE_V2_ACTION").unwrap();
    storage::faults::exit_at(point);
    run_native_fixture_operation(&home, &action).unwrap();
    panic!("requested native v2 crash point did not execute");
}

fn prepare_native_crash(running: bool, action: &str) -> tempfile::TempDir {
    storage::faults::configure(None);
    let (home, engine, _, _, mut service) = native_fixture(running);
    service.persistent_state = Some(home.path().join("simulated-native-v2.json"));
    service.save_state().unwrap();
    drop(engine);
    if matches!(action, "rollback" | "second") {
        run_native_fixture_operation(home.path(), "upgrade").unwrap();
    }
    if action == "recover" {
        let baseline = prepare_native_crash(running, "upgrade");
        storage::faults::configure(None);
        run_native_fixture_operation(baseline.path(), "upgrade").unwrap();
        let trace = storage::faults::trace();
        let point = trace
            .iter()
            .position(|s| s == "native.phase.Installed")
            .unwrap()
            + 1;
        storage::faults::configure(Some(point));
        assert!(run_native_fixture_operation(home.path(), "upgrade").is_err());
        storage::faults::configure(None);
    }
    home
}

#[test]
fn native_v2_process_exit_at_every_upgrade_rollback_and_recovery_boundary() {
    // Cargo can replace the shared test binary while another focused test builds.
    // Every crash worker must execute this matrix's immutable binary snapshot.
    let worker_directory = tempfile::tempdir().unwrap();
    let worker_executable = worker_directory.path().join("native-v2-worker");
    std::fs::copy(std::env::current_exe().unwrap(), &worker_executable).unwrap();
    for running in [false, true] {
        for action in ["upgrade", "second", "rollback", "recover"] {
            let baseline = prepare_native_crash(running, action);
            storage::faults::configure(None);
            run_native_fixture_operation(baseline.path(), action).unwrap();
            let trace = storage::faults::trace();
            assert!(
                trace.len() > 20,
                "native crash trace did not execute transaction boundaries"
            );
            for (index, label) in trace.iter().enumerate() {
                let home = prepare_native_crash(running, action);
                let status = Command::new(&worker_executable)
                    .args([
                        "--exact",
                        "upgrade::tests::native_v2_crash_worker",
                        "--ignored",
                        "--nocapture",
                    ])
                    .env("WW_NATIVE_V2_HOME", home.path())
                    .env("WW_NATIVE_V2_POINT", (index + 1).to_string())
                    .env("WW_NATIVE_V2_ACTION", action)
                    .stdout(std::process::Stdio::null())
                    .stderr(std::process::Stdio::null())
                    .status()
                    .unwrap();
                assert_eq!(
                    status.code(),
                    Some(86),
                    "running={running} {action} {} {label}",
                    index + 1
                );
                storage::faults::configure(None);
                run_native_fixture_operation(home.path(), "recover").unwrap_or_else(|e| {
                    panic!("running={running} {action} {} {label}: {e:#}", index + 1)
                });
                let engine = open(home.path());
                let contract = engine.startup_contract().unwrap().unwrap();
                let lease = startup::StartupLease::shared(home.path()).unwrap();
                validate_steady_start(&lease, &engine.startup_state().unwrap(), &contract).unwrap();
                let state = engine.startup_state().unwrap();
                let retained = read_retained_v2(&engine.paths).unwrap();
                assert_eq!(state.native, retained.current);
                assert!(!engine.paths.journal.exists());
                let service: FakeService = serde_json::from_slice(
                    &std::fs::read(home.path().join("simulated-native-v2.json")).unwrap(),
                )
                .unwrap();
                if running {
                    assert_eq!(
                        service.running_artifact.as_ref(),
                        Some(&state.native.artifact)
                    );
                } else {
                    assert_eq!(service.snapshot.running, RunningState::Stopped);
                    assert_eq!(service.restarts, 0);
                }
                assert!(
                    std::fs::read_dir(&engine.paths.proofs).unwrap().count() <= 2,
                    "proof leak after {action} {label}"
                );
            }
            eprintln!(
                "native v2 running={running} action={action}: {} real process-exit checkpoints",
                trace.len()
            );
        }
    }
}

#[test]
fn native_v2_pending_child_rereads_current_trust_but_restored_before_uses_exact_history() {
    storage::faults::configure(None);
    let (home, mut engine, current, inputs, mut service) = native_fixture(true);
    let original = engine.startup_contract().unwrap().unwrap();
    service.interrupt_guarded_restart = true;
    assert!(std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        engine.operator_upgrade(&mut service, TIMEOUT, || {
            Ok(native_request(current, inputs.clone(), NEW, 2))
        })
    }))
    .is_err());
    service.interrupt_guarded_restart = false;
    {
        let lease = startup::StartupLease::shared(home.path()).unwrap();
        assert!(
            validate_transaction_start(&lease, &original.invocation, Instant::now() + TIMEOUT)
                .is_ok()
        );
        let unrelated = signed_generation_with_key(NEW, 2, [8; 32]);
        std::fs::write(
            &original.trusted_release_keys,
            &unrelated.record().verification_keys,
        )
        .unwrap();
        assert!(
            validate_transaction_start(&lease, &original.invocation, Instant::now() + TIMEOUT)
                .is_err()
        );
    }
    service.restart_failure = false;
    engine
        .recover_prepared_until(&mut service, Instant::now() + TIMEOUT, &inputs)
        .unwrap();
    assert_eq!(
        engine.current_generation().unwrap().record(),
        signed_generation(OLD, 1).record()
    );
    assert_eq!(engine.startup_contract().unwrap(), Some(original));
}

#[test]
fn native_v2_signed_proof_substitution_and_loss_refuse_without_changing_accepted_tuple() {
    storage::faults::configure(None);
    let (_home, mut engine, current, inputs, mut service) = native_fixture(false);
    engine
        .operator_upgrade(&mut service, TIMEOUT, || {
            Ok(native_request(current, inputs, NEW, 2))
        })
        .unwrap();
    let retained = read_retained_v2(&engine.paths).unwrap();
    let proof = engine.paths.proof(&retained.current.proof).unwrap();
    let exact = storage::read_proof(&proof).unwrap();
    let unrelated = signed_generation(NEW, 77);
    std::fs::write(&proof, unrelated.proof_bytes()).unwrap();
    assert!(engine.current_generation().is_err());
    assert_eq!(std::fs::read(&engine.paths.executable).unwrap(), NEW);
    std::fs::write(&proof, &exact).unwrap();
    assert_eq!(
        engine.current_generation().unwrap().record(),
        &retained.current
    );
    std::fs::remove_file(&proof).unwrap();
    assert!(engine.current_generation().is_err());
    assert!(!engine.paths.journal.exists());
}

#[test]
fn native_v2_same_artifact_other_signed_root_preserves_contract_locator_and_history() {
    storage::faults::configure(None);
    let (_home, mut engine, current, inputs, mut service) = native_fixture(false);
    let contract = engine.startup_contract().unwrap();
    let locator = std::fs::read(&engine.paths.locator).unwrap();
    let retained = std::fs::read(&engine.paths.retained).unwrap();
    assert_eq!(
        engine
            .operator_upgrade(&mut service, TIMEOUT, || Ok(native_request(
                current, inputs, OLD, 2
            )))
            .unwrap(),
        Outcome::AlreadyInstalled
    );
    assert_eq!(engine.startup_contract().unwrap(), contract);
    assert_eq!(std::fs::read(&engine.paths.locator).unwrap(), locator);
    assert_eq!(std::fs::read(&engine.paths.retained).unwrap(), retained);
    assert!(!engine.paths.journal.exists());
}

fn run_initialization_fixture(home: &Path) -> Result<()> {
    let generation = signed_generation(OLD, 1);
    let trust = provenance::IndependentTrustSource::from_path(&home.join("release-keys"))?;
    let mut receipt = Receipt {
        version: 2,
        hashes: [
            blake3::hash(OLD).to_hex().to_string(),
            blake3::hash(b"kernel fixture").to_hex().to_string(),
            blake3::hash(b"status fixture").to_hex().to_string(),
            String::new(),
            String::new(),
        ],
        previous: None,
        complete: false,
    };
    let mut engine = Engine::open(home)?;
    if let Some(bytes) =
        storage::read_private_record(&engine.paths.store.join("startup-state.json"))?
    {
        if startup::StartupStateV1::decode(&bytes)?
            .initializing_receipt
            .is_none()
        {
            receipt.complete = true;
            ensure!(
                engine.verify_complete_installation(&generation, &receipt)?,
                "completed initialization mismatch"
            );
            return Ok(());
        }
    }
    let guard = engine.prepare_initialization(&generation, &receipt, &trust)?;
    write_initialization_software(home);
    if guard.may_create_identity()? {
        std::fs::write(home.join(".ww/identity"), b"unchanged initializer identity")?;
        std::fs::set_permissions(
            home.join(".ww/identity"),
            std::fs::Permissions::from_mode(0o600),
        )?;
    }
    receipt.complete = true;
    guard.finish(&receipt)
}

#[test]
#[ignore = "subprocess worker; invoked by initialization crash matrix"]
fn initialization_owner_crash_worker() {
    let home = PathBuf::from(std::env::var_os("WW_INIT_OWNER_HOME").unwrap());
    let point: usize = std::env::var("WW_INIT_OWNER_POINT")
        .unwrap()
        .parse()
        .unwrap();
    storage::faults::exit_at(point);
    run_initialization_fixture(&home).unwrap();
    panic!("requested initialization owner crash point did not execute");
}

#[test]
fn initialization_process_exit_at_every_owner_boundary_resumes_exact_unbound_generation() {
    storage::faults::configure(None);
    let (baseline, _, _, _) = initialization_fixture();
    run_initialization_fixture(baseline.path()).unwrap();
    let trace = storage::faults::trace();
    assert!(trace.iter().any(|s| s == "proof.write.after"));
    for (index, label) in trace.iter().enumerate() {
        storage::faults::configure(None);
        let (home, generation, _, _) = initialization_fixture();
        let status = Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "upgrade::tests::initialization_owner_crash_worker",
                "--ignored",
                "--nocapture",
            ])
            .env("WW_INIT_OWNER_HOME", home.path())
            .env("WW_INIT_OWNER_POINT", (index + 1).to_string())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .unwrap();
        assert_eq!(status.code(), Some(86), "{} {label}", index + 1);
        storage::faults::configure(None);
        run_initialization_fixture(home.path())
            .unwrap_or_else(|e| panic!("{} {label}: {e:#}", index + 1));
        let engine = open(home.path());
        assert_eq!(
            engine.current_generation().unwrap().record(),
            generation.record()
        );
        assert_eq!(
            engine.startup_state().unwrap().disposition,
            startup::StartupDisposition::RequiredUnbound
        );
        assert!(engine.startup_contract().unwrap().is_none());
        assert_eq!(
            std::fs::read(home.path().join(".ww/identity")).unwrap(),
            b"unchanged initializer identity"
        );
        assert!(Receipt::load(&home.path().join(".ww")).unwrap().complete);
    }
    eprintln!(
        "initialization owner: {} real process-exit checkpoints",
        trace.len()
    );
}

#[test]
fn legacy_postreplacement_requires_exact_independent_evidence_and_stays_stopped_without_guard_authority(
) {
    storage::faults::configure(None);
    let home = fixture();
    let mut engine = open(home.path());
    let previous = signed_generation(OLD, 1);
    let candidate = signed_generation(NEW, 2);
    let receipt = Receipt::load(&home.path().join(".ww")).unwrap();
    storage::stage(
        Cursor::new(OLD),
        &engine.paths.artifact(previous.artifact()),
        previous.artifact(),
    )
    .unwrap();
    storage::stage(
        Cursor::new(NEW),
        &engine.paths.artifact(candidate.artifact()),
        candidate.artifact(),
    )
    .unwrap();
    std::fs::write(&engine.paths.executable, NEW).unwrap();
    let journal = Journal {
        version: 1,
        operation: Operation::Upgrade,
        phase: Phase::Installed,
        previous: previous.selection().clone().into(),
        candidate: candidate.selection().clone().into(),
        before: FakeService::absent().snapshot,
        admission: None,
        receipt_before: receipt.encode().unwrap(),
        receipt_after: Some(
            receipt
                .with_executable_digest(blake3::hash(NEW).to_hex().to_string())
                .encode()
                .unwrap(),
        ),
        retained_before: None,
    };
    storage::write_record(
        &engine.paths.journal,
        &serde_json::to_vec(&journal).unwrap(),
    )
    .unwrap();
    let mut service = FakeService::absent();
    assert!(engine
        .recover_before(&mut service, Instant::now() + TIMEOUT)
        .is_err());
    assert!(engine
        .recover_authenticated_without_preparation_until(
            &mut service,
            Instant::now() + TIMEOUT,
            &candidate
        )
        .is_err());
    assert!(matches!(
        engine
            .recover_authenticated_without_preparation_until(
                &mut service,
                Instant::now() + TIMEOUT,
                &previous
            )
            .unwrap(),
        Some(Outcome::Recovered {
            action: RecoveryAction::RestoredPrevious,
            live_ready: false,
            ..
        })
    ));
    assert_eq!(std::fs::read(&engine.paths.executable).unwrap(), OLD);
    assert_eq!(
        Receipt::load(&home.path().join(".ww"))
            .unwrap()
            .encode()
            .unwrap(),
        receipt.encode().unwrap()
    );
    assert!(!startup::metadata_present(home.path()).unwrap());
    assert!(!engine.paths.locator.exists());
    assert!(!engine.paths.journal.exists());
    assert_eq!(
        (service.stops, service.restarts, service.effects),
        (0, 0, 0)
    );
}

#[test]
fn native_v2_failed_readiness_and_wrong_binary_are_blocked_and_contained_with_bounded_cleanup() {
    for failure in ["unready", "wrong-binary", "timeout"] {
        storage::faults::configure(None);
        let (home, mut engine, current, inputs, mut service) = native_fixture(true);
        match failure {
            "unready" => service.not_ready = true,
            "wrong-binary" => service.wrong_running_artifact = true,
            _ => service.readiness_timeout = true,
        }
        assert!(engine
            .operator_upgrade(&mut service, TIMEOUT, || Ok(native_request(
                current,
                inputs.clone(),
                NEW,
                2
            )))
            .is_err());
        assert_eq!(service.snapshot.running, RunningState::Stopped, "{failure}");
        let lease = startup::StartupLease::shared(home.path()).unwrap();
        assert_eq!(
            lease.read_state().unwrap().unwrap().disposition,
            startup::StartupDisposition::Blocked
        );
        drop(lease);
        assert!(engine.paths.journal.exists());
        service.not_ready = false;
        service.wrong_running_artifact = false;
        service.readiness_timeout = false;
        engine
            .recover_prepared_until(&mut service, Instant::now() + TIMEOUT, &inputs)
            .unwrap();
        assert_eq!(std::fs::read(&engine.paths.executable).unwrap(), OLD);
        assert!(!engine.paths.journal.exists());
    }
}

#[test]
fn native_v2_expired_commit_budget_uses_fresh_recorded_containment_deadline() {
    storage::faults::configure(None);
    let (_baseline, mut owner, current, inputs, mut service) = native_fixture(true);
    storage::faults::configure(None);
    owner
        .operator_upgrade(&mut service, TIMEOUT, || {
            Ok(native_request(current, inputs, NEW, 2))
        })
        .unwrap();
    let trace = storage::faults::trace();
    let point = trace
        .iter()
        .position(|s| s == "native.phase.Verified")
        .unwrap()
        + 1;
    storage::faults::configure(None);
    let (home, mut engine, current, inputs, mut service) = native_fixture(true);
    let receipt_before = std::fs::read(&engine.paths.receipt).unwrap();
    storage::faults::advance_clock_at(point);
    let operation_deadline = Instant::now() + TIMEOUT;
    let error = engine
        .operator_upgrade_until(&mut service, operation_deadline, || {
            Ok(native_request(current, inputs.clone(), NEW, 2))
        })
        .unwrap_err();
    assert!(format!("{error:#}").contains("owned service is stopped"));
    assert_eq!(
        std::fs::read(&engine.paths.receipt).unwrap(),
        receipt_before
    );
    assert!(engine.paths.journal.exists());
    assert_eq!(service.snapshot.running, RunningState::Stopped);
    assert_containment_deadline(&engine, &service, operation_deadline);
    storage::faults::configure(None);
    let lease = startup::StartupLease::shared(home.path()).unwrap();
    assert_eq!(
        lease.read_state().unwrap().unwrap().disposition,
        startup::StartupDisposition::Blocked
    );
    drop(lease);
    engine
        .recover_prepared_until(&mut service, Instant::now() + TIMEOUT, &inputs)
        .unwrap();
    assert_eq!(std::fs::read(&engine.paths.executable).unwrap(), OLD);
}

#[test]
fn native_v2_manual_instance_blocks_replacement_until_explicit_exit() {
    storage::faults::configure(None);
    let (home, mut engine, current, inputs, mut service) = native_fixture(false);
    let process = startup::InstanceLease::shared(home.path()).unwrap();
    assert!(engine
        .operator_upgrade(&mut service, TIMEOUT, || Ok(native_request(
            current,
            inputs.clone(),
            NEW,
            2
        )))
        .is_err());
    assert_eq!(std::fs::read(&engine.paths.executable).unwrap(), OLD);
    assert!(engine.paths.journal.exists());
    assert!(engine
        .recover_prepared_until(&mut service, Instant::now() + TIMEOUT, &inputs)
        .is_err());
    drop(process);
    engine
        .recover_prepared_until(&mut service, Instant::now() + TIMEOUT, &inputs)
        .unwrap();
    assert_eq!(
        engine.current_generation().unwrap().record(),
        signed_generation(OLD, 1).record()
    );
    assert_eq!(
        (service.stops, service.restarts, service.effects),
        (0, 0, 0)
    );
}

#[test]
fn native_v2_cleanup_preserves_files_shared_with_a_rotated_keyring_generation() {
    storage::faults::configure(None);
    let (home, mut engine, original, inputs, mut service) = native_fixture(false);
    engine
        .operator_upgrade(&mut service, TIMEOUT, || {
            Ok(native_request(
                signed_generation(OLD, 1),
                inputs.clone(),
                NEW,
                2,
            ))
        })
        .unwrap();
    let extra = signed_generation_with_key(THIRD, 3, [8; 32]);
    let mut keys = original.record().verification_keys.clone();
    keys.extend_from_slice(&extra.record().verification_keys);
    std::fs::write(home.path().join("release-keys"), &keys).unwrap();
    let rotated = provenance::AuthenticatedGeneration::from_proof(
        crate::release::descriptor::SignedReleaseProof::decode(original.proof_bytes()).unwrap(),
        &keys,
        &original.record().target,
    )
    .unwrap();
    assert_ne!(original.record(), rotated.record());
    assert_eq!(original.record().proof, rotated.record().proof);
    let current = engine.current_generation().unwrap();
    let rotated_record = rotated.record().clone();
    engine
        .operator_upgrade(&mut service, TIMEOUT, || {
            Ok(UpgradeRequest {
                installed: current,
                candidate: rotated,
                bytes: Cursor::new(OLD),
                inputs: inputs.clone(),
            })
        })
        .unwrap();
    let proof = engine.paths.proof(&rotated_record.proof).unwrap();
    assert!(
        proof.is_file(),
        "cleanup removed the current generation's shared proof"
    );
    assert!(
        engine.paths.artifact(&rotated_record.artifact).is_file(),
        "cleanup removed the current generation's shared artifact"
    );
    assert_eq!(
        engine.current_generation().unwrap().record(),
        &rotated_record
    );
    engine
        .operator_rollback_prepared_until(&mut service, Instant::now() + TIMEOUT, &inputs)
        .unwrap();
    assert_eq!(std::fs::read(&engine.paths.executable).unwrap(), NEW);
}

#[test]
fn native_v2_early_abort_preserves_previous_artifact_shared_with_another_signed_root() {
    storage::faults::configure(None);
    let (_home, mut engine, original, inputs, mut service) = native_fixture(false);
    engine
        .operator_upgrade(&mut service, TIMEOUT, || {
            Ok(native_request(
                signed_generation(OLD, 1),
                inputs.clone(),
                NEW,
                2,
            ))
        })
        .unwrap();
    let current = engine.current_generation().unwrap();
    let other_root = signed_generation(OLD, 3);
    let other_proof = other_root.record().proof.clone();
    service.incompatible = Some(other_root.artifact().clone());
    assert!(engine
        .operator_upgrade(&mut service, TIMEOUT, || Ok(UpgradeRequest {
            installed: current,
            candidate: other_root,
            bytes: Cursor::new(OLD),
            inputs: inputs.clone(),
        }))
        .is_err());
    service.incompatible = None;
    engine
        .recover_without_preparation_until(&mut service, Instant::now() + TIMEOUT)
        .unwrap()
        .unwrap();
    assert!(
        engine.paths.artifact(original.artifact()).is_file(),
        "early abort removed the previous generation's shared artifact"
    );
    assert!(!engine.paths.proof(&other_proof).unwrap().exists());
    assert!(engine
        .paths
        .proof(&original.record().proof)
        .unwrap()
        .is_file());
    engine
        .operator_rollback_prepared_until(&mut service, Instant::now() + TIMEOUT, &inputs)
        .unwrap();
    assert_eq!(std::fs::read(&engine.paths.executable).unwrap(), OLD);
}

fn assert_containment_deadline(
    engine: &Engine,
    service: &FakeService,
    operation_deadline: Instant,
) {
    let journal: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&engine.paths.journal).unwrap()).unwrap();
    let budget = journal["cleanup_timeout_millis"]
        .as_u64()
        .expect("journal must record a bounded cleanup budget");
    assert!((1..=30_000).contains(&budget));
    let calls = &service.service_deadlines;
    let last = calls.len();
    assert_eq!(calls[last - 2].0, "stop");
    assert_eq!(calls[last - 1].0, "snapshot");
    let cleanup_deadline = calls[last - 2].1;
    assert_eq!(
        cleanup_deadline,
        calls[last - 1].1,
        "containment must pass one deadline to stop and reobserve"
    );
    assert!(
        cleanup_deadline > operation_deadline,
        "containment reused the expired operation deadline"
    );
    let remaining =
        cleanup_deadline.saturating_duration_since(storage::faults::clock_now(Instant::now()));
    assert!(!remaining.is_zero() && remaining <= Duration::from_millis(budget));
}

#[test]
fn native_v2_expired_recovery_budget_still_contains_the_restarted_service() {
    storage::faults::configure(None);
    let (home, mut engine, original, inputs, mut service) = native_fixture(true);
    service.not_ready = true;
    assert!(engine
        .operator_upgrade(&mut service, TIMEOUT, || {
            Ok(native_request(original, inputs.clone(), NEW, 2))
        })
        .is_err());
    service.not_ready = false;
    service.expire_after_guarded_restart = true;
    let operation_deadline = Instant::now() + TIMEOUT;
    let error = engine
        .recover_prepared_until(&mut service, operation_deadline, &inputs)
        .unwrap_err();
    assert!(
        format!("{error:#}").contains("owned service is stopped"),
        "{error:#}"
    );
    assert_eq!(service.snapshot.running, RunningState::Stopped);
    assert_containment_deadline(&engine, &service, operation_deadline);
    storage::faults::configure(None);
    let lease = startup::StartupLease::shared(home.path()).unwrap();
    assert_eq!(
        lease.read_state().unwrap().unwrap().disposition,
        startup::StartupDisposition::Blocked
    );
    drop(lease);
    service.expire_after_guarded_restart = false;
    engine
        .recover_prepared_until(&mut service, Instant::now() + TIMEOUT, &inputs)
        .unwrap();
    assert_eq!(std::fs::read(&engine.paths.executable).unwrap(), OLD);
}

#[test]
fn reinstall_refuses_trust_path_rebinding_before_writing_initialization_intent() {
    storage::faults::configure(None);
    let (home, mut engine, current) = completed_initialization();
    drop(
        removal::prepare(engine.mutation_lock(), false)
            .unwrap()
            .unwrap()
            .quiesce()
            .unwrap(),
    );
    let state = startup::StartupStateV1::decode(
        &std::fs::read(engine.paths.store.join("startup-state.json")).unwrap(),
    )
    .unwrap();
    let mut receipt = Receipt::load(&home.path().join(".ww")).unwrap();
    receipt.complete = false;
    storage::remove(&engine.paths.receipt).unwrap();
    let alternate = home.path().join("alternate-release-keys");
    std::fs::write(&alternate, &current.record().verification_keys).unwrap();
    let trust = provenance::IndependentTrustSource::from_path(&alternate).unwrap();
    let candidate = signed_generation(OLD, 2);
    let result = engine.prepare_initialization(&candidate, &receipt, &trust);
    assert!(
        result.is_err(),
        "reinstall changed the bound trust path without explicit admission"
    );
    drop(result);
    assert_eq!(
        startup::StartupStateV1::decode(
            &std::fs::read(engine.paths.store.join("startup-state.json")).unwrap()
        )
        .unwrap(),
        state
    );
    assert!(!engine.paths.receipt.exists());
    assert!(!engine
        .paths
        .proof(&candidate.record().proof)
        .unwrap()
        .exists());
}

#[test]
fn reinstall_allows_key_rotation_at_the_existing_bound_trust_path() {
    storage::faults::configure(None);
    let (home, mut engine, _) = completed_initialization();
    drop(
        removal::prepare(engine.mutation_lock(), false)
            .unwrap()
            .unwrap()
            .quiesce()
            .unwrap(),
    );
    let old_state = startup::StartupStateV1::decode(
        &std::fs::read(engine.paths.store.join("startup-state.json")).unwrap(),
    )
    .unwrap();
    let mut receipt = Receipt::load(&home.path().join(".ww")).unwrap();
    receipt.complete = false;
    receipt.hashes[0] = blake3::hash(NEW).to_hex().to_string();
    storage::remove(&engine.paths.receipt).unwrap();
    let candidate = signed_generation_with_key(NEW, 2, [8; 32]);
    std::fs::write(
        home.path().join("release-keys"),
        &candidate.record().verification_keys,
    )
    .unwrap();
    let trust =
        provenance::IndependentTrustSource::from_path(&home.path().join("release-keys")).unwrap();
    let guard = engine
        .prepare_initialization(&candidate, &receipt, &trust)
        .unwrap();
    assert!(!guard.may_create_identity().unwrap());
    drop(guard);
    let new_state = startup::StartupStateV1::decode(
        &std::fs::read(engine.paths.store.join("startup-state.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(
        new_state.trusted_release_keys,
        old_state.trusted_release_keys
    );
    assert_eq!(new_state.installation_id, old_state.installation_id);
    assert_eq!(new_state.native, *candidate.record());
}

#[test]
fn initialization_receipt_recalls_original_intent_after_terminal_receipt_write() {
    storage::faults::configure(None);
    let (home, generation, trust, mut intended) = initialization_fixture();
    intended.previous = Some([
        "old-0".into(),
        "old-1".into(),
        "old-2".into(),
        "old-3".into(),
        "old-4".into(),
    ]);
    let mut engine = open(home.path());
    assert!(engine.initialization_receipt().unwrap().is_none());
    drop(
        engine
            .prepare_initialization(&generation, &intended, &trust)
            .unwrap(),
    );
    let mut terminal = intended.clone();
    terminal.complete = true;
    terminal.previous = None;
    storage::write_record(&engine.paths.receipt, &terminal.encode().unwrap()).unwrap();
    // The read-only hint remains available before marker publication and does
    // not recreate the marker. The initializer still owns full validation.
    storage::remove(&home.path().join(".ww-startup.lock")).unwrap();
    assert_eq!(
        engine
            .initialization_receipt()
            .unwrap()
            .unwrap()
            .encode()
            .unwrap(),
        intended.encode().unwrap()
    );
    assert!(!home.path().join(".ww-startup.lock").exists());
    storage::write_record(&engine.paths.journal, b"{}").unwrap();
    assert!(engine.initialization_receipt().is_err());
}

#[test]
fn native_v2_recovery_refuses_missing_zero_or_unbounded_cleanup_budgets_before_effects() {
    for budget in [None, Some(0), Some(30_001)] {
        storage::faults::configure(None);
        let (_home, mut engine, original, inputs, mut service) = native_fixture(false);
        service.incompatible = Some(signed_generation(NEW, 2).artifact().clone());
        assert!(engine
            .operator_upgrade(&mut service, TIMEOUT, || {
                Ok(native_request(original, inputs, NEW, 2))
            })
            .is_err());
        let mut journal: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&engine.paths.journal).unwrap()).unwrap();
        match budget {
            Some(value) => journal["cleanup_timeout_millis"] = value.into(),
            None => {
                journal
                    .as_object_mut()
                    .unwrap()
                    .remove("cleanup_timeout_millis");
            }
        }
        let journal = serde_json::to_vec(&journal).unwrap();
        storage::write_record(&engine.paths.journal, &journal).unwrap();
        let state_before = std::fs::read(engine.paths.store.join("startup-state.json")).unwrap();
        service.service_deadlines.clear();
        assert!(
            engine
                .recover_without_preparation_until(&mut service, Instant::now() + TIMEOUT)
                .is_err(),
            "accepted cleanup budget {budget:?}"
        );
        assert!(
            service.service_deadlines.is_empty(),
            "invalid journal reached manager observation"
        );
        assert_eq!(std::fs::read(&engine.paths.journal).unwrap(), journal);
        assert_eq!(
            std::fs::read(engine.paths.store.join("startup-state.json")).unwrap(),
            state_before
        );
    }
}

#[test]
#[ignore = "subprocess worker; invoked by native containment contention regression"]
fn native_v2_capturing_child_worker() {
    let home = PathBuf::from(std::env::var_os("WW_NATIVE_CAPTURE_HOME").unwrap());
    let _instance = startup::InstanceLease::shared(&home).unwrap();
    let lease = startup::StartupLease::shared(&home).unwrap();
    let invocation = lease.read_contract().unwrap().unwrap().invocation;
    let permit = validate_transaction_start(&lease, &invocation, Instant::now() + TIMEOUT).unwrap();
    std::fs::write(
        home.join("capture-ready"),
        permit.payload().digest().unwrap(),
    )
    .unwrap();
    let until = Instant::now() + Duration::from_secs(15);
    let denied = loop {
        let journal: serde_json::Value =
            serde_json::from_slice(&std::fs::read(home.join(".ww/host-upgrade.json")).unwrap())
                .unwrap();
        if journal["phase"] == "containing" {
            assert!(
                validate_transaction_start(&lease, &invocation, Instant::now() + TIMEOUT).is_err()
            );
            // Keep the lock contended after denial becomes durable, so success
            // requires retrying exclusive acquisition instead of one attempt.
            std::thread::sleep(Duration::from_millis(80));
            break true;
        }
        if Instant::now() >= until {
            assert!(
                validate_transaction_start(&lease, &invocation, Instant::now() + TIMEOUT).is_ok()
            );
            break false;
        }
        std::thread::sleep(Duration::from_millis(5));
    };
    std::fs::write(
        home.join("capture-result"),
        if denied { "denied" } else { "still-runnable" },
    )
    .unwrap();
}

#[test]
fn native_v2_containment_revokes_capture_permit_before_retrying_shared_lease_contention() {
    let worker_directory = tempfile::tempdir().unwrap();
    let worker = worker_directory.path().join("capturing-worker");
    std::fs::copy(std::env::current_exe().unwrap(), &worker).unwrap();
    for restored in [false, true] {
        storage::faults::configure(None);
        let (home, mut engine, original, inputs, mut service) = native_fixture(true);
        if restored {
            service.not_ready = true;
            assert!(engine
                .operator_upgrade(&mut service, TIMEOUT, || {
                    Ok(native_request(
                        signed_generation(OLD, 1),
                        inputs.clone(),
                        NEW,
                        2,
                    ))
                })
                .is_err());
            service.not_ready = false;
        }
        service.capture_fixture = Some((home.path().to_owned(), worker.clone()));
        let operation_deadline = Instant::now() + Duration::from_secs(10);
        let error = if restored {
            engine.recover_prepared_until(&mut service, operation_deadline, &inputs)
        } else {
            engine.operator_upgrade_until(&mut service, operation_deadline, || {
                Ok(native_request(original, inputs.clone(), NEW, 2))
            })
        }
        .unwrap_err();
        assert!(
            service
                .capturing_child
                .as_mut()
                .unwrap()
                .wait()
                .unwrap()
                .success(),
            "{}",
            std::fs::read_to_string(home.path().join("capture-stderr")).unwrap()
        );
        assert_eq!(
            std::fs::read_to_string(home.path().join("capture-result")).unwrap(),
            "denied",
            "restored={restored}: {error:#}"
        );
        assert!(
            format!("{error:#}").contains("owned service is stopped"),
            "{error:#}"
        );
        assert_eq!(service.snapshot.running, RunningState::Stopped);
        assert_containment_deadline(&engine, &service, operation_deadline);
        let journal: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&engine.paths.journal).unwrap()).unwrap();
        assert_eq!(journal["phase"], "containing");
        let recorded: startup::StartupPermitPayload =
            serde_json::from_value(journal["disposition"]["quiescence_permit"].clone()).unwrap();
        assert_eq!(
            recorded.role,
            if restored {
                startup::PermitTarget::RestoredBefore
            } else {
                startup::PermitTarget::Candidate
            }
        );
        service.capture_fixture = None;
        service.expected_quiescence_permit = Some(recorded);
        engine
            .recover_prepared_until(&mut service, Instant::now() + TIMEOUT, &inputs)
            .unwrap();
        assert_eq!(std::fs::read(&engine.paths.executable).unwrap(), OLD);
    }
}

fn prepare_containment_crash(restored: bool) -> tempfile::TempDir {
    storage::faults::configure(None);
    let (home, engine, _, _, mut service) = native_fixture(true);
    service.persistent_state = Some(home.path().join("simulated-native-v2.json"));
    service.not_ready = true;
    service.save_state().unwrap();
    drop(engine);
    if restored {
        assert!(run_native_fixture_operation(home.path(), "upgrade").is_err());
    }
    home
}

#[test]
fn native_v2_process_exit_after_containment_intent_preserves_exact_recovery_target() {
    let worker_directory = tempfile::tempdir().unwrap();
    let worker = worker_directory.path().join("containment-worker");
    std::fs::copy(std::env::current_exe().unwrap(), &worker).unwrap();
    for restored in [false, true] {
        let action = if restored { "recover" } else { "upgrade" };
        let baseline = prepare_containment_crash(restored);
        storage::faults::configure(None);
        assert!(run_native_fixture_operation(baseline.path(), action).is_err());
        let point = storage::faults::trace()
            .iter()
            .position(|s| s == "native.phase.Containing")
            .expect("failure must durably revoke the runnable phase before startup exclusion")
            + 1;
        let home = prepare_containment_crash(restored);
        let status = Command::new(&worker)
            .args([
                "--exact",
                "upgrade::tests::native_v2_crash_worker",
                "--ignored",
                "--nocapture",
            ])
            .env("WW_NATIVE_V2_HOME", home.path())
            .env("WW_NATIVE_V2_POINT", point.to_string())
            .env("WW_NATIVE_V2_ACTION", action)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .unwrap();
        assert_eq!(status.code(), Some(86));
        storage::faults::configure(None);
        let mut engine = open(home.path());
        let journal: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&engine.paths.journal).unwrap()).unwrap();
        assert_eq!(journal["phase"], "containing");
        let target: startup::StartupPermitPayload =
            serde_json::from_value(journal["disposition"]["quiescence_permit"].clone()).unwrap();
        assert_eq!(
            target.role,
            if restored {
                startup::PermitTarget::RestoredBefore
            } else {
                startup::PermitTarget::Candidate
            }
        );
        let lease = startup::StartupLease::shared(home.path()).unwrap();
        assert!(
            matches!(
                lease.read_state().unwrap().unwrap().disposition,
                startup::StartupDisposition::Transaction { .. }
            ),
            "owner must exit before Blocked publication"
        );
        assert!(
            validate_transaction_start(&lease, &target.invocation, Instant::now() + TIMEOUT)
                .is_err()
        );
        drop(lease);
        let mut service: FakeService = serde_json::from_slice(
            &std::fs::read(home.path().join("simulated-native-v2.json")).unwrap(),
        )
        .unwrap();
        service.not_ready = false;
        service.expected_quiescence_permit = Some(target);
        engine
            .recover_prepared_until(
                &mut service,
                Instant::now() + TIMEOUT,
                &admission_inputs(home.path()),
            )
            .unwrap();
        assert_eq!(std::fs::read(&engine.paths.executable).unwrap(), OLD);
        assert_eq!(
            service.running_artifact.as_ref(),
            Some(signed_generation(OLD, 1).artifact())
        );
        assert!(!engine.paths.journal.exists());
    }
}

#[test]
fn native_v2_containment_exhaustion_leaves_a_durably_non_runnable_journal() {
    storage::faults::configure(None);
    let (home, mut engine, original, inputs, mut service) = native_fixture(true);
    service.interrupt_guarded_restart = true;
    let interruption = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        engine.operator_upgrade(&mut service, TIMEOUT, || {
            Ok(native_request(original, inputs.clone(), NEW, 2))
        })
    }));
    assert!(interruption.is_err());
    service.interrupt_guarded_restart = false;
    let _instance = startup::InstanceLease::shared(home.path()).unwrap();
    let lease = startup::StartupLease::shared(home.path()).unwrap();
    let invocation = lease.read_contract().unwrap().unwrap().invocation;
    let permit = validate_transaction_start(&lease, &invocation, Instant::now() + TIMEOUT).unwrap();
    let mut journal: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&engine.paths.journal).unwrap()).unwrap();
    journal["cleanup_timeout_millis"] = 30.into();
    storage::write_record(
        &engine.paths.journal,
        &serde_json::to_vec(&journal).unwrap(),
    )
    .unwrap();
    let started = Instant::now();
    let error = engine
        .recover_prepared_until(&mut service, Instant::now() + TIMEOUT, &inputs)
        .unwrap_err();
    assert!(
        started.elapsed() < Duration::from_secs(1),
        "contention exceeded the recorded cleanup bound"
    );
    assert!(format!("{error:#}").contains("containment is unconfirmed"));
    let after: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&engine.paths.journal).unwrap()).unwrap();
    assert_eq!(after["phase"], "containing");
    assert!(validate_transaction_start(&lease, &invocation, Instant::now() + TIMEOUT).is_err());
    assert_eq!(
        serde_json::from_value::<startup::StartupPermitPayload>(
            after["disposition"]["quiescence_permit"].clone()
        )
        .unwrap(),
        *permit.payload()
    );
    assert!(matches!(
        lease.read_state().unwrap().unwrap().disposition,
        startup::StartupDisposition::Transaction { .. }
    ));
}
