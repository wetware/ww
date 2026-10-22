//! `make package-check` builds the real P3 producers before running these tests.

use std::fs;
use std::path::Path;

use ww::packaging::{
    read_payload, stage_fhs, validate_payload, verify_fhs, KERNEL_PATH, STATUS_PATH,
};

fn payloads() -> (Vec<u8>, Vec<u8>) {
    (
        read_payload(Path::new("std/kernel/bin/main.wasm")).expect("run make std"),
        read_payload(Path::new("std/status/bin/status.wasm")).expect("run make std"),
    )
}

#[test]
fn missing_and_empty_payloads_fail() {
    let temporary = tempfile::tempdir().unwrap();
    let path = temporary.path().join("payload.wasm");
    assert!(read_payload(&path).is_err());
    fs::write(&path, []).unwrap();
    assert!(read_payload(&path).is_err());
    assert!(validate_payload(&[], b"status").is_err());
    assert!(validate_payload(b"kernel", &[]).is_err());
}

#[test]
#[ignore = "requires canonical P3 producers; run make package-check"]
fn canonical_payloads_stage_exact_bytes_and_reject_invalid_packages() {
    let (kernel, status) = payloads();
    let temporary = tempfile::tempdir().unwrap();
    let root = temporary.path().join("fhs");
    stage_fhs(&root, &kernel, &status).unwrap();
    assert_eq!(fs::read(root.join(KERNEL_PATH)).unwrap(), kernel);
    assert_eq!(fs::read(root.join(STATUS_PATH)).unwrap(), status);
    verify_fhs(&root, &kernel, &status).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            fs::metadata(root.join(KERNEL_PATH))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o644
        );
    }

    // A valid ordinary component is not necessarily a kernel. Conversely,
    // status cannot request the kernel-private readiness interface.
    assert!(validate_payload(&status, &status).is_err());
    assert!(validate_payload(&kernel, &kernel).is_err());
    assert!(validate_payload(b"corrupt kernel", &status).is_err());
    assert!(validate_payload(&kernel, b"corrupt status").is_err());

    fs::write(root.join(STATUS_PATH), b"stale").unwrap();
    assert!(verify_fhs(&root, &kernel, &status).is_err());
    fs::remove_file(root.join(STATUS_PATH)).unwrap();
    assert!(verify_fhs(&root, &kernel, &status).is_err());
    // Rerunning staging repairs an incomplete package.
    stage_fhs(&root, &kernel, &status).unwrap();
    fs::write(root.join("identity.key"), b"private sentinel").unwrap();
    assert!(verify_fhs(&root, &kernel, &status).is_err());
    assert!(stage_fhs(&root, &kernel, &status).is_err());
    assert_eq!(
        fs::read(root.join("identity.key")).unwrap(),
        b"private sentinel"
    );
}

#[cfg(unix)]
#[test]
#[ignore = "requires canonical P3 producers; run make package-check"]
fn package_symlinks_fail_without_mutating_their_targets() {
    use std::os::unix::fs::symlink;

    let (kernel, status) = payloads();
    let temporary = tempfile::tempdir().unwrap();
    let root = temporary.path().join("fhs");
    stage_fhs(&root, &kernel, &status).unwrap();
    let sentinel = temporary.path().join("private");
    fs::write(&sentinel, b"preserve").unwrap();
    fs::remove_file(root.join(KERNEL_PATH)).unwrap();
    symlink(&sentinel, root.join(KERNEL_PATH)).unwrap();
    assert!(verify_fhs(&root, &kernel, &status).is_err());
    assert!(stage_fhs(&root, &kernel, &status).is_err());
    assert_eq!(fs::read(sentinel).unwrap(), b"preserve");
}
