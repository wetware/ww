//! Epoch types and the epoch validity guard.

use call_guard::{stale_epoch_error, CallGuard};
use capnp::Error;
use tokio::sync::watch;

/// Epoch value used by the membrane (matches capnp struct Epoch).
///
/// An epoch anchors a point-in-time snapshot of a namespace's content root.
/// The host allocates `seq` locally. Every process starts at epoch zero.
#[derive(Clone, Debug)]
pub struct Epoch {
    pub seq: u64,
    pub head: Vec<u8>,
    /// The Host-composed root. `None` means that the epoch is authoritative
    /// but its replacement generation is not ready to start.
    pub root: Option<String>,
}

impl Epoch {
    /// Construct the epoch-zero value used before a host adopts Stem state.
    pub fn zero() -> Self {
        Self {
            seq: 0,
            head: Vec::new(),
            root: None,
        }
    }
}

/// Guard that checks whether the epoch under which a capability was issued is
/// still current. Shared by all session-scoped capability servers so that
/// every RPC hard-fails once the epoch advances.
#[derive(Clone)]
pub struct EpochGuard {
    pub issued_seq: u64,
    pub receiver: watch::Receiver<Epoch>,
}

impl EpochGuard {
    /// Construct a guard whose epoch cannot advance.
    ///
    /// Embedders and tests can use this guard when no live epoch sender exists.
    /// Production `ww` hosts always use the ordinary watch channel seeded at
    /// epoch zero; without Stem, that channel does not advance.
    pub fn fixed(epoch: Epoch) -> Self {
        let issued_seq = epoch.seq;
        let (_sender, receiver) = watch::channel(epoch);
        Self {
            issued_seq,
            receiver,
        }
    }

    pub fn check(&self) -> Result<(), Error> {
        CallGuard::check(self)
    }
}

impl CallGuard for EpochGuard {
    fn check(&self) -> Result<(), Error> {
        let current = self.receiver.borrow();
        if current.seq != self.issued_seq {
            return Err(stale_epoch_error("session epoch no longer current"));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn epoch(seq: u64, head: &[u8]) -> Epoch {
        Epoch {
            seq,
            head: head.to_vec(),
            root: None,
        }
    }

    #[test]
    fn epoch_guard_ok_when_seq_matches() {
        let (_tx, rx) = watch::channel(epoch(1, b"head1"));
        let guard = EpochGuard {
            issued_seq: 1,
            receiver: rx,
        };
        assert!(guard.check().is_ok());
    }

    #[test]
    fn epoch_guard_fails_when_seq_differs() {
        let (tx, rx) = watch::channel(epoch(1, b"head1"));
        let guard = EpochGuard {
            issued_seq: 1,
            receiver: rx,
        };
        assert!(guard.check().is_ok());
        tx.send(epoch(2, b"head2")).unwrap();
        let res = guard.check();
        assert!(res.is_err());
        assert_eq!(
            call_guard::call_failure_code(&res.unwrap_err()),
            Some(call_guard::CallFailureCode::StaleEpoch)
        );
    }

    #[test]
    fn fixed_epoch_zero_guard_remains_valid() {
        let guard = EpochGuard::fixed(Epoch::zero());
        assert_eq!(guard.issued_seq, 0);
        assert!(guard.check().is_ok());
        assert!(guard.check().is_ok());
    }
}
