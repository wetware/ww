//! Composer v1: CID-oriented structural overlay, independent of HTTP and storage.
//! See `doc/composer-v1.md` for the identity and supported-input contract.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::Arc;

use anyhow::{ensure, Context, Result};
use async_trait::async_trait;
use cid::Cid;
use futures::future::BoxFuture;

use super::codec::{self, Directory, Link, Node};
use super::operation::{CompositionError, FrozenMount, FrozenSource, GuestTarget, SourceKind};

pub const PROFILE: &str = "wetware-composer-v1";
const MAX_LAYERS: usize = 128;
const MAX_DEPTH: usize = 128;
const MAX_NODES: usize = 100_000;
const MAX_BYTES: usize = 64 * 1024 * 1024;
const MAX_WORK: usize = 1_000_000;

#[derive(Clone, Copy)]
struct Limits {
    max_nodes: usize,
    max_read_bytes: usize,
    max_written_bytes: usize,
    max_work: usize,
}

const LIMITS: Limits = Limits {
    max_nodes: MAX_NODES,
    max_read_bytes: MAX_BYTES,
    max_written_bytes: MAX_BYTES,
    max_work: MAX_WORK,
};

#[async_trait]
pub(super) trait BlockSource: Sync {
    async fn get(&self, cid: &Cid) -> Result<Vec<u8>>;
}

#[async_trait]
impl BlockSource for ipfs::BootClient {
    async fn get(&self, cid: &Cid) -> Result<Vec<u8>> {
        self.block_get(cid, codec::MAX_BLOCK_BYTES)
            .await
            .map_err(|error| {
                if error
                    .downcast_ref::<ipfs::KuboResponseLimit>()
                    .is_some_and(|limit| limit.status.is_success())
                {
                    anyhow::Error::new(CompositionError::ResourceLimit(
                        "Composer v1 block exceeds 1048576 bytes",
                    ))
                    .context(error)
                } else {
                    error
                }
            })
    }
}

#[derive(Debug)]
pub(super) struct Composition {
    pub root: Cid,
    pub blocks: Vec<(Cid, Vec<u8>)>,
}

#[derive(Clone)]
struct Cached {
    kind: CachedKind,
    size: u64,
    height: usize,
}

#[derive(Clone)]
enum CachedKind {
    Directory(Arc<Directory>),
    File { logical_size: u64 },
    Raw { logical_size: Option<u64> },
    Symlink,
}

struct Composer<'a, S> {
    source: &'a S,
    limits: Limits,
    nodes: HashMap<Cid, Cached>,
    generated: BTreeMap<Cid, Vec<u8>>,
    read_bytes: usize,
    written_bytes: usize,
    work: usize,
}

pub(super) async fn compose(source: &impl BlockSource, layers: &[Cid]) -> Result<Composition> {
    compose_with_limits(source, layers, LIMITS).await
}

async fn compose_with_limits(
    source: &impl BlockSource,
    layers: &[Cid],
    limits: Limits,
) -> Result<Composition> {
    let root = GuestTarget::root();
    compose_operations(
        source,
        layers.iter().map(|cid| (*cid, &root)),
        layers.len(),
        limits,
    )
    .await
}

pub(super) async fn inspect_source(source: &impl BlockSource, cid: Cid) -> Result<FrozenSource> {
    Composer::new(source, LIMITS).inspect(cid, 0, false).await
}

pub(super) async fn compose_mounts(
    source: &impl BlockSource,
    initial_root: Option<Cid>,
    mounts: &[FrozenMount],
) -> Result<Composition> {
    compose_mounts_with_limits(source, initial_root, mounts, LIMITS).await
}

async fn compose_mounts_with_limits(
    source: &impl BlockSource,
    initial_root: Option<Cid>,
    mounts: &[FrozenMount],
    limits: Limits,
) -> Result<Composition> {
    let root = GuestTarget::root();
    let operations = initial_root.iter().map(|cid| (*cid, &root)).chain(
        mounts
            .iter()
            .map(|mount| (mount.source().cid(), mount.target())),
    );
    compose_operations(
        source,
        operations,
        mounts
            .len()
            .saturating_add(usize::from(initial_root.is_some())),
        limits,
    )
    .await
}

async fn compose_operations<'a>(
    source: &impl BlockSource,
    operations: impl Iterator<Item = (Cid, &'a GuestTarget)>,
    count: usize,
    limits: Limits,
) -> Result<Composition> {
    ensure!(count > 0, CompositionError::EmptyOperations);
    ensure!(
        count <= MAX_LAYERS,
        CompositionError::ResourceLimit("Composer v1 layer limit exceeded")
    );
    let mut composer = Composer::new(source, limits);
    // Validate every source before applying any operation. The requested depth
    // includes the entire file sub-DAG, even when cached at a shallower path.
    let mut validated = Vec::with_capacity(count);
    for (cid, target) in operations {
        let inspected = composer
            .inspect(
                cid,
                target.components().len(),
                target.components().is_empty(),
            )
            .await?;
        validated.push((
            Link {
                cid,
                size: inspected.dag_size(),
            },
            target,
        ));
    }
    let mut operations = validated.into_iter();
    let (first, target) = operations.next().expect("nonempty checked above");
    let mut root = if target.components().is_empty() {
        first.cid
    } else {
        let empty = composer.empty_directory(0)?;
        composer.apply_at(empty, target, &first, 0).await?
    };
    for (source_link, target) in operations {
        root = composer.apply_at(root, target, &source_link, 0).await?;
    }

    // Intermediate layer results are implementation details. Persist only
    // generated directories reachable from the final root.
    let mut pending = vec![root];
    let mut reachable = HashSet::new();
    while let Some(cid) = pending.pop() {
        tokio::task::yield_now().await;
        if !reachable.insert(cid) {
            continue;
        }
        let directory = composer.nodes.get(&cid).and_then(|node| match &node.kind {
            CachedKind::Directory(directory) => Some(directory.clone()),
            _ => None,
        });
        if let Some(directory) = directory {
            composer.charge_work(directory.entries.len())?;
            pending.extend(directory.entries.values().map(|link| link.cid));
        }
    }
    Ok(Composition {
        root,
        blocks: composer
            .generated
            .into_iter()
            .filter(|(cid, _)| reachable.contains(cid))
            .collect(),
    })
}

fn invalid_dag(error: anyhow::Error) -> anyhow::Error {
    if error.downcast_ref::<CompositionError>().is_some() {
        error
    } else {
        CompositionError::InvalidDag(format!("{error:#}")).into()
    }
}

impl<S: BlockSource> Composer<'_, S> {
    fn new(source: &S, limits: Limits) -> Composer<'_, S> {
        Composer {
            source,
            limits,
            nodes: HashMap::new(),
            generated: BTreeMap::new(),
            read_bytes: 0,
            written_bytes: 0,
            work: 0,
        }
    }

    async fn inspect(&mut self, cid: Cid, depth: usize, root_only: bool) -> Result<FrozenSource> {
        let node = self.load(cid, depth).await?;
        // Preserve root-only rejection before fetching an opaque raw payload.
        ensure!(
            !root_only || matches!(node.kind, CachedKind::Directory(_)),
            CompositionError::InvalidSource(format!("merge layer {cid} is not a directory"))
        );
        let node = self.measure_raw(cid, node).await?;
        let kind = match node.kind {
            CachedKind::Directory(_) => SourceKind::Directory,
            CachedKind::File { .. } | CachedKind::Raw { .. } => SourceKind::File,
            CachedKind::Symlink => {
                return Err(CompositionError::InvalidSource(format!(
                    "standalone symlink source {cid} is unsupported"
                ))
                .into())
            }
        };
        Ok(FrozenSource::inspected(cid, kind, node.size))
    }

    async fn measure_raw(&mut self, cid: Cid, node: Cached) -> Result<Cached> {
        if !matches!(node.kind, CachedKind::Raw { logical_size: None }) {
            return Ok(node);
        }
        let bytes = self.read_block(&cid).await?;
        codec::verify_cid(&cid, &bytes).map_err(invalid_dag)?;
        let size = bytes.len() as u64;
        let node = Cached {
            kind: CachedKind::Raw {
                logical_size: Some(size),
            },
            size,
            height: 0,
        };
        self.nodes.insert(cid, node.clone());
        Ok(node)
    }

    fn empty_directory(&mut self, depth: usize) -> Result<Cid> {
        self.intern_directory(
            Directory {
                data: bytes::Bytes::from_static(&[8, 1]),
                entries: BTreeMap::new(),
            },
            depth,
        )
    }

    fn apply_at<'a>(
        &'a mut self,
        current: Cid,
        target: &'a GuestTarget,
        source_link: &'a Link,
        depth: usize,
    ) -> BoxFuture<'a, Result<Cid>> {
        Box::pin(async move {
            tokio::task::yield_now().await;
            if depth == target.components().len() {
                return self.merge(current, source_link.cid, depth).await;
            }
            self.charge_work(1)?;
            let node = self
                .nodes
                .get(&current)
                .context("missing target ancestor")?;
            let CachedKind::Directory(directory) = &node.kind else {
                return Err(CompositionError::TargetObstruction {
                    target: target.clone(),
                    ancestor: target.prefix(depth),
                }
                .into());
            };
            let directory = directory.clone();
            // Charge before copying the entries and metadata for the path edit.
            self.charge_work(directory.entries.len())?;
            let name = &target.components()[depth];
            let previous = directory.entries.get(name);
            let child = if depth + 1 == target.components().len() {
                match previous {
                    Some(link) => self.merge(link.cid, source_link.cid, depth + 1).await?,
                    None => source_link.cid,
                }
            } else {
                let next = match previous {
                    Some(link) => link.cid,
                    None => self.empty_directory(depth + 1)?,
                };
                self.apply_at(next, target, source_link, depth + 1).await?
            };
            let link = if child == source_link.cid {
                source_link.clone()
            } else if let Some(previous) = previous.filter(|link| link.cid == child) {
                previous.clone()
            } else {
                Link {
                    cid: child,
                    size: self
                        .nodes
                        .get(&child)
                        .context("missing target result")?
                        .size,
                }
            };
            if previous == Some(&link) {
                return Ok(current);
            }
            let mut directory = (*directory).clone();
            directory.entries.insert(name.clone(), link);
            self.intern_directory(directory, depth)
        })
    }

    fn intern_directory(&mut self, directory: Directory, depth: usize) -> Result<Cid> {
        let height = directory
            .entries
            .values()
            .map(|link| self.nodes[&link.cid].height + 1)
            .max()
            .unwrap_or(0);
        ensure!(
            depth + height <= MAX_DEPTH,
            CompositionError::ResourceLimit("Composer v1 directory depth limit exceeded")
        );
        // Callers charge the entries before cloning/merging. Encoding and
        // height scans visit that same bounded entry set.
        let (cid, bytes, size) = codec::encode(&directory).map_err(invalid_dag)?;
        if self.nodes.contains_key(&cid) {
            return Ok(cid);
        }
        ensure!(
            self.nodes.len() < self.limits.max_nodes,
            CompositionError::ResourceLimit("Composer v1 node limit exceeded")
        );
        self.written_bytes =
            self.written_bytes
                .checked_add(bytes.len())
                .ok_or(CompositionError::ResourceLimit(
                    "composition output byte count overflow",
                ))?;
        ensure!(
            self.written_bytes <= self.limits.max_written_bytes,
            CompositionError::ResourceLimit("Composer v1 output byte limit exceeded")
        );
        self.nodes.insert(
            cid,
            Cached {
                kind: CachedKind::Directory(Arc::new(directory)),
                size,
                height,
            },
        );
        self.generated.insert(cid, bytes);
        Ok(cid)
    }

    fn charge_work(&mut self, amount: usize) -> Result<()> {
        self.work = self
            .work
            .checked_add(amount)
            .ok_or(CompositionError::ResourceLimit(
                "composition work count overflow",
            ))?;
        ensure!(
            self.work <= self.limits.max_work,
            CompositionError::ResourceLimit("Composer v1 work limit exceeded")
        );
        Ok(())
    }

    async fn read_block(&mut self, cid: &Cid) -> Result<Vec<u8>> {
        let bytes = self
            .source
            .get(cid)
            .await
            .with_context(|| format!("reading composition block {cid}"))?;
        ensure!(
            bytes.len() <= codec::MAX_BLOCK_BYTES,
            CompositionError::ResourceLimit("Composer v1 block exceeds 1048576 bytes")
        );
        self.read_bytes =
            self.read_bytes
                .checked_add(bytes.len())
                .ok_or(CompositionError::ResourceLimit(
                    "composition byte count overflow",
                ))?;
        ensure!(
            self.read_bytes <= self.limits.max_read_bytes,
            CompositionError::ResourceLimit("Composer v1 input byte limit exceeded")
        );
        Ok(bytes)
    }

    fn load_file_child(&mut self, cid: Cid, depth: usize) -> BoxFuture<'_, Result<Cached>> {
        Box::pin(async move {
            let mut child = self
                .load(cid, depth)
                .await
                .with_context(|| format!("validating UnixFS file child {cid}"))?;
            child = self.measure_raw(cid, child).await?;
            ensure!(
                matches!(
                    &child.kind,
                    CachedKind::File { .. }
                        | CachedKind::Raw {
                            logical_size: Some(_)
                        }
                ),
                CompositionError::InvalidDag("unsupported UnixFS file child type".into())
            );
            Ok(child)
        })
    }

    fn load(&mut self, cid: Cid, depth: usize) -> BoxFuture<'_, Result<Cached>> {
        Box::pin(async move {
            tokio::task::yield_now().await;
            ensure!(
                depth <= MAX_DEPTH,
                CompositionError::ResourceLimit("Composer v1 directory depth limit exceeded")
            );
            ensure!(
                cid.hash().code() == 0x12 && cid.hash().size() == 32,
                CompositionError::UnsupportedProfile("Composer v1 requires SHA-256 CIDs".into())
            );
            if let Some(node) = self.nodes.get(&cid) {
                ensure!(
                    depth + node.height <= MAX_DEPTH,
                    CompositionError::ResourceLimit("Composer v1 directory depth limit exceeded")
                );
                return Ok(node.clone());
            }
            ensure!(
                self.nodes.len() < self.limits.max_nodes,
                CompositionError::ResourceLimit("Composer v1 node limit exceeded")
            );
            // Ordinary raw directory children stay opaque. Standalone sources
            // and file chunks upgrade this cache entry through measure_raw.
            if cid.codec() == 0x55 {
                let node = Cached {
                    kind: CachedKind::Raw { logical_size: None },
                    size: 0,
                    height: 0,
                };
                self.nodes.insert(cid, node.clone());
                return Ok(node);
            }
            ensure!(
                cid.codec() == 0x70,
                CompositionError::UnsupportedProfile(format!(
                    "unsupported Composer v1 codec {}",
                    cid.codec()
                ))
            );
            let bytes = self.read_block(&cid).await?;
            let block_size = bytes.len() as u64;
            let node = match codec::decode(&cid, bytes).map_err(invalid_dag)? {
                Node::Raw { logical_size, size } => Cached {
                    kind: CachedKind::Raw {
                        logical_size: Some(logical_size),
                    },
                    size,
                    height: 0,
                },
                Node::Symlink { size } => Cached {
                    kind: CachedKind::Symlink,
                    size,
                    height: 0,
                },
                Node::File { file, size } => {
                    self.charge_work(file.links.len())?;
                    let mut logical_size = file.inline_size;
                    let mut height = 0;
                    for link in file.links {
                        let child = self.load_file_child(link.cid, depth + 1).await?;
                        ensure!(
                            link.size == child.size,
                            CompositionError::InvalidDag(format!(
                                "incorrect Tsize for {}",
                                link.cid
                            ))
                        );
                        let child_logical_size = match child.kind {
                            CachedKind::File { logical_size } => logical_size,
                            CachedKind::Raw {
                                logical_size: Some(logical_size),
                            } => logical_size,
                            _ => unreachable!("file child type checked by load_file_child"),
                        };
                        ensure!(
                            link.logical_size == child_logical_size,
                            CompositionError::InvalidDag(format!(
                                "UnixFS file blocksize for {} is {}, expected {}",
                                link.cid, link.logical_size, child_logical_size
                            ))
                        );
                        logical_size =
                            logical_size
                                .checked_add(child_logical_size)
                                .ok_or_else(|| {
                                    CompositionError::InvalidDag("UnixFS file size overflow".into())
                                })?;
                        height = height.max(child.height + 1);
                    }
                    ensure!(
                        logical_size == file.logical_size,
                        CompositionError::InvalidDag(
                            "UnixFS file size does not match inline data and child logical sizes"
                                .into()
                        )
                    );
                    Cached {
                        kind: CachedKind::File { logical_size },
                        size,
                        height,
                    }
                }
                Node::Directory(directory) => {
                    self.charge_work(directory.entries.len())?;
                    let mut size = block_size;
                    let mut height = 0;
                    for link in directory.entries.values() {
                        let child = self.load(link.cid, depth + 1).await?;
                        if link.cid.codec() != 0x55 {
                            ensure!(
                                link.size == child.size,
                                CompositionError::InvalidDag(format!(
                                    "incorrect Tsize for {}",
                                    link.cid
                                ))
                            );
                        }
                        size = size.checked_add(link.size).ok_or_else(|| {
                            CompositionError::InvalidDag("UnixFS cumulative size overflow".into())
                        })?;
                        height = height.max(child.height + 1);
                    }
                    Cached {
                        kind: CachedKind::Directory(Arc::new(directory)),
                        size,
                        height,
                    }
                }
            };
            ensure!(
                self.nodes.len() < self.limits.max_nodes,
                CompositionError::ResourceLimit("Composer v1 node limit exceeded")
            );
            self.nodes.insert(cid, node.clone());
            Ok(node)
        })
    }

    fn merge(&mut self, base: Cid, overlay: Cid, depth: usize) -> BoxFuture<'_, Result<Cid>> {
        Box::pin(async move {
            tokio::task::yield_now().await;
            ensure!(
                depth <= MAX_DEPTH,
                CompositionError::ResourceLimit("Composer v1 directory depth limit exceeded")
            );
            if base == overlay {
                return Ok(base);
            }
            let base_node = self.nodes.get(&base).context("missing base node")?.clone();
            let overlay_node = self
                .nodes
                .get(&overlay)
                .context("missing overlay node")?
                .clone();
            let (CachedKind::Directory(base_dir), CachedKind::Directory(overlay_dir)) =
                (base_node.kind, overlay_node.kind)
            else {
                return Ok(overlay);
            };
            self.charge_work(base_dir.entries.len() + overlay_dir.entries.len())?;
            let mut merged = (*base_dir).clone();
            // Later directories own metadata wholesale, including omissions.
            merged.data = overlay_dir.data.clone();
            for (name, overlay_link) in &overlay_dir.entries {
                let link = if let Some(base_link) = merged.entries.get(name) {
                    let cid = self
                        .merge(base_link.cid, overlay_link.cid, depth + 1)
                        .await?;
                    if cid == overlay_link.cid {
                        overlay_link.clone()
                    } else if cid == base_link.cid {
                        base_link.clone()
                    } else {
                        Link {
                            cid,
                            size: self
                                .nodes
                                .get(&cid)
                                .context("missing merged directory")?
                                .size,
                        }
                    }
                } else {
                    overlay_link.clone()
                };
                merged.entries.insert(name.clone(), link);
            }
            // Composer v1 selects the base representation when both inputs
            // match the composed entries and metadata (e.g. CIDv0 vs CIDv1).
            if merged == *base_dir {
                return Ok(base);
            }
            if merged == *overlay_dir {
                return Ok(overlay);
            }
            self.intern_directory(merged, depth)
        })
    }
}

#[cfg(test)]
#[path = "composer_tests.rs"]
mod tests;

#[cfg(test)]
#[path = "composer_property_tests.rs"]
mod property_tests;
