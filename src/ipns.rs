//! Host-owned IPNS record handling.
//!
//! Wetware signs and validates records locally. Kubo only transports the raw
//! signed protobuf through HTTP Routing V1. The canonical durable object is
//! always that raw signed record.

use std::cmp::Ordering;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{bail, Context, Result};
use chrono::{DateTime, Utc};
use cid::multibase::Base;
use cid::Cid;
use libp2p::identity::{Keypair, PeerId};
use reqwest::StatusCode;
use rust_ipns::Record;

pub const IPNS_RECORD_MEDIA_TYPE: &str = "application/vnd.ipfs.ipns-record";
pub const MAX_RECORD_SIZE: usize = 10_240;

/// Kubo/Boxo-compatible defaults used by the host-owned publisher.
pub const RECORD_LIFETIME: Duration = Duration::from_secs(48 * 60 * 60);
pub const RECORD_TTL: Duration = Duration::from_secs(5 * 60);
pub const REPUBLISH_INTERVAL: Duration = Duration::from_secs(4 * 60 * 60);
pub const INITIAL_REPUBLISH_DELAY: Duration = Duration::from_secs(60);
pub const REPUBLISH_RETRY_DELAY: Duration = Duration::from_secs(5 * 60);
pub const ROUTING_REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

const LIBP2P_KEY_CODEC: u64 = 0x72;

/// Parse either a canonical Base36 IPNS name or a legacy base58 Peer ID.
pub fn parse_name(value: &str) -> Result<PeerId> {
    let value = value.strip_prefix("/ipns/").unwrap_or(value);
    if let Ok(peer_id) = value.parse::<PeerId>() {
        return Ok(peer_id);
    }
    let cid = value
        .parse::<Cid>()
        .with_context(|| format!("invalid IPNS name: {value}"))?;
    if cid.codec() != LIBP2P_KEY_CODEC {
        bail!(
            "invalid IPNS name codec 0x{:x}; expected libp2p-key (0x72)",
            cid.codec()
        );
    }
    PeerId::from_multihash(cid.hash().to_owned())
        .map_err(|_| anyhow::anyhow!("IPNS name contains an invalid Peer ID multihash"))
}

/// Format a Peer ID as the canonical Base36 CIDv1 IPNS name.
pub fn canonical_name(peer_id: PeerId) -> String {
    Cid::new_v1(LIBP2P_KEY_CODEC, peer_id.into())
        .to_string_of_base(Base::Base36Lower)
        .expect("Base36 is a valid CID multibase")
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RoutingErrorKind {
    Unsupported,
    Temporary,
    Malformed,
    Rejected,
}

#[derive(Debug)]
pub struct RoutingError {
    kind: RoutingErrorKind,
    message: String,
}

impl RoutingError {
    fn new(kind: RoutingErrorKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
        }
    }

    pub fn kind(&self) -> RoutingErrorKind {
        self.kind
    }
}

impl std::fmt::Display for RoutingError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for RoutingError {}

/// Return true when an operation failed only because Routing V1 is temporarily
/// unavailable. Local state, signing, and protocol-policy failures are fatal.
pub fn is_temporary_routing_failure(error: &anyhow::Error) -> bool {
    error
        .chain()
        .find_map(|cause| cause.downcast_ref::<RoutingError>())
        .is_some_and(|error| error.kind() == RoutingErrorKind::Temporary)
}

#[derive(Debug)]
pub enum Fetch {
    Record(Vec<u8>),
    NotFound,
}

/// Narrow HTTP Routing V1 raw-record client.
#[derive(Clone)]
pub struct RoutingClient {
    base_url: String,
    http: reqwest::Client,
}

impl RoutingClient {
    pub fn new(base_url: impl Into<String>) -> Result<Self> {
        Self::with_timeout(base_url, ROUTING_REQUEST_TIMEOUT)
    }

    fn with_timeout(base_url: impl Into<String>, timeout: Duration) -> Result<Self> {
        let http = reqwest::Client::builder()
            .no_proxy()
            .timeout(timeout)
            .build()
            .context("build Routing V1 HTTP client")?;
        Ok(Self {
            base_url: base_url.into().trim_end_matches('/').to_string(),
            http,
        })
    }

    fn record_url(&self, name: PeerId) -> String {
        format!("{}/routing/v1/ipns/{}", self.base_url, canonical_name(name))
    }

    /// Verify that the configured listener exposes Routing V1.
    ///
    /// Kubo returns 400 for a malformed IPNS name when the route exists and
    /// 404 from the ordinary Gateway when `Gateway.ExposeRoutingAPI` is false.
    pub async fn probe(&self) -> std::result::Result<(), RoutingError> {
        let response = self
            .http
            .get(format!(
                "{}/routing/v1/ipns/not-an-ipns-name",
                self.base_url
            ))
            .header(reqwest::header::ACCEPT, IPNS_RECORD_MEDIA_TYPE)
            .send()
            .await
            .map_err(|error| {
                RoutingError::new(
                    RoutingErrorKind::Temporary,
                    format!("Routing V1 probe transport failure: {error}"),
                )
            })?;
        match response.status() {
            StatusCode::BAD_REQUEST => Ok(()),
            StatusCode::NOT_FOUND | StatusCode::METHOD_NOT_ALLOWED | StatusCode::NOT_IMPLEMENTED => {
                Err(RoutingError::new(
                    RoutingErrorKind::Unsupported,
                    "Routing V1 endpoint is unavailable; set Kubo Gateway.ExposeRoutingAPI=true and configure the Gateway listener, not the port 5001 RPC listener",
                ))
            }
            status
                if status.is_server_error()
                    || matches!(status, StatusCode::REQUEST_TIMEOUT | StatusCode::TOO_MANY_REQUESTS) =>
            {
                Err(RoutingError::new(
                RoutingErrorKind::Temporary,
                format!("Routing V1 probe returned {status}"),
                ))
            }
            StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN => Err(RoutingError::new(
                RoutingErrorKind::Rejected,
                format!("Routing V1 probe rejected access with {}", response.status()),
            )),
            status => Err(RoutingError::new(
                RoutingErrorKind::Malformed,
                format!("Routing V1 probe returned unexpected status {status}"),
            )),
        }
    }

    pub async fn get(&self, name: PeerId) -> std::result::Result<Fetch, RoutingError> {
        let mut response = self
            .http
            .get(self.record_url(name))
            .header(reqwest::header::ACCEPT, IPNS_RECORD_MEDIA_TYPE)
            .send()
            .await
            .map_err(|error| {
                RoutingError::new(
                    RoutingErrorKind::Temporary,
                    format!("Routing V1 GET transport failure: {error}"),
                )
            })?;
        let status = response.status();
        if status == StatusCode::NOT_FOUND {
            return Ok(Fetch::NotFound);
        }
        if matches!(
            status,
            StatusCode::METHOD_NOT_ALLOWED | StatusCode::NOT_IMPLEMENTED
        ) {
            return Err(RoutingError::new(
                RoutingErrorKind::Unsupported,
                "Routing V1 endpoint does not support raw IPNS GET; enable Kubo Gateway.ExposeRoutingAPI",
            ));
        }
        if status.is_server_error()
            || matches!(
                status,
                StatusCode::REQUEST_TIMEOUT | StatusCode::TOO_MANY_REQUESTS
            )
        {
            return Err(RoutingError::new(
                RoutingErrorKind::Temporary,
                format!("Routing V1 GET returned {status}"),
            ));
        }
        if !status.is_success() {
            return Err(RoutingError::new(
                RoutingErrorKind::Rejected,
                format!("Routing V1 GET returned {status}"),
            ));
        }
        let content_type = response
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .unwrap_or_default();
        let media_type = content_type
            .split(';')
            .next()
            .map(str::trim)
            .unwrap_or_default();
        if media_type.eq_ignore_ascii_case("text/plain") {
            return Ok(Fetch::NotFound);
        }
        if !content_type
            .to_ascii_lowercase()
            .starts_with(IPNS_RECORD_MEDIA_TYPE)
        {
            return Err(RoutingError::new(
                RoutingErrorKind::Malformed,
                format!("Routing V1 GET returned unexpected media type {content_type:?}"),
            ));
        }
        if response
            .content_length()
            .is_some_and(|length| length > MAX_RECORD_SIZE as u64)
        {
            return Err(RoutingError::new(
                RoutingErrorKind::Malformed,
                "Routing V1 GET record exceeds 10,240 bytes",
            ));
        }
        let mut bytes = Vec::new();
        while let Some(chunk) = response.chunk().await.map_err(|error| {
            RoutingError::new(
                RoutingErrorKind::Temporary,
                format!("Routing V1 GET response body failed: {error}"),
            )
        })? {
            if bytes.len() + chunk.len() > MAX_RECORD_SIZE {
                return Err(RoutingError::new(
                    RoutingErrorKind::Malformed,
                    "Routing V1 GET record exceeds 10,240 bytes",
                ));
            }
            bytes.extend_from_slice(&chunk);
        }
        Ok(Fetch::Record(bytes))
    }

    pub async fn put(&self, name: PeerId, raw: &[u8]) -> std::result::Result<(), RoutingError> {
        if raw.len() > MAX_RECORD_SIZE {
            return Err(RoutingError::new(
                RoutingErrorKind::Malformed,
                "refusing to PUT an IPNS record larger than 10,240 bytes",
            ));
        }
        let response = self
            .http
            .put(self.record_url(name))
            .header(reqwest::header::CONTENT_TYPE, IPNS_RECORD_MEDIA_TYPE)
            .body(raw.to_vec())
            .send()
            .await
            .map_err(|error| {
                RoutingError::new(
                    RoutingErrorKind::Temporary,
                    format!("Routing V1 PUT transport failure: {error}"),
                )
            })?;
        let status = response.status();
        if matches!(
            status,
            StatusCode::NOT_FOUND | StatusCode::METHOD_NOT_ALLOWED | StatusCode::NOT_IMPLEMENTED
        ) {
            return Err(RoutingError::new(
                RoutingErrorKind::Unsupported,
                "Routing V1 endpoint is unavailable; enable Kubo Gateway.ExposeRoutingAPI and use the Gateway listener",
            ));
        }
        if status.is_server_error()
            || matches!(
                status,
                StatusCode::REQUEST_TIMEOUT | StatusCode::TOO_MANY_REQUESTS
            )
        {
            return Err(RoutingError::new(
                RoutingErrorKind::Temporary,
                format!("Routing V1 PUT returned {status}"),
            ));
        }
        if !status.is_success() {
            let detail = response.text().await.unwrap_or_default();
            let detail = detail.trim();
            return Err(RoutingError::new(
                RoutingErrorKind::Rejected,
                if detail.is_empty() {
                    format!("Routing V1 PUT rejected the record with {status}")
                } else {
                    format!("Routing V1 PUT rejected the record with {status}: {detail}")
                },
            ));
        }
        Ok(())
    }
}

/// One durable raw-record location below trusted private host state.
#[derive(Clone, Debug)]
pub struct RecordStore {
    state_dir: PathBuf,
    path: PathBuf,
}

impl RecordStore {
    pub fn follower(state_dir: &Path, name: PeerId) -> Self {
        Self::new(state_dir, "follow", name)
    }

    pub fn publisher(state_dir: &Path, name: PeerId) -> Self {
        Self::new(state_dir, "publish", name)
    }

    fn new(state_dir: &Path, role: &str, name: PeerId) -> Self {
        let name = canonical_name(name);
        Self {
            state_dir: state_dir.to_path_buf(),
            path: state_dir
                .join("ipns")
                .join(role)
                .join(format!("{name}.record")),
        }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn load(&self) -> Result<Option<Vec<u8>>> {
        match std::fs::read(&self.path) {
            Ok(bytes) if bytes.len() <= MAX_RECORD_SIZE => Ok(Some(bytes)),
            Ok(_) => bail!(
                "persisted IPNS record exceeds 10,240 bytes: {}",
                self.path.display()
            ),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(error)
                .with_context(|| format!("read persisted IPNS record: {}", self.path.display())),
        }
    }

    pub fn persist(&self, raw: &[u8]) -> Result<()> {
        if raw.len() > MAX_RECORD_SIZE {
            bail!("refusing to persist an IPNS record larger than 10,240 bytes");
        }
        self.restrict_directories()?;
        crate::keys::atomic_write_private(&self.path, raw)
            .with_context(|| format!("persist signed IPNS record: {}", self.path.display()))
    }

    fn restrict_directories(&self) -> Result<()> {
        #[cfg(unix)]
        use std::os::unix::fs::PermissionsExt;

        for directory in [
            self.state_dir.clone(),
            self.state_dir.join("ipns"),
            self.path
                .parent()
                .expect("record path always has a parent")
                .to_path_buf(),
        ] {
            std::fs::create_dir_all(&directory).with_context(|| {
                format!(
                    "create private IPNS state directory: {}",
                    directory.display()
                )
            })?;
            #[cfg(unix)]
            std::fs::set_permissions(&directory, std::fs::Permissions::from_mode(0o700))
                .with_context(|| {
                    format!(
                        "restrict private IPNS state directory: {}",
                        directory.display()
                    )
                })?;
        }
        Ok(())
    }
}

/// A locally verified signed record and its authenticated semantics.
#[derive(Clone, Debug)]
pub struct SignedRecord {
    raw: Vec<u8>,
    record: Record,
    value: Vec<u8>,
    deployment_cid: Option<Cid>,
    eol: DateTime<Utc>,
    ttl: Duration,
}

impl SignedRecord {
    pub fn decode(name: PeerId, raw: Vec<u8>) -> Result<Self> {
        let record = Record::decode(&raw).context("decode raw IPNS record")?;
        record
            .verify_signature(name)
            .context("verify IPNS record signer and signature")?;
        let value = record.value().to_vec();
        let deployment_cid = parse_value(&value).ok();
        let eol = record
            .validity()
            .context("parse signed IPNS record EOL")?
            .with_timezone(&Utc);
        let ttl = Duration::from_nanos(record.ttl());
        Ok(Self {
            raw,
            record,
            value,
            deployment_cid,
            eol,
            ttl,
        })
    }

    pub fn raw(&self) -> &[u8] {
        &self.raw
    }

    pub fn value(&self) -> &[u8] {
        &self.value
    }

    pub fn deployment_cid(&self) -> Option<Cid> {
        self.deployment_cid
    }

    pub fn sequence(&self) -> u64 {
        self.record.sequence()
    }

    pub fn eol(&self) -> DateTime<Utc> {
        self.eol
    }

    pub fn ttl(&self) -> Duration {
        self.ttl
    }

    pub fn is_expired_at(&self, now: DateTime<Utc>) -> bool {
        self.eol <= now
    }

    pub fn compare(&self, other: &Self) -> Result<Ordering> {
        self.record
            .compare(&other.record)
            .context("compare signed IPNS records")
    }
}

fn parse_value(value: &[u8]) -> Result<Cid> {
    let value = std::str::from_utf8(value).context("IPNS value is not UTF-8")?;
    let cid = value
        .strip_prefix("/ipfs/")
        .context("IPNS value is not an /ipfs/<cid> deployment binding")?;
    if cid.is_empty() || cid.contains('/') {
        bail!("IPNS deployment binding must contain one CID and no subpath");
    }
    cid.parse::<Cid>()
        .context("IPNS value contains an invalid CID")
}

/// Apply the Wetware single-writer publisher policy to durable local state.
pub struct Publisher {
    keypair: Keypair,
    name: PeerId,
    routing: RoutingClient,
    store: RecordStore,
    local: Option<SignedRecord>,
    pending_put: bool,
}

/// Long-running publisher lifecycle for the host's own default IPNS name.
///
/// A followed third-party name never creates this service.
pub struct Republisher {
    publisher: Publisher,
    desired: Cid,
    initial_publish_succeeded: bool,
}

impl Republisher {
    pub fn new(publisher: Publisher, desired: Cid, initial_publish_succeeded: bool) -> Self {
        Self {
            publisher,
            desired,
            initial_publish_succeeded,
        }
    }

    async fn wait_or_shutdown(
        shutdown: &mut tokio::sync::watch::Receiver<()>,
        duration: Duration,
    ) -> bool {
        tokio::select! {
            _ = tokio::time::sleep(duration) => false,
            _ = shutdown.changed() => true,
        }
    }

    async fn run_async(mut self, mut shutdown: tokio::sync::watch::Receiver<()>) -> Result<()> {
        let mut retry_delay = !self.initial_publish_succeeded;
        let mut first = true;
        loop {
            let delay = if retry_delay {
                REPUBLISH_RETRY_DELAY
            } else if first {
                INITIAL_REPUBLISH_DELAY
            } else {
                REPUBLISH_INTERVAL
            };
            if Self::wait_or_shutdown(&mut shutdown, delay).await {
                return Ok(());
            }

            let result = if self.publisher.has_pending_put() {
                self.publisher.retry_pending(self.desired).await
            } else if retry_delay {
                self.publisher.publish(self.desired).await
            } else {
                self.publisher.refresh(self.desired).await
            };
            match result {
                Ok(()) => {
                    retry_delay = false;
                    first = false;
                    tracing::info!(
                        ipns_name = %self.publisher.canonical_name(),
                        sequence = self.publisher.current().map(SignedRecord::sequence),
                        "Host IPNS record published"
                    );
                }
                Err(error) => {
                    if !is_temporary_routing_failure(&error) {
                        return Err(error)
                            .context("host IPNS republisher stopped on a non-retryable failure");
                    }
                    retry_delay = true;
                    tracing::warn!(
                        ipns_name = %self.publisher.canonical_name(),
                        retry_secs = REPUBLISH_RETRY_DELAY.as_secs(),
                        pending_put = self.publisher.has_pending_put(),
                        "Host IPNS publication failed temporarily; publication will be retried: {error:#}"
                    );
                }
            }
        }
    }
}

impl crate::services::Service for Republisher {
    fn run(self, shutdown: tokio::sync::watch::Receiver<()>) -> Result<()> {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .context("build IPNS republisher runtime")?
            .block_on(self.run_async(shutdown))
    }
}

impl Publisher {
    pub fn open(keypair: Keypair, routing: RoutingClient, state_dir: &Path) -> Result<Self> {
        let name = keypair.public().to_peer_id();
        let store = RecordStore::publisher(state_dir, name);
        let local = store
            .load()?
            .map(|raw| SignedRecord::decode(name, raw))
            .transpose()
            .context("validate persisted publisher record")?;
        Ok(Self {
            keypair,
            name,
            routing,
            store,
            local,
            pending_put: false,
        })
    }

    pub fn name(&self) -> PeerId {
        self.name
    }

    pub fn canonical_name(&self) -> String {
        canonical_name(self.name)
    }

    pub fn state_path(&self) -> &Path {
        self.store.path()
    }

    pub fn current(&self) -> Option<&SignedRecord> {
        self.local.as_ref()
    }

    /// Publish a desired deployment CID.
    ///
    /// An already-signed matching record is retried byte-for-byte. Creating a
    /// changed binding first reconciles the valid signed network floor.
    pub async fn publish(&mut self, value: Cid) -> Result<()> {
        self.reconcile_network(value).await?;
        if let Some(local) = self.local.as_ref() {
            if local.deployment_cid() == Some(value) && !local.is_expired_at(Utc::now()) {
                let refresh_deadline = Utc::now()
                    + chrono::Duration::from_std(INITIAL_REPUBLISH_DELAY)
                        .expect("initial republish delay fits chrono");
                if local.eol() > refresh_deadline {
                    return self.put_current().await;
                }
                return self.refresh_after_reconcile(value).await;
            }
        }

        let sequence = match self.local.as_ref() {
            Some(local) if local.deployment_cid() == Some(value) => local.sequence(),
            Some(local) => local
                .sequence()
                .checked_add(1)
                .context("IPNS publisher sequence exhausted")?,
            None => 0,
        };
        self.sign_persist_and_put(value, sequence).await
    }

    /// Reconcile and extend the desired binding without changing its sequence.
    pub async fn refresh(&mut self, desired: Cid) -> Result<()> {
        self.reconcile_network(desired).await?;
        self.refresh_after_reconcile(desired).await
    }

    async fn refresh_after_reconcile(&mut self, desired: Cid) -> Result<()> {
        let current = self
            .local
            .as_ref()
            .context("cannot republish before an IPNS value has been published")?;
        if current.deployment_cid() != Some(desired) {
            let sequence = current
                .sequence()
                .checked_add(1)
                .context("IPNS publisher sequence exhausted")?;
            return self.sign_persist_and_put(desired, sequence).await;
        }

        let candidate = self.sign_record(desired, current.sequence())?;
        if candidate.compare(current)? != Ordering::Greater {
            return self.put_current().await;
        }
        self.persist_and_put(candidate).await
    }

    fn has_pending_put(&self) -> bool {
        self.pending_put
    }

    /// Retry bytes only when this process persisted them before a failed PUT.
    async fn retry_pending(&mut self, desired: Cid) -> Result<()> {
        if !self.pending_put {
            return self.publish(desired).await;
        }
        if self
            .local
            .as_ref()
            .is_none_or(|record| record.is_expired_at(Utc::now()))
        {
            self.pending_put = false;
            return self.publish(desired).await;
        }
        self.put_current().await
    }

    async fn put_current(&mut self) -> Result<()> {
        let current = self
            .local
            .as_ref()
            .context("cannot retry before an IPNS record has been persisted")?;
        let result = self
            .routing
            .put(self.name, current.raw())
            .await
            .context("publish persisted IPNS record");
        if result.is_ok() {
            self.pending_put = false;
        }
        result
    }

    async fn sign_persist_and_put(&mut self, value: Cid, sequence: u64) -> Result<()> {
        let signed = self.sign_record(value, sequence)?;
        self.persist_and_put(signed).await
    }

    fn sign_record(&self, value: Cid, sequence: u64) -> Result<SignedRecord> {
        let eol = Utc::now()
            + chrono::Duration::from_std(RECORD_LIFETIME)
                .expect("48-hour record lifetime fits chrono");
        let record = Record::new(
            &self.keypair,
            format!("/ipfs/{value}").as_bytes(),
            eol,
            sequence,
            RECORD_TTL,
        )
        .context("sign IPNS record")?;
        let raw = record.encode().context("encode signed IPNS record")?;
        SignedRecord::decode(self.name, raw)
    }

    async fn persist_and_put(&mut self, signed: SignedRecord) -> Result<()> {
        // Persist-before-PUT also makes a failed PUT retryable byte-for-byte.
        self.store.persist(signed.raw())?;
        self.local = Some(signed);
        self.pending_put = true;
        self.put_current().await
    }

    async fn reconcile_network(&mut self, desired: Cid) -> Result<()> {
        let remote = match self.routing.get(self.name).await? {
            Fetch::NotFound => return Ok(()),
            Fetch::Record(raw) => SignedRecord::decode(self.name, raw)
                .context("validate network IPNS publisher record")?,
        };
        let Some(local) = self.local.as_ref() else {
            self.store.persist(remote.raw())?;
            let conflict = remote.deployment_cid() != Some(desired);
            self.local = Some(remote);
            self.pending_put = false;
            if conflict {
                return self.unexpected_remote_value(desired);
            }
            return Ok(());
        };

        if local.sequence() == remote.sequence() && local.value() != remote.value() {
            bail!(
                "IPNS publisher equivocation at sequence {} for {}; another writer is using this private identity",
                local.sequence(),
                canonical_name(self.name)
            );
        }
        if remote.compare(local)? == Ordering::Greater {
            let conflict = remote.deployment_cid() != Some(desired);
            self.store.persist(remote.raw())?;
            self.local = Some(remote);
            self.pending_put = false;
            if conflict {
                return self.unexpected_remote_value(desired);
            }
        }
        Ok(())
    }

    fn unexpected_remote_value(&self, desired: Cid) -> Result<()> {
        let remote = self
            .local
            .as_ref()
            .expect("network reconciliation persisted the remote floor");
        bail!(
            "IPNS publisher single-writer conflict for {}: adopted network sequence {} selecting {:?}, but local configuration selects /ipfs/{desired}; no record was published",
            canonical_name(self.name),
            remote.sequence(),
            String::from_utf8_lossy(remote.value())
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    #[derive(Debug)]
    struct Request {
        method: String,
        body: Vec<u8>,
    }

    async fn read_request(stream: &mut tokio::net::TcpStream) -> Request {
        let mut bytes = Vec::new();
        let header_end = loop {
            let mut chunk = [0_u8; 4096];
            let read = stream.read(&mut chunk).await.unwrap();
            assert!(read > 0, "connection closed before HTTP headers");
            bytes.extend_from_slice(&chunk[..read]);
            if let Some(offset) = bytes.windows(4).position(|window| window == b"\r\n\r\n") {
                break offset + 4;
            }
        };
        let headers = String::from_utf8_lossy(&bytes[..header_end]);
        let method = headers.split_whitespace().next().unwrap().to_string();
        let content_length = headers
            .lines()
            .find_map(|line| {
                let (name, value) = line.split_once(':')?;
                name.eq_ignore_ascii_case("content-length")
                    .then(|| value.trim().parse::<usize>().unwrap())
            })
            .unwrap_or(0);
        while bytes.len() - header_end < content_length {
            let mut chunk = [0_u8; 4096];
            let read = stream.read(&mut chunk).await.unwrap();
            assert!(read > 0, "connection closed before HTTP body");
            bytes.extend_from_slice(&chunk[..read]);
        }
        Request {
            method,
            body: bytes[header_end..header_end + content_length].to_vec(),
        }
    }

    async fn scripted_server(
        responses: Vec<(&'static str, Vec<u8>)>,
    ) -> (String, tokio::task::JoinHandle<Vec<Request>>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let task = tokio::spawn(async move {
            let mut requests = Vec::new();
            for (status, body) in responses {
                let (mut stream, _) = listener.accept().await.unwrap();
                requests.push(read_request(&mut stream).await);
                let response = format!(
                    "HTTP/1.1 {status}\r\nContent-Type: {IPNS_RECORD_MEDIA_TYPE}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    body.len()
                );
                stream.write_all(response.as_bytes()).await.unwrap();
                stream.write_all(&body).await.unwrap();
            }
            requests
        });
        (format!("http://{address}"), task)
    }

    fn test_cid(byte: u8) -> Cid {
        use cid::multihash::Multihash;
        Cid::new_v1(0x55, Multihash::<64>::wrap(0x00, &[byte]).unwrap())
    }

    #[test]
    fn canonical_name_roundtrip() {
        let keypair = Keypair::generate_ed25519();
        let peer_id = keypair.public().to_peer_id();
        let name = canonical_name(peer_id);
        assert!(name.starts_with("k51"));
        assert_eq!(parse_name(&name).unwrap(), peer_id);
        assert_eq!(parse_name(&peer_id.to_string()).unwrap(), peer_id);
    }

    #[test]
    fn signed_record_roundtrip_uses_authenticated_fields() {
        let keypair = Keypair::generate_ed25519();
        let name = keypair.public().to_peer_id();
        let cid = test_cid(1);
        let record = Record::new(
            &keypair,
            format!("/ipfs/{cid}"),
            Utc::now() + chrono::Duration::hours(1),
            7,
            RECORD_TTL,
        )
        .unwrap();
        let decoded = SignedRecord::decode(name, record.encode().unwrap()).unwrap();
        assert_eq!(decoded.deployment_cid(), Some(cid));
        assert_eq!(decoded.sequence(), 7);
        assert_eq!(decoded.ttl(), RECORD_TTL);
    }

    #[test]
    fn official_v2_only_record_is_consumed() {
        // IPNS Record specification vector 6. The record omits every legacy
        // V1 field and therefore exercises the reviewed PR #503 behavior.
        let raw = hex::decode(concat!(
            "42406a7698585e2b170709d4947e148b18b120d76db43515edacf290df96b71e",
            "29c0ca5a16510256814f8255532ec4549ade26f4ca8335f4c41e2cfffeb1ba0",
            "aa5014a78a56354544c1b000001a3185c50006556616c756558242f697066732f",
            "6261666b7161647477676977773633746d70657168657a6c646e357a67696853",
            "657175656e6365006856616c6964697479581b323132332d30382d3134543132",
            "3a31373a30332e3639343035325a6c56616c69646974795479706500"
        ))
        .unwrap();
        let name: PeerId = "12D3KooWGuR5BdSqp23UeoeesuwYwW3ebQ9rZ8aVwfWEDU8kvCYJ"
            .parse()
            .unwrap();
        let record = SignedRecord::decode(name, raw).unwrap();
        assert_eq!(record.sequence(), 0);
        assert_eq!(record.value(), b"/ipfs/bafkqadtwgiww63tmpeqhezldn5zgi");
        assert!(record.deployment_cid().is_some());
    }

    #[cfg(unix)]
    #[test]
    fn record_store_is_private_and_outside_fhs() {
        use std::os::unix::fs::PermissionsExt;

        let directory = tempfile::tempdir().unwrap();
        let keypair = Keypair::generate_ed25519();
        let store = RecordStore::publisher(directory.path(), keypair.public().to_peer_id());
        store.persist(b"signed-record").unwrap();

        assert!(!store.path().starts_with(directory.path().join("fhs")));
        assert_eq!(
            std::fs::metadata(store.path())
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
        assert_eq!(
            std::fs::metadata(store.path().parent().unwrap())
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o700
        );
        assert_eq!(store.load().unwrap().unwrap(), b"signed-record");
    }

    #[tokio::test]
    async fn routing_get_rejects_oversized_record() {
        let (url, server) =
            scripted_server(vec![("200 OK", vec![0_u8; MAX_RECORD_SIZE + 1])]).await;
        let client = RoutingClient::new(url).unwrap();
        let error = client
            .get(Keypair::generate_ed25519().public().to_peer_id())
            .await
            .unwrap_err();
        assert_eq!(error.kind(), RoutingErrorKind::Malformed);
        server.await.unwrap();
    }

    #[tokio::test]
    async fn routing_get_accepts_current_and_newer_missing_record_representations() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            for (status, content_type, body) in [
                ("404 Not Found", IPNS_RECORD_MEDIA_TYPE, b"".as_slice()),
                (
                    "200 OK",
                    "text/plain; charset=utf-8",
                    b"not found".as_slice(),
                ),
                ("200 OK", "application/json", b"{}".as_slice()),
            ] {
                let (mut stream, _) = listener.accept().await.unwrap();
                let _request = read_request(&mut stream).await;
                let response = format!(
                    "HTTP/1.1 {status}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    body.len()
                );
                stream.write_all(response.as_bytes()).await.unwrap();
                stream.write_all(body).await.unwrap();
            }
        });
        let client = RoutingClient::new(format!("http://{address}")).unwrap();
        let name = Keypair::generate_ed25519().public().to_peer_id();

        assert!(matches!(client.get(name).await.unwrap(), Fetch::NotFound));
        assert!(matches!(client.get(name).await.unwrap(), Fetch::NotFound));
        let error = client.get(name).await.unwrap_err();
        assert_eq!(error.kind(), RoutingErrorKind::Malformed);
        server.await.unwrap();
    }

    #[tokio::test]
    async fn stalled_routing_request_times_out_as_temporary() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let _request = read_request(&mut stream).await;
            tokio::time::sleep(Duration::from_secs(1)).await;
        });
        let client =
            RoutingClient::with_timeout(format!("http://{address}"), Duration::from_millis(20))
                .unwrap();
        let error = client.probe().await.unwrap_err();
        assert_eq!(error.kind(), RoutingErrorKind::Temporary);
        server.abort();
    }

    #[tokio::test]
    async fn failed_put_retries_exact_persisted_record() {
        let directory = tempfile::tempdir().unwrap();
        let keypair = Keypair::generate_ed25519();
        let (url, server) = scripted_server(vec![
            ("404 Not Found", Vec::new()),
            ("500 Internal Server Error", Vec::new()),
            ("200 OK", Vec::new()),
        ])
        .await;
        let routing = RoutingClient::new(url).unwrap();
        let mut publisher = Publisher::open(keypair, routing, directory.path()).unwrap();

        assert!(publisher.publish(test_cid(1)).await.is_err());
        let persisted = std::fs::read(publisher.state_path()).unwrap();
        publisher.retry_pending(test_cid(1)).await.unwrap();

        let requests = server.await.unwrap();
        assert_eq!(requests[0].method, "GET");
        assert_eq!(requests[1].method, "PUT");
        assert_eq!(requests[2].method, "PUT");
        assert_eq!(requests[1].body, persisted);
        assert_eq!(requests[2].body, persisted);
    }

    #[tokio::test]
    async fn higher_remote_same_value_reconciles_without_regressing_sequence() {
        let directory = tempfile::tempdir().unwrap();
        let keypair = Keypair::generate_ed25519();
        let name = keypair.public().to_peer_id();
        let local = Record::new(
            &keypair,
            format!("/ipfs/{}", test_cid(1)),
            Utc::now() + chrono::Duration::hours(2),
            3,
            RECORD_TTL,
        )
        .unwrap()
        .encode()
        .unwrap();
        RecordStore::publisher(directory.path(), name)
            .persist(&local)
            .unwrap();
        let desired = test_cid(1);
        let remote = Record::new(
            &keypair,
            format!("/ipfs/{desired}"),
            Utc::now() + chrono::Duration::hours(3),
            7,
            RECORD_TTL,
        )
        .unwrap()
        .encode()
        .unwrap();
        let (url, server) = scripted_server(vec![("200 OK", remote), ("200 OK", Vec::new())]).await;
        let mut restarted =
            Publisher::open(keypair, RoutingClient::new(url).unwrap(), directory.path()).unwrap();
        restarted.publish(desired).await.unwrap();

        let requests = server.await.unwrap();
        let published = SignedRecord::decode(name, requests[1].body.clone()).unwrap();
        assert_eq!(published.sequence(), 7);
        assert_eq!(published.deployment_cid(), Some(desired));
        assert_eq!(
            std::fs::read(restarted.state_path()).unwrap(),
            requests[1].body
        );
    }

    #[tokio::test]
    async fn publisher_fails_on_same_sequence_different_value() {
        let directory = tempfile::tempdir().unwrap();
        let keypair = Keypair::generate_ed25519();
        let name = keypair.public().to_peer_id();
        let local = Record::new(
            &keypair,
            format!("/ipfs/{}", test_cid(1)),
            Utc::now() + chrono::Duration::hours(2),
            5,
            RECORD_TTL,
        )
        .unwrap()
        .encode()
        .unwrap();
        RecordStore::publisher(directory.path(), name)
            .persist(&local)
            .unwrap();
        let remote = Record::new(
            &keypair,
            format!("/ipfs/{}", test_cid(2)),
            Utc::now() + chrono::Duration::hours(3),
            5,
            RECORD_TTL,
        )
        .unwrap()
        .encode()
        .unwrap();
        let (url, server) =
            scripted_server(vec![("200 OK", remote.clone()), ("200 OK", remote)]).await;
        let mut publisher =
            Publisher::open(keypair, RoutingClient::new(url).unwrap(), directory.path()).unwrap();

        let error = publisher.publish(test_cid(3)).await.unwrap_err();
        assert!(format!("{error:#}").contains("equivocation"));
        assert!(!publisher.has_pending_put());
        let retry_error = publisher.publish(test_cid(3)).await.unwrap_err();
        assert!(format!("{retry_error:#}").contains("equivocation"));
        assert_eq!(std::fs::read(publisher.state_path()).unwrap(), local);
        let requests = server.await.unwrap();
        assert_eq!(requests.len(), 2, "equivocation retries must only GET");
        assert!(requests.iter().all(|request| request.method == "GET"));
    }

    #[tokio::test]
    async fn same_value_refresh_preserves_sequence_and_extends_eol() {
        let directory = tempfile::tempdir().unwrap();
        let keypair = Keypair::generate_ed25519();
        let name = keypair.public().to_peer_id();
        let original = Record::new(
            &keypair,
            format!("/ipfs/{}", test_cid(1)),
            Utc::now() + chrono::Duration::hours(1),
            11,
            RECORD_TTL,
        )
        .unwrap()
        .encode()
        .unwrap();
        let original_record = SignedRecord::decode(name, original.clone()).unwrap();
        RecordStore::publisher(directory.path(), name)
            .persist(&original)
            .unwrap();
        let (url, server) =
            scripted_server(vec![("200 OK", original), ("200 OK", Vec::new())]).await;
        let mut publisher =
            Publisher::open(keypair, RoutingClient::new(url).unwrap(), directory.path()).unwrap();
        publisher.refresh(test_cid(1)).await.unwrap();

        let requests = server.await.unwrap();
        let refreshed = SignedRecord::decode(name, requests[1].body.clone()).unwrap();
        assert_eq!(refreshed.sequence(), 11);
        assert_eq!(refreshed.deployment_cid(), Some(test_cid(1)));
        assert!(refreshed.eol() > original_record.eol());
    }

    #[tokio::test]
    async fn higher_remote_different_value_is_adopted_and_blocks_publication() {
        let directory = tempfile::tempdir().unwrap();
        let keypair = Keypair::generate_ed25519();
        let name = keypair.public().to_peer_id();
        let desired = test_cid(1);
        let local = Record::new(
            &keypair,
            format!("/ipfs/{desired}"),
            Utc::now() + chrono::Duration::hours(2),
            3,
            RECORD_TTL,
        )
        .unwrap()
        .encode()
        .unwrap();
        RecordStore::publisher(directory.path(), name)
            .persist(&local)
            .unwrap();
        let remote = Record::new(
            &keypair,
            format!("/ipfs/{}", test_cid(2)),
            Utc::now() + chrono::Duration::hours(3),
            7,
            RECORD_TTL,
        )
        .unwrap()
        .encode()
        .unwrap();
        let remote_floor = remote.clone();
        let (url, server) = scripted_server(vec![("200 OK", remote)]).await;
        let mut publisher =
            Publisher::open(keypair, RoutingClient::new(url).unwrap(), directory.path()).unwrap();

        let error = publisher.refresh(desired).await.unwrap_err();

        let requests = server.await.unwrap();
        assert_eq!(requests.len(), 1, "single-writer conflict must not PUT");
        assert_eq!(requests[0].method, "GET");
        assert!(!is_temporary_routing_failure(&error));
        assert!(format!("{error:#}").contains("single-writer conflict"));
        assert!(format!("{error:#}").contains("no record was published"));
        assert_eq!(publisher.current().unwrap().sequence(), 7);
        assert_eq!(
            publisher.current().unwrap().deployment_cid(),
            Some(test_cid(2))
        );
        assert_eq!(std::fs::read(publisher.state_path()).unwrap(), remote_floor);
        assert!(!publisher.has_pending_put());
    }

    #[tokio::test]
    async fn refresh_does_not_regress_a_later_eol_floor() {
        let directory = tempfile::tempdir().unwrap();
        let keypair = Keypair::generate_ed25519();
        let name = keypair.public().to_peer_id();
        let desired = test_cid(1);
        let future = Record::new(
            &keypair,
            format!("/ipfs/{desired}"),
            Utc::now() + chrono::Duration::hours(72),
            11,
            RECORD_TTL,
        )
        .unwrap()
        .encode()
        .unwrap();
        RecordStore::publisher(directory.path(), name)
            .persist(&future)
            .unwrap();
        let (url, server) =
            scripted_server(vec![("200 OK", future.clone()), ("200 OK", Vec::new())]).await;
        let mut publisher =
            Publisher::open(keypair, RoutingClient::new(url).unwrap(), directory.path()).unwrap();

        publisher.refresh(desired).await.unwrap();

        let requests = server.await.unwrap();
        assert_eq!(requests[1].body, future);
        assert_eq!(std::fs::read(publisher.state_path()).unwrap(), future);
    }

    #[tokio::test]
    async fn startup_refreshes_a_matching_record_before_initial_delay() {
        let directory = tempfile::tempdir().unwrap();
        let keypair = Keypair::generate_ed25519();
        let name = keypair.public().to_peer_id();
        let desired = test_cid(1);
        let near_expiry = Record::new(
            &keypair,
            format!("/ipfs/{desired}"),
            Utc::now() + chrono::Duration::seconds(30),
            4,
            RECORD_TTL,
        )
        .unwrap()
        .encode()
        .unwrap();
        RecordStore::publisher(directory.path(), name)
            .persist(&near_expiry)
            .unwrap();
        let (url, server) = scripted_server(vec![
            ("200 OK", near_expiry.clone()),
            ("200 OK", Vec::new()),
        ])
        .await;
        let mut publisher =
            Publisher::open(keypair, RoutingClient::new(url).unwrap(), directory.path()).unwrap();

        publisher.publish(desired).await.unwrap();

        let requests = server.await.unwrap();
        let refreshed = SignedRecord::decode(name, requests[1].body.clone()).unwrap();
        assert_eq!(refreshed.sequence(), 4);
        assert_eq!(refreshed.deployment_cid(), Some(desired));
        assert!(refreshed.eol() > SignedRecord::decode(name, near_expiry).unwrap().eol());
    }
}
