//! Authenticated software release descriptors and consumer freshness state.
//!
//! The Ed25519 manifest authority authenticates descriptor identity through
//! `RELEASE-DESCRIPTOR`. IPNS only discovers an immutable transport root. Its
//! transport sequence and the signed release sequence are independent floors.
//! A new consumer has no floor and cannot prove global freshness through IPNS.
//! Historical selection uses [`AuthenticatedRelease`] without a [`Consumer`].

use std::collections::{BTreeMap, BTreeSet};
use std::fs::{File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use anyhow::{bail, ensure, Context, Result};
use base64::{engine::general_purpose::STANDARD, Engine as _};
use bytes::Bytes;
use chrono::Utc;
use cid::{multihash::Multihash, Cid, Version};
use ipfs::map::{Map, Value};
use ipld_dagpb::PbNode;
use libp2p_identity::PeerId;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::auth::{TrustedKeys, MAX_MANIFEST_BYTES};
use ipfs::ipns::{canonical_name, parse_name, SignedRecord, MAX_RECORD_SIZE};

pub const COMPATIBILITY_PROFILE: &str = "wetware-host-pid0-p3-v1";
pub const BINDING_PATH: &str = "RELEASE-DESCRIPTOR";
pub const DESCRIPTOR_LINK_NAME: &str = "release-descriptor";
pub const SOURCE_REPOSITORY: &str = "https://github.com/wetware/ww";
const SCHEMA_VERSION: u64 = 1;
const MAX_ROOT_BYTES: usize = 2 * 1024 * 1024;
const MAX_STATE_BYTES: usize = 8 * 1024 * 1024;
const MAX_BINDING_BYTES: usize = 128;

/// A file's storage identity and reconstructed byte identity are distinct.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Artifact {
    pub path: String,
    pub root: Cid,
    pub sha256: [u8; 32],
    pub size: u64,
}

/// Producer assertions authenticated by the existing release signer.
/// These assertions do not establish reproducible-build equivalence.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Provenance {
    pub repository: String,
    pub revision: String,
    pub builder: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct HostArtifact {
    pub target: String,
    pub artifact: Artifact,
    pub provenance: Provenance,
    pub compatibility: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DeploymentArtifact {
    pub root: Cid,
    pub kernel: Artifact,
    pub status: Artifact,
    pub compatibility: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Descriptor {
    pub channel: String,
    pub sequence: u64,
    pub version: String,
    pub source: Provenance,
    pub hosts: Vec<HostArtifact>,
    pub deployment: DeploymentArtifact,
}

pub fn platform_path(target: &str) -> Result<&'static str> {
    match target {
        "x86_64-unknown-linux-gnu" => Ok("bin/ww/linux/x86_64/ww"),
        "aarch64-unknown-linux-gnu" => Ok("bin/ww/linux/aarch64/ww"),
        "aarch64-apple-darwin" => Ok("bin/ww/macos/aarch64/ww"),
        "x86_64-apple-darwin" => Ok("bin/ww/macos/x86_64/ww"),
        "x86_64-unknown-linux-musl" => Ok("bin/ww/linux/musl-x86_64/ww"),
        _ => bail!("unsupported release platform: {target}"),
    }
}

fn sha256_cid(cid: Cid, codecs: &[u64]) -> Result<()> {
    ensure!(
        codecs.contains(&cid.codec()),
        "unsupported release artifact codec"
    );
    ensure!(
        cid.hash().code() == 0x12 && cid.hash().size() == 32,
        "release artifact CID must use SHA-256"
    );
    Ok(())
}

fn descriptor_cid(bytes: &[u8]) -> Cid {
    Cid::new_v1(
        0x71,
        Multihash::wrap(0x12, &Sha256::digest(bytes)).expect("SHA-256"),
    )
}

fn bounded_text(value: &str, max: usize) -> Result<()> {
    ensure!(
        !value.is_empty()
            && value.len() <= max
            && value.is_ascii()
            && value.bytes().all(|byte| byte.is_ascii_graphic()),
        "invalid bounded release text"
    );
    Ok(())
}

impl Provenance {
    fn validate(&self) -> Result<()> {
        ensure!(
            self.repository == SOURCE_REPOSITORY,
            "unsupported release source repository"
        );
        ensure!(
            self.revision.len() == 40
                && self
                    .revision
                    .bytes()
                    .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte)),
            "release build revision must be 40 lowercase hexadecimal digits"
        );
        bounded_text(&self.builder, 256)
    }
}

impl Artifact {
    fn validate(&self, path: &str) -> Result<()> {
        ensure!(self.path == path, "unexpected release artifact path");
        sha256_cid(self.root, &[0x55, 0x70])?;
        ensure!(
            self.size > 0 && self.size <= i64::MAX as u64,
            "invalid release artifact size"
        );
        if self.root.codec() == 0x55 {
            ensure!(
                self.root.hash().digest() == self.sha256,
                "raw artifact CID and byte digest disagree"
            );
        }
        Ok(())
    }

    /// Verify reconstructed bytes before installation or execution.
    pub fn verify(&self, bytes: &[u8]) -> Result<()> {
        ensure!(
            bytes.len() as u64 == self.size,
            "release artifact size mismatch: {}",
            self.path
        );
        ensure!(
            Sha256::digest(bytes).as_slice() == self.sha256,
            "release artifact SHA-256 mismatch: {}",
            self.path
        );
        Ok(())
    }
}

impl Descriptor {
    pub fn validate(&self) -> Result<()> {
        ensure!(
            canonical_name(parse_name(&self.channel)?) == self.channel,
            "release channel must be a canonical IPNS name"
        );
        ensure!(
            self.sequence > 0 && self.sequence <= i64::MAX as u64,
            "invalid release sequence"
        );
        bounded_text(&self.version, 128)?;
        self.source.validate()?;
        ensure!(
            !self.hosts.is_empty() && self.hosts.len() <= 5,
            "invalid release platform count"
        );
        let mut targets = BTreeSet::new();
        for host in &self.hosts {
            ensure!(targets.insert(&host.target), "duplicate release platform");
            host.artifact.validate(platform_path(&host.target)?)?;
            host.provenance.validate()?;
            ensure!(
                host.compatibility == COMPATIBILITY_PROFILE,
                "unsupported Host compatibility profile"
            );
        }
        sha256_cid(self.deployment.root, &[0x70])?;
        self.deployment.kernel.validate("fhs/boot/kernel.wasm")?;
        self.deployment.status.validate("fhs/bin/status.wasm")?;
        ensure!(
            self.deployment.compatibility == COMPATIBILITY_PROFILE,
            "unsupported deployment compatibility profile"
        );
        Ok(())
    }

    pub fn encode(&self) -> Result<Vec<u8>> {
        self.validate()?;
        let mut hosts: Vec<_> = self.hosts.iter().collect();
        hosts.sort_by(|a, b| a.target.cmp(&b.target));
        Map::from_entries(entries([
            ("schema", integer(SCHEMA_VERSION)),
            ("channel", text(&self.channel)),
            ("sequence", integer(self.sequence)),
            ("version", text(&self.version)),
            ("source", provenance_value(&self.source)),
            (
                "hosts",
                Value::List(hosts.into_iter().map(host_value).collect()),
            ),
            (
                "deployment",
                Value::Map(entries([
                    ("root", Value::Link(self.deployment.root)),
                    ("kernel", artifact_value(&self.deployment.kernel)),
                    ("status", artifact_value(&self.deployment.status)),
                    ("compatibility", text(&self.deployment.compatibility)),
                ])),
            ),
        ]))?
        .encode()
    }

    pub fn cid(&self) -> Result<Cid> {
        Ok(descriptor_cid(&self.encode()?))
    }

    pub fn binding(&self) -> Result<Vec<u8>> {
        Ok(format!("{}\n", self.cid()?).into_bytes())
    }

    pub fn decode(cid: Cid, bytes: &[u8]) -> Result<Self> {
        ensure!(
            bytes.len() <= ipfs::map::MAX_BLOCK_BYTES,
            "release descriptor exceeds byte limit"
        );
        ensure!(
            cid.version() == Version::V1 && cid == descriptor_cid(bytes),
            "release descriptor CID does not match bytes or profile"
        );
        let map = Map::decode(bytes)?;
        let mut fields: BTreeMap<_, _> =
            map.iter().map(|(k, v)| (k.to_owned(), v.clone())).collect();
        ensure!(
            take_number(&mut fields, "schema")? == SCHEMA_VERSION,
            "unsupported release descriptor schema"
        );
        let channel = take_text(&mut fields, "channel")?;
        let sequence = take_number(&mut fields, "sequence")?;
        let version = take_text(&mut fields, "version")?;
        let source = parse_provenance(take(&mut fields, "source")?)?;
        let Value::List(hosts) = take(&mut fields, "hosts")? else {
            bail!("release hosts must be a list")
        };
        ensure!(hosts.len() <= 5, "release platform count exceeds limit");
        let hosts = hosts
            .into_iter()
            .map(parse_host)
            .collect::<Result<Vec<_>>>()?;
        let mut deployment = as_map(take(&mut fields, "deployment")?)?;
        let deployment_value = DeploymentArtifact {
            root: take_link(&mut deployment, "root")?,
            kernel: parse_artifact(take(&mut deployment, "kernel")?)?,
            status: parse_artifact(take(&mut deployment, "status")?)?,
            compatibility: take_text(&mut deployment, "compatibility")?,
        };
        empty(&deployment)?;
        empty(&fields)?;
        let descriptor = Self {
            channel,
            sequence,
            version,
            source,
            hosts,
            deployment: deployment_value,
        };
        descriptor.validate()?;
        ensure!(
            descriptor.encode()? == bytes,
            "noncanonical release descriptor"
        );
        Ok(descriptor)
    }
}

fn entries<const N: usize>(items: [(&str, Value); N]) -> BTreeMap<String, Value> {
    items
        .into_iter()
        .map(|(key, value)| (key.to_owned(), value))
        .collect()
}
fn text(value: &str) -> Value {
    Value::Text(value.to_owned())
}
fn integer(value: u64) -> Value {
    Value::Integer(value as i64)
}
fn artifact_value(value: &Artifact) -> Value {
    Value::Map(entries([
        ("path", text(&value.path)),
        ("root", Value::Link(value.root)),
        ("sha256", Value::Bytes(value.sha256.to_vec())),
        ("size", integer(value.size)),
    ]))
}
fn provenance_value(value: &Provenance) -> Value {
    Value::Map(entries([
        ("repository", text(&value.repository)),
        ("revision", text(&value.revision)),
        ("builder", text(&value.builder)),
    ]))
}
fn host_value(value: &HostArtifact) -> Value {
    Value::Map(entries([
        ("target", text(&value.target)),
        ("artifact", artifact_value(&value.artifact)),
        ("provenance", provenance_value(&value.provenance)),
        ("compatibility", text(&value.compatibility)),
    ]))
}
fn take(fields: &mut BTreeMap<String, Value>, key: &str) -> Result<Value> {
    fields
        .remove(key)
        .with_context(|| format!("missing release field: {key}"))
}
fn as_map(value: Value) -> Result<BTreeMap<String, Value>> {
    let Value::Map(fields) = value else {
        bail!("release field must be a map")
    };
    Ok(fields)
}
fn take_text(fields: &mut BTreeMap<String, Value>, key: &str) -> Result<String> {
    let Value::Text(value) = take(fields, key)? else {
        bail!("release field {key} must be text")
    };
    Ok(value)
}
fn take_number(fields: &mut BTreeMap<String, Value>, key: &str) -> Result<u64> {
    let Value::Integer(value) = take(fields, key)? else {
        bail!("release field {key} must be an integer")
    };
    u64::try_from(value).context("negative release integer")
}
fn take_link(fields: &mut BTreeMap<String, Value>, key: &str) -> Result<Cid> {
    let Value::Link(value) = take(fields, key)? else {
        bail!("release field {key} must be an IPLD link")
    };
    Ok(value)
}
fn empty(fields: &BTreeMap<String, Value>) -> Result<()> {
    ensure!(fields.is_empty(), "unknown release descriptor fields");
    Ok(())
}
fn parse_artifact(value: Value) -> Result<Artifact> {
    let mut fields = as_map(value)?;
    let path = take_text(&mut fields, "path")?;
    let root = take_link(&mut fields, "root")?;
    let Value::Bytes(digest) = take(&mut fields, "sha256")? else {
        bail!("release SHA-256 must be bytes")
    };
    let sha256 = digest
        .try_into()
        .map_err(|_| anyhow::anyhow!("release SHA-256 must contain 32 bytes"))?;
    let size = take_number(&mut fields, "size")?;
    empty(&fields)?;
    Ok(Artifact {
        path,
        root,
        sha256,
        size,
    })
}
fn parse_provenance(value: Value) -> Result<Provenance> {
    let mut fields = as_map(value)?;
    let result = Provenance {
        repository: take_text(&mut fields, "repository")?,
        revision: take_text(&mut fields, "revision")?,
        builder: take_text(&mut fields, "builder")?,
    };
    empty(&fields)?;
    Ok(result)
}
fn parse_host(value: Value) -> Result<HostArtifact> {
    let mut fields = as_map(value)?;
    let result = HostArtifact {
        target: take_text(&mut fields, "target")?,
        artifact: parse_artifact(take(&mut fields, "artifact")?)?,
        provenance: parse_provenance(take(&mut fields, "provenance")?)?,
        compatibility: take_text(&mut fields, "compatibility")?,
    };
    empty(&fields)?;
    Ok(result)
}

/// Parse exactly canonical descriptor CID text followed by one LF.
pub fn binding_cid(bytes: &[u8]) -> Result<Cid> {
    ensure!(
        bytes.len() <= MAX_BINDING_BYTES,
        "release descriptor binding exceeds limit"
    );
    let value = std::str::from_utf8(bytes)?
        .strip_suffix('\n')
        .context("binding lacks final LF")?;
    let cid = ipfs::cid_identity::parse_cid(value)?;
    ensure!(
        cid.version() == Version::V1 && cid.codec() == 0x71 && cid.to_string() == value,
        "binding must contain one canonical CIDv1 DAG-CBOR identity"
    );
    sha256_cid(cid, &[0x71])?;
    Ok(cid)
}

/// Only authentication constructs this type. The transport root adds no authority.
pub struct AuthenticatedRelease {
    descriptor: Descriptor,
    cid: Cid,
    manifest: Vec<u8>,
    signature: Vec<u8>,
    binding: Vec<u8>,
    descriptor_bytes: Vec<u8>,
}

impl AuthenticatedRelease {
    pub fn authenticate(
        keys: &TrustedKeys,
        manifest: &[u8],
        signature: &[u8],
        binding: &[u8],
        descriptor_cid: Cid,
        descriptor_bytes: &[u8],
    ) -> Result<Self> {
        ensure!(
            binding.len() <= MAX_BINDING_BYTES,
            "release binding exceeds limit"
        );
        let authenticated = keys.authenticate(manifest, signature)?;
        authenticated.verify_artifact(BINDING_PATH, binding)?;
        ensure!(
            binding_cid(binding)? == descriptor_cid,
            "authenticated descriptor binding mismatch"
        );
        let descriptor = Descriptor::decode(descriptor_cid, descriptor_bytes)?;
        for artifact in descriptor
            .hosts
            .iter()
            .map(|host| &host.artifact)
            .chain([&descriptor.deployment.kernel, &descriptor.deployment.status])
        {
            ensure!(
                authenticated.sha256_for(&artifact.path)? == &artifact.sha256,
                "descriptor and signed manifest disagree: {}",
                artifact.path
            );
        }
        Ok(Self {
            descriptor,
            cid: descriptor_cid,
            manifest: manifest.to_vec(),
            signature: signature.to_vec(),
            binding: binding.to_vec(),
            descriptor_bytes: descriptor_bytes.to_vec(),
        })
    }
    pub fn descriptor(&self) -> &Descriptor {
        &self.descriptor
    }
    pub fn descriptor_cid(&self) -> Cid {
        self.cid
    }
    pub fn select(&self, target: &str, compatibility: &str) -> Result<Selection> {
        ensure!(
            compatibility == COMPATIBILITY_PROFILE
                && self.descriptor.deployment.compatibility == compatibility,
            "release deployment is incompatible with consumer"
        );
        let host = self
            .descriptor
            .hosts
            .iter()
            .find(|host| host.target == target)
            .context("release has no artifact for requested platform")?;
        ensure!(
            host.compatibility == compatibility,
            "release Host is incompatible with deployment"
        );
        Ok(Selection {
            host: host.clone(),
            deployment: self.descriptor.deployment.clone(),
        })
    }
}

/// Authenticated selection metadata. Verify fetched bytes before consumption.
#[derive(Clone, Debug)]
pub struct Selection {
    host: HostArtifact,
    deployment: DeploymentArtifact,
}
impl Selection {
    pub fn host(&self) -> &HostArtifact {
        &self.host
    }
    pub fn deployment(&self) -> &DeploymentArtifact {
        &self.deployment
    }
    pub fn verify_host(&self, bytes: &[u8]) -> Result<()> {
        self.host.artifact.verify(bytes)
    }
    pub fn verify_deployment(&self, kernel: &[u8], status: &[u8]) -> Result<()> {
        self.deployment.kernel.verify(kernel)?;
        self.deployment.status.verify(status)
    }
}

/// Check the transport root's direct descriptor link against authenticated identity.
/// Selection reads artifact CIDs from the descriptor; unsigned tree paths cannot
/// change those identities. Legacy path parity is checked by the publisher.
pub fn verify_transport_root(root: Cid, bytes: &[u8], descriptor: Cid) -> Result<()> {
    ensure!(
        bytes.len() <= MAX_ROOT_BYTES,
        "release transport root exceeds limit"
    );
    sha256_cid(root, &[0x70])?;
    ensure!(
        root.hash().digest() == Sha256::digest(bytes).as_slice(),
        "release transport root CID mismatch"
    );
    let node = PbNode::from_bytes(Bytes::copy_from_slice(bytes))
        .context("invalid release transport DAG-PB")?;
    // Bound input first; exact re-encoding also rejects duplicate/unknown fields.
    let canonical = node.clone().into_bytes();
    let mut data_first = PbNode {
        links: vec![],
        data: node.data.clone(),
    }
    .into_bytes();
    data_first.extend(
        PbNode {
            links: node.links.clone(),
            data: None,
        }
        .into_bytes(),
    );
    ensure!(
        bytes == canonical || bytes == data_first,
        "noncanonical release transport DAG-PB"
    );
    ensure!(
        node.data.as_deref() == Some(&[0x08, 0x01][..]),
        "release transport root must be a plain UnixFS directory"
    );
    let mut names = BTreeSet::new();
    let mut matched = false;
    for link in node.links {
        let name = link.name.context("release transport link lacks name")?;
        ensure!(
            !name.is_empty() && names.insert(name.clone()),
            "duplicate or empty release transport link"
        );
        if name == DESCRIPTOR_LINK_NAME {
            ensure!(
                link.cid == descriptor,
                "transport root descriptor link mismatch"
            );
            matched = true;
        }
    }
    ensure!(matched, "transport root has no descriptor link");
    Ok(())
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Watermark {
    format: u8,
    /// Historical public trust used only to verify an already accepted floor.
    accepted_keyring: String,
    ipns: String,
    root_block: String,
    manifest: String,
    signature: String,
    binding: String,
    descriptor: String,
}

struct Floor {
    record: SignedRecord,
    sequence: u64,
    descriptor: Cid,
}

/// One locked consumer proof file stores both authenticated freshness floors.
///
/// The caller provisions a private directory on a filesystem supporting file
/// locks, atomic rename, and directory sync. There must be no filesystem rollback.
/// An initialized missing/corrupt proof is an error, never a fresh consumer.
/// This file is consumer state, not a publication journal.
pub struct Consumer {
    directory: PathBuf,
    path: PathBuf,
    lock: File,
    name: PeerId,
    keys: TrustedKeys,
    keyring: Vec<u8>,
    floor: Option<Floor>,
    poisoned: bool,
}

impl Consumer {
    pub fn open(directory: &Path, name: PeerId, keyring: &[u8]) -> Result<Self> {
        ensure!(
            directory.is_dir(),
            "release consumer state directory must exist"
        );
        let directory = directory.canonicalize()?;
        let name_text = canonical_name(name);
        let mut options = OpenOptions::new();
        options.read(true).write(true).create(true).truncate(false);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let lock = options.open(directory.join(format!("{name_text}.lock")))?;
        lock.try_lock()
            .context("another release consumer holds the watermark lock")?;
        let path = directory.join(format!("{name_text}.json"));
        let keys = TrustedKeys::from_bytes(keyring)?;
        let floor = match File::open(&path) {
            Ok(file) => {
                ensure!(
                    file.metadata()?.len() <= MAX_STATE_BYTES as u64,
                    "release watermark exceeds size limit"
                );
                let mut bytes = Vec::new();
                file.take(MAX_STATE_BYTES as u64 + 1)
                    .read_to_end(&mut bytes)?;
                ensure!(
                    bytes.len() <= MAX_STATE_BYTES,
                    "release watermark exceeds size limit"
                );
                let state: Watermark = serde_json::from_slice(&bytes)?;
                ensure!(
                    serde_json::to_vec(&state)? == bytes,
                    "noncanonical release watermark"
                );
                Some(state.validate(name)?)
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                ensure!(
                    lock.metadata()?.len() == 0,
                    "initialized release consumer watermark is missing"
                );
                None
            }
            Err(error) => return Err(error.into()),
        };
        let mut result = Self {
            directory,
            path,
            lock,
            name,
            keys,
            keyring: keyring.to_vec(),
            floor,
            poisoned: false,
        };
        if result.floor.is_some() {
            result.mark_initialized()?;
        }
        Ok(result)
    }

    pub fn watermark_path(&self) -> &Path {
        &self.path
    }
    pub fn accepted_release_sequence(&self) -> Option<u64> {
        self.floor.as_ref().map(|floor| floor.sequence)
    }
    pub fn accepted_ipns_sequence(&self) -> Option<u64> {
        self.floor.as_ref().map(|floor| floor.record.sequence())
    }

    /// Accept authenticated, compatible selection metadata and persist before
    /// returning. Fetched executable/FHS bytes still require Selection verification.
    /// IPNS acknowledgement/discovery does not prove this is globally latest.
    pub fn accept(
        &mut self,
        raw_ipns: &[u8],
        root_block: &[u8],
        release: &AuthenticatedRelease,
        target: &str,
        compatibility: &str,
    ) -> Result<Selection> {
        ensure!(
            !self.poisoned,
            "reopen release consumer after uncertain persistence"
        );
        let selection = release.select(target, compatibility)?;
        ensure!(
            raw_ipns.len() <= MAX_RECORD_SIZE,
            "IPNS record exceeds limit"
        );
        ensure!(
            root_block.len() <= MAX_ROOT_BYTES,
            "release root block exceeds limit"
        );
        // Reauthenticate with this consumer's keyring. A different authenticator
        // cannot import another authority through the public API.
        let authenticated = AuthenticatedRelease::authenticate(
            &self.keys,
            &release.manifest,
            &release.signature,
            &release.binding,
            release.cid,
            &release.descriptor_bytes,
        )?;
        let record = SignedRecord::decode(self.name, raw_ipns.to_vec())?;
        ensure!(
            !record.is_expired_at(Utc::now()),
            "release IPNS record is expired"
        );
        let root = record
            .deployment_cid()
            .context("release IPNS value must be one /ipfs/<cid>")?;
        verify_transport_root(root, root_block, authenticated.cid)?;
        ensure!(
            authenticated.descriptor.channel == canonical_name(self.name),
            "release descriptor channel mismatch"
        );
        if let Some(floor) = &self.floor {
            ensure!(
                record.sequence() >= floor.record.sequence(),
                "release IPNS sequence regressed"
            );
            if record.sequence() == floor.record.sequence() {
                ensure!(
                    record.value() == floor.record.value(),
                    "release IPNS equivocation"
                );
                ensure!(
                    record.eol() >= floor.record.eol(),
                    "release IPNS EOL regressed"
                );
            }
            ensure!(
                authenticated.descriptor.sequence >= floor.sequence,
                "authenticated release sequence regressed"
            );
            if authenticated.descriptor.sequence == floor.sequence {
                ensure!(
                    authenticated.cid == floor.descriptor,
                    "authenticated release sequence equivocation"
                );
            }
        }
        let state = Watermark {
            format: 1,
            accepted_keyring: STANDARD.encode(&self.keyring),
            ipns: STANDARD.encode(raw_ipns),
            root_block: STANDARD.encode(root_block),
            manifest: STANDARD.encode(&release.manifest),
            signature: STANDARD.encode(&release.signature),
            binding: STANDARD.encode(&release.binding),
            descriptor: STANDARD.encode(&release.descriptor_bytes),
        };
        let bytes = serde_json::to_vec(&state)?;
        ensure!(
            bytes.len() <= MAX_STATE_BYTES,
            "release watermark exceeds size limit"
        );
        // Rename or directory-sync failure can have committed the new floor.
        // Poison the handle before writing; only recovery may resolve that state.
        self.poisoned = true;
        self.persist(&bytes)?;
        self.mark_initialized()?;
        self.floor = Some(Floor {
            record,
            sequence: authenticated.descriptor.sequence,
            descriptor: authenticated.cid,
        });
        self.poisoned = false;
        Ok(selection)
    }

    // The exclusive channel lock permits one fixed temporary path. Repeated
    // process crashes cannot accumulate randomly named proof files. Only the
    // canonical JSON file is recovery authority, including after rename failure.
    fn persist(&self, bytes: &[u8]) -> Result<()> {
        let temporary = self.path.with_extension("next");
        match std::fs::remove_file(&temporary) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = options.open(&temporary)?;
        file.write_all(bytes)?;
        file.sync_all()?;
        drop(file);
        std::fs::rename(temporary, &self.path)?;
        File::open(&self.directory)?.sync_all()?;
        Ok(())
    }

    fn mark_initialized(&mut self) -> Result<()> {
        if self.lock.metadata()?.len() == 0 {
            self.lock.write_all(b"release-consumer-v1\n")?;
        }
        self.lock.sync_all()?;
        File::open(&self.directory)?.sync_all()?;
        Ok(())
    }
}

fn decode_field(value: &str, limit: usize) -> Result<Vec<u8>> {
    ensure!(
        value.len() <= limit.div_ceil(3) * 4,
        "release watermark field exceeds limit"
    );
    let bytes = STANDARD.decode(value)?;
    ensure!(
        bytes.len() <= limit && STANDARD.encode(&bytes) == value,
        "invalid release watermark field"
    );
    Ok(bytes)
}
impl Watermark {
    fn validate(self, name: PeerId) -> Result<Floor> {
        ensure!(self.format == 1, "unknown release watermark format");
        // Private durable state records trust at acceptance. A later key rotation
        // must not erase the accepted floor. This historical keyring never
        // authorizes a new candidate: accept always uses the current keyring.
        let historical = TrustedKeys::from_bytes(&decode_field(&self.accepted_keyring, 122)?)?;
        let record = SignedRecord::decode(name, decode_field(&self.ipns, MAX_RECORD_SIZE)?)?;
        // Expired records remain ordering floors; expiry forbids new acceptance.
        let root = record
            .deployment_cid()
            .context("invalid persisted release IPNS value")?;
        let binding = decode_field(&self.binding, MAX_BINDING_BYTES)?;
        let release = AuthenticatedRelease::authenticate(
            &historical,
            &decode_field(&self.manifest, MAX_MANIFEST_BYTES)?,
            &decode_field(&self.signature, 64)?,
            &binding,
            binding_cid(&binding)?,
            &decode_field(&self.descriptor, ipfs::map::MAX_BLOCK_BYTES)?,
        )?;
        ensure!(
            release.descriptor.channel == canonical_name(name),
            "persisted release channel mismatch"
        );
        verify_transport_root(
            root,
            &decode_field(&self.root_block, MAX_ROOT_BYTES)?,
            release.cid,
        )?;
        Ok(Floor {
            record,
            sequence: release.descriptor.sequence,
            descriptor: release.cid,
        })
    }
}

#[cfg(test)]
mod tests;
