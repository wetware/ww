//! CLI and shell callers share the native upgrade exclusion boundary.
use std::path::Path;
use std::process::{Command, Output};
use ww::installation_state::MutationLock;

fn command(home: &Path) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_ww"));
    for (key, _) in std::env::vars_os() {
        if key.to_string_lossy().starts_with("WW_") || key.to_string_lossy().starts_with("IPFS_") {
            command.env_remove(key);
        }
    }
    command
        .env("HOME", home)
        .env("IPFS_API", "http://127.0.0.1:1");
    command
}

fn shell_uninstall(home: &Path, args: &[&str]) -> Output {
    Command::new("sh")
        .arg(concat!(env!("CARGO_MANIFEST_DIR"), "/scripts/uninstall.sh"))
        .args(args)
        .env("HOME", home)
        .output()
        .unwrap()
}

#[test]
fn shell_uninstaller_preserves_locked_or_pending_recovery_artifacts() {
    let home = tempfile::tempdir().unwrap();
    let state = home.path().join(".ww");
    std::fs::create_dir_all(state.join("bin")).unwrap();
    std::fs::copy(env!("CARGO_BIN_EXE_ww"), state.join("bin/ww")).unwrap();
    std::fs::write(state.join("retained-host"), b"recoverable executable").unwrap();
    let lock = MutationLock::acquire(home.path()).unwrap();
    let output = shell_uninstall(home.path(), &[]);
    assert!(!output.status.success(), "{output:?}");
    assert!(String::from_utf8_lossy(&output.stderr)
        .contains("installation mutation already in progress"));
    assert_eq!(
        std::fs::read(state.join("retained-host")).unwrap(),
        b"recoverable executable"
    );
    drop(lock);
    std::fs::write(state.join("host-upgrade.json"), b"pending").unwrap();
    let output = shell_uninstall(home.path(), &[]);
    assert!(!output.status.success(), "{output:?}");
    assert!(String::from_utf8_lossy(&output.stderr).contains("pending Host upgrade"));
    assert_eq!(
        std::fs::read(state.join("retained-host")).unwrap(),
        b"recoverable executable"
    );
}

#[test]
fn shell_uninstaller_never_purges_state_without_a_managed_host() {
    let home = tempfile::tempdir().unwrap();
    let state = home.path().join(".ww");
    std::fs::create_dir(&state).unwrap();
    std::fs::write(state.join("identity"), b"private identity").unwrap();
    std::fs::write(state.join("signed-state"), b"durable signed state").unwrap();
    let output = shell_uninstall(home.path(), &[]);
    assert!(!output.status.success(), "{output:?}");
    assert_eq!(
        std::fs::read(state.join("identity")).unwrap(),
        b"private identity"
    );
    assert_eq!(
        std::fs::read(state.join("signed-state")).unwrap(),
        b"durable signed state"
    );
    assert!(!home.path().join(".ww-installation.lock").exists());
}

#[test]
fn uninstall_has_no_data_purge_option() {
    let home = tempfile::tempdir().unwrap();
    let state = home.path().join(".ww");
    std::fs::create_dir(&state).unwrap();
    let identity = state.join("identity");
    std::fs::write(&identity, b"private identity").unwrap();
    let output = command(home.path())
        .args(["uninstall", "--purge"])
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(2), "{output:?}");
    assert_eq!(std::fs::read(identity).unwrap(), b"private identity");
    assert!(!home.path().join(".ww-installation.lock").exists());
}

#[cfg(unix)]
#[test]
fn shell_uninstaller_forwards_options_and_native_exit_status() {
    use std::os::unix::fs::PermissionsExt;
    let home = tempfile::tempdir().unwrap();
    let executable = home.path().join(".ww/bin/ww");
    std::fs::create_dir_all(executable.parent().unwrap()).unwrap();
    // Only the executable boundary is replaced. The real shell wrapper must
    // delegate the explicit operation and preserve both state and exit status.
    std::fs::write(&executable, b"#!/bin/sh\nprintf '%s\\n' \"$@\"\nexit 17\n").unwrap();
    std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o755)).unwrap();
    let identity = home.path().join(".ww/identity");
    std::fs::write(&identity, b"private identity").unwrap();
    let output = shell_uninstall(home.path(), &["--check", "--json"]);
    assert_eq!(output.status.code(), Some(17));
    assert_eq!(output.stdout, b"uninstall\n--check\n--json\n");
    assert_eq!(std::fs::read(identity).unwrap(), b"private identity");
    assert!(executable.is_file());
}

#[test]
fn mutators_refuse_a_concurrent_upgrade_before_side_effects() {
    let home = tempfile::tempdir().unwrap();
    let _lock = MutationLock::acquire(home.path()).unwrap();
    for args in [
        vec!["install", "--check"],
        vec!["install"],
        vec!["image", "update"],
        vec!["upgrade", "--check"],
        vec!["uninstall"],
        vec!["daemon", "install"],
        vec!["daemon", "uninstall"],
    ] {
        let output = command(home.path()).args(&args).output().unwrap();
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
fn mutators_refuse_pending_host_recovery_after_lock_release() {
    let home = tempfile::tempdir().unwrap();
    let state = home.path().join(".ww");
    std::fs::create_dir(&state).unwrap();
    std::fs::write(
        state.join("host-upgrade.json"),
        b"pending transaction sentinel",
    )
    .unwrap();
    for args in [
        vec!["install"],
        vec!["image", "update"],
        vec!["upgrade", "--check"],
        vec!["uninstall"],
        vec!["daemon", "install"],
        vec!["daemon", "uninstall"],
    ] {
        let output = command(home.path()).args(&args).output().unwrap();
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
