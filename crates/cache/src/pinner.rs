use anyhow::Result;
use async_trait::async_trait;
use cid::Cid;
use std::{
    any::Any,
    future::Future,
    path::Path,
    pin::Pin,
    task::{Context, Poll},
    time::Duration,
};
use tokio::io::AsyncWriteExt;

/// A path writer failed before it successfully created its destination.
///
/// Callers that stage writes use this typed cause to avoid deleting a path
/// owned by another transaction.  The original failure remains available in
/// the [`anyhow::Error`] chain.
#[derive(Debug)]
pub struct PathWriteNotOwned {
    source: anyhow::Error,
}

impl PathWriteNotOwned {
    pub fn new(source: impl Into<anyhow::Error>) -> Self {
        Self {
            source: source.into(),
        }
    }
}

impl std::fmt::Display for PathWriteNotOwned {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            formatter,
            "path writer did not create the destination: {}",
            self.source
        )
    }
}

impl std::error::Error for PathWriteNotOwned {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(self.source.as_ref())
    }
}

/// A path writer caught a panic while producing bytes for its local sink.
///
/// Filesystem transactions use this cause to record a scope failure before
/// rolling back the destination they own.
#[derive(Debug)]
pub struct PathWriteSourcePanic {
    message: String,
}

impl PathWriteSourcePanic {
    pub fn new(payload: &(dyn Any + Send)) -> Self {
        let message = payload
            .downcast_ref::<&str>()
            .map(|message| (*message).to_owned())
            .or_else(|| payload.downcast_ref::<String>().cloned())
            .unwrap_or_else(|| "non-string panic payload".to_owned());
        Self { message }
    }
}

impl std::fmt::Display for PathWriteSourcePanic {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "path source panicked: {}", self.message)
    }
}

impl std::error::Error for PathWriteSourcePanic {}

struct CatchUnwind<F> {
    future: Pin<Box<F>>,
}

impl<F> CatchUnwind<F> {
    fn new(future: F) -> Self {
        Self {
            future: Box::pin(future),
        }
    }
}

impl<F: Future> Future for CatchUnwind<F> {
    type Output = std::thread::Result<F::Output>;

    fn poll(mut self: Pin<&mut Self>, context: &mut Context<'_>) -> Poll<Self::Output> {
        match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            self.future.as_mut().poll(context)
        })) {
            Ok(Poll::Ready(output)) => Poll::Ready(Ok(output)),
            Ok(Poll::Pending) => Poll::Pending,
            Err(payload) => Poll::Ready(Err(payload)),
        }
    }
}

async fn wait_for_file_ownership(mut file: tokio::fs::File) {
    let mut backoff = Duration::from_millis(1);
    loop {
        match file.try_into_std() {
            Ok(_) => return,
            Err(returned) => file = returned,
        }
        tokio::time::sleep(backoff).await;
        backoff = (backoff * 2).min(Duration::from_millis(32));
    }
}

async fn settle_failed_write(file: tokio::fs::File) {
    let mut file = match file.try_into_std() {
        Ok(_) => return,
        Err(file) => file,
    };

    if CatchUnwind::new(file.flush()).await.is_err() {
        wait_for_file_ownership(file).await;
    }
}

async fn write_fetched_bytes<F>(dst: &Path, fetch: F) -> Result<()>
where
    F: Future<Output = Result<Vec<u8>>>,
{
    if let Some(parent) = dst.parent() {
        tokio::fs::create_dir_all(parent)
            .await
            .map_err(|error| PathWriteNotOwned::new(anyhow::Error::from(error)))?;
    }
    let mut file = tokio::fs::OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(dst)
        .await
        .map_err(|error| PathWriteNotOwned::new(anyhow::Error::from(error)))?;

    let bytes = match CatchUnwind::new(fetch).await {
        Ok(Ok(bytes)) => bytes,
        Ok(Err(error)) => {
            settle_failed_write(file).await;
            return Err(error);
        }
        Err(payload) => {
            settle_failed_write(file).await;
            return Err(PathWriteSourcePanic::new(payload.as_ref()).into());
        }
    };

    match CatchUnwind::new(file.write_all(&bytes)).await {
        Ok(Ok(())) => {}
        Ok(Err(error)) => {
            settle_failed_write(file).await;
            return Err(error.into());
        }
        Err(payload) => {
            settle_failed_write(file).await;
            return Err(anyhow::anyhow!(
                "path sink panicked: {}",
                PathWriteSourcePanic::new(payload.as_ref())
            ));
        }
    }

    match CatchUnwind::new(file.flush()).await {
        Ok(result) => result.map_err(anyhow::Error::from),
        Err(payload) => {
            wait_for_file_ownership(file).await;
            Err(anyhow::anyhow!(
                "terminal path flush panicked: {}",
                PathWriteSourcePanic::new(payload.as_ref())
            ))
        }
    }
}

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

    /// Stream CID content directly into `dst` without materializing a full
    /// in-memory buffer.
    ///
    /// Implementations must exclusively create `dst`. Every error before that
    /// successful create must carry [`PathWriteNotOwned`] in its error chain;
    /// after creation, every error means the caller owns `dst`. A return,
    /// including an error return, must occur only after accepted local writes
    /// reach terminal state. This is local completion, not an `fsync` promise.
    ///
    /// The default implementation falls back to `fetch()` and terminally
    /// observed Tokio filesystem I/O.
    async fn fetch_to_path(&self, cid: &Cid, dst: &Path) -> Result<()> {
        write_fetched_bytes(dst, self.fetch(cid)).await
    }

    /// Stream CID subpath content directly into `dst`.
    ///
    /// The same exclusive-create, ownership, and terminal-completion contract
    /// as [`Pinner::fetch_to_path`] applies.
    async fn fetch_path_to_path(&self, cid: &Cid, subpath: &str, dst: &Path) -> Result<()> {
        write_fetched_bytes(dst, self.fetch_path(cid, subpath)).await
    }

    /// Get the size in bytes of the content addressed by a CID,
    /// without fetching the full content.
    async fn size(&self, cid: &Cid) -> Result<u64>;
}

#[cfg(test)]
mod tests {
    use super::{PathWriteNotOwned, Pinner};
    use anyhow::Result;
    use async_trait::async_trait;
    use cid::Cid;
    struct DefaultPathPinner {
        bytes: Option<Vec<u8>>,
        panic: bool,
    }

    #[async_trait]
    impl Pinner for DefaultPathPinner {
        async fn pin(&self, _cid: &Cid) -> Result<()> {
            Ok(())
        }

        async fn unpin(&self, _cid: &Cid) -> Result<()> {
            Ok(())
        }

        async fn fetch(&self, _cid: &Cid) -> Result<Vec<u8>> {
            if self.panic {
                panic!("default-source-panic");
            }
            self.bytes
                .clone()
                .ok_or_else(|| anyhow::anyhow!("source-failure"))
        }

        async fn size(&self, _cid: &Cid) -> Result<u64> {
            Ok(self.bytes.as_ref().map_or(0, Vec::len) as u64)
        }
    }

    fn cid() -> Cid {
        "bafkreigh2akiscaildcw453gm7nmiv5df6b5u3rtmw3klhyxqqzkjureme"
            .parse()
            .unwrap()
    }

    #[tokio::test]
    async fn default_path_writer_exclusively_creates_and_flushes() {
        let directory = tempfile::tempdir().unwrap();
        let destination = directory.path().join("materialized");
        let pinner = DefaultPathPinner {
            bytes: Some(b"terminal-bytes".to_vec()),
            panic: false,
        };

        pinner.fetch_to_path(&cid(), &destination).await.unwrap();

        assert_eq!(std::fs::read(destination).unwrap(), b"terminal-bytes");
    }

    #[tokio::test]
    async fn default_path_writer_marks_precreate_errors_and_preserves_existing_path() {
        let directory = tempfile::tempdir().unwrap();
        let destination = directory.path().join("materialized");
        std::fs::write(&destination, b"another-owner").unwrap();
        let pinner = DefaultPathPinner {
            bytes: Some(b"replacement".to_vec()),
            panic: false,
        };

        let error = pinner
            .fetch_to_path(&cid(), &destination)
            .await
            .unwrap_err();

        assert!(error.chain().any(|cause| cause.is::<PathWriteNotOwned>()));
        assert_eq!(std::fs::read(destination).unwrap(), b"another-owner");
    }

    #[tokio::test]
    async fn default_path_writer_owns_destination_before_source_failure() {
        let directory = tempfile::tempdir().unwrap();
        let destination = directory.path().join("materialized");
        let pinner = DefaultPathPinner {
            bytes: None,
            panic: false,
        };

        let error = pinner
            .fetch_to_path(&cid(), &destination)
            .await
            .unwrap_err();

        assert!(!error.chain().any(|cause| cause.is::<PathWriteNotOwned>()));
        assert_eq!(error.to_string(), "source-failure");
        assert!(destination.exists());
    }

    #[tokio::test]
    async fn default_path_writer_catches_source_panic_after_taking_ownership() {
        let directory = tempfile::tempdir().unwrap();
        let destination = directory.path().join("materialized");
        let pinner = DefaultPathPinner {
            bytes: None,
            panic: true,
        };

        let error = pinner
            .fetch_to_path(&cid(), &destination)
            .await
            .unwrap_err();

        assert!(error
            .chain()
            .any(|cause| cause.is::<super::PathWriteSourcePanic>()));
        assert!(destination.exists());
    }
}
