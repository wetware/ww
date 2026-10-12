use std::future::Future;
use std::io;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::task::Poll;

use ww_cache::{
    AdmissionKind, FilesystemOperation, FsCompletion, FsFinalizer, FsOwner, FsScope, FsWorkError,
    HostFsOwner, HostLimits, HostResources,
};

fn test_scope() -> (HostFsOwner, FsScope, FsOwner, FsFinalizer, FsCompletion) {
    let (host_owner, _host, scope, owner, finalizer, completion) = transaction_scope();
    (host_owner, scope, owner, finalizer, completion)
}

fn transaction_scope() -> (
    HostFsOwner,
    Arc<HostResources>,
    FsScope,
    FsOwner,
    FsFinalizer,
    FsCompletion,
) {
    let (host_owner, host) = HostFsOwner::start(
        tokio::runtime::Handle::current(),
        HostLimits::new(1, 2, 8).unwrap(),
    )
    .unwrap();
    let (scope, owner, finalizer, completion) = host.try_filesystem_scope().unwrap();
    (host_owner, host, scope, owner, finalizer, completion)
}

#[tokio::test]
async fn cancelled_waiter_does_not_release_actual_filesystem_work() {
    let (_host_owner, host, scope, owner, finalizer, completion) = transaction_scope();
    let transaction = host
        .try_foreground_transaction(host.try_logical_io().unwrap(), &[&scope])
        .unwrap();
    let (entered_tx, entered_rx) = tokio::sync::oneshot::channel();
    let (release_tx, release_rx) = tokio::sync::oneshot::channel();

    let waiter = tokio::spawn(async move {
        transaction
            .run_blocking(FilesystemOperation::FileStage, move || {
                entered_tx.send(()).unwrap();
                release_rx.blocking_recv().unwrap();
                42_u8
            })
            .await
    });

    entered_rx.await.unwrap();
    waiter.abort();
    assert!(waiter.await.err().unwrap().is_cancelled());
    assert_eq!(
        host.try_foreground().unwrap_err().kind(),
        AdmissionKind::ForegroundProducer
    );
    assert_eq!(
        host.try_logical_io().unwrap_err().kind(),
        AdmissionKind::HostLogical
    );
    drop(owner);

    let finalizer_task = tokio::spawn(finalizer.run());
    let mut pending_completion = Box::pin(completion.wait());
    std::future::poll_fn(|context| {
        assert!(
            matches!(pending_completion.as_mut().poll(context), Poll::Pending),
            "completion must remain pending while the blocking closure is gated"
        );
        Poll::Ready(())
    })
    .await;

    release_tx.send(()).unwrap();
    assert_eq!(completion.wait().await, Ok(()));
    assert_eq!(finalizer_task.await.unwrap(), Ok(()));
    assert!(host.try_foreground().is_ok());
    assert!(host.try_logical_io().is_ok());
}

#[tokio::test]
async fn blocking_panic_after_waiter_cancellation_fails_completion() {
    let (_host_owner, host, scope, owner, finalizer, completion) = transaction_scope();
    let transaction = host
        .try_foreground_transaction(host.try_logical_io().unwrap(), &[&scope])
        .unwrap();
    let (entered_tx, entered_rx) = tokio::sync::oneshot::channel();
    let (release_tx, release_rx) = tokio::sync::oneshot::channel();

    let waiter = tokio::spawn(async move {
        transaction
            .run_blocking(FilesystemOperation::DirectoryBuild, move || {
                entered_tx.send(()).unwrap();
                release_rx.blocking_recv().unwrap();
                panic!("injected blocking panic");
            })
            .await
    });
    entered_rx.await.unwrap();
    waiter.abort();
    assert!(waiter.await.err().unwrap().is_cancelled());
    drop(owner);
    let finalizer_task = tokio::spawn(finalizer.run());
    release_tx.send(()).unwrap();

    let error = completion.wait().await.unwrap_err();
    assert!(error.to_string().contains("DirectoryBuild panicked"));
    assert_eq!(finalizer_task.await.unwrap().unwrap_err(), error);
}

#[tokio::test]
async fn async_panic_after_waiter_cancellation_fails_completion() {
    let (_host_owner, host, scope, owner, finalizer, completion) = transaction_scope();
    let transaction = host
        .try_foreground_transaction(host.try_logical_io().unwrap(), &[&scope])
        .unwrap();
    let (entered_tx, entered_rx) = tokio::sync::oneshot::channel();
    let (release_tx, release_rx) = tokio::sync::oneshot::channel();
    let waiter = tokio::spawn(async move {
        transaction
            .run_operation::<(), io::Error, _>(FilesystemOperation::FileStage, async move {
                entered_tx.send(()).unwrap();
                release_rx.await.unwrap();
                panic!("injected async panic");
            })
            .await
    });
    entered_rx.await.unwrap();
    waiter.abort();
    assert!(waiter.await.err().unwrap().is_cancelled());
    assert_eq!(
        host.try_foreground().unwrap_err().kind(),
        AdmissionKind::ForegroundProducer
    );
    drop(owner);
    let finalizer_task = tokio::spawn(finalizer.run());
    release_tx.send(()).unwrap();

    let error = completion.wait().await.unwrap_err();
    assert!(error.to_string().contains("FileStage panicked"));
    assert_eq!(finalizer_task.await.unwrap().unwrap_err(), error);
}

#[tokio::test]
async fn cleanup_panic_after_finalizer_cancellation_fails_and_continues_cleanup() {
    let (_host_owner, scope, owner, finalizer, completion) = test_scope();
    let (entered_tx, entered_rx) = tokio::sync::oneshot::channel();
    let (release_tx, release_rx) = tokio::sync::oneshot::channel();
    let later_cleanup_ran = Arc::new(AtomicBool::new(false));

    scope
        .own_cleanup(FilesystemOperation::ProcessCleanup, move || {
            entered_tx.send(()).unwrap();
            release_rx.blocking_recv().unwrap();
            panic!("injected cleanup panic");
        })
        .unwrap();
    let later_cleanup_flag = Arc::clone(&later_cleanup_ran);
    scope
        .own_cleanup(FilesystemOperation::ProcessCleanup, move || {
            later_cleanup_flag.store(true, Ordering::SeqCst);
            Ok(())
        })
        .unwrap();
    drop(owner);

    let finalizer_task = tokio::spawn(finalizer.run());
    entered_rx.await.unwrap();
    finalizer_task.abort();
    assert!(finalizer_task.await.unwrap_err().is_cancelled());
    release_tx.send(()).unwrap();

    let error = completion.wait().await.unwrap_err();
    assert!(error.to_string().contains("ProcessCleanup panicked"));
    assert!(later_cleanup_ran.load(Ordering::SeqCst));
}

#[tokio::test]
async fn ordinary_operation_error_does_not_poison_cleanup() {
    let (_host_owner, host, scope, owner, finalizer, completion) = transaction_scope();
    let transaction = host
        .try_foreground_transaction(host.try_logical_io().unwrap(), &[&scope])
        .unwrap();
    let result = transaction
        .run_operation(FilesystemOperation::ListingRead, async {
            Err::<(), _>(io::Error::other("ordinary read failure"))
        })
        .await
        .unwrap()
        .into_inner();
    assert_eq!(result.unwrap_err().kind(), io::ErrorKind::Other);

    drop(transaction);
    drop(owner);
    assert_eq!(finalizer.run().await, Ok(()));
    assert_eq!(completion.wait().await, Ok(()));
}

#[tokio::test]
async fn private_cleanup_failure_is_repeatably_observable() {
    let (_host_owner, scope, owner, finalizer, completion) = test_scope();
    scope
        .own_cleanup(FilesystemOperation::ProcessCleanup, || {
            Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "injected cleanup denial",
            ))
        })
        .unwrap();
    drop(owner);

    let error = finalizer.run().await.unwrap_err();
    assert!(error.to_string().contains("injected cleanup denial"));
    assert_eq!(completion.wait().await.unwrap_err(), error);
    assert_eq!(completion.wait().await.unwrap_err(), error);
}

#[tokio::test]
async fn dropping_unstarted_empty_finalizer_completes() {
    let (_host_owner, _scope, owner, finalizer, completion) = test_scope();
    drop(owner);
    drop(finalizer);

    assert_eq!(completion.wait().await, Ok(()));
}

#[tokio::test]
async fn finalizer_can_subscribe_after_all_other_completion_receivers_are_gone() {
    let (host_owner, host) = HostFsOwner::start(
        tokio::runtime::Handle::current(),
        HostLimits::new(2, 2, 1).unwrap(),
    )
    .unwrap();
    let (_scope, owner, finalizer, completion) = host.try_filesystem_scope().unwrap();
    drop(completion);
    drop(owner);

    // Reacquiring the sole K permit proves the first root has published and
    // retired before `finalizer.run` creates its receiver.
    let (_second_scope, second_owner, second_finalizer, second_completion) = loop {
        match host.try_filesystem_scope() {
            Ok(root) => break root,
            Err(error) if error.kind() == AdmissionKind::CleanupObligation => {
                tokio::task::yield_now().await;
            }
            Err(error) => panic!("unexpected root admission error: {error}"),
        }
    };
    assert_eq!(finalizer.run().await, Ok(()));

    drop(second_owner);
    assert_eq!(second_finalizer.run().await, Ok(()));
    assert_eq!(second_completion.wait().await, Ok(()));
    host_owner.shutdown().await.unwrap();
}

#[tokio::test]
async fn dropped_finalizer_still_runs_private_cleanup_off_the_owner_thread() {
    let (_host_owner, scope, owner, finalizer, completion) = test_scope();
    let owner_thread = std::thread::current().id();
    let (cleaned_tx, cleaned_rx) = tokio::sync::oneshot::channel();
    scope
        .own_cleanup(FilesystemOperation::ProcessCleanup, move || {
            cleaned_tx.send(std::thread::current().id()).unwrap();
            Ok(())
        })
        .unwrap();
    drop(owner);
    drop(finalizer);

    assert_ne!(cleaned_rx.await.unwrap(), owner_thread);
    assert_eq!(completion.wait().await, Ok(()));
}

#[tokio::test]
async fn owner_close_rejects_new_foreground_work() {
    let (_host_owner, host, scope, owner, finalizer, _completion) = transaction_scope();
    drop(owner);

    let error = host
        .try_foreground_transaction(host.try_logical_io().unwrap(), &[&scope])
        .unwrap_err();
    assert_eq!(error.kind(), AdmissionKind::Closed);
    assert!(host.try_logical_io().is_ok());
    assert!(host.try_foreground().is_ok());
    let cleanup_error = scope
        .own_cleanup(FilesystemOperation::ProcessCleanup, || Ok(()))
        .unwrap_err();
    assert!(cleanup_error.to_string().contains("closed"));
    assert_eq!(finalizer.run().await, Ok(()));
}

#[tokio::test]
async fn admitted_operation_can_submit_internal_stages_after_owner_close() {
    let (_host_owner, host, scope, owner, finalizer, completion) = transaction_scope();
    let transaction = host
        .try_cleanup_transaction(host.try_logical_io().unwrap(), &[&scope])
        .unwrap();
    let phase = transaction.try_cleanup_phase().unwrap();
    let continuation = transaction.continuation();
    let (entered_tx, entered_rx) = tokio::sync::oneshot::channel();
    let (release_tx, release_rx) = tokio::sync::oneshot::channel();
    let cleaned = Arc::new(AtomicBool::new(false));
    let operation_scope = scope.clone();
    let operation_cleaned = Arc::clone(&cleaned);
    let operation = tokio::spawn(async move {
        transaction
            .run_operation(FilesystemOperation::FileStage, async move {
                entered_tx.send(()).unwrap();
                release_rx.await.unwrap();
                continuation
                    .run_blocking(FilesystemOperation::FileStage, move || {
                        operation_scope
                            .own_cleanup_from_phase(
                                phase,
                                FilesystemOperation::ProcessCleanup,
                                move || {
                                    operation_cleaned.store(true, Ordering::SeqCst);
                                    Ok(())
                                },
                            )
                            .unwrap();
                        7_u8
                    })
                    .await
                    .map(|envelope| envelope.into_inner())
            })
            .await
    });

    entered_rx.await.unwrap();
    drop(owner);
    assert_eq!(scope.try_work().unwrap_err().kind(), AdmissionKind::Closed);
    let finalizer_task = tokio::spawn(finalizer.run());
    release_tx.send(()).unwrap();

    assert_eq!(operation.await.unwrap().unwrap().into_inner().unwrap(), 7);
    assert_eq!(finalizer_task.await.unwrap(), Ok(()));
    assert_eq!(completion.wait().await, Ok(()));
    assert!(cleaned.load(Ordering::SeqCst));
    assert!(host.try_foreground().is_ok());
}

#[tokio::test]
async fn saturated_foreground_does_not_block_the_physical_cleanup_lane() {
    let (host_owner, host) = HostFsOwner::start(
        tokio::runtime::Handle::current(),
        HostLimits::new(4, 2, 1).unwrap(),
    )
    .unwrap();
    let foreground = host.try_foreground().unwrap();
    let (scope, owner, finalizer, completion) = host.try_filesystem_scope().unwrap();
    let (cleaned_tx, cleaned_rx) = tokio::sync::oneshot::channel();
    scope
        .own_cleanup(FilesystemOperation::ProcessCleanup, move || {
            cleaned_tx.send(()).unwrap();
            Ok(())
        })
        .unwrap();

    drop(owner);
    cleaned_rx.await.unwrap();
    assert_eq!(completion.wait().await, Ok(()));
    assert_eq!(finalizer.run().await, Ok(()));
    drop(foreground);
    host_owner.shutdown().await.unwrap();
}

#[tokio::test]
async fn saturated_foreground_blocking_pool_does_not_block_physical_cleanup_lane() {
    let foreground_runtime = tokio::runtime::Builder::new_current_thread()
        .max_blocking_threads(1)
        .enable_all()
        .build()
        .unwrap();
    let (blocker_entered_tx, blocker_entered_rx) = std::sync::mpsc::sync_channel(1);
    let (release_blocker_tx, release_blocker_rx) = std::sync::mpsc::sync_channel(1);
    let blocker = foreground_runtime.spawn_blocking(move || {
        blocker_entered_tx.send(()).unwrap();
        release_blocker_rx.recv().unwrap();
    });
    blocker_entered_rx.recv().unwrap();

    let (host_owner, host) = HostFsOwner::start(
        foreground_runtime.handle().clone(),
        HostLimits::new(2, 2, 1).unwrap(),
    )
    .unwrap();
    let (scope, owner, finalizer, completion) = host.try_filesystem_scope().unwrap();
    let (cleaned_tx, cleaned_rx) = tokio::sync::oneshot::channel();
    scope
        .own_cleanup(FilesystemOperation::ProcessCleanup, move || {
            cleaned_tx.send(()).unwrap();
            Ok(())
        })
        .unwrap();
    drop(owner);

    // Cleanup must make progress before the captured runtime's only blocking
    // worker is released.
    cleaned_rx.await.unwrap();
    assert_eq!(finalizer.run().await, Ok(()));
    assert_eq!(completion.wait().await, Ok(()));
    host_owner.shutdown().await.unwrap();

    release_blocker_tx.send(()).unwrap();
    tokio::task::spawn_blocking(move || {
        foreground_runtime.block_on(blocker).unwrap();
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn unready_root_never_starts_cleanup() {
    let (host_owner, scope, owner, finalizer, completion) = test_scope();
    let work = scope.try_work().unwrap();
    let (cleaned_tx, cleaned_rx) = tokio::sync::oneshot::channel();
    scope
        .own_cleanup(FilesystemOperation::ProcessCleanup, move || {
            cleaned_tx.send(()).unwrap();
            Ok(())
        })
        .unwrap();
    drop(owner);

    let mut cleaned_rx = Box::pin(cleaned_rx);
    std::future::poll_fn(|context| {
        assert!(matches!(cleaned_rx.as_mut().poll(context), Poll::Pending));
        Poll::Ready(())
    })
    .await;
    drop(work);
    cleaned_rx.await.unwrap();
    assert_eq!(finalizer.run().await, Ok(()));
    assert_eq!(completion.wait().await, Ok(()));
    host_owner.shutdown().await.unwrap();
}

#[tokio::test]
async fn resolved_registration_drops_payload_without_running_it() {
    let (host_owner, scope, owner, finalizer, completion) = test_scope();
    let ran = Arc::new(AtomicBool::new(false));
    let cleanup_ran = Arc::clone(&ran);
    let registration = scope
        .own_cleanup(FilesystemOperation::FileStage, move || {
            cleanup_ran.store(true, Ordering::SeqCst);
            Ok(())
        })
        .unwrap();
    registration.resolve().unwrap();

    drop(owner);
    assert_eq!(finalizer.run().await, Ok(()));
    assert_eq!(completion.wait().await, Ok(()));
    assert!(!ran.load(Ordering::SeqCst));
    host_owner.shutdown().await.unwrap();
}

#[tokio::test]
async fn unresolved_registration_handle_drop_preserves_cleanup() {
    let (host_owner, scope, owner, finalizer, completion) = test_scope();
    let (cleaned_tx, cleaned_rx) = tokio::sync::oneshot::channel();
    let registration = scope
        .own_cleanup(FilesystemOperation::FileStage, move || {
            cleaned_tx.send(()).unwrap();
            Ok(())
        })
        .unwrap();
    drop(registration);
    drop(owner);

    cleaned_rx.await.unwrap();
    assert_eq!(finalizer.run().await, Ok(()));
    assert_eq!(completion.wait().await, Ok(()));
    host_owner.shutdown().await.unwrap();
}

#[tokio::test]
async fn first_cleanup_failure_poisons_new_effects_but_finishes_existing_cleanup() {
    let (host_owner, host) = HostFsOwner::start(
        tokio::runtime::Handle::current(),
        HostLimits::new(4, 2, 1).unwrap(),
    )
    .unwrap();
    let (scope, owner, finalizer, completion) = host.try_filesystem_scope().unwrap();
    let admitted = host
        .try_cleanup_transaction(host.try_logical_io().unwrap(), &[&scope])
        .unwrap();
    let mandatory_phase = admitted.try_cleanup_phase().unwrap();
    let existing_ran = Arc::new(AtomicBool::new(false));
    let existing_cleanup = Arc::clone(&existing_ran);
    scope
        .own_cleanup(FilesystemOperation::ProcessCleanup, move || {
            existing_cleanup.store(true, Ordering::SeqCst);
            Ok(())
        })
        .unwrap();
    scope.record_cleanup_failure("injected rollback failure");

    // Mandatory cleanup from an already-admitted transaction remains legal
    // after poison when its finite phase was reserved before the effect.
    scope
        .own_cleanup_from_phase(mandatory_phase, FilesystemOperation::FileStage, || Ok(()))
        .unwrap();

    let new_phase_error = admitted.try_cleanup_phase().unwrap_err();
    assert_eq!(new_phase_error.kind(), AdmissionKind::CleanupPoisoned);

    let error = scope
        .own_cleanup(FilesystemOperation::FileStage, || Ok(()))
        .unwrap_err();
    assert!(matches!(
        error,
        FsWorkError::Admission(ref error) if error.kind() == AdmissionKind::CleanupPoisoned
    ));
    assert_eq!(
        scope.try_cleanup_work().unwrap_err().kind(),
        AdmissionKind::CleanupPoisoned
    );

    drop(admitted);
    drop(owner);
    let final_error = finalizer.run().await.unwrap_err();
    assert!(final_error
        .to_string()
        .contains("injected rollback failure"));
    assert_eq!(completion.wait().await.unwrap_err(), final_error);
    assert!(existing_ran.load(Ordering::SeqCst));
    host_owner.shutdown().await.unwrap();
}

#[tokio::test]
async fn async_cleanup_runs_on_the_single_reaper_runtime() {
    let (host_owner, scope, owner, finalizer, completion) = test_scope();
    let owner_thread = std::thread::current().id();
    let (cleaned_tx, cleaned_rx) = tokio::sync::oneshot::channel();
    scope
        .own_async_cleanup(FilesystemOperation::ProcessCleanup, move || async move {
            cleaned_tx.send(std::thread::current().id()).unwrap();
            Ok(())
        })
        .unwrap();
    drop(owner);

    assert_ne!(cleaned_rx.await.unwrap(), owner_thread);
    assert_eq!(finalizer.run().await, Ok(()));
    assert_eq!(completion.wait().await, Ok(()));
    host_owner.shutdown().await.unwrap();
}

#[tokio::test]
async fn many_finalizers_share_one_cleanup_thread() {
    let (host_owner, host) = HostFsOwner::start(
        tokio::runtime::Handle::current(),
        HostLimits::new(8, 2, 4).unwrap(),
    )
    .unwrap();
    let (thread_tx, mut thread_rx) = tokio::sync::mpsc::unbounded_channel();
    let mut completions = Vec::new();
    for _ in 0..4 {
        let (scope, owner, finalizer, completion) = host.try_filesystem_scope().unwrap();
        let thread_tx = thread_tx.clone();
        scope
            .own_cleanup(FilesystemOperation::ProcessCleanup, move || {
                thread_tx.send(std::thread::current().id()).unwrap();
                Ok(())
            })
            .unwrap();
        drop(owner);
        drop(finalizer);
        completions.push(completion);
    }
    drop(thread_tx);

    let first = thread_rx.recv().await.unwrap();
    for _ in 1..4 {
        assert_eq!(thread_rx.recv().await.unwrap(), first);
    }
    for completion in completions {
        assert_eq!(completion.wait().await, Ok(()));
    }
    host_owner.shutdown().await.unwrap();
}

#[tokio::test]
async fn result_payload_is_disposed_before_transaction_releases_and_root_cleans() {
    struct ResultPayload {
        host: Arc<HostResources>,
        dropped: Arc<AtomicBool>,
        ownership_released_early: Arc<AtomicBool>,
    }
    impl Drop for ResultPayload {
        fn drop(&mut self) {
            if self.host.try_foreground().is_ok() {
                self.ownership_released_early.store(true, Ordering::SeqCst);
            }
            self.dropped.store(true, Ordering::SeqCst);
        }
    }

    let (host_owner, host) = HostFsOwner::start(
        tokio::runtime::Handle::current(),
        HostLimits::new(1, 2, 1).unwrap(),
    )
    .unwrap();
    let (scope, owner, finalizer, completion) = host.try_filesystem_scope().unwrap();
    let payload_dropped = Arc::new(AtomicBool::new(false));
    let ownership_released_early = Arc::new(AtomicBool::new(false));
    let cleanup_observation = Arc::clone(&payload_dropped);
    scope
        .own_cleanup(FilesystemOperation::ProcessCleanup, move || {
            if cleanup_observation.load(Ordering::SeqCst) {
                Ok(())
            } else {
                Err(io::Error::other("cleanup ran before result disposal"))
            }
        })
        .unwrap();
    let logical = host.try_logical_io().unwrap();
    let transaction = host.try_foreground_transaction(logical, &[&scope]).unwrap();
    let envelope = transaction
        .run_operation(FilesystemOperation::DescriptorOpen, {
            let payload_dropped = Arc::clone(&payload_dropped);
            let ownership_released_early = Arc::clone(&ownership_released_early);
            let host = Arc::clone(&host);
            async move {
                Ok::<_, io::Error>(ResultPayload {
                    host,
                    dropped: payload_dropped,
                    ownership_released_early,
                })
            }
        })
        .await
        .unwrap();
    drop(transaction);
    drop(owner);

    let mut pending = Box::pin(completion.wait());
    std::future::poll_fn(|context| {
        assert!(matches!(pending.as_mut().poll(context), Poll::Pending));
        Poll::Ready(())
    })
    .await;
    assert_eq!(
        host.try_foreground().unwrap_err().kind(),
        AdmissionKind::ForegroundProducer
    );
    assert_eq!(
        host.try_logical_io().unwrap_err().kind(),
        AdmissionKind::HostLogical
    );
    drop(envelope);
    assert!(!ownership_released_early.load(Ordering::SeqCst));
    let replacement_logical = host.try_logical_io().unwrap();
    drop(replacement_logical);

    assert_eq!(finalizer.run().await, Ok(()));
    assert_eq!(completion.wait().await, Ok(()));
    host_owner.shutdown().await.unwrap();
}

#[tokio::test]
async fn accepted_transaction_survives_the_callers_runtime_loss() {
    let (host_owner, host) = HostFsOwner::start(
        tokio::runtime::Handle::current(),
        HostLimits::new(2, 2, 1).unwrap(),
    )
    .unwrap();
    let (scope, owner, finalizer, completion) = host.try_filesystem_scope().unwrap();
    let transaction = host
        .try_foreground_transaction(host.try_logical_io().unwrap(), &[&scope])
        .unwrap();
    let (accepted_tx, accepted_rx) = tokio::sync::oneshot::channel();
    let (release_tx, release_rx) = tokio::sync::oneshot::channel();
    let (worker_done_tx, worker_done_rx) = tokio::sync::oneshot::channel();

    let worker = std::thread::spawn(move || {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        runtime.block_on(async move {
            let waiter = tokio::spawn(async move {
                transaction
                    .run_operation(FilesystemOperation::FileStage, async move {
                        accepted_tx.send(()).unwrap();
                        release_rx.await.unwrap();
                        Ok::<_, io::Error>(())
                    })
                    .await
            });
            accepted_rx.await.unwrap();
            drop(waiter);
        });
        drop(runtime);
        worker_done_tx.send(()).unwrap();
    });

    worker_done_rx.await.unwrap();
    tokio::task::spawn_blocking(move || worker.join().unwrap())
        .await
        .unwrap();
    drop(owner);
    let mut pending = Box::pin(completion.wait());
    std::future::poll_fn(|context| {
        assert!(matches!(pending.as_mut().poll(context), Poll::Pending));
        Poll::Ready(())
    })
    .await;
    release_tx.send(()).unwrap();

    assert_eq!(finalizer.run().await, Ok(()));
    assert_eq!(completion.wait().await, Ok(()));
    host_owner.shutdown().await.unwrap();
}
