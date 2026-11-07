//! Bounded offline construction and acquisition of authenticated release graphs.
//!
//! The spool is immutable candidate data. Publication authority belongs to the
//! caller. Construction and acquisition validate the complete bounded graph;
//! neither operation selects publication authority or claims retention.

use crate::{
    auth::TrustedKeys,
    descriptor::{
        Artifact, AuthenticatedRelease, DeploymentArtifact, Descriptor, HostArtifact, Provenance,
        BINDING_PATH, COMPATIBILITY_PROFILE, DESCRIPTOR_LINK_NAME,
    },
};
use anyhow::{bail, ensure, Context, Result};
use bytes::Bytes;
use cid::Cid;
use ipld_dagpb::{PbLink, PbNode};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs::{self, File},
    io::{Read, Write},
    path::{Path, PathBuf},
};

const CHUNK_BYTES: usize = 1024 * 1024;
const BLOCK_BYTES: usize = 2 * 1024 * 1024;
const FANOUT: usize = 16;
const MAX_BLOCKS: usize = 32_768;
const MAX_GRAPH_BYTES: u64 = 8 * 1024 * 1024 * 1024;
const MAX_FILES: usize = 1024;
const MAX_DEPTH: usize = 64;
const DESCRIPTOR_FILE: &str = "RELEASE-DESCRIPTOR.cbor";

/// Build-process assertions. Each Host retains its actual source revision.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Metadata {
    pub channel: String,
    pub sequence: u64,
    pub version: String,
    pub source: Provenance,
    pub hosts: BTreeMap<String, Provenance>,
}

#[derive(Clone, Copy, Debug)]
struct Link {
    cid: Cid,
    size: u64,
    logical: u64,
}
#[derive(Clone)]
struct FileInfo {
    link: Link,
    sha256: [u8; 32],
}
impl FileInfo {
    fn artifact(&self, path: &str) -> Artifact {
        Artifact {
            path: path.into(),
            root: self.link.cid,
            sha256: self.sha256,
            size: self.link.logical,
        }
    }
}

/// Construct descriptor and binding before the existing Ed25519 manifest signer.
/// Requires a validated FHS and rejects symlinks throughout the candidate tree.
pub fn prepare(tree: &Path, spool: &Path, metadata: Metadata) -> Result<Descriptor> {
    ensure!(
        !tree.join("CHECKSUMS.txt.sig").exists(),
        "prepare cannot rewrite a signed release"
    );
    ensure!(
        read_bounded(&tree.join("VERSION"), 256)? == format!("{}\n", metadata.version).as_bytes(),
        "VERSION differs from release metadata"
    );
    ensure!(
        read_bounded(&tree.join("REVISION"), 256)?
            == format!("{}\n", metadata.source.revision).as_bytes(),
        "REVISION differs from release source"
    );
    let kernel = cell::packaging::read_payload(&tree.join("fhs/boot/kernel.wasm"))?;
    let status = cell::packaging::read_payload(&tree.join("fhs/bin/status.wasm"))?;
    cell::packaging::verify_fhs(&tree.join("fhs"), &kernel, &status)?;
    let mut builder = Builder::new(spool)?;
    ensure!(
        !spool.canonicalize()?.starts_with(tree.canonicalize()?),
        "spool must be outside release tree"
    );
    builder.scan(tree, "", 0, true)?;
    let mut hosts = Vec::new();
    for (target, provenance) in metadata.hosts {
        let path = crate::descriptor::platform_path(&target)?;
        hosts.push(HostArtifact {
            target,
            artifact: builder
                .files
                .get(path)
                .context("missing Host artifact")?
                .artifact(path),
            provenance,
            compatibility: COMPATIBILITY_PROFILE.into(),
        });
    }
    let descriptor = Descriptor {
        channel: metadata.channel,
        sequence: metadata.sequence,
        version: metadata.version,
        source: metadata.source,
        hosts,
        deployment: DeploymentArtifact {
            root: builder
                .directories
                .get("fhs")
                .context("missing FHS directory")?
                .cid,
            kernel: builder
                .files
                .get("fhs/boot/kernel.wasm")
                .context("missing kernel")?
                .artifact("fhs/boot/kernel.wasm"),
            status: builder
                .files
                .get("fhs/bin/status.wasm")
                .context("missing status")?
                .artifact("fhs/bin/status.wasm"),
            compatibility: COMPATIBILITY_PROFILE.into(),
        },
    };
    descriptor.validate()?;
    let bytes = descriptor.encode()?;
    builder.block(0x71, &bytes)?;
    fs::write(tree.join(DESCRIPTOR_FILE), &bytes)?;
    fs::write(tree.join(BINDING_PATH), descriptor.binding()?)?;
    let binding = builder.file(&tree.join(BINDING_PATH))?;
    builder.files.insert(BINDING_PATH.into(), binding);
    let mut manifest = String::from("# sha256\n");
    for (path, file) in &builder.files {
        manifest.push_str(&format!("{}  {path}\n", hex::encode(file.sha256)));
    }
    ensure!(
        manifest.len() <= crate::auth::MAX_MANIFEST_BYTES,
        "release manifest exceeds limit"
    );
    fs::write(tree.join("CHECKSUMS.txt"), manifest)?;
    Ok(descriptor)
}

/// Freeze a signed legacy-compatible UnixFS root with a direct descriptor link.
pub fn finalize(tree: &Path, spool: &Path, keys: &TrustedKeys) -> Result<Package> {
    let descriptor_bytes = read_bounded(&tree.join(DESCRIPTOR_FILE), ipfs::map::MAX_BLOCK_BYTES)?;
    let binding = read_bounded(&tree.join(BINDING_PATH), 256)?;
    let descriptor_cid = crate::descriptor::binding_cid(&binding)?;
    Descriptor::decode(descriptor_cid, &descriptor_bytes)?;
    let mut builder = Builder::new(spool)?;
    ensure!(
        !spool.canonicalize()?.starts_with(tree.canonicalize()?),
        "spool must be outside release tree"
    );
    let descriptor = builder.block(0x71, &descriptor_bytes)?;
    let mut root = builder.scan_entries(tree, "", 0, false)?;
    ensure!(
        !root.contains_key(DESCRIPTOR_LINK_NAME),
        "reserved descriptor link already exists"
    );
    root.insert(DESCRIPTOR_LINK_NAME.into(), descriptor);
    let root = builder.directory(root)?.cid;
    let package = builder.package(root)?;
    package.authenticate(keys)?;
    package.save()?;
    Ok(package)
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct RootIndex {
    version: u32,
    root: Cid,
}

/// An immutable candidate graph on disk, validated before it is returned.
///
/// Opening or fetching content does not authenticate release authority. Call
/// [`Self::authenticate`] with independently trusted keys before using artifacts.
pub struct Package {
    spool: PathBuf,
    root: Cid,
}
impl Package {
    pub fn open(spool: &Path) -> Result<Self> {
        let bytes = read_bounded(&spool.join("package.json"), 1024)?;
        let index: RootIndex = serde_json::from_slice(&bytes)?;
        ensure!(
            index.version == 1 && serde_json::to_vec(&index)? == bytes,
            "invalid package index"
        );
        Builder::new(spool)?.package(index.root)
    }
    pub fn root(&self) -> Cid {
        self.root
    }
    fn save(&self) -> Result<()> {
        fs::write(
            self.spool.join("package.json"),
            serde_json::to_vec(&RootIndex {
                version: 1,
                root: self.root(),
            })?,
        )?;
        Ok(())
    }
    /// Read one bounded block and verify its supported codec and SHA-256 CID.
    pub fn read_block(&self, cid: &Cid) -> Result<Vec<u8>> {
        read_block(&self.spool, cid)
    }

    /// Enumerate links from one verified package block.
    ///
    /// DAG-CBOR package blocks must be version-1 Map envelopes. Standalone
    /// inline-profile values accepted by `ipfs::map::inline_v1` are not accepted.
    pub fn links(&self, cid: &Cid) -> Result<Vec<Cid>> {
        links(cid, &self.read_block(cid)?)
    }

    /// Acquire candidate blocks from Kubo without claiming retention or authority.
    /// The caller supplies private bounded scratch storage, separate from inputs.
    pub async fn fetch(client: &ipfs::HttpClient, root: Cid, spool: &Path) -> Result<Self> {
        let mut builder = Builder::new(spool)?;
        let mut seen = BTreeSet::new();
        let mut pending = vec![(root, 0)];
        let mut total = 0u64;
        while let Some((cid, depth)) = pending.pop() {
            ensure!(depth <= MAX_DEPTH, "release graph depth exceeded");
            if !seen.insert(cid) {
                continue;
            }
            ensure!(
                seen.len() <= MAX_BLOCKS,
                "release graph block count exceeded"
            );
            let bytes = client.block_get(&cid, BLOCK_BYTES).await?;
            verify_block(&cid, &bytes)?;
            total = total
                .checked_add(bytes.len() as u64)
                .context("release graph size overflow")?;
            ensure!(
                total <= MAX_GRAPH_BYTES,
                "release graph byte limit exceeded"
            );
            let children = links(&cid, &bytes)?;
            ensure!(
                pending.len() + children.len() <= MAX_BLOCKS * 2,
                "release graph traversal bound exceeded"
            );
            pending.extend(children.into_iter().map(|child| (child, depth + 1)));
            builder.store(cid, &bytes)?;
        }
        let package = builder.package(root)?;
        package.save()?;
        Ok(package)
    }

    /// Verify authority, descriptor links, reconstructed payload hashes/sizes,
    /// and exact artifact paths before beginning any publication operation.
    pub fn authenticate(&self, keys: &TrustedKeys) -> Result<AuthenticatedRelease> {
        let manifest = self.file_bytes("CHECKSUMS.txt", crate::auth::MAX_MANIFEST_BYTES)?;
        let signature = self.file_bytes("CHECKSUMS.txt.sig", 64)?;
        let binding = self.file_bytes(BINDING_PATH, 256)?;
        let descriptor_cid = self.resolve(DESCRIPTOR_LINK_NAME)?;
        let descriptor_bytes = self.read_block(&descriptor_cid)?;
        let release = AuthenticatedRelease::authenticate(
            keys,
            &manifest,
            &signature,
            &binding,
            descriptor_cid,
            &descriptor_bytes,
        )?;
        let descriptor = release.descriptor();
        ensure!(
            self.file_bytes("VERSION", 256)? == format!("{}\n", descriptor.version).as_bytes(),
            "VERSION differs from authenticated descriptor"
        );
        ensure!(
            self.file_bytes("REVISION", 256)?
                == format!("{}\n", descriptor.source.revision).as_bytes(),
            "REVISION differs from authenticated descriptor"
        );
        ensure!(
            self.resolve("fhs")? == descriptor.deployment.root,
            "FHS root differs from descriptor"
        );
        for artifact in descriptor
            .hosts
            .iter()
            .map(|host| &host.artifact)
            .chain([&descriptor.deployment.kernel, &descriptor.deployment.status])
        {
            self.verify_artifact(artifact)?;
        }
        let authenticated = keys.authenticate(&manifest, &signature)?;
        let mut verified_bytes = 0u64;
        // Authenticate all manifest files, including migration metadata/installer.
        let text = std::str::from_utf8(&manifest)?;
        for line in text.lines().skip(1) {
            let (_, path) = line
                .split_once("  ")
                .context("invalid authenticated manifest")?;
            let root = self.resolve(path)?;
            let mut hash = Sha256::new();
            self.visit_file(root, |bytes| {
                verified_bytes += bytes.len() as u64;
                ensure!(
                    verified_bytes <= MAX_GRAPH_BYTES,
                    "manifest logical payload size exceeded"
                );
                hash.update(bytes);
                Ok(())
            })?;
            ensure!(
                hash.finalize().as_slice() == authenticated.sha256_for(path)?,
                "manifest payload mismatch: {path}"
            );
        }
        Ok(release)
    }

    fn verify_artifact(&self, artifact: &Artifact) -> Result<()> {
        ensure!(
            self.resolve(&artifact.path)? == artifact.root,
            "artifact root differs from descriptor: {}",
            artifact.path
        );
        let mut hash = Sha256::new();
        let mut size = 0u64;
        self.visit_file(artifact.root, |bytes| {
            size += bytes.len() as u64;
            ensure!(size <= artifact.size, "artifact size exceeds descriptor");
            hash.update(bytes);
            Ok(())
        })?;
        ensure!(
            size == artifact.size && hash.finalize().as_slice() == artifact.sha256,
            "artifact bytes differ from descriptor: {}",
            artifact.path
        );
        Ok(())
    }
    fn resolve(&self, path: &str) -> Result<Cid> {
        let mut root = self.root();
        ensure!(valid_path(path), "invalid package path");
        for part in path.split('/') {
            let node = pb_node(root, &self.read_block(&root)?)?;
            ensure!(
                node.data.as_deref() == Some(&[8, 1]),
                "path parent is not supported UnixFS directory"
            );
            let mut names = BTreeSet::new();
            for link in &node.links {
                let name = link
                    .name
                    .as_deref()
                    .context("directory entry has no name")?;
                ensure!(
                    valid_path(name) && !name.contains('/') && names.insert(name),
                    "invalid or duplicate directory entry"
                );
            }
            root = node
                .links
                .iter()
                .find(|link| link.name.as_deref() == Some(part))
                .context("missing package path")?
                .cid;
        }
        Ok(root)
    }
    fn file_bytes(&self, path: &str, limit: usize) -> Result<Vec<u8>> {
        let mut result = Vec::new();
        self.visit_file(self.resolve(path)?, |bytes| {
            ensure!(
                result.len() + bytes.len() <= limit,
                "release metadata size limit exceeded"
            );
            result.extend_from_slice(bytes);
            Ok(())
        })?;
        Ok(result)
    }
    fn visit_file(&self, root: Cid, mut visit: impl FnMut(&[u8]) -> Result<()>) -> Result<()> {
        fn walk(
            package: &Package,
            cid: Cid,
            depth: usize,
            remaining: &mut usize,
            visit: &mut impl FnMut(&[u8]) -> Result<()>,
        ) -> Result<u64> {
            ensure!(
                depth <= MAX_DEPTH && *remaining > 0,
                "file traversal limit exceeded"
            );
            *remaining -= 1;
            let bytes = package.read_block(&cid)?;
            if cid.codec() == 0x55 {
                visit(&bytes)?;
                return Ok(bytes.len() as u64);
            }
            let node = pb_node(cid, &bytes)?;
            let (size, sizes) = file_metadata(
                node.data
                    .as_deref()
                    .context("missing UnixFS file metadata")?,
            )?;
            ensure!(
                sizes.len() == node.links.len(),
                "UnixFS block size count differs from links"
            );
            let mut total = 0u64;
            for (link, expected) in node.links.iter().zip(sizes) {
                ensure!(link.name.as_deref() == Some(""), "named UnixFS file link");
                let actual = walk(package, link.cid, depth + 1, remaining, visit)?;
                ensure!(actual == expected, "UnixFS child size mismatch");
                total = total.checked_add(actual).context("file size overflow")?;
                ensure!(total <= MAX_GRAPH_BYTES, "file logical size bound exceeded");
            }
            ensure!(total == size, "UnixFS file size mismatch");
            Ok(total)
        }
        let mut remaining = MAX_BLOCKS;
        walk(self, root, 0, &mut remaining, &mut visit)?;
        Ok(())
    }
}

struct Builder {
    spool: PathBuf,
    stored: BTreeSet<Cid>,
    stored_bytes: u64,
    files: BTreeMap<String, FileInfo>,
    directories: BTreeMap<String, Link>,
}
impl Builder {
    fn new(spool: &Path) -> Result<Self> {
        fs::create_dir_all(spool.join("blocks"))?;
        Ok(Self {
            spool: spool.into(),
            stored: BTreeSet::new(),
            stored_bytes: 0,
            files: BTreeMap::new(),
            directories: BTreeMap::new(),
        })
    }
    fn store(&mut self, cid: Cid, bytes: &[u8]) -> Result<()> {
        verify_block(&cid, bytes)?;
        if self.stored.insert(cid) {
            self.stored_bytes = self
                .stored_bytes
                .checked_add(bytes.len() as u64)
                .context("release graph size overflow")?;
            ensure!(
                self.stored.len() <= MAX_BLOCKS && self.stored_bytes <= MAX_GRAPH_BYTES,
                "release graph exceeds bounds"
            );
        }
        let path = self.spool.join("blocks").join(cid.to_string());
        if path.exists() {
            ensure!(
                read_block(&self.spool, &cid)? == bytes,
                "conflicting spool block"
            );
        } else {
            let mut file = fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(path)?;
            file.write_all(bytes)?;
        }
        Ok(())
    }
    fn block(&mut self, codec: u64, bytes: &[u8]) -> Result<Link> {
        let cid = Cid::new_v1(
            codec,
            cid::multihash::Multihash::wrap(0x12, &Sha256::digest(bytes))?,
        );
        self.store(cid, bytes)?;
        Ok(Link {
            cid,
            size: bytes.len() as u64,
            logical: bytes.len() as u64,
        })
    }
    fn directory(&mut self, entries: BTreeMap<String, Link>) -> Result<Link> {
        ensure!(entries.len() <= MAX_FILES, "directory entry bound exceeded");
        let mut size = 0u64;
        let mut links = Vec::new();
        for (name, link) in entries {
            ensure!(
                valid_path(&name) && !name.contains('/'),
                "invalid directory entry"
            );
            size = size
                .checked_add(link.size)
                .context("directory size overflow")?;
            links.push(PbLink {
                cid: link.cid,
                name: Some(name),
                size: Some(link.size),
            });
        }
        let bytes = PbNode {
            links,
            data: Some(Bytes::from_static(&[8, 1])),
        }
        .into_bytes();
        let mut result = self.block(0x70, &bytes)?;
        result.size = size
            .checked_add(bytes.len() as u64)
            .context("directory size overflow")?;
        Ok(result)
    }
    fn file_parent(&mut self, children: &[Link]) -> Result<Link> {
        let logical: u64 = children.iter().map(|link| link.logical).sum();
        let mut data = vec![8, 2, 24];
        varint(logical, &mut data);
        for child in children {
            data.push(32);
            varint(child.logical, &mut data);
        }
        let bytes = PbNode {
            data: Some(Bytes::from(data)),
            links: children
                .iter()
                .map(|link| PbLink {
                    cid: link.cid,
                    name: Some(String::new()),
                    size: Some(link.size),
                })
                .collect(),
        }
        .into_bytes();
        let mut result = self.block(0x70, &bytes)?;
        result.logical = logical;
        result.size += children.iter().map(|child| child.size).sum::<u64>();
        Ok(result)
    }
    fn file(&mut self, path: &Path) -> Result<FileInfo> {
        ensure!(
            fs::symlink_metadata(path)?.is_file(),
            "release file must be regular and not a symlink"
        );
        let mut file = File::open(path)?;
        let mut hash = Sha256::new();
        let mut total = 0u64;
        let mut leaves = Vec::new();
        loop {
            let mut bytes = vec![0; CHUNK_BYTES];
            let mut length = 0;
            while length < bytes.len() {
                let n = file.read(&mut bytes[length..])?;
                if n == 0 {
                    break;
                }
                length += n;
            }
            if length == 0 {
                break;
            }
            bytes.truncate(length);
            total += length as u64;
            ensure!(
                total <= MAX_GRAPH_BYTES && leaves.len() < MAX_BLOCKS,
                "release file exceeds bound"
            );
            hash.update(&bytes);
            leaves.push(self.block(0x55, &bytes)?);
        }
        if leaves.is_empty() {
            leaves.push(self.block(0x55, &[])?);
        }
        while leaves.len() > 1 {
            leaves = leaves
                .chunks(FANOUT)
                .map(|children| self.file_parent(children))
                .collect::<Result<_>>()?;
        }
        Ok(FileInfo {
            link: leaves[0],
            sha256: hash.finalize().into(),
        })
    }
    fn scan(&mut self, tree: &Path, prefix: &str, depth: usize, preparation: bool) -> Result<Link> {
        let entries = self.scan_entries(tree, prefix, depth, preparation)?;
        let link = self.directory(entries)?;
        self.directories.insert(prefix.into(), link);
        Ok(link)
    }
    fn scan_entries(
        &mut self,
        tree: &Path,
        prefix: &str,
        depth: usize,
        preparation: bool,
    ) -> Result<BTreeMap<String, Link>> {
        ensure!(
            depth <= MAX_DEPTH && fs::symlink_metadata(tree)?.is_dir(),
            "invalid release directory"
        );
        let mut names = BTreeMap::new();
        for entry in fs::read_dir(tree)? {
            let entry = entry?;
            ensure!(names.len() < MAX_FILES, "directory entry bound exceeded");
            let name = entry
                .file_name()
                .into_string()
                .map_err(|_| anyhow::anyhow!("non-UTF8 release path"))?;
            ensure!(
                valid_path(&name) && !name.contains('/'),
                "invalid release path"
            );
            ensure!(name != DESCRIPTOR_LINK_NAME, "reserved descriptor path");
            if prefix.is_empty()
                && (name == DESCRIPTOR_FILE
                    || (preparation
                        && matches!(
                            name.as_str(),
                            "CHECKSUMS.txt" | "CHECKSUMS.txt.sig" | BINDING_PATH
                        )))
            {
                continue;
            }
            names.insert(name, entry.path());
        }
        let mut entries = BTreeMap::new();
        for (name, path) in names {
            let relative = if prefix.is_empty() {
                name.clone()
            } else {
                format!("{prefix}/{name}")
            };
            let metadata = fs::symlink_metadata(&path)?;
            let link = if metadata.is_dir() {
                self.scan(&path, &relative, depth + 1, preparation)?
            } else {
                ensure!(
                    metadata.is_file() && self.files.len() < MAX_FILES,
                    "invalid release file or file bound exceeded"
                );
                let file = self.file(&path)?;
                let link = file.link;
                self.files.insert(relative, file);
                link
            };
            entries.insert(name, link);
        }
        Ok(entries)
    }
    fn package(&self, root: Cid) -> Result<Package> {
        validate_graph(&self.spool, root)?;
        Ok(Package {
            spool: self.spool.clone(),
            root,
        })
    }
}

// Stage emission and its traversal-work policy belong to the private planner.
// This pass bounds unique content and examined edges independently. Memoized
// heights preserve the depth limit when a shared subtree is reached again.
fn validate_graph(spool: &Path, root: Cid) -> Result<()> {
    GraphValidation::default().walk(spool, root, 0)?;
    Ok(())
}

#[derive(Default)]
struct GraphValidation {
    visited: BTreeSet<Cid>,
    heights: BTreeMap<Cid, usize>,
    total: u64,
    visits: usize,
}

impl GraphValidation {
    fn walk(&mut self, spool: &Path, root: Cid, depth: usize) -> Result<usize> {
        self.visits += 1;
        ensure!(
            depth <= MAX_DEPTH && self.visits <= MAX_BLOCKS * 4,
            "release graph traversal bound exceeded"
        );
        if let Some(height) = self.heights.get(&root) {
            ensure!(depth + height <= MAX_DEPTH, "release graph depth exceeded");
            return Ok(*height);
        }
        let bytes = read_block(spool, &root)?;
        if self.visited.insert(root) {
            self.total = self
                .total
                .checked_add(bytes.len() as u64)
                .context("graph size overflow")?;
            ensure!(
                self.visited.len() <= MAX_BLOCKS && self.total <= MAX_GRAPH_BYTES,
                "release graph exceeds bounds"
            );
        }
        let mut height = 0;
        for child in links(&root, &bytes)? {
            height = height.max(self.walk(spool, child, depth + 1)? + 1);
        }
        self.heights.insert(root, height);
        Ok(height)
    }
}

fn read_bounded(path: &Path, limit: usize) -> Result<Vec<u8>> {
    ensure!(
        fs::symlink_metadata(path)?.is_file(),
        "expected regular nonsymlink file"
    );
    let mut bytes = Vec::new();
    File::open(path)?
        .take(limit as u64 + 1)
        .read_to_end(&mut bytes)?;
    ensure!(bytes.len() <= limit, "file exceeds byte limit");
    Ok(bytes)
}
fn read_block(spool: &Path, cid: &Cid) -> Result<Vec<u8>> {
    let bytes = read_bounded(&spool.join("blocks").join(cid.to_string()), BLOCK_BYTES)?;
    verify_block(cid, &bytes)?;
    Ok(bytes)
}
fn verify_block(cid: &Cid, bytes: &[u8]) -> Result<()> {
    ensure!(
        bytes.len() <= BLOCK_BYTES
            && matches!(cid.codec(), 0x55 | 0x70 | 0x71)
            && cid.hash().code() == 0x12
            && cid.hash().size() == 32,
        "unsupported release block"
    );
    ensure!(
        cid.hash().digest() == Sha256::digest(bytes).as_slice(),
        "release block CID mismatch"
    );
    Ok(())
}
fn pb_node(cid: Cid, bytes: &[u8]) -> Result<PbNode> {
    ensure!(cid.codec() == 0x70, "expected DAG-PB block");
    let node = PbNode::from_bytes(Bytes::copy_from_slice(bytes))?;
    ensure!(
        node.links.len() <= MAX_FILES && node.clone().into_bytes() == bytes,
        "noncanonical or oversized DAG-PB node"
    );
    Ok(node)
}
fn links(cid: &Cid, bytes: &[u8]) -> Result<Vec<Cid>> {
    match cid.codec() {
        0x55 => Ok(Vec::new()),
        0x70 => Ok(pb_node(*cid, bytes)?
            .links
            .into_iter()
            .map(|link| link.cid)
            .collect()),
        0x71 => {
            fn collect(value: &ipfs::map::Value, result: &mut Vec<Cid>) {
                match value {
                    ipfs::map::Value::Link(cid) => result.push(*cid),
                    ipfs::map::Value::List(values) => {
                        for value in values {
                            collect(value, result);
                        }
                    }
                    ipfs::map::Value::Map(values) => {
                        for value in values.values() {
                            collect(value, result);
                        }
                    }
                    _ => {}
                }
            }
            let map = ipfs::map::Map::decode(bytes)?;
            let mut result = Vec::new();
            for (_, value) in map.iter() {
                collect(value, &mut result);
            }
            Ok(result)
        }
        _ => bail!("unsupported release codec"),
    }
}
fn valid_path(path: &str) -> bool {
    !path.is_empty()
        && path
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"._/-".contains(&byte))
        && path.split('/').all(|part| !matches!(part, "" | "." | ".."))
}
fn varint(mut value: u64, bytes: &mut Vec<u8>) {
    while value >= 128 {
        bytes.push(value as u8 | 128);
        value >>= 7;
    }
    bytes.push(value as u8);
}
fn read_varint(bytes: &mut &[u8]) -> Result<u64> {
    let mut value = 0u64;
    for shift in (0..=63).step_by(7) {
        let (&byte, rest) = bytes.split_first().context("truncated UnixFS integer")?;
        *bytes = rest;
        ensure!(shift < 63 || byte <= 1, "UnixFS integer overflow");
        value |= u64::from(byte & 127) << shift;
        if byte < 128 {
            ensure!(shift == 0 || byte != 0, "nonminimal UnixFS integer");
            return Ok(value);
        }
    }
    bail!("UnixFS integer overflow")
}
fn file_metadata(mut bytes: &[u8]) -> Result<(u64, Vec<u64>)> {
    ensure!(
        bytes.starts_with(&[8, 2, 24]),
        "unsupported UnixFS file metadata"
    );
    bytes = &bytes[3..];
    let size = read_varint(&mut bytes)?;
    let mut sizes = Vec::new();
    while !bytes.is_empty() {
        ensure!(
            bytes[0] == 32 && sizes.len() < FANOUT,
            "invalid UnixFS block sizes"
        );
        bytes = &bytes[1..];
        sizes.push(read_varint(&mut bytes)?);
    }
    Ok((size, sizes))
}

#[cfg(test)]
#[path = "package/tests.rs"]
mod tests;
