//! Epoch pipeline: authority revocation precedes effective-root preparation.
//!
//! A finalized epoch first broadcasts `root: None`. That broadcast closes the
//! previous generation's authority before the Host prepares any filesystem
//! state. The pipeline then composes the finalized head with the frozen boot
//! overlays, gates activation on both pins, pre-warms and swaps `CidTree`, and
//! broadcasts the same epoch with `root: Some(effective_root)`.

use std::collections::HashSet;
use std::future::Future;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use atom::{AtomIndexer, Finalizer, FinalizerBuilder, IndexerConfig};
use authority::{Epoch, Provenance};
use rand::Rng;
use stem::StemEvent;
use tokio::sync::{broadcast, watch};
use tracing::{error, info, warn};

use crate::image::{cid_bytes_to_ipfs_path, dag_merge};
use ipfs;

const EPOCH_OPERATION_TIMEOUT: Duration = Duration::from_secs(30);
const EPOCH_RETRY_BASE_DELAY: Duration = Duration::from_secs(1);
const EPOCH_RETRY_MAX_DELAY: Duration = Duration::from_secs(60);
const EPOCH_RETRY_JITTER_MAX: Duration = Duration::from_millis(500);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum HostFailureClass {
    Transient,
    Permanent,
    Unknown,
}

#[derive(Debug)]
struct EpochOperationTimeout {
    operation: &'static str,
    timeout: Duration,
}

impl std::fmt::Display for EpochOperationTimeout {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            formatter,
            "epoch operation {} made no progress within {}s",
            self.operation,
            self.timeout.as_secs_f64()
        )
    }
}

impl std::error::Error for EpochOperationTimeout {}

#[derive(Debug)]
struct PermanentHostError(String);

impl std::fmt::Display for PermanentHostError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl std::error::Error for PermanentHostError {}

fn classify_host_failure(error: &anyhow::Error) -> HostFailureClass {
    if error.chain().any(|cause| cause.is::<PermanentHostError>()) {
        HostFailureClass::Permanent
    } else if error.chain().any(|cause| {
        cause
            .downcast_ref::<reqwest::Error>()
            .is_some_and(|error| error.is_connect() || error.is_timeout() || error.is_request())
            || cause
                .downcast_ref::<ipfs::KuboApiError>()
                .is_some_and(ipfs::KuboApiError::is_server_error)
            || cause.is::<EpochOperationTimeout>()
            || cause.is::<ipfs::KuboOperationTimeout>()
    }) {
        HostFailureClass::Transient
    } else {
        HostFailureClass::Unknown
    }
}

fn next_backoff_delay(current_delay: Duration, jitter: Duration) -> (Duration, Duration) {
    (
        current_delay + jitter,
        (current_delay * 2).min(EPOCH_RETRY_MAX_DELAY),
    )
}

async fn bounded<T>(operation: &'static str, future: impl Future<Output = Result<T>>) -> Result<T> {
    tokio::time::timeout(EPOCH_OPERATION_TIMEOUT, future)
        .await
        .map_err(|_| EpochOperationTimeout {
            operation,
            timeout: EPOCH_OPERATION_TIMEOUT,
        })?
}

#[derive(Debug, Default)]
struct PinSlots {
    head: Option<String>,
    root: Option<String>,
}

impl PinSlots {
    fn new(head: String, root: String) -> Self {
        Self {
            head: Some(head),
            root: Some(root),
        }
    }

    fn is_empty(&self) -> bool {
        self.head.is_none() && self.root.is_none()
    }

    fn belongs_to(&self, head: &str) -> bool {
        self.is_empty() || self.head.as_deref() == Some(head)
    }

    async fn release(
        &mut self,
        ipfs_client: &ipfs::HttpClient,
        protected: &HashSet<String>,
        handled: &mut HashSet<String>,
    ) -> Result<()> {
        release_pin_slot(&mut self.head, ipfs_client, protected, handled).await?;
        release_pin_slot(&mut self.root, ipfs_client, protected, handled).await
    }
}

async fn release_pin_slot(
    slot: &mut Option<String>,
    ipfs_client: &ipfs::HttpClient,
    protected: &HashSet<String>,
    handled: &mut HashSet<String>,
) -> Result<()> {
    let Some(cid) = slot.as_ref() else {
        return Ok(());
    };
    if protected.contains(cid) {
        slot.take();
        return Ok(());
    }
    if !handled.insert(cid.clone()) {
        slot.take();
        return Ok(());
    }
    bounded("pin removal", ipfs_client.pin_rm(cid)).await?;
    info!(%cid, "Released epoch pin");
    slot.take();
    Ok(())
}

async fn release_retained_pins(
    retained_pins: &mut Vec<PinSlots>,
    ipfs_client: &ipfs::HttpClient,
    active_pins: &PinSlots,
) {
    let protected: HashSet<String> = active_pins
        .head
        .iter()
        .chain(active_pins.root.iter())
        .cloned()
        .collect();
    let mut handled = HashSet::new();
    for pins in retained_pins.iter_mut() {
        if let Err(error) = pins.release(ipfs_client, &protected, &mut handled).await {
            warn!("Retained epoch pin release deferred: {error:#}");
        }
    }
    retained_pins.retain(|pins| !pins.is_empty());
}

fn bare_head_cid(head: &[u8]) -> Result<String> {
    let path = cid_bytes_to_ipfs_path(head).map_err(|error| {
        PermanentHostError(format!(
            "failed to parse authoritative epoch head: {error:#}"
        ))
    })?;
    path.strip_prefix("/ipfs/")
        .map(str::to_owned)
        .ok_or_else(|| {
            PermanentHostError("CID conversion did not return an IPFS path".to_owned()).into()
        })
}

fn broadcast_authority(epoch_tx: &watch::Sender<Epoch>, event: &StemEvent) {
    epoch_tx.send_replace(Epoch {
        seq: event.seq,
        head: event.cid.clone(),
        root: None,
        provenance: event.provenance.clone(),
    });
}

async fn prepare_effective_root(
    head: &str,
    overlays: &[String],
    ipfs_client: &ipfs::HttpClient,
    cid_tree: Option<&Arc<crate::vfs::CidTree>>,
    attempt_pins: &mut PinSlots,
) -> Result<String> {
    bounded("head pin", ipfs_client.pin_add(head))
        .await
        .context("pinning authoritative epoch head")?;
    attempt_pins.head = Some(head.to_owned());

    let mut layers = Vec::with_capacity(overlays.len() + 1);
    layers.push(head.to_owned());
    layers.extend_from_slice(overlays);
    let boot_client = ipfs::BootClient::one_attempt(ipfs_client.clone(), EPOCH_OPERATION_TIMEOUT);
    let (_cancel_tx, mut cancel_rx) = watch::channel(false);
    let effective = bounded(
        "effective-root merge",
        dag_merge(&layers, &boot_client, &mut cancel_rx),
    )
    .await
    .context("composing effective epoch root")?;
    attempt_pins.root = Some(effective.clone());

    if let Some(tree) = cid_tree {
        bounded("effective-root prewarm", tree.pre_warm(&effective))
            .await
            .context("pre-warming effective epoch root")?;
        tree.swap_root(effective.clone());
        info!(root = %effective, "CidTree effective root swapped");
    }
    Ok(effective)
}

async fn next_finalized_batch(
    events: &mut broadcast::Receiver<atom::HeadUpdatedObserved>,
    finalizer: &mut Finalizer,
) -> Option<Vec<StemEvent>> {
    loop {
        match events.recv().await {
            Ok(event) => finalizer.feed(event),
            Err(broadcast::error::RecvError::Lagged(skipped)) => {
                warn!(
                    skipped,
                    "Epoch pipeline lagged; observed events were dropped"
                );
                continue;
            }
            Err(broadcast::error::RecvError::Closed) => return None,
        }

        let tip = match finalizer.current_tip().await {
            Ok(tip) => tip,
            Err(error) => {
                warn!("Failed to fetch chain tip: {error}");
                continue;
            }
        };
        let finalized = match finalizer.drain_eligible(tip).await {
            Ok(finalized) => finalized,
            Err(error) => {
                warn!("Finalizer drain error: {error}");
                continue;
            }
        };
        if !finalized.is_empty() {
            return Some(
                finalized
                    .into_iter()
                    .map(|event| StemEvent {
                        seq: event.seq,
                        cid: event.cid,
                        provenance: Provenance::Block(event.block_number),
                    })
                    .collect(),
            );
        }
    }
}

/// Run the Atom epoch pipeline with one pending authoritative target.
#[allow(clippy::too_many_arguments)]
pub async fn run_epoch_pipeline(
    config: IndexerConfig,
    epoch_tx: watch::Sender<Epoch>,
    confirmation_depth: u64,
    ipfs_client: ipfs::HttpClient,
    cid_tree: Option<Arc<crate::vfs::CidTree>>,
    overlays: Vec<String>,
    initial_head: String,
    initial_root: String,
) -> Result<()> {
    for overlay in &overlays {
        overlay.parse::<cid::Cid>().map_err(|error| {
            PermanentHostError(format!("malformed frozen overlay CID {overlay}: {error}"))
        })?;
    }

    let indexer = Arc::new(AtomIndexer::new(config.clone()));
    let mut events = indexer.subscribe();
    let indexer_handle = {
        let indexer = Arc::clone(&indexer);
        tokio::spawn(async move {
            if let Err(error) = indexer.run().await {
                error!("Atom indexer exited with error: {error}");
            }
        })
    };
    let mut finalizer = FinalizerBuilder::new()
        .http_url(&config.http_url)
        .contract_address(config.contract_address)
        .confirmation_depth(confirmation_depth)
        .build()
        .context("Failed to build finalizer")?;

    let mut pending: Option<(u64, Vec<u8>)> = None;
    let mut current_delay = EPOCH_RETRY_BASE_DELAY;
    let mut active_pins = PinSlots::new(initial_head, initial_root);
    let mut retained_pins = Vec::new();
    let mut attempt_pins = PinSlots::default();

    loop {
        if pending.is_none() {
            let Some(batch) = next_finalized_batch(&mut events, &mut finalizer).await else {
                info!("Indexer channel closed; epoch pipeline shutting down");
                break;
            };
            for event in batch {
                pending.take();
                broadcast_authority(&epoch_tx, &event);
                info!(
                    seq = event.seq,
                    "Advancing epoch authority; readiness closed"
                );
                pending = Some((event.seq, event.cid));
                current_delay = EPOCH_RETRY_BASE_DELAY;
            }
        }

        let (seq, head_bytes) = pending.as_ref().expect("pending target exists");
        let seq = *seq;
        let head_bytes = head_bytes.clone();
        let head = match bare_head_cid(&head_bytes) {
            Ok(head) => head,
            Err(error) => {
                error!(seq, head = %hex::encode(&head_bytes), "Permanent authoritative epoch failure: {error:#}");
                indexer_handle.abort();
                return Err(error);
            }
        };

        release_retained_pins(&mut retained_pins, &ipfs_client, &active_pins).await;

        if !attempt_pins.belongs_to(&head) {
            let protected: HashSet<String> = active_pins
                .head
                .iter()
                .chain(active_pins.root.iter())
                .cloned()
                .collect();
            let mut handled = HashSet::new();
            if let Err(error) = attempt_pins
                .release(&ipfs_client, &protected, &mut handled)
                .await
            {
                warn!(seq, head = %head, "Superseded epoch pin release will retry: {error:#}");
            }
        }

        let attempt = if attempt_pins.belongs_to(&head) {
            prepare_effective_root(
                &head,
                &overlays,
                &ipfs_client,
                cid_tree.as_ref(),
                &mut attempt_pins,
            )
            .await
        } else {
            Err(anyhow::anyhow!(EpochOperationTimeout {
                operation: "superseded epoch pin release",
                timeout: EPOCH_OPERATION_TIMEOUT,
            }))
        };

        match attempt {
            Ok(effective) => {
                let provenance = epoch_tx.borrow().provenance.clone();
                epoch_tx.send_replace(Epoch {
                    seq,
                    head: head_bytes,
                    root: Some(effective.clone()),
                    provenance,
                });
                info!(seq, head = %head, root = %effective, "Effective epoch root is ready");

                let new_pins = std::mem::take(&mut attempt_pins);
                retained_pins.push(std::mem::replace(&mut active_pins, new_pins));
                release_retained_pins(&mut retained_pins, &ipfs_client, &active_pins).await;
                pending = None;
                current_delay = EPOCH_RETRY_BASE_DELAY;
            }
            Err(error) => match classify_host_failure(&error) {
                HostFailureClass::Transient => {
                    let jitter_millis =
                        rand::rng().random_range(0..EPOCH_RETRY_JITTER_MAX.as_millis() as u64);
                    let (sleep_duration, next_delay) =
                        next_backoff_delay(current_delay, Duration::from_millis(jitter_millis));
                    current_delay = next_delay;
                    warn!(
                        seq,
                        head = %head,
                        retry_ms = sleep_duration.as_millis() as u64,
                        "Transient epoch preparation failure; retry scheduled: {error:#}"
                    );

                    tokio::select! {
                        _ = tokio::time::sleep(sleep_duration) => {}
                        batch = next_finalized_batch(&mut events, &mut finalizer) => {
                            let Some(batch) = batch else {
                                continue;
                            };
                            for event in batch {
                                pending.take();
                                broadcast_authority(&epoch_tx, &event);
                                info!(seq = event.seq, "Advancing epoch authority; pending preparation superseded");
                                pending = Some((event.seq, event.cid));
                                current_delay = EPOCH_RETRY_BASE_DELAY;
                            }
                        }
                    }
                }
                HostFailureClass::Permanent | HostFailureClass::Unknown => {
                    let class = classify_host_failure(&error);
                    error!(seq, head = %head, ?class, "Epoch preparation failed hard: {error:#}");
                    indexer_handle.abort();
                    return Err(error);
                }
            },
        }
    }

    indexer_handle.abort();
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    #[test]
    fn backoff_doubles_to_cap_and_resets() {
        let mut current = EPOCH_RETRY_BASE_DELAY;
        for attempt in 0..8 {
            let jitter = Duration::from_millis(499);
            let (sleep, next) = next_backoff_delay(current, jitter);
            let expected = (EPOCH_RETRY_BASE_DELAY * 2_u32.pow(attempt)).min(EPOCH_RETRY_MAX_DELAY);
            assert_eq!(current, expected);
            assert!(sleep >= expected);
            assert!(sleep < expected + EPOCH_RETRY_JITTER_MAX);
            assert!(next >= current);
            current = next;
        }
        current = EPOCH_RETRY_BASE_DELAY;
        assert_eq!(current, EPOCH_RETRY_BASE_DELAY);
    }

    #[test]
    fn malformed_head_is_permanent_before_network_access() {
        let error = bare_head_cid(b"not-a-cid").unwrap_err();
        assert_eq!(classify_host_failure(&error), HostFailureClass::Permanent);
    }

    #[test]
    fn root_only_attempt_remains_superseded_until_release_succeeds() {
        let pins = PinSlots {
            head: None,
            root: Some("old-root".to_owned()),
        };

        assert!(!pins.belongs_to("new-head"));
    }

    #[test]
    fn authority_broadcast_precedes_root_swap() {
        let initial = Epoch {
            seq: 1,
            head: Vec::new(),
            root: Some("old-root".to_owned()),
            provenance: Provenance::Block(1),
        };
        let (epoch_tx, epoch_rx) = watch::channel(initial);
        let client = ipfs::HttpClient::new("http://127.0.0.1:1".to_owned());
        let staging = tempfile::tempdir().unwrap();
        let tree = crate::vfs::CidTree::new(
            "old-root".to_owned(),
            client,
            HashMap::new(),
            staging.path().to_owned(),
        );
        let event = StemEvent {
            seq: 2,
            cid: vec![1],
            provenance: Provenance::Block(2),
        };

        broadcast_authority(&epoch_tx, &event);

        assert_eq!(epoch_rx.borrow().seq, 2);
        assert_eq!(epoch_rx.borrow().root, None);
        assert_eq!(tree.root_cid().as_ref(), "old-root");
    }

    #[tokio::test]
    async fn four_xx_kubo_error_is_unknown() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut request = [0; 2048];
            let _ = stream.read(&mut request).await.unwrap();
            stream
                .write_all(b"HTTP/1.1 400 Bad Request\r\nContent-Length: 3\r\nConnection: close\r\n\r\nbad")
                .await
                .unwrap();
        });
        let client = ipfs::HttpClient::new(format!("http://{address}"));
        let error = client.pin_add("bafy-invalid").await.unwrap_err();
        assert_eq!(classify_host_failure(&error), HostFailureClass::Unknown);
        server.await.unwrap();
    }

    #[tokio::test]
    async fn head_pin_failure_gates_root_swap() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut request = [0; 2048];
            let _ = stream.read(&mut request).await.unwrap();
            stream
                .write_all(b"HTTP/1.1 400 Bad Request\r\nContent-Length: 3\r\nConnection: close\r\n\r\nbad")
                .await
                .unwrap();
        });
        let client = ipfs::HttpClient::new(format!("http://{address}"));
        let staging = tempfile::tempdir().unwrap();
        let tree = Arc::new(crate::vfs::CidTree::new(
            "old-root".to_owned(),
            client.clone(),
            HashMap::new(),
            staging.path().to_owned(),
        ));
        let mut pins = PinSlots::default();

        let error = prepare_effective_root("new-head", &[], &client, Some(&tree), &mut pins)
            .await
            .unwrap_err();

        assert_eq!(classify_host_failure(&error), HostFailureClass::Unknown);
        assert_eq!(tree.root_cid().as_ref(), "old-root");
        assert!(pins.is_empty());
        server.await.unwrap();
    }

    #[tokio::test]
    async fn failed_pin_release_retains_one_owner_and_guarded_release_skips_active_cid() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut request = [0; 2048];
            let _ = stream.read(&mut request).await.unwrap();
            stream
                .write_all(b"HTTP/1.1 503 Service Unavailable\r\nContent-Length: 4\r\nConnection: close\r\n\r\nbusy")
                .await
                .unwrap();
        });
        let client = ipfs::HttpClient::new(format!("http://{address}"));
        let mut retained = vec![
            PinSlots {
                head: Some("shared".to_owned()),
                root: None,
            },
            PinSlots {
                head: Some("shared".to_owned()),
                root: None,
            },
        ];
        release_retained_pins(&mut retained, &client, &PinSlots::default()).await;
        assert_eq!(retained.len(), 1);
        assert_eq!(retained[0].head.as_deref(), Some("shared"));
        server.await.unwrap();

        let active = PinSlots {
            head: Some("shared".to_owned()),
            root: None,
        };
        retained[0]
            .release(
                &client,
                &HashSet::from(["shared".to_owned()]),
                &mut HashSet::new(),
            )
            .await
            .unwrap();
        assert!(retained[0].is_empty());
        release_retained_pins(&mut retained, &client, &active).await;
        assert!(retained.is_empty());
    }
}
