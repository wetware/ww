//! Publication orchestration for an already sealed local source batch.

use std::fmt;

use cid::Cid;
use ipfs::snapshot::PublicationBudget;
use tokio::sync::watch;

use super::{await_or_cancel, verify_capture, CaptureBatch, FrozenSource};

/// Verified source identities whose recursive retention was acknowledged.
///
/// These identities carry no independent Kubo pin reference count. The caller
/// must transfer every root into deployment's frozen-source ownership before
/// preparation, including roots hidden by later operations. Dropping this value
/// deliberately leaves the pins retained.
#[derive(Debug)]
pub struct PublishedSources {
    sources: Vec<FrozenSource>,
}

impl PublishedSources {
    pub fn sources(&self) -> &[FrozenSource] {
        &self.sources
    }

    /// Transfer identities to the caller's existing retention owner.
    pub fn into_sources(self) -> Vec<FrozenSource> {
        self.sources
    }
}

/// Failed publication never exposes partially verified sources as ready.
///
/// Acknowledged roots remain retained. The current attempt may also have left
/// blocks or pins even when no CID was received. Never compensate with unpin:
/// repeated native pins do not establish independent ownership.
#[derive(Debug)]
pub struct SnapshotPublicationFailure {
    pub acknowledged_roots: Vec<Cid>,
    pub uncertain_source: Option<usize>,
    error: anyhow::Error,
}

impl fmt::Display for SnapshotPublicationFailure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "local snapshot publication failed: {:#}; {} acknowledged roots remain retained",
            self.error,
            self.acknowledged_roots.len()
        )?;
        if let Some(index) = self.uncertain_source {
            write!(f, "; source {index} may have left residual blocks or pins")?;
        }
        Ok(())
    }
}

impl std::error::Error for SnapshotPublicationFailure {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(self.error.as_ref())
    }
}

impl CaptureBatch {
    /// Publish and verify this entire sealed batch. No host paths are reopened.
    ///
    /// All local sources must enter the same capture batch. Landing C must also
    /// count nonlocal operations and the authoritative head against the total
    /// operation limit. Transport retries share this batch's accounting and use
    /// only captured bytes. Cancellation after upload can leave residual pins.
    pub async fn publish(
        self,
        client: &ipfs::BootClient,
        cancel: &mut watch::Receiver<bool>,
    ) -> Result<PublishedSources, SnapshotPublicationFailure> {
        let mut verification = verify_capture::VerificationBudget::default();
        self.publish_with_budget(client, cancel, &mut verification)
            .await
    }

    pub(super) async fn publish_with_budget(
        self,
        client: &ipfs::BootClient,
        cancel: &mut watch::Receiver<bool>,
        verification: &mut verify_capture::VerificationBudget,
    ) -> Result<PublishedSources, SnapshotPublicationFailure> {
        let publication = PublicationBudget::default();
        let mut acknowledged_roots = Vec::new();
        let mut sources = Vec::new();
        for (index, captured) in self.into_sources().into_iter().enumerate() {
            if *cancel.borrow() || cancel.has_changed().is_err() {
                return Err(SnapshotPublicationFailure {
                    acknowledged_roots,
                    uncertain_source: None,
                    error: anyhow::anyhow!("snapshot publication cancelled before upload"),
                });
            }
            let published = match await_or_cancel(
                cancel,
                client.publish_snapshot(captured.entries(), &publication),
            )
            .await
            {
                Ok(published) => published,
                Err(error) => {
                    return Err(SnapshotPublicationFailure {
                        acknowledged_roots,
                        uncertain_source: Some(index),
                        error,
                    });
                }
            };
            acknowledged_roots.push(published.events[""].cid);
            let verified = await_or_cancel(
                cancel,
                verify_capture::verify(client, &captured, &published.events, verification),
            )
            .await;
            match verified {
                Ok(source) => sources.push(source),
                Err(error) => {
                    return Err(SnapshotPublicationFailure {
                        acknowledged_roots,
                        uncertain_source: None,
                        error,
                    });
                }
            }
        }
        Ok(PublishedSources { sources })
    }
}
