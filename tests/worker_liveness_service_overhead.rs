//! Opt-in representative P3 Cell service measurements on the real ExecutorPool.
//! Copy this source unchanged to the baseline. The external driver supplies AC
//! monitoring, identical-binary controls and prospective acceptance criteria.
//! No candidate-only APIs or FuelObserver run in timed work.
#![cfg(not(target_arch = "wasm32"))]

use std::future::{poll_fn, Future};
use std::sync::Arc;
use std::time::{Duration, Instant};

use cell::engine::compile_component;
use cell::{Builder, FilesystemOwnership, Program};
use tokio::sync::{oneshot, watch};
use wasmtime::component::Component;
use ww::services::{ExecutorPool, SpawnRequest};

const WARMUPS: usize = 3;
const SAMPLES: usize = 7;
const DEADLINE: Duration = Duration::from_secs(30);
const COMPUTE_SECONDS: u64 = 6;
const IO_NS: u64 = 5_500_000_000;

type Built = (Duration, cache::FsCompletion);

struct Ticket {
    start: Instant,
    built: oneshot::Receiver<Built>,
    result: oneshot::Receiver<anyhow::Result<i32>>,
}

#[derive(serde::Serialize)]
struct Completion {
    built_ns: u128,
    completed_ns: u128,
}

fn ownership(resources: Arc<cache::HostResources>) -> FilesystemOwnership {
    FilesystemOwnership::try_new(cache::CellResources::new(
        resources,
        cache::CellLimits::default(),
    ))
    .expect("Cell filesystem ownership")
}

fn admit(
    pool: &ExecutorPool,
    resources: Arc<cache::HostResources>,
    component: Arc<Component>,
    pending: Option<oneshot::Sender<(Instant, Instant, String)>>,
) -> Ticket {
    let start = Instant::now();
    let runtime = pool.runtime_engine();
    let (built_tx, built) = oneshot::channel();
    let (result_tx, result) = oneshot::channel();
    let grants_before = if pending.is_some() {
        runtime.fuel_metrics().snapshot().scheduled.grant
    } else {
        0
    };
    let request = SpawnRequest {
        name: "worker-liveness-service-measurement".into(),
        result_tx: Some(result_tx),
        factory: Box::new(move |_| {
            Box::pin(async move {
                let fuel = runtime.fuel_metrics();
                let (builder, _handles) = Builder::ordinary(
                    Program::Precompiled(component),
                    tokio::io::empty(),
                    tokio::io::sink(),
                    tokio::io::sink(),
                    ownership(resources),
                );
                let builder = builder.with_runtime_engine(runtime);
                let completion = builder.filesystem_completion();
                let proc = builder.build().await.expect("real P3 Store build");
                assert!(built_tx.send((start.elapsed(), completion)).is_ok());
                let mut run = Box::pin(proc.run());
                let mut pending = pending;
                let first_poll = pending.as_ref().map(|_| Instant::now());
                poll_fn(|cx| {
                    let result = run.as_mut().poll(cx);
                    if pending.is_some()
                        && result.is_pending()
                        && fuel.snapshot().scheduled.grant == grants_before + 9
                    {
                        // Pending Cells are admitted sequentially. Previously
                        // admitted Cells remain in their clock waits. Initial
                        // grant + eight host returns establishes this wait.
                        if let Some(tx) = pending.take() {
                            tx.send((
                                first_poll.expect("pending first poll"),
                                Instant::now(),
                                std::thread::current()
                                    .name()
                                    .expect("executor thread name")
                                    .to_owned(),
                            ))
                            .unwrap();
                        }
                    }
                    result
                })
                .await
                .expect("P3 command completed");
                assert!(pending.is_none(), "timer completed before pending evidence");
            })
        }),
    };
    assert!(pool.spawn(request).is_ok(), "bounded Cell admission");
    Ticket {
        start,
        built,
        result,
    }
}

async fn finish(ticket: Ticket) -> Completion {
    let (built, completion) = ticket.built.await.expect("built details");
    assert_eq!(ticket.result.await.expect("executor result").unwrap(), 0);
    completion.wait().await.expect("filesystem cleanup");
    Completion {
        built_ns: built.as_nanos(),
        completed_ns: ticket.start.elapsed().as_nanos(),
    }
}

async fn batch(
    pool: &ExecutorPool,
    resources: &Arc<cache::HostResources>,
    component: &Arc<Component>,
    operations: usize,
    width: usize,
) -> Vec<Completion> {
    let mut samples = Vec::with_capacity(operations);
    for _ in 0..operations / width {
        let tickets: Vec<_> = (0..width)
            .map(|_| admit(pool, resources.clone(), component.clone(), None))
            .collect();
        // Poll all result receivers together; sequential draining would measure
        // observer head-of-line delay as an individual Cell's latency.
        samples.extend(futures::future::join_all(tickets.into_iter().map(finish)).await);
    }
    samples
}

fn cpu_seconds() -> f64 {
    let mut usage = std::mem::MaybeUninit::<libc::rusage>::uninit();
    // SAFETY: successful getrusage initializes this valid rusage pointer.
    assert_eq!(
        unsafe { libc::getrusage(libc::RUSAGE_SELF, usage.as_mut_ptr()) },
        0
    );
    let usage = unsafe { usage.assume_init() };
    usage.ru_utime.tv_sec as f64
        + usage.ru_utime.tv_usec as f64 / 1e6
        + usage.ru_stime.tv_sec as f64
        + usage.ru_stime.tv_usec as f64 / 1e6
}

fn emit(value: serde_json::Value) {
    println!("{value}");
}

fn report(
    case: &str,
    workers: usize,
    sample: usize,
    start: Instant,
    cpu: f64,
    rows: Vec<Completion>,
) {
    emit(serde_json::json!({
        "case": case, "workers": workers, "sample": sample,
        "elapsed_ns": start.elapsed().as_nanos(),
        "cpu_seconds": cpu_seconds() - cpu,
        "operations": rows.len(), "cells": rows,
    }));
}

fn compile(pool: &ExecutorPool, source: &str) -> Arc<Component> {
    Arc::new(
        compile_component(
            &pool.runtime_engine().engine(),
            &wat::parse_str(source).expect("measurement WAT"),
        )
        .expect("compile P3 fixture outside measurement"),
    )
}

async fn shut_down(pool: ExecutorPool, workers: usize, case: &str, sample: usize) {
    let start = Instant::now();
    tokio::time::timeout(DEADLINE, pool.shutdown_and_join())
        .await
        .expect("pool shutdown deadline");
    emit(serde_json::json!({
        "case": case, "workers": workers, "sample": sample,
        "elapsed_ns": start.elapsed().as_nanos(),
    }));
}

async fn measure(workers: usize, resources: Arc<cache::HostResources>) {
    for sample in 0..WARMUPS + SAMPLES {
        let (_shutdown, rx) = watch::channel(());
        let start = Instant::now();
        let pool = ExecutorPool::new(workers, rx, resources.clone());
        let elapsed = start.elapsed();
        if sample >= WARMUPS {
            emit(serde_json::json!({
                "case": "pool_creation", "workers": workers,
                "sample": sample - WARMUPS, "elapsed_ns": elapsed.as_nanos(),
            }));
            shut_down(pool, workers, "idle_shutdown", sample - WARMUPS).await;
        } else {
            pool.shutdown_and_join().await;
        }
    }

    let (_shutdown, rx) = watch::channel(());
    let pool = ExecutorPool::new(workers, rx, resources.clone());
    let immediate = compile(&pool, &fixture(false));
    let compute = compile(&pool, &fixture(true));
    // Untimed proof that the actual finite compute fixture crosses grants.
    let fuel = pool.runtime_engine().fuel_metrics();
    let before = fuel.snapshot().scheduled.grant;
    batch(&pool, &resources, &compute, workers, workers).await;
    let grants = fuel.snapshot().scheduled.grant - before;
    assert!(
        grants > workers as u64,
        "finite compute must cross fuel grants"
    );
    emit(serde_json::json!({"case": "preflight", "workers": workers, "compute_grants": grants}));

    for (case, count, width) in [("cell_startup", 1, 1), ("short_batch", 256, 32)] {
        for sample in 0..WARMUPS + SAMPLES {
            let cpu = cpu_seconds();
            let start = Instant::now();
            let rows =
                tokio::time::timeout(DEADLINE, batch(&pool, &resources, &immediate, count, width))
                    .await
                    .expect("real Cell batch deadline");
            if sample >= WARMUPS {
                report(case, workers, sample - WARMUPS, start, cpu, rows);
            }
        }
    }

    let cpu = cpu_seconds();
    let start = Instant::now();
    let mut rows = Vec::new();
    while start.elapsed() < Duration::from_secs(COMPUTE_SECONDS) {
        rows.extend(
            tokio::time::timeout(
                DEADLINE,
                batch(&pool, &resources, &compute, workers, workers),
            )
            .await
            .expect("cooperative batch deadline"),
        );
    }
    report("cooperative", workers, 0, start, cpu, rows);
    shut_down(pool, workers, "workload_shutdown", 0).await;

    // A new pool starts with zero assignment counts. Completed short batches
    // can leave stale scheduler counts; do not mistake admissions for placement.
    let (_shutdown, rx) = watch::channel(());
    let pool = ExecutorPool::new(workers, rx, resources.clone());
    let io = compile(
        &pool,
        &include_str!("fixtures/fuel-publication-component.wat")
            .replace("60000000000", &IO_NS.to_string()),
    );
    let mut tickets = Vec::new();
    let mut ready_times = Vec::new();
    for _ in 0..workers {
        let (ready_tx, ready_rx) = oneshot::channel();
        tickets.push(admit(&pool, resources.clone(), io.clone(), Some(ready_tx)));
        ready_times.push(
            tokio::time::timeout(DEADLINE, ready_rx)
                .await
                .expect("clock pending deadline")
                .expect("real P3 wait pending"),
        );
    }
    let identities: std::collections::BTreeSet<_> = ready_times
        .iter()
        .map(|(_, _, name)| name.clone())
        .collect();
    assert_eq!(identities.len(), workers, "one pending Cell per worker");
    let pending_fuel = pool.runtime_engine().fuel_metrics().snapshot();
    let cpu = cpu_seconds();
    let pending_start = Instant::now();
    tokio::time::sleep(Duration::from_secs(5)).await;
    let pending_elapsed = pending_start.elapsed();
    let pending_cpu = cpu_seconds() - cpu;
    assert_eq!(
        pool.runtime_engine().fuel_metrics().snapshot(),
        pending_fuel,
        "ordinary P3 clock waits remain pending without fuel work"
    );
    for ticket in &mut tickets {
        assert!(matches!(
            ticket.result.try_recv(),
            Err(oneshot::error::TryRecvError::Empty)
        ));
    }
    let done = futures::future::join_all(tickets.into_iter().zip(ready_times).map(
        |(ticket, (first_poll, ready, _))| async move {
            let completion = finish(ticket).await;
            // Timer installation occurs after the first Proc::run poll begins
            // and before its established Pending observation. Retain both bounds;
            // the acceptance driver gates the conservative upper bound.
            let now = Instant::now();
            let upper = now
                .duration_since(first_poll)
                .saturating_sub(Duration::from_nanos(IO_NS));
            let lower = now
                .duration_since(ready)
                .saturating_sub(Duration::from_nanos(IO_NS));
            (completion, upper.as_nanos(), lower.as_nanos())
        },
    ))
    .await;
    emit(serde_json::json!({
        "case": "pending_io", "workers": workers,
        "elapsed_ns": pending_elapsed.as_nanos(), "cpu_seconds": pending_cpu,
        "worker_threads": identities,
        "lateness_ns": done.iter().map(|(_, upper, _)| upper).collect::<Vec<_>>(),
        "lateness_lower_ns": done.iter().map(|(_, _, lower)| lower).collect::<Vec<_>>(),
        "cells": done.iter().map(|(cell, _, _)| cell).collect::<Vec<_>>(),
    }));
    shut_down(pool, workers, "io_shutdown", 0).await;

    for sample in 0..WARMUPS + SAMPLES {
        let (_shutdown, rx) = watch::channel(());
        let pool = ExecutorPool::new(workers, rx, resources.clone());
        let io = compile(
            &pool,
            include_str!("fixtures/fuel-publication-component.wat"),
        );
        let mut tickets = Vec::new();
        let mut identities = std::collections::BTreeSet::new();
        for _ in 0..workers {
            let (ready_tx, ready_rx) = oneshot::channel();
            tickets.push(admit(&pool, resources.clone(), io.clone(), Some(ready_tx)));
            let (_, _, name) = tokio::time::timeout(DEADLINE, ready_rx)
                .await
                .expect("shutdown pending deadline")
                .expect("shutdown guest pending");
            assert!(identities.insert(name), "one cancellation Cell per worker");
        }
        let start = Instant::now();
        tokio::time::timeout(DEADLINE, pool.shutdown_and_join())
            .await
            .expect("pending shutdown deadline");
        for ticket in tickets {
            let (_, completion) = ticket.built.await.unwrap();
            assert!(ticket.result.await.expect("cancelled result").is_err());
            completion
                .wait()
                .await
                .expect("cancelled filesystem cleanup");
        }
        if sample >= WARMUPS {
            emit(serde_json::json!({
                "case": "pending_shutdown", "workers": workers,
                "sample": sample - WARMUPS, "elapsed_ns": start.elapsed().as_nanos(),
                "worker_threads": identities,
            }));
        }
    }
}

fn fixture(compute: bool) -> String {
    let body = if compute {
        "i32.const 4000000 local.set $left (loop $loop local.get $left i32.const 1 i32.sub local.tee $left br_if $loop)"
    } else {
        ""
    };
    format!(
        r#"(component
          (core func $done (canon task.return (result (result))))
          (core module $guest
            (import "" "done" (func $done (param i32)))
            (func (export "run") (local $left i32)
              {body}
              i32.const 0 call $done))
          (core instance $instance (instantiate $guest
            (with "" (instance (export "done" (func $done))))))
          (func $run async (result (result))
            (canon lift (core func $instance "run") async))
          (instance (export (interface "wasi:cli/run@0.3.0"))
            (export "run" (func $run))))"#
    )
}

#[test]
#[ignore = "bounded opt-in real Cell measurement; run through the external AC-monitored driver"]
fn measure_worker_liveness_service() {
    assert_eq!(std::env::var("WW_WORKER_SERVICE_BENCH").as_deref(), Ok("1"));
    if cfg!(debug_assertions) {
        panic!("release measurements only");
    }
    assert!(std::env::var_os(cell::engine::CWASM_DIR_ENV).is_none());
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(async {
            let (owner, resources) = cache::HostFsOwner::start(
                tokio::runtime::Handle::current(),
                cache::HostLimits::new(4_096, 1_025, 1_024).unwrap(),
            )
            .unwrap();
            emit(serde_json::json!({
                "case": "metadata", "warmups": WARMUPS, "samples": SAMPLES,
                "compute_seconds": COMPUTE_SECONDS, "io_ns": IO_NS,
                "entry": "ExecutorPool::spawn -> Builder::ordinary -> Proc::run -> FsCompletion",
            }));
            for workers in [1, 4] {
                measure(workers, resources.clone()).await;
            }
            tokio::time::timeout(DEADLINE, owner.shutdown())
                .await
                .expect("Host filesystem shutdown deadline")
                .expect("Host filesystem shutdown");
        });
}
