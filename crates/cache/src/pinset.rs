use std::collections::{HashMap, HashSet, VecDeque};
use std::io;
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
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
}

struct CacheState {
    arc: ArcInner<PinEntry>,
    inflight: HashMap<Cid, Arc<EnsureFlight>>,
    retiring: HashMap<Cid, Arc<RetireFlight>>,
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

        {
            let mut state = self.state.lock().expect("cache state lock poisoned");
            let cleanup = self
                .own_pin_cleanup
                .take()
                .expect("published pin must retain cleanup ownership");
            let evicted = state.arc.put(
                self.cid,
                PinEntry {
                    size,
                    cleanup: Some(cleanup),
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
                self.terminal_retirements
                    .lock()
                    .expect("terminal retirements lock poisoned")
                    .push((cid, flight));
            }
            self.phase = EnsurePhase::Published;
        }
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
) -> Result<()> {
    let destination = destination.to_owned();
    let setup_destination = destination.clone();
    #[cfg(test)]
    let cleanup_hook = tests::take_materialization_cleanup_hook();
    let guard = Arc::new(Mutex::new(Some(MaterializationGuard::new(
        &setup_destination,
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

async fn fetch_to_path_owned(
    filesystem_scope: FsScope,
    transaction: ForegroundTransaction,
    pinner: Arc<dyn Pinner>,
    cid: Cid,
    subpath: Option<String>,
    destination: std::path::PathBuf,
) -> Result<()> {
    let operation_scope = filesystem_scope.clone();
    let operation_transaction = transaction.continuation();
    transaction
        .run_operation(crate::FilesystemOperation::FileStage, async move {
            fetch_to_path_atomically(
                &operation_scope,
                &operation_transaction,
                &*pinner,
                &cid,
                subpath.as_deref(),
                &destination,
            )
            .await
        })
        .await?
        .into_inner()
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
        })
    }

    /// Path to the shared staging directory.
    pub fn staging_dir(&self) -> &Path {
        &self.staging_path
    }

    pub fn completion(&self) -> FsCompletion {
        self.completion.clone()
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
            *cid,
            Some(subpath.to_owned()),
            dst.to_owned(),
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
            *cid,
            Some(subpath.to_owned()),
            dst.to_owned(),
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
