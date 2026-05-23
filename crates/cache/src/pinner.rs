use anyhow::Result;
use async_trait::async_trait;
use cid::Cid;

/// Abstraction over IPFS pin/unpin/fetch operations.
///
/// The cache manages pins as its eviction mechanism: pinning ensures content
/// stays in the IPFS node's blockstore, unpinning allows GC to reclaim it.
#[async_trait]
pub trait Pinner: Send + Sync {
    /// Pin a CID in the IPFS node, preventing GC.
    async fn pin(&self, cid: &Cid) -> Result<()>;

    /// Unpin a CID, allowing the IPFS node to GC the content.
    async fn unpin(&self, cid: &Cid) -> Result<()>;

    /// Fetch the raw bytes for a CID.
    async fn fetch(&self, cid: &Cid) -> Result<Vec<u8>>;

    /// Fetch raw bytes for a subpath under a CID (for example:
    /// `/ipfs/<cid>/<subpath>`).
    ///
    /// Default implementations may only support flat CID fetches.
    async fn fetch_path(&self, cid: &Cid, subpath: &str) -> Result<Vec<u8>> {
        if subpath.is_empty() {
            return self.fetch(cid).await;
        }
        Err(anyhow::anyhow!(
            "subpath fetch not supported: /ipfs/{cid}/{subpath}"
        ))
    }

    /// Get the size in bytes of the content addressed by a CID,
    /// without fetching the full content.
    async fn size(&self, cid: &Cid) -> Result<u64>;
}
