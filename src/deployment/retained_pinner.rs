//! Cache reads for deployment content without destructive shared-pin cleanup.

use std::path::Path;

use anyhow::Result;
use async_trait::async_trait;
use cache::Pinner;
use cid::Cid;

/// Deployment cache adapter that retains Kubo pins on cache eviction.
///
/// Kubo pins have CID identity, not independent owners. A cache pin can overlap
/// a frozen, prepared, or active deployment root. Cache eviction still releases
/// local staging and residency; it cannot prove ownership of the Kubo pin.
/// Retained Kubo content therefore outlives cache eviction and requires explicit
/// operator cleanup after all deployment users have released it.
pub struct RetainedPinner {
    client: ipfs::HttpClient,
}

impl RetainedPinner {
    pub fn new(client: ipfs::HttpClient) -> Self {
        Self { client }
    }
}

#[async_trait]
impl Pinner for RetainedPinner {
    async fn pin(&self, cid: &Cid) -> Result<()> {
        self.client.pin(cid).await
    }

    async fn unpin(&self, cid: &Cid) -> Result<()> {
        tracing::debug!(%cid, "cache residency released; shared Kubo pin retained");
        Ok(())
    }

    async fn fetch(&self, cid: &Cid) -> Result<Vec<u8>> {
        self.client.fetch(cid).await
    }

    async fn fetch_path(&self, cid: &Cid, subpath: &str) -> Result<Vec<u8>> {
        self.client.fetch_path(cid, subpath).await
    }

    async fn fetch_to_path(&self, cid: &Cid, dst: &Path) -> Result<()> {
        self.client.fetch_to_path(cid, dst).await
    }

    async fn fetch_path_to_path(&self, cid: &Cid, subpath: &str, dst: &Path) -> Result<()> {
        self.client.fetch_path_to_path(cid, subpath, dst).await
    }

    async fn size(&self, cid: &Cid) -> Result<u64> {
        self.client.size(cid).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn cache_release_does_not_call_kubo_unpin() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let client = ipfs::HttpClient::new(format!("http://{}", listener.local_addr().unwrap()));
        let pinner = RetainedPinner::new(client);
        let cid = "bafkreigh2akiscaildcw453gm7nmiv5df6b5u3rtmw3klhyxqqzkjureme"
            .parse()
            .unwrap();
        pinner.unpin(&cid).await.unwrap();
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(20), listener.accept())
                .await
                .is_err()
        );
    }
}
