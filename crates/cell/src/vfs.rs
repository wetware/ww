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
//! real staging-dir FDs. Directory descriptors retain a root snapshot and
//! canonical virtual path for subsequent descriptor-relative opens.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use anyhow::{bail, Context, Result};
use arc_swap::ArcSwap;
use cid::Cid;
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
    CidFile { cid: Cid, size: u64 },
    /// Directory backed by a CID. Listing via `ls_dir()`.
    CidDir { cid: Cid },
}

/// A resolved node and its canonical path within the captured root.
#[derive(Debug)]
pub struct ResolvedPath {
    pub node: ResolvedNode,
    pub path: String,
}

/// Path failures that filesystem interception maps to distinct WASI errors.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResolveError {
    NoEntry,
    NotDirectory,
    InvalidPath,
    InvalidCid,
}

impl std::fmt::Display for ResolveError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::NoEntry => "path entry not found",
            Self::NotDirectory => "path component is not a directory",
            Self::InvalidPath => "invalid descriptor-relative path",
            Self::InvalidCid => "invalid selected entry CID",
        })
    }
}

impl std::error::Error for ResolveError {}

// ── CidTree ───────────────────────────────────────────────────────

/// A lazy, cached, content-addressed filesystem tree.
///
/// Resolves guest paths by walking an IPFS directory DAG from a root CID.
/// The root is swappable for epoch updates. Directory listings are cached
/// in a 3-tier stack (memory → disk → network).
pub struct CidTree {
    /// The current root CID, swapped atomically on epoch updates.
    root: ArcSwap<Cid>,
    /// IPFS HTTP client for `ls()` calls (directory metadata).
    ipfs: ipfs::HttpClient,
    /// In-memory LRU cache for directory listings, keyed by versioned CID.
    dir_cache: Mutex<LruCache<Cid, Vec<DirEntry>>>,
    /// Staging directory for persisted directory listings.
    staging_dir: PathBuf,
    /// Authoritative Host resource domain and root that own this staging area.
    host: Arc<cache::HostResources>,
    filesystem_scope: cache::FsScope,
}

/// One serial CidTree traversal. Logical admission is lazy for standalone
/// callers so a pure in-memory lookup consumes no Host allowance. A virtual
/// Cell open supplies its already-admitted logical permit instead.
pub(crate) struct CidTreeTraversal {
    host: Arc<cache::HostResources>,
    filesystem_scope: cache::FsScope,
    // Kept before the transaction so L is released before F/root ownership.
    // Actual driver operations clone this Arc, retaining the same logical
    // debit when the waiting guest future is cancelled.
    logical: Option<Arc<TraversalLogicalLease>>,
    transaction: Option<cache::ForegroundTransaction>,
}

struct TraversalLogicalLease {
    permit: cache::LogicalIoPermit,
}

impl CidTreeTraversal {
    fn ensure_transaction(&mut self) -> Result<&cache::ForegroundTransaction> {
        if self.transaction.is_none() {
            if self.logical.is_none() {
                self.logical = Some(Arc::new(TraversalLogicalLease {
                    permit: self.host.try_logical_io()?,
                }));
            }
            let logical = self
                .logical
                .as_ref()
                .map(|lease| &lease.permit)
                .expect("CidTree logical admission was initialized");
            self.host.validate_logical(logical)?;

            // Root work is reserved before F. This transaction intentionally
            // borrows logical ownership: the virtual open carries L across the
            // later ensure and materialization producer boundaries.
            let work = self.filesystem_scope.try_work()?;
            let producer = self.host.try_foreground()?;
            self.transaction = Some(cache::ForegroundTransaction::from_parts(
                producer,
                vec![work],
            )?);
        }
        Ok(self
            .transaction
            .as_ref()
            .expect("CidTree transaction exists"))
    }

    fn logical_retention(&self) -> Arc<TraversalLogicalLease> {
        Arc::clone(
            self.logical
                .as_ref()
                .expect("CidTree logical admission was initialized"),
        )
    }

    pub(crate) fn into_logical(mut self) -> Result<cache::LogicalIoPermit> {
        let logical = self
            .logical
            .take()
            .expect("Cell traversal always starts with logical admission");
        Arc::try_unwrap(logical)
            .map(|lease| lease.permit)
            .map_err(|_| anyhow::anyhow!("CidTree traversal is still active"))
    }
}

impl CidTree {
    /// Create a new CidTree with the given root CID.
    pub fn new(
        root_cid: Cid,
        ipfs: ipfs::HttpClient,
        staging_dir: PathBuf,
        host: Arc<cache::HostResources>,
        filesystem_scope: cache::FsScope,
    ) -> Self {
        Self {
            root: ArcSwap::from_pointee(root_cid),
            ipfs,
            dir_cache: Mutex::new(LruCache::new(
                NonZeroUsize::new(DIR_CACHE_CAPACITY).unwrap(),
            )),
            staging_dir,
            host,
            filesystem_scope,
        }
    }

    fn standalone_traversal(&self) -> CidTreeTraversal {
        CidTreeTraversal {
            host: Arc::clone(&self.host),
            filesystem_scope: self.filesystem_scope.clone(),
            logical: None,
            transaction: None,
        }
    }

    pub(crate) fn traversal_in(
        &self,
        filesystem_scope: &cache::FsScope,
        logical: cache::LogicalIoPermit,
    ) -> std::result::Result<CidTreeTraversal, cache::AdmissionError> {
        self.host.validate_logical(&logical)?;
        Ok(CidTreeTraversal {
            host: Arc::clone(&self.host),
            filesystem_scope: filesystem_scope.clone(),
            logical: Some(Arc::new(TraversalLogicalLease { permit: logical })),
            transaction: None,
        })
    }

    /// The current root CID.
    pub fn root_cid(&self) -> Arc<Cid> {
        self.root.load_full()
    }

    /// Atomically swap the root CID for epoch updates.
    ///
    /// Clears the in-memory directory listing cache. This activation operation
    /// does not perform staging-directory cleanup.
    pub fn swap_root(&self, new_cid: Cid) {
        self.root.store(Arc::new(new_cid));

        if let Ok(mut cache) = self.dir_cache.lock() {
            cache.clear();
        }
    }

    /// Remove readdir stubs after a completed generation transition.
    ///
    /// Callers must stop all users of this staging directory before cleanup,
    /// including users of other CidTree instances sharing the same directory.
    /// Published CID-keyed stubs are immutable until this quiescent cleanup.
    /// Content-addressed staged files remain under `PinsetCache` ownership.
    pub fn cleanup_stubs(&self) {
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
    pub async fn pre_warm(&self, cid: &Cid) -> Result<()> {
        let _ = self.ls_dir(cid).await?;
        Ok(())
    }

    /// List directory entries for a CID, using the 3-tier cache.
    ///
    /// 1. In-memory LRU cache (hit → return immediately)
    /// 2. Staging disk (hit → populate LRU, return)
    /// 3. IPFS daemon `ls()` (populate both caches, return)
    pub async fn ls_dir(&self, cid: &Cid) -> Result<Vec<DirEntry>> {
        let mut traversal = self.standalone_traversal();
        self.ls_dir_in(&mut traversal, cid).await
    }

    pub(crate) async fn ls_dir_in(
        &self,
        traversal: &mut CidTreeTraversal,
        cid: &Cid,
    ) -> Result<Vec<DirEntry>> {
        // Tier 1: in-memory LRU
        if let Some(entries) = self
            .dir_cache
            .lock()
            .ok()
            .and_then(|mut c| c.get(cid).cloned())
        {
            return Ok(entries);
        }

        traversal.ensure_transaction()?;
        let transaction = traversal
            .transaction
            .as_ref()
            .expect("CidTree transaction exists");
        let logical = traversal.logical_retention();
        self.ls_dir_under_retaining(transaction, cid, Some(logical))
            .await
    }

    /// Complete a listing under an already-admitted serial producer. This is
    /// used when directory publication owns the producer across listing,
    /// construction, publication, descriptor open, and result installation.
    pub(crate) async fn ls_dir_under(
        &self,
        transaction: &cache::ForegroundTransaction,
        cid: &Cid,
    ) -> Result<Vec<DirEntry>> {
        self.ls_dir_under_retaining(transaction, cid, None).await
    }

    async fn ls_dir_under_retaining(
        &self,
        transaction: &cache::ForegroundTransaction,
        cid: &Cid,
        logical: Option<Arc<TraversalLogicalLease>>,
    ) -> Result<Vec<DirEntry>> {
        // Recheck after admission because another traversal may have populated
        // the shared in-memory tier while this caller was being admitted.
        if let Some(entries) = self
            .dir_cache
            .lock()
            .ok()
            .and_then(|mut cache| cache.get(cid).cloned())
        {
            return Ok(entries);
        }

        // Tier 2: staging disk
        let disk_path = self.staging_dir.join(format!("{cid}{DIRLIST_SUFFIX}"));
        let read_path = disk_path.clone();
        let read_logical = logical.clone();
        if let Some(entries) = transaction
            .run_blocking(cache::FilesystemOperation::ListingRead, move || {
                let _logical = read_logical;
                std::fs::read_to_string(read_path)
                    .ok()
                    .and_then(|data| serde_json::from_str::<Vec<DirEntry>>(&data).ok())
            })
            .await?
            .into_inner()
        {
            // Populate LRU from disk
            if let Ok(mut cache) = self.dir_cache.lock() {
                cache.put(*cid, entries.clone());
            }
            return Ok(entries);
        }

        // Tier 3: IPFS daemon
        let ipfs_path = format!("/ipfs/{cid}");
        let ipfs = self.ipfs.clone();
        let network_logical = logical.clone();
        let raw_entries = transaction
            .run_operation(cache::FilesystemOperation::ListingRead, async move {
                let _logical = network_logical;
                ipfs.ls(&ipfs_path).await
            })
            .await?
            .into_inner()
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
        let persisted_entries = entries.clone();
        let write_logical = logical;
        transaction
            .run_blocking(cache::FilesystemOperation::ListingWrite, move || {
                let _logical = write_logical;
                if let Ok(json) = serde_json::to_string(&persisted_entries) {
                    if let Some(parent) = disk_path.parent() {
                        let _ = std::fs::create_dir_all(parent);
                    }
                    let _ = std::fs::write(&disk_path, json);
                }
            })
            .await?
            .into_inner();

        // Populate LRU
        if let Ok(mut cache) = self.dir_cache.lock() {
            cache.put(*cid, entries.clone());
        }

        Ok(entries)
    }

    /// Resolve a guest path from a snapshot of the current root.
    ///
    /// This compatibility entry point accepts leading slashes. Descriptor-relative
    /// callers use `resolve_at`, which rejects absolute guest paths.
    pub async fn resolve_path(&self, path: &str) -> Result<ResolvedNode> {
        let mut traversal = self.standalone_traversal();
        self.resolve_path_in(&mut traversal, path).await
    }

    pub(crate) async fn resolve_path_in(
        &self,
        traversal: &mut CidTreeTraversal,
        path: &str,
    ) -> Result<ResolvedNode> {
        let root = self.root_cid();
        Ok(self
            .resolve_path_inner(traversal, &root, path, path.ends_with('/'), 0)
            .await?
            .node)
    }

    /// Resolve `path` relative to a directory in a captured root snapshot.
    ///
    /// `base_path` is the canonical root-relative path returned by a prior
    /// resolution. Empty paths denote the base directory. Repeated separators
    /// are ignored; dot, percent escapes, and backslashes remain literal names.
    /// Absolute paths and parent traversal are rejected before any lookup.
    pub async fn resolve_at(
        &self,
        root_cid: &Cid,
        base_path: &str,
        path: &str,
    ) -> Result<ResolvedPath> {
        let mut traversal = self.standalone_traversal();
        self.resolve_at_in(&mut traversal, root_cid, base_path, path)
            .await
    }

    pub(crate) async fn resolve_at_in(
        &self,
        traversal: &mut CidTreeTraversal,
        root_cid: &Cid,
        base_path: &str,
        path: &str,
    ) -> Result<ResolvedPath> {
        Self::validate_relative_path(base_path, path)?;
        let full_path = if base_path.is_empty() {
            path.to_string()
        } else {
            // Retaining this separator also requires a directory when path is empty.
            format!("{base_path}/{path}")
        };
        self.resolve_path_inner(traversal, root_cid, &full_path, full_path.ends_with('/'), 0)
            .await
    }

    /// Validate descriptor-relative syntax without touching a cache or the
    /// filesystem. Virtual opens call this before reserving logical capacity.
    pub(crate) fn validate_relative_path(base_path: &str, path: &str) -> Result<()> {
        for relative in [base_path, path] {
            if relative.starts_with('/') || relative.split('/').any(|part| part == "..") {
                return Err(ResolveError::InvalidPath.into());
            }
        }
        Ok(())
    }

    fn resolve_path_inner<'a>(
        &'a self,
        traversal: &'a mut CidTreeTraversal,
        root_cid: &'a Cid,
        path: &'a str,
        require_directory: bool,
        symlink_depth: usize,
    ) -> futures::future::BoxFuture<'a, Result<ResolvedPath>> {
        Box::pin(self.resolve_path_inner_impl(
            traversal,
            root_cid,
            path,
            require_directory,
            symlink_depth,
        ))
    }

    async fn resolve_path_inner_impl(
        &self,
        traversal: &mut CidTreeTraversal,
        root_cid: &Cid,
        path: &str,
        require_directory: bool,
        symlink_depth: usize,
    ) -> Result<ResolvedPath> {
        if symlink_depth > MAX_SYMLINK_DEPTH {
            bail!("symlink depth exceeded (max {MAX_SYMLINK_DEPTH})");
        }
        if path.split('/').any(|part| part == "..") {
            return Err(ResolveError::InvalidPath.into());
        }

        let components: Vec<&str> = path.split('/').filter(|part| !part.is_empty()).collect();
        let mut current_cid = *root_cid;

        for (i, component) in components.iter().enumerate() {
            let entries = self.ls_dir_in(traversal, &current_cid).await?;
            let entry = entries
                .iter()
                .find(|entry| entry.name == *component)
                .ok_or(ResolveError::NoEntry)
                .with_context(|| {
                    format!("path component '{component}' not found in CID {current_cid} (full path: {path})")
                })?;
            let is_last = i == components.len() - 1;

            match &entry.entry_type {
                EntryType::Dir => {
                    current_cid = ipfs::cid_identity::parse_cid(&entry.cid)
                        .context(ResolveError::InvalidCid)?;
                }
                EntryType::File => {
                    if !is_last || require_directory {
                        return Err(ResolveError::NotDirectory.into());
                    }
                    return Ok(ResolvedPath {
                        node: ResolvedNode::CidFile {
                            cid: ipfs::cid_identity::parse_cid(&entry.cid)
                                .context(ResolveError::InvalidCid)?,
                            size: entry.size,
                        },
                        path: components.join("/"),
                    });
                }
                EntryType::Symlink { target } => {
                    let parent = components[..i].join("/");
                    let mut resolved_target = if target.starts_with('/') || parent.is_empty() {
                        target.clone()
                    } else {
                        format!("{parent}/{target}")
                    };
                    if !is_last {
                        resolved_target.push('/');
                        resolved_target.push_str(&components[i + 1..].join("/"));
                    }
                    return self
                        .resolve_path_inner(
                            traversal,
                            root_cid,
                            &resolved_target,
                            require_directory || resolved_target.ends_with('/'),
                            symlink_depth + 1,
                        )
                        .await;
                }
            }
        }

        Ok(ResolvedPath {
            node: ResolvedNode::CidDir { cid: current_cid },
            path: components.join("/"),
        })
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
    /// from this directory, resolving descriptor-relative paths through
    /// `CidTree::resolve_at_in`. The directory's actual on-disk contents
    /// are dir-listing stubs (sparse files with correct sizes, populated
    /// by `fs_intercept` on demand
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

    #[cfg(test)]
    pub(crate) fn test_cell_resources(&self) -> Arc<cache::CellResources> {
        cache::CellResources::new(Arc::clone(&self.host), cache::CellLimits::default())
    }

    #[cfg(test)]
    pub(crate) fn test_filesystem_scope(&self) -> cache::FsScope {
        self.filesystem_scope.clone()
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
    use std::ops::Deref;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    struct TestTree {
        tree: CidTree,
        _scope_owner: cache::FsOwner,
        _scope_finalizer: cache::FsFinalizer,
        _host_owner: cache::HostFsOwner,
    }

    impl TestTree {
        fn new(root: Cid, ipfs: ipfs::HttpClient, staging: PathBuf) -> Self {
            Self::with_limits(root, ipfs, staging, cache::HostLimits::default())
        }

        fn with_limits(
            root: Cid,
            ipfs: ipfs::HttpClient,
            staging: PathBuf,
            limits: cache::HostLimits,
        ) -> Self {
            let (host_owner, host) =
                cache::HostFsOwner::start(tokio::runtime::Handle::current(), limits).unwrap();
            let (scope, scope_owner, scope_finalizer, _) = host.try_filesystem_scope().unwrap();
            Self {
                tree: CidTree::new(root, ipfs, staging, host, scope),
                _scope_owner: scope_owner,
                _scope_finalizer: scope_finalizer,
                _host_owner: host_owner,
            }
        }
    }

    impl Deref for TestTree {
        type Target = CidTree;

        fn deref(&self) -> &Self::Target {
            &self.tree
        }
    }

    fn fixture_cid(tag: u8) -> Cid {
        cid::Cid::new_v1(
            0x70,
            cid::multihash::Multihash::<64>::wrap(0x12, &[tag; 32]).unwrap(),
        )
    }

    fn fixture_entry(name: &str, tag: u8, entry_type: EntryType) -> DirEntry {
        DirEntry {
            name: name.to_string(),
            cid: fixture_cid(tag).to_string(),
            entry_type,
            size: 7,
        }
    }

    fn fixture_listing(staging: &Path, tag: u8, entries: &[DirEntry]) {
        std::fs::write(
            staging.join(format!("{}{DIRLIST_SUFFIX}", fixture_cid(tag))),
            serde_json::to_vec(entries).unwrap(),
        )
        .unwrap();
    }

    fn fixture_tree(staging: &Path) -> TestTree {
        fixture_listing(
            staging,
            1,
            &[
                fixture_entry("child", 10, EntryType::File),
                fixture_entry("nested", 2, EntryType::Dir),
            ],
        );
        fixture_listing(
            staging,
            2,
            &[
                fixture_entry("child", 11, EntryType::File),
                fixture_entry("deeper", 3, EntryType::Dir),
            ],
        );
        fixture_listing(staging, 3, &[fixture_entry("child", 12, EntryType::File)]);
        TestTree::new(
            fixture_cid(1),
            ipfs::HttpClient::new("http://127.0.0.1:1".to_string()),
            staging.to_path_buf(),
        )
    }

    #[tokio::test]
    async fn typed_root_replacement_preserves_snapshot_on_invalid_ingress() {
        use ipfs::cid_identity::parse_cid;
        let staging = tempfile::TempDir::new().unwrap();
        let original = fixture_cid(1);
        let alias = original
            .to_string_of_base(cid::multibase::Base::Base58Btc)
            .unwrap();
        let tree = TestTree::new(
            parse_cid(&alias).unwrap(),
            ipfs::HttpClient::new("http://127.0.0.1:1".into()),
            staging.path().into(),
        );
        let snapshot: Arc<cid::Cid> = tree.root_cid();
        assert_eq!(*snapshot, original);
        for malformed in [
            String::new(),
            "not-a-cid".into(),
            format!("/ipfs/{original}"),
        ] {
            let replacement = parse_cid(&malformed).map(|cid| tree.swap_root(cid));
            assert!(replacement.is_err());
            assert_eq!(*tree.root_cid(), original);
        }
        let next = fixture_cid(2);
        tree.swap_root(next);
        assert_eq!(*tree.root_cid(), next);
        assert_eq!(*snapshot, original);
        assert_eq!(std::fs::read_dir(staging.path()).unwrap().count(), 0);
    }

    #[tokio::test]
    async fn resolve_path_rejects_trailing_slash_on_file() {
        let staging = tempfile::TempDir::new().unwrap();
        let tree = fixture_tree(staging.path());
        tree.resolve_path("child/")
            .await
            .expect_err("a trailing slash requires a directory");
    }

    #[tokio::test]
    async fn resolve_at_keeps_each_base_and_canonical_chained_path() {
        let staging = tempfile::TempDir::new().unwrap();
        let tree = fixture_tree(staging.path());
        let root = tree.root_cid();
        let nested = tree.resolve_at(&root, "", "nested//").await.unwrap();
        assert_eq!(nested.path, "nested");
        let deeper = tree
            .resolve_at(&root, &nested.path, "deeper")
            .await
            .unwrap();
        assert_eq!(deeper.path, "nested/deeper");

        for (base, expected_path, expected_tag) in [
            ("", "child", 10),
            (nested.path.as_str(), "nested/child", 11),
            (deeper.path.as_str(), "nested/deeper/child", 12),
        ] {
            let resolved = tree.resolve_at(&root, base, "child").await.unwrap();
            assert_eq!(resolved.path, expected_path);
            assert!(
                matches!(resolved.node, ResolvedNode::CidFile { cid, .. } if cid == fixture_cid(expected_tag))
            );
        }
    }

    #[tokio::test]
    async fn one_foreground_lease_spans_a_traversal_and_memory_hits_need_none() {
        let staging = tempfile::TempDir::new().unwrap();
        fixture_listing(
            staging.path(),
            1,
            &[fixture_entry("nested", 2, EntryType::Dir)],
        );
        fixture_listing(
            staging.path(),
            2,
            &[fixture_entry("deeper", 3, EntryType::Dir)],
        );
        fixture_listing(
            staging.path(),
            3,
            &[fixture_entry("child", 12, EntryType::File)],
        );
        fixture_listing(staging.path(), 99, &[]);
        let tree = TestTree::with_limits(
            fixture_cid(1),
            ipfs::HttpClient::new("http://127.0.0.1:1".to_string()),
            staging.path().to_path_buf(),
            cache::HostLimits::new(4, 2, 4).unwrap(),
        );

        let resolved = tree.resolve_path("nested/deeper/child").await.unwrap();
        assert!(matches!(resolved, ResolvedNode::CidFile { cid, .. } if cid == fixture_cid(12)));

        let held_foreground = tree.host.try_foreground().unwrap();
        assert_eq!(tree.ls_dir(&fixture_cid(1)).await.unwrap().len(), 1);
        let error = tree
            .ls_dir(&fixture_cid(99))
            .await
            .expect_err("a disk lookup requires foreground admission");
        assert_eq!(
            error
                .downcast_ref::<cache::AdmissionError>()
                .map(|error| error.kind()),
            Some(cache::AdmissionKind::ForegroundProducer)
        );
        drop(held_foreground);
    }

    #[tokio::test]
    async fn cancelled_virtual_traversal_retains_logical_until_driver_work_finishes() {
        let staging = tempfile::TempDir::new().unwrap();
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let (entered_tx, entered_rx) = tokio::sync::oneshot::channel();
        let (release_tx, release_rx) = tokio::sync::oneshot::channel();
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut request = [0u8; 4096];
            assert!(stream.read(&mut request).await.unwrap() > 0);
            entered_tx.send(()).unwrap();
            release_rx.await.unwrap();
            stream
                .write_all(
                    b"HTTP/1.1 200 OK\r\nContent-Length: 26\r\nConnection: close\r\n\r\n{\"Objects\":[{\"Links\":[]}]}",
                )
                .await
                .unwrap();
        });
        let tree = Arc::new(TestTree::with_limits(
            fixture_cid(1),
            ipfs::HttpClient::new(format!("http://{address}")),
            staging.path().to_path_buf(),
            cache::HostLimits::new(1, 2, 4).unwrap(),
        ));
        let resources =
            cache::CellResources::new(Arc::clone(&tree.host), cache::CellLimits::new(1).unwrap());
        let task_tree = Arc::clone(&tree);
        let task_resources = Arc::clone(&resources);
        let task = tokio::spawn(async move {
            let logical = task_resources.try_logical_io().unwrap();
            let mut traversal = task_tree
                .traversal_in(&task_tree.filesystem_scope, logical)
                .unwrap();
            task_tree.resolve_path_in(&mut traversal, "child").await
        });

        entered_rx.await.unwrap();
        task.abort();
        assert!(task.await.unwrap_err().is_cancelled());
        assert_eq!(
            resources.try_logical_io().unwrap_err().kind(),
            cache::AdmissionKind::CellLogical,
            "driver-retained listing work must retain the virtual open's logical permit"
        );

        release_tx.send(()).unwrap();
        server.await.unwrap();
        let released = tokio::time::timeout(std::time::Duration::from_secs(10), async {
            loop {
                if let Ok(logical) = resources.try_logical_io() {
                    break logical;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("logical admission was not released after driver completion");
        drop(released);
    }

    #[tokio::test]
    async fn resolve_at_preserves_snapshot_and_canonical_symlink_targets() {
        let staging = tempfile::TempDir::new().unwrap();
        let tree = fixture_tree(staging.path());
        fixture_listing(
            staging.path(),
            1,
            &[
                fixture_entry("nested", 2, EntryType::Dir),
                fixture_entry(
                    "relative",
                    20,
                    EntryType::Symlink {
                        target: "nested//deeper".to_string(),
                    },
                ),
                fixture_entry(
                    "absolute",
                    21,
                    EntryType::Symlink {
                        target: "/nested/deeper".to_string(),
                    },
                ),
            ],
        );
        fixture_listing(staging.path(), 4, &[]);
        fixture_listing(
            staging.path(),
            2,
            &[
                fixture_entry("child", 11, EntryType::File),
                fixture_entry("deeper", 3, EntryType::Dir),
                fixture_entry(
                    "alias",
                    22,
                    EntryType::Symlink {
                        target: "deeper".to_string(),
                    },
                ),
            ],
        );
        let root = tree.root_cid();
        let nested = tree.resolve_at(&root, "", "nested").await.unwrap();
        tree.swap_root(fixture_cid(4));

        for path in ["relative", "absolute", "nested/alias"] {
            let directory = tree.resolve_at(&root, "", path).await.unwrap();
            assert_eq!(directory.path, "nested/deeper");
            let child = tree
                .resolve_at(&root, &directory.path, "child")
                .await
                .unwrap();
            assert_eq!(child.path, "nested/deeper/child");
            assert!(
                matches!(child.node, ResolvedNode::CidFile { cid, .. } if cid == fixture_cid(12))
            );
        }
        let child = tree.resolve_at(&root, &nested.path, "child").await.unwrap();
        assert!(matches!(child.node, ResolvedNode::CidFile { cid, .. } if cid == fixture_cid(11)));
        assert!(tree.resolve_path("nested/child").await.is_err());
    }

    #[tokio::test]
    async fn resolve_at_rejects_escape_before_lookup_and_preserves_literal_names() {
        let staging = tempfile::TempDir::new().unwrap();
        let tree = fixture_tree(staging.path());
        let root = tree.root_cid();
        for path in ["..", "../sibling", "a/../../b", "/child", "//child", "/"] {
            let error = tree.resolve_at(&root, "missing", path).await.unwrap_err();
            assert_eq!(
                error.downcast_ref::<ResolveError>(),
                Some(&ResolveError::InvalidPath),
                "{path}: {error:#}"
            );
        }
        fixture_listing(
            staging.path(),
            2,
            &[
                fixture_entry("%2e%2e", 30, EntryType::File),
                fixture_entry("%2f", 31, EntryType::File),
                fixture_entry("..\\child", 32, EntryType::File),
                fixture_entry("ipfs", 3, EntryType::Dir),
            ],
        );
        for (path, tag) in [
            ("%2e%2e", 30),
            ("%2f", 31),
            ("..\\child", 32),
            ("ipfs//child", 12),
        ] {
            let resolved = tree.resolve_at(&root, "nested", path).await.unwrap();
            assert!(
                matches!(resolved.node, ResolvedNode::CidFile { cid, .. } if cid == fixture_cid(tag))
            );
        }
        let error = tree.resolve_at(&root, "nested", ".").await.unwrap_err();
        assert_eq!(
            error.downcast_ref::<ResolveError>(),
            Some(&ResolveError::NoEntry)
        );
    }

    #[tokio::test]
    async fn resolve_at_distinguishes_missing_entries_and_non_directories() {
        let staging = tempfile::TempDir::new().unwrap();
        let tree = fixture_tree(staging.path());
        let root = tree.root_cid();
        for (base, path, expected) in [
            ("", "missing", ResolveError::NoEntry),
            ("nested", "missing", ResolveError::NoEntry),
            ("", "child/", ResolveError::NotDirectory),
            ("", "child/other", ResolveError::NotDirectory),
            ("child", "", ResolveError::NotDirectory),
            ("child", "other", ResolveError::NotDirectory),
        ] {
            let error = tree.resolve_at(&root, base, path).await.unwrap_err();
            assert_eq!(
                error.downcast_ref::<ResolveError>(),
                Some(&expected),
                "{base}/{path}: {error:#}"
            );
        }
        let directory = tree.resolve_at(&root, "nested", "").await.unwrap();
        assert_eq!(directory.path, "nested");
        assert!(matches!(directory.node, ResolvedNode::CidDir { cid } if cid == fixture_cid(2)));
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

    #[tokio::test]
    async fn resolve_path_rejects_invalid_intermediate_directory_cid_before_io() {
        let root_cid = "QmYwAPJzv5CZsnN625s3Xf2nemtYgPpHdWEz79ojWnPbdG";
        let root = tempfile::TempDir::new().unwrap();
        let staging = root.path().join("staging");
        std::fs::create_dir(&staging).unwrap();
        std::fs::create_dir(staging.join("segment")).unwrap();
        let entries = vec![DirEntry {
            name: "hostile".to_string(),
            cid: "segment/../../escaped".to_string(),
            entry_type: EntryType::Dir,
            size: 0,
        }];
        std::fs::write(
            staging.join(format!("{root_cid}{DIRLIST_SUFFIX}")),
            serde_json::to_vec(&entries).unwrap(),
        )
        .unwrap();

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let lookups = Arc::new(AtomicUsize::new(0));
        let server_lookups = Arc::clone(&lookups);
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            server_lookups.fetch_add(1, Ordering::SeqCst);
            let mut request = [0u8; 4096];
            let bytes_read = stream.read(&mut request).await.unwrap();
            assert!(bytes_read > 0, "Kubo request must not be empty");
            stream
                .write_all(
                    b"HTTP/1.1 200 OK\r\nContent-Length: 26\r\nConnection: close\r\n\r\n{\"Objects\":[{\"Links\":[]}]}",
                )
                .await
                .unwrap();
        });
        let tree = TestTree::new(
            ipfs::cid_identity::parse_cid(root_cid).unwrap(),
            ipfs::HttpClient::new(format!("http://{address}")),
            staging.clone(),
        );

        let error = tree
            .resolve_path("hostile/child")
            .await
            .expect_err("an invalid intermediate directory CID must fail resolution");

        assert!(
            error.downcast_ref::<ResolveError>() == Some(&ResolveError::InvalidCid),
            "unexpected resolution error: {error:#}"
        );
        assert_eq!(
            lookups.load(Ordering::SeqCst),
            0,
            "an invalid CID must not reach Kubo"
        );
        assert!(
            !root
                .path()
                .join(format!("escaped{DIRLIST_SUFFIX}"))
                .exists(),
            "an invalid CID must not create a cache file outside staging"
        );
        assert_eq!(
            std::fs::read_dir(root.path()).unwrap().count(),
            1,
            "resolution must not create any path outside staging"
        );

        server.abort();
        let _ = server.await;
    }

    #[tokio::test]
    async fn valid_cid_representations_share_canonical_directory_cache_identity() {
        let canonical = "bafkreibm6jg3ux5quy7flfgn5gmxk5ubm6yur3apcu3to3d6tmjzptm2ye";
        let parsed = canonical.parse::<cid::Cid>().unwrap();
        let alternate = parsed
            .to_string_of_base(cid::multibase::Base::Base58Btc)
            .unwrap();
        assert_ne!(alternate, canonical);

        let staging = tempfile::TempDir::new().unwrap();
        let entries = vec![DirEntry {
            name: "child".to_string(),
            cid: canonical.to_string(),
            entry_type: EntryType::File,
            size: 7,
        }];
        let disk_path = staging.path().join(format!("{canonical}{DIRLIST_SUFFIX}"));
        std::fs::write(&disk_path, serde_json::to_vec(&entries).unwrap()).unwrap();
        let tree = TestTree::new(
            ipfs::cid_identity::parse_cid(&alternate).unwrap(),
            ipfs::HttpClient::new("http://127.0.0.1:1".to_string()),
            staging.path().to_path_buf(),
        );

        let resolved = tree.resolve_path("child").await.unwrap();
        assert!(matches!(
            resolved,
            ResolvedNode::CidFile { cid, size: 7 } if cid == parsed
        ));

        std::fs::remove_file(disk_path).unwrap();
        let cached = tree.ls_dir(&parsed).await.unwrap();
        assert_eq!(cached.len(), 1);
        assert_eq!(cached[0].name, "child");
        assert_eq!(tree.dir_cache.lock().unwrap().len(), 1);
        assert!(tree.dir_cache.lock().unwrap().contains(&parsed));
    }
}
