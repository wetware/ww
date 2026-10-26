//! Bounded counters for fuel decisions and settled consumption across one Host's Cells.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

/// Shared production collector. Clones retain counts after Cells terminate.
/// Independent collectors have independent lifetimes; there is no global owner.
#[derive(Clone, Default)]
pub struct FuelMetrics {
    counters: Arc<[[AtomicU64; 4]; 2]>,
}

#[derive(Clone, Copy)]
pub(crate) enum FuelPolicy {
    Scheduled = 0,
    Oneshot = 1,
}

/// Applied outcomes for one fixed fuel policy.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct FuelDecisionSnapshot {
    pub grant: u64,
    pub suspend: u64,
    pub exhausted: u64,
}

/// Independently loaded counters, not an atomic view across all eight series.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct FuelSnapshot {
    pub scheduled: FuelDecisionSnapshot,
    pub oneshot: FuelDecisionSnapshot,
    /// Physical fuel accounted for at authoritative scheduled settlement boundaries.
    /// Excludes consumption left unsettled at termination.
    pub scheduled_settled_units: u64,
    /// Physical fuel accounted for at authoritative one-shot settlement boundaries.
    /// Includes reserve overshoot; saturating ledger debits can be smaller.
    pub oneshot_settled_units: u64,
}

impl FuelMetrics {
    pub fn snapshot(&self) -> FuelSnapshot {
        let policy = |index: usize| FuelDecisionSnapshot {
            grant: self.counters[index][0].load(Ordering::Relaxed),
            suspend: self.counters[index][1].load(Ordering::Relaxed),
            exhausted: self.counters[index][2].load(Ordering::Relaxed),
        };
        FuelSnapshot {
            scheduled: policy(0),
            oneshot: policy(1),
            scheduled_settled_units: self.counters[0][3].load(Ordering::Relaxed),
            oneshot_settled_units: self.counters[1][3].load(Ordering::Relaxed),
        }
    }

    pub(crate) fn record_grant(&self, policy: FuelPolicy) {
        self.counters[policy as usize][0].fetch_add(1, Ordering::Relaxed);
    }

    pub(crate) fn record_suspend(&self, policy: FuelPolicy) {
        self.counters[policy as usize][1].fetch_add(1, Ordering::Relaxed);
    }

    pub(crate) fn record_exhausted(&self, policy: FuelPolicy) {
        self.counters[policy as usize][2].fetch_add(1, Ordering::Relaxed);
    }

    pub(crate) fn record_settled(&self, policy: FuelPolicy, consumed: u64) {
        if consumed != 0 {
            self.counters[policy as usize][3].fetch_add(consumed, Ordering::Relaxed);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn concurrent_settlements_retain_exact_policy_totals_after_writers_drop() {
        let metrics = FuelMetrics::default();
        let unrelated = FuelMetrics::default();
        std::thread::scope(|scope| {
            for _ in 0..4 {
                let writer = metrics.clone();
                scope.spawn(move || {
                    for _ in 0..1_000 {
                        writer.record_settled(FuelPolicy::Scheduled, 7);
                        writer.record_settled(FuelPolicy::Oneshot, 11);
                        writer.record_settled(FuelPolicy::Oneshot, 0);
                    }
                });
            }
        });
        let snapshot = metrics.snapshot();
        assert_eq!(snapshot.scheduled_settled_units, 28_000);
        assert_eq!(snapshot.oneshot_settled_units, 44_000);
        assert_eq!(snapshot.scheduled, FuelDecisionSnapshot::default());
        assert_eq!(snapshot.oneshot, FuelDecisionSnapshot::default());
        assert_eq!(unrelated.snapshot(), FuelSnapshot::default());
    }
}
