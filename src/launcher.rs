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
use rpc::observability::EventCode;
use tokio::io;
use tokio::sync::{mpsc, oneshot};
use tracing::Instrument;

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
static NEXT_CHILD_CELL_ID: AtomicU64 = AtomicU64::new(1);

#[cfg(test)]
#[path = "launcher_tracing_tests.rs"]
mod tracing_tests;

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
    filesystem_resources: Arc<cache::HostResources>,
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
            filesystem_resources: Arc::clone(&self.filesystem_resources),
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
    filesystem_resources: Arc<cache::HostResources>,
) -> system_capnp::runtime::Client {
    create_runtime_client_with_options(
        wasm_debug,
        guard,
        runtime_engine,
        compile_tx,
        cache_policy,
        filesystem_resources,
        RuntimeClientOptions::default(),
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
    filesystem_resources: Arc<cache::HostResources>,
) -> system_capnp::runtime::Client {
    create_runtime_client_with_options(
        wasm_debug,
        guard,
        runtime_engine,
        compile_tx,
        cache_policy,
        filesystem_resources,
        RuntimeClientOptions {
            pinset_cache,
            ..RuntimeClientOptions::default()
        },
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
    filesystem_resources: Arc<cache::HostResources>,
) -> system_capnp::runtime::Client {
    create_runtime_client_with_options(
        wasm_debug,
        guard,
        runtime_engine,
        compile_tx,
        cache_policy,
        filesystem_resources,
        RuntimeClientOptions {
            fuel_observer: Some(fuel_observer),
            ..RuntimeClientOptions::default()
        },
    )
}

#[derive(Default)]
struct RuntimeClientOptions {
    pinset_cache: Option<Arc<cache::PinsetCache>>,
    fuel_observer: Option<FuelObserver>,
}

fn create_runtime_client_with_options(
    wasm_debug: bool,
    guard: EpochGuard,
    runtime_engine: cell::engine::RuntimeEngine,
    compile_tx: Option<mpsc::Sender<CompileRequest>>,
    cache_policy: CachePolicy,
    filesystem_resources: Arc<cache::HostResources>,
    options: RuntimeClientOptions,
) -> system_capnp::runtime::Client {
    let runtime = RuntimeImpl {
        wasm_debug,
        guard,
        cache_policy,
        executor_cache: RefCell::new(HashMap::new()),
        runtime_engine,
        compile_tx,
        pinset_cache: options.pinset_cache,
        fuel_observer: options.fuel_observer,
        filesystem_resources,
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
    filesystem_resources: Arc<cache::HostResources>,
}

/// Owns every resource whose lifetime is exactly one running child.
///
/// The task running this value owns the delegated authority through child exit.
struct OwnedChildLifecycle {
    proc: Option<Proc>,
    filesystem_completion: cache::FsCompletion,
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
                tracing::trace!(
                    event_code = EventCode::CellTerminationRequested as u16,
                    "executor: child process killed"
                );
                break 137;
            }
            tokio::select! {
                biased;
                _ = shutdown.cancelled() => break 137,
                changed = self.kill_rx.changed() => {
                    if changed.is_err() {
                        // No termination controller remains to own execution.
                        break 137;
                    }
                }
                result = &mut proc_run => {
                    break match result {
                        Ok(()) => 0,
                        Err(error) => {
                            tracing::error!(event_code = EventCode::CellFailed as u16, error = error.as_ref() as &dyn std::error::Error, "executor: child process failed");
                            1
                        }
                    };
                }
            }
        };

        // Drop the Store and its guest transport before stopping the host RPC
        // system. This closes the peer side of the connection and cancels all
        // guest-owned RPC questions.
        drop(proc_run);
        self.bootstrap_control.clear();
        let filesystem_completion = self.filesystem_completion.clone();
        // The tracing macro evaluates this bounded read only when enabled.
        // Guest exit can precede native completion; do not infer completion here.
        tracing::debug!(
            filesystem = ?filesystem_completion.try_snapshot(),
            "executor: child filesystem ownership after guest exit"
        );
        let (filesystem_result, rpc_terminalization_completed) =
            tokio::join!(filesystem_completion.wait(), self.stop_auxiliary_tasks());
        let filesystem_completed = match filesystem_result {
            Ok(()) => true,
            Err(error) => {
                tracing::error!(
                    event_code = EventCode::CellCleanupFailed as u16,
                    error = &error as &dyn std::error::Error,
                    "executor: child filesystem cleanup was not certified"
                );
                false
            }
        };
        // Release the delegated Membrane after child RPC and the stored guest
        // bootstrap are gone but before reporting exit to the parent.
        drop(self.membrane.take());
        tracing::trace!(
            event_code = EventCode::CellExited as u16,
            exit_code,
            "executor: child process exited"
        );
        if filesystem_completed && rpc_terminalization_completed {
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
                let error: &dyn std::error::Error = match &result {
                    Ok(Err(error)) => error,
                    Err(error) => error,
                    Ok(Ok(())) => unreachable!("failure branch"),
                };
                tracing::warn!(
                    event_code = EventCode::RpcCleanupFailed as u16,
                    timeout_ms = DISCONNECT_GRACE.as_millis() as u64,
                    error,
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
        let filesystem_resources = Arc::clone(&self.filesystem_resources);

        // A spawned child has its own lifetime and identity. Unlike the named
        // ExecutorPool owner (`cell_name`), each child gets a process-local ID.
        // The caller caused the spawn but does not own all later child work.
        let span = tracing::info_span!(
            parent: None,
            "cell",
            cell_id = NEXT_CHILD_CELL_ID.fetch_add(1, Ordering::Relaxed),
        );
        span.follows_from(tracing::Span::current());
        let child_span = span.clone();
        let future = async move {
            let cell_resources =
                cache::CellResources::new(filesystem_resources, cache::CellLimits::default());
            let filesystem = cell::FilesystemOwnership::try_new(cell_resources)
                .map_err(|error| capnp::Error::overloaded(error.to_string()))?;
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
                Builder::ordinary(program, guest_stdin, guest_stdout, guest_stderr, filesystem);
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

            let filesystem_completion = builder.filesystem_completion();
            let filesystem_finalizer = builder
                .take_filesystem_finalizer()
                .expect("new child builder owns one filesystem finalizer");
            tokio::task::spawn_local(
                rpc::local_tasks::track(async move {
                    if let Err(error) = filesystem_finalizer.run().await {
                        tracing::error!(
                            event_code = EventCode::CellCleanupFailed as u16,
                            error = &error as &dyn std::error::Error,
                            "executor: child filesystem finalization failed"
                        );
                    }
                })
                .instrument(child_span.clone()),
            );

            let proc = match builder.build().await {
                Ok(proc) => proc,
                Err(error) => {
                    filesystem_completion
                        .wait()
                        .await
                        .map_err(|cleanup_error| {
                            capnp::Error::failed(format!(
                                "{error}; process filesystem cleanup failed: {cleanup_error}"
                            ))
                        })?;
                    return Err(capnp::Error::failed(error.to_string()));
                }
            };

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
                    // Guest output is untrusted; ordinary logs never render its contents.
                    tracing::trace!(bytes = line.len(), "guest stderr line redacted");
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
                    filesystem_completion,
                    rpc_task: Some(rpc_task),
                    stderr_task: Some(stderr_task),
                    membrane: Some(child_membrane),
                    bootstrap_control,
                    kill_rx,
                    cleanup: Some(cleanup),
                }
                .run()
                .instrument(child_span),
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
        };
        Promise::from_future(future.instrument(span))
    }

    fn bytecode_cid(
        self: capnp::capability::Rc<Self>,
        _params: system_capnp::executor::BytecodeCidParams,
        mut results: system_capnp::executor::BytecodeCidResults,
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
    use std::collections::HashMap;
    use std::path::{Path, PathBuf};
    use std::rc::{Rc, Weak};
    use std::time::Duration;

    const NATIVE_FIXTURE_CID: &str = "QmYwAPJzv5CZsnN625s3Xf2nemtYgPpHdWEz79ojWnPbdG";

    fn test_filesystem_host() -> Arc<cache::HostResources> {
        crate::services::test_filesystem_resources()
    }

    fn test_filesystem_ownership() -> cell::FilesystemOwnership {
        let resources =
            cache::CellResources::new(test_filesystem_host(), cache::CellLimits::default());
        cell::FilesystemOwnership::try_new(resources).expect("test process filesystem ownership")
    }

    struct NativeFixturePinner {
        contents: HashMap<cid::Cid, Vec<u8>>,
        fetch_gate: Option<Arc<dyn Fn() + Send + Sync>>,
    }

    #[async_trait::async_trait]
    impl cache::Pinner for NativeFixturePinner {
        async fn pin(&self, _: &cid::Cid) -> anyhow::Result<()> {
            Ok(())
        }

        async fn unpin(&self, _: &cid::Cid) -> anyhow::Result<()> {
            Ok(())
        }

        async fn fetch(&self, cid: &cid::Cid) -> anyhow::Result<Vec<u8>> {
            if let Some(gate) = &self.fetch_gate {
                gate();
            }
            self.contents
                .get(cid)
                .cloned()
                .ok_or_else(|| anyhow::anyhow!("unknown native fixture CID {cid}"))
        }

        async fn fetch_to_path(&self, cid: &cid::Cid, destination: &Path) -> anyhow::Result<()> {
            let bytes = self.fetch(cid).await?;
            if let Some(parent) = destination.parent() {
                tokio::fs::create_dir_all(parent).await?;
            }
            tokio::fs::write(destination, bytes).await?;
            Ok(())
        }

        async fn size(&self, cid: &cid::Cid) -> anyhow::Result<u64> {
            Ok(self
                .contents
                .get(cid)
                .ok_or_else(|| anyhow::anyhow!("unknown native fixture CID {cid}"))?
                .len() as u64)
        }
    }

    fn native_fixture_cid(seed: u8) -> cid::Cid {
        cid::Cid::new_v1(
            0x55,
            cid::multihash::Multihash::wrap(0x12, &[seed; 32]).unwrap(),
        )
    }

    async fn native_filesystem_fixture(
        filesystem: &cell::FilesystemOwnership,
        fetch_gate: Option<Arc<dyn Fn() + Send + Sync>>,
    ) -> (tempfile::TempDir, Arc<cell::vfs::CidTree>, cache::CacheMode) {
        let staging = tempfile::TempDir::new().unwrap();
        let known: cid::Cid = NATIVE_FIXTURE_CID.parse().unwrap();
        let nested_file = native_fixture_cid(11);
        let root_entries = vec![
            cell::vfs::DirEntry {
                name: "known.txt".into(),
                cid: known.to_string(),
                entry_type: cell::vfs::EntryType::File,
                size: 16,
            },
            cell::vfs::DirEntry {
                name: "nested".into(),
                cid: native_fixture_cid(2).to_string(),
                entry_type: cell::vfs::EntryType::Dir,
                size: 0,
            },
        ];
        let mut nested_entries = vec![cell::vfs::DirEntry {
            name: "child".into(),
            cid: nested_file.to_string(),
            entry_type: cell::vfs::EntryType::File,
            size: 12,
        }];
        nested_entries.extend((0..2_048).map(|index| cell::vfs::DirEntry {
            name: format!("stub-{index:04}"),
            cid: nested_file.to_string(),
            entry_type: cell::vfs::EntryType::File,
            size: (index % 17) as u64,
        }));
        for (cid, entries) in [
            (native_fixture_cid(1), root_entries),
            (native_fixture_cid(2), nested_entries),
        ] {
            std::fs::write(
                staging.path().join(format!("{cid}.dirlist.json")),
                serde_json::to_vec(&entries).unwrap(),
            )
            .unwrap();
        }
        let tree = Arc::new(cell::vfs::CidTree::new(
            native_fixture_cid(1),
            ipfs::HttpClient::new("http://127.0.0.1:1".into()),
            staging.path().to_owned(),
            Arc::clone(filesystem.test_resources().host()),
            filesystem.test_scope().clone(),
        ));
        let pinner = Arc::new(NativeFixturePinner {
            contents: HashMap::from([
                (known, b"lazy image bytes".to_vec()),
                (nested_file, b"nested child".to_vec()),
            ]),
            fetch_gate,
        });
        let resources = filesystem.test_resources();
        let host = Arc::clone(resources.host());
        let scope = filesystem.test_scope().clone();
        let logical = resources.try_logical_io().unwrap();
        let setup = host.try_cleanup_transaction(logical, &[&scope]).unwrap();
        let cache = cache::CacheMode::Isolated(
            cache::IsolatedPinset::new(pinner, scope, &setup)
                .await
                .unwrap(),
        );
        (staging, tree, cache)
    }

    fn native_kernel_fixture_component(
        runtime_engine: &cell::engine::RuntimeEngine,
    ) -> Arc<wasmtime::component::Component> {
        use wasm_encoder::{
            ComponentImportSection, ComponentSection, ComponentTypeRef, ComponentTypeSection,
            ComponentValType, InstanceType, TypeBounds,
        };

        let artifact = std::env::var_os("WW_NATIVE_P3_FIXTURE")
            .map(PathBuf::from)
            .unwrap_or_else(|| {
                PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                    .join("target/native-p3-fixture/wasm32-wasip3/release/native_p3_fixture.wasm")
            });
        let mut bytes = std::fs::read(&artifact)
            .unwrap_or_else(|error| panic!("read {}: {error}", artifact.display()));
        let types = wasmparser::Validator::new_with_features(wasmparser::WasmFeatures::all())
            .validate_all(&bytes)
            .expect("valid native P3 fixture");
        let readiness_type = types.as_ref().component_type_count();

        // These filesystem tests need the kernel's CidTree. Add the kernel-only
        // readiness contract to the in-memory fixture without changing its code
        // or the ordinary Cell artifact. The guest does not call this import.
        let mut readiness = InstanceType::new();
        readiness
            .ty()
            .defined_type()
            .enum_type(["stale-generation"]);
        readiness.export("ready-error", ComponentTypeRef::Type(TypeBounds::Eq(0)));
        readiness
            .ty()
            .defined_type()
            .result(None, Some(ComponentValType::Type(1)));
        readiness
            .ty()
            .function()
            .params(std::iter::empty::<(&str, ComponentValType)>())
            .result(Some(ComponentValType::Type(2)));
        readiness.export("kernel-ready", ComponentTypeRef::Func(3));
        let mut types = ComponentTypeSection::new();
        types.instance(&readiness);
        types.append_to_component(&mut bytes);
        let mut imports = ComponentImportSection::new();
        imports.import(
            "wetware:kernel-runtime/readiness@1.0.0",
            ComponentTypeRef::Instance(readiness_type),
        );
        imports.append_to_component(&mut bytes);
        Arc::new(
            cell::engine::compile_component(&runtime_engine.engine(), &bytes)
                .unwrap_or_else(|error| panic!("load {}: {error}", artifact.display())),
        )
    }

    fn spawn_spinning_sibling(
        pool: &crate::services::ExecutorPool,
        runtime_engine: cell::engine::RuntimeEngine,
        progress: std::sync::mpsc::Sender<()>,
    ) {
        let wasm = Arc::new(
            wasmtime::component::Component::new(
                &runtime_engine.engine(),
                wat::parse_str(include_str!("../tests/fixtures/spinning-component.wat")).unwrap(),
            )
            .unwrap(),
        );
        assert!(pool
            .spawn(crate::services::SpawnRequest {
                name: "filesystem-progress-sibling".into(),
                factory: Box::new(move |_| {
                    Box::pin(async move {
                        let fuel = FuelObserver::default();
                        let (mut builder, _) = Builder::ordinary(
                            Program::Precompiled(wasm),
                            io::empty(),
                            io::sink(),
                            io::sink(),
                            test_filesystem_ownership(),
                        );
                        builder = builder
                            .with_runtime_engine(runtime_engine)
                            .with_fuel_estimator(FuelEstimator::new_oneshot(u64::MAX, 10_000, 0))
                            .with_fuel_observer(fuel.clone());
                        let completion = builder.filesystem_completion();
                        let finalizer = builder.take_filesystem_finalizer().unwrap();
                        tokio::task::spawn_local(rpc::local_tasks::track(async move {
                            let _ = finalizer.run().await;
                        }));
                        let proc = builder.build().await.unwrap();
                        let mut run = Box::pin(proc.run());
                        tokio::select! {
                            result = &mut run => { result.unwrap(); }
                            _ = fuel.wait_for_suspension(1) => {
                                progress.send(()).unwrap();
                                let _ = run.as_mut().await;
                            }
                        }
                        drop(run);
                        completion.wait().await.unwrap();
                    })
                }),
                result_tx: None,
            })
            .is_ok());
    }

    struct ScratchLifecycleSignals {
        cleanup_entered: std::sync::mpsc::Sender<PathBuf>,
        release_rx: std::sync::mpsc::Receiver<()>,
        wait_pending: std::sync::mpsc::Sender<()>,
        wait_completed: std::sync::mpsc::Sender<i32>,
        shutdown_observed: Option<std::sync::mpsc::Sender<()>>,
    }

    fn spawn_scratch_lifecycle(
        pool: &crate::services::ExecutorPool,
        runtime_engine: cell::engine::RuntimeEngine,
        component: Arc<wasmtime::component::Component>,
        signals: ScratchLifecycleSignals,
    ) {
        let ScratchLifecycleSignals {
            cleanup_entered,
            release_rx,
            wait_pending,
            wait_completed,
            shutdown_observed,
        } = signals;
        let release_rx = Arc::new(std::sync::Mutex::new(Some(release_rx)));
        let (cleanup_signal_tx, cleanup_signal_rx) = tokio::sync::oneshot::channel();
        let cleanup_signal_tx = Arc::new(std::sync::Mutex::new(Some(cleanup_signal_tx)));
        let hook: Arc<dyn Fn(cache::FsTestEvent) + Send + Sync> = Arc::new(move |event| {
            if event.operation == cache::FilesystemOperation::ProcessCleanup
                && event.stage == cache::FsTestStage::CleanupStarted
                && event
                    .path
                    .as_ref()
                    .is_some_and(|path| path.join("probe.txt").is_file())
            {
                cleanup_entered.send(event.path.unwrap()).unwrap();
                if let Some(sender) = cleanup_signal_tx.lock().unwrap().take() {
                    let _ = sender.send(());
                }
                let receiver = release_rx.lock().unwrap().take().unwrap();
                let _ = receiver.recv();
            }
        });
        assert!(pool
            .spawn(crate::services::SpawnRequest {
                name: "scratch-cleanup-lifecycle".into(),
                factory: Box::new(move |_| {
                    Box::pin(async move {
                        use std::future::Future;
                        use std::task::Poll;
                        use tokio::io::AsyncReadExt;

                        let (mut lifecycle, process, _, _, observer) = lifecycle_loss_fixture();
                        if let Some(old_rpc) = lifecycle.rpc_task.take() {
                            old_rpc.shutdown();
                        }
                        let filesystem = test_filesystem_ownership();
                        let (_tree_staging, tree, cache) =
                            native_filesystem_fixture(&filesystem, None).await;
                        let (_epoch_tx, epoch_rx) = tokio::sync::watch::channel(authority::Epoch {
                            seq: 1,
                            head: Vec::new(),
                            root: None,
                        });
                        let readiness = Arc::new(authority::KernelReadyGate::new(epoch_rx));
                        readiness.bind_generation(1);
                        let (mut stdout_reader, stdout_writer) = io::duplex(64);
                        let (mut builder, mut handles) = Builder::kernel(
                            Program::Precompiled(component),
                            io::empty(),
                            stdout_writer,
                            io::sink(),
                            tree,
                            readiness,
                            filesystem,
                        );
                        builder = builder
                            .with_runtime_engine(runtime_engine)
                            .with_args(vec!["native-p3-fixture".into(), "scratch-wait".into()])
                            .with_cache(cache)
                            .with_filesystem_test_hook(hook);
                        let filesystem_completion = builder.filesystem_completion();
                        let finalizer = builder.take_filesystem_finalizer().unwrap();
                        tokio::task::spawn_local(rpc::local_tasks::track(async move {
                            let _ = finalizer.run().await;
                        }));
                        lifecycle.proc = Some(builder.build().await.unwrap());
                        lifecycle.filesystem_completion = filesystem_completion;
                        let (reader, writer) = handles.take_host_split().unwrap();
                        let (rpc, _) = graft::build_child_membrane_rpc(
                            reader,
                            writer,
                            lifecycle.membrane.as_ref().unwrap().clone(),
                        );
                        lifecycle.rpc_task = Some(rpc);
                        let owner = tokio::task::spawn_local(lifecycle.run());
                        let owner = if let Some(shutdown_observed) = shutdown_observed {
                            let shutdown = rpc::local_tasks::shutdown_token();
                            tokio::task::spawn_local(rpc::local_tasks::track(async move {
                                shutdown.cancelled().await;
                                owner.abort();
                                assert!(owner.await.unwrap_err().is_cancelled());
                                shutdown_observed.send(()).unwrap();
                            }));
                            None
                        } else {
                            Some(owner)
                        };

                        let mut marker = vec![0; b"scratch-ready\n".len()];
                        stdout_reader.read_exact(&mut marker).await.unwrap();
                        assert_eq!(marker, b"scratch-ready\n");
                        let mut wait = Box::pin(process.wait_request().send().promise);
                        process.kill_request().send().promise.await.unwrap();
                        cleanup_signal_rx.await.unwrap();
                        assert_eq!(observer.state(), rpc::CleanupState::Running);
                        std::future::poll_fn(|context| {
                            assert!(matches!(wait.as_mut().poll(context), Poll::Pending));
                            Poll::Ready(())
                        })
                        .await;
                        wait_pending.send(()).unwrap();
                        let exit_code = wait.await.unwrap().get().unwrap().get_exit_code();
                        wait_completed.send(exit_code).unwrap();
                        if let Some(owner) = owner {
                            owner.await.unwrap();
                            assert_eq!(observer.state(), rpc::CleanupState::Cleaned(137));
                        }
                    })
                }),
                result_tx: None,
            })
            .is_ok());
    }

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

    fn completed_filesystem_scope() -> cache::FsCompletion {
        let (_scope, owner, finalizer, completion) = test_filesystem_host()
            .try_filesystem_scope()
            .expect("test filesystem scope");
        drop(owner);
        drop(finalizer);
        completion
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
                filesystem_completion: completed_filesystem_scope(),
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
            filesystem_completion: completed_filesystem_scope(),
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
                let (builder, mut handles) = Builder::ordinary(
                    Program::Bytes(wasm),
                    io::empty(),
                    io::sink(),
                    io::sink(),
                    test_filesystem_ownership(),
                );
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
            let (builder, mut handles) = Builder::ordinary(
                Program::Bytes(wasm),
                io::empty(),
                io::sink(),
                io::sink(),
                test_filesystem_ownership(),
            );
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
                let (builder, mut handles) = Builder::ordinary(
                    Program::Bytes(wasm),
                    io::empty(),
                    io::sink(),
                    io::sink(),
                    test_filesystem_ownership(),
                );
                let mut builder = builder
                    .with_runtime_engine(runtime_engine)
                    .with_fuel_estimator(FuelEstimator::new_oneshot(100_000, 10_000, 0))
                    .with_fuel_observer(fuel.clone());
                let filesystem_completion = builder.filesystem_completion();
                let finalizer = builder.take_filesystem_finalizer().unwrap();
                tokio::task::spawn_local(async move {
                    let _ = finalizer.run().await;
                });
                let proc = builder.build().await.unwrap();
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
                        filesystem_completion,
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
    fn cold_file_staging_preserves_sibling_and_worker_zero_epoch_progress() {
        let (_shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(());
        let pool = crate::services::ExecutorPool::new(1, shutdown_rx, test_filesystem_host());
        let runtime_engine = pool.runtime_engine();
        let component = native_kernel_fixture_component(&runtime_engine);
        let mut epoch = runtime_engine.subscribe();
        let (entered_tx, entered_rx) = std::sync::mpsc::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let release_rx = Arc::new(std::sync::Mutex::new(Some(release_rx)));
        let fetch_gate: Arc<dyn Fn() + Send + Sync> = Arc::new(move || {
            if let Some(receiver) = release_rx.lock().unwrap().take() {
                entered_tx.send(()).unwrap();
                let _ = receiver.recv();
            }
        });
        let (result_tx, result_rx) = tokio::sync::oneshot::channel();
        let cell_engine = runtime_engine.clone();
        assert!(pool
            .spawn(crate::services::SpawnRequest {
                name: "cold-file-progress".into(),
                factory: Box::new(move |_| {
                    Box::pin(async move {
                        let filesystem = test_filesystem_ownership();
                        let (_tree_staging, tree, cache) =
                            native_filesystem_fixture(&filesystem, Some(fetch_gate)).await;
                        let (_epoch_tx, epoch_rx) = tokio::sync::watch::channel(authority::Epoch {
                            seq: 1,
                            head: Vec::new(),
                            root: None,
                        });
                        let readiness = Arc::new(authority::KernelReadyGate::new(epoch_rx));
                        readiness.bind_generation(1);
                        let (mut builder, _) = Builder::kernel(
                            Program::Precompiled(component),
                            io::empty(),
                            io::sink(),
                            io::sink(),
                            tree,
                            readiness,
                            filesystem,
                        );
                        builder = builder
                            .with_runtime_engine(cell_engine)
                            .with_args(vec!["native-p3-fixture".into(), "cold-file".into()])
                            .with_cache(cache);
                        let completion = builder.filesystem_completion();
                        let finalizer = builder.take_filesystem_finalizer().unwrap();
                        tokio::task::spawn_local(rpc::local_tasks::track(async move {
                            let _ = finalizer.run().await;
                        }));
                        builder.build().await.unwrap().run().await.unwrap();
                        completion.wait().await.unwrap();
                    })
                }),
                result_tx: Some(result_tx),
            })
            .is_ok());

        entered_rx.recv_timeout(Duration::from_secs(10)).unwrap();
        let held_epoch = runtime_engine.current_epoch();
        let (sibling_tx, sibling_rx) = std::sync::mpsc::channel();
        spawn_spinning_sibling(&pool, runtime_engine.clone(), sibling_tx);
        sibling_rx.recv_timeout(Duration::from_secs(10)).unwrap();
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(async {
                tokio::time::timeout(Duration::from_secs(10), async {
                    loop {
                        if epoch.changed().await.unwrap() > held_epoch {
                            break;
                        }
                    }
                })
                .await
                .unwrap();
            });
        release_tx.send(()).unwrap();
        assert_eq!(result_rx.blocking_recv().unwrap().unwrap(), 0);
        drop(pool);
    }

    #[test]
    fn directory_publication_preserves_sibling_progress_and_final_contents() {
        let (_shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(());
        let pool = crate::services::ExecutorPool::new(1, shutdown_rx, test_filesystem_host());
        let runtime_engine = pool.runtime_engine();
        let component = native_kernel_fixture_component(&runtime_engine);
        let (entered_tx, entered_rx) = std::sync::mpsc::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let release_rx = Arc::new(std::sync::Mutex::new(Some(release_rx)));
        let hook: Arc<dyn Fn(cache::FsTestEvent) + Send + Sync> = Arc::new(move |event| {
            if event.operation == cache::FilesystemOperation::DirectoryBuild
                && event.stage == cache::FsTestStage::BeforePublish
            {
                entered_tx.send(event.path).unwrap();
                let receiver = release_rx.lock().unwrap().take().unwrap();
                let _ = receiver.recv();
            }
        });
        let (contents_tx, contents_rx) = std::sync::mpsc::channel();
        let (result_tx, result_rx) = tokio::sync::oneshot::channel();
        let cell_engine = runtime_engine.clone();
        assert!(pool
            .spawn(crate::services::SpawnRequest {
                name: "directory-progress".into(),
                factory: Box::new(move |_| {
                    Box::pin(async move {
                        let filesystem = test_filesystem_ownership();
                        let (tree_staging, tree, cache) =
                            native_filesystem_fixture(&filesystem, None).await;
                        let nested = tree_staging
                            .path()
                            .join(format!("dir-{}", native_fixture_cid(2)));
                        let (_epoch_tx, epoch_rx) = tokio::sync::watch::channel(authority::Epoch {
                            seq: 1,
                            head: Vec::new(),
                            root: None,
                        });
                        let readiness = Arc::new(authority::KernelReadyGate::new(epoch_rx));
                        readiness.bind_generation(1);
                        let (mut builder, _) = Builder::kernel(
                            Program::Precompiled(component),
                            io::empty(),
                            io::sink(),
                            io::sink(),
                            tree,
                            readiness,
                            filesystem,
                        );
                        builder = builder
                            .with_runtime_engine(cell_engine)
                            .with_args(vec!["native-p3-fixture".into(), "directory".into()])
                            .with_cache(cache)
                            .with_filesystem_test_hook(hook);
                        let completion = builder.filesystem_completion();
                        let finalizer = builder.take_filesystem_finalizer().unwrap();
                        tokio::task::spawn_local(rpc::local_tasks::track(async move {
                            let _ = finalizer.run().await;
                        }));
                        builder.build().await.unwrap().run().await.unwrap();
                        completion.wait().await.unwrap();
                        contents_tx
                            .send(
                                nested.is_dir()
                                    && nested.join("child").metadata().unwrap().len() == 12
                                    && std::fs::read_dir(&nested).unwrap().count() == 2_049,
                            )
                            .unwrap();
                    })
                }),
                result_tx: Some(result_tx),
            })
            .is_ok());

        let temporary = entered_rx
            .recv_timeout(Duration::from_secs(10))
            .unwrap()
            .unwrap();
        assert!(temporary.join("child").is_file());
        assert_eq!(std::fs::read_dir(&temporary).unwrap().count(), 2_049);
        let (sibling_tx, sibling_rx) = std::sync::mpsc::channel();
        spawn_spinning_sibling(&pool, runtime_engine, sibling_tx);
        sibling_rx.recv_timeout(Duration::from_secs(10)).unwrap();
        release_tx.send(()).unwrap();
        assert_eq!(result_rx.blocking_recv().unwrap().unwrap(), 0);
        assert!(contents_rx.recv_timeout(Duration::from_secs(10)).unwrap());
        drop(pool);
    }

    #[test]
    fn slow_scratch_teardown_keeps_process_wait_pending_and_sibling_running() {
        let (_shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(());
        let pool = crate::services::ExecutorPool::new(1, shutdown_rx, test_filesystem_host());
        let runtime_engine = pool.runtime_engine();
        let component = native_kernel_fixture_component(&runtime_engine);
        let (cleanup_tx, cleanup_rx) = std::sync::mpsc::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let (pending_tx, pending_rx) = std::sync::mpsc::channel();
        let (completed_tx, completed_rx) = std::sync::mpsc::channel();
        spawn_scratch_lifecycle(
            &pool,
            runtime_engine.clone(),
            component,
            ScratchLifecycleSignals {
                cleanup_entered: cleanup_tx,
                release_rx,
                wait_pending: pending_tx,
                wait_completed: completed_tx,
                shutdown_observed: None,
            },
        );

        let scratch_path = cleanup_rx.recv_timeout(Duration::from_secs(10)).unwrap();
        assert!(scratch_path.join("probe.txt").is_file());
        pending_rx.recv_timeout(Duration::from_secs(10)).unwrap();
        let (sibling_tx, sibling_rx) = std::sync::mpsc::channel();
        spawn_spinning_sibling(&pool, runtime_engine, sibling_tx);
        sibling_rx.recv_timeout(Duration::from_secs(10)).unwrap();
        release_tx.send(()).unwrap();
        assert_eq!(
            completed_rx.recv_timeout(Duration::from_secs(10)).unwrap(),
            137
        );
        assert!(!scratch_path.exists());
        drop(pool);
    }

    #[test]
    fn executor_pool_shutdown_waits_for_owned_scratch_cleanup() {
        let (_shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(());
        let pool = crate::services::ExecutorPool::new(1, shutdown_rx, test_filesystem_host());
        let runtime_engine = pool.runtime_engine();
        let component = native_kernel_fixture_component(&runtime_engine);
        let (cleanup_tx, cleanup_rx) = std::sync::mpsc::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let (pending_tx, pending_rx) = std::sync::mpsc::channel();
        let (completed_tx, _completed_rx) = std::sync::mpsc::channel();
        let (shutdown_observed_tx, shutdown_observed_rx) = std::sync::mpsc::channel();
        spawn_scratch_lifecycle(
            &pool,
            runtime_engine,
            component,
            ScratchLifecycleSignals {
                cleanup_entered: cleanup_tx,
                release_rx,
                wait_pending: pending_tx,
                wait_completed: completed_tx,
                shutdown_observed: Some(shutdown_observed_tx),
            },
        );
        let scratch_path = cleanup_rx.recv_timeout(Duration::from_secs(10)).unwrap();
        pending_rx.recv_timeout(Duration::from_secs(10)).unwrap();
        let (drop_started_tx, drop_started_rx) = std::sync::mpsc::channel();
        let (drop_done_tx, drop_done_rx) = std::sync::mpsc::channel();
        let drop_thread = std::thread::spawn(move || {
            drop_started_tx.send(()).unwrap();
            drop(pool);
            drop_done_tx.send(()).unwrap();
        });
        drop_started_rx
            .recv_timeout(Duration::from_secs(10))
            .unwrap();
        shutdown_observed_rx
            .recv_timeout(Duration::from_secs(10))
            .unwrap();
        assert!(matches!(
            drop_done_rx.try_recv(),
            Err(std::sync::mpsc::TryRecvError::Empty)
        ));
        release_tx.send(()).unwrap();
        drop_done_rx.recv_timeout(Duration::from_secs(10)).unwrap();
        drop_thread.join().unwrap();
        assert!(!scratch_path.exists());
    }

    #[test]
    fn executor_pool_shutdown_joins_child_lifecycle_and_rpc() {
        for close_spawn_channel in [false, true] {
            let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(());
            let pool = crate::services::ExecutorPool::new(1, shutdown_rx, test_filesystem_host());
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
                            test_filesystem_ownership(),
                        );
                        let mut builder = builder
                            .with_runtime_engine(engine)
                            .with_fuel_estimator(FuelEstimator::new_oneshot(u64::MAX, 10_000, 0))
                            .with_fuel_observer(fuel.clone());
                        let filesystem_completion = builder.filesystem_completion();
                        let finalizer = builder.take_filesystem_finalizer().unwrap();
                        tokio::task::spawn_local(rpc::local_tasks::track(async move {
                            let _ = finalizer.run().await;
                        }));
                        lifecycle.proc = Some(builder.build().await.unwrap());
                        lifecycle.filesystem_completion = filesystem_completion;
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
                    test_filesystem_host(),
                );

                let mut load = runtime.load_request();
                load.get().set_wasm(b"wetware");
                let executor = load
                    .send()
                    .promise
                    .await
                    .expect("fresh Runtime must load")
                    .get()
                    .expect("load results")
                    .get_executor()
                    .expect("Executor result");
                let response = executor
                    .bytecode_cid_request()
                    .send()
                    .promise
                    .await
                    .expect("fresh Executor must answer");
                assert_eq!(
                    response.get().unwrap().get_cid().unwrap().to_str().unwrap(),
                    "bafkr4id4izs4xqbbbmlhwbc7gu5bl3ogbclvqyepzdqfxgjpyoovqmqtnm",
                    "Executor must return the canonical bytecode CID",
                );

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
                    executor
                        .bytecode_cid_request()
                        .send()
                        .promise
                        .await
                        .is_err(),
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
                    test_filesystem_host(),
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
