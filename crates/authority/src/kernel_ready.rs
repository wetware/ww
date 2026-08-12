//! Host-local PID0 generation readiness state.
//!
//! PID0's process-local graft binds the generation, the private Wasm host
//! import commits it, and host readiness consumers read the result. The gate
//! is deliberately not represented by a Cap'n Proto capability and the import
//! is never installed for ordinary child cells.

use std::sync::Mutex;

use tokio::sync::watch;

use crate::Epoch;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum KernelReadyError {
    NotBound,
    StaleGeneration,
}

/// Authoritative readiness for trusted PID0's current graft generation.
pub struct KernelReadyGate {
    epoch_rx: watch::Receiver<Epoch>,
    state: Mutex<KernelReadyState>,
}

#[derive(Default)]
struct KernelReadyState {
    bound_seq: Option<u64>,
    committed_seq: Option<u64>,
}

impl KernelReadyGate {
    pub fn new(epoch_rx: watch::Receiver<Epoch>) -> Self {
        Self {
            epoch_rx,
            state: Mutex::new(KernelReadyState::default()),
        }
    }

    /// Bind readiness to the authoritative generation used for one local PID0 graft.
    pub fn bind_generation(&self, issued_seq: u64) {
        let mut state = self.state.lock().expect("kernel readiness gate poisoned");
        state.bound_seq = Some(issued_seq);
        state.committed_seq = None;
    }

    /// Close readiness when the trusted PID0 execution stops.
    pub fn clear(&self) {
        *self.state.lock().expect("kernel readiness gate poisoned") = KernelReadyState::default();
    }

    /// Whether trusted PID0 committed the currently authoritative generation.
    ///
    /// Holding the epoch borrow through the state read prevents an epoch
    /// publisher from interleaving between the two observations.
    pub fn is_ready(&self) -> bool {
        let current = self.epoch_rx.borrow();
        self.state
            .lock()
            .map(|state| state.committed_seq == Some(current.seq))
            .unwrap_or(false)
    }

    /// Commit the locally bound generation if it is still authoritative.
    ///
    /// The watch borrow remains live through the comparison and state commit,
    /// preventing an epoch publisher from interleaving between those actions.
    pub fn kernel_ready(&self) -> Result<(), KernelReadyError> {
        self.kernel_ready_with_post_commit(|| {})
    }

    fn kernel_ready_with_post_commit(
        &self,
        post_commit: impl FnOnce(),
    ) -> Result<(), KernelReadyError> {
        let current = self.epoch_rx.borrow();
        let mut state = self.state.lock().expect("kernel readiness gate poisoned");
        let bound_seq = state.bound_seq.ok_or(KernelReadyError::NotBound)?;
        if bound_seq != current.seq {
            return Err(KernelReadyError::StaleGeneration);
        }
        state.committed_seq = Some(bound_seq);
        drop(state);
        post_commit();
        drop(current);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Provenance;

    fn epoch(seq: u64) -> Epoch {
        Epoch {
            seq,
            head: Vec::new(),
            root: None,
            provenance: Provenance::Block(0),
        }
    }

    #[test]
    fn commits_only_the_bound_current_generation() {
        let (_tx, rx) = watch::channel(epoch(7));
        let gate = KernelReadyGate::new(rx);
        assert!(!gate.is_ready());
        assert_eq!(gate.kernel_ready(), Err(KernelReadyError::NotBound));
        gate.bind_generation(7);
        assert!(!gate.is_ready());
        assert_eq!(gate.kernel_ready(), Ok(()));
        assert!(gate.is_ready());
    }

    #[test]
    fn stale_generation_fails_without_committing_and_rebind_succeeds() {
        let (tx, rx) = watch::channel(epoch(1));
        let gate = KernelReadyGate::new(rx);
        gate.bind_generation(1);
        tx.send_replace(epoch(2));
        assert_eq!(gate.kernel_ready(), Err(KernelReadyError::StaleGeneration));
        assert!(!gate.is_ready());
        gate.bind_generation(2);
        assert_eq!(gate.kernel_ready(), Ok(()));
        assert!(gate.is_ready());
    }

    #[test]
    fn binding_a_generation_closes_a_previous_commit_until_pid0_recommits() {
        let (_tx, rx) = watch::channel(epoch(1));
        let gate = KernelReadyGate::new(rx);
        gate.bind_generation(1);
        gate.kernel_ready().unwrap();
        assert!(gate.is_ready());

        gate.bind_generation(1);
        assert!(!gate.is_ready());
        gate.kernel_ready().unwrap();
        assert!(gate.is_ready());
    }

    #[test]
    fn generation_zero_is_fail_closed_until_pid0_commits() {
        let (_tx, rx) = watch::channel(epoch(0));
        let gate = KernelReadyGate::new(rx);
        assert!(!gate.is_ready());
        gate.bind_generation(0);
        assert!(!gate.is_ready());
        gate.kernel_ready().unwrap();
        assert!(gate.is_ready());
    }

    #[test]
    fn pid0_exit_clears_a_committed_generation() {
        let (_tx, rx) = watch::channel(epoch(1));
        let gate = KernelReadyGate::new(rx);
        gate.bind_generation(1);
        gate.kernel_ready().unwrap();
        assert!(gate.is_ready());

        gate.clear();
        assert!(!gate.is_ready());
        assert_eq!(gate.kernel_ready(), Err(KernelReadyError::NotBound));
    }

    #[test]
    fn kernel_ready_holds_authoritative_borrow_through_commit() {
        let (tx, rx) = watch::channel(epoch(1));
        let gate = std::sync::Arc::new(KernelReadyGate::new(rx));
        gate.bind_generation(1);

        let (stored_tx, stored_rx) = std::sync::mpsc::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let ready_gate = gate.clone();
        let ready = std::thread::spawn(move || {
            ready_gate
                .kernel_ready_with_post_commit(|| {
                    stored_tx.send(()).unwrap();
                    release_rx.recv().unwrap();
                })
                .unwrap();
        });
        stored_rx
            .recv_timeout(std::time::Duration::from_secs(1))
            .expect("kernel_ready reached its post-commit hook");
        assert!(
            gate.is_ready(),
            "kernel_ready must commit before the test hook"
        );

        let (publisher_started_tx, publisher_started_rx) = std::sync::mpsc::channel();
        let (published_tx, published_rx) = std::sync::mpsc::channel();
        let publisher = std::thread::spawn(move || {
            publisher_started_tx.send(()).unwrap();
            tx.send_replace(epoch(2));
            published_tx.send(()).unwrap();
        });
        publisher_started_rx
            .recv_timeout(std::time::Duration::from_secs(1))
            .expect("epoch publisher started");
        assert!(published_rx
            .recv_timeout(std::time::Duration::from_millis(25))
            .is_err(),
            "epoch publication must remain blocked after the readiness commit while kernel_ready holds the borrow"
        );

        release_tx.send(()).unwrap();
        ready.join().unwrap();
        published_rx
            .recv_timeout(std::time::Duration::from_secs(1))
            .unwrap();
        publisher.join().unwrap();
        assert_eq!(gate.epoch_rx.borrow().seq, 2);
    }
}
