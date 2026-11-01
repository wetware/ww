use std::future::Future;
use std::io;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Barrier};
use std::task::Poll;
use std::time::Duration;

use tokio::sync::oneshot;
use ww_cache::{
    AdmissionKind, FilesystemOperation, ForegroundTransaction, FsCleanupState, FsCompletion,
    FsDirectoryBuildSnapshot, FsOperationBackend, FsOperationReceipt, FsOperationWaiter, FsScope,
    FsSnapshot, FsSnapshotError, HostFsOwner, HostLimits, HostResources,
    FS_DIRECTORY_BUILD_CAPACITY,
};

async fn deadline<F: Future>(future: F) -> F::Output {
    tokio::time::timeout(Duration::from_secs(5), future)
        .await
        .expect("directory lifecycle test exceeded its deadline")
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

fn start_host(work: usize, roots: usize) -> (HostFsOwner, Arc<HostResources>) {
    HostFsOwner::start(
        tokio::runtime::Handle::current(),
        HostLimits::new(work, work + 1, roots).unwrap(),
    )
    .unwrap()
}

fn admit_transaction(host: &Arc<HostResources>, scope: &FsScope) -> ForegroundTransaction {
    host.try_foreground_transaction(host.try_logical_io().unwrap(), &[scope])
        .unwrap()
}

fn only_operation(snapshot: &FsSnapshot) -> FsDirectoryBuildSnapshot {
    let mut operations = snapshot.directory_builds().iter().flatten();
    let operation = *operations.next().expect("one directory build");
    assert!(operations.next().is_none());
    operation
}

fn operation(snapshot: &FsSnapshot, id: u64) -> FsDirectoryBuildSnapshot {
    *snapshot
        .directory_builds()
        .iter()
        .flatten()
        .find(|operation| operation.id() == id)
        .expect("operation remains in the bounded snapshot")
}

#[tokio::test]
async fn late_root_close_retains_completed_unpolled_directory_result_until_disposal() {
    struct NativeResult(Option<std::fs::File>, Arc<AtomicBool>);
    impl Drop for NativeResult {
        fn drop(&mut self) {
            drop(self.0.take());
            self.1.store(true, Ordering::SeqCst);
        }
    }
    let (host_owner, host) = start_host(1, 1);
    let (scope, owner, finalizer, completion) = host.try_filesystem_scope().unwrap();
    let cancellation = scope.cancellation();
    let staging = tempfile::tempdir().unwrap();
    let path = staging.path().join("result");
    let disposed = Arc::new(AtomicBool::new(false));
    let native_result = NativeResult(
        Some(std::fs::File::create(&path).unwrap()),
        Arc::clone(&disposed),
    );
    let cleanup_witness = Arc::clone(&disposed);
    let cleanup_path = path.clone();
    scope
        .own_cleanup(FilesystemOperation::ProcessCleanup, move || {
            assert!(cleanup_witness.load(Ordering::SeqCst));
            std::fs::remove_file(cleanup_path)
        })
        .unwrap();
    let transaction = admit_transaction(&host, &scope);
    let operation_scope = scope.clone();
    let mut waiter = Box::pin(async move {
        transaction
            .run_directory_build(&operation_scope, move || Ok::<_, io::Error>(native_result))
            .await
    });
    std::future::poll_fn(|cx| {
        assert!(waiter.as_mut().poll(cx).is_pending());
        Poll::Ready(())
    })
    .await;
    deadline(async {
        while only_operation(&snapshot(&completion).await).backend()
            != FsOperationBackend::Succeeded
        {
            tokio::task::yield_now().await;
        }
    })
    .await;
    // The result waiter has not been polled since submission, even though its
    // native backend finished. Root closure cannot dispose its native payload.
    drop(owner);
    assert!(cancellation.is_cancelled());
    assert!(!disposed.load(Ordering::SeqCst));
    assert!(path.exists());
    assert!(host.try_foreground().is_err());
    assert!(host.try_logical_io().is_err());
    assert_eq!(
        only_operation(&snapshot(&completion).await).receipt(),
        FsOperationReceipt::Outstanding
    );
    drop(waiter);
    deadline(finalizer.run()).await.unwrap();
    assert!(disposed.load(Ordering::SeqCst));
    assert!(!path.exists());
    assert!(host.try_foreground().is_ok());
    assert!(host.try_logical_io().is_ok());
    deadline(host_owner.shutdown()).await.unwrap();
}

#[tokio::test]
async fn backend_return_does_not_release_a_retained_result_receipt() {
    struct ResultResource(Arc<AtomicBool>);
    impl Drop for ResultResource {
        fn drop(&mut self) {
            self.0.store(true, Ordering::SeqCst);
        }
    }

    let (host_owner, host) = start_host(1, 1);
    let (scope, owner, finalizer, completion) = host.try_filesystem_scope().unwrap();
    let transaction = admit_transaction(&host, &scope);
    let disposed = Arc::new(AtomicBool::new(false));
    let resource = ResultResource(Arc::clone(&disposed));
    let result =
        deadline(transaction.run_directory_build(&scope, move || Ok::<_, io::Error>(resource)))
            .await
            .unwrap();

    let returned = snapshot(&completion).await;
    let operation = only_operation(&returned);
    assert_eq!(operation.waiter(), FsOperationWaiter::Returned);
    assert_eq!(operation.backend(), FsOperationBackend::Succeeded);
    assert_eq!(operation.receipt(), FsOperationReceipt::Outstanding);
    assert_eq!(returned.outstanding_work(), 1);
    assert!(!disposed.load(Ordering::SeqCst));

    drop(transaction);
    drop(owner);
    let retained = snapshot(&completion).await;
    assert_eq!(retained.cleanup(), FsCleanupState::Pending);
    assert_eq!(
        only_operation(&retained).receipt(),
        FsOperationReceipt::Outstanding
    );
    assert!(matches!(
        host.try_foreground(),
        Err(error) if error.kind() == AdmissionKind::ForegroundProducer
    ));

    result.dispose();
    deadline(completion.wait()).await.unwrap();
    assert!(disposed.load(Ordering::SeqCst));
    let completed = snapshot(&completion).await;
    assert_eq!(
        only_operation(&completed).receipt(),
        FsOperationReceipt::Released
    );
    assert_eq!(completed.outstanding_work(), 0);
    assert_eq!(completed.cleanup(), FsCleanupState::Completed);
    // The capacity witness is independent of the diagnostic fields.
    drop(host.try_foreground().unwrap());
    deadline(finalizer.run()).await.unwrap();
    deadline(host_owner.shutdown()).await.unwrap();
}

#[tokio::test]
async fn dropped_waiter_preserves_native_execution_until_cleanup_can_start() {
    let (host_owner, host) = start_host(1, 1);
    let (scope, owner, finalizer, completion) = host.try_filesystem_scope().unwrap();
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("native-output");
    let cleanup_path = path.clone();
    let (cleanup_entered_tx, cleanup_entered_rx) = oneshot::channel();
    let (cleanup_release_tx, cleanup_release_rx) = oneshot::channel();
    scope
        .own_cleanup(FilesystemOperation::ProcessCleanup, move || {
            assert_eq!(
                std::fs::read(&cleanup_path)?,
                b"native work survived waiter drop"
            );
            cleanup_entered_tx.send(()).unwrap();
            cleanup_release_rx.blocking_recv().unwrap();
            std::fs::remove_file(cleanup_path)
        })
        .unwrap();
    let transaction = admit_transaction(&host, &scope);
    let operation_scope = scope.clone();
    let operation_path = path.clone();
    let (entered_tx, entered_rx) = oneshot::channel();
    let (release_tx, release_rx) = oneshot::channel();
    let waiter = tokio::spawn(async move {
        transaction
            .run_directory_build(&operation_scope, move || {
                entered_tx.send(()).unwrap();
                release_rx.blocking_recv().unwrap();
                std::fs::write(operation_path, b"native work survived waiter drop")
            })
            .await
    });
    deadline(entered_rx).await.unwrap();
    waiter.abort();
    assert!(deadline(waiter).await.err().unwrap().is_cancelled());
    drop(owner);

    let cancelled = snapshot(&completion).await;
    let operation = only_operation(&cancelled);
    assert_eq!(operation.waiter(), FsOperationWaiter::Dropped);
    assert_eq!(operation.backend(), FsOperationBackend::Running);
    assert_eq!(operation.receipt(), FsOperationReceipt::Outstanding);
    assert_eq!(cancelled.outstanding_work(), 1);
    assert!(!cancelled.root_admission_open());
    assert_eq!(cancelled.cleanup(), FsCleanupState::Pending);
    assert!(!path.exists());
    assert!(matches!(
        host.try_foreground(),
        Err(error) if error.kind() == AdmissionKind::ForegroundProducer
    ));
    assert_eq!(scope.try_work().unwrap_err().kind(), AdmissionKind::Closed);

    release_tx.send(()).unwrap();
    deadline(cleanup_entered_rx).await.unwrap();
    let cleaning = snapshot(&completion).await;
    let operation = only_operation(&cleaning);
    assert_eq!(operation.waiter(), FsOperationWaiter::Dropped);
    assert_eq!(operation.backend(), FsOperationBackend::Succeeded);
    assert_eq!(operation.receipt(), FsOperationReceipt::Released);
    assert_eq!(cleaning.outstanding_work(), 0);
    assert_eq!(cleaning.cleanup(), FsCleanupState::Running);
    assert!(path.exists());
    let mut completion_wait = Box::pin(completion.wait());
    std::future::poll_fn(|cx| {
        assert!(completion_wait.as_mut().poll(cx).is_pending());
        Poll::Ready(())
    })
    .await;

    cleanup_release_tx.send(()).unwrap();
    deadline(completion_wait).await.unwrap();
    assert!(!path.exists());
    assert_eq!(
        snapshot(&completion).await.cleanup(),
        FsCleanupState::Completed
    );
    deadline(finalizer.run()).await.unwrap();
    deadline(host_owner.shutdown()).await.unwrap();
}

#[tokio::test]
async fn backend_error_is_visible_without_changing_root_failure_semantics() {
    let (host_owner, host) = start_host(1, 1);
    let (scope, owner, finalizer, completion) = host.try_filesystem_scope().unwrap();
    let transaction = admit_transaction(&host, &scope);
    let error = deadline(transaction.run_directory_build(&scope, || {
        Err::<(), _>(io::Error::other("/private/capability-sensitive-path"))
    }))
    .await
    .unwrap()
    .into_inner()
    .unwrap_err();
    assert_eq!(error.kind(), io::ErrorKind::Other);
    let failed = snapshot(&completion).await;
    assert_eq!(
        only_operation(&failed).backend(),
        FsOperationBackend::Failed
    );
    assert_eq!(
        only_operation(&failed).waiter(),
        FsOperationWaiter::Returned
    );
    assert!(!failed.failure_recorded());
    assert!(!failed.cleanup_poisoned());
    assert!(!format!("{failed:?}").contains("capability-sensitive-path"));
    drop(transaction);
    drop(owner);
    deadline(completion.wait()).await.unwrap();
    deadline(finalizer.run()).await.unwrap();
    deadline(host_owner.shutdown()).await.unwrap();
}

#[tokio::test]
async fn backend_panic_keeps_the_existing_recorded_failure() {
    let (host_owner, host) = start_host(1, 1);
    let (scope, owner, finalizer, completion) = host.try_filesystem_scope().unwrap();
    let transaction = admit_transaction(&host, &scope);
    let result = deadline(
        transaction.run_directory_build(&scope, || -> io::Result<()> {
            panic!("deliberate native backend panic")
        }),
    )
    .await;
    assert!(result.is_err());
    let failed = snapshot(&completion).await;
    assert_eq!(
        only_operation(&failed).backend(),
        FsOperationBackend::Panicked
    );
    assert_eq!(
        only_operation(&failed).waiter(),
        FsOperationWaiter::Returned
    );
    assert!(failed.failure_recorded());
    assert!(!failed.cleanup_poisoned());
    drop(transaction);
    drop(owner);
    assert!(deadline(completion.wait()).await.is_err());
    assert!(deadline(finalizer.run()).await.is_err());
    deadline(host_owner.shutdown()).await.unwrap();
}

#[tokio::test]
async fn independent_operations_keep_distinct_waiter_backend_and_receipt_states() {
    let (host_owner, host) = start_host(2, 1);
    let (scope, owner, finalizer, completion) = host.try_filesystem_scope().unwrap();
    let first_transaction = admit_transaction(&host, &scope);
    let first_scope = scope.clone();
    let (first_entered_tx, first_entered_rx) = oneshot::channel();
    let (first_release_tx, first_release_rx) = oneshot::channel();
    let first_finished = Arc::new(AtomicBool::new(false));
    let first_flag = Arc::clone(&first_finished);
    let first = tokio::spawn(async move {
        first_transaction
            .run_directory_build(&first_scope, move || {
                first_entered_tx.send(()).unwrap();
                first_release_rx.blocking_recv().unwrap();
                first_flag.store(true, Ordering::SeqCst);
                Ok::<_, io::Error>(11)
            })
            .await
    });
    deadline(first_entered_rx).await.unwrap();
    let first_id = only_operation(&snapshot(&completion).await).id();
    let second_transaction = admit_transaction(&host, &scope);
    let second_scope = scope.clone();
    let (second_entered_tx, second_entered_rx) = oneshot::channel();
    let (second_release_tx, second_release_rx) = oneshot::channel();
    let second = tokio::spawn(async move {
        second_transaction
            .run_directory_build(&second_scope, move || {
                second_entered_tx.send(()).unwrap();
                second_release_rx.blocking_recv().unwrap();
                Ok::<_, io::Error>(22)
            })
            .await
    });
    deadline(second_entered_rx).await.unwrap();
    let concurrent = snapshot(&completion).await;
    assert_eq!(concurrent.outstanding_work(), 2);
    let second_id = concurrent
        .directory_builds()
        .iter()
        .flatten()
        .find(|operation| operation.id() != first_id)
        .unwrap()
        .id();
    first.abort();
    assert!(deadline(first).await.err().unwrap().is_cancelled());
    second_release_tx.send(()).unwrap();
    let second_result = deadline(second).await.unwrap().unwrap();
    let mixed = snapshot(&completion).await;
    assert_eq!(
        operation(&mixed, first_id).waiter(),
        FsOperationWaiter::Dropped
    );
    assert_eq!(
        operation(&mixed, first_id).backend(),
        FsOperationBackend::Running
    );
    assert_eq!(
        operation(&mixed, second_id).waiter(),
        FsOperationWaiter::Returned
    );
    assert_eq!(
        operation(&mixed, second_id).backend(),
        FsOperationBackend::Succeeded
    );
    assert_eq!(
        operation(&mixed, second_id).receipt(),
        FsOperationReceipt::Outstanding
    );
    assert!(!first_finished.load(Ordering::SeqCst));
    assert_eq!(second_result.into_inner().unwrap(), 22);
    let one_left = snapshot(&completion).await;
    assert_eq!(one_left.outstanding_work(), 1);
    assert_eq!(
        operation(&one_left, second_id).receipt(),
        FsOperationReceipt::Released
    );
    assert_eq!(
        operation(&one_left, first_id).receipt(),
        FsOperationReceipt::Outstanding
    );
    drop(owner);
    first_release_tx.send(()).unwrap();
    deadline(completion.wait()).await.unwrap();
    assert!(first_finished.load(Ordering::SeqCst));
    assert_eq!(
        operation(&snapshot(&completion).await, first_id).receipt(),
        FsOperationReceipt::Released
    );
    deadline(finalizer.run()).await.unwrap();
    deadline(host_owner.shutdown()).await.unwrap();
}

#[tokio::test]
async fn full_observation_slots_do_not_reject_work_or_replace_live_receipts() {
    let (host_owner, host) = start_host(1, 1);
    let (scope, owner, finalizer, completion) = host.try_filesystem_scope().unwrap();
    let transaction = admit_transaction(&host, &scope);
    for value in 0..FS_DIRECTORY_BUILD_CAPACITY {
        let actual =
            deadline(transaction.run_directory_build(&scope, move || Ok::<_, io::Error>(value)))
                .await
                .unwrap()
                .into_inner()
                .unwrap();
        assert_eq!(actual, value);
    }
    let full = snapshot(&completion).await;
    let ids: Vec<_> = full
        .directory_builds()
        .iter()
        .flatten()
        .map(|record| record.id())
        .collect();
    assert_eq!(ids.len(), FS_DIRECTORY_BUILD_CAPACITY);
    assert_eq!(full.omitted_directory_builds(), 0);
    assert!(full
        .directory_builds()
        .iter()
        .flatten()
        .all(|record| record.receipt() == FsOperationReceipt::Outstanding));
    // These serial operations share one transaction receipt. They must not be
    // fabricated from the aggregate receipt count.
    assert_eq!(full.outstanding_work(), 1);
    let executed = Arc::new(AtomicBool::new(false));
    let execution_flag = Arc::clone(&executed);
    deadline(transaction.run_directory_build(&scope, move || {
        execution_flag.store(true, Ordering::SeqCst);
        Ok::<_, io::Error>(())
    }))
    .await
    .unwrap()
    .into_inner()
    .unwrap();
    assert!(executed.load(Ordering::SeqCst));
    let omitted = snapshot(&completion).await;
    assert_eq!(omitted.omitted_directory_builds(), 1);
    assert_eq!(omitted.directory_builds(), full.directory_builds());
    drop(transaction);
    let released = snapshot(&completion).await;
    assert!(released
        .directory_builds()
        .iter()
        .flatten()
        .all(|record| record.receipt() == FsOperationReceipt::Released));

    let replacement = admit_transaction(&host, &scope);
    deadline(replacement.run_directory_build(&scope, || Ok::<_, io::Error>(())))
        .await
        .unwrap()
        .dispose();
    let reused = snapshot(&completion).await;
    assert_eq!(
        reused.directory_builds().iter().flatten().count(),
        FS_DIRECTORY_BUILD_CAPACITY
    );
    assert_eq!(reused.omitted_directory_builds(), 1);
    let fresh = reused
        .directory_builds()
        .iter()
        .flatten()
        .find(|record| !ids.contains(&record.id()))
        .unwrap();
    assert!(fresh.id() > *ids.iter().max().unwrap());
    assert_eq!(fresh.receipt(), FsOperationReceipt::Outstanding);
    assert!(reused
        .directory_builds()
        .iter()
        .flatten()
        .filter(|record| record.id() != fresh.id())
        .all(|record| record.receipt() == FsOperationReceipt::Released));
    drop(replacement);
    drop(owner);
    deadline(completion.wait()).await.unwrap();
    deadline(finalizer.run()).await.unwrap();
    deadline(host_owner.shutdown()).await.unwrap();
}

#[tokio::test]
async fn receipt_release_makes_cleanup_ready_before_reaper_claims_it() {
    let (host_owner, host) = start_host(1, 2);
    let (blocker, blocker_owner, blocker_finalizer, _) = host.try_filesystem_scope().unwrap();
    let (blocker_entered_tx, blocker_entered_rx) = oneshot::channel();
    let (blocker_release_tx, blocker_release_rx) = oneshot::channel();
    blocker
        .own_cleanup(FilesystemOperation::ProcessCleanup, move || {
            blocker_entered_tx.send(()).unwrap();
            blocker_release_rx.blocking_recv().unwrap();
            Ok(())
        })
        .unwrap();
    drop(blocker_owner);
    deadline(blocker_entered_rx).await.unwrap();
    let (scope, owner, finalizer, completion) = host.try_filesystem_scope().unwrap();
    let (cleanup_entered_tx, cleanup_entered_rx) = oneshot::channel();
    let (cleanup_release_tx, cleanup_release_rx) = oneshot::channel();
    scope
        .own_cleanup(FilesystemOperation::ProcessCleanup, move || {
            cleanup_entered_tx.send(()).unwrap();
            cleanup_release_rx.blocking_recv().unwrap();
            Ok(())
        })
        .unwrap();
    let transaction = admit_transaction(&host, &scope);
    deadline(transaction.run_directory_build(&scope, || Ok::<_, io::Error>(())))
        .await
        .unwrap()
        .dispose();
    drop(owner);
    let retained = snapshot(&completion).await;
    assert_eq!(retained.cleanup(), FsCleanupState::Pending);
    assert_eq!(
        only_operation(&retained).backend(),
        FsOperationBackend::Succeeded
    );
    drop(transaction);
    let ready = snapshot(&completion).await;
    assert_eq!(ready.cleanup(), FsCleanupState::Ready);
    assert_eq!(
        only_operation(&ready).receipt(),
        FsOperationReceipt::Released
    );
    assert_eq!(ready.outstanding_work(), 0);
    blocker_release_tx.send(()).unwrap();
    deadline(cleanup_entered_rx).await.unwrap();
    assert_eq!(
        snapshot(&completion).await.cleanup(),
        FsCleanupState::Running
    );
    cleanup_release_tx.send(()).unwrap();
    deadline(completion.wait()).await.unwrap();
    assert_eq!(
        snapshot(&completion).await.cleanup(),
        FsCleanupState::Completed
    );
    deadline(finalizer.run()).await.unwrap();
    deadline(blocker_finalizer.run()).await.unwrap();
    deadline(host_owner.shutdown()).await.unwrap();
}

#[tokio::test]
async fn operation_records_do_not_cross_reused_root_generations() {
    let (host_owner, host) = start_host(1, 1);
    let (old_scope, old_owner, old_finalizer, old_completion) =
        host.try_filesystem_scope().unwrap();
    let old_transaction = admit_transaction(&host, &old_scope);
    deadline(old_transaction.run_directory_build(&old_scope, || Ok::<_, io::Error>(())))
        .await
        .unwrap()
        .dispose();
    drop(old_transaction);
    drop(old_owner);
    deadline(old_finalizer.run()).await.unwrap();
    let old_snapshot = snapshot(&old_completion).await;
    let (scope, owner, finalizer, completion) = deadline(async {
        loop {
            match host.try_filesystem_scope() {
                Ok(root) => break root,
                Err(error) if error.kind() == AdmissionKind::CleanupObligation => {
                    tokio::task::yield_now().await
                }
                Err(error) => panic!("unexpected admission error: {error}"),
            }
        }
    })
    .await;
    let empty = snapshot(&completion).await;
    assert_ne!(old_snapshot.root(), empty.root());
    assert_eq!(
        empty.root().generation(),
        old_snapshot.root().generation() + 1
    );
    assert!(empty.directory_builds().iter().all(Option::is_none));
    assert_eq!(empty.omitted_directory_builds(), 0);
    let transaction = admit_transaction(&host, &scope);
    deadline(transaction.run_directory_build(&scope, || {
        Err::<(), _>(io::Error::other("new root failure"))
    }))
    .await
    .unwrap()
    .dispose();
    let current = snapshot(&completion).await;
    assert_eq!(
        only_operation(&current).backend(),
        FsOperationBackend::Failed
    );
    assert_eq!(
        only_operation(&snapshot(&old_completion).await).backend(),
        FsOperationBackend::Succeeded
    );
    assert_eq!(
        only_operation(&old_snapshot).receipt(),
        FsOperationReceipt::Released
    );
    drop(transaction);
    drop(owner);
    deadline(finalizer.run()).await.unwrap();
    deadline(host_owner.shutdown()).await.unwrap();
    drop(old_scope);
    assert_eq!(
        old_completion.try_snapshot(),
        Err(FsSnapshotError::Unavailable)
    );
    assert_eq!(snapshot(&completion).await.root(), current.root());
}

#[tokio::test]
async fn owner_shutdown_waits_for_native_resource_disposal_and_cleanup() {
    struct NativeResult {
        file: Option<std::fs::File>,
        released: Arc<AtomicBool>,
    }
    impl Drop for NativeResult {
        fn drop(&mut self) {
            drop(self.file.take());
            self.released.store(true, Ordering::SeqCst);
        }
    }

    let (host_owner, host) = start_host(1, 1);
    let (scope, owner, finalizer, completion) = host.try_filesystem_scope().unwrap();
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("native-result");
    let released = Arc::new(AtomicBool::new(false));
    let result = NativeResult {
        file: Some(std::fs::File::create(&path).unwrap()),
        released: Arc::clone(&released),
    };
    let cleanup_path = path.clone();
    let cleanup_released = Arc::clone(&released);
    scope
        .own_cleanup(FilesystemOperation::ProcessCleanup, move || {
            assert!(
                cleanup_released.load(Ordering::SeqCst),
                "native result must be disposed before cleanup"
            );
            std::fs::remove_file(cleanup_path)
        })
        .unwrap();
    let transaction = admit_transaction(&host, &scope);
    let operation_scope = scope.clone();
    let (entered_tx, entered_rx) = oneshot::channel();
    let (release_tx, release_rx) = oneshot::channel();
    let waiter = tokio::spawn(async move {
        transaction
            .run_directory_build(&operation_scope, move || {
                entered_tx.send(()).unwrap();
                release_rx.blocking_recv().unwrap();
                Ok::<_, io::Error>(result)
            })
            .await
    });
    deadline(entered_rx).await.unwrap();
    waiter.abort();
    assert!(deadline(waiter).await.err().unwrap().is_cancelled());
    drop(owner);
    drop(finalizer);
    let mut shutdown = Box::pin(host_owner.shutdown());
    std::future::poll_fn(|cx| {
        assert!(shutdown.as_mut().poll(cx).is_pending());
        Poll::Ready(())
    })
    .await;
    assert!(!released.load(Ordering::SeqCst));
    assert!(path.exists());
    assert_eq!(
        only_operation(&snapshot(&completion).await).backend(),
        FsOperationBackend::Running
    );
    release_tx.send(()).unwrap();
    deadline(shutdown).await.unwrap();
    assert!(released.load(Ordering::SeqCst));
    assert!(!path.exists());
    let stopped = snapshot(&completion).await;
    assert_eq!(stopped.cleanup(), FsCleanupState::Completed);
    assert_eq!(stopped.outstanding_work(), 0);
    assert_eq!(
        only_operation(&stopped).receipt(),
        FsOperationReceipt::Released
    );
    drop(scope);
    assert_eq!(completion.try_snapshot(), Err(FsSnapshotError::Unavailable));
    deadline(completion.wait()).await.unwrap();
}

#[tokio::test]
async fn snapshots_remain_coherent_when_backend_completion_races_observation() {
    let (host_owner, host) = start_host(1, 1);
    let (scope, owner, finalizer, completion) = host.try_filesystem_scope().unwrap();
    let transaction = admit_transaction(&host, &scope);
    let operation_scope = scope.clone();
    let (entered_tx, entered_rx) = oneshot::channel();
    let (release_tx, release_rx) = oneshot::channel();
    let waiter = tokio::spawn(async move {
        transaction
            .run_directory_build(&operation_scope, move || {
                entered_tx.send(()).unwrap();
                release_rx.blocking_recv().unwrap();
                Ok::<_, io::Error>(())
            })
            .await
    });
    deadline(entered_rx).await.unwrap();
    let before = snapshot(&completion).await;
    assert_eq!(
        only_operation(&before).backend(),
        FsOperationBackend::Running
    );
    waiter.abort();
    assert!(deadline(waiter).await.err().unwrap().is_cancelled());
    drop(owner);
    let barrier = Arc::new(Barrier::new(2));
    let reader_barrier = Arc::clone(&barrier);
    let reader_completion = completion.clone();
    let (reader_done_tx, reader_done_rx) = oneshot::channel();
    let reader = std::thread::spawn(move || {
        reader_barrier.wait();
        for _ in 0..10_000 {
            match reader_completion.try_snapshot() {
                Ok(snapshot) => {
                    let operation = only_operation(&snapshot);
                    assert_eq!(operation.waiter(), FsOperationWaiter::Dropped);
                    match operation.receipt() {
                        FsOperationReceipt::Outstanding => {
                            assert_eq!(snapshot.outstanding_work(), 1);
                            assert_eq!(snapshot.cleanup(), FsCleanupState::Pending);
                        }
                        FsOperationReceipt::Released => {
                            assert_eq!(snapshot.outstanding_work(), 0);
                            assert_eq!(operation.backend(), FsOperationBackend::Succeeded);
                        }
                    }
                    if operation.backend() == FsOperationBackend::Running {
                        assert_eq!(operation.receipt(), FsOperationReceipt::Outstanding);
                    }
                }
                Err(FsSnapshotError::Busy) => {}
                Err(error) => panic!("unexpected concurrent snapshot error: {error:?}"),
            }
        }
        reader_done_tx.send(()).unwrap();
    });
    barrier.wait();
    release_tx.send(()).unwrap();
    deadline(completion.wait()).await.unwrap();
    deadline(reader_done_rx).await.unwrap();
    reader.join().unwrap();
    let after = snapshot(&completion).await;
    assert_eq!(after.cleanup(), FsCleanupState::Completed);
    assert_eq!(
        only_operation(&after).backend(),
        FsOperationBackend::Succeeded
    );
    assert_eq!(
        only_operation(&after).receipt(),
        FsOperationReceipt::Released
    );
    // Earlier detached observations cannot change when the backend completes.
    assert_eq!(
        only_operation(&before).backend(),
        FsOperationBackend::Running
    );
    deadline(finalizer.run()).await.unwrap();
    deadline(host_owner.shutdown()).await.unwrap();
}
