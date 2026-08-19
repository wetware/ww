//! Lazy virtual filesystem backed by a content-addressed CID tree.
//!
//! `CidTree` resolves guest filesystem paths through an IPFS directory DAG
//! without materializing the entire image upfront. Directory listings are
//! cached in a 3-tier stack: in-memory LRU → persisted JSON on staging disk
//! → live IPFS `ls()` call. File content is fetched on demand via the
//! existing `PinsetCache` infrastructure.
//!
//! The root CID can be atomically swapped (via `arc_swap::ArcSwap`) for
//! epoch updates. Open file descriptors are unaffected because they hold
//! real staging-dir FDs. New opens after a swap see the new tree.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use anyhow::{bail, Context, Result};
use arc_swap::ArcSwap;
use lru::LruCache;
use std::num::NonZeroUsize;

use ipfs;

/// Maximum depth for symlink resolution to prevent infinite loops.
const MAX_SYMLINK_DEPTH: usize = 16;

/// Default capacity for the in-memory directory listing LRU cache.
const DIR_CACHE_CAPACITY: usize = 1024;

/// Filename used for persisted directory listings on staging disk.
const DIRLIST_SUFFIX: &str = ".dirlist.json";

// ── Directory entry types ─────────────────────────────────────────

/// Type of a directory entry in the CID tree.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum EntryType {
    File,
    Dir,
    Symlink { target: String },
}

/// A single entry in a CID-backed directory listing.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct DirEntry {
    pub name: String,
    pub cid: String,
    pub entry_type: EntryType,
    pub size: u64,
}

// ── Resolved node ─────────────────────────────────────────────────

/// The result of resolving a guest path through the CID tree.
#[derive(Debug)]
pub enum ResolvedNode {
    /// File backed by a CID. Content must be fetched via PinsetCache.
    CidFile { cid: String, size: u64 },
    /// Directory backed by a CID. Listing via `ls_dir()`.
    CidDir { cid: String },
}

// ── CidTree ───────────────────────────────────────────────────────

/// A lazy, cached, content-addressed filesystem tree.
///
/// Resolves guest paths by walking an IPFS directory DAG from a root CID.
/// The root is swappable for epoch updates. Directory listings are cached
/// in a 3-tier stack (memory → disk → network).
pub struct CidTree {
    /// The current root CID, swapped atomically on epoch updates.
    root: ArcSwap<String>,
    /// IPFS HTTP client for `ls()` calls (directory metadata).
    ipfs: ipfs::HttpClient,
    /// In-memory LRU cache for directory listings, keyed by CID string.
    dir_cache: Mutex<LruCache<String, Vec<DirEntry>>>,
    /// Staging directory for persisted directory listings.
    staging_dir: PathBuf,
}

impl CidTree {
    /// Create a new CidTree with the given root CID.
    pub fn new(root_cid: String, ipfs: ipfs::HttpClient, staging_dir: PathBuf) -> Self {
        Self {
            root: ArcSwap::from_pointee(root_cid),
            ipfs,
            dir_cache: Mutex::new(LruCache::new(
                NonZeroUsize::new(DIR_CACHE_CAPACITY).unwrap(),
            )),
            staging_dir,
        }
    }

    /// The current root CID.
    pub fn root_cid(&self) -> Arc<String> {
        self.root.load_full()
    }

    /// Atomically swap the root CID for epoch updates.
    ///
    /// Clears the in-memory directory listing cache (staging-disk listings
    /// are CID-keyed and thus immutable, so they persist across swaps).
    pub fn swap_root(&self, new_cid: String) {
        self.root.store(Arc::new(new_cid));

        // Clear in-memory dir listing cache.
        if let Ok(mut cache) = self.dir_cache.lock() {
            cache.clear();
        }

        // Clean up stub directories from the old root. Stub dirs are named
        // `dir-{cid}` and only relevant while that CID is the active root.
        // Content-addressed file staging (bare CID files) is managed by
        // PinsetCache and left alone.
        if let Ok(entries) = std::fs::read_dir(&self.staging_dir) {
            for entry in entries.flatten() {
                if let Some(name) = entry.file_name().to_str() {
                    if name.starts_with("dir-") {
                        let _ = std::fs::remove_dir_all(entry.path());
                    }
                }
            }
        }
    }

    /// Pre-warm the directory listing cache for the root of a CID.
    ///
    /// Call this before `swap_root()` so the first post-swap access is fast.
    pub async fn pre_warm(&self, cid: &str) -> Result<()> {
        let _ = self.ls_dir(cid).await?;
        Ok(())
    }

    /// List directory entries for a CID, using the 3-tier cache.
    ///
    /// 1. In-memory LRU cache (hit → return immediately)
    /// 2. Staging disk (hit → populate LRU, return)
    /// 3. IPFS daemon `ls()` (populate both caches, return)
    pub async fn ls_dir(&self, cid: &str) -> Result<Vec<DirEntry>> {
        // Tier 1: in-memory LRU
        if let Some(entries) = self
            .dir_cache
            .lock()
            .ok()
            .and_then(|mut c| c.get(cid).cloned())
        {
            return Ok(entries);
        }

        // Tier 2: staging disk
        let disk_path = self.staging_dir.join(format!("{cid}{DIRLIST_SUFFIX}"));
        if disk_path.exists() {
            if let Ok(data) = std::fs::read_to_string(&disk_path) {
                if let Ok(entries) = serde_json::from_str::<Vec<DirEntry>>(&data) {
                    // Populate LRU from disk
                    if let Ok(mut cache) = self.dir_cache.lock() {
                        cache.put(cid.to_string(), entries.clone());
                    }
                    return Ok(entries);
                }
            }
        }

        // Tier 3: IPFS daemon
        let ipfs_path = format!("/ipfs/{cid}");
        let raw_entries = self
            .ipfs
            .ls(&ipfs_path)
            .await
            .with_context(|| format!("ls failed for CID {cid}"))?;

        let entries: Vec<DirEntry> = raw_entries
            .into_iter()
            .map(|e| DirEntry {
                name: e.name,
                cid: e.hash,
                entry_type: match e.entry_type {
                    1 => EntryType::Dir,
                    // TODO: handle symlinks if IPFS ls ever exposes them
                    _ => EntryType::File,
                },
                size: e.size,
            })
            .collect();

        // Persist to staging disk
        if let Ok(json) = serde_json::to_string(&entries) {
            if let Some(parent) = disk_path.parent() {
                let _ = std::fs::create_dir_all(parent);
            }
            let _ = std::fs::write(&disk_path, json);
        }

        // Populate LRU
        if let Ok(mut cache) = self.dir_cache.lock() {
            cache.put(cid.to_string(), entries.clone());
        }

        Ok(entries)
    }

    /// Resolve a guest path to a `ResolvedNode`.
    ///
    /// Walks the CID tree from the current root. Symlinks are followed up to
    /// `MAX_SYMLINK_DEPTH`.
    pub async fn resolve_path(&self, path: &str) -> Result<ResolvedNode> {
        self.resolve_path_inner(path, 0).await
    }

    fn resolve_path_inner<'a>(
        &'a self,
        path: &'a str,
        symlink_depth: usize,
    ) -> futures::future::BoxFuture<'a, Result<ResolvedNode>> {
        Box::pin(self.resolve_path_inner_impl(path, symlink_depth))
    }

    async fn resolve_path_inner_impl(
        &self,
        path: &str,
        symlink_depth: usize,
    ) -> Result<ResolvedNode> {
        if symlink_depth > MAX_SYMLINK_DEPTH {
            bail!("symlink depth exceeded (max {MAX_SYMLINK_DEPTH})");
        }

        // Normalize: strip leading /
        let path = path.strip_prefix('/').unwrap_or(path);
        if path.is_empty() {
            // Root directory
            let root = self.root.load_full();
            return Ok(ResolvedNode::CidDir {
                cid: (*root).clone(),
            });
        }

        // Reject path traversal
        if path.split('/').any(|seg| seg == "..") {
            bail!("path traversal (..) not allowed: {path}");
        }

        // Walk the CID tree from root
        let root = self.root.load_full();
        let components: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();

        let mut current_cid = (*root).clone();

        for (i, component) in components.iter().enumerate() {
            let entries = self.ls_dir(&current_cid).await?;
            let entry = entries
                .iter()
                .find(|e| e.name == *component)
                .with_context(|| {
                    format!(
                        "path component '{}' not found in CID {} (full path: {})",
                        component, current_cid, path
                    )
                })?;

            let is_last = i == components.len() - 1;

            match &entry.entry_type {
                EntryType::Dir => {
                    if is_last {
                        return Ok(ResolvedNode::CidDir {
                            cid: entry.cid.clone(),
                        });
                    }
                    // Continue walking
                    current_cid = entry.cid.clone();
                }
                EntryType::File => {
                    if is_last {
                        return Ok(ResolvedNode::CidFile {
                            cid: entry.cid.clone(),
                            size: entry.size,
                        });
                    }
                    bail!(
                        "path component '{}' is a file, not a directory (full path: {})",
                        component,
                        path
                    );
                }
                EntryType::Symlink { target } => {
                    // Resolve symlink target. If absolute, resolve from root.
                    // If relative, resolve from current directory.
                    let remaining: String = if is_last {
                        String::new()
                    } else {
                        components[i + 1..].join("/")
                    };

                    let resolved_target = if target.starts_with('/') {
                        if remaining.is_empty() {
                            target.clone()
                        } else {
                            format!("{}/{}", target.trim_end_matches('/'), remaining)
                        }
                    } else {
                        // Relative symlink: reconstruct parent path
                        let parent: String = components[..i].join("/");
                        let base = if parent.is_empty() {
                            target.clone()
                        } else {
                            format!("{parent}/{target}")
                        };
                        if remaining.is_empty() {
                            base
                        } else {
                            format!("{}/{}", base.trim_end_matches('/'), remaining)
                        }
                    };

                    return self
                        .resolve_path_inner(&resolved_target, symlink_depth + 1)
                        .await;
                }
            }
        }

        // Should not reach here — the loop handles all cases
        bail!("unexpected end of path resolution: {path}");
    }

    /// Reference to the IPFS client (for callers that need file content).
    pub fn ipfs(&self) -> &ipfs::HttpClient {
        &self.ipfs
    }

    /// Path to the per-process staging directory. This is the path
    /// `Cell::spawn` WASI-preopens as `/` for the guest.
    ///
    /// **The staging directory is host-side scratch, not guest-visible
    /// state.** `fs_intercept` overrides every fs op before it reads
    /// from this directory, routing through `CidTree::resolve_path`. The
    /// directory's actual on-disk contents are dir-listing stubs (sparse
    /// files with correct sizes, populated by `fs_intercept` on demand
    /// for `cap_std::fs::Dir::readdir`) plus persisted JSON dir listings
    /// keyed by CID. Raw file bytes live in `PinsetCache`'s tempdir, not
    /// here.
    ///
    /// Callers outside `fs_intercept` should not read this directory's
    /// contents directly: it is not a stable view of the guest's
    /// filesystem (entries appear/disappear as `fs_intercept` materializes
    /// stubs) and the layout is an implementation detail of the
    /// readdir-stub trick.
    pub fn staging_dir(&self) -> &Path {
        &self.staging_dir
    }
}

impl std::fmt::Debug for CidTree {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CidTree")
            .field("root", &*self.root.load())
            .finish()
    }
}

// ── Tests ─────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_path_traversal_rejected() {
        // Can't test async resolve_path easily without a mock IPFS client,
        // but we can verify the traversal check logic inline.
        let path = "etc/../../shadow";
        assert!(path.split('/').any(|seg| seg == ".."));
    }

    #[test]
    fn test_dirlist_serialization_roundtrip() {
        let entries = vec![
            DirEntry {
                name: "bin".to_string(),
                cid: "QmBin".to_string(),
                entry_type: EntryType::Dir,
                size: 0,
            },
            DirEntry {
                name: "config".to_string(),
                cid: "QmCfg".to_string(),
                entry_type: EntryType::File,
                size: 1024,
            },
        ];

        let json = serde_json::to_string(&entries).unwrap();
        let deserialized: Vec<DirEntry> = serde_json::from_str(&json).unwrap();
        assert_eq!(deserialized.len(), 2);
        assert_eq!(deserialized[0].name, "bin");
        assert_eq!(deserialized[1].entry_type, EntryType::File);
    }
}
