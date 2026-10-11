use std::collections::HashMap;
use std::io;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use crate::arc::ArcInner;
use crate::pinner::Pinner;
use anyhow::{Context, Result};
use cid::Cid;
use tempfile::TempDir;
use tokio::sync::{Mutex, Notify};

/// A cached pin entry. Tracks that a CID is pinned in the IPFS node
/// and its size (for weight-aware ARC eviction). Bytes are not held
/// in memory — the filesystem staging directory serves as the local cache.
pub struct PinEntry {
    /// Size in bytes of the pinned content.
    pub size: u64,
}

struct CacheState {
    arc: ArcInner<PinEntry>,
    inflight: HashMap<Cid, Arc<Notify>>,
}

struct EnsureCancellationGuard {
    state: Arc<Mutex<CacheState>>,
    pinner: Arc<dyn Pinner>,
    cid: Cid,
    notify: Arc<Notify>,
    pin_started: bool,
    armed: bool,
}

impl EnsureCancellationGuard {
    fn new(
        state: Arc<Mutex<CacheState>>,
        pinner: Arc<dyn Pinner>,
        cid: Cid,
        notify: Arc<Notify>,
    ) -> Self {
        Self {
            state,
            pinner,
            cid,
            notify,
            pin_started: false,
            armed: true,
        }
    }

    fn mark_pin_started(&mut self) {
        self.pin_started = true;
    }

    fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for EnsureCancellationGuard {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }

        let state = Arc::clone(&self.state);
        let pinner = Arc::clone(&self.pinner);
        let cid = self.cid;
        let notify = Arc::clone(&self.notify);
        let pin_started = self.pin_started;
        tokio::spawn(async move {
            let owns_inflight = {
                let state = state.lock().await;
                state
                    .inflight
                    .get(&cid)
                    .is_some_and(|current| Arc::ptr_eq(current, &notify))
            };
            if !owns_inflight {
                return;
            }

            if pin_started {
                // Keep the in-flight slot occupied until rollback completes.
                // Otherwise a waiter could install a fresh pin that this
                // cleanup would then accidentally remove.
                //
                // Pin RPC cancellation does not prove whether the backend
                // committed. Best-effort unpin prevents an untracked pin from
                // escaping the cache lifecycle. Cancellation before pin
                // begins never unpins someone else's existing state.
                unpin_with_retry(&*pinner, &cid).await;
            }

            let removed = {
                let mut state = state.lock().await;
                let still_owns_inflight = state
                    .inflight
                    .get(&cid)
                    .is_some_and(|current| Arc::ptr_eq(current, &notify));
                if still_owns_inflight {
                    state.inflight.remove(&cid);
                }
                still_owns_inflight
            };
            if removed {
                notify.notify_waiters();
            }
        });
    }
}

/// CID-keyed cache backed by IPFS pins, with a weight-aware ARC eviction policy.
///
/// Manages which CIDs stay pinned in the IPFS node. Does not hold file
/// content in memory — callers either fetch bytes on demand or stream content
/// directly to local staging files.
///
/// Thread-safe: the inner ARC is wrapped in a `Mutex`. The expensive I/O
/// operations (pin, unpin) happen outside the lock.
pub struct PinsetCache {
    state: Arc<Mutex<CacheState>>,
    pinner: Arc<dyn Pinner>,
    /// Host-wide staging directory for materialized IPFS content.
    /// Shared across all processes using `CacheMode::Shared`.
    staging: TempDir,
    /// Lock-free bloom filter for definite-miss detection.
    /// Bits are set after a successful `put()` and never cleared.
    bloom: crate::bloom::AtomicBloom,
}

/// Maximum retry attempts for failed unpins.
const MAX_UNPIN_RETRIES: u32 = 3;
static MATERIALIZATION_SEQ: AtomicU64 = AtomicU64::new(1);

struct MaterializationGuard {
    temporary_path: std::path::PathBuf,
    committed: bool,
    #[cfg(test)]
    cleanup_hook: Option<tests::MaterializationCleanupHook>,
}

enum MaterializationCommit {
    Published,
    Existing,
}

impl MaterializationGuard {
    fn new(
        destination: &Path,
        #[cfg(test)] cleanup_hook: Option<tests::MaterializationCleanupHook>,
    ) -> Result<Self> {
        let parent = destination
            .parent()
            .ok_or_else(|| anyhow::anyhow!("materialization destination has no parent"))?;
        std::fs::create_dir_all(parent)?;
        let name = destination
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("cid");
        let seq = MATERIALIZATION_SEQ.fetch_add(1, Ordering::Relaxed);
        Ok(Self {
            temporary_path: parent.join(format!(".{name}.ww-part-{seq}")),
            committed: false,
            #[cfg(test)]
            cleanup_hook,
        })
    }

    fn commit(&mut self, destination: &Path) -> Result<MaterializationCommit> {
        if destination.exists() {
            return Ok(MaterializationCommit::Existing);
        }
        std::fs::rename(&self.temporary_path, destination).with_context(|| {
            format!(
                "failed to commit CAS materialization to {}",
                destination.display()
            )
        })?;
        self.committed = true;
        Ok(MaterializationCommit::Published)
    }

    fn cleanup(&mut self) -> io::Result<()> {
        #[cfg(test)]
        if let Some(hook) = self.cleanup_hook.as_mut() {
            hook(&self.temporary_path)?;
        }
        let result = remove_materialized_path(&self.temporary_path);
        self.committed = result.is_ok();
        result
    }
}

fn cleanup_materialization_slot(slot: &mut Option<MaterializationGuard>) -> io::Result<()> {
    let result = slot.as_mut().map_or(Ok(()), MaterializationGuard::cleanup);
    if result.is_ok() {
        slot.take();
    }
    result
}

impl Drop for MaterializationGuard {
    fn drop(&mut self) {
        if !self.committed {
            let _ = remove_materialized_path(&self.temporary_path);
        }
    }
}

fn remove_materialized_path(path: &Path) -> io::Result<()> {
    if path.is_dir() {
        std::fs::remove_dir_all(path)
    } else {
        match std::fs::remove_file(path) {
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
            result => result,
        }
    }
}

async fn fetch_to_path_atomically(
    filesystem_scope: &crate::FsScope,
    pinner: &dyn Pinner,
    cid: &Cid,
    subpath: Option<&str>,
    destination: &Path,
) -> Result<()> {
    let destination = destination.to_owned();
    let setup_destination = destination.clone();
    let setup_scope = filesystem_scope.clone();
    #[cfg(test)]
    let cleanup_hook = tests::take_materialization_cleanup_hook();
    let guard = filesystem_scope
        .run_blocking(crate::FilesystemOperation::FileStage, move || {
            let guard = Arc::new(std::sync::Mutex::new(Some(MaterializationGuard::new(
                &setup_destination,
                #[cfg(test)]
                cleanup_hook,
            )?)));
            let cleanup_guard = Arc::clone(&guard);
            setup_scope
                .own_cleanup(crate::FilesystemOperation::FileStage, move || {
                    let mut guard = cleanup_guard
                        .lock()
                        .expect("materialization cleanup lock poisoned");
                    cleanup_materialization_slot(&mut guard)
                })
                .map_err(anyhow::Error::from)?;
            Ok::<_, anyhow::Error>(guard)
        })
        .await??;
    let temporary_path = guard
        .lock()
        .expect("materialization guard lock poisoned")
        .as_ref()
        .expect("materialization guard already consumed")
        .temporary_path
        .clone();
    let fetch_result = match subpath {
        Some(subpath) => {
            pinner
                .fetch_path_to_path(cid, subpath, &temporary_path)
                .await
        }
        None => pinner.fetch_to_path(cid, &temporary_path).await,
    };
    if let Err(error) = fetch_result {
        let cleanup_guard = Arc::clone(&guard);
        let cleanup = filesystem_scope
            .run_blocking(crate::FilesystemOperation::FileStage, move || {
                let mut guard = cleanup_guard
                    .lock()
                    .expect("materialization guard lock poisoned");
                cleanup_materialization_slot(&mut guard)
            })
            .await?;
        if let Err(cleanup_error) = cleanup {
            filesystem_scope.record_cleanup_failure(cleanup_error);
        }
        return Err(error);
    }
    let commit_scope = filesystem_scope.clone();
    filesystem_scope
        .run_blocking(crate::FilesystemOperation::FileStage, move || {
            let mut guard = guard.lock().expect("materialization guard lock poisoned");
            let commit = guard
                .as_mut()
                .expect("materialization guard already consumed")
                .commit(&destination);
            match commit {
                Ok(MaterializationCommit::Published) => {
                    guard.take();
                    Ok(())
                }
                Ok(MaterializationCommit::Existing) => {
                    if let Err(error) = cleanup_materialization_slot(&mut guard) {
                        let error = anyhow::Error::from(error);
                        commit_scope.record_cleanup_failure(&error);
                        return Err(error);
                    }
                    Ok(())
                }
                Err(error) => {
                    if let Err(cleanup_error) = cleanup_materialization_slot(&mut guard) {
                        commit_scope.record_cleanup_failure(cleanup_error);
                    }
                    Err(error)
                }
            }
        })
        .await??;
    Ok(())
}

async fn fetch_to_path_owned(
    filesystem_scope: &crate::FsScope,
    pinner: Arc<dyn Pinner>,
    cid: Cid,
    subpath: Option<String>,
    destination: std::path::PathBuf,
) -> Result<()> {
    let operation_scope = filesystem_scope.clone();
    filesystem_scope
        .run_operation(crate::FilesystemOperation::FileStage, async move {
            fetch_to_path_atomically(
                &operation_scope,
                &*pinner,
                &cid,
                subpath.as_deref(),
                &destination,
            )
            .await
        })
        .await??;
    Ok(())
}

async fn fetch_to_path_standalone(
    pinner: Arc<dyn Pinner>,
    cid: Cid,
    subpath: Option<String>,
    destination: std::path::PathBuf,
) -> Result<()> {
    let (scope, owner, finalizer, _completion) = crate::FsScope::new();
    let finalizer_task = tokio::spawn(finalizer.run());
    let result = fetch_to_path_owned(&scope, pinner, cid, subpath, destination).await;
    drop(owner);
    finalizer_task
        .await
        .context("filesystem finalizer failed")??;
    result
}

fn pin_entry_weighter(_k: &Cid, v: &PinEntry) -> usize {
    v.size as usize
}

impl PinsetCache {
    /// Default ghost capacity for the ARC cache.
    /// Each ghost entry is a CID key only (~40 bytes), so 4096 ghosts ≈ 160KB.
    const DEFAULT_GHOST_CAPACITY: usize = 4096;

    /// Create a new pinset cache.
    ///
    /// - `pinner`: the IPFS pin backend
    /// - `budget`: maximum total weight (bytes) of pinned entries managed by the ARC
    ///
    /// Creates a host-wide staging directory in `/tmp` for materialized IPFS content.
    pub fn new(pinner: Arc<dyn Pinner>, budget: usize) -> Result<Self> {
        let staging = TempDir::new().context("failed to create shared IPFS staging directory")?;
        Ok(Self {
            state: Arc::new(Mutex::new(CacheState {
                arc: ArcInner::new(budget, Self::DEFAULT_GHOST_CAPACITY, pin_entry_weighter),
                inflight: HashMap::new(),
            })),
            pinner,
            staging,
            bloom: crate::bloom::AtomicBloom::new(100_000, 0.00001),
        })
    }

    /// Path to the shared staging directory.
    pub fn staging_dir(&self) -> &Path {
        self.staging.path()
    }

    /// Ensure a CID is pinned in the IPFS node and tracked by the ARC.
    ///
    /// Concurrent callers for the same CID are deduplicated: only one pin
    /// operation is performed, and all waiters receive the result.
    /// Lock-free probabilistic check: returns true if the CID was probably cached
    /// at some point. False positives possible (stale entries), false negatives
    /// impossible for CIDs that completed a successful `ensure()`.
    pub fn probably_cached(&self, cid: &Cid) -> bool {
        self.bloom.probably_contains(cid)
    }

    pub async fn ensure(&self, cid: &Cid) -> Result<()> {
        // Check cache under lock.
        let notify = loop {
            // Re-evaluate after every wake. A waiter can begin before the
            // winning insertion sets the bloom bits.
            let maybe_cached = self.bloom.probably_contains(cid);
            let mut state = self.state.lock().await;

            if maybe_cached && state.arc.get(cid).is_some() {
                return Ok(());
            }

            // Check if another task is already pinning this CID.
            if let Some(notify) = state.inflight.get(cid) {
                let notify = Arc::clone(notify);
                let notified = notify.notified();
                tokio::pin!(notified);
                // Register while the state lock still prevents the owner from
                // removing the slot and broadcasting completion.
                notified.as_mut().enable();
                drop(state);

                // Wait for the in-flight pin to complete, then re-check.
                notified.await;
                continue;
            }

            // Register ourselves as the in-flight pinner.
            let notify = Arc::new(Notify::new());
            state.inflight.insert(*cid, Arc::clone(&notify));
            break notify;
        };

        let mut cancellation = EnsureCancellationGuard::new(
            Arc::clone(&self.state),
            Arc::clone(&self.pinner),
            *cid,
            Arc::clone(&notify),
        );

        // Slow path: size + pin outside the lock. Record the exact point at
        // which cancellation starts carrying an uncertain backend pin effect.
        let result: Result<PinEntry> = async {
            let size = self
                .pinner
                .size(cid)
                .await
                .context("failed to get size for CID")?;
            cancellation.mark_pin_started();
            self.pinner.pin(cid).await.context("failed to pin CID")?;
            Ok(PinEntry { size })
        }
        .await;

        // A failed pin may have committed remotely. Roll it back while this
        // operation still owns the in-flight slot so a waiter cannot repin
        // the CID until the uncertain effect has been removed.
        if result.is_err() && cancellation.pin_started {
            unpin_with_retry(&*self.pinner, cid).await;
            cancellation.pin_started = false;
        }

        // Re-lock and finalize.
        let mut state = self.state.lock().await;
        let owns_inflight = state
            .inflight
            .get(cid)
            .is_some_and(|current| Arc::ptr_eq(current, &notify));
        let removed_notify = if owns_inflight {
            state.inflight.remove(cid)
        } else {
            None
        };

        match result {
            Ok(entry) => {
                let evicted = state.arc.put(*cid, entry);
                self.bloom.insert(cid);

                // Notify waiters before spawning unpins.
                if let Some(n) = removed_notify {
                    n.notify_waiters();
                }

                // Unpin evicted entries in background.
                self.spawn_unpins(evicted);
                cancellation.disarm();

                Ok(())
            }
            Err(e) => {
                // Notify waiters that we failed.
                if let Some(n) = removed_notify {
                    n.notify_waiters();
                }
                cancellation.disarm();
                Err(e)
            }
        }
    }

    /// Fetch raw bytes for a CID from the IPFS node.
    ///
    /// The CID should already be pinned via a prior `ensure()` call.
    pub async fn fetch(&self, cid: &Cid) -> Result<Vec<u8>> {
        self.pinner.fetch(cid).await.context("failed to fetch CID")
    }

    /// Fetch raw bytes for a subpath under a CID from the IPFS node.
    ///
    /// The CID should already be pinned via a prior `ensure()` call.
    pub async fn fetch_path(&self, cid: &Cid, subpath: &str) -> Result<Vec<u8>> {
        self.pinner
            .fetch_path(cid, subpath)
            .await
            .with_context(|| format!("failed to fetch CID subpath /ipfs/{cid}/{subpath}"))
    }

    /// Stream CID content directly to `dst`.
    ///
    /// The CID should already be pinned via a prior `ensure()` call.
    pub async fn fetch_to_path(&self, cid: &Cid, dst: &Path) -> Result<()> {
        fetch_to_path_standalone(Arc::clone(&self.pinner), *cid, None, dst.to_owned())
            .await
            .with_context(|| format!("failed to stream CID /ipfs/{cid} to {}", dst.display()))
    }

    pub async fn fetch_to_path_in(
        &self,
        filesystem_scope: &crate::FsScope,
        cid: &Cid,
        dst: &Path,
    ) -> Result<()> {
        fetch_to_path_owned(
            filesystem_scope,
            Arc::clone(&self.pinner),
            *cid,
            None,
            dst.to_owned(),
        )
        .await
    }

    /// Stream CID subpath content directly to `dst`.
    ///
    /// The CID should already be pinned via a prior `ensure()` call.
    pub async fn fetch_path_to_path(&self, cid: &Cid, subpath: &str, dst: &Path) -> Result<()> {
        fetch_to_path_standalone(
            Arc::clone(&self.pinner),
            *cid,
            Some(subpath.to_owned()),
            dst.to_owned(),
        )
        .await
        .with_context(|| {
            format!(
                "failed to stream CID subpath /ipfs/{cid}/{subpath} to {}",
                dst.display()
            )
        })
    }

    pub async fn fetch_path_to_path_in(
        &self,
        filesystem_scope: &crate::FsScope,
        cid: &Cid,
        subpath: &str,
        dst: &Path,
    ) -> Result<()> {
        fetch_to_path_owned(
            filesystem_scope,
            Arc::clone(&self.pinner),
            *cid,
            Some(subpath.to_owned()),
            dst.to_owned(),
        )
        .await
    }

    /// Spawn background tasks to unpin evicted entries with retry.
    fn spawn_unpins(&self, evicted: Vec<(Cid, PinEntry)>) {
        for (cid, _entry) in evicted {
            let pinner = Arc::clone(&self.pinner);
            tokio::spawn(async move {
                unpin_with_retry(&*pinner, &cid).await;
            });
        }
    }
}

/// Unpin a CID with up to MAX_UNPIN_RETRIES attempts and exponential backoff.
async fn unpin_with_retry(pinner: &dyn Pinner, cid: &Cid) {
    let mut delay = Duration::from_millis(100);

    for attempt in 1..=MAX_UNPIN_RETRIES {
        match pinner.unpin(cid).await {
            Ok(()) => return,
            Err(e) => {
                if attempt == MAX_UNPIN_RETRIES {
                    tracing::warn!(
                        %cid,
                        attempts = MAX_UNPIN_RETRIES,
                        error = %e,
                        "failed to unpin evicted CID after all retries; pin leaked"
                    );
                    return;
                }
                tracing::debug!(
                    %cid,
                    attempt,
                    error = %e,
                    "unpin failed, retrying"
                );
                tokio::time::sleep(delay).await;
                delay *= 2;
            }
        }
    }
}

/// How a process interacts with the cache.
pub enum CacheMode {
    /// All processes share a global cache — efficient, but cache timing
    /// side-channels are possible between processes.
    Shared(Arc<PinsetCache>),
    /// Process gets its own isolated pinset — no shared state, no
    /// side-channels. All pins are removed when the process exits.
    Isolated(IsolatedPinset),
}

impl CacheMode {
    /// Transfer process-private staging cleanup to the Cell filesystem
    /// finalizer. Shared host cache state remains owned by the host cache.
    pub fn own_process_staging(&mut self, filesystem_scope: &crate::FsScope) -> Result<()> {
        match self {
            CacheMode::Shared(_) => Ok(()),
            CacheMode::Isolated(pinset) => pinset.transfer_staging_cleanup(filesystem_scope),
        }
    }

    /// Path to the staging directory for materialized IPFS content.
    ///
    /// - `Shared`: host-wide directory shared across all processes
    /// - `Isolated`: per-process directory, cleaned up on drop
    pub fn staging_dir(&self) -> &Path {
        match self {
            CacheMode::Shared(cache) => cache.staging_dir(),
            CacheMode::Isolated(pinset) => pinset.staging_dir(),
        }
    }

    /// Ensure a CID is pinned.
    pub async fn ensure(&self, cid: &Cid) -> Result<()> {
        match self {
            CacheMode::Shared(cache) => cache.ensure(cid).await,
            CacheMode::Isolated(pinset) => pinset.ensure(cid).await,
        }
    }

    /// Fetch raw bytes for a CID.
    pub async fn fetch(&self, cid: &Cid) -> Result<Vec<u8>> {
        match self {
            CacheMode::Shared(cache) => cache.fetch(cid).await,
            CacheMode::Isolated(pinset) => pinset.fetch(cid).await,
        }
    }

    /// Fetch raw bytes for a subpath under a CID.
    pub async fn fetch_path(&self, cid: &Cid, subpath: &str) -> Result<Vec<u8>> {
        match self {
            CacheMode::Shared(cache) => cache.fetch_path(cid, subpath).await,
            CacheMode::Isolated(pinset) => pinset.fetch_path(cid, subpath).await,
        }
    }

    /// Stream CID content directly to `dst`.
    pub async fn fetch_to_path(&self, cid: &Cid, dst: &Path) -> Result<()> {
        match self {
            CacheMode::Shared(cache) => cache.fetch_to_path(cid, dst).await,
            CacheMode::Isolated(pinset) => pinset.fetch_to_path(cid, dst).await,
        }
    }

    pub async fn fetch_to_path_in(
        &self,
        filesystem_scope: &crate::FsScope,
        cid: &Cid,
        dst: &Path,
    ) -> Result<()> {
        match self {
            CacheMode::Shared(cache) => cache.fetch_to_path_in(filesystem_scope, cid, dst).await,
            CacheMode::Isolated(pinset) => {
                pinset.fetch_to_path_in(filesystem_scope, cid, dst).await
            }
        }
    }

    /// Stream CID subpath content directly to `dst`.
    pub async fn fetch_path_to_path(&self, cid: &Cid, subpath: &str, dst: &Path) -> Result<()> {
        match self {
            CacheMode::Shared(cache) => cache.fetch_path_to_path(cid, subpath, dst).await,
            CacheMode::Isolated(pinset) => pinset.fetch_path_to_path(cid, subpath, dst).await,
        }
    }

    pub async fn fetch_path_to_path_in(
        &self,
        filesystem_scope: &crate::FsScope,
        cid: &Cid,
        subpath: &str,
        dst: &Path,
    ) -> Result<()> {
        match self {
            CacheMode::Shared(cache) => {
                cache
                    .fetch_path_to_path_in(filesystem_scope, cid, subpath, dst)
                    .await
            }
            CacheMode::Isolated(pinset) => {
                pinset
                    .fetch_path_to_path_in(filesystem_scope, cid, subpath, dst)
                    .await
            }
        }
    }
}

/// A per-process isolated pinset. Does not use ARC; simply tracks
/// what this process has pinned and unpins everything on drop.
pub struct IsolatedPinset {
    pinner: Arc<dyn Pinner>,
    pins: Mutex<HashMap<Cid, ()>>,
    /// Per-process staging directory. Cleaned up on drop.
    staging_path: std::path::PathBuf,
    staging: Option<TempDir>,
}

impl IsolatedPinset {
    /// Create a new isolated pinset with its own staging directory.
    pub fn new(pinner: Arc<dyn Pinner>) -> Result<Self> {
        let staging = TempDir::new().context("failed to create isolated IPFS staging directory")?;
        Ok(Self {
            pinner,
            pins: Mutex::new(HashMap::new()),
            staging_path: staging.path().to_owned(),
            staging: Some(staging),
        })
    }

    /// Path to the per-process staging directory.
    pub fn staging_dir(&self) -> &Path {
        &self.staging_path
    }

    fn transfer_staging_cleanup(&mut self, filesystem_scope: &crate::FsScope) -> Result<()> {
        if let Some(staging) = self.staging.take() {
            filesystem_scope
                .own_temp_dir(crate::FilesystemOperation::ProcessCleanup, staging)
                .map_err(anyhow::Error::from)?;
        }
        Ok(())
    }

    /// Ensure a CID is pinned. Idempotent within this pinset.
    ///
    /// Each IsolatedPinset pins independently in IPFS, giving cross-process
    /// refcounting: the IPFS node keeps content as long as any process holds
    /// a pin. Lock is held across the async pin to prevent double-pinning
    /// within a single process (contention is low for per-proc pinsets).
    pub async fn ensure(&self, cid: &Cid) -> Result<()> {
        let mut pins = self.pins.lock().await;
        if pins.contains_key(cid) {
            return Ok(());
        }

        self.pinner.pin(cid).await.context("failed to pin CID")?;
        pins.insert(*cid, ());

        Ok(())
    }

    /// Fetch raw bytes for a CID from the IPFS node.
    ///
    /// The CID should already be pinned via a prior `ensure()` call.
    pub async fn fetch(&self, cid: &Cid) -> Result<Vec<u8>> {
        self.pinner.fetch(cid).await.context("failed to fetch CID")
    }

    /// Fetch raw bytes for a subpath under a CID from the IPFS node.
    ///
    /// The CID should already be pinned via a prior `ensure()` call.
    pub async fn fetch_path(&self, cid: &Cid, subpath: &str) -> Result<Vec<u8>> {
        self.pinner
            .fetch_path(cid, subpath)
            .await
            .with_context(|| format!("failed to fetch CID subpath /ipfs/{cid}/{subpath}"))
    }

    /// Stream CID content directly to `dst`.
    ///
    /// The CID should already be pinned via a prior `ensure()` call.
    pub async fn fetch_to_path(&self, cid: &Cid, dst: &Path) -> Result<()> {
        fetch_to_path_standalone(Arc::clone(&self.pinner), *cid, None, dst.to_owned())
            .await
            .with_context(|| format!("failed to stream CID /ipfs/{cid} to {}", dst.display()))
    }

    pub async fn fetch_to_path_in(
        &self,
        filesystem_scope: &crate::FsScope,
        cid: &Cid,
        dst: &Path,
    ) -> Result<()> {
        fetch_to_path_owned(
            filesystem_scope,
            Arc::clone(&self.pinner),
            *cid,
            None,
            dst.to_owned(),
        )
        .await
    }

    /// Stream CID subpath content directly to `dst`.
    ///
    /// The CID should already be pinned via a prior `ensure()` call.
    pub async fn fetch_path_to_path(&self, cid: &Cid, subpath: &str, dst: &Path) -> Result<()> {
        fetch_to_path_standalone(
            Arc::clone(&self.pinner),
            *cid,
            Some(subpath.to_owned()),
            dst.to_owned(),
        )
        .await
        .with_context(|| {
            format!(
                "failed to stream CID subpath /ipfs/{cid}/{subpath} to {}",
                dst.display()
            )
        })
    }

    pub async fn fetch_path_to_path_in(
        &self,
        filesystem_scope: &crate::FsScope,
        cid: &Cid,
        subpath: &str,
        dst: &Path,
    ) -> Result<()> {
        fetch_to_path_owned(
            filesystem_scope,
            Arc::clone(&self.pinner),
            *cid,
            Some(subpath.to_owned()),
            dst.to_owned(),
        )
        .await
    }
}

impl Drop for IsolatedPinset {
    fn drop(&mut self) {
        // Use try_current() to avoid panicking if the runtime is shutting down.
        let Ok(handle) = tokio::runtime::Handle::try_current() else {
            tracing::debug!("no tokio runtime during IsolatedPinset drop; pins leaked");
            return;
        };

        // We can use get_mut() since we have &mut self in drop — no other references.
        let pins = self.pins.get_mut();
        let cids: Vec<Cid> = pins.keys().copied().collect();
        let pinner = Arc::clone(&self.pinner);

        handle.spawn(async move {
            for cid in cids {
                unpin_with_retry(&*pinner, &cid).await;
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

    pub(super) type MaterializationCleanupHook = Box<dyn FnMut(&Path) -> io::Result<()> + Send>;
    thread_local! {
        static MATERIALIZATION_CLEANUP_HOOK: std::cell::RefCell<Option<MaterializationCleanupHook>> =
            const { std::cell::RefCell::new(None) };
    }

    pub(super) fn take_materialization_cleanup_hook() -> Option<MaterializationCleanupHook> {
        MATERIALIZATION_CLEANUP_HOOK.with_borrow_mut(Option::take)
    }

    struct MaterializationCleanupHookGuard;

    impl MaterializationCleanupHookGuard {
        fn install(hook: impl FnMut(&Path) -> io::Result<()> + Send + 'static) -> Self {
            MATERIALIZATION_CLEANUP_HOOK.with_borrow_mut(|slot| {
                assert!(slot.is_none());
                *slot = Some(Box::new(hook));
            });
            Self
        }
    }

    impl Drop for MaterializationCleanupHookGuard {
        fn drop(&mut self) {
            MATERIALIZATION_CLEANUP_HOOK.with_borrow_mut(|slot| *slot = None);
        }
    }

    fn test_cid(n: u8) -> Cid {
        let mh = cid::multihash::Multihash::wrap(0x00, &[n]).unwrap();
        Cid::new_v1(0x55, mh)
    }

    struct MockPinner {
        pinned: Mutex<HashSet<Cid>>,
        data: HashMap<Cid, Vec<u8>>,
        pin_count: AtomicUsize,
        unpin_count: AtomicUsize,
        fail_unpin: AtomicBool,
        fail_fetch: AtomicBool,
        fail_pin: AtomicBool,
        fail_size: AtomicBool,
    }

    impl MockPinner {
        fn new(data: HashMap<Cid, Vec<u8>>) -> Self {
            Self {
                pinned: Mutex::new(HashSet::new()),
                data,
                pin_count: AtomicUsize::new(0),
                unpin_count: AtomicUsize::new(0),
                fail_unpin: AtomicBool::new(false),
                fail_fetch: AtomicBool::new(false),
                fail_pin: AtomicBool::new(false),
                fail_size: AtomicBool::new(false),
            }
        }
    }

    #[async_trait::async_trait]
    impl Pinner for MockPinner {
        async fn pin(&self, cid: &Cid) -> Result<()> {
            if self.fail_pin.load(Ordering::Relaxed) {
                anyhow::bail!("mock pin failure");
            }
            self.pin_count.fetch_add(1, Ordering::Relaxed);
            self.pinned.lock().await.insert(*cid);
            Ok(())
        }

        async fn unpin(&self, cid: &Cid) -> Result<()> {
            if self.fail_unpin.load(Ordering::Relaxed) {
                anyhow::bail!("mock unpin failure");
            }
            self.unpin_count.fetch_add(1, Ordering::Relaxed);
            self.pinned.lock().await.remove(cid);
            Ok(())
        }

        async fn fetch(&self, cid: &Cid) -> Result<Vec<u8>> {
            if self.fail_fetch.load(Ordering::Relaxed) {
                anyhow::bail!("mock fetch failure");
            }
            self.data
                .get(cid)
                .cloned()
                .ok_or_else(|| anyhow::anyhow!("CID not found"))
        }

        async fn size(&self, cid: &Cid) -> Result<u64> {
            if self.fail_size.load(Ordering::Relaxed) {
                anyhow::bail!("mock size failure");
            }
            self.data
                .get(cid)
                .map(|d| d.len() as u64)
                .ok_or_else(|| anyhow::anyhow!("CID not found"))
        }
    }

    fn mock_with_entries(entries: &[(u8, &[u8])]) -> Arc<MockPinner> {
        let data: HashMap<Cid, Vec<u8>> = entries
            .iter()
            .map(|(n, bytes)| (test_cid(*n), bytes.to_vec()))
            .collect();
        Arc::new(MockPinner::new(data))
    }

    #[tokio::test]
    async fn test_ensure_pins_on_miss() {
        let pinner = mock_with_entries(&[(1, b"hello")]);
        let cache = PinsetCache::new(pinner.clone(), 1024).unwrap();

        cache.ensure(&test_cid(1)).await.unwrap();
        assert_eq!(pinner.pin_count.load(Ordering::Relaxed), 1);
    }

    #[tokio::test]
    async fn test_ensure_is_idempotent() {
        let pinner = mock_with_entries(&[(1, b"data")]);
        let cache = PinsetCache::new(pinner.clone(), 1024).unwrap();

        cache.ensure(&test_cid(1)).await.unwrap();
        cache.ensure(&test_cid(1)).await.unwrap();
        // Should only pin once — second call hits cache.
        assert_eq!(pinner.pin_count.load(Ordering::Relaxed), 1);
    }

    #[tokio::test]
    async fn test_fetch_returns_bytes() {
        let pinner = mock_with_entries(&[(1, b"hello")]);
        let cache = PinsetCache::new(pinner, 1024).unwrap();

        cache.ensure(&test_cid(1)).await.unwrap();
        let bytes = cache.fetch(&test_cid(1)).await.unwrap();
        assert_eq!(bytes, b"hello");
    }

    #[tokio::test]
    async fn test_eviction_unpins() {
        let pinner = mock_with_entries(&[(1, &[0u8; 60]), (2, &[0u8; 60])]);
        let cache = PinsetCache::new(pinner.clone(), 80).unwrap(); // budget=80

        cache.ensure(&test_cid(1)).await.unwrap(); // weight 60
        cache.ensure(&test_cid(2)).await.unwrap(); // weight 60, total 120 > 80

        // Give the background unpin task time to run.
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(
            pinner.unpin_count.load(Ordering::Relaxed) > 0,
            "eviction should trigger unpin"
        );
    }

    #[tokio::test]
    async fn test_concurrent_dedup() {
        let pinner = mock_with_entries(&[(1, b"data")]);
        let cache = Arc::new(PinsetCache::new(pinner.clone(), 1024).unwrap());

        let mut handles = Vec::new();
        for _ in 0..10 {
            let c = Arc::clone(&cache);
            let cid = test_cid(1);
            handles.push(tokio::spawn(async move { c.ensure(&cid).await }));
        }

        for h in handles {
            h.await.unwrap().unwrap();
        }

        // Should only pin once despite 10 concurrent callers.
        assert_eq!(pinner.pin_count.load(Ordering::Relaxed), 1);
    }

    struct CancelledPinPinner {
        started: Notify,
        unpin_count: AtomicUsize,
    }

    struct CancellationRacePinner {
        first_pin_started: Notify,
        unpin_started: Notify,
        release_unpin: Notify,
        second_pin_started: Notify,
        pin_count: AtomicUsize,
        unpin_count: AtomicUsize,
        pinned: AtomicBool,
    }

    struct GatedPinPinner {
        pin_started: Notify,
        release_pin: Notify,
        pin_count: AtomicUsize,
    }

    struct CancelledFetchPinner {
        started: Notify,
        release: Notify,
        finished: AtomicBool,
    }

    struct ExistingWinnerPinner {
        destination: std::path::PathBuf,
    }

    struct MissingTemporaryPinner;

    #[async_trait::async_trait]
    impl Pinner for ExistingWinnerPinner {
        async fn pin(&self, _cid: &Cid) -> Result<()> {
            Ok(())
        }

        async fn unpin(&self, _cid: &Cid) -> Result<()> {
            Ok(())
        }

        async fn fetch(&self, _cid: &Cid) -> Result<Vec<u8>> {
            anyhow::bail!("streaming path expected")
        }

        async fn fetch_to_path(&self, _cid: &Cid, dst: &Path) -> Result<()> {
            std::fs::write(dst, b"loser")?;
            std::fs::write(&self.destination, b"winner")?;
            Ok(())
        }

        async fn size(&self, _cid: &Cid) -> Result<u64> {
            Ok(6)
        }
    }

    #[async_trait::async_trait]
    impl Pinner for MissingTemporaryPinner {
        async fn pin(&self, _cid: &Cid) -> Result<()> {
            Ok(())
        }

        async fn unpin(&self, _cid: &Cid) -> Result<()> {
            Ok(())
        }

        async fn fetch(&self, _cid: &Cid) -> Result<Vec<u8>> {
            anyhow::bail!("streaming path expected")
        }

        async fn fetch_to_path(&self, _cid: &Cid, dst: &Path) -> Result<()> {
            std::fs::write(dst, b"temporary")?;
            std::fs::remove_file(dst)?;
            Ok(())
        }

        async fn size(&self, _cid: &Cid) -> Result<u64> {
            Ok(9)
        }
    }

    #[async_trait::async_trait]
    impl Pinner for CancelledFetchPinner {
        async fn pin(&self, _cid: &Cid) -> Result<()> {
            Ok(())
        }

        async fn unpin(&self, _cid: &Cid) -> Result<()> {
            Ok(())
        }

        async fn fetch(&self, _cid: &Cid) -> Result<Vec<u8>> {
            anyhow::bail!("streaming path expected")
        }

        async fn fetch_to_path(&self, _cid: &Cid, dst: &Path) -> Result<()> {
            std::fs::write(dst, b"partial")?;
            self.started.notify_waiters();
            self.release.notified().await;
            self.finished.store(true, Ordering::SeqCst);
            anyhow::bail!("released cancelled fetch")
        }

        async fn size(&self, _cid: &Cid) -> Result<u64> {
            Ok(7)
        }
    }

    #[async_trait::async_trait]
    impl Pinner for CancelledPinPinner {
        async fn pin(&self, _cid: &Cid) -> Result<()> {
            self.started.notify_waiters();
            std::future::pending::<Result<()>>().await
        }

        async fn unpin(&self, _cid: &Cid) -> Result<()> {
            self.unpin_count.fetch_add(1, Ordering::Relaxed);
            Ok(())
        }

        async fn fetch(&self, _cid: &Cid) -> Result<Vec<u8>> {
            anyhow::bail!("not used")
        }

        async fn size(&self, _cid: &Cid) -> Result<u64> {
            Ok(1)
        }
    }

    #[async_trait::async_trait]
    impl Pinner for CancellationRacePinner {
        async fn pin(&self, _cid: &Cid) -> Result<()> {
            let call = self.pin_count.fetch_add(1, Ordering::SeqCst) + 1;
            if call == 1 {
                self.first_pin_started.notify_waiters();
                std::future::pending::<Result<()>>().await
            } else {
                self.pinned.store(true, Ordering::SeqCst);
                self.second_pin_started.notify_waiters();
                Ok(())
            }
        }

        async fn unpin(&self, _cid: &Cid) -> Result<()> {
            self.unpin_count.fetch_add(1, Ordering::SeqCst);
            self.unpin_started.notify_waiters();
            self.release_unpin.notified().await;
            self.pinned.store(false, Ordering::SeqCst);
            Ok(())
        }

        async fn fetch(&self, _cid: &Cid) -> Result<Vec<u8>> {
            anyhow::bail!("not used")
        }

        async fn size(&self, _cid: &Cid) -> Result<u64> {
            Ok(1)
        }
    }

    #[async_trait::async_trait]
    impl Pinner for GatedPinPinner {
        async fn pin(&self, _cid: &Cid) -> Result<()> {
            self.pin_count.fetch_add(1, Ordering::SeqCst);
            self.pin_started.notify_one();
            self.release_pin.notified().await;
            Ok(())
        }

        async fn unpin(&self, _cid: &Cid) -> Result<()> {
            Ok(())
        }

        async fn fetch(&self, _cid: &Cid) -> Result<Vec<u8>> {
            anyhow::bail!("not used")
        }

        async fn size(&self, _cid: &Cid) -> Result<u64> {
            Ok(1)
        }
    }

    #[tokio::test]
    async fn cancelled_ensure_releases_inflight_and_pin_state() {
        let pinner = Arc::new(CancelledPinPinner {
            started: Notify::new(),
            unpin_count: AtomicUsize::new(0),
        });
        let cache = Arc::new(PinsetCache::new(pinner.clone(), 1024).unwrap());
        let cid = test_cid(9);
        let started = pinner.started.notified();
        let task = {
            let cache = cache.clone();
            tokio::spawn(async move { cache.ensure(&cid).await })
        };
        started.await;
        task.abort();
        let _ = task.await;
        tokio::time::timeout(Duration::from_secs(1), async {
            loop {
                let inflight = cache.state.lock().await.inflight.contains_key(&cid);
                if !inflight && pinner.unpin_count.load(Ordering::Relaxed) == 1 {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("cancellation cleanup should finish");

        let state = cache.state.lock().await;
        assert!(
            !state.inflight.contains_key(&cid),
            "cancelling materialization must not strand an in-flight cache entry"
        );
        assert!(
            !state.arc.contains(&cid),
            "cancelling materialization must not publish cache ownership"
        );
        drop(state);
        assert_eq!(
            pinner.unpin_count.load(Ordering::Relaxed),
            1,
            "a cancelled cache insertion must release any partial backend pin"
        );
    }

    #[tokio::test]
    async fn cancellation_rollback_precedes_waiter_repin() {
        let pinner = Arc::new(CancellationRacePinner {
            first_pin_started: Notify::new(),
            unpin_started: Notify::new(),
            release_unpin: Notify::new(),
            second_pin_started: Notify::new(),
            pin_count: AtomicUsize::new(0),
            unpin_count: AtomicUsize::new(0),
            pinned: AtomicBool::new(false),
        });
        let cache = Arc::new(PinsetCache::new(pinner.clone(), 1024).unwrap());
        let cid = test_cid(11);

        let first_started = pinner.first_pin_started.notified();
        let first = {
            let cache = cache.clone();
            tokio::spawn(async move { cache.ensure(&cid).await })
        };
        first_started.await;
        let unpin_started = pinner.unpin_started.notified();
        first.abort();
        let _ = first.await;
        unpin_started.await;

        let second_pin_started = pinner.second_pin_started.notified();
        let second = {
            let cache = cache.clone();
            tokio::spawn(async move { cache.ensure(&cid).await })
        };
        assert!(
            tokio::time::timeout(Duration::from_millis(25), second_pin_started)
                .await
                .is_err(),
            "a waiter must remain blocked while cancellation rollback owns the in-flight slot"
        );
        assert_eq!(pinner.pin_count.load(Ordering::SeqCst), 1);

        pinner.release_unpin.notify_one();
        second.await.unwrap().unwrap();

        assert_eq!(pinner.unpin_count.load(Ordering::SeqCst), 1);
        assert_eq!(pinner.pin_count.load(Ordering::SeqCst), 2);
        assert!(
            pinner.pinned.load(Ordering::SeqCst),
            "late cleanup must not remove the waiter's fresh pin"
        );
    }

    #[tokio::test]
    async fn concurrent_waiter_observes_completed_cache_entry_without_repinning() {
        let pinner = Arc::new(GatedPinPinner {
            pin_started: Notify::new(),
            release_pin: Notify::new(),
            pin_count: AtomicUsize::new(0),
        });
        let cache = Arc::new(PinsetCache::new(pinner.clone(), 1024).unwrap());
        let cid = test_cid(12);

        let first_started = pinner.pin_started.notified();
        let first = {
            let cache = cache.clone();
            tokio::spawn(async move { cache.ensure(&cid).await })
        };
        first_started.await;

        let second = {
            let cache = cache.clone();
            tokio::spawn(async move { cache.ensure(&cid).await })
        };
        // Poll the waiter through registration before allowing the owner to
        // broadcast. This exercises the no-lost-wakeup ordering.
        tokio::task::yield_now().await;
        pinner.release_pin.notify_one();

        tokio::time::timeout(Duration::from_secs(1), async {
            first.await.unwrap().unwrap();
            second.await.unwrap().unwrap();
        })
        .await
        .expect("both concurrent ensures should complete");
        assert_eq!(
            pinner.pin_count.load(Ordering::SeqCst),
            1,
            "the waiter must observe the winning ARC entry instead of repinning"
        );
    }

    #[tokio::test]
    async fn cancelled_fetch_does_not_publish_or_leak_partial_staging_files() {
        let pinner = Arc::new(CancelledFetchPinner {
            started: Notify::new(),
            release: Notify::new(),
            finished: AtomicBool::new(false),
        });
        let cache = Arc::new(PinsetCache::new(pinner.clone(), 1024).unwrap());
        let cid = test_cid(10);
        cache.ensure(&cid).await.unwrap();
        let destination = cache.staging_dir().join(cid.to_string());
        let (filesystem_scope, filesystem_owner, filesystem_finalizer, completion) =
            crate::FsScope::new();
        let finalizer_task = tokio::spawn(filesystem_finalizer.run());
        let started = pinner.started.notified();
        let task = {
            let cache = cache.clone();
            let destination = destination.clone();
            let filesystem_scope = filesystem_scope.clone();
            tokio::spawn(async move {
                cache
                    .fetch_to_path_in(&filesystem_scope, &cid, &destination)
                    .await
            })
        };
        started.await;
        task.abort();
        let _ = task.await;
        pinner.release.notify_one();
        drop(filesystem_owner);
        completion.wait().await.unwrap();
        finalizer_task.await.unwrap().unwrap();
        assert!(
            pinner.finished.load(Ordering::SeqCst),
            "waiter cancellation must not cancel the admitted backend operation"
        );

        assert!(
            !destination.exists(),
            "cancelled bytes must never become the committed CID path"
        );
        let leaked: Vec<_> = std::fs::read_dir(cache.staging_dir())
            .unwrap()
            .filter_map(Result::ok)
            .collect();
        assert!(
            leaked.is_empty(),
            "cancellation must remove temporary staging artifacts: {leaked:?}"
        );
        let state = cache.state.lock().await;
        assert!(state.inflight.is_empty());
        assert!(
            state.arc.contains(&cid),
            "the successfully committed bounded cache pin remains an intentional cache effect"
        );
    }

    #[tokio::test]
    async fn commit_cleanup_failure_poisons_completion_and_preserves_winner() {
        let staging = tempfile::TempDir::new().unwrap();
        let destination = staging.path().join("winner");
        let pinner = Arc::new(ExistingWinnerPinner {
            destination: destination.clone(),
        });
        let cid = test_cid(13);
        let first_cleanup = Arc::new(AtomicBool::new(true));
        let first_cleanup_in_hook = Arc::clone(&first_cleanup);
        let _hook = MaterializationCleanupHookGuard::install(move |_| {
            if first_cleanup_in_hook.swap(false, Ordering::SeqCst) {
                Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    "injected cleanup failure",
                ))
            } else {
                Ok(())
            }
        });
        let (scope, owner, finalizer, completion) = crate::FsScope::new();
        let finalizer_task = tokio::spawn(finalizer.run());

        let result = fetch_to_path_owned(&scope, pinner, cid, None, destination.clone()).await;
        assert!(
            result.is_err(),
            "loser cleanup failure must reach its caller"
        );
        assert_eq!(std::fs::read(&destination).unwrap(), b"winner");
        drop(owner);

        let error = completion
            .wait()
            .await
            .expect_err("commit cleanup failure must poison filesystem completion");
        assert!(error.to_string().contains("cleanup failed"));
        assert!(finalizer_task.await.unwrap().is_err());
        assert_eq!(std::fs::read(&destination).unwrap(), b"winner");
        assert_eq!(std::fs::read_dir(staging.path()).unwrap().count(), 1);
    }

    #[tokio::test]
    async fn commit_error_with_successful_cleanup_does_not_poison_completion() {
        let staging = tempfile::TempDir::new().unwrap();
        let destination = staging.path().join("destination");
        let (scope, owner, finalizer, completion) = crate::FsScope::new();
        let finalizer_task = tokio::spawn(finalizer.run());

        let result = fetch_to_path_owned(
            &scope,
            Arc::new(MissingTemporaryPinner),
            test_cid(14),
            None,
            destination.clone(),
        )
        .await;
        assert!(result.is_err(), "missing staged data must fail publication");
        drop(owner);

        completion.wait().await.unwrap();
        finalizer_task.await.unwrap().unwrap();
        assert!(!destination.exists());
        assert_eq!(std::fs::read_dir(staging.path()).unwrap().count(), 0);
    }

    #[tokio::test]
    async fn test_isolated_cleanup() {
        let pinner = mock_with_entries(&[(1, b"a"), (2, b"b")]);
        let iso = IsolatedPinset::new(pinner.clone()).unwrap();

        iso.ensure(&test_cid(1)).await.unwrap();
        iso.ensure(&test_cid(2)).await.unwrap();
        assert_eq!(pinner.pin_count.load(Ordering::Relaxed), 2);

        drop(iso);

        // Give the spawned unpin tasks time to run.
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert_eq!(pinner.unpin_count.load(Ordering::Relaxed), 2);
    }

    #[tokio::test]
    async fn test_pin_failure_no_cache() {
        let pinner = mock_with_entries(&[(1, b"data")]);
        pinner.fail_pin.store(true, Ordering::Relaxed);
        let cache = PinsetCache::new(pinner, 1024).unwrap();

        let result = cache.ensure(&test_cid(1)).await;
        assert!(result.is_err(), "pin failure should propagate");

        // The entry should NOT be in the cache.
        let state = cache.state.lock().await;
        assert!(!state.arc.contains(&test_cid(1)));
    }

    #[tokio::test]
    async fn test_size_failure_before_pin() {
        let pinner = mock_with_entries(&[(1, b"data")]);
        pinner.fail_size.store(true, Ordering::Relaxed);
        let cache = PinsetCache::new(pinner.clone(), 1024).unwrap();

        let result = cache.ensure(&test_cid(1)).await;
        assert!(result.is_err());
        // Pin should never have been called.
        assert_eq!(pinner.pin_count.load(Ordering::Relaxed), 0);
    }

    #[tokio::test]
    async fn test_unpin_retry() {
        let pinner = mock_with_entries(&[(1, &[0u8; 60]), (2, &[0u8; 60])]);
        // Fail unpins initially.
        pinner.fail_unpin.store(true, Ordering::Relaxed);
        let cache = PinsetCache::new(pinner.clone(), 80).unwrap();

        cache.ensure(&test_cid(1)).await.unwrap();

        // Allow unpins after first attempt fails.
        let pinner2 = pinner.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(150)).await;
            pinner2.fail_unpin.store(false, Ordering::Relaxed);
        });

        cache.ensure(&test_cid(2)).await.unwrap(); // triggers eviction

        // Wait for retries to complete.
        tokio::time::sleep(Duration::from_millis(500)).await;
        assert!(
            pinner.unpin_count.load(Ordering::Relaxed) > 0,
            "retry should eventually succeed"
        );
    }

    #[tokio::test]
    async fn test_unpin_retry_exhausted() {
        let pinner = mock_with_entries(&[(1, &[0u8; 60]), (2, &[0u8; 60])]);
        pinner.fail_unpin.store(true, Ordering::Relaxed);
        let cache = PinsetCache::new(pinner.clone(), 80).unwrap();

        cache.ensure(&test_cid(1)).await.unwrap();
        cache.ensure(&test_cid(2)).await.unwrap(); // triggers eviction + unpin

        // Wait for all retries to exhaust.
        tokio::time::sleep(Duration::from_millis(1000)).await;

        // Unpin was attempted but always failed — count stays 0 since mock
        // bails before incrementing.
        assert_eq!(pinner.unpin_count.load(Ordering::Relaxed), 0);
    }

    // test_isolated_drop_no_runtime — tested via the Drop impl design
    // (Handle::try_current returns Err, no panic). Hard to test in-process
    // since we're already in a tokio runtime. Validated by code review.
}
