//! Real CLI callers must share the native upgrade exclusion boundary.
use std::process::Command;
use ww::installation_state::MutationLock;

fn shell_uninstall(home: &std::path::Path) -> std::process::Output {
    use std::io::Write;
    let mut child = Command::new("sh")
        .arg(concat!(env!("CARGO_MANIFEST_DIR"), "/scripts/uninstall.sh"))
        .env("HOME", home)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    child.stdin.take().unwrap().write_all(b"y\n").unwrap();
    child.wait_with_output().unwrap()
}

#[test]
fn shell_uninstaller_preserves_locked_or_pending_recovery_artifacts() {
    let home = tempfile::tempdir().unwrap();
    let state = home.path().join(".ww");
    std::fs::create_dir(&state).unwrap();
    std::fs::write(state.join("retained-host"), b"recoverable executable").unwrap();
    let lock = MutationLock::acquire(home.path()).unwrap();
    let output = shell_uninstall(home.path());
    assert!(!output.status.success(), "{output:?}");
    assert_eq!(
        std::fs::read(state.join("retained-host")).unwrap(),
        b"recoverable executable"
    );
    drop(lock);
    std::fs::write(state.join("host-upgrade.json"), b"pending").unwrap();
    let output = shell_uninstall(home.path());
    assert!(!output.status.success(), "{output:?}");
    assert_eq!(
        std::fs::read(state.join("retained-host")).unwrap(),
        b"recoverable executable"
    );
}

#[test]
fn shell_uninstaller_keeps_its_previous_removal_behavior_and_shared_lock_inode() {
    let home = tempfile::tempdir().unwrap();
    let state = home.path().join(".ww");
    std::fs::create_dir(&state).unwrap();
    std::fs::write(state.join("legacy-data"), b"existing legacy purge behavior").unwrap();
    let output = shell_uninstall(home.path());
    assert!(output.status.success(), "{output:?}");
    assert!(!state.exists());
    assert!(home.path().join(".ww-installation.lock").is_file());
    assert!(MutationLock::acquire(home.path()).is_ok());
}

#[test]
fn legacy_mutators_refuse_a_concurrent_upgrade_before_side_effects() {
    let home = tempfile::tempdir().unwrap();
    let _lock = MutationLock::acquire(home.path()).unwrap();
    for args in [
        vec!["perform", "install", "--check"],
        vec!["perform", "install"],
        vec!["perform", "update"],
        vec!["perform", "uninstall"],
        vec!["daemon", "install"],
        vec!["daemon", "uninstall"],
    ] {
        let output = Command::new(env!("CARGO_BIN_EXE_ww"))
            .args(&args)
            .env("HOME", home.path())
            .env("IPFS_API", "http://127.0.0.1:1")
            .env("IPFS_ROUTING_API", "http://127.0.0.1:1")
            .output()
            .unwrap();
        let error = String::from_utf8_lossy(&output.stderr);
        assert!(!output.status.success(), "{args:?}: {output:?}");
        assert!(
            error.contains("installation mutation already in progress"),
            "{args:?}: {error}"
        );
        assert!(
            !home.path().join(".ww").exists(),
            "{args:?} wrote installation state"
        );
    }
}

#[test]
fn legacy_mutators_refuse_pending_recovery_after_lock_release() {
    let home = tempfile::tempdir().unwrap();
    let state = home.path().join(".ww");
    std::fs::create_dir(&state).unwrap();
    std::fs::write(
        state.join("host-upgrade.json"),
        b"pending transaction sentinel",
    )
    .unwrap();
    for args in [
        ["perform", "install"],
        ["perform", "update"],
        ["perform", "uninstall"],
        ["daemon", "install"],
        ["daemon", "uninstall"],
    ] {
        let output = Command::new(env!("CARGO_BIN_EXE_ww"))
            .args(args)
            .env("HOME", home.path())
            .output()
            .unwrap();
        assert!(!output.status.success());
        assert!(
            String::from_utf8_lossy(&output.stderr).contains("pending Host upgrade"),
            "{args:?}: {output:?}"
        );
        assert_eq!(
            std::fs::read(state.join("host-upgrade.json")).unwrap(),
            b"pending transaction sentinel"
        );
    }
}
