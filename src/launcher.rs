//! Cell-launching capability impls (`RuntimeImpl`, `ExecutorImpl`).
//!
//! Wires the rpc protocol layer to the cell execution layer. These capnp
//! `Server` impls build cells from RPC requests, so they sit at the
//! orchestration seam between `crate::rpc` (protocol) and `crate::cell`
//! (execution). Hosting them here lets `rpc` stay free of any `cell` dep.
#![cfg(not(target_arch = "wasm32"))]

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;
use std::sync::Arc;

use capnp::capability::Promise;
use capnp_rpc::pry;
use futures::FutureExt;
use tokio::io;
use tokio::sync::{mpsc, oneshot};

use ::membrane::EpochGuard;

use crate::host::SwarmCommand;
use crate::services::CompileRequest;
use crate::system_capnp;
use cell::proc::{Builder as ProcBuilder, FuelEstimator};
use rpc::{
    build_peer_rpc, canonicalize_schema_node, graft, ByteStreamImpl, CachePolicy, NetworkState,
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
/// **System-wide singleton**: RuntimeImpl is created once and every membrane
/// graft (including child cells) receives a clone of the same client. This
/// guarantees system-wide cache sharing by construction.
///
/// **OCAP discipline**: Runtime is the powerful capability (can load any binary).
/// Only pid0 gets it from `graft()`. Executor is the attenuated capability
/// (bound to one binary, can only spawn instances). pid0 hands Executors to
/// listeners, never Runtime.
pub struct RuntimeImpl {
    network_state: NetworkState,
    swarm_cmd_tx: mpsc::Sender<SwarmCommand>,
    wasm_debug: bool,
    guard: Option<EpochGuard>,
    epoch_rx: Option<tokio::sync::watch::Receiver<::membrane::Epoch>>,
    signing_key: Option<Arc<ed25519_dalek::SigningKey>>,
    stream_control: Option<libp2p_stream::Control>,
    /// Runtime-wide cache policy (from `WW_RUNTIME_CACHE_POLICY` env var).
    cache_policy: CachePolicy,
    /// BLAKE3(wasm bytes) → cached Executor client (used when policy = Shared).
    ///
    /// RefCell is correct because Cap'n Proto server dispatch runs on a
    /// single-threaded LocalSet.
    executor_cache: RefCell<HashMap<[u8; 32], system_capnp::executor::Client>>,
    /// Back-reference to this Runtime's own client. Injected by
    /// [`create_runtime_client`] after construction. Cloned into each
    /// ExecutorImpl so child cells receive the same Runtime through their
    /// membrane graft.
    self_client: Rc<RefCell<Option<system_capnp::runtime::Client>>>,
    /// Shared Wasmtime engine for this runtime and all executors it creates.
    engine: Arc<wasmtime::Engine>,
    /// Optional compilation service channel.
    compile_tx: Option<mpsc::Sender<CompileRequest>>,
    /// IPFS HTTP client for Kubo API calls (e.g. IPNS resolution via routing).
    ipfs_client: crate::ipfs::HttpClient,
    /// Allowed outbound HTTP hosts — inherited by child cells.
    http_dial: Vec<String>,
}

impl RuntimeImpl {
    fn check_epoch(&self) -> Result<(), capnp::Error> {
        match self.guard {
            Some(ref g) => g.check(),
            None => Ok(()),
        }
    }

    /// Create a new ExecutorImpl bound to the given bytecode and wrap it as a client.
    fn make_executor(
        &self,
        bytecode: Arc<Vec<u8>>,
        component: Option<Arc<wasmtime::component::Component>>,
    ) -> system_capnp::executor::Client {
        let runtime_client = self
            .self_client
            .borrow()
            .clone()
            .expect("runtime self-reference must be set (use create_runtime_client)");
        capnp_rpc::new_client(ExecutorImpl {
            bytecode,
            component,
            engine: self.engine.clone(),
            wasm_debug: self.wasm_debug,
            network_state: self.network_state.clone(),
            swarm_cmd_tx: self.swarm_cmd_tx.clone(),
            guard: self.guard.clone(),
            epoch_rx: self.epoch_rx.clone(),
            signing_key: self.signing_key.clone(),
            stream_control: self.stream_control.clone(),
            runtime_client,
            ipfs_client: self.ipfs_client.clone(),
            http_dial: self.http_dial.clone(),
        })
    }
}

fn build_wasmtime_engine() -> Arc<wasmtime::Engine> {
    let mut wasm_config = wasmtime::Config::new();
    wasm_config.consume_fuel(true);
    wasm_config.epoch_interruption(true);
    Arc::new(wasmtime::Engine::new(&wasm_config).expect("failed to create wasmtime engine"))
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

/// Create a RuntimeImpl, wrap it as a client, and inject the self-reference.
///
/// This is the only way to construct a `runtime::Client` backed by a real RuntimeImpl.
/// The returned client is a singleton — clone it wherever a Runtime is needed to
/// ensure all cells share the same compilation/executor cache.
#[allow(clippy::too_many_arguments)]
pub fn create_runtime_client(
    network_state: NetworkState,
    swarm_cmd_tx: mpsc::Sender<SwarmCommand>,
    wasm_debug: bool,
    guard: Option<EpochGuard>,
    epoch_rx: Option<tokio::sync::watch::Receiver<::membrane::Epoch>>,
    signing_key: Option<Arc<ed25519_dalek::SigningKey>>,
    stream_control: Option<libp2p_stream::Control>,
    engine: Option<Arc<wasmtime::Engine>>,
    compile_tx: Option<mpsc::Sender<CompileRequest>>,
    cache_policy: CachePolicy,
    ipfs_client: crate::ipfs::HttpClient,
    http_dial: Vec<String>,
) -> system_capnp::runtime::Client {
    let self_client = Rc::new(RefCell::new(None));
    let runtime = RuntimeImpl {
        network_state,
        swarm_cmd_tx,
        wasm_debug,
        guard,
        epoch_rx,
        signing_key,
        stream_control,
        cache_policy,
        executor_cache: RefCell::new(HashMap::new()),
        self_client: self_client.clone(),
        engine: engine.unwrap_or_else(build_wasmtime_engine),
        compile_tx,
        ipfs_client,
        http_dial,
    };
    let client: system_capnp::runtime::Client = capnp_rpc::new_client(runtime);
    *self_client.borrow_mut() = Some(client.clone());
    client
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
    network_state: NetworkState,
    swarm_cmd_tx: mpsc::Sender<SwarmCommand>,
    guard: Option<EpochGuard>,
    epoch_rx: Option<tokio::sync::watch::Receiver<::membrane::Epoch>>,
    signing_key: Option<Arc<ed25519_dalek::SigningKey>>,
    stream_control: Option<libp2p_stream::Control>,
    /// Runtime client (singleton) — passed to child cells through their membrane graft.
    runtime_client: system_capnp::runtime::Client,
    /// IPFS HTTP client — passed to child cells through their membrane graft.
    ipfs_client: crate::ipfs::HttpClient,
    /// Allowed outbound HTTP hosts — inherited by child cells.
    http_dial: Vec<String>,
}

#[allow(refining_impl_trait)]
impl system_capnp::executor::Server for ExecutorImpl {
    fn spawn(
        self: capnp::capability::Rc<Self>,
        params: system_capnp::executor::SpawnParams,
        mut results: system_capnp::executor::SpawnResults,
    ) -> Promise<(), capnp::Error> {
        if let Some(ref guard) = self.guard {
            pry!(guard.check());
        }

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

        // Read optional caps from spawn request (forwarded from init.d `with` block).
        // Each entry carries its canonical Schema.Node bytes through to the
        // child cell's graft response so guests can introspect the cap's
        // interface without hardcoded fallbacks.
        let extra_caps: Vec<(String, capnp::capability::Client, Vec<u8>)> = {
            let mut caps_vec = Vec::new();
            if let Ok(caps_reader) = params.get_caps() {
                for entry in caps_reader.iter() {
                    if let (Ok(name), Ok(cap)) = (
                        entry.get_name().map(|n| n.to_string().unwrap_or_default()),
                        entry.get_cap().get_as_capability(),
                    ) {
                        let schema_bytes = match entry.get_schema() {
                            Ok(node) => canonicalize_schema_node(node).unwrap_or_default(),
                            Err(_) => Vec::new(),
                        };
                        caps_vec.push((name, cap, schema_bytes));
                    }
                }
            }
            caps_vec
        };

        let bytecode = self.bytecode.clone();
        let bootstrap_schema = match rpc::decode_cell_section(&bytecode) {
            Ok(Some(rpc::CellType::Capnp(schema))) => schema,
            _ => Vec::new(),
        };
        let component = self.component.clone();
        let engine = self.engine.clone();
        let wasm_debug = self.wasm_debug;
        let network_state = self.network_state.clone();
        let swarm_cmd_tx = self.swarm_cmd_tx.clone();
        let epoch_rx = self.epoch_rx.clone();
        let signing_key = self.signing_key.clone();
        let stream_control = self.stream_control.clone();
        let runtime_client = self.runtime_client.clone();
        let ipfs_client = self.ipfs_client.clone();
        let http_dial = self.http_dial.clone();

        Promise::from_future(async move {
            let (host_stderr, guest_stderr) = io::duplex(64 * 1024);
            let (host_stdin, guest_stdin) = io::duplex(64 * 1024);
            let (host_stdout, guest_stdout) = io::duplex(64 * 1024);

            let (exit_tx, exit_rx) = tokio::sync::oneshot::channel();
            let (kill_tx, kill_rx) = tokio::sync::watch::channel(false);
            // All cells get data_streams + membrane RPC.
            // stdin/stdout semantics vary by cell type (wire protocol, CGI,
            // or shutdown signal), but the WIT membrane channel is universal.
            let mut proc_builder = ProcBuilder::new()
                .with_engine(engine)
                .with_env(env)
                .with_args(args)
                .with_wasm_debug(wasm_debug)
                .with_bytecode((*bytecode).clone())
                .with_stdio(guest_stdin, guest_stdout, guest_stderr);
            if let Some(component) = component {
                proc_builder = proc_builder.with_component(component);
            }
            if let Some(est) = fuel_estimator {
                proc_builder = proc_builder.with_fuel_estimator(est);
            }
            let (builder, mut handles) = proc_builder.with_data_streams();

            let proc = builder
                .build()
                .await
                .map_err(|err| capnp::Error::failed(err.to_string()))?;

            let (reader, writer) = handles
                .take_host_split()
                .ok_or_else(|| capnp::Error::failed("host stream missing".into()))?;

            let mut bootstrap_cap: Option<capnp::capability::Client> = None;
            let child_rpc_system = if let (Some(erx), Some(sc)) = (epoch_rx, stream_control) {
                let (rpc, guest) = graft::build_membrane_rpc(
                    reader,
                    writer,
                    network_state,
                    swarm_cmd_tx,
                    wasm_debug,
                    erx,
                    signing_key,
                    sc,
                    None, // route_registry: spawned cells don't get HTTP routes
                    runtime_client,
                    extra_caps,
                    ipfs_client,
                    http_dial,
                );
                bootstrap_cap = Some(guest.client);
                rpc
            } else {
                build_peer_rpc(reader, writer, network_state, swarm_cmd_tx, wasm_debug)
            };

            let mut kill_rx = kill_rx;
            // Spawn RPC system and stderr drain on the ambient LocalSet.
            tokio::task::spawn_local(child_rpc_system.map(|_| ()));

            tokio::task::spawn_local(async move {
                use tokio::io::AsyncBufReadExt;
                let reader = tokio::io::BufReader::new(host_stderr);
                let mut lines = reader.lines();
                while let Ok(Some(line)) = lines.next_line().await {
                    tracing::info!("{}", line);
                }
            });

            tokio::task::spawn_local(async move {
                let mut proc_run = Box::pin(proc.run());
                let mut watch_kill = true;
                let exit_code = loop {
                    if watch_kill {
                        tokio::select! {
                            result = &mut proc_run => {
                                break match result {
                                    Ok(()) => 0,
                                    Err(e) => {
                                        tracing::error!("executor: child process failed: {}", e);
                                        1
                                    }
                                };
                            }
                            changed = kill_rx.changed() => {
                                match changed {
                                    Ok(()) => {
                                        if *kill_rx.borrow() {
                                            tracing::info!("executor: child process killed");
                                            break 137; // SIGKILL convention
                                        }
                                        // Spurious wakeup/value refresh with `false`: keep waiting.
                                    }
                                    Err(_) => {
                                        // All kill handles were dropped. Stop polling kill_rx and
                                        // await natural process exit to avoid a tight ready-loop.
                                        watch_kill = false;
                                    }
                                }
                            }
                        }
                    } else {
                        break match proc_run.await {
                            Ok(()) => 0,
                            Err(e) => {
                                tracing::error!("executor: child process failed: {}", e);
                                1
                            }
                        };
                    }
                };
                tracing::info!("executor: child process exited with code {}", exit_code);
                let _ = exit_tx.send(exit_code);
            });

            let stdin =
                capnp_rpc::new_client(ByteStreamImpl::new(host_stdin, StreamMode::WriteOnly));
            let stdout =
                capnp_rpc::new_client(ByteStreamImpl::new(host_stdout, StreamMode::ReadOnly));
            let (dummy_stderr, _) = io::duplex(1);
            let stderr =
                capnp_rpc::new_client(ByteStreamImpl::new(dummy_stderr, StreamMode::ReadOnly));

            let process_impl = if let Some(cap) = bootstrap_cap {
                ProcessImpl::with_bootstrap(
                    stdin,
                    stdout,
                    stderr,
                    exit_rx,
                    cap,
                    bootstrap_schema.clone(),
                    kill_tx,
                )
            } else {
                ProcessImpl::new(stdin, stdout, stderr, exit_rx, kill_tx)
            };
            let process_client: system_capnp::process::Client = capnp_rpc::new_client(process_impl);
            results.get().set_process(process_client);

            Ok(())
        })
    }
}
