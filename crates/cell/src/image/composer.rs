//! Composer v1: CID-oriented structural overlay, independent of HTTP and storage.
//! See `doc/composer-v1.md` for the identity and supported-input contract.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::Arc;

use anyhow::{ensure, Context, Result};
use async_trait::async_trait;
use cid::Cid;
use futures::future::BoxFuture;

use super::codec::{self, Directory, Link, Node};

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
        self.block_get(cid, codec::MAX_BLOCK_BYTES).await
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
    ensure!(!layers.is_empty(), "No CIDs to merge");
    ensure!(
        layers.len() <= MAX_LAYERS,
        "Composer v1 layer limit exceeded"
    );
    let mut composer = Composer {
        source,
        limits,
        nodes: HashMap::new(),
        generated: BTreeMap::new(),
        read_bytes: 0,
        written_bytes: 0,
        work: 0,
    };
    // Validate all directory subtrees, including those reused wholesale. This
    // makes the HAMT/type boundary independent of which names happen to collide.
    for cid in layers {
        let node = composer.load(*cid, 0).await?;
        ensure!(
            matches!(node.kind, CachedKind::Directory(_)),
            "merge layer {cid} is not a directory"
        );
    }
    let mut root = layers[0];
    for overlay in &layers[1..] {
        root = composer.merge(root, *overlay, 0).await?;
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

impl<S: BlockSource> Composer<'_, S> {
    fn charge_work(&mut self, amount: usize) -> Result<()> {
        self.work = self
            .work
            .checked_add(amount)
            .context("composition work count overflow")?;
        ensure!(
            self.work <= self.limits.max_work,
            "Composer v1 work limit exceeded"
        );
        Ok(())
    }

    async fn read_block(&mut self, cid: &Cid) -> Result<Vec<u8>> {
        let bytes = self
            .source
            .get(cid)
            .await
            .with_context(|| format!("reading composition block {cid}"))?;
        self.read_bytes = self
            .read_bytes
            .checked_add(bytes.len())
            .context("composition byte count overflow")?;
        ensure!(
            self.read_bytes <= self.limits.max_read_bytes,
            "Composer v1 input byte limit exceeded"
        );
        Ok(bytes)
    }

    fn load_file_child(&mut self, cid: Cid, depth: usize) -> BoxFuture<'_, Result<Cached>> {
        Box::pin(async move {
            let mut child = self
                .load(cid, depth)
                .await
                .with_context(|| format!("validating UnixFS file child {cid}"))?;
            if matches!(&child.kind, CachedKind::Raw { logical_size: None }) {
                let bytes = self.read_block(&cid).await?;
                codec::verify_cid(&cid, &bytes)?;
                let logical_size = bytes.len() as u64;
                child = Cached {
                    kind: CachedKind::Raw {
                        logical_size: Some(logical_size),
                    },
                    size: logical_size,
                    height: 0,
                };
                self.nodes.insert(cid, child.clone());
            }
            ensure!(
                matches!(
                    &child.kind,
                    CachedKind::File { .. }
                        | CachedKind::Raw {
                            logical_size: Some(_)
                        }
                ),
                "unsupported UnixFS file child type"
            );
            Ok(child)
        })
    }

    fn load(&mut self, cid: Cid, depth: usize) -> BoxFuture<'_, Result<Cached>> {
        Box::pin(async move {
            tokio::task::yield_now().await;
            ensure!(
                depth <= MAX_DEPTH,
                "Composer v1 directory depth limit exceeded"
            );
            ensure!(
                cid.hash().code() == 0x12 && cid.hash().size() == 32,
                "Composer v1 requires SHA-256 CIDs"
            );
            if let Some(node) = self.nodes.get(&cid) {
                ensure!(
                    depth + node.height <= MAX_DEPTH,
                    "Composer v1 directory depth limit exceeded"
                );
                return Ok(node.clone());
            }
            ensure!(
                self.nodes.len() < self.limits.max_nodes,
                "Composer v1 node limit exceeded"
            );
            // Raw files have no directory structure. Never fetch their payload.
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
                "unsupported Composer v1 codec {}",
                cid.codec()
            );
            let bytes = self.read_block(&cid).await?;
            let block_size = bytes.len() as u64;
            let node = match codec::decode(&cid, bytes)? {
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
                        ensure!(link.size == child.size, "incorrect Tsize for {}", link.cid);
                        let child_logical_size = match child.kind {
                            CachedKind::File { logical_size } => logical_size,
                            CachedKind::Raw {
                                logical_size: Some(logical_size),
                            } => logical_size,
                            _ => unreachable!("file child type checked by load_file_child"),
                        };
                        ensure!(
                            link.logical_size == child_logical_size,
                            "UnixFS file blocksize for {} is {}, expected {}",
                            link.cid,
                            link.logical_size,
                            child_logical_size
                        );
                        logical_size = logical_size
                            .checked_add(child_logical_size)
                            .context("UnixFS file size overflow")?;
                        height = height.max(child.height + 1);
                    }
                    ensure!(
                        logical_size == file.logical_size,
                        "UnixFS file size does not match inline data and child logical sizes"
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
                            ensure!(link.size == child.size, "incorrect Tsize for {}", link.cid);
                        }
                        size = size
                            .checked_add(link.size)
                            .context("UnixFS cumulative size overflow")?;
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
                "Composer v1 node limit exceeded"
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
                "Composer v1 directory depth limit exceeded"
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
            let (cid, bytes, size) = codec::encode(&merged)?;
            if self.nodes.contains_key(&cid) {
                return Ok(cid);
            }
            let height = merged
                .entries
                .values()
                .map(|link| self.nodes[&link.cid].height + 1)
                .max()
                .unwrap_or(0);
            ensure!(
                depth + height <= MAX_DEPTH,
                "Composer v1 directory depth limit exceeded"
            );
            ensure!(
                self.nodes.len() < self.limits.max_nodes,
                "Composer v1 node limit exceeded"
            );
            self.written_bytes = self
                .written_bytes
                .checked_add(bytes.len())
                .context("composition output byte count overflow")?;
            ensure!(
                self.written_bytes <= self.limits.max_written_bytes,
                "Composer v1 output byte limit exceeded"
            );
            self.nodes.insert(
                cid,
                Cached {
                    kind: CachedKind::Directory(Arc::new(merged)),
                    size,
                    height,
                },
            );
            self.generated.insert(cid, bytes);
            Ok(cid)
        })
    }
}

#[cfg(test)]
#[path = "composer_tests.rs"]
mod tests;
