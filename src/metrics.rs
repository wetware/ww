//! Admin HTTP server for node introspection.
//!
//! Serves liveness/readiness checks at `GET /healthz` and `GET /readyz`,
//! build provenance at `GET /version`, Prometheus metrics at `GET /metrics`,
//! and host identity/address/NAT information at `GET /host/id`,
//! `GET /host/addrs`, and `GET /host/nat`.
//!
//! Wasmtime cache metrics read the live process-wide engine factory. Only
//! families with production collectors are declared and exposed.

use std::fmt::Write as _;
use std::sync::{Arc, RwLock};

use axum::{extract::State, http::StatusCode, response::IntoResponse, routing::get, Router};
use tokio::sync::watch;

use crate::cell::engine::{WasmtimeCacheMetrics, WasmtimeCacheSnapshot, WasmtimeCacheState};
use crate::cell::lifecycle::{WasmtimeLifecycleSnapshot, DURATION_BUCKET_SECONDS};
use crate::cell::metrics::FuelMetrics;
use crate::services::{
    ExecutorMetrics, ExecutorSnapshot, WorkerState, WORKER_HEARTBEAT_INTERVAL,
    WORKER_STALL_THRESHOLD,
};

/// Immutable build and artifact identity exposed by `GET /version`.
#[derive(Clone, Debug, serde::Serialize)]
pub struct VersionInfo {
    pub git_sha: String,
    pub oci_image_id: Option<String>,
    pub kernel_identity: crate::kernel::IdentityState,
}

/// Mutable boot diagnostics shared between the boot path and admin server.
///
/// Kernel readiness itself lives only in [`authority::KernelReadyGate`].
#[derive(Clone, Debug)]
pub struct RuntimeStatus {
    inner: Arc<RwLock<RuntimeStatusSnapshot>>,
}

#[derive(Clone, Debug, serde::Serialize)]
struct RuntimeStatusSnapshot {
    phase: String,
    degraded: bool,
    degraded_reasons: Vec<String>,
}

impl RuntimeStatus {
    pub fn starting() -> Self {
        Self {
            inner: Arc::new(RwLock::new(RuntimeStatusSnapshot {
                phase: "starting".to_string(),
                degraded: false,
                degraded_reasons: Vec::new(),
            })),
        }
    }

    pub fn set_phase(&self, phase: impl Into<String>) {
        if let Ok(mut status) = self.inner.write() {
            status.phase = phase.into();
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
                phase: "status-unavailable".to_string(),
                degraded: true,
                degraded_reasons: vec!["runtime status lock poisoned".to_string()],
            })
    }
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
    kernel_ready_gate: Arc<authority::KernelReadyGate>,
    route_registry: Option<rpc::dispatch::RouteRegistry>,
    wasmtime_cache_metrics: WasmtimeCacheMetrics,
    executor_metrics: ExecutorMetrics,
    fuel_metrics: FuelMetrics,
}

/// Render all metrics in Prometheus text exposition format.
fn render_metrics(state: &AdminState) -> String {
    let mut out = String::with_capacity(2048);
    render_wasmtime_cache_metrics(&mut out, &state.wasmtime_cache_metrics.snapshot());
    render_wasmtime_lifecycle_metrics(&mut out, state.wasmtime_cache_metrics.lifecycle_snapshot());
    render_executor_metrics(&mut out, state.executor_metrics.snapshot());
    render_worker_liveness(&mut out, &state.executor_metrics);
    render_fuel_metrics(&mut out, &state.fuel_metrics);
    crate::telemetry::render_metrics(&mut out);
    out
}

fn render_fuel_metrics(out: &mut String, metrics: &FuelMetrics) {
    let snapshot = metrics.snapshot();
    out.push_str(
        "# HELP ww_fuel_decisions_total Applied fuel decisions over this Host lifetime.\n",
    );
    out.push_str("# TYPE ww_fuel_decisions_total counter\n");
    for (policy, counts) in [
        ("scheduled", snapshot.scheduled),
        ("oneshot", snapshot.oneshot),
    ] {
        for (decision, value) in [
            ("grant", counts.grant),
            ("suspend", counts.suspend),
            ("exhausted", counts.exhausted),
        ] {
            writeln!(
                out,
                "ww_fuel_decisions_total{{policy=\"{policy}\",decision=\"{decision}\"}} {value}"
            )
            .unwrap();
        }
    }
    out.push_str("# HELP ww_fuel_settled_units_total Published physical fuel units accounted for at settlement boundaries over this Host lifetime; live Stores can have unpublished values; excludes unsettled termination tails.\n");
    out.push_str("# TYPE ww_fuel_settled_units_total counter\n");
    for (policy, value) in [
        ("scheduled", snapshot.scheduled_settled_units),
        ("oneshot", snapshot.oneshot_settled_units),
    ] {
        writeln!(
            out,
            "ww_fuel_settled_units_total{{policy=\"{policy}\"}} {value}"
        )
        .unwrap();
    }
}

fn render_wasmtime_lifecycle_metrics(out: &mut String, snapshot: WasmtimeLifecycleSnapshot) {
    for (counter, histogram, help, counts, include_timeout) in [
        (
            "ww_wasmtime_component_compile_total",
            "ww_wasmtime_component_compile_duration_seconds",
            "Returned canonical Component::from_binary operations, including persistent-cache loads",
            snapshot.compilation,
            false,
        ),
        (
            "ww_wasmtime_instantiations_total",
            "ww_wasmtime_instantiation_duration_seconds",
            "Executable CommandPre instantiations, including start functions; excludes command execution and dropped pending futures",
            snapshot.instantiation,
            true,
        ),
    ] {
        writeln!(out, "# HELP {counter} {help} by outcome over this process lifetime.").unwrap();
        writeln!(out, "# TYPE {counter} counter").unwrap();
        for (outcome, value) in [("success", counts.success), ("error", counts.error)] {
            writeln!(out, "{counter}{{outcome=\"{outcome}\"}} {value}").unwrap();
        }
        if include_timeout {
            writeln!(out, "{counter}{{outcome=\"timeout\"}} {}", counts.timeout).unwrap();
        }
        writeln!(out, "# HELP {histogram} {help}: elapsed seconds for observed outcomes, including failures.").unwrap();
        writeln!(out, "# TYPE {histogram} histogram").unwrap();
        for (bound, count) in DURATION_BUCKET_SECONDS.iter().zip(counts.duration.buckets) {
            writeln!(out, "{histogram}_bucket{{le=\"{bound}\"}} {count}").unwrap();
        }
        writeln!(out, "{histogram}_bucket{{le=\"+Inf\"}} {}", counts.duration.count).unwrap();
        writeln!(out, "{histogram}_sum {}", counts.duration.sum_nanos as f64 / 1e9).unwrap();
        writeln!(out, "{histogram}_count {}", counts.duration.count).unwrap();
    }
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
    out.push_str(&format!("ww_wasmtime_cache_hits_total {}\n", snapshot.hits));
    out.push_str("# HELP ww_wasmtime_cache_stores_total Successful Wasmtime compilation cache stores; this is not a lookup-miss count.\n");
    out.push_str("# TYPE ww_wasmtime_cache_stores_total counter\n");
    out.push_str(&format!(
        "ww_wasmtime_cache_stores_total {}\n",
        snapshot.stores
    ));
    out.push_str("# HELP ww_wasmtime_component_compilations_total Calls to ww's canonical Component::from_binary path; persistent-cache hits and failed attempts are included.\n");
    out.push_str("# TYPE ww_wasmtime_component_compilations_total counter\n");
    out.push_str(&format!(
        "ww_wasmtime_component_compilations_total {}\n",
        snapshot.component_compilations
    ));
}

/// The label sets are fixed. Counters span the Host's collector lifetime and
/// remain available after executor teardown; occupancy returns to zero.
fn render_executor_metrics(out: &mut String, snapshot: ExecutorSnapshot) {
    out.push_str("# HELP ww_executor_cell_slots Executor-admitted Cell task slots, queued or running, across all workers.\n");
    out.push_str("# TYPE ww_executor_cell_slots gauge\n");
    writeln!(out, "ww_executor_cell_slots {}", snapshot.occupied).unwrap();
    out.push_str("# HELP ww_executor_spawn_admissions_total Spawn attempts by admission outcome over this Host lifetime.\n");
    out.push_str("# TYPE ww_executor_spawn_admissions_total counter\n");
    for (outcome, value) in [
        ("accepted", snapshot.accepted),
        ("full", snapshot.full),
        ("closed", snapshot.closed),
    ] {
        writeln!(
            out,
            "ww_executor_spawn_admissions_total{{outcome=\"{outcome}\"}} {value}"
        )
        .unwrap();
    }
    out.push_str("# HELP ww_executor_cell_terminations_total Admitted task lifetimes ended by executor outcome over this Host lifetime; completed means the factory future returned, not guest success.\n");
    out.push_str("# TYPE ww_executor_cell_terminations_total counter\n");
    for (outcome, value) in [
        ("completed", snapshot.completed),
        ("cancelled", snapshot.cancelled),
        ("panicked", snapshot.panicked),
    ] {
        writeln!(
            out,
            "ww_executor_cell_terminations_total{{outcome=\"{outcome}\"}} {value}"
        )
        .unwrap();
    }
}

/// Worker indices are bounded by the configured pool size. Collection runs on
/// the admin thread and never asks an executor worker to answer a scrape.
fn render_worker_liveness(out: &mut String, metrics: &ExecutorMetrics) {
    let workers = metrics.worker_liveness();
    out.push_str("# HELP ww_executor_worker_heartbeat_interval_seconds Target interval between LocalSet progress publications; missed ticks are skipped.\n");
    out.push_str("# TYPE ww_executor_worker_heartbeat_interval_seconds gauge\n");
    writeln!(
        out,
        "ww_executor_worker_heartbeat_interval_seconds {}",
        WORKER_HEARTBEAT_INTERVAL.as_secs_f64()
    )
    .unwrap();
    out.push_str("# HELP ww_executor_worker_stall_threshold_seconds Progress age at which a worker with confirmed progress becomes stalled; stopped workers are excluded.\n");
    out.push_str("# TYPE ww_executor_worker_stall_threshold_seconds gauge\n");
    writeln!(
        out,
        "ww_executor_worker_stall_threshold_seconds {}",
        WORKER_STALL_THRESHOLD.as_secs_f64()
    )
    .unwrap();
    out.push_str("# HELP ww_executor_worker_state Worker lifecycle state; exactly one state is 1. Shutdown requested does not imply progress.\n");
    out.push_str("# TYPE ww_executor_worker_state gauge\n");
    for worker in &workers {
        for (state, name) in [
            (WorkerState::Starting, "starting"),
            (WorkerState::Running, "running"),
            (WorkerState::ShuttingDown, "shutting_down"),
            (WorkerState::Stopped, "stopped"),
        ] {
            writeln!(
                out,
                "ww_executor_worker_state{{worker=\"{}\",state=\"{name}\"}} {}",
                worker.worker_id,
                u8::from(worker.state == state)
            )
            .unwrap();
        }
    }
    out.push_str("# HELP ww_executor_worker_stalled Worker has no confirmed LocalSet progress for the stall threshold; clears on progress or stopped state, not shutdown request. Does not prove permanent deadlock.\n");
    out.push_str("# TYPE ww_executor_worker_stalled gauge\n");
    for worker in &workers {
        writeln!(
            out,
            "ww_executor_worker_stalled{{worker=\"{}\"}} {}",
            worker.worker_id,
            u8::from(worker.stalled)
        )
        .unwrap();
    }
    out.push_str("# HELP ww_executor_worker_progress_age_seconds Monotonic elapsed time since confirmed LocalSet progress; absent before first publication, retained through shutdown.\n");
    out.push_str("# TYPE ww_executor_worker_progress_age_seconds gauge\n");
    for worker in &workers {
        if let Some(age) = worker.progress_age {
            writeln!(
                out,
                "ww_executor_worker_progress_age_seconds{{worker=\"{}\"}} {}",
                worker.worker_id,
                age.as_secs_f64()
            )
            .unwrap();
        }
    }
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

/// `GET /readyz` — reports the current PID0 commit and a live HTTP route.
async fn readyz_handler(State(state): State<AdminState>) -> impl IntoResponse {
    let status = state.runtime_status.snapshot();
    let kernel_ready = state.kernel_ready_gate.is_ready();
    let routes_ready = state
        .route_registry
        .as_ref()
        .map(|registry| matches!(rpc::dispatch::live_route_count(registry), Ok(count) if count > 0))
        .unwrap_or(true);
    let ready = kernel_ready && routes_ready;
    let phase = if ready {
        "ready".to_string()
    } else if kernel_ready {
        "waiting-for-http-route".to_string()
    } else if matches!(status.phase.as_str(), "starting-kernel" | "ready") {
        "kernel-not-ready".to_string()
    } else {
        status.phase.clone()
    };
    let code = if ready {
        StatusCode::OK
    } else {
        StatusCode::SERVICE_UNAVAILABLE
    };
    let payload = serde_json::json!({
        "ready": ready,
        "phase": phase,
        "degraded": status.degraded,
        "degraded_reasons": status.degraded_reasons,
    })
    .to_string();
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
    let running_identity = state.version_info.kernel_identity.get();
    let kernel_identity = running_identity.as_ref().map(|identity| &identity.content);
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
        "kernel_epoch_seq": running_identity.as_ref().map(|identity| identity.epoch_seq),
        "kernel_effective_root": running_identity.as_ref().map(|identity| identity.effective_root.to_string()),
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
    /// The sole host-observable PID0 readiness state.
    pub kernel_ready_gate: Arc<authority::KernelReadyGate>,
    /// When WAGI serving is enabled, readiness also requires a live route.
    pub route_registry: Option<rpc::dispatch::RouteRegistry>,
    pub wasmtime_cache_metrics: WasmtimeCacheMetrics,
    /// The same collector supplied to the Host executor pool.
    pub executor_metrics: ExecutorMetrics,
    /// Shared with the Host runtime engine, including locally spawned Cells.
    pub fuel_metrics: FuelMetrics,
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
                kernel_ready_gate: self.kernel_ready_gate,
                route_registry: self.route_registry,
                wasmtime_cache_metrics: self.wasmtime_cache_metrics,
                executor_metrics: self.executor_metrics,
                fuel_metrics: self.fuel_metrics,
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
    use authority::Epoch;

    fn test_state() -> AdminState {
        let (_epoch_tx, epoch_rx) = tokio::sync::watch::channel(Epoch {
            seq: 1,
            head: Vec::new(),
            root: None,
        });
        let kernel_ready_gate = Arc::new(authority::KernelReadyGate::new(epoch_rx.clone()));
        let kernel_identity = crate::kernel::IdentityState::pending(epoch_rx);
        AdminState {
            peer_id: "12D3KooWTestPeerId".to_string(),
            network_state: rpc::NetworkState::new(),
            version_info: VersionInfo {
                git_sha: "0123456789abcdef".to_string(),
                oci_image_id: Some("sha256:image".to_string()),
                kernel_identity,
            },
            runtime_status: RuntimeStatus::starting(),
            kernel_ready_gate,
            route_registry: None,
            wasmtime_cache_metrics: crate::cell::engine::wasmtime_cache_metrics(),
            executor_metrics: ExecutorMetrics::default(),
            fuel_metrics: FuelMetrics::default(),
        }
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

    fn epoch(seq: u64) -> Epoch {
        Epoch {
            seq,
            head: Vec::new(),
            root: None,
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
    async fn readyz_reads_only_the_authoritative_kernel_gate() {
        let state = test_state();
        state.runtime_status.set_phase("ready");
        let (code, body) = readyz_json(&state).await;
        assert_eq!(code, StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(body["ready"], false);
        assert_eq!(body["phase"], "kernel-not-ready");

        state.kernel_ready_gate.bind_generation(1);
        state.kernel_ready_gate.kernel_ready().unwrap();
        let (code, body) = readyz_json(&state).await;
        assert_eq!(code, StatusCode::OK);
        assert_eq!(body["ready"], true);
        assert_eq!(body["phase"], "ready");
    }

    #[tokio::test]
    async fn readyz_waits_for_a_live_http_route_after_kernel_commits() {
        let mut state = test_state();
        state.route_registry = Some(rpc::dispatch::new_registry());
        state.kernel_ready_gate.bind_generation(1);
        state.kernel_ready_gate.kernel_ready().unwrap();

        let (code, body) = readyz_json(&state).await;
        assert_eq!(code, StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(body["ready"], false);
        assert_eq!(body["phase"], "waiting-for-http-route");
    }

    #[tokio::test]
    async fn epoch_publish_closes_readyz_until_pid0_commits_the_replacement() {
        let mut state = test_state();
        let (epoch_tx, epoch_rx) = tokio::sync::watch::channel(epoch(1));
        state.kernel_ready_gate = Arc::new(authority::KernelReadyGate::new(epoch_rx));
        state.kernel_ready_gate.bind_generation(1);
        state.kernel_ready_gate.kernel_ready().unwrap();
        assert_eq!(readyz_json(&state).await.0, StatusCode::OK);

        epoch_tx.send_replace(epoch(2));
        let (code, body) = readyz_json(&state).await;
        assert_eq!(code, StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(body["ready"], false);

        state.kernel_ready_gate.bind_generation(2);
        state.kernel_ready_gate.kernel_ready().unwrap();
        assert_eq!(readyz_json(&state).await.0, StatusCode::OK);
    }

    #[tokio::test]
    async fn stale_kernel_commit_cannot_reopen_readyz() {
        let mut state = test_state();
        let (epoch_tx, epoch_rx) = tokio::sync::watch::channel(epoch(1));
        state.kernel_ready_gate = Arc::new(authority::KernelReadyGate::new(epoch_rx));
        state.kernel_ready_gate.bind_generation(1);
        epoch_tx.send_replace(epoch(2));

        assert_eq!(
            state.kernel_ready_gate.kernel_ready(),
            Err(authority::KernelReadyError::StaleGeneration)
        );
        assert_eq!(readyz_json(&state).await.0, StatusCode::SERVICE_UNAVAILABLE);
    }

    #[tokio::test]
    async fn version_reports_provenance_and_cache_degradation() {
        let mut state = test_state();
        let root = crate::kernel::runtime_cid(b"effective root");
        let (epoch_tx, epoch_rx) = tokio::sync::watch::channel(Epoch {
            seq: 2,
            head: Vec::new(),
            root: Some(root),
        });
        state.version_info.kernel_identity = crate::kernel::IdentityState::pending(epoch_rx);
        let _identity = state.version_info.kernel_identity.publish_for_test(
            crate::kernel::Identity {
                cid: "kernel-cid".to_string(),
                source: "/ipfs/root/boot/kernel.wasm".to_string(),
                wasm_blake3: "kernel".to_string(),
                source_cid: None,
                size: 42,
            },
            2,
            root,
        );
        let response = version_handler(State(state.clone())).await.into_response();
        assert_eq!(response.status(), StatusCode::OK);
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("version response body");
        let value: serde_json::Value = serde_json::from_slice(&body).expect("version JSON");
        assert_eq!(value["git_sha"], "0123456789abcdef");
        assert_eq!(value["oci_image_id"], "sha256:image");
        assert_eq!(value["kernel_cid"], "kernel-cid");
        assert_eq!(value["kernel_source"], "/ipfs/root/boot/kernel.wasm");
        assert_eq!(value["kernel_wasm_blake3"], "kernel");
        assert_eq!(value["kernel_size"], 42);
        assert_eq!(value["kernel_epoch_seq"], 2);
        assert_eq!(value["kernel_effective_root"], root.to_string());
        assert!(value["wasmtime_cache_hits_total"].is_u64());
        assert!(value["wasmtime_cache_stores_total"].is_u64());
        assert!(value["wasmtime_component_compilations_total"].is_u64());

        // Authority revokes identity immediately, before the old generation's
        // lifetime guard is dropped or its asynchronous cleanup completes.
        epoch_tx.send_replace(Epoch {
            seq: 3,
            head: Vec::new(),
            root: None,
        });
        let response = version_handler(State(state)).await.into_response();
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let value: serde_json::Value = serde_json::from_slice(&body).unwrap();
        for field in [
            "kernel_cid",
            "kernel_source_cid",
            "kernel_wasm_blake3",
            "kernel_size",
            "kernel_epoch_seq",
            "kernel_effective_root",
        ] {
            assert_eq!(value[field], serde_json::Value::Null, "{field}");
        }
    }

    #[tokio::test]
    async fn version_remains_available_while_kernel_identity_is_pending() {
        let mut state = test_state();
        state.version_info.kernel_identity =
            crate::kernel::IdentityState::pending(tokio::sync::watch::channel(Epoch::zero()).1);
        let response = version_handler(State(state)).await.into_response();
        assert_eq!(response.status(), StatusCode::OK);
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("version response body");
        let value: serde_json::Value = serde_json::from_slice(&body).expect("version JSON");
        assert_eq!(value["kernel_cid"], serde_json::Value::Null);
        assert_eq!(value["kernel_wasm_blake3"], serde_json::Value::Null);
        assert_eq!(value["kernel_epoch_seq"], serde_json::Value::Null);
        assert_eq!(value["kernel_effective_root"], serde_json::Value::Null);
        assert_eq!(value["kernel_source"], "<pending: /boot/kernel.wasm>");
    }

    #[test]
    fn renders_only_live_metric_families() {
        let output = render_metrics(&test_state());
        for prefix in ["ww_cell_fuel_", "ww_rpc_", "ww_cache_", "ww_stream_"] {
            assert!(
                !output.contains(prefix),
                "obsolete metric declaration: {prefix}"
            );
        }
        assert_eq!(
            output
                .lines()
                .filter(|line| line.starts_with("# HELP "))
                .count(),
            24
        );
        assert_eq!(
            output
                .lines()
                .filter(|line| line.starts_with("# TYPE "))
                .count(),
            24
        );
        assert_eq!(
            output.lines().filter(|line| !line.starts_with('#')).count(),
            62
        );
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
    fn live_wasmtime_families_are_contiguous() {
        let output = render_metrics(&test_state());
        let mut current_family = "";
        for line in output.lines() {
            if let Some(help) = line.strip_prefix("# HELP ") {
                current_family = help.split_whitespace().next().unwrap();
            } else if line.starts_with("ww_wasmtime_") {
                let name = line.split(['{', ' ']).next().unwrap();
                let family = name
                    .strip_suffix("_bucket")
                    .or_else(|| name.strip_suffix("_sum"))
                    .or_else(|| name.strip_suffix("_count"))
                    .unwrap_or(name);
                assert_eq!(family, current_family, "interleaved family: {line}");
            }
        }
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
