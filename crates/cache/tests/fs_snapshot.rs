use std::future::Future;
use std::io;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Barrier};
use std::task::Poll;
use std::time::{Duration, Instant};

use ww_cache::{
    AdmissionKind, FilesystemOperation, FsCleanupState, FsCompletion, FsFinalizer, FsOwner,
    FsScope, FsSnapshot, FsSnapshotError, HostFsOwner, HostLimits, HostResources,
};

async fn deadline<F: Future>(future: F) -> F::Output {
    tokio::time::timeout(Duration::from_secs(5), future)
        .await
        .expect("filesystem snapshot test exceeded its deadline")
}

async fn snapshot(completion: &FsCompletion) -> FsSnapshot {
    deadline(async {
        loop {
            match completion.try_snapshot() {
                Ok(snapshot) => return snapshot,
                Err(FsSnapshotError::Busy) => tokio::task::yield_now().await,
                Err(error) => panic!("unexpected snapshot error: {error:?}"),
            }
        }
    })
    .await
}

fn start_host(roots: usize) -> (HostFsOwner, Arc<HostResources>) {
    HostFsOwner::start(
        tokio::runtime::Handle::current(),
        HostLimits::new(2, 2, roots).unwrap(),
    )
    .unwrap()
}

async fn next_root(host: &Arc<HostResources>) -> (FsScope, FsOwner, FsFinalizer, FsCompletion) {
    // Completion publication precedes root-table retirement. Wait for actual
    // capacity return rather than treating a completed snapshot as capacity.
    deadline(async {
        loop {
            match host.try_filesystem_scope() {
                Ok(root) => return root,
                Err(error) if error.kind() == AdmissionKind::CleanupObligation => {
                    tokio::task::yield_now().await;
                }
                Err(error) => panic!("unexpected root admission error: {error}"),
            }
        }
    })
    .await
}

#[tokio::test]
async fn snapshots_follow_receipts_and_root_admission() {
    let (host_owner, host) = start_host(1);
    let (scope, owner, finalizer, completion) = host.try_filesystem_scope().unwrap();

    let before = Instant::now();
    let initial = snapshot(&completion).await;
    assert!(initial.captured_at() >= before);
    assert!(initial.captured_at() <= Instant::now());
    assert!(initial.root_admission_open());
    assert!(!initial.cleanup_poisoned());
    assert!(!initial.failure_recorded());
    assert_eq!(initial.outstanding_work(), 0);
    assert_eq!(initial.cleanup(), FsCleanupState::Pending);

    let first = scope.try_work().unwrap();
    let second = scope.try_work().unwrap();
    let active = snapshot(&completion).await;
    assert_eq!(active.root(), initial.root());
    assert_eq!(active.outstanding_work(), 2);
    assert_eq!(active.cleanup(), FsCleanupState::Pending);

    host_owner.close_admission();
    assert_eq!(
        host.try_logical_io().unwrap_err().kind(),
        AdmissionKind::Closed
    );
    assert!(snapshot(&completion).await.root_admission_open());

    drop(owner);
    let closed = snapshot(&completion).await;
    assert!(!closed.root_admission_open());
    assert_eq!(closed.outstanding_work(), 2);
    assert_eq!(closed.cleanup(), FsCleanupState::Pending);
    assert_eq!(scope.try_work().unwrap_err().kind(), AdmissionKind::Closed);
    // A detached sample retains its original values after root mutation.
    assert!(initial.root_admission_open());
    assert_eq!(initial.outstanding_work(), 0);

    drop(first);
    assert_eq!(snapshot(&completion).await.outstanding_work(), 1);
    drop(second);
    deadline(completion.wait()).await.unwrap();
    deadline(finalizer.run()).await.unwrap();
    let completed = snapshot(&completion).await;
    assert_eq!(completed.cleanup(), FsCleanupState::Completed);
    assert_eq!(completed.outstanding_work(), 0);
    assert!(!completed.root_admission_open());

    deadline(host_owner.shutdown()).await.unwrap();
    // Retaining a scope retains a completed root, even after host shutdown.
    assert_eq!(
        snapshot(&completion).await.cleanup(),
        FsCleanupState::Completed
    );
    drop(scope);
    assert!(matches!(
        completion.try_snapshot(),
        Err(FsSnapshotError::Unavailable)
    ));
    // Loss of the diagnostic source does not erase the completion receipt.
    deadline(completion.wait()).await.unwrap();
}

#[tokio::test]
async fn cleanup_ready_and_running_do_not_claim_completion() {
    let (host_owner, host) = start_host(2);
    let (blocker_scope, blocker_owner, blocker_finalizer, blocker_completion) =
        host.try_filesystem_scope().unwrap();
    let (blocker_entered_tx, blocker_entered_rx) = tokio::sync::oneshot::channel();
    let (blocker_release_tx, blocker_release_rx) = tokio::sync::oneshot::channel();
    blocker_scope
        .own_cleanup(FilesystemOperation::ProcessCleanup, move || {
            blocker_entered_tx.send(()).unwrap();
            blocker_release_rx.blocking_recv().unwrap();
            Ok(())
        })
        .unwrap();
    drop(blocker_owner);
    deadline(blocker_entered_rx).await.unwrap();

    let (scope, owner, finalizer, completion) = host.try_filesystem_scope().unwrap();
    let work = scope.try_work().unwrap();
    let cleaned = Arc::new(AtomicBool::new(false));
    let cleanup_flag = Arc::clone(&cleaned);
    let (entered_tx, entered_rx) = tokio::sync::oneshot::channel();
    let (release_tx, release_rx) = tokio::sync::oneshot::channel();
    scope
        .own_cleanup(FilesystemOperation::ProcessCleanup, move || {
            entered_tx.send(()).unwrap();
            release_rx.blocking_recv().unwrap();
            cleanup_flag.store(true, Ordering::SeqCst);
            Ok(())
        })
        .unwrap();
    drop(owner);
    assert_eq!(
        snapshot(&completion).await.cleanup(),
        FsCleanupState::Pending
    );
    drop(work);

    // The other root holds the sole cleanup lane, so Ready cannot race Running.
    let ready = snapshot(&completion).await;
    assert_eq!(ready.outstanding_work(), 0);
    assert_eq!(ready.cleanup(), FsCleanupState::Ready);
    assert!(!cleaned.load(Ordering::SeqCst));

    blocker_release_tx.send(()).unwrap();
    deadline(entered_rx).await.unwrap();
    let running = snapshot(&completion).await;
    assert_eq!(running.cleanup(), FsCleanupState::Running);
    assert_eq!(running.outstanding_work(), 0);
    assert!(!cleaned.load(Ordering::SeqCst));
    let mut pending = Box::pin(completion.wait());
    std::future::poll_fn(|cx| {
        assert!(matches!(pending.as_mut().poll(cx), Poll::Pending));
        Poll::Ready(())
    })
    .await;
    drop(pending);

    release_tx.send(()).unwrap();
    deadline(completion.wait()).await.unwrap();
    assert!(cleaned.load(Ordering::SeqCst));
    assert_eq!(
        snapshot(&completion).await.cleanup(),
        FsCleanupState::Completed
    );
    deadline(finalizer.run()).await.unwrap();
    deadline(blocker_completion.wait()).await.unwrap();
    deadline(blocker_finalizer.run()).await.unwrap();
    deadline(host_owner.shutdown()).await.unwrap();
}

#[tokio::test]
async fn failed_cleanup_exposes_presence_without_failure_text() {
    let (host_owner, host) = start_host(1);
    let (scope, owner, finalizer, completion) = host.try_filesystem_scope().unwrap();
    let private_text = "/private/capability-sensitive-path/".repeat(1024);
    scope
        .own_cleanup(FilesystemOperation::ProcessCleanup, move || {
            Err(io::Error::other(private_text))
        })
        .unwrap();
    drop(owner);
    assert!(deadline(completion.wait()).await.is_err());
    let failed = snapshot(&completion).await;
    assert!(failed.failure_recorded());
    assert!(failed.cleanup_poisoned());
    assert_eq!(failed.cleanup(), FsCleanupState::Completed);
    assert_eq!(failed.outstanding_work(), 0);
    let formatted = format!("{failed:?}");
    assert!(!formatted.contains("capability-sensitive-path"));
    assert!(
        formatted.len() < 1024,
        "snapshot diagnostic must stay bounded"
    );
    assert!(deadline(finalizer.run()).await.is_err());
    deadline(host_owner.shutdown()).await.unwrap();
}

#[tokio::test]
async fn native_write_remains_owned_after_guest_waiter_cancellation() {
    let (host_owner, host) = start_host(1);
    let (scope, owner, finalizer, completion) = host.try_filesystem_scope().unwrap();
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("native-output");
    let cleanup_path = path.clone();
    let cleaned = Arc::new(AtomicBool::new(false));
    let cleanup_flag = Arc::clone(&cleaned);
    scope
        .own_cleanup(FilesystemOperation::ProcessCleanup, move || {
            assert_eq!(
                std::fs::read(&cleanup_path)?,
                b"written after guest cancellation"
            );
            std::fs::remove_file(cleanup_path)?;
            cleanup_flag.store(true, Ordering::SeqCst);
            Ok(())
        })
        .unwrap();
    let transaction = host
        .try_foreground_transaction(host.try_logical_io().unwrap(), &[&scope])
        .unwrap();
    let (entered_tx, entered_rx) = tokio::sync::oneshot::channel();
    let (release_tx, release_rx) = tokio::sync::oneshot::channel();
    let operation_path = path.clone();
    let waiter = tokio::spawn(async move {
        transaction
            .run_blocking(FilesystemOperation::FileStage, move || {
                entered_tx.send(()).unwrap();
                release_rx.blocking_recv().unwrap();
                std::fs::write(operation_path, b"written after guest cancellation")
            })
            .await
    });
    deadline(entered_rx).await.unwrap();
    assert_eq!(snapshot(&completion).await.outstanding_work(), 1);
    waiter.abort();
    assert!(deadline(waiter).await.err().unwrap().is_cancelled());
    drop(owner);
    let cancelled = snapshot(&completion).await;
    assert!(!cancelled.root_admission_open());
    assert_eq!(cancelled.outstanding_work(), 1);
    assert_eq!(cancelled.cleanup(), FsCleanupState::Pending);
    assert!(!path.exists());
    assert!(!cleaned.load(Ordering::SeqCst));

    release_tx.send(()).unwrap();
    deadline(completion.wait()).await.unwrap();
    assert!(cleaned.load(Ordering::SeqCst));
    assert!(!path.exists());
    let completed = snapshot(&completion).await;
    assert_eq!(completed.outstanding_work(), 0);
    assert_eq!(completed.cleanup(), FsCleanupState::Completed);
    assert!(!completed.failure_recorded());
    deadline(finalizer.run()).await.unwrap();
    deadline(host_owner.shutdown()).await.unwrap();
}

#[tokio::test]
async fn returned_result_retains_work_after_backend_completion() {
    let (host_owner, host) = start_host(1);
    let (scope, owner, finalizer, completion) = host.try_filesystem_scope().unwrap();
    let transaction = host
        .try_foreground_transaction(host.try_logical_io().unwrap(), &[&scope])
        .unwrap();
    let result = deadline(transaction.run_blocking(FilesystemOperation::ListingRead, || 42))
        .await
        .unwrap();
    drop(transaction);
    drop(owner);
    let retained = snapshot(&completion).await;
    assert_eq!(retained.outstanding_work(), 1);
    assert_eq!(retained.cleanup(), FsCleanupState::Pending);
    assert_eq!(result.into_inner(), 42);
    deadline(completion.wait()).await.unwrap();
    assert_eq!(
        snapshot(&completion).await.cleanup(),
        FsCleanupState::Completed
    );
    deadline(finalizer.run()).await.unwrap();
    deadline(host_owner.shutdown()).await.unwrap();
}

#[tokio::test]
async fn ordinary_operation_error_is_not_a_recorded_owner_failure() {
    let (host_owner, host) = start_host(1);
    let (scope, owner, finalizer, completion) = host.try_filesystem_scope().unwrap();
    let transaction = host
        .try_foreground_transaction(host.try_logical_io().unwrap(), &[&scope])
        .unwrap();
    let result = deadline(
        transaction.run_operation(FilesystemOperation::ListingRead, async {
            Err::<(), _>(io::Error::other("ordinary operation failure"))
        }),
    )
    .await
    .unwrap()
    .into_inner();
    assert_eq!(result.unwrap_err().kind(), io::ErrorKind::Other);
    assert!(!snapshot(&completion).await.failure_recorded());
    drop(transaction);
    drop(owner);
    deadline(completion.wait()).await.unwrap();
    let completed = snapshot(&completion).await;
    assert_eq!(completed.cleanup(), FsCleanupState::Completed);
    assert!(!completed.failure_recorded());
    assert!(!completed.cleanup_poisoned());
    deadline(finalizer.run()).await.unwrap();
    deadline(host_owner.shutdown()).await.unwrap();
}

#[tokio::test]
async fn late_recorded_failure_does_not_rewrite_completion_receipt() {
    let (host_owner, host) = start_host(1);
    let (scope, owner, finalizer, completion) = host.try_filesystem_scope().unwrap();
    drop(owner);
    deadline(completion.wait()).await.unwrap();
    deadline(finalizer.run()).await.unwrap();
    let completed = snapshot(&completion).await;
    assert!(!completed.failure_recorded());

    scope.record_cleanup_failure("late cleanup failure");
    let late_failure = snapshot(&completion).await;
    assert_eq!(late_failure.cleanup(), FsCleanupState::Completed);
    assert!(late_failure.failure_recorded());
    assert!(late_failure.cleanup_poisoned());
    assert!(!completed.failure_recorded());
    deadline(completion.wait()).await.unwrap();
    deadline(host_owner.shutdown()).await.unwrap();
}

#[tokio::test]
async fn repeated_reads_and_retained_completions_do_not_hold_root_capacity() {
    let (host_owner, host) = start_host(1);
    let mut previous: Option<(FsSnapshot, FsCompletion)> = None;
    for _ in 0..64 {
        let (scope, owner, finalizer, completion) = next_root(&host).await;
        let current = snapshot(&completion).await;
        if let Some((old_snapshot, old_completion)) = previous.take() {
            assert_ne!(old_snapshot.root(), current.root());
            assert_eq!(
                current.root().generation(),
                old_snapshot.root().generation() + 1
            );
            // The reaper releases capacity before dropping its local Arc.
            // A retained completion must still lose its source after that drop.
            deadline(async {
                loop {
                    match old_completion.try_snapshot() {
                        Err(FsSnapshotError::Unavailable) => break,
                        Err(FsSnapshotError::Busy) => {}
                        Ok(retiring) => {
                            assert_eq!(retiring.root(), old_snapshot.root());
                            assert_eq!(retiring.cleanup(), FsCleanupState::Completed);
                        }
                        Err(error) => panic!("unexpected retiring snapshot error: {error:?}"),
                    }
                    tokio::task::yield_now().await;
                }
            })
            .await;
            deadline(old_completion.wait()).await.unwrap();
        }
        for _ in 0..64 {
            let sample = snapshot(&completion).await;
            assert_eq!(sample.root(), current.root());
            assert_eq!(sample.outstanding_work(), 0);
            assert_eq!(sample.cleanup(), FsCleanupState::Pending);
        }
        match host.try_filesystem_scope() {
            Err(error) => assert_eq!(error.kind(), AdmissionKind::CleanupObligation),
            Ok(_) => panic!("diagnostic reads must not alter the one-root capacity limit"),
        }
        drop(owner);
        deadline(finalizer.run()).await.unwrap();
        drop(scope);
        previous = Some((current, completion));
    }
    deadline(host_owner.shutdown()).await.unwrap();
    let (_, stopped_completion) = previous.unwrap();
    assert!(matches!(
        stopped_completion.try_snapshot(),
        Err(FsSnapshotError::Unavailable)
    ));

    let (restarted_owner, restarted_host) = start_host(1);
    let (_scope, owner, finalizer, completion) = restarted_host.try_filesystem_scope().unwrap();
    assert_eq!(snapshot(&completion).await.root().generation(), 1);
    assert!(matches!(
        stopped_completion.try_snapshot(),
        Err(FsSnapshotError::Unavailable)
    ));
    drop(owner);
    deadline(finalizer.run()).await.unwrap();
    deadline(restarted_owner.shutdown()).await.unwrap();
}

#[tokio::test]
async fn concurrent_reads_are_coherent_during_receipt_release_and_cleanup() {
    let (host_owner, host) = start_host(1);
    let (scope, owner, finalizer, completion) = host.try_filesystem_scope().unwrap();
    let mut work: Vec<_> = (0..32).map(|_| scope.try_work().unwrap()).collect();
    let boundary = Arc::new(Barrier::new(5));
    let mut readers = Vec::new();
    for _ in 0..4 {
        let boundary = Arc::clone(&boundary);
        let completion = completion.clone();
        readers.push(std::thread::spawn(move || {
            let mut samples = Vec::with_capacity(32);
            for _ in 0..32 {
                boundary.wait();
                samples.push(completion.try_snapshot());
                boundary.wait();
            }
            samples
        }));
    }
    let mut owner = Some(owner);
    for _ in 0..32 {
        boundary.wait();
        drop(owner.take());
        drop(work.pop().unwrap());
        boundary.wait();
    }
    for reader in readers {
        let mut last_count = 32;
        let mut last_capture = None;
        let mut saw_closed = false;
        let mut saw_completed = false;
        for result in reader.join().unwrap() {
            let sample = match result {
                Ok(sample) => sample,
                Err(FsSnapshotError::Busy) => continue,
                Err(error) => panic!("unexpected concurrent snapshot error: {error:?}"),
            };
            assert!(sample.outstanding_work() <= last_count);
            if saw_closed {
                assert!(!sample.root_admission_open());
            }
            if let Some(last_capture) = last_capture {
                assert!(sample.captured_at() >= last_capture);
            }
            if sample.cleanup() != FsCleanupState::Pending {
                assert!(!sample.root_admission_open());
                assert_eq!(sample.outstanding_work(), 0);
            }
            if saw_completed {
                assert_eq!(sample.cleanup(), FsCleanupState::Completed);
            }
            saw_closed |= !sample.root_admission_open();
            saw_completed |= sample.cleanup() == FsCleanupState::Completed;
            last_count = sample.outstanding_work();
            last_capture = Some(sample.captured_at());
        }
    }
    deadline(completion.wait()).await.unwrap();
    assert_eq!(
        snapshot(&completion).await.cleanup(),
        FsCleanupState::Completed
    );
    deadline(finalizer.run()).await.unwrap();
    deadline(host_owner.shutdown()).await.unwrap();
}
