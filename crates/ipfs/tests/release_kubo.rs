//! Release retention against disposable, offline Kubo 0.33.0 repositories.
//! Run with WW_TEST_REQUIRE_KUBO=1 and the pinned `ipfs` binary on PATH.
//! Publication is a fixture; these tests never publish IPNS records.

use std::collections::BTreeMap;
use std::path::Path;
use std::process::{Child, Command, Output, Stdio};
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Mutex,
};
use std::time::{Duration, Instant};

use anyhow::{bail, ensure, Result};
use async_trait::async_trait;
use cid::Cid;
use ipfs::release::{KuboRetention, Owner, Publication, PublicationIntent, Publisher, Retention};
use sha2::{Digest, Sha256};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

const DOMAIN: &str = "disposable-release-fixture";

struct Kubo {
    child: Child,
    repo: tempfile::TempDir,
    url: String,
    api: String,
}

impl Kubo {
    fn command(repo: &Path) -> Command {
        let mut command = Command::new("ipfs");
        command.env("IPFS_PATH", repo);
        command
    }

    fn checked(output: Output) -> Vec<u8> {
        assert!(
            output.status.success(),
            "Kubo command failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        output.stdout
    }

    fn start() -> Option<Self> {
        if std::env::var_os("WW_TEST_REQUIRE_KUBO").is_none() {
            eprintln!("skipping real release retention; set WW_TEST_REQUIRE_KUBO=1");
            return None;
        }
        let version = Self::checked(
            Command::new("ipfs")
                .args(["version", "--number"])
                .output()
                .expect("WW_TEST_REQUIRE_KUBO=1 requires ipfs on PATH"),
        );
        assert_eq!(String::from_utf8(version).unwrap().trim(), "0.33.0");
        let repo = tempfile::tempdir().unwrap();
        let run =
            |args: &[&str]| Self::checked(Self::command(repo.path()).args(args).output().unwrap());
        run(&["init", "--profile=test"]);
        run(&["config", "Addresses.API", "/ip4/127.0.0.1/tcp/0"]);
        run(&["config", "Addresses.Gateway", "/ip4/127.0.0.1/tcp/0"]);
        run(&["config", "--json", "Addresses.Swarm", "[]"]);
        // Flush blocks while the CAR request is still streaming, so the GC
        // test can observe imported data before releasing the final bytes.
        run(&["config", "--json", "Import.BatchMaxNodes", "1"]);
        let child = Self::command(repo.path())
            .args(["daemon", "--offline", "--enable-gc"])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let mut kubo = Self {
            child,
            repo,
            url: String::new(),
            api: String::new(),
        };
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            if let Ok(api) = std::fs::read_to_string(kubo.repo.path().join("api")) {
                kubo.api = api.trim().to_owned();
                let parts: Vec<_> = kubo.api.split('/').collect();
                kubo.url = format!("http://{}:{}", parts[2], parts[4]);
                if kubo
                    .command_api()
                    .arg("id")
                    .output()
                    .unwrap()
                    .status
                    .success()
                {
                    return Some(kubo);
                }
            }
            assert!(Instant::now() < deadline, "isolated Kubo startup timed out");
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    fn restart(&mut self) {
        self.child.kill().unwrap();
        self.child.wait().unwrap();
        self.child = Self::command(self.repo.path())
            .args(["daemon", "--offline", "--enable-gc"])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            if let Ok(api) = std::fs::read_to_string(self.repo.path().join("api")) {
                self.api = api.trim().to_owned();
                let parts: Vec<_> = self.api.split('/').collect();
                self.url = format!("http://{}:{}", parts[2], parts[4]);
                if self
                    .command_api()
                    .arg("id")
                    .output()
                    .unwrap()
                    .status
                    .success()
                {
                    return;
                }
            }
            assert!(Instant::now() < deadline, "isolated Kubo restart timed out");
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    fn command_api(&self) -> Command {
        let mut command = Self::command(self.repo.path());
        command.arg(format!("--api={}", self.api));
        command
    }

    fn cli(&self, args: &[&str]) -> Vec<u8> {
        Self::checked(self.command_api().args(args).output().unwrap())
    }

    fn client(&self) -> ipfs::HttpClient {
        ipfs::HttpClient::new(self.url.clone())
    }

    fn retention(&self) -> KuboRetention {
        KuboRetention::new(self.client(), DOMAIN.to_owned(), Duration::from_secs(30)).unwrap()
    }

    fn assert_pinned(&self, cid: Cid) {
        self.cli(&["pin", "ls", "--type=recursive", &cid.to_string()]);
    }
}

impl Drop for Kubo {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn block(codec: u64, bytes: Vec<u8>) -> (Cid, Vec<u8>) {
    let digest = cid::multihash::Multihash::wrap(0x12, &Sha256::digest(&bytes)).unwrap();
    (Cid::new_v1(codec, digest), bytes)
}

fn links(roots: &[Cid]) -> (Cid, Vec<u8>) {
    block(
        0x71,
        serde_ipld_dagcbor::to_vec(&BTreeMap::from([("artifacts", roots)])).unwrap(),
    )
}

#[derive(Default)]
struct FixturePublisher(Mutex<Option<Publication>>);

#[async_trait]
impl Publisher for FixturePublisher {
    fn domain(&self) -> &str {
        DOMAIN
    }
    async fn prepare(&self, intent: &PublicationIntent) -> Result<Vec<u8>> {
        Ok(serde_json::to_vec(intent)?)
    }
    async fn publish(&self, intent: &PublicationIntent, announcement: &[u8]) -> Result<()> {
        assert_eq!(announcement, serde_json::to_vec(intent)?);
        let mut current = self.0.lock().unwrap();
        if *current == Some(intent.next.clone()) {
            return Ok(());
        }
        ensure!(
            *current == intent.previous,
            "fixture conditional publication conflict"
        );
        *current = Some(intent.next.clone());
        Ok(())
    }
}

struct LoseAck {
    backend: KuboRetention,
    once: AtomicBool,
}

#[async_trait]
impl Retention for LoseAck {
    fn domain(&self) -> &str {
        DOMAIN
    }
    async fn import(&self, root: &Cid, blocks: &[(Cid, Vec<u8>)]) -> Result<()> {
        self.backend.import(root, blocks).await?;
        if self.once.swap(false, Ordering::SeqCst) {
            bail!("injected acknowledgement loss after real Kubo retention");
        }
        Ok(())
    }
}

#[tokio::test]
async fn lost_retention_ack_reconciles_after_restart_and_gc() {
    let Some(mut kubo) = Kubo::start() else {
        return;
    };
    let state = tempfile::tempdir().unwrap();
    let leaf = block(0x55, b"acknowledgement recovery".to_vec());
    let root = links(&[leaf.0]);
    let mut owner = Owner::open(state.path(), DOMAIN).unwrap();
    let id = owner.begin(root.0, None).unwrap();
    let backend = LoseAck {
        backend: kubo.retention(),
        once: AtomicBool::new(true),
    };
    assert!(owner
        .retain_stage(id, root.0, &[root.clone(), leaf.clone()], &backend)
        .await
        .is_err());
    assert_eq!(owner.current().unwrap(), None);
    assert!(owner.finish_retention(id).is_err());
    kubo.assert_pinned(root.0);
    drop(owner);
    kubo.restart();
    kubo.cli(&["repo", "gc"]);
    let mut owner = Owner::open(state.path(), DOMAIN).unwrap();
    owner
        .reconcile_retention(id, &kubo.retention())
        .await
        .unwrap();
    owner.finish_retention(id).unwrap();
    let publisher = FixturePublisher::default();
    owner.publish(id, &publisher).await.unwrap();
    assert_eq!(owner.current().unwrap().unwrap().root, root.0);
    assert_eq!(
        kubo.client().block_get(&leaf.0, 1024).await.unwrap(),
        leaf.1
    );
}

struct CancelAck {
    backend: KuboRetention,
    retained: tokio::sync::Notify,
}

#[async_trait]
impl Retention for CancelAck {
    fn domain(&self) -> &str {
        DOMAIN
    }
    async fn import(&self, root: &Cid, blocks: &[(Cid, Vec<u8>)]) -> Result<()> {
        self.backend.import(root, blocks).await?;
        self.retained.notify_one();
        std::future::pending().await
    }
}

#[tokio::test]
async fn cancellation_after_remote_retention_requires_reconciliation() {
    let Some(kubo) = Kubo::start() else { return };
    let state = tempfile::tempdir().unwrap();
    let root = block(0x55, b"cancelled after remote pin".to_vec());
    let mut owner = Owner::open(state.path(), DOMAIN).unwrap();
    let id = owner.begin(root.0, None).unwrap();
    let backend = CancelAck {
        backend: kubo.retention(),
        retained: tokio::sync::Notify::new(),
    };
    {
        let importing = owner.retain_stage(id, root.0, std::slice::from_ref(&root), &backend);
        tokio::pin!(importing);
        tokio::select! {
            result = &mut importing => panic!("retention unexpectedly completed: {result:?}"),
            () = backend.retained.notified() => (),
        }
        // Drop the operation after the real Kubo pin but before the owner sees
        // an acknowledgement. Cancellation cannot promote the candidate.
    }
    assert!(owner.finish_retention(id).is_err());
    assert_eq!(owner.current().unwrap(), None);
    drop(owner);
    kubo.cli(&["repo", "gc"]);
    let mut owner = Owner::open(state.path(), DOMAIN).unwrap();
    owner
        .reconcile_retention(id, &kubo.retention())
        .await
        .unwrap();
    owner.finish_retention(id).unwrap();
    owner
        .publish(id, &FixturePublisher::default())
        .await
        .unwrap();
    kubo.assert_pinned(root.0);
}

#[tokio::test]
async fn incomplete_graph_and_corrupt_block_never_advance_publication() {
    let Some(kubo) = Kubo::start() else { return };
    let state = tempfile::tempdir().unwrap();
    let missing = block(0x55, b"not imported".to_vec());
    let root = links(&[missing.0]);
    let mut owner = Owner::open(state.path(), DOMAIN).unwrap();
    let id = owner.begin(root.0, None).unwrap();
    let retention = kubo.retention();
    assert!(owner
        .retain_stage(id, root.0, std::slice::from_ref(&root), &retention)
        .await
        .is_err());
    assert!(owner
        .retain_stage(
            id,
            root.0,
            &[root.clone(), (missing.0, b"corrupt".to_vec())],
            &retention
        )
        .await
        .is_err());
    assert!(owner.finish_retention(id).is_err());
    assert!(owner
        .publish(id, &FixturePublisher::default())
        .await
        .is_err());
    assert_eq!(owner.current().unwrap(), None);
    // The actual Kubo endpoint returns HTTP 200 with PinErrorMsg for a
    // well-formed CAR whose reachable descendant is absent.
    let error = kubo
        .client()
        .import_composed(&root.0, std::slice::from_ref(&root))
        .await
        .unwrap_err();
    assert!(error.to_string().contains("failed to pin"), "{error:#}");
    assert!(kubo
        .command_api()
        .args(["pin", "ls", "--type=recursive", &root.0.to_string()])
        .output()
        .unwrap()
        .status
        .code()
        .is_some_and(|code| code != 0));
}

#[tokio::test]
async fn larger_than_car_limit_graph_uses_retained_stages_and_shared_descendants() {
    let Some(kubo) = Kubo::start() else { return };
    let state = tempfile::tempdir().unwrap();
    // Derive artifact identities before allocating any stage. Only one 24MiB
    // CAR input is held at a time; the final reachable payload is 72MiB.
    let stage_roots: Vec<_> = (0..3)
        .map(|stage| {
            let cids: Vec<_> = (0..24)
                .map(|index| block(0x55, vec![(stage * 24 + index) as u8; 1024 * 1024]).0)
                .collect();
            links(&cids).0
        })
        .collect();
    // Exercise the actual versioned Map as the release graph root.
    let map = ipfs::map::Map::new()
        .insert(
            "artifacts",
            ipfs::map::Value::List(
                stage_roots
                    .iter()
                    .copied()
                    .map(ipfs::map::Value::Link)
                    .collect(),
            ),
        )
        .unwrap();
    let release = (map.cid().unwrap(), map.encode().unwrap());
    let mut owner = Owner::open(state.path(), DOMAIN).unwrap();
    let id = owner.begin(release.0, None).unwrap();
    let retention = kubo.retention();
    for (stage, expected_root) in stage_roots.iter().enumerate() {
        let mut blocks: Vec<_> = (0..24)
            .map(|index| block(0x55, vec![(stage * 24 + index) as u8; 1024 * 1024]))
            .collect();
        let root = links(&blocks.iter().map(|(cid, _)| *cid).collect::<Vec<_>>());
        assert_eq!(root.0, *expected_root);
        blocks.push(root.clone());
        owner
            .retain_stage(id, root.0, &blocks, &retention)
            .await
            .unwrap();
        kubo.cli(&["repo", "gc"]);
        kubo.assert_pinned(root.0);
    }
    owner
        .retain_stage(id, release.0, std::slice::from_ref(&release), &retention)
        .await
        .unwrap();
    owner.finish_retention(id).unwrap();
    let publisher = FixturePublisher::default();
    owner.publish(id, &publisher).await.unwrap();
    let previous = owner.current().unwrap();
    // The next release reuses the first complete release twice. CID pins do
    // not create independent claims, and retirement must retain descendants.
    let second = links(&[release.0, release.0]);
    let second_id = owner.begin(second.0, previous).unwrap();
    owner
        .retain_stage(
            second_id,
            second.0,
            std::slice::from_ref(&second),
            &retention,
        )
        .await
        .unwrap();
    owner.finish_retention(second_id).unwrap();
    owner.publish(second_id, &publisher).await.unwrap();
    kubo.cli(&["repo", "gc"]);
    for root in stage_roots {
        kubo.assert_pinned(root);
    }
    kubo.assert_pinned(release.0);
    kubo.assert_pinned(second.0);
    let first = block(0x55, vec![0; 1024 * 1024]);
    assert_eq!(
        kubo.client()
            .block_get(&first.0, 1024 * 1024)
            .await
            .unwrap(),
        first.1
    );
}

async fn read_request(stream: &mut tokio::net::TcpStream) -> (Vec<u8>, usize) {
    let mut request = Vec::new();
    let header_end = loop {
        let mut chunk = [0; 8192];
        let count = stream.read(&mut chunk).await.unwrap();
        assert!(count > 0);
        request.extend_from_slice(&chunk[..count]);
        if let Some(end) = request.windows(4).position(|w| w == b"\r\n\r\n") {
            break end + 4;
        }
        assert!(request.len() < 65536);
    };
    let length = std::str::from_utf8(&request[..header_end])
        .unwrap()
        .lines()
        .find_map(|line| {
            let (key, value) = line.split_once(':')?;
            key.eq_ignore_ascii_case("content-length")
                .then(|| value.trim().parse::<usize>().unwrap())
        })
        .unwrap();
    while request.len() < header_end + length {
        let mut chunk = [0; 8192];
        let count = stream.read(&mut chunk).await.unwrap();
        assert!(count > 0);
        request.extend_from_slice(&chunk[..count]);
    }
    (request, header_end)
}

#[tokio::test]
async fn concurrent_gc_waits_until_streaming_import_retains_complete_graph() {
    let Some(kubo) = Kubo::start() else { return };
    let state = tempfile::tempdir().unwrap();
    let first = block(0x55, vec![101; 1024 * 1024]);
    let last = block(0x55, vec![102; 1024 * 1024]);
    let root = links(&[first.0, last.0]);
    let blocks = [first.clone(), last, root.clone()];
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let backend = kubo.url.trim_start_matches("http://").to_owned();
    let (paused_tx, paused_rx) = tokio::sync::oneshot::channel();
    let (resume_tx, resume_rx) = tokio::sync::oneshot::channel();
    let proxy = tokio::spawn(async move {
        let (mut downstream, _) = listener.accept().await.unwrap();
        let (request, header_end) = read_request(&mut downstream).await;
        let mut upstream = tokio::net::TcpStream::connect(backend).await.unwrap();
        upstream
            .write_all(&request[..header_end - 2])
            .await
            .unwrap();
        upstream
            .write_all(b"Connection: close\r\n\r\n")
            .await
            .unwrap();
        let pause_at = request.len() - 65536;
        upstream
            .write_all(&request[header_end..pause_at])
            .await
            .unwrap();
        paused_tx.send(()).unwrap();
        resume_rx.await.unwrap();
        upstream.write_all(&request[pause_at..]).await.unwrap();
        let mut response = Vec::new();
        upstream.read_to_end(&mut response).await.unwrap();
        downstream.write_all(&response).await.unwrap();
    });
    let mut owner = Owner::open(state.path(), DOMAIN).unwrap();
    let id = owner.begin(root.0, None).unwrap();
    let retention = KuboRetention::new(
        ipfs::HttpClient::new(format!("http://{address}")),
        DOMAIN.to_owned(),
        Duration::from_secs(30),
    )
    .unwrap();
    let interleave = async {
        paused_rx.await.unwrap();
        let http = reqwest::Client::new();
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let response = http
                .post(format!("{}/api/v0/block/stat", kubo.url))
                .query(&[("arg", first.0.to_string())])
                .send()
                .await
                .unwrap();
            if response.status().is_success() {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "first CAR block never reached the real blockstore"
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        let gc_url = format!("{}/api/v0/repo/gc", kubo.url);
        let mut gc = tokio::spawn(async move {
            http.post(gc_url)
                .send()
                .await
                .unwrap()
                .bytes()
                .await
                .unwrap()
        });
        assert!(
            tokio::time::timeout(Duration::from_millis(150), &mut gc)
                .await
                .is_err(),
            "GC completed while the CAR request remained unfinished"
        );
        resume_tx.send(()).unwrap();
        tokio::time::timeout(Duration::from_secs(10), gc)
            .await
            .unwrap()
            .unwrap();
    };
    let (retained, ()) = tokio::join!(
        owner.retain_stage(id, root.0, &blocks, &retention),
        interleave
    );
    retained.unwrap();
    proxy.await.unwrap();
    owner.finish_retention(id).unwrap();
    kubo.assert_pinned(root.0);
    assert_eq!(
        kubo.client()
            .block_get(&first.0, 1024 * 1024)
            .await
            .unwrap(),
        first.1
    );
    owner
        .publish(id, &FixturePublisher::default())
        .await
        .unwrap();
}
