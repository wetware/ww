use super::*;
use sha2::{Digest, Sha256};
use std::sync::Mutex;

fn block(data: &[u8]) -> (Cid, Vec<u8>) {
    (
        Cid::new_v1(
            0x55,
            cid::multihash::Multihash::wrap(0x12, &Sha256::digest(data)).unwrap(),
        ),
        data.to_vec(),
    )
}

#[derive(Default)]
struct Backend {
    retained: Mutex<Vec<Cid>>,
    lose_ack: std::sync::atomic::AtomicBool,
}
#[async_trait]
impl Retention for Backend {
    fn domain(&self) -> &str {
        "fixture"
    }
    async fn import(&self, root: &Cid, _blocks: &[(Cid, Vec<u8>)]) -> Result<()> {
        self.retained.lock().unwrap().push(*root);
        ensure!(
            !self
                .lose_ack
                .swap(false, std::sync::atomic::Ordering::SeqCst),
            "lost retention acknowledgement"
        );
        Ok(())
    }
}
#[derive(Default)]
struct FixturePublisher {
    current: Mutex<Option<Publication>>,
    lose_ack: std::sync::atomic::AtomicBool,
}
#[async_trait]
impl Publisher for FixturePublisher {
    fn domain(&self) -> &str {
        "fixture"
    }
    async fn prepare(&self, intent: &PublicationIntent) -> Result<Vec<u8>> {
        Ok(serde_json::to_vec(intent)?)
    }
    async fn publish(&self, intent: &PublicationIntent, announcement: &[u8]) -> Result<()> {
        assert_eq!(announcement, serde_json::to_vec(intent)?);
        let mut current = self.current.lock().unwrap();
        if *current == Some(intent.next.clone()) {
            return Ok(());
        }
        ensure!(
            *current == intent.previous,
            "conflicting remote publication"
        );
        *current = Some(intent.next.clone());
        ensure!(
            !self
                .lose_ack
                .swap(false, std::sync::atomic::Ordering::SeqCst),
            "lost publication acknowledgement"
        );
        Ok(())
    }
}

#[tokio::test]
async fn ordering_and_recovery_of_both_lost_acknowledgements() {
    let dir = tempfile::tempdir().unwrap();
    let backend = Backend::default();
    let publisher = FixturePublisher::default();
    let (root, data) = block(b"release");
    let mut owner = Owner::open(dir.path(), "fixture").unwrap();
    let id = owner.begin(root, None).unwrap();
    assert!(owner.publish(id, &publisher).await.is_err());
    assert_eq!(*publisher.current.lock().unwrap(), None);
    backend
        .lose_ack
        .store(true, std::sync::atomic::Ordering::SeqCst);
    assert!(owner
        .retain_stage(id, root, &[(root, data)], &backend)
        .await
        .is_err());
    assert!(owner.finish_retention(id).is_err());
    drop(owner);
    let mut owner = Owner::open(dir.path(), "fixture").unwrap();
    owner.reconcile_retention(id, &backend).await.unwrap();
    owner.finish_retention(id).unwrap();
    publisher
        .lose_ack
        .store(true, std::sync::atomic::Ordering::SeqCst);
    assert!(owner.publish(id, &publisher).await.is_err());
    assert!(owner.retire(id).is_err());
    assert!(owner.begin(block(b"competitor").0, None).is_err());
    drop(owner);
    let mut owner = Owner::open(dir.path(), "fixture").unwrap();
    owner.publish(id, &publisher).await.unwrap();
    assert_eq!(
        owner.current().unwrap(),
        Some(Publication {
            generation: id,
            root
        })
    );
    owner.publish(id, &publisher).await.unwrap();
    assert!(owner.prune(id).is_err());
}

#[test]
fn directory_lock_and_domain_binding() {
    let dir = tempfile::tempdir().unwrap();
    let owner = Owner::open(dir.path(), "fixture").unwrap();
    assert!(Owner::open(dir.path(), "fixture").is_err());
    drop(owner);
    assert!(Owner::open(dir.path(), "another-owner").is_err());
    Owner::open(dir.path(), "fixture").unwrap();
}

async fn retained(owner: &mut Owner, backend: &Backend, data: &[u8]) -> u64 {
    let (root, bytes) = block(data);
    let id = owner.begin(root, owner.current().unwrap()).unwrap();
    owner
        .retain_stage(id, root, &[(root, bytes)], backend)
        .await
        .unwrap();
    owner.finish_retention(id).unwrap();
    id
}

#[tokio::test]
async fn stale_competing_and_retired_publications_cannot_advance() {
    let dir = tempfile::tempdir().unwrap();
    let mut owner = Owner::open(dir.path(), "fixture").unwrap();
    let backend = Backend::default();
    let publisher = FixturePublisher::default();
    let (first, _) = block(b"first");
    let id = owner.begin(first, None).unwrap();
    assert_eq!(owner.begin(first, None).unwrap(), id);
    assert!(owner.begin(block(b"competitor").0, None).is_err());
    let id = retained(&mut owner, &backend, b"first").await;
    owner.publish(id, &publisher).await.unwrap();
    assert!(owner.retire(id).is_err());
    assert!(owner.begin(block(b"second").0, None).is_err());
    let second = retained(&mut owner, &backend, b"second").await;
    owner.publish(second, &publisher).await.unwrap();
    assert_eq!(owner.releases().unwrap()[0].state, ReleaseState::Retired);
    assert!(owner.publish(id, &publisher).await.is_err());
    assert_eq!(
        publisher
            .current
            .lock()
            .unwrap()
            .as_ref()
            .unwrap()
            .generation,
        second
    );
    assert_eq!(backend.retained.lock().unwrap().len(), 2);
}

#[tokio::test]
async fn invalid_or_incomplete_stage_has_no_backend_effect() {
    let dir = tempfile::tempdir().unwrap();
    let mut owner = Owner::open(dir.path(), "fixture").unwrap();
    let backend = Backend::default();
    let (root, _) = block(b"expected");
    let id = owner.begin(root, None).unwrap();
    assert!(owner.retain_stage(id, root, &[], &backend).await.is_err());
    assert!(owner
        .retain_stage(id, root, &[(root, b"corrupted".to_vec())], &backend)
        .await
        .is_err());
    assert!(backend.retained.lock().unwrap().is_empty());
    assert!(owner.releases().unwrap()[0].pending_stage.is_none());
}

#[tokio::test]
async fn cancellation_keeps_pending_retention_and_publication_intents() {
    struct Waiting;
    #[async_trait]
    impl Retention for Waiting {
        fn domain(&self) -> &str {
            "fixture"
        }
        async fn import(&self, _: &Cid, _: &[(Cid, Vec<u8>)]) -> Result<()> {
            std::future::pending().await
        }
    }
    #[async_trait]
    impl Publisher for Waiting {
        fn domain(&self) -> &str {
            "fixture"
        }
        async fn prepare(&self, intent: &PublicationIntent) -> Result<Vec<u8>> {
            Ok(serde_json::to_vec(intent)?)
        }
        async fn publish(&self, _: &PublicationIntent, _: &[u8]) -> Result<()> {
            std::future::pending().await
        }
    }
    let dir = tempfile::tempdir().unwrap();
    let mut owner = Owner::open(dir.path(), "fixture").unwrap();
    let (root, data) = block(b"cancel");
    let id = owner.begin(root, None).unwrap();
    assert!(tokio::time::timeout(
        Duration::from_millis(5),
        owner.retain_stage(id, root, &[(root, data)], &Waiting)
    )
    .await
    .is_err());
    assert_eq!(owner.releases().unwrap()[0].pending_stage, Some(root));
    assert!(owner.finish_retention(id).is_err());
    // A retry resupplies immutable content if acquisition/import did not finish.
    owner
        .retain_stage(id, root, &[(root, b"cancel".to_vec())], &Backend::default())
        .await
        .unwrap();
    owner.finish_retention(id).unwrap();
    assert!(
        tokio::time::timeout(Duration::from_millis(5), owner.publish(id, &Waiting))
            .await
            .is_err()
    );
    assert!(owner.releases().unwrap()[0].publishing);
    assert!(owner.retire(id).is_err());
    drop(owner);
    let mut owner = Owner::open(dir.path(), "fixture").unwrap();
    owner
        .publish(id, &FixturePublisher::default())
        .await
        .unwrap();
}

#[tokio::test]
async fn authentication_refusal_has_no_commit_or_delivery() {
    struct Reject;
    #[async_trait]
    impl Publisher for Reject {
        fn domain(&self) -> &str {
            "fixture"
        }
        async fn prepare(&self, _: &PublicationIntent) -> Result<Vec<u8>> {
            anyhow::bail!("unauthenticated release")
        }
        async fn publish(&self, _: &PublicationIntent, _: &[u8]) -> Result<()> {
            panic!("unauthenticated release delivered")
        }
    }
    let dir = tempfile::tempdir().unwrap();
    let mut owner = Owner::open(dir.path(), "fixture").unwrap();
    let id = retained(&mut owner, &Backend::default(), b"rejected").await;
    assert!(owner.publish(id, &Reject).await.is_err());
    assert_eq!(owner.current().unwrap(), None);
    assert_eq!(owner.releases().unwrap()[0].state, ReleaseState::Retained);
    owner.retire(id).unwrap();
    owner.begin(block(b"next").0, None).unwrap();
}

#[tokio::test]
async fn shared_descendants_and_duplicate_roots_do_not_create_independent_claims() {
    let dir = tempfile::tempdir().unwrap();
    let mut owner = Owner::open(dir.path(), "fixture").unwrap();
    let backend = Backend::default();
    let publisher = FixturePublisher::default();
    let (shared, bytes) = block(b"shared");
    let first = retained(&mut owner, &backend, b"shared").await;
    owner.publish(first, &publisher).await.unwrap();
    // Reusing an acknowledged CID needs no supplied bytes. The new release
    // still acquires its own durable stage acknowledgement before publishing.
    let second = owner.begin(shared, owner.current().unwrap()).unwrap();
    owner
        .retain_stage(second, shared, &[], &backend)
        .await
        .unwrap();
    owner
        .retain_stage(second, shared, &[(shared, bytes)], &backend)
        .await
        .unwrap();
    assert_eq!(owner.releases().unwrap()[1].stages.len(), 1);
    owner.finish_retention(second).unwrap();
    owner.publish(second, &publisher).await.unwrap();
    assert!(owner.prune(first).is_err());
    assert_eq!(backend.retained.lock().unwrap().len(), 2);
}

#[tokio::test]
async fn domains_are_checked_before_any_effect() {
    let a = tempfile::tempdir().unwrap();
    let b = tempfile::tempdir().unwrap();
    let mut owner = Owner::open(a.path(), "other").unwrap();
    let backend = Backend::default();
    let (root, data) = block(b"domain");
    let id = owner.begin(root, None).unwrap();
    assert!(owner
        .retain_stage(id, root, &[(root, data)], &backend)
        .await
        .is_err());
    assert!(backend.retained.lock().unwrap().is_empty());
    // Another journal cannot reuse roots merely because Kubo has those CIDs.
    let mut other = Owner::open(b.path(), "fixture").unwrap();
    let id = other.begin(root, None).unwrap();
    assert!(other.retain_stage(id, root, &[], &backend).await.is_err());
    let id = retained(&mut other, &backend, b"domain").await;
    other.state.domain = "other".to_owned();
    assert!(other
        .publish(id, &FixturePublisher::default())
        .await
        .is_err());
}

#[test]
fn corrupt_missing_noncanonical_and_oversized_journals_fail_closed() {
    for content in [
        b"{bad".as_slice(),
        br#"{"version":3,"domain":"fixture","current":null,"releases":[]}"#,
        br#"{"version":1,"domain":"fixture","current":null,"releases":[]} "#,
    ] {
        let dir = tempfile::tempdir().unwrap();
        drop(Owner::open(dir.path(), "fixture").unwrap());
        std::fs::write(dir.path().join("state.json"), content).unwrap();
        assert!(Owner::open(dir.path(), "fixture").is_err());
    }
    let dir = tempfile::tempdir().unwrap();
    drop(Owner::open(dir.path(), "fixture").unwrap());
    std::fs::remove_file(dir.path().join("state.json")).unwrap();
    assert!(Owner::open(dir.path(), "fixture").is_err());
    std::fs::write(
        dir.path().join("state.json"),
        vec![b' '; journal::MAX_STATE_BYTES as usize + 1],
    )
    .unwrap();
    assert!(Owner::open(dir.path(), "fixture").is_err());
}

#[test]
fn journal_write_failures_poison_owner_until_reopen() {
    use journal::Checkpoint::*;
    for point in [Write, FileSync, Rename, DirectorySync] {
        let dir = tempfile::tempdir().unwrap();
        let mut owner = Owner::open(dir.path(), "fixture").unwrap();
        owner.journal.fault = Some((point, false));
        assert!(owner.begin(block(b"fault").0, None).is_err());
        assert!(owner.current().is_err());
        assert!(owner.begin(block(b"another").0, None).is_err());
        drop(owner);
        let mut owner = Owner::open(dir.path(), "fixture").unwrap();
        owner.begin(block(b"fault").0, None).unwrap();
    }
}

#[test]
fn bounded_release_history_does_not_reuse_generation_numbers() {
    let dir = tempfile::tempdir().unwrap();
    let mut owner = Owner::open(dir.path(), "fixture").unwrap();
    for expected in 1..=MAX_RELEASES {
        let id = owner.begin(block(b"same").0, None).unwrap();
        assert_eq!(id, expected as u64);
        owner.retire(id).unwrap();
    }
    assert!(owner.begin(block(b"overflow").0, None).is_err());
    drop(owner);
    let mut owner = Owner::open(dir.path(), "fixture").unwrap();
    assert!(owner.begin(block(b"overflow").0, None).is_err());
}

// Each subprocess exits inside a real filesystem commit. No destructors run.
// These tests exercise file/directory sync; they do not emulate power loss.
#[test]
fn crash_child() {
    let Some(directory) = std::env::var_os("WW_RELEASE_CRASH_DIRECTORY") else {
        return;
    };
    let transition: u8 = std::env::var("WW_RELEASE_CRASH_TRANSITION")
        .unwrap()
        .parse()
        .unwrap();
    let checkpoint: u8 = std::env::var("WW_RELEASE_CRASH_CHECKPOINT")
        .unwrap()
        .parse()
        .unwrap();
    let point = [
        journal::Checkpoint::Write,
        journal::Checkpoint::FileSync,
        journal::Checkpoint::Rename,
        journal::Checkpoint::DirectorySync,
    ][checkpoint as usize];
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    rt.block_on(async {
        let mut owner = Owner::open(Path::new(&directory), "fixture").unwrap();
        let (root, data) = block(b"crash");
        let backend = Backend::default();
        let publisher = FixturePublisher::default();
        if transition == 0 {
            owner.journal.fault = Some((point, true));
            owner.begin(root, None).unwrap();
        }
        let id = owner.begin(root, None).unwrap();
        if transition == 1 {
            owner.journal.fault = Some((point, true));
            owner
                .retain_stage(id, root, &[(root, data.clone())], &backend)
                .await
                .unwrap();
        }
        if transition == 2 {
            backend
                .lose_ack
                .store(true, std::sync::atomic::Ordering::SeqCst);
            assert!(owner
                .retain_stage(id, root, &[(root, data.clone())], &backend)
                .await
                .is_err());
            owner.journal.fault = Some((point, true));
            owner.reconcile_retention(id, &backend).await.unwrap();
        }
        owner
            .retain_stage(id, root, &[(root, data)], &backend)
            .await
            .unwrap();
        if transition == 3 {
            owner.journal.fault = Some((point, true));
            owner.finish_retention(id).unwrap();
        }
        owner.finish_retention(id).unwrap();
        if transition == 4 {
            owner.journal.fault = Some((point, true));
            owner.publish(id, &publisher).await.unwrap();
        }
        if transition == 5 {
            publisher
                .lose_ack
                .store(true, std::sync::atomic::Ordering::SeqCst);
            assert!(owner.publish(id, &publisher).await.is_err());
            owner.journal.fault = Some((point, true));
            owner.publish(id, &publisher).await.unwrap();
        }
        if transition == 6 {
            owner.publish(id, &publisher).await.unwrap();
            let next = owner
                .begin(block(b"abandoned").0, owner.current().unwrap())
                .unwrap();
            owner.journal.fault = Some((point, true));
            owner.retire(next).unwrap();
        }
        panic!("crash hook did not exit");
    });
}

#[test]
fn process_crash_at_every_durable_transition_boundary() {
    for transition in 0..7 {
        for checkpoint in 0..4 {
            let dir = tempfile::tempdir().unwrap();
            let output = std::process::Command::new(std::env::current_exe().unwrap())
                .args(["--exact", "release::tests::crash_child", "--nocapture"])
                .env("WW_RELEASE_CRASH_DIRECTORY", dir.path())
                .env("WW_RELEASE_CRASH_TRANSITION", transition.to_string())
                .env("WW_RELEASE_CRASH_CHECKPOINT", checkpoint.to_string())
                .output()
                .unwrap();
            assert_eq!(
                output.status.code(),
                Some(86),
                "transition {transition}, checkpoint {checkpoint}: {}",
                String::from_utf8_lossy(&output.stdout)
            );
            let owner = Owner::open(dir.path(), "fixture").unwrap();
            // Recovery validates all state invariants. Publication implies the
            // exact root acknowledgement survived the same durable journal.
            if let Some(current) = owner.current().unwrap() {
                let release = owner
                    .releases()
                    .unwrap()
                    .iter()
                    .find(|r| r.publication == current)
                    .unwrap();
                assert!(release.stages.contains(&current.root));
                assert_eq!(release.state, ReleaseState::Published);
                assert!(release.announcement.is_some());
            }
            for release in owner.releases().unwrap() {
                if release.publishing {
                    assert_eq!(release.state, ReleaseState::Published);
                }
            }
        }
    }
}

#[test]
fn lock_probe_child() {
    let Some(directory) = std::env::var_os("WW_RELEASE_LOCK_PROBE") else {
        return;
    };
    assert!(
        Owner::open(Path::new(&directory), "fixture").is_err(),
        "another process acquired a live owner's journal"
    );
}

#[test]
fn competing_process_cannot_open_the_live_owner_journal() {
    let dir = tempfile::tempdir().unwrap();
    let owner = Owner::open(dir.path(), "fixture").unwrap();
    let output = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "release::tests::lock_probe_child", "--nocapture"])
        .env("WW_RELEASE_LOCK_PROBE", dir.path())
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stdout)
    );
    drop(owner);
    Owner::open(dir.path(), "fixture").unwrap();
}

#[tokio::test]
async fn stage_limit_is_global_persistent_and_checked_before_import() {
    let dir = tempfile::tempdir().unwrap();
    let mut owner = Owner::open(dir.path(), "fixture").unwrap();
    let (root, bytes) = block(b"stage-overflow");
    let id = owner.begin(root, None).unwrap();
    // Populate the last valid snapshot directly to avoid 4096 fsyncs. The
    // real transition path and recovery both validate this same bounded state.
    let mut full = owner.state.clone();
    full.releases[0].stages = (0..MAX_STAGES)
        .map(|index| block(&index.to_le_bytes()).0)
        .collect();
    owner.commit(full).unwrap();
    drop(owner);
    let mut owner = Owner::open(dir.path(), "fixture").unwrap();
    let backend = Backend::default();
    assert!(owner
        .retain_stage(id, root, &[(root, bytes.clone())], &backend)
        .await
        .is_err());
    assert!(backend.retained.lock().unwrap().is_empty());
    owner.retire(id).unwrap();
    let next = owner.begin(root, None).unwrap();
    assert!(owner
        .retain_stage(next, root, &[(root, bytes)], &backend)
        .await
        .is_err());
    assert!(backend.retained.lock().unwrap().is_empty());
}

#[tokio::test]
async fn delayed_older_delivery_can_regress_discovery_but_not_local_authority() {
    use std::sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    };
    use tokio::sync::Notify;

    struct DelayedPublisher {
        current: Arc<Mutex<Option<Publication>>>,
        delay_once: AtomicBool,
        release: Arc<Notify>,
        settled: Arc<Notify>,
    }
    fn compare_and_publish(
        current: &Mutex<Option<Publication>>,
        intent: &PublicationIntent,
    ) -> Result<()> {
        let mut current = current.lock().unwrap();
        if *current == Some(intent.next.clone()) {
            return Ok(());
        }
        ensure!(*current == intent.previous, "stale delayed publication");
        *current = Some(intent.next.clone());
        Ok(())
    }
    #[async_trait]
    impl Publisher for DelayedPublisher {
        fn domain(&self) -> &str {
            "fixture"
        }
        async fn prepare(&self, intent: &PublicationIntent) -> Result<Vec<u8>> {
            Ok(serde_json::to_vec(intent)?)
        }
        async fn publish(&self, intent: &PublicationIntent, announcement: &[u8]) -> Result<()> {
            assert_eq!(announcement, serde_json::to_vec(intent)?);
            if self.delay_once.swap(false, Ordering::SeqCst) {
                let current = self.current.clone();
                let release = self.release.clone();
                let settled = self.settled.clone();
                let intent = intent.clone();
                // Models a remote request that outlives the dropped waiter.
                tokio::spawn(async move {
                    release.notified().await;
                    // A receiver already admitted this request before the
                    // newer write; its delayed storage write is not fenced.
                    *current.lock().unwrap() = Some(intent.next);
                    settled.notify_one();
                });
                std::future::pending::<()>().await;
            }
            compare_and_publish(&self.current, intent)?;
            Ok(())
        }
    }
    let publisher = DelayedPublisher {
        current: Arc::new(Mutex::new(None)),
        delay_once: AtomicBool::new(true),
        release: Arc::new(Notify::new()),
        settled: Arc::new(Notify::new()),
    };
    let dir = tempfile::tempdir().unwrap();
    let mut owner = Owner::open(dir.path(), "fixture").unwrap();
    let backend = Backend::default();
    let first = retained(&mut owner, &backend, b"delayed first").await;
    assert!(
        tokio::time::timeout(Duration::from_millis(5), owner.publish(first, &publisher))
            .await
            .is_err()
    );
    assert!(owner.begin(block(b"second").0, None).is_err());
    drop(owner);
    let mut owner = Owner::open(dir.path(), "fixture").unwrap();
    owner.publish(first, &publisher).await.unwrap();
    let second = retained(&mut owner, &backend, b"second").await;
    owner.publish(second, &publisher).await.unwrap();
    publisher.release.notify_one();
    tokio::time::timeout(Duration::from_secs(1), publisher.settled.notified())
        .await
        .unwrap();
    assert_eq!(
        publisher
            .current
            .lock()
            .unwrap()
            .as_ref()
            .unwrap()
            .generation,
        first
    );
    assert_eq!(owner.current().unwrap().unwrap().generation, second);
    assert!(owner.publish(first, &publisher).await.is_err());
}

#[tokio::test]
async fn publication_adapter_observes_durable_retention_and_intent_before_effect() {
    struct InspectPublisher(std::path::PathBuf);
    #[async_trait]
    impl Publisher for InspectPublisher {
        fn domain(&self) -> &str {
            "fixture"
        }
        async fn prepare(&self, intent: &PublicationIntent) -> Result<Vec<u8>> {
            Ok(serde_json::to_vec(intent)?)
        }
        async fn publish(&self, intent: &PublicationIntent, announcement: &[u8]) -> Result<()> {
            assert_eq!(announcement, serde_json::to_vec(intent)?);
            let bytes = std::fs::read(self.0.join("state.json"))?;
            let state: State = serde_json::from_slice(&bytes)?;
            state.validate()?;
            let release = state
                .releases
                .iter()
                .find(|r| r.publication == intent.next)
                .unwrap();
            assert_eq!(release.state, ReleaseState::Published);
            assert!(release.publishing);
            assert!(release.pending_stage.is_none());
            assert!(release.stages.contains(&intent.next.root));
            assert_eq!(state.current, Some(intent.next.clone()));
            Ok(())
        }
    }
    let dir = tempfile::tempdir().unwrap();
    let mut owner = Owner::open(dir.path(), "fixture").unwrap();
    let id = retained(&mut owner, &Backend::default(), b"durably retained").await;
    owner
        .publish(id, &InspectPublisher(dir.path().to_owned()))
        .await
        .unwrap();
}

#[tokio::test]
async fn local_commit_survives_uncertain_delivery_and_can_be_superseded() {
    let dir = tempfile::tempdir().unwrap();
    let backend = Backend::default();
    let publisher = FixturePublisher::default();
    let mut owner = Owner::open(dir.path(), "fixture").unwrap();
    let first = retained(&mut owner, &backend, b"local-first").await;
    publisher
        .lose_ack
        .store(true, std::sync::atomic::Ordering::SeqCst);
    assert!(owner.publish(first, &publisher).await.is_err());
    assert_eq!(owner.current().unwrap().unwrap().generation, first);
    drop(owner);
    let mut owner = Owner::open(dir.path(), "fixture").unwrap();
    assert_eq!(owner.current().unwrap().unwrap().generation, first);
    let next = retained(&mut owner, &backend, b"local-next").await;
    owner.publish(next, &publisher).await.unwrap();
    assert!(owner.publish(first, &publisher).await.is_err());
    assert_eq!(owner.current().unwrap().unwrap().generation, next);
}
