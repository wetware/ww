use std::process::Command;

#[test]
fn unsuppressed_repeated_warning_is_non_blocking_unless_denied() {
    let temp = tempfile::tempdir().expect("temporary fixture directory");
    let fixture = temp.path().join("repeated-runtime.glia");
    std::fs::write(
        &fixture,
        ";; grant-lint: allow WWG101 runtime -- trusted first loader\n\
         (cell first :grants {:runtime runtime})\n\
         (cell second :grants {:runtime runtime})",
    )
    .expect("write lint fixture");

    let normal = Command::new(env!("CARGO_BIN_EXE_grant-lint"))
        .arg(&fixture)
        .status()
        .expect("run grant-lint normally");
    assert!(
        normal.success(),
        "warnings should be non-blocking by default"
    );

    let denied = Command::new(env!("CARGO_BIN_EXE_grant-lint"))
        .arg("--deny-warnings")
        .arg(&fixture)
        .status()
        .expect("run grant-lint with denied warnings");
    assert_eq!(
        denied.code(),
        Some(1),
        "the unsuppressed second warning must remain blocking"
    );
}
