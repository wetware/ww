//! Explicit Cell ownership of native outbound attempts and RPC sessions.
//!
//! A token selects a live scope; it grants no capability authority. The registry
//! never selects an owner from the task currently polling an RPC operation.
use capnp::capability::Promise;
use capnp::private::capability::InvocationOrigin;
use capnp_rpc::{Connection, IncomingMessage, OutgoingMessage, VatNetwork};
use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::rc::{Rc, Weak};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use tokio::sync::watch;
use tokio::time::{Duration, Instant};
use tokio_util::sync::CancellationToken;

#[derive(Clone, Copy, Debug)]
pub struct EffectLimits {
    pub max_owners: usize,
    pub max_effects_per_cell: usize,
    pub max_total_effects: usize,
}

impl Default for EffectLimits {
    fn default() -> Self {
        Self {
            max_owners: 1024,
            max_effects_per_cell: 64,
            max_total_effects: 4096,
        }
    }
}

/// Additional admission budget for one Host lifetime. Every registry also
/// charges the shared process ceiling, including registries with explicit Host
/// budgets. Keep the same Host budget across generation registries. Unknown work
/// stays charged; only a proven terminal receipt releases its reservations.
#[derive(Clone)]
pub struct EffectBudget(Arc<HostBudget>);

struct HostBudget {
    capacity: usize,
    active: AtomicUsize,
}

impl EffectBudget {
    /// Construct an additional Host limit under the shared process ceiling.
    /// Creating a new budget cannot recover unresolved process reservations.
    pub fn new(capacity: usize) -> Self {
        Self(Arc::new(HostBudget {
            capacity,
            active: AtomicUsize::new(0),
        }))
    }

    pub fn active(&self) -> usize {
        self.0.active.load(Ordering::Acquire)
    }

    fn acquire_one(&self) -> capnp::Result<()> {
        self.0
            .active
            .try_update(Ordering::AcqRel, Ordering::Acquire, |active| {
                (active < self.0.capacity).then(|| active + 1)
            })
            .map(|_| ())
            .map_err(|_| capnp::Error::overloaded("outbound Host effect limit reached".into()))
    }

    fn release_one(&self) {
        let previous = self.0.active.fetch_sub(1, Ordering::AcqRel);
        debug_assert!(
            previous > 0,
            "terminal receipt must own a Host budget charge"
        );
    }
}

impl Default for EffectBudget {
    /// The process-lifetime ceiling, also charged by explicit Host budgets.
    /// Registry recreation cannot clear unresolved or lost native obligations.
    fn default() -> Self {
        static DEFAULT_BUDGET: OnceLock<EffectBudget> = OnceLock::new();
        DEFAULT_BUDGET
            .get_or_init(|| EffectBudget::new(4096))
            .clone()
    }
}

/// Fixed-size retained evidence. Cumulative counters saturate rather than grow
/// an unbounded history. `failed` means required native or worker completion evidence was lost.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct EffectsSnapshot {
    pub closed: bool,
    pub active: usize,
    pub committed: usize,
    pub joined: usize,
    pub failed: bool,
    pub rpc_errors: usize,
}

/// Maximum wait after the first scope close for local cleanup evidence.
/// Expiry reports unresolved work; it does not cancel work or release capacity.
pub const EFFECT_CLEANUP_GRACE: Duration = Duration::from_secs(30);

#[derive(Clone)]
pub struct EffectsCompletion {
    receiver: watch::Receiver<EffectsSnapshot>,
    closure: EffectsCloseHandle,
}

impl EffectsCompletion {
    pub fn snapshot(&self) -> EffectsSnapshot {
        *self.receiver.borrow()
    }

    /// Observe terminal receipts or report unresolved cleanup at the scope's
    /// first-close deadline. A later observation may see eventual completion;
    /// it cannot change an earlier failed Cell cleanup certificate.
    pub async fn wait(&self) -> capnp::Result<EffectsSnapshot> {
        let mut receiver = self.receiver.clone();
        loop {
            let snapshot = *receiver.borrow_and_update();
            if snapshot.failed {
                return Err(capnp::Error::failed(
                    "outbound completion evidence lost before certified cleanup".into(),
                ));
            }
            // Cross-thread closure alone cannot certify this snapshot. An
            // admitted count may not yet have reached the watch publisher.
            // Local closure publication follows all prior local admissions.
            if snapshot.closed && snapshot.active == 0 {
                return Ok(snapshot);
            }
            let closed_at = *self
                .closure
                .gate
                .lock()
                .unwrap_or_else(|error| error.into_inner());
            let changed = if let Some(closed_at) = closed_at {
                let deadline = closed_at + EFFECT_CLEANUP_GRACE;
                if Instant::now() >= deadline {
                    return Err(capnp::Error::failed(format!(
                        "outbound cleanup unresolved after {EFFECT_CLEANUP_GRACE:?}: {} active obligations; local closure acknowledged={}",
                        snapshot.active, snapshot.closed,
                    )));
                }
                tokio::select! {
                    biased;
                    changed = receiver.changed() => Some(changed),
                    _ = tokio::time::sleep_until(deadline) => None,
                }
            } else {
                tokio::select! {
                    biased;
                    changed = receiver.changed() => Some(changed),
                    _ = self.closure.cancelled.cancelled() => None,
                }
            };
            if matches!(changed, Some(Err(_))) {
                return Err(capnp::Error::failed(
                    "outbound completion publisher lost".into(),
                ));
            }
        }
    }
}

/// May cross into an executor before a scope exists. Dropping an unbound
/// publisher certifies an empty scope; after binding, loss cannot certify work.
pub struct EffectsPublisher {
    sender: watch::Sender<EffectsSnapshot>,
    closure: EffectsCloseHandle,
    bound: bool,
}

pub fn effects_completion() -> (EffectsPublisher, EffectsCompletion) {
    let (sender, receiver) = watch::channel(EffectsSnapshot::default());
    let closure = EffectsCloseHandle {
        gate: Arc::new(Mutex::new(None)),
        cancelled: CancellationToken::new(),
    };
    (
        EffectsPublisher {
            sender,
            closure: closure.clone(),
            bound: false,
        },
        EffectsCompletion { receiver, closure },
    )
}

impl Drop for EffectsPublisher {
    fn drop(&mut self) {
        self.closure.close();
        let mut state = *self.sender.borrow();
        if !state.closed || state.active != 0 {
            state.closed = true;
            state.failed |= self.bound;
            self.sender.send_replace(state);
        }
    }
}

/// Every local registry uses this directory. It resolves explicit request tokens,
/// never the task or executor currently polling the request.
#[derive(Default)]
struct OriginDirectory {
    owners: RefCell<HashMap<InvocationOrigin, Weak<Scope>>>,
}

thread_local! {
    static ORIGIN_DIRECTORY: Rc<OriginDirectory> = Rc::default();
}

const MAX_PROCESS_OWNERS: usize = 4096;
static LIVE_PROCESS_OWNERS: AtomicUsize = AtomicUsize::new(0);

/// A live origin consumes a process slot independently of its effect charges.
/// Retiring an origin releases only this slot; unresolved effects stay charged.
struct OwnerSlot;

impl OwnerSlot {
    fn acquire() -> capnp::Result<Self> {
        LIVE_PROCESS_OWNERS
            .try_update(Ordering::AcqRel, Ordering::Acquire, |active| {
                (active < MAX_PROCESS_OWNERS).then(|| active + 1)
            })
            .map(|_| Self)
            .map_err(|_| capnp::Error::overloaded("outbound process owner limit reached".into()))
    }
}

impl Drop for OwnerSlot {
    fn drop(&mut self) {
        let previous = LIVE_PROCESS_OWNERS.fetch_sub(1, Ordering::AcqRel);
        debug_assert!(
            previous > 0,
            "live origin must retain its process owner slot"
        );
    }
}

struct Registry {
    directory: Rc<OriginDirectory>,
    limits: EffectLimits,
    budget: EffectBudget,
    process_budget: EffectBudget,
    owners: RefCell<HashMap<InvocationOrigin, Weak<Scope>>>,
    active: Cell<usize>,
}

impl Registry {
    fn acquire(&self) -> capnp::Result<()> {
        self.process_budget.acquire_one().map_err(|_| {
            capnp::Error::overloaded("outbound process effect limit reached".into())
        })?;
        if !Arc::ptr_eq(&self.budget.0, &self.process_budget.0) {
            if let Err(error) = self.budget.acquire_one() {
                self.process_budget.release_one();
                return Err(error);
            }
        }
        Ok(())
    }

    fn release(&self) {
        if !Arc::ptr_eq(&self.budget.0, &self.process_budget.0) {
            self.budget.release_one();
        }
        self.process_budget.release_one();
    }
}

#[derive(Clone)]
pub struct OutboundRegistry(Rc<Registry>);

impl Default for OutboundRegistry {
    fn default() -> Self {
        Self::new(EffectLimits::default())
    }
}

impl OutboundRegistry {
    pub fn new(limits: EffectLimits) -> Self {
        Self::new_with_budget(limits, EffectBudget::default())
    }

    pub fn new_with_budget(limits: EffectLimits, budget: EffectBudget) -> Self {
        Self(Rc::new(Registry {
            directory: ORIGIN_DIRECTORY.with(Rc::clone),
            limits,
            budget,
            process_budget: EffectBudget::default(),
            owners: RefCell::new(HashMap::new()),
            active: Cell::new(0),
        }))
    }

    pub fn register(&self) -> capnp::Result<CellEffects> {
        self.register_with_completion(effects_completion().0)
    }

    pub fn register_with_completion(
        &self,
        mut publisher: EffectsPublisher,
    ) -> capnp::Result<CellEffects> {
        let mut owners = self.0.owners.borrow_mut();
        if owners.len() >= self.0.limits.max_owners {
            return Err(capnp::Error::overloaded(
                "outbound owner limit reached".into(),
            ));
        }
        let owner_slot = OwnerSlot::acquire()?;
        publisher.bound = true;
        let scope = Rc::new(Scope {
            registry: Rc::downgrade(&self.0),
            owner_slot: RefCell::new(Some(owner_slot)),
            origin: InvocationOrigin::new(),
            closed: publisher.closure.cancelled.clone(),
            gate: publisher.closure.gate.clone(),
            state: Cell::new(EffectsSnapshot::default()),
            publisher,
        });
        owners.insert(scope.origin.clone(), Rc::downgrade(&scope));
        self.0
            .directory
            .owners
            .borrow_mut()
            .insert(scope.origin.clone(), Rc::downgrade(&scope));
        Ok(CellEffects {
            scope,
            registry: self.clone(),
        })
    }

    pub fn total_active(&self) -> usize {
        self.0.active.get()
    }

    pub fn live_snapshots(&self) -> Vec<(InvocationOrigin, EffectsSnapshot)> {
        self.0
            .owners
            .borrow()
            .iter()
            .filter_map(|(origin, scope)| {
                scope
                    .upgrade()
                    .map(|scope| (origin.clone(), scope.snapshot()))
            })
            .collect()
    }

    pub(crate) fn admit(&self, origin: Option<InvocationOrigin>) -> capnp::Result<Admission> {
        let origin =
            origin.ok_or_else(|| capnp::Error::failed("missing invocation origin".into()))?;
        let scope = self
            .0
            .directory
            .owners
            .borrow()
            .get(&origin)
            .and_then(Weak::upgrade)
            .ok_or_else(|| capnp::Error::failed("unknown or retired invocation origin".into()))?;
        // The grant's registry is only a directory entry point. Limits and
        // charges follow the initiating scope's home registry, including when
        // the original VatClient has been delegated across Runtime instances.
        let registry =
            OutboundRegistry(scope.registry.upgrade().ok_or_else(|| {
                capnp::Error::failed("invocation origin owner registry lost".into())
            })?);
        let closed = scope.gate.lock().unwrap_or_else(|error| error.into_inner());
        if closed.is_some() {
            return Err(capnp::Error::failed("outbound Cell scope closed".into()));
        }
        if scope.snapshot().active >= registry.0.limits.max_effects_per_cell
            || registry.total_active() >= registry.0.limits.max_total_effects
        {
            return Err(capnp::Error::overloaded(
                "outbound effect limit reached".into(),
            ));
        }
        registry.0.acquire()?;
        registry.0.active.set(registry.total_active() + 1);
        let mut state = scope.snapshot();
        state.active += 1;
        scope.state.set(state);
        drop(closed);
        scope.publish();
        Ok(Admission {
            scope,
            registry,
            submission_authorized: Rc::new(Cell::new(false)),
            completed: false,
        })
    }
}

pub struct CellEffects {
    scope: Rc<Scope>,
    registry: OutboundRegistry,
}

impl CellEffects {
    pub fn origin(&self) -> InvocationOrigin {
        self.scope.origin.clone()
    }
    pub fn completion(&self) -> EffectsCompletion {
        EffectsCompletion {
            receiver: self.scope.publisher.sender.subscribe(),
            closure: self.scope.publisher.closure.clone(),
        }
    }
    pub fn close_handle(&self) -> EffectsCloseHandle {
        self.scope.publisher.closure.clone()
    }
    pub fn close(&self) {
        // Publish closure and retire without awaiting or running user callbacks.
        self.close_handle().close();
        self.scope.update(|state| state.closed = true);
        self.registry
            .0
            .owners
            .borrow_mut()
            .remove(&self.scope.origin);
        self.registry
            .0
            .directory
            .owners
            .borrow_mut()
            .remove(&self.scope.origin);
        // Idempotent close releases the live-owner slot exactly once. This
        // does not release any admitted effect or failed-cleanup charge.
        self.scope.owner_slot.borrow_mut().take();
    }
    pub async fn close_and_join(&self) -> capnp::Result<EffectsSnapshot> {
        self.close();
        self.completion().wait().await
    }
}

/// Cross-thread closure authority without ownership of the Cell or its effects.
#[derive(Clone)]
pub struct EffectsCloseHandle {
    gate: Arc<Mutex<Option<Instant>>>,
    cancelled: CancellationToken,
}
impl EffectsCloseHandle {
    pub(crate) fn is_closed(&self) -> bool {
        self.gate
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .is_some()
    }

    /// Stop new admission, submission authorization, session commit and caller
    /// delivery decisions. Authorized attempts remain owned during the drain.
    /// This starts cleanup; only EffectsCompletion can certify its outcome.
    pub fn close(&self) {
        {
            let mut closed_at = self.gate.lock().unwrap_or_else(|error| error.into_inner());
            closed_at.get_or_insert_with(Instant::now);
        }
        // Cancellation can invoke wakers; release the admission gate first.
        self.cancelled.cancel();
    }
}

impl Drop for CellEffects {
    fn drop(&mut self) {
        self.close();
    }
}

pub(crate) struct Scope {
    registry: Weak<Registry>,
    owner_slot: RefCell<Option<OwnerSlot>>,
    origin: InvocationOrigin,
    pub(crate) closed: CancellationToken,
    gate: Arc<Mutex<Option<Instant>>>,
    state: Cell<EffectsSnapshot>,
    publisher: EffectsPublisher,
}

impl Scope {
    pub(crate) fn close_handle(&self) -> EffectsCloseHandle {
        self.publisher.closure.clone()
    }

    fn snapshot(&self) -> EffectsSnapshot {
        self.state.get()
    }
    fn publish(&self) {
        self.publisher.sender.send_replace(self.snapshot());
    }
    fn update(&self, update: impl FnOnce(&mut EffectsSnapshot)) {
        let mut state = self.snapshot();
        update(&mut state);
        self.state.set(state);
        self.publish();
    }
    pub(crate) fn is_closed(&self) -> bool {
        self.gate
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .is_some()
    }
    pub(crate) fn commit(&self) -> bool {
        // No constructors, hook calls, drops or wake callbacks under this gate.
        // The caller already owns the prepared driver and must join rejection.
        let closed = self.gate.lock().unwrap_or_else(|error| error.into_inner());
        if closed.is_some() {
            return false;
        }
        let mut state = self.snapshot();
        state.committed = state.committed.saturating_add(1);
        self.state.set(state);
        drop(closed);
        self.publish();
        true
    }
    fn lost(&self) {
        self.publisher.closure.close();
        self.update(|state| {
            state.closed = true;
            state.failed = true;
        });
    }
}

/// Stays with the join monitor, including before its first poll. Only that
/// monitor may release a permit after joining the actual attempt worker.
pub(crate) struct Admission {
    pub(crate) scope: Rc<Scope>,
    registry: OutboundRegistry,
    submission_authorized: Rc<Cell<bool>>,
    completed: bool,
}

/// One attempt's logical submission decision, retained with its Admission.
/// Authorization is not evidence of native enqueue or handler emission.
pub(crate) struct Submission {
    scope: Rc<Scope>,
    authorized: Rc<Cell<bool>>,
}

impl Submission {
    /// Order one irreversible supervised start against Cell closure. A winner
    /// may poll/enqueue after closure; its Admission must remain charged until
    /// terminal completion and worker join. No backend runs under this gate.
    pub(crate) fn authorize(self) -> bool {
        // The current worker consumes exactly one token. If an internal caller
        // issues another token, fail the worker instead of treating prior work
        // as a never-started cancellation and releasing its receipt.
        assert!(
            !self.authorized.get(),
            "one submission authorization per admission"
        );
        let closed = self
            .scope
            .gate
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if closed.is_some() {
            return false;
        }
        self.authorized.set(true);
        true
    }
}

#[derive(Default)]
pub(crate) struct EffectOutcome {
    pub(crate) session_joined: bool,
    pub(crate) lost: bool,
    pub(crate) rpc_error: bool,
}

impl Admission {
    pub(crate) fn submission(&self) -> Submission {
        Submission {
            scope: self.scope.clone(),
            authorized: self.submission_authorized.clone(),
        }
    }

    pub(crate) fn finish(mut self, outcome: EffectOutcome) {
        if outcome.lost {
            // Unknown backend/driver completion quarantines capacity. An outer
            // attempt-task join alone is not a native cancellation receipt.
            self.scope.lost();
        } else {
            self.release(outcome);
        }
        self.completed = true;
    }
    fn release(&self, outcome: EffectOutcome) {
        self.registry.0.release();
        self.registry.0.active.set(self.registry.total_active() - 1);
        self.scope.update(|state| {
            state.active -= 1;
            state.joined = state
                .joined
                .saturating_add(usize::from(outcome.session_joined));
            state.failed |= outcome.lost;
            state.rpc_errors = state
                .rpc_errors
                .saturating_add(usize::from(outcome.rpc_error));
        });
    }
}

impl Drop for Admission {
    fn drop(&mut self) {
        if !self.completed {
            self.scope.lost();
        }
    }
}

/// Trusted ingress classification only. Session ownership never calls this
/// helper for remote ingress merely because a Cell opened that connection.
pub fn with_origin<VatId: 'static>(
    inner: Box<dyn VatNetwork<VatId>>,
    origin: InvocationOrigin,
) -> Box<dyn VatNetwork<VatId>> {
    Box::new(OriginNetwork { inner, origin })
}

struct OriginNetwork<VatId> {
    inner: Box<dyn VatNetwork<VatId>>,
    origin: InvocationOrigin,
}
impl<VatId: 'static> VatNetwork<VatId> for OriginNetwork<VatId> {
    fn connect(&mut self, vat: VatId) -> Option<Box<dyn Connection<VatId>>> {
        self.inner.connect(vat).map(|inner| {
            Box::new(OriginConnection {
                inner,
                origin: self.origin.clone(),
            }) as Box<dyn Connection<VatId>>
        })
    }
    fn accept(&mut self) -> Promise<Box<dyn Connection<VatId>>, capnp::Error> {
        let connection = self.inner.accept();
        let origin = self.origin.clone();
        Promise::from_future(async move {
            Ok(Box::new(OriginConnection {
                inner: connection.await?,
                origin,
            }) as Box<dyn Connection<VatId>>)
        })
    }
    fn drive_until_shutdown(&mut self) -> Promise<(), capnp::Error> {
        self.inner.drive_until_shutdown()
    }
}
struct OriginConnection<VatId> {
    inner: Box<dyn Connection<VatId>>,
    origin: InvocationOrigin,
}
impl<VatId> Connection<VatId> for OriginConnection<VatId> {
    fn incoming_invocation_origin(&self) -> Option<InvocationOrigin> {
        Some(self.origin.clone())
    }
    fn get_peer_vat_id(&self) -> VatId {
        self.inner.get_peer_vat_id()
    }
    fn new_outgoing_message(&mut self, size: u32) -> Box<dyn OutgoingMessage> {
        self.inner.new_outgoing_message(size)
    }
    fn receive_incoming_message(
        &mut self,
    ) -> Promise<Option<Box<dyn IncomingMessage>>, capnp::Error> {
        self.inner.receive_incoming_message()
    }
    fn new_stream(
        &mut self,
    ) -> (
        Box<dyn capnp_rpc::FlowController>,
        Promise<(), capnp::Error>,
    ) {
        self.inner.new_stream()
    }
    fn shutdown(&mut self, result: capnp::Result<()>) -> Promise<(), capnp::Error> {
        self.inner.shutdown(result)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::managed_rpc::{ManagedRpc, ManagedRpcSystem};
    use crate::vat_dial::connect_owned;
    use authority::system_capnp::process;
    use futures::{io::AsyncReadExt as _, FutureExt};
    use tokio::sync::oneshot;
    use tokio_util::compat::TokioAsyncReadCompatExt;
    type Stream = tokio_util::compat::Compat<tokio::io::DuplexStream>;

    #[test]
    fn cross_registry_admission_charges_initiating_home_limits_and_budget() {
        let granting_budget = EffectBudget::new(0);
        let home_budget = EffectBudget::new(3);
        let granting = OutboundRegistry::new_with_budget(
            EffectLimits {
                max_owners: 1,
                max_effects_per_cell: 0,
                max_total_effects: 0,
            },
            granting_budget.clone(),
        );
        let home = OutboundRegistry::new_with_budget(
            EffectLimits {
                max_owners: 3,
                max_effects_per_cell: 1,
                max_total_effects: 2,
            },
            home_budget.clone(),
        );
        let a = granting.register().unwrap();
        let b = home.register().unwrap();
        let c = home.register().unwrap();
        let d = home.register().unwrap();
        let first = granting
            .admit(Some(b.origin()))
            .expect("B owns A's delegated invocation");
        assert_eq!((granting.total_active(), home.total_active()), (0, 1));
        assert_eq!((granting_budget.active(), home_budget.active()), (0, 1));
        assert!(
            home.admit(Some(b.origin())).is_err(),
            "B's per-Cell limit follows B"
        );
        assert!(
            home.admit(Some(a.origin())).is_err(),
            "A cannot borrow B's limits"
        );
        let second = granting.admit(Some(c.origin())).unwrap();
        assert_eq!((granting.total_active(), home.total_active()), (0, 2));
        assert!(
            granting.admit(Some(d.origin())).is_err(),
            "home registry total limits all of its Cells"
        );
        assert_eq!(home_budget.active(), 2);
        a.close();
        assert!(
            first.submission().authorize(),
            "A closure cannot close B's admitted work"
        );
        first.finish(EffectOutcome::default());
        let recovered = granting.admit(Some(b.origin())).unwrap();
        second.finish(EffectOutcome::default());
        recovered.finish(EffectOutcome::default());
        assert_eq!((granting_budget.active(), home_budget.active()), (0, 0));
    }

    #[test]
    fn cross_registry_lost_worker_quarantines_home_budget_after_retirement() {
        let granting = limits(1, 4, 4);
        let budget = EffectBudget::new(1);
        let home = OutboundRegistry::new_with_budget(EffectLimits::default(), budget.clone());
        let owner = home.register().unwrap();
        let receipt = granting.admit(Some(owner.origin())).unwrap();
        drop(receipt);
        assert!(owner.completion().snapshot().failed);
        assert_eq!(
            (
                granting.total_active(),
                home.total_active(),
                budget.active()
            ),
            (0, 1, 1)
        );
        owner.close();
        drop(owner);
        drop(home);
        let replacement =
            OutboundRegistry::new_with_budget(EffectLimits::default(), budget.clone());
        let next = replacement.register().unwrap();
        assert!(granting.admit(Some(next.origin())).is_err());
        assert_eq!(
            budget.active(),
            1,
            "lost evidence cannot release the initiating owner's budget"
        );
    }

    #[test]
    fn cross_registry_lookup_rejects_closed_retired_and_unknown_origins() {
        let granting = limits(1, 1, 1);
        let home = limits(1, 1, 1);
        let owner = home.register().unwrap();
        let old = owner.origin();
        let close = owner.close_handle();
        std::thread::spawn(move || close.close()).join().unwrap();
        assert!(granting.admit(Some(old.clone())).is_err());
        owner.close();
        owner.close();
        let replacement = home.register().unwrap();
        assert_ne!(old, replacement.origin());
        assert!(granting.admit(Some(old)).is_err());
        assert!(granting.admit(Some(InvocationOrigin::new())).is_err());
        assert!(granting.admit(None).is_err());
        granting
            .admit(Some(replacement.origin()))
            .unwrap()
            .finish(EffectOutcome::default());
    }

    #[test]
    fn cross_registry_admission_retains_home_without_a_strong_cycle() {
        let granting = limits(1, 1, 1);
        let home = limits(1, 1, 1);
        let weak_home = Rc::downgrade(&home.0);
        let owner = home.register().unwrap();
        let old = owner.origin();
        let admission = granting.admit(Some(old.clone())).unwrap();
        drop(home);
        owner.close();
        drop(owner);
        assert!(
            weak_home.upgrade().is_some(),
            "pending receipt retains its home accounting"
        );
        assert!(granting.admit(Some(old)).is_err());
        admission.finish(EffectOutcome::default());
        assert!(
            weak_home.upgrade().is_none(),
            "directory and scope must not retain a registry cycle"
        );
    }

    #[test]
    #[ignore = "requires a dedicated test process to exhaust the process owner budget"]
    fn process_owner_limit_spans_threads_and_recovers_after_idempotent_close() {
        let admitted = Arc::new(std::sync::Barrier::new(5));
        let release = Arc::new(std::sync::Barrier::new(5));
        let threads: Vec<_> = (0..4)
            .map(|_| {
                let admitted = admitted.clone();
                let release = release.clone();
                std::thread::spawn(move || {
                    let registry = OutboundRegistry::default();
                    let owners: Vec<_> = (0..1024).map(|_| registry.register().unwrap()).collect();
                    admitted.wait();
                    release.wait();
                    for owner in owners {
                        owner.close();
                        owner.close();
                    }
                })
            })
            .collect();
        admitted.wait();
        let registry = OutboundRegistry::default();
        let exhausted = registry.register().is_err();
        release.wait();
        for thread in threads {
            thread.join().unwrap();
        }
        assert!(
            exhausted,
            "4096 live process owners must reject another registry"
        );
        let recovered = registry.register().unwrap();
        recovered.close();
    }

    #[test]
    fn closure_winning_submission_authorization_keeps_receipt_until_join() {
        let registry = limits(1, 1, 1);
        let owner = registry.register().unwrap();
        let admission = registry.admit(Some(owner.origin())).unwrap();
        let submission = admission.submission();
        let close = owner.close_handle();
        std::thread::spawn(move || close.close()).join().unwrap();
        assert!(!submission.authorize());
        assert!(!admission.submission_authorized.get());
        assert_eq!(registry.total_active(), 1);
        assert!(registry.admit(Some(owner.origin())).is_err());
        // This unit test supplies the no-start worker's terminal join outcome.
        // Real worker ordering is exercised separately by the real-swarm tests.
        admission.finish(EffectOutcome::default());
        assert_eq!(registry.total_active(), 0);
    }

    #[test]
    fn authorization_winning_closure_remains_a_charged_drain_obligation() {
        let registry = limits(1, 1, 1);
        let owner = registry.register().unwrap();
        let admission = registry.admit(Some(owner.origin())).unwrap();
        assert!(admission.submission().authorize());
        assert!(admission.submission_authorized.get());
        let close = owner.close_handle();
        std::thread::spawn(move || close.close()).join().unwrap();
        assert_eq!(registry.total_active(), 1);
        assert!(registry.admit(Some(owner.origin())).is_err());
        // A repeated token cannot turn a started attempt into a no-start result.
        assert!(std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            admission.submission().authorize()
        }))
        .is_err());
        assert_eq!(registry.total_active(), 1);
        admission.finish(EffectOutcome::default());
        assert_eq!(registry.total_active(), 0);
    }
    thread_local! {
        static CLOSE_ON_CONSTRUCTION: RefCell<Option<EffectsCloseHandle>> = const { RefCell::new(None) };
    }
    struct ClosingClient(capnp::capability::Client);
    impl capnp::capability::FromClientHook for ClosingClient {
        fn new(hook: Box<dyn capnp::private::capability::ClientHook>) -> Self {
            CLOSE_ON_CONSTRUCTION.with(|slot| slot.borrow_mut().take().unwrap().close());
            Self(capnp::capability::Client::new(hook))
        }
        fn into_client_hook(self) -> Box<dyn capnp::private::capability::ClientHook> {
            self.0.hook
        }
        fn as_client_hook(&self) -> &dyn capnp::private::capability::ClientHook {
            &*self.0.hook
        }
    }
    struct PanickingClient;
    impl capnp::capability::FromClientHook for PanickingClient {
        fn new(_: Box<dyn capnp::private::capability::ClientHook>) -> Self {
            panic!("client constructor panicked after ManagedRpc spawn")
        }
        fn into_client_hook(self) -> Box<dyn capnp::private::capability::ClientHook> {
            unreachable!()
        }
        fn as_client_hook(&self) -> &dyn capnp::private::capability::ClientHook {
            unreachable!()
        }
    }

    #[test]
    fn client_constructor_can_close_same_scope_without_deadlock_or_commit() {
        let (done, finished) = std::sync::mpsc::channel();
        // A separate thread gives a watchdog for a synchronous reentrant
        // deadlock; an async timeout on the affected LocalSet cannot do so.
        std::thread::spawn(move || {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            runtime.block_on(tokio::task::LocalSet::new().run_until(async {
                let registry = OutboundRegistry::default();
                let owner = registry.register().unwrap();
                CLOSE_ON_CONSTRUCTION.with(|slot| *slot.borrow_mut() = Some(owner.close_handle()));
                let (stream, _peer) = tokio::io::duplex(8192);
                let request =
                    connect_owned::<_, ClosingClient, _>(&registry, Some(owner.origin()), async {
                        Ok(stream.compat())
                    })
                    .unwrap();
                assert!(request.await.is_err());
                let snapshot = owner.close_and_join().await.unwrap();
                assert_eq!(snapshot.committed, 0);
                assert_eq!(registry.total_active(), 0);
                done.send(()).unwrap();
            }));
        });
        finished
            .recv_timeout(std::time::Duration::from_secs(4))
            .expect("same-thread constructor close must not deadlock admission gate");
    }

    fn limits(owners: usize, cell: usize, total: usize) -> OutboundRegistry {
        OutboundRegistry::new(EffectLimits {
            max_owners: owners,
            max_effects_per_cell: cell,
            max_total_effects: total,
        })
    }

    #[test]
    fn retirement_and_capacity_do_not_reuse_generation_identity() {
        let registry = limits(1, 1, 1);
        let a = registry.register().unwrap();
        let old = a.origin();
        assert!(registry.register().is_err());
        a.close();
        let b = registry.register().unwrap();
        assert_ne!(old, b.origin());
        assert!(registry.admit(Some(old)).is_err());
        assert!(registry.admit(None).is_err());
        assert!(registry.admit(Some(InvocationOrigin::new())).is_err());
        let permit = registry.admit(Some(b.origin())).unwrap();
        assert!(registry.admit(Some(b.origin())).is_err());
        permit.finish(EffectOutcome::default());
        assert_eq!(registry.total_active(), 0);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn unbound_publisher_is_empty_completion_and_cross_thread_close_stops_admission() {
        fn assert_send<T: Send>() {}
        assert_send::<EffectsPublisher>();
        assert_send::<EffectsCompletion>();
        assert_send::<EffectsCloseHandle>();
        let (publisher, completion) = effects_completion();
        drop(publisher);
        assert_eq!(completion.wait().await.unwrap().active, 0);
        let registry = OutboundRegistry::default();
        let owner = registry.register().unwrap();
        let close = owner.close_handle();
        std::thread::spawn(move || close.close()).join().unwrap();
        assert!(registry.admit(Some(owner.origin())).is_err());
        assert!(!owner.scope.commit());
        assert_eq!(owner.close_and_join().await.unwrap().committed, 0);
    }

    #[test]
    fn shared_host_budget_recovers_only_after_terminal_receipt() {
        let budget = EffectBudget::new(1);
        let limits = EffectLimits {
            max_owners: 1,
            max_effects_per_cell: 1,
            max_total_effects: 1,
        };
        let first = OutboundRegistry::new_with_budget(limits, budget.clone());
        let second = OutboundRegistry::new_with_budget(limits, budget.clone());
        let a = first.register().unwrap();
        let b = second.register().unwrap();
        let admission = first.admit(Some(a.origin())).unwrap();
        assert_eq!(budget.active(), 1);
        assert!(second.admit(Some(b.origin())).is_err());
        assert_eq!(
            second.total_active(),
            0,
            "failed Host admission must not charge registry"
        );
        a.close();
        drop(a);
        drop(first);
        assert!(second.admit(Some(b.origin())).is_err());
        admission.finish(EffectOutcome::default());
        assert_eq!(budget.active(), 0);
        let recovered = second.admit(Some(b.origin())).unwrap();
        assert_eq!(budget.active(), 1);
        recovered.finish(EffectOutcome::default());
        assert_eq!(budget.active(), 0);
    }

    #[test]
    fn lost_host_obligation_survives_registry_recreation() {
        let budget = EffectBudget::new(1);
        let registry = OutboundRegistry::new_with_budget(EffectLimits::default(), budget.clone());
        let owner = registry.register().unwrap();
        let admission = registry.admit(Some(owner.origin())).unwrap();
        drop(admission);
        assert!(owner.completion().snapshot().failed);
        drop(owner);
        drop(registry);
        assert_eq!(budget.active(), 1);
        let replacement =
            OutboundRegistry::new_with_budget(EffectLimits::default(), budget.clone());
        let owner = replacement.register().unwrap();
        assert!(replacement.admit(Some(owner.origin())).is_err());
        assert_eq!(replacement.total_active(), 0);
        assert_eq!(
            budget.active(),
            1,
            "unknown work cannot be recovered by replacing its registry"
        );
    }

    #[test]
    fn concurrent_registries_cannot_exceed_shared_host_budget() {
        let budget = EffectBudget::new(1);
        let start = Arc::new(std::sync::Barrier::new(3));
        let admitted = Arc::new(std::sync::Barrier::new(3));
        let release = Arc::new(std::sync::Barrier::new(3));
        let winners = Arc::new(AtomicUsize::new(0));
        let threads: Vec<_> = (0..2)
            .map(|_| {
                let budget = budget.clone();
                let start = start.clone();
                let admitted = admitted.clone();
                let release = release.clone();
                let winners = winners.clone();
                std::thread::spawn(move || {
                    let registry =
                        OutboundRegistry::new_with_budget(EffectLimits::default(), budget);
                    let owner = registry.register().unwrap();
                    start.wait();
                    let admission = registry.admit(Some(owner.origin())).ok();
                    if admission.is_some() {
                        winners.fetch_add(1, Ordering::Relaxed);
                    }
                    admitted.wait();
                    release.wait();
                    if let Some(admission) = admission {
                        admission.finish(EffectOutcome::default());
                    }
                })
            })
            .collect();
        start.wait();
        admitted.wait();
        let active = budget.active();
        let admitted_count = winners.load(Ordering::Relaxed);
        release.wait();
        for thread in threads {
            thread.join().unwrap();
        }
        assert_eq!(active, 1);
        assert_eq!(admitted_count, 1);
        assert_eq!(budget.active(), 0);
    }

    // Run this in its own test process: it intentionally fills the process-wide
    // default budget, so unrelated parallel tests must not share this process.
    #[test]
    #[ignore = "requires a dedicated test process to exhaust the default Host budget"]
    fn default_budget_bounds_work_across_registry_recreation() {
        let limits = EffectLimits {
            max_owners: 1,
            max_effects_per_cell: 4096,
            max_total_effects: 4096,
        };
        let registry = OutboundRegistry::new(limits);
        let owner = registry.register().unwrap();
        let mut admitted = Vec::new();
        for _ in 0..4096 {
            admitted.push(registry.admit(Some(owner.origin())).unwrap());
        }
        owner.close();
        drop(owner);
        drop(registry);
        let replacement = OutboundRegistry::new(limits);
        let next = replacement.register().unwrap();
        assert!(
            replacement.admit(Some(next.origin())).is_err(),
            "recreating a registry must not create capacity while old work remains unresolved"
        );
        admitted.pop().unwrap().finish(EffectOutcome::default());
        let recovered = replacement.admit(Some(next.origin())).unwrap();
        recovered.finish(EffectOutcome::default());
        for admission in admitted {
            admission.finish(EffectOutcome::default());
        }
    }

    // Explicit Host limits cannot multiply the process ceiling. This test
    // fills the real ceiling, so it must run without other tests in the process.
    #[test]
    #[ignore = "requires a dedicated test process to exhaust the process effect budget"]
    fn explicit_host_budgets_cannot_bypass_process_limit() {
        let limits = EffectLimits {
            max_owners: 1,
            max_effects_per_cell: 4096,
            max_total_effects: 4096,
        };
        let first_budget = EffectBudget::new(4096);
        let second_budget = EffectBudget::new(4096);
        let first = OutboundRegistry::new_with_budget(limits, first_budget.clone());
        let second = OutboundRegistry::new_with_budget(limits, second_budget.clone());
        let a = first.register().unwrap();
        let b = second.register().unwrap();
        let mut receipts = Vec::new();
        for _ in 0..2048 {
            receipts.push(first.admit(Some(a.origin())).unwrap());
            receipts.push(first.admit(Some(b.origin())).unwrap());
        }
        assert_eq!(
            (first_budget.active(), second_budget.active()),
            (2048, 2048)
        );
        let third_budget = EffectBudget::new(4096);
        let third = OutboundRegistry::new_with_budget(limits, third_budget.clone());
        let c = third.register().unwrap();
        assert!(third.admit(Some(c.origin())).is_err());
        assert_eq!(EffectBudget::default().active(), 4096);
        assert_eq!(
            third_budget.active(),
            0,
            "failed process admission cannot charge Host"
        );
        receipts.pop().unwrap().finish(EffectOutcome::default());
        let recovered = third.admit(Some(c.origin())).unwrap();
        assert_eq!(EffectBudget::default().active(), 4096);
        recovered.finish(EffectOutcome::default());
        for receipt in receipts {
            receipt.finish(EffectOutcome::default());
        }
        assert_eq!(EffectBudget::default().active(), 0);
        assert_eq!(
            (
                first_budget.active(),
                second_budget.active(),
                third_budget.active()
            ),
            (0, 0, 0)
        );
    }

    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn cleanup_deadline_preserves_capacity_until_late_receipt() {
        let registry = limits(2, 1, 1);
        let owner = registry.register().unwrap();
        let other = registry.register().unwrap();
        let admission = registry.admit(Some(owner.origin())).unwrap();
        let completion = owner.completion();
        owner.close();
        assert!(completion.wait().now_or_never().is_none());
        tokio::time::advance(std::time::Duration::from_secs(31)).await;
        let missed_deadline = completion
            .wait()
            .now_or_never()
            .expect("a closed scope must report unresolved cleanup within its deadline");
        assert!(missed_deadline
            .as_ref()
            .unwrap_err()
            .extra
            .contains("unresolved"));
        assert_eq!(completion.snapshot().active, 1);
        assert!(!completion.snapshot().failed);
        assert_eq!(registry.total_active(), 1);
        assert!(registry.admit(Some(other.origin())).is_err());
        admission.finish(EffectOutcome::default());
        assert_eq!(completion.wait().await.unwrap().active, 0);
        assert!(
            missed_deadline.is_err(),
            "late receipt cannot rewrite prior failure"
        );
        let recovered = registry.admit(Some(other.origin())).unwrap();
        recovered.finish(EffectOutcome::default());
        assert_eq!(registry.total_active(), 0);
    }

    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn concurrent_cleanup_observers_share_first_close_deadline() {
        let registry = limits(1, 1, 1);
        let owner = registry.register().unwrap();
        let admission = registry.admit(Some(owner.origin())).unwrap();
        let first = owner.completion();
        assert!(first.wait().now_or_never().is_none());
        tokio::time::advance(std::time::Duration::from_secs(3600)).await;
        assert!(
            first.wait().now_or_never().is_none(),
            "open scope has no cleanup deadline"
        );
        owner.close();
        tokio::time::advance(std::time::Duration::from_secs(15)).await;
        let second = owner.completion();
        assert!(second.wait().now_or_never().is_none());
        owner.close_handle().close();
        tokio::time::advance(std::time::Duration::from_secs(16)).await;
        for completion in [&first, &second] {
            assert!(completion
                .wait()
                .now_or_never()
                .expect("new observer or repeated close must not restart the deadline")
                .is_err());
        }
        admission.finish(EffectOutcome::default());
    }

    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn closure_before_first_observer_still_has_original_deadline() {
        let registry = limits(1, 1, 1);
        let owner = registry.register().unwrap();
        let admission = registry.admit(Some(owner.origin())).unwrap();
        owner.close();
        tokio::time::advance(std::time::Duration::from_secs(31)).await;
        assert!(owner
            .completion()
            .wait()
            .now_or_never()
            .expect("deadline starts at closure, not the first observer poll")
            .is_err());
        admission.finish(EffectOutcome::default());
    }

    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn cross_thread_close_reports_deadline_without_local_owner_poll() {
        let registry = limits(1, 1, 1);
        let owner = registry.register().unwrap();
        let admission = registry.admit(Some(owner.origin())).unwrap();
        let completion = owner.completion();
        let close = owner.close_handle();
        let runtime = tokio::runtime::Handle::current();
        std::thread::spawn(move || {
            let _entered = runtime.enter();
            close.close();
        })
        .join()
        .unwrap();
        assert!(completion.wait().now_or_never().is_none());
        tokio::time::advance(std::time::Duration::from_secs(31)).await;
        let result = completion
            .wait()
            .now_or_never()
            .expect("cross-thread closure must start the cleanup deadline");
        assert!(result.unwrap_err().extra.contains("unresolved"));
        assert_eq!(registry.total_active(), 1);
        admission.finish(EffectOutcome::default());
        owner.close();
        assert_eq!(completion.wait().await.unwrap().active, 0);
    }

    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn cross_thread_close_requires_local_closure_acknowledgement_for_success() {
        let registry = limits(1, 1, 1);
        let owner = registry.register().unwrap();
        let completion = owner.completion();
        owner.close_handle().close();
        assert!(
            completion.wait().now_or_never().is_none(),
            "cross-thread closure cannot certify a potentially stale active snapshot"
        );
        tokio::time::advance(std::time::Duration::from_secs(31)).await;
        assert!(completion
            .wait()
            .now_or_never()
            .expect("missing local closure acknowledgement must produce bounded failure")
            .is_err());
        owner.close();
        assert_eq!(completion.wait().await.unwrap().active, 0);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn cancellation_before_worker_poll_never_starts_backend() {
        tokio::task::LocalSet::new()
            .run_until(async {
                let registry = limits(1, 1, 1);
                let owner = registry.register().unwrap();
                let polled = Rc::new(Cell::new(false));
                let observed = polled.clone();
                let request = connect_owned::<Stream, process::Client, _>(
                    &registry,
                    Some(owner.origin()),
                    async move {
                        observed.set(true);
                        Err(capnp::Error::failed("backend should not run".into()))
                    },
                )
                .unwrap();
                assert_eq!(registry.total_active(), 1);
                drop(request);
                let done = owner.close_and_join().await.unwrap();
                assert!(!polled.get());
                assert_eq!(
                    (done.active, done.committed, registry.total_active()),
                    (0, 0, 0)
                );
            })
            .await;
    }

    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn timeout_retains_native_attempt_and_closed_scope_rejects_late_stream() {
        tokio::task::LocalSet::new()
            .run_until(async {
                let registry = limits(2, 1, 1);
                let owner = registry.register().unwrap();
                let other = registry.register().unwrap();
                let (started, start) = oneshot::channel();
                let (release, result) = oneshot::channel::<Stream>();
                let request = connect_owned::<Stream, process::Client, _>(
                    &registry,
                    Some(owner.origin()),
                    async move {
                        started.send(()).unwrap();
                        Ok(result.await.unwrap())
                    },
                )
                .unwrap();
                start.await.unwrap();
                assert!(
                    tokio::time::timeout(std::time::Duration::from_secs(30), request)
                        .await
                        .is_err()
                );
                owner.close();
                let completion = owner.completion();
                assert!(completion.wait().now_or_never().is_none());
                assert_eq!(registry.total_active(), 1);
                assert!(registry.admit(Some(other.origin())).is_err());
                let (stream, mut peer) = tokio::io::duplex(1024);
                release.send(stream.compat()).unwrap();
                let done = completion.wait().await.unwrap();
                assert_eq!((done.active, done.committed, done.joined), (0, 0, 0));
                assert_eq!(registry.total_active(), 0);
                assert_eq!(
                    tokio::io::AsyncReadExt::read(&mut peer, &mut [0u8])
                        .await
                        .unwrap(),
                    0,
                    "late stream must close without RPC Bootstrap traffic"
                );
            })
            .await;
    }

    struct Probe(Rc<Cell<usize>>);
    impl process::Server for Probe {
        async fn stdin(
            self: capnp::capability::Rc<Self>,
            _: process::StdinParams,
            _: process::StdinResults,
        ) -> capnp::Result<()> {
            self.0.set(self.0.get() + 1);
            Ok(())
        }
    }

    #[tokio::test(flavor = "current_thread")]
    async fn committed_session_survives_reply_drop_until_cell_join_and_releases_retained_client() {
        tokio::task::LocalSet::new()
            .run_until(async {
                let registry = OutboundRegistry::default();
                let owner = registry.register().unwrap();
                let (stream, remote) = tokio::io::duplex(8192);
                let (reader, writer) = remote.compat().split();
                let calls = Rc::new(Cell::new(0));
                let bootstrap: process::Client = capnp_rpc::new_client(Probe(calls.clone()));
                let mut remote = ManagedRpc::spawn(ManagedRpcSystem::new(
                    Box::new(capnp_rpc::twoparty::VatNetwork::new(
                        reader,
                        writer,
                        capnp_rpc::rpc_twoparty_capnp::Side::Server,
                        Default::default(),
                    )),
                    Some(bootstrap.client),
                ));
                let client = connect_owned::<_, process::Client, _>(
                    &registry,
                    Some(owner.origin()),
                    async { Ok(stream.compat()) },
                )
                .unwrap()
                .await
                .unwrap();
                // Awaiting consumed and dropped the reply, which must not cancel a
                // committed session. A retained import also must not own the Cell.
                client.stdin_request().send().promise.await.unwrap();
                client.stdin_request().send().promise.await.unwrap();
                assert_eq!(calls.get(), 2);
                assert_eq!(owner.completion().snapshot().committed, 1);
                assert_eq!(registry.total_active(), 1);
                let done = owner.close_and_join().await.unwrap();
                assert_eq!((done.committed, done.joined, done.active), (1, 1, 0));
                assert!(client.stdin_request().send().promise.await.is_err());
                assert_eq!(registry.total_active(), 0);
                assert!(remote.shutdown_and_join().await.is_ok());
            })
            .await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn lost_native_worker_fails_certificate_and_quarantines_capacity() {
        tokio::task::LocalSet::new()
            .run_until(async {
                let registry = limits(1, 1, 1);
                let owner = registry.register().unwrap();
                let request = connect_owned::<Stream, process::Client, _>(
                    &registry,
                    Some(owner.origin()),
                    async { panic!("native backend failure") },
                )
                .unwrap();
                assert!(request.await.is_err());
                assert!(owner.close_and_join().await.is_err());
                let done = owner.completion().snapshot();
                assert!(done.failed);
                assert_eq!(
                    (done.active, registry.total_active()),
                    (1, 1),
                    "unknown native completion must quarantine capacity"
                );
                assert!(registry.admit(Some(owner.origin())).is_err());
            })
            .await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn localset_loss_before_join_monitor_poll_never_certifies_cleanup() {
        let registry = OutboundRegistry::default();
        let owner = registry.register().unwrap();
        let local = tokio::task::LocalSet::new();
        let request = {
            let _entered = local.enter();
            connect_owned::<Stream, process::Client, _>(
                &registry,
                Some(owner.origin()),
                std::future::pending(),
            )
            .unwrap()
        };
        drop(local);
        drop(request);
        assert!(owner.close_and_join().await.is_err());
        assert_eq!(
            registry.total_active(),
            1,
            "lost join monitor must not recycle capacity"
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn constructor_panic_after_driver_spawn_quarantines_unjoined_session() {
        tokio::task::LocalSet::new()
            .run_until(async {
                let registry = limits(1, 1, 1);
                let owner = registry.register().unwrap();
                let (stream, _peer) = tokio::io::duplex(8192);
                let request = connect_owned::<_, PanickingClient, _>(
                    &registry,
                    Some(owner.origin()),
                    async { Ok(stream.compat()) },
                )
                .unwrap();
                assert!(request.await.is_err());
                assert!(owner.close_and_join().await.is_err());
                let done = owner.completion().snapshot();
                assert_eq!((done.active, done.committed, done.joined), (1, 0, 0));
                assert_eq!(registry.total_active(), 1);
            })
            .await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn acknowledged_backend_error_returns_capacity_for_another_generation() {
        tokio::task::LocalSet::new()
            .run_until(async {
                let registry = limits(1, 1, 1);
                for _ in 0..2 {
                    let owner = registry.register().unwrap();
                    let request = connect_owned::<Stream, process::Client, _>(
                        &registry,
                        Some(owner.origin()),
                        async { Err(capnp::Error::failed("backend returned failure".into())) },
                    )
                    .unwrap();
                    let error = request.await.err().unwrap();
                    assert!(error.extra.contains("backend returned failure"));
                    let done = owner.close_and_join().await.unwrap();
                    assert_eq!((done.active, done.committed, done.joined), (0, 0, 0));
                    assert!(!done.failed);
                    assert_eq!(registry.total_active(), 0);
                }
            })
            .await;
    }
}
