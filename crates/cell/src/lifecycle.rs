//! Process-wide observations of completed Wasmtime lifecycle operations.

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

/// Finite histogram bounds in seconds; the final bucket is positive infinity.
pub const DURATION_BUCKET_SECONDS: [f64; 9] = [0.0001, 0.001, 0.01, 0.1, 0.5, 1.0, 2.0, 5.0, 10.0];
const DURATION_BUCKET_NANOS: [u64; 9] = [
    100_000,
    1_000_000,
    10_000_000,
    100_000_000,
    500_000_000,
    1_000_000_000,
    2_000_000_000,
    5_000_000_000,
    10_000_000_000,
];

/// Returned operation outcomes. Dropping a pending future is not an outcome.
pub(crate) enum Outcome {
    Success,
    Error,
    Timeout,
}

#[derive(Default)]
pub(crate) struct LifecycleMetrics {
    outcomes: [AtomicU64; 3],
    // Store disjoint buckets: recording touches one bucket, never all buckets.
    buckets: [AtomicU64; 10],
    sum_nanos: AtomicU64,
}

/// Independently loaded totals, not a transaction across counters and duration.
/// Concurrent scrapes can observe partial publication. Totals wrap modulo 2^64.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct LifecycleSnapshot {
    pub success: u64,
    pub error: u64,
    pub timeout: u64,
    pub duration: LifecycleDurationSnapshot,
}

/// Cumulative buckets and their sum. Count equals the positive-infinity bucket.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct LifecycleDurationSnapshot {
    pub buckets: [u64; 10],
    pub sum_nanos: u64,
    pub count: u64,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct WasmtimeLifecycleSnapshot {
    pub compilation: LifecycleSnapshot,
    pub instantiation: LifecycleSnapshot,
}

impl LifecycleMetrics {
    /// Bounded, allocation-free recording with three relaxed atomic additions.
    pub(crate) fn record(&self, outcome: Outcome, elapsed: Duration) {
        let nanos = elapsed.as_nanos().min(u64::MAX as u128) as u64;
        let bucket = DURATION_BUCKET_NANOS.partition_point(|bound| *bound < nanos);
        self.buckets[bucket].fetch_add(1, Ordering::Relaxed);
        self.sum_nanos.fetch_add(nanos, Ordering::Relaxed);
        self.outcomes[outcome as usize].fetch_add(1, Ordering::Relaxed);
    }

    pub(crate) fn snapshot(&self) -> LifecycleSnapshot {
        let mut count = 0u64;
        let buckets = std::array::from_fn(|index| {
            count = count.wrapping_add(self.buckets[index].load(Ordering::Relaxed));
            count
        });
        LifecycleSnapshot {
            success: self.outcomes[0].load(Ordering::Relaxed),
            error: self.outcomes[1].load(Ordering::Relaxed),
            timeout: self.outcomes[2].load(Ordering::Relaxed),
            duration: LifecycleDurationSnapshot {
                buckets,
                sum_nanos: self.sum_nanos.load(Ordering::Relaxed),
                count,
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn duration_edges_include_failures_and_overflow_bucket() {
        let metrics = LifecycleMetrics::default();
        metrics.record(Outcome::Success, Duration::ZERO);
        for bound in DURATION_BUCKET_NANOS {
            metrics.record(Outcome::Error, Duration::from_nanos(bound));
            metrics.record(Outcome::Error, Duration::from_nanos(bound + 1));
        }
        metrics.record(Outcome::Timeout, Duration::from_secs(11));
        let snapshot = metrics.snapshot();
        assert_eq!(
            (snapshot.success, snapshot.error, snapshot.timeout),
            (1, 18, 1)
        );
        assert_eq!(
            snapshot.duration.buckets,
            [2, 4, 6, 8, 10, 12, 14, 16, 18, 20]
        );
        assert_eq!(snapshot.duration.count, 20);
        assert_eq!(
            snapshot.duration.sum_nanos,
            DURATION_BUCKET_NANOS.iter().sum::<u64>() * 2 + 9 + 11_000_000_000
        );
    }

    #[test]
    fn concurrent_recording_and_scraping_retain_totals() {
        let metrics = LifecycleMetrics::default();
        std::thread::scope(|scope| {
            for _ in 0..4 {
                let metrics = &metrics;
                scope.spawn(move || {
                    for _ in 0..1000 {
                        metrics.record(Outcome::Success, Duration::from_millis(1));
                        metrics.record(Outcome::Error, Duration::from_millis(2));
                    }
                });
            }
            for _ in 0..100 {
                let duration = metrics.snapshot().duration;
                assert!(duration.buckets.windows(2).all(|pair| pair[0] <= pair[1]));
                assert_eq!(duration.buckets[9], duration.count);
            }
        });
        let snapshot = metrics.snapshot();
        assert_eq!(
            (snapshot.success, snapshot.error, snapshot.timeout),
            (4000, 4000, 0)
        );
        assert_eq!(snapshot.duration.count, 8000);
        assert_eq!(snapshot.duration.sum_nanos, 12_000_000_000);
    }
}
