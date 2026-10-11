use std::future::Future;
use std::io;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::task::Poll;

use ww_cache::{FilesystemOperation, FsScope};

#[tokio::test]
async fn cancelled_waiter_does_not_release_actual_filesystem_work() {
    let (scope, owner, finalizer, completion) = FsScope::new();
    let (entered_tx, entered_rx) = tokio::sync::oneshot::channel();
    let (release_tx, release_rx) = tokio::sync::oneshot::channel();

    let waiter = tokio::spawn({
        let scope = scope.clone();
        async move {
            scope
                .run_blocking(FilesystemOperation::FileStage, move || {
                    entered_tx.send(()).unwrap();
                    release_rx.blocking_recv().unwrap();
                    42_u8
                })
                .await
        }
    });

    entered_rx.await.unwrap();
    waiter.abort();
    assert!(waiter.await.unwrap_err().is_cancelled());
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
}

#[tokio::test]
async fn blocking_panic_after_waiter_cancellation_fails_completion() {
    let (scope, owner, finalizer, completion) = FsScope::new();
    let (entered_tx, entered_rx) = tokio::sync::oneshot::channel();
    let (release_tx, release_rx) = tokio::sync::oneshot::channel();

    let waiter = tokio::spawn(async move {
        scope
            .run_blocking(FilesystemOperation::DirectoryBuild, move || {
                entered_tx.send(()).unwrap();
                release_rx.blocking_recv().unwrap();
                panic!("injected blocking panic");
            })
            .await
    });
    entered_rx.await.unwrap();
    waiter.abort();
    assert!(waiter.await.unwrap_err().is_cancelled());
    drop(owner);
    let finalizer_task = tokio::spawn(finalizer.run());
    release_tx.send(()).unwrap();

    let error = completion.wait().await.unwrap_err();
    assert!(error.to_string().contains("DirectoryBuild panicked"));
    assert_eq!(finalizer_task.await.unwrap().unwrap_err(), error);
}

#[tokio::test]
async fn cleanup_panic_after_finalizer_cancellation_fails_and_continues_cleanup() {
    let (scope, owner, finalizer, completion) = FsScope::new();
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
    let (scope, owner, finalizer, completion) = FsScope::new();
    let result = scope
        .run_operation(FilesystemOperation::ListingRead, async {
            Err::<(), _>(io::Error::other("ordinary read failure"))
        })
        .await
        .unwrap();
    assert_eq!(result.unwrap_err().kind(), io::ErrorKind::Other);

    drop(owner);
    assert_eq!(finalizer.run().await, Ok(()));
    assert_eq!(completion.wait().await, Ok(()));
}

#[tokio::test]
async fn private_cleanup_failure_is_repeatably_observable() {
    let (scope, owner, finalizer, completion) = FsScope::new();
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
    let (_scope, owner, finalizer, completion) = FsScope::new();
    drop(owner);
    drop(finalizer);

    assert_eq!(completion.wait().await, Ok(()));
}

#[tokio::test]
async fn dropped_finalizer_still_runs_private_cleanup_off_the_owner_thread() {
    let (scope, owner, finalizer, completion) = FsScope::new();
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
    let (scope, owner, finalizer, _completion) = FsScope::new();
    drop(owner);

    let error = scope
        .run_blocking(FilesystemOperation::DescriptorOpen, || ())
        .await
        .unwrap_err();
    assert!(error.to_string().contains("closed"));
    let cleanup_error = scope
        .own_cleanup(FilesystemOperation::ProcessCleanup, || Ok(()))
        .unwrap_err();
    assert!(cleanup_error.to_string().contains("closed"));
    assert_eq!(finalizer.run().await, Ok(()));
}

#[tokio::test]
async fn admitted_operation_can_submit_internal_stages_after_owner_close() {
    let (scope, owner, finalizer, completion) = FsScope::new();
    let (entered_tx, entered_rx) = tokio::sync::oneshot::channel();
    let (release_tx, release_rx) = tokio::sync::oneshot::channel();
    let cleaned = Arc::new(AtomicBool::new(false));
    let operation_scope = scope.clone();
    let operation_cleaned = Arc::clone(&cleaned);
    let operation = tokio::spawn(async move {
        let inner_scope = operation_scope.clone();
        operation_scope
            .run_operation(FilesystemOperation::FileStage, async move {
                entered_tx.send(()).unwrap();
                release_rx.await.unwrap();
                let cleanup_scope = inner_scope.clone();
                inner_scope
                    .run_blocking(FilesystemOperation::FileStage, move || {
                        cleanup_scope
                            .own_cleanup(FilesystemOperation::ProcessCleanup, move || {
                                operation_cleaned.store(true, Ordering::SeqCst);
                                Ok(())
                            })
                            .unwrap();
                        7_u8
                    })
                    .await
            })
            .await
    });

    entered_rx.await.unwrap();
    drop(owner);
    let finalizer_task = tokio::spawn(finalizer.run());
    release_tx.send(()).unwrap();

    assert_eq!(operation.await.unwrap().unwrap().unwrap(), 7);
    assert_eq!(finalizer_task.await.unwrap(), Ok(()));
    assert_eq!(completion.wait().await, Ok(()));
    assert!(cleaned.load(Ordering::SeqCst));
}
