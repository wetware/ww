//! Abrupt process exits in the real guarded-removal owner, without unwinding.

use super::*;
use crate::upgrade::tests::completed_initialization;
use std::os::unix::fs::MetadataExt;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

fn remove_owner(home: &Path) {
    let mutation = MutationLock::acquire(home).unwrap();
    let removal = prepare(&mutation, false).unwrap().unwrap();
    let _quiescent = removal.quiesce().unwrap();
}

#[test]
#[ignore = "subprocess worker; invoked by guarded removal crash matrix"]
fn removal_crash_worker() {
    let home = std::env::var_os("WW_REMOVAL_CRASH_HOME").unwrap();
    let point = std::env::var("WW_REMOVAL_CRASH_POINT")
        .unwrap()
        .parse()
        .unwrap();
    storage::faults::exit_at(point);
    remove_owner(Path::new(&home));
    panic!("requested guarded removal crash point did not execute");
}

#[test]
fn guarded_removal_process_exit_at_every_state_boundary_releases_leases_and_retries() {
    // Removing either durable Blocked/Uninstalled transition, replacing a stable
    // lease inode, or deleting retained authority/private state breaks this test.
    storage::faults::configure(None);
    let (baseline, engine, _) = completed_initialization();
    drop(engine);
    storage::faults::configure(None);
    remove_owner(baseline.path());
    let trace = storage::faults::trace();
    assert!(trace.iter().any(|label| label == "record.sync.after"));
    assert_eq!(
        trace
            .iter()
            .filter(|label| *label == "rename.after")
            .count(),
        2
    );

    // Another Cargo invocation must not replace the executable of a worker.
    let executable_dir = tempfile::tempdir().unwrap();
    let worker = executable_dir.path().join("removal-worker");
    std::fs::copy(std::env::current_exe().unwrap(), &worker).unwrap();
    for (index, label) in trace.iter().enumerate() {
        storage::faults::configure(None);
        let (home, engine, generation) = completed_initialization();
        let state_before = engine.startup_state().unwrap();
        let proof = engine.paths.proof(&generation.record().proof).unwrap();
        let stable = [
            home.path().join(".ww-startup.lock"),
            home.path().join(".ww-host-instance.lock"),
        ];
        let stable_inodes = stable
            .each_ref()
            .map(|path| std::fs::metadata(path).unwrap().ino());
        let preserved = [
            home.path().join(".ww/identity"),
            home.path().join(".ww-startup.lock"),
            home.path().join(".ww-host-instance.lock"),
            home.path().join(".ww/fhs/boot/kernel.wasm"),
            home.path().join(".ww/fhs/bin/status.wasm"),
            engine.paths.executable.clone(),
            engine.paths.receipt.clone(),
            engine.paths.locator.clone(),
            engine.paths.retained.clone(),
            proof,
        ]
        .map(|path| {
            let bytes = std::fs::read(&path).unwrap();
            (path, bytes)
        });
        drop(engine);
        let stderr_path = home.path().join("worker-stderr");
        let mut child = Command::new(&worker)
            .args([
                "--exact",
                "upgrade::removal::crash_tests::removal_crash_worker",
                "--ignored",
                "--nocapture",
            ])
            .env("WW_REMOVAL_CRASH_HOME", home.path())
            .env("WW_REMOVAL_CRASH_POINT", (index + 1).to_string())
            .stdout(Stdio::null())
            .stderr(std::fs::File::create(&stderr_path).unwrap())
            .spawn()
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(15);
        let status = loop {
            if let Some(status) = child.try_wait().unwrap() {
                break status;
            }
            if Instant::now() >= deadline {
                let _ = child.kill();
                child.wait().unwrap();
                panic!("removal worker timed out at {} {label}", index + 1);
            }
            std::thread::sleep(Duration::from_millis(5));
        };
        assert_eq!(
            status.code(),
            Some(86),
            "{} {label}: {}",
            index + 1,
            std::fs::read_to_string(&stderr_path).unwrap()
        );
        storage::faults::configure(None);
        let lease = StartupLease::shared(home.path()).unwrap();
        let after_exit = lease.read_state().unwrap().unwrap();
        assert_eq!(after_exit.native, state_before.native);
        assert_eq!(after_exit.installation_id, state_before.installation_id);
        assert!(matches!(
            after_exit.disposition,
            StartupDisposition::RequiredUnbound
                | StartupDisposition::Blocked
                | StartupDisposition::Uninstalled
        ));
        // Once the blocked rename occurred, process death cannot make the old
        // state current again, even before the directory sync completes.
        if trace[..=index].iter().any(|label| label == "rename.after") {
            assert_ne!(after_exit.disposition, StartupDisposition::RequiredUnbound);
        }
        drop(lease);
        // Abrupt exit must release all three kernel locks. The real owner then
        // accepts the recorded intermediate state and completes the tombstone.
        remove_owner(home.path());
        let lease = StartupLease::shared(home.path()).unwrap();
        assert_eq!(
            lease.read_state().unwrap().unwrap().disposition,
            StartupDisposition::Uninstalled
        );
        drop(lease);
        let instance = InstanceLease::exclusive(home.path()).unwrap();
        drop(instance);
        for (path, inode) in stable.iter().zip(stable_inodes) {
            assert_eq!(std::fs::metadata(path).unwrap().ino(), inode);
        }
        for (path, bytes) in preserved {
            assert_eq!(
                std::fs::read(&path).unwrap(),
                bytes,
                "{} {label}: {}",
                index + 1,
                path.display()
            );
        }
        // Repeating a completed removal owner must retain the same authority.
        remove_owner(home.path());
    }
    eprintln!(
        "guarded removal: {} real process-exit state checkpoints",
        trace.len()
    );
}
