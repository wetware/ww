use super::*;
use std::collections::VecDeque;

struct FakeManager {
    platform: Platform,
    home: PathBuf,
    enabled: EnabledState,
    running: RunningState,
    loaded: bool,
    fail: Option<&'static str>,
    calls: Vec<&'static str>,
    before_disable: Option<Box<dyn FnOnce()>>,
    snapshots: VecDeque<ServiceSnapshot>,
}
impl FakeManager {
    fn new(platform: Platform, home: &Path) -> Self {
        Self {
            platform,
            home: home.to_owned(),
            enabled: EnabledState::Disabled,
            running: RunningState::Stopped,
            loaded: false,
            fail: None,
            calls: vec![],
            before_disable: None,
            snapshots: VecDeque::new(),
        }
    }
    fn action(&mut self, name: &'static str) -> Result<()> {
        self.calls.push(name);
        ensure!(self.fail != Some(name), "injected {name} failure");
        Ok(())
    }
}
impl Manager for FakeManager {
    fn snapshot(&mut self) -> Result<ServiceSnapshot> {
        if let Some(snapshot) = self.snapshots.pop_front() {
            return Ok(snapshot);
        }
        Ok(ServiceSnapshot {
            registered: self.loaded,
            enabled: self.enabled.clone(),
            running: self.running.clone(),
            failed: false,
            executable: self.loaded.then(|| self.home.join(".ww/bin/ww")),
            configuration_identity: "fixture-service".into(),
        })
    }
    fn disable(&mut self, _: &MutationLock) -> Result<()> {
        if let Some(probe) = self.before_disable.take() {
            probe();
        }
        self.action("disable")?;
        self.enabled = EnabledState::Disabled;
        Ok(())
    }
    fn stop(&mut self, _: &MutationLock, _: bool) -> Result<()> {
        self.action("stop")?;
        self.running = RunningState::Stopped;
        if self.platform == Platform::Macos {
            self.loaded = false;
        }
        Ok(())
    }
    fn reload(&mut self, _: &MutationLock) -> Result<()> {
        self.action("reload")?;
        if self.platform == Platform::Linux {
            self.loaded = self.platform.path(&self.home).exists();
        }
        Ok(())
    }
    fn disabled(&mut self) -> Result<bool> {
        Ok(self.enabled == EnabledState::Disabled)
    }
}
fn definition(platform: Platform, home: &Path) -> Vec<u8> {
    render_service(
        platform,
        &home.join(".ww/bin/ww"),
        &[
            "run".into(),
            "--identity".into(),
            home.join(".ww/identity").display().to_string(),
        ],
        home,
    )
    .into_bytes()
}

#[test]
fn fresh_registration_is_disabled_before_plist_publication() {
    let home = tempfile::tempdir().unwrap();
    let lock = MutationLock::acquire(home.path()).unwrap();
    let mut manager = FakeManager::new(Platform::Macos, home.path());
    manager.enabled = EnabledState::Enabled; // stale persistent enable override
    let path = Platform::Macos.path(home.path());
    manager.before_disable = Some(Box::new(move || {
        assert!(!path.exists(), "plist exposed before disablement")
    }));
    let bytes = definition(Platform::Macos, home.path());
    assert!(install_locked(&lock, Platform::Macos, &bytes, false, &mut manager).unwrap());
    assert_eq!(manager.calls, ["disable", "reload"]);
    assert_eq!(
        std::fs::read(Platform::Macos.path(home.path())).unwrap(),
        bytes
    );
    assert_eq!(
        Registration::read(home.path()).unwrap().unwrap().phase,
        Phase::Registered
    );
    assert_eq!(manager.running, RunningState::Stopped);
    assert!(!home.path().join(".ww/identity").exists());
    assert!(!home.path().join(".ww/fhs").exists());
    assert!(!home.path().join(".ww/etc/ns/ww").exists());
}

#[test]
fn registration_creates_private_directories_under_group_writable_umask() {
    use std::os::unix::fs::PermissionsExt;
    use std::os::unix::process::CommandExt;

    const CHILD: &str = "WW_TEST_REGISTRATION_UMASK_CHILD";
    if std::env::var_os(CHILD).is_none() {
        let mut child = Command::new(std::env::current_exe().unwrap());
        child
            .args([
                "--exact",
                "daemon_cmd::tests::registration_creates_private_directories_under_group_writable_umask",
                "--nocapture",
            ])
            .env(CHILD, "1");
        // SAFETY: umask is async-signal-safe. Only the forked test process changes
        // its mask; other tests in this process keep their original policy.
        unsafe {
            child.pre_exec(|| {
                libc::umask(0o002);
                Ok(())
            });
        }
        let output = child.output().unwrap();
        assert!(
            output.status.success(),
            "{}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(String::from_utf8_lossy(&output.stdout).contains("1 passed"));
        return;
    }
    for platform in [Platform::Linux, Platform::Macos] {
        let home = tempfile::tempdir().unwrap();
        let lock = MutationLock::acquire(home.path()).unwrap();
        let mut manager = FakeManager::new(platform, home.path());
        let bytes = definition(platform, home.path());
        assert!(install_locked(&lock, platform, &bytes, false, &mut manager).unwrap());
        for entry in walkdir::WalkDir::new(home.path()).min_depth(1) {
            let entry = entry.unwrap();
            if entry.file_type().is_dir() {
                assert_eq!(
                    entry.metadata().unwrap().permissions().mode() & 0o022,
                    0,
                    "{} permits other-user writes",
                    entry.path().display()
                );
            }
        }
        assert!(!install_locked(&lock, platform, &bytes, false, &mut manager).unwrap());
    }
}

#[test]
fn failed_disablement_does_not_publish_launchd_plist() {
    let home = tempfile::tempdir().unwrap();
    let lock = MutationLock::acquire(home.path()).unwrap();
    let mut manager = FakeManager::new(Platform::Macos, home.path());
    manager.fail = Some("disable");
    assert!(install_locked(
        &lock,
        Platform::Macos,
        &definition(Platform::Macos, home.path()),
        false,
        &mut manager
    )
    .is_err());
    assert!(!Platform::Macos.path(home.path()).exists());
    assert_eq!(
        Registration::read(home.path()).unwrap().unwrap().phase,
        Phase::Installing
    );
}

#[test]
fn repeat_registration_preserves_running_and_enabled_state() {
    for platform in [Platform::Linux, Platform::Macos] {
        let home = tempfile::tempdir().unwrap();
        let lock = MutationLock::acquire(home.path()).unwrap();
        let mut manager = FakeManager::new(platform, home.path());
        let bytes = definition(platform, home.path());
        install_locked(&lock, platform, &bytes, false, &mut manager).unwrap();
        manager.calls.clear();
        manager.loaded = true;
        manager.enabled = EnabledState::Enabled;
        manager.running = RunningState::Running { pid: 123 };
        assert!(!install_locked(&lock, platform, &bytes, false, &mut manager).unwrap());
        assert!(manager.calls.is_empty());
        assert_eq!(manager.running, RunningState::Running { pid: 123 });
        assert_eq!(manager.enabled, EnabledState::Enabled);
    }
}

#[test]
fn different_owned_and_unowned_definitions_are_preserved() {
    for owned_definition in [false, true] {
        let home = tempfile::tempdir().unwrap();
        let lock = MutationLock::acquire(home.path()).unwrap();
        let platform = Platform::Linux;
        let mut manager = FakeManager::new(platform, home.path());
        let original = definition(platform, home.path());
        if owned_definition {
            install_locked(&lock, platform, &original, false, &mut manager).unwrap();
        } else {
            write_file(home.path(), &platform.path(home.path()), &original, true).unwrap();
        }
        manager.calls.clear();
        assert!(install_locked(&lock, platform, b"changed defaults", false, &mut manager).is_err());
        assert_eq!(std::fs::read(platform.path(home.path())).unwrap(), original);
        assert!(manager.calls.is_empty());
    }
}

#[test]
fn registration_check_and_loaded_collision_make_no_writes() {
    for loaded in [false, true] {
        let home = tempfile::tempdir().unwrap();
        let lock = MutationLock::acquire(home.path()).unwrap();
        let mut manager = FakeManager::new(Platform::Macos, home.path());
        manager.loaded = loaded;
        let result = install_locked(
            &lock,
            Platform::Macos,
            &definition(Platform::Macos, home.path()),
            true,
            &mut manager,
        );
        assert_eq!(result.is_err(), loaded);
        assert!(!home.path().join(".ww").exists());
        assert!(!Platform::Macos.path(home.path()).exists());
        assert!(manager.calls.is_empty());
    }
}

#[test]
fn removal_checks_stop_and_preserves_registration_on_failure() {
    for platform in [Platform::Linux, Platform::Macos] {
        for failure in ["stop", "disable"] {
            let home = tempfile::tempdir().unwrap();
            let lock = MutationLock::acquire(home.path()).unwrap();
            let mut manager = FakeManager::new(platform, home.path());
            let bytes = definition(platform, home.path());
            install_locked(&lock, platform, &bytes, false, &mut manager).unwrap();
            manager.loaded = true;
            manager.running = RunningState::Running { pid: 123 };
            manager.enabled = EnabledState::Enabled;
            manager.fail = Some(failure);
            manager.calls.clear();
            let plan = prepare_removal(&lock, platform, &mut manager).unwrap();
            assert!(execute_removal(&lock, plan, false, &mut manager).is_err());
            assert_eq!(std::fs::read(platform.path(home.path())).unwrap(), bytes);
            let backup = home
                .path()
                .join(".ww/daemon-backups")
                .join(format!("{}.service", digest(&bytes)));
            assert_eq!(std::fs::read(backup).unwrap(), bytes);
            assert_eq!(
                Registration::read(home.path()).unwrap().unwrap().phase,
                Phase::Removing
            );
        }
    }
}

#[test]
fn successful_removal_retains_exact_backup_and_private_state() {
    for platform in [Platform::Linux, Platform::Macos] {
        let home = tempfile::tempdir().unwrap();
        let lock = MutationLock::acquire(home.path()).unwrap();
        let mut manager = FakeManager::new(platform, home.path());
        let bytes = definition(platform, home.path());
        install_locked(&lock, platform, &bytes, false, &mut manager).unwrap();
        let identity = home.path().join(".ww/identity");
        std::fs::write(&identity, b"keep identity").unwrap();
        manager.loaded = true;
        manager.running = RunningState::Running { pid: 123 };
        manager.enabled = EnabledState::Enabled;
        manager.calls.clear();
        let plan = prepare_removal(&lock, platform, &mut manager).unwrap();
        let report = execute_removal(&lock, plan, false, &mut manager).unwrap();
        assert!(report.changed && !report.running && !report.enabled && !report.registered);
        assert!(!platform.path(home.path()).exists());
        assert_eq!(std::fs::read(report.backup.unwrap()).unwrap(), bytes);
        assert_eq!(std::fs::read(identity).unwrap(), b"keep identity");
        assert_eq!(manager.calls, ["stop", "disable", "reload"]);
        assert_eq!(
            Registration::read(home.path()).unwrap().unwrap().phase,
            Phase::Removed
        );
    }
}

#[test]
fn removal_preflight_refuses_custom_service_and_dropins() {
    let home = tempfile::tempdir().unwrap();
    let lock = MutationLock::acquire(home.path()).unwrap();
    let platform = Platform::Linux;
    let mut manager = FakeManager::new(platform, home.path());
    write_file(
        home.path(),
        &platform.path(home.path()),
        b"custom unit",
        true,
    )
    .unwrap();
    assert!(prepare_removal(&lock, platform, &mut manager).is_err());
    assert_eq!(
        std::fs::read(platform.path(home.path())).unwrap(),
        b"custom unit"
    );
    let overrides = home.path().join(".config/systemd/user/ww.service.d");
    std::fs::create_dir(&overrides).unwrap();
    assert!(install_locked(&lock, platform, b"unit", false, &mut manager).is_err());
    assert!(manager.calls.is_empty());
}

#[test]
fn legacy_receipt_can_own_service_without_changing_other_slots() {
    let home = tempfile::tempdir().unwrap();
    let lock = MutationLock::acquire(home.path()).unwrap();
    let platform = Platform::Linux;
    let mut manager = FakeManager::new(platform, home.path());
    let bytes = definition(platform, home.path());
    write_file(home.path(), &platform.path(home.path()), &bytes, true).unwrap();
    let receipt = Receipt {
        version: 1,
        hashes: [
            "native".into(),
            "kernel".into(),
            "status".into(),
            "namespace".into(),
            digest(&bytes),
        ],
        previous: None,
        complete: true,
    };
    let receipt_bytes = receipt.encode().unwrap();
    write_file(
        home.path(),
        &home.path().join(".ww/installation.json"),
        &receipt_bytes,
        true,
    )
    .unwrap();
    let plan = prepare_removal(&lock, platform, &mut manager).unwrap();
    execute_removal(&lock, plan, false, &mut manager).unwrap();
    assert_eq!(
        std::fs::read(home.path().join(".ww/installation.json")).unwrap(),
        receipt_bytes
    );
}

#[test]
fn symlink_service_and_receipt_do_not_modify_target() {
    use std::os::unix::fs::symlink;
    for target_path in [".ww/daemon.json", ".config/systemd/user/ww.service"] {
        let home = tempfile::tempdir().unwrap();
        let lock = MutationLock::acquire(home.path()).unwrap();
        let sentinel = home.path().join("private");
        std::fs::write(&sentinel, b"keep").unwrap();
        let link = home.path().join(target_path);
        std::fs::create_dir_all(link.parent().unwrap()).unwrap();
        symlink(&sentinel, &link).unwrap();
        let mut manager = FakeManager::new(Platform::Linux, home.path());
        assert!(install_locked(&lock, Platform::Linux, b"new", false, &mut manager).is_err());
        assert_eq!(std::fs::read(sentinel).unwrap(), b"keep");
        assert!(manager.calls.is_empty());
    }
}

#[test]
fn renderers_keep_argument_boundaries_and_launchd_disabled_property() {
    let args = vec![
        "run".into(),
        "--identity".into(),
        "/home/a b&c/identity".into(),
        "--ipfs-url".into(),
        "http://127.0.0.1:45001/?x=%20&y=$v".into(),
    ];
    let plist = render_service(
        Platform::Macos,
        Path::new("/home/a b&c/ww"),
        &args,
        Path::new("/home/a b&c"),
    );
    assert!(plist.contains("<key>Disabled</key>\n    <true/>"));
    assert!(plist.contains("<string>/home/a b&amp;c/ww</string>"));
    assert!(plist.contains("<string>http://127.0.0.1:45001/?x=%20&amp;y=$v</string>"));
    let unit = render_service(
        Platform::Linux,
        Path::new("/home/a b%$/ww"),
        &args,
        Path::new("/home/a b"),
    );
    assert!(unit.contains("ExecStart=\"/home/a b%%$$/ww\" run --identity \"/home/a b&c/identity\""));
    assert!(unit.contains("--ipfs-url \"http://127.0.0.1:45001/?x=%%20&y=$$v\""));
}

#[test]
fn interrupted_linux_registration_resumes_without_starting_service() {
    let home = tempfile::tempdir().unwrap();
    let lock = MutationLock::acquire(home.path()).unwrap();
    let platform = Platform::Linux;
    let bytes = definition(platform, home.path());
    let mut manager = FakeManager::new(platform, home.path());
    manager.fail = Some("reload");
    assert!(install_locked(&lock, platform, &bytes, false, &mut manager).is_err());
    assert_eq!(
        Registration::read(home.path()).unwrap().unwrap().phase,
        Phase::Installing
    );
    manager.fail = None;
    manager.loaded = true;
    manager.calls.clear();
    assert!(install_locked(&lock, platform, &bytes, false, &mut manager).unwrap());
    assert_eq!(manager.calls, ["reload"]);
    assert_eq!(manager.running, RunningState::Stopped);
    assert_eq!(
        Registration::read(home.path()).unwrap().unwrap().phase,
        Phase::Registered
    );
}

#[test]
fn service_argv_preserves_custom_run_options_and_absolute_sources() {
    use clap::Parser;
    let home = tempfile::tempdir().unwrap();
    let identity = home.path().join("custom identity");
    ww::keys::save(&ww::keys::generate().unwrap(), &identity).unwrap();
    let fhs = home.path().join("fhs");
    let image = home.path().join("image");
    std::fs::create_dir(&fhs).unwrap();
    std::fs::create_dir(&image).unwrap();
    let cli = super::super::Cli::try_parse_from([
        "ww",
        "daemon",
        "install",
        "--identity",
        identity.to_str().unwrap(),
        "--fhs-root",
        fhs.to_str().unwrap(),
        "--listen",
        "/ip4/127.0.0.1/tcp/23025",
        "--http-listen",
        "127.0.0.1:2089",
        "--http-dial",
        "example.com",
        "--with-http-admin",
        "127.0.0.1:2040",
        "--ipfs-url",
        "http://127.0.0.1:5009",
        "--ipns-routing-url",
        "http://127.0.0.1:8089",
        "--runtime-cache-policy",
        "isolated",
        "--executor-threads",
        "3",
        "--wasm-debug",
        image.to_str().unwrap(),
    ])
    .unwrap();
    let super::super::Commands::Daemon {
        action: super::super::DaemonAction::Install { run, .. },
    } = cli.command
    else {
        panic!("wrong parsed command")
    };
    let args = run.service_argv(home.path()).unwrap();
    for pair in [
        ["--identity", identity.to_str().unwrap()],
        ["--fhs-root", fhs.to_str().unwrap()],
        ["--listen", "/ip4/127.0.0.1/tcp/23025"],
        ["--http-listen", "127.0.0.1:2089"],
        ["--http-dial", "example.com"],
        ["--with-http-admin", "127.0.0.1:2040"],
        ["--ipfs-url", "http://127.0.0.1:5009"],
        ["--ipns-routing-url", "http://127.0.0.1:8089"],
        ["--executor-threads", "3"],
        ["--runtime-cache-policy", "isolated"],
    ] {
        assert!(
            args.windows(2).any(|window| window == pair),
            "missing {pair:?}: {args:?}"
        );
    }
    assert!(args.contains(&"--wasm-debug".to_owned()));
    assert_eq!(args.last().unwrap(), image.to_str().unwrap());
    let metadata = std::fs::read(&identity).unwrap();
    assert!(!metadata.is_empty());
    assert!(!home.path().join(".ww").exists());
}

#[test]
fn service_argv_missing_identity_refuses_without_creating_private_state() {
    use clap::Parser;
    let home = tempfile::tempdir().unwrap();
    let cli = super::super::Cli::try_parse_from(["ww", "daemon", "install"]).unwrap();
    let super::super::Commands::Daemon {
        action: super::super::DaemonAction::Install { run, .. },
    } = cli.command
    else {
        panic!("wrong parsed command")
    };
    let error = run.service_argv(home.path()).unwrap_err();
    assert!(error.to_string().contains("existing identity"));
    assert!(!home.path().join(".ww").exists());
}

#[test]
fn fresh_linux_registration_refuses_stale_enablement_before_writes() {
    let home = tempfile::tempdir().unwrap();
    let lock = MutationLock::acquire(home.path()).unwrap();
    let mut manager = FakeManager::new(Platform::Linux, home.path());
    manager.enabled = EnabledState::Enabled;
    assert!(install_locked(
        &lock,
        Platform::Linux,
        &definition(Platform::Linux, home.path()),
        false,
        &mut manager
    )
    .is_err());
    assert!(!home.path().join(".ww").exists());
    assert!(!Platform::Linux.path(home.path()).exists());
    assert!(manager.calls.is_empty());
}

#[test]
fn service_argv_freezes_relative_mount_and_preserves_namespace_without_default_http() {
    use clap::Parser;
    let home = tempfile::tempdir().unwrap();
    let cwd = std::env::current_dir().unwrap();
    let mount = tempfile::tempdir_in(&cwd).unwrap();
    let relative = format!("./{}", mount.path().file_name().unwrap().to_str().unwrap());
    let identity = home.path().join("identity");
    ww::keys::save(&ww::keys::generate().unwrap(), &identity).unwrap();
    let fhs = home.path().join("fhs");
    std::fs::create_dir(&fhs).unwrap();
    let namespace = home.path().join("config");
    std::fs::create_dir_all(namespace.join("etc/ns")).unwrap();
    let cli = super::super::Cli::try_parse_from([
        "ww",
        "daemon",
        "install",
        "--identity",
        identity.to_str().unwrap(),
        "--fhs-root",
        fhs.to_str().unwrap(),
        "--namespace-root",
        namespace.to_str().unwrap(),
        &relative,
    ])
    .unwrap();
    let super::super::Commands::Daemon {
        action: super::super::DaemonAction::Install { run, .. },
    } = cli.command
    else {
        panic!("wrong command")
    };
    let args = run.service_argv(home.path()).unwrap();
    assert!(args
        .windows(2)
        .any(|pair| pair == ["--namespace-root", namespace.to_str().unwrap()]));
    assert!(!args.contains(&"--http-listen".to_owned()));
    assert_eq!(args.last().unwrap(), mount.path().to_str().unwrap());
    for platform in [Platform::Linux, Platform::Macos] {
        let rendered = render_service(platform, &home.path().join("ww"), &args, home.path());
        assert!(rendered.contains("--namespace-root"));
        assert!(!rendered.contains("--http-listen"));
        assert!(rendered.contains("--ipfs-url"));
        assert!(rendered.contains("--ipns-routing-url"));
    }
}

#[test]
fn removal_recovers_after_unlink_and_reload_failure_then_allows_reconfiguration() {
    for platform in [Platform::Linux, Platform::Macos] {
        let home = tempfile::tempdir().unwrap();
        let lock = MutationLock::acquire(home.path()).unwrap();
        let mut manager = FakeManager::new(platform, home.path());
        let bytes = definition(platform, home.path());
        install_locked(&lock, platform, &bytes, false, &mut manager).unwrap();
        manager.fail = Some("reload");
        let plan = prepare_removal(&lock, platform, &mut manager).unwrap();
        assert!(execute_removal(&lock, plan, false, &mut manager).is_err());
        assert!(!platform.path(home.path()).exists());
        assert_eq!(
            Registration::read(home.path()).unwrap().unwrap().phase,
            Phase::Removing
        );
        manager.fail = None;
        let plan = prepare_removal(&lock, platform, &mut manager).unwrap();
        let report = execute_removal(&lock, plan, false, &mut manager).unwrap();
        assert!(report.changed);
        assert_eq!(std::fs::read(report.backup.unwrap()).unwrap(), bytes);
        assert_eq!(
            Registration::read(home.path()).unwrap().unwrap().phase,
            Phase::Removed
        );
        let new_definition = [bytes.as_slice(), b"\n"].concat();
        assert!(install_locked(&lock, platform, &new_definition, false, &mut manager).unwrap());
    }
}
