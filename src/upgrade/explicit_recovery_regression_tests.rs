//! Explicit authenticated recovery from a restarting candidate in a disposable manager.
use super::tests::signed_generation;
use super::*;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;

fn end() -> Instant {
    Instant::now() + Duration::from_secs(30)
}
fn home() -> Result<PathBuf> {
    let home = PathBuf::from(
        std::env::var_os("WW_CONTAINMENT_TEST_HOME").context("fixture home missing")?,
    );
    ensure!(
        home == Path::new("/home/probe739"),
        "disposable fixture home required"
    );
    Ok(home)
}
fn bytes(name: &str) -> Result<Vec<u8>> {
    Ok(std::fs::read(
        std::env::var_os(name).context("fixture executable missing")?,
    )?)
}
fn manager(args: &[&str]) -> Result<String> {
    let output = Command::new("systemctl")
        .arg("--user")
        .args(args)
        .output()?;
    ensure!(
        output.status.success(),
        "fixture systemctl failed: {:?}",
        output
    );
    Ok(String::from_utf8(output.stdout)?)
}
fn setup(home: &Path, old: &[u8]) -> Result<()> {
    manager(&["stop", "ww.service"])?;
    let ww = home.join(".ww");
    if ww.exists() {
        std::fs::remove_dir_all(&ww)?;
    }
    for sub in ["bin", "fhs/bin", "fhs/boot"] {
        std::fs::create_dir_all(ww.join(sub))?;
    }
    for (file, data) in [
        ("bin/ww", old),
        ("fhs/bin/status.wasm", b"status".as_slice()),
        ("fhs/boot/kernel.wasm", b"kernel".as_slice()),
    ] {
        std::fs::write(ww.join(file), data)?;
    }
    std::fs::set_permissions(ww.join("bin/ww"), std::fs::Permissions::from_mode(0o755))?;
    let receipt = Receipt {
        version: 2,
        hashes: [
            blake3::hash(old).to_hex().to_string(),
            blake3::hash(b"kernel").to_hex().to_string(),
            blake3::hash(b"status").to_hex().to_string(),
            String::new(),
            String::new(),
        ],
        previous: None,
        complete: true,
    };
    storage::write_record(&ww.join("installation.json"), &receipt.encode()?)?;
    UpgradeCoordinator::open(home)?
        .initialize_completed_installation(&signed_generation(old, 1))?;
    manager(&["reset-failed", "ww.service"])?;
    let mut service = service::NativeService::new(home);
    let before = service.snapshot(end())?;
    let lock = MutationLock::acquire(home)?;
    service
        .start_ready(&before, &lock, end())
        .context("fixture setup: start healthy previous executable")?;
    Ok(())
}

fn wait_for(mut predicate: impl FnMut() -> Result<bool>) -> Result<()> {
    let deadline = end();
    while !predicate()? {
        ensure!(Instant::now() < deadline, "fixture observation timed out");
        std::thread::sleep(Duration::from_millis(10));
    }
    Ok(())
}

#[test]
#[ignore = "worker for disposable explicit recovery test"]
fn disposable_upgrade_worker() -> Result<()> {
    let home = home()?;
    let old = bytes("WW_CONTAINMENT_OLD_ELF")?;
    let candidate = bytes("WW_RECOVERY_FAILING_ELF")?;
    let mut owner = UpgradeCoordinator::open(&home)?;
    let mut service = service::NativeService::new(&home);
    owner.operator_upgrade_until(&mut service, end(), || {
        Ok(UpgradeRequest {
            installed: signed_generation(&old, 1),
            candidate: signed_generation(&candidate, 2),
            bytes: candidate.as_slice(),
        })
    })?;
    anyhow::bail!("failing candidate unexpectedly committed")
}

#[test]
#[ignore = "requires disposable GNU ARM64 systemd and authenticated ELF fixtures"]
fn disposable_explicit_recovery_from_restarting_candidate() -> Result<()> {
    use std::os::unix::process::ExitStatusExt;
    let home = home()?;
    let old = bytes("WW_CONTAINMENT_OLD_ELF")?;
    let candidate = bytes("WW_RECOVERY_FAILING_ELF")?;
    let unit = home.join(".config/systemd/user/ww.service");
    let original = std::fs::read(&unit)?;
    let unrelated_before = manager(&[
        "show",
        "unrelated-running.service",
        "-p",
        "MainPID",
        "-p",
        "InvocationID",
    ])?;
    let failed_before = manager(&[
        "show",
        "unrelated-failed.service",
        "-p",
        "ActiveState",
        "-p",
        "Result",
    ])?;
    setup(&home, &old)?;
    let marker = home.join("candidate-starts");
    ensure!(!marker.exists(), "fixture candidate marker already exists");
    let mut child = Command::new(std::env::current_exe()?)
        .args([
            "--exact",
            "upgrade::explicit_recovery_regression_tests::disposable_upgrade_worker",
            "--ignored",
            "--nocapture",
        ])
        .spawn()?;
    // Freeze only the updater once the candidate executes. This makes its
    // later SIGKILL deterministic while systemd completes failures and retries.
    let result = (|| -> Result<()> {
        wait_for(|| {
            ensure!(
                child.try_wait()?.is_none(),
                "updater exited before candidate observation"
            );
            Ok(marker.exists())
        })?;
        ensure!(
            unsafe { libc::kill(child.id() as i32, libc::SIGSTOP) } == 0,
            "updater SIGSTOP failed"
        );
        wait_for(|| Ok(std::fs::read(&marker)?.len() >= 3))?;
        let restarts = manager(&["show", "ww.service", "-p", "NRestarts", "--value"])?;
        ensure!(
            restarts.trim().parse::<u64>()? >= 2,
            "candidate did not restart twice"
        );
        ensure!(
            unsafe { libc::kill(child.id() as i32, libc::SIGKILL) } == 0,
            "updater SIGKILL failed"
        );
        ensure!(
            child.wait()?.signal() == Some(libc::SIGKILL),
            "updater did not die by SIGKILL"
        );
        println!(
            "updater killed after candidate starts={} NRestarts={}",
            std::fs::read(&marker)?.len(),
            restarts.trim()
        );

        let mut owner = UpgradeCoordinator::open(&home)?;
        let journal_bytes = std::fs::read(&owner.paths.journal)?;
        let journal: serde_json::Value = serde_json::from_slice(&journal_bytes)?;
        ensure!(
            journal["work"]["transaction"]["phase"] == "Starting",
            "interruption did not retain Starting journal"
        );
        ensure!(
            std::fs::read(home.join(".ww/bin/ww"))? == candidate,
            "installed candidate bytes changed"
        );
        storage::verify(
            &owner.paths.executable,
            signed_generation(&candidate, 2).artifact(),
        )?;
        let rollback = owner.paths.artifact(signed_generation(&old, 1).artifact());
        ensure!(
            std::fs::read(&rollback)? == old,
            "exact rollback bytes not retained"
        );
        storage::verify(&rollback, signed_generation(&old, 1).artifact())?;
        ensure!(
            std::fs::read(&unit)? == original,
            "operator unit changed before recovery"
        );
        ensure!(
            manager(&["show", "ww.service", "-p", "Restart", "--value"])?.trim() == "on-failure",
            "restart policy changed"
        );
        wait_for(|| {
            Ok(
                manager(&["show", "ww.service", "-p", "SubState", "--value"])?.trim()
                    == "auto-restart",
            )
        })?;
        println!(
            "explicit recovery enters with SubState=auto-restart; journal bytes={}",
            journal_bytes.len()
        );
        owner.validate_recovery_executable(&signed_generation(&old, 1))?;
        let mut service = service::NativeService::new(&home);
        owner.recover_until(&mut service, end())?;
        ensure!(!owner.paths.journal.exists(), "recovery left journal");
        ensure!(
            std::fs::read(home.join(".ww/bin/ww"))? == old,
            "recovery did not restore exact previous bytes"
        );
        storage::verify(
            &owner.paths.executable,
            owner.current_generation()?.artifact(),
        )?;
        let current = service.snapshot(end())?;
        service.verify_ready(&current, end())?;
        let starts_after = std::fs::read(&marker)?;
        std::thread::sleep(Duration::from_secs(2));
        ensure!(
            std::fs::read(&marker)? == starts_after,
            "candidate executed after recovery"
        );
        ensure!(
            std::fs::read(&unit)? == original,
            "operator unit changed after recovery"
        );
        ensure!(
            manager(&[
                "show",
                "unrelated-running.service",
                "-p",
                "MainPID",
                "-p",
                "InvocationID"
            ])? == unrelated_before,
            "unrelated running unit changed"
        );
        ensure!(
            manager(&[
                "show",
                "unrelated-failed.service",
                "-p",
                "ActiveState",
                "-p",
                "Result"
            ])? == failed_before,
            "unrelated failed unit changed"
        );
        println!("PASS: exact rollback bytes, running executable identity, readiness, journal removal, operator configuration, unrelated running/failed units");
        Ok(())
    })();
    if child.try_wait()?.is_none() {
        child.kill()?;
        child.wait()?;
    }
    result
}
