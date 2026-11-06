//! Official release publication through one infrastructure-owned release journal.
//!
//! Kubo owns the discovery key. The signed release descriptor uses the separate
//! #412 release authority. Kubo acknowledgement is not proof of discoverability.

use std::{
    io::Read,
    path::{Path, PathBuf},
    time::Duration,
};

use anyhow::{ensure, Context, Result};
use async_trait::async_trait;
use cid::Cid;
use ipfs::release::{
    KuboRetention, Owner, Publication, PublicationIntent, Publisher, ReleaseState,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::package::Package;
use ww::release::{auth::TrustedKeys, descriptor::AuthenticatedRelease};

pub const MAX_REQUEST_BYTES: usize = 512;
const MAX_RESPONSE_BYTES: usize = 16 * 1024;
const MAX_CONFIG_BYTES: usize = 8 * 1024;
pub const CONFIG_PATH: &str = "/etc/wetware/release-publisher.json";

/// The historical transport field `descriptor` identifies the complete candidate
/// envelope, which links the authenticated descriptor and all artifact stages.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Request {
    pub version: u32,
    pub request_id: String,
    pub descriptor: String,
}

impl Request {
    pub fn new(root: Cid) -> Self {
        let descriptor = root.to_string();
        let digest = Sha256::digest(descriptor.as_bytes());
        Self {
            version: 1,
            request_id: hex::encode(&digest[..16]),
            descriptor,
        }
    }

    pub fn parse(bytes: &[u8]) -> Result<Self> {
        ensure!(
            bytes.len() <= MAX_REQUEST_BYTES,
            "publisher request exceeds byte limit"
        );
        let request: Self = serde_json::from_slice(bytes).context("invalid publisher request")?;
        ensure!(
            request.version == 1,
            "unsupported publisher request version"
        );
        let root = request.root()?;
        ensure!(
            request.request_id == Self::new(root).request_id,
            "request ID does not bind candidate reference"
        );
        Ok(request)
    }

    pub fn root(&self) -> Result<Cid> {
        let root = ipfs::cid_identity::parse_cid(&self.descriptor)?;
        ensure!(
            root.version() == cid::Version::V1
                && root.codec() == 0x70
                && root.hash().code() == 0x12
                && root.hash().size() == 32
                && root.to_string() == self.descriptor,
            "candidate must be a canonical CIDv1 SHA-256 UnixFS root"
        );
        Ok(root)
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    pub version: u32,
    pub domain: String,
    pub channel: String,
    pub key: String,
    pub journal: PathBuf,
    pub trusted_keys: PathBuf,
    pub operation_timeout_seconds: u64,
}

impl Config {
    pub fn load(path: &Path) -> Result<Self> {
        let metadata = std::fs::symlink_metadata(path)?;
        ensure!(
            metadata.file_type().is_file(),
            "publisher configuration must be a regular file"
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            ensure!(
                metadata.permissions().mode() & 0o022 == 0,
                "publisher configuration must not be group or world writable"
            );
        }
        let config: Self = serde_json::from_slice(&read_bounded(path, MAX_CONFIG_BYTES)?)?;
        config.validate()?;
        Ok(config)
    }

    fn validate(&self) -> Result<()> {
        ensure!(
            self.version == 1,
            "unsupported publisher configuration version"
        );
        ensure!(
            self.channel == ww::ipns::canonical_name(ww::ipns::parse_name(&self.channel)?),
            "publisher channel must use canonical Base36 IPNS identity"
        );
        ensure!(
            !self.key.is_empty()
                && self.key.len() <= 64
                && self
                    .key
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b"_-".contains(&b)),
            "invalid configured Kubo key alias"
        );
        ensure!(
            self.key != "self",
            "official publication must use a dedicated Kubo key"
        );
        ensure!(
            self.journal.is_absolute() && self.trusted_keys.is_absolute(),
            "publisher state and trust paths must be absolute"
        );
        ensure!(
            (1..=120).contains(&self.operation_timeout_seconds),
            "invalid publisher operation timeout"
        );
        Ok(())
    }
}

pub fn read_bounded(path: &Path, limit: usize) -> Result<Vec<u8>> {
    let mut bytes = Vec::new();
    std::fs::File::open(path)?
        .take(limit as u64 + 1)
        .read_to_end(&mut bytes)?;
    ensure!(
        bytes.len() <= limit,
        "file exceeds byte limit: {}",
        path.display()
    );
    Ok(bytes)
}

/// One Kubo endpoint shared by retention, acquisition, and discovery requests.
/// Unix sockets are mandatory for the installed worker; TCP is available only
/// through explicit CLI configuration for disposable qualification and CI seeds.
pub struct KuboApi {
    http: reqwest::Client,
    base: String,
    content: ipfs::HttpClient,
}

impl KuboApi {
    pub fn new(endpoint: &str, timeout: Duration) -> Result<Self> {
        ensure!(!timeout.is_zero(), "Kubo request timeout must be positive");
        let mut builder = reqwest::Client::builder()
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .timeout(timeout);
        let base = if let Some(path) = endpoint.strip_prefix("/unix") {
            ensure!(
                path.starts_with('/') && path.len() > 1,
                "invalid Kubo Unix socket path"
            );
            #[cfg(unix)]
            {
                builder = builder.unix_socket(path);
            }
            #[cfg(not(unix))]
            {
                anyhow::bail!("Kubo Unix socket transport requires Unix");
            }
            "http://localhost".to_owned()
        } else {
            let url = reqwest::Url::parse(endpoint).context("invalid Kubo endpoint")?;
            ensure!(
                matches!(url.scheme(), "http" | "https")
                    && url.username().is_empty()
                    && url.password().is_none()
                    && url.query().is_none()
                    && url.fragment().is_none()
                    && url.path() == "/",
                "invalid Kubo endpoint components"
            );
            endpoint.trim_end_matches('/').to_owned()
        };
        let http = builder.build()?;
        let content = ipfs::HttpClient::with_http_client(base.clone(), http.clone());
        Ok(Self {
            http,
            base,
            content,
        })
    }

    pub fn content(&self) -> &ipfs::HttpClient {
        &self.content
    }

    async fn response(response: reqwest::Response) -> Result<Vec<u8>> {
        ensure!(
            response.status().is_success(),
            "Kubo refused publication request with HTTP {}",
            response.status()
        );
        ensure!(
            response
                .headers()
                .get_all("X-Stream-Error")
                .iter()
                .all(|h| h.as_bytes().is_empty()),
            "Kubo publication stream failed"
        );
        ensure!(
            response
                .content_length()
                .is_none_or(|n| n <= MAX_RESPONSE_BYTES as u64),
            "Kubo publication response exceeds limit"
        );
        let mut response = response;
        let mut bytes = Vec::new();
        while let Some(chunk) = response.chunk().await? {
            ensure!(
                bytes.len() + chunk.len() <= MAX_RESPONSE_BYTES,
                "Kubo publication response exceeds limit"
            );
            bytes.extend_from_slice(&chunk);
        }
        Ok(bytes)
    }

    async fn check_identity(&self, key: &str, channel: &str) -> Result<()> {
        #[derive(Deserialize)]
        struct Keys {
            #[serde(rename = "Keys")]
            keys: Vec<Key>,
        }
        #[derive(Deserialize)]
        struct Key {
            #[serde(rename = "Name")]
            name: String,
            #[serde(rename = "Id")]
            id: String,
        }
        let response = self
            .http
            .post(format!("{}/api/v0/key/list", self.base))
            .send()
            .await?;
        let keys: Keys = serde_json::from_slice(&Self::response(response).await?)?;
        let matches: Vec<_> = keys.keys.iter().filter(|entry| entry.name == key).collect();
        ensure!(
            matches.len() == 1,
            "configured publication key is missing or ambiguous"
        );
        ensure!(
            ww::ipns::parse_name(&matches[0].id)? == ww::ipns::parse_name(channel)?,
            "configured Kubo key does not match release channel"
        );
        Ok(())
    }

    async fn announce(&self, key: &str, channel: &str, root: Cid) -> Result<()> {
        self.check_identity(key, channel).await?;
        let value = format!("/ipfs/{root}");
        let response = self
            .http
            .post(format!("{}/api/v0/name/publish", self.base))
            .query(&[
                ("arg", value.as_str()),
                ("key", key),
                ("resolve", "true"),
                ("lifetime", "48h"),
                ("ttl", "5m"),
            ])
            .send()
            .await
            .context("Kubo announcement acknowledgement unknown")?;
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Published {
            #[serde(rename = "Name")]
            name: String,
            #[serde(rename = "Value")]
            value: String,
        }
        let published: Published = serde_json::from_slice(&Self::response(response).await?)
            .context("invalid Kubo publication acknowledgement")?;
        ensure!(
            published.value == value
                && ww::ipns::parse_name(&published.name)? == ww::ipns::parse_name(channel)?,
            "Kubo acknowledged another publication"
        );
        Ok(())
    }
}

/// Immutable delivery request. The journal is trusted local authority; the
/// descriptor sequence was authenticated before these fields were committed.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Announcement {
    version: u32,
    domain: String,
    channel: String,
    key: String,
    publication: Publication,
    descriptor: Cid,
    release_sequence: u64,
    source_revision: String,
}

impl Announcement {
    fn decode(raw: &[u8]) -> Result<Self> {
        ensure!(
            raw.len() <= ipfs::release::MAX_ANNOUNCEMENT_BYTES,
            "announcement exceeds byte limit"
        );
        let announcement: Self = serde_json::from_slice(raw)?;
        ensure!(
            announcement.version == 1
                && announcement.release_sequence > 0
                && announcement.publication.generation > 0,
            "invalid publication announcement schema"
        );
        ensure!(
            serde_json::to_vec(&announcement)? == raw,
            "noncanonical publication announcement"
        );
        Ok(announcement)
    }

    fn matches(
        &self,
        intent: &PublicationIntent,
        domain: &str,
        channel: &str,
        key: &str,
    ) -> Result<()> {
        ensure!(
            self.publication == intent.next
                && self.domain == domain
                && self.channel == channel
                && self.key == key,
            "announcement does not match committed publication or configuration"
        );
        Ok(())
    }
}

struct KuboPublisher<'a> {
    api: &'a KuboApi,
    config: &'a Config,
    release: &'a AuthenticatedRelease,
    root: Cid,
    previous: Option<Announcement>,
}

impl KuboPublisher<'_> {
    fn validate(&self, intent: &PublicationIntent) -> Result<()> {
        let descriptor = self.release.descriptor();
        ensure!(
            descriptor.channel == self.config.channel && intent.next.root == self.root,
            "candidate channel or root differs from publication intent"
        );
        ensure!(
            intent.previous.as_ref() == self.previous.as_ref().map(|a| &a.publication),
            "previous publication evidence differs from journal"
        );
        if let Some(previous) = &self.previous {
            ensure!(
                previous.domain == self.config.domain
                    && previous.channel == self.config.channel
                    && previous.key == self.config.key,
                "previous announcement configuration differs"
            );
            ensure!(
                descriptor.sequence > previous.release_sequence,
                "release sequence must advance committed channel state"
            );
        }
        Ok(())
    }
}

#[async_trait]
impl Publisher for KuboPublisher<'_> {
    fn domain(&self) -> &str {
        &self.config.domain
    }

    async fn prepare(&self, intent: &PublicationIntent) -> Result<Vec<u8>> {
        self.validate(intent)?;
        self.api
            .check_identity(&self.config.key, &self.config.channel)
            .await?;
        let descriptor = self.release.descriptor();
        Ok(serde_json::to_vec(&Announcement {
            version: 1,
            domain: self.config.domain.clone(),
            channel: self.config.channel.clone(),
            key: self.config.key.clone(),
            publication: intent.next.clone(),
            descriptor: self.release.descriptor_cid(),
            release_sequence: descriptor.sequence,
            source_revision: descriptor.source.revision.clone(),
        })?)
    }

    async fn publish(&self, intent: &PublicationIntent, raw: &[u8]) -> Result<()> {
        self.validate(intent)?;
        let announcement = Announcement::decode(raw)?;
        announcement.matches(
            intent,
            &self.config.domain,
            &self.config.channel,
            &self.config.key,
        )?;
        ensure!(
            announcement.descriptor == self.release.descriptor_cid()
                && announcement.release_sequence == self.release.descriptor().sequence
                && announcement.source_revision == self.release.descriptor().source.revision,
            "persisted announcement differs from authenticated descriptor"
        );
        self.api
            .announce(
                &announcement.key,
                &announcement.channel,
                announcement.publication.root,
            )
            .await
    }
}

#[derive(Debug, Serialize)]
pub struct Outcome {
    pub version: u32,
    pub request_id: String,
    pub descriptor: String,
    pub local_committed: bool,
    pub delivery_acknowledged: bool,
    pub root: String,
    pub generation: u64,
    pub release_sequence: u64,
}

/// Process one authenticated candidate under the existing durable owner lock.
/// A delivery failure after commitment returns an explicit pending result.
pub async fn publish_request(
    request: Request,
    config: &Config,
    endpoint: &str,
    local_package: Option<&Path>,
) -> Result<Outcome> {
    config.validate()?;
    let requested = Request::parse(&serde_json::to_vec(&request)?)?.root()?;
    let keys = TrustedKeys::from_bytes(&read_bounded(&config.trusted_keys, 122)?)?;
    let mut owner = Owner::open(&config.journal, &config.domain)?;
    let api = KuboApi::new(
        endpoint,
        Duration::from_secs(config.operation_timeout_seconds),
    )?;
    let scratch = config.journal.join("acquisition");
    let package = if let Some(path) = local_package {
        Package::open(path)?
    } else {
        // This fixed private path contains disposable content, never authority.
        // Holding Owner's lock bounds scratch across crashes and retries.
        match std::fs::symlink_metadata(&scratch) {
            Ok(metadata) => {
                ensure!(
                    metadata.file_type().is_dir(),
                    "publisher acquisition path is not a directory"
                );
                std::fs::remove_dir_all(&scratch)?;
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
        Package::fetch(api.content(), requested, &scratch).await?
    };
    ensure!(
        package.root() == requested,
        "candidate package root does not match request"
    );
    let release = package.authenticate(&keys)?;
    ensure!(
        release.descriptor().channel == config.channel,
        "authenticated release belongs to another channel"
    );
    ensure!(
        release.descriptor().hosts.len() == 5,
        "official release requires all five supported Host targets"
    );

    let current = owner.current()?;
    let same_current = current.as_ref().is_some_and(|p| p.root == requested);
    let expected = if same_current {
        owner
            .releases()?
            .iter()
            .find(|r| Some(&r.publication) == current.as_ref())
            .context("current publication is missing")?
            .previous
            .clone()
    } else {
        current.clone()
    };
    let previous = expected
        .as_ref()
        .map(|p| -> Result<_> {
            let release = owner
                .releases()?
                .iter()
                .find(|r| &r.publication == p)
                .context("previous release is missing")?;
            Announcement::decode(
                release
                    .announcement
                    .as_deref()
                    .context("previous release has no announcement")?,
            )
        })
        .transpose()?;
    let publisher = KuboPublisher {
        api: &api,
        config,
        release: &release,
        root: requested,
        previous,
    };
    // Validate freshness before reserving a candidate or retaining any stage.
    let proposed = PublicationIntent {
        previous: expected.clone(),
        next: Publication {
            root: requested,
            generation: 1,
        },
    };
    publisher.validate(&proposed)?;

    let generation = if same_current {
        current.context("missing current publication")?.generation
    } else {
        owner.begin(requested, expected)?
    };
    let state = owner
        .releases()?
        .iter()
        .find(|r| r.publication.generation == generation)
        .context("reserved release is missing")?
        .state;
    if state == ReleaseState::Candidate {
        let retention = KuboRetention::new(
            api.content().clone(),
            config.domain.clone(),
            Duration::from_secs(config.operation_timeout_seconds),
        )?;
        package.retain(&mut owner, generation, &retention).await?;
        owner.finish_retention(generation)?;
    }
    owner.commit_publication(generation, &publisher).await?;
    let delivery_acknowledged = owner.publish(generation, &publisher).await.is_ok();
    Ok(Outcome {
        version: 1,
        request_id: request.request_id,
        descriptor: request.descriptor,
        local_committed: true,
        delivery_acknowledged,
        root: requested.to_string(),
        generation,
        release_sequence: release.descriptor().sequence,
    })
}

#[cfg(test)]
#[path = "publisher/tests.rs"]
mod tests;
