//! Real Cell incident and descriptive cost probes for the local observability stack.
//! Run each ignored probe in its own process: lifecycle collectors are global.
#![cfg(not(target_arch = "wasm32"))]

mod support;

use std::{
    net::TcpListener,
    sync::Arc,
    time::{Duration, Instant},
};

use cell::{
    engine::{runtime_engine_with_fuel_metrics, wasmtime_cache_metrics, RuntimeEngine},
    metrics::FuelMetrics,
    proc::FuelEstimator,
    Builder, Program,
};
use tokio::sync::{oneshot, watch};
use ww::{
    metrics::{AdminService, RuntimeStatus, VersionInfo},
    services::{ExecutorMetrics, ExecutorPool, Service, SpawnRequest},
};

fn command(start: &str) -> Vec<u8> {
    wat::parse_str(format!(
        r#"(component
      (core func $task-return (canon task.return (result (result))))
      (core module $command
        (import "" "task-return" (func $task-return (param i32)))
        {start}
        (func (export "run") i32.const 0 call $task-return))
      (core instance $instance (instantiate $command
        (with "" (instance (export "task-return" (func $task-return))))))
      (func $run async (result (result)) (canon lift (core func $instance "run") async))
      (instance (export (interface "wasi:cli/run@0.3.0")) (export "run" (func $run))))"#
    ))
    .unwrap()
}

async fn execute(
    pool: &ExecutorPool,
    runtime: RuntimeEngine,
    bytes: Vec<u8>,
    stall: bool,
) -> anyhow::Result<()> {
    let (result_tx, result_rx) = oneshot::channel();
    let (retired_tx, retired_rx) = oneshot::channel();
    let request = SpawnRequest {
        name: "observability-fixture".into(),
        result_tx: Some(retired_tx),
        factory: Box::new(move |_| {
            Box::pin(async move {
                let (builder, _handles) = Builder::ordinary(
                    Program::Bytes(bytes),
                    tokio::io::empty(),
                    tokio::io::sink(),
                    tokio::io::sink(),
                    support::filesystem_ownership(),
                );
                let builder = builder.with_runtime_engine(runtime);
                let builder = if stall {
                    builder.with_fuel_estimator(FuelEstimator::new_oneshot(25_000, 12_500, 0))
                } else {
                    builder
                };
                let completion = builder.filesystem_completion();
                let result = match builder.build().await {
                    Ok(proc) => proc.run().await,
                    Err(error) => Err(error),
                };
                tokio::time::timeout(Duration::from_secs(5), completion.wait())
                    .await
                    .expect("filesystem cleanup deadline")
                    .expect("filesystem cleanup");
                let _ = result_tx.send(result);
            })
        }),
    };
    assert!(pool.spawn(request).is_ok(), "fixture admitted");
    let result = tokio::time::timeout(Duration::from_secs(12), result_rx)
        .await
        .expect("Cell result deadline")
        .expect("Cell result");
    tokio::time::timeout(Duration::from_secs(5), retired_rx)
        .await
        .expect("retirement deadline")
        .expect("retirement notification")
        .expect("executor task completed");
    result
}

fn admin(
    executor_metrics: ExecutorMetrics,
    fuel_metrics: FuelMetrics,
    shutdown: watch::Receiver<()>,
) -> (String, std::thread::JoinHandle<anyhow::Result<()>>) {
    let address = std::env::var("WW_OBSERVABILITY_ADMIN").unwrap_or_else(|_| "127.0.0.1:0".into());
    let listener = TcpListener::bind(address).expect("bind fixture admin endpoint");
    listener.set_nonblocking(true).unwrap();
    let url = format!("http://{}/metrics", listener.local_addr().unwrap());
    let (_epoch_tx, epoch_rx) = watch::channel(authority::Epoch::zero());
    let service = AdminService {
        listener,
        peer_id: "observability-fixture".into(),
        network_state: rpc::NetworkState::new(),
        version_info: VersionInfo {
            git_sha: "observability-fixture".into(),
            oci_image_id: None,
            kernel_identity: ww::kernel::IdentityState::pending(epoch_rx.clone()),
        },
        runtime_status: RuntimeStatus::starting(),
        kernel_ready_gate: Arc::new(authority::KernelReadyGate::new(epoch_rx)),
        route_registry: None,
        wasmtime_cache_metrics: wasmtime_cache_metrics(),
        executor_metrics,
        fuel_metrics,
    };
    (url, std::thread::spawn(move || service.run(shutdown)))
}

async fn scrape(url: &str) -> String {
    reqwest::Client::builder()
        .no_proxy()
        .timeout(Duration::from_secs(5))
        .build()
        .unwrap()
        .get(url)
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .text()
        .await
        .unwrap()
}

fn telemetry_metrics() {
    let mut metrics = String::new();
    ww::telemetry::render_metrics(&mut metrics);
    if let Some(path) = std::env::var_os("WW_OBSERVABILITY_OUTPUT") {
        let path = std::path::PathBuf::from(path);
        std::fs::create_dir_all(&path).unwrap();
        std::fs::write(path.join("telemetry.prom"), metrics).unwrap();
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "local observability incident; run through scripts/observability-incident.py"]
async fn incident_probe() {
    let telemetry = ww::config::init_tracing();
    let (shutdown, shutdown_rx) = watch::channel(());
    let executor = ExecutorMetrics::default();
    let fuel = FuelMetrics::default();
    let pool = ExecutorPool::try_new_with_collectors(
        1,
        shutdown_rx.clone(),
        support::filesystem_resources(),
        executor.clone(),
        fuel.clone(),
    )
    .unwrap();
    let (url, admin) = admin(executor.clone(), fuel.clone(), shutdown_rx);
    let output = std::env::var_os("WW_OBSERVABILITY_OUTPUT").map(std::path::PathBuf::from);
    if let Some(path) = &output {
        std::fs::create_dir_all(path).unwrap();
    }
    let before = scrape(&url).await;
    if let Some(path) = &output {
        std::fs::write(path.join("before.prom"), &before).unwrap();
    }
    // Allow the local Prometheus scraper to observe the zero baseline.
    let hold = std::env::var("WW_OBSERVABILITY_HOLD_SECONDS")
        .ok()
        .map(|v| v.parse::<u64>().unwrap())
        .unwrap_or(0)
        .min(60);
    tokio::time::sleep(Duration::from_secs(hold)).await;
    let ordinary_start = Instant::now();
    execute(&pool, pool.runtime_engine(), command(""), false)
        .await
        .unwrap();
    let ordinary_ns = ordinary_start.elapsed().as_nanos();
    // Deliberately retain this private engine's publisher without advancing it.
    // ExecutorPool's production epoch publisher continues ticking independently.
    let (stalled_runtime, _publisher) = runtime_engine_with_fuel_metrics(fuel.clone()).unwrap();
    let bytes = command(
        r#"(func $start (local $remaining i32)
      i32.const 3000 local.set $remaining
      (loop $spin local.get $remaining i32.const 1 i32.sub
        local.tee $remaining br_if $spin)) (start $start)"#,
    );
    let start = Instant::now();
    let error = execute(&pool, stalled_runtime, bytes, true)
        .await
        .expect_err("start waits until the existing runtime timeout");
    assert_eq!(error.to_string(), "guest component instantiation timed out");
    let elapsed_ns = start.elapsed().as_nanos();
    let snapshot = wasmtime_cache_metrics().lifecycle_snapshot();
    assert_eq!(snapshot.compilation.success, 2);
    assert_eq!(snapshot.instantiation.success, 1);
    assert_eq!(snapshot.instantiation.timeout, 1);
    assert_eq!(snapshot.instantiation.error, 0);
    assert_eq!(executor.snapshot().occupied, 0);
    assert_eq!(executor.snapshot().completed, 2);
    assert!(fuel.snapshot().oneshot.suspend > 0);
    let after = scrape(&url).await;
    assert!(after.contains("ww_wasmtime_instantiations_total{outcome=\"timeout\"} 1"));
    if let Some(path) = &output {
        std::fs::write(path.join("after.prom"), &after).unwrap();
    }
    println!(
        "OBSERVABILITY_EVIDENCE={}",
        serde_json::json!({"ordinary_cell_ns":ordinary_ns,"incident_ns":elapsed_ns,"instantiation_timeout":snapshot.instantiation.timeout,"oneshot_suspend":fuel.snapshot().oneshot.suspend,"completed_executor_tasks":executor.snapshot().completed,"occupied":executor.snapshot().occupied,"admin_url":url,"recorded_explanation":"instantiation exceeded the 5000ms budget; telemetry does not identify why the epoch stopped"})
    );
    tokio::time::sleep(Duration::from_secs(hold)).await;
    shutdown.send(()).unwrap();
    pool.shutdown_and_join().await;
    admin.join().unwrap().unwrap();
    let drain = Instant::now();
    drop(telemetry);
    telemetry_metrics();
    println!("OBSERVABILITY_DRAIN_NS={}", drain.elapsed().as_nanos());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "descriptive Cell cost probe, no latency assertions"]
async fn cost_probe() {
    let telemetry = ww::config::init_tracing();
    let (shutdown, shutdown_rx) = watch::channel(());
    let pool = ExecutorPool::try_new(1, shutdown_rx, support::filesystem_resources()).unwrap();
    let bytes = command("");
    let count = std::env::var("WW_OBSERVABILITY_SAMPLES")
        .ok()
        .map(|value| value.parse::<usize>().unwrap())
        .unwrap_or(35)
        .clamp(6, 1000);
    for sample in 0..count {
        let start = Instant::now();
        execute(&pool, pool.runtime_engine(), bytes.clone(), false)
            .await
            .unwrap();
        println!(
            "OBSERVABILITY_COST={}",
            serde_json::json!({"sample":sample,"warmup":sample<5,"wall_ns":start.elapsed().as_nanos(),"boundary":"ExecutorPool admission through compilation, real P3 no-op command, filesystem cleanup and task retirement"})
        );
    }
    shutdown.send(()).unwrap();
    pool.shutdown_and_join().await;
    let drain = Instant::now();
    drop(telemetry);
    println!("OBSERVABILITY_DRAIN_NS={}", drain.elapsed().as_nanos());
    telemetry_metrics();
}
