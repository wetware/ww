use std::collections::BTreeMap;
use std::fmt;
use std::future::Future;
use std::io;
use std::panic::AssertUnwindSafe;
use std::pin::Pin;
use std::sync::{Arc, Mutex, Weak};

#[cfg(feature = "test-support")]
use std::path::PathBuf;

use futures::FutureExt;
use tokio::sync::{oneshot, watch};

use crate::fs_driver::ReaperInner;
use crate::resources::{
    AdmissionError, AdmissionKind, CleanupObligation, ForegroundTransaction, HostResources,
};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FilesystemOperation {
    ListingRead,
    ListingWrite,
    DescriptorOpen,
    DirectoryBuild,
    FileStage,
    GenerationCleanup,
    CachePin,
    CacheUnpin,
    ProcessSetup,
    ProcessCleanup,
}

#[cfg(feature = "test-support")]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FsTestStage {
    Started,
    DirectoryEntry,
    BeforePublish,
    Published,
    CleanupStarted,
    Completed,
}

#[cfg(feature = "test-support")]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FsTestEvent {
    pub operation: FilesystemOperation,
    pub stage: FsTestStage,
    pub path: Option<PathBuf>,
}

#[cfg(feature = "test-support")]
type FsTestHook = Arc<dyn Fn(FsTestEvent) + Send + Sync>;

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum FsWorkError {
    Message(Arc<str>),
    Admission(AdmissionError),
}

impl FsWorkError {
    pub(crate) fn new(message: impl Into<String>) -> Self {
        Self::Message(Arc::from(message.into()))
    }
}

impl From<AdmissionError> for FsWorkError {
    fn from(error: AdmissionError) -> Self {
        Self::Admission(error)
    }
}

impl fmt::Display for FsWorkError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Message(message) => formatter.write_str(message),
            Self::Admission(error) => error.fmt(formatter),
        }
    }
}

impl std::error::Error for FsWorkError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Message(_) => None,
            Self::Admission(error) => Some(error),
        }
    }
}

type SyncCleanup = Box<dyn FnOnce() -> io::Result<()> + Send + 'static>;
type AsyncCleanupFuture = Pin<Box<dyn Future<Output = io::Result<()>> + Send + 'static>>;
type AsyncCleanup = Box<dyn FnOnce() -> AsyncCleanupFuture + Send + 'static>;

enum CleanupAction {
    Sync(SyncCleanup),
    Async(AsyncCleanup),
}

#[derive(Clone)]
enum CompletionState {
    Running,
    Finished(Result<(), FsWorkError>),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct RootId {
    pub(crate) slot: usize,
    pub(crate) generation: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct CleanupId {
    slot: usize,
    generation: u64,
}

pub(crate) struct CleanupEntry {
    id: CleanupId,
    kind: FilesystemOperation,
    action: CleanupAction,
}

struct Lifecycle {
    accepting: bool,
    cleanup_admission: bool,
    cleanup_started: bool,
    work: usize,
    next_cleanup_generation: u64,
    cleanups: BTreeMap<usize, CleanupEntry>,
    first_failure: Option<FsWorkError>,
    finished: bool,
}

pub(crate) struct State {
    id: RootId,
    host: Arc<HostResources>,
    reaper: Weak<ReaperInner>,
    obligation: Mutex<Option<CleanupObligation>>,
    lifecycle: Mutex<Lifecycle>,
    completion_tx: watch::Sender<CompletionState>,
    #[cfg(feature = "test-support")]
    test_hook: Mutex<Option<FsTestHook>>,
}

impl State {
    pub(crate) fn new(
        id: RootId,
        host: Arc<HostResources>,
        reaper: Weak<ReaperInner>,
        obligation: CleanupObligation,
    ) -> Arc<Self> {
        let (completion_tx, _) = watch::channel(CompletionState::Running);
        Arc::new(Self {
            id,
            host,
            reaper,
            obligation: Mutex::new(Some(obligation)),
            lifecycle: Mutex::new(Lifecycle {
                accepting: true,
                cleanup_admission: true,
                cleanup_started: false,
                work: 0,
                next_cleanup_generation: 0,
                cleanups: BTreeMap::new(),
                first_failure: None,
                finished: false,
            }),
            completion_tx,
            #[cfg(feature = "test-support")]
            test_hook: Mutex::new(None),
        })
    }

    pub(crate) fn id(&self) -> RootId {
        self.id
    }

    fn try_work(self: &Arc<Self>, cleanup_bearing: bool) -> Result<FsWorkToken, AdmissionError> {
        let mut lifecycle = self
            .lifecycle
            .lock()
            .expect("filesystem lifecycle poisoned");
        if lifecycle.cleanup_started || lifecycle.finished {
            return Err(AdmissionError::new(AdmissionKind::Closed));
        }
        if cleanup_bearing && !lifecycle.cleanup_admission {
            return Err(AdmissionError::new(AdmissionKind::CleanupPoisoned));
        }
        if !lifecycle.accepting {
            return Err(AdmissionError::new(AdmissionKind::Closed));
        }
        lifecycle.work = lifecycle
            .work
            .checked_add(1)
            .ok_or_else(|| AdmissionError::new(AdmissionKind::Closed))?;
        Ok(FsWorkToken {
            state: Arc::clone(self),
            active: true,
            cleanup_authorized: cleanup_bearing,
        })
    }

    fn close(&self) {
        let ready = {
            let mut lifecycle = self
                .lifecycle
                .lock()
                .expect("filesystem lifecycle poisoned");
            lifecycle.accepting = false;
            lifecycle.work == 0 && !lifecycle.cleanup_started
        };
        if ready {
            self.notify_reaper();
        }
    }

    fn check_cleanup_admission(&self) -> Result<(), AdmissionError> {
        let lifecycle = self
            .lifecycle
            .lock()
            .expect("filesystem lifecycle poisoned");
        if lifecycle.cleanup_started || lifecycle.finished {
            Err(AdmissionError::new(AdmissionKind::Closed))
        } else if !lifecycle.cleanup_admission {
            Err(AdmissionError::new(AdmissionKind::CleanupPoisoned))
        } else {
            Ok(())
        }
    }

    fn notify_reaper(&self) {
        if let Some(reaper) = self.reaper.upgrade() {
            reaper.notify();
        }
    }

    fn fail(&self, error: FsWorkError, poison_cleanup: bool) {
        let mut lifecycle = self
            .lifecycle
            .lock()
            .expect("filesystem lifecycle poisoned");
        if lifecycle.first_failure.is_none() {
            lifecycle.first_failure = Some(error);
        }
        if poison_cleanup {
            lifecycle.cleanup_admission = false;
        }
    }

    pub(crate) fn is_ready(&self) -> bool {
        let lifecycle = self
            .lifecycle
            .lock()
            .expect("filesystem lifecycle poisoned");
        !lifecycle.accepting
            && lifecycle.work == 0
            && !lifecycle.cleanup_started
            && !lifecycle.finished
    }

    pub(crate) fn claim_cleanup(&self) -> Option<Vec<CleanupEntry>> {
        let mut lifecycle = self
            .lifecycle
            .lock()
            .expect("filesystem lifecycle poisoned");
        if lifecycle.accepting
            || lifecycle.work != 0
            || lifecycle.cleanup_started
            || lifecycle.finished
        {
            return None;
        }
        lifecycle.cleanup_started = true;
        Some(
            std::mem::take(&mut lifecycle.cleanups)
                .into_values()
                .collect(),
        )
    }

    pub(crate) fn run_cleanup(
        &self,
        runtime: &tokio::runtime::Runtime,
        cleanups: Vec<CleanupEntry>,
    ) {
        for CleanupEntry { kind, action, .. } in cleanups {
            let result = match action {
                CleanupAction::Sync(cleanup) => std::panic::catch_unwind(AssertUnwindSafe(cleanup)),
                CleanupAction::Async(cleanup) => {
                    std::panic::catch_unwind(AssertUnwindSafe(|| runtime.block_on(cleanup())))
                }
            };
            match result {
                Ok(Ok(())) => {}
                Ok(Err(error)) => self.fail(
                    FsWorkError::new(format!("filesystem cleanup {kind:?} failed: {error}")),
                    true,
                ),
                Err(_) => self.fail(
                    FsWorkError::new(format!("filesystem cleanup {kind:?} panicked")),
                    true,
                ),
            }
        }
    }

    pub(crate) fn publish_completion(&self) {
        let result = {
            let mut lifecycle = self
                .lifecycle
                .lock()
                .expect("filesystem lifecycle poisoned");
            if lifecycle.finished {
                return;
            }
            lifecycle.finished = true;
            lifecycle.first_failure.clone().map_or(Ok(()), Err)
        };
        self.completion_tx
            .send_replace(CompletionState::Finished(result));
    }

    pub(crate) fn release_obligation(&self) {
        let obligation = self
            .obligation
            .lock()
            .expect("filesystem cleanup obligation poisoned")
            .take();
        drop(obligation);
    }

    fn register_cleanup(
        self: &Arc<Self>,
        kind: FilesystemOperation,
        cleanup: CleanupAction,
        allow_after_close: bool,
        allow_after_poison: bool,
    ) -> Result<CleanupRegistration, FsWorkError> {
        let mut lifecycle = self
            .lifecycle
            .lock()
            .expect("filesystem lifecycle poisoned");
        if !lifecycle.cleanup_admission && !allow_after_poison {
            return Err(AdmissionError::new(AdmissionKind::CleanupPoisoned).into());
        }
        if lifecycle.cleanup_started || lifecycle.finished {
            return Err(AdmissionError::new(AdmissionKind::Closed).into());
        }
        if !lifecycle.accepting && !allow_after_close {
            return Err(AdmissionError::new(AdmissionKind::Closed).into());
        }
        lifecycle.next_cleanup_generation = lifecycle
            .next_cleanup_generation
            .checked_add(1)
            .ok_or_else(|| FsWorkError::new("filesystem cleanup generation exhausted"))?;
        let slot = (0..)
            .find(|slot| !lifecycle.cleanups.contains_key(slot))
            .expect("usize cleanup slot space exhausted before generation space");
        let id = CleanupId {
            slot,
            generation: lifecycle.next_cleanup_generation,
        };
        lifecycle.cleanups.insert(
            slot,
            CleanupEntry {
                id,
                kind,
                action: cleanup,
            },
        );
        Ok(CleanupRegistration {
            state: Arc::clone(self),
            id,
            resolved: false,
        })
    }

    fn resolve_cleanup(&self, id: CleanupId) -> Result<(), FsWorkError> {
        let removed = {
            let mut lifecycle = self
                .lifecycle
                .lock()
                .expect("filesystem lifecycle poisoned");
            match lifecycle.cleanups.get(&id.slot) {
                Some(entry) if entry.id.generation == id.generation => {
                    lifecycle.cleanups.remove(&id.slot)
                }
                _ => None,
            }
        };
        match removed {
            Some(entry) => {
                drop(entry);
                Ok(())
            }
            None => Err(FsWorkError::new(
                "filesystem cleanup registration is stale or already resolved",
            )),
        }
    }

    #[cfg(feature = "test-support")]
    fn test_event(&self, event: FsTestEvent) {
        let hook = self
            .test_hook
            .lock()
            .expect("filesystem test hook poisoned")
            .clone();
        if let Some(hook) = hook {
            if std::panic::catch_unwind(AssertUnwindSafe(|| hook(event))).is_err() {
                self.fail(FsWorkError::new("filesystem test hook panicked"), false);
            }
        }
    }
}

#[derive(Clone)]
pub struct FsScope {
    state: Arc<State>,
}

pub struct FsOwner {
    state: Arc<State>,
}

pub struct FsFinalizer {
    state: Option<Arc<State>>,
}

#[derive(Clone)]
pub struct FsCompletion {
    receiver: watch::Receiver<CompletionState>,
}

pub(crate) fn new_registered_root(
    state: Arc<State>,
) -> (FsScope, FsOwner, FsFinalizer, FsCompletion) {
    let receiver = state.completion_tx.subscribe();
    (
        FsScope {
            state: Arc::clone(&state),
        },
        FsOwner {
            state: Arc::clone(&state),
        },
        FsFinalizer { state: Some(state) },
        FsCompletion { receiver },
    )
}

pub struct FsWorkToken {
    state: Arc<State>,
    active: bool,
    cleanup_authorized: bool,
}

impl fmt::Debug for FsWorkToken {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("FsWorkToken")
            .field("root", &self.state.id)
            .field("cleanup_authorized", &self.cleanup_authorized)
            .finish_non_exhaustive()
    }
}

impl FsWorkToken {
    pub(crate) fn cleanup_authorized(&self) -> bool {
        self.cleanup_authorized
    }

    pub(crate) fn check_current_cleanup_admission(&self) -> Result<(), AdmissionError> {
        self.state.check_cleanup_admission()
    }

    pub(crate) fn belongs_to(&self, scope: &FsScope) -> bool {
        Arc::ptr_eq(&self.state, &scope.state)
    }

    pub(crate) fn belongs_to_host(&self, host: &Arc<HostResources>) -> bool {
        Arc::ptr_eq(&self.state.host, host)
    }

    pub(crate) fn record_failure(&self, error: FsWorkError) {
        self.state.fail(error, false);
    }
}

impl Drop for FsWorkToken {
    fn drop(&mut self) {
        if !self.active {
            return;
        }
        self.active = false;
        let ready = {
            let mut lifecycle = self
                .state
                .lifecycle
                .lock()
                .expect("filesystem lifecycle poisoned");
            lifecycle.work = lifecycle
                .work
                .checked_sub(1)
                .expect("filesystem work token underflow");
            !lifecycle.accepting && lifecycle.work == 0 && !lifecycle.cleanup_started
        };
        if ready {
            self.state.notify_reaper();
        }
    }
}

pub struct FsResultEnvelope<T> {
    value: Option<T>,
    ownership: Option<ForegroundTransaction>,
}

/// One noncloneable cleanup-bearing phase admitted before its private effect.
/// It can transfer exactly one mandatory cleanup registration even if another
/// cleanup failure poisons the root before registration completes.
pub struct CleanupPhase {
    transaction: ForegroundTransaction,
}

impl fmt::Debug for CleanupPhase {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("CleanupPhase")
            .finish_non_exhaustive()
    }
}

impl CleanupPhase {
    pub(crate) fn new(transaction: ForegroundTransaction) -> Self {
        Self { transaction }
    }
}

impl<T> FsResultEnvelope<T> {
    fn from_transaction(value: T, transaction: ForegroundTransaction) -> Self {
        Self {
            value: Some(value),
            ownership: Some(transaction),
        }
    }

    pub fn install<R>(mut self, install: impl FnOnce(T) -> R) -> R {
        let value = self
            .value
            .take()
            .expect("filesystem result already consumed");
        let result = install(value);
        drop(self);
        result
    }

    pub fn into_inner(mut self) -> T {
        let value = self
            .value
            .take()
            .expect("filesystem result already consumed");
        drop(self);
        value
    }

    pub fn dispose(self) {
        drop(self);
    }
}

impl<T> Drop for FsResultEnvelope<T> {
    fn drop(&mut self) {
        drop(self.value.take());
        drop(self.ownership.take());
    }
}

impl FsScope {
    pub(crate) fn belongs_to(&self, host: &Arc<HostResources>) -> bool {
        Arc::ptr_eq(&self.state.host, host)
    }

    pub fn try_work(&self) -> Result<FsWorkToken, AdmissionError> {
        self.state.try_work(false)
    }

    pub fn try_cleanup_work(&self) -> Result<FsWorkToken, AdmissionError> {
        self.state.try_work(true)
    }

    pub fn own_cleanup<F>(
        &self,
        kind: FilesystemOperation,
        cleanup: F,
    ) -> Result<CleanupRegistration, FsWorkError>
    where
        F: FnOnce() -> io::Result<()> + Send + 'static,
    {
        self.state
            .register_cleanup(kind, CleanupAction::Sync(Box::new(cleanup)), false, false)
    }

    pub fn own_cleanup_in<F>(
        &self,
        transaction: &ForegroundTransaction,
        kind: FilesystemOperation,
        cleanup: F,
    ) -> Result<CleanupRegistration, FsWorkError>
    where
        F: FnOnce() -> io::Result<()> + Send + 'static,
    {
        if !transaction.owns_scope(self) {
            return Err(AdmissionError::new(AdmissionKind::Closed).into());
        }
        transaction.try_cleanup_phase()?;
        self.state
            .register_cleanup(kind, CleanupAction::Sync(Box::new(cleanup)), true, false)
    }

    pub fn own_cleanup_from_phase<F>(
        &self,
        phase: CleanupPhase,
        kind: FilesystemOperation,
        cleanup: F,
    ) -> Result<CleanupRegistration, FsWorkError>
    where
        F: FnOnce() -> io::Result<()> + Send + 'static,
    {
        if !phase.transaction.owns_scope(self) {
            return Err(AdmissionError::new(AdmissionKind::Closed).into());
        }
        self.state
            .register_cleanup(kind, CleanupAction::Sync(Box::new(cleanup)), true, true)
    }

    pub fn own_async_cleanup<F, Fut>(
        &self,
        kind: FilesystemOperation,
        cleanup: F,
    ) -> Result<CleanupRegistration, FsWorkError>
    where
        F: FnOnce() -> Fut + Send + 'static,
        Fut: Future<Output = io::Result<()>> + Send + 'static,
    {
        self.state.register_cleanup(
            kind,
            CleanupAction::Async(Box::new(move || Box::pin(cleanup()))),
            false,
            false,
        )
    }

    pub fn own_async_cleanup_in<F, Fut>(
        &self,
        transaction: &ForegroundTransaction,
        kind: FilesystemOperation,
        cleanup: F,
    ) -> Result<CleanupRegistration, FsWorkError>
    where
        F: FnOnce() -> Fut + Send + 'static,
        Fut: Future<Output = io::Result<()>> + Send + 'static,
    {
        if !transaction.owns_scope(self) {
            return Err(AdmissionError::new(AdmissionKind::Closed).into());
        }
        transaction.try_cleanup_phase()?;
        self.state.register_cleanup(
            kind,
            CleanupAction::Async(Box::new(move || Box::pin(cleanup()))),
            true,
            false,
        )
    }

    pub fn own_async_cleanup_from_phase<F, Fut>(
        &self,
        phase: CleanupPhase,
        kind: FilesystemOperation,
        cleanup: F,
    ) -> Result<CleanupRegistration, FsWorkError>
    where
        F: FnOnce() -> Fut + Send + 'static,
        Fut: Future<Output = io::Result<()>> + Send + 'static,
    {
        if !phase.transaction.owns_scope(self) {
            return Err(AdmissionError::new(AdmissionKind::Closed).into());
        }
        self.state.register_cleanup(
            kind,
            CleanupAction::Async(Box::new(move || Box::pin(cleanup()))),
            true,
            true,
        )
    }

    pub fn record_cleanup_failure(&self, error: impl fmt::Display) {
        self.state.fail(
            FsWorkError::new(format!("filesystem cleanup failed: {error}")),
            true,
        );
    }

    #[cfg(feature = "test-support")]
    pub fn set_test_hook(&self, hook: FsTestHook) {
        *self
            .state
            .test_hook
            .lock()
            .expect("filesystem test hook poisoned") = Some(hook);
    }

    #[cfg(feature = "test-support")]
    pub fn test_event(
        &self,
        operation: FilesystemOperation,
        stage: FsTestStage,
        path: Option<PathBuf>,
    ) {
        self.state.test_event(FsTestEvent {
            operation,
            stage,
            path,
        });
    }
}

impl ForegroundTransaction {
    /// Runs an admitted, resource-free owner transaction independently of any
    /// waiter. The terminal callback runs only after logical/F/root ownership
    /// has been released, so it can remove an inflight marker and notify
    /// waiters without making completed-but-unpolled results retain capacity.
    pub fn spawn_retained_terminal<F, C>(self, kind: FilesystemOperation, operation: F, terminal: C)
    where
        F: Future<Output = ()> + Send + 'static,
        C: FnOnce(Result<(), FsWorkError>) + Send + 'static,
    {
        let driver = self.driver().clone();
        driver.spawn(async move {
            let outcome = match AssertUnwindSafe(operation).catch_unwind().await {
                Ok(()) => Ok(()),
                Err(_) => {
                    let error = FsWorkError::new(format!("filesystem operation {kind:?} panicked"));
                    self.record_failure(error.clone());
                    Err(error)
                }
            };
            drop(self);
            terminal(outcome);
        });
    }

    pub async fn run_operation<T, E, F>(
        &self,
        kind: FilesystemOperation,
        operation: F,
    ) -> Result<FsResultEnvelope<Result<T, E>>, FsWorkError>
    where
        T: Send + 'static,
        E: Send + 'static,
        F: Future<Output = Result<T, E>> + Send + 'static,
    {
        let transaction = self.continuation();
        let failure_owner = transaction.continuation();
        let driver = self.driver().clone();
        let (result_tx, result_rx) = oneshot::channel();
        driver.spawn(async move {
            match AssertUnwindSafe(operation).catch_unwind().await {
                Ok(result) => {
                    let _ = result_tx.send(FsResultEnvelope::from_transaction(result, transaction));
                }
                Err(_) => failure_owner.record_failure(FsWorkError::new(format!(
                    "filesystem operation {kind:?} panicked"
                ))),
            }
        });
        result_rx.await.map_err(|_| {
            FsWorkError::new(format!(
                "filesystem operation {kind:?} ended without a result"
            ))
        })
    }

    pub async fn run_blocking<T, F>(
        &self,
        kind: FilesystemOperation,
        operation: F,
    ) -> Result<FsResultEnvelope<T>, FsWorkError>
    where
        T: Send + 'static,
        F: FnOnce() -> T + Send + 'static,
    {
        let transaction = self.continuation();
        let failure_owner = transaction.continuation();
        let driver = self.driver().clone();
        let (result_tx, result_rx) = oneshot::channel();
        driver.spawn(async move {
            let result = tokio::task::spawn_blocking(move || {
                std::panic::catch_unwind(AssertUnwindSafe(operation))
            })
            .await;
            match result {
                Ok(Ok(value)) => {
                    let _ =
                        result_tx.send(Ok(FsResultEnvelope::from_transaction(value, transaction)));
                }
                Ok(Err(_)) => {
                    let error = FsWorkError::new(format!(
                        "filesystem blocking operation {kind:?} panicked"
                    ));
                    failure_owner.record_failure(error.clone());
                    let _ = result_tx.send(Err(error));
                }
                Err(error) => {
                    let error = FsWorkError::new(format!(
                        "filesystem blocking operation {kind:?} failed: {error}"
                    ));
                    failure_owner.record_failure(error.clone());
                    let _ = result_tx.send(Err(error));
                }
            }
        });
        result_rx.await.map_err(|_| {
            FsWorkError::new(format!(
                "filesystem blocking operation {kind:?} ended without a result"
            ))
        })?
    }

    pub fn own_result<T>(&self, value: T) -> FsResultEnvelope<T> {
        FsResultEnvelope::from_transaction(value, self.continuation())
    }
}

pub struct CleanupRegistration {
    state: Arc<State>,
    id: CleanupId,
    resolved: bool,
}

impl fmt::Debug for CleanupRegistration {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("CleanupRegistration")
            .field("root", &self.state.id)
            .field("slot", &self.id.slot)
            .field("generation", &self.id.generation)
            .finish_non_exhaustive()
    }
}

impl CleanupRegistration {
    pub fn resolve(mut self) -> Result<(), FsWorkError> {
        self.state.resolve_cleanup(self.id)?;
        self.resolved = true;
        Ok(())
    }
}

impl Drop for CleanupRegistration {
    fn drop(&mut self) {
        let _ = self.resolved;
    }
}

impl Drop for FsOwner {
    fn drop(&mut self) {
        self.state.close();
    }
}

struct FinalizerGuard {
    state: Option<Arc<State>>,
}

impl Drop for FinalizerGuard {
    fn drop(&mut self) {
        if let Some(state) = self.state.take() {
            state.close();
        }
    }
}

impl FsFinalizer {
    pub async fn run(mut self) -> Result<(), FsWorkError> {
        let state = self
            .state
            .take()
            .expect("filesystem finalizer already taken");
        let mut guard = FinalizerGuard { state: Some(state) };
        let result = FsCompletion {
            receiver: guard
                .state
                .as_ref()
                .expect("filesystem finalizer state missing")
                .completion_tx
                .subscribe(),
        }
        .wait()
        .await;
        guard.state = None;
        result
    }
}

impl Drop for FsFinalizer {
    fn drop(&mut self) {
        if let Some(state) = self.state.take() {
            state.close();
        }
    }
}

impl FsCompletion {
    pub async fn wait(&self) -> Result<(), FsWorkError> {
        let mut receiver = self.receiver.clone();
        loop {
            let state = receiver.borrow().clone();
            match state {
                CompletionState::Running => {}
                CompletionState::Finished(result) => return result,
            }
            receiver.changed().await.map_err(|_| {
                FsWorkError::new("filesystem completion owner disappeared without a result")
            })?;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{HostFsOwner, HostLimits};

    #[tokio::test]
    async fn stale_cleanup_generation_cannot_remove_reused_slot() {
        let (host_owner, host) = HostFsOwner::start(
            tokio::runtime::Handle::current(),
            HostLimits::new(2, 2, 1).unwrap(),
        )
        .unwrap();
        let (scope, owner, finalizer, completion) = host.try_filesystem_scope().unwrap();
        let first = scope
            .own_cleanup(FilesystemOperation::FileStage, || Ok(()))
            .unwrap();
        let stale_id = first.id;
        let state = Arc::clone(&first.state);
        first.resolve().unwrap();

        let current = scope
            .own_cleanup(FilesystemOperation::FileStage, || Ok(()))
            .unwrap();
        assert_eq!(stale_id.slot, current.id.slot);
        assert_ne!(stale_id.generation, current.id.generation);
        assert!(state.resolve_cleanup(stale_id).is_err());
        current.resolve().unwrap();
        for _ in 0..128 {
            scope
                .own_cleanup(FilesystemOperation::FileStage, || Ok(()))
                .unwrap()
                .resolve()
                .unwrap();
        }
        assert!(state
            .lifecycle
            .lock()
            .expect("filesystem lifecycle poisoned")
            .cleanups
            .is_empty());

        drop(owner);
        assert_eq!(finalizer.run().await, Ok(()));
        assert_eq!(completion.wait().await, Ok(()));
        host_owner.shutdown().await.unwrap();
    }
}
