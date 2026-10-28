//! Bounded counters for fuel decisions and settled consumption across one Host's Cells.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use crossbeam_utils::CachePadded;

// Bound unpublished settlement events, not elapsed time or fuel units.
const SETTLEMENT_BATCH: u16 = 256;

/// Shared production collector. Clones retain counts after Cells terminate.
/// Independent collectors have independent lifetimes; there is no global owner.
#[derive(Clone, Default)]
pub struct FuelMetrics {
    counters: Arc<FuelCounters>,
}

#[derive(Default)]
struct FuelCounters {
    decisions: CachePadded<[[AtomicU64; 3]; 2]>,
    // CachePadded uses target-dependent alignment estimates. Counting correctness
    // does not depend on those estimates matching every CPU's cache geometry.
    settled: CachePadded<[AtomicU64; 2]>,
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

/// Independently loaded published counters, not a transaction across series.
/// Settled totals can lag live Stores, including idle Stores. They are exact
/// after all relevant writers flush or complete destruction, modulo 2^64.
/// Unsettled termination tails are excluded. Exits without Rust destruction
/// can lose pending values.
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
    /// Create a unique pending-counter owner for one Store. Moving the owner
    /// transfers its pending values; cloning the collector never copies them.
    pub(crate) fn writer(&self) -> FuelMetricsWriter {
        FuelMetricsWriter {
            metrics: self.clone(),
            pending: [0; 2],
            settlements: 0,
        }
    }

    pub fn snapshot(&self) -> FuelSnapshot {
        let policy = |index: usize| FuelDecisionSnapshot {
            grant: self.counters.decisions[index][0].load(Ordering::Relaxed),
            suspend: self.counters.decisions[index][1].load(Ordering::Relaxed),
            exhausted: self.counters.decisions[index][2].load(Ordering::Relaxed),
        };
        FuelSnapshot {
            scheduled: policy(0),
            oneshot: policy(1),
            scheduled_settled_units: self.counters.settled[0].load(Ordering::Relaxed),
            oneshot_settled_units: self.counters.settled[1].load(Ordering::Relaxed),
        }
    }
}

/// Exclusive Store-owned pending values. Deliberately neither Clone nor Copy.
/// Drop publishes committed settlements but never settles remaining guest fuel.
pub(crate) struct FuelMetricsWriter {
    metrics: FuelMetrics,
    pending: [u64; 2],
    settlements: u16,
}

impl FuelMetricsWriter {
    pub(crate) fn record_grant(&self, policy: FuelPolicy) {
        self.metrics.counters.decisions[policy as usize][0].fetch_add(1, Ordering::Relaxed);
    }

    pub(crate) fn record_suspend(&self, policy: FuelPolicy) {
        self.metrics.counters.decisions[policy as usize][1].fetch_add(1, Ordering::Relaxed);
    }

    pub(crate) fn record_exhausted(&self, policy: FuelPolicy) {
        self.metrics.counters.decisions[policy as usize][2].fetch_add(1, Ordering::Relaxed);
    }

    /// Record every authoritative settlement, including zero deltas. Between
    /// publications this touches only ordinary integers owned by the Store.
    #[inline]
    pub(crate) fn record_settled(&mut self, policy: FuelPolicy, consumed: u64) {
        let pending = &mut self.pending[policy as usize];
        *pending = pending.wrapping_add(consumed);
        self.settlements += 1;
        if self.settlements == SETTLEMENT_BATCH {
            self.flush();
        }
    }

    /// Publish under exclusive ownership, without awaits or fallible work.
    /// Clearing first makes subsequent explicit flushes and Drop no-ops.
    #[cold]
    pub(crate) fn flush(&mut self) {
        let pending = std::mem::take(&mut self.pending);
        self.settlements = 0;
        for (counter, amount) in self.metrics.counters.settled.iter().zip(pending) {
            if amount != 0 {
                counter.fetch_add(amount, Ordering::Relaxed);
            }
        }
    }
}

impl Drop for FuelMetricsWriter {
    fn drop(&mut self) {
        self.flush();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Barrier;

    #[test]
    fn store_local_settlement_waits_for_publication_and_drop_flushes() {
        let metrics = FuelMetrics::default();
        let mut writer = metrics.writer();
        writer.record_settled(FuelPolicy::Scheduled, 7);
        assert_eq!(metrics.snapshot().scheduled_settled_units, 0);
        drop(writer);
        assert_eq!(metrics.snapshot().scheduled_settled_units, 7);
    }

    #[test]
    fn threshold_publishes_exact_policy_totals_and_counts_zero_deltas() {
        let metrics = FuelMetrics::default();
        let mut writer = metrics.writer();
        writer.record_settled(FuelPolicy::Scheduled, 7);
        writer.record_settled(FuelPolicy::Oneshot, 11);
        for _ in 2..255 {
            writer.record_settled(FuelPolicy::Scheduled, 0);
        }
        assert_eq!(metrics.snapshot(), FuelSnapshot::default());
        writer.record_settled(FuelPolicy::Oneshot, 0);
        let published = metrics.snapshot();
        assert_eq!(published.scheduled_settled_units, 7);
        assert_eq!(published.oneshot_settled_units, 11);
        writer.record_settled(FuelPolicy::Scheduled, 13);
        assert_eq!(metrics.snapshot(), published);
        drop(writer);
        assert_eq!(metrics.snapshot().scheduled_settled_units, 20);
        assert_eq!(metrics.snapshot().oneshot_settled_units, 11);
    }

    #[test]
    fn explicit_flush_resets_threshold_and_drop_does_not_duplicate() {
        let metrics = FuelMetrics::default();
        let mut writer = metrics.writer();
        for _ in 0..255 {
            writer.record_settled(FuelPolicy::Scheduled, 1);
        }
        writer.flush();
        writer.flush();
        assert_eq!(metrics.snapshot().scheduled_settled_units, 255);
        for _ in 0..255 {
            writer.record_settled(FuelPolicy::Oneshot, 1);
        }
        assert_eq!(metrics.snapshot().oneshot_settled_units, 0);
        writer.record_settled(FuelPolicy::Oneshot, 1);
        assert_eq!(metrics.snapshot().oneshot_settled_units, 256);
        let published = metrics.snapshot();
        drop(writer);
        assert_eq!(metrics.snapshot(), published);
    }

    #[test]
    fn moved_writer_retains_pending_ownership_and_collector_lifetime() {
        let metrics = FuelMetrics::default();
        let retained = metrics.clone();
        let independent = FuelMetrics::default();
        let mut writer = metrics.writer();
        writer.record_settled(FuelPolicy::Scheduled, 17);
        drop(metrics);
        let mut writer = std::thread::spawn(move || {
            writer.record_settled(FuelPolicy::Oneshot, 23);
            writer
        })
        .join()
        .unwrap();
        assert_eq!(retained.snapshot(), FuelSnapshot::default());
        writer.record_settled(FuelPolicy::Scheduled, 5);
        std::thread::spawn(move || drop(writer)).join().unwrap();
        assert_eq!(retained.snapshot().scheduled_settled_units, 22);
        assert_eq!(retained.snapshot().oneshot_settled_units, 23);
        assert_eq!(independent.snapshot(), FuelSnapshot::default());
    }

    #[test]
    fn colliding_publications_and_concurrent_snapshots_retain_exact_totals() {
        let metrics = FuelMetrics::default();
        let barrier = Barrier::new(5);
        let mut snapshots = Vec::new();
        std::thread::scope(|scope| {
            for _ in 0..4 {
                let mut writer = metrics.writer();
                let barrier = &barrier;
                scope.spawn(move || {
                    barrier.wait();
                    for _ in 0..1001 {
                        writer.record_settled(FuelPolicy::Scheduled, 7);
                        writer.record_settled(FuelPolicy::Oneshot, 11);
                        writer.record_grant(FuelPolicy::Scheduled);
                    }
                    // Scope join establishes actual destruction, not only quiescence.
                });
            }
            barrier.wait();
            for _ in 0..128 {
                snapshots.push(metrics.snapshot());
            }
        });
        for pair in snapshots.windows(2) {
            assert!(pair[0].scheduled_settled_units <= pair[1].scheduled_settled_units);
            assert!(pair[0].oneshot_settled_units <= pair[1].oneshot_settled_units);
            assert!(pair[0].scheduled.grant <= pair[1].scheduled.grant);
        }
        let snapshot = metrics.snapshot();
        assert_eq!(snapshot.scheduled_settled_units, 28_028);
        assert_eq!(snapshot.oneshot_settled_units, 44_044);
        assert_eq!(snapshot.scheduled.grant, 4_004);
        assert_eq!(snapshot.scheduled.suspend, 0);
        assert_eq!(snapshot.scheduled.exhausted, 0);
        assert_eq!(snapshot.oneshot, FuelDecisionSnapshot::default());
    }

    #[test]
    fn local_and_published_counter_wrap_preserves_modular_totals() {
        let metrics = FuelMetrics::default();
        let mut first = metrics.writer();
        let mut second = metrics.writer();
        first.record_settled(FuelPolicy::Scheduled, u64::MAX - 3);
        first.record_settled(FuelPolicy::Scheduled, 7);
        first.flush();
        assert_eq!(metrics.snapshot().scheduled_settled_units, 3);
        second.record_settled(FuelPolicy::Scheduled, u64::MAX);
        second.flush();
        assert_eq!(metrics.snapshot().scheduled_settled_units, 2);
        first.record_settled(FuelPolicy::Oneshot, u64::MAX);
        first.record_settled(FuelPolicy::Oneshot, 1);
        // A wrapped zero sum must still reset the settlement-event count.
        first.flush();
        for _ in 0..255 {
            first.record_settled(FuelPolicy::Oneshot, 1);
        }
        assert_eq!(metrics.snapshot().oneshot_settled_units, 0);
        first.record_settled(FuelPolicy::Oneshot, 1);
        assert_eq!(metrics.snapshot().oneshot_settled_units, 256);
        drop(first);
        drop(second);
        assert_eq!(metrics.snapshot().scheduled_settled_units, 2);
        assert_eq!(metrics.snapshot().oneshot_settled_units, 256);
    }
}
