//! Real admin HTTP scrapes against the process-global Wasmtime producer.
//! Each cache policy runs in a subprocess because EngineFactory reads env once.
#![cfg(not(target_arch = "wasm32"))]

use std::collections::{BTreeMap, BTreeSet};
use std::net::TcpListener;
use std::process::Command;
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};

use cell::engine::{
    compile_component, wasm_engine, wasmtime_cache_metrics, WasmtimeCacheSnapshot,
    CWASM_CACHE_MAX_BYTES_ENV, CWASM_DIR_ENV,
};
use tokio::sync::watch;
use ww::metrics::{
    new_cache_metrics, new_fuel_registry, new_rpc_metrics, new_stream_metrics, AdminService,
    RuntimeStatus, VersionInfo,
};
use ww::services::Service;

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

#[test]
fn poisoned_registries_do_not_break_live_http_collection() {
    run_probe("poisoned", false);
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
        "disabled" | "poisoned" => {}
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
    fn start(poisoned: bool) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let url = format!("http://{}/metrics", listener.local_addr().unwrap());
        let (_epoch_tx, epoch_rx) = watch::channel(authority::Epoch::zero());
        let fuel = new_fuel_registry();
        let rpc = new_rpc_metrics();
        let cache = new_cache_metrics();
        let stream = new_stream_metrics();
        if poisoned {
            poison(&fuel);
            poison(&rpc);
            poison(&cache);
            poison(&stream);
        }
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
            fuel_registry: fuel,
            rpc_metrics: rpc,
            cache_metrics: cache,
            stream_metrics: stream,
            wasmtime_cache_metrics: wasmtime_cache_metrics(),
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

fn poison<T>(registry: &Arc<RwLock<T>>) {
    // Deliberate failure injection only; no fabricated producer observations.
    assert!(std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let _guard = registry.write().unwrap();
        panic!("intentional registry poisoning");
    }))
    .is_err());
    assert!(registry.is_poisoned());
}

#[derive(Debug)]
struct Scrape {
    body: String,
    samples: BTreeMap<String, u64>,
    http_elapsed: Duration,
}

impl Scrape {
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
                // The live producer has integer samples and one bounded label.
                // This is a parser for this endpoint's live subset, not a general
                // Prometheus parser for future fuel/RPC observations.
                let (identity, value) = line.split_once(' ').expect("sample value");
                let name = identity.split('{').next().unwrap();
                assert!(name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_'));
                assert!(types.contains_key(name), "sample without TYPE: {line}");
                if identity.contains('{') {
                    assert_eq!(name, "ww_wasmtime_cache_state");
                    assert!(["enabled", "disabled", "fallback"]
                        .iter()
                        .any(|state| { identity == format!("{name}{{state=\"{state}\"}}") }));
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
            ("ww_cell_fuel_remaining", "gauge"),
            ("ww_cell_fuel_consumed_total", "counter"),
            ("ww_rpc_calls_total", "counter"),
            ("ww_rpc_duration_seconds", "histogram"),
            ("ww_cache_hits_total", "counter"),
            ("ww_cache_misses_total", "counter"),
            ("ww_cache_evictions_total", "counter"),
            ("ww_cache_weight_bytes", "gauge"),
            ("ww_cache_entries", "gauge"),
            ("ww_stream_bytes_pumped_total", "counter"),
            ("ww_stream_pump_ops_total", "counter"),
            ("ww_wasmtime_cache_state", "gauge"),
            ("ww_wasmtime_cache_hits_total", "counter"),
            ("ww_wasmtime_cache_stores_total", "counter"),
            ("ww_wasmtime_component_compilations_total", "counter"),
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

    fn assert_unsupported_absent(&self) {
        for name in self.samples.keys() {
            assert!(
                name.starts_with("ww_wasmtime_"),
                "unsupported sample exposed: {name}"
            );
        }
        assert_eq!(self.samples.len(), 6);
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
            if !name.starts_with("ww_wasmtime_") {
                continue;
            }
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
    let admin = Admin::start(mode == "poisoned");
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
        let expected_state = if mode == "poisoned" {
            "disabled"
        } else {
            mode.as_str()
        };
        assert_eq!(initial.state.as_str(), expected_state);
        assert_eq!(
            (initial.hits, initial.stores, initial.component_compilations),
            (0, 0, 0)
        );
        let empty = scrape(&client, &admin).await;
        empty.assert_snapshot(initial);
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

        // Measurement mode also works against the unfixed baseline, recording
        // its unsupported samples without accepting them in the contract tests.
        if !measure {
            empty.assert_unsupported_absent();
            populated.assert_unsupported_absent();
            empty.assert_live_families_contiguous();
            populated.assert_live_families_contiguous();
        }
    });
}
