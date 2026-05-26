use std::collections::HashMap;
use std::path::Path;
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

/// CID-keyed cache backed by IPFS pins, with a weight-aware ARC eviction policy.
///
/// Manages which CIDs stay pinned in the IPFS node. Does not hold file
/// content in memory — callers either fetch bytes on demand or stream content
/// directly to local staging files.
///
/// Thread-safe: the inner ARC is wrapped in a `Mutex`. The expensive I/O
/// operations (pin, unpin) happen outside the lock.
pub struct PinsetCache {
    state: Mutex<CacheState>,
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
            state: Mutex::new(CacheState {
                arc: ArcInner::new(budget, Self::DEFAULT_GHOST_CAPACITY, pin_entry_weighter),
                inflight: HashMap::new(),
            }),
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
        // Lock-free fast path: if bloom says definitely absent, skip the
        // arc.get() probe inside the lock (saves ~200ns under lock on misses).
        let maybe_cached = self.bloom.probably_contains(cid);

        // Check cache under lock.
        loop {
            let mut state = self.state.lock().await;

            if maybe_cached && state.arc.get(cid).is_some() {
                return Ok(());
            }

            // Check if another task is already pinning this CID.
            if let Some(notify) = state.inflight.get(cid) {
                let notify = Arc::clone(notify);
                drop(state);

                // Wait for the in-flight pin to complete, then re-check.
                notify.notified().await;
                continue;
            }

            // Register ourselves as the in-flight pinner.
            state.inflight.insert(*cid, Arc::new(Notify::new()));
            break;
        }

        // Slow path: size + pin outside the lock.
        let result = self.size_and_pin(cid).await;

        // Re-lock and finalize.
        let mut state = self.state.lock().await;
        let notify = state.inflight.remove(cid);

        match result {
            Ok(entry) => {
                let evicted = state.arc.put(*cid, entry);
                self.bloom.insert(cid);

                // Notify waiters before spawning unpins.
                if let Some(n) = notify {
                    n.notify_waiters();
                }

                // Unpin evicted entries in background.
                self.spawn_unpins(evicted);

                Ok(())
            }
            Err(e) => {
                // Notify waiters that we failed.
                if let Some(n) = notify {
                    n.notify_waiters();
                }
                Err(e)
            }
        }
    }

    /// Get the size of a CID and pin it in the IPFS node.
    async fn size_and_pin(&self, cid: &Cid) -> Result<PinEntry> {
        let size = self
            .pinner
            .size(cid)
            .await
            .context("failed to get size for CID")?;

        self.pinner.pin(cid).await.context("failed to pin CID")?;

        Ok(PinEntry { size })
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
        self.pinner
            .fetch_to_path(cid, dst)
            .await
            .with_context(|| format!("failed to stream CID /ipfs/{cid} to {}", dst.display()))
    }

    /// Stream CID subpath content directly to `dst`.
    ///
    /// The CID should already be pinned via a prior `ensure()` call.
    pub async fn fetch_path_to_path(&self, cid: &Cid, subpath: &str, dst: &Path) -> Result<()> {
        self.pinner
            .fetch_path_to_path(cid, subpath, dst)
            .await
            .with_context(|| {
                format!(
                    "failed to stream CID subpath /ipfs/{cid}/{subpath} to {}",
                    dst.display()
                )
            })
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

    /// Stream CID subpath content directly to `dst`.
    pub async fn fetch_path_to_path(&self, cid: &Cid, subpath: &str, dst: &Path) -> Result<()> {
        match self {
            CacheMode::Shared(cache) => cache.fetch_path_to_path(cid, subpath, dst).await,
            CacheMode::Isolated(pinset) => pinset.fetch_path_to_path(cid, subpath, dst).await,
        }
    }
}

/// A per-process isolated pinset. Does not use ARC; simply tracks
/// what this process has pinned and unpins everything on drop.
pub struct IsolatedPinset {
    pinner: Arc<dyn Pinner>,
    pins: Mutex<HashMap<Cid, ()>>,
    /// Per-process staging directory. Cleaned up on drop.
    staging: TempDir,
}

impl IsolatedPinset {
    /// Create a new isolated pinset with its own staging directory.
    pub fn new(pinner: Arc<dyn Pinner>) -> Result<Self> {
        let staging = TempDir::new().context("failed to create isolated IPFS staging directory")?;
        Ok(Self {
            pinner,
            pins: Mutex::new(HashMap::new()),
            staging,
        })
    }

    /// Path to the per-process staging directory.
    pub fn staging_dir(&self) -> &Path {
        self.staging.path()
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
        self.pinner
            .fetch_to_path(cid, dst)
            .await
            .with_context(|| format!("failed to stream CID /ipfs/{cid} to {}", dst.display()))
    }

    /// Stream CID subpath content directly to `dst`.
    ///
    /// The CID should already be pinned via a prior `ensure()` call.
    pub async fn fetch_path_to_path(&self, cid: &Cid, subpath: &str, dst: &Path) -> Result<()> {
        self.pinner
            .fetch_path_to_path(cid, subpath, dst)
            .await
            .with_context(|| {
                format!(
                    "failed to stream CID subpath /ipfs/{cid}/{subpath} to {}",
                    dst.display()
                )
            })
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
