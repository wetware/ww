//! Cell-launching capability impls (`RuntimeImpl`, `ExecutorImpl`).
//!
//! Wires the rpc protocol layer to the cell execution layer. These capnp
//! `Server` impls build cells from RPC requests, so they sit at the
//! orchestration seam between `crate::rpc` (protocol) and `crate::cell`
//! (execution). Hosting them here lets `rpc` stay free of any `cell` dep.
#![cfg(not(target_arch = "wasm32"))]

use std::cell::RefCell;
use std::collections::HashMap;
use std::sync::Arc;

use capnp::capability::Promise;
use capnp_rpc::pry;
use futures::FutureExt;
use tokio::io;
use tokio::sync::{mpsc, oneshot};

use ::authority::EpochGuard;

use crate::services::CompileRequest;
use crate::system_capnp;
use cell::proc::FuelEstimator;
use cell::{Builder, Proc, Program};
use rpc::{
    graft, ByteStreamImpl, CachePolicy, InitialAuthorityRecord, ProcessBootstrapControl,
    ProcessImpl, StreamMode,
};

/// Maximum WASM binary size accepted by the Executor.
///
/// Rejects oversized binaries before compilation to bound memory and
/// CPU spent on untrusted guest code while still accommodating larger
/// practical WASM guests.
const MAX_WASM_BYTES: usize = 8 * 1024 * 1024;

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
    engine: Arc<wasmtime::Engine>,
    /// Optional compilation service channel.
    compile_tx: Option<mpsc::Sender<CompileRequest>>,
    /// Optional host-managed cache for the accepted known-CID read substrate.
    ///
    /// This is process substrate, not a grant: it permits reads only when the
    /// guest already knows a CID and exposes no enumeration, mutation, pin,
    /// publishing, routing, or network-dial API. Reads can still consume node
    /// network, disk, pin/cache budget, and eviction work.
    pinset_cache: Option<Arc<cache::PinsetCache>>,
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
            engine: self.engine.clone(),
            wasm_debug: self.wasm_debug,
            guard: self.guard.clone(),
            pinset_cache: self.pinset_cache.clone(),
        })
    }
}

fn build_wasmtime_engine() -> Arc<wasmtime::Engine> {
    Arc::new(cell::engine::wasm_engine().expect("failed to create wasmtime engine"))
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
    engine: Option<Arc<wasmtime::Engine>>,
    compile_tx: Option<mpsc::Sender<CompileRequest>>,
    cache_policy: CachePolicy,
) -> system_capnp::runtime::Client {
    create_runtime_client_with_pinset(wasm_debug, guard, engine, compile_tx, cache_policy, None)
}

/// Create a Runtime whose Executors receive the fixed known-CID read
/// substrate through a host-managed shared pinset cache.
///
/// Ordinary tests and embedded callers can continue using
/// [`create_runtime_client`] for a cache-free substrate.
pub fn create_runtime_client_with_pinset(
    wasm_debug: bool,
    guard: EpochGuard,
    engine: Option<Arc<wasmtime::Engine>>,
    compile_tx: Option<mpsc::Sender<CompileRequest>>,
    cache_policy: CachePolicy,
    pinset_cache: Option<Arc<cache::PinsetCache>>,
) -> system_capnp::runtime::Client {
    let runtime = RuntimeImpl {
        wasm_debug,
        guard,
        cache_policy,
        executor_cache: RefCell::new(HashMap::new()),
        engine: engine.unwrap_or_else(build_wasmtime_engine),
        compile_tx,
        pinset_cache,
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

        if wasm.len() > MAX_WASM_BYTES {
            return Promise::err(capnp::Error::failed(format!(
                "WASM binary too large ({} bytes, max {})",
                wasm.len(),
                MAX_WASM_BYTES
            )));
        }

        let key = *blake3::hash(&wasm).as_bytes();
        let bytecode = Arc::new(wasm);
        let compile_tx = self.compile_tx.clone();
        let engine = self.engine.clone();
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

/// An Executor bound to a specific WASM binary. Each `spawn(args, env)` creates
/// a fresh WASI process from the stored bytecode with the given args and env.
///
/// This is the attenuated capability in the OCAP model: the holder can spawn
/// workers but cannot load arbitrary code. Args and env are late-bound per-spawn,
/// which solves the WAGI CGI env var problem (per-request env vars like
/// REQUEST_METHOD, PATH_INFO, etc.).
pub struct ExecutorImpl {
    bytecode: Arc<Vec<u8>>,
    component: Option<Arc<wasmtime::component::Component>>,
    engine: Arc<wasmtime::Engine>,
    wasm_debug: bool,
    guard: EpochGuard,
    pinset_cache: Option<Arc<cache::PinsetCache>>,
}

/// Owns every resource whose lifetime is exactly one running child.
///
/// The task running this value is the authority-record lifetime boundary:
/// process exit or kill aborts child RPC/stderr work and releases the record.
/// Grant references cannot keep the process or its RPC task alive.
struct OwnedChildLifecycle {
    proc: Option<Proc>,
    rpc_task: Option<tokio::task::JoinHandle<()>>,
    stderr_task: Option<tokio::task::JoinHandle<()>>,
    record: Option<InitialAuthorityRecord>,
    bootstrap_control: ProcessBootstrapControl,
    kill_rx: tokio::sync::watch::Receiver<bool>,
    exit_tx: Option<tokio::sync::oneshot::Sender<i32>>,
}

impl OwnedChildLifecycle {
    async fn run(mut self) {
        let proc = self
            .proc
            .take()
            .expect("owned child lifecycle starts with a process");
        let mut proc_run = Box::pin(proc.run());
        let mut watch_kill = true;
        let exit_code = loop {
            if watch_kill {
                tokio::select! {
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
                        match changed {
                            Ok(()) if *self.kill_rx.borrow() => {
                                tracing::info!("executor: child process killed");
                                break 137;
                            }
                            Ok(()) => {}
                            Err(_) => {
                                // Dropping every Process handle is not an
                                // implicit kill operation. Await natural exit.
                                watch_kill = false;
                            }
                        }
                    }
                }
            } else {
                break match proc_run.await {
                    Ok(()) => 0,
                    Err(error) => {
                        tracing::error!("executor: child process failed: {error}");
                        1
                    }
                };
            }
        };

        self.stop_auxiliary_tasks().await;
        self.bootstrap_control.clear();
        // Keep the immutable record live through process execution, then
        // release it after child RPC and the stored guest bootstrap are gone
        // but before reporting exit to the parent.
        drop(self.record.take());
        tracing::info!("executor: child process exited with code {exit_code}");
        if let Some(exit_tx) = self.exit_tx.take() {
            let _ = exit_tx.send(exit_code);
        }
    }

    fn abort_auxiliary_tasks(&mut self) {
        if let Some(task) = self.rpc_task.take() {
            task.abort();
        }
        if let Some(task) = self.stderr_task.take() {
            task.abort();
        }
    }

    async fn stop_auxiliary_tasks(&mut self) {
        let rpc_task = self.rpc_task.take();
        let stderr_task = self.stderr_task.take();
        if let Some(task) = &rpc_task {
            task.abort();
        }
        if let Some(task) = &stderr_task {
            task.abort();
        }
        if let Some(task) = rpc_task {
            let _ = task.await;
        }
        if let Some(task) = stderr_task {
            let _ = task.await;
        }
    }
}

impl Drop for OwnedChildLifecycle {
    fn drop(&mut self) {
        self.abort_auxiliary_tasks();
    }
}

/// Armed only until the Process capability is installed in the spawn result.
///
/// If the RPC call is cancelled or errors during that handoff, aborting the
/// owner task drops the process, streams, record, and child RPC task together.
struct SpawnHandoffGuard {
    abort_handle: Option<tokio::task::AbortHandle>,
    kill_tx: tokio::sync::watch::Sender<bool>,
}

impl SpawnHandoffGuard {
    fn new(
        abort_handle: tokio::task::AbortHandle,
        kill_tx: tokio::sync::watch::Sender<bool>,
    ) -> Self {
        Self {
            abort_handle: Some(abort_handle),
            kill_tx,
        }
    }

    fn complete(mut self) {
        self.abort_handle.take();
    }
}

impl Drop for SpawnHandoffGuard {
    fn drop(&mut self) {
        if let Some(abort_handle) = self.abort_handle.take() {
            let _ = self.kill_tx.send(true);
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

        // Final child-admission chokepoint: decode, validate, and retain the
        // complete immutable record before stdio allocation or process build.
        // Promise/pipelined/broken references remain opaque here.
        let initial_authority = pry!(params.get_caps().and_then(InitialAuthorityRecord::decode));

        let bytecode = self.bytecode.clone();
        let component = self.component.clone();
        let engine = self.engine.clone();
        let wasm_debug = self.wasm_debug;
        let pinset_cache = self.pinset_cache.clone();

        Promise::from_future(async move {
            let (host_stderr, guest_stderr) = io::duplex(64 * 1024);
            let (host_stdin, guest_stdin) = io::duplex(64 * 1024);
            let (host_stdout, guest_stdout) = io::duplex(64 * 1024);

            let (exit_tx, exit_rx) = tokio::sync::oneshot::channel();
            let (kill_tx, kill_rx) = tokio::sync::watch::channel(false);
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
                .with_engine(engine)
                .with_env(env)
                .with_args(args)
                .with_wasm_debug(wasm_debug);
            if let Some(est) = fuel_estimator {
                builder = builder.with_fuel_estimator(est);
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

            // Every Runtime/Executor configuration uses the same bounded,
            // record-served child authority model. There is no raw Host
            // fallback when epoch or stream wiring is absent.
            let (child_rpc_system, guest_bootstrap) =
                graft::build_initial_authority_rpc(reader, writer, initial_authority.clone());

            let rpc_task = tokio::task::spawn_local(child_rpc_system.map(|_| ()));
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
                exit_rx,
                guest_bootstrap,
                kill_tx.clone(),
            );

            let lifecycle_task = tokio::task::spawn_local(
                OwnedChildLifecycle {
                    proc: Some(proc),
                    rpc_task: Some(rpc_task),
                    stderr_task: Some(stderr_task),
                    record: Some(initial_authority),
                    bootstrap_control,
                    kill_rx,
                    exit_tx: Some(exit_tx),
                }
                .run(),
            );
            let handoff = SpawnHandoffGuard::new(lifecycle_task.abort_handle(), kill_tx.clone());

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
        const RAW_CODEC: u64 = 0x55;
        const BLAKE3_MULTIHASH_CODE: u64 = 0x1e;

        let digest = blake3::hash(&self.bytecode);
        let mh = cid::multihash::Multihash::<64>::wrap(BLAKE3_MULTIHASH_CODE, digest.as_bytes())
            .expect("valid blake3 multihash");
        let cid = cid::Cid::new_v1(RAW_CODEC, mh);
        results.get().set_cid(cid.to_string());
        Promise::ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;
    use std::rc::Rc;

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
            provenance: authority::Provenance::Block(seq),
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
                // process, RPC task, streams, and record are all captured by
                // the one abort target guarded until Process handoff.
                let lifecycle_task = tokio::task::spawn_local(async move {
                    let _resources = resources;
                    std::future::pending::<()>().await;
                });
                let (kill_tx, mut kill_rx) = tokio::sync::watch::channel(false);
                let handoff = SpawnHandoffGuard::new(lifecycle_task.abort_handle(), kill_tx);

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
                let runtime =
                    create_runtime_client(false, guard, None, None, CachePolicy::Isolated);

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
                    provenance: authority::Provenance::Block(2),
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
                let runtime = create_runtime_client(
                    false,
                    authority::EpochGuard::fixed(authority::Epoch::zero()),
                    None,
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
