//! IPFS client API for interacting with IPFS nodes.
//!
//! Provides a thin HTTP wrapper around Kubo's `/api/v0/*` endpoints. This is
//! an internal helper — guests do not receive an IPFS capability. Content
//! is served to guests through the WASI virtual filesystem (`CidTree`).

use std::{
    future::Future,
    io,
    path::Path,
    pin::Pin,
    sync::Arc,
    task::{Context as TaskContext, Poll},
    time::{Duration, Instant},
};

use anyhow::{Context, Result};
use async_trait::async_trait;
use cache::{PathWriteNotOwned, PathWriteSourcePanic};
use tokio::io::{AsyncWrite, AsyncWriteExt};

pub mod cid_identity;
mod composed;

pub use composed::{parse_kubo_dag_import_response, KuboResponseLimit};

// A failed TCP connection must not hold an IPFS operation indefinitely.
const KUBO_CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
// `/api/v0/id` is the readiness probe: it should be small and local. Bound
// that probe separately from content and DHT operations, which may legitimately
// transfer or resolve for much longer than a readiness interval.
const KUBO_INFO_TIMEOUT: Duration = Duration::from_secs(30);

type RetryObserver = Arc<dyn Fn(&str, u64) + Send + Sync>;

/// HTTP client for talking to a Kubo node's `/api/v0/*` endpoints.
#[derive(Clone)]
pub struct HttpClient {
    pub(crate) http_client: reqwest::Client,
    pub(crate) base_url: String,
}

enum PumpFailure {
    Source(anyhow::Error),
    Sink(anyhow::Error),
    Panic(PathWriteSourcePanic),
}

impl PumpFailure {
    fn into_error(self) -> anyhow::Error {
        match self {
            Self::Source(error) | Self::Sink(error) => error,
            Self::Panic(error) => error.into(),
        }
    }
}

#[async_trait]
trait LocalFilePump: Send {
    async fn pump(self, file: &mut tokio::fs::File) -> std::result::Result<u64, PumpFailure>;
}

struct HttpFilePump<'a> {
    client: &'a HttpClient,
    path: &'a str,
}

#[async_trait]
impl LocalFilePump for HttpFilePump<'_> {
    async fn pump(self, file: &mut tokio::fs::File) -> std::result::Result<u64, PumpFailure> {
        let mut response = self.client.cat_response(self.path).await.map_err(|error| {
            PumpFailure::Source(error.context(format!(
                "Failed to start streaming IPFS content from {}",
                self.path
            )))
        })?;
        let mut total = 0u64;
        loop {
            let chunk = response
                .chunk()
                .await
                .with_context(|| format!("Failed to read streaming chunk from {}", self.path))
                .map_err(PumpFailure::Source)?;
            let Some(chunk) = chunk else {
                return Ok(total);
            };
            file.write_all(&chunk)
                .await
                .with_context(|| format!("Failed writing streaming chunk from {}", self.path))
                .map_err(PumpFailure::Sink)?;
            total += chunk.len() as u64;
        }
    }
}

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

    fn poll(mut self: Pin<&mut Self>, context: &mut TaskContext<'_>) -> Poll<Self::Output> {
        match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            self.future.as_mut().poll(context)
        })) {
            Ok(Poll::Ready(output)) => Poll::Ready(Ok(output)),
            Ok(Poll::Pending) => Poll::Pending,
            Err(payload) => Poll::Ready(Err(payload)),
        }
    }
}

#[async_trait]
trait FileTerminal: Send {
    fn try_into_std(
        &mut self,
        file: tokio::fs::File,
    ) -> std::result::Result<std::fs::File, tokio::fs::File>;

    async fn flush(&mut self, file: &mut tokio::fs::File) -> io::Result<()>;
}

struct TokioFileTerminal;

#[async_trait]
impl FileTerminal for TokioFileTerminal {
    fn try_into_std(
        &mut self,
        file: tokio::fs::File,
    ) -> std::result::Result<std::fs::File, tokio::fs::File> {
        file.try_into_std()
    }

    async fn flush(&mut self, file: &mut tokio::fs::File) -> io::Result<()> {
        file.flush().await
    }
}

async fn wait_for_file_ownership<T: FileTerminal>(mut file: tokio::fs::File, terminal: &mut T) {
    let mut backoff = Duration::from_millis(1);
    loop {
        match terminal.try_into_std(file) {
            Ok(_) => return,
            Err(returned) => file = returned,
        }
        tokio::time::sleep(backoff).await;
        backoff = (backoff * 2).min(Duration::from_millis(32));
    }
}

async fn settle_failed_local_write<T: FileTerminal>(file: tokio::fs::File, terminal: &mut T) {
    let mut file = match terminal.try_into_std(file) {
        Ok(_) => return,
        Err(file) => file,
    };

    if CatchUnwind::new(terminal.flush(&mut file)).await.is_err() {
        wait_for_file_ownership(file, terminal).await;
    }
}

async fn finish_local_file_write<P, T>(
    mut file: tokio::fs::File,
    pump: P,
    terminal: &mut T,
) -> Result<()>
where
    P: LocalFilePump,
    T: FileTerminal,
{
    let pump_result = match CatchUnwind::new(pump.pump(&mut file)).await {
        Ok(result) => result,
        Err(payload) => Err(PumpFailure::Panic(PathWriteSourcePanic::new(
            payload.as_ref(),
        ))),
    };

    match pump_result {
        Ok(_) => match CatchUnwind::new(terminal.flush(&mut file)).await {
            Ok(Ok(())) => Ok(()),
            Ok(Err(error)) => Err(PumpFailure::Sink(error.into()).into_error()),
            Err(payload) => {
                wait_for_file_ownership(file, terminal).await;
                Err(anyhow::anyhow!(
                    "terminal path flush panicked: {}",
                    PathWriteSourcePanic::new(payload.as_ref())
                ))
            }
        },
        Err(primary) => {
            settle_failed_local_write(file, terminal).await;
            Err(primary.into_error())
        }
    }
}

async fn open_exclusive_write_only(dst: &Path) -> Result<tokio::fs::File> {
    if let Some(parent) = dst.parent() {
        tokio::fs::create_dir_all(parent)
            .await
            .map_err(|error| PathWriteNotOwned::new(anyhow::Error::from(error)))?;
    }
    tokio::fs::OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(dst)
        .await
        .map_err(|error| PathWriteNotOwned::new(anyhow::Error::from(error)).into())
}

/// A non-success response from Kubo's HTTP API.
///
/// Keeping the status in the error chain lets boot retry policy distinguish a
/// transient Kubo/server failure from a bad caller request without parsing
/// diagnostic prose.
#[derive(Debug)]
pub struct KuboApiError {
    operation: String,
    status: reqwest::StatusCode,
    message: String,
}

impl KuboApiError {
    async fn from_response(operation: impl Into<String>, response: reqwest::Response) -> Self {
        let status = response.status();
        let message = response.text().await.unwrap_or_default();
        Self {
            operation: operation.into(),
            status,
            message,
        }
    }

    /// Kubo uses 500 for ordinary caller/content errors, so only rate limiting
    /// and gateway failures are safe to retry without inspecting unstable
    /// diagnostic prose.
    pub fn is_retryable(&self) -> bool {
        matches!(
            self.status,
            reqwest::StatusCode::TOO_MANY_REQUESTS
                | reqwest::StatusCode::BAD_GATEWAY
                | reqwest::StatusCode::SERVICE_UNAVAILABLE
                | reqwest::StatusCode::GATEWAY_TIMEOUT
        )
    }

    /// Whether Kubo returned a server-side HTTP failure.
    ///
    /// Epoch preparation uses this typed status predicate. It must not inspect
    /// Kubo's diagnostic message because that text is not a stable API.
    pub fn is_server_error(&self) -> bool {
        self.status.is_server_error()
    }
}

impl std::fmt::Display for KuboApiError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if self.message.is_empty() {
            write!(formatter, "{} failed with {}", self.operation, self.status)
        } else {
            write!(
                formatter,
                "{} failed with {}: {}",
                self.operation, self.status, self.message
            )
        }
    }
}

impl std::error::Error for KuboApiError {}

/// A boot-operation watchdog elapsed before Kubo made progress.
#[derive(Debug)]
pub struct KuboOperationTimeout {
    operation: String,
    timeout: Duration,
}

impl KuboOperationTimeout {
    pub fn new(operation: impl Into<String>, timeout: Duration) -> Self {
        Self {
            operation: operation.into(),
            timeout,
        }
    }
}

impl std::fmt::Display for KuboOperationTimeout {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            formatter,
            "{} made no progress within {}s",
            self.operation,
            self.timeout.as_secs_f64()
        )
    }
}

impl std::error::Error for KuboOperationTimeout {}

/// The finite retry budget for a Kubo boot operation elapsed while Kubo
/// remained unavailable.
#[derive(Debug)]
pub struct KuboRetryBudgetExhausted {
    operation: String,
    elapsed: Duration,
}

impl KuboRetryBudgetExhausted {
    fn new(operation: impl Into<String>, elapsed: Duration) -> Self {
        Self {
            operation: operation.into(),
            elapsed,
        }
    }
}

impl std::fmt::Display for KuboRetryBudgetExhausted {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            formatter,
            "Kubo boot operation {} did not complete after {}s",
            self.operation,
            self.elapsed.as_secs()
        )
    }
}

impl std::error::Error for KuboRetryBudgetExhausted {}

/// Whether an IPFS error is safe to retry during critical boot resolution.
/// Transport failures, 429, and gateway failures retry; local filesystem and
/// ordinary Kubo API errors fail immediately.
pub fn is_retryable_kubo_error(error: &anyhow::Error) -> bool {
    error.chain().any(|cause| {
        cause
            .downcast_ref::<reqwest::Error>()
            .is_some_and(|error| error.is_connect() || error.is_timeout() || error.is_request())
            || cause
                .downcast_ref::<KuboApiError>()
                .is_some_and(KuboApiError::is_retryable)
            || cause.is::<KuboOperationTimeout>()
    })
}

/// Whether a failed boot operation represents Kubo unavailability rather than
/// a caller or protocol fault. Callers may use an already-validated offline
/// fallback only for this class of failure.
pub fn is_kubo_unavailable_error(error: &anyhow::Error) -> bool {
    is_retryable_kubo_error(error)
        || error.chain().any(|cause| {
            cause.is::<KuboOperationTimeout>() || cause.is::<KuboRetryBudgetExhausted>()
        })
}

/// Retry policy used only while the host is resolving its initial boot image.
/// The ordinary [`HttpClient`] remains deadline-free for content and DHT work
/// after startup.
#[derive(Clone)]
pub struct BootClient {
    client: HttpClient,
    retry_deadline: Option<Duration>,
    operation_timeout: Option<Duration>,
    retry_observer: Option<RetryObserver>,
    retry_transient: bool,
}

impl BootClient {
    /// `retry_max_secs=0` retries transient failures indefinitely. Likewise,
    /// `operation_timeout_secs=0` disables the per-request no-progress
    /// watchdog rather than silently selecting an aggressive timeout.
    pub fn new(client: HttpClient, retry_max_secs: u64, operation_timeout_secs: u64) -> Self {
        Self {
            client,
            retry_deadline: (retry_max_secs > 0).then(|| Duration::from_secs(retry_max_secs)),
            operation_timeout: (operation_timeout_secs > 0)
                .then(|| Duration::from_secs(operation_timeout_secs)),
            retry_observer: None,
            retry_transient: true,
        }
    }

    /// Create a bounded client that returns the original typed error after one
    /// attempt. Deployment owns retries and classifies that error.
    pub fn one_attempt(client: HttpClient, operation_timeout: Duration) -> Self {
        Self {
            client,
            retry_deadline: None,
            operation_timeout: Some(operation_timeout),
            retry_observer: None,
            retry_transient: false,
        }
    }

    pub fn client(&self) -> &HttpClient {
        &self.client
    }

    pub fn operation_timeout(&self) -> Option<Duration> {
        self.operation_timeout
    }

    /// Maximum cumulative retry duration for one boot operation, if finite.
    pub fn retry_max_duration(&self) -> Option<Duration> {
        self.retry_deadline
    }

    /// Bound one request by the stricter of its watchdog and finite retry
    /// budget. The caller can retry a larger composition operation.
    fn bounded_timeout(&self) -> Option<Duration> {
        match (self.operation_timeout, self.retry_deadline) {
            (Some(watchdog), Some(budget)) => Some(watchdog.min(budget)),
            (Some(watchdog), None) => Some(watchdog),
            (None, Some(budget)) => Some(budget),
            (None, None) => None,
        }
    }

    /// Attach boot-status reporting without coupling this crate to the host's
    /// metrics implementation.
    pub fn with_retry_observer(mut self, observer: RetryObserver) -> Self {
        self.retry_observer = Some(observer);
        self
    }

    /// Record a retry of a composite boot operation in the configured observer.
    pub fn report_retry(&self, operation_name: &str, attempt: u64) {
        if let Some(observer) = &self.retry_observer {
            observer(operation_name, attempt);
        }
    }

    async fn retry<T, F, Fut>(&self, operation_name: &str, mut operation: F) -> Result<T>
    where
        F: FnMut() -> Fut,
        Fut: Future<Output = Result<T>>,
    {
        let started = Instant::now();
        let deadline = self.retry_deadline.map(|duration| started + duration);
        let mut backoff = Duration::from_millis(500);
        let backoff_cap = Duration::from_secs(15);
        let mut attempt = 0u64;

        loop {
            attempt += 1;
            let remaining =
                deadline.map(|deadline| deadline.saturating_duration_since(Instant::now()));
            if remaining.is_some_and(|remaining| remaining.is_zero()) {
                return Err(
                    KuboRetryBudgetExhausted::new(operation_name, started.elapsed()).into(),
                );
            }

            let attempt_timeout = match (self.bounded_timeout(), remaining) {
                (Some(timeout), Some(remaining)) => Some(timeout.min(remaining)),
                (Some(timeout), None) => Some(timeout),
                (None, Some(remaining)) => Some(remaining),
                (None, None) => None,
            };
            let retry_reason = match attempt_timeout {
                Some(timeout) => match tokio::time::timeout(timeout, operation()).await {
                    Ok(Ok(value)) => return Ok(value),
                    Ok(Err(error)) if !self.retry_transient => return Err(error),
                    Ok(Err(error)) if is_retryable_kubo_error(&error) => {
                        format!("transient Kubo failure: {error:#}")
                    }
                    Ok(Err(error)) => return Err(error),
                    Err(_) if !self.retry_transient => {
                        return Err(KuboOperationTimeout::new(operation_name, timeout).into())
                    }
                    Err(_) => format!("made no progress within {}s", timeout.as_secs_f64()),
                },
                None => match operation().await {
                    Ok(value) => return Ok(value),
                    Err(error) if !self.retry_transient => return Err(error),
                    Err(error) if is_retryable_kubo_error(&error) => {
                        format!("transient Kubo failure: {error:#}")
                    }
                    Err(error) => return Err(error),
                },
            };

            tracing::warn!(
                operation = operation_name,
                attempt,
                reason = %retry_reason,
                "Kubo boot operation will retry"
            );
            self.report_retry(operation_name, attempt);

            if let Some(remaining) =
                deadline.map(|deadline| deadline.saturating_duration_since(Instant::now()))
            {
                if remaining.is_zero() {
                    return Err(
                        KuboRetryBudgetExhausted::new(operation_name, started.elapsed()).into(),
                    );
                }
                tokio::time::sleep(backoff.min(remaining)).await;
            } else {
                tokio::time::sleep(backoff).await;
            }
            backoff = (backoff * 2).min(backoff_cap);
        }
    }

    /// Run one bounded pin attempt and return its typed failure to the caller.
    /// The caller decides whether the pin gates readiness or is best-effort.
    pub async fn pin_add_once(&self, cid: &str) -> Result<()> {
        match self.bounded_timeout() {
            Some(timeout) => tokio::time::timeout(timeout, self.client.pin_add(cid))
                .await
                .map_err(|_| KuboOperationTimeout::new("IPFS pin add", timeout))?,
            None => self.client.pin_add(cid).await,
        }
    }

    pub async fn name_resolve(&self, name: &str) -> Result<String> {
        self.retry("IPNS mount resolution", || self.client.name_resolve(name))
            .await
    }

    pub async fn resolve(&self, path: &str) -> Result<String> {
        self.retry("IPFS subpath resolution", || self.client.resolve(path))
            .await
    }

    pub async fn add_dir(&self, directory: &Path) -> Result<String> {
        self.retry("local mount upload", || self.client.add_dir(directory))
            .await
    }

    pub async fn ls(&self, path: &str) -> Result<Vec<LsEntry>> {
        self.retry("IPFS directory listing", || self.client.ls(path))
            .await
    }
}

impl HttpClient {
    /// Create a new IPFS client with the given HTTP API endpoint URL.
    pub fn new(ipfs_url: String) -> Self {
        let http_client = reqwest::Client::builder()
            .connect_timeout(KUBO_CONNECT_TIMEOUT)
            .build()
            .expect("fixed Kubo HTTP client configuration must be valid");

        Self::with_http_client(ipfs_url, http_client)
    }

    fn with_http_client(ipfs_url: String, http_client: reqwest::Client) -> Self {
        Self {
            http_client,
            base_url: ipfs_url.trim_end_matches('/').to_string(),
        }
    }

    /// The Kubo HTTP API base URL, for diagnostics/logging.
    pub fn base_url(&self) -> &str {
        &self.base_url
    }

    async fn cat_response(&self, path: &str) -> Result<reqwest::Response> {
        let url = format!("{}/api/v0/cat?arg={}", self.base_url, path);
        let response = self
            .http_client
            .post(&url)
            .send()
            .await
            .with_context(|| format!("Failed to connect to IPFS node at {}", self.base_url))?;

        if !response.status().is_success() {
            return Err(
                KuboApiError::from_response(format!("IPFS cat (path: {path})"), response)
                    .await
                    .into(),
            );
        }

        Ok(response)
    }

    /// Fetch file content from an IPFS path (calls `/api/v0/cat`).
    ///
    /// Accepts IPFS-family paths: `/ipfs/<cid>`, `/ipns/...`, `/ipld/...`.
    pub async fn cat(&self, path: &str) -> Result<Vec<u8>> {
        let response = self.cat_response(path).await?;

        response
            .bytes()
            .await
            .with_context(|| format!("Failed to read IPFS content from {path}"))
            .map(|b| b.to_vec())
    }

    /// Stream file content from an IPFS path into an [`AsyncWrite`] sink.
    ///
    /// Returns the total number of bytes written.
    pub async fn cat_to_writer<W>(&self, path: &str, dst: &mut W) -> Result<u64>
    where
        W: AsyncWrite + Unpin,
    {
        let mut response = self.cat_response(path).await?;
        let mut total = 0u64;

        while let Some(chunk) = response
            .chunk()
            .await
            .with_context(|| format!("Failed to read streaming chunk from {path}"))?
        {
            dst.write_all(&chunk)
                .await
                .with_context(|| format!("Failed writing streaming chunk from {path}"))?;
            total += chunk.len() as u64;
        }

        dst.flush()
            .await
            .with_context(|| format!("Failed to flush streaming sink for {path}"))?;
        Ok(total)
    }

    /// Stream file content from an IPFS path directly to `dst`.
    ///
    /// Accepts IPFS-family paths: `/ipfs/<cid>`, `/ipns/...`, `/ipld/...`.
    /// Exclusively creates `dst` and returns only after every accepted local
    /// write reaches terminal state. This does not provide `fsync` durability.
    pub async fn cat_to_path(&self, path: &str, dst: &Path) -> Result<()> {
        let file = open_exclusive_write_only(dst).await?;
        let mut terminal = TokioFileTerminal;
        finish_local_file_write(file, HttpFilePump { client: self, path }, &mut terminal).await
    }

    /// Fetch an IPFS directory and extract it to a local path.
    ///
    /// Uses kubo's `/api/v0/get?arg=<path>&archive=true` which returns the
    /// directory contents as a TAR archive. The top-level CID directory in
    /// the TAR is stripped so files land directly under `dst`.
    pub async fn get_dir(&self, ipfs_path: &str, dst: &Path) -> Result<()> {
        let url = format!(
            "{}/api/v0/get?arg={}&archive=true",
            self.base_url, ipfs_path
        );
        let response =
            self.http_client.post(&url).send().await.with_context(|| {
                format!("Failed to fetch IPFS directory from {}", self.base_url)
            })?;

        if !response.status().is_success() {
            return Err(KuboApiError::from_response(
                format!("IPFS directory fetch (path: {ipfs_path})"),
                response,
            )
            .await
            .into());
        }

        let bytes = response
            .bytes()
            .await
            .with_context(|| format!("Failed to read IPFS archive from {ipfs_path}"))?;

        let cursor = std::io::Cursor::new(bytes);
        let mut archive = tar::Archive::new(cursor);

        // Kubo wraps the directory in a top-level entry named after the CID.
        // Strip that prefix so files land directly under dst.
        for entry in archive.entries()? {
            let mut entry = entry?;
            let path = entry.path()?.into_owned();

            let stripped: std::path::PathBuf = path.components().skip(1).collect();
            if stripped.as_os_str().is_empty() {
                continue;
            }

            let target = dst.join(&stripped);
            if entry.header().entry_type().is_dir() {
                std::fs::create_dir_all(&target)?;
            } else {
                if let Some(parent) = target.parent() {
                    std::fs::create_dir_all(parent)?;
                }
                let mut file = std::fs::File::create(&target)?;
                std::io::copy(&mut entry, &mut file)?;
            }
        }

        Ok(())
    }

    /// Resolve an IPNS name to a CID path.
    ///
    /// Calls `/api/v0/name/resolve` on the IPFS node. The name should be a
    /// peer ID or IPNS key name (e.g., "k51qzi5uqu5d..." or "self").
    ///
    /// Returns the resolved path (e.g., "/ipfs/QmHash...").
    pub async fn name_resolve(&self, name: &str) -> anyhow::Result<String> {
        let url = format!("{}/api/v0/name/resolve", self.base_url);
        let response = self
            .http_client
            .post(&url)
            .query(&[("arg", name), ("nocache", "true")])
            .send()
            .await
            .context("IPNS name resolve request failed")?;
        if !response.status().is_success() {
            return Err(KuboApiError::from_response("IPNS name resolve", response)
                .await
                .into());
        }
        let body: serde_json::Value = response
            .json()
            .await
            .context("Failed to parse name resolve response")?;
        body["Path"]
            .as_str()
            .map(|s| s.to_string())
            .ok_or_else(|| anyhow::anyhow!("name resolve response missing Path field"))
    }

    /// Resolve an IPFS path, including a subpath, to its canonical CID path.
    pub async fn resolve(&self, path: &str) -> anyhow::Result<String> {
        let url = format!("{}/api/v0/resolve", self.base_url);
        let response = self
            .http_client
            .post(&url)
            .query(&[("arg", path)])
            .send()
            .await
            .with_context(|| format!("Failed to resolve IPFS path {path}"))?;
        if !response.status().is_success() {
            return Err(KuboApiError::from_response(
                format!("IPFS resolve (path: {path})"),
                response,
            )
            .await
            .into());
        }
        let body: serde_json::Value = response
            .json()
            .await
            .with_context(|| format!("Failed to parse IPFS resolve response for {path}"))?;
        body["Path"]
            .as_str()
            .map(str::to_owned)
            .ok_or_else(|| anyhow::anyhow!("IPFS resolve response missing Path field"))
    }
}

#[cfg(test)]
mod tests {
    use std::{
        fs::OpenOptions as StdOpenOptions,
        io,
        path::Path,
        sync::{
            atomic::{AtomicUsize, Ordering},
            Arc,
        },
        time::{Duration, Instant},
    };

    use async_trait::async_trait;
    use cache::{PathWriteNotOwned, PathWriteSourcePanic};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;
    use tokio::sync::oneshot;

    use super::{
        finish_local_file_write, is_retryable_kubo_error, parse_kubo_ls_response,
        parse_kubo_mfs_ls_response, BootClient, FileTerminal, HttpClient, PumpFailure,
    };

    enum AcceptedPumpEnd {
        SourceError,
        SourcePanic,
    }

    struct AcceptedPump {
        accepted: Option<oneshot::Sender<()>>,
        end: AcceptedPumpEnd,
    }

    #[async_trait]
    impl super::LocalFilePump for AcceptedPump {
        async fn pump(
            mut self,
            file: &mut tokio::fs::File,
        ) -> std::result::Result<u64, PumpFailure> {
            file.write_all(b"accepted-local-write")
                .await
                .map_err(|error| PumpFailure::Sink(error.into()))?;
            self.accepted.take().unwrap().send(()).unwrap();
            match self.end {
                AcceptedPumpEnd::SourceError => {
                    Err(PumpFailure::Source(anyhow::anyhow!("source-primary")))
                }
                AcceptedPumpEnd::SourcePanic => panic!("source-panic"),
            }
        }
    }

    struct ProbeNotifier {
        busy: Option<oneshot::Sender<()>>,
    }

    #[async_trait]
    impl FileTerminal for ProbeNotifier {
        fn try_into_std(
            &mut self,
            file: tokio::fs::File,
        ) -> std::result::Result<std::fs::File, tokio::fs::File> {
            match file.try_into_std() {
                Ok(file) => Ok(file),
                Err(file) => {
                    if let Some(busy) = self.busy.take() {
                        busy.send(()).unwrap();
                    }
                    Err(file)
                }
            }
        }

        async fn flush(&mut self, file: &mut tokio::fs::File) -> io::Result<()> {
            file.flush().await
        }
    }

    enum ScriptedFlush {
        Ok,
        Error(&'static str),
        Panic,
    }

    struct ScriptedTerminal {
        busy_probes: usize,
        probes: Arc<AtomicUsize>,
        flushes: Arc<AtomicUsize>,
        flush: ScriptedFlush,
    }

    #[async_trait]
    impl FileTerminal for ScriptedTerminal {
        fn try_into_std(
            &mut self,
            file: tokio::fs::File,
        ) -> std::result::Result<std::fs::File, tokio::fs::File> {
            self.probes.fetch_add(1, Ordering::SeqCst);
            if self.busy_probes > 0 {
                self.busy_probes -= 1;
                return Err(file);
            }
            file.try_into_std()
        }

        async fn flush(&mut self, _file: &mut tokio::fs::File) -> io::Result<()> {
            let prior = self.flushes.fetch_add(1, Ordering::SeqCst);
            assert_eq!(prior, 0, "a consumed terminal completion was repolled");
            match self.flush {
                ScriptedFlush::Ok => Ok(()),
                ScriptedFlush::Error(message) => Err(io::Error::other(message)),
                ScriptedFlush::Panic => panic!("terminal-flush-panic"),
            }
        }
    }

    struct ImmediatePump(std::result::Result<u64, PumpFailure>);

    #[async_trait]
    impl super::LocalFilePump for ImmediatePump {
        async fn pump(self, _file: &mut tokio::fs::File) -> std::result::Result<u64, PumpFailure> {
            self.0
        }
    }

    fn fresh_exclusive_file(path: &Path) -> tokio::fs::File {
        tokio::fs::File::from_std(
            StdOpenOptions::new()
                .create_new(true)
                .write(true)
                .open(path)
                .unwrap(),
        )
    }

    fn run_accepted_write_failure(end: AcceptedPumpEnd) -> anyhow::Error {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("staging");
        let file = fresh_exclusive_file(&path);
        let runtime = tokio::runtime::Builder::new_current_thread()
            .max_blocking_threads(1)
            .enable_all()
            .build()
            .unwrap();

        runtime.block_on(async move {
            let (blocker_started_tx, blocker_started_rx) = oneshot::channel();
            let (release_blocker_tx, release_blocker_rx) = std::sync::mpsc::sync_channel(0);
            let blocker = tokio::task::spawn_blocking(move || {
                blocker_started_tx.send(()).unwrap();
                release_blocker_rx.recv().unwrap();
            });
            tokio::time::timeout(Duration::from_secs(5), blocker_started_rx)
                .await
                .unwrap()
                .unwrap();

            let (accepted_tx, accepted_rx) = oneshot::channel();
            let (busy_tx, busy_rx) = oneshot::channel();
            let transfer = tokio::spawn(async move {
                let mut terminal = ProbeNotifier {
                    busy: Some(busy_tx),
                };
                finish_local_file_write(
                    file,
                    AcceptedPump {
                        accepted: Some(accepted_tx),
                        end,
                    },
                    &mut terminal,
                )
                .await
            });

            tokio::time::timeout(Duration::from_secs(5), accepted_rx)
                .await
                .unwrap()
                .unwrap();
            tokio::time::timeout(Duration::from_secs(5), busy_rx)
                .await
                .unwrap()
                .unwrap();
            assert!(
                !transfer.is_finished(),
                "primary failure returned before the accepted write became terminal"
            );

            release_blocker_tx.send(()).unwrap();
            blocker.await.unwrap();
            let error = tokio::time::timeout(Duration::from_secs(5), transfer)
                .await
                .unwrap()
                .unwrap()
                .unwrap_err();
            assert_eq!(std::fs::read(path).unwrap(), b"accepted-local-write");
            error
        })
    }

    async fn read_request(stream: &mut tokio::net::TcpStream) -> String {
        let mut request = Vec::new();
        let mut chunk = [0u8; 4096];
        loop {
            let read = stream.read(&mut chunk).await.unwrap();
            request.extend_from_slice(&chunk[..read]);
            if read == 0 || request.windows(4).any(|window| window == b"\r\n\r\n") {
                return String::from_utf8(request).unwrap();
            }
        }
    }

    async fn client_with_json_response(
        body: impl Into<String>,
    ) -> (HttpClient, tokio::task::JoinHandle<()>) {
        let body = body.into();
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            read_request(&mut stream).await;
            stream
                .write_all(
                    format!(
                        "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                        body.len()
                    )
                    .as_bytes(),
                )
                .await
                .unwrap();
        });
        (HttpClient::new(format!("http://{address}")), server)
    }

    #[test]
    fn parse_kubo_ls_response_decodes_all_raw_entries() {
        let entries = parse_kubo_ls_response(
            br#"{"Objects":[{"Links":[{"Name":"first","Hash":"bafy-first","Size":1,"Type":2},{"Name":"second","Hash":"bafy-second","Size":2,"Type":1}]}]}"#,
        )
        .unwrap();

        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].name, "first");
        assert_eq!(entries[0].hash, "bafy-first");
        assert_eq!(entries[0].size, 1);
        assert_eq!(entries[0].entry_type, 2);
        assert_eq!(entries[1].name, "second");
        assert_eq!(entries[1].hash, "bafy-second");
        assert_eq!(entries[1].size, 2);
        assert_eq!(entries[1].entry_type, 1);
    }

    #[test]
    fn parse_kubo_ls_response_rejects_malformed_raw_entry_atomically() {
        let body = br#"{"Objects":[{"Links":[{"Name":"valid","Hash":"bafy-valid","Size":1,"Type":2},{"Name":"invalid","Hash":false,"Size":2,"Type":2}]}]}"#;

        assert!(parse_kubo_ls_response(body).is_err());
    }

    #[test]
    fn parse_kubo_mfs_ls_response_decodes_all_raw_entries() {
        let entries = parse_kubo_mfs_ls_response(
            br#"{"Entries":[{"Name":"first","Hash":"bafy-first","Size":1,"Type":0},{"Name":"second","Hash":"bafy-second","Size":2,"Type":1}]}"#,
        )
        .unwrap();

        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].name, "first");
        assert_eq!(entries[0].hash, "bafy-first");
        assert_eq!(entries[0].size, 1);
        assert_eq!(entries[0].entry_type, 0);
        assert_eq!(entries[1].name, "second");
        assert_eq!(entries[1].hash, "bafy-second");
        assert_eq!(entries[1].size, 2);
        assert_eq!(entries[1].entry_type, 1);
    }

    #[test]
    fn parse_kubo_mfs_ls_response_rejects_malformed_raw_entry_atomically() {
        let body = br#"{"Entries":[{"Name":"valid","Hash":"bafy-valid","Size":1,"Type":0},{"Name":"invalid","Hash":false,"Size":2,"Type":0}]}"#;

        assert!(parse_kubo_mfs_ls_response(body).is_err());
    }

    #[tokio::test]
    async fn ls_accepts_valid_populated_and_empty_listings() {
        let (client, server) = client_with_json_response(
            r#"{"Objects":[{"Hash":"bafy-dir","Links":[{"Name":"bin","Hash":"bafy-child","Size":42,"Type":1}]}]}"#,
        )
        .await;
        let entries = client.ls("/ipfs/bafy-dir").await.unwrap();
        server.await.unwrap();

        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].name, "bin");
        assert_eq!(entries[0].hash, "bafy-child");
        assert_eq!(entries[0].size, 42);
        assert_eq!(entries[0].entry_type, 1);

        let (client, server) =
            client_with_json_response(r#"{"Objects":[{"Hash":"bafy-empty","Links":[]}]}"#).await;
        assert!(client.ls("/ipfs/bafy-empty").await.unwrap().is_empty());
        server.await.unwrap();
    }

    #[tokio::test]
    async fn ls_rejects_malformed_envelopes() {
        for body in [
            r#"[]"#,
            r#"{}"#,
            r#"{"Objects":"not-an-array"}"#,
            r#"{"Objects":[]}"#,
            r#"{"Objects":[42]}"#,
            r#"{"Objects":[{}]}"#,
            r#"{"Objects":[{"Links":"not-an-array"}]}"#,
        ] {
            let (client, server) = client_with_json_response(body).await;
            let error = client.ls("/ipfs/bafy-dir").await.unwrap_err();
            server.await.unwrap();
            assert!(
                error
                    .to_string()
                    .contains("Malformed IPFS ls response for /ipfs/bafy-dir"),
                "unexpected error for {body}: {error:#}"
            );
        }
    }

    #[tokio::test]
    async fn ls_rejects_a_malformed_link_entry() {
        let body = r#"{"Objects":[{"Hash":"bafy-dir","Links":[{"Name":"valid","Hash":"bafy-valid","Size":1,"Type":2},{"Name":42,"Hash":"bafy-invalid","Size":2,"Type":2}]}]}"#;
        let (client, server) = client_with_json_response(body).await;
        let error = client.ls("/ipfs/bafy-dir").await.unwrap_err();
        server.await.unwrap();

        assert!(
            error
                .to_string()
                .contains("Malformed IPFS ls directory entry at Objects[0].Links[1]"),
            "unexpected error: {error:#}"
        );
    }

    #[tokio::test]
    async fn ls_rejects_positional_object_representations() {
        for body in [
            r#"[[{"Links":[]}]]"#,
            r#"{"Objects":[[[]]]}"#,
            r#"{"Objects":[{"Links":[["file","bafy-file",1,2]]}]}"#,
        ] {
            let (client, server) = client_with_json_response(body).await;
            let error = client.ls("/ipfs/bafy-dir").await.unwrap_err();
            server.await.unwrap();
            assert!(
                error.to_string().contains("Malformed IPFS ls"),
                "unexpected error for {body}: {error:#}"
            );
        }
    }

    #[tokio::test]
    async fn ls_rejects_duplicate_recognized_entry_fields() {
        for (body, expected_error) in [
            (
                r#"{"Objects":[{"Links":[{"Name":"file","Hash":false,"Hash":"bafy-file","Size":1,"Type":2}]}]}"#,
                "Malformed IPFS ls directory entry at Objects[0].Links[0]",
            ),
            (
                r#"{"Objects":[{"Links":[{"Name":"file","Hash":"bafy-first","Hash":"bafy-second","Size":1,"Type":2}]}]}"#,
                "duplicate field `Hash`",
            ),
        ] {
            let (client, server) = client_with_json_response(body).await;
            let error = client.ls("/ipfs/bafy-dir").await.unwrap_err();
            server.await.unwrap();

            assert!(
                error.to_string().contains(expected_error),
                "unexpected error: {error:#}"
            );
        }
    }

    #[tokio::test]
    async fn files_ls_accepts_valid_populated_and_empty_listings() {
        let (client, server) = client_with_json_response(
            r#"{"Entries":[{"Name":"file","Hash":"bafy-file","Size":7,"Type":0}]}"#,
        )
        .await;
        let entries = client.mfs().files_ls("/workspace").await.unwrap();
        server.await.unwrap();

        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].name, "file");
        assert_eq!(entries[0].hash, "bafy-file");
        assert_eq!(entries[0].size, 7);
        assert_eq!(entries[0].entry_type, 0);

        for body in [r#"{"Entries":[]}"#, r#"{"Entries":null}"#] {
            let (client, server) = client_with_json_response(body).await;
            assert!(client
                .mfs()
                .files_ls("/workspace")
                .await
                .unwrap()
                .is_empty());
            server.await.unwrap();
        }
    }

    #[tokio::test]
    async fn files_ls_rejects_malformed_envelopes() {
        for body in [r#"[]"#, r#"{}"#, r#"{"Entries":"not-an-array"}"#] {
            let (client, server) = client_with_json_response(body).await;
            let error = client.mfs().files_ls("/workspace").await.unwrap_err();
            server.await.unwrap();
            assert!(
                error
                    .to_string()
                    .contains("Malformed MFS ls response for /workspace"),
                "unexpected error for {body}: {error:#}"
            );
        }
    }

    #[tokio::test]
    async fn files_ls_rejects_malformed_entries_atomically() {
        for (body, expected_index) in [
            (
                r#"{"Entries":[{"Name":"invalid","Hash":false,"Size":2,"Type":0}]}"#,
                0,
            ),
            (
                r#"{"Entries":[{"Name":"valid","Hash":"bafy-valid","Size":1,"Type":0},{"Name":"invalid","Hash":false,"Size":2,"Type":0}]}"#,
                1,
            ),
        ] {
            let (client, server) = client_with_json_response(body).await;
            let error = client.mfs().files_ls("/workspace").await.unwrap_err();
            server.await.unwrap();

            assert!(
                error.to_string().contains(&format!(
                    "Malformed MFS directory entry at Entries[{expected_index}]"
                )),
                "unexpected error: {error:#}"
            );
        }
    }

    #[tokio::test]
    async fn files_ls_rejects_positional_object_representations_atomically() {
        for body in [
            r#"[null]"#,
            r#"{"Entries":[["file","bafy-file",1,0]]}"#,
            r#"{"Entries":[{"Name":"valid","Hash":"bafy-valid","Size":1,"Type":0},["invalid","bafy-invalid",2,0]]}"#,
        ] {
            let (client, server) = client_with_json_response(body).await;
            let error = client.mfs().files_ls("/workspace").await.unwrap_err();
            server.await.unwrap();
            assert!(
                error.to_string().contains("Malformed MFS"),
                "unexpected error for {body}: {error:#}"
            );
        }
    }

    #[tokio::test]
    async fn files_ls_rejects_duplicate_recognized_entry_fields() {
        for (body, expected_error) in [
            (
                r#"{"Entries":[{"Name":"file","Hash":false,"Hash":"bafy-file","Size":1,"Type":0}]}"#,
                "Malformed MFS directory entry at Entries[0]",
            ),
            (
                r#"{"Entries":[{"Name":"file","Hash":"bafy-first","Hash":"bafy-second","Size":1,"Type":0}]}"#,
                "duplicate field `Hash`",
            ),
        ] {
            let (client, server) = client_with_json_response(body).await;
            let error = client.mfs().files_ls("/workspace").await.unwrap_err();
            server.await.unwrap();

            assert!(
                error.to_string().contains(expected_error),
                "unexpected error: {error:#}"
            );
        }
    }

    #[tokio::test]
    async fn image_merge_requests_preserve_arguments_and_flags() {
        // These values must remain data after Kubo decodes the HTTP query.
        let paths = [
            "/workspace/name?arg=/escape&parents=false&recursive=false&hash=false#fragment",
            "/workspace/space + percent%2F equals= ampersand&",
            "/workspace/café-e\u{301}-日本語",
            "/workspace/control\0\r\n\t",
        ];
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let mut requests = Vec::new();
            for _ in 0..paths.len() * 12 {
                let (mut stream, _) = listener.accept().await.unwrap();
                let request = read_request(&mut stream).await;
                let target = request.split_whitespace().nth(1).unwrap();
                let url = reqwest::Url::parse(&format!("http://localhost{target}")).unwrap();
                requests.push((
                    url.path().to_string(),
                    url.query_pairs().into_owned().collect::<Vec<_>>(),
                ));
                let body = r#"{"Path":"/ipfs/resolved","Objects":[{"Links":[]}],"Entries":[],"Hash":"hash","Size":0,"Type":"directory"}"#;
                stream
                    .write_all(
                        format!("HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len())
                            .as_bytes(),
                    )
                    .await
                    .unwrap();
            }
            requests
        });

        let client = HttpClient::new(format!("http://{address}"));
        let mfs = client.mfs();
        let mut expected = Vec::new();
        for path in paths {
            let arg = ("arg".to_string(), path.to_string());
            for flag in [false, true] {
                mfs.files_mkdir(path, flag).await.unwrap();
                expected.push((
                    "/api/v0/files/mkdir".to_string(),
                    vec![arg.clone(), ("parents".to_string(), flag.to_string())],
                ));
                mfs.files_stat(path, flag).await.unwrap();
                expected.push((
                    "/api/v0/files/stat".to_string(),
                    vec![arg.clone(), ("hash".to_string(), flag.to_string())],
                ));
                mfs.files_rm(path, flag).await.unwrap();
                expected.push((
                    "/api/v0/files/rm".to_string(),
                    vec![arg.clone(), ("recursive".to_string(), flag.to_string())],
                ));
            }
            let destination = format!("{path}-destination&arg=/injected");
            mfs.files_cp(path, &destination).await.unwrap();
            expected.push((
                "/api/v0/files/cp".to_string(),
                vec![arg.clone(), ("arg".to_string(), destination)],
            ));
            mfs.files_ls(path).await.unwrap();
            expected.push((
                "/api/v0/files/ls".to_string(),
                vec![arg.clone(), ("long".to_string(), "true".to_string())],
            ));
            client.ls(path).await.unwrap();
            expected.push(("/api/v0/ls".to_string(), vec![arg.clone()]));
            client.resolve(path).await.unwrap();
            expected.push(("/api/v0/resolve".to_string(), vec![arg.clone()]));
            client.name_resolve(path).await.unwrap();
            expected.push((
                "/api/v0/name/resolve".to_string(),
                vec![arg.clone(), ("nocache".to_string(), "true".to_string())],
            ));
            client.pin_add(path).await.unwrap();
            expected.push(("/api/v0/pin/add".to_string(), vec![arg]));
        }
        assert_eq!(server.await.unwrap(), expected);
    }

    #[tokio::test]
    async fn kubo_info_times_out_after_connection_without_response() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (_connection, _) = listener.accept().await.unwrap();
            std::future::pending::<()>().await;
        });

        let client = HttpClient::new(format!("http://{address}"));

        let started = Instant::now();
        let error = client
            .kubo_info_with_timeout(Duration::from_millis(100))
            .await
            .expect_err("a Kubo connection that never responds must time out");

        assert!(
            started.elapsed() >= Duration::from_millis(75),
            "request failed before the readiness timeout elapsed: {error:#}"
        );
        assert!(
            started.elapsed() < Duration::from_secs(1),
            "request timeout was not enforced promptly: {error:#}"
        );
        assert!(
            error.to_string().contains("kubo id timed out"),
            "expected the readiness timeout error, got: {error:#}"
        );
        server.abort();
    }

    #[tokio::test]
    async fn boot_client_retries_gateway_failure_then_recovers() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut first, _) = listener.accept().await.unwrap();
            read_request(&mut first).await;
            first
                .write_all(b"HTTP/1.1 503 Service Unavailable\r\nContent-Length: 18\r\nConnection: close\r\n\r\n{\"Message\":\"busy\"}")
                .await
                .unwrap();

            let (mut second, _) = listener.accept().await.unwrap();
            read_request(&mut second).await;
            second
                .write_all(
                    b"HTTP/1.1 200 OK\r\nContent-Length: 26\r\nConnection: close\r\n\r\n{\"Path\":\"/ipfs/recovered\"}",
                )
                .await
                .unwrap();
        });

        let client = BootClient::new(HttpClient::new(format!("http://{address}")), 30, 5);
        assert_eq!(
            client.name_resolve("k51-test").await.unwrap(),
            "/ipfs/recovered"
        );
        server.await.unwrap();
    }

    #[tokio::test]
    async fn dropped_inflight_request_is_retryable() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            read_request(&mut stream).await;
            // Dropping an accepted connection before response headers models a
            // Kubo restart after the client has already connected.
            drop(stream);
        });

        let client = HttpClient::new(format!("http://{address}"));
        let error = client.name_resolve("k51-test").await.unwrap_err();
        assert!(
            is_retryable_kubo_error(&error),
            "an in-flight transport reset must retry: {error:#}"
        );
        server.await.unwrap();
    }

    #[tokio::test]
    async fn kubo_429_is_retryable() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            read_request(&mut stream).await;
            stream
                .write_all(b"HTTP/1.1 429 Too Many Requests\r\nContent-Length: 18\r\nConnection: close\r\n\r\n{\"Message\":\"busy\"}")
                .await
                .unwrap();
        });

        let client = HttpClient::new(format!("http://{address}"));
        let error = client.name_resolve("k51-test").await.unwrap_err();
        assert!(is_retryable_kubo_error(&error));
        server.await.unwrap();
    }

    #[tokio::test]
    async fn add_dir_rejects_malformed_root_acknowledgement() {
        for hash in [
            "not-a-cid".to_owned(),
            cid::multibase::encode(cid::multibase::Base::Base32Lower, [1, 0x55, 0, 1, b'x', 0]),
        ] {
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let address = listener.local_addr().unwrap();
            let server = tokio::spawn(async move {
                let (mut stream, _) = listener.accept().await.unwrap();
                let request = read_request(&mut stream).await;
                assert!(request.contains("/api/v0/add?"));
                let body = serde_json::json!({"Name":"", "Hash":hash, "Size":"0"}).to_string();
                stream.write_all(format!("HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).as_bytes()).await.unwrap();
            });
            let directory = tempfile::tempdir().unwrap();
            let client = HttpClient::new(format!("http://{address}"));
            let result = client.add_dir(directory.path()).await;
            assert!(
                result.is_err(),
                "accepted malformed upload root: {result:?}"
            );
            assert!(!is_retryable_kubo_error(&result.unwrap_err()));
            server.await.unwrap();
        }
    }

    #[tokio::test]
    async fn missing_local_directory_is_not_retryable() {
        let directory = tempfile::TempDir::new().unwrap();
        let missing = directory.path().join("does-not-exist");
        let client = HttpClient::new("http://127.0.0.1:1".into());
        let error = client.add_dir(&missing).await.unwrap_err();

        assert!(error.to_string().contains("Failed to read directory"));
        assert!(
            !is_retryable_kubo_error(&error),
            "local filesystem failures must not become boot retry loops: {error:#}"
        );
    }

    #[tokio::test]
    async fn kubo_500_is_terminal_even_with_a_json_error_body() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            read_request(&mut stream).await;
            stream
                .write_all(b"HTTP/1.1 500 Internal Server Error\r\nContent-Length: 25\r\nConnection: close\r\n\r\n{\"Message\":\"invalid CID\"}")
                .await
                .unwrap();
        });
        let client = HttpClient::new(format!("http://{address}"));
        let error = client.name_resolve("not-a-cid").await.unwrap_err();

        assert!(!is_retryable_kubo_error(&error));
        server.await.unwrap();
    }

    #[tokio::test]
    async fn non_json_success_response_is_terminal() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            read_request(&mut stream).await;
            stream
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 18\r\nConnection: close\r\n\r\n<html>error</html>")
                .await
                .unwrap();
        });
        let client = HttpClient::new(format!("http://{address}"));
        let error = client.name_resolve("k51-test").await.unwrap_err();

        assert!(!is_retryable_kubo_error(&error));
        server.await.unwrap();
    }

    #[tokio::test]
    async fn finite_retry_budget_bounds_attempt_when_watchdog_is_disabled() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            read_request(&mut stream).await;
            std::future::pending::<()>().await;
        });

        let client = BootClient::new(HttpClient::new(format!("http://{address}")), 1, 0);
        let started = Instant::now();
        let error = client.name_resolve("k51-test").await.unwrap_err();
        assert!(started.elapsed() < Duration::from_secs(2));
        assert!(error.to_string().contains("did not complete"));
        server.abort();
    }

    #[tokio::test]
    async fn finite_retry_budget_bounds_best_effort_pin_without_watchdog() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            read_request(&mut stream).await;
            std::future::pending::<()>().await;
        });

        let client = BootClient::new(HttpClient::new(format!("http://{address}")), 1, 0);
        let started = Instant::now();
        let error = client.pin_add_once("bafy-test").await.unwrap_err();
        assert!(started.elapsed() < Duration::from_secs(2));
        assert!(error.to_string().contains("IPFS pin add made no progress"));
        server.abort();
    }

    #[tokio::test]
    async fn exclusive_path_creation_preserves_a_preexisting_destination() {
        let directory = tempfile::tempdir().unwrap();
        let destination = directory.path().join("existing");
        std::fs::write(&destination, b"preexisting-owner").unwrap();
        let client = HttpClient::new("http://127.0.0.1:1".into());

        let error = client
            .cat_to_path("/ipfs/bafy-test", &destination)
            .await
            .unwrap_err();

        assert!(error.chain().any(|cause| cause.is::<PathWriteNotOwned>()));
        assert_eq!(std::fs::read(destination).unwrap(), b"preexisting-owner");
    }

    #[tokio::test]
    async fn cat_to_path_flushes_streamed_bytes_before_success() {
        let body = "streamed-path-content";
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            read_request(&mut stream).await;
            stream
                .write_all(
                    format!(
                        "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                        body.len()
                    )
                    .as_bytes(),
                )
                .await
                .unwrap();
        });
        let directory = tempfile::tempdir().unwrap();
        let destination = directory.path().join("materialized");
        let client = HttpClient::new(format!("http://{address}"));

        client
            .cat_to_path("/ipfs/bafy-test", &destination)
            .await
            .unwrap();

        server.await.unwrap();
        assert_eq!(std::fs::read(destination).unwrap(), body.as_bytes());
    }

    #[test]
    fn source_error_waits_for_an_accepted_real_tokio_file_write() {
        let error = run_accepted_write_failure(AcceptedPumpEnd::SourceError);

        assert_eq!(error.to_string(), "source-primary");
    }

    #[test]
    fn source_panic_is_typed_and_waits_for_an_accepted_real_tokio_file_write() {
        let error = run_accepted_write_failure(AcceptedPumpEnd::SourcePanic);

        assert!(error
            .chain()
            .any(|cause| cause.is::<PathWriteSourcePanic>()));
        assert!(error.to_string().contains("source-panic"));
    }

    #[tokio::test]
    async fn successful_pump_observes_deferred_flush_error_before_returning() {
        let directory = tempfile::tempdir().unwrap();
        let file = fresh_exclusive_file(&directory.path().join("staging"));
        let probes = Arc::new(AtomicUsize::new(0));
        let flushes = Arc::new(AtomicUsize::new(0));
        let mut terminal = ScriptedTerminal {
            busy_probes: 0,
            probes: Arc::clone(&probes),
            flushes: Arc::clone(&flushes),
            flush: ScriptedFlush::Error("deferred-write-failure"),
        };

        let error = finish_local_file_write(file, ImmediatePump(Ok(0)), &mut terminal)
            .await
            .unwrap_err();

        assert!(error.to_string().contains("deferred-write-failure"));
        assert_eq!(flushes.load(Ordering::SeqCst), 1);
        assert_eq!(probes.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn primary_pump_error_wins_and_terminal_completion_is_observed_once() {
        for primary in [
            PumpFailure::Source(anyhow::anyhow!("source-primary")),
            PumpFailure::Sink(anyhow::anyhow!("sink-primary")),
        ] {
            let directory = tempfile::tempdir().unwrap();
            let file = fresh_exclusive_file(&directory.path().join("staging"));
            let probes = Arc::new(AtomicUsize::new(0));
            let flushes = Arc::new(AtomicUsize::new(0));
            let mut terminal = ScriptedTerminal {
                busy_probes: 1,
                probes: Arc::clone(&probes),
                flushes: Arc::clone(&flushes),
                flush: ScriptedFlush::Error("secondary-terminal-failure"),
            };
            let expected = match &primary {
                PumpFailure::Source(_) => "source-primary",
                PumpFailure::Sink(_) => "sink-primary",
                PumpFailure::Panic(_) => unreachable!(),
            };

            let error = finish_local_file_write(file, ImmediatePump(Err(primary)), &mut terminal)
                .await
                .unwrap_err();

            assert_eq!(error.to_string(), expected);
            assert_eq!(flushes.load(Ordering::SeqCst), 1);
            assert_eq!(probes.load(Ordering::SeqCst), 1);
        }
    }

    #[tokio::test]
    async fn terminal_flush_panic_retains_file_until_ownership_probe_succeeds() {
        let directory = tempfile::tempdir().unwrap();
        let file = fresh_exclusive_file(&directory.path().join("staging"));
        let probes = Arc::new(AtomicUsize::new(0));
        let flushes = Arc::new(AtomicUsize::new(0));
        let mut terminal = ScriptedTerminal {
            // One busy probe enters terminal flush; two more prove that a
            // flush panic does not release the File before backing work ends.
            busy_probes: 3,
            probes: Arc::clone(&probes),
            flushes: Arc::clone(&flushes),
            flush: ScriptedFlush::Panic,
        };

        let error = finish_local_file_write(
            file,
            ImmediatePump(Err(PumpFailure::Source(anyhow::anyhow!("source-primary")))),
            &mut terminal,
        )
        .await
        .unwrap_err();

        assert_eq!(error.to_string(), "source-primary");
        assert_eq!(flushes.load(Ordering::SeqCst), 1);
        assert_eq!(probes.load(Ordering::SeqCst), 4);
    }

    #[tokio::test]
    async fn already_quiescent_primary_error_skips_terminal_flush() {
        let directory = tempfile::tempdir().unwrap();
        let file = fresh_exclusive_file(&directory.path().join("staging"));
        let flushes = Arc::new(AtomicUsize::new(0));
        let mut terminal = ScriptedTerminal {
            busy_probes: 0,
            probes: Arc::new(AtomicUsize::new(0)),
            flushes: Arc::clone(&flushes),
            flush: ScriptedFlush::Ok,
        };

        let error = finish_local_file_write(
            file,
            ImmediatePump(Err(PumpFailure::Source(anyhow::anyhow!("source-primary")))),
            &mut terminal,
        )
        .await
        .unwrap_err();

        assert_eq!(error.to_string(), "source-primary");
        assert_eq!(flushes.load(Ordering::SeqCst), 0);
    }
}

/// Directory listing entry from Kubo's `/api/v0/ls` endpoint.
#[derive(Debug, Clone, serde::Deserialize)]
pub struct LsEntry {
    #[serde(rename = "Name")]
    pub name: String,
    #[serde(rename = "Hash")]
    pub hash: String,
    #[serde(rename = "Size")]
    pub size: u64,
    #[serde(rename = "Type")]
    pub entry_type: u32, // 1 = directory, 2 = file
}

struct ObjectOnly<T>(T);

impl<'de, T> serde::Deserialize<'de> for ObjectOnly<T>
where
    T: serde::Deserialize<'de>,
{
    fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        struct ObjectOnlyVisitor<T>(std::marker::PhantomData<fn() -> T>);

        impl<'de, T> serde::de::Visitor<'de> for ObjectOnlyVisitor<T>
        where
            T: serde::Deserialize<'de>,
        {
            type Value = ObjectOnly<T>;

            fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                formatter.write_str("a JSON object")
            }

            fn visit_map<A>(self, map: A) -> std::result::Result<Self::Value, A::Error>
            where
                A: serde::de::MapAccess<'de>,
            {
                T::deserialize(serde::de::value::MapAccessDeserializer::new(map)).map(ObjectOnly)
            }
        }

        deserializer.deserialize_map(ObjectOnlyVisitor(std::marker::PhantomData))
    }
}

fn deserialize_object_array<'de, D, T>(
    deserializer: D,
    context: &'static str,
) -> std::result::Result<Vec<T>, D::Error>
where
    D: serde::Deserializer<'de>,
    T: serde::Deserialize<'de>,
{
    struct ObjectArrayVisitor<T> {
        context: &'static str,
        marker: std::marker::PhantomData<fn() -> T>,
    }

    impl<'de, T> serde::de::Visitor<'de> for ObjectArrayVisitor<T>
    where
        T: serde::Deserialize<'de>,
    {
        type Value = Vec<T>;

        fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            formatter.write_str("an array of JSON objects")
        }

        fn visit_seq<A>(self, mut sequence: A) -> std::result::Result<Self::Value, A::Error>
        where
            A: serde::de::SeqAccess<'de>,
        {
            let mut values = Vec::with_capacity(sequence.size_hint().unwrap_or(0));
            let mut index = 0;
            loop {
                match sequence.next_element::<ObjectOnly<T>>() {
                    Ok(Some(ObjectOnly(value))) => values.push(value),
                    Ok(None) => return Ok(values),
                    Err(error) => {
                        return Err(<A::Error as serde::de::Error>::custom(format!(
                            "{}[{index}]: {error}",
                            self.context
                        )));
                    }
                }
                index += 1;
            }
        }
    }

    deserializer.deserialize_seq(ObjectArrayVisitor {
        context,
        marker: std::marker::PhantomData,
    })
}

fn deserialize_ls_objects<'de, D>(deserializer: D) -> std::result::Result<Vec<LsObject>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    deserialize_object_array(deserializer, "Malformed IPFS ls object at Objects")
}

fn deserialize_ls_links<'de, D>(deserializer: D) -> std::result::Result<Vec<LsEntry>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    deserialize_object_array(
        deserializer,
        "Malformed IPFS ls directory entry at Objects[0].Links",
    )
}

#[derive(serde::Deserialize)]
struct LsResponse {
    #[serde(rename = "Objects", deserialize_with = "deserialize_ls_objects")]
    objects: Vec<LsObject>,
}

#[derive(serde::Deserialize)]
struct LsObject {
    #[serde(rename = "Links", deserialize_with = "deserialize_ls_links")]
    links: Vec<LsEntry>,
}

/// Parses a raw Kubo `/api/v0/ls` response body.
///
/// This function is public so external fuzz targets can exercise the same
/// JSON boundary as the production HTTP client.
#[doc(hidden)]
pub fn parse_kubo_ls_response(response_body: &[u8]) -> Result<Vec<LsEntry>> {
    let ObjectOnly(body): ObjectOnly<LsResponse> = serde_json::from_slice(response_body)?;

    if body.objects.len() != 1 {
        anyhow::bail!("expected one Objects entry, found {}", body.objects.len());
    }

    Ok(body
        .objects
        .into_iter()
        .next()
        .expect("length checked above")
        .links)
}

impl HttpClient {
    /// List directory entries at an IPFS path.
    ///
    /// Calls Kubo's `/api/v0/ls?arg=<path>` and parses the JSON response.
    pub async fn ls(&self, path: &str) -> Result<Vec<LsEntry>> {
        let url = format!("{}/api/v0/ls", self.base_url);
        let response = self
            .http_client
            .post(&url)
            .query(&[("arg", path)])
            .send()
            .await
            .with_context(|| format!("Failed to ls IPFS path {path}"))?;

        if !response.status().is_success() {
            return Err(
                KuboApiError::from_response(format!("IPFS ls (path: {path})"), response)
                    .await
                    .into(),
            );
        }

        let response_body = response
            .bytes()
            .await
            .with_context(|| format!("Failed to read IPFS ls response for {path}"))?;
        parse_kubo_ls_response(&response_body)
            .map_err(|error| anyhow::anyhow!("Malformed IPFS ls response for {path}: {error}"))
    }

    /// Pin a CID on the IPFS node.
    ///
    /// Calls Kubo's `/api/v0/pin/add?arg=<cid>`.
    pub async fn pin_add(&self, cid: &str) -> Result<()> {
        let url = format!("{}/api/v0/pin/add", self.base_url);
        let response = self
            .http_client
            .post(&url)
            .query(&[("arg", cid)])
            .send()
            .await
            .with_context(|| format!("Failed to pin {cid}"))?;

        if !response.status().is_success() {
            return Err(KuboApiError::from_response(
                format!("IPFS pin add (cid: {cid})"),
                response,
            )
            .await
            .into());
        }

        Ok(())
    }

    /// Unpin a CID from the IPFS node.
    ///
    /// Calls Kubo's `/api/v0/pin/rm?arg=<cid>`.
    pub async fn pin_rm(&self, cid: &str) -> Result<()> {
        let url = format!("{}/api/v0/pin/rm?arg={}", self.base_url, cid);
        let response = self
            .http_client
            .post(&url)
            .send()
            .await
            .with_context(|| format!("Failed to unpin {cid}"))?;

        if !response.status().is_success() {
            return Err(
                KuboApiError::from_response(format!("IPFS pin rm (cid: {cid})"), response)
                    .await
                    .into(),
            );
        }

        Ok(())
    }

    /// Add a directory tree to IPFS and return the root CID.
    ///
    /// Adds all files in the directory to IPFS and returns
    /// the CID of the root directory. Skips common build artifacts.
    pub async fn add_dir(&self, dir_path: &Path) -> Result<String> {
        use std::collections::VecDeque;
        use std::fs;

        let url = format!(
            "{}/api/v0/add?wrap-with-directory=true&progress=false",
            self.base_url
        );
        let mut form = reqwest::multipart::Form::new();

        // Collect all files first to get directory structure right
        let mut files_to_add: Vec<(String, Vec<u8>)> = Vec::new();

        // Use iterative approach to collect all files
        let mut queue = VecDeque::new();
        queue.push_back((dir_path.to_path_buf(), String::new()));

        while let Some((current_dir, prefix)) = queue.pop_front() {
            let entries = fs::read_dir(&current_dir)
                .with_context(|| format!("Failed to read directory: {}", current_dir.display()))?;

            let mut dir_entries: Vec<_> = entries.collect::<std::result::Result<Vec<_>, _>>()?;
            // Sort for consistent ordering
            dir_entries.sort_by_key(|e| e.file_name());

            for entry in dir_entries {
                let path = entry.path();
                let file_name = entry.file_name();
                let file_name_str = file_name.to_string_lossy().to_string();

                // Skip Cargo build artifacts and version control
                if file_name_str == "target"
                    || file_name_str == ".git"
                    || file_name_str == ".gitignore"
                    || file_name_str == "Cargo.lock"
                {
                    continue;
                }

                let rel_path = if prefix.is_empty() {
                    file_name_str.clone()
                } else {
                    format!("{}/{}", prefix, file_name_str)
                };

                if path.is_dir() {
                    // Queue subdirectories for processing
                    queue.push_back((path, rel_path));
                } else if path.is_file() {
                    // Read regular files only. Non-regular entries (UDS
                    // sockets, FIFOs, dangling symlinks, etc.) would fail
                    // `fs::read` with platform-specific errors and aren't
                    // meaningful as IPFS-added content. The admin UDS
                    // endpoint at `~/.ww/run/<peer-id>.sock` is the
                    // concrete case that motivated this guard.
                    let bytes = fs::read(&path)
                        .with_context(|| format!("Failed to read file: {}", path.display()))?;
                    files_to_add.push((rel_path, bytes));
                }
                // (else: silently skip non-regular non-directory entries)
            }
        }

        // Add files to form in sorted order for consistent structure
        for (path, bytes) in files_to_add {
            let part = reqwest::multipart::Part::bytes(bytes);
            form = form.part("file".to_string(), part.file_name(path));
        }

        let response = self
            .http_client
            .post(&url)
            .multipart(form)
            .send()
            .await
            .context("Failed to add directory to IPFS")?;

        if !response.status().is_success() {
            return Err(KuboApiError::from_response("IPFS add directory", response)
                .await
                .into());
        }

        let body = response.text().await?;

        // Parse all lines and find the wrapped root directory
        // With wrap-with-directory=true, the last entry is the wrapping directory
        for line in body.lines().rev() {
            if let Ok(json) = serde_json::from_str::<serde_json::Value>(line) {
                if let Some(hash) = json.get("Hash").and_then(|h| h.as_str()) {
                    return cid_identity::parse_cid(hash)
                        .context("IPFS add directory returned an invalid root CID")
                        .map(|cid| cid.to_string());
                }
            }
        }

        anyhow::bail!("Failed to extract CID from IPFS response")
    }
}

// ── Routing / Node Identity API ────────────────────────────────────

/// Node identity returned by Kubo's `/api/v0/id` endpoint.
///
/// Used by the Wetware swarm to bootstrap the in-process Kademlia client
/// against the local Kubo node (Amino DHT).
#[derive(Debug, Clone)]
pub struct KuboInfo {
    /// Kubo's libp2p peer ID as a base58-encoded string (e.g. `"12D3KooW..."`).
    pub peer_id: String,
    /// Kubo's swarm listen addresses (may include `/p2p/<peer-id>` suffix).
    pub swarm_addrs: Vec<String>,
}

impl HttpClient {
    /// Fetch the local Kubo node's identity for Kad bootstrap.
    ///
    /// Calls `POST /api/v0/id` and returns the peer ID and swarm addresses.
    /// Returns an error if Kubo is not reachable.
    pub async fn kubo_info(&self) -> Result<KuboInfo> {
        self.kubo_info_with_timeout(KUBO_INFO_TIMEOUT).await
    }

    async fn kubo_info_with_timeout(&self, request_timeout: Duration) -> Result<KuboInfo> {
        let url = format!("{}/api/v0/id", self.base_url);
        let response = tokio::time::timeout(request_timeout, async {
            let response = self
                .http_client
                .post(&url)
                .send()
                .await
                .with_context(|| format!("kubo id failed: {}", self.base_url))?;

            if !response.status().is_success() {
                anyhow::bail!("kubo id failed: {}", response.status());
            }

            response
                .json()
                .await
                .context("kubo id: failed to parse JSON response")
        })
        .await
        .map_err(|_| {
            anyhow::anyhow!(
                "kubo id timed out after {}s: {}",
                request_timeout.as_secs_f32(),
                self.base_url
            )
        })??;

        let body: serde_json::Value = response;

        let peer_id = body["ID"]
            .as_str()
            .ok_or_else(|| anyhow::anyhow!("kubo id: missing ID field"))?
            .to_string();

        let swarm_addrs = body["Addresses"]
            .as_array()
            .unwrap_or(&Vec::new())
            .iter()
            .filter_map(|v| v.as_str().map(|s| s.to_string()))
            .collect();

        Ok(KuboInfo {
            peer_id,
            swarm_addrs,
        })
    }

    /// Fetch Kubo's connected swarm peers.
    ///
    /// Calls `POST /api/v0/swarm/peers` and returns `(PeerId, Multiaddr)` pairs.
    /// Used to populate the in-process Kad client's routing table so iterative
    /// queries have enough peers to converge.
    pub async fn swarm_peers(&self) -> Result<Vec<(String, String)>> {
        let url = format!("{}/api/v0/swarm/peers", self.base_url);
        let response = self
            .http_client
            .post(&url)
            .send()
            .await
            .with_context(|| format!("kubo swarm/peers failed: {}", self.base_url))?;

        if !response.status().is_success() {
            anyhow::bail!("kubo swarm/peers failed: {}", response.status());
        }

        let body: serde_json::Value = response
            .json()
            .await
            .context("kubo swarm/peers: failed to parse JSON response")?;

        let peers = body["Peers"]
            .as_array()
            .map(|arr| {
                arr.iter()
                    .filter_map(|entry| {
                        let peer_id = entry["Peer"].as_str()?.to_string();
                        let addr = entry["Addr"].as_str()?.to_string();
                        Some((peer_id, addr))
                    })
                    .collect()
            })
            .unwrap_or_default();

        Ok(peers)
    }

    /// Add raw bytes to IPFS, returning the CID.
    ///
    /// Calls Kubo's `POST /api/v0/add`.
    pub async fn add_bytes(&self, data: &[u8]) -> Result<String> {
        let url = format!("{}/api/v0/add", self.base_url);
        let part = reqwest::multipart::Part::bytes(data.to_vec()).file_name("data");
        let form = reqwest::multipart::Form::new().part("file", part);

        let response = self
            .http_client
            .post(&url)
            .multipart(form)
            .send()
            .await
            .context("ipfs add failed")?;

        if !response.status().is_success() {
            let body = response.text().await.unwrap_or_default();
            anyhow::bail!("ipfs add failed: {body}");
        }

        let body: serde_json::Value = response.json().await?;
        body.get("Hash")
            .and_then(|h| h.as_str())
            .map(|s| s.to_string())
            .ok_or_else(|| anyhow::anyhow!("ipfs add: missing Hash in response"))
    }

    /// Find providers of a CID via Kubo's routing API.
    ///
    /// Calls `POST /api/v0/routing/findprovs?arg=<cid>&num-providers=<n>`.
    /// Returns `(peer_id, Vec<multiaddr>)` pairs for each provider found.
    /// The response is NDJSON; we collect entries with `Type == 4` (provider).
    pub async fn find_providers(
        &self,
        cid: &str,
        num_providers: usize,
    ) -> Result<Vec<(String, Vec<String>)>> {
        let url = format!(
            "{}/api/v0/routing/findprovs?arg={}&num-providers={}",
            self.base_url, cid, num_providers
        );
        let response = self
            .http_client
            .post(&url)
            .send()
            .await
            .with_context(|| format!("kubo routing/findprovs failed: {}", self.base_url))?;

        if !response.status().is_success() {
            anyhow::bail!("kubo routing/findprovs failed: {}", response.status());
        }

        let body = response
            .text()
            .await
            .context("kubo routing/findprovs: failed to read response")?;

        let mut providers = Vec::new();
        for line in body.lines() {
            let entry: serde_json::Value = match serde_json::from_str(line) {
                Ok(v) => v,
                Err(_) => continue,
            };
            // Type 4 = provider response in Kubo's routing API
            if entry.get("Type").and_then(|t| t.as_u64()) != Some(4) {
                continue;
            }
            if let Some(responses) = entry.get("Responses").and_then(|r| r.as_array()) {
                for resp in responses {
                    let peer_id = match resp.get("ID").and_then(|id| id.as_str()) {
                        Some(id) => id.to_string(),
                        None => continue,
                    };
                    let addrs: Vec<String> = resp
                        .get("Addrs")
                        .and_then(|a| a.as_array())
                        .map(|arr| {
                            arr.iter()
                                .filter_map(|v| v.as_str().map(|s| s.to_string()))
                                .collect()
                        })
                        .unwrap_or_default();
                    providers.push((peer_id, addrs));
                }
            }
        }

        Ok(providers)
    }
}

// ── Pinner impl for cache crate ────────────────────────────────────

#[async_trait]
impl cache::Pinner for HttpClient {
    async fn pin(&self, cid: &cid::Cid) -> Result<()> {
        self.pin_add(&cid.to_string()).await
    }

    async fn unpin(&self, cid: &cid::Cid) -> Result<()> {
        self.pin_rm(&cid.to_string()).await
    }

    async fn fetch(&self, cid: &cid::Cid) -> Result<Vec<u8>> {
        self.cat(&format!("/ipfs/{cid}")).await
    }

    async fn fetch_path(&self, cid: &cid::Cid, subpath: &str) -> Result<Vec<u8>> {
        let path = if subpath.is_empty() {
            format!("/ipfs/{cid}")
        } else {
            format!("/ipfs/{cid}/{subpath}")
        };
        self.cat(&path).await
    }

    async fn fetch_to_path(&self, cid: &cid::Cid, dst: &Path) -> Result<()> {
        self.cat_to_path(&format!("/ipfs/{cid}"), dst).await
    }

    async fn fetch_path_to_path(&self, cid: &cid::Cid, subpath: &str, dst: &Path) -> Result<()> {
        let path = if subpath.is_empty() {
            format!("/ipfs/{cid}")
        } else {
            format!("/ipfs/{cid}/{subpath}")
        };
        self.cat_to_path(&path, dst).await
    }

    async fn size(&self, cid: &cid::Cid) -> Result<u64> {
        let entries = self.ls(&format!("/ipfs/{cid}")).await?;
        Ok(entries.iter().map(|e| e.size).sum())
    }
}

/// Check if a path is a valid IPFS-family path (IPFS, IPNS, or IPLD)
///
/// This centralizes IPFS path validation similar to Go's `path.NewPath(str)`.
/// Returns true if the path starts with a valid IPFS namespace prefix.
pub fn is_ipfs_path(path: &str) -> bool {
    path.starts_with("/ipfs/") || path.starts_with("/ipns/") || path.starts_with("/ipld/")
}

// ── MFS (Mutable File System) API ──────────────────────────────────

/// MFS directory entry from `/api/v0/files/ls`.
#[derive(Debug, Clone, serde::Deserialize)]
pub struct MfsEntry {
    #[serde(rename = "Name")]
    pub name: String,
    #[serde(rename = "Hash")]
    pub hash: String,
    #[serde(rename = "Size")]
    pub size: u64,
    /// 0 = file, 1 = directory
    #[serde(rename = "Type")]
    pub entry_type: u32,
}

#[derive(serde::Deserialize)]
struct MfsLsResponse {
    #[serde(
        rename = "Entries",
        deserialize_with = "deserialize_nullable_mfs_entries"
    )]
    entries: Vec<MfsEntry>,
}

fn deserialize_nullable_mfs_entries<'de, D>(
    deserializer: D,
) -> std::result::Result<Vec<MfsEntry>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    struct NullableEntriesVisitor;

    impl<'de> serde::de::Visitor<'de> for NullableEntriesVisitor {
        type Value = Vec<MfsEntry>;

        fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            formatter.write_str("null or an array of MFS entry objects")
        }

        fn visit_none<E>(self) -> std::result::Result<Self::Value, E>
        where
            E: serde::de::Error,
        {
            Ok(Vec::new())
        }

        fn visit_some<D>(self, deserializer: D) -> std::result::Result<Self::Value, D::Error>
        where
            D: serde::Deserializer<'de>,
        {
            deserialize_object_array(deserializer, "Malformed MFS directory entry at Entries")
        }
    }

    deserializer.deserialize_option(NullableEntriesVisitor)
}

/// Parses a raw Kubo `/api/v0/files/ls` response body.
///
/// This function is public so external fuzz targets can exercise the same
/// JSON boundary as the production HTTP client.
#[doc(hidden)]
pub fn parse_kubo_mfs_ls_response(response_body: &[u8]) -> Result<Vec<MfsEntry>> {
    let ObjectOnly(body): ObjectOnly<MfsLsResponse> = serde_json::from_slice(response_body)?;
    Ok(body.entries)
}

/// MFS stat result from `/api/v0/files/stat`.
#[derive(Debug, Clone, serde::Deserialize)]
pub struct MfsStat {
    #[serde(rename = "Hash")]
    pub hash: String,
    #[serde(rename = "Size")]
    pub size: u64,
    #[serde(rename = "Type")]
    pub entry_type: String,
}

/// Borrowing reference to the MFS API.
pub struct MFS<'a> {
    client: &'a HttpClient,
}

impl HttpClient {
    /// Get a borrowing reference to the MFS API.
    pub fn mfs(&self) -> MFS<'_> {
        MFS { client: self }
    }
}

impl MFS<'_> {
    /// Create a directory in MFS.
    pub async fn files_mkdir(&self, path: &str, parents: bool) -> Result<()> {
        let url = format!("{}/api/v0/files/mkdir", self.client.base_url);
        let response = self
            .client
            .http_client
            .post(&url)
            .query(&[("arg", path)])
            .query(&[("parents", parents)])
            .send()
            .await
            .with_context(|| format!("MFS mkdir failed for {path}"))?;

        if !response.status().is_success() {
            return Err(
                KuboApiError::from_response(format!("MFS mkdir (path: {path})"), response)
                    .await
                    .into(),
            );
        }
        Ok(())
    }

    /// Copy a file or directory in MFS. Source can be `/ipfs/<cid>`.
    pub async fn files_cp(&self, src: &str, dst: &str) -> Result<()> {
        let url = format!("{}/api/v0/files/cp", self.client.base_url);
        let response = self
            .client
            .http_client
            .post(&url)
            .query(&[("arg", src), ("arg", dst)])
            .send()
            .await
            .with_context(|| format!("MFS cp failed: {src} -> {dst}"))?;

        if !response.status().is_success() {
            return Err(KuboApiError::from_response(
                format!("MFS copy ({src} -> {dst})"),
                response,
            )
            .await
            .into());
        }
        Ok(())
    }

    /// List entries in an MFS directory.
    pub async fn files_ls(&self, path: &str) -> Result<Vec<MfsEntry>> {
        let url = format!("{}/api/v0/files/ls", self.client.base_url);
        let response = self
            .client
            .http_client
            .post(&url)
            .query(&[("arg", path), ("long", "true")])
            .send()
            .await
            .with_context(|| format!("MFS ls failed for {path}"))?;

        if !response.status().is_success() {
            return Err(
                KuboApiError::from_response(format!("MFS ls (path: {path})"), response)
                    .await
                    .into(),
            );
        }

        let response_body = response
            .bytes()
            .await
            .with_context(|| format!("Failed to read MFS ls response for {path}"))?;
        parse_kubo_mfs_ls_response(&response_body)
            .map_err(|error| anyhow::anyhow!("Malformed MFS ls response for {path}: {error}"))
    }

    /// Stat an MFS path, optionally computing its hash.
    pub async fn files_stat(&self, path: &str, hash: bool) -> Result<MfsStat> {
        let url = format!("{}/api/v0/files/stat", self.client.base_url);
        let response = self
            .client
            .http_client
            .post(&url)
            .query(&[("arg", path)])
            .query(&[("hash", hash)])
            .send()
            .await
            .with_context(|| format!("MFS stat failed for {path}"))?;

        if !response.status().is_success() {
            return Err(
                KuboApiError::from_response(format!("MFS stat (path: {path})"), response)
                    .await
                    .into(),
            );
        }

        response
            .json()
            .await
            .with_context(|| format!("Failed to parse MFS stat response for {path}"))
    }

    /// Remove an MFS path.
    pub async fn files_rm(&self, path: &str, recursive: bool) -> Result<()> {
        let url = format!("{}/api/v0/files/rm", self.client.base_url);
        let response = self
            .client
            .http_client
            .post(&url)
            .query(&[("arg", path)])
            .query(&[("recursive", recursive)])
            .send()
            .await
            .with_context(|| format!("MFS rm failed for {path}"))?;

        if !response.status().is_success() {
            return Err(
                KuboApiError::from_response(format!("MFS rm (path: {path})"), response)
                    .await
                    .into(),
            );
        }
        Ok(())
    }
}
