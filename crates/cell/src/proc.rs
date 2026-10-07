use anyhow::{anyhow, Result};
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{ready, Context, Poll};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::sync::{Mutex, OwnedMutexGuard};
use wasmtime::component::{Component, HasSelf, Linker, ResourceTable};
#[cfg(test)]
use wasmtime::Engine;
use wasmtime::{AsContextMut, CallHook, CallHookHandler, Store, StoreContextMut};
use wasmtime_wasi::cli::{AsyncStdinStream, IsTerminal, StdoutStream};
use wasmtime_wasi::p3::bindings::CommandPre as WasiCliCommandPre;
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
// Fuel is both the resource-metering unit and a cooperative preemption input.
// `fuel_async_yield_interval` requests periodic async yields. Intermediate
// yields shorten the next interval to the grant's remaining authority.
//
// The EWMA quantum is the desired scheduling grant. Larger quanta give cells
// more instructions between accounting boundaries. The estimator tracks the
// consumed/installed ratio and sizes the next quantum inversely.
//
// Fuel accounting runs only from call hooks where Cranelift has flushed its
// function-local fuel counter. The epoch callback records a notification for
// the next flushed hook.
//
// One-shot Cells clamp every grant by cumulative total and per-epoch
// authority. Scheduled-cell exhaustion and yielding remain separate concerns.
// ---------------------------------------------------------------------------

use crate::sched::{INITIAL_FUEL, MAX_FUEL, MIN_FUEL, RATIO_SCALE, YIELD_INTERVAL, YIELD_RESERVE};

/// Ratio-based EWMA fuel estimator for WASM cells.
///
/// Tracks the consumed/installed ratio via an exponentially weighted moving
/// average (α=0.3) and sizes the quantum inversely: low ratio (I/O-bound)
/// → large quantum, high ratio (compute-bound) → small quantum.
///
/// Using the ratio instead of absolute consumed avoids a feedback loop
/// where consumed depends on the installed grant, which would spiral to
/// MIN_FUEL under
/// bursty workloads.
///
/// Design doc: `doc/designs/fuel-scheduling.md`
pub struct FuelEstimator {
    /// Physical fuel most recently passed to `Store::set_fuel`.
    physical_installed: u64,
    /// Compute authority represented by the current physical fuel slice.
    authority_grant: u64,
    /// Store fuel expected at the next async out-of-gas return.
    next_yield_remaining: u64,
    /// Desired EWMA grant quantum. Authority ledgers can reduce the actual grant.
    quantum: u64,
    /// EWMA of consumed/installed ratio (fixed-point, 0..RATIO_SCALE).
    avg_ratio: u64,
    /// False until the first measured fuel observation.
    initialized: bool,
    /// Host calls observed since the last opened epoch.
    host_calls_this_epoch: u32,
    /// Per-cell ceiling for the EWMA quantum (default: MAX_FUEL).
    max_quantum: u64,
    /// Per-cell floor for the EWMA quantum (default: MIN_FUEL).
    min_quantum: u64,
    /// One-shot authority restored at each epoch transition.
    epoch_limit: Option<u64>,
    /// Cumulative fuel authority left for a oneshot cell.
    /// `None` means that the cell is scheduled and has no cumulative limit.
    total_remaining: Option<u64>,
    /// Fuel authority left in the current epoch for a oneshot cell.
    epoch_remaining: Option<u64>,
}

#[derive(Clone, Copy)]
enum FuelBoundary {
    HostReturn,
    Slice,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum FuelDecision {
    Grant(u64),
    Suspend,
    Exhausted,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct FuelUpdate {
    consumed: u64,
    decision: FuelDecision,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct FuelSlice {
    authority: u64,
    physical: u64,
    yield_interval: u64,
}

impl FuelSlice {
    fn for_authority(authority: u64) -> Self {
        debug_assert!(authority > 0);
        Self {
            authority,
            physical: authority
                .checked_add(YIELD_RESERVE)
                .expect("bounded authority grant plus yield reserve must fit u64"),
            yield_interval: authority.min(YIELD_INTERVAL),
        }
    }
}

/// One epoch opening observed at a fuel-flushed call hook.
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

/// Opt-in observer for production fuel decisions and epoch openings.
#[doc(hidden)]
#[derive(Clone)]
pub struct FuelObserver {
    observations: Arc<std::sync::Mutex<Vec<FuelEpochObservation>>>,
    decisions: Arc<std::sync::Mutex<Vec<FuelDecision>>>,
    updates: Arc<std::sync::Mutex<Vec<FuelUpdate>>>,
    slice_boundaries: Arc<std::sync::atomic::AtomicUsize>,
    decision_tx: tokio::sync::watch::Sender<usize>,
}

impl Default for FuelObserver {
    fn default() -> Self {
        let (decision_tx, _decision_rx) = tokio::sync::watch::channel(0);
        Self {
            observations: Arc::default(),
            decisions: Arc::default(),
            updates: Arc::default(),
            slice_boundaries: Arc::default(),
            decision_tx,
        }
    }
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

    fn record_decision(&self, update: FuelUpdate) {
        let count = {
            let mut decisions = self.decisions.lock().expect("fuel decision lock poisoned");
            decisions.push(update.decision);
            decisions.len()
        };
        self.updates
            .lock()
            .expect("fuel update lock poisoned")
            .push(update);
        self.decision_tx.send_replace(count);
    }

    #[cfg(test)]
    fn updates(&self) -> Vec<FuelUpdate> {
        self.updates
            .lock()
            .expect("fuel update lock poisoned")
            .clone()
    }

    fn record_slice_boundary(&self) {
        let count = self
            .slice_boundaries
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed)
            + 1;
        self.decision_tx.send_replace(count);
    }

    async fn wait_for_decision(&self, expected: FuelDecision, occurrence: usize) {
        let mut rx = self.decision_tx.subscribe();
        let result = tokio::time::timeout(std::time::Duration::from_secs(5), async {
            loop {
                let count = self
                    .decisions
                    .lock()
                    .expect("fuel decision lock poisoned")
                    .iter()
                    .filter(|decision| **decision == expected)
                    .count();
                if count >= occurrence {
                    return;
                }
                rx.changed().await.expect("fuel observer closed");
            }
        })
        .await;
        if result.is_err() {
            let decisions = self.decisions.lock().expect("fuel decision lock poisoned");
            panic!(
                "timed out waiting for fuel decision {expected:?} occurrence {occurrence}; decisions={decisions:?}, slice_boundaries={}",
                self.slice_boundaries
                    .load(std::sync::atomic::Ordering::Relaxed)
            );
        }
    }

    /// Wait until the process has entered one-shot epoch suspension.
    #[doc(hidden)]
    pub async fn wait_for_suspension(&self, occurrence: usize) {
        self.wait_for_decision(FuelDecision::Suspend, occurrence)
            .await;
    }

    /// Return the number of observed one-shot epoch suspensions.
    #[doc(hidden)]
    pub fn suspension_count(&self) -> usize {
        self.decisions
            .lock()
            .expect("fuel decision lock poisoned")
            .iter()
            .filter(|decision| **decision == FuelDecision::Suspend)
            .count()
    }

    #[cfg(test)]
    async fn wait_for_slice_boundaries(&self, expected: usize) {
        let mut rx = self.decision_tx.subscribe();
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            while self
                .slice_boundaries
                .load(std::sync::atomic::Ordering::Relaxed)
                < expected
            {
                rx.changed().await.expect("fuel observer closed");
            }
        })
        .await
        .expect("timed out waiting for repeated fuel slice boundaries");
    }
}

impl FuelEstimator {
    #[must_use]
    pub fn new(initial_quantum: u64) -> Self {
        Self {
            physical_installed: 0,
            authority_grant: 0,
            next_yield_remaining: 0,
            quantum: initial_quantum.min(MAX_FUEL),
            avg_ratio: RATIO_SCALE / 2,
            initialized: false,
            host_calls_this_epoch: 0,
            max_quantum: MAX_FUEL,
            min_quantum: MIN_FUEL,
            epoch_limit: None,
            total_remaining: None,
            epoch_remaining: None,
        }
    }

    /// Create an estimator for a oneshot (budgeted) cell.
    ///
    /// `max_per_epoch` is the cumulative authority available in each epoch and
    /// also caps the EWMA quantum. `min_per_epoch` sets only the EWMA floor.
    /// Zero values select the system defaults.
    #[must_use]
    pub fn new_oneshot(total_budget: u64, max_per_epoch: u64, min_per_epoch: u64) -> Self {
        let effective_max = if max_per_epoch > 0 {
            max_per_epoch.min(MAX_FUEL)
        } else {
            MAX_FUEL
        };
        let effective_min = if min_per_epoch > 0 {
            min_per_epoch.min(effective_max)
        } else {
            MIN_FUEL.min(effective_max)
        };
        Self {
            physical_installed: 0,
            authority_grant: 0,
            next_yield_remaining: 0,
            quantum: INITIAL_FUEL,
            avg_ratio: RATIO_SCALE / 2,
            initialized: false,
            host_calls_this_epoch: 0,
            max_quantum: effective_max,
            min_quantum: effective_min,
            epoch_limit: Some(effective_max),
            total_remaining: Some(total_budget),
            epoch_remaining: Some(effective_max),
        }
    }

    #[cfg(test)]
    fn initial_decision(&self) -> FuelDecision {
        self.current_decision()
    }

    #[cfg(test)]
    fn initial_grant(&mut self) -> u64 {
        match self.initial_decision() {
            FuelDecision::Grant(grant) => {
                self.install_authority_grant(grant);
                grant
            }
            FuelDecision::Suspend | FuelDecision::Exhausted => 0,
        }
    }

    fn available_grant(&self) -> u64 {
        self.quantum
            .min(self.total_remaining.unwrap_or(u64::MAX))
            .min(self.epoch_remaining.unwrap_or(u64::MAX))
    }

    fn current_decision(&self) -> FuelDecision {
        match (self.total_remaining, self.epoch_remaining) {
            (Some(0), _) => FuelDecision::Exhausted,
            (Some(_), Some(0)) => FuelDecision::Suspend,
            _ => FuelDecision::Grant(self.available_grant()),
        }
    }

    fn install_authority_grant(&mut self, authority: u64) -> FuelSlice {
        let slice = FuelSlice::for_authority(authority);
        self.authority_grant = authority;
        self.physical_installed = slice.physical;
        self.next_yield_remaining = slice.physical.saturating_sub(slice.yield_interval);
        slice
    }

    fn yield_boundary_reached(&self, current_fuel: u64) -> bool {
        self.authority_grant > 0 && current_fuel <= self.next_yield_remaining
    }

    fn rearm_yield_boundary(&mut self, current_fuel: u64, interval: u64) {
        self.next_yield_remaining = current_fuel.saturating_sub(interval);
    }

    fn authority_boundary_reached(&self, current_fuel: u64) -> bool {
        self.authority_grant > 0
            && self.physical_installed.saturating_sub(current_fuel) >= self.authority_grant
    }

    fn has_unsettled_consumption(&self, current_fuel: u64) -> bool {
        self.authority_grant > 0 && self.physical_installed > current_fuel
    }

    fn remaining_authority(&self, current_fuel: u64) -> u64 {
        self.authority_grant
            .saturating_sub(self.physical_installed.saturating_sub(current_fuel))
    }

    fn observe(&mut self, installed: u64, consumed: u64) {
        let ratio = (consumed.saturating_mul(RATIO_SCALE))
            .checked_div(installed)
            .unwrap_or(RATIO_SCALE / 2)
            .min(RATIO_SCALE);

        if !self.initialized {
            // Seed from first real observation — avoids cold-start bias.
            self.avg_ratio = ratio;
            self.initialized = true;
        } else {
            // EWMA α=0.3: single division for less truncation.
            self.avg_ratio = (self.avg_ratio * 7 + ratio * 3) / 10;
        }

        // Quantum inversely proportional to utilization ratio.
        // ratio=0 (pure I/O) → quantum=MAX_FUEL
        // ratio=1000 (pure compute) → quantum=0, clamped to MIN_FUEL
        self.quantum = (self.max_quantum * (RATIO_SCALE - self.avg_ratio) / RATIO_SCALE)
            .clamp(self.min_quantum, self.max_quantum);
    }

    /// Settle physical fuel exactly once and choose the next authority action.
    /// Reserve consumption caused by one inter-checkpoint overshoot is charged
    /// to both one-shot ledgers, but installing reserve never debits a ledger.
    fn settle_and_decide(&mut self, current_fuel: u64, boundary: FuelBoundary) -> FuelUpdate {
        let physical_installed = self.physical_installed;
        let authority_grant = self.authority_grant;
        let consumed = physical_installed.saturating_sub(current_fuel);
        if let Some(total_remaining) = self.total_remaining.as_mut() {
            *total_remaining = total_remaining.saturating_sub(consumed);
        }
        if let Some(epoch_remaining) = self.epoch_remaining.as_mut() {
            *epoch_remaining = epoch_remaining.saturating_sub(consumed);
        }

        match boundary {
            FuelBoundary::HostReturn => {
                self.host_calls_this_epoch += 1;
                self.observe(authority_grant, consumed);
            }
            FuelBoundary::Slice => {
                self.observe(authority_grant, consumed);
            }
        }

        self.physical_installed = current_fuel;
        self.authority_grant = 0;
        self.next_yield_remaining = current_fuel;
        FuelUpdate {
            consumed,
            decision: self.current_decision(),
        }
    }

    fn open_epoch(&mut self) {
        self.host_calls_this_epoch = 0;
        self.epoch_remaining = self.epoch_limit;
    }

    /// Settle fuel at a `ReturningFromHost` boundary.
    ///
    /// `remaining` is the fuel left in the store at the moment the guest
    /// re-enters WASM after a host call. The estimator computes the
    /// consumed/installed ratio, updates the EWMA, and returns the next grant.
    ///
    /// The returned grant is bounded by one-shot authority when present.
    #[cfg(test)]
    fn on_host_return(&mut self, remaining: u64) -> u64 {
        if self.physical_installed == 0 {
            let grant = self.available_grant();
            if grant == 0 {
                return 0;
            }
            self.install_authority_grant(grant);
        }
        let physical_remaining = remaining.saturating_add(YIELD_RESERVE);
        match self
            .settle_and_decide(physical_remaining, FuelBoundary::HostReturn)
            .decision
        {
            FuelDecision::Grant(grant) => {
                self.install_authority_grant(grant);
                grant
            }
            FuelDecision::Suspend | FuelDecision::Exhausted => 0,
        }
    }

    /// Model a flushed hook followed by an epoch opening.
    #[cfg(test)]
    fn on_flushed_epoch_boundary(&mut self, remaining: u64) -> u64 {
        if self.physical_installed == 0 {
            let grant = self.available_grant();
            if grant == 0 {
                return 0;
            }
            self.install_authority_grant(grant);
        }
        let physical_remaining = remaining.saturating_add(YIELD_RESERVE);
        self.settle_and_decide(physical_remaining, FuelBoundary::Slice);
        self.open_epoch();
        match self.current_decision() {
            FuelDecision::Grant(grant) => {
                self.install_authority_grant(grant);
                grant
            }
            FuelDecision::Suspend | FuelDecision::Exhausted => 0,
        }
    }

    /// Returns the current desired EWMA quantum before authority clamps.
    pub fn quantum(&self) -> u64 {
        self.quantum
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
    /// CidTree directory contexts, root routing entries, and private `/tmp` identities.
    ///
    /// The CidTree interceptor uses this execution-context state to delegate
    /// scratch operations to WASI without making the image root writable.
    pub(crate) fs_descriptors: crate::fs_intercept::DescriptorContexts,
    /// EWMA estimator and one-shot fuel authority ledgers.
    pub fuel_estimator: FuelEstimator,
    /// Epoch subscription paired with this Store's Wasmtime Engine.
    epoch_clock: crate::engine::EpochSubscription,
    /// Latest epoch sequence whose one-shot allowance this Store opened.
    opened_epoch: u64,
    /// Latest epoch notification waiting for a fuel-flushed call hook.
    pending_epoch: Option<u64>,
    /// The next unmarked host return belongs to the unflushed epoch libcall.
    returning_from_epoch_callback: bool,
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

    fn note_epoch_callback(&mut self, epoch: u64) {
        if epoch > self.opened_epoch {
            self.pending_epoch = Some(
                self.pending_epoch
                    .map_or(epoch, |pending| pending.max(epoch)),
            );
        }
        self.returning_from_epoch_callback = true;
    }

    fn take_epoch_callback_return(&mut self, hook: CallHook) -> bool {
        if matches!(hook, CallHook::ReturningFromHost) && self.returning_from_epoch_callback {
            self.returning_from_epoch_callback = false;
            true
        } else {
            false
        }
    }

    fn latest_unopened_epoch(&self) -> Option<u64> {
        let latest = self
            .pending_epoch
            .unwrap_or(0)
            .max(self.epoch_clock.current_epoch());
        (latest > self.opened_epoch).then_some(latest)
    }

    fn open_epoch(&mut self, epoch: u64) -> bool {
        if self.pending_epoch.is_some_and(|pending| pending <= epoch) {
            self.pending_epoch = None;
        }
        if epoch <= self.opened_epoch {
            return false;
        }
        self.opened_epoch = epoch;
        self.fuel_estimator.open_epoch();
        true
    }

    fn open_latest_epoch(&mut self) -> Option<u64> {
        let epoch = self.latest_unopened_epoch()?;
        self.open_epoch(epoch).then_some(epoch)
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
    runtime_engine: Option<crate::engine::RuntimeEngine>,
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
    store.call_hook_async(FuelCallHook);
}

struct FuelCallHook;

#[async_trait::async_trait]
impl CallHookHandler<ComponentRunStates> for FuelCallHook {
    async fn handle_call_event(
        &self,
        mut ctx: StoreContextMut<'_, ComponentRunStates>,
        hook: CallHook,
    ) -> wasmtime::Result<()> {
        let marked_host_return = ctx.data_mut().host_call_frames.on_call_hook(hook);
        if ctx.data_mut().take_epoch_callback_return(hook) {
            return Ok(());
        }
        if !matches!(hook, CallHook::ReturningFromHost) {
            return Ok(());
        }

        let remaining = ctx.get_fuel().unwrap_or(0);
        let slice_boundary =
            !marked_host_return && ctx.data().fuel_estimator.yield_boundary_reached(remaining);
        if slice_boundary {
            if let Some(observer) = ctx.data().fuel_observer.as_ref() {
                observer.record_slice_boundary();
            }
        }
        let fuel_boundary = !marked_host_return
            && ctx
                .data()
                .fuel_estimator
                .authority_boundary_reached(remaining);
        if !marked_host_return && !slice_boundary {
            return Ok(());
        }

        let pending_epoch = ctx.data().latest_unopened_epoch();
        if !marked_host_return && !fuel_boundary && pending_epoch.is_none() {
            let remaining_authority = ctx.data().fuel_estimator.remaining_authority(remaining);
            let interval = remaining_authority.min(YIELD_INTERVAL);
            ctx.fuel_async_yield_interval(Some(interval))?;
            ctx.data_mut()
                .fuel_estimator
                .rearm_yield_boundary(remaining, interval);
            return Ok(());
        }

        let boundary = if marked_host_return {
            FuelBoundary::HostReturn
        } else {
            FuelBoundary::Slice
        };
        settle_and_apply_fuel_boundary(&mut ctx, remaining, boundary).await
    }
}

async fn settle_and_apply_fuel_boundary(
    ctx: &mut StoreContextMut<'_, ComponentRunStates>,
    remaining: u64,
    boundary: FuelBoundary,
) -> wasmtime::Result<()> {
    let mut update = ctx
        .data_mut()
        .fuel_estimator
        .settle_and_decide(remaining, boundary);
    let host_calls_this_epoch = ctx.data().fuel_estimator.host_calls_this_epoch;
    let opened_epoch = ctx.data_mut().open_latest_epoch();
    if opened_epoch.is_some() {
        ctx.set_epoch_deadline(1);
        update.decision = ctx.data().fuel_estimator.current_decision();
        if let Some(observer) = ctx.data().fuel_observer.as_ref() {
            observer.record(FuelEpochObservation {
                current_fuel: remaining,
                measured_consumption: update.consumed,
                host_calls_this_epoch,
                budget: match update.decision {
                    FuelDecision::Grant(grant) => grant,
                    FuelDecision::Suspend | FuelDecision::Exhausted => 0,
                },
                avg_ratio: ctx.data().fuel_estimator.avg_ratio(),
            });
        }
    }
    if let Some(observer) = ctx.data().fuel_observer.as_ref() {
        observer.record_decision(update);
    }
    apply_fuel_decision(ctx, update.decision).await?;
    tracing::debug!(
        consumed = update.consumed,
        remaining,
        avg_ratio = ctx.data().fuel_estimator.avg_ratio(),
        "fuel.boundary"
    );
    Ok(())
}

async fn apply_fuel_decision(
    ctx: &mut StoreContextMut<'_, ComponentRunStates>,
    mut decision: FuelDecision,
) -> wasmtime::Result<()> {
    loop {
        match decision {
            FuelDecision::Grant(grant) => {
                install_fuel_slice_in_context(ctx, grant)?;
                return Ok(());
            }
            FuelDecision::Exhausted => {
                return Err(wasmtime::Error::msg(
                    "one-shot total fuel authority exhausted",
                ));
            }
            FuelDecision::Suspend => {
                if ctx.data_mut().open_latest_epoch().is_none() {
                    ctx.data_mut().epoch_clock.changed().await.map_err(|_| {
                        wasmtime::Error::msg("epoch clock closed while one-shot Cell was suspended")
                    })?;
                    continue;
                }
                ctx.set_epoch_deadline(1);
                decision = ctx.data().fuel_estimator.current_decision();
            }
        }
    }
}

fn install_fuel_slice_in_context(
    ctx: &mut StoreContextMut<'_, ComponentRunStates>,
    authority: u64,
) -> wasmtime::Result<()> {
    let slice = ctx
        .data_mut()
        .fuel_estimator
        .install_authority_grant(authority);
    ctx.fuel_async_yield_interval(Some(slice.yield_interval))?;
    ctx.set_fuel(slice.physical)?;
    Ok(())
}

fn install_fuel_slice_in_store(
    store: &mut Store<ComponentRunStates>,
    authority: u64,
) -> wasmtime::Result<()> {
    let slice = store
        .data_mut()
        .fuel_estimator
        .install_authority_grant(authority);
    store.fuel_async_yield_interval(Some(slice.yield_interval))?;
    store.set_fuel(slice.physical)?;
    Ok(())
}

fn install_epoch_fuel_callback(store: &mut Store<ComponentRunStates>) {
    store.epoch_deadline_callback(|mut ctx| {
        let current_epoch = ctx.data().epoch_clock.engine_epoch();
        ctx.data_mut().note_epoch_callback(current_epoch);
        tracing::trace!(current_epoch, "fuel.epoch_pending");
        Ok(wasmtime::UpdateDeadline::Continue(1))
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
    runtime_engine: Option<crate::engine::RuntimeEngine>,
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
            runtime_engine: None,
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

    /// Provide the paired Wasmtime engine and epoch clock for this process.
    pub fn with_runtime_engine(mut self, runtime_engine: crate::engine::RuntimeEngine) -> Self {
        self.runtime_engine = Some(runtime_engine);
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
    /// the process store. The Cap'n Proto `Executor.spawn` boundary uses this
    /// override to apply one-shot authority from the `FuelPolicy` schema.
    pub fn with_fuel_estimator(mut self, est: FuelEstimator) -> Self {
        self.fuel_estimator = Some(est);
        self
    }

    /// Install opt-in observation of production fuel decisions and epoch openings.
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
            runtime_engine: self.runtime_engine,
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

/// Prepared Cell process and its runtime configuration.
///
/// `Builder::build` compiles, links, and validates the component without
/// executable instantiation or physical fuel. `Proc::run` applies the initial
/// fuel decision before `instantiate_async` can execute guest start functions.
/// Start functions use the same hooks and settlement path as `wasi:cli/run`.
pub struct Proc {
    /// Validated command prepared for executable instantiation.
    pre: WasiCliCommandPre<ComponentRunStates>,
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
            runtime_engine,
            stdin,
            stdout,
            stderr,
            granted_transport,
            cache_mode,
            mode,
            fuel_estimator,
            fuel_observer,
        } = init;
        if let Program::Bytes(bytes) = &program {
            if bytes.len() > crate::sched::MAX_COMPONENT_BYTES {
                return Err(anyhow!(
                    "component exceeds the fuel segment size bound: {} bytes, maximum {}",
                    bytes.len(),
                    crate::sched::MAX_COMPONENT_BYTES
                ));
            }
        }
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
        //   epoch_interruption — enables the epoch accounting callback.
        let runtime_engine = runtime_engine.ok_or_else(|| {
            anyhow!("Cell Builder requires a paired runtime Engine and epoch clock")
        })?;
        let engine = runtime_engine.engine();
        let epoch_clock = runtime_engine.subscribe();
        let opened_epoch = epoch_clock.current_epoch();
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

        let fuel_estimator = fuel_estimator.unwrap_or_else(|| FuelEstimator::new(INITIAL_FUEL));
        let state = ComponentRunStates {
            wasi_ctx: wasi,
            resource_table: ResourceTable::new(),
            image_root: filesystem.image_root,
            scratch: filesystem.scratch,
            granted_transport,
            cache_mode,
            cid_tree,
            fs_descriptors: crate::fs_intercept::DescriptorContexts::default(),
            fuel_estimator,
            epoch_clock,
            opened_epoch,
            pending_epoch: None,
            returning_from_epoch_callback: false,
            fuel_observer,
            host_call_frames: HostCallFrames::default(),
            kernel_ready_gate,
        };

        let mut store = Store::new(&engine, state);

        // The epoch callback records only the latest transition. A later
        // fuel-flushed hook settles the old epoch before opening the new one.
        install_epoch_fuel_callback(&mut store);
        store.set_epoch_deadline(1);

        // The EWMA accounting hook observes linked imports when they return.
        //
        // The estimator tracks the consumed/installed ratio via EWMA and sizes
        // the next quantum inversely. Wasmtime 48 also emits these hooks for internal libcalls
        // and fuel/epoch yields. Linked imports mark the Store state from
        // their host implementation. Confirmed out-of-gas returns are the
        // unmarked fuel boundaries. Intermediate returns re-arm the next yield
        // to the remaining authority before execution resumes.
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

        Ok(Self { pre, store })
    }

    /// Instantiate the prepared guest and invoke `wasi:cli/run#run`.
    pub async fn run(mut self) -> Result<()> {
        match self.store.data().fuel_estimator.current_decision() {
            FuelDecision::Grant(grant) => {
                // Physical fuel includes a runtime reserve that is not Cell
                // authority. The normal fuel and epoch hooks were installed
                // during preparation, before executable instantiation.
                install_fuel_slice_in_store(&mut self.store, grant)?;
                tracing::trace!(grant, "fuel.initial");
            }
            FuelDecision::Exhausted => {
                return Err(anyhow!("one-shot total fuel authority exhausted"));
            }
            FuelDecision::Suspend => {
                unreachable!("a new one-shot epoch starts with authority")
            }
        }

        let start = std::time::Instant::now();
        let command = match tokio::time::timeout(
            std::time::Duration::from_secs(5),
            self.pre.instantiate_async(&mut self.store),
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

        let remaining = self.store.get_fuel()?;
        if self
            .store
            .data()
            .fuel_estimator
            .has_unsettled_consumption(remaining)
        {
            let mut ctx = self.store.as_context_mut();
            settle_and_apply_fuel_boundary(&mut ctx, remaining, FuelBoundary::Slice).await?;
        }

        let result = self
            .store
            .run_concurrent(async move |access| command.wasi_cli_run().call_run(access).await)
            .await
            .map_err(|error| anyhow!("P3 command event loop failed: {error:#}"))?
            .map_err(|error| anyhow!("failed to call `wasi:cli/run`: {error:#}"))?;
        result.map_err(|()| anyhow!("guest returned non-zero exit status"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
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

    struct PollCounter<F> {
        future: Pin<Box<F>>,
        polls: Arc<AtomicUsize>,
    }

    impl<F: Future> Future for PollCounter<F> {
        type Output = F::Output;

        fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
            self.polls.fetch_add(1, Ordering::Relaxed);
            self.future.as_mut().poll(cx)
        }
    }

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

    const P3_TRAPPING_START_COMMAND: &str = r#"
        (component
          (core func $task-return (canon task.return (result (result))))
          (core module $command
            (import "" "task-return" (func $task-return (param i32)))
            (func $start
              unreachable)
            (start $start)
            (func (export "run")
              i32.const 0
              call $task-return))
          (core instance $instance
            (instantiate $command
              (with "" (instance
                (export "task-return" (func $task-return))))))
          (func $run async (result (result))
            (canon lift (core func $instance "run") async))
          (instance (export (interface "wasi:cli/run@0.3.0"))
            (export "run" (func $run))))
    "#;

    const P3_COUNTED_START_COMMAND: &str = r#"
        (component
          (core func $task-return (canon task.return (result (result))))
          (core module $command
            (import "" "task-return" (func $task-return (param i32)))
            (func $start
              (local $remaining i32)
              i32.const 3000
              local.set $remaining
              (loop $spin
                local.get $remaining
                i32.const 1
                i32.sub
                local.tee $remaining
                br_if $spin))
            (start $start)
            (func (export "run")
              i32.const 0
              call $task-return))
          (core instance $instance
            (instantiate $command
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

    fn component_test_state_with_fuel(
        fuel_estimator: FuelEstimator,
        fuel_observer: Option<FuelObserver>,
    ) -> ComponentRunStates {
        let (runtime_engine, _publisher) =
            crate::engine::runtime_engine().expect("component test runtime engine");
        component_test_state_with_runtime(fuel_estimator, fuel_observer, &runtime_engine)
    }

    fn component_test_state_with_runtime(
        fuel_estimator: FuelEstimator,
        fuel_observer: Option<FuelObserver>,
        runtime_engine: &crate::engine::RuntimeEngine,
    ) -> ComponentRunStates {
        let (_host, granted_transport) = crate::p3::HostTransport::bounded_pair();
        let epoch_clock = runtime_engine.subscribe();
        let opened_epoch = epoch_clock.current_epoch();
        ComponentRunStates {
            wasi_ctx: WasiCtxBuilder::new().build(),
            resource_table: ResourceTable::new(),
            image_root: None,
            scratch: tempfile::TempDir::new().expect("component test scratch"),
            granted_transport,
            cache_mode: None,
            cid_tree: None,
            fs_descriptors: crate::fs_intercept::DescriptorContexts::default(),
            fuel_estimator,
            epoch_clock,
            opened_epoch,
            pending_epoch: None,
            returning_from_epoch_callback: false,
            fuel_observer,
            host_call_frames: HostCallFrames::default(),
            kernel_ready_gate: None,
        }
    }

    fn component_test_state() -> ComponentRunStates {
        component_test_state_with_fuel(FuelEstimator::new(INITIAL_FUEL), None)
    }

    async fn linked_routing_key_probe_bytes_with_fuel(
        bytes: &[u8],
        fuel_estimator: FuelEstimator,
        fuel_observer: Option<FuelObserver>,
    ) -> (
        Store<ComponentRunStates>,
        wasmtime::component::TypedFunc<(), (String,)>,
        crate::engine::EpochPublisher,
    ) {
        let (runtime_engine, publisher) =
            crate::engine::runtime_engine().expect("component runtime engine");
        let engine = runtime_engine.engine();
        let component = Component::new(&engine, bytes).expect("routing-key probe component");
        let mut linker = Linker::new(&engine);
        add_p3_wasi_to_linker(&mut linker).expect("install P3 WASI imports");
        add_routing_key_to_linker(&mut linker).expect("install routing-key import");
        let initial_decision = fuel_estimator.initial_decision();
        let mut store = Store::new(
            &engine,
            component_test_state_with_runtime(fuel_estimator, fuel_observer, &runtime_engine),
        );
        match initial_decision {
            FuelDecision::Grant(grant) => {
                install_fuel_slice_in_store(&mut store, grant).expect("routing-key test fuel")
            }
            FuelDecision::Exhausted => {
                store
                    .fuel_async_yield_interval(Some(YIELD_INTERVAL))
                    .expect("routing-key yield interval");
                store
                    .set_fuel(YIELD_RESERVE)
                    .expect("routing-key exhausted reserve");
            }
            FuelDecision::Suspend => unreachable!("new test epoch has authority"),
        }
        store.set_epoch_deadline(1);
        install_epoch_fuel_callback(&mut store);
        install_host_return_fuel_hook(&mut store);
        let instance = linker
            .instantiate_async(&mut store, &component)
            .await
            .expect("instantiate routing-key probe");
        let probe = instance
            .get_typed_func::<(), (String,)>(&mut store, "probe")
            .expect("typed routing-key probe export");
        (store, probe, publisher)
    }

    async fn linked_routing_key_probe_bytes(
        bytes: &[u8],
    ) -> (
        Store<ComponentRunStates>,
        wasmtime::component::TypedFunc<(), (String,)>,
        crate::engine::EpochPublisher,
    ) {
        linked_routing_key_probe_bytes_with_fuel(bytes, FuelEstimator::new(INITIAL_FUEL), None)
            .await
    }

    async fn linked_routing_key_probe() -> (
        Store<ComponentRunStates>,
        wasmtime::component::TypedFunc<(), (String,)>,
        crate::engine::EpochPublisher,
    ) {
        linked_routing_key_probe_bytes(ROUTING_KEY_PROBE_COMPONENT.as_bytes()).await
    }

    fn compiled_routing_key_probe_bytes() -> Vec<u8> {
        static BYTES: std::sync::OnceLock<Vec<u8>> = std::sync::OnceLock::new();
        BYTES
            .get_or_init(|| {
                let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
                let artifact = root
                    .join("target/routing-key-probe/wasm32-wasip3/release/routing_key_probe.wasm");
                if !artifact.is_file() {
                    let status = std::process::Command::new("make")
                        .current_dir(&root)
                        .arg("routing-key-probe")
                        .status()
                        .expect("launch routing-key-probe build");
                    assert!(status.success(), "routing-key-probe build failed");
                }
                std::fs::read(&artifact)
                    .unwrap_or_else(|error| panic!("read {}: {error}", artifact.display()))
            })
            .clone()
    }

    #[tokio::test]
    async fn routing_key_wit_call_matches_golden_vector_and_is_deterministic() {
        let (mut store, probe, _publisher) = linked_routing_key_probe().await;

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
        let bytes = compiled_routing_key_probe_bytes();
        let (mut store, probe, _publisher) = linked_routing_key_probe_bytes(&bytes).await;

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

    fn charged_oneshot_fuel(store: &Store<ComponentRunStates>, total_budget: u64) -> u64 {
        let est = &store.data().fuel_estimator;
        let settled = total_budget - est.total_remaining.expect("one-shot total ledger");
        let current = store.get_fuel().expect("read one-shot Store fuel");
        settled + est.physical_installed.saturating_sub(current)
    }

    #[tokio::test]
    async fn oneshot_fuel_tiny_p3_guest_exhausts_explicitly() {
        let bytecode = wat::parse_str(include_str!(
            "../../../tests/fixtures/spinning-component.wat"
        ))
        .expect("parse P3 spinning component fixture");

        let (runtime_engine, _publisher) =
            crate::engine::runtime_engine().expect("component runtime engine");
        let (builder, _handles) = Builder::ordinary(
            Program::Bytes(bytecode),
            tokio::io::empty(),
            tokio::io::sink(),
            tokio::io::sink(),
        );
        let proc = builder
            .with_runtime_engine(runtime_engine)
            .with_fuel_estimator(FuelEstimator::new_oneshot(1_337, 10_000, 0))
            .build()
            .await
            .expect("build budgeted P3 spinning component");
        assert_eq!(
            proc.store.get_fuel().expect("read prepared P3 fuel"),
            0,
            "construction installed executable fuel before Proc::run"
        );
        assert_eq!(
            proc.store.data().fuel_estimator.current_decision(),
            FuelDecision::Grant(1_337)
        );

        let error = tokio::time::timeout(std::time::Duration::from_secs(5), proc.run())
            .await
            .expect("budgeted P3 spinning component timed out")
            .expect_err("budgeted P3 spinning component must exhaust fuel");
        let message = format!("{error:#}");
        assert!(message.contains("one-shot total fuel authority exhausted"));
        assert!(!message.contains("all fuel consumed by WebAssembly"));
    }

    #[tokio::test]
    async fn oneshot_fuel_rearms_the_final_partial_yield_interval() {
        let bytecode = wat::parse_str(include_str!(
            "../../../tests/fixtures/spinning-component.wat"
        ))
        .expect("parse P3 spinning component fixture");

        for authority in [12_500, 15_000, 25_000, 100_001] {
            let (runtime_engine, _publisher) =
                crate::engine::runtime_engine().expect("component runtime engine");
            let observer = FuelObserver::default();
            let (builder, _handles) = Builder::ordinary(
                Program::Bytes(bytecode.clone()),
                tokio::io::empty(),
                tokio::io::sink(),
                tokio::io::sink(),
            );
            let proc = builder
                .with_runtime_engine(runtime_engine)
                .with_fuel_estimator(FuelEstimator::new_oneshot(authority, authority, 0))
                .with_fuel_observer(observer.clone())
                .build()
                .await
                .expect("build budgeted P3 spinning component");

            let error = tokio::time::timeout(std::time::Duration::from_secs(5), proc.run())
                .await
                .expect("budgeted P3 spinning component timed out")
                .expect_err("budgeted P3 spinning component must exhaust fuel");
            let message = format!("{error:#}");
            assert!(message.contains("one-shot total fuel authority exhausted"));
            assert!(!message.contains("all fuel consumed by WebAssembly"));

            let updates = observer.updates();
            let consumed: u64 = updates.iter().map(|update| update.consumed).sum();
            assert!(
                consumed >= authority,
                "authority {authority} settled only {consumed} fuel: {updates:?}"
            );
            assert!(
                consumed <= authority + 4,
                "authority {authority} crossed its final yield interval: {updates:?}"
            );
        }
    }

    #[tokio::test]
    async fn oneshot_fuel_compiled_p3_host_calls_cannot_mint_total_authority() {
        const TOTAL_BUDGET: u64 = 25_000;
        let bytes = compiled_routing_key_probe_bytes();
        let (mut store, probe, _publisher) = linked_routing_key_probe_bytes_with_fuel(
            &bytes,
            FuelEstimator::new_oneshot(TOTAL_BUDGET, 100_000, 0),
            None,
        )
        .await;

        let mut completed = 0;
        let failure = loop {
            match probe.call_async(&mut store, ()).await {
                Ok((key,)) => {
                    assert_eq!(
                        key,
                        "bafkr4ifcoue3f52zpzpz2xei7dqhs3gajm326llyljbwisxkwea7hbowyy"
                    );
                    completed += 1;
                    assert!(completed < 10_000, "one-shot guest did not exhaust");
                }
                Err(error) => break error,
            }
        };

        assert!(completed > 1, "test did not exercise repeated host returns");
        assert_eq!(charged_oneshot_fuel(&store, TOTAL_BUDGET), TOTAL_BUDGET);
        assert!(
            format!("{failure:#}").contains("one-shot total fuel authority exhausted"),
            "unexpected one-shot failure: {failure:#}"
        );
    }

    #[tokio::test]
    async fn oneshot_fuel_compiled_p3_host_calls_share_one_epoch_limit() {
        const TOTAL_BUDGET: u64 = 25_000;
        const MAX_PER_EPOCH: u64 = 10_000;
        let bytes = compiled_routing_key_probe_bytes();
        let observer = FuelObserver::default();
        let (mut store, probe, mut publisher) = linked_routing_key_probe_bytes_with_fuel(
            &bytes,
            FuelEstimator::new_oneshot(TOTAL_BUDGET, MAX_PER_EPOCH, 0),
            Some(observer.clone()),
        )
        .await;

        let mut completed = 0;
        loop {
            let mut call = Box::pin(probe.call_async(&mut store, ()));
            match tokio::time::timeout(std::time::Duration::from_millis(20), &mut call).await {
                Ok(Ok(_)) => {
                    completed += 1;
                    assert!(completed < 10_000, "one-shot P3 guest did not suspend");
                }
                Ok(Err(error)) => {
                    panic!("one-shot P3 guest terminated before epoch wake: {error:#}")
                }
                Err(_) => {
                    publisher.tick();
                    call.await
                        .expect("host-call-heavy guest resumes after epoch wake");
                    break;
                }
            }
        }

        assert!(completed > 1, "test did not exercise repeated host returns");
        assert!(observer
            .decisions
            .lock()
            .expect("fuel decision lock")
            .contains(&FuelDecision::Suspend));
        assert!(charged_oneshot_fuel(&store, TOTAL_BUDGET) > MAX_PER_EPOCH);
    }

    #[tokio::test]
    async fn oneshot_fuel_compiled_p3_spans_epochs_without_resetting_total() {
        const TOTAL_BUDGET: u64 = 25_000;
        const MAX_PER_EPOCH: u64 = 10_000;
        let bytes = compiled_routing_key_probe_bytes();
        let observer = FuelObserver::default();
        let (mut store, probe, mut publisher) = linked_routing_key_probe_bytes_with_fuel(
            &bytes,
            FuelEstimator::new_oneshot(TOTAL_BUDGET, MAX_PER_EPOCH, 0),
            Some(observer.clone()),
        )
        .await;

        let mut completed = 0;
        loop {
            publisher.tick();
            match probe.call_async(&mut store, ()).await {
                Ok(_) => {
                    completed += 1;
                    assert!(
                        completed < 10_000,
                        "multi-epoch one-shot guest did not exhaust"
                    );
                }
                Err(_) => break,
            }
        }

        let observations = observer.observations();
        assert!(
            observations.len() >= 2,
            "guest did not cross multiple production epochs: {observations:?}"
        );
        assert!(observations.iter().all(|sample| {
            sample.measured_consumption <= MAX_PER_EPOCH && sample.budget <= MAX_PER_EPOCH
        }));
        assert_eq!(charged_oneshot_fuel(&store, TOTAL_BUDGET), TOTAL_BUDGET);
    }

    #[tokio::test]
    async fn oneshot_fuel_compiled_p3_consumes_a_final_partial_grant() {
        const CALIBRATION_TOTAL: u64 = 100_000;
        const FINAL_PARTIAL: u64 = 1_337;
        let bytes = compiled_routing_key_probe_bytes();
        let (mut calibration_store, calibration_probe, _calibration_publisher) =
            linked_routing_key_probe_bytes_with_fuel(
                &bytes,
                FuelEstimator::new_oneshot(CALIBRATION_TOTAL, CALIBRATION_TOTAL, 0),
                None,
            )
            .await;
        calibration_probe
            .call_async(&mut calibration_store, ())
            .await
            .expect("calibrate one compiled P3 probe call");
        let first_call_fuel = charged_oneshot_fuel(&calibration_store, CALIBRATION_TOTAL);
        assert!(first_call_fuel > 0);

        let total_budget = first_call_fuel + FINAL_PARTIAL;
        let observer = FuelObserver::default();
        let (mut store, probe, mut publisher) = linked_routing_key_probe_bytes_with_fuel(
            &bytes,
            FuelEstimator::new_oneshot(total_budget, CALIBRATION_TOTAL, 0),
            Some(observer.clone()),
        )
        .await;
        probe
            .call_async(&mut store, ())
            .await
            .expect("first budgeted compiled P3 probe call");
        assert_eq!(charged_oneshot_fuel(&store, total_budget), first_call_fuel);

        publisher.tick();
        let error = probe
            .call_async(&mut store, ())
            .await
            .expect_err("the second compiled P3 probe must exhaust the partial grant");
        let message = format!("{error:#}");
        assert!(message.contains("one-shot total fuel authority exhausted"));
        assert!(!message.contains("all fuel consumed by WebAssembly"));

        let observations = observer.observations();
        let final_epoch = observations
            .iter()
            .find(|sample| sample.budget > 0 && sample.budget <= FINAL_PARTIAL)
            .unwrap_or_else(|| {
                panic!("no final partial epoch grant was observed: {observations:?}")
            });
        assert!(
            final_epoch.measured_consumption > 0,
            "old-epoch work was not charged before the final epoch opened"
        );
        assert!(observer.updates().iter().any(|update| {
            update.consumed == final_epoch.measured_consumption
                && update.decision == FuelDecision::Grant(final_epoch.budget)
        }));
        assert_eq!(charged_oneshot_fuel(&store, total_budget), total_budget);
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
        let (runtime_engine, _publisher) =
            crate::engine::runtime_engine().expect("component runtime engine");
        let engine = runtime_engine.engine();
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
            .with_runtime_engine(runtime_engine)
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
        let (mut store, probe, _publisher) = linked_routing_key_probe().await;

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

    #[tokio::test]
    async fn epoch_callback_defers_accounting_until_a_flushed_hook() {
        let (runtime_engine, mut publisher) =
            crate::engine::runtime_engine().expect("component runtime engine");
        let engine = runtime_engine.engine();
        let module = wasmtime::Module::new(
            &engine,
            r#"
                (module
                    (import "host" "block" (func $block))
                    (func (export "run")
                        call $block))
            "#,
        )
        .expect("blocking core module");
        let mut linker = wasmtime::Linker::new(&engine);
        linker
            .func_wrap_async(
                "host",
                "block",
                |_caller: wasmtime::Caller<'_, ComponentRunStates>, (): ()| {
                    Box::new(std::future::pending::<()>())
                },
            )
            .expect("link blocking host import");

        let observer = FuelObserver::default();
        let mut store = Store::new(
            &engine,
            component_test_state_with_runtime(
                FuelEstimator::new_oneshot(25_000, 10_000, 0),
                Some(observer.clone()),
                &runtime_engine,
            ),
        );
        install_fuel_slice_in_store(&mut store, 10_000).expect("initial callback test fuel");
        install_epoch_fuel_callback(&mut store);
        install_host_return_fuel_hook(&mut store);
        store.set_epoch_deadline(1);
        let instance = linker
            .instantiate_async(&mut store, &module)
            .await
            .expect("instantiate blocking core module");
        let run = instance
            .get_typed_func::<(), ()>(&mut store, "run")
            .expect("run export");

        publisher.tick();
        let mut call = Box::pin(run.call_async(&mut store, ()));
        let first_poll = std::future::poll_fn(|cx| Poll::Ready(call.as_mut().poll(cx))).await;
        assert!(first_poll.is_pending(), "blocking host import completed");
        assert!(
            observer.observations().is_empty(),
            "epoch callback performed fuel accounting before a flushed hook"
        );
        assert!(
            observer.updates().is_empty(),
            "epoch callback produced a fuel decision before a flushed hook"
        );
        drop(call);
        assert_eq!(
            store.data().opened_epoch,
            0,
            "epoch allowance opened before a flushed hook"
        );
    }

    #[tokio::test]
    async fn pending_epoch_settles_once_at_the_next_marked_host_return() {
        let (runtime_engine, mut publisher) =
            crate::engine::runtime_engine().expect("component runtime engine");
        let engine = runtime_engine.engine();
        let module = wasmtime::Module::new(
            &engine,
            r#"
                (module
                    (import "host" "ping" (func $ping))
                    (func (export "run")
                        call $ping))
            "#,
        )
        .expect("marked-return core module");
        let mut linker = wasmtime::Linker::new(&engine);
        linker
            .func_wrap(
                "host",
                "ping",
                |mut caller: wasmtime::Caller<'_, ComponentRunStates>| {
                    caller.data_mut().mark_host_call();
                },
            )
            .expect("link marked host import");

        let observer = FuelObserver::default();
        let mut store = Store::new(
            &engine,
            component_test_state_with_runtime(
                FuelEstimator::new_oneshot(25_000, 10_000, 0),
                Some(observer.clone()),
                &runtime_engine,
            ),
        );
        install_fuel_slice_in_store(&mut store, 10_000).expect("initial marked-return fuel");
        install_epoch_fuel_callback(&mut store);
        install_host_return_fuel_hook(&mut store);
        store.set_epoch_deadline(1);
        let instance = linker
            .instantiate_async(&mut store, &module)
            .await
            .expect("instantiate marked-return core module");
        let run = instance
            .get_typed_func::<(), ()>(&mut store, "run")
            .expect("run export");

        publisher.tick();
        run.call_async(&mut store, ())
            .await
            .expect("marked host return after epoch callback");

        assert_eq!(store.data().opened_epoch, 1);
        assert_eq!(store.data().pending_epoch, None);
        assert!(!store.data().returning_from_epoch_callback);
        let observations = observer.observations();
        let updates = observer.updates();
        assert_eq!(observations.len(), 1, "epoch opened more than once");
        assert_eq!(updates.len(), 1, "old epoch settled more than once");
        assert_eq!(observations[0].measured_consumption, updates[0].consumed);
        assert_eq!(observations[0].host_calls_this_epoch, 1);
    }

    #[tokio::test]
    async fn pending_epoch_ticks_coalesce_at_the_next_out_of_gas_return() {
        const TOTAL_BUDGET: u64 = 25_000;
        let (runtime_engine, mut publisher) =
            crate::engine::runtime_engine().expect("component runtime engine");
        let engine = runtime_engine.engine();
        let module = wasmtime::Module::new(
            &engine,
            r#"
                (module
                    (func (export "run")
                        (loop $spin
                            br $spin)))
            "#,
        )
        .expect("spinning core module");
        let observer = FuelObserver::default();
        let mut store = Store::new(
            &engine,
            component_test_state_with_runtime(
                FuelEstimator::new_oneshot(TOTAL_BUDGET, TOTAL_BUDGET, 0),
                Some(observer.clone()),
                &runtime_engine,
            ),
        );
        install_fuel_slice_in_store(&mut store, TOTAL_BUDGET)
            .expect("initial out-of-gas callback fuel");
        install_epoch_fuel_callback(&mut store);
        install_host_return_fuel_hook(&mut store);
        store.set_epoch_deadline(1);
        let instance = wasmtime::Linker::new(&engine)
            .instantiate_async(&mut store, &module)
            .await
            .expect("instantiate spinning core module");
        let run = instance
            .get_typed_func::<(), ()>(&mut store, "run")
            .expect("run export");

        assert_eq!(publisher.tick(), 1);
        assert_eq!(publisher.tick(), 2);
        assert_eq!(publisher.tick(), 3);
        let error = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            run.call_async(&mut store, ()),
        )
        .await
        .expect("spinning core module timed out")
        .expect_err("spinning core module must exhaust total authority");
        assert!(format!("{error:#}").contains("one-shot total fuel authority exhausted"));

        assert_eq!(store.data().opened_epoch, 3);
        assert_eq!(store.data().pending_epoch, None);
        assert_eq!(
            observer.observations().len(),
            1,
            "coalesced ticks opened more than one allowance"
        );
        let updates = observer.updates();
        let consumed: u64 = updates.iter().map(|update| update.consumed).sum();
        assert!(consumed >= TOTAL_BUDGET, "under-counted fuel: {updates:?}");
        assert!(
            consumed <= TOTAL_BUDGET + 4,
            "coalesced ticks minted fuel: {updates:?}"
        );
    }

    #[tokio::test]
    async fn epoch_callback_mid_chunk_installs_only_the_final_authority_at_a_flushed_hook() {
        for final_authority in [1, 100, 1337, 9999] {
            let total_budget = YIELD_INTERVAL + final_authority + 2;
            let (runtime_engine, publisher) =
                crate::engine::runtime_engine().expect("component runtime engine");
            let engine = runtime_engine.engine();
            let module = wasmtime::Module::new(
                &engine,
                r#"
                    (module
                        (import "host" "tighten-and-tick" (func $tighten-and-tick))
                        (global $counter (mut i32) (i32.const 0))
                        (func (export "run")
                            call $tighten-and-tick
                            (loop $spin
                                global.get $counter
                                i32.const 1
                                i32.add
                                global.set $counter
                                br $spin)))
                "#,
            )
            .expect("mid-chunk core module");
            let publisher = Arc::new(std::sync::Mutex::new(publisher));
            let callback_publisher = Arc::clone(&publisher);
            let mut linker = wasmtime::Linker::new(&engine);
            linker
                .func_wrap(
                    "host",
                    "tighten-and-tick",
                    move |mut caller: wasmtime::Caller<'_, ComponentRunStates>| {
                        caller.data_mut().fuel_estimator.epoch_limit = Some(final_authority);
                        callback_publisher
                            .lock()
                            .expect("epoch publisher lock")
                            .tick();
                    },
                )
                .expect("link epoch trigger");

            let observer = FuelObserver::default();
            let mut store = Store::new(
                &engine,
                component_test_state_with_runtime(
                    FuelEstimator::new_oneshot(total_budget, total_budget, 0),
                    Some(observer.clone()),
                    &runtime_engine,
                ),
            );
            install_fuel_slice_in_store(&mut store, total_budget).expect("initial mid-chunk fuel");
            install_epoch_fuel_callback(&mut store);
            install_host_return_fuel_hook(&mut store);
            store.set_epoch_deadline(1);
            let instance = linker
                .instantiate_async(&mut store, &module)
                .await
                .expect("instantiate mid-chunk core module");
            let run = instance
                .get_typed_func::<(), ()>(&mut store, "run")
                .expect("run export");

            let error = tokio::time::timeout(
                std::time::Duration::from_secs(5),
                run.call_async(&mut store, ()),
            )
            .await
            .expect("mid-chunk core module did not reach total exhaustion")
            .expect_err("mid-chunk core module must exhaust total authority");
            assert!(format!("{error:#}").contains("one-shot total fuel authority exhausted"));

            let updates = observer.updates();
            assert_eq!(updates.len(), 2, "unexpected settlement trace: {updates:?}");
            assert!(
                updates[0].consumed >= YIELD_INTERVAL,
                "old epoch work was not settled at the flushed boundary: {updates:?}"
            );
            assert_eq!(updates[0].decision, FuelDecision::Grant(final_authority));
            assert!(
                updates[1].consumed >= final_authority,
                "guest executed less than the final authority: {updates:?}"
            );
            // The fixture loop has five fixed-cost operators. Crossing after
            // its first operator can execute at most four more fuel units.
            assert!(
                updates[1].consumed <= final_authority + 4,
                "guest executed beyond one inter-checkpoint segment: {updates:?}"
            );
            assert_eq!(updates[1].decision, FuelDecision::Exhausted);
            let measured_consumption: u64 = updates.iter().map(|update| update.consumed).sum();
            assert!(measured_consumption >= total_budget);
            assert!(measured_consumption <= total_budget + 4);
        }
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

    #[tokio::test]
    async fn oversized_component_is_rejected_before_compilation() {
        let (runtime_engine, _publisher) =
            crate::engine::runtime_engine().expect("component runtime engine");
        let (builder, _handles) = Builder::ordinary(
            Program::Bytes(vec![0; crate::sched::MAX_COMPONENT_BYTES + 1]),
            tokio::io::empty(),
            tokio::io::sink(),
            tokio::io::sink(),
        );

        let error = match builder.with_runtime_engine(runtime_engine).build().await {
            Ok(_) => panic!("oversized component unexpectedly compiled"),
            Err(error) => error,
        };
        assert!(format!("{error:#}").contains("component exceeds the fuel segment size bound"));
    }

    #[tokio::test]
    async fn zero_budget_builds_without_executing_a_trapping_start_function() {
        let (runtime_engine, _publisher) =
            crate::engine::runtime_engine().expect("component runtime engine");
        let bytecode =
            wat::parse_str(P3_TRAPPING_START_COMMAND).expect("parse P3 trapping-start component");
        let (builder, _handles) = Builder::ordinary(
            Program::Bytes(bytecode),
            tokio::io::empty(),
            tokio::io::sink(),
            tokio::io::sink(),
        );

        let proc = builder
            .with_runtime_engine(runtime_engine)
            .with_fuel_estimator(FuelEstimator::new_oneshot(0, 10_000, 0))
            .build()
            .await
            .expect("zero-budget construction must not execute the start function");
        assert_eq!(
            proc.store.get_fuel().expect("read zero-budget fuel"),
            0,
            "zero-budget construction installed executable physical fuel"
        );
        let error = proc
            .run()
            .await
            .expect_err("zero-budget process must reject guest execution");
        assert!(format!("{error:#}").contains("one-shot total fuel authority exhausted"));
    }

    #[tokio::test]
    async fn nonzero_start_trap_is_reported_by_run_after_preparation() {
        let (runtime_engine, _publisher) =
            crate::engine::runtime_engine().expect("component runtime engine");
        let bytecode =
            wat::parse_str(P3_TRAPPING_START_COMMAND).expect("parse P3 trapping-start component");
        let (builder, _handles) = Builder::ordinary(
            Program::Bytes(bytecode),
            tokio::io::empty(),
            tokio::io::sink(),
            tokio::io::sink(),
        );

        let proc = builder
            .with_runtime_engine(runtime_engine)
            .with_fuel_estimator(FuelEstimator::new_oneshot(1_000, 1_000, 0))
            .build()
            .await
            .expect("preparation must not execute the trapping start function");
        assert_eq!(proc.store.get_fuel().expect("read prepared Store fuel"), 0);

        let error = proc
            .run()
            .await
            .expect_err("executable instantiation must report the start trap");
        let message = format!("{error:#}");
        assert!(
            message.contains("command!start") && message.contains("`unreachable` instruction"),
            "unexpected start-function error: {message}"
        );
        assert!(!message.contains("one-shot total fuel authority exhausted"));
    }

    #[tokio::test]
    async fn start_function_uses_runtime_boundaries_and_settles_on_host_return() {
        const TOTAL_BUDGET: u64 = 25_000;
        const EPOCH_BUDGET: u64 = 12_500;

        let (runtime_engine, mut publisher) =
            crate::engine::runtime_engine().expect("component runtime engine");
        let bytecode =
            wat::parse_str(P3_COUNTED_START_COMMAND).expect("parse P3 counted-start component");
        let observer = FuelObserver::default();
        let (builder, _handles) = Builder::ordinary(
            Program::Bytes(bytecode),
            tokio::io::empty(),
            tokio::io::sink(),
            tokio::io::sink(),
        );
        let proc = builder
            .with_runtime_engine(runtime_engine)
            .with_fuel_estimator(FuelEstimator::new_oneshot(TOTAL_BUDGET, EPOCH_BUDGET, 0))
            .with_fuel_observer(observer.clone())
            .build()
            .await
            .expect("build counted-start component");
        assert_eq!(
            proc.store.get_fuel().expect("read prepared Store fuel"),
            0,
            "construction installed executable fuel before Proc::run"
        );

        let mut run = Box::pin(proc.run());
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            tokio::select! {
                result = &mut run => panic!(
                    "start function completed before its first epoch boundary: {result:?}"
                ),
                () = async {
                    loop {
                        if !observer.updates().is_empty() {
                            break;
                        }
                        tokio::task::yield_now().await;
                    }
                } => {}
            }
        })
        .await
        .expect("start function did not reach its first epoch boundary");
        let first_updates = observer.updates();
        assert_eq!(
            first_updates.len(),
            1,
            "start function did not use the normal fuel hook: {first_updates:?}"
        );
        assert_eq!(first_updates[0].decision, FuelDecision::Suspend);
        assert!(first_updates[0].consumed >= EPOCH_BUDGET);
        assert!(
            observer.slice_boundaries.load(Ordering::Relaxed) >= 2,
            "non-multiple startup authority did not use an intermediate F1 re-arm"
        );

        publisher.tick();
        tokio::time::timeout(std::time::Duration::from_secs(5), run)
            .await
            .expect("counted start function timed out after the next epoch")
            .expect("counted-start command failed");

        let updates = observer.updates();
        assert_eq!(
            updates.len(),
            2,
            "successful instantiation did not settle remaining start work: {updates:?}"
        );
        assert!(updates[1].consumed > 0, "start tail was not charged");
        let consumed: u64 = updates.iter().map(|update| update.consumed).sum();
        assert!(consumed > EPOCH_BUDGET, "start work was under-counted");
        assert!(
            consumed < TOTAL_BUDGET,
            "start work exhausted total authority"
        );
        let expected_total_remaining = TOTAL_BUDGET - consumed;
        let expected_epoch_remaining = EPOCH_BUDGET - updates[1].consumed;
        assert_eq!(
            updates[1].decision,
            FuelDecision::Grant(expected_total_remaining.min(expected_epoch_remaining)),
            "start work did not reduce the next executable grant: {updates:?}"
        );
    }

    #[tokio::test]
    async fn start_function_can_exhaust_total_authority_during_instantiation() {
        const TOTAL_BUDGET: u64 = 1_000;

        let (runtime_engine, _publisher) =
            crate::engine::runtime_engine().expect("component runtime engine");
        let bytecode =
            wat::parse_str(P3_COUNTED_START_COMMAND).expect("parse P3 counted-start component");
        let observer = FuelObserver::default();
        let (builder, _handles) = Builder::ordinary(
            Program::Bytes(bytecode),
            tokio::io::empty(),
            tokio::io::sink(),
            tokio::io::sink(),
        );
        let proc = builder
            .with_runtime_engine(runtime_engine)
            .with_fuel_estimator(FuelEstimator::new_oneshot(TOTAL_BUDGET, 10_000, 0))
            .with_fuel_observer(observer.clone())
            .build()
            .await
            .expect("build counted-start component");

        let error = proc
            .run()
            .await
            .expect_err("start function must exhaust its total authority");
        let message = format!("{error:#}");
        assert!(message.contains("one-shot total fuel authority exhausted"));
        assert!(!message.contains("all fuel consumed by WebAssembly"));

        let updates = observer.updates();
        assert_eq!(updates.len(), 1, "unexpected startup trace: {updates:?}");
        assert!(updates[0].consumed >= TOTAL_BUDGET);
        assert!(updates[0].consumed <= TOTAL_BUDGET + 4);
        assert_eq!(updates[0].decision, FuelDecision::Exhausted);
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
            ipfs::cid_identity::parse_cid(root_cid).unwrap(),
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
        assert_eq!(
            *tree.root_cid(),
            ipfs::cid_identity::parse_cid(root_cid).unwrap()
        );
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
        let (runtime_engine, _publisher) =
            crate::engine::runtime_engine().expect("component runtime engine");
        let engine = runtime_engine.engine();
        let (builder, _handles) = Builder::ordinary(
            Program::Bytes(bytecode),
            tokio::io::empty(),
            tokio::io::sink(),
            tokio::io::sink(),
        );
        let mut proc = builder
            .with_runtime_engine(runtime_engine.clone())
            .build()
            .await
            .expect("build P3 spinning component");

        // Observe an engine epoch deadline before cancelling the running task.
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
            .with_runtime_engine(runtime_engine)
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
        let (runtime_engine, mut publisher) =
            crate::engine::runtime_engine().expect("component runtime engine");
        let fuel_observer = FuelObserver::default();
        let (builder, _handles) = Builder::ordinary(
            Program::Bytes(bytecode),
            tokio::io::empty(),
            tokio::io::sink(),
            tokio::io::sink(),
        );
        let proc = builder
            .with_runtime_engine(runtime_engine.clone())
            .with_fuel_observer(fuel_observer.clone())
            .build()
            .await
            .expect("build production-config P3 spinning component");

        let mut proc_task = tokio::spawn(async move { proc.run().await });
        fuel_observer.wait_for_slice_boundaries(20).await;
        for expected_ticks in 1..=20 {
            publisher.tick();
            tokio::time::timeout(std::time::Duration::from_secs(5), async {
                while fuel_observer.observations().len() < expected_ticks {
                    tokio::task::yield_now().await;
                }
            })
            .await
            .unwrap_or_else(|_| {
                panic!("production epoch callback did not observe tick {expected_ticks}")
            });
            assert!(
                !proc_task.is_finished(),
                "scheduled P3 guest terminated after epoch tick {expected_ticks}"
            );
        }
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
            .with_runtime_engine(runtime_engine)
            .build()
            .await
            .expect("build healthy P3 component on shared Engine");
        tokio::time::timeout(std::time::Duration::from_secs(5), healthy_proc.run())
            .await
            .expect("healthy P3 component did not complete")
            .expect("healthy P3 component failed");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn oneshot_compute_suspends_across_epochs_then_exhausts_explicitly() {
        const TOTAL_BUDGET: u64 = 25_000;
        const MAX_PER_EPOCH: u64 = 10_000;
        let bytecode = wat::parse_str(include_str!(
            "../../../tests/fixtures/spinning-component.wat"
        ))
        .expect("parse P3 spinning component fixture");
        let (runtime_engine, mut publisher) =
            crate::engine::runtime_engine().expect("component runtime engine");
        let observer = FuelObserver::default();
        let (builder, _handles) = Builder::ordinary(
            Program::Bytes(bytecode),
            tokio::io::empty(),
            tokio::io::sink(),
            tokio::io::sink(),
        );
        let proc = builder
            .with_runtime_engine(runtime_engine)
            .with_fuel_estimator(FuelEstimator::new_oneshot(TOTAL_BUDGET, MAX_PER_EPOCH, 0))
            .with_fuel_observer(observer.clone())
            .build()
            .await
            .expect("build one-shot P3 spinning component");

        let polls = Arc::new(AtomicUsize::new(0));
        let mut task = tokio::spawn(PollCounter {
            future: Box::pin(proc.run()),
            polls: Arc::clone(&polls),
        });
        observer.wait_for_decision(FuelDecision::Suspend, 1).await;
        assert!(
            !task.is_finished(),
            "one-shot Cell terminated at epoch limit"
        );
        tokio::task::yield_now().await;
        let suspended_polls = polls.load(Ordering::Relaxed);
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        assert_eq!(
            polls.load(Ordering::Relaxed),
            suspended_polls,
            "suspended Proc::run future was hot-polled"
        );

        publisher.tick();
        observer.wait_for_decision(FuelDecision::Suspend, 2).await;
        assert!(
            !task.is_finished(),
            "one-shot Cell terminated after one resume"
        );

        publisher.tick();
        let error = tokio::time::timeout(std::time::Duration::from_secs(5), &mut task)
            .await
            .expect("one-shot Cell did not terminate at total exhaustion")
            .expect("one-shot process task panicked")
            .expect_err("one-shot Cell must terminate at total exhaustion");
        let message = format!("{error:#}");
        assert!(
            message.contains("one-shot total fuel authority exhausted"),
            "unexpected terminal error: {message}"
        );
        assert!(!message.contains("all fuel consumed by WebAssembly"));

        let terminal_updates: Vec<_> = observer
            .updates()
            .into_iter()
            .filter(|update| {
                matches!(
                    update.decision,
                    FuelDecision::Suspend | FuelDecision::Exhausted
                )
            })
            .collect();
        assert_eq!(
            terminal_updates
                .iter()
                .map(|update| update.decision)
                .collect::<Vec<_>>(),
            vec![
                FuelDecision::Suspend,
                FuelDecision::Suspend,
                FuelDecision::Exhausted
            ]
        );
        assert!(terminal_updates[0].consumed >= MAX_PER_EPOCH);
        assert!(terminal_updates[0].consumed <= MAX_PER_EPOCH + 4);
        assert!(terminal_updates[1].consumed >= MAX_PER_EPOCH);
        assert!(terminal_updates[1].consumed <= MAX_PER_EPOCH + 4);
        let cumulative_consumption: u64 =
            terminal_updates.iter().map(|update| update.consumed).sum();
        assert!(cumulative_consumption >= TOTAL_BUDGET);
        assert!(cumulative_consumption <= TOTAL_BUDGET + 4);
    }

    #[tokio::test]
    async fn host_call_heavy_oneshot_does_not_refill_before_epoch_wake() {
        let (runtime_engine, mut publisher) =
            crate::engine::runtime_engine().expect("component runtime engine");
        let engine = runtime_engine.engine();
        let module = wasmtime::Module::new(
            &engine,
            r#"
                (module
                    (import "host" "ping" (func $ping))
                    (func (export "run")
                        (loop $again
                            call $ping
                            br $again)))
            "#,
        )
        .expect("host-call-heavy module");
        let mut linker = wasmtime::Linker::new(&engine);
        linker
            .func_wrap(
                "host",
                "ping",
                |mut caller: wasmtime::Caller<'_, ComponentRunStates>| {
                    caller.data_mut().mark_host_call();
                },
            )
            .expect("link marked host call");

        let estimator = FuelEstimator::new_oneshot(25_000, 10_000, 0);
        let observer = FuelObserver::default();
        let mut store = Store::new(
            &engine,
            component_test_state_with_runtime(estimator, Some(observer.clone()), &runtime_engine),
        );
        install_fuel_slice_in_store(&mut store, 10_000).expect("initial host-call fuel");
        install_epoch_fuel_callback(&mut store);
        install_host_return_fuel_hook(&mut store);
        store.set_epoch_deadline(1);
        let instance = linker
            .instantiate_async(&mut store, &module)
            .await
            .expect("instantiate host-call-heavy module");
        let run = instance
            .get_typed_func::<(), ()>(&mut store, "run")
            .expect("run export");
        let mut call = Box::pin(run.call_async(&mut store, ()));

        for occurrence in 1..=2 {
            assert!(
                tokio::time::timeout(std::time::Duration::from_millis(50), &mut call)
                    .await
                    .is_err(),
                "host-call-heavy one-shot did not suspend in epoch {occurrence}"
            );
            assert_eq!(
                observer
                    .decisions
                    .lock()
                    .expect("fuel decision lock")
                    .iter()
                    .filter(|decision| **decision == FuelDecision::Suspend)
                    .count(),
                occurrence,
                "same-epoch host returns granted new authority"
            );
            publisher.tick();
        }

        let error = tokio::time::timeout(std::time::Duration::from_secs(5), &mut call)
            .await
            .expect("host-call-heavy one-shot did not reach total exhaustion")
            .expect_err("host-call-heavy one-shot must exhaust total authority");
        let message = format!("{error:#}");
        assert!(message.contains("one-shot total fuel authority exhausted"));
        assert!(!message.contains("all fuel consumed by WebAssembly"));
        drop(call);
        assert_eq!(store.data().fuel_estimator.total_remaining, Some(0));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn suspended_oneshot_terminates_when_epoch_clock_closes() {
        let bytecode = wat::parse_str(include_str!(
            "../../../tests/fixtures/spinning-component.wat"
        ))
        .expect("parse P3 spinning component fixture");
        let (runtime_engine, publisher) =
            crate::engine::runtime_engine().expect("component runtime engine");
        let observer = FuelObserver::default();
        let (builder, _handles) = Builder::ordinary(
            Program::Bytes(bytecode),
            tokio::io::empty(),
            tokio::io::sink(),
            tokio::io::sink(),
        );
        let proc = builder
            .with_runtime_engine(runtime_engine)
            .with_fuel_estimator(FuelEstimator::new_oneshot(25_000, 10_000, 0))
            .with_fuel_observer(observer.clone())
            .build()
            .await
            .expect("build one-shot P3 spinning component");

        let mut task = tokio::spawn(async move { proc.run().await });
        observer.wait_for_decision(FuelDecision::Suspend, 1).await;
        drop(publisher);
        let error = tokio::time::timeout(std::time::Duration::from_secs(5), &mut task)
            .await
            .expect("Cell remained suspended after epoch clock closed")
            .expect("one-shot process task panicked")
            .expect_err("closed epoch clock must terminate the Cell");
        assert!(
            format!("{error:#}").contains("epoch clock closed while one-shot Cell was suspended")
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn suspended_oneshot_store_drops_on_task_cancellation() {
        let bytecode = wat::parse_str(include_str!(
            "../../../tests/fixtures/spinning-component.wat"
        ))
        .expect("parse P3 spinning component fixture");
        let (runtime_engine, _publisher) =
            crate::engine::runtime_engine().expect("component runtime engine");
        let observer = FuelObserver::default();
        let (builder, _handles) = Builder::ordinary(
            Program::Bytes(bytecode),
            tokio::io::empty(),
            tokio::io::sink(),
            tokio::io::sink(),
        );
        let proc = builder
            .with_runtime_engine(runtime_engine)
            .with_fuel_estimator(FuelEstimator::new_oneshot(25_000, 10_000, 0))
            .with_fuel_observer(observer.clone())
            .build()
            .await
            .expect("build one-shot P3 spinning component");

        let mut task = tokio::spawn(async move { proc.run().await });
        observer.wait_for_decision(FuelDecision::Suspend, 1).await;
        task.abort();
        let error = tokio::time::timeout(std::time::Duration::from_secs(5), &mut task)
            .await
            .expect("suspended Cell cancellation exceeded timeout")
            .expect_err("aborted suspended Cell must return JoinError");
        assert!(error.is_cancelled());
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
        let budget = est.quantum();
        let remaining = budget - (budget * 100 / RATIO_SCALE);
        est.on_host_return(remaining);
        assert_eq!(est.avg_ratio(), 100);
    }

    #[test]
    fn fuel_estimator_io_bound_converges_to_max() {
        let mut est = FuelEstimator::new(1_000_000);
        // Repeatedly consume 0 fuel — pure I/O proxy
        for _ in 0..50 {
            let budget = est.quantum();
            est.on_host_return(budget); // consumed = 0
        }
        // Ratio → 0, budget → MAX_FUEL
        assert!(
            est.avg_ratio() < 5,
            "ratio should be near 0, got {}",
            est.avg_ratio()
        );
        assert_eq!(est.quantum(), MAX_FUEL);
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
        assert_eq!(est.quantum(), MIN_FUEL);
    }

    #[test]
    fn unmarked_low_consumption_epochs_retain_a_high_budget() {
        let mut est = FuelEstimator::new(INITIAL_FUEL);
        let mut trajectory = Vec::with_capacity(60);

        for _ in 0..60 {
            let remaining = est.quantum().saturating_sub(1_000);
            trajectory.push(est.on_flushed_epoch_boundary(remaining));
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
            est.on_flushed_epoch_boundary(0);
        }

        assert!(est.avg_ratio() > 990);
        assert_eq!(est.quantum(), MIN_FUEL);
    }

    #[test]
    fn fuel_estimator_bursty_no_spiral() {
        let mut est = FuelEstimator::new(1_000_000);
        // Alternate: I/O round (consumed=0) and compute round (consumed=budget)
        for _ in 0..100 {
            let budget = est.quantum();
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
            est.quantum() > MIN_FUEL * 10,
            "budget should not spiral to MIN_FUEL, got {}",
            est.quantum()
        );
    }

    #[test]
    fn fuel_estimator_clamps_to_min() {
        let mut est = FuelEstimator::new(MIN_FUEL);
        // All fuel consumed → ratio = 1000 → budget clamped to MIN_FUEL
        est.on_host_return(0);
        assert_eq!(est.quantum(), MIN_FUEL);
    }

    #[test]
    fn fuel_estimator_clamps_to_max() {
        let mut est = FuelEstimator::new(MAX_FUEL);
        // Zero consumed → ratio = 0 → budget = MAX_FUEL
        est.on_host_return(MAX_FUEL);
        assert_eq!(est.quantum(), MAX_FUEL);
    }

    #[test]
    fn fuel_estimator_clamps_an_oversized_initial_quantum() {
        assert_eq!(FuelEstimator::new(u64::MAX).quantum(), MAX_FUEL);
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
            let budget = est.quantum();
            let remaining = budget - (budget / 10); // 10% utilization
            est.on_host_return(remaining);
        }
        let io_ratio = est.avg_ratio();
        assert!(io_ratio < 150, "I/O ratio should be <150, got {}", io_ratio);

        // Shift to compute-heavy (ratio ~900)
        for _ in 0..20 {
            let budget = est.quantum();
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

    #[tokio::test]
    async fn oneshot_fuel_prepared_store_defers_bounded_grant_until_run() {
        for (total_budget, max_per_epoch, expected_grant) in [
            (1, 10_000, 1_u64),
            (9_999, 10_000, 9_999),
            (10_000, 10_000, 10_000),
            (25_000, 10_000, 10_000),
        ] {
            let (runtime_engine, _publisher) =
                crate::engine::runtime_engine().expect("component runtime engine");
            let bytecode = wat::parse_str(P3_NOOP_COMMAND).expect("parse P3 no-op component");
            let (builder, _handles) = Builder::ordinary(
                Program::Bytes(bytecode),
                tokio::io::empty(),
                tokio::io::sink(),
                tokio::io::sink(),
            );
            let proc = builder
                .with_runtime_engine(runtime_engine)
                .with_fuel_estimator(FuelEstimator::new_oneshot(total_budget, max_per_epoch, 0))
                .build()
                .await
                .expect("build one-shot P3 component");

            assert_eq!(
                proc.store.get_fuel().expect("read prepared Store fuel"),
                0,
                "construction installed executable fuel: \
                 total_budget={total_budget}, max_per_epoch={max_per_epoch}"
            );
            assert_eq!(
                proc.store.data().fuel_estimator.current_decision(),
                FuelDecision::Grant(expected_grant),
                "total_budget={total_budget}, max_per_epoch={max_per_epoch}"
            );
            assert_eq!(proc.store.data().fuel_estimator.authority_grant, 0);
            assert_eq!(proc.store.data().fuel_estimator.physical_installed, 0);
        }
    }

    #[test]
    fn oneshot_fuel_small_epoch_limit_does_not_panic_or_overgrant() {
        let mut est = FuelEstimator::new_oneshot(25_000, 5_000, 0);

        assert!(est.on_host_return(0) <= 5_000);
    }

    #[test]
    fn oneshot_fuel_decision_distinguishes_suspend_from_total_exhaustion() {
        let mut est = FuelEstimator::new_oneshot(25_000, 10_000, 0);
        assert_eq!(est.initial_decision(), FuelDecision::Grant(10_000));

        est.install_authority_grant(10_000);
        let update = est.settle_and_decide(YIELD_RESERVE, FuelBoundary::Slice);
        assert_eq!(update.consumed, 10_000);
        assert_eq!(update.decision, FuelDecision::Suspend);

        est.open_epoch();
        assert_eq!(est.current_decision(), FuelDecision::Grant(10_000));
        est.install_authority_grant(10_000);
        let update = est.settle_and_decide(YIELD_RESERVE, FuelBoundary::Slice);
        assert_eq!(update.decision, FuelDecision::Suspend);

        est.open_epoch();
        assert_eq!(est.current_decision(), FuelDecision::Grant(5_000));
        est.install_authority_grant(5_000);
        let update = est.settle_and_decide(YIELD_RESERVE, FuelBoundary::Slice);
        assert_eq!(update.decision, FuelDecision::Exhausted);
    }

    #[test]
    fn physical_fuel_plan_keeps_reserve_out_of_authority_ledgers() {
        let plan = FuelSlice::for_authority(1_337);
        assert_eq!(plan.authority, 1_337);
        assert_eq!(plan.physical, 1_337 + YIELD_RESERVE);
        assert_eq!(plan.yield_interval, 1_337);

        let mut est = FuelEstimator::new_oneshot(1_337, 10_000, 0);
        est.install_authority_grant(plan.authority);
        assert_eq!(est.total_remaining, Some(1_337));
        assert_eq!(est.epoch_remaining, Some(10_000));
    }

    #[test]
    fn inter_checkpoint_overshoot_is_charged_without_minting_authority() {
        let mut est = FuelEstimator::new_oneshot(25_000, 10_000, 0);
        est.install_authority_grant(10_000);

        let update = est.settle_and_decide(YIELD_RESERVE - 500, FuelBoundary::Slice);
        assert_eq!(update.consumed, 10_500);
        assert_eq!(est.total_remaining, Some(14_500));
        assert_eq!(est.epoch_remaining, Some(0));
        assert_eq!(update.decision, FuelDecision::Suspend);
    }

    #[test]
    fn oneshot_fuel_marked_returns_share_the_total_ledger() {
        let mut est = FuelEstimator::new_oneshot(25_000, 100_000, 0);
        assert_eq!(est.initial_grant(), 25_000);

        let mut charged = 0;
        for consumed in [9_616, 9_616, 5_768] {
            let current_fuel = est.physical_installed - YIELD_RESERVE - consumed;
            charged += consumed;
            let grant = est.on_host_return(current_fuel);

            assert_eq!(est.total_remaining, Some(25_000 - charged));
            assert_eq!(est.epoch_remaining, Some(100_000 - charged));
            if grant > 0 {
                assert_eq!(est.physical_installed, grant + YIELD_RESERVE);
            }
            assert!(grant <= 25_000 - charged);
        }

        assert_eq!(charged, 25_000);
        assert_eq!(est.authority_grant, 0);
        assert_eq!(est.on_host_return(0), 0);
    }

    #[test]
    fn oneshot_fuel_epoch_limit_is_cumulative_and_resets_after_settlement() {
        let mut est = FuelEstimator::new_oneshot(50_000, 10_000, 0);
        assert_eq!(est.initial_grant(), 10_000);

        let grant = est.on_host_return(6_000);
        assert_eq!(est.total_remaining, Some(46_000));
        assert_eq!(est.epoch_remaining, Some(6_000));
        assert!(grant <= 6_000);

        let grant = est.on_host_return(0);
        assert_eq!(est.total_remaining, Some(40_000));
        assert_eq!(est.epoch_remaining, Some(0));
        assert_eq!(grant, 0);

        let grant = est.on_flushed_epoch_boundary(0);
        assert_eq!(est.total_remaining, Some(40_000));
        assert_eq!(est.epoch_remaining, Some(10_000));
        assert_eq!(grant, 10_000);
    }

    #[test]
    fn oneshot_fuel_epoch_settlement_honors_the_final_partial_grant() {
        let mut est = FuelEstimator::new_oneshot(21_337, 10_000, 0);
        assert_eq!(est.initial_grant(), 10_000);

        assert_eq!(est.on_flushed_epoch_boundary(0), 10_000);
        assert_eq!(est.total_remaining, Some(11_337));
        assert_eq!(est.on_flushed_epoch_boundary(0), 1_337);
        assert_eq!(est.total_remaining, Some(1_337));
        assert_eq!(est.on_flushed_epoch_boundary(0), 0);
        assert_eq!(est.total_remaining, Some(0));
    }

    #[test]
    fn oneshot_fuel_exact_quantum_boundary_grants_zero_next() {
        let mut est = FuelEstimator::new_oneshot(20_000, 10_000, 0);
        assert_eq!(est.initial_grant(), 10_000);
        assert_eq!(est.on_flushed_epoch_boundary(0), 10_000);
        assert_eq!(est.on_flushed_epoch_boundary(0), 0);
        assert_eq!(est.total_remaining, Some(0));
        assert_eq!(est.authority_grant, 0);
    }

    #[test]
    fn oneshot_fuel_epoch_tick_does_not_double_charge_a_host_return() {
        let mut est = FuelEstimator::new_oneshot(50_000, 10_000, 0);
        assert_eq!(est.initial_grant(), 10_000);

        assert_eq!(est.on_host_return(6_000), 6_000);
        assert_eq!(est.total_remaining, Some(46_000));
        assert_eq!(est.on_flushed_epoch_boundary(6_000), 10_000);
        assert_eq!(est.total_remaining, Some(46_000));
        assert_eq!(est.epoch_remaining, Some(10_000));
    }

    #[test]
    fn oneshot_fuel_zero_work_host_return_after_epoch_tick_charges_nothing() {
        let mut est = FuelEstimator::new_oneshot(50_000, 10_000, 0);
        assert_eq!(est.initial_grant(), 10_000);

        assert_eq!(est.on_flushed_epoch_boundary(6_000), 10_000);
        assert_eq!(est.total_remaining, Some(46_000));
        assert_eq!(est.epoch_remaining, Some(10_000));

        assert_eq!(est.on_host_return(10_000), 10_000);
        assert_eq!(est.total_remaining, Some(46_000));
        assert_eq!(est.epoch_remaining, Some(10_000));
    }

    #[test]
    fn fuel_estimator_new_oneshot_basic() {
        let est = FuelEstimator::new_oneshot(5_000_000, 0, 0);
        assert_eq!(est.total_remaining, Some(5_000_000));
        assert_eq!(est.max_quantum, MAX_FUEL);
        assert_eq!(est.min_quantum, MIN_FUEL);
    }

    #[test]
    fn fuel_estimator_new_oneshot_custom_bounds() {
        let est = FuelEstimator::new_oneshot(5_000_000, 5_000_000, 50_000);
        assert_eq!(est.max_quantum, 5_000_000);
        assert_eq!(est.min_quantum, 50_000);
    }

    #[test]
    fn fuel_estimator_new_oneshot_max_clamped() {
        // maxPerEpoch > MAX_FUEL should be clamped
        let est = FuelEstimator::new_oneshot(5_000_000, 100_000_000, 0);
        assert_eq!(est.max_quantum, MAX_FUEL);
    }

    #[test]
    fn fuel_estimator_default_unchanged() {
        // Verify new() still produces the same behavior
        let est = FuelEstimator::new(INITIAL_FUEL);
        assert_eq!(est.total_remaining, None);
        assert_eq!(est.max_quantum, MAX_FUEL);
        assert_eq!(est.min_quantum, MIN_FUEL);
    }

    #[test]
    fn fuel_estimator_oneshot_uses_custom_bounds() {
        let mut est = FuelEstimator::new_oneshot(5_000_000, 5_000_000, 50_000);
        // After observing high utilization, budget should clamp to custom min, not global MIN_FUEL
        for _ in 0..20 {
            est.on_host_return(0); // simulate 100% consumption
        }
        assert!(
            est.quantum() >= 50_000,
            "should clamp to custom min quantum, got {}",
            est.quantum()
        );
        assert!(
            est.quantum() <= 5_000_000,
            "should clamp to custom max quantum, got {}",
            est.quantum()
        );
    }
}
