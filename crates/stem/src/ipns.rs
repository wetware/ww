//! `IpnsSource`: off-chain epoch source backed by IPNS record resolution.
//!
//! Polls an IPNS name via the IPFS HTTP API, comparing the resolved CID's
//! sequence number against the last known value. When the sequence advances,
//! emits a new epoch with `Provenance::Timestamp`.
//!
//! For Phase 1, this uses polling (no gossipsub). Gossipsub-based instant
//! notification is deferred to Phase 2.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use async_trait::async_trait;
use authority::{Epoch, Provenance};
use tokio::sync::watch;
use tokio::time::sleep;
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};

use crate::StemSource;

/// Configuration for an IPNS epoch source.
#[derive(Clone, Debug)]
pub struct IpnsConfig {
    /// The IPNS name to resolve (peer ID or key name, e.g., "k51qzi5uqu5d...").
    pub name: String,
    /// Base URL of the IPFS HTTP API (e.g., "<http://localhost:5001>").
    pub ipfs_api_url: String,
    /// How often to poll for IPNS record changes.
    pub poll_interval: Duration,
    /// Maximum consecutive failures before logging at WARN level.
    pub max_silent_failures: u32,
}

impl Default for IpnsConfig {
    fn default() -> Self {
        Self {
            name: String::new(),
            ipfs_api_url: "http://localhost:5001".into(),
            poll_interval: Duration::from_secs(30),
            max_silent_failures: 3,
        }
    }
}

/// Off-chain epoch source: IPNS record resolution via IPFS HTTP API.
///
/// Polls the configured IPNS name on an interval. When the resolved CID
/// changes (detected by comparing the path string), emits a new epoch.
/// The sequence number is tracked locally and incremented on each change.
///
/// # Consistency model
///
/// Single-writer: only the IPNS key holder can publish new records. Recipients
/// may see stale records (eventual consistency). The sequence number is
/// monotonically increasing per the IPNS v2 spec.
pub struct IpnsSource {
    pub config: IpnsConfig,
}

/// Resolve an IPNS name via the IPFS HTTP API.
///
/// Returns the resolved IPFS path (e.g., "/ipfs/bafy...").
async fn resolve_name(http_client: &reqwest::Client, api_url: &str, name: &str) -> Result<String> {
    let url = format!("{api_url}/api/v0/name/resolve?arg={name}&nocache=true");
    let response = http_client
        .post(&url)
        .send()
        .await
        .context("IPNS name resolve request failed")?;
    let status = response.status();
    let body: serde_json::Value = response
        .json()
        .await
        .context("Failed to parse name resolve response")?;
    if !status.is_success() {
        let msg = body["Message"].as_str().unwrap_or("unknown error");
        anyhow::bail!("IPNS resolve failed ({status}): {msg}");
    }
    body["Path"]
        .as_str()
        .map(std::string::ToString::to_string)
        .ok_or_else(|| anyhow::anyhow!("name resolve response missing Path field"))
}

/// Extract raw CID bytes from an IPFS path like "/ipfs/bafy...".
fn path_to_cid_bytes(ipfs_path: &str) -> Result<Vec<u8>> {
    let cid_str = ipfs_path.strip_prefix("/ipfs/").unwrap_or(ipfs_path);
    // Parse as a CID to get canonical bytes.
    // For now, store the CID string as UTF-8 bytes (consistent with Atom's
    // approach where head bytes are the raw CID encoding).
    Ok(cid_str.as_bytes().to_vec())
}

fn now_unix_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

#[async_trait]
impl StemSource for IpnsSource {
    async fn run(self, epoch_tx: watch::Sender<Epoch>, shutdown: CancellationToken) -> Result<()> {
        let http_client = reqwest::Client::builder()
            .no_proxy()
            .timeout(Duration::from_secs(30))
            .build()
            .expect("reqwest client");

        let mut current_path: Option<String> = None;
        let mut current_seq: u64 = epoch_tx.borrow().seq;
        let mut consecutive_failures: u32 = 0;

        info!(
            name = %self.config.name,
            poll_interval = ?self.config.poll_interval,
            "IpnsSource starting"
        );

        loop {
            tokio::select! {
                () = shutdown.cancelled() => {
                    info!("IpnsSource shutting down");
                    return Ok(());
                }
                () = sleep(self.config.poll_interval) => {
                    match resolve_name(
                        &http_client,
                        &self.config.ipfs_api_url,
                        &self.config.name,
                    ).await {
                        Ok(resolved_path) => {
                            consecutive_failures = 0;

                            // Check if the resolved path changed.
                            let changed =
                                current_path.as_deref() != Some(resolved_path.as_str());

                            if changed {
                                current_seq += 1;
                                let cid_bytes = match path_to_cid_bytes(&resolved_path) {
                                    Ok(b) => b,
                                    Err(e) => {
                                        warn!("Failed to parse CID from {resolved_path}: {e}");
                                        continue;
                                    }
                                };

                                let epoch = Epoch {
                                    seq: current_seq,
                                    head: cid_bytes,
                                    provenance: Provenance::Timestamp(now_unix_secs()),
                                };

                                info!(
                                    seq = epoch.seq,
                                    path = %resolved_path,
                                    "IpnsSource: epoch advanced"
                                );

                                epoch_tx.send(epoch).ok();
                                current_path = Some(resolved_path);
                            } else {
                                debug!(
                                    name = %self.config.name,
                                    path = %resolved_path,
                                    "IpnsSource: no change"
                                );
                            }
                        }
                        Err(e) => {
                            consecutive_failures += 1;
                            if consecutive_failures <= self.config.max_silent_failures {
                                debug!(
                                    name = %self.config.name,
                                    attempt = consecutive_failures,
                                    "IpnsSource resolve failed: {e}"
                                );
                            } else {
                                warn!(
                                    name = %self.config.name,
                                    attempt = consecutive_failures,
                                    "IpnsSource resolve failed: {e}"
                                );
                            }
                        }
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn path_to_cid_bytes_strips_prefix() {
        let bytes =
            path_to_cid_bytes("/ipfs/bafybeigdyrzt5sfp7udm7hu76uh7y26nf3efuylqabf3oclgtqy55fbzdi")
                .unwrap();
        assert_eq!(
            String::from_utf8(bytes).unwrap(),
            "bafybeigdyrzt5sfp7udm7hu76uh7y26nf3efuylqabf3oclgtqy55fbzdi"
        );
    }

    #[test]
    fn path_to_cid_bytes_handles_bare_cid() {
        let bytes = path_to_cid_bytes("bafyabc123").unwrap();
        assert_eq!(String::from_utf8(bytes).unwrap(), "bafyabc123");
    }

    #[test]
    fn default_config() {
        let cfg = IpnsConfig::default();
        assert_eq!(cfg.poll_interval, Duration::from_secs(30));
        assert_eq!(cfg.max_silent_failures, 3);
    }
}
