//! Cell-launching capability impls (`RuntimeImpl`, `ExecutorImpl`).
//!
//! Wires the rpc protocol layer to the cell execution layer. These capnp
//! `Server` impls build cells from RPC requests, so they sit at the
//! orchestration seam between `crate::rpc` (protocol) and `crate::cell`
//! (execution). Hosting them here lets `rpc` stay free of any `cell` dep.
#![cfg(not(target_arch = "wasm32"))]

use std::cell::RefCell;
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use capnp::capability::Promise;
use capnp_rpc::pry;
use tokio::io;
use tokio::sync::{mpsc, oneshot};

use ::authority::EpochGuard;

use crate::services::CompileRequest;
use crate::system_capnp;
use cell::proc::{FuelEstimator, FuelObserver};
use cell::{Builder, Proc, Program};
use rpc::{
    cleanup_channel, graft, ByteStreamImpl, CachePolicy, CleanupPublisher, ProcessBootstrapControl,
    ProcessImpl, StreamMode, TerminationHandle,
};

use rpc::managed_rpc::{ManagedRpc, DISCONNECT_GRACE};

static RPC_EOF_FALLBACKS: AtomicU64 = AtomicU64::new(0);

/// Number of child RPC disconnects that failed or exceeded their grace.
///
/// This counter makes the one-second EOF grace path observable in tests and
/// diagnostics without changing process completion semantics.
#[doc(hidden)]
pub fn rpc_eof_fallback_count() -> u64 {
    RPC_EOF_FALLBACKS.load(Ordering::Relaxed)
}

// =========================================================================
// RuntimeImpl — system-wide WASM compilation + execution runtime
// =========================================================================

/// The Runtime capability: compiles WASM and returns attenuated Executors.
///
/// **OCAP discipline**: Runtime is the powerful capability (can load any binary).
/// Executor is the attenuated capability (bound to one binary, can only spawn
/// instances). Compilation and executor caching remain implementation details
/// behind Runtime; Executors carry no Runtime or ambient host authority.
pub struct RuntimeImpl {
    wasm_debug: bool,
    guard: EpochGuard,
    /// Runtime-wide cache policy (from `WW_RUNTIME_CACHE_POLICY` env var).
    cache_policy: CachePolicy,
    /// BLAKE3(wasm bytes) → cached Executor client (used when policy = Shared).
    ///
    /// RefCell is correct because Cap'n Proto server dispatch runs on a
    /// single-threaded LocalSet.
    executor_cache: RefCell<HashMap<[u8; 32], system_capnp::executor::Client>>,
    /// Shared Wasmtime engine for this runtime and all executors it creates.
    runtime_engine: cell::engine::RuntimeEngine,
    /// Optional compilation service channel.
    compile_tx: Option<mpsc::Sender<CompileRequest>>,
    /// Optional host-managed cache for the accepted known-CID read substrate.
    ///
    /// This is process substrate, not a grant: it permits reads only when the
    /// guest already knows a CID and exposes no enumeration, mutation, pin,
    /// publishing, routing, or network-dial API. Reads can still consume node
    /// network, disk, pin/cache budget, and eviction work.
    pinset_cache: Option<Arc<cache::PinsetCache>>,
    /// Optional integration-test observation of production fuel callbacks.
    fuel_observer: Option<FuelObserver>,
}

impl RuntimeImpl {
    fn check_epoch(&self) -> Result<(), capnp::Error> {
        self.guard.check()
    }

    /// Create a new ExecutorImpl bound to the given bytecode and wrap it as a client.
    fn make_executor(
        &self,
        bytecode: Arc<Vec<u8>>,
        component: Option<Arc<wasmtime::component::Component>>,
    ) -> system_capnp::executor::Client {
        capnp_rpc::new_client(ExecutorImpl {
            bytecode,
            component,
            runtime_engine: self.runtime_engine.clone(),
            wasm_debug: self.wasm_debug,
            guard: self.guard.clone(),
            pinset_cache: self.pinset_cache.clone(),
            fuel_observer: self.fuel_observer.clone(),
        })
    }
}

async fn compile_with_service(
    compile_tx: Option<mpsc::Sender<CompileRequest>>,
    engine: Arc<wasmtime::Engine>,
    bytecode: Arc<Vec<u8>>,
) -> Result<Option<Arc<wasmtime::component::Component>>, capnp::Error> {
    let Some(tx) = compile_tx else {
        return Ok(None);
    };

    let (result_tx, result_rx) = oneshot::channel();
    tx.send(CompileRequest {
        bytecode: (*bytecode).clone(),
        engine,
        result_tx,
    })
    .await
    .map_err(|_| capnp::Error::failed("compilation service unavailable".into()))?;

    let component = result_rx
        .await
        .map_err(|_| capnp::Error::failed("compilation worker dropped request".into()))?
        .map_err(|err| capnp::Error::failed(err.to_string()))?;

    Ok(Some(Arc::new(component)))
}

/// Create the image-loading Runtime capability.
///
/// This is the only way to construct a `runtime::Client` backed by a real RuntimeImpl.
/// The returned client owns compilation and executor-cache state only; host
/// services used by pid0's graft are passed separately at the pid0 call site.
pub fn create_runtime_client(
    wasm_debug: bool,
    guard: EpochGuard,
    runtime_engine: cell::engine::RuntimeEngine,
    compile_tx: Option<mpsc::Sender<CompileRequest>>,
    cache_policy: CachePolicy,
) -> system_capnp::runtime::Client {
    create_runtime_client_with_options(
        wasm_debug,
        guard,
        runtime_engine,
        compile_tx,
        cache_policy,
        None,
        None,
    )
}

/// Create a Runtime whose Executors receive the fixed known-CID read
/// substrate through a host-managed shared pinset cache.
///
/// Ordinary tests and embedded callers can continue using
/// [`create_runtime_client`] for a cache-free substrate.
pub fn create_runtime_client_with_pinset(
    wasm_debug: bool,
    guard: EpochGuard,
    runtime_engine: cell::engine::RuntimeEngine,
    compile_tx: Option<mpsc::Sender<CompileRequest>>,
    cache_policy: CachePolicy,
    pinset_cache: Option<Arc<cache::PinsetCache>>,
) -> system_capnp::runtime::Client {
    create_runtime_client_with_options(
        wasm_debug,
        guard,
        runtime_engine,
        compile_tx,
        cache_policy,
        pinset_cache,
        None,
    )
}

/// Create a Runtime with opt-in observation of production fuel callbacks.
#[doc(hidden)]
pub fn create_runtime_client_with_fuel_observer(
    wasm_debug: bool,
    guard: EpochGuard,
    runtime_engine: cell::engine::RuntimeEngine,
    compile_tx: Option<mpsc::Sender<CompileRequest>>,
    cache_policy: CachePolicy,
    fuel_observer: FuelObserver,
) -> system_capnp::runtime::Client {
    create_runtime_client_with_options(
        wasm_debug,
        guard,
        runtime_engine,
        compile_tx,
        cache_policy,
        None,
        Some(fuel_observer),
    )
}

fn create_runtime_client_with_options(
    wasm_debug: bool,
    guard: EpochGuard,
    runtime_engine: cell::engine::RuntimeEngine,
    compile_tx: Option<mpsc::Sender<CompileRequest>>,
    cache_policy: CachePolicy,
    pinset_cache: Option<Arc<cache::PinsetCache>>,
    fuel_observer: Option<FuelObserver>,
) -> system_capnp::runtime::Client {
    let runtime = RuntimeImpl {
        wasm_debug,
        guard,
        cache_policy,
        executor_cache: RefCell::new(HashMap::new()),
        runtime_engine,
        compile_tx,
        pinset_cache,
        fuel_observer,
    };
    capnp_rpc::new_client(runtime)
}

fn read_text_list(list: capnp::text_list::Reader<'_>) -> Vec<String> {
    let mut out = Vec::with_capacity(list.len() as usize);
    for idx in 0..list.len() {
        if let Ok(text) = list.get(idx) {
            if let Ok(text) = text.to_str() {
                out.push(text.to_string());
            }
        }
    }
    out
}

fn read_text_list_result(list: capnp::Result<capnp::text_list::Reader<'_>>) -> Vec<String> {
    match list {
        Ok(reader) => read_text_list(reader),
        Err(_) => Vec::new(),
    }
}

fn read_data_result(data: capnp::Result<capnp::data::Reader<'_>>) -> Vec<u8> {
    match data {
        Ok(reader) => reader.to_vec(),
        Err(_) => Vec::new(),
    }
}

#[allow(refining_impl_trait)]
impl system_capnp::runtime::Server for RuntimeImpl {
    fn load(
        self: capnp::capability::Rc<Self>,
        params: system_capnp::runtime::LoadParams,
        mut results: system_capnp::runtime::LoadResults,
    ) -> Promise<(), capnp::Error> {
        pry!(self.check_epoch());
        let wasm = read_data_result(pry!(params.get()).get_wasm());

        if wasm.len() > cell::sched::MAX_COMPONENT_BYTES {
            return Promise::err(capnp::Error::failed(format!(
                "WASM binary too large ({} bytes, max {})",
                wasm.len(),
                cell::sched::MAX_COMPONENT_BYTES
            )));
        }

        let key = *blake3::hash(&wasm).as_bytes();
        let bytecode = Arc::new(wasm);
        let compile_tx = self.compile_tx.clone();
        let engine = self.runtime_engine.engine();
        let server = self.clone();

        Promise::from_future(async move {
            let executor = match server.cache_policy {
                CachePolicy::Shared => {
                    let cached = server.executor_cache.borrow().get(&key).cloned();
                    if let Some(client) = cached {
                        tracing::debug!(?key, "runtime.load: executor cache hit (shared)");
                        client
                    } else {
                        tracing::debug!(?key, "runtime.load: executor cache miss, creating");
                        let component = compile_with_service(
                            compile_tx.clone(),
                            engine.clone(),
                            bytecode.clone(),
                        )
                        .await?;
                        let client = server.make_executor(bytecode.clone(), component);
                        server
                            .executor_cache
                            .borrow_mut()
                            .insert(key, client.clone());
                        client
                    }
                }
                CachePolicy::Isolated => {
                    tracing::debug!(?key, "runtime.load: creating isolated executor");
                    let component =
                        compile_with_service(compile_tx, engine, bytecode.clone()).await?;
                    server.make_executor(bytecode, component)
                }
            };

            results.get().set_executor(executor);
            Ok(())
        })
    }

    fn shutdown(
        self: capnp::capability::Rc<Self>,
        _params: system_capnp::runtime::ShutdownParams,
        _results: system_capnp::runtime::ShutdownResults,
    ) -> Promise<(), capnp::Error> {
        pry!(self.check_epoch());
        tracing::info!("runtime.shutdown: stub (tokio-runtime-per-Runtime is a future PR)");
        Promise::ok(())
    }
}

// =========================================================================
// ExecutorImpl — attenuated capability bound to one WASM binary
// =========================================================================

/// An Executor bound to a specific WASM binary. Each
/// `spawn(args, env, membrane)` creates a fresh WASI process from the stored
/// bytecode with the given arguments, environment, and Membrane.
///
/// This is the attenuated capability in the OCAP model: the holder can spawn
/// workers but cannot load arbitrary code. Args and env are late-bound per-spawn,
/// which solves the WAGI CGI env var problem (per-request env vars like
/// REQUEST_METHOD, PATH_INFO, etc.).
pub struct ExecutorImpl {
    bytecode: Arc<Vec<u8>>,
    component: Option<Arc<wasmtime::component::Component>>,
    runtime_engine: cell::engine::RuntimeEngine,
    wasm_debug: bool,
    guard: EpochGuard,
    pinset_cache: Option<Arc<cache::PinsetCache>>,
    fuel_observer: Option<FuelObserver>,
}

/// Owns every resource whose lifetime is exactly one running child.
///
/// The task running this value owns the delegated authority through child exit.
struct OwnedChildLifecycle {
    proc: Option<Proc>,
    rpc_task: Option<ManagedRpc>,
    stderr_task: Option<tokio::task::JoinHandle<()>>,
    membrane: Option<system_capnp::membrane::Client>,
    bootstrap_control: ProcessBootstrapControl,
    kill_rx: tokio::sync::watch::Receiver<bool>,
    cleanup: Option<CleanupPublisher>,
}

impl OwnedChildLifecycle {
    fn run(self) -> impl std::future::Future<Output = ()> {
        let shutdown = rpc::local_tasks::shutdown_token();
        rpc::local_tasks::track(self.run_until_shutdown(shutdown))
    }

    async fn run_until_shutdown(mut self, shutdown: tokio_util::sync::CancellationToken) {
        let proc = self
            .proc
            .take()
            .expect("owned child lifecycle starts with a process");
        let mut proc_run = Box::pin(proc.run());
        let exit_code = loop {
            // A request may predate this owner's first poll.
            if *self.kill_rx.borrow() {
                tracing::info!("executor: child process killed");
                break 137;
            }
            tokio::select! {
                _ = shutdown.cancelled() => break 137,
                result = &mut proc_run => {
                    break match result {
                        Ok(()) => 0,
                        Err(error) => {
                            tracing::error!("executor: child process failed: {error}");
                            1
                        }
                    };
                }
                changed = self.kill_rx.changed() => {
                    if changed.is_err() {
                        // No termination controller remains to own execution.
                        break 137;
                    }
                }
            }
        };

        // Drop the Store and its guest transport before stopping the host RPC
        // system. This closes the peer side of the connection and cancels all
        // guest-owned RPC questions.
        drop(proc_run);
        self.bootstrap_control.clear();
        let rpc_terminalization_completed = self.stop_auxiliary_tasks().await;
        // Release the delegated Membrane after child RPC and the stored guest
        // bootstrap are gone but before reporting exit to the parent.
        drop(self.membrane.take());
        tracing::info!("executor: child process exited with code {exit_code}");
        if rpc_terminalization_completed {
            if let Some(cleanup) = self.cleanup.take() {
                cleanup.cleaned(exit_code);
            }
        }
        // A worker lost before managed disconnect returned cannot certify
        // teardown. Publisher drop preserves Lost rather than fabricating exit.
    }

    fn cancel_auxiliary_tasks(&mut self) {
        if let Some(task) = self.rpc_task.take() {
            task.shutdown();
        }
        if let Some(task) = self.stderr_task.take() {
            task.abort();
        }
    }

    async fn stop_auxiliary_tasks(&mut self) -> bool {
        let mut rpc_terminalization_completed = true;
        // Keep handles in the owner across every await so cancellation still
        // reaches them through OwnedChildLifecycle::drop.
        if let Some(task) = &self.stderr_task {
            task.abort();
        }
        if let Some(task) = self.rpc_task.as_mut() {
            // Store destruction has closed the guest transport. Explicitly
            // release connection exports, then join the existing worker within
            // its one-second grace before publishing backend cleanup.
            let result = task.shutdown_and_join().await;
            // Both outer Ok results prove the worker reached its terminal
            // return point. Inner errors still count as RPC failures; only a
            // JoinError prevents the owner from certifying cleanup.
            rpc_terminalization_completed = result.is_ok();
            if !matches!(result, Ok(Ok(()))) {
                RPC_EOF_FALLBACKS.fetch_add(1, Ordering::Relaxed);
                tracing::warn!(
                    grace_ms = DISCONNECT_GRACE.as_millis() as u64,
                    result = ?result,
                    "executor: child RPC disconnect failed or exceeded grace"
                );
            }
        }
        self.rpc_task.take();
        if let Some(task) = self.stderr_task.as_mut() {
            let _ = task.await;
        }
        self.stderr_task.take();
        rpc_terminalization_completed
    }
}

impl Drop for OwnedChildLifecycle {
    fn drop(&mut self) {
        self.bootstrap_control.clear();
        drop(self.proc.take());
        self.cancel_auxiliary_tasks();
        drop(self.membrane.take());
        // Cancellation/unwind can request release but cannot prove async task
        // teardown. Publisher loss records Lost, never a synthetic exit code.
        drop(self.cleanup.take());
    }
}

/// Armed only until the Process capability is installed in the spawn result.
///
/// If the RPC call is cancelled or errors during that handoff, aborting the
/// owner task drops the process, streams, and Membrane, and requests managed
/// child RPC disconnection.
struct SpawnHandoffGuard {
    abort_handle: Option<tokio::task::AbortHandle>,
    terminate: TerminationHandle,
}

impl SpawnHandoffGuard {
    fn new(abort_handle: tokio::task::AbortHandle, terminate: TerminationHandle) -> Self {
        Self {
            abort_handle: Some(abort_handle),
            terminate,
        }
    }

    fn complete(mut self) {
        self.abort_handle.take();
    }
}

impl Drop for SpawnHandoffGuard {
    fn drop(&mut self) {
        if let Some(abort_handle) = self.abort_handle.take() {
            self.terminate.request();
            abort_handle.abort();
        }
    }
}

#[allow(refining_impl_trait)]
impl system_capnp::executor::Server for ExecutorImpl {
    fn spawn(
        self: capnp::capability::Rc<Self>,
        params: system_capnp::executor::SpawnParams,
        mut results: system_capnp::executor::SpawnResults,
    ) -> Promise<(), capnp::Error> {
        pry!(self.guard.check());

        let params = pry!(params.get());
        let args = read_text_list_result(params.get_args());
        let env = read_text_list_result(params.get_env());

        // Read fuel policy (defaults to Scheduled if not provided).
        // Construct the appropriate FuelEstimator based on the policy variant.
        let fuel_estimator = if params.has_fuel_policy() {
            match pry!(pry!(params.get_fuel_policy()).which()) {
                system_capnp::fuel_policy::Scheduled(()) => None,
                system_capnp::fuel_policy::Oneshot(Ok(oneshot)) => {
                    let total_budget = oneshot.get_total_budget();
                    let max_per_epoch = oneshot.get_max_per_epoch();
                    let min_per_epoch = oneshot.get_min_per_epoch();
                    Some(FuelEstimator::new_oneshot(
                        total_budget,
                        max_per_epoch,
                        min_per_epoch,
                    ))
                }
                system_capnp::fuel_policy::Oneshot(Err(e)) => {
                    return Promise::err(capnp::Error::failed(format!(
                        "invalid oneshot fuel policy: {e}"
                    )));
                }
            }
        } else {
            None // Default: scheduled (unlimited)
        };

        // Retain the exact Membrane supplied by the parent. The host forwards
        // this object unchanged as the child's bootstrap capability.
        if !params.has_membrane() {
            return Promise::err(capnp::Error::failed(
                "executor.spawn: membrane is required".into(),
            ));
        }
        let child_membrane = pry!(params.get_membrane());

        let bytecode = self.bytecode.clone();
        let component = self.component.clone();
        let runtime_engine = self.runtime_engine.clone();
        let wasm_debug = self.wasm_debug;
        let pinset_cache = self.pinset_cache.clone();
        let fuel_observer = self.fuel_observer.clone();

        Promise::from_future(async move {
            let (host_stderr, guest_stderr) = io::duplex(64 * 1024);
            let (host_stdin, guest_stdin) = io::duplex(64 * 1024);
            let (host_stdout, guest_stdout) = io::duplex(64 * 1024);

            let (cleanup, observer) = cleanup_channel();
            let (kill_tx, kill_rx) = tokio::sync::watch::channel(false);
            let terminate = TerminationHandle::new(kill_tx);
            // All cells get data_streams + membrane RPC.
            // stdin/stdout semantics vary by cell type (wire protocol, CGI,
            // or shutdown signal), but the WIT membrane channel is universal.
            let program = match component {
                Some(component) => Program::Precompiled(component),
                None => Program::Bytes((*bytecode).clone()),
            };
            let (mut builder, mut handles) =
                Builder::ordinary(program, guest_stdin, guest_stdout, guest_stderr);
            builder = builder
                .with_runtime_engine(runtime_engine)
                .with_env(env)
                .with_args(args)
                .with_wasm_debug(wasm_debug);
            if let Some(est) = fuel_estimator {
                builder = builder.with_fuel_estimator(est);
            }
            if let Some(observer) = fuel_observer {
                builder = builder.with_fuel_observer(observer);
            }
            if let Some(pinset_cache) = pinset_cache {
                builder = builder.with_cache(cache::CacheMode::Shared(pinset_cache));
            }

            let proc = builder
                .build()
                .await
                .map_err(|err| capnp::Error::failed(err.to_string()))?;

            let (reader, writer) = handles
                .take_host_split()
                .ok_or_else(|| capnp::Error::failed("host stream missing".into()))?;

            let (rpc_task, guest_bootstrap) =
                graft::build_child_membrane_rpc(reader, writer, child_membrane.clone());

            let stderr_task = tokio::task::spawn_local(async move {
                use tokio::io::AsyncBufReadExt;
                let reader = tokio::io::BufReader::new(host_stderr);
                let mut lines = reader.lines();
                while let Ok(Some(line)) = lines.next_line().await {
                    tracing::info!("{}", line);
                }
            });

            let stdin =
                capnp_rpc::new_client(ByteStreamImpl::new(host_stdin, StreamMode::WriteOnly));
            let stdout =
                capnp_rpc::new_client(ByteStreamImpl::new(host_stdout, StreamMode::ReadOnly));
            let (dummy_stderr, _) = io::duplex(1);
            let stderr =
                capnp_rpc::new_client(ByteStreamImpl::new(dummy_stderr, StreamMode::ReadOnly));
            let (process_impl, bootstrap_control) = ProcessImpl::with_controlled_bootstrap(
                stdin,
                stdout,
                stderr,
                observer,
                guest_bootstrap,
                terminate.clone(),
            );

            let lifecycle_task = tokio::task::spawn_local(
                OwnedChildLifecycle {
                    proc: Some(proc),
                    rpc_task: Some(rpc_task),
                    stderr_task: Some(stderr_task),
                    membrane: Some(child_membrane),
                    bootstrap_control,
                    kill_rx,
                    cleanup: Some(cleanup),
                }
                .run(),
            );
            let handoff = SpawnHandoffGuard::new(lifecycle_task.abort_handle(), terminate);

            // Make the post-instantiation cancellation boundary explicit. If
            // the caller drops the spawn promise here, `handoff` aborts the
            // complete owned lifecycle before any Process becomes visible.
            tokio::task::yield_now().await;

            // Expose the parent-held process authority only after the entire
            // owned child lifecycle exists.
            let process_client: system_capnp::process::Client = capnp_rpc::new_client(process_impl);
            results.get().set_process(process_client);
            handoff.complete();

            // Detach only the single lifecycle owner. It remains responsible
            // for aborting its subordinate RPC/stderr tasks on every exit path.
            drop(lifecycle_task);

            Ok(())
        })
    }

    fn cid(
        self: capnp::capability::Rc<Self>,
        _params: system_capnp::executor::CidParams,
        mut results: system_capnp::executor::CidResults,
    ) -> Promise<(), capnp::Error> {
        pry!(self.guard.check());
        let cid = cell::routing_key::derive(&self.bytecode);
        results.get().set_cid(cid.to_string());
        Promise::ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::FutureExt;
    use std::cell::{Cell, RefCell};
    use std::rc::{Rc, Weak};
    use std::time::Duration;

    fn live_guard(
        seq: u64,
    ) -> (
        tokio::sync::watch::Sender<authority::Epoch>,
        authority::EpochGuard,
    ) {
        let (sender, receiver) = tokio::sync::watch::channel(authority::Epoch {
            seq,
            head: Vec::new(),
            root: None,
        });
        (
            sender,
            authority::EpochGuard {
                issued_seq: seq,
                receiver,
            },
        )
    }

    struct DropFlag {
        dropped: Rc<Cell<u32>>,
    }

    impl Drop for DropFlag {
        fn drop(&mut self) {
            self.dropped.set(self.dropped.get() + 1);
        }
    }

    struct TrackedMembrane {
        _drop_flag: DropFlag,
    }

    impl system_capnp::membrane::Server for TrackedMembrane {}

    struct ReentrantBootstrap {
        control: Weak<RefCell<Option<ProcessBootstrapControl>>>,
        dropped: Rc<Cell<bool>>,
    }

    impl system_capnp::membrane::Server for ReentrantBootstrap {}

    impl Drop for ReentrantBootstrap {
        fn drop(&mut self) {
            self.control
                .upgrade()
                .expect("bootstrap control remains available")
                .borrow()
                .as_ref()
                .expect("bootstrap control installed")
                .clear();
            self.dropped.set(true);
        }
    }

    // A task-set gate for lifecycle ordering tests. The real managed owner
    // still runs Cap'n Proto disconnect; only transport completion is gated.
    struct GatedNetwork(Option<std::pin::Pin<Box<dyn std::future::Future<Output = ()>>>>);
    impl capnp_rpc::VatNetwork<capnp_rpc::rpc_twoparty_capnp::Side> for GatedNetwork {
        fn connect(
            &mut self,
            _: capnp_rpc::rpc_twoparty_capnp::Side,
        ) -> Option<Box<dyn capnp_rpc::Connection<capnp_rpc::rpc_twoparty_capnp::Side>>> {
            None
        }
        fn accept(
            &mut self,
        ) -> Promise<
            Box<dyn capnp_rpc::Connection<capnp_rpc::rpc_twoparty_capnp::Side>>,
            capnp::Error,
        > {
            Promise::from_future(std::future::pending())
        }
        fn drive_until_shutdown(&mut self) -> Promise<(), capnp::Error> {
            let complete = self.0.take().unwrap();
            Promise::from_future(async move {
                complete.await;
                Ok(())
            })
        }
    }

    fn gated_rpc_task(complete: impl std::future::Future<Output = ()> + 'static) -> ManagedRpc {
        ManagedRpc::spawn(rpc::managed_rpc::ManagedRpcSystem::new(
            Box::new(GatedNetwork(Some(Box::pin(complete)))),
            None,
        ))
    }

    fn lifecycle_loss_fixture() -> (
        OwnedChildLifecycle,
        system_capnp::process::Client,
        Rc<Cell<u32>>,
        [oneshot::Receiver<()>; 2],
        rpc::CleanupObserver,
    ) {
        let dropped = Rc::new(Cell::new(0));
        let tracked_cap = || -> system_capnp::membrane::Client {
            capnp_rpc::new_client(TrackedMembrane {
                _drop_flag: DropFlag {
                    dropped: dropped.clone(),
                },
            })
        };
        let (rpc_dropped_tx, rpc_dropped_rx) = oneshot::channel();
        let (stderr_dropped_tx, stderr_dropped_rx) = oneshot::channel();
        let rpc_task = gated_rpc_task(async move {
            let _on_drop = rpc_dropped_tx;
            std::future::pending::<()>().await;
        });
        let stderr_task = tokio::task::spawn_local(async move {
            let _on_drop = stderr_dropped_tx;
            std::future::pending::<()>().await;
        });
        let (stream, _) = io::duplex(1);
        let stream: system_capnp::byte_stream::Client =
            capnp_rpc::new_client(ByteStreamImpl::new(stream, StreamMode::ReadOnly));
        let (cleanup, observer) = cleanup_channel();
        let (kill_tx, kill_rx) = tokio::sync::watch::channel(false);
        let (process, bootstrap_control) = ProcessImpl::with_controlled_bootstrap(
            stream.clone(),
            stream.clone(),
            stream,
            observer.clone(),
            tracked_cap().client,
            TerminationHandle::new(kill_tx),
        );
        (
            OwnedChildLifecycle {
                // These tests exercise loss of the owner before child polling,
                // without allocating a Wasmtime Store.
                proc: None,
                rpc_task: Some(rpc_task),
                stderr_task: Some(stderr_task),
                membrane: Some(tracked_cap()),
                bootstrap_control,
                kill_rx,
                cleanup: Some(cleanup),
            },
            capnp_rpc::new_client(process),
            dropped,
            [rpc_dropped_rx, stderr_dropped_rx],
            observer,
        )
    }

    #[test]
    fn owned_child_lifecycle_drop_releases_reentrant_bootstrap() {
        let control_slot = Rc::new(RefCell::new(None));
        let dropped = Rc::new(Cell::new(false));
        let bootstrap: system_capnp::membrane::Client = capnp_rpc::new_client(ReentrantBootstrap {
            control: Rc::downgrade(&control_slot),
            dropped: dropped.clone(),
        });
        let (stream, _) = io::duplex(1);
        let stream: system_capnp::byte_stream::Client =
            capnp_rpc::new_client(ByteStreamImpl::new(stream, StreamMode::ReadOnly));
        let (_cleanup, observer) = cleanup_channel();
        let (kill_tx, kill_rx) = tokio::sync::watch::channel(false);
        let (process, bootstrap_control) = ProcessImpl::with_controlled_bootstrap(
            stream.clone(),
            stream.clone(),
            stream,
            observer,
            bootstrap.client,
            TerminationHandle::new(kill_tx),
        );
        *control_slot.borrow_mut() = Some(bootstrap_control.clone());
        drop(process);

        drop(OwnedChildLifecycle {
            proc: None,
            rpc_task: None,
            stderr_task: None,
            membrane: None,
            bootstrap_control,
            kill_rx,
            cleanup: None,
        });

        assert!(
            dropped.get(),
            "bootstrap ownership released during teardown"
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn owned_child_lifecycle_cancellation_clears_bootstrap_and_reports_loss() {
        tokio::task::LocalSet::new()
            .run_until(async {
                let (lifecycle, process, dropped, tasks_dropped, observer) =
                    lifecycle_loss_fixture();
                let owner = tokio::task::spawn_local(lifecycle.run());
                owner.abort();
                assert!(owner.await.unwrap_err().is_cancelled());
                for task_dropped in tasks_dropped {
                    assert!(
                        task_dropped.await.is_err(),
                        "auxiliary task released its resources"
                    );
                }
                assert!(
                    process.bootstrap_request().send().promise.await.is_err(),
                    "owner cancellation clears the retained guest bootstrap"
                );
                assert_eq!(
                    observer.state(),
                    rpc::CleanupState::Lost(rpc::LostReason::OwnerDropped)
                );
                assert_eq!(
                    dropped.get(),
                    2,
                    "bootstrap and delegated Membrane released"
                );
                assert!(
                    process.wait_request().send().promise.await.is_err(),
                    "owner cancellation is not completed teardown"
                );
            })
            .await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn owned_child_lifecycle_panic_reports_loss() {
        tokio::task::LocalSet::new()
            .run_until(async {
                let (lifecycle, process, _, tasks_dropped, _) = lifecycle_loss_fixture();
                // The missing initial Proc deliberately panics at the owner's
                // first poll, exercising the production owner's unwind guard.
                let owner = tokio::task::spawn_local(lifecycle.run());
                assert!(owner.await.unwrap_err().is_panic());
                for task_dropped in tasks_dropped {
                    assert!(task_dropped.await.is_err());
                }
                assert!(process.wait_request().send().promise.await.is_err());
            })
            .await;
    }

    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn owned_child_lifecycle_cancel_during_auxiliary_teardown_releases_tasks() {
        tokio::task::LocalSet::new()
            .run_until(async {
                let (mut lifecycle, process, _, tasks_dropped, _) = lifecycle_loss_fixture();
                let mut teardown = Box::pin(lifecycle.stop_auxiliary_tasks());
                assert!(teardown.as_mut().now_or_never().is_none());
                drop(teardown);
                drop(lifecycle);
                for task_dropped in tasks_dropped {
                    assert!(
                        tokio::time::timeout(Duration::from_secs(2), task_dropped)
                            .await
                            .is_ok(),
                        "cancelling cleanup must abort every subordinate task"
                    );
                }
                assert!(process.wait_request().send().promise.await.is_err());
            })
            .await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn owned_child_lifecycle_publishes_cleaned_only_after_full_teardown() {
        use tokio::io::AsyncReadExt;
        tokio::task::LocalSet::new()
            .run_until(async {
                let (mut lifecycle, process, dropped, tasks_dropped, observer) =
                    lifecycle_loss_fixture();
                let wasm = wat::parse_str(
                    r#"
                    (component
                      (core func $return (canon task.return (result (result))))
                      (core module $noop
                        (import "" "return" (func $return (param i32)))
                        (func (export "run") i32.const 0 call $return))
                      (core instance $instance (instantiate $noop
                        (with "" (instance (export "return" (func $return))))))
                      (func $run async (result (result))
                        (canon lift (core func $instance "run") async))
                      (instance (export (interface "wasi:cli/run@0.3.0"))
                        (export "run" (func $run))))
                "#,
                )
                .expect("P3 no-op component");
                let (builder, mut handles) =
                    Builder::ordinary(Program::Bytes(wasm), io::empty(), io::sink(), io::sink());
                let (runtime_engine, _publisher) =
                    cell::engine::runtime_engine().expect("child runtime engine");
                lifecycle.proc = Some(
                    builder
                        .with_runtime_engine(runtime_engine)
                        .build()
                        .await
                        .expect("prepare child"),
                );
                let (mut reader, writer) = handles.take_host_split().expect("child RPC transport");
                let (store_dropped_tx, store_dropped_rx) = oneshot::channel();
                let (rpc_release_tx, rpc_release_rx) = oneshot::channel();
                drop(lifecycle.rpc_task.take());
                lifecycle.rpc_task = Some(gated_rpc_task(async move {
                    let _writer = writer;
                    assert_eq!(
                        reader.read(&mut [0]).await.unwrap(),
                        0,
                        "Store closed guest transport"
                    );
                    store_dropped_tx.send(()).unwrap();
                    rpc_release_rx
                        .await
                        .expect("release completed RPC teardown");
                }));
                let owner = tokio::task::spawn_local(lifecycle.run());
                store_dropped_rx.await.expect("child Store released");
                assert_eq!(
                    dropped.get(),
                    1,
                    "guest bootstrap released before RPC teardown"
                );
                assert_eq!(observer.state(), rpc::CleanupState::Running);
                let mut wait = Box::pin(process.wait_request().send().promise);
                assert!(
                    wait.as_mut().now_or_never().is_none(),
                    "RPC join still prevents cleanup acknowledgement"
                );
                rpc_release_tx.send(()).unwrap();
                owner.await.expect("child teardown owner");
                for task_dropped in tasks_dropped {
                    assert!(task_dropped.await.is_err());
                }
                assert_eq!(
                    dropped.get(),
                    2,
                    "delegated Membrane released before cleanup publication"
                );
                assert_eq!(wait.await.unwrap().get().unwrap().get_exit_code(), 0);
                assert_eq!(observer.clone().wait().await, Ok(0));
                drop(process);
                assert_eq!(observer.state(), rpc::CleanupState::Cleaned(0));
            })
            .await;
    }

    struct PanickingBootstrap;
    impl system_capnp::membrane::Server for PanickingBootstrap {}
    impl Drop for PanickingBootstrap {
        fn drop(&mut self) {
            panic!("injected RPC bootstrap destructor panic");
        }
    }

    async fn lifecycle_rpc_failure(worker_lost: bool) {
        use tokio::io::AsyncReadExt;

        tokio::task::LocalSet::new().run_until(async {
            let (mut lifecycle, process, dropped, tasks_dropped, observer) = lifecycle_loss_fixture();
            let wasm = wat::parse_str(r#"(component
                (core func $return (canon task.return (result (result))))
                (core module $noop
                    (import "" "return" (func $return (param i32)))
                    (func (export "run") i32.const 0 call $return))
                (core instance $i (instantiate $noop (with "" (instance (export "return" (func $return))))))
                (func $run async (result (result)) (canon lift (core func $i "run") async))
                (instance (export (interface "wasi:cli/run@0.3.0")) (export "run" (func $run))))"#).unwrap();
            let (builder, mut handles) = Builder::ordinary(Program::Bytes(wasm), io::empty(), io::sink(), io::sink());
            let (engine, _publisher) = cell::engine::runtime_engine().unwrap();
            lifecycle.proc = Some(builder.with_runtime_engine(engine).build().await.unwrap());
            let (mut reader, _writer) = handles.take_host_split().expect("child RPC transport");
            lifecycle.rpc_task = Some(if worker_lost {
                // Dropping a LocalSet cancels its actual worker. This is outer
                // JoinError loss, independent of contained capability panics.
                let local = tokio::task::LocalSet::new();
                let task = {
                    let _entered = local.enter();
                    gated_rpc_task(std::future::pending())
                };
                drop(local);
                task
            } else {
                let bootstrap: system_capnp::membrane::Client = capnp_rpc::new_client(PanickingBootstrap);
                ManagedRpc::spawn(rpc::managed_rpc::ManagedRpcSystem::new(
                    Box::new(GatedNetwork(Some(Box::pin(std::future::pending())))), Some(bootstrap.client),
                ))
            });
            tokio::time::timeout(Duration::from_secs(2), lifecycle.run()).await.expect("bounded lifecycle terminalization");
            assert_eq!(dropped.get(), 2, "bootstrap and Membrane released before publication");
            assert_eq!(reader.read(&mut [0]).await.unwrap(), 0, "Store released guest transport");
            for task_dropped in tasks_dropped {
                assert!(task_dropped.await.is_err());
            }
            if worker_lost {
                assert_eq!(observer.state(), rpc::CleanupState::Lost(rpc::LostReason::OwnerDropped));
                assert!(process.wait_request().send().promise.await.is_err());
            } else {
                assert_eq!(observer.state(), rpc::CleanupState::Cleaned(0));
                assert_eq!(process.wait_request().send().promise.await.unwrap().get().unwrap().get_exit_code(), 0);
                assert_eq!(observer.wait().await, Ok(0));
            }
        }).await;
    }

    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn owned_child_lifecycle_cleans_after_rpc_inner_error() {
        lifecycle_rpc_failure(false).await;
    }

    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn owned_child_lifecycle_does_not_claim_cleanup_after_rpc_worker_loss() {
        lifecycle_rpc_failure(true).await;
    }

    struct ProcessExports(Rc<RefCell<Option<system_capnp::process::Client>>>);
    impl system_capnp::membrane::Server for ProcessExports {
        fn graft(
            self: capnp::capability::Rc<Self>,
            _: system_capnp::membrane::GraftParams,
            mut results: system_capnp::membrane::GraftResults,
        ) -> impl std::future::Future<Output = capnp::Result<()>> + 'static {
            let process = self.0.borrow_mut().take().expect("one process handoff");
            let mut export = results.get().init_extras(1).get(0);
            export.set_name("process");
            export.init_cap().set_as_capability(process.client.hook);
            Promise::ok(())
        }
    }

    struct RemoteProcessOwner {
        server: ManagedRpc,
        _client: ManagedRpc,
        exports: Rc<RefCell<Option<system_capnp::process::Client>>>,
        remote: system_capnp::membrane::Client,
        supplied_membrane: system_capnp::membrane::Client,
    }

    impl RemoteProcessOwner {
        fn new() -> Self {
            use capnp_rpc::{rpc_twoparty_capnp::Side, twoparty::VatNetwork};
            use rpc::managed_rpc::ManagedRpcSystem as RpcSystem;
            use tokio_util::compat::{TokioAsyncReadCompatExt, TokioAsyncWriteCompatExt};
            let exports = Rc::new(RefCell::new(None));
            let bootstrap: system_capnp::membrane::Client =
                capnp_rpc::new_client(ProcessExports(exports.clone()));
            let peer_membrane: system_capnp::membrane::Client =
                capnp_rpc::new_client(TrackedMembrane {
                    _drop_flag: DropFlag {
                        dropped: Rc::new(Cell::new(0)),
                    },
                });
            let (a, b) = io::duplex(8192);
            let (ar, aw) = io::split(a);
            let (br, bw) = io::split(b);
            let mut server = RpcSystem::new(
                Box::new(VatNetwork::new(
                    ar.compat(),
                    aw.compat_write(),
                    Side::Server,
                    Default::default(),
                )),
                Some(bootstrap.client),
            );
            let supplied_membrane = server.bootstrap(Side::Client);
            let mut client = RpcSystem::new(
                Box::new(VatNetwork::new(
                    br.compat(),
                    bw.compat_write(),
                    Side::Client,
                    Default::default(),
                )),
                Some(peer_membrane.client),
            );
            let remote = client.bootstrap(Side::Server);
            Self {
                server: ManagedRpc::spawn(server),
                _client: ManagedRpc::spawn(client),
                exports,
                remote,
                supplied_membrane,
            }
        }

        async fn acquire(
            &self,
            process: system_capnp::process::Client,
        ) -> system_capnp::process::Client {
            *self.exports.borrow_mut() = Some(process);
            self.remote
                .graft_request()
                .send()
                .promise
                .await
                .unwrap()
                .get()
                .unwrap()
                .get_extras()
                .unwrap()
                .get(0)
                .get_cap()
                .get_as()
                .unwrap()
        }
    }

    struct AllowProcess;
    impl membrane::Policy for AllowProcess {
        fn check(&self, _: u64, _: u16) -> capnp::Result<()> {
            Ok(())
        }
    }

    async fn remote_cleanup_case(two_owners: bool) {
        tokio::task::LocalSet::new()
            .run_until(async move {
                let mut b = RemoteProcessOwner::new();
                let mut c = two_owners.then(RemoteProcessOwner::new);
                let wasm = wat::parse_str(include_str!("../tests/fixtures/spinning-component.wat"))
                    .unwrap();
                let (runtime_engine, _publisher) = cell::engine::runtime_engine().unwrap();
                let fuel = FuelObserver::default();
                let (builder, mut handles) =
                    Builder::ordinary(Program::Bytes(wasm), io::empty(), io::sink(), io::sink());
                let proc = builder
                    .with_runtime_engine(runtime_engine)
                    .with_fuel_estimator(FuelEstimator::new_oneshot(100_000, 10_000, 0))
                    .with_fuel_observer(fuel.clone())
                    .build()
                    .await
                    .unwrap();
                let (reader, writer) = handles.take_host_split().unwrap();
                let (rpc_task, guest_bootstrap) =
                    graft::build_child_membrane_rpc(reader, writer, b.supplied_membrane.clone());
                let (cleanup, observer) = cleanup_channel();
                let (kill_tx, kill_rx) = tokio::sync::watch::channel(false);
                let (stream, _) = io::duplex(16);
                let stream: system_capnp::byte_stream::Client =
                    capnp_rpc::new_client(ByteStreamImpl::new(stream, StreamMode::ReadOnly));
                let (process, bootstrap_control) = ProcessImpl::with_controlled_bootstrap(
                    stream.clone(),
                    stream.clone(),
                    stream,
                    observer.clone(),
                    guest_bootstrap,
                    TerminationHandle::new(kill_tx),
                );
                let process: system_capnp::process::Client = capnp_rpc::new_client(process);
                let process = membrane::membrane(process, Rc::new(AllowProcess));
                let remote_b = b.acquire(process.clone()).await;
                let remote_c = if let Some(c) = &c {
                    Some(c.acquire(process.clone()).await)
                } else {
                    None
                };
                drop(process);
                let termination = kill_rx.clone();
                let owner = tokio::task::spawn_local(
                    OwnedChildLifecycle {
                        proc: Some(proc),
                        rpc_task: Some(rpc_task),
                        stderr_task: None,
                        membrane: Some(b.supplied_membrane.clone()),
                        bootstrap_control,
                        kill_rx,
                        cleanup: Some(cleanup),
                    }
                    .run(),
                );
                // Run real guest instructions until a deterministic fuel gate parks
                // the Store. No clock tick or sleep is needed for test ordering.
                fuel.wait_for_suspension(1).await;
                assert_eq!(observer.state(), rpc::CleanupState::Running);
                let _ = b.server.shutdown_and_join().await;
                if let Some(remote_c) = remote_c {
                    assert!(
                        !*termination.borrow(),
                        "C retains execution after B disconnects"
                    );
                    assert_eq!(observer.state(), rpc::CleanupState::Running);
                    remote_c.stdout_request().send().promise.await.unwrap();
                    drop(remote_c); // final Release, without disconnecting C
                }
                assert_eq!(
                    tokio::time::timeout(Duration::from_secs(2), observer.wait())
                        .await
                        .unwrap(),
                    Ok(137)
                );
                owner.await.unwrap();
                assert!(remote_b.wait_request().send().promise.await.is_err());
                assert_eq!(
                    observer.clone().wait().await,
                    Ok(137),
                    "RPC loss does not consume backend truth"
                );
                if let Some(c) = c.as_mut() {
                    let _ = c.server.shutdown_and_join().await;
                }
            })
            .await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn remote_process_disconnect_cuts_transport_cycle_and_cleans_backend() {
        remote_cleanup_case(false).await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn remote_process_multiple_owners_retain_backend_until_final_release() {
        remote_cleanup_case(true).await;
    }

    #[test]
    fn executor_pool_shutdown_joins_child_lifecycle_and_rpc() {
        for close_spawn_channel in [false, true] {
            let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(());
            let pool = crate::services::ExecutorPool::new(1, shutdown_rx);
            let engine = pool.runtime_engine();
            let (ready_tx, ready_rx) = std::sync::mpsc::channel();
            assert!(pool
                .spawn(crate::services::SpawnRequest {
                    name: "shutdown-child".into(),
                    factory: Box::new(move |_| Box::pin(async move {
                        let (mut lifecycle, process, _, _, observer) = lifecycle_loss_fixture();
                        let wasm = wat::parse_str(include_str!(
                            "../tests/fixtures/spinning-component.wat"
                        ))
                        .unwrap();
                        let fuel = FuelObserver::default();
                        let (builder, mut handles) = Builder::ordinary(
                            Program::Bytes(wasm),
                            io::empty(),
                            io::sink(),
                            io::sink(),
                        );
                        lifecycle.proc = Some(
                            builder
                                .with_runtime_engine(engine)
                                .with_fuel_estimator(FuelEstimator::new_oneshot(
                                    u64::MAX,
                                    10_000,
                                    0,
                                ))
                                .with_fuel_observer(fuel.clone())
                                .build()
                                .await
                                .unwrap(),
                        );
                        let (reader, writer) = handles.take_host_split().unwrap();
                        let (rpc, _) = graft::build_child_membrane_rpc(
                            reader,
                            writer,
                            lifecycle.membrane.as_ref().unwrap().clone(),
                        );
                        lifecycle.rpc_task = Some(rpc);
                        let owner = tokio::task::spawn_local(lifecycle.run());
                        fuel.wait_for_suspension(1).await;
                        ready_tx.send(observer).unwrap();
                        let _retained = (process, owner);
                        std::future::pending::<()>().await;
                    })),
                    result_tx: None,
                })
                .is_ok());
            let observer = ready_rx.recv_timeout(Duration::from_secs(10)).unwrap();
            assert_eq!(observer.state(), rpc::CleanupState::Running);
            if !close_spawn_channel {
                drop(shutdown_tx);
            }
            drop(pool);
            assert_eq!(observer.state(), rpc::CleanupState::Cleaned(137),
                "worker must join the child teardown before destroying its LocalSet (channel close: {close_spawn_channel})");
        }
    }

    #[tokio::test(flavor = "current_thread")]
    async fn cancelled_spawn_handoff_aborts_all_owned_child_resources() {
        let local = tokio::task::LocalSet::new();
        local
            .run_until(async {
                let dropped = Rc::new(Cell::new(0));
                let resources: Vec<_> = (0..4)
                    .map(|_| DropFlag {
                        dropped: dropped.clone(),
                    })
                    .collect();

                // Model the production owner task after process construction:
                // process, RPC task, streams, and Membrane are all captured by
                // the one abort target guarded until Process handoff.
                let lifecycle_task = tokio::task::spawn_local(async move {
                    let _resources = resources;
                    std::future::pending::<()>().await;
                });
                let (kill_tx, mut kill_rx) = tokio::sync::watch::channel(false);
                let handoff = SpawnHandoffGuard::new(
                    lifecycle_task.abort_handle(),
                    TerminationHandle::new(kill_tx),
                );

                drop(handoff);
                assert!(
                    kill_rx.changed().await.is_ok() && *kill_rx.borrow(),
                    "cancellation must signal child termination"
                );
                let result = lifecycle_task.await;
                assert!(result.is_err() && result.unwrap_err().is_cancelled());
                assert_eq!(
                    dropped.get(),
                    4,
                    "process, RPC task, streams, and record must all be released"
                );
            })
            .await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn runtime_and_executor_become_stale_after_epoch_advance() {
        tokio::task::LocalSet::new()
            .run_until(async {
                let (epoch_tx, guard) = live_guard(1);
                let (runtime_engine, _publisher) =
                    cell::engine::runtime_engine().expect("test runtime engine");
                let runtime = create_runtime_client(
                    false,
                    guard,
                    runtime_engine,
                    None,
                    CachePolicy::Isolated,
                );

                let mut load = runtime.load_request();
                load.get().set_wasm(b"executor identity bytes");
                let executor = load
                    .send()
                    .promise
                    .await
                    .expect("fresh Runtime must load")
                    .get()
                    .expect("load results")
                    .get_executor()
                    .expect("Executor result");
                executor
                    .cid_request()
                    .send()
                    .promise
                    .await
                    .expect("fresh Executor must answer");

                epoch_tx.send_replace(authority::Epoch {
                    seq: 2,
                    head: Vec::new(),
                    root: None,
                });

                let mut stale_load = runtime.load_request();
                stale_load.get().set_wasm(b"new bytes");
                assert!(
                    stale_load.send().promise.await.is_err(),
                    "Runtime must reject calls after its epoch advances"
                );
                assert!(
                    executor.cid_request().send().promise.await.is_err(),
                    "Executor must reject calls after its epoch advances"
                );
            })
            .await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn fixed_epoch_zero_runtime_remains_valid() {
        tokio::task::LocalSet::new()
            .run_until(async {
                let (runtime_engine, _publisher) =
                    cell::engine::runtime_engine().expect("test runtime engine");
                let runtime = create_runtime_client(
                    false,
                    authority::EpochGuard::fixed(authority::Epoch::zero()),
                    runtime_engine,
                    None,
                    CachePolicy::Isolated,
                );
                for bytes in [b"first".as_slice(), b"second".as_slice()] {
                    let mut load = runtime.load_request();
                    load.get().set_wasm(bytes);
                    load.send()
                        .promise
                        .await
                        .expect("fixed epoch-zero Runtime must remain valid");
                }
            })
            .await;
    }
}
