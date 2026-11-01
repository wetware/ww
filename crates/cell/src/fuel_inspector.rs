//! Bounded, host-only requests for one Store owner's fuel snapshot.
//!
//! Possession of a [`FuelInspector`] selects one process generation. The handle
//! exposes no guest capability and never accesses a Store. Only the Store owner
//! constructs snapshots at an authoritative fuel boundary.

use std::fmt;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::{Duration, Instant};
use tokio::sync::{mpsc, oneshot, OwnedSemaphorePermit, Semaphore};

/// Maximum deadline interval for one request. Delivery requires caller scheduling.
pub const MAX_INSPECTION_TIMEOUT: Duration = Duration::from_secs(1);

/// Fuel policy recorded by the Store owner.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FuelInspectionPolicy {
    Scheduled,
    Oneshot,
}

impl From<crate::metrics::FuelPolicy> for FuelInspectionPolicy {
    fn from(policy: crate::metrics::FuelPolicy) -> Self {
        match policy {
            crate::metrics::FuelPolicy::Scheduled => Self::Scheduled,
            crate::metrics::FuelPolicy::Oneshot => Self::Oneshot,
        }
    }
}

/// State at the capture boundary, not a prediction of later execution.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FuelState {
    Ready,
    Running,
    Suspended,
    Exhausted,
}

/// Fuel-flushed boundary at which a settlement committed.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SettlementBoundary {
    HostReturn,
    Slice,
}

/// Last committed physical-fuel settlement, including any reserve overshoot.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct LastSettlement {
    pub consumed: u64,
    pub physical_before: u64,
    pub physical_remaining: u64,
    pub authority_before: u64,
    pub epoch: u64,
    pub boundary: SettlementBoundary,
}

/// Last grant whose Wasmtime yield configuration and fuel installation succeeded.
///
/// The ledgers and desired quantum describe the grant's application boundary.
/// Later settlement or epoch opening can change the current snapshot values.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct AppliedGrant {
    pub authority: u64,
    pub physical: u64,
    pub yield_interval: u64,
    pub desired_quantum: u64,
    pub total_remaining: Option<u64>,
    pub epoch_remaining: Option<u64>,
    pub opened_epoch: u64,
}

/// Immutable value copied from one Store at one authoritative fuel boundary.
///
/// `published_epoch` is an independent Host-clock observation. It is not part
/// of a transaction with the owner-held fields. Scheduled-policy ledger values
/// are `None`, because no cumulative authority limit applies. Missing physical
/// fuel or historical records are also explicit; zero is never an unavailable
/// sentinel. A retained snapshot remains historical even if its owner stops.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct FuelInspectionSnapshot {
    /// Process-local identity, never reused for a later Store generation.
    pub identity: u64,
    pub captured_at: Instant,
    pub policy: FuelInspectionPolicy,
    pub state: FuelState,
    pub ewma_initialized: bool,
    pub avg_ratio: u64,
    pub ratio_scale: u64,
    pub desired_quantum: u64,
    pub min_quantum: u64,
    pub max_quantum: u64,
    pub authority_grant: u64,
    /// Baseline for the next settlement; not necessarily the last installation.
    pub physical_accounting_baseline: u64,
    pub physical_remaining: Option<u64>,
    pub yield_reserve: u64,
    pub opened_epoch: u64,
    pub pending_epoch: Option<u64>,
    /// Independently observed latest Host publication, not an opened allowance.
    pub published_epoch: u64,
    pub total_remaining: Option<u64>,
    pub epoch_remaining: Option<u64>,
    pub epoch_limit: Option<u64>,
    pub last_settlement: Option<LastSettlement>,
    pub last_applied_grant: Option<AppliedGrant>,
}

impl FuelInspectionSnapshot {
    /// Age of this historical value when the caller evaluates it.
    pub fn age(&self) -> Duration {
        self.captured_at.elapsed()
    }
}

/// A request could not obtain a fresh owner snapshot.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum InspectError {
    /// Another request or unread response holds the single admission permit.
    Busy,
    /// The owner did not answer before the bounded deadline.
    Timeout,
    /// The request channel closed; no terminal snapshot is inferred.
    OwnerStopped,
}

impl fmt::Display for InspectError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Busy => "fuel inspector already has an outstanding request",
            Self::Timeout => "fuel inspection deadline elapsed",
            Self::OwnerStopped => "fuel inspection owner stopped",
        })
    }
}

impl std::error::Error for InspectError {}

/// Host-only read authority for one Store generation.
///
/// Clones share one admission permit and a capacity-one mailbox. Admission never
/// waits. A timed-out or cancelled queued request keeps its permit until the
/// owner discards it, so repeated deadlines cannot accumulate queued work.
#[derive(Clone)]
pub struct FuelInspector {
    identity: u64,
    tx: mpsc::Sender<Request>,
    admission: Arc<Semaphore>,
}

impl FuelInspector {
    pub fn identity(&self) -> u64 {
        self.identity
    }

    /// Request a new owner snapshot with a deadline capped at one second.
    ///
    /// A deadline does not interrupt guest execution or borrow the Store. A
    /// blocked owner can therefore produce `Timeout`. The requester must be
    /// scheduled to deliver that timeout; this is not a wall-clock return bound.
    /// Timeout and owner stop
    /// return no cached state. Previously returned snapshots retain their
    /// original identity and capture time.
    pub async fn inspect(&self, timeout: Duration) -> Result<FuelInspectionSnapshot, InspectError> {
        if self.tx.is_closed() {
            return Err(InspectError::OwnerStopped);
        }
        let timeout = timeout.min(MAX_INSPECTION_TIMEOUT);
        if timeout.is_zero() {
            return Err(InspectError::Timeout);
        }
        let permit = Arc::clone(&self.admission)
            .try_acquire_owned()
            .map_err(|_| InspectError::Busy)?;
        let expires_at = Instant::now() + timeout;
        let (reply, response) = oneshot::channel();
        self.tx
            .try_send(Request {
                expires_at,
                reply,
                permit,
            })
            .map_err(|error| match error {
                mpsc::error::TrySendError::Full(_) => InspectError::Busy,
                mpsc::error::TrySendError::Closed(_) => InspectError::OwnerStopped,
            })?;
        let response = tokio::time::timeout_at(expires_at.into(), response)
            .await
            .map_err(|_| InspectError::Timeout)?;
        // The permit remains held until this response is consumed or dropped.
        // Check the wall-clock deadline even if a late response and timer become
        // ready together, because timeout polls its inner future first.
        if Instant::now() >= expires_at {
            return Err(InspectError::Timeout);
        }
        let response = response.map_err(|_| InspectError::OwnerStopped)?;
        Ok(response.snapshot)
    }
}

struct Response {
    snapshot: FuelInspectionSnapshot,
    _permit: OwnedSemaphorePermit,
}

pub(crate) struct Request {
    expires_at: Instant,
    reply: oneshot::Sender<Response>,
    permit: OwnedSemaphorePermit,
}

impl Request {
    pub(crate) fn is_closed(&self) -> bool {
        self.reply.is_closed()
    }

    pub(crate) fn is_expired(&self) -> bool {
        Instant::now() >= self.expires_at
    }

    pub(crate) fn respond(self, snapshot: FuelInspectionSnapshot) {
        if self.is_closed() || self.is_expired() {
            return;
        }
        let _ = self.reply.send(Response {
            snapshot,
            _permit: self.permit,
        });
    }
}

/// Store-owned request receiver and fixed-size decision history.
pub(crate) struct Owner {
    pub(crate) identity: u64,
    pub(crate) state: FuelState,
    pub(crate) last_settlement: Option<LastSettlement>,
    pub(crate) last_applied_grant: Option<AppliedGrant>,
    rx: mpsc::Receiver<Request>,
}

impl Owner {
    pub(crate) fn new() -> (Self, FuelInspector) {
        static NEXT_IDENTITY: AtomicU64 = AtomicU64::new(1);
        let identity = NEXT_IDENTITY
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |value| {
                value.checked_add(1)
            })
            .expect("fuel inspection generation space exhausted");
        let (tx, rx) = mpsc::channel(1);
        (
            Self {
                identity,
                state: FuelState::Ready,
                last_settlement: None,
                last_applied_grant: None,
                rx,
            },
            FuelInspector {
                identity,
                tx,
                admission: Arc::new(Semaphore::new(1)),
            },
        )
    }

    /// Consume at most one queued request. Never loop on a runnable fuel path.
    pub(crate) fn try_request(&mut self) -> Option<Request> {
        self.rx
            .try_recv()
            .ok()
            .filter(|request| !request.is_closed() && !request.is_expired())
    }

    /// Poll requests while the Store is already waiting for an epoch.
    ///
    /// Consume at most one message per poll. After discarding a stale message,
    /// arrange another poll to register the receiver waker or receive a new
    /// request. This bounds work even if callers repeatedly cancel requests.
    pub(crate) fn poll_request(&mut self, cx: &mut Context<'_>) -> Poll<Option<Request>> {
        match self.rx.poll_recv(cx) {
            Poll::Ready(Some(request)) if request.is_closed() || request.is_expired() => {
                drop(request);
                cx.waker().wake_by_ref();
                Poll::Pending
            }
            poll => poll,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn snapshot(identity: u64) -> FuelInspectionSnapshot {
        FuelInspectionSnapshot {
            identity,
            captured_at: Instant::now(),
            policy: FuelInspectionPolicy::Oneshot,
            state: FuelState::Running,
            ewma_initialized: false,
            avg_ratio: 500,
            ratio_scale: 1_000,
            desired_quantum: 1_000_000,
            min_quantum: 10_000,
            max_quantum: 10_000_000,
            authority_grant: 1_337,
            physical_accounting_baseline: 10_001_337,
            physical_remaining: Some(10_001_337),
            yield_reserve: 10_000_000,
            opened_epoch: 0,
            pending_epoch: None,
            published_epoch: 0,
            total_remaining: Some(1_337),
            epoch_remaining: Some(10_000_000),
            epoch_limit: Some(10_000_000),
            last_settlement: None,
            last_applied_grant: None,
        }
    }

    #[tokio::test]
    async fn admission_stays_held_through_unread_response() {
        let (mut owner, inspector) = Owner::new();
        let clone = inspector.clone();
        let mut first = Box::pin(inspector.inspect(MAX_INSPECTION_TIMEOUT));
        assert!(futures::poll!(&mut first).is_pending());
        for _ in 0..32 {
            assert_eq!(
                clone.inspect(MAX_INSPECTION_TIMEOUT).await,
                Err(InspectError::Busy)
            );
        }
        let expected = snapshot(inspector.identity());
        owner
            .try_request()
            .expect("one queued request")
            .respond(expected);
        assert!(owner.try_request().is_none());
        assert_eq!(
            clone.inspect(MAX_INSPECTION_TIMEOUT).await,
            Err(InspectError::Busy)
        );
        assert_eq!(first.await, Ok(expected));

        let mut second = Box::pin(inspector.inspect(MAX_INSPECTION_TIMEOUT));
        assert!(futures::poll!(&mut second).is_pending());
        owner.try_request().unwrap().respond(expected);
        assert_eq!(second.await, Ok(expected));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn simultaneous_readers_share_one_admission_and_one_unread_response() {
        const READERS: usize = 64;
        let (mut owner, inspector) = Owner::new();
        let start = Arc::new(tokio::sync::Barrier::new(READERS + 1));
        let release = Arc::new(tokio::sync::Notify::new());
        let (admitted_tx, mut admitted_rx) = mpsc::channel(READERS);
        let mut readers = tokio::task::JoinSet::new();
        for _ in 0..READERS {
            let inspector = inspector.clone();
            let start = Arc::clone(&start);
            let release = Arc::clone(&release);
            let admitted_tx = admitted_tx.clone();
            readers.spawn(async move {
                start.wait().await;
                let mut request = Box::pin(inspector.inspect(MAX_INSPECTION_TIMEOUT));
                match futures::poll!(&mut request) {
                    Poll::Pending => {
                        admitted_tx.send(true).await.unwrap();
                        // Retain the future without reading its eventual response.
                        release.notified().await;
                        request.await
                    }
                    Poll::Ready(result) => {
                        admitted_tx.send(false).await.unwrap();
                        assert_eq!(result, Err(InspectError::Busy));
                        result
                    }
                }
            });
        }
        drop(admitted_tx);
        start.wait().await;
        let mut admitted = 0;
        let mut attempted = 0;
        while let Some(accepted) = admitted_rx.recv().await {
            admitted += usize::from(accepted);
            attempted += 1;
            if attempted == READERS {
                break;
            }
        }
        assert_eq!(attempted, READERS);
        assert_eq!(admitted, 1);
        let expected = snapshot(inspector.identity());
        owner.try_request().unwrap().respond(expected);
        assert!(owner.try_request().is_none());
        assert_eq!(
            inspector.inspect(MAX_INSPECTION_TIMEOUT).await,
            Err(InspectError::Busy),
            "the unread response must retain admission"
        );
        release.notify_one();
        let mut responses = 0;
        let mut rejected = 0;
        while let Some(result) = readers.join_next().await {
            match result.unwrap() {
                Ok(snapshot) => {
                    assert_eq!(snapshot, expected);
                    responses += 1;
                }
                Err(InspectError::Busy) => rejected += 1,
                Err(error) => panic!("unexpected inspection error: {error}"),
            }
        }
        assert_eq!((responses, rejected), (1, READERS - 1));
    }

    #[tokio::test]
    async fn cancelled_requests_are_discarded_before_capture() {
        let (mut owner, inspector) = Owner::new();
        let mut pending = Box::pin(inspector.inspect(MAX_INSPECTION_TIMEOUT));
        assert!(futures::poll!(&mut pending).is_pending());
        drop(pending);
        assert_eq!(
            inspector.inspect(MAX_INSPECTION_TIMEOUT).await,
            Err(InspectError::Busy)
        );
        assert!(owner.try_request().is_none());

        let mut next = Box::pin(inspector.inspect(MAX_INSPECTION_TIMEOUT));
        assert!(futures::poll!(&mut next).is_pending());
        let expected = snapshot(inspector.identity());
        std::future::poll_fn(|cx| owner.poll_request(cx))
            .await
            .unwrap()
            .respond(expected);
        assert_eq!(next.await, Ok(expected));
    }

    #[tokio::test]
    async fn timeout_retains_capacity_until_owner_discards_stale_request() {
        let (mut owner, inspector) = Owner::new();
        assert_eq!(
            inspector.inspect(Duration::from_millis(1)).await,
            Err(InspectError::Timeout)
        );
        assert_eq!(
            inspector.inspect(MAX_INSPECTION_TIMEOUT).await,
            Err(InspectError::Busy)
        );
        assert!(owner.try_request().is_none());
        let mut next = Box::pin(inspector.inspect(MAX_INSPECTION_TIMEOUT));
        assert!(futures::poll!(&mut next).is_pending());
        drop(owner);
        assert_eq!(next.await, Err(InspectError::OwnerStopped));
    }

    #[tokio::test]
    async fn stopping_owner_closes_requests_and_generations_do_not_alias() {
        let (owner, inspector) = Owner::new();
        let old_identity = inspector.identity();
        let retained = snapshot(old_identity);
        let mut pending = Box::pin(inspector.inspect(MAX_INSPECTION_TIMEOUT));
        assert!(futures::poll!(&mut pending).is_pending());
        drop(owner);
        assert_eq!(pending.await, Err(InspectError::OwnerStopped));
        assert_eq!(
            inspector.inspect(MAX_INSPECTION_TIMEOUT).await,
            Err(InspectError::OwnerStopped)
        );

        let (_replacement, replacement_inspector) = Owner::new();
        assert_ne!(replacement_inspector.identity(), old_identity);
        assert_eq!(retained.identity, old_identity);
    }

    #[tokio::test]
    async fn dropped_unread_response_releases_capacity() {
        let (mut owner, inspector) = Owner::new();
        let mut pending = Box::pin(inspector.inspect(MAX_INSPECTION_TIMEOUT));
        assert!(futures::poll!(&mut pending).is_pending());
        owner
            .try_request()
            .unwrap()
            .respond(snapshot(inspector.identity()));
        drop(pending);
        let mut next = Box::pin(inspector.inspect(MAX_INSPECTION_TIMEOUT));
        assert_eq!(futures::poll!(&mut next), Poll::Pending);
        assert!(owner.try_request().is_some());
    }

    #[tokio::test]
    async fn caller_cannot_extend_deadline_or_queue_zero_duration_request() {
        let (mut owner, inspector) = Owner::new();
        assert_eq!(
            inspector.inspect(Duration::ZERO).await,
            Err(InspectError::Timeout)
        );
        assert!(owner.try_request().is_none());
        let mut pending = Box::pin(inspector.inspect(Duration::MAX));
        assert!(futures::poll!(&mut pending).is_pending());
        let request = owner.try_request().unwrap();
        assert!(
            request.expires_at.saturating_duration_since(Instant::now()) <= MAX_INSPECTION_TIMEOUT
        );
        request.respond(snapshot(inspector.identity()));
        assert!(pending.await.is_ok());
    }

    #[tokio::test]
    async fn closed_request_channel_does_not_wait_for_an_epoch() {
        let (mut owner, inspector) = Owner::new();
        drop(inspector);
        assert!(std::future::poll_fn(|cx| owner.poll_request(cx))
            .await
            .is_none());
    }
}
