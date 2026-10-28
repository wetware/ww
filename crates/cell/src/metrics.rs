//! Bounded counters for fuel decisions and settled consumption across one Host's Cells.

use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::Arc;

use crossbeam_utils::CachePadded;

// Bound storage independently of the number of Cells created over a Host's life.
// Colliding writers remain correct; this distributes writes, not ownership.
const SETTLEMENT_SHARDS: usize = 32;

/// Shared production collector. Clones retain counts after Cells terminate.
/// Independent collectors have independent lifetimes; there is no global owner.
#[derive(Clone, Default)]
pub struct FuelMetrics {
    counters: Arc<FuelCounters>,
    shard: usize,
}

#[derive(Default)]
struct FuelCounters {
    decisions: CachePadded<[[AtomicU64; 3]; 2]>,
    // CachePadded uses target-dependent alignment estimates. Counting correctness
    // does not depend on those estimates matching every CPU's cache geometry.
    settled: [CachePadded<[AtomicU64; 2]>; SETTLEMENT_SHARDS],
    next_writer: CachePadded<AtomicUsize>,
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

/// Independently loaded counters, not a transactional view across shards or series.
/// Totals are exact after writers quiesce. Concurrent snapshots can combine
/// observations from different instants; additions retain u64 wrapping semantics.
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
    /// Select once during Store construction, outside settlement. A writer keeps
    /// its assignment when cloned or moved between threads. Multiple Stores can
    /// share a shard; there is no registration, reclamation, or drop-time flush.
    pub(crate) fn writer(&self) -> Self {
        let ordinal = self.counters.next_writer.fetch_add(1, Ordering::Relaxed);
        Self {
            counters: Arc::clone(&self.counters),
            shard: ordinal % SETTLEMENT_SHARDS,
        }
    }

    pub fn snapshot(&self) -> FuelSnapshot {
        let policy = |index: usize| FuelDecisionSnapshot {
            grant: self.counters.decisions[index][0].load(Ordering::Relaxed),
            suspend: self.counters.decisions[index][1].load(Ordering::Relaxed),
            exhausted: self.counters.decisions[index][2].load(Ordering::Relaxed),
        };
        let mut settled = [0u64; 2];
        for shard in &self.counters.settled {
            for (total, counter) in settled.iter_mut().zip(shard.iter()) {
                *total = total.wrapping_add(counter.load(Ordering::Relaxed));
            }
        }
        FuelSnapshot {
            scheduled: policy(0),
            oneshot: policy(1),
            scheduled_settled_units: settled[0],
            oneshot_settled_units: settled[1],
        }
    }

    pub(crate) fn record_grant(&self, policy: FuelPolicy) {
        self.counters.decisions[policy as usize][0].fetch_add(1, Ordering::Relaxed);
    }

    pub(crate) fn record_suspend(&self, policy: FuelPolicy) {
        self.counters.decisions[policy as usize][1].fetch_add(1, Ordering::Relaxed);
    }

    pub(crate) fn record_exhausted(&self, policy: FuelPolicy) {
        self.counters.decisions[policy as usize][2].fetch_add(1, Ordering::Relaxed);
    }

    pub(crate) fn record_settled(&self, policy: FuelPolicy, consumed: u64) {
        if consumed != 0 {
            self.counters.settled[self.shard][policy as usize]
                .fetch_add(consumed, Ordering::Relaxed);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Barrier;

    #[test]
    fn settlement_shards_use_cache_padded_alignment_without_shared_write_blocks() {
        let metrics = FuelMetrics::default();
        // Protect the selected portable padding mechanism. This is not a claim
        // that every target's actual cache line equals CachePadded's alignment.
        let alignment = std::mem::align_of::<CachePadded<[AtomicU64; 2]>>();
        let decision_start = &metrics.counters.decisions[0][0] as *const _ as usize;
        let decision_end = &metrics.counters.decisions[1][2] as *const _ as usize;
        let allocator = &*metrics.counters.next_writer as *const _ as usize / alignment;
        let mut previous = None;
        for settled in &metrics.counters.settled {
            let address = &settled[0] as *const _ as usize;
            let oneshot = &settled[1] as *const _ as usize;
            assert_eq!(address % alignment, 0);
            let block = address / alignment;
            assert_eq!(oneshot / alignment, block);
            assert!(!(decision_start / alignment..=decision_end / alignment).contains(&block));
            assert_ne!(block, allocator);
            if let Some(previous) = previous {
                assert!(block > previous);
            }
            previous = Some(block);
        }
    }

    #[test]
    fn concurrent_shards_and_collisions_retain_exact_totals_and_monotonic_snapshots() {
        const THREADS: usize = 4;
        const PHASES: usize = 4;
        const WRITES: usize = 100;
        let metrics = FuelMetrics::default();
        let retained = metrics.clone();
        let unrelated = FuelMetrics::default();
        let mut groups: [Vec<FuelMetrics>; THREADS] = std::array::from_fn(|_| Vec::new());
        let mut assigned = [0; SETTLEMENT_SHARDS];
        // Two writers per shard: exercise every shard and deliberate collisions.
        for index in 0..2 * SETTLEMENT_SHARDS {
            let writer = metrics.writer();
            assert!(writer.shard < SETTLEMENT_SHARDS);
            assigned[writer.shard] += 1;
            // Put the second traversal of each shard on a different OS thread.
            groups[(index + index / SETTLEMENT_SHARDS) % THREADS].push(writer);
        }
        assert_eq!(assigned, [2; SETTLEMENT_SHARDS]);
        let barrier = Barrier::new(THREADS + 1);
        let mut observed = Vec::new();
        let mut quiescent = Vec::new();
        std::thread::scope(|scope| {
            for writers in groups {
                let barrier = &barrier;
                scope.spawn(move || {
                    for _ in 0..PHASES {
                        barrier.wait();
                        for _ in 0..WRITES {
                            for writer in &writers {
                                writer.record_settled(FuelPolicy::Scheduled, 7);
                                writer.record_settled(FuelPolicy::Oneshot, 11);
                                writer.record_settled(FuelPolicy::Oneshot, 0);
                                writer.record_settled(FuelPolicy::Scheduled, 0);
                                writer.record_grant(FuelPolicy::Scheduled);
                            }
                        }
                        barrier.wait();
                    }
                });
            }
            for _ in 0..PHASES {
                barrier.wait();
                for _ in 0..128 {
                    observed.push(metrics.snapshot());
                }
                barrier.wait();
                // Writers cannot start another phase before our next barrier.
                let snapshot = metrics.snapshot();
                observed.push(snapshot);
                quiescent.push(snapshot);
            }
        });
        // Assert outside the barriers so a failed assertion cannot strand writers.
        for pair in observed.windows(2) {
            assert!(pair[1].scheduled_settled_units >= pair[0].scheduled_settled_units);
            assert!(pair[1].oneshot_settled_units >= pair[0].oneshot_settled_units);
            assert!(pair[1].scheduled.grant >= pair[0].scheduled.grant);
        }
        for (phase, snapshot) in quiescent.iter().enumerate() {
            let writes = ((phase + 1) * 2 * SETTLEMENT_SHARDS * WRITES) as u64;
            assert_eq!(snapshot.scheduled_settled_units, writes * 7);
            assert_eq!(snapshot.oneshot_settled_units, writes * 11);
            assert_eq!(snapshot.scheduled.grant, writes);
            assert_eq!(snapshot.scheduled.suspend, 0);
            assert_eq!(snapshot.scheduled.exhausted, 0);
            assert_eq!(snapshot.oneshot, FuelDecisionSnapshot::default());
        }
        let terminal = metrics.snapshot();
        drop(metrics);
        assert_eq!(retained.snapshot(), terminal);
        assert_eq!(unrelated.snapshot(), FuelSnapshot::default());
    }

    #[test]
    fn writer_clone_retains_assignment_and_counts_across_thread_moves() {
        let metrics = FuelMetrics::default();
        let writer = metrics.writer();
        let shard = writer.shard;
        let sequence = metrics.counters.next_writer.load(Ordering::Relaxed);
        let clone = writer.clone();
        assert_eq!(clone.shard, shard);
        assert_eq!(
            metrics.counters.next_writer.load(Ordering::Relaxed),
            sequence
        );
        assert!(Arc::ptr_eq(&writer.counters, &clone.counters));
        let writer = std::thread::spawn(move || {
            writer.record_settled(FuelPolicy::Scheduled, 17);
            writer
        })
        .join()
        .unwrap();
        assert_eq!(writer.shard, shard);
        std::thread::spawn(move || {
            writer.record_settled(FuelPolicy::Oneshot, 23);
        })
        .join()
        .unwrap();
        clone.record_settled(FuelPolicy::Scheduled, 5);
        assert_eq!(
            metrics.counters.settled[shard][0].load(Ordering::Relaxed),
            22
        );
        assert_eq!(
            metrics.counters.settled[shard][1].load(Ordering::Relaxed),
            23
        );
        drop(metrics);
        let snapshot = clone.snapshot();
        assert_eq!(snapshot.scheduled_settled_units, 22);
        assert_eq!(snapshot.oneshot_settled_units, 23);
    }

    #[test]
    fn writer_assignment_stays_bounded_across_setup_sequence_wrap() {
        let metrics = FuelMetrics::default();
        metrics
            .counters
            .next_writer
            .store(usize::MAX - 1, Ordering::Relaxed);
        let writers: Vec<_> = (0..4).map(|_| metrics.writer()).collect();
        for (index, writer) in writers.iter().enumerate() {
            assert_eq!(
                writer.shard,
                (usize::MAX - 1).wrapping_add(index) % SETTLEMENT_SHARDS
            );
            writer.record_settled(FuelPolicy::Scheduled, index as u64 + 1);
        }
        assert_eq!(metrics.counters.next_writer.load(Ordering::Relaxed), 2);
        assert_eq!(metrics.snapshot().scheduled_settled_units, 10);
        assert_eq!(metrics.snapshot().oneshot_settled_units, 0);
    }

    #[test]
    fn shard_aggregation_and_individual_atomics_keep_u64_wrapping_semantics() {
        let metrics = FuelMetrics::default();
        let first = metrics.writer();
        let second = metrics.writer();
        assert_ne!(first.shard, second.shard);
        first.record_settled(FuelPolicy::Scheduled, u64::MAX - 3);
        second.record_settled(FuelPolicy::Scheduled, 7);
        // Aggregate addition wraps although neither shard has wrapped yet.
        assert_eq!(metrics.snapshot().scheduled_settled_units, 3);
        first.record_settled(FuelPolicy::Scheduled, 10);
        // The first atomic now wraps too; the aggregate remains modulo 2^64.
        assert_eq!(metrics.snapshot().scheduled_settled_units, 13);
        second.record_settled(FuelPolicy::Oneshot, u64::MAX);
        second.record_settled(FuelPolicy::Oneshot, 1);
        assert_eq!(metrics.snapshot().oneshot_settled_units, 0);
        assert_eq!(metrics.snapshot().scheduled_settled_units, 13);
        assert_eq!(
            metrics.snapshot().scheduled,
            FuelDecisionSnapshot::default()
        );
        assert_eq!(metrics.snapshot().oneshot, FuelDecisionSnapshot::default());
    }
}
