//! Thread-per-subsystem runtime inspired by Cloudflare Pingora.
//!
//! Long-running services such as the libp2p swarm and WASM executors run on
//! dedicated OS threads with their own tokio runtimes. The [`Host`] supervisor
//! owns those threads and coordinates shutdown. Deployment lifecycle runs on
//! the main host runtime rather than as a `Service` thread.
//!
//! Executor threads use `current_thread` + `LocalSet` because `wasmtime::Store`
//! is `!Send`.  M:N cell scheduling comes from the EWMA fuel estimator
//! (`crates/cell/src/proc.rs`), not tokio work stealing.
//!
//! `SwarmService` is the one exception: it uses a `multi_thread` runtime so
//! the per-connection upgrade tasks that libp2p-swarm spawns (each containing
//! a synchronous rustls TLS handshake with Ed25519 verification) distribute
//! across worker threads instead of serializing onto one. See `doc/runtimes.md`.

use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

use anyhow::{Context, Result};
use cell::sched::EPOCH_TICK_MS;
use tokio::sync::{mpsc, watch};
use tracing::Instrument;

#[cfg(test)]
pub(crate) fn test_filesystem_resources() -> Arc<cache::HostResources> {
    static RESOURCES: std::sync::OnceLock<Arc<cache::HostResources>> = std::sync::OnceLock::new();
    Arc::clone(RESOURCES.get_or_init(|| {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .expect("test Host runtime");
        let (owner, resources) = cache::HostFsOwner::start(
            runtime.handle().clone(),
            cache::HostLimits::new(4_096, 1_025, 1_024).unwrap(),
        )
        .expect("test Host filesystem owner");
        Box::leak(Box::new(runtime));
        Box::leak(Box::new(owner));
        resources
    }))
}

#[cfg(test)]
pub(crate) fn test_filesystem_scope() -> cache::FsScope {
    static SCOPE: std::sync::OnceLock<cache::FsScope> = std::sync::OnceLock::new();
    SCOPE
        .get_or_init(|| {
            let (scope, owner, finalizer, completion) = test_filesystem_resources()
                .try_filesystem_scope()
                .expect("test Host filesystem root");
            Box::leak(Box::new(owner));
            Box::leak(Box::new(finalizer));
            drop(completion);
            scope
        })
        .clone()
}

// ---------------------------------------------------------------------------
// Service trait
// ---------------------------------------------------------------------------

/// A subsystem that runs on its own OS thread.
///
/// Implementations should enter a tracing span with the service name
/// (e.g., `tracing::info_span!("swarm")`) for observability.
pub trait Service: Send + 'static {
    /// Run the service until shutdown is signaled.
    /// Returns `Err` for non-panic failures (e.g., swarm fails to bind).
    fn run(self, shutdown: watch::Receiver<()>) -> Result<()>;
}

// ---------------------------------------------------------------------------
// Host supervisor
// ---------------------------------------------------------------------------

/// The Host supervisor owns all subsystem threads and coordinates shutdown.
///
/// ```text
/// Host (Rust-side supervisor)
///  ├── Thread 1: SwarmService    — libp2p event loop
///  └── Thread 2..N: ExecutorPool — cells via fuel scheduling
/// ```
pub struct Host {
    threads: Vec<(String, JoinHandle<Result<()>>)>,
    shutdown_tx: watch::Sender<()>,
    shutdown_rx: watch::Receiver<()>,
    service_exit_tx: mpsc::UnboundedSender<ServiceExit>,
    service_exit_rx: mpsc::UnboundedReceiver<ServiceExit>,
    filesystem_resources: Arc<cache::HostResources>,
    filesystem_owner: Option<cache::HostFsOwner>,
}

/// An unexpected service-thread exit observed before host shutdown.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ServiceExit {
    pub name: String,
    pub error: Option<String>,
}

impl Host {
    /// Create a new Host supervisor.
    pub fn new(
        filesystem_resources: Arc<cache::HostResources>,
        filesystem_owner: cache::HostFsOwner,
    ) -> Self {
        let (shutdown_tx, shutdown_rx) = watch::channel(());
        let (service_exit_tx, service_exit_rx) = mpsc::unbounded_channel();
        Self {
            threads: Vec::new(),
            shutdown_tx,
            shutdown_rx,
            service_exit_tx,
            service_exit_rx,
            filesystem_resources,
            filesystem_owner: Some(filesystem_owner),
        }
    }

    pub fn filesystem_resources(&self) -> &Arc<cache::HostResources> {
        &self.filesystem_resources
    }

    /// Get a shutdown receiver for passing to services or other components.
    pub fn shutdown_rx(&self) -> watch::Receiver<()> {
        self.shutdown_rx.clone()
    }

    /// Spawn a service on its own OS thread.
    pub fn try_spawn<S: Service>(&mut self, name: &str, service: S) -> Result<()> {
        let shutdown = self.shutdown_rx.clone();
        let thread_name = name.to_string();
        let exit_name = thread_name.clone();
        let service_exit_tx = self.service_exit_tx.clone();
        let handle = std::thread::Builder::new()
            .name(thread_name.clone())
            .spawn(move || {
                let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    service.run(shutdown)
                }));
                match result {
                    Ok(result) => {
                        let error = result.as_ref().err().map(|error| format!("{error:#}"));
                        let _ = service_exit_tx.send(ServiceExit {
                            name: exit_name,
                            error,
                        });
                        result
                    }
                    Err(panic) => {
                        let message = panic
                            .downcast_ref::<&str>()
                            .copied()
                            .or_else(|| panic.downcast_ref::<String>().map(String::as_str))
                            .unwrap_or("<non-string panic>")
                            .to_string();
                        let _ = service_exit_tx.send(ServiceExit {
                            name: exit_name,
                            error: Some(format!("service panicked: {message}")),
                        });
                        Err(anyhow::anyhow!("service panicked: {message}"))
                    }
                }
            })
            .with_context(|| format!("failed to spawn service thread '{thread_name}'"))?;
        self.threads.push((name.to_string(), handle));
        Ok(())
    }

    /// Spawn a service on its own OS thread.
    ///
    /// Prefer `try_spawn` in startup paths that should return typed errors.
    pub fn spawn<S: Service>(&mut self, name: &str, service: S) {
        self.try_spawn(name, service)
            .unwrap_or_else(|e| panic!("{e:#}"));
    }

    /// Wait for any supervised service to exit. Before shutdown, even a
    /// successful return is unexpected because services are long-lived.
    pub async fn next_service_exit(&mut self) -> Option<ServiceExit> {
        self.service_exit_rx.recv().await
    }

    /// Signal all services to shut down and join all threads.
    ///
    /// Panicked or errored threads are logged but don't prevent other
    /// threads from shutting down.
    pub fn close_filesystem_admission(&self) {
        self.filesystem_owner
            .as_ref()
            .expect("filesystem owner already shut down")
            .close_admission();
    }

    pub async fn shutdown_filesystem(&mut self) -> Result<()> {
        let Some(owner) = self.filesystem_owner.take() else {
            return Ok(());
        };
        owner.shutdown().await.map_err(anyhow::Error::from)
    }

    pub async fn shutdown_services(mut self) {
        drop(self.shutdown_tx);
        let threads = std::mem::take(&mut self.threads);
        let _ = tokio::task::spawn_blocking(move || join_service_threads(threads)).await;
    }

    pub async fn shutdown(mut self) {
        self.close_filesystem_admission();
        if let Err(error) = self.shutdown_filesystem().await {
            tracing::error!(%error, "filesystem services failed to shut down");
        }
        self.shutdown_services().await;
    }
}

fn join_service_threads(threads: Vec<(String, JoinHandle<Result<()>>)>) {
    for (name, handle) in threads {
        match handle.join() {
            Ok(Ok(())) => tracing::info!(name, "service stopped"),
            Ok(Err(e)) => tracing::error!(name, error = %e, "service failed"),
            Err(panic) => {
                let msg = panic
                    .downcast_ref::<&str>()
                    .copied()
                    .or_else(|| panic.downcast_ref::<String>().map(String::as_str))
                    .unwrap_or("<non-string panic>");
                tracing::error!(name, panic = msg, "service panicked");
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Executor pool
// ---------------------------------------------------------------------------

/// Factory closure that produces a cell's future. Crosses the thread boundary
/// (`Send`) and returns a `!Send` future that runs on the worker's `LocalSet`.
pub type SpawnFactory =
    Box<dyn FnOnce(watch::Receiver<()>) -> Pin<Box<dyn Future<Output = ()>>> + Send>;

/// A request to spawn a cell on an executor worker thread.
///
/// The factory closure crosses the thread boundary (Send) and produces
/// a !Send future that runs on the worker's LocalSet.
pub struct SpawnRequest {
    /// Human-readable name for tracing spans (e.g. "kernel", "echo-cell").
    pub name: String,
    /// Factory that produces the cell's future. Receives a shutdown receiver
    /// so cells can drain gracefully.
    pub factory: SpawnFactory,
    /// Optional channel to send the cell's exit code back to the caller.
    /// Used by the kernel to pipe its exit code to the CLI.
    pub result_tx: Option<tokio::sync::oneshot::Sender<Result<i32>>>,
}

/// Pool of executor worker threads for M:N cell scheduling.
///
/// Each worker is an OS thread with a `current_thread` tokio runtime and a
/// `LocalSet`.  Cells are assigned to workers and cooperatively scheduled
/// via the EWMA fuel estimator.
/// Channel depth per worker. Matches the connection rate limit TODO (64
/// concurrent cells per protocol). Prevents OOM under spawn bursts.
const SPAWN_CHANNEL_DEPTH: usize = 64;

pub struct ExecutorPool {
    senders: Vec<mpsc::Sender<SpawnRequest>>,
    threads: Vec<Option<JoinHandle<Result<()>>>>,
    cell_counts: Arc<Vec<AtomicUsize>>,
    next: AtomicUsize,
    /// Shared engine for all cells. Callers pass this to `cell::Builder` so
    /// all cells on a worker respond to `increment_epoch()` on the same engine.
    runtime_engine: cell::engine::RuntimeEngine,
    filesystem_resources: Arc<cache::HostResources>,
}

impl ExecutorPool {
    /// Create a new executor pool with `n` worker threads.
    ///
    /// Each worker thread runs its own `current_thread` tokio runtime.
    /// Pass `0` to use `std::thread::available_parallelism()`.
    pub fn try_new(
        n: usize,
        shutdown: watch::Receiver<()>,
        filesystem_resources: Arc<cache::HostResources>,
    ) -> Result<Self> {
        let n = if n == 0 {
            std::thread::available_parallelism()
                .map(|p| p.get())
                .unwrap_or(1)
        } else {
            n
        };

        // Create one Engine and one published epoch clock for the pool. Every
        // Store callback records transitions from this paired clock.
        let (runtime_engine, epoch_publisher) = cell::engine::runtime_engine()
            .map_err(|e| anyhow::anyhow!("failed to create shared wasmtime engine: {e}"))?;
        let mut epoch_publisher = Some(epoch_publisher);

        let mut senders = Vec::with_capacity(n);
        let mut threads = Vec::with_capacity(n);
        let cell_counts: Vec<AtomicUsize> = (0..n).map(|_| AtomicUsize::new(0)).collect();
        let cell_counts = Arc::new(cell_counts);

        for i in 0..n {
            let (tx, rx) = mpsc::channel(SPAWN_CHANNEL_DEPTH);
            let shutdown = shutdown.clone();
            let counts = cell_counts.clone();
            let worker_epoch_publisher = (i == 0).then(|| {
                epoch_publisher
                    .take()
                    .expect("worker 0 owns the sole epoch publisher")
            });
            let handle = std::thread::Builder::new()
                .name(format!("executor-{}", i))
                .spawn(move || worker_loop(i, rx, shutdown, counts, worker_epoch_publisher))
                .with_context(|| format!("failed to spawn executor worker thread {i}"))?;
            senders.push(tx);
            threads.push(Some(handle));
        }

        tracing::info!(workers = n, "executor pool started");

        Ok(Self {
            senders,
            threads,
            cell_counts,
            next: AtomicUsize::new(0),
            runtime_engine,
            filesystem_resources,
        })
    }

    /// Create a new executor pool, panicking on startup errors.
    ///
    /// Prefer `try_new` in startup paths that should return typed errors.
    pub fn new(
        n: usize,
        shutdown: watch::Receiver<()>,
        filesystem_resources: Arc<cache::HostResources>,
    ) -> Self {
        Self::try_new(n, shutdown, filesystem_resources).unwrap_or_else(|e| panic!("{e:#}"))
    }

    /// Submit a cell to the pool using least-loaded assignment.
    ///
    /// Returns `Err` if the chosen worker's channel is full or closed.
    /// Uses `try_send` to avoid deadlock when a cell on worker N spawns
    /// a child that routes back to the same worker.
    pub fn spawn(&self, request: SpawnRequest) -> Result<(), SpawnRequest> {
        let n = self.senders.len();

        // Find the worker with the fewest cells, falling back to
        // round-robin when all counts are equal (including all-zero).
        let mut best = 0;
        let mut best_count = self.cell_counts[0].load(Ordering::Relaxed);
        let mut all_equal = true;
        for i in 1..n {
            let count = self.cell_counts[i].load(Ordering::Relaxed);
            if count < best_count {
                best = i;
                best_count = count;
                all_equal = false;
            } else if count != best_count {
                all_equal = false;
            }
        }
        if all_equal {
            best = self.next.fetch_add(1, Ordering::Relaxed) % n;
        }

        match self.senders[best].try_send(request) {
            Ok(()) => {
                self.cell_counts[best].fetch_add(1, Ordering::Relaxed);
                Ok(())
            }
            Err(mpsc::error::TrySendError::Full(r) | mpsc::error::TrySendError::Closed(r)) => {
                Err(r)
            }
        }
    }

    /// Number of worker threads in the pool.
    pub fn worker_count(&self) -> usize {
        self.senders.len()
    }

    /// Shared Wasmtime engine for all cells in this pool.
    ///
    /// Pass this to `cell::Builder::with_runtime_engine()` so all cells share the same
    /// Engine and respond to `Engine::increment_epoch()`.
    pub fn runtime_engine(&self) -> cell::engine::RuntimeEngine {
        self.runtime_engine.clone()
    }

    pub fn filesystem_resources(&self) -> &Arc<cache::HostResources> {
        &self.filesystem_resources
    }

    pub async fn shutdown_and_join(mut self) {
        self.senders.clear();
        let mut threads = std::mem::take(&mut self.threads);
        if let Err(error) = tokio::task::spawn_blocking(move || {
            for handle in &mut threads {
                join_executor_thread(handle);
            }
        })
        .await
        {
            tracing::error!(%error, "executor join task failed");
        }
    }
}

impl Drop for ExecutorPool {
    fn drop(&mut self) {
        // Close all channels so workers exit their recv loops.
        self.senders.clear();
        // Join all worker threads.
        for handle in &mut self.threads {
            join_executor_thread(handle);
        }
    }
}

fn join_executor_thread(handle: &mut Option<JoinHandle<Result<()>>>) {
    if let Some(handle) = handle.take() {
        match handle.join() {
            Ok(Ok(())) => {}
            Ok(Err(error)) => tracing::error!(%error, "executor worker failed"),
            Err(panic) => {
                let message = panic
                    .downcast_ref::<&str>()
                    .copied()
                    .or_else(|| panic.downcast_ref::<String>().map(String::as_str))
                    .unwrap_or("<non-string panic>");
                tracing::error!(panic = message, "executor worker panicked");
            }
        }
    }
}

/// Drop guard that decrements the cell count when a cell task completes
/// or panics.  Prevents counter leaks on panic (adversarial finding #3).
struct CellCountGuard {
    counts: Arc<Vec<AtomicUsize>>,
    worker_id: usize,
}

impl Drop for CellCountGuard {
    fn drop(&mut self) {
        // Saturating subtract prevents underflow to usize::MAX (finding #4).
        let _ = self.counts[self.worker_id].try_update(Ordering::Relaxed, Ordering::Relaxed, |c| {
            Some(c.saturating_sub(1))
        });
    }
}

/// The event loop for a single executor worker thread.
///
/// Runs a `current_thread` tokio runtime with a `LocalSet`.  Receives
/// `SpawnRequest` factories over the channel, spawns them as local tasks.
/// Each cell cooperatively yields via the EWMA fuel estimator.
///
/// An epoch tick task publishes a paired Wasmtime epoch every EPOCH_TICK_MS.
/// Store callbacks record the latest transition. Fuel-flushed hooks settle
/// old-epoch work before they open the new allowance.
fn worker_loop(
    id: usize,
    rx: mpsc::Receiver<SpawnRequest>,
    shutdown: watch::Receiver<()>,
    cell_counts: Arc<Vec<AtomicUsize>>,
    epoch_publisher: Option<cell::engine::EpochPublisher>,
) -> Result<()> {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .with_context(|| format!("executor-{id}: failed to build runtime"))?;

    let local = tokio::task::LocalSet::new();
    let tasks = rpc::local_tasks::LocalTaskScope::enter();
    let _span = tracing::info_span!("executor", worker = id).entered();

    rt.block_on(local.run_until(async move {
        // The sole publisher advances the shared Engine and then publishes the
        // paired sequence. Store callbacks only record the transition. A
        // fuel-flushed hook performs settlement and installs the next grant.
        if let Some(mut epoch_publisher) = epoch_publisher {
            tokio::task::spawn_local(async move {
                let mut interval = tokio::time::interval(Duration::from_millis(EPOCH_TICK_MS));
                interval.tick().await; // skip immediate first tick
                loop {
                    interval.tick().await;
                    epoch_publisher.tick();
                }
            });
        }

        let mut rx = rx;
        let mut shutdown = shutdown;
        let mut cells = tokio::task::JoinSet::new();
        loop {
            tokio::select! {
                _ = cells.join_next(), if !cells.is_empty() => {},
                req = rx.recv() => match req {
                    Some(spawn_req) => {
                        let cell_name = spawn_req.name;
                        let factory = spawn_req.factory;
                        let result_tx = spawn_req.result_tx;
                        let cell_shutdown = shutdown.clone();
                        let guard = CellCountGuard {
                            counts: cell_counts.clone(),
                            worker_id: id,
                        };
                        let span = tracing::info_span!("cell", name = %cell_name);
                        let cancelled = rpc::local_tasks::shutdown_token();
                        let handle = tokio::task::spawn_local(async move {
                            let _guard = guard;
                            tokio::select! {
                                biased;
                                _ = cancelled.cancelled() => false,
                                _ = (factory)(cell_shutdown) => true,
                            }
                        }.instrument(span));
                        // Monitor the cell task for panics and send exit code.
                        let cell_name_log = cell_name.clone();
                        cells.spawn_local(async move {
                            match handle.await {
                                Ok(true) => {
                                    if let Some(tx) = result_tx {
                                        let _ = tx.send(Ok(0));
                                    }
                                }
                                Ok(false) => {
                                    if let Some(tx) = result_tx {
                                        let _ = tx.send(Err(anyhow::anyhow!("executor shutting down")));
                                    }
                                }
                                Err(e) if e.is_panic() => {
                                    tracing::error!(
                                        cell = %cell_name_log,
                                        "cell panicked: {}",
                                        e,
                                    );
                                    if let Some(tx) = result_tx {
                                        let _ = tx.send(Err(anyhow::anyhow!("cell panicked")));
                                    }
                                }
                                Err(e) => {
                                    tracing::error!(
                                        cell = %cell_name_log,
                                        "cell task failed: {}",
                                        e,
                                    );
                                    if let Some(tx) = result_tx {
                                        let _ = tx.send(Err(e.into()));
                                    }
                                }
                            }
                        });
                    }
                    None => break, // channel closed
                },
                _ = shutdown.changed() => break,
            }
        }
        // Stop admission, then release and join the root cell owners. Their
        // nested RPC workers and OwnedChildLifecycle tasks remain scheduled on
        // this LocalSet until managed disconnection and backend teardown finish.
        rx.close();
        drop(rx);
        tasks.shutdown();
        while cells.join_next().await.is_some() {}
        tasks.wait().await;
        tracing::info!("executor worker shutting down");
    }));

    Ok(())
}

// ---------------------------------------------------------------------------
// SwarmService
// ---------------------------------------------------------------------------

use crate::host::{KuboBootstrapInfo, SwarmCommand};
use rpc::NetworkState;

// Re-export WagiService so cli/main.rs can use `ww::services::WagiService`.
pub use crate::dispatcher::server::WagiService;

// Re-export AdminService so cli/main.rs can use `ww::services::AdminService`.
pub use crate::metrics::AdminService;

/// Parameters for constructing a libp2p [`crate::host::Net`] inside the swarm thread.
///
/// The host must be constructed on the same tokio runtime that will poll it,
/// because `with_tokio()` registers TCP listeners with the current reactor.
/// Constructing on one runtime and polling on another is a cross-runtime bug.
pub struct SwarmServiceParams {
    pub listen: Vec<libp2p::Multiaddr>,
    pub keypair: libp2p::identity::Keypair,
    pub kubo_bootstrap: Option<KuboBootstrapInfo>,
    pub kubo_peers: Vec<(libp2p::PeerId, libp2p::Multiaddr)>,
}

/// The libp2p swarm running on its own thread.
///
/// Sends back `stream_control` and `network_state` via oneshot channels
/// after constructing the host on the correct runtime.
pub struct SwarmService {
    pub params: SwarmServiceParams,
    pub cmd_rx: mpsc::Receiver<SwarmCommand>,
    pub ready_tx: tokio::sync::oneshot::Sender<Result<SwarmReady>>,
    pub network_state: NetworkState,
}

/// Values sent back from SwarmService after host construction.
pub struct SwarmReady {
    pub stream_control: libp2p_stream::Control,
    pub network_state: NetworkState,
}

impl Service for SwarmService {
    fn run(self, mut shutdown: watch::Receiver<()>) -> Result<()> {
        // Multi-thread runtime so libp2p-swarm's per-connection upgrade tasks
        // (each running a synchronous rustls TLS handshake with Ed25519
        // verification) distribute across worker threads. With current_thread,
        // simultaneous QUIC handshakes serialize through one core; profiling
        // showed ~50% busy time in curve25519_dalek during kad bootstrap
        // storms (#456). The event loop itself (select_next_some) still runs
        // on the OS thread Host::spawn created (named "swarm") via block_on;
        // worker threads only execute spawned tasks. See doc/runtimes.md.
        let rt = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .thread_name("ww-swarm-worker")
            .build()?;
        let _span = tracing::info_span!("swarm").entered();

        rt.block_on(async move {
            // Construct the host on THIS runtime so TCP listeners register
            // with the correct reactor. If construction fails, forward the
            // error to the main thread so the user sees the real cause
            // (e.g. bind failure) instead of a "channel closed" symptom.
            let p = self.params;
            let host =
                match crate::host::Net::new(p.listen, p.keypair, p.kubo_bootstrap, p.kubo_peers) {
                    Ok(h) => h,
                    Err(e) => {
                        let _ = self.ready_tx.send(Err(e));
                        return Ok(());
                    }
                };
            let network_state = self.network_state;
            let stream_control = host.stream_control();

            // Send construction results back to the main thread.
            let _ = self.ready_tx.send(Ok(SwarmReady {
                stream_control,
                network_state: network_state.clone(),
            }));

            tokio::select! {
                result = host.run(network_state, self.cmd_rx) => result,
                _ = shutdown.changed() => {
                    tracing::info!("swarm shutting down");
                    Ok(())
                }
            }
        })
    }
}

// ---------------------------------------------------------------------------
// CompilationService
// ---------------------------------------------------------------------------

/// Request to compile WASM bytecode into a wasmtime Component.
pub struct CompileRequest {
    pub bytecode: Vec<u8>,
    pub engine: Arc<wasmtime::Engine>,
    pub result_tx: tokio::sync::oneshot::Sender<Result<wasmtime::component::Component>>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
struct CompileKey {
    wasm_hash: [u8; 32],
    engine_id: usize,
}

impl CompileKey {
    fn new(bytecode: &[u8], engine: &Arc<wasmtime::Engine>) -> Self {
        Self {
            wasm_hash: *blake3::hash(bytecode).as_bytes(),
            engine_id: Arc::as_ptr(engine) as usize,
        }
    }
}

struct CompileJob {
    key: CompileKey,
    bytecode: Vec<u8>,
    engine: Arc<wasmtime::Engine>,
}

struct CompileOutcome {
    key: CompileKey,
    result: std::result::Result<wasmtime::component::Component, String>,
}

fn default_compile_workers() -> usize {
    let cpu_count = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(1);
    (cpu_count / 2).clamp(1, 4)
}

fn compile_worker_count() -> usize {
    match std::env::var("WW_COMPILE_WORKERS") {
        Ok(raw) => raw
            .parse::<usize>()
            .ok()
            .filter(|n| *n > 0)
            .unwrap_or_else(default_compile_workers),
        Err(_) => default_compile_workers(),
    }
}

/// Dedicated component-load thread that offloads cache misses in
/// `Component::from_binary` away from executor worker threads.
///
/// Caches compiled components by `(wasm_blake3, engine identity)` and deduplicates
/// concurrent compiles of the same key.
pub struct CompilationService {
    pub request_rx: mpsc::Receiver<CompileRequest>,
}

impl Service for CompilationService {
    fn run(self, mut shutdown: watch::Receiver<()>) -> Result<()> {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()?;
        let _span = tracing::info_span!("compiler").entered();

        rt.block_on(async move {
            let mut rx = self.request_rx;
            let mut cache: HashMap<CompileKey, wasmtime::component::Component> = HashMap::new();
            let mut inflight: HashMap<
                CompileKey,
                Vec<tokio::sync::oneshot::Sender<Result<wasmtime::component::Component>>>,
            > = HashMap::new();

            let worker_count = compile_worker_count();
            tracing::info!(workers = worker_count, "starting compile worker pool");

            let (job_tx, job_rx) = std::sync::mpsc::channel::<CompileJob>();
            let job_rx = Arc::new(Mutex::new(job_rx));
            let (outcome_tx, mut outcome_rx) = tokio::sync::mpsc::unbounded_channel();
            let mut worker_handles = Vec::with_capacity(worker_count);

            for idx in 0..worker_count {
                let job_rx = Arc::clone(&job_rx);
                let outcome_tx = outcome_tx.clone();
                match std::thread::Builder::new()
                    .name(format!("compiler-worker-{idx}"))
                    .spawn(move || {
                        loop {
                            let recv_result = {
                                match job_rx.lock() {
                                    Ok(lock) => lock.recv(),
                                    Err(e) => {
                                        tracing::error!(error = %e, worker = idx, "compile worker queue lock poisoned");
                                        break;
                                    }
                                }
                            };

                            let job = match recv_result {
                                Ok(job) => job,
                                Err(_) => break, // service dropped sender; time to exit
                            };

                            let start = std::time::Instant::now();
                            let result = cell::engine::compile_component(&job.engine, &job.bytecode)
                                .map_err(|e| e.to_string());
                            if let Ok(ref _component) = result {
                                tracing::info!(
                                    ?job.key.wasm_hash,
                                    engine_id = job.key.engine_id,
                                    elapsed_ms = start.elapsed().as_millis(),
                                    "loaded component through Wasmtime cache"
                                );
                            }

                            if outcome_tx
                                .send(CompileOutcome {
                                    key: job.key,
                                    result,
                                })
                                .is_err()
                            {
                                break; // service loop exited
                            }
                        }
                    })
                {
                    Ok(handle) => worker_handles.push(handle),
                    Err(e) => {
                        tracing::error!(error = %e, worker = idx, "failed to spawn compile worker thread");
                    }
                }
            }
            if worker_handles.is_empty() {
                anyhow::bail!("failed to start compile worker pool");
            }
            drop(outcome_tx);

            loop {
                tokio::select! {
                    req = rx.recv() => match req {
                        Some(req) => {
                            let key = CompileKey::new(&req.bytecode, &req.engine);
                            if let Some(component) = cache.get(&key) {
                                tracing::debug!(
                                    ?key.wasm_hash,
                                    engine_id = key.engine_id,
                                    "compilation cache hit"
                                );
                                let _ = req.result_tx.send(Ok(component.clone()));
                            } else if let Some(waiters) = inflight.get_mut(&key) {
                                tracing::debug!(
                                    ?key.wasm_hash,
                                    engine_id = key.engine_id,
                                    waiters = waiters.len() + 1,
                                    "compilation inflight dedupe"
                                );
                                waiters.push(req.result_tx);
                            } else {
                                inflight.insert(key, vec![req.result_tx]);
                                if job_tx
                                    .send(CompileJob {
                                        key,
                                        bytecode: req.bytecode,
                                        engine: req.engine,
                                    })
                                    .is_err()
                                {
                                    if let Some(waiters) = inflight.remove(&key) {
                                        for waiter in waiters {
                                            let _ = waiter.send(Err(anyhow::anyhow!(
                                                "compilation worker pool unavailable"
                                            )));
                                        }
                                    }
                                }
                            }
                        }
                        None => break,
                    },
                    maybe_outcome = outcome_rx.recv() => {
                        match maybe_outcome {
                            Some(outcome) => {
                                if let Some(waiters) = inflight.remove(&outcome.key) {
                                    match outcome.result {
                                        Ok(component) => {
                                            cache.insert(outcome.key, component.clone());
                                            for waiter in waiters {
                                                let _ = waiter.send(Ok(component.clone()));
                                            }
                                        }
                                        Err(err) => {
                                            for waiter in waiters {
                                                let _ = waiter.send(Err(anyhow::anyhow!(err.clone())));
                                            }
                                        }
                                    }
                                }
                            }
                            None => break,
                        }
                    }
                    _ = shutdown.changed() => break,
                }
            }

            drop(job_tx);
            for handle in worker_handles {
                if let Err(panic) = handle.join() {
                    let msg = panic
                        .downcast_ref::<&str>()
                        .copied()
                        .or_else(|| panic.downcast_ref::<String>().map(String::as_str))
                        .unwrap_or("<non-string panic>");
                    tracing::error!(panic = msg, "compile worker thread panicked");
                }
            }

            tracing::info!("compilation service shutting down");
            Ok(())
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn test_host() -> Host {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .expect("test Host runtime");
        let (owner, resources) = cache::HostFsOwner::start(
            runtime.handle().clone(),
            cache::HostLimits::new(128, 65, 64).unwrap(),
        )
        .expect("test Host filesystem owner");
        Box::leak(Box::new(runtime));
        Host::new(resources, owner)
    }

    fn shutdown_host(host: Host) {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("test shutdown runtime")
            .block_on(host.shutdown());
    }

    struct StartedService {
        name: &'static str,
        started: std::sync::mpsc::Sender<&'static str>,
    }

    impl Service for StartedService {
        fn run(self, mut shutdown: watch::Receiver<()>) -> Result<()> {
            self.started.send(self.name)?;
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()?;
            rt.block_on(async move {
                let _ = shutdown.changed().await;
            });
            Ok(())
        }
    }

    #[test]
    fn host_spawns_and_shuts_down_services() {
        let mut host = test_host();
        let (started_tx, started_rx) = std::sync::mpsc::channel();
        for name in ["svc-1", "svc-2"] {
            host.spawn(
                name,
                StartedService {
                    name,
                    started: started_tx.clone(),
                },
            );
        }
        let mut started = [
            started_rx.recv_timeout(Duration::from_secs(5)).unwrap(),
            started_rx.recv_timeout(Duration::from_secs(5)).unwrap(),
        ];
        started.sort_unstable();
        assert_eq!(started, ["svc-1", "svc-2"]);

        shutdown_host(host);
    }

    #[test]
    fn executor_pool_runs_cells() {
        let (shutdown_tx, shutdown_rx) = watch::channel(());
        let pool = ExecutorPool::new(2, shutdown_rx, test_filesystem_resources());

        let (tx, rx) = std::sync::mpsc::channel();
        let request = SpawnRequest {
            name: "test-cell".into(),
            factory: Box::new(move |_shutdown| {
                Box::pin(async move {
                    tx.send(42).unwrap();
                })
            }),
            result_tx: None,
        };

        assert!(pool.spawn(request).is_ok(), "spawn failed");

        let result = rx.recv_timeout(Duration::from_secs(5)).unwrap();
        assert_eq!(result, 42);

        drop(shutdown_tx);
    }

    #[tokio::test]
    async fn executor_pool_least_loaded_assignment() {
        let (shutdown_tx, shutdown_rx) = watch::channel(());
        let pool = ExecutorPool::new(2, shutdown_rx, test_filesystem_resources());

        let (block_tx, block_rx) = tokio::sync::oneshot::channel::<()>();
        let (started_tx, started_rx) = tokio::sync::oneshot::channel();
        let long_cell = SpawnRequest {
            name: "long-cell".into(),
            factory: Box::new(move |_shutdown| {
                Box::pin(async move {
                    started_tx.send(std::thread::current().id()).unwrap();
                    let _ = block_rx.await;
                })
            }),
            result_tx: None,
        };
        assert!(pool.spawn(long_cell).is_ok(), "spawn long_cell failed");
        let busy_worker = tokio::time::timeout(Duration::from_secs(5), started_rx)
            .await
            .expect("long cell must start")
            .unwrap();

        // Both calls must avoid the occupied worker. Round-robin alone would
        // route the second call back to it.
        for _ in 0..2 {
            let (worker_tx, worker_rx) = tokio::sync::oneshot::channel();
            let (result_tx, result_rx) = tokio::sync::oneshot::channel();
            let (release_tx, release_rx) = tokio::sync::oneshot::channel();
            let short_cell = SpawnRequest {
                name: "short-cell".into(),
                factory: Box::new(move |_shutdown| {
                    Box::pin(async move {
                        release_rx.await.unwrap();
                        worker_tx.send(std::thread::current().id()).unwrap();
                    })
                }),
                result_tx: Some(result_tx),
            };
            assert!(pool.spawn(short_cell).is_ok(), "spawn short_cell failed");
            // spawn increments the count after enqueueing. Hold completion
            // until that increment has happened.
            release_tx.send(()).unwrap();
            assert_eq!(
                tokio::time::timeout(Duration::from_secs(5), result_rx)
                    .await
                    .expect("short cell must finish")
                    .unwrap()
                    .unwrap(),
                0
            );
            assert_ne!(worker_rx.await.unwrap(), busy_worker);
        }

        let _ = block_tx.send(());
        drop(shutdown_tx);
    }

    #[test]
    fn host_handles_service_error() {
        struct FailService;
        impl Service for FailService {
            fn run(self, _shutdown: watch::Receiver<()>) -> Result<()> {
                anyhow::bail!("intentional failure")
            }
        }

        let mut host = test_host();
        host.spawn("fail-svc", FailService);
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let exit = rt
            .block_on(host.next_service_exit())
            .expect("service exit event");
        assert_eq!(exit.name, "fail-svc");
        assert_eq!(exit.error.as_deref(), Some("intentional failure"));
        shutdown_host(host);
    }

    #[test]
    fn host_handles_service_panic() {
        struct PanicService;
        impl Service for PanicService {
            fn run(self, _shutdown: watch::Receiver<()>) -> Result<()> {
                panic!("intentional panic")
            }
        }

        let mut host = test_host();
        host.spawn("panic-svc", PanicService);
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let exit = rt
            .block_on(host.next_service_exit())
            .expect("service exit event");
        assert_eq!(exit.name, "panic-svc");
        assert_eq!(
            exit.error.as_deref(),
            Some("service panicked: intentional panic")
        );
        shutdown_host(host);
    }

    #[tokio::test]
    async fn executor_pool_spawn_after_shutdown() {
        let (shutdown_tx, shutdown_rx) = watch::channel(());
        let pool = ExecutorPool::new(1, shutdown_rx, test_filesystem_resources());

        drop(shutdown_tx);
        tokio::time::timeout(Duration::from_secs(5), pool.senders[0].closed())
            .await
            .expect("worker must close admission after shutdown");

        // Spawn should fail gracefully.
        let request = SpawnRequest {
            name: "doomed-cell".into(),
            factory: Box::new(|_| Box::pin(async {})),
            result_tx: None,
        };
        assert!(
            pool.spawn(request).is_err(),
            "spawn after shutdown should fail"
        );
    }

    #[tokio::test]
    async fn round_robin_distributes_across_idle_workers() {
        let (shutdown_tx, shutdown_rx) = watch::channel(());
        let pool = ExecutorPool::new(4, shutdown_rx, test_filesystem_resources());

        let mut workers = std::collections::HashSet::new();
        for i in 0..4 {
            let (worker_tx, worker_rx) = tokio::sync::oneshot::channel();
            let (result_tx, result_rx) = tokio::sync::oneshot::channel();
            let (release_tx, release_rx) = tokio::sync::oneshot::channel();
            let request = SpawnRequest {
                name: format!("rr-cell-{i}"),
                factory: Box::new(move |_shutdown| {
                    Box::pin(async move {
                        release_rx.await.unwrap();
                        worker_tx.send(std::thread::current().id()).unwrap();
                    })
                }),
                result_tx: Some(result_tx),
            };
            assert!(pool.spawn(request).is_ok());
            // Hold completion until spawn has incremented the count. The
            // result then certifies all workers are idle for the next call.
            release_tx.send(()).unwrap();
            assert_eq!(
                tokio::time::timeout(Duration::from_secs(5), result_rx)
                    .await
                    .expect("cell must finish")
                    .unwrap()
                    .unwrap(),
                0
            );
            assert!(workers.insert(worker_rx.await.unwrap()));
        }

        drop(shutdown_tx);
    }

    #[test]
    fn spawn_request_result_tx_receives_exit_code() {
        let (shutdown_tx, shutdown_rx) = watch::channel(());
        let pool = ExecutorPool::new(1, shutdown_rx, test_filesystem_resources());

        let (result_tx, result_rx) = tokio::sync::oneshot::channel();
        let request = SpawnRequest {
            name: "exit-cell".into(),
            factory: Box::new(|_shutdown| Box::pin(async {})),
            result_tx: Some(result_tx),
        };
        assert!(pool.spawn(request).is_ok());

        // The worker sends Ok(0) on successful completion.
        let result = result_rx.blocking_recv().unwrap();
        assert_eq!(result.unwrap(), 0);

        drop(shutdown_tx);
    }

    #[test]
    fn spawn_request_panic_sends_error_via_result_tx() {
        let (shutdown_tx, shutdown_rx) = watch::channel(());
        let pool = ExecutorPool::new(1, shutdown_rx, test_filesystem_resources());

        let (result_tx, result_rx) = tokio::sync::oneshot::channel();
        let request = SpawnRequest {
            name: "panic-cell".into(),
            factory: Box::new(|_shutdown| {
                Box::pin(async {
                    panic!("intentional cell panic");
                })
            }),
            result_tx: Some(result_tx),
        };
        assert!(pool.spawn(request).is_ok());

        // The worker catches the panic and sends Err.
        let result = result_rx.blocking_recv().unwrap();
        assert!(result.is_err(), "panicked cell should produce Err");

        drop(shutdown_tx);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn executor_shutdown_joins_admitted_cell_and_reports_cancellation() {
        let (_shutdown_tx, shutdown_rx) = watch::channel(());
        let pool = ExecutorPool::new(1, shutdown_rx, test_filesystem_resources());
        let (entered_tx, entered_rx) = tokio::sync::oneshot::channel();
        let (result_tx, result_rx) = tokio::sync::oneshot::channel();
        assert!(pool
            .spawn(SpawnRequest {
                name: "admitted-shutdown".into(),
                factory: Box::new(move |_shutdown| {
                    Box::pin(async move {
                        let _ = entered_tx.send(());
                        std::future::pending::<()>().await;
                    })
                }),
                result_tx: Some(result_tx),
            })
            .is_ok());
        entered_rx.await.expect("admitted cell started");

        pool.shutdown_and_join().await;
        let error = result_rx
            .await
            .expect("worker must settle the admitted result")
            .unwrap_err();
        assert!(error.to_string().contains("executor shutting down"));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn startup_failure_runs_ordered_filesystem_teardown() {
        let mut host = test_host();
        let resources = Arc::clone(host.filesystem_resources());
        let (scope, owner, finalizer, completion) = resources.try_filesystem_scope().unwrap();
        let setup = resources
            .try_cleanup_transaction(resources.try_logical_io().unwrap(), &[&scope])
            .unwrap();
        let cleanup = setup.try_cleanup_phase().unwrap();
        let cleanup_scope = scope.clone();
        let path = setup
            .run_blocking(
                cache::FilesystemOperation::ProcessSetup,
                tempfile::TempDir::new,
            )
            .await
            .unwrap()
            .install(|directory| {
                let directory = directory.unwrap();
                let path = directory.path().to_owned();
                cleanup_scope
                    .own_cleanup_from_phase(
                        cleanup,
                        cache::FilesystemOperation::ProcessCleanup,
                        move || directory.close(),
                    )
                    .unwrap();
                path
            });
        drop(setup);
        assert!(path.exists());

        let startup_result = Err::<(), _>(anyhow::anyhow!("injected startup failure"));
        host.close_filesystem_admission();
        drop(scope);
        drop(owner);
        assert_eq!(finalizer.run().await, Ok(()));
        assert_eq!(completion.wait().await, Ok(()));
        host.shutdown_filesystem().await.unwrap();
        host.shutdown_services().await;

        assert!(startup_result.is_err());
        assert!(!path.exists());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn host_shutdown_waits_for_admitted_work_and_root_cleanup() {
        use std::future::Future;
        use std::task::Poll;

        let mut host = test_host();
        let resources = Arc::clone(host.filesystem_resources());
        let (scope, owner, finalizer, completion) = resources.try_filesystem_scope().unwrap();
        let setup = resources
            .try_cleanup_transaction(resources.try_logical_io().unwrap(), &[&scope])
            .unwrap();
        let cleanup = setup.try_cleanup_phase().unwrap();
        let directory = tempfile::TempDir::new().unwrap();
        let path = directory.path().to_owned();
        scope
            .own_cleanup_from_phase(
                cleanup,
                cache::FilesystemOperation::ProcessCleanup,
                move || directory.close(),
            )
            .unwrap();
        let (release_tx, release_rx) = tokio::sync::oneshot::channel();
        let (terminal_tx, terminal_rx) = tokio::sync::oneshot::channel();
        setup.spawn_retained_terminal(
            cache::FilesystemOperation::ProcessSetup,
            async move {
                let _ = release_rx.await;
            },
            move |result| {
                let _ = terminal_tx.send(result);
            },
        );

        host.close_filesystem_admission();
        drop(scope);
        drop(owner);
        let mut shutdown = Box::pin(host.shutdown_filesystem());
        std::future::poll_fn(|context| {
            assert!(matches!(shutdown.as_mut().poll(context), Poll::Pending));
            Poll::Ready(())
        })
        .await;
        assert!(path.exists());

        release_tx.send(()).unwrap();
        shutdown.await.unwrap();
        terminal_rx.await.unwrap().unwrap();
        assert_eq!(finalizer.run().await, Ok(()));
        assert_eq!(completion.wait().await, Ok(()));
        assert!(!path.exists());
        host.shutdown_services().await;
    }

    #[test]
    fn executor_pool_drop_joins_workers() {
        let (shutdown_tx, shutdown_rx) = watch::channel(());
        let pool = ExecutorPool::new(2, shutdown_rx, test_filesystem_resources());

        // Spawn a cell so workers are doing real work.
        let (tx, rx) = std::sync::mpsc::channel();
        let request = SpawnRequest {
            name: "drop-test".into(),
            factory: Box::new(move |_shutdown| {
                Box::pin(async move {
                    tx.send(()).unwrap();
                })
            }),
            result_tx: None,
        };
        assert!(pool.spawn(request).is_ok());
        rx.recv_timeout(Duration::from_secs(5)).unwrap();

        // Drop should close channels and join threads without hanging.
        drop(shutdown_tx);
        drop(pool); // should not hang
    }

    #[test]
    fn executor_pool_shutdown_disconnects_detached_rpc_owners() {
        use crate::system_capnp::{byte_stream, membrane, process};
        use capnp::capability::Promise;
        use capnp_rpc::{rpc_twoparty_capnp::Side, twoparty::VatNetwork};
        use rpc::managed_rpc::{ManagedRpc, ManagedRpcSystem};
        use std::{cell::RefCell, rc::Rc};
        use tokio_util::compat::{TokioAsyncReadCompatExt, TokioAsyncWriteCompatExt};

        struct Export(Rc<RefCell<Option<process::Client>>>);
        impl membrane::Server for Export {
            fn graft(
                self: capnp::capability::Rc<Self>,
                _: membrane::GraftParams,
                mut results: membrane::GraftResults,
            ) -> impl Future<Output = capnp::Result<()>> + 'static {
                let process = self.0.borrow_mut().take().unwrap();
                results
                    .get()
                    .init_extras(1)
                    .get(0)
                    .init_cap()
                    .set_as_capability(process.client.hook);
                Promise::ok(())
            }
        }

        for close_spawn_channel in [false, true] {
            let (shutdown_tx, shutdown_rx) = watch::channel(());
            let pool = ExecutorPool::new(1, shutdown_rx, test_filesystem_resources());
            let (ready_tx, ready_rx) = std::sync::mpsc::channel();
            assert!(pool
                .spawn(SpawnRequest {
                    name: "detached-vat".into(),
                    factory: Box::new(move |_| Box::pin(async move {
                        let exported = Rc::new(RefCell::new(None));
                        let bootstrap: membrane::Client =
                            capnp_rpc::new_client(Export(exported.clone()));
                        let (a, b) = tokio::io::duplex(8192);
                        let (ar, aw) = tokio::io::split(a);
                        let (br, bw) = tokio::io::split(b);
                        let mut server = ManagedRpcSystem::new(
                            Box::new(VatNetwork::new(
                                ar.compat(),
                                aw.compat_write(),
                                Side::Server,
                                Default::default(),
                            )),
                            Some(bootstrap.client),
                        );
                        let retained: membrane::Client = server.bootstrap(Side::Client);
                        let mut client = ManagedRpcSystem::new(
                            Box::new(VatNetwork::new(
                                br.compat(),
                                bw.compat_write(),
                                Side::Client,
                                Default::default(),
                            )),
                            None,
                        );
                        let remote: membrane::Client = client.bootstrap(Side::Server);
                        let (cleanup, observer) = rpc::cleanup_channel();
                        let (kill_tx, kill_rx) = watch::channel(false);
                        let (stream, _) = tokio::io::duplex(16);
                        let stream: byte_stream::Client = capnp_rpc::new_client(
                            rpc::ByteStreamImpl::new(stream, rpc::StreamMode::ReadOnly),
                        );
                        // The exported Process's bootstrap retains this same local
                        // connection. Dropping the LocalSet alone leaves the cycle.
                        *exported.borrow_mut() =
                            Some(capnp_rpc::new_client(rpc::ProcessImpl::with_bootstrap(
                                stream.clone(),
                                stream.clone(),
                                stream,
                                observer,
                                retained.client,
                                rpc::TerminationHandle::new(kill_tx),
                            )));
                        let server = ManagedRpc::spawn(server);
                        let client = ManagedRpc::spawn(client);
                        let response = remote.graft_request().send().promise.await.unwrap();
                        let process: process::Client = response
                            .get()
                            .unwrap()
                            .get_extras()
                            .unwrap()
                            .get(0)
                            .get_cap()
                            .get_as()
                            .unwrap();
                        drop(response);
                        process.stdout_request().send().promise.await.unwrap();
                        tokio::task::spawn_local(async move {
                            let _retained = (server, client, process, remote, cleanup);
                            std::future::pending::<()>().await;
                        });
                        ready_tx.send(kill_rx).unwrap();
                    })),
                    result_tx: None,
                })
                .is_ok());
            let kill = ready_rx.recv_timeout(Duration::from_secs(5)).unwrap();
            assert!(!*kill.borrow());
            if !close_spawn_channel {
                drop(shutdown_tx);
            }
            drop(pool);
            assert!(*kill.borrow(), "worker shutdown must disconnect detached RPC exports (channel close: {close_spawn_channel})");
        }
    }

    #[test]
    fn executor_pool_publishes_epochs_and_closes_the_clock_on_drop() {
        let (_shutdown_tx, shutdown_rx) = watch::channel(());
        let pool = ExecutorPool::new(1, shutdown_rx, test_filesystem_resources());
        let mut subscription = pool.runtime_engine().subscribe();
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("test runtime");

        rt.block_on(async {
            tokio::time::timeout(Duration::from_secs(1), subscription.changed())
                .await
                .expect("executor pool did not publish an epoch")
                .expect("executor pool epoch clock closed early");
        });

        drop(pool);
        rt.block_on(async {
            tokio::time::timeout(Duration::from_secs(1), async {
                while subscription.changed().await.is_ok() {}
            })
            .await
            .expect("executor pool epoch clock remained open after pool drop");
        });
    }

    #[test]
    fn cell_count_guard_saturates_on_underflow() {
        // Verify CellCountGuard doesn't wrap to usize::MAX.
        let counts = Arc::new(vec![AtomicUsize::new(0)]);
        {
            let _guard = CellCountGuard {
                counts: counts.clone(),
                worker_id: 0,
            };
            // Count is already 0 — dropping guard should saturate at 0.
        }
        assert_eq!(counts[0].load(Ordering::Relaxed), 0);
    }

    #[test]
    fn executor_pool_auto_detect_parallelism() {
        // n=0 should auto-detect available parallelism.
        let (_shutdown_tx, shutdown_rx) = watch::channel(());
        let pool = ExecutorPool::new(0, shutdown_rx, test_filesystem_resources());
        assert!(
            pool.worker_count() >= 1,
            "auto-detected pool should have at least 1 worker"
        );
    }

    #[test]
    fn spawn_request_no_result_tx_fire_and_forget() {
        // Cells without result_tx should complete without error.
        let (shutdown_tx, shutdown_rx) = watch::channel(());
        let pool = ExecutorPool::new(1, shutdown_rx, test_filesystem_resources());

        let (done_tx, done_rx) = std::sync::mpsc::channel();
        let request = SpawnRequest {
            name: "fire-forget".into(),
            factory: Box::new(move |_shutdown| {
                Box::pin(async move {
                    done_tx.send(()).unwrap();
                })
            }),
            result_tx: None,
        };
        assert!(pool.spawn(request).is_ok());
        done_rx.recv_timeout(Duration::from_secs(5)).unwrap();

        drop(shutdown_tx);
    }
}

#[cfg(test)]
#[path = "services_tracing_tests.rs"]
mod tracing_tests;
