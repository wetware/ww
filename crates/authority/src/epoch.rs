//! Epoch types and the epoch validity guard.

use capnp::Error;
use tokio::sync::watch;

/// Backend-specific metadata about how an epoch was adopted.
///
/// - `Block` — stem::atomic (on-chain via Atom contract): the Ethereum block
///   number at which the HeadUpdated event was finalized.
/// - `Timestamp` — stem::eventual (off-chain via IPNS): the wall-clock Unix
///   timestamp from the IPNS record validity field.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Provenance {
    Block(u64),
    Timestamp(u64),
}

/// Epoch value used by the membrane (matches capnp struct Epoch).
///
/// An epoch anchors a point-in-time snapshot of a namespace's content root.
/// The `seq` field is monotonically increasing regardless of the source backend.
#[derive(Clone, Debug)]
pub struct Epoch {
    pub seq: u64,
    pub head: Vec<u8>,
    pub provenance: Provenance,
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
    pub fn check(&self) -> Result<(), Error> {
        let current = self.receiver.borrow();
        if current.seq != self.issued_seq {
            return Err(Error::failed(
                "staleEpoch: session epoch no longer current".to_string(),
            ));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn epoch(seq: u64, head: &[u8], block: u64) -> Epoch {
        Epoch {
            seq,
            head: head.to_vec(),
            provenance: Provenance::Block(block),
        }
    }

    #[test]
    fn epoch_guard_ok_when_seq_matches() {
        let (_tx, rx) = watch::channel(epoch(1, b"head1", 100));
        let guard = EpochGuard {
            issued_seq: 1,
            receiver: rx,
        };
        assert!(guard.check().is_ok());
    }

    #[test]
    fn epoch_guard_fails_when_seq_differs() {
        let (tx, rx) = watch::channel(epoch(1, b"head1", 100));
        let guard = EpochGuard {
            issued_seq: 1,
            receiver: rx,
        };
        assert!(guard.check().is_ok());
        tx.send(epoch(2, b"head2", 101)).unwrap();
        let res = guard.check();
        assert!(res.is_err());
        assert!(res.unwrap_err().to_string().contains("staleEpoch"));
    }
}
