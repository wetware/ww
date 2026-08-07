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
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, RwLock};

use axum::{extract::State, http::StatusCode, response::IntoResponse, routing::get, Router};
use tokio::sync::watch;

use crate::cell::engine::{WasmtimeCacheMetrics, WasmtimeCacheSnapshot, WasmtimeCacheState};

/// Immutable build and artifact identity exposed by `GET /version`.
#[derive(Clone, Debug, serde::Serialize)]
pub struct VersionInfo {
    pub git_sha: String,
    pub oci_image_id: Option<String>,
    pub kernel_identity: crate::kernel::KernelIdentityState,
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

    #[cfg(test)]
    pub(crate) fn is_degraded(&self) -> bool {
        self.snapshot().degraded
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
    route_registry: Option<rpc::dispatch::RouteRegistry>,
    epoch_rx: watch::Receiver<authority::Epoch>,
    activated_seq: Arc<AtomicU64>,
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
    let mut status = state.runtime_status.snapshot();
    if status.ready {
        if let Some(registry) = &state.route_registry {
            let live_routes = rpc::dispatch::live_route_count(registry);
            apply_route_readiness(
                &mut status,
                live_routes,
                &state.epoch_rx,
                &state.activated_seq,
            );
        }
    }
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

fn apply_route_readiness(
    status: &mut RuntimeStatusSnapshot,
    live_routes: Result<usize, &'static str>,
    epoch_rx: &watch::Receiver<authority::Epoch>,
    activated_seq: &AtomicU64,
) {
    match live_routes {
        Ok(0) => {
            status.ready = false;
            status.phase = "waiting-for-http-route".to_string();
        }
        Ok(_) => {
            // Hold the authoritative watch read lock through the Acquire load.
            // The epoch publisher cannot install a replacement between these
            // two observations, so equality is a fail-closed snapshot.
            let current = epoch_rx.borrow();
            if activated_seq.load(Ordering::Acquire) != current.seq {
                status.ready = false;
                status.phase = "replacing-generation".to_string();
            }
        }
        Err(reason) => {
            status.ready = false;
            status.phase = "route-status-unavailable".to_string();
            status.degraded = true;
            if !status.degraded_reasons.iter().any(|entry| entry == reason) {
                status.degraded_reasons.push(reason.to_string());
            }
        }
    }
}

/// `GET /version` — returns source, image, and embedded artifact provenance.
async fn version_handler(State(state): State<AdminState>) -> impl IntoResponse {
    let runtime = state.runtime_status.snapshot();
    let cache = state.wasmtime_cache_metrics.snapshot();
    let kernel_identity = state.version_info.kernel_identity.get();
    let kernel_source = kernel_identity
        .map(|identity| identity.source.clone())
        .unwrap_or_else(|| state.version_info.kernel_identity.pending_source());
    let payload = serde_json::json!({
        "git_sha": state.version_info.git_sha,
        "oci_image_id": state.version_info.oci_image_id,
        "kernel_cid": kernel_identity.map(|identity| identity.cid.as_str()),
        "kernel_source": kernel_source,
        "kernel_source_cid": kernel_identity.and_then(|identity| identity.source_cid.as_deref()),
        "kernel_wasm_blake3": kernel_identity.map(|identity| identity.wasm_blake3.as_str()),
        "kernel_size": kernel_identity.map(|identity| identity.size),
        "kernel_abi": crate::kernel::KERNEL_ABI_VERSION,
        "kernel_abi_fingerprint": crate::kernel::KERNEL_ABI_FINGERPRINT,
        "shell_wasm_blake3": state.version_info.shell_wasm_blake3,
        "degraded": runtime.degraded || cache.state == WasmtimeCacheState::Fallback,
        "degraded_reasons": runtime.degraded_reasons,
        "wasmtime_cache_state": cache.state.as_str(),
        "wasmtime_cache_hits_total": cache.hits,
        "wasmtime_cache_stores_total": cache.stores,
        "wasmtime_component_compilations_total": cache.component_compilations,
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
    /// When WAGI serving is enabled, readiness is derived from this same live
    /// registration map rather than a separately maintained route count.
    pub route_registry: Option<rpc::dispatch::RouteRegistry>,
    /// Authoritative epoch receiver shared with every pid0 `EpochGuard`.
    pub epoch_rx: watch::Receiver<authority::Epoch>,
    /// Last pid0 generation committed after successful initialization.
    pub activated_seq: Arc<AtomicU64>,
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
                route_registry: self.route_registry,
                epoch_rx: self.epoch_rx,
                activated_seq: self.activated_seq,
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
    use authority::{system_capnp, Epoch, EpochGuard, Provenance};
    use capnp::capability::Promise;

    fn test_state() -> AdminState {
        let (_epoch_tx, epoch_rx) = tokio::sync::watch::channel(Epoch {
            seq: 1,
            head: Vec::new(),
            provenance: Provenance::Block(0),
        });
        let source = crate::kernel::KernelSource::Embedded("main");
        let kernel_identity = crate::kernel::KernelIdentityState::pending(&source);
        kernel_identity
            .publish(crate::kernel::KernelIdentity {
                cid: "kernel-cid".to_string(),
                source: "embedded:main".to_string(),
                wasm_blake3: "kernel".to_string(),
                source_cid: None,
                size: 42,
                abi: crate::kernel::KERNEL_ABI_VERSION.to_string(),
                abi_fingerprint: crate::kernel::KERNEL_ABI_FINGERPRINT.to_string(),
            })
            .unwrap();
        AdminState {
            peer_id: "12D3KooWTestPeerId".to_string(),
            network_state: rpc::NetworkState::new(),
            version_info: VersionInfo {
                git_sha: "0123456789abcdef".to_string(),
                oci_image_id: Some("sha256:image".to_string()),
                kernel_identity,
                shell_wasm_blake3: Some("shell".to_string()),
            },
            runtime_status: RuntimeStatus::starting(),
            route_registry: None,
            epoch_rx,
            activated_seq: Arc::new(AtomicU64::new(1)),
            fuel_registry: new_fuel_registry(),
            rpc_metrics: new_rpc_metrics(),
            cache_metrics: new_cache_metrics(),
            stream_metrics: new_stream_metrics(),
            wasmtime_cache_metrics: crate::cell::engine::wasmtime_cache_metrics(),
        }
    }

    struct ReadinessExecutor;

    #[allow(refining_impl_trait)]
    impl system_capnp::executor::Server for ReadinessExecutor {
        fn spawn(
            self: capnp::capability::Rc<Self>,
            _params: system_capnp::executor::SpawnParams,
            _results: system_capnp::executor::SpawnResults,
        ) -> Promise<(), capnp::Error> {
            Promise::err(capnp::Error::failed(
                "readiness executor does not spawn".into(),
            ))
        }

        fn cid(
            self: capnp::capability::Rc<Self>,
            _params: system_capnp::executor::CidParams,
            mut results: system_capnp::executor::CidResults,
        ) -> Promise<(), capnp::Error> {
            results
                .get()
                .set_cid("bafkr4if3s6yv23hd3hgfvftj2g2uwdrqazv53p36p5lqyy7n77d5t5p54a");
            Promise::ok(())
        }
    }

    fn readiness_epoch() -> (tokio::sync::watch::Sender<Epoch>, EpochGuard) {
        let (tx, receiver) = tokio::sync::watch::channel(Epoch {
            seq: 1,
            head: Vec::new(),
            provenance: Provenance::Block(0),
        });
        (
            tx,
            EpochGuard {
                issued_seq: 1,
                receiver,
            },
        )
    }

    fn readiness_guard(tx: &tokio::sync::watch::Sender<Epoch>, issued_seq: u64) -> EpochGuard {
        EpochGuard {
            issued_seq,
            receiver: tx.subscribe(),
        }
    }

    fn advance_readiness_epoch(tx: &tokio::sync::watch::Sender<Epoch>, seq: u64) {
        tx.send_replace(Epoch {
            seq,
            head: Vec::new(),
            provenance: Provenance::Block(0),
        });
    }

    async fn install_readiness_route(registry: &rpc::dispatch::RouteRegistry, guard: EpochGuard) {
        let listener: system_capnp::http_listener::Client = capnp_rpc::new_client(
            rpc::http_listener::HttpListenerImpl::new(guard, registry.clone()),
        );
        let mut request = listener.listen_request();
        request
            .get()
            .set_executor(capnp_rpc::new_client(ReadinessExecutor));
        request.get().set_prefix("/status");
        request
            .send()
            .promise
            .await
            .expect("install readiness route");
        tokio::time::timeout(std::time::Duration::from_secs(1), async {
            while rpc::dispatch::live_route_count(registry) != Ok(1) {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("route target readiness preflight");
    }

    async fn assert_readyz(state: &AdminState, expected: StatusCode) {
        let response = readyz_handler(State(state.clone())).await.into_response();
        assert_eq!(response.status(), expected);
    }

    async fn readyz_json(state: &AdminState) -> (StatusCode, serde_json::Value) {
        let response = readyz_handler(State(state.clone())).await.into_response();
        let code = response.status();
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("readiness response body");
        let json = serde_json::from_slice(&body).expect("readiness JSON");
        (code, json)
    }

    async fn run_readiness_local(future: impl std::future::Future<Output = ()>) {
        tokio::task::LocalSet::new().run_until(future).await;
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
    async fn live_replacement_route_waits_for_generation_activation() {
        run_readiness_local(async {
            let mut state = test_state();
            let registry = rpc::dispatch::new_registry();
            state.route_registry = Some(registry.clone());
            state.runtime_status.set_ready();
            let (epoch_tx, _old_guard) = readiness_epoch();
            advance_readiness_epoch(&epoch_tx, 2);
            state.epoch_rx = epoch_tx.subscribe();
            state.activated_seq.store(1, Ordering::Release);

            install_readiness_route(&registry, readiness_guard(&epoch_tx, 2)).await;
            let (code, body) = readyz_json(&state).await;
            assert_eq!(code, StatusCode::SERVICE_UNAVAILABLE);
            assert_eq!(body["phase"], "replacing-generation");

            state.activated_seq.store(2, Ordering::Release);
            assert_readyz(&state, StatusCode::OK).await;
        })
        .await;
    }

    #[tokio::test]
    async fn epoch_publish_closes_readiness_immediately_without_observer_lag() {
        run_readiness_local(async {
            let mut state = test_state();
            let registry = rpc::dispatch::new_registry();
            state.route_registry = Some(registry.clone());
            state.runtime_status.set_ready();
            let (epoch_tx, old_guard) = readiness_epoch();
            state.epoch_rx = epoch_tx.subscribe();
            state.activated_seq.store(1, Ordering::Release);
            install_readiness_route(&registry, old_guard).await;
            assert_readyz(&state, StatusCode::OK).await;

            advance_readiness_epoch(&epoch_tx, 2);
            assert_readyz(&state, StatusCode::SERVICE_UNAVAILABLE).await;
        })
        .await;
    }

    #[test]
    fn authoritative_epoch_borrow_blocks_publish_through_activation_compare() {
        let (epoch_tx, epoch_rx) = tokio::sync::watch::channel(Epoch {
            seq: 1,
            head: Vec::new(),
            provenance: Provenance::Block(0),
        });
        let activated_seq = AtomicU64::new(1);
        let current = epoch_rx.borrow();
        let (started_tx, started_rx) = std::sync::mpsc::channel();
        let (published_tx, published_rx) = std::sync::mpsc::channel();
        let publisher = std::thread::spawn(move || {
            started_tx.send(()).expect("publisher start signal");
            epoch_tx.send_replace(Epoch {
                seq: 2,
                head: Vec::new(),
                provenance: Provenance::Block(0),
            });
            published_tx.send(()).expect("publish completion signal");
        });

        started_rx
            .recv_timeout(std::time::Duration::from_secs(1))
            .expect("publisher thread started");
        assert!(published_rx
            .recv_timeout(std::time::Duration::from_millis(25))
            .is_err());
        assert_eq!(activated_seq.load(Ordering::Acquire), current.seq);
        drop(current);
        published_rx
            .recv_timeout(std::time::Duration::from_secs(1))
            .expect("epoch publisher resumes after comparison borrow drops");
        publisher.join().expect("epoch publisher thread");
    }

    #[tokio::test]
    async fn generation_zero_activation_baseline_remains_ready() {
        run_readiness_local(async {
            let mut state = test_state();
            let registry = rpc::dispatch::new_registry();
            let (epoch_tx, epoch_rx) = tokio::sync::watch::channel(Epoch {
                seq: 0,
                head: Vec::new(),
                provenance: Provenance::Block(0),
            });
            state.route_registry = Some(registry.clone());
            state.runtime_status.set_ready();
            state.epoch_rx = epoch_rx;
            state.activated_seq.store(0, Ordering::Release);
            install_readiness_route(&registry, readiness_guard(&epoch_tx, 0)).await;
            assert_readyz(&state, StatusCode::OK).await;
        })
        .await;
    }

    #[tokio::test]
    async fn readyz_tracks_epoch_scoped_http_registration_lifecycle() {
        let local = tokio::task::LocalSet::new();
        local
            .run_until(async {
                let mut state = test_state();
                let registry = rpc::dispatch::new_registry();
                state.route_registry = Some(registry.clone());
                state.runtime_status.set_ready();
                let (epoch_tx, old_guard) = readiness_epoch();
                state.epoch_rx = epoch_tx.subscribe();
                state.activated_seq.store(1, Ordering::Release);

                // Replacement init has not installed anything yet.
                assert_readyz(&state, StatusCode::SERVICE_UNAVAILABLE).await;

                install_readiness_route(&registry, old_guard).await;
                assert_eq!(rpc::dispatch::live_route_count(&registry), Ok(1));
                assert_readyz(&state, StatusCode::OK).await;

                // Epoch liveness is checked from the entry itself, so the old
                // route stops counting even before its cleanup task is polled.
                advance_readiness_epoch(&epoch_tx, 2);
                assert_eq!(rpc::dispatch::live_route_count(&registry), Ok(0));
                assert_readyz(&state, StatusCode::SERVICE_UNAVAILABLE).await;

                // Incomplete replacement init leaves readiness false.
                tokio::task::yield_now().await;
                assert_readyz(&state, StatusCode::SERVICE_UNAVAILABLE).await;

                install_readiness_route(&registry, readiness_guard(&epoch_tx, 2)).await;
                assert_eq!(rpc::dispatch::live_route_count(&registry), Ok(1));
                assert_readyz(&state, StatusCode::SERVICE_UNAVAILABLE).await;
                state.activated_seq.store(2, Ordering::Release);
                assert_readyz(&state, StatusCode::OK).await;

                // A late cleanup from epoch 1 must not disturb the epoch-2
                // route or its derived readiness.
                tokio::task::yield_now().await;
                tokio::task::yield_now().await;
                assert_eq!(registry.read().expect("registry lock").len(), 1);
                assert_eq!(rpc::dispatch::live_route_count(&registry), Ok(1));
                assert_readyz(&state, StatusCode::OK).await;

                // A failed epoch-3 replacement performs no registration.
                advance_readiness_epoch(&epoch_tx, 3);
                tokio::task::yield_now().await;
                assert_eq!(rpc::dispatch::live_route_count(&registry), Ok(0));
                assert_readyz(&state, StatusCode::SERVICE_UNAVAILABLE).await;

                // Repeated replacements overwrite the one path instead of
                // accumulating route or readiness counts.
                for seq in 3..=6 {
                    install_readiness_route(&registry, readiness_guard(&epoch_tx, seq)).await;
                    assert_eq!(registry.read().expect("registry lock").len(), 1);
                    assert_eq!(rpc::dispatch::live_route_count(&registry), Ok(1));
                    state.activated_seq.store(seq, Ordering::Release);
                    assert_readyz(&state, StatusCode::OK).await;

                    advance_readiness_epoch(&epoch_tx, seq + 1);
                    assert_eq!(rpc::dispatch::live_route_count(&registry), Ok(0));
                    assert_readyz(&state, StatusCode::SERVICE_UNAVAILABLE).await;
                }

                tokio::task::yield_now().await;
                assert!(registry.read().expect("registry lock").is_empty());
            })
            .await;
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
        assert_eq!(value["kernel_cid"], "kernel-cid");
        assert_eq!(value["kernel_source"], "embedded:main");
        assert_eq!(value["kernel_wasm_blake3"], "kernel");
        assert_eq!(value["kernel_size"], 42);
        assert_eq!(value["kernel_abi"], crate::kernel::KERNEL_ABI_VERSION);
        assert_eq!(
            value["kernel_abi_fingerprint"],
            crate::kernel::KERNEL_ABI_FINGERPRINT
        );
        assert_eq!(value["shell_wasm_blake3"], "shell");
        assert!(value["wasmtime_cache_hits_total"].is_u64());
        assert!(value["wasmtime_cache_stores_total"].is_u64());
        assert!(value["wasmtime_component_compilations_total"].is_u64());
    }

    #[tokio::test]
    async fn version_remains_available_while_kernel_identity_is_pending() {
        let mut state = test_state();
        state.version_info.kernel_identity = crate::kernel::KernelIdentityState::pending(
            &crate::kernel::KernelSource::Path("/tmp/pid0.wasm".into()),
        );
        let response = version_handler(State(state)).await.into_response();
        assert_eq!(response.status(), StatusCode::OK);
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("version response body");
        let value: serde_json::Value = serde_json::from_slice(&body).expect("version JSON");
        assert_eq!(value["kernel_cid"], serde_json::Value::Null);
        assert_eq!(value["kernel_wasm_blake3"], serde_json::Value::Null);
        assert_eq!(value["kernel_source"], "<pending: file:/tmp/pid0.wasm>");
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
