//! Real admin HTTP scrapes against the Wasmtime and ExecutorPool producers.
//! Each cache policy runs in a subprocess because EngineFactory reads env once.
#![cfg(not(target_arch = "wasm32"))]

mod support;

use std::collections::{BTreeMap, BTreeSet};
use std::net::TcpListener;
use std::process::Command;
use std::sync::Arc;
use std::time::{Duration, Instant};

use cell::engine::{
    compile_component, wasm_engine, wasmtime_cache_metrics, WasmtimeCacheSnapshot,
    CWASM_CACHE_MAX_BYTES_ENV, CWASM_DIR_ENV,
};
use cell::metrics::FuelMetrics;
use tokio::sync::{oneshot, watch};
use ww::metrics::{AdminService, RuntimeStatus, VersionInfo};
use ww::services::{ExecutorMetrics, ExecutorPool, Service, SpawnFactory, SpawnRequest};

const PROBE_MODE: &str = "WW_METRICS_HTTP_TEST_MODE";
const MEASURE: &str = "WW_METRICS_HTTP_TEST_MEASURE";

#[test]
fn enabled_cache_http_tracks_stores_hits_and_failed_attempts() {
    run_probe("enabled", false);
}

#[test]
fn disabled_cache_http_counts_attempts_without_hits_or_stores() {
    run_probe("disabled", false);
}

#[test]
fn fallback_cache_http_counts_attempts_without_hits_or_stores() {
    run_probe("fallback", false);
}

/// Run unchanged on either side of the exposition fix:
/// cargo test --test metrics_http measure_http_scrapes -- --ignored --nocapture
/// Measurements include loopback HTTP transport, body transfer, and scheduling.
#[test]
#[ignore = "opt-in scrape measurements; no machine-dependent latency assertions"]
fn measure_http_scrapes() {
    for mode in ["enabled", "disabled", "fallback"] {
        run_probe(mode, true);
    }
}

fn run_probe(mode: &str, measure: bool) {
    let directory = tempfile::tempdir().expect("private cache directory");
    let invalid_directory = directory.path().join("regular-file");
    std::fs::write(&invalid_directory, b"not a directory").unwrap();
    let mut command = Command::new(std::env::current_exe().unwrap());
    command
        .args(["--exact", "metrics_http_probe", "--ignored", "--nocapture"])
        .env(PROBE_MODE, mode)
        .env_remove(MEASURE)
        .env_remove(CWASM_DIR_ENV)
        .env_remove(CWASM_CACHE_MAX_BYTES_ENV);
    match mode {
        "enabled" => {
            command.env(CWASM_DIR_ENV, directory.path().join("cache"));
        }
        "fallback" => {
            command.env(CWASM_DIR_ENV, invalid_directory);
        }
        "disabled" => {}
        _ => panic!("unknown mode {mode}"),
    }
    if measure {
        command.env(MEASURE, "1");
    }
    // Inherit output so benchmark samples cannot fill a subprocess pipe.
    let mut child = command.spawn().expect("spawn isolated metrics probe");
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        if let Some(status) = child.try_wait().unwrap() {
            assert!(status.success(), "{mode} metrics probe failed: {status}");
            break;
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            panic!("{mode} metrics probe exceeded 60 seconds");
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

struct Admin {
    url: String,
    shutdown: watch::Sender<()>,
    done: std::sync::mpsc::Receiver<anyhow::Result<()>>,
}

impl Admin {
    fn start() -> Self {
        Self::with_executor_metrics(ExecutorMetrics::default())
    }

    fn with_executor_metrics(executor_metrics: ExecutorMetrics) -> Self {
        Self::with_metrics(executor_metrics, FuelMetrics::default())
    }

    fn with_metrics(executor_metrics: ExecutorMetrics, fuel_metrics: FuelMetrics) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let url = format!("http://{}/metrics", listener.local_addr().unwrap());
        let (_epoch_tx, epoch_rx) = watch::channel(authority::Epoch::zero());
        let service = AdminService {
            listener,
            peer_id: "metrics-http-test".into(),
            network_state: rpc::NetworkState::new(),
            version_info: VersionInfo {
                git_sha: "test".into(),
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
        let (shutdown, shutdown_rx) = watch::channel(());
        let (done_tx, done) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let _ = done_tx.send(service.run(shutdown_rx));
        });
        Self {
            url,
            shutdown,
            done,
        }
    }
}

impl Drop for Admin {
    fn drop(&mut self) {
        let _ = self.shutdown.send(());
        let result = self.done.recv_timeout(Duration::from_secs(5));
        if !std::thread::panicking() {
            result
                .expect("admin shutdown deadline")
                .expect("admin service");
        }
    }
}

#[derive(Debug)]
struct Scrape {
    body: String,
    samples: BTreeMap<String, u64>,
    http_elapsed: Duration,
}

impl Scrape {
    fn fuel(&self, policy: &str, decision: &str) -> u64 {
        self.samples
            [&format!("ww_fuel_decisions_total{{policy=\"{policy}\",decision=\"{decision}\"}}")]
    }

    fn assert_fuel_unchanged(&self, previous: &Self) {
        for policy in ["scheduled", "oneshot"] {
            for decision in ["grant", "suspend", "exhausted"] {
                assert_eq!(self.fuel(policy, decision), previous.fuel(policy, decision));
            }
        }
        self.assert_unsupported_absent();
        self.assert_live_families_contiguous();
    }

    fn parse(body: String, http_elapsed: Duration) -> Self {
        assert!(
            body.ends_with('\n'),
            "Prometheus text requires a final newline"
        );
        let mut samples = BTreeMap::new();
        let mut types = BTreeMap::new();
        let mut helps = BTreeSet::new();
        for line in body.lines() {
            if let Some(descriptor) = line.strip_prefix("# HELP ") {
                let (name, text) = descriptor.split_once(' ').expect("HELP text");
                assert!(!text.is_empty());
                assert!(helps.insert(name.to_owned()), "duplicate HELP: {name}");
            } else if let Some(descriptor) = line.strip_prefix("# TYPE ") {
                let (name, kind) = descriptor.split_once(' ').expect("TYPE kind");
                assert!(matches!(kind, "gauge" | "counter" | "histogram"));
                assert!(types.insert(name.to_owned(), kind.to_owned()).is_none());
            } else {
                // The live producers have integer samples and bounded labels.
                // This is a parser for this endpoint's live subset, not a general
                // Prometheus parser for future fuel/RPC observations.
                let (identity, value) = line.split_once(' ').expect("sample value");
                let name = identity.split('{').next().unwrap();
                assert!(name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_'));
                assert!(types.contains_key(name), "sample without TYPE: {line}");
                if identity.contains('{') {
                    if name == "ww_fuel_decisions_total" {
                        assert!(
                            ["scheduled", "oneshot"].iter().any(|policy| {
                                ["grant", "suspend", "exhausted"].iter().any(|decision| {
                                    identity
                                        == format!(
                                            "{name}{{policy=\"{policy}\",decision=\"{decision}\"}}"
                                        )
                                })
                            }),
                            "unsupported fuel labels: {identity}"
                        );
                    } else {
                        let (label, values): (&str, &[&str]) = match name {
                            "ww_wasmtime_cache_state" => {
                                ("state", &["enabled", "disabled", "fallback"])
                            }
                            "ww_executor_spawn_admissions_total" => {
                                ("outcome", &["accepted", "full", "closed"])
                            }
                            "ww_executor_cell_terminations_total" => {
                                ("outcome", &["completed", "cancelled", "panicked"])
                            }
                            _ => panic!("unsupported metric labels: {identity}"),
                        };
                        assert!(values
                            .iter()
                            .any(|value| { identity == format!("{name}{{{label}=\"{value}\"}}") }));
                    }
                }
                let value: u64 = value.parse().expect("nonnegative integer sample");
                assert!(
                    samples.insert(identity.to_owned(), value).is_none(),
                    "duplicate sample"
                );
            }
        }
        assert_eq!(helps, types.keys().cloned().collect());
        let expected_types = [
            ("ww_wasmtime_cache_state", "gauge"),
            ("ww_wasmtime_cache_hits_total", "counter"),
            ("ww_wasmtime_cache_stores_total", "counter"),
            ("ww_wasmtime_component_compilations_total", "counter"),
            ("ww_executor_cell_slots", "gauge"),
            ("ww_executor_spawn_admissions_total", "counter"),
            ("ww_executor_cell_terminations_total", "counter"),
            ("ww_fuel_decisions_total", "counter"),
        ]
        .into_iter()
        .map(|(name, kind)| (name.to_owned(), kind.to_owned()))
        .collect();
        assert_eq!(types, expected_types, "public metric metadata changed");
        Self {
            body,
            samples,
            http_elapsed,
        }
    }

    fn assert_snapshot(&self, snapshot: WasmtimeCacheSnapshot) {
        for state in ["enabled", "disabled", "fallback"] {
            assert_eq!(
                self.samples[&format!("ww_wasmtime_cache_state{{state=\"{state}\"}}")],
                u64::from(state == snapshot.state.as_str())
            );
        }
        assert_eq!(self.samples["ww_wasmtime_cache_hits_total"], snapshot.hits);
        assert_eq!(
            self.samples["ww_wasmtime_cache_stores_total"],
            snapshot.stores
        );
        assert_eq!(
            self.samples["ww_wasmtime_component_compilations_total"],
            snapshot.component_compilations
        );
    }

    fn assert_executor(&self, occupied: u64, admissions: [u64; 3], terminations: [u64; 3]) {
        assert_eq!(self.samples["ww_executor_cell_slots"], occupied);
        for (outcome, expected) in ["accepted", "full", "closed"].into_iter().zip(admissions) {
            assert_eq!(
                self.samples
                    [&format!("ww_executor_spawn_admissions_total{{outcome=\"{outcome}\"}}")],
                expected,
                "{outcome} admissions"
            );
        }
        for (outcome, expected) in ["completed", "cancelled", "panicked"]
            .into_iter()
            .zip(terminations)
        {
            assert_eq!(
                self.samples
                    [&format!("ww_executor_cell_terminations_total{{outcome=\"{outcome}\"}}")],
                expected,
                "{outcome} terminations"
            );
        }
        self.assert_unsupported_absent();
        self.assert_live_families_contiguous();
    }

    fn assert_unsupported_absent(&self) {
        for name in self.samples.keys() {
            assert!(
                name.starts_with("ww_wasmtime_")
                    || name.starts_with("ww_executor_")
                    || name.starts_with("ww_fuel_decisions_total{"),
                "unsupported sample exposed: {name}"
            );
        }
        assert_eq!(self.samples.len(), 19);
        assert!(self.body.len() < 4096, "unbounded metrics response");
    }

    fn assert_live_families_contiguous(&self) {
        let mut seen = BTreeSet::new();
        let mut current = None;
        for line in self.body.lines() {
            let content = line
                .strip_prefix("# HELP ")
                .or_else(|| line.strip_prefix("# TYPE "))
                .unwrap_or(line);
            let name = content.split([' ', '{']).next().unwrap();
            if current != Some(name) {
                assert!(
                    seen.insert(name),
                    "noncontiguous live metric family: {name}"
                );
                current = Some(name);
            }
        }
    }
}

async fn scrape(client: &reqwest::Client, admin: &Admin) -> Scrape {
    let start = Instant::now();
    let response = client.get(&admin.url).send().await.expect("GET /metrics");
    assert_eq!(response.status(), reqwest::StatusCode::OK);
    assert_eq!(
        response.headers()[reqwest::header::CONTENT_TYPE],
        "text/plain; version=0.0.4; charset=utf-8"
    );
    let body = response.text().await.expect("metrics body");
    let elapsed = start.elapsed();
    Scrape::parse(body, elapsed)
}

type CellResult = oneshot::Receiver<anyhow::Result<i32>>;

fn submit(pool: &ExecutorPool, name: &str, factory: SpawnFactory) -> CellResult {
    let (result_tx, result) = oneshot::channel();
    assert!(
        pool.spawn(SpawnRequest {
            name: name.into(),
            factory,
            result_tx: Some(result_tx),
        })
        .is_ok(),
        "admit {name}"
    );
    result
}

fn held_cell(
    pool: &ExecutorPool,
    name: &str,
) -> (oneshot::Sender<()>, oneshot::Receiver<()>, CellResult) {
    let (release, released) = oneshot::channel();
    let (started_tx, started) = oneshot::channel();
    let result = submit(
        pool,
        name,
        Box::new(move |_| {
            Box::pin(async move {
                started_tx.send(()).unwrap();
                let _ = released.await;
            })
        }),
    );
    (release, started, result)
}

async fn cell_result(result: CellResult) -> anyhow::Result<i32> {
    tokio::time::timeout(Duration::from_secs(5), result)
        .await
        .expect("Cell termination deadline")
        .expect("Cell result sender")
}

#[tokio::test]
async fn fuel_http_tracks_real_p3_grants_suspension_exhaustion_and_retains_history() {
    use cell::proc::{FuelEstimator, FuelObserver};
    let fuel = FuelMetrics::default();
    let admin = Admin::with_metrics(ExecutorMetrics::default(), fuel.clone());
    let other_admin = Admin::start();
    let client = reqwest::Client::builder()
        .no_proxy()
        .timeout(Duration::from_secs(5))
        .build()
        .unwrap();
    let initial = scrape(&client, &admin).await;
    for policy in ["scheduled", "oneshot"] {
        for decision in ["grant", "suspend", "exhausted"] {
            assert_eq!(initial.fuel(policy, decision), 0);
        }
    }
    let (engine, mut publisher) = cell::engine::runtime_engine_with_fuel_metrics(fuel).unwrap();
    let wasm = wat::parse_str(include_str!("fixtures/spinning-component.wat")).unwrap();
    let scheduled_wasm = wat::parse_str(
        r#"(component
        (core func $return (canon task.return (result (result))))
        (core module $noop
            (import "" "return" (func $return (param i32)))
            (func (export "run") i32.const 0 call $return))
        (core instance $i (instantiate $noop (with "" (instance (export "return" (func $return))))))
        (func $run async (result (result)) (canon lift (core func $i "run") async))
        (instance (export (interface "wasi:cli/run@0.3.0")) (export "run" (func $run))))"#,
    )
    .unwrap();
    let (builder, _handles) = cell::Builder::ordinary(
        cell::Program::Bytes(scheduled_wasm),
        tokio::io::empty(),
        tokio::io::sink(),
        tokio::io::sink(),
        support::filesystem_ownership(),
    );
    let proc = builder
        .with_runtime_engine(engine.clone())
        .build()
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(5), proc.run())
        .await
        .unwrap()
        .unwrap();
    let after_scheduled = scrape(&client, &admin).await;
    assert!(after_scheduled.fuel("scheduled", "grant") >= 1);
    assert_eq!(after_scheduled.fuel("scheduled", "suspend"), 0);
    assert_eq!(after_scheduled.fuel("scheduled", "exhausted"), 0);

    let observer = FuelObserver::default();
    let (builder, _handles) = cell::Builder::ordinary(
        cell::Program::Bytes(wasm),
        tokio::io::empty(),
        tokio::io::sink(),
        tokio::io::sink(),
        support::filesystem_ownership(),
    );
    let proc = builder
        .with_runtime_engine(engine.clone())
        .with_fuel_estimator(FuelEstimator::new_oneshot(25_000, 10_000, 0))
        .with_fuel_observer(observer.clone())
        .build()
        .await
        .unwrap();
    let task = tokio::spawn(proc.run());
    tokio::time::timeout(Duration::from_secs(5), observer.wait_for_suspension(1))
        .await
        .unwrap();
    let first_wait = scrape(&client, &admin).await;
    assert_eq!(first_wait.fuel("oneshot", "suspend"), 1);
    assert!(first_wait.fuel("oneshot", "grant") >= 1);
    assert_eq!(first_wait.fuel("oneshot", "exhausted"), 0);
    publisher.tick();
    tokio::time::timeout(Duration::from_secs(5), observer.wait_for_suspension(2))
        .await
        .unwrap();
    let second_wait = scrape(&client, &admin).await;
    assert_eq!(second_wait.fuel("oneshot", "suspend"), 2);
    assert!(second_wait.fuel("oneshot", "grant") > first_wait.fuel("oneshot", "grant"));
    publisher.tick();
    let error = tokio::time::timeout(Duration::from_secs(5), task)
        .await
        .unwrap()
        .unwrap()
        .unwrap_err();
    assert!(format!("{error:#}").contains("one-shot total fuel authority exhausted"));
    let terminal = scrape(&client, &admin).await;
    assert_eq!(terminal.fuel("oneshot", "exhausted"), 1);
    assert_eq!(terminal.fuel("oneshot", "suspend"), 2);
    assert!(terminal.fuel("oneshot", "grant") > second_wait.fuel("oneshot", "grant"));
    drop(engine);
    drop(publisher);
    scrape(&client, &admin)
        .await
        .assert_fuel_unchanged(&terminal);
    let independent = scrape(&client, &other_admin).await;
    for policy in ["scheduled", "oneshot"] {
        for decision in ["grant", "suspend", "exhausted"] {
            assert_eq!(
                independent.fuel(policy, decision),
                0,
                "independent Host collector"
            );
        }
    }
}

#[tokio::test]
async fn fuel_http_runtime_spawn_uses_pool_collector_across_child_and_pool_teardown() {
    struct EmptyMembrane;
    impl ww::system_capnp::membrane::Server for EmptyMembrane {}

    tokio::task::LocalSet::new()
        .run_until(async {
            let fuel = FuelMetrics::default();
            let executor_metrics = ExecutorMetrics::default();
            let admin = Admin::with_metrics(executor_metrics.clone(), fuel.clone());
            let (_shutdown, shutdown_rx) = watch::channel(());
            let pool = ExecutorPool::try_new_with_collectors(
                1,
                shutdown_rx,
                support::filesystem_resources(),
                executor_metrics,
                fuel,
            )
            .unwrap();
            let client = reqwest::Client::builder()
                .no_proxy()
                .timeout(Duration::from_secs(5))
                .build()
                .unwrap();
            let runtime = ww::launcher::create_runtime_client(
                false,
                authority::EpochGuard::fixed(authority::Epoch::zero()),
                pool.runtime_engine(),
                None,
                rpc::CachePolicy::Isolated,
                support::filesystem_resources(),
            );
            let mut load = runtime.load_request();
            load.get().set_wasm(
                &wat::parse_str(include_str!("fixtures/spinning-component.wat")).unwrap(),
            );
            let executor = load
                .send()
                .promise
                .await
                .unwrap()
                .get()
                .unwrap()
                .get_executor()
                .unwrap();
            for budget in [1, 0] {
                let mut spawn = executor.spawn_request();
                spawn
                    .get()
                    .set_membrane(capnp_rpc::new_client(EmptyMembrane));
                spawn
                    .get()
                    .init_fuel_policy()
                    .init_oneshot()
                    .set_total_budget(budget);
                let process = spawn
                    .send()
                    .promise
                    .await
                    .unwrap()
                    .get()
                    .unwrap()
                    .get_process()
                    .unwrap();
                let result = tokio::time::timeout(
                    Duration::from_secs(5),
                    process.wait_request().send().promise,
                )
                .await
                .expect("spawned child termination")
                .expect("process wait");
                assert_ne!(result.get().unwrap().get_exit_code(), 0);
            }
            let terminal = scrape(&client, &admin).await;
            assert_eq!(terminal.fuel("oneshot", "grant"), 1);
            assert_eq!(terminal.fuel("oneshot", "exhausted"), 2);
            assert_eq!(terminal.fuel("oneshot", "suspend"), 0);
            assert_eq!(terminal.fuel("scheduled", "grant"), 0);
            // These children use the launcher directly and consume no pool admission.
            terminal.assert_executor(0, [0, 0, 0], [0, 0, 0]);
            drop(executor);
            drop(runtime);
            pool.shutdown_and_join().await;
            scrape(&client, &admin)
                .await
                .assert_fuel_unchanged(&terminal);
        })
        .await;
}

#[tokio::test]
async fn executor_http_tracks_operations_and_retains_counters_after_teardown() {
    let metrics = ExecutorMetrics::default();
    let admin = Admin::with_executor_metrics(metrics.clone());
    let client = reqwest::Client::builder()
        .no_proxy()
        .timeout(Duration::from_secs(5))
        .build()
        .unwrap();
    let (shutdown, shutdown_rx) = watch::channel(());
    let pool = ExecutorPool::try_new_with_metrics(
        2,
        shutdown_rx,
        support::filesystem_resources(),
        metrics,
    )
    .unwrap();
    scrape(&client, &admin)
        .await
        .assert_executor(0, [0, 0, 0], [0, 0, 0]);

    // Both futures stay live, so least-loaded assignment occupies both workers.
    let (complete, started, completed) = held_cell(&pool, "private-cell-name-complete");
    let (cancel_one_release, cancel_one_started, cancelled_one) =
        held_cell(&pool, "private-cell-name-cancel-one");
    tokio::time::timeout(Duration::from_secs(5), async {
        started.await.unwrap();
        cancel_one_started.await.unwrap();
    })
    .await
    .expect("both workers started their Cells");
    let occupied = scrape(&client, &admin).await;
    occupied.assert_executor(2, [2, 0, 0], [0, 0, 0]);
    assert!(!occupied.body.contains("private-cell-name"));

    complete.send(()).unwrap();
    assert_eq!(cell_result(completed).await.unwrap(), 0);
    scrape(&client, &admin)
        .await
        .assert_executor(1, [2, 0, 0], [1, 0, 0]);

    let panicked = submit(
        &pool,
        "private-cell-name-panic",
        Box::new(|_| Box::pin(async { panic!("intentional metrics probe panic") })),
    );
    assert!(cell_result(panicked).await.is_err());
    scrape(&client, &admin)
        .await
        .assert_executor(1, [3, 0, 0], [1, 0, 1]);

    let (cancel_two_release, cancel_two_started, cancelled_two) =
        held_cell(&pool, "private-cell-name-cancel-two");
    tokio::time::timeout(Duration::from_secs(5), cancel_two_started)
        .await
        .expect("second worker started a replacement Cell")
        .unwrap();
    shutdown.send(()).unwrap();
    assert!(cell_result(cancelled_one).await.is_err());
    assert!(cell_result(cancelled_two).await.is_err());
    // Each worker closes its admission channel before cancelling its Cell.
    // These result acknowledgements establish that both channels are closed.
    assert!(pool
        .spawn(SpawnRequest {
            name: "private-cell-name-rejected".into(),
            factory: Box::new(|_| Box::pin(async {})),
            result_tx: None,
        })
        .is_err());
    pool.shutdown_and_join().await;
    drop((cancel_one_release, cancel_two_release));

    let stopped = scrape(&client, &admin).await;
    stopped.assert_executor(0, [4, 0, 1], [1, 2, 1]);
    for _ in 0..8 {
        assert_eq!(scrape(&client, &admin).await.body, stopped.body);
    }
    println!(
        "EXECUTOR_HTTP_EVIDENCE={}",
        serde_json::json!({"occupied": occupied.samples, "after_teardown": stopped.samples,
            "body_bytes": stopped.body.len()})
    );
}

#[tokio::test]
async fn executor_http_counts_full_admission_and_cancels_queued_slots() {
    let metrics = ExecutorMetrics::default();
    let admin = Admin::with_executor_metrics(metrics.clone());
    let client = reqwest::Client::builder()
        .no_proxy()
        .timeout(Duration::from_secs(5))
        .build()
        .unwrap();
    let (shutdown, shutdown_rx) = watch::channel(());
    let pool = ExecutorPool::try_new_with_metrics(
        1,
        shutdown_rx,
        support::filesystem_resources(),
        metrics,
    )
    .unwrap();
    let (started_tx, started) = oneshot::channel();
    let (release, released) = std::sync::mpsc::channel();
    let blocked = submit(
        &pool,
        "private-cell-name-blocked",
        Box::new(move |_| {
            started_tx.send(()).unwrap();
            // Hold the worker thread before it can dequeue another request.
            // The deadline also bounds cleanup if an earlier assertion fails.
            released.recv_timeout(Duration::from_secs(10)).unwrap();
            Box::pin(std::future::pending())
        }),
    );
    tokio::time::timeout(Duration::from_secs(5), started)
        .await
        .expect("worker entered the blocking factory")
        .unwrap();
    let mut accepted = 1;
    loop {
        let request = SpawnRequest {
            name: format!("private-queued-cell-{accepted}"),
            factory: Box::new(|_| Box::pin(std::future::pending())),
            result_tx: None,
        };
        if pool.spawn(request).is_err() {
            break;
        }
        accepted += 1;
        assert!(accepted < 1024, "spawn queue must remain bounded");
    }
    assert!(accepted > 1, "test must exercise queued requests");
    let queued = scrape(&client, &admin).await;
    queued.assert_executor(accepted, [accepted, 1, 0], [0, 0, 0]);
    assert!(!queued.body.contains("private-"));

    shutdown.send(()).unwrap();
    release.send(()).unwrap();
    assert!(cell_result(blocked).await.is_err());
    pool.shutdown_and_join().await;
    scrape(&client, &admin)
        .await
        .assert_executor(0, [accepted, 1, 0], [0, accepted, 0]);
}

async fn repeat_scrapes(
    client: &reqwest::Client,
    admin: &Admin,
    mode: &str,
    phase: &str,
    measure: bool,
) {
    let snapshot = wasmtime_cache_metrics().snapshot();
    let first = scrape(client, admin).await;
    if let Some(directory) = std::env::var_os("WW_METRICS_HTTP_TEST_OUTPUT_DIR") {
        let directory = std::path::PathBuf::from(directory);
        std::fs::create_dir_all(&directory).unwrap();
        std::fs::write(directory.join(format!("{mode}-{phase}.prom")), &first.body).unwrap();
    }
    let mut nanos = Vec::new();
    for _ in 0..if measure { 200 } else { 8 } {
        let response = scrape(client, admin).await;
        nanos.push(response.http_elapsed.as_nanos());
        assert_eq!(
            response.body, first.body,
            "passive scrape changed exposition"
        );
        response.assert_snapshot(snapshot);
    }
    assert_eq!(
        wasmtime_cache_metrics().snapshot(),
        snapshot,
        "scrapes changed producer counters"
    );
    if measure {
        let mut sorted = nanos.clone();
        sorted.sort_unstable();
        println!(
            "{}",
            serde_json::json!({
                "mode": mode, "phase": phase, "body_bytes": first.body.len(),
                "samples": nanos.len(), "http_ns": nanos,
                "p50_ns": sorted[99], "p95_ns": sorted[189], "p99_ns": sorted[197],
            })
        );
    }
}

#[test]
#[ignore = "invoked only by isolated cache-policy subprocesses"]
fn metrics_http_probe() {
    let mode = std::env::var(PROBE_MODE).expect("run a public metrics_http test");
    let measure = std::env::var_os(MEASURE).is_some();
    let admin = Admin::start();
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    runtime.block_on(async {
        let client = reqwest::Client::builder()
            .no_proxy()
            .timeout(Duration::from_secs(5))
            .build()
            .unwrap();
        let metrics = wasmtime_cache_metrics();
        let initial = metrics.snapshot();
        assert_eq!(initial.state.as_str(), mode);
        assert_eq!(
            (initial.hits, initial.stores, initial.component_compilations),
            (0, 0, 0)
        );
        let empty = scrape(&client, &admin).await;
        empty.assert_snapshot(initial);
        empty.assert_executor(0, [0, 0, 0], [0, 0, 0]);
        repeat_scrapes(&client, &admin, &mode, "before_compile", measure).await;

        let wasm = wat::parse_str("(component)").unwrap();
        compile_component(&wasm_engine().unwrap(), &wasm).expect("cold compilation");
        let cold = metrics.snapshot();
        assert_eq!(cold.component_compilations, 1);
        assert_eq!(cold.hits, 0);
        assert_eq!(cold.stores, u64::from(mode == "enabled"));
        scrape(&client, &admin).await.assert_snapshot(cold);

        // A new engine uses the same process cache; a hit is still an attempt.
        compile_component(&wasm_engine().unwrap(), &wasm).expect("second compilation");
        let warm = metrics.snapshot();
        assert_eq!(warm.component_compilations, 2);
        assert_eq!(warm.hits, u64::from(mode == "enabled"));
        assert_eq!(warm.stores, cold.stores);
        scrape(&client, &admin).await.assert_snapshot(warm);

        assert!(compile_component(&wasm_engine().unwrap(), b"not wasm").is_err());
        let failed = metrics.snapshot();
        assert_eq!(failed.component_compilations, 3);
        assert_eq!((failed.hits, failed.stores), (warm.hits, warm.stores));
        assert_eq!(failed.state, initial.state);
        let populated = scrape(&client, &admin).await;
        populated.assert_snapshot(failed);
        repeat_scrapes(&client, &admin, &mode, "after_compile", measure).await;

        empty.assert_unsupported_absent();
        populated.assert_unsupported_absent();
        empty.assert_live_families_contiguous();
        populated.assert_live_families_contiguous();
    });
}
