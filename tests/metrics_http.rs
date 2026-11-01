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
    kernel_ready_gate: Arc<authority::KernelReadyGate>,
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
        let kernel_ready_gate = Arc::new(authority::KernelReadyGate::new(epoch_rx.clone()));
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
            kernel_ready_gate: kernel_ready_gate.clone(),
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
            kernel_ready_gate,
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
    duration_sums: BTreeMap<String, f64>,
    seconds: BTreeMap<String, f64>,
    http_elapsed: Duration,
}

impl Scrape {
    fn fuel(&self, policy: &str, decision: &str) -> u64 {
        self.samples
            [&format!("ww_fuel_decisions_total{{policy=\"{policy}\",decision=\"{decision}\"}}")]
    }

    fn settled_fuel(&self, policy: &str) -> u64 {
        self.samples[&format!("ww_fuel_settled_units_total{{policy=\"{policy}\"}}")]
    }

    fn assert_fuel_unchanged(&self, previous: &Self) {
        for policy in ["scheduled", "oneshot"] {
            assert_eq!(self.settled_fuel(policy), previous.settled_fuel(policy));
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
        let mut duration_sums = BTreeMap::new();
        let mut seconds = BTreeMap::new();
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
                // This parser validates the endpoint's fixed families and labels.
                // Duration sums and worker timing gauges use seconds.
                let (identity, value) = line.split_once(' ').expect("sample value");
                let name = identity.split('{').next().unwrap();
                assert!(name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_'));
                let family = metric_family(name);
                assert!(types.contains_key(family), "sample without TYPE: {line}");
                if family != name {
                    assert_eq!(types[family], "histogram");
                    if name.ends_with("_bucket") {
                        assert!(
                            LIFECYCLE_BUCKETS
                                .iter()
                                .any(|bound| { identity == format!("{name}{{le=\"{bound}\"}}") }),
                            "unsupported histogram labels: {identity}"
                        );
                    } else {
                        assert_eq!(identity, name, "unexpected duration labels");
                    }
                } else if identity.contains('{') {
                    if name.starts_with("ww_executor_worker_") {
                        let labels = identity
                            .strip_prefix(&format!("{name}{{worker=\""))
                            .expect("worker is the first label");
                        let (worker, suffix) = labels.split_once('"').unwrap();
                        let index: usize = worker.parse().expect("numeric worker index");
                        assert_eq!(worker, index.to_string());
                        match name {
                            "ww_executor_worker_state" => {
                                assert!(["starting", "running", "shutting_down", "stopped"]
                                    .iter()
                                    .any(|state| suffix == format!(",state=\"{state}\"}}")))
                            }
                            "ww_executor_worker_stalled"
                            | "ww_executor_worker_progress_age_seconds" => assert_eq!(suffix, "}"),
                            _ => panic!("unsupported worker labels: {identity}"),
                        }
                    } else if name == "ww_fuel_decisions_total" {
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
                            "ww_wasmtime_component_compile_total" => {
                                ("outcome", &["success", "error"])
                            }
                            "ww_wasmtime_instantiations_total" => {
                                ("outcome", &["success", "error", "timeout"])
                            }
                            "ww_fuel_settled_units_total" => ("policy", &["scheduled", "oneshot"]),
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
                if name.ends_with("_sum") {
                    let value: f64 = value.parse().expect("duration sum in seconds");
                    assert!(value.is_finite() && value >= 0.0);
                    assert!(duration_sums.insert(identity.to_owned(), value).is_none());
                    continue;
                }
                if name.starts_with("ww_executor_worker_") && name.ends_with("_seconds") {
                    let value: f64 = value.parse().expect("worker timing in seconds");
                    assert!(value.is_finite() && value >= 0.0);
                    assert!(seconds.insert(identity.to_owned(), value).is_none());
                    continue;
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
            ("ww_wasmtime_component_compile_total", "counter"),
            (
                "ww_wasmtime_component_compile_duration_seconds",
                "histogram",
            ),
            ("ww_wasmtime_instantiations_total", "counter"),
            ("ww_wasmtime_instantiation_duration_seconds", "histogram"),
            ("ww_executor_cell_slots", "gauge"),
            ("ww_executor_spawn_admissions_total", "counter"),
            ("ww_executor_cell_terminations_total", "counter"),
            ("ww_executor_worker_heartbeat_interval_seconds", "gauge"),
            ("ww_executor_worker_stall_threshold_seconds", "gauge"),
            ("ww_executor_worker_state", "gauge"),
            ("ww_executor_worker_stalled", "gauge"),
            ("ww_executor_worker_progress_age_seconds", "gauge"),
            ("ww_fuel_decisions_total", "counter"),
            ("ww_fuel_settled_units_total", "counter"),
        ]
        .into_iter()
        .map(|(name, kind)| (name.to_owned(), kind.to_owned()))
        .collect();
        assert_eq!(types, expected_types, "public metric metadata changed");
        Self {
            body,
            samples,
            duration_sums,
            seconds,
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

    fn assert_lifecycle(&self, compilation: [u64; 2], instantiation: [u64; 3]) {
        for (name, outcomes) in [
            (
                "ww_wasmtime_component_compile_total",
                compilation.as_slice(),
            ),
            ("ww_wasmtime_instantiations_total", instantiation.as_slice()),
        ] {
            for (outcome, count) in ["success", "error", "timeout"].into_iter().zip(outcomes) {
                assert_eq!(
                    self.samples[&format!("{name}{{outcome=\"{outcome}\"}}")],
                    *count
                );
            }
        }
        let snapshot = wasmtime_cache_metrics().lifecycle_snapshot();
        for (name, expected, duration) in [
            (
                "ww_wasmtime_component_compile_duration_seconds",
                compilation.iter().sum::<u64>(),
                snapshot.compilation.duration,
            ),
            (
                "ww_wasmtime_instantiation_duration_seconds",
                instantiation.iter().sum::<u64>(),
                snapshot.instantiation.duration,
            ),
        ] {
            assert_eq!(self.samples[&format!("{name}_count")], expected);
            assert_eq!(duration.count, expected);
            let mut previous = 0;
            for (bound, count) in LIFECYCLE_BUCKETS.iter().zip(duration.buckets) {
                let observed = self.samples[&format!("{name}_bucket{{le=\"{bound}\"}}")];
                assert_eq!(observed, count);
                assert!(observed >= previous && observed <= expected);
                previous = observed;
            }
            assert_eq!(previous, expected, "+Inf must equal count");
            let sum = self.duration_sums[&format!("{name}_sum")];
            assert!((sum - duration.sum_nanos as f64 / 1_000_000_000.0).abs() < 1e-9);
            if expected == 0 {
                assert_eq!(sum, 0.0);
            } else {
                assert!(
                    sum > 0.0,
                    "completed operations must have timed observations"
                );
            }
        }
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
                    || name.starts_with("ww_fuel_decisions_total{")
                    || name.starts_with("ww_fuel_settled_units_total{"),
                "unsupported sample exposed: {name}"
            );
        }
        let workers = self
            .samples
            .keys()
            .filter(|name| name.starts_with("ww_executor_worker_stalled{"))
            .count();
        assert_eq!(self.samples.len(), 48 + 5 * workers);
        assert_eq!(self.duration_sums.len(), 2);
        assert!((2..=2 + workers).contains(&self.seconds.len()));
        assert_eq!(
            self.seconds["ww_executor_worker_heartbeat_interval_seconds"],
            0.25
        );
        assert_eq!(
            self.seconds["ww_executor_worker_stall_threshold_seconds"],
            5.0
        );
        assert!(
            self.body.len() < 10_240 + 768 * workers,
            "unbounded metrics response"
        );
    }

    fn assert_live_families_contiguous(&self) {
        let mut seen = BTreeSet::new();
        let mut current = None;
        for line in self.body.lines() {
            let content = line
                .strip_prefix("# HELP ")
                .or_else(|| line.strip_prefix("# TYPE "))
                .unwrap_or(line);
            let name = metric_family(content.split([' ', '{']).next().unwrap());
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

const LIFECYCLE_BUCKETS: [&str; 10] = [
    "0.0001", "0.001", "0.01", "0.1", "0.5", "1", "2", "5", "10", "+Inf",
];

fn metric_family(name: &str) -> &str {
    for suffix in ["_bucket", "_count", "_sum"] {
        if let Some(family) = name.strip_suffix(suffix) {
            return family;
        }
    }
    name
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
async fn fuel_http_pending_store_lags_until_cancelled_owner_finishes_teardown() {
    let fuel = FuelMetrics::default();
    let admin = Admin::with_metrics(ExecutorMetrics::default(), fuel.clone());
    let client = reqwest::Client::builder()
        .no_proxy()
        .timeout(Duration::from_secs(5))
        .build()
        .unwrap();
    let (runtime, _publisher) =
        cell::engine::runtime_engine_with_fuel_metrics(fuel.clone()).unwrap();
    let wasm = wat::parse_str(include_str!("fixtures/fuel-publication-component.wat")).unwrap();
    let (builder, _handles) = cell::Builder::ordinary(
        cell::Program::Bytes(wasm),
        tokio::io::empty(),
        tokio::io::sink(),
        tokio::io::sink(),
        support::filesystem_ownership(),
    );
    let proc = builder.with_runtime_engine(runtime).build().await.unwrap();
    let task = tokio::spawn(proc.run());
    tokio::time::timeout(Duration::from_secs(5), async {
        while fuel.snapshot().scheduled.grant < 9 {
            assert!(
                !task.is_finished(),
                "P3 command exited before the pending wait"
            );
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("eight marked P3 returns before the pending wait");

    let active = scrape(&client, &admin).await;
    assert_eq!(active.fuel("scheduled", "grant"), 9);
    assert_eq!(active.settled_fuel("scheduled"), 0);
    assert_eq!(active.settled_fuel("oneshot"), 0);
    assert!(!task.is_finished());
    // A second scrape with no Store progress still cannot publish local state.
    scrape(&client, &admin).await.assert_fuel_unchanged(&active);

    task.abort();
    let error = tokio::time::timeout(Duration::from_secs(5), task)
        .await
        .expect("cancelled Store teardown deadline")
        .expect_err("cancelled owner task");
    assert!(error.is_cancelled());
    let terminal = scrape(&client, &admin).await;
    // Eight fixture returns settle 6 * 8 - 2 = 46 units. The pending
    // wait's tail remains unsettled and contributes nothing during Drop.
    assert_eq!(terminal.settled_fuel("scheduled"), 46);
    assert_eq!(terminal.settled_fuel("oneshot"), 0);
    assert_eq!(terminal.fuel("scheduled", "grant"), 9);
    assert_eq!(terminal.fuel("scheduled", "suspend"), 0);
    assert_eq!(terminal.fuel("scheduled", "exhausted"), 0);
    scrape(&client, &admin)
        .await
        .assert_fuel_unchanged(&terminal);
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
        assert_eq!(initial.settled_fuel(policy), 0);
        for decision in ["grant", "suspend", "exhausted"] {
            assert_eq!(initial.fuel(policy, decision), 0);
        }
    }
    let (engine, mut publisher) = cell::engine::runtime_engine_with_fuel_metrics(fuel).unwrap();
    let wasm = wat::parse_str(include_str!("fixtures/spinning-component.wat")).unwrap();
    let scheduled_wasm = wat::parse_str(
        r#"(component
        (import "wasi:clocks/monotonic-clock@0.3.0" (instance $clock
            (export "now" (func (result u64)))))
        (core func $now (canon lower (func $clock "now")))
        (core func $return (canon task.return (result (result))))
        (core module $guest
            (import "" "now" (func $now (result i64)))
            (import "" "return" (func $return (param i32)))
            (func (export "run") (local $left i32)
                i32.const 8 local.set $left
                (loop $loop
                    call $now drop
                    local.get $left i32.const 1 i32.sub local.tee $left br_if $loop)
                i32.const 0 call $return))
        (core instance $i (instantiate $guest (with "" (instance
            (export "now" (func $now)) (export "return" (func $return))))))
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
    // Each clock call marks a host-return settlement after guest instructions.
    assert!(after_scheduled.settled_fuel("scheduled") >= 8);
    assert_eq!(after_scheduled.settled_fuel("oneshot"), 0);

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
    assert!(first_wait.settled_fuel("oneshot") >= 10_000);
    assert_eq!(
        first_wait.settled_fuel("scheduled"),
        after_scheduled.settled_fuel("scheduled")
    );
    publisher.tick();
    tokio::time::timeout(Duration::from_secs(5), observer.wait_for_suspension(2))
        .await
        .unwrap();
    let second_wait = scrape(&client, &admin).await;
    assert_eq!(second_wait.fuel("oneshot", "suspend"), 2);
    assert!(second_wait.fuel("oneshot", "grant") > first_wait.fuel("oneshot", "grant"));
    assert!(second_wait.settled_fuel("oneshot") > first_wait.settled_fuel("oneshot"));
    assert_eq!(
        second_wait.settled_fuel("scheduled"),
        after_scheduled.settled_fuel("scheduled")
    );
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
    assert!(terminal.settled_fuel("oneshot") > second_wait.settled_fuel("oneshot"));
    // Physical checkpoint overshoot can make settled consumption exceed authority.
    assert!(terminal.settled_fuel("oneshot") >= 25_000);
    assert_eq!(
        terminal.settled_fuel("scheduled"),
        after_scheduled.settled_fuel("scheduled")
    );
    drop(engine);
    drop(publisher);
    scrape(&client, &admin)
        .await
        .assert_fuel_unchanged(&terminal);
    let independent = scrape(&client, &other_admin).await;
    for policy in ["scheduled", "oneshot"] {
        assert_eq!(
            independent.settled_fuel(policy),
            0,
            "independent Host collector"
        );
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
                let before = scrape(&client, &admin).await;
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
                let after = scrape(&client, &admin).await;
                if budget == 0 {
                    assert_eq!(
                        after.settled_fuel("oneshot"),
                        before.settled_fuel("oneshot")
                    );
                } else {
                    assert!(after.settled_fuel("oneshot") > before.settled_fuel("oneshot"));
                }
            }
            let terminal = scrape(&client, &admin).await;
            assert_eq!(terminal.fuel("oneshot", "grant"), 1);
            assert_eq!(terminal.fuel("oneshot", "exhausted"), 2);
            assert_eq!(terminal.fuel("oneshot", "suspend"), 0);
            assert_eq!(terminal.fuel("scheduled", "grant"), 0);
            assert_eq!(terminal.settled_fuel("scheduled"), 0);
            assert!(terminal.settled_fuel("oneshot") > 0);
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
async fn worker_stall_is_observed_over_http_without_changing_probe_contracts() {
    let metrics = ExecutorMetrics::default();
    let admin = Admin::with_executor_metrics(metrics.clone());
    admin.kernel_ready_gate.bind_generation(0);
    admin.kernel_ready_gate.kernel_ready().unwrap();
    let client = reqwest::Client::builder()
        .no_proxy()
        .timeout(Duration::from_secs(3))
        .build()
        .unwrap();
    let before_pool = scrape(&client, &admin).await;
    before_pool.assert_unsupported_absent();
    assert!(!before_pool
        .samples
        .keys()
        .any(|name| name.starts_with("ww_executor_worker_")));

    let (_shutdown, shutdown_rx) = watch::channel(());
    let pool = ExecutorPool::try_new_with_metrics(
        2,
        shutdown_rx,
        support::filesystem_resources(),
        metrics.clone(),
    )
    .unwrap();
    tokio::time::timeout(Duration::from_secs(5), async {
        while metrics
            .worker_liveness()
            .iter()
            .any(|worker| worker.progress_age.is_none())
        {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("both workers confirm initial progress");

    let (entered_tx, entered) = oneshot::channel();
    let (release, released) = std::sync::mpsc::channel();
    let completed = submit(
        &pool,
        "private-liveness-blocked-cell",
        Box::new(move |_| {
            let thread = std::thread::current();
            let worker: usize = thread
                .name()
                .unwrap()
                .strip_prefix("executor-")
                .unwrap()
                .parse()
                .unwrap();
            entered_tx.send(worker).unwrap();
            // Independent OS timeout and sender-drop cleanup bound a test failure.
            // The entry acknowledgement establishes blocking order, not a sleep.
            let _ = released.recv_timeout(Duration::from_secs(20));
            Box::pin(async {})
        }),
    );
    let blocked_worker = tokio::time::timeout(Duration::from_secs(5), entered)
        .await
        .expect("worker entered blocking operation")
        .unwrap();
    let stalled_name = format!("ww_executor_worker_stalled{{worker=\"{blocked_worker}\"}}");
    let age_name =
        format!("ww_executor_worker_progress_age_seconds{{worker=\"{blocked_worker}\"}}");
    let stalled = tokio::time::timeout(Duration::from_secs(12), async {
        loop {
            let observed = scrape(&client, &admin).await;
            if observed.samples[&stalled_name] == 1 {
                break observed;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("independent admin observes worker stall");
    stalled.assert_unsupported_absent();
    stalled.assert_live_families_contiguous();
    assert!(stalled.seconds[&age_name] >= 5.0);
    assert_eq!(
        stalled.samples[&format!(
            "ww_executor_worker_stalled{{worker=\"{}\"}}",
            1 - blocked_worker
        )],
        0
    );
    assert!(!stalled.body.contains("private-liveness"));
    let base = admin.url.strip_suffix("/metrics").unwrap();
    let healthy = client.get(format!("{base}/healthz")).send().await.unwrap();
    assert_eq!(healthy.status(), reqwest::StatusCode::OK);
    assert_eq!(healthy.text().await.unwrap(), "ok\n");
    let ready = client.get(format!("{base}/readyz")).send().await.unwrap();
    assert_eq!(ready.status(), reqwest::StatusCode::OK);
    assert_eq!(
        ready.json::<serde_json::Value>().await.unwrap()["ready"],
        true
    );

    release.send(()).unwrap();
    cell_result(completed).await.unwrap();
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let recovered = scrape(&client, &admin).await;
            if recovered.samples[&stalled_name] == 0 {
                assert!(recovered.seconds[&age_name] < 5.0);
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("a new publication clears the stall");
    pool.shutdown_and_join().await;
    let stopped = scrape(&client, &admin).await;
    for worker in 0..2 {
        assert_eq!(
            stopped.samples
                [&format!("ww_executor_worker_state{{worker=\"{worker}\",state=\"stopped\"}}")],
            1
        );
        assert_eq!(
            stopped.samples[&format!("ww_executor_worker_stalled{{worker=\"{worker}\"}}")],
            0
        );
        assert!(stopped.seconds.contains_key(&format!(
            "ww_executor_worker_progress_age_seconds{{worker=\"{worker}\"}}"
        )));
    }
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
    // Concurrent P3 tests can change the process-wide compilation counters.
    // This pool's executor counters must retain their exact terminal values.
    for _ in 0..8 {
        scrape(&client, &admin)
            .await
            .assert_executor(0, [4, 0, 1], [1, 2, 1]);
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
        empty.assert_lifecycle([0, 0], [0, 0, 0]);
        empty.assert_executor(0, [0, 0, 0], [0, 0, 0]);
        repeat_scrapes(&client, &admin, &mode, "before_compile", measure).await;

        let wasm = wat::parse_str("(component)").unwrap();
        compile_component(&wasm_engine().unwrap(), &wasm).expect("cold compilation");
        let cold = metrics.snapshot();
        assert_eq!(cold.component_compilations, 1);
        assert_eq!(cold.hits, 0);
        assert_eq!(cold.stores, u64::from(mode == "enabled"));
        let cold_scrape = scrape(&client, &admin).await;
        cold_scrape.assert_snapshot(cold);
        cold_scrape.assert_lifecycle([1, 0], [0, 0, 0]);

        // A new engine uses the same process cache; a hit is still an attempt.
        compile_component(&wasm_engine().unwrap(), &wasm).expect("second compilation");
        let warm = metrics.snapshot();
        assert_eq!(warm.component_compilations, 2);
        assert_eq!(warm.hits, u64::from(mode == "enabled"));
        assert_eq!(warm.stores, cold.stores);
        let warm_scrape = scrape(&client, &admin).await;
        warm_scrape.assert_snapshot(warm);
        warm_scrape.assert_lifecycle([2, 0], [0, 0, 0]);

        let before_error_duration = metrics.lifecycle_snapshot().compilation.duration.sum_nanos;
        assert!(compile_component(&wasm_engine().unwrap(), b"not wasm").is_err());
        assert!(
            metrics.lifecycle_snapshot().compilation.duration.sum_nanos > before_error_duration
        );
        let failed = metrics.snapshot();
        assert_eq!(failed.component_compilations, 3);
        assert_eq!((failed.hits, failed.stores), (warm.hits, warm.stores));
        assert_eq!(failed.state, initial.state);
        let populated = scrape(&client, &admin).await;
        populated.assert_snapshot(failed);
        populated.assert_lifecycle([2, 1], [0, 0, 0]);
        repeat_scrapes(&client, &admin, &mode, "after_compile", measure).await;

        empty.assert_unsupported_absent();
        populated.assert_unsupported_absent();
        empty.assert_live_families_contiguous();
        populated.assert_live_families_contiguous();
    });
}

#[test]
fn lifecycle_http_distinguishes_preparation_instantiation_and_command_execution() {
    // Global lifecycle totals need a process isolated from other P3 tests.
    let mut child = Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "lifecycle_http_probe",
            "--ignored",
            "--nocapture",
        ])
        .env_remove(CWASM_DIR_ENV)
        .env_remove(CWASM_CACHE_MAX_BYTES_ENV)
        .spawn()
        .expect("spawn lifecycle HTTP probe");
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        if let Some(status) = child.try_wait().unwrap() {
            assert!(status.success(), "lifecycle HTTP probe failed: {status}");
            return;
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            panic!("lifecycle HTTP probe exceeded 60 seconds");
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

fn lifecycle_command(start: &str, exit_status: i32) -> Vec<u8> {
    wat::parse_str(format!(
        r#"(component
          (core func $task-return (canon task.return (result (result))))
          (core module $command
            (import "" "task-return" (func $task-return (param i32)))
            {start}
            (func (export "run")
              i32.const {exit_status}
              call $task-return))
          (core instance $instance
            (instantiate $command
              (with "" (instance (export "task-return" (func $task-return))))))
          (func $run async (result (result))
            (canon lift (core func $instance "run") async))
          (instance (export (interface "wasi:cli/run@0.3.0"))
            (export "run" (func $run))))"#,
    ))
    .unwrap()
}

fn lifecycle_builder(
    program: cell::Program,
    runtime: &cell::engine::RuntimeEngine,
) -> cell::Builder {
    let (builder, _handles) = cell::Builder::ordinary(
        program,
        tokio::io::empty(),
        tokio::io::sink(),
        tokio::io::sink(),
        support::filesystem_ownership(),
    );
    builder.with_runtime_engine(runtime.clone())
}

async fn wait_for_start_suspension(
    task: &tokio::task::JoinHandle<anyhow::Result<()>>,
    fuel: &FuelMetrics,
    expected: u64,
) {
    tokio::time::timeout(Duration::from_secs(2), async {
        while fuel.snapshot().oneshot.suspend < expected {
            assert!(!task.is_finished(), "start completed before suspension");
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("guest start reached the epoch wait");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "invoked only by the isolated lifecycle HTTP subprocess"]
async fn lifecycle_http_probe() {
    use cell::proc::{FuelEstimator, ValidatedKernel};
    use cell::Program;

    let metrics = wasmtime_cache_metrics();
    let admin = Admin::start();
    let client = reqwest::Client::builder()
        .no_proxy()
        .timeout(Duration::from_secs(5))
        .build()
        .unwrap();
    let (runtime, publisher) = cell::engine::runtime_engine().unwrap();
    scrape(&client, &admin)
        .await
        .assert_lifecycle([0, 0], [0, 0, 0]);

    let bytes = lifecycle_command("", 0);
    let proc = lifecycle_builder(Program::Bytes(bytes.clone()), &runtime)
        .build()
        .await
        .unwrap();
    // Building compiles and validates imports/exports, but executes no guest start.
    scrape(&client, &admin)
        .await
        .assert_lifecycle([1, 0], [0, 0, 0]);
    proc.run().await.unwrap();
    scrape(&client, &admin)
        .await
        .assert_lifecycle([1, 0], [1, 0, 0]);

    let precompiled = Arc::new(compile_component(&runtime.engine(), &bytes).unwrap());
    lifecycle_builder(Program::Precompiled(precompiled.clone()), &runtime)
        .build()
        .await
        .unwrap()
        .run()
        .await
        .unwrap();
    scrape(&client, &admin)
        .await
        .assert_lifecycle([2, 0], [2, 0, 0]);

    // Valid binary, missing wasi:cli/run: compilation succeeds, pre-validation fails.
    assert!(lifecycle_builder(
        Program::Bytes(wat::parse_str("(component)").unwrap()),
        &runtime
    )
    .build()
    .await
    .is_err());
    scrape(&client, &admin)
        .await
        .assert_lifecycle([3, 0], [2, 0, 0]);

    let trap_bytes = lifecycle_command("(func $start unreachable) (start $start)", 0);
    let trapped = Arc::new(compile_component(&runtime.engine(), &trap_bytes).unwrap());
    let proc = lifecycle_builder(Program::Precompiled(trapped.clone()), &runtime)
        .build()
        .await
        .expect("static validation must not execute start");
    scrape(&client, &admin)
        .await
        .assert_lifecycle([4, 0], [2, 0, 0]);
    let before_trap = metrics
        .lifecycle_snapshot()
        .instantiation
        .duration
        .sum_nanos;
    let error = proc.run().await.expect_err("start must trap");
    assert!(format!("{error:#}").contains("unreachable"));
    scrape(&client, &admin)
        .await
        .assert_lifecycle([4, 0], [2, 1, 0]);
    assert!(
        metrics
            .lifecycle_snapshot()
            .instantiation
            .duration
            .sum_nanos
            > before_trap
    );

    // Initial authority rejection happens before executable instantiation.
    let error = lifecycle_builder(Program::Precompiled(trapped), &runtime)
        .with_fuel_estimator(FuelEstimator::new_oneshot(0, 0, 0))
        .build()
        .await
        .unwrap()
        .run()
        .await
        .expect_err("zero fuel authority");
    assert!(error
        .to_string()
        .contains("one-shot total fuel authority exhausted"));
    scrape(&client, &admin)
        .await
        .assert_lifecycle([4, 0], [2, 1, 0]);

    let error = lifecycle_builder(Program::Bytes(lifecycle_command("", 1)), &runtime)
        .build()
        .await
        .unwrap()
        .run()
        .await
        .expect_err("nonzero guest exit");
    assert!(error
        .to_string()
        .contains("guest returned non-zero exit status"));
    scrape(&client, &admin)
        .await
        .assert_lifecycle([5, 0], [3, 1, 0]);

    let kernel_bytes = wat::parse_str(include_str!(
        "../crates/cell/tests/fixtures/kernel-command.wat"
    ))
    .unwrap();
    let validated = Arc::new(ValidatedKernel::compile(&runtime, &kernel_bytes).unwrap());
    scrape(&client, &admin)
        .await
        .assert_lifecycle([6, 0], [3, 1, 0]);
    let staging = tempfile::tempdir().unwrap();
    let filesystem = support::filesystem_ownership();
    let cid = cell::routing_key::derive(b"lifecycle test root");
    let tree = Arc::new(cell::vfs::CidTree::new(
        cid,
        ipfs::HttpClient::new("http://127.0.0.1:1".into()),
        staging.path().to_owned(),
        Arc::clone(filesystem.test_resources().host()),
        filesystem.test_scope().clone(),
    ));
    let (_epoch_tx, epoch_rx) = watch::channel(authority::Epoch {
        seq: 1,
        head: cid.to_bytes(),
        root: Some(cid),
    });
    let gate = Arc::new(authority::KernelReadyGate::new(epoch_rx));
    let (builder, _handles) = cell::Builder::kernel(
        Program::ValidatedKernel(validated),
        tokio::io::empty(),
        tokio::io::sink(),
        tokio::io::sink(),
        tree,
        gate,
        filesystem,
    );
    let proc = builder
        .with_runtime_engine(runtime.clone())
        .build()
        .await
        .unwrap();
    scrape(&client, &admin)
        .await
        .assert_lifecycle([6, 0], [3, 1, 0]);
    proc.run().await.unwrap();
    scrape(&client, &admin)
        .await
        .assert_lifecycle([6, 0], [4, 1, 0]);

    let mut tasks = tokio::task::JoinSet::new();
    for _ in 0..4 {
        let runtime = runtime.clone();
        let precompiled = precompiled.clone();
        tasks.spawn(async move {
            lifecycle_builder(Program::Precompiled(precompiled), &runtime)
                .build()
                .await
                .unwrap()
                .run()
                .await
                .unwrap();
        });
    }
    while let Some(result) = tasks.join_next().await {
        result.unwrap();
    }
    scrape(&client, &admin)
        .await
        .assert_lifecycle([6, 0], [8, 1, 0]);

    let counted_start = lifecycle_command(
        r#"(func $start (local $remaining i32)
            i32.const 3000 local.set $remaining
            (loop $spin local.get $remaining i32.const 1 i32.sub
                local.tee $remaining br_if $spin)) (start $start)"#,
        0,
    );
    let counted_start = Arc::new(compile_component(&runtime.engine(), &counted_start).unwrap());
    let fuel = runtime.fuel_metrics();
    let proc = lifecycle_builder(Program::Precompiled(counted_start.clone()), &runtime)
        .with_fuel_estimator(FuelEstimator::new_oneshot(25_000, 12_500, 0))
        .build()
        .await
        .unwrap();
    let task = tokio::spawn(proc.run());
    wait_for_start_suspension(&task, &fuel, 1).await;
    let suspended = scrape(&client, &admin).await;
    suspended.assert_lifecycle([7, 0], [8, 1, 0]);
    let before_cancel = metrics.lifecycle_snapshot();
    task.abort();
    assert!(task.await.unwrap_err().is_cancelled());
    assert_eq!(
        metrics.lifecycle_snapshot(),
        before_cancel,
        "dropping pending work must not invent an outcome or duration"
    );
    scrape(&client, &admin)
        .await
        .assert_lifecycle([7, 0], [8, 1, 0]);

    // Retain the publisher but never tick: instantiation waits for a new epoch.
    // Use real elapsed time so the duration assertion also verifies seconds units.
    let proc = lifecycle_builder(Program::Precompiled(counted_start), &runtime)
        .with_fuel_estimator(FuelEstimator::new_oneshot(25_000, 12_500, 0))
        .build()
        .await
        .unwrap();
    let task = tokio::spawn(proc.run());
    wait_for_start_suspension(&task, &fuel, 2).await;
    scrape(&client, &admin)
        .await
        .assert_lifecycle([7, 0], [8, 1, 0]);
    let error = tokio::time::timeout(Duration::from_secs(10), task)
        .await
        .expect("instantiation timeout deadline")
        .unwrap()
        .expect_err("start must time out");
    assert_eq!(error.to_string(), "guest component instantiation timed out");
    let timed_out = metrics.lifecycle_snapshot();
    assert!(
        timed_out.instantiation.duration.sum_nanos - before_cancel.instantiation.duration.sum_nanos
            >= 5_000_000_000
    );
    let terminal = scrape(&client, &admin).await;
    terminal.assert_lifecycle([7, 0], [8, 1, 1]);
    terminal.assert_unsupported_absent();
    terminal.assert_live_families_contiguous();

    drop((runtime, publisher, precompiled));
    let retained = Admin::start();
    scrape(&client, &retained)
        .await
        .assert_lifecycle([7, 0], [8, 1, 1]);
    assert_eq!(
        metrics.lifecycle_snapshot(),
        timed_out,
        "engine teardown must retain process totals"
    );
    if std::env::var_os("WW_METRICS_HTTP_REQUIRE_COMPILED_P3").is_some() {
        compiled_p3_lifecycle_http(&client, &admin).await;
    }
    println!(
        "LIFECYCLE_HTTP_EVIDENCE={}",
        serde_json::json!({
            "samples": terminal.samples, "duration_sums_seconds": terminal.duration_sums,
            "body_bytes": terminal.body.len(), "cancelled_pending_observations": 0,
        })
    );
}

/// Opt in with WW_METRICS_HTTP_REQUIRE_COMPILED_P3=1 after `make authority-probe`.
/// The explicit request fails if the compiled Rust native P3 artifact is missing.
async fn compiled_p3_lifecycle_http(client: &reqwest::Client, admin: &Admin) {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let artifact = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("target/authority-probe/wasm32-wasip3/release/authority_probe.wasm");
    let bytes = std::fs::read(&artifact).unwrap_or_else(|error| {
        panic!(
            "read compiled P3 fixture {}: {error}; run `make authority-probe`",
            artifact.display()
        )
    });
    let (runtime, mut publisher) = cell::engine::runtime_engine().unwrap();
    let (mut stdout_read, stdout_write) = tokio::io::duplex(64 * 1024);
    let (builder, mut handles) = cell::Builder::ordinary(
        cell::Program::Bytes(bytes),
        tokio::io::empty(),
        stdout_write,
        tokio::io::sink(),
        support::filesystem_ownership(),
    );
    let mut transport = handles.take_host_stream().unwrap();
    let proc = builder
        .with_runtime_engine(runtime)
        .with_args(vec!["authority-probe".into(), "no-graft".into()])
        .build()
        .await
        .expect("prepare compiled Rust P3 command");
    scrape(client, admin)
        .await
        .assert_lifecycle([8, 0], [8, 1, 1]);

    let guest = async {
        let run = proc.run();
        tokio::pin!(run);
        let mut interval = tokio::time::interval(Duration::from_millis(10));
        loop {
            tokio::select! {
                result = &mut run => break result,
                _ = interval.tick() => { publisher.tick(); }
            }
        }
    };
    let transport_io = async {
        let mut discarded = Vec::new();
        transport.read_to_end(&mut discarded).await.unwrap();
        transport.shutdown().await.unwrap();
    };
    let output = async {
        let mut bytes = Vec::new();
        stdout_read.read_to_end(&mut bytes).await.unwrap();
        bytes
    };
    let (result, (), output) = tokio::time::timeout(Duration::from_secs(10), async {
        tokio::join!(guest, transport_io, output)
    })
    .await
    .expect("compiled P3 command completion deadline");
    result.expect("compiled Rust P3 command succeeds");
    let output: serde_json::Value = serde_json::from_slice(&output).expect("native P3 stdout JSON");
    assert_eq!(output["mode"], "no-graft");
    assert_eq!(output["ok"], true);
    let terminal = scrape(client, admin).await;
    terminal.assert_lifecycle([8, 0], [9, 1, 1]);
    println!(
        "COMPILED_P3_LIFECYCLE_HTTP_EVIDENCE={}",
        serde_json::json!({
            "artifact": artifact, "guest_stdout": output,
            "samples": terminal.samples, "duration_sums_seconds": terminal.duration_sums,
        })
    );
}
