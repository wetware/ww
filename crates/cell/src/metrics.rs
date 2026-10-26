//! Bounded counters for applied fuel decisions across one Host's Cells.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

/// Shared production collector. Clones retain counts after Cells terminate.
/// Independent collectors have independent lifetimes; there is no global owner.
#[derive(Clone, Default)]
pub struct FuelMetrics {
    counters: Arc<[[AtomicU64; 3]; 2]>,
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

/// Independently loaded counters, not an atomic view across all six series.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct FuelSnapshot {
    pub scheduled: FuelDecisionSnapshot,
    pub oneshot: FuelDecisionSnapshot,
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
}
