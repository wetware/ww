//! Epoch pipeline: source-agnostic pin/swap/broadcast for epoch advances.
//!
//! The pipeline handles the downstream side of any epoch source:
//! 1. Pin new CID on IPFS
//! 2. Pre-warm and swap CidTree root (FS sees new content)
//! 3. Unpin old CID
//! 4. Broadcast epoch (capabilities die, guests re-negotiate)
//!
//! For the Atom-specific indexer+finalizer pipeline (legacy entry point),
//! see `run_atom_pipeline` which wraps `AtomSource` from `crates/stem/`.

use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use authority::Epoch;
use stem::StemEvent;
use tokio::sync::watch;
use tracing::{info, warn};

use crate::image::cid_bytes_to_ipfs_path;
use ipfs;

/// Process a single epoch advance: pin new CID, swap CidTree, unpin old, broadcast.
///
/// This is the shared downstream pipeline that all epoch sources feed into.
/// The ordering follows the two-layer revocation model:
/// 1. Pin new CID
/// 2. Pre-warm CidTree directory cache for new root
/// 3. Swap CidTree root (FS sees new content)
/// 4. Unpin old CID
/// 5. Drain — SIGTERM window for in-flight operations
/// 6. Broadcast epoch — SIGKILL, capabilities die, guests re-negotiate
///
/// `drain_duration` controls graceful shutdown: when non-zero, capabilities have
/// this long to finish in-flight work before the epoch advances and they die.
/// During the drain window the FS already serves new content (CidTree was swapped),
/// but capabilities still reference the old epoch.
pub async fn handle_epoch_advance(
    event: &StemEvent,
    epoch_tx: &watch::Sender<Epoch>,
    ipfs_client: &ipfs::HttpClient,
    prev_ipfs_path: &mut Option<String>,
    cid_tree: Option<&Arc<crate::vfs::CidTree>>,
    drain_duration: Duration,
) -> Result<()> {
    let ipfs_path =
        cid_bytes_to_ipfs_path(&event.cid).context("Failed to convert CID to IPFS path")?;

    // Extract CID string from /ipfs/<cid>
    let cid_str = ipfs_path
        .strip_prefix("/ipfs/")
        .unwrap_or(&ipfs_path)
        .to_string();

    // 1. Pin the new head.
    if let Err(e) = ipfs_client.pin_add(&ipfs_path).await {
        warn!(path = %ipfs_path, "Failed to pin new head (continuing): {e}");
    } else {
        info!(seq = event.seq, path = %ipfs_path, "Pinned new head");
    }

    // 2-3. Pre-warm and swap CidTree root (FS swap happens-before capability death).
    if let Some(tree) = cid_tree {
        if let Err(e) = tree.pre_warm(&cid_str).await {
            warn!(cid = %cid_str, "CidTree pre-warm failed (continuing): {e}");
        }
        tree.swap_root(cid_str);
        info!(seq = event.seq, "CidTree root swapped");
    }

    // 4. Unpin the previous head.
    if let Some(prev) = prev_ipfs_path.take() {
        if let Err(e) = ipfs_client.pin_rm(&prev).await {
            warn!(path = %prev, "Failed to unpin old head (continuing): {e}");
        } else {
            info!(path = %prev, "Unpinned old head");
        }
    }

    *prev_ipfs_path = Some(ipfs_path);

    // 5. Drain — give in-flight operations time to finish.
    if !drain_duration.is_zero() {
        info!(
            seq = event.seq,
            drain_ms = drain_duration.as_millis() as u64,
            "Draining epoch — capabilities have {}s to finish",
            drain_duration.as_secs_f32()
        );
        tokio::time::sleep(drain_duration).await;
    }

    // 6. Broadcast epoch (capabilities die, guests re-negotiate).
    let new_epoch = Epoch {
        seq: event.seq,
        head: event.cid.clone(),
        provenance: event.provenance.clone(),
    };

    info!(
        seq = new_epoch.seq,
        ?new_epoch.provenance,
        "Advancing epoch"
    );

    epoch_tx.send(new_epoch).ok();
    Ok(())
}

// ── Legacy entry point (Atom-specific) ──────────────────────────────

use atom::{AtomIndexer, FinalizerBuilder, IndexerConfig};
use authority::Provenance;

/// Run the Atom-specific epoch pipeline (legacy entry point).
///
/// Wraps the old AtomIndexer + Finalizer flow with the shared
/// `handle_epoch_advance` downstream. Callers should migrate to
/// `AtomSource::run()` from `crates/stem/` for new code.
///
/// `drain_duration` controls graceful shutdown: when non-zero, capabilities have
/// this long to finish in-flight work before the epoch advances and they die.
pub async fn run_epoch_pipeline(
    config: IndexerConfig,
    epoch_tx: watch::Sender<Epoch>,
    confirmation_depth: u64,
    ipfs_client: ipfs::HttpClient,
    cid_tree: Option<Arc<crate::vfs::CidTree>>,
    drain_duration: Duration,
) -> Result<()> {
    let indexer = Arc::new(AtomIndexer::new(config.clone()));
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
        .http_url(&config.http_url)
        .contract_address(config.contract_address)
        .confirmation_depth(confirmation_depth)
        .build()
        .context("Failed to build finalizer")?;

    let mut prev_ipfs_path: Option<String> = None;

    loop {
        match events.recv().await {
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
                    let stem_event = StemEvent {
                        seq: fe.seq,
                        cid: fe.cid.clone(),
                        provenance: Provenance::Block(fe.block_number),
                    };
                    if let Err(e) = handle_epoch_advance(
                        &stem_event,
                        &epoch_tx,
                        &ipfs_client,
                        &mut prev_ipfs_path,
                        cid_tree.as_ref(),
                        drain_duration,
                    )
                    .await
                    {
                        warn!(seq = fe.seq, "Failed to handle finalized event: {e}");
                    }
                }
            }
            Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                warn!(
                    skipped = n,
                    "Epoch pipeline lagged; some events were dropped"
                );
            }
            Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                info!("Indexer channel closed; epoch pipeline shutting down");
                break;
            }
        }
    }

    indexer_handle.abort();
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use authority::Provenance;
    use std::collections::HashMap;

    fn test_stem_event(seq: u64) -> StemEvent {
        // Build a valid CIDv1 (raw codec, identity multihash) so
        // cid_bytes_to_ipfs_path succeeds.
        let mh = cid::multihash::Multihash::<64>::wrap(0x00, b"test").expect("identity mh");
        let c = cid::Cid::new_v1(0x55, mh);
        StemEvent {
            seq,
            cid: c.to_bytes(),
            provenance: Provenance::Block(42),
        }
    }

    fn event_cid_string(event: &StemEvent) -> String {
        cid_bytes_to_ipfs_path(&event.cid)
            .expect("valid event cid")
            .strip_prefix("/ipfs/")
            .expect("ipfs path prefix")
            .to_string()
    }

    /// Drain delay defers epoch broadcast by the configured duration.
    #[tokio::test]
    async fn drain_delay_defers_epoch_broadcast() {
        let (epoch_tx, mut epoch_rx) = watch::channel(Epoch {
            seq: 0,
            head: vec![],
            provenance: Provenance::Block(0),
        });

        let event = test_stem_event(1);
        let ipfs_client = ipfs::HttpClient::new("http://127.0.0.1:1".into());

        let drain = Duration::from_millis(200);
        let start = tokio::time::Instant::now();

        let _ = handle_epoch_advance(&event, &epoch_tx, &ipfs_client, &mut None, None, drain).await;

        let elapsed = start.elapsed();
        assert!(
            elapsed >= drain,
            "epoch broadcast should be deferred by drain duration ({drain:?}), but only {elapsed:?} elapsed"
        );

        epoch_rx.mark_changed();
        let epoch = epoch_rx.borrow_and_update().clone();
        assert_eq!(epoch.seq, 1, "epoch should have advanced to seq=1");
    }

    /// Zero drain duration broadcasts immediately (no regression).
    #[tokio::test]
    async fn zero_drain_broadcasts_immediately() {
        let (epoch_tx, mut epoch_rx) = watch::channel(Epoch {
            seq: 0,
            head: vec![],
            provenance: Provenance::Block(0),
        });

        let event = test_stem_event(1);
        let ipfs_client = ipfs::HttpClient::new("http://127.0.0.1:1".into());

        let start = tokio::time::Instant::now();
        let _ = handle_epoch_advance(
            &event,
            &epoch_tx,
            &ipfs_client,
            &mut None,
            None,
            Duration::ZERO,
        )
        .await;
        let elapsed = start.elapsed();

        assert!(
            elapsed < Duration::from_millis(100),
            "zero drain should broadcast immediately, took {elapsed:?}"
        );

        epoch_rx.mark_changed();
        let epoch = epoch_rx.borrow_and_update().clone();
        assert_eq!(epoch.seq, 1);
    }

    /// CidTree root swap happens before the epoch broadcast drain completes.
    #[tokio::test]
    async fn cid_tree_root_swaps_to_event_cid_before_epoch_broadcast() {
        let (epoch_tx, epoch_rx) = watch::channel(Epoch {
            seq: 0,
            head: vec![],
            provenance: Provenance::Block(0),
        });

        let event = test_stem_event(1);
        let target_cid = event_cid_string(&event);
        let ipfs_client = ipfs::HttpClient::new("http://127.0.0.1:1".into());
        let staging_dir = tempfile::tempdir().expect("temp staging dir");
        let cid_tree = Arc::new(crate::vfs::CidTree::new(
            "initial-root".to_string(),
            ipfs_client.clone(),
            HashMap::new(),
            staging_dir.path().to_path_buf(),
        ));
        let mut prev_ipfs_path = None;
        let drain = Duration::from_millis(500);

        tokio::time::timeout(Duration::from_secs(2), async {
            let handle = handle_epoch_advance(
                &event,
                &epoch_tx,
                &ipfs_client,
                &mut prev_ipfs_path,
                Some(&cid_tree),
                drain,
            );
            tokio::pin!(handle);

            loop {
                tokio::select! {
                    result = &mut handle => {
                        result.expect("epoch advance succeeds despite unreachable IPFS");
                        panic!("epoch advance completed before observing CidTree root swap");
                    }
                    _ = tokio::time::sleep(Duration::from_millis(10)) => {
                        if cid_tree.root_cid().as_ref() == &target_cid {
                            assert_eq!(cid_tree.root_cid().as_ref(), &target_cid);
                            assert!(
                                !epoch_rx.has_changed().expect("epoch channel open"),
                                "epoch broadcast should not happen until after drain"
                            );
                            break;
                        }
                    }
                }
            }

            handle.await.expect("epoch advance succeeds");
        })
        .await
        .expect("CidTree root should swap promptly");

        assert_eq!(cid_tree.root_cid().as_ref(), &target_cid);
    }
}
