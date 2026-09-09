use anyhow::{anyhow, Result};
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{ready, Context, Poll};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::sync::{Mutex, OwnedMutexGuard};
use wasmtime::component::{Component, HasSelf, Linker, ResourceTable};
use wasmtime::{CallHook, Engine, Store};
use wasmtime_wasi::cli::{AsyncStdinStream, IsTerminal, StdoutStream};
use wasmtime_wasi::p3::bindings::{Command as WasiCliCommand, CommandPre as WasiCliCommandPre};
use wasmtime_wasi::WasiCtxBuilder;
use wasmtime_wasi::{FsPerms, WasiCtx, WasiCtxView, WasiView};

mod pid0_runtime {
    wasmtime::component::bindgen!({
        world: "pid0",
        path: "../../std/kernel/wit",
        with: {
            "wasi": wasmtime_wasi::p3::bindings,
        },
    });
}

mod routing_key_runtime {
    wasmtime::component::bindgen!({
        world: "key-client",
        path: "../guest/routing-key/wit",
    });
}

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
    /// Host calls observed since the last epoch tick. Cells that make linked
    /// host calls are already handled by the call hook, so the epoch callback
    /// refuels them without double-observing. An unmarked epoch still records
    /// measured consumption because Component Model builtins do not mark
    /// `HostCallFrames`.
    host_calls_this_epoch: u32,
    /// Per-cell ceiling for the EWMA budget (default: MAX_FUEL).
    max_fuel: u64,
    /// Per-cell floor for the EWMA budget (default: MIN_FUEL).
    min_fuel: u64,
    /// Total fuel budget from a oneshot quote.  `None` = unlimited (scheduled cell).
    /// When this reaches 0 the epoch callback stops refueling and the cell traps.
    remaining_budget: Option<u64>,
}

/// One observation from the production epoch fuel callback.
///
/// This type supports production-path integration tests. Recording is disabled
/// unless a caller explicitly installs a [`FuelObserver`] on the process.
#[doc(hidden)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct FuelEpochObservation {
    pub current_fuel: u64,
    pub measured_consumption: u64,
    pub host_calls_this_epoch: u32,
    pub budget: u64,
    pub avg_ratio: u64,
}

/// Opt-in observer for production epoch fuel callbacks.
#[doc(hidden)]
#[derive(Clone, Default)]
pub struct FuelObserver {
    observations: Arc<std::sync::Mutex<Vec<FuelEpochObservation>>>,
}

impl FuelObserver {
    fn record(&self, observation: FuelEpochObservation) {
        self.observations
            .lock()
            .expect("fuel observer lock poisoned")
            .push(observation);
    }

    pub fn observations(&self) -> Vec<FuelEpochObservation> {
        self.observations
            .lock()
            .expect("fuel observer lock poisoned")
            .clone()
    }
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

    /// Record one epoch boundary and return the next fuel budget.
    ///
    /// Linked host calls already record their observation in the call hook.
    /// Otherwise, use the Store's actual remaining fuel. This distinguishes
    /// low-consumption Component Model I/O from genuinely compute-bound work.
    fn on_epoch_tick(&mut self, remaining: u64) -> u64 {
        if self.host_calls_this_epoch == 0 {
            self.on_host_return(remaining);
        }
        self.host_calls_this_epoch = 0;
        self.budget
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

enum SharedWriterState {
    Ready(Arc<Mutex<BoxAsyncWrite>>),
    Locking(Pin<Box<dyn Future<Output = OwnedMutexGuard<BoxAsyncWrite>> + Send + Sync + 'static>>),
    Locked(OwnedMutexGuard<BoxAsyncWrite>),
    Closed,
}

struct SharedWriter {
    state: SharedWriterState,
}

impl SharedWriter {
    fn new(writer: Arc<Mutex<BoxAsyncWrite>>) -> Self {
        Self {
            state: SharedWriterState::Ready(writer),
        }
    }

    fn poll_lock(&mut self, cx: &mut Context<'_>) -> Poll<()> {
        loop {
            match &mut self.state {
                SharedWriterState::Ready(writer) => {
                    self.state = SharedWriterState::Locking(Box::pin(writer.clone().lock_owned()));
                }
                SharedWriterState::Locking(lock) => match lock.as_mut().poll(cx) {
                    Poll::Ready(guard) => self.state = SharedWriterState::Locked(guard),
                    Poll::Pending => return Poll::Pending,
                },
                SharedWriterState::Locked(_) => return Poll::Ready(()),
                SharedWriterState::Closed => unreachable!("shared writer entered a closed state"),
            }
        }
    }

    fn with_guard<T>(
        &mut self,
        operation: impl FnOnce(Pin<&mut BoxAsyncWrite>) -> Poll<std::io::Result<T>>,
    ) -> Poll<std::io::Result<T>> {
        let SharedWriterState::Locked(mut guard) =
            std::mem::replace(&mut self.state, SharedWriterState::Closed)
        else {
            unreachable!("shared writer operation requires its lock")
        };
        let result = operation(Pin::new(&mut *guard));
        if result.is_ready() {
            let writer = OwnedMutexGuard::mutex(&guard).clone();
            drop(guard);
            self.state = SharedWriterState::Ready(writer);
        } else {
            self.state = SharedWriterState::Locked(guard);
        }
        result
    }
}

impl AsyncWrite for SharedWriter {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        bytes: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        ready!(self.poll_lock(cx));
        self.with_guard(|mut writer| writer.as_mut().poll_write(cx, bytes))
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        ready!(self.poll_lock(cx));
        self.with_guard(|mut writer| writer.as_mut().poll_flush(cx))
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        ready!(self.poll_lock(cx));
        self.with_guard(|mut writer| writer.as_mut().poll_shutdown(cx))
    }
}

/// P3 stdio writes directly to the configured host writer.
///
/// Wasmtime 48's buffered `AsyncStdoutStream` reports its queued flush before
/// the background writer completes it. A finite P3 command can then lose its
/// final bytes when Store teardown aborts that writer. This adapter serializes
/// independent P3 handles and preserves `AsyncWrite::poll_flush` completion.
struct FlushGatedStdoutStream {
    writer: Arc<Mutex<BoxAsyncWrite>>,
}

impl FlushGatedStdoutStream {
    fn new(writer: BoxAsyncWrite) -> Self {
        Self {
            writer: Arc::new(Mutex::new(writer)),
        }
    }
}

impl IsTerminal for FlushGatedStdoutStream {
    fn is_terminal(&self) -> bool {
        false
    }
}

impl StdoutStream for FlushGatedStdoutStream {
    fn async_stream(&self) -> Box<dyn AsyncWrite + Send + Sync> {
        Box::new(SharedWriter::new(self.writer.clone()))
    }
}

#[derive(Default)]
pub(crate) struct HostCallFrames {
    frames: Vec<bool>,
}

impl HostCallFrames {
    pub(crate) fn mark(&mut self) {
        if let Some(frame) = self.frames.last_mut() {
            *frame = true;
        }
    }

    pub(crate) fn on_call_hook(&mut self, hook: CallHook) -> bool {
        match hook {
            CallHook::CallingHost => {
                self.frames.push(false);
                false
            }
            CallHook::ReturningFromHost => self.frames.pop().unwrap_or(false),
            CallHook::CallingWasm | CallHook::ReturningFromWasm => false,
        }
    }

    #[cfg(test)]
    pub(crate) fn is_empty(&self) -> bool {
        self.frames.is_empty()
    }

    #[cfg(test)]
    pub(crate) fn depth(&self) -> usize {
        self.frames.len()
    }
}

// Required for WASI IO to work.
pub struct ComponentRunStates {
    pub wasi_ctx: WasiCtx,
    pub resource_table: ResourceTable,
    /// Private empty root used by byte-loaded Executor children.
    ///
    /// A CidTree-backed cell uses the tree staging directory instead. Keeping
    /// this TempDir in the store makes the preopen valid for the process
    /// lifetime without exposing any host filesystem path.
    pub image_root: Option<tempfile::TempDir>,
    /// Process-private writable scratch preopened at `/tmp`.
    pub scratch: tempfile::TempDir,
    /// Single-use, capability-granted P3 transport endpoint.
    pub(crate) granted_transport: crate::p3::GrantedTransport,
    /// Cache mode for this process. `None` means no cache (default).
    /// `Shared` shares a global pinset cache; `Isolated` gets a private one.
    /// The staging directory for IPFS content is owned by the cache mode itself:
    /// host-wide shared dir for `Shared`, per-process dir for `Isolated`.
    pub cache_mode: Option<Arc<cache::CacheMode>>,
    /// Virtual filesystem tree (lazy CID-based resolution).
    /// When `Some`, the guest filesystem is backed by a CidTree
    /// instead of a preopened host directory.
    pub cid_tree: Option<std::sync::Arc<crate::vfs::CidTree>>,
    /// Descriptor identities rooted at the private writable `/tmp` preopen.
    ///
    /// The CidTree interceptor uses this execution-context state to delegate
    /// scratch operations to WASI without making the image root writable.
    pub(crate) writable_fs_descriptors: std::collections::HashSet<u32>,
    /// EWMA fuel estimator, refuels at host call boundaries.
    pub fuel_estimator: FuelEstimator,
    /// Opt-in production callback observations used by integration tests.
    fuel_observer: Option<FuelObserver>,
    /// Real-import markers paired with live `CallingHost` hook frames.
    ///
    /// P3 may have more than one outstanding host transition. A marker on each
    /// frame keeps nested transitions from collapsing into one observation.
    host_call_frames: HostCallFrames,
    /// Present only for the trusted PID0 process. Ordinary child linkers omit
    /// the corresponding WIT import entirely.
    pub kernel_ready_gate: Option<Arc<authority::KernelReadyGate>>,
}

impl ComponentRunStates {
    pub(crate) fn mark_host_call(&mut self) {
        self.host_call_frames.mark();
    }
}

impl pid0_runtime::wetware::kernel_runtime::readiness::Host for ComponentRunStates {
    fn kernel_ready(
        &mut self,
    ) -> Result<(), pid0_runtime::wetware::kernel_runtime::readiness::ReadyError> {
        self.mark_host_call();
        let gate = self
            .kernel_ready_gate
            .as_ref()
            .expect("kernel readiness import installed without gate");
        commit_kernel_ready(gate)
    }
}

impl routing_key_runtime::wetware::routing::key::Host for ComponentRunStates {
    fn derive(&mut self, data: Vec<u8>) -> String {
        self.mark_host_call();
        crate::routing_key::derive(&data).to_string()
    }
}

fn commit_kernel_ready(
    gate: &authority::KernelReadyGate,
) -> Result<(), pid0_runtime::wetware::kernel_runtime::readiness::ReadyError> {
    use authority::KernelReadyError;
    use pid0_runtime::wetware::kernel_runtime::readiness::ReadyError;

    match gate.kernel_ready() {
        Ok(()) => Ok(()),
        Err(KernelReadyError::StaleGeneration | KernelReadyError::NotBound) => {
            Err(ReadyError::StaleGeneration)
        }
    }
}

fn add_routing_key_to_linker(linker: &mut Linker<ComponentRunStates>) -> Result<()> {
    routing_key_runtime::KeyClient::add_to_linker::<ComponentRunStates, HasSelf<ComponentRunStates>>(
        linker,
        |state| state,
    )?;
    Ok(())
}

fn add_pid0_readiness_to_linker(linker: &mut Linker<ComponentRunStates>) -> Result<()> {
    pid0_runtime::wetware::kernel_runtime::readiness::add_to_linker::<
        ComponentRunStates,
        HasSelf<ComponentRunStates>,
    >(linker, |state| state)?;
    Ok(())
}

// Required for WASI IO to work.
impl WasiView for ComponentRunStates {
    fn ctx(&mut self) -> WasiCtxView<'_> {
        self.mark_host_call();
        WasiCtxView {
            ctx: &mut self.wasi_ctx,
            table: &mut self.resource_table,
        }
    }
}

impl crate::p3::TransportHostState for ComponentRunStates {
    fn granted_transport(&mut self) -> &mut crate::p3::GrantedTransport {
        self.mark_host_call();
        &mut self.granted_transport
    }
}

struct ProcInit {
    env: Vec<String>,
    args: Vec<String>,
    program: Program,
    engine: Option<Arc<Engine>>,
    stdin: BoxAsyncRead,
    stdout: BoxAsyncWrite,
    stderr: BoxAsyncWrite,
    granted_transport: crate::p3::GrantedTransport,
    cache_mode: Option<cache::CacheMode>,
    mode: ConstructionMode,
    fuel_estimator: Option<FuelEstimator>,
    fuel_observer: Option<FuelObserver>,
}

enum ConstructionMode {
    Ordinary,
    Kernel {
        root: Arc<crate::vfs::CidTree>,
        readiness_gate: Arc<authority::KernelReadyGate>,
    },
}

struct ProcessFilesystem {
    image_root: Option<tempfile::TempDir>,
    scratch: tempfile::TempDir,
}

fn configure_process_filesystem(
    wasi_builder: &mut WasiCtxBuilder,
    cid_tree: Option<&Arc<crate::vfs::CidTree>>,
) -> Result<ProcessFilesystem> {
    let image_root = if let Some(tree) = cid_tree {
        wasi_builder
            .preopened_dir(tree.staging_dir(), "/", FsPerms::ReadOnly)
            .map_err(|e| anyhow!("failed to preopen CidTree staging dir at /: {e}"))?;
        tracing::debug!(
            staging = %tree.staging_dir().display(),
            "Mounted CidTree staging at / (fs_intercept routes via virtual FS)"
        );
        None
    } else {
        let root = tempfile::TempDir::new()
            .map_err(|e| anyhow!("failed to create private process image root: {e}"))?;
        wasi_builder
            .preopened_dir(root.path(), "/", FsPerms::ReadOnly)
            .map_err(|e| anyhow!("failed to preopen private image root at /: {e}"))?;
        Some(root)
    };

    let scratch = tempfile::TempDir::new()
        .map_err(|e| anyhow!("failed to create private process scratch: {e}"))?;
    wasi_builder
        .preopened_dir(scratch.path(), "/tmp", FsPerms::ReadWrite)
        .map_err(|e| anyhow!("failed to preopen private process scratch at /tmp: {e}"))?;

    Ok(ProcessFilesystem {
        image_root,
        scratch,
    })
}

/// Link the production P3 authority surface without registering sockets.
fn add_p3_wasi_to_linker(linker: &mut Linker<ComponentRunStates>) -> Result<()> {
    use wasmtime_wasi::cli::{WasiCli, WasiCliView};
    use wasmtime_wasi::clocks::{WasiClocks, WasiClocksView};
    use wasmtime_wasi::filesystem::{WasiFilesystem, WasiFilesystemView};
    use wasmtime_wasi::p3::bindings::{cli, clocks, filesystem, random};
    use wasmtime_wasi::random::{WasiRandom, WasiRandomView};

    let cli = <ComponentRunStates as WasiCliView>::cli;
    cli::exit::add_to_linker::<_, WasiCli>(linker, cli)?;
    cli::environment::add_to_linker::<_, WasiCli>(linker, cli)?;
    cli::stdin::add_to_linker::<_, WasiCli>(linker, cli)?;
    cli::stdout::add_to_linker::<_, WasiCli>(linker, cli)?;
    cli::stderr::add_to_linker::<_, WasiCli>(linker, cli)?;
    // Rust's `IsTerminal` implementation imports these query interfaces.
    // Ordinary Cells receive no terminal resources from their `WasiCtx`.
    cli::terminal_input::add_to_linker::<_, WasiCli>(linker, cli)?;
    cli::terminal_output::add_to_linker::<_, WasiCli>(linker, cli)?;
    cli::terminal_stdin::add_to_linker::<_, WasiCli>(linker, cli)?;
    cli::terminal_stdout::add_to_linker::<_, WasiCli>(linker, cli)?;
    cli::terminal_stderr::add_to_linker::<_, WasiCli>(linker, cli)?;

    let clocks = <ComponentRunStates as WasiClocksView>::clocks;
    clocks::types::add_to_linker::<_, WasiClocks>(linker, clocks)?;
    clocks::monotonic_clock::add_to_linker::<_, WasiClocks>(linker, clocks)?;
    clocks::system_clock::add_to_linker::<_, WasiClocks>(linker, clocks)?;

    let filesystem = <ComponentRunStates as WasiFilesystemView>::filesystem;
    filesystem::types::add_to_linker::<_, WasiFilesystem>(linker, filesystem)?;
    filesystem::preopens::add_to_linker::<_, WasiFilesystem>(linker, filesystem)?;

    let random = <ComponentRunStates as WasiRandomView>::random;
    random::random::add_to_linker::<_, WasiRandom>(linker, random)?;
    random::insecure_seed::add_to_linker::<_, WasiRandom>(linker, random)?;
    Ok(())
}

fn install_host_return_fuel_hook(store: &mut Store<ComponentRunStates>) {
    store.call_hook(|mut ctx, hook| {
        if ctx.data_mut().host_call_frames.on_call_hook(hook) {
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
}

/// A native P3 program supplied as bytes or compiled for the builder's engine.
pub enum Program {
    Bytes(Vec<u8>),
    Precompiled(Arc<Component>),
}

/// Builder for constructing one Cell's host-side process representation.
///
/// Use [`Builder::ordinary`] for an ordinary child with a private root. Use
/// [`Builder::kernel`] for a kernel Cell with an authoritative `CidTree` root
/// and the private readiness import. Both modes install one capability-granted
/// `wetware:transport` endpoint.
pub struct Builder {
    env: Vec<String>,
    args: Vec<String>,
    wasm_debug: bool,
    program: Program,
    engine: Option<Arc<Engine>>,
    stdin: BoxAsyncRead,
    stdout: BoxAsyncWrite,
    stderr: BoxAsyncWrite,
    granted_transport: crate::p3::GrantedTransport,
    cache_mode: Option<cache::CacheMode>,
    mode: ConstructionMode,
    fuel_estimator: Option<FuelEstimator>,
    fuel_observer: Option<FuelObserver>,
}

/// Handle for the host side of the capability-granted transport.
pub struct DataStreamHandles {
    host_transport: Option<crate::p3::HostTransport>,
}

impl DataStreamHandles {
    pub fn take_host_stream(&mut self) -> Option<crate::p3::HostTransport> {
        self.host_transport.take()
    }

    pub fn take_host_split(
        &mut self,
    ) -> Option<(
        tokio::io::ReadHalf<crate::p3::HostTransport>,
        tokio::io::WriteHalf<crate::p3::HostTransport>,
    )> {
        self.host_transport.take().map(tokio::io::split)
    }
}

impl Builder {
    /// Construct an ordinary Cell with a private empty root.
    pub fn ordinary<R, W1, W2>(
        program: Program,
        stdin: R,
        stdout: W1,
        stderr: W2,
    ) -> (Self, DataStreamHandles)
    where
        R: AsyncRead + Send + Sync + Unpin + 'static,
        W1: AsyncWrite + Send + Sync + Unpin + 'static,
        W2: AsyncWrite + Send + Sync + Unpin + 'static,
    {
        Self::new(program, stdin, stdout, stderr, ConstructionMode::Ordinary)
    }

    /// Construct a kernel Cell with an authoritative root and readiness import.
    pub fn kernel<R, W1, W2>(
        program: Program,
        stdin: R,
        stdout: W1,
        stderr: W2,
        root: Arc<crate::vfs::CidTree>,
        readiness_gate: Arc<authority::KernelReadyGate>,
    ) -> (Self, DataStreamHandles)
    where
        R: AsyncRead + Send + Sync + Unpin + 'static,
        W1: AsyncWrite + Send + Sync + Unpin + 'static,
        W2: AsyncWrite + Send + Sync + Unpin + 'static,
    {
        Self::new(
            program,
            stdin,
            stdout,
            stderr,
            ConstructionMode::Kernel {
                root,
                readiness_gate,
            },
        )
    }

    fn new<R, W1, W2>(
        program: Program,
        stdin: R,
        stdout: W1,
        stderr: W2,
        mode: ConstructionMode,
    ) -> (Self, DataStreamHandles)
    where
        R: AsyncRead + Send + Sync + Unpin + 'static,
        W1: AsyncWrite + Send + Sync + Unpin + 'static,
        W2: AsyncWrite + Send + Sync + Unpin + 'static,
    {
        let (host_transport, granted_transport) = crate::p3::HostTransport::bounded_pair();
        let handles = DataStreamHandles {
            host_transport: Some(host_transport),
        };
        let builder = Self {
            env: Vec::new(),
            args: Vec::new(),
            wasm_debug: false,
            program,
            engine: None,
            stdin: Box::new(stdin),
            stdout: Box::new(stdout),
            stderr: Box::new(stderr),
            granted_transport,
            cache_mode: None,
            mode,
            fuel_estimator: None,
            fuel_observer: None,
        };
        (builder, handles)
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

    /// Provide a shared Wasmtime engine to reuse across processes.
    pub fn with_engine(mut self, engine: Arc<Engine>) -> Self {
        self.engine = Some(engine);
        self
    }

    /// Set the cache mode for this process.
    ///
    /// - `CacheMode::Shared`: shares a global pinset cache (efficient, default for trusted procs)
    /// - `CacheMode::Isolated`: private pinset, no shared state (for untrusted guests)
    pub fn with_cache(mut self, mode: cache::CacheMode) -> Self {
        self.cache_mode = Some(mode);
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

    /// Install opt-in observation of the production epoch fuel callback.
    #[doc(hidden)]
    pub fn with_fuel_observer(mut self, observer: FuelObserver) -> Self {
        self.fuel_observer = Some(observer);
        self
    }

    /// Build the host-side process representation.
    pub async fn build(self) -> Result<Proc> {
        Proc::new(ProcInit {
            env: self.env,
            args: self.args,
            program: self.program,
            engine: self.engine,
            stdin: self.stdin,
            stdout: self.stdout,
            stderr: self.stderr,
            granted_transport: self.granted_transport,
            cache_mode: self.cache_mode,
            mode: self.mode,
            fuel_estimator: self.fuel_estimator,
            fuel_observer: self.fuel_observer,
        })
        .await
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
            program,
            engine,
            stdin,
            stdout,
            stderr,
            granted_transport,
            cache_mode,
            mode,
            fuel_estimator,
            fuel_observer,
        } = init;
        let cache_mode = cache_mode.map(Arc::new);
        let (cid_tree, kernel_ready_gate) = match mode {
            ConstructionMode::Ordinary => (None, None),
            ConstructionMode::Kernel {
                root,
                readiness_gate,
            } => (Some(root), Some(readiness_gate)),
        };
        let stdin_stream = AsyncStdinStream::new(stdin);
        let stdout_stream = FlushGatedStdoutStream::new(stdout);
        let stderr_stream = FlushGatedStdoutStream::new(stderr);

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
            Arc::new(crate::engine::wasm_engine()?)
        };
        let mut linker = Linker::new(&engine);
        add_p3_wasi_to_linker(&mut linker)?;
        crate::p3::add_transport_to_linker(&mut linker)?;
        add_routing_key_to_linker(&mut linker)?;
        if kernel_ready_gate.is_some() {
            add_pid0_readiness_to_linker(&mut linker)?;
        }

        // Override filesystem bindings when CidTree or cache is active.
        // CidTree mode: ALL filesystem ops resolve through the virtual tree.
        // Cache-only mode: only `/ipfs/` paths are intercepted.
        if cid_tree.is_some() || cache_mode.is_some() {
            crate::fs_intercept::override_p3_filesystem_linker(&mut linker)?;
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
        let filesystem = configure_process_filesystem(&mut wasi_builder, cid_tree.as_ref())?;

        let wasi = wasi_builder.build();

        let state = ComponentRunStates {
            wasi_ctx: wasi,
            resource_table: ResourceTable::new(),
            image_root: filesystem.image_root,
            scratch: filesystem.scratch,
            granted_transport,
            cache_mode,
            cid_tree,
            writable_fs_descriptors: std::collections::HashSet::new(),
            fuel_estimator: fuel_estimator.unwrap_or_else(|| FuelEstimator::new(INITIAL_FUEL)),
            fuel_observer,
            host_call_frames: HostCallFrames::default(),
            kernel_ready_gate,
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

            let measured_consumption = est.budget.saturating_sub(current_fuel);
            let host_calls_this_epoch = est.host_calls_this_epoch;
            let budget = est.on_epoch_tick(current_fuel);
            let avg_ratio = est.avg_ratio();
            ctx.set_fuel(budget)?;
            if let Some(observer) = ctx.data().fuel_observer.as_ref() {
                observer.record(FuelEpochObservation {
                    current_fuel,
                    measured_consumption,
                    host_calls_this_epoch,
                    budget,
                    avg_ratio,
                });
            }
            tracing::trace!(
                budget,
                current_fuel,
                measured_consumption,
                host_calls_this_epoch,
                avg_ratio,
                "fuel.epoch_refuel"
            );
            Ok(wasmtime::UpdateDeadline::Continue(1))
        });
        store.set_epoch_deadline(1);

        // EWMA refueling hook: observes linked guest imports when they return.
        //
        // The estimator tracks the consumed/budget ratio via EWMA and sizes
        // the budget inversely.  set_fuel() reloads the tank so the guest can
        // continue. Wasmtime 48 also emits these hooks for internal libcalls
        // and fuel/epoch yields. Linked imports mark the Store state from
        // their host implementation; the hook ignores unmarked transitions.
        //
        // Compute-bound cells that don't make host calls are refueled by the
        // epoch_deadline_callback above to prevent Trap::OutOfFuel.
        install_host_return_fuel_hook(&mut store);

        // Instantiate it as a normal component. The canonical engine has
        // Wasmtime's optional persistent cache configured, so this path stays
        // portable while reusing host-local compiled code when available.
        let component = match program {
            Program::Precompiled(component) => {
                tracing::debug!("Using precompiled guest component");
                component
            }
            Program::Bytes(bytecode) => {
                let start = std::time::Instant::now();
                let compiled = crate::engine::compile_component(&engine, &bytecode)?;
                tracing::debug!(
                    elapsed_ms = start.elapsed().as_millis(),
                    "Guest component ready"
                );
                Arc::new(compiled)
            }
        };
        let component_type = component.component_type();
        tracing::trace!(
            imports = component_type.imports(&engine).len(),
            exports = component_type.exports(&engine).len(),
            "Guest component type summary"
        );
        for (name, item) in component_type.imports(&engine) {
            tracing::trace!(name, item = ?item, "Guest component import");
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
        let command = self.command;
        let result = self
            .store
            .run_concurrent(async move |access| command.wasi_cli_run().call_run(access).await)
            .await
            .map_err(|error| anyhow!("P3 command event loop failed: {error}"))?
            .map_err(|error| anyhow!("failed to call `wasi:cli/run`: {error}"))?;
        result.map_err(|()| anyhow!("guest returned non-zero exit status"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, Ordering};
    use tokio::io::AsyncReadExt;

    struct PendingFlushWriter {
        bytes: Arc<std::sync::Mutex<Vec<u8>>>,
        flush_open: Arc<AtomicBool>,
        flush_waker: Arc<std::sync::Mutex<Option<std::task::Waker>>>,
    }

    struct FailingTransportReader;

    impl AsyncRead for FailingTransportReader {
        fn poll_read(
            self: Pin<&mut Self>,
            _cx: &mut Context<'_>,
            _buffer: &mut tokio::io::ReadBuf<'_>,
        ) -> Poll<std::io::Result<()>> {
            Poll::Ready(Err(std::io::Error::new(
                std::io::ErrorKind::ConnectionReset,
                "injected transport read failure",
            )))
        }
    }

    struct FailingTransportWriter;

    impl AsyncWrite for FailingTransportWriter {
        fn poll_write(
            self: Pin<&mut Self>,
            _cx: &mut Context<'_>,
            _bytes: &[u8],
        ) -> Poll<std::io::Result<usize>> {
            Poll::Ready(Err(std::io::Error::new(
                std::io::ErrorKind::BrokenPipe,
                "injected transport write failure",
            )))
        }

        fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
            Poll::Ready(Err(std::io::Error::new(
                std::io::ErrorKind::BrokenPipe,
                "injected transport flush failure",
            )))
        }

        fn poll_shutdown(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
            Poll::Ready(Ok(()))
        }
    }

    impl AsyncWrite for PendingFlushWriter {
        fn poll_write(
            self: Pin<&mut Self>,
            _cx: &mut Context<'_>,
            bytes: &[u8],
        ) -> Poll<std::io::Result<usize>> {
            self.bytes
                .lock()
                .expect("pending-flush byte lock")
                .extend_from_slice(bytes);
            Poll::Ready(Ok(bytes.len()))
        }

        fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
            if self.flush_open.load(Ordering::Acquire) {
                Poll::Ready(Ok(()))
            } else {
                *self.flush_waker.lock().expect("pending-flush waker lock") =
                    Some(cx.waker().clone());
                Poll::Pending
            }
        }

        fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
            self.poll_flush(cx)
        }
    }

    const ROUTING_KEY_PROBE_COMPONENT: &str = r#"
        (component
          (type $key-type
            (instance
              (type $derive-type
                (func (param "data" (list u8)) (result string)))
              (export "derive" (func (type $derive-type)))))
          (import "wetware:routing/key@0.1.0"
            (instance $key (type $key-type)))
          (alias export $key "derive" (func $derive))

          (core module $libc
            (memory (export "memory") 1)
            (global $last (mut i32) (i32.const 4096))
            (func $realloc (export "realloc")
              (param $old-ptr i32)
              (param $old-size i32)
              (param $align i32)
              (param $new-size i32)
              (result i32)
              (local $ret i32)

              local.get $old-ptr
              if unreachable end

              (global.set $last
                (i32.and
                  (i32.add
                    (global.get $last)
                    (i32.add (local.get $align) (i32.const -1)))
                  (i32.xor
                    (i32.add (local.get $align) (i32.const -1))
                    (i32.const -1))))
              global.get $last
              local.set $ret
              (global.set $last
                (i32.add (global.get $last) (local.get $new-size)))
              local.get $ret))
          (core instance $libc (instantiate $libc))

          (core func $derive-lowered
            (canon lower (func $derive)
              (memory $libc "memory")
              (realloc (func $libc "realloc"))))

          (core module $probe
            (import "libc" "memory" (memory 1))
            (import "" "derive" (func $derive (param i32 i32 i32)))
            (data (i32.const 0) "ww.chess.v1")
            (func (export "probe") (result i32)
              i32.const 0
              i32.const 11
              i32.const 96
              call $derive
              i32.const 96))
          (core instance $probe
            (instantiate $probe
              (with "libc" (instance $libc))
              (with "" (instance
                (export "derive" (func $derive-lowered))))))

          (func (export "probe") (result string)
            (canon lift (core func $probe "probe")
              (memory $libc "memory")
              (realloc (func $libc "realloc")))))
    "#;

    const P3_NOOP_COMMAND: &str = r#"
        (component
          (core func $task-return (canon task.return (result (result))))
          (core module $noop
            (import "" "task-return" (func $task-return (param i32)))
            (func (export "run")
              i32.const 0
              call $task-return))
          (core instance $instance
            (instantiate $noop
              (with "" (instance
                (export "task-return" (func $task-return))))))
          (func $run async (result (result))
            (canon lift (core func $instance "run") async))
          (instance (export (interface "wasi:cli/run@0.3.0"))
            (export "run" (func $run))))
    "#;

    struct EpochTicker {
        stop: Arc<AtomicBool>,
        thread: Option<std::thread::JoinHandle<()>>,
    }

    impl EpochTicker {
        fn start(engine: Arc<Engine>) -> Self {
            let stop = Arc::new(AtomicBool::new(false));
            let thread_stop = Arc::clone(&stop);
            let thread = std::thread::spawn(move || {
                while !thread_stop.load(Ordering::Acquire) {
                    std::thread::sleep(std::time::Duration::from_millis(
                        crate::sched::EPOCH_TICK_MS,
                    ));
                    if !thread_stop.load(Ordering::Acquire) {
                        engine.increment_epoch();
                    }
                }
            });
            Self {
                stop,
                thread: Some(thread),
            }
        }
    }

    impl Drop for EpochTicker {
        fn drop(&mut self) {
            self.stop.store(true, Ordering::Release);
            if let Some(thread) = self.thread.take() {
                thread.join().expect("epoch ticker thread");
            }
        }
    }

    #[tokio::test]
    async fn p3_stdio_flush_waits_for_the_underlying_host_writer() {
        use tokio::io::AsyncWriteExt;

        let bytes = Arc::new(std::sync::Mutex::new(Vec::new()));
        let flush_open = Arc::new(AtomicBool::new(false));
        let flush_waker = Arc::new(std::sync::Mutex::new(None));
        let stdout = FlushGatedStdoutStream::new(Box::new(PendingFlushWriter {
            bytes: bytes.clone(),
            flush_open: flush_open.clone(),
            flush_waker: flush_waker.clone(),
        }));
        let mut writer = Box::into_pin(stdout.async_stream());

        writer.as_mut().write_all(b"final response").await.unwrap();
        assert!(
            tokio::time::timeout(
                std::time::Duration::from_millis(20),
                writer.as_mut().flush()
            )
            .await
            .is_err(),
            "P3 stdio flush completed before the host writer"
        );

        flush_open.store(true, Ordering::Release);
        flush_waker
            .lock()
            .expect("pending-flush waker lock")
            .take()
            .expect("pending flush registered no waker")
            .wake();
        writer.as_mut().flush().await.unwrap();
        assert_eq!(
            bytes.lock().expect("pending-flush byte lock").as_slice(),
            b"final response"
        );
    }

    fn component_test_state() -> ComponentRunStates {
        let (_host, granted_transport) = crate::p3::HostTransport::bounded_pair();
        ComponentRunStates {
            wasi_ctx: WasiCtxBuilder::new().build(),
            resource_table: ResourceTable::new(),
            image_root: None,
            scratch: tempfile::TempDir::new().expect("component test scratch"),
            granted_transport,
            cache_mode: None,
            cid_tree: None,
            writable_fs_descriptors: std::collections::HashSet::new(),
            fuel_estimator: FuelEstimator::new(INITIAL_FUEL),
            fuel_observer: None,
            host_call_frames: HostCallFrames::default(),
            kernel_ready_gate: None,
        }
    }

    async fn linked_routing_key_probe_bytes(
        bytes: &[u8],
    ) -> (
        Store<ComponentRunStates>,
        wasmtime::component::TypedFunc<(), (String,)>,
    ) {
        let engine = crate::engine::wasm_engine().expect("component engine");
        let component = Component::new(&engine, bytes).expect("routing-key probe component");
        let mut linker = Linker::new(&engine);
        add_p3_wasi_to_linker(&mut linker).expect("install P3 WASI imports");
        add_routing_key_to_linker(&mut linker).expect("install routing-key import");
        let mut store = Store::new(&engine, component_test_state());
        store.set_fuel(INITIAL_FUEL).expect("routing-key test fuel");
        store
            .fuel_async_yield_interval(Some(YIELD_INTERVAL))
            .expect("routing-key yield interval");
        store.set_epoch_deadline(1);
        install_host_return_fuel_hook(&mut store);
        let instance = linker
            .instantiate_async(&mut store, &component)
            .await
            .expect("instantiate routing-key probe");
        let probe = instance
            .get_typed_func::<(), (String,)>(&mut store, "probe")
            .expect("typed routing-key probe export");
        (store, probe)
    }

    async fn linked_routing_key_probe() -> (
        Store<ComponentRunStates>,
        wasmtime::component::TypedFunc<(), (String,)>,
    ) {
        linked_routing_key_probe_bytes(ROUTING_KEY_PROBE_COMPONENT.as_bytes()).await
    }

    #[tokio::test]
    async fn routing_key_wit_call_matches_golden_vector_and_is_deterministic() {
        let (mut store, probe) = linked_routing_key_probe().await;

        for _ in 0..2 {
            let (key,) = probe
                .call_async(&mut store, ())
                .await
                .expect("call routing-key probe");
            assert_eq!(
                key,
                "bafkr4ifcoue3f52zpzpz2xei7dqhs3gajm326llyljbwisxkwea7hbowyy"
            );
        }
    }

    #[tokio::test]
    async fn compiled_routing_key_guest_wrapper_matches_golden_vector() {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        let artifact =
            root.join("target/routing-key-probe/wasm32-wasip3/release/routing_key_probe.wasm");
        if !artifact.is_file() {
            let status = std::process::Command::new("make")
                .current_dir(&root)
                .arg("routing-key-probe")
                .status()
                .expect("launch routing-key-probe build");
            assert!(status.success(), "routing-key-probe build failed");
        }
        let bytes = std::fs::read(&artifact)
            .unwrap_or_else(|error| panic!("read {}: {error}", artifact.display()));
        let (mut store, probe) = linked_routing_key_probe_bytes(&bytes).await;

        for _ in 0..2 {
            let (key,) = probe
                .call_async(&mut store, ())
                .await
                .expect("call compiled routing-key probe");
            assert_eq!(
                key,
                "bafkr4ifcoue3f52zpzpz2xei7dqhs3gajm326llyljbwisxkwea7hbowyy"
            );
        }
    }

    #[tokio::test]
    async fn real_p3_transport_failure_reaches_the_cell_root() {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        let artifact =
            root.join("target/authority-probe/wasm32-wasip3/release/authority_probe.wasm");
        assert!(
            artifact.is_file(),
            "authority-probe artifact missing; run `make authority-probe`: {}",
            artifact.display()
        );
        let bytes = std::fs::read(&artifact)
            .unwrap_or_else(|error| panic!("read {}: {error}", artifact.display()));
        let engine = Arc::new(crate::engine::wasm_engine().expect("component engine"));
        let _ticker = EpochTicker::start(Arc::clone(&engine));
        let (stderr_read, stderr_write) = tokio::io::duplex(64 * 1024);
        let (mut builder, _handles) = Builder::ordinary(
            Program::Bytes(bytes),
            tokio::io::empty(),
            tokio::io::sink(),
            stderr_write,
        );
        builder.granted_transport =
            crate::p3::GrantedTransport::from_parts(FailingTransportReader, FailingTransportWriter);
        let proc = builder
            .with_engine(engine)
            .with_args(vec!["authority-probe".into(), "enumerate".into()])
            .build()
            .await
            .expect("build transport-failure P3 probe");

        let run_error = tokio::time::timeout(std::time::Duration::from_secs(5), proc.run())
            .await
            .expect("transport-failure P3 probe timed out")
            .expect_err("transport failure must fail the Cell root");
        assert!(
            run_error
                .to_string()
                .contains("guest returned non-zero exit status"),
            "unexpected Cell root error: {run_error:#}"
        );

        let mut stderr = Vec::new();
        let mut stderr_read = stderr_read;
        stderr_read
            .read_to_end(&mut stderr)
            .await
            .expect("read transport-failure stderr");
        let stderr = String::from_utf8(stderr).expect("transport-failure stderr UTF-8");
        assert!(
            stderr.contains("P3 transport failed: transport read failed")
                || stderr.contains("P3 transport failed: transport write failed"),
            "guest did not report the transport completion error: {stderr:?}"
        );
    }

    #[tokio::test]
    async fn linked_production_host_import_marks_and_refuels_once() {
        let (mut store, probe) = linked_routing_key_probe().await;

        probe
            .call_async(&mut store, ())
            .await
            .expect("call linked production routing-key import");

        assert!(store.data().fuel_estimator.initialized);
        assert_eq!(store.data().fuel_estimator.host_calls_this_epoch, 1);
        assert!(store.data().host_call_frames.is_empty());
    }

    #[test]
    fn concurrent_host_call_frames_do_not_collapse_markers() {
        let mut state = component_test_state();

        assert!(!state.host_call_frames.on_call_hook(CallHook::CallingHost));
        state.mark_host_call();
        assert!(!state.host_call_frames.on_call_hook(CallHook::CallingHost));
        state.mark_host_call();
        assert!(state
            .host_call_frames
            .on_call_hook(CallHook::ReturningFromHost));
        assert!(state
            .host_call_frames
            .on_call_hook(CallHook::ReturningFromHost));
        assert!(state.host_call_frames.is_empty());
    }

    #[test]
    fn unmarked_nested_host_frame_cannot_consume_outer_marker() {
        let mut state = component_test_state();

        assert!(!state.host_call_frames.on_call_hook(CallHook::CallingHost));
        state.mark_host_call();
        assert!(!state.host_call_frames.on_call_hook(CallHook::CallingHost));
        assert!(!state
            .host_call_frames
            .on_call_hook(CallHook::ReturningFromHost));
        assert!(state
            .host_call_frames
            .on_call_hook(CallHook::ReturningFromHost));
        assert!(state.host_call_frames.is_empty());
    }

    #[tokio::test]
    async fn internal_fuel_yields_do_not_update_host_call_ewma() {
        let engine = crate::engine::wasm_engine().expect("component engine");
        let module = wasmtime::Module::new(
            &engine,
            r#"
                (module
                    (func (export "spin")
                        (loop $spin
                            br $spin)))
            "#,
        )
        .expect("spinning core module");
        let mut store = Store::new(&engine, component_test_state());
        store.set_fuel(35).expect("spinning test fuel");
        store
            .fuel_async_yield_interval(Some(10))
            .expect("spinning test yield interval");
        install_host_return_fuel_hook(&mut store);
        let instance = wasmtime::Linker::new(&engine)
            .instantiate_async(&mut store, &module)
            .await
            .expect("instantiate spinning core module");
        let spin = instance
            .get_typed_func::<(), ()>(&mut store, "spin")
            .expect("spin export");

        spin.call_async(&mut store, ())
            .await
            .expect_err("spinning core module must exhaust fuel");
        assert!(!store.data().fuel_estimator.initialized);
        assert_eq!(store.data().fuel_estimator.host_calls_this_epoch, 0);
    }

    #[test]
    fn routing_key_registration_does_not_require_a_guest_import() {
        let engine = crate::engine::wasm_engine().expect("component engine");
        let component = Component::new(&engine, "(component)").expect("empty component");
        assert!(component
            .component_type()
            .imports(&engine)
            .all(|(name, _)| name != "wetware:routing/key@0.1.0"));

        let mut linker = Linker::new(&engine);
        add_routing_key_to_linker(&mut linker).expect("install routing-key import");
        linker
            .instantiate_pre(&component)
            .expect("extra routing-key host support must not affect a non-importing component");
    }

    #[test]
    fn routing_key_import_is_registered_for_ordinary_and_pid0_linkers() {
        let engine = crate::engine::wasm_engine().expect("component engine");
        let component = Component::new(&engine, ROUTING_KEY_PROBE_COMPONENT)
            .expect("routing-key probe component");

        let mut ordinary = Linker::new(&engine);
        add_routing_key_to_linker(&mut ordinary).expect("ordinary routing-key import");
        ordinary
            .instantiate_pre(&component)
            .expect("ordinary linker satisfies routing-key import");

        let mut pid0 = Linker::new(&engine);
        add_routing_key_to_linker(&mut pid0).expect("PID0 routing-key import");
        add_pid0_readiness_to_linker(&mut pid0).expect("install private PID0 import");
        pid0.instantiate_pre(&component)
            .expect("PID0 linker satisfies routing-key import");
    }

    fn private_kernel_import_component() -> Vec<u8> {
        use wit_component::{ComponentEncoder, StringEncoding};
        use wit_parser::{ManglingAndAbi, Resolve};

        let mut resolve = Resolve::default();
        let wit = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../std/kernel/wit");
        let (package, _) = resolve.push_dir(wit).expect("parse private kernel WIT");
        let world = resolve
            .select_world(&[package], Some("pid0"))
            .expect("select private PID0 world");
        let mut module = wit_component::dummy_module(&resolve, world, ManglingAndAbi::Standard32);
        wit_component::embed_component_metadata(&mut module, &resolve, world, StringEncoding::UTF8)
            .expect("embed private PID0 component metadata");
        ComponentEncoder::default()
            .module(&module)
            .expect("encode private PID0 core module")
            .validate(true)
            .encode()
            .expect("encode private PID0 component")
    }

    #[test]
    fn private_kernel_import_is_absent_from_ordinary_child_linker() {
        let engine = crate::engine::wasm_engine().expect("component engine");
        let component = Component::from_binary(&engine, &private_kernel_import_component())
            .expect("private import component");

        let mut ordinary = Linker::<ComponentRunStates>::new(&engine);
        add_p3_wasi_to_linker(&mut ordinary).expect("install ordinary P3 WASI imports");
        let error = match ordinary.instantiate_pre(&component) {
            Ok(_) => panic!("ordinary child linker must not satisfy private PID0 import"),
            Err(error) => error,
        };
        assert!(
            error
                .to_string()
                .contains("wetware:kernel-runtime/readiness@1.0.0"),
            "unexpected missing-import error: {error}"
        );

        let mut pid0 = Linker::<ComponentRunStates>::new(&engine);
        add_p3_wasi_to_linker(&mut pid0).expect("install PID0 P3 WASI imports");
        add_pid0_readiness_to_linker(&mut pid0).expect("install private PID0 import");
        pid0.instantiate_pre(&component)
            .expect("PID0 linker satisfies private readiness import");
    }

    #[test]
    fn private_kernel_import_maps_gate_results_fail_closed() {
        use authority::Epoch;
        fn epoch(seq: u64) -> Epoch {
            Epoch {
                seq,
                head: Vec::new(),
                root: None,
            }
        }

        let (epoch_tx, epoch_rx) = tokio::sync::watch::channel(epoch(1));
        let gate = authority::KernelReadyGate::new(epoch_rx);

        assert!(matches!(
            commit_kernel_ready(&gate),
            Err(pid0_runtime::wetware::kernel_runtime::readiness::ReadyError::StaleGeneration)
        ));
        assert!(!gate.is_ready());
        gate.bind_generation(1);
        assert_eq!(commit_kernel_ready(&gate), Ok(()));
        assert!(gate.is_ready());

        epoch_tx.send_replace(epoch(2));
        assert!(matches!(
            commit_kernel_ready(&gate),
            Err(pid0_runtime::wetware::kernel_runtime::readiness::ReadyError::StaleGeneration)
        ));
        assert!(!gate.is_ready());
    }

    #[test]
    fn ordinary_builder_has_explicit_required_mechanisms() {
        let (builder, _handles) = Builder::ordinary(
            Program::Bytes(vec![0]),
            tokio::io::empty(),
            tokio::io::sink(),
            tokio::io::sink(),
        );
        assert!(!builder.wasm_debug);
        assert!(builder.env.is_empty());
        assert!(builder.args.is_empty());
        assert!(matches!(builder.mode, ConstructionMode::Ordinary));
    }

    #[test]
    fn builder_sets_optional_execution_configuration() {
        let (builder, _handles) = Builder::ordinary(
            Program::Bytes(vec![0]),
            tokio::io::empty(),
            tokio::io::sink(),
            tokio::io::sink(),
        );
        let builder = builder
            .with_wasm_debug(true)
            .with_env(vec!["TEST=1".to_string()])
            .with_args(vec!["arg1".to_string()]);

        assert!(builder.wasm_debug);
        assert_eq!(builder.env.len(), 1);
        assert_eq!(builder.args.len(), 1);
    }

    #[test]
    fn byte_loaded_filesystem_uses_private_empty_root_and_cleans_scratch() {
        let mut wasi = WasiCtxBuilder::new();
        let filesystem =
            configure_process_filesystem(&mut wasi, None).expect("configure byte-loaded roots");
        let root = filesystem
            .image_root
            .as_ref()
            .expect("byte-loaded process owns an empty image root");
        assert!(
            std::fs::read_dir(root.path())
                .expect("read private root")
                .next()
                .is_none(),
            "byte-loaded image root must contain no host or FHS files"
        );
        let root_path = root.path().to_path_buf();
        let scratch_path = filesystem.scratch.path().to_path_buf();
        assert_ne!(root_path, scratch_path);
        std::fs::write(scratch_path.join("marker"), b"private").expect("write scratch marker");

        drop(filesystem);
        assert!(!root_path.exists(), "private empty root must be cleaned up");
        assert!(
            !scratch_path.exists(),
            "private /tmp must be cleaned up on process-state drop"
        );
    }

    #[test]
    fn image_backed_filesystem_retains_cid_tree_root_and_private_scratch() {
        let staging = tempfile::tempdir().expect("image staging");
        let root_cid = "bafkreibm6jg3ux5quy7flfgn5gmxk5ubm6yur3apcu3to3d6tmjzptm2ye";
        let tree = Arc::new(crate::vfs::CidTree::new(
            root_cid.to_string(),
            ipfs::HttpClient::new("http://127.0.0.1:1".to_string()),
            staging.path().to_path_buf(),
        ));
        let mut wasi = WasiCtxBuilder::new();
        let filesystem = configure_process_filesystem(&mut wasi, Some(&tree))
            .expect("configure image-backed roots");

        assert!(
            filesystem.image_root.is_none(),
            "image-backed cells must not be replaced by the byte-loaded empty root"
        );
        assert_eq!(tree.root_cid().as_str(), root_cid);
        assert_eq!(tree.staging_dir(), staging.path());
        assert_ne!(filesystem.scratch.path(), tree.staging_dir());
    }

    #[tokio::test]
    async fn test_data_stream_handles_take_host_split() {
        let (_builder, mut handles) = Builder::ordinary(
            Program::Bytes(vec![0]),
            tokio::io::empty(),
            tokio::io::sink(),
            tokio::io::sink(),
        );

        let split = handles.take_host_split();
        assert!(split.is_some());

        // Second take returns None
        let split2 = handles.take_host_split();
        assert!(split2.is_none());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn busy_looping_component_can_be_aborted_within_a_bound() {
        let bytecode = wat::parse_str(include_str!(
            "../../../tests/fixtures/spinning-component.wat"
        ))
        .expect("parse P3 spinning component fixture");
        let engine = Arc::new(crate::engine::wasm_engine().expect("component engine"));
        let (builder, _handles) = Builder::ordinary(
            Program::Bytes(bytecode),
            tokio::io::empty(),
            tokio::io::sink(),
            tokio::io::sink(),
        );
        let mut proc = builder
            .with_engine(Arc::clone(&engine))
            .build()
            .await
            .expect("build P3 spinning component");

        // Remove fuel-based cooperative yields so task cancellation can only
        // become observable after Wasmtime handles an engine epoch deadline.
        proc.store
            .fuel_async_yield_interval(None)
            .expect("disable fuel-based yields");
        proc.store
            .set_fuel(u64::MAX)
            .expect("set non-exhausting test fuel");
        let epoch_observed = Arc::new(AtomicBool::new(false));
        let callback_observed = Arc::clone(&epoch_observed);
        proc.store.epoch_deadline_callback(move |_context| {
            callback_observed.store(true, Ordering::Release);
            Ok(wasmtime::UpdateDeadline::Yield(1))
        });
        proc.store.set_epoch_deadline(1);

        let mut proc_task = tokio::spawn(async move { proc.run().await });
        let _ticker = EpochTicker::start(Arc::clone(&engine));
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            while !epoch_observed.load(Ordering::Acquire) {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("P3 spinning guest did not reach an epoch interruption");

        assert!(
            !proc_task.is_finished(),
            "P3 spinning guest exited before cancellation"
        );
        let started = std::time::Instant::now();
        proc_task.abort();
        let error = tokio::time::timeout(std::time::Duration::from_secs(5), &mut proc_task)
            .await
            .expect("P3 compute-bound cancellation exceeded timeout")
            .expect_err("aborted P3 process task must return JoinError");
        assert!(error.is_cancelled());
        assert!(started.elapsed() < std::time::Duration::from_secs(5));

        let noop = wat::parse_str(P3_NOOP_COMMAND).expect("parse P3 no-op component");
        let (builder, _handles) = Builder::ordinary(
            Program::Bytes(noop),
            tokio::io::empty(),
            tokio::io::sink(),
            tokio::io::sink(),
        );
        let healthy_proc = builder
            .with_engine(Arc::clone(&engine))
            .build()
            .await
            .expect("build second P3 component on shared Engine");
        tokio::time::timeout(std::time::Duration::from_secs(5), healthy_proc.run())
            .await
            .expect("second P3 component did not complete")
            .expect("second P3 component failed");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn production_config_busy_loop_can_be_aborted_and_shared_engine_remains_healthy() {
        let bytecode = wat::parse_str(include_str!(
            "../../../tests/fixtures/spinning-component.wat"
        ))
        .expect("parse P3 spinning component fixture");
        let engine = Arc::new(crate::engine::wasm_engine().expect("component engine"));
        // Production starts the shared Engine ticker before it admits Cells.
        let _ticker = EpochTicker::start(Arc::clone(&engine));
        let fuel_observer = FuelObserver::default();
        let (builder, _handles) = Builder::ordinary(
            Program::Bytes(bytecode),
            tokio::io::empty(),
            tokio::io::sink(),
            tokio::io::sink(),
        );
        let proc = builder
            .with_engine(Arc::clone(&engine))
            .with_fuel_observer(fuel_observer.clone())
            .build()
            .await
            .expect("build production-config P3 spinning component");

        let mut proc_task = tokio::spawn(async move { proc.run().await });
        // Epoch callbacks run only while the Store executes the export. Two
        // observations prove that the non-terminating guest remained active
        // across production Engine ticks and exercised `Continue(1)`.
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            while fuel_observer.observations().len() < 2 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("production epoch callback did not observe the compute-bound guest");
        if proc_task.is_finished() {
            let result = (&mut proc_task)
                .await
                .expect("production-config P3 task panicked");
            panic!("production-config P3 spinning guest exited before cancellation: {result:?}");
        }

        let started = std::time::Instant::now();
        proc_task.abort();
        let error = tokio::time::timeout(std::time::Duration::from_secs(5), &mut proc_task)
            .await
            .expect("production-config P3 cancellation exceeded timeout")
            .expect_err("aborted production-config P3 task must return JoinError");
        assert!(error.is_cancelled());
        assert!(started.elapsed() < std::time::Duration::from_secs(5));

        let noop = wat::parse_str(P3_NOOP_COMMAND).expect("parse P3 no-op component");
        let (builder, _handles) = Builder::ordinary(
            Program::Bytes(noop),
            tokio::io::empty(),
            tokio::io::sink(),
            tokio::io::sink(),
        );
        let healthy_proc = builder
            .with_engine(Arc::clone(&engine))
            .build()
            .await
            .expect("build healthy P3 component on shared Engine");
        tokio::time::timeout(std::time::Duration::from_secs(5), healthy_proc.run())
            .await
            .expect("healthy P3 component did not complete")
            .expect("healthy P3 component failed");
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
    fn unmarked_low_consumption_epochs_retain_a_high_budget() {
        let mut est = FuelEstimator::new(INITIAL_FUEL);
        let mut trajectory = Vec::with_capacity(60);

        for _ in 0..60 {
            let remaining = est.budget().saturating_sub(1_000);
            trajectory.push(est.on_epoch_tick(remaining));
        }

        assert!(
            trajectory.iter().all(|budget| *budget >= MAX_FUEL * 9 / 10),
            "low-consumption epoch trajectory decayed: {trajectory:?}"
        );
        assert_eq!(trajectory[0], 9_990_000);
        assert_eq!(trajectory[1], MAX_FUEL);
        assert_eq!(trajectory[59], MAX_FUEL);
        assert!(
            est.avg_ratio() < 5,
            "low-consumption epoch ratio must stay near zero: {}",
            est.avg_ratio()
        );
    }

    #[test]
    fn unmarked_compute_bound_epochs_converge_to_minimum_budget() {
        let mut est = FuelEstimator::new(INITIAL_FUEL);

        for _ in 0..60 {
            est.on_epoch_tick(0);
        }

        assert!(est.avg_ratio() > 990);
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
