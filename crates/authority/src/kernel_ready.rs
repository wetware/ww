//! Host-local PID0 generation readiness state.
//!
//! This state is shared only between PID0's process-local graft server and the
//! private Wasm host import. It is deliberately not represented by a Cap'n
//! Proto capability and is never installed for ordinary child cells.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use tokio::sync::watch;

use crate::Epoch;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum KernelReadyError {
    NotBound,
    StaleGeneration,
}

/// The generation bound to trusted PID0's most recent process-local graft.
pub struct KernelReadyGate {
    epoch_rx: watch::Receiver<Epoch>,
    activated_seq: Arc<AtomicU64>,
    bound_seq: Mutex<Option<u64>>,
}

impl KernelReadyGate {
    pub fn new(epoch_rx: watch::Receiver<Epoch>, activated_seq: Arc<AtomicU64>) -> Self {
        Self {
            epoch_rx,
            activated_seq,
            bound_seq: Mutex::new(None),
        }
    }

    /// Bind readiness to the authoritative generation used for one local PID0 graft.
    pub fn bind_generation(&self, issued_seq: u64) {
        *self
            .bound_seq
            .lock()
            .expect("kernel readiness gate poisoned") = Some(issued_seq);
    }

    /// Commit the locally bound generation if it is still authoritative.
    ///
    /// The watch borrow remains live through the comparison and Release store,
    /// preventing an epoch publisher from interleaving between those actions.
    pub fn kernel_ready(&self) -> Result<(), KernelReadyError> {
        let current = self.epoch_rx.borrow();
        let bound_seq = self
            .bound_seq
            .lock()
            .expect("kernel readiness gate poisoned")
            .ok_or(KernelReadyError::NotBound)?;
        if bound_seq != current.seq {
            return Err(KernelReadyError::StaleGeneration);
        }
        self.activated_seq.store(bound_seq, Ordering::Release);
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
            provenance: Provenance::Block(0),
        }
    }

    #[test]
    fn commits_only_the_bound_current_generation() {
        let (_tx, rx) = watch::channel(epoch(7));
        let activated = Arc::new(AtomicU64::new(6));
        let gate = KernelReadyGate::new(rx, activated.clone());
        assert_eq!(gate.kernel_ready(), Err(KernelReadyError::NotBound));
        gate.bind_generation(7);
        assert_eq!(gate.kernel_ready(), Ok(()));
        assert_eq!(activated.load(Ordering::Acquire), 7);
    }

    #[test]
    fn stale_generation_fails_without_committing_and_rebind_succeeds() {
        let (tx, rx) = watch::channel(epoch(1));
        let activated = Arc::new(AtomicU64::new(0));
        let gate = KernelReadyGate::new(rx, activated.clone());
        gate.bind_generation(1);
        tx.send_replace(epoch(2));
        assert_eq!(gate.kernel_ready(), Err(KernelReadyError::StaleGeneration));
        assert_eq!(activated.load(Ordering::Acquire), 0);
        gate.bind_generation(2);
        assert_eq!(gate.kernel_ready(), Ok(()));
        assert_eq!(activated.load(Ordering::Acquire), 2);
    }

    #[test]
    fn epoch_publish_waits_for_the_authoritative_borrow() {
        let (tx, rx) = watch::channel(epoch(1));
        let activated = Arc::new(AtomicU64::new(0));
        let _gate = KernelReadyGate::new(rx.clone(), activated);

        let current = rx.borrow();
        let (published_tx, published_rx) = std::sync::mpsc::channel();
        let publisher = std::thread::spawn(move || {
            tx.send_replace(epoch(2));
            published_tx.send(()).unwrap();
        });
        assert!(published_rx
            .recv_timeout(std::time::Duration::from_millis(25))
            .is_err());
        drop(current);
        published_rx
            .recv_timeout(std::time::Duration::from_secs(1))
            .unwrap();
        publisher.join().unwrap();
    }
}
