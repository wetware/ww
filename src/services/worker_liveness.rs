//! Per-worker progress, independent of Cell execution and epoch publication.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use tokio::sync::watch;

pub const WORKER_HEARTBEAT_INTERVAL: Duration = Duration::from_millis(250);
pub const WORKER_STALL_THRESHOLD: Duration = Duration::from_secs(5);

/// Lifecycle is separate from responsiveness. Shutdown does not imply progress.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WorkerState {
    Starting,
    Running,
    ShuttingDown,
    Stopped,
}

/// One atomic progress/lifecycle sample plus an independent shutdown read.
/// Age uses a monotonic clock. `None` means no heartbeat has run yet.
/// A stall is a threshold violation, not proof of permanent deadlock.
#[derive(Clone, Copy, Debug)]
pub struct WorkerLivenessSnapshot {
    pub worker_id: usize,
    pub state: WorkerState,
    pub progress_age: Option<Duration>,
    pub stalled: bool,
}

// Low two bits encode lifecycle. Upper bits encode elapsed microseconds + 1;
// zero means no confirmed progress. One word prevents torn time/state samples.
const STARTING: u64 = 0;
const RUNNING: u64 = 1;
const SHUTTING_DOWN: u64 = 2;
const STOPPED: u64 = 3;
const PHASE_MASK: u64 = 3;

#[derive(Debug)]
struct WorkerProgress {
    word: AtomicU64,
    origin: Instant,
}

impl WorkerProgress {
    fn publish(&self) {
        self.publish_at(self.origin.elapsed());
    }

    fn publish_at(&self, elapsed: Duration) {
        let timestamp = (elapsed.as_micros().min(u128::from((u64::MAX >> 2) - 1)) as u64 + 1) << 2;
        let phase = self.word.load(Ordering::Relaxed) & PHASE_MASK;
        // Only the owning worker writes this word. Its heartbeat and lifecycle
        // transitions execute on the same OS thread, with no await in between.
        let phase = if phase == STARTING { RUNNING } else { phase };
        self.word.store(timestamp | phase, Ordering::Relaxed);
    }

    fn set_phase(&self, phase: u64) {
        let word = self.word.load(Ordering::Relaxed);
        self.word
            .store((word & !PHASE_MASK) | phase, Ordering::Relaxed);
    }

    fn sample(&self, worker_id: usize, shutdown: bool) -> WorkerLivenessSnapshot {
        let word = self.word.load(Ordering::Relaxed);
        // Read time after publication: a concurrent heartbeat cannot look older
        // because the observer took its timestamp before loading the word.
        Self::decode(word, self.origin.elapsed(), worker_id, shutdown)
    }

    fn decode(
        word: u64,
        now: Duration,
        worker_id: usize,
        shutdown: bool,
    ) -> WorkerLivenessSnapshot {
        let phase = word & PHASE_MASK;
        let state = match phase {
            STOPPED => WorkerState::Stopped,
            _ if shutdown || phase == SHUTTING_DOWN => WorkerState::ShuttingDown,
            RUNNING => WorkerState::Running,
            _ => WorkerState::Starting,
        };
        let progress_age = (word >> 2)
            .checked_sub(1)
            .map(|micros| now.saturating_sub(Duration::from_micros(micros)));
        let stalled = state != WorkerState::Stopped
            && progress_age.is_some_and(|age| age >= WORKER_STALL_THRESHOLD);
        WorkerLivenessSnapshot {
            worker_id,
            state,
            progress_age,
            stalled,
        }
    }
}

#[derive(Debug)]
pub(super) struct PoolLiveness {
    workers: Vec<Arc<WorkerProgress>>,
    shutdown_requested: AtomicBool,
    shutdown: watch::Receiver<()>,
}

impl PoolLiveness {
    pub(super) fn request_shutdown(&self) {
        self.shutdown_requested.store(true, Ordering::Relaxed);
    }

    fn snapshot(&self) -> Vec<WorkerLivenessSnapshot> {
        let shutdown = self.shutdown_requested.load(Ordering::Relaxed)
            || self.shutdown.has_changed().unwrap_or(true);
        self.workers
            .iter()
            .enumerate()
            .map(|(id, worker)| worker.sample(id, shutdown))
            .collect()
    }
}

/// Cold registration lock only. Workers never acquire this lock; readers clone
/// the current pool then release it before sampling. Replacing a stopped pool
/// discards its records instead of accumulating per-generation history.
#[derive(Debug, Default)]
pub(super) struct WorkerRegistry(Mutex<Option<Arc<PoolLiveness>>>);

impl WorkerRegistry {
    pub(super) fn register(
        &self,
        count: usize,
        shutdown: watch::Receiver<()>,
    ) -> anyhow::Result<(Arc<PoolLiveness>, Vec<WorkerGuard>)> {
        let mut current = self.0.lock().unwrap_or_else(|error| error.into_inner());
        anyhow::ensure!(
            current
                .as_ref()
                .is_none_or(|pool| pool.workers.iter().all(|worker| worker
                    .word
                    .load(Ordering::Acquire)
                    & PHASE_MASK
                    == STOPPED)),
            "executor metrics collector already has an active pool"
        );
        let origin = Instant::now();
        let workers: Vec<_> = (0..count)
            .map(|_| {
                Arc::new(WorkerProgress {
                    word: AtomicU64::new(STARTING),
                    origin,
                })
            })
            .collect();
        let guards = workers
            .iter()
            .map(|worker| WorkerGuard(Arc::clone(worker)))
            .collect();
        let pool = Arc::new(PoolLiveness {
            workers,
            shutdown_requested: AtomicBool::new(false),
            shutdown,
        });
        *current = Some(Arc::clone(&pool));
        Ok((pool, guards))
    }

    pub(super) fn snapshot(&self) -> Vec<WorkerLivenessSnapshot> {
        let pool = self
            .0
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .clone();
        pool.map_or_else(Vec::new, |pool| pool.snapshot())
    }
}

/// Must outlive the worker runtime, LocalSet and all their destructors.
pub(super) struct WorkerGuard(Arc<WorkerProgress>);

impl WorkerGuard {
    pub(super) fn start_heartbeat(&self) {
        // The root future is running: establish progress before admitting a Cell
        // whose very first poll could block the worker.
        self.0.publish();
        let progress = Arc::clone(&self.0);
        tokio::task::spawn_local(async move {
            let mut interval = tokio::time::interval(WORKER_HEARTBEAT_INTERVAL);
            interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            interval.tick().await; // initial progress was published by the root
            loop {
                interval.tick().await;
                progress.publish();
            }
        });
    }

    pub(super) fn draining(&self) {
        self.0.set_phase(SHUTTING_DOWN);
    }
}

impl Drop for WorkerGuard {
    fn drop(&mut self) {
        let word = self.0.word.load(Ordering::Relaxed);
        // Registration's Acquire observes completion of the old worker's cleanup.
        self.0
            .word
            .store((word & !PHASE_MASK) | STOPPED, Ordering::Release);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::services::{test_filesystem_resources, ExecutorMetrics, ExecutorPool, SpawnRequest};

    fn wait_until(mut condition: impl FnMut() -> bool) {
        let deadline = Instant::now() + Duration::from_secs(10);
        while !condition() {
            assert!(Instant::now() < deadline, "worker observation deadline");
            std::thread::yield_now();
        }
    }

    #[test]
    fn threshold_recovery_and_lifecycle_use_confirmed_progress() {
        let progress = WorkerProgress {
            word: AtomicU64::new(STARTING),
            origin: Instant::now(),
        };
        let sample = |now, shutdown| {
            WorkerProgress::decode(progress.word.load(Ordering::Relaxed), now, 7, shutdown)
        };
        let starting = sample(Duration::from_secs(100), false);
        assert_eq!(starting.state, WorkerState::Starting);
        assert_eq!(starting.progress_age, None);
        assert!(!starting.stalled);
        progress.publish_at(Duration::from_secs(1));
        assert!(!sample(Duration::from_secs(6) - Duration::from_micros(1), false).stalled);
        let stalled = sample(Duration::from_secs(6), false);
        assert_eq!(stalled.worker_id, 7);
        assert_eq!(stalled.progress_age, Some(WORKER_STALL_THRESHOLD));
        assert!(stalled.stalled);
        progress.publish_at(Duration::from_secs(6));
        assert!(!sample(Duration::from_secs(6), false).stalled);
        // External shutdown does not invent progress or require acknowledgement.
        let stopping = sample(Duration::from_secs(12), true);
        assert_eq!(stopping.state, WorkerState::ShuttingDown);
        assert_eq!(stopping.progress_age, Some(Duration::from_secs(6)));
        assert!(stopping.stalled);
        progress.set_phase(SHUTTING_DOWN);
        progress.publish_at(Duration::from_secs(13));
        assert_eq!(
            sample(Duration::from_secs(13), false).state,
            WorkerState::ShuttingDown
        );
        progress.set_phase(STOPPED);
        let stopped = sample(Duration::from_secs(20), true);
        assert_eq!(stopped.state, WorkerState::Stopped);
        assert_eq!(stopped.progress_age, Some(Duration::from_secs(7)));
        assert!(!stopped.stalled);
    }

    #[test]
    fn root_publishes_before_the_heartbeat_task_can_run() {
        let registry = WorkerRegistry::default();
        let (_shutdown, rx) = watch::channel(());
        let (_pool, mut guards) = registry.register(1, rx).unwrap();
        let guard = guards.pop().unwrap();
        let (entered_tx, entered_rx) = std::sync::mpsc::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel::<()>();
        let thread = std::thread::spawn(move || {
            let _guard = guard;
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            let local = tokio::task::LocalSet::new();
            runtime.block_on(local.run_until(async {
                _guard.start_heartbeat();
                entered_tx.send(()).unwrap();
                // No yield: the spawned heartbeat task cannot have run yet.
                let _ = release_rx.recv_timeout(Duration::from_secs(10));
            }));
        });
        entered_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        let sample = registry.snapshot()[0];
        assert_eq!(sample.state, WorkerState::Running);
        assert!(sample.progress_age.is_some());
        drop(release_tx);
        thread.join().unwrap();
        assert_eq!(registry.snapshot()[0].state, WorkerState::Stopped);
    }

    #[test]
    fn registry_rejects_overlap_replaces_stopped_identity_and_bounds_records() {
        let registry = WorkerRegistry::default();
        let (_shutdown, rx) = watch::channel(());
        for count in [2, 1, 4, 1] {
            let (_pool, guards) = registry.register(count, rx.clone()).unwrap();
            assert!(registry.register(count, rx.clone()).is_err());
            assert_eq!(registry.snapshot().len(), count);
            for (id, sample) in registry.snapshot().iter().enumerate() {
                assert_eq!(sample.worker_id, id);
                assert_eq!(sample.state, WorkerState::Starting);
                assert_eq!(sample.progress_age, None);
            }
            // Includes never-started workers after a partial startup failure.
            drop(guards);
            assert!(registry
                .snapshot()
                .iter()
                .all(|s| s.state == WorkerState::Stopped));
        }
    }

    #[test]
    fn idle_progress_shutdown_restart_and_concurrent_readers() {
        let metrics = ExecutorMetrics::default();
        let (shutdown, rx) = watch::channel(());
        let pool =
            ExecutorPool::try_new_with_metrics(2, rx, test_filesystem_resources(), metrics.clone())
                .unwrap();
        wait_until(|| {
            metrics
                .worker_liveness()
                .iter()
                .all(|s| s.state == WorkerState::Running)
        });
        std::thread::scope(|scope| {
            for _ in 0..4 {
                let metrics = &metrics;
                scope.spawn(move || {
                    for _ in 0..1_000 {
                        let samples = metrics.worker_liveness();
                        assert_eq!(samples.len(), 2);
                        assert!(samples
                            .iter()
                            .all(|s| s.progress_age.is_some() && !s.stalled));
                    }
                });
            }
        });
        drop(shutdown);
        drop(pool);
        assert!(metrics
            .worker_liveness()
            .iter()
            .all(|s| s.state == WorkerState::Stopped));
        let (_shutdown, rx) = watch::channel(());
        let pool =
            ExecutorPool::try_new_with_metrics(1, rx, test_filesystem_resources(), metrics.clone())
                .unwrap();
        wait_until(|| metrics.worker_liveness()[0].state == WorkerState::Running);
        assert_eq!(metrics.worker_liveness().len(), 1);
        drop(pool);
    }

    #[test]
    fn blocked_worker_zero_does_not_block_observation_or_other_workers() {
        let metrics = ExecutorMetrics::default();
        let (shutdown, rx) = watch::channel(());
        let pool =
            ExecutorPool::try_new_with_metrics(2, rx, test_filesystem_resources(), metrics.clone())
                .unwrap();
        wait_until(|| {
            metrics
                .worker_liveness()
                .iter()
                .all(|s| s.state == WorkerState::Running)
        });
        let (entered_tx, entered_rx) = std::sync::mpsc::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel::<()>();
        // Drop releases the worker even if an assertion unwinds before cleanup.
        pool.spawn(SpawnRequest {
            name: "blocked-worker".into(),
            result_tx: None,
            factory: Box::new(move |_| {
                Box::pin(async move {
                    entered_tx.send(()).unwrap();
                    let _ = release_rx.recv_timeout(Duration::from_secs(15));
                })
            }),
        })
        .unwrap_or_else(|_| panic!("admit blocker"));
        entered_rx.recv_timeout(Duration::from_secs(10)).unwrap();
        let registry = metrics.inner.workers.0.lock().unwrap().clone().unwrap();
        let before = registry.workers[0].word.load(Ordering::Relaxed);
        let other_before = registry.workers[1].word.load(Ordering::Relaxed);
        let epoch = pool.runtime_engine().subscribe();
        let epoch_before = epoch.current_epoch();
        wait_until(|| registry.workers[1].word.load(Ordering::Relaxed) != other_before);
        assert_eq!(registry.workers[0].word.load(Ordering::Relaxed), before);
        assert_eq!(
            epoch.current_epoch(),
            epoch_before,
            "worker 0 still owns the epoch publisher"
        );
        // Controlled observation clock proves the threshold without spending
        // five seconds blocked. HTTP integration covers the production clock.
        let then = Duration::from_micros((before >> 2) - 1) + WORKER_STALL_THRESHOLD;
        assert!(WorkerProgress::decode(before, then, 0, false).stalled);
        drop(shutdown);
        let stopping = metrics.worker_liveness()[0];
        assert_eq!(stopping.state, WorkerState::ShuttingDown);
        assert!(stopping.progress_age.is_some());
        drop(release_tx);
        drop(pool);
        assert!(metrics
            .worker_liveness()
            .iter()
            .all(|s| s.state == WorkerState::Stopped));
    }

    #[test]
    #[ignore = "opt-in heartbeat cadence measurement; no timing assertions"]
    fn measure_worker_heartbeat_cadence() {
        let (_shutdown, rx) = watch::channel(());
        let pool = ExecutorPool::new(4, rx, test_filesystem_resources());
        let metrics = pool.metrics();
        wait_until(|| {
            metrics
                .worker_liveness()
                .iter()
                .all(|s| s.state == WorkerState::Running)
        });
        let registry = metrics.inner.workers.0.lock().unwrap().clone().unwrap();
        let mut previous: Vec<_> = registry
            .workers
            .iter()
            .map(|w| w.word.load(Ordering::Relaxed))
            .collect();
        let mut counts = [0_u64; 4];
        let start = Instant::now();
        while start.elapsed() < Duration::from_secs(3) {
            for (id, worker) in registry.workers.iter().enumerate() {
                let word = worker.word.load(Ordering::Relaxed);
                if word != previous[id] {
                    counts[id] += 1;
                    previous[id] = word;
                }
            }
            std::thread::sleep(Duration::from_millis(1));
        }
        println!(
            "worker heartbeat publications over {:.3}s: {counts:?}",
            start.elapsed().as_secs_f64()
        );
        drop(pool);
    }

    #[test]
    #[ignore = "opt-in publication microbenchmark; no timing assertions"]
    fn measure_worker_progress_publication() {
        let progress = WorkerProgress {
            word: AtomicU64::new(STARTING),
            origin: Instant::now(),
        };
        let iterations = 1_000_000;
        let start = Instant::now();
        for _ in 0..iterations {
            progress.publish();
            std::hint::black_box(progress.word.load(Ordering::Relaxed));
        }
        println!(
            "worker progress: {:.2} ns/publication; {} bytes/record (excluding Arc allocation)",
            start.elapsed().as_nanos() as f64 / iterations as f64,
            std::mem::size_of::<WorkerProgress>()
        );
    }
}
