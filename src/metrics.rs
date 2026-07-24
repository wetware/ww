//! Admin HTTP server for node introspection.
//!
//! Serves liveness/readiness checks at `GET /healthz` and `GET /readyz`,
//! build provenance at `GET /version`, Prometheus metrics at `GET /metrics`,
//! and host identity/address/NAT information at `GET /host/id`,
//! `GET /host/addrs`, and `GET /host/nat`.
//!
//! Fuel metrics (`ww_cell_fuel_remaining`, `ww_cell_fuel_consumed_total`)
//! are live from host-side [`FuelEstimator`] state.  Auction-specific
//! metrics.

use std::collections::HashMap;
use std::sync::{Arc, RwLock};

use axum::{extract::State, http::StatusCode, response::IntoResponse, routing::get, Router};
use tokio::sync::watch;

use crate::cell::engine::{WasmtimeCacheMetrics, WasmtimeCacheSnapshot, WasmtimeCacheState};

/// Immutable build and artifact identity exposed by `GET /version`.
#[derive(Clone, Debug, serde::Serialize)]
pub struct VersionInfo {
    pub git_sha: String,
    pub oci_image_id: Option<String>,
    pub kernel_wasm_blake3: Option<String>,
    pub shell_wasm_blake3: Option<String>,
}

/// Mutable process readiness shared between the boot path and admin server.
#[derive(Clone, Debug)]
pub struct RuntimeStatus {
    inner: Arc<RwLock<RuntimeStatusSnapshot>>,
}

#[derive(Clone, Debug, serde::Serialize)]
struct RuntimeStatusSnapshot {
    ready: bool,
    phase: String,
    degraded: bool,
    degraded_reasons: Vec<String>,
}

impl RuntimeStatus {
    pub fn starting() -> Self {
        Self {
            inner: Arc::new(RwLock::new(RuntimeStatusSnapshot {
                ready: false,
                phase: "starting".to_string(),
                degraded: false,
                degraded_reasons: Vec::new(),
            })),
        }
    }

    pub fn set_phase(&self, phase: impl Into<String>) {
        if let Ok(mut status) = self.inner.write() {
            status.phase = phase.into();
            status.ready = false;
        }
    }

    pub fn set_ready(&self) {
        if let Ok(mut status) = self.inner.write() {
            status.phase = "ready".to_string();
            status.ready = true;
        }
    }

    pub fn mark_degraded(&self, reason: impl Into<String>) {
        if let Ok(mut status) = self.inner.write() {
            let reason = reason.into();
            status.degraded = true;
            if !status.degraded_reasons.contains(&reason) {
                status.degraded_reasons.push(reason);
            }
        }
    }

    fn snapshot(&self) -> RuntimeStatusSnapshot {
        self.inner
            .read()
            .map(|status| status.clone())
            .unwrap_or_else(|_| RuntimeStatusSnapshot {
                ready: false,
                phase: "status-unavailable".to_string(),
                degraded: true,
                degraded_reasons: vec!["runtime status lock poisoned".to_string()],
            })
    }
}

// ---------------------------------------------------------------------------
// Per-cell fuel snapshot
// ---------------------------------------------------------------------------

/// A point-in-time snapshot of a cell's fuel state, published by the epoch
/// callback and consumed by the metrics scrape handler.
#[derive(Clone, Debug)]
pub struct CellFuelSnapshot {
    /// Fuel remaining in the current epoch budget.
    pub remaining: u64,
    /// Cumulative fuel consumed over the cell's lifetime.
    pub consumed_total: u64,
}

/// Shared registry of per-cell fuel snapshots.
///
/// Keys are cell identifiers (e.g. "kernel", or a CID-derived name for
/// spawned children).  The epoch callback writes; the metrics handler reads.
pub type FuelRegistry = Arc<RwLock<HashMap<String, CellFuelSnapshot>>>;

/// Create a new, empty fuel registry.
pub fn new_fuel_registry() -> FuelRegistry {
    Arc::new(RwLock::new(HashMap::new()))
}

// ---------------------------------------------------------------------------
// RPC latency histogram (Prometheus native format)
// ---------------------------------------------------------------------------

/// Fixed-bucket Prometheus histogram for RPC call latency.
/// Constant memory: 9 bucket counters + sum + count per method.
#[derive(Clone, Debug)]
pub struct LatencyHistogram {
    /// (le_boundary, count) pairs. Last entry is +Inf.
    buckets: [(f64, u64); 9],
    sum: f64,
    count: u64,
}

const HISTOGRAM_BOUNDARIES: [f64; 8] = [0.001, 0.005, 0.01, 0.05, 0.1, 0.5, 1.0, 5.0];

impl LatencyHistogram {
    fn new() -> Self {
        let mut buckets = [(0.0, 0u64); 9];
        for (i, &le) in HISTOGRAM_BOUNDARIES.iter().enumerate() {
            buckets[i].0 = le;
        }
        buckets[8].0 = f64::INFINITY; // +Inf bucket
        Self {
            buckets,
            sum: 0.0,
            count: 0,
        }
    }

    /// Record an observation (duration in seconds).
    pub fn observe(&mut self, value: f64) {
        self.sum += value;
        self.count += 1;
        for bucket in &mut self.buckets {
            if value <= bucket.0 {
                bucket.1 += 1;
            }
        }
    }
}

/// Per-method RPC metrics: call counts + latency histograms.
pub struct RpcMetrics {
    pub histograms: HashMap<String, LatencyHistogram>,
    pub calls_total: HashMap<String, u64>,
}

impl RpcMetrics {
    fn new() -> Self {
        Self {
            histograms: HashMap::new(),
            calls_total: HashMap::new(),
        }
    }

    /// Record an RPC call with its duration in seconds.
    pub fn observe(&mut self, method: &str, duration_secs: f64) {
        *self.calls_total.entry(method.to_string()).or_insert(0) += 1;
        self.histograms
            .entry(method.to_string())
            .or_insert_with(LatencyHistogram::new)
            .observe(duration_secs);
    }
}

pub type RpcMetricsRegistry = Arc<RwLock<RpcMetrics>>;

/// Create a new, empty RPC metrics registry.
pub fn new_rpc_metrics() -> RpcMetricsRegistry {
    Arc::new(RwLock::new(RpcMetrics::new()))
}

/// Cache hit/miss/eviction counters + current state gauges.
pub struct CacheMetrics {
    pub hits: u64,
    pub misses: u64,
    pub evictions: u64,
    pub weight_bytes: u64,
    pub entries: u64,
}

impl CacheMetrics {
    fn new() -> Self {
        Self {
            hits: 0,
            misses: 0,
            evictions: 0,
            weight_bytes: 0,
            entries: 0,
        }
    }
}

pub type CacheMetricsRegistry = Arc<RwLock<CacheMetrics>>;

/// Create a new, empty cache metrics registry.
pub fn new_cache_metrics() -> CacheMetricsRegistry {
    Arc::new(RwLock::new(CacheMetrics::new()))
}

/// Stream pump throughput counters.
pub struct StreamMetrics {
    pub bytes_pumped_total: u64,
    pub pump_ops_total: u64,
}

impl StreamMetrics {
    fn new() -> Self {
        Self {
            bytes_pumped_total: 0,
            pump_ops_total: 0,
        }
    }
}

pub type StreamMetricsRegistry = Arc<RwLock<StreamMetrics>>;

/// Create a new, empty stream metrics registry.
pub fn new_stream_metrics() -> StreamMetricsRegistry {
    Arc::new(RwLock::new(StreamMetrics::new()))
}

// ---------------------------------------------------------------------------
// Metrics HTTP handler
// ---------------------------------------------------------------------------

/// Shared state for the admin axum handlers.
#[derive(Clone)]
struct AdminState {
    peer_id: String,
    network_state: rpc::NetworkState,
    version_info: VersionInfo,
    runtime_status: RuntimeStatus,
    fuel_registry: FuelRegistry,
    rpc_metrics: RpcMetricsRegistry,
    cache_metrics: CacheMetricsRegistry,
    stream_metrics: StreamMetricsRegistry,
    wasmtime_cache_metrics: WasmtimeCacheMetrics,
}

/// Render all metrics in Prometheus text exposition format.
fn render_metrics(state: &AdminState) -> String {
    let mut out = String::with_capacity(2048);

    // ---- Per-cell fuel metrics (Phase 1: live) ----

    out.push_str("# HELP ww_cell_fuel_remaining Per-cell remaining fuel budget.\n");
    out.push_str("# TYPE ww_cell_fuel_remaining gauge\n");

    out.push_str("# HELP ww_cell_fuel_consumed_total Per-cell cumulative fuel consumed.\n");
    out.push_str("# TYPE ww_cell_fuel_consumed_total counter\n");

    if let Ok(registry) = state.fuel_registry.read() {
        for (cell_id, snap) in registry.iter() {
            out.push_str(&format!(
                "ww_cell_fuel_remaining{{cell_id=\"{}\"}} {}\n",
                cell_id, snap.remaining,
            ));
            out.push_str(&format!(
                "ww_cell_fuel_consumed_total{{cell_id=\"{}\"}} {}\n",
                cell_id, snap.consumed_total,
            ));
        }
    }

    // ---- RPC metrics ----

    out.push_str("# HELP ww_rpc_calls_total Total RPC calls by method.\n");
    out.push_str("# TYPE ww_rpc_calls_total counter\n");

    out.push_str("# HELP ww_rpc_duration_seconds RPC call latency.\n");
    out.push_str("# TYPE ww_rpc_duration_seconds histogram\n");

    if let Ok(rpc) = state.rpc_metrics.read() {
        for (method, count) in &rpc.calls_total {
            out.push_str(&format!(
                "ww_rpc_calls_total{{method=\"{method}\"}} {count}\n",
            ));
        }
        for (method, hist) in &rpc.histograms {
            for &(le, count) in &hist.buckets {
                let le_str = if le.is_infinite() {
                    "+Inf".to_string()
                } else {
                    format!("{le}")
                };
                out.push_str(&format!(
                    "ww_rpc_duration_seconds_bucket{{method=\"{method}\",le=\"{le_str}\"}} {count}\n",
                ));
            }
            out.push_str(&format!(
                "ww_rpc_duration_seconds_sum{{method=\"{method}\"}} {}\n",
                hist.sum,
            ));
            out.push_str(&format!(
                "ww_rpc_duration_seconds_count{{method=\"{method}\"}} {}\n",
                hist.count,
            ));
        }
    }

    // ---- Cache metrics ----

    out.push_str("# HELP ww_cache_hits_total ARC cache hits.\n");
    out.push_str("# TYPE ww_cache_hits_total counter\n");
    out.push_str("# HELP ww_cache_misses_total ARC cache misses.\n");
    out.push_str("# TYPE ww_cache_misses_total counter\n");
    out.push_str("# HELP ww_cache_evictions_total ARC cache evictions.\n");
    out.push_str("# TYPE ww_cache_evictions_total counter\n");
    out.push_str("# HELP ww_cache_weight_bytes Current ARC cache weight in bytes.\n");
    out.push_str("# TYPE ww_cache_weight_bytes gauge\n");
    out.push_str("# HELP ww_cache_entries Current ARC cache entry count.\n");
    out.push_str("# TYPE ww_cache_entries gauge\n");

    if let Ok(cache) = state.cache_metrics.read() {
        out.push_str(&format!("ww_cache_hits_total {}\n", cache.hits));
        out.push_str(&format!("ww_cache_misses_total {}\n", cache.misses));
        out.push_str(&format!("ww_cache_evictions_total {}\n", cache.evictions));
        out.push_str(&format!("ww_cache_weight_bytes {}\n", cache.weight_bytes));
        out.push_str(&format!("ww_cache_entries {}\n", cache.entries));
    }

    render_wasmtime_cache_metrics(&mut out, &state.wasmtime_cache_metrics.snapshot());

    // ---- Stream metrics ----

    out.push_str(
        "# HELP ww_stream_bytes_pumped_total Total bytes pumped through stream listeners.\n",
    );
    out.push_str("# TYPE ww_stream_bytes_pumped_total counter\n");
    out.push_str("# HELP ww_stream_pump_ops_total Total pump read/write cycles.\n");
    out.push_str("# TYPE ww_stream_pump_ops_total counter\n");

    if let Ok(stream) = state.stream_metrics.read() {
        out.push_str(&format!(
            "ww_stream_bytes_pumped_total {}\n",
            stream.bytes_pumped_total,
        ));
        out.push_str(&format!(
            "ww_stream_pump_ops_total {}\n",
            stream.pump_ops_total,
        ));
    }

    out
}

fn render_wasmtime_cache_metrics(out: &mut String, snapshot: &WasmtimeCacheSnapshot) {
    out.push_str("# HELP ww_wasmtime_cache_state Wasmtime compilation cache state (one active state has value 1).\n");
    out.push_str("# TYPE ww_wasmtime_cache_state gauge\n");
    for state in [
        WasmtimeCacheState::Enabled,
        WasmtimeCacheState::Disabled,
        WasmtimeCacheState::Fallback,
    ] {
        let value = u8::from(snapshot.state == state);
        out.push_str(&format!(
            "ww_wasmtime_cache_state{{state=\"{}\"}} {value}\n",
            state.as_str()
        ));
    }

    out.push_str(
        "# HELP ww_wasmtime_cache_hits_total Successful Wasmtime compilation cache loads.\n",
    );
    out.push_str("# TYPE ww_wasmtime_cache_hits_total counter\n");
    out.push_str("# HELP ww_wasmtime_cache_stores_total Successful Wasmtime compilation cache stores; this is not a lookup-miss count.\n");
    out.push_str("# TYPE ww_wasmtime_cache_stores_total counter\n");
    out.push_str("# HELP ww_wasmtime_component_compilations_total Calls to ww's canonical Component::from_binary path; persistent-cache hits are included.\n");
    out.push_str("# TYPE ww_wasmtime_component_compilations_total counter\n");
    out.push_str(&format!("ww_wasmtime_cache_hits_total {}\n", snapshot.hits));
    out.push_str(&format!(
        "ww_wasmtime_cache_stores_total {}\n",
        snapshot.stores
    ));
    out.push_str(&format!(
        "ww_wasmtime_component_compilations_total {}\n",
        snapshot.component_compilations
    ));
}

/// `GET /metrics` handler.
async fn metrics_handler(State(state): State<AdminState>) -> impl IntoResponse {
    let body = render_metrics(&state);
    (
        [(
            axum::http::header::CONTENT_TYPE,
            "text/plain; version=0.0.4; charset=utf-8",
        )],
        body,
    )
}

/// `GET /healthz` — confirms that the localhost control plane is serving.
async fn healthz_handler() -> impl IntoResponse {
    (StatusCode::OK, "ok\n")
}

/// `GET /readyz` — reports whether the host has reached its serving phase.
async fn readyz_handler(State(state): State<AdminState>) -> impl IntoResponse {
    let status = state.runtime_status.snapshot();
    let code = if status.ready {
        StatusCode::OK
    } else {
        StatusCode::SERVICE_UNAVAILABLE
    };
    let payload = serde_json::to_string(&status).unwrap_or_else(|_| {
        r#"{"ready":false,"phase":"serialization-error","degraded":true}"#.to_string()
    });
    (
        code,
        [(
            axum::http::header::CONTENT_TYPE,
            "application/json; charset=utf-8",
        )],
        payload,
    )
}

/// `GET /version` — returns source, image, and embedded artifact provenance.
async fn version_handler(State(state): State<AdminState>) -> impl IntoResponse {
    let runtime = state.runtime_status.snapshot();
    let cache = state.wasmtime_cache_metrics.snapshot();
    let payload = serde_json::json!({
        "git_sha": state.version_info.git_sha,
        "oci_image_id": state.version_info.oci_image_id,
        "kernel_wasm_blake3": state.version_info.kernel_wasm_blake3,
        "shell_wasm_blake3": state.version_info.shell_wasm_blake3,
        "degraded": runtime.degraded || cache.state == WasmtimeCacheState::Fallback,
        "degraded_reasons": runtime.degraded_reasons,
        "wasmtime_cache_state": cache.state.as_str(),
    });
    (
        [(
            axum::http::header::CONTENT_TYPE,
            "application/json; charset=utf-8",
        )],
        payload.to_string(),
    )
}

// ---------------------------------------------------------------------------
// Host introspection handlers
// ---------------------------------------------------------------------------

/// `GET /host/id` — returns the node's peer ID as plain text.
async fn host_id_handler(State(state): State<AdminState>) -> impl IntoResponse {
    (
        [(
            axum::http::header::CONTENT_TYPE,
            "text/plain; charset=utf-8",
        )],
        state.peer_id,
    )
}

/// `GET /host/addrs` — returns the node's listen addresses, one per line.
async fn host_addrs_handler(State(state): State<AdminState>) -> impl IntoResponse {
    let snapshot = state.network_state.snapshot().await;
    let lines: Vec<String> = snapshot
        .listen_addrs
        .iter()
        .filter_map(|bytes| libp2p::Multiaddr::try_from(bytes.clone()).ok())
        .map(|a| a.to_string())
        .collect();
    (
        [(
            axum::http::header::CONTENT_TYPE,
            "text/plain; charset=utf-8",
        )],
        lines.join("\n"),
    )
}

/// `GET /host/nat` — returns node-level reachability and recent AutoNAT probe outcomes.
async fn host_nat_handler(State(state): State<AdminState>) -> impl IntoResponse {
    let snapshot = state.network_state.snapshot().await;
    let body = serde_json::json!({
        "nat_status": snapshot.nat_status,
        "recent_probes": snapshot.nat_probe_events,
    });
    let payload = serde_json::to_string(&body).unwrap_or_else(|_| "{}".to_string());
    (
        [(
            axum::http::header::CONTENT_TYPE,
            "application/json; charset=utf-8",
        )],
        payload,
    )
}

// ---------------------------------------------------------------------------
// AdminService (runtime::Service implementation)
// ---------------------------------------------------------------------------

/// A [`crate::services::Service`] that serves admin HTTP endpoints:
/// Prometheus metrics, host identity, and listen addresses.
pub struct AdminService {
    /// Already-bound listener. Binding occurs during host startup so a
    /// configured control-plane endpoint is guaranteed to be available.
    pub listener: std::net::TcpListener,
    pub peer_id: String,
    pub network_state: rpc::NetworkState,
    pub version_info: VersionInfo,
    pub runtime_status: RuntimeStatus,
    pub fuel_registry: FuelRegistry,
    pub rpc_metrics: RpcMetricsRegistry,
    pub cache_metrics: CacheMetricsRegistry,
    pub stream_metrics: StreamMetricsRegistry,
    pub wasmtime_cache_metrics: WasmtimeCacheMetrics,
}

impl crate::services::Service for AdminService {
    fn run(self, mut shutdown: watch::Receiver<()>) -> anyhow::Result<()> {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()?;
        let _span = tracing::info_span!("admin").entered();

        rt.block_on(async move {
            let state = AdminState {
                peer_id: self.peer_id,
                network_state: self.network_state,
                version_info: self.version_info,
                runtime_status: self.runtime_status,
                fuel_registry: self.fuel_registry,
                rpc_metrics: self.rpc_metrics,
                cache_metrics: self.cache_metrics,
                stream_metrics: self.stream_metrics,
                wasmtime_cache_metrics: self.wasmtime_cache_metrics,
            };

            let app = Router::new()
                .route("/healthz", get(healthz_handler))
                .route("/readyz", get(readyz_handler))
                .route("/version", get(version_handler))
                .route("/metrics", get(metrics_handler))
                .route("/host/id", get(host_id_handler))
                .route("/host/addrs", get(host_addrs_handler))
                .route("/host/nat", get(host_nat_handler))
                .with_state(state);

            let listener = tokio::net::TcpListener::from_std(self.listener)?;
            let local_addr = listener.local_addr()?;
            tracing::info!(%local_addr, "Admin server listening");

            axum::serve(listener, app)
                .with_graceful_shutdown(async move {
                    let _ = shutdown.changed().await;
                    tracing::info!("Admin server shutting down");
                })
                .await?;

            Ok(())
        })
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn test_state() -> AdminState {
        AdminState {
            peer_id: "12D3KooWTestPeerId".to_string(),
            network_state: rpc::NetworkState::new(),
            version_info: VersionInfo {
                git_sha: "0123456789abcdef".to_string(),
                oci_image_id: Some("sha256:image".to_string()),
                kernel_wasm_blake3: Some("kernel".to_string()),
                shell_wasm_blake3: Some("shell".to_string()),
            },
            runtime_status: RuntimeStatus::starting(),
            fuel_registry: new_fuel_registry(),
            rpc_metrics: new_rpc_metrics(),
            cache_metrics: new_cache_metrics(),
            stream_metrics: new_stream_metrics(),
            wasmtime_cache_metrics: crate::cell::engine::wasmtime_cache_metrics(),
        }
    }

    #[tokio::test]
    async fn healthz_returns_probe_contract() {
        let response = healthz_handler().await.into_response();
        assert_eq!(response.status(), StatusCode::OK);
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("healthz response body");
        assert_eq!(&body[..], b"ok\n");
    }

    #[tokio::test]
    async fn readyz_is_unavailable_until_runtime_is_ready() {
        let state = test_state();
        let response = readyz_handler(State(state.clone())).await.into_response();
        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);

        state.runtime_status.set_ready();
        let response = readyz_handler(State(state)).await.into_response();
        assert_eq!(response.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn version_reports_provenance_and_cache_degradation() {
        let state = test_state();
        let response = version_handler(State(state)).await.into_response();
        assert_eq!(response.status(), StatusCode::OK);
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("version response body");
        let value: serde_json::Value = serde_json::from_slice(&body).expect("version JSON");
        assert_eq!(value["git_sha"], "0123456789abcdef");
        assert_eq!(value["oci_image_id"], "sha256:image");
        assert_eq!(value["kernel_wasm_blake3"], "kernel");
        assert_eq!(value["shell_wasm_blake3"], "shell");
    }

    #[test]
    fn render_empty_registry() {
        let state = test_state();
        let output = render_metrics(&state);
        assert!(output.contains("# TYPE ww_cell_fuel_remaining gauge"));
        assert!(output.contains("# TYPE ww_cell_fuel_consumed_total counter"));
        assert!(output.contains("# TYPE ww_rpc_calls_total counter"));
        assert!(output.contains("# TYPE ww_rpc_duration_seconds histogram"));
        assert!(output.contains("# TYPE ww_cache_hits_total counter"));
        assert!(output.contains("# TYPE ww_stream_bytes_pumped_total counter"));
        // No data lines when registries are empty.
        assert!(!output.contains("cell_id="));
        assert!(!output.contains("method="));
    }

    #[test]
    fn render_with_cells() {
        let state = test_state();
        {
            let mut map = state.fuel_registry.write().unwrap();
            map.insert(
                "kernel".into(),
                CellFuelSnapshot {
                    remaining: 500_000,
                    consumed_total: 1_200_000,
                },
            );
            map.insert(
                "worker-1".into(),
                CellFuelSnapshot {
                    remaining: 0,
                    consumed_total: 5_000_000,
                },
            );
        }
        let output = render_metrics(&state);
        assert!(output.contains("ww_cell_fuel_remaining{cell_id=\"kernel\"} 500000"));
        assert!(output.contains("ww_cell_fuel_consumed_total{cell_id=\"kernel\"} 1200000"));
        assert!(output.contains("ww_cell_fuel_remaining{cell_id=\"worker-1\"} 0"));
        assert!(output.contains("ww_cell_fuel_consumed_total{cell_id=\"worker-1\"} 5000000"));
    }

    #[test]
    fn render_rpc_histogram() {
        let state = test_state();
        {
            let mut rpc = state.rpc_metrics.write().unwrap();
            rpc.observe("host.id", 0.005);
            rpc.observe("host.id", 0.050);
        }
        let output = render_metrics(&state);
        // 0.005s falls in le=0.005 bucket (<=)
        assert!(
            output.contains("ww_rpc_duration_seconds_bucket{method=\"host.id\",le=\"0.005\"} 1")
        );
        // 0.050s falls in le=0.05 bucket
        assert!(output.contains("ww_rpc_duration_seconds_bucket{method=\"host.id\",le=\"0.05\"} 2"));
        // +Inf always has all observations
        assert!(output.contains("ww_rpc_duration_seconds_bucket{method=\"host.id\",le=\"+Inf\"} 2"));
        assert!(output.contains("ww_rpc_duration_seconds_count{method=\"host.id\"} 2"));
        assert!(output.contains("ww_rpc_calls_total{method=\"host.id\"} 2"));
    }

    #[test]
    fn render_rpc_histogram_empty() {
        let state = test_state();
        let output = render_metrics(&state);
        assert!(output.contains("# HELP ww_rpc_duration_seconds"));
        assert!(output.contains("# TYPE ww_rpc_duration_seconds histogram"));
        // No bucket lines when no observations
        assert!(!output.contains("ww_rpc_duration_seconds_bucket"));
    }

    #[test]
    fn render_cache_metrics() {
        let state = test_state();
        {
            let mut cache = state.cache_metrics.write().unwrap();
            cache.hits = 42;
            cache.misses = 7;
            cache.evictions = 3;
            cache.weight_bytes = 1_048_576;
            cache.entries = 100;
        }
        let output = render_metrics(&state);
        assert!(output.contains("ww_cache_hits_total 42"));
        assert!(output.contains("ww_cache_misses_total 7"));
        assert!(output.contains("ww_cache_evictions_total 3"));
        assert!(output.contains("ww_cache_weight_bytes 1048576"));
        assert!(output.contains("ww_cache_entries 100"));
    }

    #[test]
    fn renders_wasmtime_cache_metrics() {
        let mut output = String::new();
        render_wasmtime_cache_metrics(
            &mut output,
            &WasmtimeCacheSnapshot {
                state: WasmtimeCacheState::Enabled,
                hits: 42,
                stores: 7,
                component_compilations: 49,
            },
        );
        assert!(output.contains("ww_wasmtime_cache_state{state=\"enabled\"} 1"));
        assert!(output.contains("ww_wasmtime_cache_state{state=\"fallback\"} 0"));
        assert!(output.contains("ww_wasmtime_cache_hits_total 42"));
        assert!(output.contains("ww_wasmtime_cache_stores_total 7"));
        assert!(output.contains("ww_wasmtime_component_compilations_total 49"));
        assert!(output.contains("stores; this is not a lookup-miss count"));
    }

    #[test]
    fn render_stream_metrics() {
        let state = test_state();
        {
            let mut stream = state.stream_metrics.write().unwrap();
            stream.bytes_pumped_total = 1_000_000;
            stream.pump_ops_total = 500;
        }
        let output = render_metrics(&state);
        assert!(output.contains("ww_stream_bytes_pumped_total 1000000"));
        assert!(output.contains("ww_stream_pump_ops_total 500"));
    }

    #[test]
    fn host_id_returns_peer_id() {
        let state = test_state();
        assert_eq!(state.peer_id, "12D3KooWTestPeerId");
    }

    #[tokio::test]
    async fn host_addrs_returns_listen_addresses() {
        let state = test_state();
        let addr: libp2p::Multiaddr = "/ip4/127.0.0.1/tcp/2025".parse().unwrap();
        state.network_state.add_listen_addr(addr.to_vec()).await;

        let snapshot = state.network_state.snapshot().await;
        let addrs: Vec<String> = snapshot
            .listen_addrs
            .iter()
            .filter_map(|bytes| libp2p::Multiaddr::try_from(bytes.clone()).ok())
            .map(|a| a.to_string())
            .collect();
        let body = addrs.join("\n");
        assert!(body.contains("/ip4/127.0.0.1/tcp/2025"));
    }

    #[tokio::test]
    async fn host_addrs_empty_when_no_listeners() {
        let state = test_state();
        let snapshot = state.network_state.snapshot().await;
        assert!(snapshot.listen_addrs.is_empty());
    }

    #[tokio::test]
    async fn host_nat_returns_status_and_recent_probe_events() {
        let state = test_state();
        state
            .network_state
            .set_nat_status(rpc::NatReachability::Public)
            .await;
        state
            .network_state
            .record_nat_probe_event(rpc::NatProbeEvent {
                tested_addr: "/ip4/127.0.0.1/tcp/2025".to_string(),
                server_peer_id: "12D3KooWTest".to_string(),
                success: true,
                timestamp_unix_ms: 42,
            })
            .await;

        let response = host_nat_handler(State(state)).await.into_response();
        assert_eq!(response.status(), axum::http::StatusCode::OK);
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("read response body");
        let value: serde_json::Value = serde_json::from_slice(&body).expect("parse JSON");
        assert_eq!(value["nat_status"], "Public");
        assert_eq!(
            value["recent_probes"].as_array().map(|a| a.len()),
            Some(1),
            "expected exactly one recent probe event"
        );
    }
}
