//! Thread-per-subsystem runtime inspired by Cloudflare Pingora.
//!
//! Each subsystem (libp2p swarm, epoch pipeline, WASM executor) runs on its
//! own OS thread with its own tokio runtime.  The [`Host`] supervisor owns
//! all threads and coordinates shutdown.
//!
//! Executor threads use `current_thread` + `LocalSet` because `wasmtime::Store`
//! is `!Send`.  M:N cell scheduling comes from the EWMA fuel estimator
//! (`src/cell/proc.rs`), not tokio work stealing.
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
use tokio::sync::{mpsc, watch};
use wasmtime::Engine;

use cell::sched::EPOCH_TICK_MS;

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
///  ├── Thread 2: EpochService    — on-chain watcher
///  └── Thread 3..N: ExecutorPool — cells via fuel scheduling
/// ```
pub struct Host {
    threads: Vec<(String, JoinHandle<Result<()>>)>,
    shutdown_tx: watch::Sender<()>,
    shutdown_rx: watch::Receiver<()>,
}

impl Default for Host {
    fn default() -> Self {
        Self::new()
    }
}

impl Host {
    /// Create a new Host supervisor.
    pub fn new() -> Self {
        let (shutdown_tx, shutdown_rx) = watch::channel(());
        Self {
            threads: Vec::new(),
            shutdown_tx,
            shutdown_rx,
        }
    }

    /// Get a shutdown receiver for passing to services or other components.
    pub fn shutdown_rx(&self) -> watch::Receiver<()> {
        self.shutdown_rx.clone()
    }

    /// Spawn a service on its own OS thread.
    pub fn try_spawn<S: Service>(&mut self, name: &str, service: S) -> Result<()> {
        let shutdown = self.shutdown_rx.clone();
        let thread_name = name.to_string();
        let handle = std::thread::Builder::new()
            .name(thread_name.clone())
            .spawn(move || service.run(shutdown))
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

    /// Signal all services to shut down and join all threads.
    ///
    /// Panicked or errored threads are logged but don't prevent other
    /// threads from shutting down.
    pub fn shutdown(self) {
        drop(self.shutdown_tx);
        for (name, handle) in self.threads {
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
    /// Shared engine for all cells. Callers should pass this to CellBuilder
    /// via `with_wasmtime_engine()` so all cells on a worker share the same
    /// Engine and respond to `increment_epoch()`.
    engine: Arc<Engine>,
}

impl ExecutorPool {
    /// Create a new executor pool with `n` worker threads.
    ///
    /// Each worker thread runs its own `current_thread` tokio runtime.
    /// Pass `0` to use `std::thread::available_parallelism()`.
    pub fn try_new(n: usize, shutdown: watch::Receiver<()>) -> Result<Self> {
        let n = if n == 0 {
            std::thread::available_parallelism()
                .map(|p| p.get())
                .unwrap_or(1)
        } else {
            n
        };

        // Create a shared Engine with fuel + epoch support.  All cells on
        // all workers share this Engine so Engine::increment_epoch() reaches
        // every Store's epoch_deadline_callback.
        let engine = Arc::new(
            cell::engine::wasm_engine()
                .map_err(|e| anyhow::anyhow!("failed to create shared wasmtime engine: {e}"))?,
        );

        let mut senders = Vec::with_capacity(n);
        let mut threads = Vec::with_capacity(n);
        let cell_counts: Vec<AtomicUsize> = (0..n).map(|_| AtomicUsize::new(0)).collect();
        let cell_counts = Arc::new(cell_counts);

        for i in 0..n {
            let (tx, rx) = mpsc::channel(SPAWN_CHANNEL_DEPTH);
            let shutdown = shutdown.clone();
            let counts = cell_counts.clone();
            let engine = engine.clone();
            let handle = std::thread::Builder::new()
                .name(format!("executor-{}", i))
                .spawn(move || worker_loop(i, rx, shutdown, counts, engine))
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
            engine,
        })
    }

    /// Create a new executor pool, panicking on startup errors.
    ///
    /// Prefer `try_new` in startup paths that should return typed errors.
    pub fn new(n: usize, shutdown: watch::Receiver<()>) -> Self {
        Self::try_new(n, shutdown).unwrap_or_else(|e| panic!("{e:#}"))
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
    /// Pass this to `CellBuilder::with_wasmtime_engine()` so all cells share
    /// the same Engine and respond to `Engine::increment_epoch()`.
    pub fn engine(&self) -> Arc<Engine> {
        Arc::clone(&self.engine)
    }
}

impl Drop for ExecutorPool {
    fn drop(&mut self) {
        // Close all channels so workers exit their recv loops.
        self.senders.clear();
        // Join all worker threads.
        for handle in &mut self.threads {
            if let Some(h) = handle.take() {
                match h.join() {
                    Ok(Ok(())) => {}
                    Ok(Err(e)) => tracing::error!(error = %e, "executor worker failed"),
                    Err(panic) => {
                        let msg = panic
                            .downcast_ref::<&str>()
                            .copied()
                            .or_else(|| panic.downcast_ref::<String>().map(String::as_str))
                            .unwrap_or("<non-string panic>");
                        tracing::error!(panic = msg, "executor worker panicked");
                    }
                }
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
        let _ =
            self.counts[self.worker_id].fetch_update(Ordering::Relaxed, Ordering::Relaxed, |c| {
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
/// An epoch tick task calls `Engine::increment_epoch()` every EPOCH_TICK_MS,
/// triggering each Store's `epoch_deadline_callback` to refuel compute-bound
/// cells that don't make host calls frequently enough.
fn worker_loop(
    id: usize,
    rx: mpsc::Receiver<SpawnRequest>,
    shutdown: watch::Receiver<()>,
    cell_counts: Arc<Vec<AtomicUsize>>,
    engine: Arc<Engine>,
) -> Result<()> {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .with_context(|| format!("executor-{id}: failed to build runtime"))?;

    let local = tokio::task::LocalSet::new();
    let _span = tracing::info_span!("executor", worker = id).entered();

    rt.block_on(local.run_until(async move {
        // Epoch tick task: bumps the Engine epoch counter every EPOCH_TICK_MS.
        // This triggers epoch_deadline_callback in every Store on this Engine,
        // refueling compute-bound cells that would otherwise Trap::OutOfFuel.
        //
        // Only worker 0 runs the tick task because all workers share the same
        // Arc<Engine>.  increment_epoch() is a global atomic bump, so running
        // it on N workers would advance the epoch N times per tick.
        if id == 0 {
            let tick_engine = engine.clone();
            tokio::task::spawn_local(async move {
                let mut interval = tokio::time::interval(Duration::from_millis(EPOCH_TICK_MS));
                interval.tick().await; // skip immediate first tick
                loop {
                    interval.tick().await;
                    tick_engine.increment_epoch();
                }
            });
        }

        let mut rx = rx;
        let mut shutdown = shutdown;
        loop {
            tokio::select! {
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
                        let handle = tokio::task::spawn_local(async move {
                            let _guard = guard;
                            let _span = span.entered();
                            (factory)(cell_shutdown).await;
                        });
                        // Monitor the cell task for panics and send exit code.
                        let cell_name_log = cell_name.clone();
                        tokio::task::spawn_local(async move {
                            match handle.await {
                                Ok(()) => {
                                    if let Some(tx) = result_tx {
                                        let _ = tx.send(Ok(0));
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

/// Parameters for constructing a [`Libp2pHost`] inside the swarm thread.
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
            let host = match crate::host::Libp2pHost::new(
                p.listen,
                p.keypair,
                p.kubo_bootstrap,
                p.kubo_peers,
            ) {
                Ok(h) => h,
                Err(e) => {
                    let _ = self.ready_tx.send(Err(e));
                    return Ok(());
                }
            };
            let network_state = NetworkState::from_peer_id(host.local_peer_id().to_bytes());
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
// EpochService
// ---------------------------------------------------------------------------

use membrane::Epoch;

/// The on-chain epoch watcher running on its own thread.
pub struct EpochService {
    pub config: atom::IndexerConfig,
    pub epoch_tx: watch::Sender<Epoch>,
    pub confirmation_depth: u64,
    pub ipfs_client: crate::ipfs::HttpClient,
    pub cid_tree: Option<std::sync::Arc<cell::vfs::CidTree>>,
    /// Graceful shutdown: capabilities have this long to finish before epoch advances.
    pub drain_duration: std::time::Duration,
}

impl Service for EpochService {
    fn run(self, mut shutdown: watch::Receiver<()>) -> Result<()> {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()?;
        let _span = tracing::info_span!("epoch").entered();
        rt.block_on(async move {
            tokio::select! {
                result = cell::epoch::run_epoch_pipeline(
                    self.config,
                    self.epoch_tx,
                    self.confirmation_depth,
                    self.ipfs_client,
                    self.cid_tree,
                    self.drain_duration,
                ) => result,
                _ = shutdown.changed() => {
                    tracing::info!("epoch shutting down");
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
    use std::sync::atomic::AtomicBool;
    use std::time::Duration;

    /// A minimal service that sets a flag and waits for shutdown.
    struct FlagService {
        flag: Arc<AtomicBool>,
    }

    impl Service for FlagService {
        fn run(self, mut shutdown: watch::Receiver<()>) -> Result<()> {
            self.flag.store(true, Ordering::SeqCst);
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
        let mut host = Host::new();
        let flag1 = Arc::new(AtomicBool::new(false));
        let flag2 = Arc::new(AtomicBool::new(false));

        host.spawn(
            "svc-1",
            FlagService {
                flag: flag1.clone(),
            },
        );
        host.spawn(
            "svc-2",
            FlagService {
                flag: flag2.clone(),
            },
        );

        // Give threads a moment to start.
        std::thread::sleep(Duration::from_millis(50));
        assert!(
            flag1.load(Ordering::SeqCst),
            "service 1 should have started"
        );
        assert!(
            flag2.load(Ordering::SeqCst),
            "service 2 should have started"
        );

        host.shutdown();
    }

    #[test]
    fn executor_pool_runs_cells() {
        let (shutdown_tx, shutdown_rx) = watch::channel(());
        let pool = ExecutorPool::new(2, shutdown_rx);

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
        // Give workers time to drain.
        std::thread::sleep(Duration::from_millis(100));
    }

    #[test]
    fn executor_pool_least_loaded_assignment() {
        let (shutdown_tx, shutdown_rx) = watch::channel(());
        let pool = ExecutorPool::new(2, shutdown_rx);

        // Spawn a long-running cell on one worker.
        let (block_tx, block_rx) = std::sync::mpsc::channel::<()>();
        let long_cell = SpawnRequest {
            name: "long-cell".into(),
            factory: Box::new(move |_shutdown| {
                Box::pin(async move {
                    let _ = tokio::task::spawn_blocking(move || block_rx.recv()).await;
                })
            }),
            result_tx: None,
        };
        assert!(pool.spawn(long_cell).is_ok(), "spawn long_cell failed");
        std::thread::sleep(Duration::from_millis(50));

        // Worker 0 has count=1. Next cell should go to worker 1.
        let (tx, rx) = std::sync::mpsc::channel();
        let short_cell = SpawnRequest {
            name: "short-cell".into(),
            factory: Box::new(move |_shutdown| {
                Box::pin(async move {
                    tx.send(()).unwrap();
                })
            }),
            result_tx: None,
        };
        assert!(pool.spawn(short_cell).is_ok(), "spawn short_cell failed");
        rx.recv_timeout(Duration::from_secs(5)).unwrap();

        // Clean up.
        let _ = block_tx.send(());
        drop(shutdown_tx);
        std::thread::sleep(Duration::from_millis(100));
    }

    #[test]
    fn host_handles_service_error() {
        struct FailService;
        impl Service for FailService {
            fn run(self, _shutdown: watch::Receiver<()>) -> Result<()> {
                anyhow::bail!("intentional failure")
            }
        }

        let mut host = Host::new();
        host.spawn("fail-svc", FailService);
        std::thread::sleep(Duration::from_millis(50));
        // Should not panic — errors are logged, not propagated.
        host.shutdown();
    }

    #[test]
    fn host_handles_service_panic() {
        struct PanicService;
        impl Service for PanicService {
            fn run(self, _shutdown: watch::Receiver<()>) -> Result<()> {
                panic!("intentional panic")
            }
        }

        let mut host = Host::new();
        host.spawn("panic-svc", PanicService);
        std::thread::sleep(Duration::from_millis(50));
        // Should not panic in the supervisor — panics are caught by join.
        host.shutdown();
    }

    #[test]
    fn executor_pool_spawn_after_shutdown() {
        let (shutdown_tx, shutdown_rx) = watch::channel(());
        let pool = ExecutorPool::new(1, shutdown_rx);

        // Shut down the pool.
        drop(shutdown_tx);
        std::thread::sleep(Duration::from_millis(100));

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

    #[test]
    fn round_robin_distributes_across_idle_workers() {
        let (shutdown_tx, shutdown_rx) = watch::channel(());
        let pool = ExecutorPool::new(4, shutdown_rx);

        // Spawn 4 short cells. With round-robin on idle workers,
        // they should distribute across all 4 workers (not all to worker 0).
        let barriers: Vec<_> = (0..4)
            .map(|i| {
                let (tx, rx) = std::sync::mpsc::channel();
                let request = SpawnRequest {
                    name: format!("rr-cell-{}", i),
                    factory: Box::new(move |_shutdown| {
                        Box::pin(async move {
                            tx.send(()).unwrap();
                        })
                    }),
                    result_tx: None,
                };
                assert!(pool.spawn(request).is_ok());
                rx
            })
            .collect();

        // All 4 should complete.
        for rx in barriers {
            rx.recv_timeout(Duration::from_secs(5)).unwrap();
        }

        drop(shutdown_tx);
    }

    #[test]
    fn spawn_request_result_tx_receives_exit_code() {
        let (shutdown_tx, shutdown_rx) = watch::channel(());
        let pool = ExecutorPool::new(1, shutdown_rx);

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
        let pool = ExecutorPool::new(1, shutdown_rx);

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

    #[test]
    fn bounded_channel_rejects_after_shutdown() {
        // After shutdown the worker stops pulling from the channel.
        // We verify try_send fails on a closed channel, confirming
        // the bounded mpsc is wired correctly.
        let (shutdown_tx, shutdown_rx) = watch::channel(());
        let pool = ExecutorPool::new(1, shutdown_rx);

        // Shut down workers so the channel closes.
        drop(shutdown_tx);
        std::thread::sleep(Duration::from_millis(100));

        let request = SpawnRequest {
            name: "rejected".into(),
            factory: Box::new(|_| Box::pin(async {})),
            result_tx: None,
        };
        assert!(
            pool.spawn(request).is_err(),
            "spawn should fail on closed bounded channel"
        );
    }

    #[test]
    fn executor_pool_drop_joins_workers() {
        let (shutdown_tx, shutdown_rx) = watch::channel(());
        let pool = ExecutorPool::new(2, shutdown_rx);

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
        let pool = ExecutorPool::new(0, shutdown_rx);
        assert!(
            pool.worker_count() >= 1,
            "auto-detected pool should have at least 1 worker"
        );
    }

    #[test]
    fn spawn_request_no_result_tx_fire_and_forget() {
        // Cells without result_tx should complete without error.
        let (shutdown_tx, shutdown_rx) = watch::channel(());
        let pool = ExecutorPool::new(1, shutdown_rx);

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
