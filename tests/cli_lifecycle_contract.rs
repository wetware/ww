//! CLI error output and identity creation are public automation contracts.
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::os::unix::process::CommandExt;
use std::process::Command;

fn ww(home: &std::path::Path) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_ww"));
    command.env("HOME", home).current_dir(home);
    command
}

#[test]
fn keygen_relative_filename_is_private_and_never_replaced() {
    let home = tempfile::tempdir().unwrap();
    std::fs::set_permissions(home.path(), std::fs::Permissions::from_mode(0o770)).unwrap();
    for relative in ["identity", "new-parent/nested/identity"] {
        let generate = || {
            let mut command = ww(home.path());
            command.args(["keygen", "--output", relative]);
            // SAFETY: umask is async-signal-safe and affects only this child.
            unsafe {
                command.pre_exec(|| {
                    libc::umask(0o002);
                    Ok(())
                });
            }
            command.output().unwrap()
        };
        let output = generate();
        assert!(output.status.success(), "{output:?}");
        let path = home.path().join(relative);
        let original = std::fs::read(&path).unwrap();
        assert_eq!(std::fs::metadata(&path).unwrap().mode() & 0o777, 0o600);
        for parent in path
            .parent()
            .unwrap()
            .ancestors()
            .take_while(|p| *p != home.path())
        {
            assert_eq!(std::fs::metadata(parent).unwrap().mode() & 0o777, 0o700);
        }
        let again = generate();
        assert!(!again.status.success());
        assert_eq!(std::fs::read(path).unwrap(), original);
        assert_eq!(
            std::fs::metadata(home.path()).unwrap().mode() & 0o777,
            0o770
        );
    }
}

#[test]
fn lifecycle_operational_failures_emit_one_json_result() {
    for args in [
        vec!["install"],
        vec!["image", "update"],
        vec!["uninstall"],
        vec!["daemon", "install"],
    ] {
        let home = tempfile::tempdir().unwrap();
        let output = ww(home.path()).args(&args).arg("--json").output().unwrap();
        assert!(!output.status.success(), "{args:?}: {output:?}");
        let result: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(result["version"], 1);
        assert_eq!(result["status"], "failed");
        assert!(result["error"].as_str().is_some_and(|s| !s.is_empty()));
        assert!(result["activation"].is_null());
        assert!(result["recovery"].is_object());
    }
}
