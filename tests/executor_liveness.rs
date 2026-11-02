//! Real P3 waits and cooperative execution must leave the executor responsive.
//! Each case has a watchdog in a separate process, independent of worker progress.
#![cfg(not(target_arch = "wasm32"))]

mod support;

use std::future::{poll_fn, Future};
use std::process::Command;
use std::sync::Arc;
use std::task::Poll;
use std::time::{Duration, Instant};

use cell::engine::{compile_component, runtime_engine};
use cell::proc::{FuelEstimator, FuelObserver};
use cell::{Builder, Program};
use tokio::sync::{oneshot, watch};
use ww::services::{
    ExecutorMetrics, ExecutorPool, SpawnRequest, WorkerState, WORKER_HEARTBEAT_INTERVAL,
    WORKER_STALL_THRESHOLD,
};

const PROBE: &str = "WW_EXECUTOR_LIVENESS_PROBE";
const DEADLINE: Duration = Duration::from_secs(10);

#[test]
fn cooperative_p3_compute_keeps_worker_responsive() {
    run_probe("compute");
}

#[test]
fn pending_p3_io_keeps_worker_responsive() {
    run_probe("io");
}

#[test]
fn suspended_p3_fuel_keeps_worker_responsive_and_resumes() {
    run_probe("fuel");
}

fn run_probe(mode: &str) {
    let mut child = Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "executor_liveness_probe",
            "--ignored",
            "--nocapture",
        ])
        .env(PROBE, mode)
        .spawn()
        .expect("isolated executor liveness probe");
    let deadline = Instant::now() + Duration::from_secs(45);
    loop {
        if let Some(status) = child.try_wait().unwrap() {
            assert!(status.success(), "{mode} liveness probe failed: {status}");
            return;
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            panic!("{mode} liveness probe exceeded independent watchdog deadline");
        }
        // Poll process completion; this delay establishes no guest ordering.
        std::thread::sleep(Duration::from_millis(10));
    }
}

#[derive(Clone, Copy, Debug)]
enum Work {
    Compute,
    Io,
    Fuel,
}

#[test]
#[ignore = "subprocess entry point with an independent parent watchdog"]
fn executor_liveness_probe() {
    let work = match std::env::var(PROBE).as_deref() {
        Ok("compute") => Work::Compute,
        Ok("io") => Work::Io,
        Ok("fuel") => Work::Fuel,
        _ => return,
    };
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(exercise(work));
}

async fn exercise(work: Work) {
    let (_shutdown_tx, shutdown_rx) = watch::channel(());
    let pool = ExecutorPool::new(1, shutdown_rx, support::filesystem_resources());
    let metrics = pool.metrics();
    // Compute shares the production worker-0 epoch publisher. For the two
    // pending cases, retain an unticked publisher to hold the exact boundary.
    let (runtime, mut controlled_publisher) = match work {
        Work::Compute => (pool.runtime_engine(), None),
        Work::Io | Work::Fuel => {
            let (runtime, publisher) = runtime_engine().unwrap();
            (runtime, Some(publisher))
        }
    };
    let source = match work {
        Work::Io => include_str!("fixtures/fuel-publication-component.wat"),
        Work::Compute | Work::Fuel => include_str!("fixtures/spinning-component.wat"),
    };
    let component =
        Arc::new(compile_component(&runtime.engine(), &wat::parse_str(source).unwrap()).unwrap());
    let fuel_metrics = runtime.fuel_metrics();
    let observer = FuelObserver::default();
    let task_observer = observer.clone();
    let task_fuel_metrics = fuel_metrics.clone();
    let (ready_tx, ready_rx) = oneshot::channel();
    let (stop_tx, stop_rx) = oneshot::channel();
    let (result_tx, result_rx) = oneshot::channel();
    pool.spawn(SpawnRequest {
        name: format!("liveness-{work:?}"),
        factory: Box::new(move |_| {
            Box::pin(async move {
                let (builder, _handles) = Builder::ordinary(
                    Program::Precompiled(component),
                    tokio::io::empty(),
                    tokio::io::sink(),
                    tokio::io::sink(),
                    support::filesystem_ownership(),
                );
                let mut builder = builder.with_runtime_engine(runtime);
                if matches!(work, Work::Fuel) {
                    builder = builder
                        .with_fuel_estimator(FuelEstimator::new_oneshot(25_000, 10_000, 0))
                        .with_fuel_observer(task_observer);
                }
                let completion = builder.filesystem_completion();
                let proc = builder.build().await.expect("build real P3 process");
                {
                    let mut run = Box::pin(proc.run());
                    let mut ready_tx = Some(ready_tx);
                    let pending = poll_fn(|cx| {
                        if let Poll::Ready(result) = run.as_mut().poll(cx) {
                            panic!("infinite or gated P3 guest returned: {result:?}");
                        }
                        let snapshot = task_fuel_metrics.snapshot();
                        let ready = match work {
                            Work::Compute => snapshot.scheduled.grant >= 2,
                            // Initial grant plus the fixture's eight marked
                            // host returns. This Pending is the P3 clock wait.
                            Work::Io => snapshot.scheduled.grant == 9,
                            Work::Fuel => snapshot.oneshot.suspend == 1,
                        };
                        if ready {
                            if let Some(tx) = ready_tx.take() {
                                tx.send(snapshot).expect("observer awaiting guest state");
                            }
                        }
                        Poll::<()>::Pending
                    });
                    tokio::select! {
                        biased;
                        _ = stop_rx => {},
                        () = pending => unreachable!(),
                    }
                }
                completion.wait().await.expect("guest filesystem cleanup");
            })
        }),
        result_tx: Some(result_tx),
    })
    .unwrap_or_else(|_| panic!("admit real P3 guest"));

    let ready = tokio::time::timeout(DEADLINE, ready_rx)
        .await
        .expect("guest state deadline")
        .expect("guest established pending state");
    observe_responsive_worker(&metrics).await;
    let after = fuel_metrics.snapshot();
    match work {
        Work::Compute => {
            assert!(after.scheduled.grant > ready.scheduled.grant);
        }
        Work::Io => {
            assert_eq!(after, ready, "P3 wait remained pending without fuel work");
        }
        Work::Fuel => {
            assert_eq!(after, ready, "fuel suspension consumed no new authority");
            assert_eq!(observer.suspension_count(), 1);
            controlled_publisher.as_mut().unwrap().tick();
            observer.wait_for_suspension(2).await;
            assert_eq!(observer.suspension_count(), 2);
            tokio::time::timeout(DEADLINE, async {
                while fuel_metrics.snapshot().oneshot.suspend < 2 {
                    tokio::task::yield_now().await;
                }
            })
            .await
            .expect("second suspension applied after epoch publication");
            assert_eq!(fuel_metrics.snapshot().oneshot.suspend, 2);
        }
    }
    stop_tx.send(()).unwrap();
    tokio::time::timeout(DEADLINE, result_rx)
        .await
        .expect("guest cancellation deadline")
        .expect("executor result")
        .expect("guest factory completed");
    pool.shutdown_and_join().await;
    assert_eq!(metrics.worker_liveness()[0].state, WorkerState::Stopped);
}

async fn observe_responsive_worker(metrics: &ExecutorMetrics) {
    // The guest has already reached its asserted state. Observe for longer
    // than the production stall threshold, with no guessed ordering delay.
    let observation = WORKER_STALL_THRESHOLD + WORKER_HEARTBEAT_INTERVAL * 2;
    let mut samples = tokio::time::interval(WORKER_HEARTBEAT_INTERVAL / 4);
    samples.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    tokio::time::timeout(DEADLINE, async {
        loop {
            samples.tick().await;
            if metrics.worker_liveness()[0].state == WorkerState::Running {
                break;
            }
        }
    })
    .await
    .expect("first heartbeat deadline");
    let deadline = Instant::now() + observation;
    let mut previous_age = None;
    let mut renewals = 0;
    loop {
        samples.tick().await;
        let workers = metrics.worker_liveness();
        assert_eq!(workers.len(), 1);
        let worker = &workers[0];
        assert_eq!(worker.worker_id, 0);
        assert_eq!(worker.state, WorkerState::Running);
        assert!(
            !worker.stalled,
            "responsive P3 case marked stalled: {worker:?}"
        );
        let age = worker.progress_age.expect("confirmed executor progress");
        if previous_age.is_some_and(|previous| age < previous) {
            renewals += 1;
        }
        previous_age = Some(age);
        if Instant::now() >= deadline {
            break;
        }
    }
    assert!(renewals >= 3, "worker must publish repeated fresh progress");
}
