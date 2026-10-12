use std::error::Error;
use std::fmt;
use std::num::NonZeroUsize;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use tokio::sync::{OwnedSemaphorePermit, Semaphore, TryAcquireError};

use crate::fs_driver::{HostFsDriver, ReaperInner};
use crate::fs_work::{
    new_registered_root, CleanupPhase, FsCompletion, FsFinalizer, FsOwner, FsScope, FsWorkToken,
};

const DEFAULT_HOST_LOGICAL_IO: usize = 128;
const DEFAULT_HOST_BACKEND_PRODUCERS: usize = 33;
const DEFAULT_HOST_CLEANUP_ROOTS: usize = 64;
const DEFAULT_CELL_LOGICAL_IO: usize = 16;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AdmissionKind {
    CellLogical,
    HostLogical,
    ForegroundProducer,
    CleanupObligation,
    Closed,
    CleanupPoisoned,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AdmissionError {
    kind: AdmissionKind,
}

impl AdmissionError {
    pub(crate) const fn new(kind: AdmissionKind) -> Self {
        Self { kind }
    }

    pub const fn kind(&self) -> AdmissionKind {
        self.kind
    }
}

impl fmt::Display for AdmissionError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let message = match self.kind {
            AdmissionKind::CellLogical => "Cell logical filesystem capacity is exhausted",
            AdmissionKind::HostLogical => "Host logical filesystem capacity is exhausted",
            AdmissionKind::ForegroundProducer => "Host foreground filesystem capacity is exhausted",
            AdmissionKind::CleanupObligation => {
                "Host filesystem cleanup-obligation capacity is exhausted"
            }
            AdmissionKind::Closed => "filesystem ownership is closed",
            AdmissionKind::CleanupPoisoned => "filesystem cleanup-bearing admission is poisoned",
        };
        formatter.write_str(message)
    }
}

impl Error for AdmissionError {}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ResourceLimitError(&'static str);

impl fmt::Display for ResourceLimitError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.0)
    }
}

impl Error for ResourceLimitError {}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct HostLimits {
    logical_io: NonZeroUsize,
    backend_producers: NonZeroUsize,
    cleanup_roots: NonZeroUsize,
}

impl HostLimits {
    pub fn new(
        logical_io: usize,
        backend_producers: usize,
        cleanup_roots: usize,
    ) -> Result<Self, ResourceLimitError> {
        let logical_io = NonZeroUsize::new(logical_io).ok_or(ResourceLimitError(
            "Host logical filesystem limit must be nonzero",
        ))?;
        let backend_producers = NonZeroUsize::new(backend_producers).ok_or(ResourceLimitError(
            "Host backend producer limit must be nonzero",
        ))?;
        if backend_producers.get() < 2 {
            return Err(ResourceLimitError(
                "Host backend producer limit must reserve one foreground and one cleanup lane",
            ));
        }
        let cleanup_roots = NonZeroUsize::new(cleanup_roots).ok_or(ResourceLimitError(
            "Host cleanup-obligation limit must be nonzero",
        ))?;
        Ok(Self {
            logical_io,
            backend_producers,
            cleanup_roots,
        })
    }

    pub(crate) const fn logical_io(self) -> usize {
        self.logical_io.get()
    }

    pub(crate) const fn foreground_producers(self) -> usize {
        self.backend_producers.get() - 1
    }

    pub(crate) const fn cleanup_roots(self) -> usize {
        self.cleanup_roots.get()
    }
}

impl Default for HostLimits {
    fn default() -> Self {
        Self::new(
            DEFAULT_HOST_LOGICAL_IO,
            DEFAULT_HOST_BACKEND_PRODUCERS,
            DEFAULT_HOST_CLEANUP_ROOTS,
        )
        .expect("private Host filesystem defaults are valid")
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CellLimits {
    logical_io: NonZeroUsize,
}

impl CellLimits {
    pub fn new(logical_io: usize) -> Result<Self, ResourceLimitError> {
        Ok(Self {
            logical_io: NonZeroUsize::new(logical_io).ok_or(ResourceLimitError(
                "Cell logical filesystem limit must be nonzero",
            ))?,
        })
    }
}

impl Default for CellLimits {
    fn default() -> Self {
        Self::new(DEFAULT_CELL_LOGICAL_IO).expect("private Cell filesystem defaults are valid")
    }
}

pub struct HostResources {
    logical: Arc<Semaphore>,
    foreground: Arc<Semaphore>,
    cleanup_roots: Arc<Semaphore>,
    _cleanup_active: Arc<Semaphore>,
    accepting: AtomicBool,
    driver: HostFsDriver,
    reaper: std::sync::Weak<ReaperInner>,
}

impl fmt::Debug for HostResources {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("HostResources")
            .field("accepting", &self.accepting.load(Ordering::Acquire))
            .finish_non_exhaustive()
    }
}

impl HostResources {
    pub(crate) fn new(
        limits: HostLimits,
        cleanup_active: Arc<Semaphore>,
        driver: HostFsDriver,
        reaper: std::sync::Weak<ReaperInner>,
    ) -> Self {
        Self {
            logical: Arc::new(Semaphore::new(limits.logical_io())),
            foreground: Arc::new(Semaphore::new(limits.foreground_producers())),
            cleanup_roots: Arc::new(Semaphore::new(limits.cleanup_roots())),
            _cleanup_active: cleanup_active,
            accepting: AtomicBool::new(true),
            driver,
            reaper,
        }
    }

    fn ensure_accepting(&self) -> Result<(), AdmissionError> {
        if self.accepting.load(Ordering::Acquire) {
            Ok(())
        } else {
            Err(AdmissionError::new(AdmissionKind::Closed))
        }
    }

    fn try_owned(
        &self,
        semaphore: &Arc<Semaphore>,
        exhausted: AdmissionKind,
    ) -> Result<OwnedSemaphorePermit, AdmissionError> {
        self.ensure_accepting()?;
        let permit = Arc::clone(semaphore)
            .try_acquire_owned()
            .map_err(|error| match error {
                TryAcquireError::NoPermits => AdmissionError::new(exhausted),
                TryAcquireError::Closed => AdmissionError::new(AdmissionKind::Closed),
            })?;
        if let Err(error) = self.ensure_accepting() {
            drop(permit);
            return Err(error);
        }
        Ok(permit)
    }

    pub fn try_logical_io(self: &Arc<Self>) -> Result<LogicalIoPermit, AdmissionError> {
        Ok(LogicalIoPermit {
            host: Arc::clone(self),
            _host_permit: self.try_owned(&self.logical, AdmissionKind::HostLogical)?,
            cell_permit: None,
        })
    }

    pub fn validate_logical(
        self: &Arc<Self>,
        logical: &LogicalIoPermit,
    ) -> Result<(), AdmissionError> {
        if Arc::ptr_eq(self, &logical.host) {
            Ok(())
        } else {
            Err(AdmissionError::new(AdmissionKind::Closed))
        }
    }

    pub fn try_foreground(self: &Arc<Self>) -> Result<ForegroundProducerPermit, AdmissionError> {
        Ok(ForegroundProducerPermit {
            host: Arc::clone(self),
            _permit: self.try_owned(&self.foreground, AdmissionKind::ForegroundProducer)?,
        })
    }

    pub fn try_foreground_transaction(
        self: &Arc<Self>,
        logical: LogicalIoPermit,
        scopes: &[&FsScope],
    ) -> Result<ForegroundTransaction, AdmissionError> {
        self.try_transaction(logical, scopes, false)
    }

    pub fn try_cleanup_transaction(
        self: &Arc<Self>,
        logical: LogicalIoPermit,
        scopes: &[&FsScope],
    ) -> Result<ForegroundTransaction, AdmissionError> {
        self.try_transaction(logical, scopes, true)
    }

    fn try_transaction(
        self: &Arc<Self>,
        logical: LogicalIoPermit,
        scopes: &[&FsScope],
        cleanup_bearing: bool,
    ) -> Result<ForegroundTransaction, AdmissionError> {
        if !Arc::ptr_eq(self, &logical.host) {
            return Err(AdmissionError::new(AdmissionKind::Closed));
        }

        let mut work = Vec::with_capacity(scopes.len());
        for scope in scopes {
            if !scope.belongs_to(self) {
                return Err(AdmissionError::new(AdmissionKind::Closed));
            }
            if work
                .iter()
                .any(|token: &FsWorkToken| token.belongs_to(scope))
            {
                continue;
            }
            work.push(if cleanup_bearing {
                scope.try_cleanup_work()?
            } else {
                scope.try_work()?
            });
        }

        // Root receipts are deliberately reserved before F. If F rejects,
        // `work` and `logical` roll back through ordinary RAII.
        let producer = self.try_foreground()?;
        Ok(ForegroundTransaction {
            inner: Arc::new(ForegroundTransactionInner {
                logical: Some(logical),
                producer,
                cleanup_authorized: cleanup_bearing,
                work,
                driver: self.driver.clone(),
            }),
        })
    }

    pub fn try_filesystem_scope(
        self: &Arc<Self>,
    ) -> Result<(FsScope, FsOwner, FsFinalizer, FsCompletion), AdmissionError> {
        let obligation = CleanupObligation {
            _permit: self.try_owned(&self.cleanup_roots, AdmissionKind::CleanupObligation)?,
        };
        let reaper = self
            .reaper
            .upgrade()
            .ok_or_else(|| AdmissionError::new(AdmissionKind::Closed))?;
        let state = reaper.register_root(Arc::clone(self), obligation)?;
        Ok(new_registered_root(state))
    }

    pub(crate) fn close_admission(&self) {
        self.accepting.store(false, Ordering::Release);
        self.logical.close();
        self.foreground.close();
        self.cleanup_roots.close();
    }
}

pub struct CellResources {
    host: Arc<HostResources>,
    logical: Arc<Semaphore>,
}

impl fmt::Debug for CellResources {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("CellResources")
            .finish_non_exhaustive()
    }
}

impl CellResources {
    pub fn new(host: Arc<HostResources>, limits: CellLimits) -> Arc<Self> {
        Arc::new(Self {
            host,
            logical: Arc::new(Semaphore::new(limits.logical_io.get())),
        })
    }

    pub fn host(&self) -> &Arc<HostResources> {
        &self.host
    }

    pub fn try_logical_io(&self) -> Result<LogicalIoPermit, AdmissionError> {
        self.host.ensure_accepting()?;
        let cell_permit =
            Arc::clone(&self.logical)
                .try_acquire_owned()
                .map_err(|error| match error {
                    TryAcquireError::NoPermits => AdmissionError::new(AdmissionKind::CellLogical),
                    TryAcquireError::Closed => AdmissionError::new(AdmissionKind::Closed),
                })?;
        // Cell is intentionally first. A Host rejection drops this local
        // permit immediately instead of retaining a partial reservation.
        let host_permit = self
            .host
            .try_owned(&self.host.logical, AdmissionKind::HostLogical)?;
        Ok(LogicalIoPermit {
            host: Arc::clone(&self.host),
            _host_permit: host_permit,
            cell_permit: Some(cell_permit),
        })
    }
}

pub struct LogicalIoPermit {
    host: Arc<HostResources>,
    _host_permit: OwnedSemaphorePermit,
    cell_permit: Option<OwnedSemaphorePermit>,
}

impl fmt::Debug for LogicalIoPermit {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("LogicalIoPermit")
            .field("cell", &self.cell_permit.is_some())
            .finish_non_exhaustive()
    }
}

pub struct ForegroundProducerPermit {
    host: Arc<HostResources>,
    _permit: OwnedSemaphorePermit,
}

impl fmt::Debug for ForegroundProducerPermit {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ForegroundProducerPermit")
            .finish_non_exhaustive()
    }
}

pub(crate) struct CleanupObligation {
    _permit: OwnedSemaphorePermit,
}

pub(crate) struct CleanupActivePermit {
    _permit: OwnedSemaphorePermit,
}

impl CleanupActivePermit {
    pub(crate) fn new(permit: OwnedSemaphorePermit) -> Self {
        Self { _permit: permit }
    }
}

pub struct ForegroundTransaction {
    pub(crate) inner: Arc<ForegroundTransactionInner>,
}

pub(crate) struct ForegroundTransactionInner {
    // Drop L and F before the final root receipt can make cleanup ready.
    logical: Option<LogicalIoPermit>,
    producer: ForegroundProducerPermit,
    cleanup_authorized: bool,
    work: Vec<FsWorkToken>,
    driver: HostFsDriver,
}

impl Clone for ForegroundTransaction {
    fn clone(&self) -> Self {
        Self {
            inner: Arc::clone(&self.inner),
        }
    }
}

impl fmt::Debug for ForegroundTransaction {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ForegroundTransaction")
            .field("roots", &self.inner.work.len())
            .finish_non_exhaustive()
    }
}

impl ForegroundTransaction {
    /// Adopts an elected producer and root receipts acquired before an
    /// external cache mutex. This is the shared-ensure construction path: the
    /// caller's logical permit remains caller-owned while the producer and
    /// origin/cache receipts survive waiter cancellation.
    pub fn from_parts(
        producer: ForegroundProducerPermit,
        work: Vec<FsWorkToken>,
    ) -> Result<Self, AdmissionError> {
        if work.is_empty()
            || work
                .iter()
                .any(|token| !token.belongs_to_host(&producer.host))
        {
            return Err(AdmissionError::new(AdmissionKind::Closed));
        }
        let cleanup_authorized = work.iter().all(FsWorkToken::cleanup_authorized);
        let driver = producer.host.driver.clone();
        Ok(Self {
            inner: Arc::new(ForegroundTransactionInner {
                logical: None,
                producer,
                cleanup_authorized,
                work,
                driver,
            }),
        })
    }

    /// Shares the same admitted transaction across a serial continuation.
    /// This does not authorize a second independently progressing producer.
    pub fn continuation(&self) -> Self {
        self.clone()
    }

    /// Shares a cleanup-bearing transaction that was admitted before effects.
    /// This remains valid after external owner close or later poison so its
    /// already-required rollback can finish. It does not admit a new effect.
    pub fn try_cleanup_continuation(&self) -> Result<Self, AdmissionError> {
        if !self.inner.cleanup_authorized {
            return Err(AdmissionError::new(AdmissionKind::CleanupPoisoned));
        }
        Ok(self.clone())
    }

    /// Admits one new cleanup-bearing phase against the roots' current poison
    /// state. The returned noncloneable phase can install one mandatory
    /// cleanup record after creating/receiving its private effect.
    pub fn try_cleanup_phase(&self) -> Result<CleanupPhase, AdmissionError> {
        if !self.inner.cleanup_authorized {
            return Err(AdmissionError::new(AdmissionKind::CleanupPoisoned));
        }
        for token in &self.inner.work {
            token.check_current_cleanup_admission()?;
        }
        Ok(CleanupPhase::new(self.clone()))
    }

    pub(crate) fn driver(&self) -> &HostFsDriver {
        &self.inner.driver
    }

    pub(crate) fn host(&self) -> &Arc<HostResources> {
        match &self.inner.logical {
            Some(logical) => &logical.host,
            None => &self.inner.producer.host,
        }
    }

    pub(crate) fn record_failure(&self, error: crate::FsWorkError) {
        for token in &self.inner.work {
            token.record_failure(error.clone());
        }
    }

    pub(crate) fn owns_scope(&self, scope: &FsScope) -> bool {
        self.inner.work.iter().any(|token| token.belongs_to(scope))
    }
}
