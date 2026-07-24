//! `AtomSource`: on-chain epoch source backed by the Atom contract.
//!
//! Wraps the existing `AtomIndexer` + `Finalizer` pipeline behind the
//! `StemSource` trait. The downstream pipeline (pin, swap, broadcast) is
//! handled by the caller via `StemEvent`.

use std::sync::Arc;

use anyhow::{Context, Result};
use async_trait::async_trait;
use authority::Provenance;
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;
use tracing::{info, warn};

use atom::{AtomIndexer, FinalizerBuilder, IndexerConfig};
use authority::Epoch;

use crate::{StemEvent, StemSource};

/// On-chain epoch source: Atom contract `HeadUpdated` events.
///
/// Runs the existing `AtomIndexer` + Finalizer pipeline and converts
/// finalized events into `StemEvent` values for the shared pipeline.
pub struct AtomSource {
    pub config: IndexerConfig,
    pub confirmation_depth: u64,
}

/// Callback type for processing stem events in the shared pipeline.
///
/// The caller provides this to handle pin/swap/broadcast for each
/// finalized event. This keeps `AtomSource` decoupled from IPFS/CidTree.
pub type EventHandler = Box<
    dyn Fn(StemEvent) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<()>> + Send>>
        + Send,
>;

#[async_trait]
impl StemSource for AtomSource {
    async fn run(self, epoch_tx: watch::Sender<Epoch>, shutdown: CancellationToken) -> Result<()> {
        let indexer = Arc::new(AtomIndexer::new(self.config.clone()));
        let mut events = indexer.subscribe();

        let indexer_handle = {
            let idx = indexer.clone();
            tokio::spawn(async move {
                if let Err(e) = idx.run().await {
                    tracing::error!("Atom indexer exited with error: {e}");
                }
            })
        };

        let mut finalizer = FinalizerBuilder::new()
            .http_url(&self.config.http_url)
            .contract_address(self.config.contract_address)
            .confirmation_depth(self.confirmation_depth)
            .build()
            .context("Failed to build finalizer")?;

        loop {
            tokio::select! {
                () = shutdown.cancelled() => {
                    info!("AtomSource shutting down");
                    break;
                }
                result = events.recv() => {
                    match result {
                        Ok(ev) => {
                            finalizer.feed(ev);

                            let tip = match finalizer.current_tip().await {
                                Ok(t) => t,
                                Err(e) => {
                                    warn!("Failed to fetch chain tip: {e}");
                                    continue;
                                }
                            };

                            let finalized = match finalizer.drain_eligible(tip).await {
                                Ok(f) => f,
                                Err(e) => {
                                    warn!("Finalizer drain error: {e}");
                                    continue;
                                }
                            };

                            for fe in finalized {
                                let epoch = Epoch {
                                    seq: fe.seq,
                                    head: fe.cid.clone(),
                                    provenance: Provenance::Block(fe.block_number),
                                };
                                info!(
                                    seq = epoch.seq,
                                    ?epoch.provenance,
                                    "AtomSource: advancing epoch"
                                );
                                epoch_tx.send(epoch).ok();
                            }
                        }
                        Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                            warn!(skipped = n, "AtomSource lagged; some events were dropped");
                        }
                        Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                            info!("Indexer channel closed; AtomSource shutting down");
                            break;
                        }
                    }
                }
            }
        }

        indexer_handle.abort();
        Ok(())
    }
}
