//! Opt-in ExecutorPool measurements. This file uses only APIs present before
//! executor instrumentation, so copy it unchanged into the baseline checkout.
//!
//! Run both checkouts with the same Rust compiler and release profile:
//! WW_EXECUTOR_BENCH_LABEL=baseline cargo test --release --test executor_overhead \
//!     measure_executor_operations -- --ignored --exact --nocapture
//!
//! Alternate baseline and instrumented runs on an otherwise idle machine.
//! JSON lines contain raw samples; there are no machine-dependent thresholds.
//! These synthetic futures measure executor bookkeeping, not Wasmtime guest
//! execution or fuel hooks. Completion measurements include request allocation,
//! channels, and cross-thread scheduling. Full-queue measurements reuse a rejected
//! request. Shutdown measurements exclude pool creation and task startup.
#![cfg(not(target_arch = "wasm32"))]

use std::hint::black_box;
use std::process::Command;
use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::sync::{mpsc, oneshot, watch};
use ww::services::{ExecutorPool, SpawnRequest};

const PROBE: &str = "WW_EXECUTOR_OVERHEAD_PROBE";
const WARMUPS: usize = 2;
const SAMPLES: usize = 7;
const DEADLINE: Duration = Duration::from_secs(30);
const BATCH_WIDTH: usize = 32;

#[test]
#[ignore = "opt-in ExecutorPool measurements; no machine-dependent thresholds"]
fn measure_executor_operations() {
    let mut child = Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "executor_overhead_probe",
            "--ignored",
            "--nocapture",
            "--test-threads=1",
        ])
        .env(PROBE, "1")
        .spawn()
        .expect("start isolated executor benchmark");
    let deadline = Instant::now() + Duration::from_secs(120);
    loop {
        if let Some(status) = child.try_wait().unwrap() {
            assert!(status.success(), "executor benchmark failed: {status}");
            break;
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            panic!("executor benchmark exceeded 120 seconds");
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

#[test]
#[ignore = "subprocess entry point for measure_executor_operations"]
fn executor_overhead_probe() {
    if std::env::var(PROBE).as_deref() != Ok("1") {
        return;
    }
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    runtime.block_on(async {
        let (owner, resources) = cache::HostFsOwner::start(
            tokio::runtime::Handle::current(),
            cache::HostLimits::new(4_096, 1_025, 1_024).unwrap(),
        )
        .unwrap();
        println!(
            "{}",
            serde_json::json!({
                "benchmark": "executor_metadata",
                "label": label(),
                "debug_assertions": cfg!(debug_assertions),
                "warmups": WARMUPS,
                "samples": SAMPLES,
                "batch_width": BATCH_WIDTH,
            })
        );
        for workers in [1, 4] {
            measure_completions(workers, Arc::clone(&resources)).await;
            measure_cancellation(workers, Arc::clone(&resources)).await;
        }
        measure_full_queue(resources).await;
        tokio::time::timeout(DEADLINE, owner.shutdown())
            .await
            .expect("filesystem shutdown deadline")
            .expect("filesystem shutdown");
    });
}

fn label() -> String {
    std::env::var("WW_EXECUTOR_BENCH_LABEL").unwrap_or_else(|_| "unspecified".into())
}

fn sample(case: &str, workers: usize, iteration: usize, operations: usize, elapsed: Duration) {
    if iteration < WARMUPS {
        return;
    }
    println!(
        "{}",
        serde_json::json!({
            "benchmark": "executor",
            "label": label(),
            "case": case,
            "workers": workers,
            "sample": iteration - WARMUPS,
            "operations": operations,
            "elapsed_ns": elapsed.as_nanos(),
            "ns_per_operation": elapsed.as_nanos() as f64 / operations as f64,
            "operations_per_second": operations as f64 / elapsed.as_secs_f64(),
        })
    );
}

#[derive(Clone, Copy)]
enum Work {
    Immediate,
    Mixed,
}

fn completion_request(work: Work) -> (SpawnRequest, oneshot::Receiver<anyhow::Result<i32>>) {
    let (result_tx, result_rx) = oneshot::channel();
    (
        SpawnRequest {
            name: "executor-benchmark".into(),
            factory: Box::new(move |_| {
                Box::pin(async move {
                    if let Work::Mixed = work {
                        let mut value = black_box(1_u64);
                        for _ in 0..4 {
                            for _ in 0..64 {
                                value = black_box(value.rotate_left(7).wrapping_mul(31));
                            }
                            tokio::task::yield_now().await;
                        }
                        black_box(value);
                    }
                })
            }),
            result_tx: Some(result_tx),
        },
        result_rx,
    )
}

async fn complete(pool: &ExecutorPool, work: Work, operations: usize, width: usize) {
    let mut results = Vec::with_capacity(width);
    for _ in 0..operations / width {
        for _ in 0..width {
            let (request, result) = completion_request(work);
            assert!(pool.spawn(request).is_ok(), "bounded batch admitted");
            results.push(result);
        }
        for result in results.drain(..) {
            assert_eq!(result.await.expect("completion channel").unwrap(), 0);
        }
    }
}

async fn measure_completions(workers: usize, resources: Arc<cache::HostResources>) {
    let (_shutdown, shutdown_rx) = watch::channel(());
    let pool = ExecutorPool::new(workers, shutdown_rx, resources);
    for (case, work, operations, width) in [
        ("immediate_batch", Work::Immediate, 4_096, BATCH_WIDTH),
        ("mixed_batch", Work::Mixed, 2_048, BATCH_WIDTH),
        ("immediate_serial", Work::Immediate, 512, 1),
    ] {
        for iteration in 0..WARMUPS + SAMPLES {
            let start = Instant::now();
            tokio::time::timeout(DEADLINE, complete(&pool, work, operations, width))
                .await
                .expect("completion batch deadline");
            sample(case, workers, iteration, operations, start.elapsed());
        }
    }
    tokio::time::timeout(DEADLINE, pool.shutdown_and_join())
        .await
        .expect("completion pool shutdown deadline");
}

async fn measure_cancellation(workers: usize, resources: Arc<cache::HostResources>) {
    for iteration in 0..WARMUPS + SAMPLES {
        let (shutdown, shutdown_rx) = watch::channel(());
        let pool = ExecutorPool::new(workers, shutdown_rx, Arc::clone(&resources));
        let (started_tx, mut started_rx) = mpsc::unbounded_channel();
        let mut results = Vec::with_capacity(BATCH_WIDTH);
        for _ in 0..BATCH_WIDTH {
            let started = started_tx.clone();
            let (result_tx, result_rx) = oneshot::channel();
            assert!(pool
                .spawn(SpawnRequest {
                    name: "executor-benchmark-cancel".into(),
                    factory: Box::new(move |_| {
                        Box::pin(async move {
                            started.send(()).unwrap();
                            std::future::pending::<()>().await;
                        })
                    }),
                    result_tx: Some(result_tx),
                })
                .is_ok());
            results.push(result_rx);
        }
        tokio::time::timeout(DEADLINE, async {
            for _ in 0..BATCH_WIDTH {
                started_rx.recv().await.expect("pending Cell started");
            }
        })
        .await
        .expect("pending Cell startup deadline");
        let start = Instant::now();
        shutdown.send(()).unwrap();
        tokio::time::timeout(DEADLINE, pool.shutdown_and_join())
            .await
            .expect("cancellation shutdown deadline");
        let elapsed = start.elapsed();
        for result in results {
            assert!(result.await.expect("cancellation channel").is_err());
        }
        sample("cancel_and_join", workers, iteration, BATCH_WIDTH, elapsed);
    }
}

async fn measure_full_queue(resources: Arc<cache::HostResources>) {
    let (_shutdown, shutdown_rx) = watch::channel(());
    let pool = ExecutorPool::new(1, shutdown_rx, resources);
    let (started_tx, started_rx) = oneshot::channel();
    let (release_tx, release_rx) = std::sync::mpsc::channel();
    let (result_tx, result_rx) = oneshot::channel();
    assert!(pool
        .spawn(SpawnRequest {
            name: "executor-benchmark-blocker".into(),
            factory: Box::new(move |_| {
                // Block the sole worker while filling its bounded admission
                // queue. The deadline also bounds cleanup after a failed probe.
                started_tx.send(()).unwrap();
                release_rx.recv_timeout(DEADLINE).unwrap();
                Box::pin(async {})
            }),
            result_tx: Some(result_tx),
        })
        .is_ok());
    tokio::time::timeout(DEADLINE, started_rx)
        .await
        .expect("blocking factory startup deadline")
        .expect("blocking factory started");

    let mut accepted = Vec::new();
    let mut rejected = None;
    // Discover the bounded queue capacity without depending on its private
    // constant. The loop bound detects an accidentally unbounded admission path.
    for _ in 0..1_024 {
        let (request, result) = completion_request(Work::Immediate);
        match pool.spawn(request) {
            Ok(()) => accepted.push(result),
            Err(request) => {
                rejected = Some(request);
                break;
            }
        }
    }
    let mut rejected = rejected.expect("admission queue must become full");
    for iteration in 0..WARMUPS + SAMPLES {
        let operations = 16_384;
        let start = Instant::now();
        for _ in 0..operations {
            rejected = match pool.spawn(rejected) {
                Err(request) => request,
                Ok(()) => panic!("blocked worker unexpectedly admitted another request"),
            };
        }
        sample(
            "full_queue_rejection",
            1,
            iteration,
            operations,
            start.elapsed(),
        );
    }
    drop(rejected);
    release_tx.send(()).unwrap();
    tokio::time::timeout(DEADLINE, async {
        assert_eq!(result_rx.await.expect("blocker completion").unwrap(), 0);
        for result in accepted {
            assert_eq!(result.await.expect("queued completion").unwrap(), 0);
        }
    })
    .await
    .expect("full queue drain deadline");
    tokio::time::timeout(DEADLINE, pool.shutdown_and_join())
        .await
        .expect("full queue pool shutdown deadline");
}
