//! Bounded admission for inbound long-lived connections.

use std::fmt;
use std::sync::Arc;

use tokio::sync::{OwnedSemaphorePermit, Semaphore};

pub const DEFAULT_MAX_INBOUND_CONNECTIONS: usize = 64;

#[derive(Clone)]
pub struct ConnectionBudget {
    semaphore: Arc<Semaphore>,
    capacity: usize,
}

impl Default for ConnectionBudget {
    fn default() -> Self {
        Self::new(DEFAULT_MAX_INBOUND_CONNECTIONS)
            .expect("default inbound connection limit is non-zero")
    }
}

impl ConnectionBudget {
    pub fn new(capacity: usize) -> Result<Self, InvalidConnectionLimit> {
        if capacity == 0 {
            return Err(InvalidConnectionLimit);
        }
        Ok(Self {
            semaphore: Arc::new(Semaphore::new(capacity)),
            capacity,
        })
    }

    pub fn capacity(&self) -> usize {
        self.capacity
    }

    pub fn active(&self) -> usize {
        self.capacity - self.semaphore.available_permits()
    }

    pub fn try_acquire(&self) -> Result<ConnectionPermit, ConnectionLimitReached> {
        Arc::clone(&self.semaphore)
            .try_acquire_owned()
            .map(ConnectionPermit)
            .map_err(|_| ConnectionLimitReached {
                capacity: self.capacity,
            })
    }
}

pub struct ConnectionPermit(#[allow(dead_code)] OwnedSemaphorePermit);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct InvalidConnectionLimit;

impl fmt::Display for InvalidConnectionLimit {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("inbound connection limit must be greater than zero")
    }
}

impl std::error::Error for InvalidConnectionLimit {}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ConnectionLimitReached {
    pub capacity: usize,
}

impl fmt::Display for ConnectionLimitReached {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "inbound connection limit reached (capacity {})",
            self.capacity
        )
    }
}

impl std::error::Error for ConnectionLimitReached {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_capacity_is_64() {
        assert_eq!(
            ConnectionBudget::default().capacity(),
            DEFAULT_MAX_INBOUND_CONNECTIONS
        );
    }

    #[test]
    fn zero_capacity_is_rejected() {
        assert!(ConnectionBudget::new(0).is_err());
    }

    #[test]
    fn concurrent_last_slot_has_one_winner() {
        let budget = ConnectionBudget::new(1).unwrap();
        let first = budget.clone();
        let second = budget.clone();
        let (left, right) = std::thread::scope(|scope| {
            let left = scope.spawn(move || first.try_acquire());
            let right = scope.spawn(move || second.try_acquire());
            (left.join().unwrap(), right.join().unwrap())
        });
        assert_ne!(left.is_ok(), right.is_ok());
        assert_eq!(budget.active(), 1);
    }

    #[test]
    fn dropping_permit_releases_capacity() {
        let budget = ConnectionBudget::new(1).unwrap();
        let permit = budget.try_acquire().unwrap();
        assert!(budget.try_acquire().is_err());
        drop(permit);
        assert!(budget.try_acquire().is_ok());
    }

    #[tokio::test]
    async fn cancellation_releases_capacity() {
        let budget = ConnectionBudget::new(1).unwrap();
        let permit = budget.try_acquire().unwrap();
        let task = tokio::spawn(async move {
            let _permit = permit;
            std::future::pending::<()>().await;
        });
        tokio::task::yield_now().await;
        assert!(budget.try_acquire().is_err());
        task.abort();
        let _ = task.await;
        assert!(budget.try_acquire().is_ok());
    }
}
