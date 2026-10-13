use std::collections::{HashMap, HashSet, VecDeque};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, Weak};
use std::time::Duration;

use crate::arc::ArcInner;
use crate::pinner::{PathWriteNotOwned, PathWriteSourcePanic, Pinner};
use crate::{
    CleanupRegistration, ForegroundTransaction, FsCompletion, FsFinalizer, FsOwner, FsScope,
    FsWorkError, HostResources, LogicalIoPermit,
};
use anyhow::{Context, Result};
use cid::Cid;
use futures::FutureExt;
use tempfile::TempDir;
use tokio::sync::{oneshot, Notify};

/// A cached pin entry. Tracks that a CID is pinned in the IPFS node
/// and its size (for weight-aware ARC eviction). Bytes are not held
/// in memory — the filesystem staging directory serves as the local cache.
pub struct PinEntry {
    /// Size in bytes of the pinned content.
    pub size: u64,
    cleanup: Option<CleanupRegistration>,
    staged: Option<StagedContentLease>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{HostFsOwner, HostLimits};
    use std::sync::atomic::{AtomicBool, AtomicUsize};

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
                assert!(slot.replace(Box::new(hook)).is_none());
            });
            Self
        }
    }

    impl Drop for MaterializationCleanupHookGuard {
        fn drop(&mut self) {
            MATERIALIZATION_CLEANUP_HOOK.with_borrow_mut(|slot| {
                slot.take();
            });
        }
    }

    fn cid(n: u8) -> Cid {
        let hash = cid::multihash::Multihash::wrap(0x00, &[n]).unwrap();
        Cid::new_v1(0x55, hash)
    }

    struct TestCache {
        cache: Arc<PinsetCache>,
        _host_owner: HostFsOwner,
    }

    async fn test_cache(pinner: Arc<dyn Pinner>, budget: usize) -> TestCache {
        test_cache_with_backend_limit(pinner, budget, 2).await
    }

    async fn test_cache_with_backend_limit(
        pinner: Arc<dyn Pinner>,
        budget: usize,
        backend_limit: usize,
    ) -> TestCache {
        let (host_owner, host) = HostFsOwner::start(
            tokio::runtime::Handle::current(),
            HostLimits::new(32, backend_limit, 8).unwrap(),
        )
        .unwrap();
        let cache = Arc::new(PinsetCache::new(pinner, budget, host).await.unwrap());
        TestCache {
            cache,
            _host_owner: host_owner,
        }
    }

    struct MockPinner {
        sizes: HashMap<Cid, u64>,
        pins: Mutex<Vec<Cid>>,
        unpins: Mutex<Vec<Cid>>,
        fail_size: AtomicBool,
        fail_pin: AtomicBool,
        fail_unpin: AtomicBool,
        gate_pin: AtomicBool,
        gate_first_unpin: AtomicBool,
        pin_started: Notify,
        release_pin: Notify,
        unpin_started: Notify,
        release_unpin: Notify,
    }

    impl MockPinner {
        fn new(entries: &[(u8, u64)]) -> Arc<Self> {
            Arc::new(Self {
                sizes: entries.iter().map(|(n, size)| (cid(*n), *size)).collect(),
                pins: Mutex::new(Vec::new()),
                unpins: Mutex::new(Vec::new()),
                fail_size: AtomicBool::new(false),
                fail_pin: AtomicBool::new(false),
                fail_unpin: AtomicBool::new(false),
                gate_pin: AtomicBool::new(false),
                gate_first_unpin: AtomicBool::new(false),
                pin_started: Notify::new(),
                release_pin: Notify::new(),
                unpin_started: Notify::new(),
                release_unpin: Notify::new(),
            })
        }
    }

    struct NotOwnedPathPinner;

    struct SourcePanicPathPinner;

    struct CancelledFetchPinner {
        started: Notify,
        release: Notify,
        finished: AtomicBool,
    }

    struct ExistingWinnerPinner {
        destination: std::path::PathBuf,
    }

    struct MissingTemporaryPinner;

    struct ConcurrentFetchPinner {
        calls: AtomicUsize,
        both_started: Notify,
        release: Notify,
    }

    #[async_trait::async_trait]
    impl Pinner for ConcurrentFetchPinner {
        async fn pin(&self, _cid: &Cid) -> Result<()> {
            Ok(())
        }

        async fn unpin(&self, _cid: &Cid) -> Result<()> {
            Ok(())
        }

        async fn fetch(&self, _cid: &Cid) -> Result<Vec<u8>> {
            anyhow::bail!("path fetch expected")
        }

        async fn fetch_to_path(&self, _cid: &Cid, dst: &Path) -> Result<()> {
            use std::io::Write as _;

            let released = self.release.notified();
            let call = self.calls.fetch_add(1, Ordering::SeqCst) + 1;
            let mut file = std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(dst)?;
            file.write_all(&[call as u8])?;
            if call == 2 {
                self.both_started.notify_one();
            }
            released.await;
            Ok(())
        }

        async fn size(&self, _cid: &Cid) -> Result<u64> {
            Ok(1)
        }
    }

    #[async_trait::async_trait]
    impl Pinner for NotOwnedPathPinner {
        async fn pin(&self, _cid: &Cid) -> Result<()> {
            Ok(())
        }

        async fn unpin(&self, _cid: &Cid) -> Result<()> {
            Ok(())
        }

        async fn fetch(&self, _cid: &Cid) -> Result<Vec<u8>> {
            anyhow::bail!("path fetch expected")
        }

        async fn fetch_to_path(&self, _cid: &Cid, _dst: &Path) -> Result<()> {
            Err(PathWriteNotOwned::new(anyhow::anyhow!("exclusive create rejected")).into())
        }

        async fn size(&self, _cid: &Cid) -> Result<u64> {
            Ok(1)
        }
    }

    #[async_trait::async_trait]
    impl Pinner for SourcePanicPathPinner {
        async fn pin(&self, _cid: &Cid) -> Result<()> {
            Ok(())
        }

        async fn unpin(&self, _cid: &Cid) -> Result<()> {
            Ok(())
        }

        async fn fetch(&self, _cid: &Cid) -> Result<Vec<u8>> {
            anyhow::bail!("path fetch expected")
        }

        async fn fetch_to_path(&self, _cid: &Cid, dst: &Path) -> Result<()> {
            std::fs::write(dst, b"partial")?;
            let payload: &(dyn std::any::Any + Send) = &"source panic";
            Err(PathWriteSourcePanic::new(payload).into())
        }

        async fn size(&self, _cid: &Cid) -> Result<u64> {
            Ok(1)
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
            anyhow::bail!("path fetch expected")
        }

        async fn fetch_to_path(&self, _cid: &Cid, dst: &Path) -> Result<()> {
            std::fs::write(dst, b"partial")?;
            self.started.notify_waiters();
            self.release.notified().await;
            self.finished.store(true, Ordering::SeqCst);
            anyhow::bail!("released cancelled fetch")
        }

        async fn size(&self, _cid: &Cid) -> Result<u64> {
            Ok(1)
        }
    }

    #[async_trait::async_trait]
    impl Pinner for ExistingWinnerPinner {
        async fn pin(&self, _cid: &Cid) -> Result<()> {
            Ok(())
        }

        async fn unpin(&self, _cid: &Cid) -> Result<()> {
            Ok(())
        }

        async fn fetch(&self, _cid: &Cid) -> Result<Vec<u8>> {
            anyhow::bail!("path fetch expected")
        }

        async fn fetch_to_path(&self, _cid: &Cid, dst: &Path) -> Result<()> {
            std::fs::write(dst, b"loser")?;
            std::fs::write(&self.destination, b"winner")?;
            Ok(())
        }

        async fn size(&self, _cid: &Cid) -> Result<u64> {
            Ok(1)
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
            anyhow::bail!("path fetch expected")
        }

        async fn fetch_to_path(&self, _cid: &Cid, dst: &Path) -> Result<()> {
            std::fs::write(dst, b"temporary")?;
            std::fs::remove_file(dst)?;
            Ok(())
        }

        async fn size(&self, _cid: &Cid) -> Result<u64> {
            Ok(1)
        }
    }

    #[async_trait::async_trait]
    impl Pinner for MockPinner {
        async fn pin(&self, cid: &Cid) -> Result<()> {
            self.pins
                .lock()
                .expect("mock pins lock poisoned")
                .push(*cid);
            if self.gate_pin.swap(false, Ordering::SeqCst) {
                self.pin_started.notify_waiters();
                self.release_pin.notified().await;
            }
            if self.fail_pin.load(Ordering::SeqCst) {
                anyhow::bail!("primary pin failure");
            }
            Ok(())
        }

        async fn unpin(&self, cid: &Cid) -> Result<()> {
            self.unpins
                .lock()
                .expect("mock unpins lock poisoned")
                .push(*cid);
            if self.gate_first_unpin.swap(false, Ordering::SeqCst) {
                self.unpin_started.notify_waiters();
                self.release_unpin.notified().await;
            }
            if self.fail_unpin.load(Ordering::SeqCst) {
                anyhow::bail!("rollback unpin failure");
            }
            Ok(())
        }

        async fn fetch(&self, cid: &Cid) -> Result<Vec<u8>> {
            Ok(vec![0; *self.sizes.get(cid).unwrap_or(&0) as usize])
        }

        async fn size(&self, cid: &Cid) -> Result<u64> {
            if self.fail_size.load(Ordering::SeqCst) {
                anyhow::bail!("size failure");
            }
            self.sizes
                .get(cid)
                .copied()
                .ok_or_else(|| anyhow::anyhow!("missing CID"))
        }
    }

    struct OwnerCancelPinner {
        pin_calls: AtomicUsize,
        unpin_calls: AtomicUsize,
        pinned: AtomicBool,
        pin_started: Notify,
        unpin_started: Notify,
        release_unpin: Notify,
    }

    #[async_trait::async_trait]
    impl Pinner for OwnerCancelPinner {
        async fn pin(&self, _cid: &Cid) -> Result<()> {
            let call = self.pin_calls.fetch_add(1, Ordering::SeqCst);
            self.pinned.store(true, Ordering::SeqCst);
            if call == 0 {
                self.pin_started.notify_waiters();
                std::future::pending::<Result<()>>().await
            } else {
                Ok(())
            }
        }

        async fn unpin(&self, _cid: &Cid) -> Result<()> {
            self.unpin_calls.fetch_add(1, Ordering::SeqCst);
            self.unpin_started.notify_waiters();
            self.release_unpin.notified().await;
            self.pinned.store(false, Ordering::SeqCst);
            Ok(())
        }

        async fn fetch(&self, _cid: &Cid) -> Result<Vec<u8>> {
            Ok(Vec::new())
        }

        async fn size(&self, _cid: &Cid) -> Result<u64> {
            Ok(1)
        }
    }

    struct FirstPinFailsPinner {
        pin_calls: AtomicUsize,
        unpin_calls: AtomicUsize,
        first_started: Notify,
        release_first: Notify,
    }

    #[async_trait::async_trait]
    impl Pinner for FirstPinFailsPinner {
        async fn pin(&self, _cid: &Cid) -> Result<()> {
            let call = self.pin_calls.fetch_add(1, Ordering::SeqCst);
            if call == 0 {
                self.first_started.notify_waiters();
                self.release_first.notified().await;
                anyhow::bail!("first elected owner failed")
            }
            Ok(())
        }

        async fn unpin(&self, _cid: &Cid) -> Result<()> {
            self.unpin_calls.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }

        async fn fetch(&self, _cid: &Cid) -> Result<Vec<u8>> {
            Ok(Vec::new())
        }

        async fn size(&self, _cid: &Cid) -> Result<u64> {
            Ok(1)
        }
    }

    struct PublishedCancelPinner {
        victim: Cid,
        unpins: Mutex<Vec<Cid>>,
        first_unpin_started: Notify,
        recovery_unpin_started: Notify,
        release_recovery: Notify,
    }

    #[async_trait::async_trait]
    impl Pinner for PublishedCancelPinner {
        async fn pin(&self, _cid: &Cid) -> Result<()> {
            Ok(())
        }

        async fn unpin(&self, cid: &Cid) -> Result<()> {
            let call = {
                let mut unpins = self.unpins.lock().expect("published unpins lock poisoned");
                unpins.push(*cid);
                unpins
                    .iter()
                    .filter(|candidate| **candidate == *cid)
                    .count()
            };
            if *cid == self.victim && call == 1 {
                self.first_unpin_started.notify_waiters();
                std::future::pending::<Result<()>>().await
            } else if *cid == self.victim && call == 2 {
                self.recovery_unpin_started.notify_waiters();
                self.release_recovery.notified().await;
                Ok(())
            } else {
                Ok(())
            }
        }

        async fn fetch(&self, _cid: &Cid) -> Result<Vec<u8>> {
            Ok(Vec::new())
        }

        async fn size(&self, _cid: &Cid) -> Result<u64> {
            Ok(1)
        }
    }

    struct OverlapPinner {
        first_victim: Cid,
        second_victim: Cid,
        first_unpin_started: Notify,
        second_unpin_started: Notify,
        release_first: Notify,
        release_second: Notify,
    }

    struct RetryingUnpinPinner {
        fail_attempts: usize,
        unpin_attempts: AtomicUsize,
    }

    #[async_trait::async_trait]
    impl Pinner for RetryingUnpinPinner {
        async fn pin(&self, _cid: &Cid) -> Result<()> {
            Ok(())
        }

        async fn unpin(&self, _cid: &Cid) -> Result<()> {
            let attempt = self.unpin_attempts.fetch_add(1, Ordering::SeqCst);
            if attempt < self.fail_attempts {
                anyhow::bail!("injected unpin failure")
            }
            Ok(())
        }

        async fn fetch(&self, _cid: &Cid) -> Result<Vec<u8>> {
            Ok(Vec::new())
        }

        async fn size(&self, _cid: &Cid) -> Result<u64> {
            Ok(1)
        }
    }

    #[async_trait::async_trait]
    impl Pinner for OverlapPinner {
        async fn pin(&self, _cid: &Cid) -> Result<()> {
            Ok(())
        }

        async fn unpin(&self, cid: &Cid) -> Result<()> {
            if *cid == self.first_victim {
                self.first_unpin_started.notify_waiters();
                self.release_first.notified().await;
            } else if *cid == self.second_victim {
                self.second_unpin_started.notify_waiters();
                self.release_second.notified().await;
            }
            Ok(())
        }

        async fn fetch(&self, _cid: &Cid) -> Result<Vec<u8>> {
            Ok(Vec::new())
        }

        async fn size(&self, _cid: &Cid) -> Result<u64> {
            Ok(1)
        }
    }

    #[tokio::test]
    async fn ensure_is_idempotent_and_sets_bloom_after_publication() {
        let pinner = MockPinner::new(&[(1, 4)]);
        let fixture = test_cache(pinner.clone(), 64).await;
        fixture.cache.ensure(&cid(1)).await.unwrap();
        fixture.cache.ensure(&cid(1)).await.unwrap();
        assert_eq!(pinner.pins.lock().unwrap().as_slice(), &[cid(1)]);
        assert!(fixture.cache.probably_cached(&cid(1)));
        assert_eq!(fixture.cache.fetch(&cid(1)).await.unwrap(), vec![0; 4]);
    }

    #[tokio::test]
    async fn same_cid_waiters_deduplicate_before_the_single_foreground_slot() {
        let pinner = MockPinner::new(&[(2, 4)]);
        pinner.gate_pin.store(true, Ordering::SeqCst);
        let fixture = test_cache(pinner.clone(), 64).await;
        let started = pinner.pin_started.notified();
        let mut waiters = Vec::new();
        for _ in 0..8 {
            let cache = Arc::clone(&fixture.cache);
            waiters.push(tokio::spawn(async move { cache.ensure(&cid(2)).await }));
        }
        started.await;
        tokio::task::yield_now().await;
        assert_eq!(pinner.pins.lock().unwrap().len(), 1);
        pinner.release_pin.notify_one();
        for waiter in waiters {
            waiter.await.unwrap().unwrap();
        }
        assert_eq!(pinner.pins.lock().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn cancelling_initiating_waiter_does_not_cancel_retained_owner() {
        let pinner = MockPinner::new(&[(3, 4)]);
        pinner.gate_pin.store(true, Ordering::SeqCst);
        let fixture = test_cache(pinner.clone(), 64).await;
        let started = pinner.pin_started.notified();
        let first = {
            let cache = Arc::clone(&fixture.cache);
            tokio::spawn(async move { cache.ensure(&cid(3)).await })
        };
        started.await;
        first.abort();
        let _ = first.await;
        pinner.release_pin.notify_one();
        tokio::time::timeout(Duration::from_secs(1), async {
            loop {
                if fixture.cache.probably_cached(&cid(3))
                    && fixture.cache.state.lock().unwrap().inflight.is_empty()
                {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        fixture.cache.ensure(&cid(3)).await.unwrap();
        assert_eq!(pinner.pins.lock().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn cancelling_non_owner_waiter_does_not_abort_or_rollback_owner() {
        let pinner = MockPinner::new(&[(12, 1)]);
        pinner.gate_pin.store(true, Ordering::SeqCst);
        let fixture = test_cache(pinner.clone(), 64).await;
        let pin_started = pinner.pin_started.notified();
        let owner = {
            let cache = Arc::clone(&fixture.cache);
            tokio::spawn(async move { cache.ensure(&cid(12)).await })
        };
        pin_started.await;

        let joined = fixture.cache.ensure_waiter_joined.notified();
        let waiter = {
            let cache = Arc::clone(&fixture.cache);
            tokio::spawn(async move { cache.ensure(&cid(12)).await })
        };
        joined.await;
        waiter.abort();
        let _ = waiter.await;
        pinner.release_pin.notify_one();

        owner.await.unwrap().unwrap();
        assert_eq!(pinner.pins.lock().unwrap().as_slice(), &[cid(12)]);
        assert!(pinner.unpins.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn true_owner_cancellation_rolls_back_under_retained_full_f_before_retry() {
        let pinner = Arc::new(OwnerCancelPinner {
            pin_calls: AtomicUsize::new(0),
            unpin_calls: AtomicUsize::new(0),
            pinned: AtomicBool::new(false),
            pin_started: Notify::new(),
            unpin_started: Notify::new(),
            release_unpin: Notify::new(),
        });
        let fixture = test_cache(pinner.clone(), 64).await;
        let cancel = Arc::new(Notify::new());
        *fixture
            .cache
            .cancel_next_owner
            .lock()
            .expect("ensure cancellation hook lock poisoned") = Some(Arc::clone(&cancel));

        let pin_started = pinner.pin_started.notified();
        let owner = {
            let cache = Arc::clone(&fixture.cache);
            tokio::spawn(async move { cache.ensure(&cid(13)).await })
        };
        pin_started.await;
        let unpin_started = pinner.unpin_started.notified();
        cancel.notify_one();
        unpin_started.await;

        assert!(fixture.cache.host.try_foreground().is_err());
        let retry = {
            let cache = Arc::clone(&fixture.cache);
            tokio::spawn(async move { cache.ensure(&cid(13)).await })
        };
        tokio::task::yield_now().await;
        assert_eq!(pinner.pin_calls.load(Ordering::SeqCst), 1);
        pinner.release_unpin.notify_one();

        assert!(owner
            .await
            .unwrap()
            .unwrap_err()
            .to_string()
            .contains("cancelled"));
        retry.await.unwrap().unwrap();
        assert_eq!(pinner.pin_calls.load(Ordering::SeqCst), 2);
        assert_eq!(pinner.unpin_calls.load(Ordering::SeqCst), 1);
        assert!(pinner.pinned.load(Ordering::SeqCst));
    }

    #[tokio::test]
    async fn failed_owner_wakes_waiters_and_one_retry_owner_publishes() {
        let pinner = Arc::new(FirstPinFailsPinner {
            pin_calls: AtomicUsize::new(0),
            unpin_calls: AtomicUsize::new(0),
            first_started: Notify::new(),
            release_first: Notify::new(),
        });
        let fixture = test_cache(pinner.clone(), 64).await;
        let first_started = pinner.first_started.notified();
        let first = {
            let cache = Arc::clone(&fixture.cache);
            tokio::spawn(async move { cache.ensure(&cid(14)).await })
        };
        first_started.await;
        let joined = fixture.cache.ensure_waiter_joined.notified();
        let mut waiters = Vec::new();
        for _ in 0..3 {
            let cache = Arc::clone(&fixture.cache);
            waiters.push(tokio::spawn(async move { cache.ensure(&cid(14)).await }));
        }
        joined.await;
        pinner.release_first.notify_one();

        assert!(first.await.unwrap().is_err());
        for waiter in waiters {
            waiter.await.unwrap().unwrap();
        }
        assert_eq!(pinner.pin_calls.load(Ordering::SeqCst), 2);
        assert_eq!(pinner.unpin_calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn published_owner_cancellation_finishes_victim_without_unpinning_published_cid() {
        let victim = cid(15);
        let published = cid(16);
        let pinner = Arc::new(PublishedCancelPinner {
            victim,
            unpins: Mutex::new(Vec::new()),
            first_unpin_started: Notify::new(),
            recovery_unpin_started: Notify::new(),
            release_recovery: Notify::new(),
        });
        let fixture = test_cache(pinner.clone(), 1).await;
        fixture.cache.ensure(&victim).await.unwrap();
        let cancel = Arc::new(Notify::new());
        *fixture
            .cache
            .cancel_next_owner
            .lock()
            .expect("ensure cancellation hook lock poisoned") = Some(Arc::clone(&cancel));

        let first_unpin = pinner.first_unpin_started.notified();
        let owner = {
            let cache = Arc::clone(&fixture.cache);
            tokio::spawn(async move { cache.ensure(&published).await })
        };
        first_unpin.await;
        let recovery_unpin = pinner.recovery_unpin_started.notified();
        cancel.notify_one();
        recovery_unpin.await;
        assert!(fixture.cache.host.try_foreground().is_err());
        assert_eq!(pinner.unpins.lock().unwrap().as_slice(), &[victim, victim]);
        pinner.release_recovery.notify_one();

        assert!(owner.await.unwrap().is_err());
        fixture.cache.ensure(&published).await.unwrap();
        assert!(fixture.cache.probably_cached(&published));
        assert!(!pinner.unpins.lock().unwrap().contains(&published));
    }

    #[tokio::test]
    async fn inflight_and_retiring_identities_for_same_cid_are_independent() {
        let first_victim = cid(17);
        let overlapping = cid(18);
        let replacement = cid(19);
        let pinner = Arc::new(OverlapPinner {
            first_victim,
            second_victim: overlapping,
            first_unpin_started: Notify::new(),
            second_unpin_started: Notify::new(),
            release_first: Notify::new(),
            release_second: Notify::new(),
        });
        let fixture = test_cache_with_backend_limit(pinner.clone(), 1, 3).await;
        fixture.cache.ensure(&first_victim).await.unwrap();

        let first_unpin = pinner.first_unpin_started.notified();
        let first_owner = {
            let cache = Arc::clone(&fixture.cache);
            tokio::spawn(async move { cache.ensure(&overlapping).await })
        };
        first_unpin.await;
        let second_unpin = pinner.second_unpin_started.notified();
        let second_owner = {
            let cache = Arc::clone(&fixture.cache);
            tokio::spawn(async move { cache.ensure(&replacement).await })
        };
        second_unpin.await;

        {
            let state = fixture.cache.state.lock().unwrap();
            assert!(state.inflight.contains_key(&overlapping));
            assert!(state.retiring.contains_key(&overlapping));
        }
        pinner.release_second.notify_one();
        second_owner.await.unwrap().unwrap();
        {
            let state = fixture.cache.state.lock().unwrap();
            assert!(state.inflight.contains_key(&overlapping));
            assert!(!state.retiring.contains_key(&overlapping));
        }
        pinner.release_first.notify_one();
        first_owner.await.unwrap().unwrap();
        assert!(!fixture
            .cache
            .state
            .lock()
            .unwrap()
            .inflight
            .contains_key(&overlapping));
    }

    #[tokio::test]
    async fn retirement_fence_blocks_repin_until_old_unpin_finishes() {
        let pinner = MockPinner::new(&[(4, 1), (5, 1)]);
        let fixture = test_cache(pinner.clone(), 1).await;
        fixture.cache.ensure(&cid(4)).await.unwrap();
        pinner.gate_first_unpin.store(true, Ordering::SeqCst);
        let unpin_started = pinner.unpin_started.notified();
        let insert_five = {
            let cache = Arc::clone(&fixture.cache);
            tokio::spawn(async move { cache.ensure(&cid(5)).await })
        };
        unpin_started.await;
        assert!(fixture.cache.host.try_foreground().is_err());
        let repin_four = {
            let cache = Arc::clone(&fixture.cache);
            tokio::spawn(async move { cache.ensure(&cid(4)).await })
        };
        tokio::task::yield_now().await;
        assert_eq!(pinner.pins.lock().unwrap().as_slice(), &[cid(4), cid(5)]);
        pinner.release_unpin.notify_one();
        insert_five.await.unwrap().unwrap();
        repin_four.await.unwrap().unwrap();
        assert_eq!(
            pinner.pins.lock().unwrap().as_slice(),
            &[cid(4), cid(5), cid(4)]
        );
    }

    #[tokio::test]
    async fn f_one_size_pin_publication_and_victim_unpin_are_one_serial_owner() {
        let pinner = MockPinner::new(&[(20, 1), (21, 1)]);
        let fixture = test_cache(pinner.clone(), 1).await;
        fixture.cache.ensure(&cid(20)).await.unwrap();
        fixture.cache.ensure(&cid(21)).await.unwrap();
        assert_eq!(pinner.pins.lock().unwrap().as_slice(), &[cid(20), cid(21)]);
        assert_eq!(pinner.unpins.lock().unwrap().as_slice(), &[cid(20)]);
    }

    #[tokio::test]
    async fn victim_unpin_retries_to_success_before_owner_completion() {
        let pinner = Arc::new(RetryingUnpinPinner {
            fail_attempts: 2,
            unpin_attempts: AtomicUsize::new(0),
        });
        let fixture = test_cache(pinner.clone(), 1).await;
        fixture.cache.ensure(&cid(22)).await.unwrap();
        fixture.cache.ensure(&cid(23)).await.unwrap();
        assert_eq!(pinner.unpin_attempts.load(Ordering::SeqCst), 3);
    }

    #[tokio::test]
    async fn exhausted_victim_unpin_retries_fail_owner_and_poison_cache_root() {
        let pinner = Arc::new(RetryingUnpinPinner {
            fail_attempts: usize::MAX,
            unpin_attempts: AtomicUsize::new(0),
        });
        let fixture = test_cache(pinner.clone(), 1).await;
        fixture.cache.ensure(&cid(24)).await.unwrap();
        let error = fixture.cache.ensure(&cid(25)).await.unwrap_err();
        assert!(error.to_string().contains("failed to unpin"));
        assert_eq!(pinner.unpin_attempts.load(Ordering::SeqCst), 3);
        assert!(fixture.cache.ensure(&cid(26)).await.is_err());
    }

    #[tokio::test]
    async fn size_failure_has_no_pin_or_unpin_effect() {
        let pinner = MockPinner::new(&[(6, 1)]);
        pinner.fail_size.store(true, Ordering::SeqCst);
        let fixture = test_cache(pinner.clone(), 64).await;
        assert!(fixture.cache.ensure(&cid(6)).await.is_err());
        assert!(pinner.pins.lock().unwrap().is_empty());
        assert!(pinner.unpins.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn path_not_owned_preserves_primary_and_skips_foreign_cleanup() {
        let fixture = test_cache(Arc::new(NotOwnedPathPinner), 64).await;
        let directory = TempDir::new().unwrap();
        let destination = directory.path().join("never-created");
        let error = fixture
            .cache
            .fetch_to_path(&cid(10), &destination)
            .await
            .unwrap_err();
        assert!(error.chain().any(|cause| cause.is::<PathWriteNotOwned>()));
        assert!(!destination.exists());
        assert_eq!(std::fs::read_dir(directory.path()).unwrap().count(), 0);
    }

    #[tokio::test]
    async fn path_source_panic_preserves_primary_rolls_back_and_poisons_root() {
        let fixture = test_cache(Arc::new(SourcePanicPathPinner), 64).await;
        let directory = TempDir::new().unwrap();
        let destination = directory.path().join("rolled-back");
        let error = fixture
            .cache
            .fetch_to_path(&cid(11), &destination)
            .await
            .unwrap_err();
        assert!(error
            .chain()
            .any(|cause| cause.is::<PathWriteSourcePanic>()));
        assert!(!destination.exists());
        assert_eq!(std::fs::read_dir(directory.path()).unwrap().count(), 0);

        let completion = fixture.cache.completion();
        let TestCache { cache, _host_owner } = fixture;
        drop(cache);
        assert!(completion.wait().await.is_err());
        drop(_host_owner);
    }

    #[tokio::test]
    async fn cancelled_path_waiter_finishes_backend_and_removes_partial_staging_file() {
        let pinner = Arc::new(CancelledFetchPinner {
            started: Notify::new(),
            release: Notify::new(),
            finished: AtomicBool::new(false),
        });
        let fixture = test_cache(pinner.clone(), 64).await;
        let directory = TempDir::new().unwrap();
        let destination = directory.path().join("cancelled");
        let started = pinner.started.notified();
        let waiter = {
            let cache = Arc::clone(&fixture.cache);
            let destination = destination.clone();
            tokio::spawn(async move { cache.fetch_to_path(&cid(27), &destination).await })
        };
        started.await;
        waiter.abort();
        let _ = waiter.await;
        pinner.release.notify_one();

        tokio::time::timeout(Duration::from_secs(1), async {
            while !pinner.finished.load(Ordering::SeqCst)
                || destination.exists()
                || std::fs::read_dir(directory.path()).unwrap().count() != 0
            {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("accepted path write and rollback must finish after waiter cancellation");
        assert_eq!(std::fs::read_dir(directory.path()).unwrap().count(), 0);
    }

    #[tokio::test]
    async fn loser_cleanup_failure_poisons_completion_and_preserves_winner() {
        let directory = TempDir::new().unwrap();
        let destination = directory.path().join("winner");
        let pinner = Arc::new(ExistingWinnerPinner {
            destination: destination.clone(),
        });
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
        let fixture = test_cache(pinner, 64).await;

        let result = fixture.cache.fetch_to_path(&cid(28), &destination).await;
        assert!(
            result.is_err(),
            "loser cleanup failure must reach its caller"
        );
        assert_eq!(std::fs::read(&destination).unwrap(), b"winner");
        let completion = fixture.cache.completion();
        let TestCache { cache, _host_owner } = fixture;
        drop(cache);
        let error = completion
            .wait()
            .await
            .expect_err("loser cleanup failure must poison cache completion");
        assert!(error.to_string().contains("cleanup failed"));
        assert_eq!(std::fs::read(&destination).unwrap(), b"winner");
        assert_eq!(std::fs::read_dir(directory.path()).unwrap().count(), 1);
        drop(_host_owner);
    }

    #[tokio::test]
    async fn commit_error_with_successful_cleanup_does_not_poison_completion() {
        let directory = TempDir::new().unwrap();
        let destination = directory.path().join("destination");
        let fixture = test_cache(Arc::new(MissingTemporaryPinner), 64).await;

        let result = fixture.cache.fetch_to_path(&cid(29), &destination).await;
        assert!(result.is_err(), "missing staged data must fail publication");
        let completion = fixture.cache.completion();
        let TestCache { cache, _host_owner } = fixture;
        drop(cache);
        completion.wait().await.unwrap();
        assert!(!destination.exists());
        assert_eq!(std::fs::read_dir(directory.path()).unwrap().count(), 0);
        drop(_host_owner);
    }

    #[tokio::test]
    async fn failed_pin_preserves_primary_and_failed_rollback_poisons_completion() {
        let pinner = MockPinner::new(&[(7, 1)]);
        pinner.fail_pin.store(true, Ordering::SeqCst);
        pinner.fail_unpin.store(true, Ordering::SeqCst);
        let fixture = test_cache(pinner, 64).await;
        let error = fixture.cache.ensure(&cid(7)).await.unwrap_err();
        assert!(error.to_string().contains("failed to pin CID"));
        assert!(!fixture.cache.state.lock().unwrap().arc.contains(&cid(7)));
        let completion = fixture.cache.completion();
        let TestCache { cache, _host_owner } = fixture;
        drop(cache);
        let completion_error = completion.wait().await.unwrap_err();
        assert!(completion_error.to_string().contains("cleanup failed"));
        drop(_host_owner);
    }

    #[tokio::test]
    async fn isolated_pins_are_unpinned_sequentially_by_root_cleanup() {
        let pinner = MockPinner::new(&[(8, 1), (9, 1)]);
        let (host_owner, host) = HostFsOwner::start(
            tokio::runtime::Handle::current(),
            HostLimits::new(8, 2, 2).unwrap(),
        )
        .unwrap();
        let (scope, owner, finalizer, completion) = host.try_filesystem_scope().unwrap();
        let logical = host.try_logical_io().unwrap();
        let setup = host.try_cleanup_transaction(logical, &[&scope]).unwrap();
        let isolated = IsolatedPinset::new(pinner.clone(), scope, &setup)
            .await
            .unwrap();
        drop(setup);
        isolated.ensure(&cid(8)).await.unwrap();
        isolated.ensure(&cid(9)).await.unwrap();
        drop(isolated);
        drop(owner);
        finalizer.run().await.unwrap();
        completion.wait().await.unwrap();
        assert_eq!(pinner.unpins.lock().unwrap().len(), 2);
        drop(host_owner);
    }

    #[tokio::test]
    async fn arc_eviction_reclaims_an_unowned_staged_generation() {
        let pinner = MockPinner::new(&[(40, 1), (41, 1), (42, 1), (43, 1), (44, 1)]);
        let fixture = test_cache(pinner, 1).await;
        let mut previous: Option<(PathBuf, FsCompletion)> = None;
        for n in 40..44 {
            fixture.cache.ensure(&cid(n)).await.unwrap();
            if let Some((path, completion)) = previous.take() {
                completion.wait().await.unwrap();
                assert!(!path.exists(), "eviction must reclaim the exact generation");
            }
            let staged = fixture.cache.staged_content(&cid(n)).await.unwrap();
            let path = staged.content_path("");
            std::fs::write(&path, b"old").unwrap();
            previous = Some((path, staged.completion()));
            drop(staged);
        }
        fixture.cache.ensure(&cid(44)).await.unwrap();
        let (path, completion) = previous.unwrap();
        completion.wait().await.unwrap();
        assert!(!path.exists(), "final eviction must reclaim its generation");
    }

    #[tokio::test]
    async fn a_live_content_lease_retains_the_whole_generation_after_eviction() {
        let pinner = MockPinner::new(&[(42, 1), (43, 1)]);
        let fixture = test_cache(pinner, 1).await;
        fixture.cache.ensure(&cid(42)).await.unwrap();
        let staged = fixture.cache.staged_content(&cid(42)).await.unwrap();
        let first = staged.content_path("a");
        let sibling = staged.content_path("b/nested");
        std::fs::create_dir_all(first.parent().unwrap()).unwrap();
        std::fs::create_dir_all(sibling.parent().unwrap()).unwrap();
        std::fs::write(&first, b"first").unwrap();
        std::fs::write(&sibling, b"sibling").unwrap();
        let lease = staged.lease();
        let completion = staged.completion();
        drop(staged);

        fixture.cache.ensure(&cid(43)).await.unwrap();
        assert_eq!(std::fs::read(&first).unwrap(), b"first");
        assert_eq!(std::fs::read(&sibling).unwrap(), b"sibling");

        drop(lease);
        completion.wait().await.unwrap();
        assert!(!first.exists());
        assert!(!sibling.exists());
    }

    #[tokio::test]
    async fn oversized_unresident_content_has_a_publisher_lease_then_reclaims() {
        let pinner = MockPinner::new(&[(44, 2)]);
        let fixture = test_cache(pinner, 1).await;
        fixture.cache.ensure(&cid(44)).await.unwrap();
        let staged = fixture.cache.staged_content(&cid(44)).await.unwrap();
        let path = fixture
            .cache
            .materialize_staged(&staged, None)
            .await
            .unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), vec![0; 2]);
        let completion = staged.completion();
        drop(staged);
        completion.wait().await.unwrap();
        assert!(!path.exists());
    }

    #[tokio::test]
    async fn accepted_publisher_survives_eviction_and_waiter_cancellation() {
        let pinner = Arc::new(CancelledFetchPinner {
            started: Notify::new(),
            release: Notify::new(),
            finished: AtomicBool::new(false),
        });
        let fixture = test_cache_with_backend_limit(pinner.clone(), 1, 3).await;
        fixture.cache.ensure(&cid(50)).await.unwrap();
        let staged = fixture.cache.staged_content(&cid(50)).await.unwrap();
        let generation_path = staged.lease.0.path.clone();
        let destination = staged.content_path("");
        let completion = staged.completion();
        let started = pinner.started.notified();
        let cache = Arc::clone(&fixture.cache);
        let mut waiter = tokio::spawn(async move { cache.materialize_staged(&staged, None).await });
        started.await;
        assert!(!destination.exists());

        fixture.cache.ensure(&cid(51)).await.unwrap();
        waiter.abort();
        let _ = (&mut waiter).await;
        assert!(fixture.cache.test_generation(&cid(50)).is_some());
        assert!(generation_path.exists());
        assert!(!destination.exists());

        pinner.release.notify_one();
        completion.wait().await.unwrap();
        assert!(pinner.finished.load(Ordering::SeqCst));
        assert!(!generation_path.exists());
    }

    #[tokio::test]
    async fn concurrent_same_cid_publishers_converge_without_loser_parts() {
        let pinner = Arc::new(ConcurrentFetchPinner {
            calls: AtomicUsize::new(0),
            both_started: Notify::new(),
            release: Notify::new(),
        });
        let fixture = test_cache_with_backend_limit(pinner.clone(), 1, 3).await;
        fixture.cache.ensure(&cid(52)).await.unwrap();
        let first = fixture.cache.staged_content(&cid(52)).await.unwrap();
        let second = fixture.cache.staged_content(&cid(52)).await.unwrap();
        let destination = first.content_path("");
        let parts = first.parts_path();
        assert_eq!(first.lease.0.path, second.lease.0.path);
        let both_started = pinner.both_started.notified();
        let first_cache = Arc::clone(&fixture.cache);
        let second_cache = Arc::clone(&fixture.cache);
        let first_task =
            tokio::spawn(async move { first_cache.materialize_staged(&first, None).await });
        let second_task =
            tokio::spawn(async move { second_cache.materialize_staged(&second, None).await });
        both_started.await;
        assert!(!destination.exists());
        pinner.release.notify_waiters();
        first_task.await.unwrap().unwrap();
        second_task.await.unwrap().unwrap();

        assert!(matches!(
            std::fs::read(&destination).unwrap().as_slice(),
            [1] | [2]
        ));
        assert_eq!(std::fs::read_dir(parts).unwrap().count(), 0);
    }

    #[tokio::test]
    async fn delayed_cleanup_cannot_delete_a_readmitted_same_cid_generation() {
        let pinner = MockPinner::new(&[(45, 1), (46, 1)]);
        let fixture = test_cache(pinner, 1).await;
        let (started_tx, started_rx) = std::sync::mpsc::channel();
        let gate = Arc::new((std::sync::Mutex::new(false), std::sync::Condvar::new()));
        let cleanup_gate = Arc::clone(&gate);
        *fixture.cache.generation_cleanup_hook.lock().unwrap() =
            Some(Arc::new(move |path, attempt| {
                let _ = path;
                if attempt == 1 {
                    let _ = started_tx.send(());
                    let (lock, ready) = &*cleanup_gate;
                    let mut released = lock.lock().unwrap();
                    while !*released {
                        released = ready.wait(released).unwrap();
                    }
                }
                Ok(())
            }));
        fixture.cache.ensure(&cid(45)).await.unwrap();
        let first = fixture.cache.staged_content(&cid(45)).await.unwrap();
        let first_path = first.lease.0.path.clone();
        let first_file = first.content_path("");
        std::fs::write(&first_file, b"first").unwrap();
        let first_completion = first.completion();
        drop(first);
        fixture.cache.ensure(&cid(46)).await.unwrap();
        started_rx
            .recv_timeout(Duration::from_secs(2))
            .expect("generation A cleanup did not start");

        fixture.cache.ensure(&cid(45)).await.unwrap();
        let second = fixture.cache.staged_content(&cid(45)).await.unwrap();
        let second_path = second.lease.0.path.clone();
        let second_file = second.content_path("");
        std::fs::write(&second_file, b"second").unwrap();
        assert_ne!(first_path, second_path);

        let (lock, ready) = &*gate;
        *lock.lock().unwrap() = true;
        ready.notify_all();
        first_completion.wait().await.unwrap();
        assert!(!first_path.exists());
        assert_eq!(std::fs::read(second_file).unwrap(), b"second");
    }

    #[tokio::test]
    async fn generation_cleanup_retries_three_times_and_reports_terminal_failure() {
        let pinner = MockPinner::new(&[(47, 2)]);
        let fixture = test_cache(pinner, 1).await;
        fixture.cache.ensure(&cid(47)).await.unwrap();
        let attempts = Arc::new(AtomicUsize::new(0));
        let hook_attempts = Arc::clone(&attempts);
        *fixture.cache.generation_cleanup_hook.lock().unwrap() = Some(Arc::new(move |_, _| {
            hook_attempts.fetch_add(1, Ordering::SeqCst);
            Err(io::Error::other("injected generation cleanup failure"))
        }));
        let staged = fixture.cache.staged_content(&cid(47)).await.unwrap();
        let generation_completion = staged.completion();
        let cache_completion = fixture.cache.completion();
        drop(staged);
        assert!(generation_completion.wait().await.is_err());
        assert_eq!(attempts.load(Ordering::SeqCst), 3);
        let poisoned = match fixture.cache.staged_content(&cid(47)).await {
            Err(error) => error,
            Ok(_) => panic!("cleanup-poisoned cache admitted another generation"),
        };
        assert!(matches!(
            poisoned.downcast_ref::<crate::AdmissionError>(),
            Some(error) if error.kind() == crate::AdmissionKind::CleanupPoisoned
        ));
        drop(fixture.cache);
        assert!(cache_completion.wait().await.is_err());
    }

    #[tokio::test]
    async fn generation_cleanup_succeeds_on_the_final_retry() {
        let pinner = MockPinner::new(&[(53, 2)]);
        let fixture = test_cache(pinner, 1).await;
        fixture.cache.ensure(&cid(53)).await.unwrap();
        let attempts = Arc::new(AtomicUsize::new(0));
        let hook_attempts = Arc::clone(&attempts);
        *fixture.cache.generation_cleanup_hook.lock().unwrap() = Some(Arc::new(move |_, _| {
            let attempt = hook_attempts.fetch_add(1, Ordering::SeqCst) + 1;
            if attempt < 3 {
                Err(io::Error::other("injected retryable cleanup failure"))
            } else {
                Ok(())
            }
        }));
        let staged = fixture.cache.staged_content(&cid(53)).await.unwrap();
        let completion = staged.completion();
        let path = staged.lease.0.path.clone();
        drop(staged);

        completion.wait().await.unwrap();
        assert_eq!(attempts.load(Ordering::SeqCst), 3);
        assert!(!path.exists());
    }

    #[tokio::test]
    async fn generation_cleanup_uses_the_dedicated_lane_when_foreground_is_full() {
        let pinner = MockPinner::new(&[(54, 2)]);
        let fixture = test_cache(pinner, 1).await;
        fixture.cache.ensure(&cid(54)).await.unwrap();
        let staged = fixture.cache.staged_content(&cid(54)).await.unwrap();
        let lease = staged.lease();
        let completion = staged.completion();
        let path = staged.lease.0.path.clone();
        drop(staged);
        let foreground = fixture.cache.host.try_foreground().unwrap();
        assert!(fixture.cache.host.try_foreground().is_err());

        drop(lease);
        tokio::time::timeout(Duration::from_secs(2), completion.wait())
            .await
            .expect("cleanup did not progress while foreground was saturated")
            .unwrap();
        assert!(!path.exists());
        drop(foreground);
    }

    #[tokio::test]
    async fn cache_completion_waits_for_the_final_generation_owner() {
        let pinner = MockPinner::new(&[(55, 2)]);
        let fixture = test_cache(pinner, 1).await;
        fixture.cache.ensure(&cid(55)).await.unwrap();
        let staged = fixture.cache.staged_content(&cid(55)).await.unwrap();
        let lease = staged.lease();
        let generation_completion = staged.completion();
        let cache_completion = fixture.cache.completion();
        let path = staged.lease.0.path.clone();
        drop(staged);
        drop(fixture.cache);

        assert!(cache_completion.wait().now_or_never().is_none());
        assert!(path.exists());
        drop(lease);
        generation_completion.wait().await.unwrap();
        cache_completion.wait().await.unwrap();
        assert!(!path.exists());
    }

    #[tokio::test]
    async fn cleanup_root_exhaustion_precedes_generation_filesystem_effects() {
        let pinner = MockPinner::new(&[(48, 2), (49, 2)]);
        let (host_owner, host) = HostFsOwner::start(
            tokio::runtime::Handle::current(),
            HostLimits::new(8, 2, 2).unwrap(),
        )
        .unwrap();
        let cache = Arc::new(PinsetCache::new(pinner, 1, host).await.unwrap());
        cache.ensure(&cid(48)).await.unwrap();
        let first = cache.staged_content(&cid(48)).await.unwrap();
        let first_lease = first.lease();
        drop(first);
        let before = std::fs::read_dir(cache.staging_dir()).unwrap().count();

        cache.ensure(&cid(49)).await.unwrap();
        let error = match cache.staged_content(&cid(49)).await {
            Err(error) => error,
            Ok(_) => panic!("cleanup-root exhaustion admitted a generation"),
        };
        assert!(matches!(
            error.downcast_ref::<crate::AdmissionError>(),
            Some(error) if error.kind() == crate::AdmissionKind::CleanupObligation
        ));
        assert_eq!(
            std::fs::read_dir(cache.staging_dir()).unwrap().count(),
            before
        );
        drop(first_lease);
        drop(cache);
        drop(host_owner);
    }
}

struct CacheState {
    arc: ArcInner<PinEntry>,
    inflight: HashMap<Cid, Arc<EnsureFlight>>,
    retiring: HashMap<Cid, Arc<RetireFlight>>,
    generations: HashMap<Cid, Weak<StagedContentGeneration>>,
    generation_inflight: HashMap<Cid, Arc<GenerationFlight>>,
}

struct GenerationFlight {
    changed: Notify,
    done: AtomicBool,
}

impl GenerationFlight {
    fn new() -> Self {
        Self {
            changed: Notify::new(),
            done: AtomicBool::new(false),
        }
    }
}

struct EnsureFlight {
    changed: Notify,
    done: AtomicBool,
}

impl EnsureFlight {
    fn new() -> Self {
        Self {
            changed: Notify::new(),
            done: AtomicBool::new(false),
        }
    }
}

struct RetireFlight {
    changed: Notify,
    done: AtomicBool,
}

impl RetireFlight {
    fn new() -> Self {
        Self {
            changed: Notify::new(),
            done: AtomicBool::new(false),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum EnsurePhase {
    BeforePin,
    PinUncertain,
    Published,
}

struct RetiringPin {
    cid: Cid,
    cleanup: Option<CleanupRegistration>,
}

type RetirementIdentities = Arc<Mutex<Vec<(Cid, Arc<RetireFlight>)>>>;

struct EnsureOwner {
    state: Arc<Mutex<CacheState>>,
    pinner: Arc<dyn Pinner>,
    scope: FsScope,
    cid: Cid,
    transaction: ForegroundTransaction,
    phase: EnsurePhase,
    own_pin_cleanup: Option<CleanupRegistration>,
    retiring: VecDeque<RetiringPin>,
    terminal_retirements: RetirementIdentities,
    outcome: Arc<Mutex<Option<Result<()>>>>,
    bloom: Arc<crate::bloom::AtomicBloom>,
    #[cfg(test)]
    cancel: Option<Arc<Notify>>,
}

impl EnsureOwner {
    async fn run(mut self) {
        enum Exit<T> {
            Finished(T),
            #[cfg(test)]
            Cancelled,
        }

        #[cfg(test)]
        let cancel = self.cancel.clone();
        let exit = {
            let operation = std::panic::AssertUnwindSafe(self.run_inner()).catch_unwind();
            tokio::pin!(operation);
            #[cfg(test)]
            {
                if let Some(cancel) = cancel {
                    tokio::select! {
                        result = &mut operation => Exit::Finished(result),
                        () = cancel.notified() => Exit::Cancelled,
                    }
                } else {
                    Exit::Finished(operation.await)
                }
            }
            #[cfg(not(test))]
            {
                Exit::Finished(operation.await)
            }
        };
        let result = match exit {
            Exit::Finished(result) => match result {
                Ok(result) => result,
                Err(_) => {
                    let error = anyhow::anyhow!("shared ensure owner panicked for {}", self.cid);
                    self.transaction
                        .record_failure(FsWorkError::new(error.to_string()));
                    if let Err(cleanup_error) = self.recover_after_interruption().await {
                        self.scope.record_cleanup_failure(&cleanup_error);
                    }
                    Err(error)
                }
            },
            #[cfg(test)]
            Exit::Cancelled => {
                let error = anyhow::anyhow!("shared ensure owner cancelled for {}", self.cid);
                if let Err(cleanup_error) = self.recover_after_interruption().await {
                    self.scope.record_cleanup_failure(&cleanup_error);
                }
                Err(error)
            }
        };
        *self.outcome.lock().expect("ensure outcome lock poisoned") = Some(result);
    }

    async fn run_inner(&mut self) -> Result<()> {
        let cleanup_pinner = Arc::clone(&self.pinner);
        let cleanup_cid = self.cid;
        self.own_pin_cleanup = Some(self.scope.own_async_cleanup_in(
            &self.transaction,
            crate::FilesystemOperation::CacheUnpin,
            move || async move {
                unpin_with_retry(&*cleanup_pinner, &cleanup_cid)
                    .await
                    .map_err(anyhow_to_io)
            },
        )?);

        let size = match self.pinner.size(&self.cid).await {
            Ok(size) => size,
            Err(error) => {
                if let Err(cleanup_error) = self.resolve_own_pin_cleanup() {
                    self.scope.record_cleanup_failure(&cleanup_error);
                }
                return Err(error).context("failed to get size for CID");
            }
        };

        self.phase = EnsurePhase::PinUncertain;
        if let Err(error) = self.pinner.pin(&self.cid).await {
            let primary = error.context("failed to pin CID");
            if let Err(cleanup_error) = self.rollback_uncertain_pin().await {
                self.scope.record_cleanup_failure(&cleanup_error);
            }
            return Err(primary);
        }

        let retired_staged = {
            let mut state = self.state.lock().expect("cache state lock poisoned");
            let mut retired_staged = Vec::new();
            let cleanup = self
                .own_pin_cleanup
                .take()
                .expect("published pin must retain cleanup ownership");
            let staged = state
                .generations
                .get(&self.cid)
                .and_then(Weak::upgrade)
                .map(StagedContentLease);
            let evicted = state.arc.put(
                self.cid,
                PinEntry {
                    size,
                    cleanup: Some(cleanup),
                    staged,
                },
            );
            for (cid, mut entry) in evicted {
                let flight = Arc::new(RetireFlight::new());
                let previous = state.retiring.insert(cid, Arc::clone(&flight));
                debug_assert!(previous.is_none(), "ARC returned an already-retiring CID");
                self.retiring.push_back(RetiringPin {
                    cid,
                    cleanup: entry.cleanup.take(),
                });
                retired_staged.extend(entry.staged.take());
                self.terminal_retirements
                    .lock()
                    .expect("terminal retirements lock poisoned")
                    .push((cid, flight));
            }
            self.phase = EnsurePhase::Published;
            retired_staged
        };
        drop(retired_staged);
        self.bloom.insert(&self.cid);

        self.finish_retirements().await
    }

    fn resolve_own_pin_cleanup(&mut self) -> Result<()> {
        if let Some(cleanup) = self.own_pin_cleanup.take() {
            cleanup.resolve()?;
        }
        Ok(())
    }

    async fn rollback_uncertain_pin(&mut self) -> Result<()> {
        let result = std::panic::AssertUnwindSafe(unpin_with_retry(&*self.pinner, &self.cid))
            .catch_unwind()
            .await
            .unwrap_or_else(|_| {
                Err(anyhow::anyhow!(
                    "unpin panicked while rolling back {}",
                    self.cid
                ))
            });
        if result.is_ok() {
            self.resolve_own_pin_cleanup()?;
        }
        self.phase = EnsurePhase::BeforePin;
        result
    }

    async fn finish_retirements(&mut self) -> Result<()> {
        while let Some(cid) = self.retiring.front().map(|retiring| retiring.cid) {
            let result = std::panic::AssertUnwindSafe(unpin_with_retry(&*self.pinner, &cid))
                .catch_unwind()
                .await
                .unwrap_or_else(|_| Err(anyhow::anyhow!("unpin panicked while retiring {cid}")));
            if result.is_ok() {
                let mut retiring = self
                    .retiring
                    .pop_front()
                    .expect("retiring front disappeared without an await");
                if let Some(cleanup) = retiring.cleanup.take() {
                    if let Err(error) = cleanup.resolve() {
                        self.scope.record_cleanup_failure(&error);
                        return Err(error.into());
                    }
                }
            } else if let Err(error) = &result {
                self.scope.record_cleanup_failure(error);
                return Err(anyhow::anyhow!(error.to_string()));
            }
        }
        Ok(())
    }

    async fn recover_after_interruption(&mut self) -> Result<()> {
        match self.phase {
            EnsurePhase::BeforePin => self.resolve_own_pin_cleanup(),
            EnsurePhase::PinUncertain => self.rollback_uncertain_pin().await,
            EnsurePhase::Published => self.finish_retirements().await,
        }
    }
}

fn remove_retiring(state: &Mutex<CacheState>, cid: Cid, flight: &Arc<RetireFlight>) {
    let removed = {
        let mut state = state.lock().expect("cache state lock poisoned");
        let owns = state
            .retiring
            .get(&cid)
            .is_some_and(|current| Arc::ptr_eq(current, flight));
        if owns {
            state.retiring.remove(&cid);
        }
        owns
    };
    if removed {
        flight.done.store(true, Ordering::Release);
        flight.changed.notify_waiters();
    }
}

fn finish_ensure(state: &Mutex<CacheState>, cid: Cid, flight: &Arc<EnsureFlight>) {
    let removed = {
        let mut state = state.lock().expect("cache state lock poisoned");
        let owns = state
            .inflight
            .get(&cid)
            .is_some_and(|current| Arc::ptr_eq(current, flight));
        if owns {
            state.inflight.remove(&cid);
        }
        owns
    };
    if removed {
        flight.done.store(true, Ordering::Release);
        flight.changed.notify_waiters();
    }
}

fn anyhow_to_io(error: anyhow::Error) -> io::Error {
    io::Error::other(error.to_string())
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
    host: Arc<HostResources>,
    scope: FsScope,
    _owner: FsOwner,
    _finalizer: FsFinalizer,
    completion: FsCompletion,
    staging_path: std::path::PathBuf,
    _staging_cleanup: CleanupRegistration,
    bloom: Arc<crate::bloom::AtomicBloom>,
    #[cfg(test)]
    cancel_next_owner: Mutex<Option<Arc<Notify>>>,
    #[cfg(test)]
    ensure_waiter_joined: Notify,
    #[cfg(test)]
    generation_cleanup_hook: Mutex<Option<GenerationCleanupHook>>,
}

#[cfg(test)]
type GenerationCleanupHook = Arc<dyn Fn(&Path, u32) -> io::Result<()> + Send + Sync>;

/// Maximum retry attempts for failed unpins.
const MAX_UNPIN_RETRIES: u32 = 3;
static MATERIALIZATION_SEQ: AtomicU64 = AtomicU64::new(1);
static GENERATION_SEQ: AtomicU64 = AtomicU64::new(1);

/// One claim on a physically unique staged CID generation.
///
/// Staged subpaths are retained at CID-generation granularity. A live
/// descriptor or native handle for any path in the generation retains the
/// entire staged generation until the final owner closes. This deliberately
/// avoids unsafe ancestor deletion and a path-level ownership graph.
#[derive(Clone)]
pub struct StagedContentLease(Arc<StagedContentGeneration>);

struct StagedContentGeneration {
    cid: Cid,
    path: PathBuf,
    scope: FsScope,
    _cleanup: CleanupRegistration,
    _owner: FsOwner,
    _finalizer: FsFinalizer,
    #[cfg(any(test, feature = "test-support"))]
    completion: FsCompletion,
}

/// An accepted materialization publisher and its admitted filesystem work.
pub struct StagedContent {
    lease: StagedContentLease,
    transaction: ForegroundTransaction,
}

impl StagedContent {
    pub fn lease(&self) -> StagedContentLease {
        self.lease.clone()
    }

    pub fn content_path(&self, subpath: &str) -> PathBuf {
        self.lease.content_path(subpath)
    }

    pub fn parts_path(&self) -> PathBuf {
        self.lease.0.path.join("parts")
    }

    pub fn transaction(&self) -> &ForegroundTransaction {
        &self.transaction
    }

    #[cfg(any(test, feature = "test-support"))]
    pub fn completion(&self) -> FsCompletion {
        self.lease.0.completion.clone()
    }
}

impl StagedContentLease {
    pub fn content_path(&self, subpath: &str) -> PathBuf {
        let content = self.0.path.join("content");
        if subpath.is_empty() {
            content.join("root")
        } else {
            content.join("subpaths").join(subpath)
        }
    }
}

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
        parts: Option<&Path>,
        #[cfg(test)] cleanup_hook: Option<tests::MaterializationCleanupHook>,
    ) -> Result<Self> {
        let parent = parts
            .or_else(|| destination.parent())
            .ok_or_else(|| anyhow::anyhow!("materialization destination has no parent"))?;
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
        use rustix::fs::{renameat_with, RenameFlags, CWD};

        match renameat_with(
            CWD,
            &self.temporary_path,
            CWD,
            destination,
            RenameFlags::NOREPLACE,
        ) {
            Ok(()) => {
                self.committed = true;
                Ok(MaterializationCommit::Published)
            }
            Err(rustix::io::Errno::EXIST) => Ok(MaterializationCommit::Existing),
            Err(error) => {
                Err(io::Error::from_raw_os_error(error.raw_os_error())).with_context(|| {
                    format!(
                        "failed to commit CAS materialization to {}",
                        destination.display()
                    )
                })
            }
        }
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

    fn disarm(&mut self) {
        self.committed = true;
    }
}

fn cleanup_materialization_slot(slot: &mut Option<MaterializationGuard>) -> io::Result<()> {
    let result = slot.as_mut().map_or(Ok(()), MaterializationGuard::cleanup);
    if result.is_ok() {
        slot.take();
    }
    result
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
    filesystem_scope: &FsScope,
    transaction: &ForegroundTransaction,
    pinner: &dyn Pinner,
    cid: &Cid,
    subpath: Option<&str>,
    destination: &Path,
    parts: Option<&Path>,
) -> Result<()> {
    let destination = destination.to_owned();
    let setup_destination = destination.clone();
    #[cfg(test)]
    let cleanup_hook = tests::take_materialization_cleanup_hook();
    let guard = Arc::new(Mutex::new(Some(MaterializationGuard::new(
        &setup_destination,
        parts,
        #[cfg(test)]
        cleanup_hook,
    )?)));
    let cleanup_guard = Arc::clone(&guard);
    let cleanup = filesystem_scope.own_cleanup_in(
        transaction,
        crate::FilesystemOperation::FileStage,
        move || {
            let mut guard = cleanup_guard
                .lock()
                .expect("materialization cleanup lock poisoned");
            cleanup_materialization_slot(&mut guard)
        },
    )?;
    let parent = destination
        .parent()
        .ok_or_else(|| anyhow::anyhow!("materialization destination has no parent"))?
        .to_owned();
    let parent_result = transaction
        .run_blocking(crate::FilesystemOperation::FileStage, move || {
            std::fs::create_dir_all(parent)
        })
        .await?
        .into_inner();
    if let Err(primary) = parent_result {
        if let Err(cleanup_error) = cleanup.resolve() {
            filesystem_scope.record_cleanup_failure(&cleanup_error);
        }
        return Err(primary.into());
    }
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
        if error.chain().any(|cause| cause.is::<PathWriteNotOwned>()) {
            guard
                .lock()
                .expect("materialization guard lock poisoned")
                .as_mut()
                .expect("materialization guard already consumed")
                .disarm();
            guard
                .lock()
                .expect("materialization guard lock poisoned")
                .take();
            if let Err(cleanup_error) = cleanup.resolve() {
                filesystem_scope.record_cleanup_failure(cleanup_error);
            }
            return Err(error);
        }
        if error
            .chain()
            .any(|cause| cause.is::<PathWriteSourcePanic>())
        {
            filesystem_scope.record_cleanup_failure(&error);
        }
        let cleanup_guard = Arc::clone(&guard);
        let cleanup_result = transaction
            .run_blocking(crate::FilesystemOperation::FileStage, move || {
                let mut guard = cleanup_guard
                    .lock()
                    .expect("materialization guard lock poisoned");
                cleanup_materialization_slot(&mut guard)
            })
            .await?
            .into_inner();
        if let Err(cleanup_error) = cleanup_result {
            filesystem_scope.record_cleanup_failure(cleanup_error);
        } else {
            if let Err(cleanup_error) = cleanup.resolve() {
                filesystem_scope.record_cleanup_failure(cleanup_error);
            }
        }
        return Err(error);
    }
    let commit_scope = filesystem_scope.clone();
    let commit_guard = Arc::clone(&guard);
    let commit_result = transaction
        .run_blocking(crate::FilesystemOperation::FileStage, move || {
            let mut guard = commit_guard
                .lock()
                .expect("materialization guard lock poisoned");
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
        .await?
        .into_inner();
    if let Err(primary) = commit_result {
        let cleanup_finished = guard
            .lock()
            .expect("materialization guard lock poisoned")
            .is_none();
        if cleanup_finished {
            if let Err(cleanup_error) = cleanup.resolve() {
                filesystem_scope.record_cleanup_failure(&cleanup_error);
            }
        }
        return Err(primary);
    }
    if let Err(error) = cleanup.resolve() {
        filesystem_scope.record_cleanup_failure(&error);
        return Err(error.into());
    }
    Ok(())
}

struct OwnedMaterialization {
    cid: Cid,
    subpath: Option<String>,
    destination: PathBuf,
    parts: Option<PathBuf>,
    publisher: Option<StagedContentLease>,
}

async fn fetch_to_path_owned(
    filesystem_scope: FsScope,
    transaction: ForegroundTransaction,
    pinner: Arc<dyn Pinner>,
    materialization: OwnedMaterialization,
) -> Result<()> {
    let OwnedMaterialization {
        cid,
        subpath,
        destination,
        parts,
        publisher,
    } = materialization;
    let operation_scope = filesystem_scope.clone();
    let operation_transaction = transaction.continuation();
    transaction
        .run_operation(crate::FilesystemOperation::FileStage, async move {
            let _publisher = publisher;
            fetch_to_path_atomically(
                &operation_scope,
                &operation_transaction,
                &*pinner,
                &cid,
                subpath.as_deref(),
                &destination,
                parts.as_deref(),
            )
            .await
        })
        .await?
        .into_inner()
}

struct GenerationFlightOwner {
    state: Arc<Mutex<CacheState>>,
    cid: Cid,
    flight: Arc<GenerationFlight>,
}

impl GenerationFlightOwner {
    fn finish(&mut self) {
        let removed = {
            let mut state = self.state.lock().expect("cache state lock poisoned");
            if state
                .generation_inflight
                .get(&self.cid)
                .is_some_and(|current| Arc::ptr_eq(current, &self.flight))
            {
                state.generation_inflight.remove(&self.cid);
                true
            } else {
                false
            }
        };
        if removed {
            self.flight.done.store(true, Ordering::Release);
            self.flight.changed.notify_waiters();
        }
    }
}

impl Drop for GenerationFlightOwner {
    fn drop(&mut self) {
        self.finish();
    }
}

const MAX_GENERATION_CLEANUP_ATTEMPTS: u32 = 3;

fn cleanup_staged_generation(
    path: PathBuf,
    owns_path: bool,
    parent_scope: FsScope,
    _parent_receipt: crate::FsWorkToken,
    #[cfg(test)] cleanup_hook: Option<GenerationCleanupHook>,
) -> io::Result<()> {
    if !owns_path {
        return Ok(());
    }
    for attempt in 1..=MAX_GENERATION_CLEANUP_ATTEMPTS {
        #[cfg(test)]
        let result = cleanup_hook
            .as_ref()
            .map_or(Ok(()), |hook| hook(&path, attempt))
            .and_then(|()| std::fs::remove_dir_all(&path));
        #[cfg(not(test))]
        let result = std::fs::remove_dir_all(&path);
        match result {
            Ok(()) => return Ok(()),
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
            Err(error) if attempt < MAX_GENERATION_CLEANUP_ATTEMPTS => {
                tracing::debug!(
                    path = %path.display(),
                    attempt,
                    %error,
                    "staged generation cleanup failed, retrying"
                );
                std::thread::sleep(Duration::from_millis(10 * u64::from(attempt)));
            }
            Err(error) => {
                let error = io::Error::new(
                    error.kind(),
                    format!(
                        "failed to remove staged generation {} after {} attempts: {error}",
                        path.display(),
                        MAX_GENERATION_CLEANUP_ATTEMPTS
                    ),
                );
                parent_scope.record_cleanup_failure(&error);
                return Err(error);
            }
        }
    }
    unreachable!("finite generation cleanup loop must return")
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
    pub async fn new(
        pinner: Arc<dyn Pinner>,
        budget: usize,
        host: Arc<HostResources>,
    ) -> Result<Self> {
        let (scope, owner, finalizer, completion) = host.try_filesystem_scope()?;
        let logical = host.try_logical_io()?;
        let transaction = host.try_cleanup_transaction(logical, &[&scope])?;
        let staging = transaction
            .run_blocking(crate::FilesystemOperation::ProcessSetup, TempDir::new)
            .await?
            .into_inner()
            .context("failed to create shared IPFS staging directory")?;
        let staging_path = staging.path().to_owned();
        let staging_cleanup = scope.own_cleanup_in(
            &transaction,
            crate::FilesystemOperation::ProcessCleanup,
            move || staging.close(),
        )?;
        drop(transaction);
        Ok(Self {
            state: Arc::new(Mutex::new(CacheState {
                arc: ArcInner::new(budget, Self::DEFAULT_GHOST_CAPACITY, pin_entry_weighter),
                inflight: HashMap::new(),
                retiring: HashMap::new(),
                generations: HashMap::new(),
                generation_inflight: HashMap::new(),
            })),
            pinner,
            host,
            scope,
            _owner: owner,
            _finalizer: finalizer,
            completion,
            staging_path,
            _staging_cleanup: staging_cleanup,
            bloom: Arc::new(crate::bloom::AtomicBloom::new(100_000, 0.00001)),
            #[cfg(test)]
            cancel_next_owner: Mutex::new(None),
            #[cfg(test)]
            ensure_waiter_joined: Notify::new(),
            #[cfg(test)]
            generation_cleanup_hook: Mutex::new(None),
        })
    }

    /// Path to the shared staging directory.
    pub fn staging_dir(&self) -> &Path {
        &self.staging_path
    }

    pub fn completion(&self) -> FsCompletion {
        self.completion.clone()
    }

    #[cfg(any(test, feature = "test-support"))]
    pub fn test_generation(&self, cid: &Cid) -> Option<(PathBuf, FsCompletion)> {
        let generation = {
            let state = self.state.lock().expect("cache state lock poisoned");
            state.generations.get(cid)?.upgrade()?
        };
        Some((generation.path.clone(), generation.completion.clone()))
    }

    /// Acquire one publisher claim on the current CID generation.
    pub async fn staged_content(&self, cid: &Cid) -> Result<StagedContent> {
        let logical = self.host.try_logical_io()?;
        self.staged_content_in(&self.scope, logical, cid).await
    }

    pub async fn staged_content_in(
        &self,
        origin: &FsScope,
        logical: LogicalIoPermit,
        cid: &Cid,
    ) -> Result<StagedContent> {
        let mut logical = Some(logical);
        loop {
            enum Decision {
                Existing(Arc<StagedContentGeneration>),
                Wait(Arc<GenerationFlight>),
                Owner(Arc<GenerationFlight>),
            }
            let decision = {
                let mut state = self.state.lock().expect("cache state lock poisoned");
                state
                    .generations
                    .retain(|_, generation| generation.strong_count() > 0);
                if let Some(generation) = state.generations.get(cid).and_then(Weak::upgrade) {
                    Decision::Existing(generation)
                } else if let Some(flight) = state.generation_inflight.get(cid) {
                    Decision::Wait(Arc::clone(flight))
                } else {
                    state.generations.remove(cid);
                    let flight = Arc::new(GenerationFlight::new());
                    state.generation_inflight.insert(*cid, Arc::clone(&flight));
                    Decision::Owner(flight)
                }
            };

            match decision {
                Decision::Existing(generation) => {
                    let transaction = self.host.try_cleanup_transaction(
                        logical.take().expect("logical permit consumed once"),
                        &[origin, &self.scope, &generation.scope],
                    )?;
                    let lease = StagedContentLease(generation);
                    self.retain_resident_generation(cid, &lease);
                    return Ok(StagedContent { lease, transaction });
                }
                Decision::Wait(flight) => {
                    let notified = flight.changed.notified();
                    tokio::pin!(notified);
                    notified.as_mut().enable();
                    if !flight.done.load(Ordering::Acquire) {
                        notified.await;
                    }
                }
                Decision::Owner(flight) => {
                    let mut terminal = GenerationFlightOwner {
                        state: Arc::clone(&self.state),
                        cid: *cid,
                        flight,
                    };
                    let staged = self
                        .create_staged_generation(
                            origin,
                            logical.take().expect("logical permit consumed once"),
                            *cid,
                        )
                        .await?;
                    let replaced = {
                        let mut state = self.state.lock().expect("cache state lock poisoned");
                        state
                            .generations
                            .insert(*cid, Arc::downgrade(&staged.lease.0));
                        if let Some(entry) = state.arc.value_mut(cid) {
                            entry.staged.replace(staged.lease.clone())
                        } else {
                            None
                        }
                    };
                    drop(replaced);
                    terminal.finish();
                    return Ok(staged);
                }
            }
        }
    }

    fn retain_resident_generation(&self, cid: &Cid, lease: &StagedContentLease) {
        let mut state = self.state.lock().expect("cache state lock poisoned");
        if let Some(entry) = state.arc.value_mut(cid) {
            if entry.staged.is_none() {
                entry.staged = Some(lease.clone());
            }
        }
    }

    async fn create_staged_generation(
        &self,
        origin: &FsScope,
        logical: LogicalIoPermit,
        cid: Cid,
    ) -> Result<StagedContent> {
        // K is reserved before a generation-private pathname can exist.
        let (scope, owner, finalizer, completion) = self.host.try_filesystem_scope()?;
        let parent_receipt = self.scope.try_cleanup_work()?;
        let transaction = self
            .host
            .try_cleanup_transaction(logical, &[origin, &self.scope, &scope])?;
        let cleanup_phase = transaction.try_cleanup_phase()?;
        let sequence = GENERATION_SEQ
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |current| {
                current.checked_add(1)
            })
            .map_err(|_| anyhow::anyhow!("staged CID generation identity exhausted"))?;
        let path = self.staging_path.join(format!(".ww-stage-{sequence}"));
        let cleanup_path = path.clone();
        let parent_scope = self.scope.clone();
        let owns_path = Arc::new(AtomicBool::new(false));
        let cleanup_owns_path = Arc::clone(&owns_path);
        #[cfg(test)]
        let cleanup_hook = self
            .generation_cleanup_hook
            .lock()
            .expect("generation cleanup hook lock poisoned")
            .clone();
        let cleanup = scope.own_cleanup_from_phase(
            cleanup_phase,
            crate::FilesystemOperation::GenerationCleanup,
            move || {
                cleanup_staged_generation(
                    cleanup_path,
                    cleanup_owns_path.load(Ordering::Acquire),
                    parent_scope,
                    parent_receipt,
                    #[cfg(test)]
                    cleanup_hook,
                )
            },
        )?;
        let generation = Arc::new(StagedContentGeneration {
            cid,
            path: path.clone(),
            scope,
            _cleanup: cleanup,
            _owner: owner,
            _finalizer: finalizer,
            #[cfg(any(test, feature = "test-support"))]
            completion: completion.clone(),
        });
        #[cfg(not(any(test, feature = "test-support")))]
        drop(completion);
        let setup_path = path;
        transaction
            .run_blocking(crate::FilesystemOperation::FileStage, move || {
                std::fs::create_dir(&setup_path)?;
                owns_path.store(true, Ordering::Release);
                std::fs::create_dir(setup_path.join("content"))?;
                std::fs::create_dir(setup_path.join("parts"))
            })
            .await?
            .into_inner()
            .context("failed to create staged CID generation")?;
        Ok(StagedContent {
            lease: StagedContentLease(generation),
            transaction,
        })
    }

    /// Materialize one path into an accepted generation and return its path.
    pub async fn materialize_staged(
        &self,
        staged: &StagedContent,
        subpath: Option<&str>,
    ) -> Result<PathBuf> {
        if !staged.transaction.owns_scope(&self.scope)
            || !staged.transaction.owns_scope(&staged.lease.0.scope)
        {
            return Err(crate::AdmissionError::new(crate::AdmissionKind::Closed).into());
        }
        let destination = staged.lease.content_path(subpath.unwrap_or_default());
        let check_path = destination.clone();
        let exists = staged
            .transaction
            .run_blocking(crate::FilesystemOperation::DescriptorOpen, move || {
                check_path.exists()
            })
            .await?
            .into_inner();
        if !exists {
            fetch_to_path_owned(
                self.scope.clone(),
                staged.transaction.continuation(),
                Arc::clone(&self.pinner),
                OwnedMaterialization {
                    cid: staged.lease.0.cid,
                    subpath: subpath.map(str::to_owned),
                    destination: destination.clone(),
                    parts: Some(staged.parts_path()),
                    publisher: Some(staged.lease()),
                },
            )
            .await?;
        }
        Ok(destination)
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
        let logical = self.host.try_logical_io()?;
        self.ensure_in(&self.scope, &logical, cid).await
    }

    pub async fn ensure_in(
        &self,
        origin: &FsScope,
        logical: &LogicalIoPermit,
        cid: &Cid,
    ) -> Result<()> {
        self.host.validate_logical(logical)?;
        loop {
            let origin_work = origin.try_cleanup_work()?;
            let mut work = vec![origin_work];
            if !work[0].belongs_to(&self.scope) {
                work.push(self.scope.try_cleanup_work()?);
            }

            enum Decision {
                WaitEnsure(Arc<EnsureFlight>),
                WaitRetire(Arc<RetireFlight>),
                Owner(Arc<EnsureFlight>, ForegroundTransaction),
                Hit,
            }
            let decision = {
                let mut state = self.state.lock().expect("cache state lock poisoned");
                if let Some(flight) = state.inflight.get(cid) {
                    Decision::WaitEnsure(Arc::clone(flight))
                } else if let Some(flight) = state.retiring.get(cid) {
                    Decision::WaitRetire(Arc::clone(flight))
                } else if state.arc.get(cid).is_some() {
                    Decision::Hit
                } else {
                    let producer = self.host.try_foreground()?;
                    let transaction = ForegroundTransaction::from_parts(producer, work)?;
                    let flight = Arc::new(EnsureFlight::new());
                    state.inflight.insert(*cid, Arc::clone(&flight));
                    Decision::Owner(flight, transaction)
                }
            };

            match decision {
                Decision::Hit => return Ok(()),
                Decision::WaitEnsure(flight) => {
                    #[cfg(test)]
                    self.ensure_waiter_joined.notify_waiters();
                    let notified = flight.changed.notified();
                    tokio::pin!(notified);
                    notified.as_mut().enable();
                    if !flight.done.load(Ordering::Acquire) {
                        notified.await;
                    }
                }
                Decision::WaitRetire(flight) => {
                    let notified = flight.changed.notified();
                    tokio::pin!(notified);
                    notified.as_mut().enable();
                    if !flight.done.load(Ordering::Acquire) {
                        notified.await;
                    }
                }
                Decision::Owner(flight, transaction) => {
                    let outcome = Arc::new(Mutex::new(None));
                    let terminal_retirements = Arc::new(Mutex::new(Vec::new()));
                    let owner = EnsureOwner {
                        state: Arc::clone(&self.state),
                        pinner: Arc::clone(&self.pinner),
                        scope: self.scope.clone(),
                        cid: *cid,
                        transaction: transaction.continuation(),
                        phase: EnsurePhase::BeforePin,
                        own_pin_cleanup: None,
                        retiring: VecDeque::new(),
                        terminal_retirements: Arc::clone(&terminal_retirements),
                        outcome: Arc::clone(&outcome),
                        bloom: Arc::clone(&self.bloom),
                        #[cfg(test)]
                        cancel: self
                            .cancel_next_owner
                            .lock()
                            .expect("ensure cancellation hook lock poisoned")
                            .take(),
                    };
                    let state = Arc::clone(&self.state);
                    let owner_cid = *cid;
                    let terminal_flight = Arc::clone(&flight);
                    let terminal_outcome = Arc::clone(&outcome);
                    let terminal_retirements = Arc::clone(&terminal_retirements);
                    let (result_tx, result_rx) = oneshot::channel();
                    transaction.spawn_retained_terminal(
                        crate::FilesystemOperation::CachePin,
                        owner.run(),
                        move |terminal| {
                            let result = terminal_outcome
                                .lock()
                                .expect("ensure outcome lock poisoned")
                                .take()
                                .unwrap_or_else(|| terminal.map_err(anyhow::Error::from));
                            let retirements = terminal_retirements
                                .lock()
                                .expect("terminal retirements lock poisoned")
                                .drain(..)
                                .collect::<Vec<_>>();
                            for (retiring_cid, retiring_flight) in retirements {
                                remove_retiring(&state, retiring_cid, &retiring_flight);
                            }
                            finish_ensure(&state, owner_cid, &terminal_flight);
                            let _ = result_tx.send(result);
                        },
                    );
                    return result_rx
                        .await
                        .context("shared ensure owner ended without a result")?;
                }
            }
        }
    }

    /// Fetch raw bytes for a CID from the IPFS node.
    ///
    /// The CID should already be pinned via a prior `ensure()` call.
    pub async fn fetch(&self, cid: &Cid) -> Result<Vec<u8>> {
        let logical = self.host.try_logical_io()?;
        self.fetch_in(&self.scope, logical, cid).await
    }

    pub async fn fetch_in(
        &self,
        origin: &FsScope,
        logical: LogicalIoPermit,
        cid: &Cid,
    ) -> Result<Vec<u8>> {
        let transaction = self
            .host
            .try_foreground_transaction(logical, &[origin, &self.scope])?;
        let pinner = Arc::clone(&self.pinner);
        let cid = *cid;
        transaction
            .run_operation(crate::FilesystemOperation::FileStage, async move {
                pinner.fetch(&cid).await.context("failed to fetch CID")
            })
            .await?
            .into_inner()
    }

    /// Fetch raw bytes for a subpath under a CID from the IPFS node.
    ///
    /// The CID should already be pinned via a prior `ensure()` call.
    pub async fn fetch_path(&self, cid: &Cid, subpath: &str) -> Result<Vec<u8>> {
        let logical = self.host.try_logical_io()?;
        self.fetch_path_in(&self.scope, logical, cid, subpath).await
    }

    pub async fn fetch_path_in(
        &self,
        origin: &FsScope,
        logical: LogicalIoPermit,
        cid: &Cid,
        subpath: &str,
    ) -> Result<Vec<u8>> {
        let transaction = self
            .host
            .try_foreground_transaction(logical, &[origin, &self.scope])?;
        let pinner = Arc::clone(&self.pinner);
        let cid = *cid;
        let subpath = subpath.to_owned();
        transaction
            .run_operation(crate::FilesystemOperation::FileStage, async move {
                pinner
                    .fetch_path(&cid, &subpath)
                    .await
                    .with_context(|| format!("failed to fetch CID subpath /ipfs/{cid}/{subpath}"))
            })
            .await?
            .into_inner()
    }

    /// Stream CID content directly to `dst`.
    ///
    /// The CID should already be pinned via a prior `ensure()` call.
    pub async fn fetch_to_path(&self, cid: &Cid, dst: &Path) -> Result<()> {
        let logical = self.host.try_logical_io()?;
        let transaction = self.try_materialization_transaction(&self.scope, logical)?;
        self.fetch_to_path_in(&self.scope, &transaction, cid, dst)
            .await
    }

    pub fn try_materialization_transaction(
        &self,
        origin: &FsScope,
        logical: LogicalIoPermit,
    ) -> Result<ForegroundTransaction> {
        Ok(self
            .host
            .try_cleanup_transaction(logical, &[origin, &self.scope])?)
    }

    pub async fn fetch_to_path_in(
        &self,
        filesystem_scope: &FsScope,
        transaction: &ForegroundTransaction,
        cid: &Cid,
        dst: &Path,
    ) -> Result<()> {
        if !transaction.owns_scope(filesystem_scope) || !transaction.owns_scope(&self.scope) {
            return Err(crate::AdmissionError::new(crate::AdmissionKind::Closed).into());
        }
        fetch_to_path_owned(
            filesystem_scope.clone(),
            transaction.continuation(),
            Arc::clone(&self.pinner),
            OwnedMaterialization {
                cid: *cid,
                subpath: None,
                destination: dst.to_owned(),
                parts: None,
                publisher: None,
            },
        )
        .await
    }

    /// Stream CID subpath content directly to `dst`.
    ///
    /// The CID should already be pinned via a prior `ensure()` call.
    pub async fn fetch_path_to_path(&self, cid: &Cid, subpath: &str, dst: &Path) -> Result<()> {
        let logical = self.host.try_logical_io()?;
        let transaction = self.try_materialization_transaction(&self.scope, logical)?;
        self.fetch_path_to_path_in(&self.scope, &transaction, cid, subpath, dst)
            .await
    }

    pub async fn fetch_path_to_path_in(
        &self,
        filesystem_scope: &FsScope,
        transaction: &ForegroundTransaction,
        cid: &Cid,
        subpath: &str,
        dst: &Path,
    ) -> Result<()> {
        if !transaction.owns_scope(filesystem_scope) || !transaction.owns_scope(&self.scope) {
            return Err(crate::AdmissionError::new(crate::AdmissionKind::Closed).into());
        }
        fetch_to_path_owned(
            filesystem_scope.clone(),
            transaction.continuation(),
            Arc::clone(&self.pinner),
            OwnedMaterialization {
                cid: *cid,
                subpath: Some(subpath.to_owned()),
                destination: dst.to_owned(),
                parts: None,
                publisher: None,
            },
        )
        .await
    }
}

/// Unpin a CID with up to MAX_UNPIN_RETRIES attempts and exponential backoff.
async fn unpin_with_retry(pinner: &dyn Pinner, cid: &Cid) -> Result<()> {
    let mut delay = Duration::from_millis(100);

    for attempt in 1..=MAX_UNPIN_RETRIES {
        match pinner.unpin(cid).await {
            Ok(()) => return Ok(()),
            Err(e) => {
                if attempt == MAX_UNPIN_RETRIES {
                    tracing::warn!(
                        %cid,
                        attempts = MAX_UNPIN_RETRIES,
                        error = %e,
                        "failed to unpin evicted CID after all retries; pin leaked"
                    );
                    return Err(e).with_context(|| {
                        format!("failed to unpin {cid} after {MAX_UNPIN_RETRIES} attempts")
                    });
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
    unreachable!("finite unpin retry loop must return")
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

    pub async fn ensure_in(
        &self,
        origin: &FsScope,
        logical: &LogicalIoPermit,
        cid: &Cid,
    ) -> Result<()> {
        match self {
            CacheMode::Shared(cache) => cache.ensure_in(origin, logical, cid).await,
            CacheMode::Isolated(pinset) => pinset.ensure_in(origin, logical, cid).await,
        }
    }

    /// Fetch raw bytes for a CID.
    pub async fn fetch(&self, cid: &Cid) -> Result<Vec<u8>> {
        match self {
            CacheMode::Shared(cache) => cache.fetch(cid).await,
            CacheMode::Isolated(pinset) => pinset.fetch(cid).await,
        }
    }

    pub async fn fetch_in(
        &self,
        origin: &FsScope,
        logical: LogicalIoPermit,
        cid: &Cid,
    ) -> Result<Vec<u8>> {
        match self {
            CacheMode::Shared(cache) => cache.fetch_in(origin, logical, cid).await,
            CacheMode::Isolated(pinset) => pinset.fetch_in(origin, logical, cid).await,
        }
    }

    /// Fetch raw bytes for a subpath under a CID.
    pub async fn fetch_path(&self, cid: &Cid, subpath: &str) -> Result<Vec<u8>> {
        match self {
            CacheMode::Shared(cache) => cache.fetch_path(cid, subpath).await,
            CacheMode::Isolated(pinset) => pinset.fetch_path(cid, subpath).await,
        }
    }

    pub async fn fetch_path_in(
        &self,
        origin: &FsScope,
        logical: LogicalIoPermit,
        cid: &Cid,
        subpath: &str,
    ) -> Result<Vec<u8>> {
        match self {
            CacheMode::Shared(cache) => cache.fetch_path_in(origin, logical, cid, subpath).await,
            CacheMode::Isolated(pinset) => {
                pinset.fetch_path_in(origin, logical, cid, subpath).await
            }
        }
    }

    /// Stream CID content directly to `dst`.
    pub async fn fetch_to_path(&self, cid: &Cid, dst: &Path) -> Result<()> {
        match self {
            CacheMode::Shared(cache) => cache.fetch_to_path(cid, dst).await,
            CacheMode::Isolated(pinset) => pinset.fetch_to_path(cid, dst).await,
        }
    }

    pub fn try_materialization_transaction(
        &self,
        origin: &FsScope,
        logical: LogicalIoPermit,
    ) -> Result<ForegroundTransaction> {
        match self {
            CacheMode::Shared(cache) => cache.try_materialization_transaction(origin, logical),
            CacheMode::Isolated(pinset) => pinset.try_materialization_transaction(origin, logical),
        }
    }

    pub async fn fetch_to_path_in(
        &self,
        filesystem_scope: &FsScope,
        transaction: &ForegroundTransaction,
        cid: &Cid,
        dst: &Path,
    ) -> Result<()> {
        match self {
            CacheMode::Shared(cache) => {
                cache
                    .fetch_to_path_in(filesystem_scope, transaction, cid, dst)
                    .await
            }
            CacheMode::Isolated(pinset) => {
                pinset
                    .fetch_to_path_in(filesystem_scope, transaction, cid, dst)
                    .await
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
        filesystem_scope: &FsScope,
        transaction: &ForegroundTransaction,
        cid: &Cid,
        subpath: &str,
        dst: &Path,
    ) -> Result<()> {
        match self {
            CacheMode::Shared(cache) => {
                cache
                    .fetch_path_to_path_in(filesystem_scope, transaction, cid, subpath, dst)
                    .await
            }
            CacheMode::Isolated(pinset) => {
                pinset
                    .fetch_path_to_path_in(filesystem_scope, transaction, cid, subpath, dst)
                    .await
            }
        }
    }
}

/// A per-process isolated pinset. Does not use ARC; simply tracks
/// what this process has pinned and unpins everything on drop.
pub struct IsolatedPinset {
    pinner: Arc<dyn Pinner>,
    host: Arc<HostResources>,
    scope: FsScope,
    state: Arc<Mutex<IsolatedState>>,
    staging_path: std::path::PathBuf,
    _staging_cleanup: CleanupRegistration,
    _pin_cleanup: CleanupRegistration,
}

struct IsolatedState {
    pins: HashSet<Cid>,
    inflight: HashMap<Cid, Arc<EnsureFlight>>,
}

impl IsolatedPinset {
    /// Create a new isolated pinset with its own staging directory.
    pub async fn new(
        pinner: Arc<dyn Pinner>,
        scope: FsScope,
        context: &ForegroundTransaction,
    ) -> Result<Self> {
        let host = Arc::clone(context.host());
        let phase = context.try_cleanup_phase()?;
        let staging = context
            .run_blocking(crate::FilesystemOperation::ProcessSetup, TempDir::new)
            .await?
            .into_inner()
            .context("failed to create isolated IPFS staging directory")?;
        let staging_path = staging.path().to_owned();
        let staging_cleanup = scope.own_cleanup_from_phase(
            phase,
            crate::FilesystemOperation::ProcessCleanup,
            move || staging.close(),
        )?;
        let state = Arc::new(Mutex::new(IsolatedState {
            pins: HashSet::new(),
            inflight: HashMap::new(),
        }));
        let cleanup_state = Arc::clone(&state);
        let cleanup_pinner = Arc::clone(&pinner);
        let pin_cleanup = scope.own_async_cleanup_in(
            context,
            crate::FilesystemOperation::CacheUnpin,
            move || async move {
                let pins = {
                    let mut state = cleanup_state.lock().expect("isolated pinset lock poisoned");
                    state.pins.drain().collect::<Vec<_>>()
                };
                let mut first_error = None;
                for cid in pins {
                    if let Err(error) = unpin_with_retry(&*cleanup_pinner, &cid).await {
                        if first_error.is_none() {
                            first_error = Some(anyhow_to_io(error));
                        }
                    }
                }
                match first_error {
                    Some(error) => Err(error),
                    None => Ok(()),
                }
            },
        )?;
        Ok(Self {
            pinner,
            host,
            scope,
            state,
            staging_path,
            _staging_cleanup: staging_cleanup,
            _pin_cleanup: pin_cleanup,
        })
    }

    /// Path to the per-process staging directory.
    pub fn staging_dir(&self) -> &Path {
        &self.staging_path
    }

    /// Ensure a CID is pinned. Idempotent within this pinset.
    ///
    /// Each IsolatedPinset pins independently in IPFS, giving cross-process
    /// refcounting: the IPFS node keeps content as long as any process holds
    /// a pin. Lock is held across the async pin to prevent double-pinning
    /// within a single process (contention is low for per-proc pinsets).
    pub async fn ensure(&self, cid: &Cid) -> Result<()> {
        let logical = self.host.try_logical_io()?;
        self.ensure_in(&self.scope, &logical, cid).await
    }

    pub async fn ensure_in(
        &self,
        origin: &FsScope,
        logical: &LogicalIoPermit,
        cid: &Cid,
    ) -> Result<()> {
        self.host.validate_logical(logical)?;
        loop {
            let origin_work = origin.try_cleanup_work()?;
            let mut work = vec![origin_work];
            if !work[0].belongs_to(&self.scope) {
                work.push(self.scope.try_cleanup_work()?);
            }
            let decision = {
                let mut state = self.state.lock().expect("isolated pinset lock poisoned");
                if let Some(flight) = state.inflight.get(cid) {
                    Err(Arc::clone(flight))
                } else if state.pins.contains(cid) {
                    return Ok(());
                } else {
                    let producer = self.host.try_foreground()?;
                    let transaction = ForegroundTransaction::from_parts(producer, work)?;
                    let flight = Arc::new(EnsureFlight::new());
                    state.inflight.insert(*cid, Arc::clone(&flight));
                    Ok((flight, transaction))
                }
            };
            match decision {
                Err(flight) => {
                    let notified = flight.changed.notified();
                    tokio::pin!(notified);
                    notified.as_mut().enable();
                    if !flight.done.load(Ordering::Acquire) {
                        notified.await;
                    }
                }
                Ok((flight, transaction)) => {
                    let pinner = Arc::clone(&self.pinner);
                    let state = Arc::clone(&self.state);
                    let scope = self.scope.clone();
                    let owner_cid = *cid;
                    let operation_tx = transaction.continuation();
                    let outcome = Arc::new(Mutex::new(None));
                    let operation_outcome = Arc::clone(&outcome);
                    let terminal_outcome = Arc::clone(&outcome);
                    let terminal_state = Arc::clone(&state);
                    let terminal_flight = Arc::clone(&flight);
                    let (result_tx, result_rx) = oneshot::channel();
                    transaction.spawn_retained_terminal(
                        crate::FilesystemOperation::CachePin,
                        async move {
                            let result = isolated_pin_owner(
                                &scope,
                                &operation_tx,
                                &state,
                                pinner,
                                owner_cid,
                            )
                            .await;
                            *operation_outcome
                                .lock()
                                .expect("isolated outcome lock poisoned") = Some(result);
                        },
                        move |terminal| {
                            let result = terminal_outcome
                                .lock()
                                .expect("isolated outcome lock poisoned")
                                .take()
                                .unwrap_or_else(|| terminal.map_err(anyhow::Error::from));
                            let removed =
                                {
                                    let mut state = terminal_state
                                        .lock()
                                        .expect("isolated pinset lock poisoned");
                                    if state.inflight.get(&owner_cid).is_some_and(|current| {
                                        Arc::ptr_eq(current, &terminal_flight)
                                    }) {
                                        state.inflight.remove(&owner_cid);
                                        true
                                    } else {
                                        false
                                    }
                                };
                            if removed {
                                terminal_flight.done.store(true, Ordering::Release);
                                terminal_flight.changed.notify_waiters();
                            }
                            let _ = result_tx.send(result);
                        },
                    );
                    return result_rx
                        .await
                        .context("isolated ensure owner ended without a result")?;
                }
            }
        }
    }

    /// Fetch raw bytes for a CID from the IPFS node.
    ///
    /// The CID should already be pinned via a prior `ensure()` call.
    pub async fn fetch(&self, cid: &Cid) -> Result<Vec<u8>> {
        let logical = self.host.try_logical_io()?;
        self.fetch_in(&self.scope, logical, cid).await
    }

    pub async fn fetch_in(
        &self,
        origin: &FsScope,
        logical: LogicalIoPermit,
        cid: &Cid,
    ) -> Result<Vec<u8>> {
        let transaction = self
            .host
            .try_foreground_transaction(logical, &[origin, &self.scope])?;
        let pinner = Arc::clone(&self.pinner);
        let cid = *cid;
        transaction
            .run_operation(crate::FilesystemOperation::FileStage, async move {
                pinner.fetch(&cid).await.context("failed to fetch CID")
            })
            .await?
            .into_inner()
    }

    /// Fetch raw bytes for a subpath under a CID from the IPFS node.
    ///
    /// The CID should already be pinned via a prior `ensure()` call.
    pub async fn fetch_path(&self, cid: &Cid, subpath: &str) -> Result<Vec<u8>> {
        let logical = self.host.try_logical_io()?;
        self.fetch_path_in(&self.scope, logical, cid, subpath).await
    }

    pub async fn fetch_path_in(
        &self,
        origin: &FsScope,
        logical: LogicalIoPermit,
        cid: &Cid,
        subpath: &str,
    ) -> Result<Vec<u8>> {
        let transaction = self
            .host
            .try_foreground_transaction(logical, &[origin, &self.scope])?;
        let pinner = Arc::clone(&self.pinner);
        let cid = *cid;
        let subpath = subpath.to_owned();
        transaction
            .run_operation(crate::FilesystemOperation::FileStage, async move {
                pinner
                    .fetch_path(&cid, &subpath)
                    .await
                    .with_context(|| format!("failed to fetch CID subpath /ipfs/{cid}/{subpath}"))
            })
            .await?
            .into_inner()
    }

    /// Stream CID content directly to `dst`.
    ///
    /// The CID should already be pinned via a prior `ensure()` call.
    pub async fn fetch_to_path(&self, cid: &Cid, dst: &Path) -> Result<()> {
        let logical = self.host.try_logical_io()?;
        let transaction = self.try_materialization_transaction(&self.scope, logical)?;
        self.fetch_to_path_in(&self.scope, &transaction, cid, dst)
            .await
    }

    pub fn try_materialization_transaction(
        &self,
        origin: &FsScope,
        logical: LogicalIoPermit,
    ) -> Result<ForegroundTransaction> {
        Ok(self
            .host
            .try_cleanup_transaction(logical, &[origin, &self.scope])?)
    }

    pub async fn fetch_to_path_in(
        &self,
        filesystem_scope: &FsScope,
        transaction: &ForegroundTransaction,
        cid: &Cid,
        dst: &Path,
    ) -> Result<()> {
        if !transaction.owns_scope(filesystem_scope) || !transaction.owns_scope(&self.scope) {
            return Err(crate::AdmissionError::new(crate::AdmissionKind::Closed).into());
        }
        fetch_to_path_owned(
            filesystem_scope.clone(),
            transaction.continuation(),
            Arc::clone(&self.pinner),
            OwnedMaterialization {
                cid: *cid,
                subpath: None,
                destination: dst.to_owned(),
                parts: None,
                publisher: None,
            },
        )
        .await
    }

    /// Stream CID subpath content directly to `dst`.
    ///
    /// The CID should already be pinned via a prior `ensure()` call.
    pub async fn fetch_path_to_path(&self, cid: &Cid, subpath: &str, dst: &Path) -> Result<()> {
        let logical = self.host.try_logical_io()?;
        let transaction = self.try_materialization_transaction(&self.scope, logical)?;
        self.fetch_path_to_path_in(&self.scope, &transaction, cid, subpath, dst)
            .await
    }

    pub async fn fetch_path_to_path_in(
        &self,
        filesystem_scope: &FsScope,
        transaction: &ForegroundTransaction,
        cid: &Cid,
        subpath: &str,
        dst: &Path,
    ) -> Result<()> {
        if !transaction.owns_scope(filesystem_scope) || !transaction.owns_scope(&self.scope) {
            return Err(crate::AdmissionError::new(crate::AdmissionKind::Closed).into());
        }
        fetch_to_path_owned(
            filesystem_scope.clone(),
            transaction.continuation(),
            Arc::clone(&self.pinner),
            OwnedMaterialization {
                cid: *cid,
                subpath: Some(subpath.to_owned()),
                destination: dst.to_owned(),
                parts: None,
                publisher: None,
            },
        )
        .await
    }
}

async fn isolated_pin_owner(
    scope: &FsScope,
    transaction: &ForegroundTransaction,
    state: &Mutex<IsolatedState>,
    pinner: Arc<dyn Pinner>,
    cid: Cid,
) -> Result<()> {
    let rollback_pinner = Arc::clone(&pinner);
    let rollback = scope.own_async_cleanup_in(
        transaction,
        crate::FilesystemOperation::CacheUnpin,
        move || async move {
            unpin_with_retry(&*rollback_pinner, &cid)
                .await
                .map_err(anyhow_to_io)
        },
    )?;
    let pin_result = std::panic::AssertUnwindSafe(pinner.pin(&cid))
        .catch_unwind()
        .await;
    let primary = match pin_result {
        Ok(Ok(())) => None,
        Ok(Err(error)) => Some(error.context("failed to pin CID")),
        Err(_) => {
            let error = anyhow::anyhow!("isolated pin owner panicked for {cid}");
            transaction.record_failure(FsWorkError::new(error.to_string()));
            Some(error)
        }
    };
    if let Some(primary) = primary {
        let rollback_result = std::panic::AssertUnwindSafe(unpin_with_retry(&*pinner, &cid))
            .catch_unwind()
            .await
            .unwrap_or_else(|_| {
                Err(anyhow::anyhow!(
                    "unpin panicked while rolling back isolated {cid}"
                ))
            });
        if let Err(cleanup_error) = rollback_result {
            scope.record_cleanup_failure(cleanup_error);
        } else if let Err(cleanup_error) = rollback.resolve() {
            scope.record_cleanup_failure(cleanup_error);
        }
        return Err(primary);
    }
    state
        .lock()
        .expect("isolated pinset lock poisoned")
        .pins
        .insert(cid);
    if let Err(error) = rollback.resolve() {
        scope.record_cleanup_failure(&error);
        return Err(error.into());
    }
    Ok(())
}
