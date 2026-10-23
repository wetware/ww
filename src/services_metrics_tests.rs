//! Executor metrics follow real admission and task ownership boundaries.

use super::*;

const TEST_TIMEOUT: Duration = Duration::from_secs(10);

async fn settled(result: tokio::sync::oneshot::Receiver<Result<i32>>) -> Result<i32> {
    tokio::time::timeout(TEST_TIMEOUT, result)
        .await
        .expect("executor task must settle")
        .expect("executor task must report its outcome")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn immediate_completions_release_metrics_before_result_acknowledgement() {
    let (_shutdown_tx, shutdown_rx) = watch::channel(());
    let pool = ExecutorPool::new(1, shutdown_rx, test_filesystem_resources());
    let metrics = pool.metrics();
    for accepted in 1..=256 {
        let (result_tx, result_rx) = tokio::sync::oneshot::channel();
        assert!(pool
            .spawn(SpawnRequest {
                name: "immediate".into(),
                factory: Box::new(|_| Box::pin(async {})),
                result_tx: Some(result_tx),
            })
            .is_ok());
        assert_eq!(settled(result_rx).await.unwrap(), 0);
        assert_eq!(
            metrics.snapshot(),
            ExecutorSnapshot {
                accepted,
                completed: accepted,
                ..ExecutorSnapshot::default()
            }
        );
    }
    pool.shutdown_and_join().await;
    assert_eq!(metrics.snapshot().occupied, 0);
    assert_eq!(metrics.snapshot().completed, 256);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_admissions_preserve_all_lifetime_counts() {
    let (_shutdown_tx, shutdown_rx) = watch::channel(());
    let pool = Arc::new(ExecutorPool::new(
        4,
        shutdown_rx,
        test_filesystem_resources(),
    ));
    let metrics = pool.metrics();
    let mut producers = tokio::task::JoinSet::new();
    for _ in 0..8 {
        let pool = pool.clone();
        producers.spawn(async move {
            for _ in 0..64 {
                let (result_tx, result_rx) = tokio::sync::oneshot::channel();
                assert!(pool
                    .spawn(SpawnRequest {
                        name: "concurrent".into(),
                        factory: Box::new(|_| Box::pin(async {})),
                        result_tx: Some(result_tx),
                    })
                    .is_ok());
                assert_eq!(settled(result_rx).await.unwrap(), 0);
            }
        });
    }
    tokio::time::timeout(TEST_TIMEOUT, async {
        while let Some(result) = producers.join_next().await {
            result.unwrap();
        }
    })
    .await
    .expect("concurrent producers must finish");
    let pool = Arc::try_unwrap(pool)
        .ok()
        .expect("all producers released pool");
    pool.shutdown_and_join().await;
    assert_eq!(
        metrics.snapshot(),
        ExecutorSnapshot {
            accepted: 512,
            completed: 512,
            ..ExecutorSnapshot::default()
        }
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn admissions_racing_shutdown_retire_every_accepted_slot() {
    const PRODUCERS: usize = 4;
    const ATTEMPTS_PER_PRODUCER: usize = 128;

    let (shutdown_tx, shutdown_rx) = watch::channel(());
    let pool = Arc::new(ExecutorPool::new(
        2,
        shutdown_rx,
        test_filesystem_resources(),
    ));
    let metrics = pool.metrics();
    let (entered_tx, entered_rx) = tokio::sync::oneshot::channel();
    assert!(pool
        .spawn(SpawnRequest {
            name: "pending-before-shutdown".into(),
            factory: Box::new(move |_| {
                Box::pin(async move {
                    let _ = entered_tx.send(());
                    std::future::pending::<()>().await;
                })
            }),
            result_tx: None,
        })
        .is_ok());
    tokio::time::timeout(TEST_TIMEOUT, entered_rx)
        .await
        .expect("preexisting admitted task must start")
        .unwrap();

    let start = Arc::new(tokio::sync::Barrier::new(PRODUCERS + 1));
    let mut producers = tokio::task::JoinSet::new();
    for _ in 0..PRODUCERS {
        let pool = pool.clone();
        let start = start.clone();
        producers.spawn(async move {
            start.wait().await;
            for _ in 0..ATTEMPTS_PER_PRODUCER {
                let _ = pool.spawn(SpawnRequest {
                    name: "racing-shutdown".into(),
                    factory: Box::new(|_| Box::pin(std::future::pending())),
                    result_tx: None,
                });
                tokio::task::yield_now().await;
            }
        });
    }
    tokio::time::timeout(TEST_TIMEOUT, async {
        start.wait().await;
        drop(shutdown_tx);
        while let Some(result) = producers.join_next().await {
            result.unwrap();
        }
    })
    .await
    .expect("racing admission producers must finish");
    let pool = Arc::try_unwrap(pool)
        .ok()
        .expect("all producers released pool");
    tokio::time::timeout(TEST_TIMEOUT, pool.shutdown_and_join())
        .await
        .expect("shutdown must join every admitted task");

    let snapshot = metrics.snapshot();
    assert_eq!(snapshot.occupied, 0);
    assert!(snapshot.accepted >= 1);
    assert_eq!(snapshot.accepted, snapshot.cancelled);
    assert_eq!(
        snapshot.accepted + snapshot.full + snapshot.closed,
        1 + (PRODUCERS * ATTEMPTS_PER_PRODUCER) as u64
    );
    assert_eq!(snapshot.completed, 0);
    assert_eq!(snapshot.panicked, 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn full_admission_preserves_request_and_shutdown_releases_queued_slots() {
    let (shutdown_tx, shutdown_rx) = watch::channel(());
    let pool = ExecutorPool::new(1, shutdown_rx.clone(), test_filesystem_resources());
    let metrics = pool.metrics();
    let (entered_tx, entered_rx) = tokio::sync::oneshot::channel();
    let (release_tx, release_rx) = std::sync::mpsc::channel();
    let (result_tx, result_rx) = tokio::sync::oneshot::channel();
    assert!(pool
        .spawn(SpawnRequest {
            name: "blocked-worker".into(),
            factory: Box::new(move |_| {
                let _ = entered_tx.send(());
                release_rx
                    .recv_timeout(TEST_TIMEOUT)
                    .expect("test must release blocked worker");
                Box::pin(std::future::pending())
            }),
            result_tx: Some(result_tx),
        })
        .is_ok());
    tokio::time::timeout(TEST_TIMEOUT, entered_rx)
        .await
        .expect("worker must enter factory")
        .unwrap();

    for _ in 0..SPAWN_CHANNEL_DEPTH {
        assert!(pool
            .spawn(SpawnRequest {
                name: "queued".into(),
                factory: Box::new(|_| Box::pin(std::future::pending())),
                result_tx: None,
            })
            .is_ok());
    }
    let (returned_tx, returned_rx) = tokio::sync::oneshot::channel();
    let rejected = pool.spawn(SpawnRequest {
        name: "must-return-intact".into(),
        factory: Box::new(move |_| {
            Box::pin(async move {
                let _ = returned_tx.send(());
            })
        }),
        result_tx: None,
    });
    let rejected = rejected.expect_err("full worker must reject request");
    assert_eq!(rejected.name, "must-return-intact");
    (rejected.factory)(shutdown_rx).await;
    assert!(
        returned_rx.await.is_ok(),
        "returned factory retains its capture"
    );
    assert_eq!(
        metrics.snapshot(),
        ExecutorSnapshot {
            occupied: SPAWN_CHANNEL_DEPTH + 1,
            accepted: SPAWN_CHANNEL_DEPTH as u64 + 1,
            full: 1,
            ..ExecutorSnapshot::default()
        }
    );

    drop(shutdown_tx);
    release_tx.send(()).unwrap();
    tokio::time::timeout(TEST_TIMEOUT, pool.shutdown_and_join())
        .await
        .expect("shutdown must join admitted tasks");
    assert_eq!(
        settled(result_rx).await.unwrap_err().to_string(),
        "executor shutting down"
    );
    assert_eq!(
        metrics.snapshot(),
        ExecutorSnapshot {
            accepted: SPAWN_CHANNEL_DEPTH as u64 + 1,
            full: 1,
            cancelled: SPAWN_CHANNEL_DEPTH as u64 + 1,
            ..ExecutorSnapshot::default()
        }
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn closed_admission_does_not_occupy_a_slot() {
    let (shutdown_tx, shutdown_rx) = watch::channel(());
    let pool = ExecutorPool::new(1, shutdown_rx, test_filesystem_resources());
    let metrics = pool.metrics();
    drop(shutdown_tx);
    tokio::time::timeout(TEST_TIMEOUT, pool.senders[0].closed())
        .await
        .expect("worker must close admission");
    let request = pool
        .spawn(SpawnRequest {
            name: "closed".into(),
            factory: Box::new(|_| Box::pin(async {})),
            result_tx: None,
        })
        .expect_err("closed worker must reject request");
    assert_eq!(request.name, "closed");
    pool.shutdown_and_join().await;
    assert_eq!(
        metrics.snapshot(),
        ExecutorSnapshot {
            closed: 1,
            ..ExecutorSnapshot::default()
        }
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn factory_and_future_panics_release_slots_and_count_panics() {
    let (_shutdown_tx, shutdown_rx) = watch::channel(());
    let pool = ExecutorPool::new(1, shutdown_rx, test_filesystem_resources());
    let metrics = pool.metrics();
    let factories: [SpawnFactory; 2] = [
        Box::new(|_| panic!("factory panic")),
        Box::new(|_| Box::pin(async { panic!("future panic") })),
    ];
    for factory in factories {
        let (result_tx, result_rx) = tokio::sync::oneshot::channel();
        assert!(pool
            .spawn(SpawnRequest {
                name: "panic".into(),
                factory,
                result_tx: Some(result_tx),
            })
            .is_ok());
        assert_eq!(
            settled(result_rx).await.unwrap_err().to_string(),
            "cell panicked"
        );
        assert_eq!(metrics.snapshot().occupied, 0);
    }
    pool.shutdown_and_join().await;
    assert_eq!(
        metrics.snapshot(),
        ExecutorSnapshot {
            accepted: 2,
            panicked: 2,
            ..ExecutorSnapshot::default()
        }
    );
}

#[test]
fn retained_metrics_release_running_slot_on_pool_drop() {
    let (_shutdown_tx, shutdown_rx) = watch::channel(());
    let metrics = ExecutorMetrics::default();
    let pool = ExecutorPool::try_new_with_metrics(
        1,
        shutdown_rx,
        test_filesystem_resources(),
        metrics.clone(),
    )
    .unwrap();
    let (entered_tx, entered_rx) = std::sync::mpsc::channel();
    assert!(pool
        .spawn(SpawnRequest {
            name: "cancel-on-drop".into(),
            factory: Box::new(move |_| {
                Box::pin(async move {
                    entered_tx.send(()).unwrap();
                    std::future::pending::<()>().await;
                })
            }),
            result_tx: None,
        })
        .is_ok());
    entered_rx.recv_timeout(TEST_TIMEOUT).unwrap();
    assert_eq!(metrics.snapshot().occupied, 1);
    drop(pool);
    assert_eq!(
        metrics.snapshot(),
        ExecutorSnapshot {
            accepted: 1,
            cancelled: 1,
            ..ExecutorSnapshot::default()
        }
    );
}
