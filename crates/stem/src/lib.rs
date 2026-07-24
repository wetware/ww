//! Stem: epoch source abstraction.
//!
//! Stem generalizes the epoch anchor into a trait so the same epoch-guard
//! machinery (pin, `CidTree` swap, broadcast, `EpochGuard` invalidation) works
//! against different backends:
//!
//! - **`stem::atomic`** (on-chain) — Atom contract events with blockchain finality.
//! - **`stem::eventual`** (off-chain) — IPNS records with DHT + gossipsub propagation.
//!
//! ```text
//!                 UnixPath (CID -> directory)    Value (capnp, future)
//!   +-----------+-----------------------------+-------------------------+
//!   | atomic    | stem::atomic::UnixPath       | future                  |
//!   | (chain)   | <- current Atom contract      |                         |
//!   +-----------+-----------------------------+-------------------------+
//!   | eventual  | stem::eventual::UnixPath     | future                  |
//!   | (IPNS)    | <- this crate                 |                         |
//!   +-----------+-----------------------------+-------------------------+
//! ```
//!
//! The [`StemSource`] trait is the core abstraction. Implementations run a
//! long-lived loop that sends [`Epoch`] values to a `watch::Sender` whenever
//! the content root advances. The downstream pipeline (IPFS pinning, `CidTree`
//! swap, epoch broadcast) is shared and lives in the host's `epoch.rs`.

pub mod atom;
pub mod ipns;

use anyhow::Result;
use async_trait::async_trait;
use authority::{Epoch, Provenance};
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;

/// A backend-agnostic epoch event ready for the shared pipeline.
///
/// Both `AtomSource` and `IpnsSource` convert their backend-specific events
/// into `StemEvent` before feeding the shared pin/swap/broadcast pipeline.
#[derive(Debug, Clone)]
pub struct StemEvent {
    pub seq: u64,
    pub cid: Vec<u8>,
    pub provenance: Provenance,
}

/// A source of epoch events.
///
/// Implementations run until the `shutdown` token is cancelled or an
/// unrecoverable error occurs. On each new epoch, the source sends the
/// updated [`Epoch`] to `epoch_tx`. The downstream pipeline (pin new CID,
/// swap `CidTree` root, unpin old CID, broadcast epoch) is handled by the
/// caller, not the source.
///
/// # Implementations
///
/// - `AtomSource` — wraps `AtomIndexer` + `Finalizer` for on-chain epochs.
/// - `IpnsSource` — resolves IPNS records via DHT for off-chain epochs.
#[async_trait]
pub trait StemSource: Send + 'static {
    /// Run the epoch source loop.
    ///
    /// Sends new epochs to `epoch_tx` as they are discovered/finalized.
    /// Returns `Ok(())` on clean shutdown (cancellation), or `Err` on
    /// unrecoverable failure.
    async fn run(self, epoch_tx: watch::Sender<Epoch>, shutdown: CancellationToken) -> Result<()>;
}

#[cfg(test)]
mod tests {
    use super::*;
    use authority::Provenance;

    /// A trivial StemSource that sends one epoch and exits.
    struct OneShotSource {
        epoch: Epoch,
    }

    #[async_trait]
    impl StemSource for OneShotSource {
        async fn run(
            self,
            epoch_tx: watch::Sender<Epoch>,
            _shutdown: CancellationToken,
        ) -> Result<()> {
            epoch_tx.send(self.epoch).ok();
            Ok(())
        }
    }

    #[tokio::test]
    async fn stem_source_sends_epoch() {
        let initial = Epoch {
            seq: 0,
            head: vec![],
            provenance: Provenance::Block(0),
        };
        let (tx, mut rx) = watch::channel(initial);
        let shutdown = CancellationToken::new();

        let source = OneShotSource {
            epoch: Epoch {
                seq: 1,
                head: b"abc".to_vec(),
                provenance: Provenance::Block(42),
            },
        };

        source.run(tx, shutdown).await.unwrap();

        let epoch = rx.borrow_and_update().clone();
        assert_eq!(epoch.seq, 1);
        assert_eq!(epoch.head, b"abc");
        assert_eq!(epoch.provenance, Provenance::Block(42));
    }

    #[tokio::test]
    async fn stem_source_timestamp_provenance() {
        let initial = Epoch {
            seq: 0,
            head: vec![],
            provenance: Provenance::Timestamp(0),
        };
        let (tx, mut rx) = watch::channel(initial);
        let shutdown = CancellationToken::new();

        let source = OneShotSource {
            epoch: Epoch {
                seq: 1,
                head: b"ipns-root".to_vec(),
                provenance: Provenance::Timestamp(1712438400),
            },
        };

        source.run(tx, shutdown).await.unwrap();

        let epoch = rx.borrow_and_update().clone();
        assert_eq!(epoch.seq, 1);
        assert_eq!(epoch.provenance, Provenance::Timestamp(1712438400));
    }
}
