use std::error::Error;
use std::fmt;
use std::future::Future;
use std::sync::{Arc, Condvar, Mutex};
use std::thread::JoinHandle;

use tokio::runtime::Handle;
use tokio::sync::Semaphore;
use tokio_util::task::TaskTracker;

use crate::fs_work::{RootId, State};
use crate::resources::{
    AdmissionError, AdmissionKind, CleanupActivePermit, CleanupObligation, HostLimits,
    HostResources,
};

#[derive(Clone)]
pub(crate) struct HostFsDriver {
    runtime: Handle,
    retained: TaskTracker,
}

impl HostFsDriver {
    fn new(runtime: Handle) -> Self {
        Self {
            runtime,
            retained: TaskTracker::new(),
        }
    }

    pub(crate) fn spawn<F>(&self, future: F)
    where
        F: Future<Output = ()> + Send + 'static,
    {
        self.retained.spawn_on(future, &self.runtime);
    }

    fn close(&self) {
        self.retained.close();
    }

    async fn wait(&self) {
        self.retained.wait().await;
    }
}

#[derive(Debug)]
pub struct HostFsStartError(String);

impl fmt::Display for HostFsStartError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl Error for HostFsStartError {}

struct RootSlot {
    generation: u64,
    state: Option<Arc<State>>,
}

struct RootTable {
    accepting: bool,
    shutdown: bool,
    slots: Vec<RootSlot>,
}

#[derive(Default)]
struct WakeState {
    epoch: u64,
}

pub(crate) struct ReaperInner {
    table: Mutex<RootTable>,
    wake: Mutex<WakeState>,
    wake_cv: Condvar,
    cleanup_active: Arc<Semaphore>,
}

impl ReaperInner {
    fn new(cleanup_roots: usize, cleanup_active: Arc<Semaphore>) -> Arc<Self> {
        Arc::new(Self {
            table: Mutex::new(RootTable {
                accepting: true,
                shutdown: false,
                slots: (0..cleanup_roots)
                    .map(|_| RootSlot {
                        generation: 0,
                        state: None,
                    })
                    .collect(),
            }),
            wake: Mutex::new(WakeState::default()),
            wake_cv: Condvar::new(),
            cleanup_active,
        })
    }

    pub(crate) fn register_root(
        self: &Arc<Self>,
        host: Arc<HostResources>,
        obligation: CleanupObligation,
    ) -> Result<Arc<State>, AdmissionError> {
        let mut table = self.table.lock().expect("filesystem root table poisoned");
        if !table.accepting {
            return Err(AdmissionError::new(AdmissionKind::Closed));
        }
        let (slot_index, slot) = table
            .slots
            .iter_mut()
            .enumerate()
            .find(|(_, slot)| slot.state.is_none())
            .ok_or_else(|| AdmissionError::new(AdmissionKind::CleanupObligation))?;
        slot.generation = slot
            .generation
            .checked_add(1)
            .ok_or_else(|| AdmissionError::new(AdmissionKind::Closed))?;
        let id = RootId {
            slot: slot_index,
            generation: slot.generation,
        };
        let state = State::new(id, host, Arc::downgrade(self), obligation);
        slot.state = Some(Arc::clone(&state));
        drop(table);
        self.notify();
        Ok(state)
    }

    pub(crate) fn notify(&self) {
        let mut wake = self.wake.lock().expect("filesystem reaper wake poisoned");
        wake.epoch = wake
            .epoch
            .checked_add(1)
            .expect("filesystem reaper wake epoch exhausted");
        self.wake_cv.notify_one();
    }

    fn close_admission(&self) {
        self.table
            .lock()
            .expect("filesystem root table poisoned")
            .accepting = false;
        self.notify();
    }

    fn request_shutdown(&self) {
        let mut table = self.table.lock().expect("filesystem root table poisoned");
        table.accepting = false;
        table.shutdown = true;
        drop(table);
        self.notify();
    }

    fn candidates_and_shutdown(&self) -> (Vec<Arc<State>>, bool) {
        let table = self.table.lock().expect("filesystem root table poisoned");
        (
            table
                .slots
                .iter()
                .filter_map(|slot| slot.state.as_ref().map(Arc::clone))
                .collect(),
            table.shutdown && table.slots.iter().all(|slot| slot.state.is_none()),
        )
    }

    fn remove(&self, id: RootId) {
        let removed = {
            let mut table = self.table.lock().expect("filesystem root table poisoned");
            let Some(slot) = table.slots.get_mut(id.slot) else {
                return;
            };
            if slot.generation != id.generation {
                return;
            }
            slot.state.take()
        };
        // A State can contain user cleanup payloads. Never drop the table's
        // reference while holding the root-table lock.
        drop(removed);
        self.notify();
    }

    fn run(self: Arc<Self>, runtime: tokio::runtime::Runtime) {
        loop {
            let observed_epoch = self
                .wake
                .lock()
                .expect("filesystem reaper wake poisoned")
                .epoch;
            let (candidates, shutdown) = self.candidates_and_shutdown();
            if shutdown {
                return;
            }

            let mut cleaned = false;
            for state in candidates {
                if !state.is_ready() {
                    continue;
                }
                let Ok(permit) = Arc::clone(&self.cleanup_active)
                    .try_acquire_owned()
                    .map(CleanupActivePermit::new)
                else {
                    continue;
                };
                let Some(cleanups) = state.claim_cleanup() else {
                    drop(permit);
                    continue;
                };
                state.run_cleanup(&runtime, cleanups);
                state.publish_completion();
                let id = state.id();
                drop(permit);
                self.remove(id);
                state.release_obligation();
                cleaned = true;
            }
            if cleaned {
                continue;
            }

            // Snapshot-before-scan plus epoch recheck makes notifications
            // durable across the scan-to-wait transition.
            let mut wake = self.wake.lock().expect("filesystem reaper wake poisoned");
            if wake.epoch == observed_epoch {
                wake = self
                    .wake_cv
                    .wait(wake)
                    .expect("filesystem reaper wake poisoned");
            }
            drop(wake);
        }
    }
}

/// Unique owner for the process-root filesystem driver and cleanup thread.
pub struct HostFsOwner {
    resources: Arc<HostResources>,
    driver: HostFsDriver,
    reaper: Arc<ReaperInner>,
    cleanup_thread: Option<JoinHandle<()>>,
}

impl HostFsOwner {
    pub fn start(
        runtime: Handle,
        limits: HostLimits,
    ) -> Result<(Self, Arc<HostResources>), HostFsStartError> {
        let driver = HostFsDriver::new(runtime);
        let cleanup_active = Arc::new(Semaphore::new(1));
        let reaper = ReaperInner::new(limits.cleanup_roots(), Arc::clone(&cleanup_active));
        let thread_reaper = Arc::clone(&reaper);
        let (ready_tx, ready_rx) = std::sync::mpsc::sync_channel(1);
        let cleanup_thread = std::thread::Builder::new()
            .name("ww-fs-reaper".to_owned())
            .spawn(move || {
                let runtime = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build();
                match runtime {
                    Ok(runtime) => {
                        let _ = ready_tx.send(Ok::<(), String>(()));
                        thread_reaper.run(runtime);
                    }
                    Err(error) => {
                        let _ = ready_tx.send(Err(error.to_string()));
                    }
                }
            })
            .map_err(|error| {
                HostFsStartError(format!(
                    "failed to start filesystem cleanup thread: {error}"
                ))
            })?;
        match ready_rx.recv() {
            Ok(Ok(())) => {}
            Ok(Err(error)) => {
                let _ = cleanup_thread.join();
                return Err(HostFsStartError(format!(
                    "failed to start filesystem cleanup runtime: {error}"
                )));
            }
            Err(error) => {
                let _ = cleanup_thread.join();
                return Err(HostFsStartError(format!(
                    "filesystem cleanup thread exited during startup: {error}"
                )));
            }
        }

        let resources = Arc::new(HostResources::new(
            limits,
            cleanup_active,
            driver.clone(),
            Arc::downgrade(&reaper),
        ));
        let owner = Self {
            resources: Arc::clone(&resources),
            driver,
            reaper,
            cleanup_thread: Some(cleanup_thread),
        };
        Ok((owner, resources))
    }

    pub fn close_admission(&self) {
        self.resources.close_admission();
        self.reaper.close_admission();
    }

    pub async fn shutdown(mut self) -> Result<(), HostFsStartError> {
        self.close_admission();
        self.driver.close();
        self.driver.wait().await;
        self.reaper.request_shutdown();
        let thread = self
            .cleanup_thread
            .take()
            .expect("filesystem cleanup thread already joined");
        tokio::task::spawn_blocking(move || thread.join())
            .await
            .map_err(|error| {
                HostFsStartError(format!("filesystem cleanup join task failed: {error}"))
            })?
            .map_err(|_| HostFsStartError("filesystem cleanup thread panicked".to_owned()))
    }
}

impl Drop for HostFsOwner {
    fn drop(&mut self) {
        self.close_admission();
        self.driver.close();
        self.reaper.request_shutdown();
        // Explicit shutdown owns the join. Drop is a fail-safe signal only;
        // it cannot synchronously join from an arbitrary runtime/thread.
        let _ = self.cleanup_thread.take();
    }
}
