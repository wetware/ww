use anyhow::{anyhow, Result};
use std::sync::Arc;
use tokio::io::{AsyncRead, AsyncWrite};
use wasmtime::component::bindgen;
use wasmtime::component::{
    types::ComponentItem, Component, Linker, Resource, ResourceTable, ResourceType,
};
use wasmtime::StoreContextMut;
use wasmtime::{CallHook, Engine, Store};
use wasmtime_wasi::cli::{AsyncStdinStream, AsyncStdoutStream};
use wasmtime_wasi::p2::add_to_linker_async;
use wasmtime_wasi::p2::bindings::{Command as WasiCliCommand, CommandPre as WasiCliCommandPre};
use wasmtime_wasi::p2::pipe::{AsyncReadStream, AsyncWriteStream};
use wasmtime_wasi::WasiCtxBuilder;
use wasmtime_wasi::{DirPerms, FilePerms, WasiCtx, WasiCtxView, WasiView};
use wasmtime_wasi_io::streams::{DynInputStream, DynOutputStream};

use crate::Loader;

// Generate bindings from WIT file
// Resources are defined within the interface
bindgen!({
    world: "streams-world",
    path: "wit",
    with: {
        "wasi:io/streams@0.2.9.input-stream": wasmtime_wasi_io::streams::DynInputStream,
        "wasi:io/streams@0.2.9.output-stream": wasmtime_wasi_io::streams::DynOutputStream,
    },
});

// Import generated types - Connection is a Resource type alias
use exports::wetware::streams::streams::Connection;

pub const BUFFER_SIZE: usize = 1024;
const PIPE_BUFFER_SIZE: usize = 64 * 1024;

// ---------------------------------------------------------------------------
// Fuel metering
//
// Fuel is both the resource-metering unit and the cooperative preemption
// primitive.  Every YIELD_INTERVAL instructions Wasmtime suspends the guest
// and returns Poll::Pending to the Tokio LocalSet, giving other cells a turn.
//
// The fuel budget IS the scheduling quantum: larger budgets give cells higher
// effective priority (more instructions per yield cycle).  The EWMA estimator
// tracks consumed/budget ratio and sizes the budget inversely: I/O-bound
// cells get large budgets, compute-heavy cells get small ones.
//
// Two refueling paths:
//   - call_hook (ReturningFromHost): fires on every host call, EWMA adapts.
//   - epoch_deadline_callback: fires every EPOCH_TICK_MS, prevents
//     Trap::OutOfFuel for cells that don't make host calls.
// ---------------------------------------------------------------------------

use crate::sched::{INITIAL_FUEL, MAX_FUEL, MIN_FUEL, RATIO_SCALE, YIELD_INTERVAL};

/// Ratio-based EWMA fuel estimator for WASM cells.
///
/// Tracks the consumed/budget ratio via an exponentially weighted moving
/// average (α=0.3) and sizes the budget inversely: low ratio (I/O-bound)
/// → large budget, high ratio (compute-bound) → small budget.
///
/// Using the ratio instead of absolute consumed avoids a feedback loop
/// where consumed depends on budget, which would spiral to MIN_FUEL under
/// bursty workloads.
///
/// Design doc: `doc/designs/fuel-scheduling.md`
pub struct FuelEstimator {
    budget: u64,
    /// EWMA of consumed/budget ratio (fixed-point, 0..RATIO_SCALE).
    avg_ratio: u64,
    /// False until the first on_host_return observation.
    initialized: bool,
    /// Host calls observed since the last epoch tick.  The epoch callback
    /// only updates the EWMA when this is zero (genuinely compute-bound).
    /// Cells that make host calls are already handled by the call_hook,
    /// so the epoch callback just refuels without double-observing.
    host_calls_this_epoch: u32,
    /// Per-cell ceiling for the EWMA budget (default: MAX_FUEL).
    max_fuel: u64,
    /// Per-cell floor for the EWMA budget (default: MIN_FUEL).
    min_fuel: u64,
    /// Total fuel budget from a oneshot quote.  `None` = unlimited (scheduled cell).
    /// When this reaches 0 the epoch callback stops refueling and the cell traps.
    remaining_budget: Option<u64>,
}

impl FuelEstimator {
    #[must_use]
    pub fn new(initial: u64) -> Self {
        Self {
            budget: initial,
            avg_ratio: RATIO_SCALE / 2,
            initialized: false,
            host_calls_this_epoch: 0,
            max_fuel: MAX_FUEL,
            min_fuel: MIN_FUEL,
            remaining_budget: None,
        }
    }

    /// Create an estimator for a oneshot (budgeted) cell.
    ///
    /// `max_fuel`/`min_fuel`: per-epoch bounds. 0 = use system defaults.
    /// `total_budget`: total fuel credits. Cell traps when exhausted.
    #[must_use]
    pub fn new_oneshot(total_budget: u64, max_fuel: u64, min_fuel: u64) -> Self {
        let effective_max = if max_fuel > 0 {
            max_fuel.min(MAX_FUEL)
        } else {
            MAX_FUEL
        };
        let effective_min = if min_fuel > 0 {
            min_fuel.min(effective_max)
        } else {
            MIN_FUEL
        };
        Self {
            budget: INITIAL_FUEL,
            avg_ratio: RATIO_SCALE / 2,
            initialized: false,
            host_calls_this_epoch: 0,
            max_fuel: effective_max,
            min_fuel: effective_min,
            remaining_budget: Some(total_budget),
        }
    }

    /// Adjust the fuel budget at a `ReturningFromHost` boundary.
    ///
    /// `remaining` is the fuel left in the store at the moment the guest
    /// re-enters WASM after a host call.  The estimator computes the
    /// consumed/budget ratio, updates the EWMA, and returns a new budget
    /// sized inversely to the ratio.
    ///
    /// Returns the new budget to install via `store.set_fuel(...)`.
    pub fn on_host_return(&mut self, remaining: u64) -> u64 {
        let consumed = self.budget.saturating_sub(remaining);
        let ratio = (consumed * RATIO_SCALE)
            .checked_div(self.budget)
            .unwrap_or(RATIO_SCALE / 2);

        if !self.initialized {
            // Seed from first real observation — avoids cold-start bias.
            self.avg_ratio = ratio;
            self.initialized = true;
        } else {
            // EWMA α=0.3: single division for less truncation.
            self.avg_ratio = (self.avg_ratio * 7 + ratio * 3) / 10;
        }

        // Budget inversely proportional to utilization ratio.
        // ratio=0 (pure I/O) → budget=MAX_FUEL
        // ratio=1000 (pure compute) → budget=0, clamped to MIN_FUEL
        let new_budget = (self.max_fuel * (RATIO_SCALE - self.avg_ratio) / RATIO_SCALE)
            .clamp(self.min_fuel, self.max_fuel);
        self.budget = new_budget;
        new_budget
    }

    /// Returns the current budget.
    pub fn budget(&self) -> u64 {
        self.budget
    }

    /// Returns the current EWMA ratio (0..RATIO_SCALE).
    pub fn avg_ratio(&self) -> u64 {
        self.avg_ratio
    }
}

type BoxAsyncRead = Box<dyn AsyncRead + Send + Sync + Unpin + 'static>;
type BoxAsyncWrite = Box<dyn AsyncWrite + Send + Sync + Unpin + 'static>;

// Required for WASI IO to work.
pub struct ComponentRunStates {
    pub wasi_ctx: WasiCtx,
    pub resource_table: ResourceTable,
    pub loader: Option<Box<dyn Loader>>,
    // Guest-side bidirectional stream used to build WASI io/streams resources.
    pub data_stream: Option<tokio::io::DuplexStream>,
    /// Cache mode for this process. `None` means no cache (default).
    /// `Shared` shares a global pinset cache; `Isolated` gets a private one.
    /// The staging directory for IPFS content is owned by the cache mode itself:
    /// host-wide shared dir for `Shared`, per-process dir for `Isolated`.
    pub cache_mode: Option<cache::CacheMode>,
    /// Virtual filesystem tree (lazy CID-based resolution).
    /// When `Some`, the guest filesystem is backed by a CidTree
    /// instead of a preopened host directory.
    pub cid_tree: Option<std::sync::Arc<crate::vfs::CidTree>>,
    /// EWMA fuel estimator, refuels at host call boundaries.
    pub fuel_estimator: FuelEstimator,
}

// Required for WASI IO to work.
impl WasiView for ComponentRunStates {
    fn ctx(&mut self) -> WasiCtxView<'_> {
        WasiCtxView {
            ctx: &mut self.wasi_ctx,
            table: &mut self.resource_table,
        }
    }
}

// Internal connection representation that stores stream wrappers
struct ConnectionState {
    input_stream: Option<DynInputStream>,
    output_stream: Option<DynOutputStream>,
}

struct ProcInit {
    env: Vec<String>,
    args: Vec<String>,
    bytecode: Vec<u8>,
    component: Option<Arc<Component>>,
    loader: Option<Box<dyn Loader>>,
    engine: Option<Arc<Engine>>,
    stdin: BoxAsyncRead,
    stdout: BoxAsyncWrite,
    stderr: BoxAsyncWrite,
    data_streams: Option<tokio::io::DuplexStream>,
    cache_mode: Option<cache::CacheMode>,
    cid_tree: Option<std::sync::Arc<crate::vfs::CidTree>>,
    fuel_estimator: Option<FuelEstimator>,
}

/// Builder for constructing a Proc configuration
pub struct Builder {
    env: Vec<String>,
    args: Vec<String>,
    wasm_debug: bool,
    bytecode: Option<Vec<u8>>,
    component: Option<Arc<Component>>,
    loader: Option<Box<dyn Loader>>,
    engine: Option<Arc<Engine>>,
    stdin: Option<BoxAsyncRead>,
    stdout: Option<BoxAsyncWrite>,
    stderr: Option<BoxAsyncWrite>,
    data_streams: Option<tokio::io::DuplexStream>,
    cache_mode: Option<cache::CacheMode>,
    cid_tree: Option<std::sync::Arc<crate::vfs::CidTree>>,
    fuel_estimator: Option<FuelEstimator>,
}

/// Handles for accessing the host-side of data streams.
///
/// These allow the host to read from and write to the data streams
/// that are exposed to the guest via the connection resource.
pub struct DataStreamHandles {
    /// Host-side duplex stream for RPC transport.
    host_stream: Option<tokio::io::DuplexStream>,
}

impl DataStreamHandles {
    pub fn take_host_stream(&mut self) -> Option<tokio::io::DuplexStream> {
        self.host_stream.take()
    }

    pub fn take_host_split(
        &mut self,
    ) -> Option<(
        tokio::io::ReadHalf<tokio::io::DuplexStream>,
        tokio::io::WriteHalf<tokio::io::DuplexStream>,
    )> {
        self.host_stream.take().map(tokio::io::split)
    }
}

impl Builder {
    /// Create a new Proc builder
    pub fn new() -> Self {
        Self {
            env: Vec::new(),
            args: Vec::new(),
            wasm_debug: false,
            bytecode: None,
            component: None,
            loader: None,
            engine: None,
            stdin: None,
            stdout: None,
            stderr: None,
            data_streams: None,
            cache_mode: None,
            cid_tree: None,
            fuel_estimator: None,
        }
    }

    /// Set WASM debug mode
    pub fn with_wasm_debug(mut self, debug: bool) -> Self {
        self.wasm_debug = debug;
        self
    }

    /// Add environment variables
    pub fn with_env(mut self, env: Vec<String>) -> Self {
        self.env = env;
        self
    }

    /// Add command line arguments
    pub fn with_args(mut self, args: Vec<String>) -> Self {
        self.args = args;
        self
    }

    /// Provide the component bytecode
    pub fn with_bytecode(mut self, bytecode: Vec<u8>) -> Self {
        self.bytecode = Some(bytecode);
        self
    }

    /// Provide a pre-compiled component for this process.
    ///
    /// When set, `Proc::new` skips `Component::from_binary` and directly
    /// instantiates this component on the provided engine.
    pub fn with_component(mut self, component: Arc<Component>) -> Self {
        self.component = Some(component);
        self
    }

    /// Provide the optional loader used for host callbacks
    pub fn with_loader(mut self, loader: Option<Box<dyn Loader>>) -> Self {
        self.loader = loader;
        self
    }

    /// Provide a shared Wasmtime engine to reuse across processes.
    pub fn with_engine(mut self, engine: Arc<Engine>) -> Self {
        self.engine = Some(engine);
        self
    }

    /// Provide the stdin handle
    pub fn with_stdin<R>(mut self, stdin: R) -> Self
    where
        R: AsyncRead + Send + Sync + Unpin + 'static,
    {
        self.stdin = Some(Box::new(stdin));
        self
    }

    /// Provide the stdout handle
    pub fn with_stdout<W>(mut self, stdout: W) -> Self
    where
        W: AsyncWrite + Send + Sync + Unpin + 'static,
    {
        self.stdout = Some(Box::new(stdout));
        self
    }

    /// Provide the stderr handle
    pub fn with_stderr<W>(mut self, stderr: W) -> Self
    where
        W: AsyncWrite + Send + Sync + Unpin + 'static,
    {
        self.stderr = Some(Box::new(stderr));
        self
    }

    /// Convenience helper to set all stdio handles at once.
    pub fn with_stdio<R, W1, W2>(self, stdin: R, stdout: W1, stderr: W2) -> Self
    where
        R: AsyncRead + Send + Sync + Unpin + 'static,
        W1: AsyncWrite + Send + Sync + Unpin + 'static,
        W2: AsyncWrite + Send + Sync + Unpin + 'static,
    {
        self.with_stdin(stdin)
            .with_stdout(stdout)
            .with_stderr(stderr)
    }

    /// Enable bidirectional data streams for host-guest communication.
    ///
    /// This creates in-memory pipes that are exposed to the guest via
    /// a custom connection resource. Returns handles that the host can use
    /// to communicate with the guest.
    pub fn with_data_streams(mut self) -> (Self, DataStreamHandles) {
        let (host_stream, guest_stream) = tokio::io::duplex(PIPE_BUFFER_SIZE);
        let handles = DataStreamHandles {
            host_stream: Some(host_stream),
        };

        self.data_streams = Some(guest_stream);

        (self, handles)
    }

    /// Set the cache mode for this process.
    ///
    /// - `CacheMode::Shared`: shares a global pinset cache (efficient, default for trusted procs)
    /// - `CacheMode::Isolated`: private pinset, no shared state (for untrusted guests)
    pub fn with_cache(mut self, mode: cache::CacheMode) -> Self {
        self.cache_mode = Some(mode);
        self
    }

    /// Set the CidTree for virtual filesystem resolution.
    pub fn with_cid_tree(mut self, tree: std::sync::Arc<crate::vfs::CidTree>) -> Self {
        self.cid_tree = Some(tree);
        self
    }

    /// Override the default fuel estimator.
    ///
    /// When set, this estimator replaces `FuelEstimator::new(INITIAL_FUEL)` in
    /// the process store.  Used by `ExecutorImpl::spawn()` to inject oneshot
    /// budget constraints from the `FuelPolicy` schema.
    pub fn with_fuel_estimator(mut self, est: FuelEstimator) -> Self {
        self.fuel_estimator = Some(est);
        self
    }

    /// Build a Proc instance. All required parameters must be supplied first.
    pub async fn build(self) -> Result<Proc> {
        let bytecode = self
            .bytecode
            .ok_or_else(|| anyhow!("bytecode must be provided to Proc::Builder"))?;
        let stdin = self
            .stdin
            .ok_or_else(|| anyhow!("stdin handle must be provided to Proc::Builder"))?;
        let stdout = self
            .stdout
            .ok_or_else(|| anyhow!("stdout handle must be provided to Proc::Builder"))?;
        let stderr = self
            .stderr
            .ok_or_else(|| anyhow!("stderr handle must be provided to Proc::Builder"))?;

        Proc::new(ProcInit {
            env: self.env,
            args: self.args,
            bytecode,
            component: self.component,
            loader: self.loader,
            engine: self.engine,
            stdin,
            stdout,
            stderr,
            data_streams: self.data_streams,
            cache_mode: self.cache_mode,
            cid_tree: self.cid_tree,
            fuel_estimator: self.fuel_estimator,
        })
        .await
    }
}

impl Default for Builder {
    fn default() -> Self {
        Self::new()
    }
}

/// Cell process that encapsulates a WASM instance and its configuration.
///
/// Designed for per-stream instantiation - each incoming stream gets its own Proc instance.
/// This enables concurrent execution of multiple services.
pub struct Proc {
    /// Typed handle to the guest command world
    pub command: WasiCliCommand,
    /// Cell runtime store
    pub store: Store<ComponentRunStates>,
}

impl Proc {
    /// Create a new WASM process with explicit stdio handles provided by the host.
    async fn new(init: ProcInit) -> Result<Self> {
        let ProcInit {
            env,
            args,
            bytecode,
            component,
            loader,
            engine,
            stdin,
            stdout,
            stderr,
            data_streams,
            cache_mode,
            cid_tree,
            fuel_estimator,
        } = init;

        let stdin_stream = AsyncStdinStream::new(stdin);
        let stdout_stream = AsyncStdoutStream::new(BUFFER_SIZE, stdout);
        let stderr_stream = AsyncStdoutStream::new(BUFFER_SIZE, stderr);

        // Build a Wasmtime engine with two settings for cooperative scheduling:
        //   consume_fuel       — enables instruction counting; without this,
        //                        fuel methods are no-ops and the estimator is
        //                        inert.
        //   epoch_interruption — enables the epoch_deadline_callback that
        //                        refuels compute-bound cells, preventing
        //                        Trap::OutOfFuel for guests that don't make
        //                        host calls frequently enough.
        let engine = if let Some(engine) = engine {
            engine
        } else {
            Arc::new(Engine::new(&crate::engine::wasm_engine_config())?)
        };
        let mut linker = Linker::new(&engine);
        add_to_linker_async(&mut linker)?;

        // Override filesystem bindings when CidTree or cache is active.
        // CidTree mode: ALL filesystem ops resolve through the virtual tree.
        // Cache-only mode: only `/ipfs/` paths are intercepted.
        if cid_tree.is_some() || cache_mode.is_some() {
            crate::fs_intercept::override_filesystem_linker(&mut linker)?;
        }

        // Add loader host function if loader is provided
        if loader.is_some() {
            add_loader_to_linker(&mut linker)?;
        }

        // Prepare environment variables as key-value pairs
        let envs: Vec<(&str, &str)> = env.iter().filter_map(|var| var.split_once('=')).collect();

        // Wire the guest to inherit the host stdio handles.
        let mut wasi_builder = WasiCtxBuilder::new();
        wasi_builder
            .stdin(stdin_stream)
            .stdout(stdout_stream)
            .stderr(stderr_stream)
            .envs(&envs)
            .args(&args);

        // Anchor the guest's WASI filesystem at `/` so wasi-libc has a
        // starting descriptor for absolute-path resolution. The preopened
        // path is `CidTree::staging_dir()` purely as a protocol anchor:
        // `fs_intercept` overrides every open/readdir/stat call before
        // it reads from this directory and routes through
        // `CidTree::resolve_path`. The preopen's actual on-disk contents
        // are dir-listing stubs populated lazily by `fs_intercept`, not
        // the guest's view. See doc/capabilities.md.
        if let Some(ref tree) = cid_tree {
            wasi_builder
                .preopened_dir(tree.staging_dir(), "/", DirPerms::READ, FilePerms::READ)
                .map_err(|e| anyhow!("failed to preopen CidTree staging dir at /: {e}"))?;
            tracing::debug!(
                staging = %tree.staging_dir().display(),
                "Mounted CidTree staging at / (fs_intercept routes via virtual FS)"
            );
        }

        let wasi = wasi_builder.build();

        // Set up data streams if enabled
        let data_stream = if let Some(stream) = data_streams {
            add_streams_to_linker(&mut linker)?;
            Some(stream)
        } else {
            None
        };

        let state = ComponentRunStates {
            wasi_ctx: wasi,
            resource_table: ResourceTable::new(),
            loader,
            data_stream,
            cache_mode,
            cid_tree,
            fuel_estimator: fuel_estimator.unwrap_or_else(|| FuelEstimator::new(INITIAL_FUEL)),
        };

        let mut store = Store::new(&engine, state);

        // Load the initial fuel budget.  fuel_async_yield_interval controls how
        // often Wasmtime suspends the guest to poll other Tokio tasks — this is
        // independent of the EWMA budget ceiling.  A cell with MAX_FUEL still
        // yields every YIELD_INTERVAL instructions.
        store.set_fuel(INITIAL_FUEL)?;
        store.fuel_async_yield_interval(Some(YIELD_INTERVAL))?;
        tracing::trace!(budget = INITIAL_FUEL, "fuel.initial");

        // Epoch-based refueling: prevents Trap::OutOfFuel for compute-bound
        // cells.  When Engine::increment_epoch() is called (by the epoch tick
        // task in runtime.rs), this callback fires inside the Store context
        // and refuels the cell with its current EWMA-estimated budget.
        //
        // For I/O-bound cells this is a no-op (they're already refueled by
        // the call_hook below).  For compute-bound cells, this is the only
        // refueling path — without it, fuel exhaustion causes Trap::OutOfFuel.
        store.epoch_deadline_callback(|mut ctx| {
            // Read fuel level before borrowing the estimator mutably.
            let current_fuel = ctx.get_fuel().unwrap_or(0);
            let est = &mut ctx.data_mut().fuel_estimator;

            // Budget exhaustion check for oneshot cells.
            // When remaining_budget hits 0, stop refueling.  The cell traps on
            // the next instruction that would consume fuel.
            if let Some(ref mut remaining) = est.remaining_budget {
                let consumed = est.budget.saturating_sub(current_fuel);
                *remaining = remaining.saturating_sub(consumed);
                if *remaining == 0 {
                    tracing::info!(remaining_budget = 0, "fuel.budget.exhausted");
                    return Ok(wasmtime::UpdateDeadline::Continue(1));
                }
                let remaining_snap = *remaining;
                let avg = est.avg_ratio();
                tracing::debug!(
                    remaining_budget = remaining_snap,
                    consumed_this_epoch = consumed,
                    avg_ratio = avg,
                    "fuel.budget.epoch_tick"
                );
            }

            if est.host_calls_this_epoch == 0 {
                // No host calls this epoch — cell is compute-bound.
                // Observe full consumption so EWMA converges toward MIN_FUEL.
                est.on_host_return(0);
            }
            // I/O cells: the call_hook already updated the EWMA.
            // Just refuel, don't double-observe.
            est.host_calls_this_epoch = 0;
            let budget = est.budget();
            ctx.set_fuel(budget)?;
            tracing::trace!(budget, "fuel.epoch_refuel");
            Ok(wasmtime::UpdateDeadline::Continue(1))
        });
        store.set_epoch_deadline(1);

        // EWMA refueling hook: fires on every ReturningFromHost transition.
        //
        // The estimator tracks the consumed/budget ratio via EWMA and sizes
        // the budget inversely.  set_fuel() reloads the tank so the guest can
        // continue.  This hook does NOT fire on fuel-yield events (those go
        // through Poll::Pending); it fires when the guest makes a deliberate
        // host call (WASI import, etc.).
        //
        // Compute-bound cells that don't make host calls are refueled by the
        // epoch_deadline_callback above to prevent Trap::OutOfFuel.
        store.call_hook(|mut ctx, hook| {
            if matches!(hook, CallHook::ReturningFromHost) {
                let remaining = ctx.get_fuel().unwrap_or(0);
                ctx.data_mut().fuel_estimator.host_calls_this_epoch += 1;
                let new_budget = ctx.data_mut().fuel_estimator.on_host_return(remaining);
                ctx.set_fuel(new_budget)?;
                tracing::debug!(
                    new_budget,
                    remaining,
                    avg_ratio = ctx.data().fuel_estimator.avg_ratio(),
                    "fuel.refuel"
                );
            }
            Ok(())
        });

        // Instantiate it as a normal component. When no precompiled component
        // was handed in (pid0 and any spawn without a compile service), go
        // through the baked `.cwasm` cache: WW_CWASM_DIR hit = deserialize
        // (~1400x cheaper); miss or mismatch = fresh compile, same as before.
        // This is the boot-dominant compile path, so bypassing the cache here
        // forfeits the precompile win exactly where it matters (#587).
        let component = if let Some(component) = component {
            tracing::debug!("Using precompiled guest component");
            component
        } else {
            let start = std::time::Instant::now();
            let compiled = crate::cwasm::load_or_compile(
                &engine,
                &bytecode,
                crate::cwasm::cache_dir().as_deref(),
            )?;
            tracing::debug!(
                elapsed_ms = start.elapsed().as_millis(),
                "Guest component ready"
            );
            Arc::new(compiled)
        };
        let component_type = component.component_type();
        tracing::trace!(
            imports = component_type.imports(&engine).len(),
            exports = component_type.exports(&engine).len(),
            "Guest component type summary"
        );
        for (name, item) in component_type.imports(&engine) {
            tracing::trace!(name, item = ?item, "Guest component import");
            if name == "wetware:streams/streams" {
                if let ComponentItem::ComponentInstance(instance) = item {
                    for (export_name, export_item) in instance.exports(&engine) {
                        tracing::trace!(
                            name,
                            export = export_name,
                            item = ?export_item,
                            "Guest streams instance export"
                        );
                    }
                }
            }
        }
        for (name, item) in component_type.exports(&engine) {
            tracing::trace!(name, item = ?item, "Guest component export");
        }

        let pre_start = std::time::Instant::now();
        let pre_instance = linker.instantiate_pre(&component)?;
        let pre = WasiCliCommandPre::new(pre_instance)?;
        tracing::trace!(
            elapsed_ms = pre_start.elapsed().as_millis(),
            "Guest component pre-instantiated"
        );

        let start = std::time::Instant::now();
        let command = match tokio::time::timeout(
            std::time::Duration::from_secs(5),
            pre.instantiate_async(&mut store),
        )
        .await
        {
            Ok(result) => result?,
            Err(_) => {
                tracing::error!("Guest component instantiation timed out");
                return Err(anyhow!("guest component instantiation timed out"));
            }
        };
        tracing::trace!(
            elapsed_ms = start.elapsed().as_millis(),
            "Guest component instantiated"
        );

        Ok(Self { command, store })
    }

    /// Invoke the guest's `wasi:cli/run#run` export and wait for completion.
    pub async fn run(mut self) -> Result<()> {
        self.command
            .wasi_cli_run()
            .call_run(&mut self.store)
            .await
            .map_err(|e| anyhow!("failed to call `wasi:cli/run`: {e}"))?
            .map_err(|()| anyhow!("guest returned non-zero exit status"))
    }
}

/// Add the streams interface to the Wasmtime linker
///
/// This exports the wetware:streams interface, allowing guests to create
/// connection resources and access bidirectional data streams.
fn add_streams_to_linker(linker: &mut Linker<ComponentRunStates>) -> Result<()> {
    let mut streams_instance = linker.instance("wetware:streams/streams@0.1.0")?;

    streams_instance.resource(
        "connection",
        ResourceType::host::<ConnectionState>(),
        |_, _| Ok(()),
    )?;

    streams_instance.func_wrap_async(
        "create-connection",
        |mut store: StoreContextMut<'_, ComponentRunStates>, (): ()| {
            Box::new(async move {
                tracing::debug!("streams#create-connection invoked");
                let state = store.data_mut();
                let guest_stream = state
                    .data_stream
                    .take()
                    .ok_or_else(|| wasmtime::Error::msg("data streams not enabled"))?;

                let (guest_read, guest_write) = tokio::io::split(guest_stream);
                let input_stream: DynInputStream = Box::new(AsyncReadStream::new(guest_read));
                let output_stream: DynOutputStream =
                    Box::new(AsyncWriteStream::new(PIPE_BUFFER_SIZE, guest_write));

                let conn_state = ConnectionState {
                    input_stream: Some(input_stream),
                    output_stream: Some(output_stream),
                };

                let conn_resource = state.resource_table.push(conn_state)?;
                let connection = Connection::try_from_resource(conn_resource, &mut store)?;
                tracing::debug!("streams#create-connection: connection ready");
                Ok((connection,))
            })
        },
    )?;

    streams_instance.func_wrap_async(
        "[method]connection.get-input-stream",
        |mut store: StoreContextMut<'_, ComponentRunStates>,
         (connection,): (Resource<ConnectionState>,)| {
            Box::new(async move {
                tracing::debug!("streams#connection.get-input-stream invoked");
                let stream = {
                    let conn_state = store.data_mut().resource_table.get_mut(&connection)?;
                    conn_state
                        .input_stream
                        .take()
                        .ok_or_else(|| wasmtime::Error::msg("input stream already taken"))?
                };

                let state = store.data_mut();
                let resource = state.resource_table.push(stream)?;
                tracing::debug!("streams#connection.get-input-stream: resource ready");
                Ok((resource,))
            })
        },
    )?;

    streams_instance.func_wrap_async(
        "[method]connection.get-output-stream",
        |mut store: StoreContextMut<'_, ComponentRunStates>,
         (connection,): (Resource<ConnectionState>,)| {
            Box::new(async move {
                tracing::debug!("streams#connection.get-output-stream invoked");
                let stream = {
                    let conn_state = store.data_mut().resource_table.get_mut(&connection)?;
                    conn_state
                        .output_stream
                        .take()
                        .ok_or_else(|| wasmtime::Error::msg("output stream already taken"))?
                };

                let state = store.data_mut();
                let resource = state.resource_table.push(stream)?;
                tracing::debug!("streams#connection.get-output-stream: resource ready");
                Ok((resource,))
            })
        },
    )?;

    Ok(())
}

/// Add the loader host function to the Wasmtime linker
///
/// This exports a host function that allows WASM guests to call back into
/// the host to load bytecode from various sources (IPFS, filesystem, etc.).
///
/// Note: This requires a WIT interface definition. For now, this is a
/// placeholder that can be implemented once the WIT interface is defined.
fn add_loader_to_linker<T>(_linker: &mut Linker<T>) -> Result<()> {
    // TODO: Implement using WIT interface
    // The WIT interface would look something like:
    //
    // package wetware:loader;
    //
    // interface loader {
    //   load: func(path: string) -> result<list<u8>, string>;
    // }
    //
    // world wetware {
    //   import loader: self.loader;
    // }
    //
    // Then we'd use wit-bindgen to generate bindings and implement:
    // linker.root().func_wrap_async("wetware:loader/loader", "load", |mut store, (path,): (String,)| async move {
    //     let state = store.data_mut();
    //     if let Some(ref loader) = state.loader {
    //         match loader.load(&path).await {
    //             Ok(data) => Ok((data,)),
    //             Err(e) => Err(e.to_string()),
    //         }
    //     } else {
    //         Err("Loader not available".to_string())
    //     }
    // })?;

    // For now, this is a no-op placeholder
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    #[test]
    fn test_proc_builder_creation() {
        let builder = Builder::new();
        assert!(!builder.wasm_debug);
        assert!(builder.env.is_empty());
        assert!(builder.args.is_empty());
    }

    #[test]
    fn test_proc_builder() {
        let builder = Builder::new()
            .with_wasm_debug(true)
            .with_env(vec!["TEST=1".to_string()])
            .with_args(vec!["arg1".to_string()]);

        assert!(builder.wasm_debug);
        assert_eq!(builder.env.len(), 1);
        assert_eq!(builder.args.len(), 1);
    }

    #[tokio::test]
    async fn test_data_stream_handles_full_duplex() {
        // Enable data streams and capture the returned handles
        let (mut builder, mut handles) = Builder::new().with_data_streams();

        let guest_stream = builder
            .data_streams
            .take()
            .expect("data streams should be configured");
        let host_stream = handles
            .take_host_stream()
            .expect("host stream should be configured");

        let (mut host_read, mut host_write) = tokio::io::split(host_stream);
        let (mut guest_read, mut guest_write) = tokio::io::split(guest_stream);

        host_write.write_all(b"ping").await.unwrap();
        let mut buf = [0u8; 4];
        guest_read.read_exact(&mut buf).await.unwrap();
        assert_eq!(&buf, b"ping");

        guest_write.write_all(b"pong").await.unwrap();
        let mut buf = [0u8; 4];
        host_read.read_exact(&mut buf).await.unwrap();
        assert_eq!(&buf, b"pong");
    }

    #[tokio::test]
    async fn test_builder_fails_without_bytecode() {
        let err = Builder::new()
            .with_stdio(tokio::io::empty(), tokio::io::sink(), tokio::io::sink())
            .build()
            .await
            .err()
            .expect("should fail");
        assert!(err.to_string().contains("bytecode"));
    }

    #[tokio::test]
    async fn test_builder_fails_without_stdin() {
        let err = Builder::new()
            .with_bytecode(vec![0])
            .with_stdout(tokio::io::sink())
            .with_stderr(tokio::io::sink())
            .build()
            .await
            .err()
            .expect("should fail");
        assert!(err.to_string().contains("stdin"));
    }

    #[tokio::test]
    async fn test_builder_fails_without_stdout() {
        let err = Builder::new()
            .with_bytecode(vec![0])
            .with_stdin(tokio::io::empty())
            .with_stderr(tokio::io::sink())
            .build()
            .await
            .err()
            .expect("should fail");
        assert!(err.to_string().contains("stdout"));
    }

    #[tokio::test]
    async fn test_builder_fails_without_stderr() {
        let err = Builder::new()
            .with_bytecode(vec![0])
            .with_stdin(tokio::io::empty())
            .with_stdout(tokio::io::sink())
            .build()
            .await
            .err()
            .expect("should fail");
        assert!(err.to_string().contains("stderr"));
    }

    #[tokio::test]
    async fn test_data_stream_handles_take_host_split() {
        let (_builder, mut handles) = Builder::new().with_data_streams();

        let split = handles.take_host_split();
        assert!(split.is_some());

        // Second take returns None
        let split2 = handles.take_host_split();
        assert!(split2.is_none());
    }

    #[test]
    fn test_builder_default() {
        let builder = Builder::default();
        assert!(!builder.wasm_debug);
        assert!(builder.bytecode.is_none());
        assert!(builder.engine.is_none());
        assert!(builder.loader.is_none());
    }

    // =========================================================================
    // FuelEstimator EWMA tests
    // =========================================================================

    #[test]
    fn fuel_estimator_seeds_from_first_observation() {
        let mut est = FuelEstimator::new(1_000_000);
        assert!(!est.initialized);
        // First call: consumed = 100K of 1M → ratio = 100
        est.on_host_return(900_000);
        assert!(est.initialized);
        assert_eq!(est.avg_ratio(), 100); // seeded directly, not blended
    }

    #[test]
    fn fuel_estimator_ewma_blends_after_first() {
        let mut est = FuelEstimator::new(1_000_000);
        // Seed: ratio = 100 (10% utilization)
        est.on_host_return(900_000);
        assert_eq!(est.avg_ratio(), 100);
        // Second call: same ratio. EWMA: (100*7 + 100*3) / 10 = 100
        let budget = est.budget();
        let remaining = budget - (budget * 100 / RATIO_SCALE);
        est.on_host_return(remaining);
        assert_eq!(est.avg_ratio(), 100);
    }

    #[test]
    fn fuel_estimator_io_bound_converges_to_max() {
        let mut est = FuelEstimator::new(1_000_000);
        // Repeatedly consume 0 fuel — pure I/O proxy
        for _ in 0..50 {
            let budget = est.budget();
            est.on_host_return(budget); // consumed = 0
        }
        // Ratio → 0, budget → MAX_FUEL
        assert!(
            est.avg_ratio() < 5,
            "ratio should be near 0, got {}",
            est.avg_ratio()
        );
        assert_eq!(est.budget(), MAX_FUEL);
    }

    #[test]
    fn fuel_estimator_compute_bound_converges_to_min() {
        let mut est = FuelEstimator::new(1_000_000);
        // Repeatedly consume all fuel
        for _ in 0..50 {
            est.on_host_return(0); // consumed = budget
        }
        // Ratio → 1000, budget → MIN_FUEL
        assert!(
            est.avg_ratio() > 990,
            "ratio should be near 1000, got {}",
            est.avg_ratio()
        );
        assert_eq!(est.budget(), MIN_FUEL);
    }

    #[test]
    fn fuel_estimator_bursty_no_spiral() {
        let mut est = FuelEstimator::new(1_000_000);
        // Alternate: I/O round (consumed=0) and compute round (consumed=budget)
        for _ in 0..100 {
            let budget = est.budget();
            est.on_host_return(budget); // I/O: consumed = 0
            est.on_host_return(0); // Compute: consumed = budget
        }
        // Ratio should stabilize around 500 (alternating 0 and 1000).
        // Budget should NOT spiral to MIN_FUEL.
        let ratio = est.avg_ratio();
        assert!(
            (400..600).contains(&ratio),
            "bursty ratio should be ~500, got {}",
            ratio
        );
        assert!(
            est.budget() > MIN_FUEL * 10,
            "budget should not spiral to MIN_FUEL, got {}",
            est.budget()
        );
    }

    #[test]
    fn fuel_estimator_clamps_to_min() {
        let mut est = FuelEstimator::new(MIN_FUEL);
        // All fuel consumed → ratio = 1000 → budget clamped to MIN_FUEL
        est.on_host_return(0);
        assert_eq!(est.budget(), MIN_FUEL);
    }

    #[test]
    fn fuel_estimator_clamps_to_max() {
        let mut est = FuelEstimator::new(MAX_FUEL);
        // Zero consumed → ratio = 0 → budget = MAX_FUEL
        est.on_host_return(MAX_FUEL);
        assert_eq!(est.budget(), MAX_FUEL);
    }

    #[test]
    fn fuel_estimator_zero_budget_defaults_ratio() {
        let mut est = FuelEstimator::new(0);
        // Budget is 0 — ratio defaults to 500
        est.on_host_return(0);
        assert_eq!(est.avg_ratio(), RATIO_SCALE / 2);
    }

    #[test]
    fn fuel_estimator_workload_shift_converges() {
        let mut est = FuelEstimator::new(1_000_000);
        // Start as I/O-bound (ratio ~100)
        for _ in 0..20 {
            let budget = est.budget();
            let remaining = budget - (budget / 10); // 10% utilization
            est.on_host_return(remaining);
        }
        let io_ratio = est.avg_ratio();
        assert!(io_ratio < 150, "I/O ratio should be <150, got {}", io_ratio);

        // Shift to compute-heavy (ratio ~900)
        for _ in 0..20 {
            let budget = est.budget();
            let remaining = budget / 10; // 90% utilization
            est.on_host_return(remaining);
        }
        let compute_ratio = est.avg_ratio();
        assert!(
            compute_ratio > 800,
            "compute ratio should be >800, got {}",
            compute_ratio
        );
    }

    // =========================================================================
    // FuelEstimator oneshot / fuel-policy tests
    // =========================================================================

    #[test]
    fn fuel_estimator_new_oneshot_basic() {
        let est = FuelEstimator::new_oneshot(5_000_000, 0, 0);
        assert_eq!(est.remaining_budget, Some(5_000_000));
        assert_eq!(est.max_fuel, MAX_FUEL);
        assert_eq!(est.min_fuel, MIN_FUEL);
    }

    #[test]
    fn fuel_estimator_new_oneshot_custom_bounds() {
        let est = FuelEstimator::new_oneshot(5_000_000, 5_000_000, 50_000);
        assert_eq!(est.max_fuel, 5_000_000);
        assert_eq!(est.min_fuel, 50_000);
    }

    #[test]
    fn fuel_estimator_new_oneshot_max_clamped() {
        // maxPerEpoch > MAX_FUEL should be clamped
        let est = FuelEstimator::new_oneshot(5_000_000, 100_000_000, 0);
        assert_eq!(est.max_fuel, MAX_FUEL);
    }

    #[test]
    fn fuel_estimator_default_unchanged() {
        // Verify new() still produces the same behavior
        let est = FuelEstimator::new(INITIAL_FUEL);
        assert_eq!(est.remaining_budget, None);
        assert_eq!(est.max_fuel, MAX_FUEL);
        assert_eq!(est.min_fuel, MIN_FUEL);
    }

    #[test]
    fn fuel_estimator_oneshot_uses_custom_bounds() {
        let mut est = FuelEstimator::new_oneshot(5_000_000, 5_000_000, 50_000);
        // After observing high utilization, budget should clamp to custom min, not global MIN_FUEL
        for _ in 0..20 {
            est.on_host_return(0); // simulate 100% consumption
        }
        assert!(
            est.budget() >= 50_000,
            "should clamp to custom min_fuel, got {}",
            est.budget()
        );
        assert!(
            est.budget() <= 5_000_000,
            "should clamp to custom max_fuel, got {}",
            est.budget()
        );
    }
}
