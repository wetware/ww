//! Composer v1 interoperability against isolated Kubo 0.33.0 repositories.
//! Run with WW_TEST_REQUIRE_KUBO=1 and the pinned `ipfs` binary on PATH.

use std::collections::BTreeMap;
use std::path::Path;
use std::process::{Child, Command, Output, Stdio};
use std::time::{Duration, Instant};

use cell::image::{compose_mounts, inspect_source, FrozenMount, GuestTarget, SourceKind};
use cid::Cid;
use serde_json::Value;

async fn capture_local(requests: Vec<cell::image::CaptureRequest>) -> cell::image::CaptureBatch {
    static CAPTURE: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());
    let _capture = CAPTURE.lock().await;
    let (owner, host) = cache::HostFsOwner::start(
        tokio::runtime::Handle::current(),
        cache::HostLimits::default(),
    )
    .unwrap();
    let (scope, fs_owner, finalizer, completion) = host.try_filesystem_scope().unwrap();
    let transaction = host
        .try_foreground_transaction(host.try_logical_io().unwrap(), &[&scope])
        .unwrap();
    let result =
        cell::image::CaptureBatch::capture(requests, &transaction, Default::default()).await;
    drop(transaction);
    drop(fs_owner);
    finalizer.run().await.unwrap();
    completion.wait().await.unwrap();
    owner.shutdown().await.unwrap();
    result.unwrap()
}

// Forward one complete test request to a real isolated backend. Closing the
// backend connection makes response completion explicit, including chunk trailers.
async fn forward_snapshot_request(
    stream: &mut tokio::net::TcpStream,
    backend: &str,
) -> (String, Vec<u8>) {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
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
    let headers = String::from_utf8(request[..header_end].to_vec()).unwrap();
    let length = headers
        .lines()
        .find_map(|line| {
            let (key, value) = line.split_once(':')?;
            key.eq_ignore_ascii_case("content-length")
                .then(|| value.trim().parse::<usize>().unwrap())
        })
        .unwrap_or(0);
    while request.len() < header_end + length {
        let mut chunk = [0; 8192];
        let count = stream.read(&mut chunk).await.unwrap();
        assert!(count > 0);
        request.extend_from_slice(&chunk[..count]);
    }
    let mut upstream = tokio::net::TcpStream::connect(backend).await.unwrap();
    upstream
        .write_all(&request[..header_end - 2])
        .await
        .unwrap();
    upstream
        .write_all(b"Connection: close\r\n\r\n")
        .await
        .unwrap();
    upstream.write_all(&request[header_end..]).await.unwrap();
    let mut response = Vec::new();
    upstream.read_to_end(&mut response).await.unwrap();
    assert!(
        response.starts_with(b"HTTP/1.1 200"),
        "backend failed: {}",
        String::from_utf8_lossy(&response)
    );
    (headers.lines().next().unwrap().to_owned(), response)
}

// Withhold the last multipart file until GC finishes. A file event proves
// /add acquired its pin lock; GC completion with an unfinished request proves
// Kubo yielded that lock while retaining its partially imported directory.
#[tokio::test(flavor = "multi_thread")]
async fn captured_directory_yields_to_gc_before_upload_finishes() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let Some(kubo) = Kubo::start() else { return };
    let local = tempfile::tempdir().unwrap();
    let local_path = local.path().canonicalize().unwrap();
    const FILES: usize = 40;
    for index in 0..FILES {
        std::fs::write(
            local_path.join(format!("a{index:02}")),
            vec![index as u8; 64 * 1024],
        )
        .unwrap();
    }
    let batch = capture_local(vec![cell::image::CaptureRequest {
        path: local_path,
        root_mount: false,
    }])
    .await;
    local.close().unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let api: Vec<_> = kubo.api.split('/').collect();
    let backend = format!("{}:{}", api[2], api[4]);
    let gc_repo = kubo.repo.path().to_owned();
    let gc_api = kubo.api.clone();
    let (yielded_tx, yielded_rx) = tokio::sync::oneshot::channel();
    let server = tokio::spawn(async move {
        let (mut downstream, _) = listener.accept().await.unwrap();
        let mut request = Vec::new();
        let header_end = loop {
            let mut chunk = [0; 8192];
            let count = downstream.read(&mut chunk).await.unwrap();
            assert!(count > 0);
            request.extend_from_slice(&chunk[..count]);
            if let Some(end) = request.windows(4).position(|w| w == b"\r\n\r\n") {
                break end + 4;
            }
            assert!(request.len() < 65536);
        };
        let headers = std::str::from_utf8(&request[..header_end])
            .unwrap()
            .to_owned();
        assert!(headers.lines().next().unwrap().contains("/api/v0/add?"));
        let length = headers
            .lines()
            .find_map(|line| {
                let (name, value) = line.split_once(':')?;
                name.eq_ignore_ascii_case("content-length")
                    .then(|| value.trim().parse::<usize>().unwrap())
            })
            .unwrap();
        let boundary = headers
            .lines()
            .find_map(|line| {
                let (name, value) = line.split_once(':')?;
                name.eq_ignore_ascii_case("content-type")
                    .then(|| value.split("boundary=").nth(1).unwrap().trim())
            })
            .unwrap();
        while request.len() < header_end + length {
            let mut chunk = [0; 8192];
            let count = downstream.read(&mut chunk).await.unwrap();
            assert!(count > 0);
            request.extend_from_slice(&chunk[..count]);
        }
        // Every gate ends after the following part's headers. The multipart
        // parser can then finish the previous file without receiving the next.
        let delimiter = format!("--{boundary}\r\n");
        let body = &request[header_end..];
        let starts: Vec<_> = body
            .windows(delimiter.len())
            .enumerate()
            .filter_map(|(index, bytes)| (bytes == delimiter.as_bytes()).then_some(index))
            .collect();
        assert_eq!(starts.len(), FILES + 1); // one directory and FILES files
        let gates: Vec<_> = starts
            .iter()
            .map(|start| {
                start
                    + body[*start..]
                        .windows(4)
                        .position(|w| w == b"\r\n\r\n")
                        .unwrap()
                    + 4
            })
            .collect();
        let mut upstream = tokio::net::TcpStream::connect(&backend).await.unwrap();
        upstream
            .write_all(&request[..header_end - 2])
            .await
            .unwrap();
        upstream
            .write_all(b"Connection: close\r\n\r\n")
            .await
            .unwrap();
        let (mut upstream_read, mut upstream_write) = upstream.into_split();
        let (_downstream_read, mut downstream_write) = downstream.into_split();
        let (first_event_tx, first_event_rx) = tokio::sync::oneshot::channel();
        let mut readers = tokio::task::JoinSet::new();
        readers.spawn(async move {
            let mut first_event_tx = Some(first_event_tx);
            let mut response = Vec::new();
            loop {
                let mut chunk = [0; 8192];
                let count = upstream_read.read(&mut chunk).await.unwrap();
                if count == 0 {
                    break;
                }
                response.extend_from_slice(&chunk[..count]);
                assert!(response.len() <= 1024 * 1024);
                if response
                    .windows(b"\"Name\":\"source/a00\"".len())
                    .any(|w| w == b"\"Name\":\"source/a00\"")
                {
                    if let Some(sender) = first_event_tx.take() {
                        sender.send(()).unwrap();
                    }
                }
                downstream_write.write_all(&chunk[..count]).await.unwrap();
            }
            assert!(response.starts_with(b"HTTP/1.1 200"));
            assert!(
                first_event_tx.is_none(),
                "Kubo never emitted the first file event"
            );
        });
        let mut sent = gates[2]; // root directory, a00, and a01 headers
        upstream_write.write_all(&body[..sent]).await.unwrap();
        tokio::time::timeout(Duration::from_secs(5), first_event_rx)
            .await
            .expect("Kubo did not stream an event before the upload finished")
            .unwrap();
        let mut command = Kubo::command(&gc_repo);
        command
            .arg(format!("--api={gc_api}"))
            .args(["repo", "gc"])
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        let mut gc = tokio::process::Command::from(command)
            .kill_on_drop(true)
            .spawn()
            .unwrap();
        tokio::time::timeout(Duration::from_secs(10), async {
            let mut completed = false;
            for &gate in &gates[3..] {
                if let Ok(status) =
                    tokio::time::timeout(Duration::from_millis(200), gc.wait()).await
                {
                    assert!(status.unwrap().success());
                    completed = true;
                    break;
                }
                // Leave the final file body withheld even at the last gate.
                upstream_write.write_all(&body[sent..gate]).await.unwrap();
                sent = gate;
            }
            if !completed {
                assert!(tokio::time::timeout(Duration::from_secs(1), gc.wait())
                    .await
                    .expect("GC did not finish while /add remained unfinished")
                    .unwrap()
                    .success());
            }
        })
        .await
        .expect("GC-yield gate exceeded its deadline");
        assert!(sent <= *gates.last().unwrap() && sent < body.len());
        yielded_tx.send(()).unwrap();
        upstream_write.write_all(&body[sent..]).await.unwrap();
        readers.join_next().await.unwrap().unwrap();
        // The production publisher still has to acknowledge the recursive pin
        // and independently verify every block after the interrupted import.
        loop {
            let (mut downstream, _) = listener.accept().await.unwrap();
            let (_, response) = forward_snapshot_request(&mut downstream, &backend).await;
            downstream.write_all(&response).await.unwrap();
        }
    });
    let client = ipfs::BootClient::one_attempt(
        ipfs::HttpClient::new(format!("http://{address}")),
        Duration::from_secs(20),
    );
    let (_sender, mut cancel) = tokio::sync::watch::channel(false);
    let result =
        tokio::time::timeout(Duration::from_secs(25), batch.publish(&client, &mut cancel)).await;
    server.abort();
    let server_result = server.await;
    if let Err(error) = server_result {
        assert!(error.is_cancelled(), "gated proxy failed: {error}");
    }
    let published = result.expect("gated publication timed out").unwrap();
    yielded_rx
        .await
        .expect("GC completion was not observed before the final file");
    let root = published.sources()[0].cid().to_string();
    kubo.cli(&["repo", "gc"]);
    kubo.cli(&["pin", "ls", "--type=recursive", &root]);
    for index in 0..FILES {
        assert_eq!(
            kubo.bytes(&["cat", &format!("/ipfs/{root}/a{index:02}")]),
            vec![index as u8; 64 * 1024]
        );
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn interrupted_snapshot_publication_retains_overlapping_real_pins() {
    use tokio::io::AsyncWriteExt;
    let Some(kubo) = Kubo::start() else { return };
    let bytes = b"shared captured bytes";
    let existing = kubo.add_file(bytes, "1", "true");
    let api: Vec<_> = kubo.api.split('/').collect();
    let backend = format!("{}:{}", api[2], api[4]);
    // Lose /add completion, lose CAR acknowledgement after backend pin, then
    // cancel after CAR acknowledgement while verification is pending.
    for stop_at in 0..3 {
        let file = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(file.path(), bytes).unwrap();
        let batch = capture_local(vec![cell::image::CaptureRequest {
            path: file.path().canonicalize().unwrap(),
            root_mount: false,
        }])
        .await;
        file.close().unwrap();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let backend = backend.clone();
        let (cancel_tx, mut cancel) = tokio::sync::watch::channel(false);
        let server = tokio::spawn(async move {
            for step in 0..=stop_at {
                let (mut downstream, _) = listener.accept().await.unwrap();
                if step == 2 {
                    // Receiving the block request proves publication returned a
                    // valid CAR acknowledgement before cancellation.
                    use tokio::io::AsyncReadExt;
                    let mut request = [0; 4096];
                    let count = downstream.read(&mut request).await.unwrap();
                    assert!(
                        String::from_utf8_lossy(&request[..count]).contains("/api/v0/block/get")
                    );
                    cancel_tx.send(true).unwrap();
                    return;
                }
                let (request, response) = forward_snapshot_request(&mut downstream, &backend).await;
                assert!(request.contains(if step == 0 {
                    "/api/v0/add?"
                } else {
                    "/api/v0/dag/import?"
                }));
                if step == stop_at {
                    return;
                }
                downstream.write_all(&response).await.unwrap();
            }
        });
        let client = ipfs::BootClient::one_attempt(
            ipfs::HttpClient::new(format!("http://{address}")),
            Duration::from_secs(5),
        );
        let failure = batch.publish(&client, &mut cancel).await.unwrap_err();
        if stop_at == 2 {
            assert_eq!(
                failure.acknowledged_roots,
                [existing.parse::<Cid>().unwrap()]
            );
            assert_eq!(failure.uncertain_source, None);
        } else {
            assert_eq!(failure.uncertain_source, Some(0));
            assert!(failure.acknowledged_roots.is_empty());
        }
        server.await.unwrap();
        kubo.cli(&["repo", "gc"]);
        kubo.cli(&["pin", "ls", "--type=recursive", &existing]);
        assert_eq!(kubo.bytes(&["cat", &existing]), bytes);
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn captured_local_sources_publish_verify_and_survive_gc() {
    use cell::image::{CaptureRequest, SourceKind};
    let Some(kubo) = Kubo::start() else { return };
    let local = tempfile::tempdir().unwrap();
    // macOS /var is a symlink. Tests select its explicit real path; production
    // capture must reject symlink components rather than canonicalize them.
    let local_path = local.path().canonicalize().unwrap();
    let standalone = local_path.join("standalone");
    let chunks: Vec<u8> = (0..700_001).map(|n| (n % 251) as u8).collect();
    std::fs::write(&standalone, &chunks).unwrap();
    let directory = local_path.join("directory");
    std::fs::create_dir_all(directory.join("empty/nested")).unwrap();
    for name in [
        "target",
        ".git",
        ".gitignore",
        "Cargo.lock",
        "café",
        "a\\b",
        "a\"b",
        "\u{85}\u{200d}",
    ] {
        std::fs::write(directory.join(name), name.as_bytes()).unwrap();
    }
    let empty = local_path.join("empty");
    std::fs::create_dir(&empty).unwrap();
    let empty_file = local_path.join("empty-file");
    let small_file = local_path.join("small-file");
    std::fs::write(&empty_file, []).unwrap();
    std::fs::write(&small_file, b"small captured file").unwrap();
    std::fs::create_dir(directory.join("prefix")).unwrap();
    std::fs::write(directory.join("prefix/child"), b"child").unwrap();
    std::fs::write(directory.join("prefix-sibling"), b"sibling").unwrap();
    let batch = capture_local(vec![
        CaptureRequest {
            path: standalone.clone(),
            root_mount: false,
        },
        CaptureRequest {
            path: directory.clone(),
            root_mount: false,
        },
        CaptureRequest {
            path: directory.clone(),
            root_mount: true,
        },
        CaptureRequest {
            path: empty,
            root_mount: false,
        },
        CaptureRequest {
            path: empty_file,
            root_mount: false,
        },
        CaptureRequest {
            path: small_file,
            root_mount: false,
        },
    ])
    .await;
    // Destruction after capture cannot affect any upload or verification read.
    local.close().unwrap();
    let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let gc_stop = stop.clone();
    let gc_repo = kubo.repo.path().to_owned();
    let gc_api = kubo.api.clone();
    let gc = std::thread::spawn(move || {
        let mut runs = 0;
        while !gc_stop.load(std::sync::atomic::Ordering::Acquire) {
            let status = Kubo::command(&gc_repo)
                .arg(format!("--api={gc_api}"))
                .args(["repo", "gc"])
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status()
                .unwrap();
            assert!(status.success());
            runs += 1;
        }
        runs
    });
    let (_sender, mut cancel) = tokio::sync::watch::channel(false);
    let result = batch.publish(&kubo.client(), &mut cancel).await;
    stop.store(true, std::sync::atomic::Ordering::Release);
    assert!(gc.join().unwrap() > 0);
    let published = result.unwrap();
    let sources = published.sources();
    assert_eq!(sources.len(), 6);
    assert_eq!(sources[0].kind(), SourceKind::File);
    assert_eq!(
        sources[0].dag_size(),
        kubo.measured_dag_size(sources[0].cid())
    );
    assert_eq!(kubo.bytes(&["cat", &sources[0].cid().to_string()]), chunks);
    let targeted = sources[1].cid().to_string();
    let root = sources[2].cid().to_string();
    for name in ["target", ".git", ".gitignore", "Cargo.lock"] {
        assert_eq!(
            kubo.bytes(&["cat", &format!("/ipfs/{targeted}/{name}")]),
            name.as_bytes()
        );
        assert!(!kubo
            .command_api()
            .args(["resolve", &format!("/ipfs/{root}/{name}")])
            .output()
            .unwrap()
            .status
            .success());
    }
    assert!(kubo
        .node(&kubo.cid_at(&targeted, "empty/nested"))
        .links
        .is_empty());
    assert!(kubo.node(&sources[3].cid().to_string()).links.is_empty());
    assert_eq!(sources[4].dag_size(), 0);
    assert_eq!(sources[4].kind(), SourceKind::File);
    assert!(kubo
        .bytes(&["cat", &sources[4].cid().to_string()])
        .is_empty());
    assert_eq!(
        kubo.bytes(&["cat", &sources[5].cid().to_string()]),
        b"small captured file"
    );
    let retained: Vec<_> = sources.iter().map(|s| s.cid()).collect();
    drop(published);
    kubo.cli(&["repo", "gc"]);
    for cid in retained {
        kubo.cli(&["pin", "ls", "--type=recursive", &cid.to_string()]);
        kubo.bytes(&["block", "get", &cid.to_string()]);
    }
    assert_eq!(
        kubo.bytes(&["cat", &kubo.cid_at(&targeted, "café")]),
        "café".as_bytes()
    );
}

struct Kubo {
    child: Child,
    repo: tempfile::TempDir,
    api: String,
}

impl Drop for Kubo {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
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
            eprintln!("skipping composer interoperability; set WW_TEST_REQUIRE_KUBO=1");
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
        let child = Self::command(repo.path())
            .args(["daemon", "--offline", "--enable-gc"])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let mut kubo = Self {
            child,
            repo,
            api: String::new(),
        };
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            if let Ok(api) = std::fs::read_to_string(kubo.repo.path().join("api")) {
                kubo.api = api.trim().to_owned();
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

    fn command_api(&self) -> Command {
        let mut command = Self::command(self.repo.path());
        command.arg(format!("--api={}", self.api));
        command
    }

    fn bytes(&self, args: &[&str]) -> Vec<u8> {
        Self::checked(self.command_api().args(args).output().unwrap())
    }

    fn cli(&self, args: &[&str]) -> String {
        String::from_utf8(self.bytes(args))
            .unwrap()
            .trim()
            .to_owned()
    }

    fn client(&self) -> ipfs::BootClient {
        let parts: Vec<_> = self.api.split('/').collect();
        let url = format!("http://{}:{}", parts[2], parts[4]);
        ipfs::BootClient::new(ipfs::HttpClient::new(url), 0, 1)
    }

    fn add(&self, files: &[(&str, &str)]) -> String {
        let dir = tempfile::tempdir().unwrap();
        for (name, content) in files {
            let path = dir.path().join(name);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, content).unwrap();
        }
        self.cli(&[
            "add",
            "-Qr",
            "--cid-version=1",
            "--raw-leaves=true",
            dir.path().to_str().unwrap(),
        ])
    }

    fn cid_at(&self, root: &str, path: &str) -> String {
        self.cli(&["resolve", &format!("/ipfs/{root}/{path}")])
            .trim_start_matches("/ipfs/")
            .to_owned()
    }

    fn node(&self, cid: &str) -> ipld_dagpb::PbNode {
        ipld_dagpb::PbNode::from_bytes(bytes::Bytes::from(self.bytes(&["block", "get", cid])))
            .unwrap()
    }

    fn link(&self, root: &str, name: &str) -> ipld_dagpb::PbLink {
        self.node(root)
            .links
            .into_iter()
            .find(|link| link.name.as_deref() == Some(name))
            .unwrap()
    }

    fn add_file(&self, content: &[u8], cid_version: &str, raw_leaves: &str) -> String {
        let file = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(file.path(), content).unwrap();
        self.cli(&[
            "add",
            "-Q",
            "--pin=true",
            "--wrap-with-directory=false",
            &format!("--cid-version={cid_version}"),
            "--hash=sha2-256",
            &format!("--raw-leaves={raw_leaves}"),
            "--chunker=size-262144",
            "--trickle=false",
            "--inline=false",
            "--nocopy=false",
            "--fscache=false",
            "--preserve-mode=false",
            "--preserve-mtime=false",
            "--progress=false",
            "--silent=false",
            "--only-hash=false",
            file.path().to_str().unwrap(),
        ])
    }

    // Measure actual serialized blocks per occurrence, independently of declared
    // Tsize and logical UnixFS filesize. Repeated links count repeatedly.
    fn measured_dag_size(&self, cid: Cid) -> u64 {
        let block = self.bytes(&["block", "get", &cid.to_string()]);
        let children = if cid.codec() == 0x70 {
            ipld_dagpb::PbNode::from_bytes(bytes::Bytes::from(block.clone()))
                .unwrap()
                .links
                .into_iter()
                .map(|link| self.measured_dag_size(link.cid))
                .sum()
        } else {
            assert_eq!(cid.codec(), 0x55);
            0
        };
        block.len() as u64 + children
    }

    async fn tree(&self, root: &str) -> BTreeMap<String, String> {
        let mut output = BTreeMap::new();
        let mut pending = vec![(root.to_owned(), String::new())];
        while let Some((cid, prefix)) = pending.pop() {
            for entry in self.client().ls(&format!("/ipfs/{cid}")).await.unwrap() {
                let path = format!("{prefix}{}", entry.name);
                if entry.entry_type == 1 {
                    output.insert(format!("{path}/"), "directory".to_owned());
                    pending.push((entry.hash, format!("{path}/")));
                } else {
                    output.insert(path, entry.hash);
                }
            }
        }
        output
    }
}

async fn compose(kubo: &Kubo, layers: &[String]) -> anyhow::Result<String> {
    let (_cancel_tx, mut cancel) = tokio::sync::watch::channel(false);
    cell::image::dag_merge(layers, &kubo.client(), &mut cancel).await
}

async fn freeze(kubo: &Kubo, cid: &str, target: &str) -> FrozenMount {
    let (_cancel_tx, mut cancel) = tokio::sync::watch::channel(false);
    let source = inspect_source(cid.parse().unwrap(), &kubo.client(), &mut cancel)
        .await
        .unwrap();
    FrozenMount::new(source, GuestTarget::new(target).unwrap()).unwrap()
}

async fn compose_frozen(
    kubo: &Kubo,
    initial: Option<&str>,
    mounts: &[FrozenMount],
) -> anyhow::Result<String> {
    let (_cancel_tx, mut cancel) = tokio::sync::watch::channel(false);
    compose_mounts(
        initial.map(|cid| cid.parse().unwrap()),
        mounts,
        &kubo.client(),
        &mut cancel,
    )
    .await
    .map(|cid| cid.to_string())
}

#[tokio::test]
async fn visible_semantics_match_mfs_and_survive_gc() {
    let Some(kubo) = Kubo::start() else { return };
    let base = kubo.add(&[
        ("keep/deep/file", "unchanged"),
        ("shared/keep", "keep"),
        ("shared/change", "old"),
        ("was-file", "file"),
        ("was-directory/old", "directory"),
        ("replace", "old"),
    ]);
    let overlay = kubo.add(&[
        ("shared/change", "new"),
        ("shared/added", "added"),
        ("was-file/child", "now directory"),
        ("was-directory", "now file"),
        ("replace", "new"),
        ("雪 e\u{301}é &?+#%", "exact UTF-8"),
    ]);
    let layers = [base.clone(), overlay.clone()];
    let root = compose(&kubo, &layers).await.unwrap();
    assert_eq!(root, compose(&kubo, &layers).await.unwrap());
    assert_eq!(kubo.cid_at(&root, "keep"), kubo.cid_at(&base, "keep"));
    assert_eq!(
        kubo.cid_at(&root, "was-file"),
        kubo.cid_at(&overlay, "was-file")
    );
    assert_eq!(
        kubo.cid_at(&root, "shared/keep"),
        kubo.cid_at(&base, "shared/keep")
    );
    assert_ne!(kubo.cid_at(&root, "shared"), kubo.cid_at(&base, "shared"));
    assert_eq!(
        kubo.cli(&["cat", &format!("/ipfs/{root}/shared/change")]),
        "new"
    );

    // Reference the historical visible operations, without requiring root identity.
    kubo.cli(&["files", "cp", &format!("/ipfs/{base}"), "/reference"]);
    for name in ["shared/change", "was-file", "was-directory", "replace"] {
        kubo.cli(&["files", "rm", "-r", &format!("/reference/{name}")]);
    }
    for name in [
        "shared/change",
        "shared/added",
        "was-file",
        "was-directory",
        "replace",
        "雪 e\u{301}é &?+#%",
    ] {
        kubo.cli(&[
            "files",
            "cp",
            &format!("/ipfs/{overlay}/{name}"),
            &format!("/reference/{name}"),
        ]);
    }
    let reference = kubo.cli(&["files", "stat", "--hash", "/reference"]);
    assert_eq!(kubo.tree(&root).await, kubo.tree(&reference).await);
    kubo.cli(&["files", "rm", "-r", "/reference"]);
    kubo.cli(&["pin", "rm", &base, &overlay]);
    kubo.cli(&["repo", "gc"]);
    kubo.cli(&["pin", "ls", "--type=recursive", &root]);
    assert_eq!(
        kubo.cli(&["cat", &format!("/ipfs/{root}/keep/deep/file")]),
        "unchanged"
    );
    assert_eq!(kubo.tree(&root).await.len(), 12);
}

#[tokio::test]
async fn directory_metadata_uses_overlay_wholesale_on_recursive_merge() {
    let Some(kubo) = Kubo::start() else { return };
    let base = kubo.add(&[("base", "base")]);
    let overlay = kubo.add(&[("overlay", "overlay")]);
    kubo.cli(&["files", "cp", &format!("/ipfs/{base}"), "/metadata"]);
    kubo.cli(&["files", "chmod", "750", "/metadata"]);
    kubo.cli(&[
        "files",
        "touch",
        "--mtime=123",
        "--mtime-nsecs=456",
        "/metadata",
    ]);
    let base = kubo.cli(&["files", "stat", "--hash", "/metadata"]);
    kubo.cli(&["pin", "add", &base]);
    // No overlay fields means no inherited mode or mtime.
    let overlay_node: Value = serde_json::from_str(&kubo.cli(&["dag", "get", &overlay])).unwrap();
    let root = compose(&kubo, &[base.clone(), overlay.clone()])
        .await
        .unwrap();
    let merged_node: Value = serde_json::from_str(&kubo.cli(&["dag", "get", &root])).unwrap();
    assert_eq!(merged_node["Data"], overlay_node["Data"]);
    assert_eq!(kubo.cid_at(&root, "base"), kubo.cid_at(&base, "base"));
    assert_eq!(
        kubo.cid_at(&root, "overlay"),
        kubo.cid_at(&overlay, "overlay")
    );

    // Both explicit overlay fields replace the earlier directory's fields.
    kubo.cli(&["files", "cp", &format!("/ipfs/{overlay}"), "/overlay"]);
    kubo.cli(&["files", "chmod", "700", "/overlay"]);
    kubo.cli(&[
        "files",
        "touch",
        "--mtime=789",
        "--mtime-nsecs=1",
        "/overlay",
    ]);
    let overlay = kubo.cli(&["files", "stat", "--hash", "/overlay"]);
    kubo.cli(&["pin", "add", &overlay]);
    let expected: Value = serde_json::from_str(&kubo.cli(&["dag", "get", &overlay])).unwrap();
    let layers = [base.clone(), overlay];
    let root = compose(&kubo, &layers).await.unwrap();
    let actual: Value = serde_json::from_str(&kubo.cli(&["dag", "get", &root])).unwrap();
    assert_eq!(actual["Data"], expected["Data"]);
    assert_eq!(compose(&kubo, &layers).await.unwrap(), root);
    assert_eq!(kubo.cid_at(&root, "base"), kubo.cid_at(&base, "base"));
    assert_eq!(
        compose(&kubo, std::slice::from_ref(&base)).await.unwrap(),
        base
    );
}

#[tokio::test]
async fn actual_kubo_hamt_fixture_fails_closed_at_root_and_nested() {
    let Some(kubo) = Kubo::start() else { return };
    let fixture = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/composer");
    let manifest: Value =
        serde_json::from_slice(&std::fs::read(fixture.join("hamt.json")).unwrap()).unwrap();
    let hamt = manifest["cid"].as_str().unwrap();
    kubo.cli(&["dag", "import", fixture.join("hamt.car").to_str().unwrap()]);
    assert_eq!(
        kubo.bytes(&["block", "get", hamt]),
        include_bytes!("fixtures/composer/hamt-root.dagpb")
    );
    assert_eq!(kubo.tree(hamt).await.len(), 4000);
    let error = compose(&kubo, &[hamt.to_owned()]).await.unwrap_err();
    assert!(format!("{error:#}").contains("HAMT"), "{error:#}");
    kubo.cli(&["files", "mkdir", "/nested"]);
    kubo.cli(&["files", "cp", &format!("/ipfs/{hamt}"), "/nested/large"]);
    let nested = kubo.cli(&["files", "stat", "--hash", "/nested"]);
    kubo.cli(&["pin", "add", &nested]);
    let ordinary = kubo.add(&[("ordinary", "file")]);
    let error = compose(&kubo, &[ordinary, nested]).await.unwrap_err();
    assert!(format!("{error:#}").contains("HAMT"), "{error:#}");
}

#[cfg(unix)]
#[tokio::test]
async fn cidv0_chunked_files_and_symlinks_reuse_their_original_dags() {
    let Some(kubo) = Kubo::start() else { return };
    let directory = tempfile::tempdir().unwrap();
    let content: Vec<u8> = (0..300_000).map(|index| (index % 251) as u8).collect();
    std::fs::write(directory.path().join("chunked"), &content).unwrap();
    std::os::unix::fs::symlink("chunked", directory.path().join("alias")).unwrap();
    let base = kubo.cli(&[
        "add",
        "-Qr",
        "--cid-version=0",
        "--raw-leaves=false",
        "--chunker=size-65536",
        directory.path().to_str().unwrap(),
    ]);
    assert!(base.starts_with("Qm"), "fixture must use CIDv0");
    // Kubo `resolve` promotes output CIDs to the starting path's CID version.
    // Inspect the serialized link to assert exact CIDv0 reference reuse.
    let link_cid = |root: &str, name: &str| {
        let node =
            ipld_dagpb::PbNode::from_bytes(bytes::Bytes::from(kubo.bytes(&["block", "get", root])))
                .unwrap();
        node.links
            .into_iter()
            .find(|link| link.name.as_deref() == Some(name))
            .unwrap()
            .cid
            .to_string()
    };
    let file = link_cid(&base, "chunked");
    let file_node: Value = serde_json::from_str(&kubo.cli(&["dag", "get", &file])).unwrap();
    assert!(file_node["Links"].as_array().unwrap().len() > 1);
    let symlink = link_cid(&base, "alias");
    let overlay = kubo.add(&[("added", "overlay")]);
    let root = compose(&kubo, &[base, overlay]).await.unwrap();
    assert_eq!(link_cid(&root, "chunked"), file);
    assert_eq!(link_cid(&root, "alias"), symlink);
    assert_eq!(
        kubo.bytes(&["cat", &format!("/ipfs/{root}/chunked")]),
        content
    );
    let destination = tempfile::tempdir().unwrap();
    let extracted = destination.path().join("image");
    kubo.cli(&["get", "--output", extracted.to_str().unwrap(), &root]);
    assert_eq!(
        std::fs::read_link(extracted.join("alias")).unwrap(),
        Path::new("chunked")
    );
    assert_eq!(std::fs::read(extracted.join("alias")).unwrap(), content);
}

#[tokio::test]
async fn import_rejects_http_success_when_kubo_cannot_pin_a_missing_descendant() {
    use cid::{multihash::Multihash, Cid};
    use ipld_dagpb::{PbLink, PbNode};
    use sha2::{Digest, Sha256};

    let Some(kubo) = Kubo::start() else { return };
    let hash = Sha256::digest(b"deliberately absent raw block");
    let missing = Cid::new_v1(0x55, Multihash::wrap(0x12, hash.as_slice()).unwrap());
    let block = PbNode {
        links: vec![PbLink {
            cid: missing,
            name: Some("missing".to_owned()),
            size: Some(29),
        }],
        data: Some(bytes::Bytes::from_static(&[0x08, 0x01])),
    }
    .into_bytes();
    let hash = Sha256::digest(&block);
    let root = Cid::new_v1(0x70, Multihash::wrap(0x12, hash.as_slice()).unwrap());
    let error = kubo
        .client()
        .client()
        .import_composed(&root, &[(root, block)])
        .await
        .unwrap_err();
    // This message comes from PinErrorMsg parsing after a successful HTTP status.
    assert!(
        format!("{error:#}").contains("DAG import failed to pin root"),
        "{error:#}"
    );
    assert!(error.downcast_ref::<ipfs::KuboApiError>().is_none());
    let pinned = kubo
        .command_api()
        .args(["pin", "ls", "--type=recursive", &root.to_string()])
        .output()
        .unwrap();
    assert!(
        !pinned.status.success(),
        "incomplete root must not be pinned"
    );
}

#[tokio::test]
async fn kubo_mtime_absent_and_positive_fraction_boundaries_remain_readable() {
    let Some(kubo) = Kubo::start() else { return };
    let base = kubo.add(&[("file", "content")]);
    for (index, (fraction, expected)) in [
        (None, vec![0x08, 0x01, 0x42, 0x02, 0x08, 0x7b]),
        (
            Some(1),
            vec![0x08, 0x01, 0x42, 0x07, 0x08, 0x7b, 0x15, 0x01, 0, 0, 0],
        ),
        (
            Some(999_999_999),
            vec![
                0x08, 0x01, 0x42, 0x07, 0x08, 0x7b, 0x15, 0xff, 0xc9, 0x9a, 0x3b,
            ],
        ),
    ]
    .into_iter()
    .enumerate()
    {
        let path = format!("/fraction-{index}");
        kubo.cli(&["files", "cp", &format!("/ipfs/{base}"), &path]);
        let mut args = vec![
            "files".to_owned(),
            "touch".to_owned(),
            "--mtime=123".to_owned(),
        ];
        if let Some(fraction) = fraction {
            args.push(format!("--mtime-nsecs={fraction}"));
        }
        args.push(path.clone());
        kubo.cli(&args.iter().map(String::as_str).collect::<Vec<_>>());
        let root = kubo.cli(&["files", "stat", "--hash", &path]);
        kubo.cli(&["pin", "add", &root]);
        let node = ipld_dagpb::PbNode::from_bytes(bytes::Bytes::from(
            kubo.bytes(&["block", "get", &root]),
        ))
        .unwrap();
        assert_eq!(
            node.data.unwrap().as_ref(),
            expected,
            "fraction={fraction:?}"
        );
        assert_eq!(
            compose(&kubo, std::slice::from_ref(&root)).await.unwrap(),
            root
        );
        assert_eq!(kubo.cli(&["cat", &format!("/ipfs/{root}/file")]), "content");
    }
}

#[tokio::test]
async fn legacy_dagpb_raw_with_explicit_filesize_remains_readable() {
    use cid::{multihash::Multihash, Cid};
    use ipld_dagpb::{PbLink, PbNode};
    use sha2::{Digest, Sha256};

    let Some(kubo) = Kubo::start() else { return };
    let address = |block: &[u8]| {
        Cid::new_v1(
            0x70,
            Multihash::wrap(0x12, Sha256::digest(block).as_slice()).unwrap(),
        )
    };
    // UnixFS Raw, Data="x", filesize=1. This is distinct from a raw-codec block.
    let raw_block = PbNode {
        links: vec![],
        data: Some(bytes::Bytes::from_static(&[
            0x08, 0x00, 0x12, 0x01, b'x', 0x18, 0x01,
        ])),
    }
    .into_bytes();
    let raw_cid = address(&raw_block);
    let directory = PbNode {
        links: vec![PbLink {
            cid: raw_cid,
            name: Some("legacy".to_owned()),
            size: Some(raw_block.len() as u64),
        }],
        data: Some(bytes::Bytes::from_static(&[0x08, 0x01])),
    }
    .into_bytes();
    let base = address(&directory);
    kubo.client()
        .import_composed(&base, &[(raw_cid, raw_block), (base, directory)])
        .await
        .unwrap();
    assert_eq!(kubo.bytes(&["cat", &format!("/ipfs/{base}/legacy")]), b"x");
    let overlay = kubo.add(&[("opaque", "raw-codec payload")]);
    let root = compose(&kubo, &[base.to_string(), overlay.clone()])
        .await
        .unwrap();
    assert_eq!(kubo.cid_at(&root, "legacy"), raw_cid.to_string());
    assert_eq!(kubo.bytes(&["cat", &format!("/ipfs/{root}/legacy")]), b"x");
    assert_eq!(
        kubo.cid_at(&root, "opaque"),
        kubo.cid_at(&overlay, "opaque")
    );
    assert_eq!(
        kubo.cli(&["cat", &format!("/ipfs/{root}/opaque")]),
        "raw-codec payload"
    );
    let (_cancel_tx, mut cancel) = tokio::sync::watch::channel(false);
    let source = inspect_source(raw_cid, &kubo.client(), &mut cancel)
        .await
        .unwrap();
    assert_eq!(source.kind(), SourceKind::File);
    assert_eq!(source.dag_size(), kubo.measured_dag_size(raw_cid));
    let operation = FrozenMount::new(source, GuestTarget::new("/standalone").unwrap()).unwrap();
    let root = compose_frozen(&kubo, None, &[operation]).await.unwrap();
    assert_eq!(kubo.link(&root, "standalone").cid, raw_cid);
    assert_eq!(
        kubo.bytes(&["cat", &format!("/ipfs/{root}/standalone")]),
        b"x"
    );
}

#[tokio::test]
async fn file_linking_to_directory_is_unreadable_in_kubo_and_rejected_by_composer() {
    use cid::{multihash::Multihash, Cid};
    use ipld_dagpb::{PbLink, PbNode};
    use sha2::{Digest, Sha256};

    let Some(kubo) = Kubo::start() else { return };
    let address = |codec, block: &[u8]| {
        Cid::new_v1(
            codec,
            Multihash::wrap(0x12, Sha256::digest(block).as_slice()).unwrap(),
        )
    };
    let child = PbNode {
        links: vec![],
        data: Some(bytes::Bytes::from_static(&[0x08, 0x01])),
    }
    .into_bytes();
    let child_cid = address(0x70, &child);
    let file = PbNode {
        links: vec![PbLink {
            cid: child_cid,
            name: None,
            size: Some(child.len() as u64),
        }],
        // File, filesize=0, blocksizes=[0].
        data: Some(bytes::Bytes::from_static(&[
            0x08, 0x02, 0x18, 0x00, 0x20, 0x00,
        ])),
    }
    .into_bytes();
    let file_cid = address(0x70, &file);
    let file_tsize = file.len() as u64 + child.len() as u64;
    let root_block = PbNode {
        links: vec![PbLink {
            cid: file_cid,
            name: Some("invalid".to_owned()),
            size: Some(file_tsize),
        }],
        data: Some(bytes::Bytes::from_static(&[0x08, 0x01])),
    }
    .into_bytes();
    let root = address(0x70, &root_block);
    kubo.client()
        .import_composed(
            &root,
            &[(child_cid, child), (file_cid, file), (root, root_block)],
        )
        .await
        .unwrap();

    let cat = kubo
        .command_api()
        .args(["cat", &format!("/ipfs/{root}/invalid")])
        .output()
        .unwrap();
    assert!(
        !cat.status.success(),
        "Kubo unexpectedly read File -> Directory"
    );
    let error = compose(&kubo, &[root.to_string()]).await.unwrap_err();
    assert!(format!("{error:#}").contains("file child"), "{error:#}");
}

#[tokio::test]
async fn incorrect_file_blocksizes_disagree_with_kubo_cat_and_are_rejected() {
    use cid::{multihash::Multihash, Cid};
    use ipld_dagpb::{PbLink, PbNode};
    use sha2::{Digest, Sha256};

    let Some(kubo) = Kubo::start() else { return };
    let address = |codec, block: &[u8]| {
        Cid::new_v1(
            codec,
            Multihash::wrap(0x12, Sha256::digest(block).as_slice()).unwrap(),
        )
    };
    let chunk = b"x".to_vec();
    let chunk_cid = address(0x55, &chunk);
    let file = PbNode {
        links: vec![PbLink {
            cid: chunk_cid,
            name: None,
            size: Some(1),
        }],
        // File, filesize=7, blocksizes=[7]. The linked raw block is one byte.
        data: Some(bytes::Bytes::from_static(&[
            0x08, 0x02, 0x18, 0x07, 0x20, 0x07,
        ])),
    }
    .into_bytes();
    let file_cid = address(0x70, &file);
    let file_tsize = file.len() as u64 + 1;
    let root_block = PbNode {
        links: vec![PbLink {
            cid: file_cid,
            name: Some("invalid".to_owned()),
            size: Some(file_tsize),
        }],
        data: Some(bytes::Bytes::from_static(&[0x08, 0x01])),
    }
    .into_bytes();
    let root = address(0x70, &root_block);
    kubo.client()
        .import_composed(
            &root,
            &[(chunk_cid, chunk), (file_cid, file), (root, root_block)],
        )
        .await
        .unwrap();

    let entries = kubo.client().ls(&format!("/ipfs/{root}")).await.unwrap();
    assert_eq!(entries[0].size, 7);
    assert_eq!(kubo.bytes(&["cat", &format!("/ipfs/{root}/invalid")]), b"x");
    let error = compose(&kubo, &[root.to_string()]).await.unwrap_err();
    assert!(format!("{error:#}").contains("blocksize"), "{error:#}");
}

#[tokio::test]
async fn targeted_mounts_preserve_ancestor_metadata_and_replace_target_metadata() {
    let Some(kubo) = Kubo::start() else { return };
    let base = kubo.add(&[("etc/app/keep", "keep"), ("unrelated/file", "unchanged")]);
    kubo.cli(&["files", "cp", &format!("/ipfs/{base}"), "/head"]);
    // Mutate bottom-up so ancestors have deliberately distinct final metadata.
    for (path, mode, seconds) in [
        ("/head/etc/app", "700", "123"),
        ("/head/etc", "750", "456"),
        ("/head", "755", "789"),
    ] {
        kubo.cli(&["files", "chmod", mode, path]);
        kubo.cli(&["files", "touch", &format!("--mtime={seconds}"), path]);
    }
    let head = kubo.cli(&["files", "stat", "--hash", "/head"]);
    kubo.cli(&["pin", "add", &head]);
    let ancestors = ["", "etc", "etc/app"].map(|path| {
        let cid = if path.is_empty() {
            head.clone()
        } else {
            kubo.cid_at(&head, path)
        };
        (path, kubo.node(&cid).data.unwrap())
    });
    assert_ne!(ancestors[0].1, ancestors[1].1);
    assert_ne!(ancestors[1].1, ancestors[2].1);

    let file = kubo.add_file(b"frozen config", "1", "true");
    let operation = freeze(&kubo, &file, "/etc/app/config").await;
    let root = compose_frozen(&kubo, Some(&head), &[operation])
        .await
        .unwrap();
    for (path, expected) in &ancestors {
        let cid = if path.is_empty() {
            root.clone()
        } else {
            kubo.cid_at(&root, path)
        };
        assert_eq!(kubo.node(&cid).data.as_ref().unwrap(), expected, "{path}");
    }
    assert_eq!(kubo.cid_at(&root, "etc/app/config"), file);
    assert_eq!(
        kubo.cid_at(&root, "unrelated"),
        kubo.cid_at(&head, "unrelated")
    );

    let directory = kubo.add(&[("added", "added")]);
    let operation = freeze(&kubo, &directory, "/etc/app").await;
    let merged = compose_frozen(&kubo, Some(&root), &[operation])
        .await
        .unwrap();
    assert_eq!(kubo.node(&merged).data, Some(ancestors[0].1.clone()));
    assert_eq!(
        kubo.node(&kubo.cid_at(&merged, "etc")).data,
        Some(ancestors[1].1.clone())
    );
    assert_eq!(
        kubo.node(&kubo.cid_at(&merged, "etc/app")).data,
        kubo.node(&directory).data
    );
    assert_eq!(
        kubo.cli(&["cat", &format!("/ipfs/{merged}/etc/app/keep")]),
        "keep"
    );
    assert_eq!(kubo.cid_at(&merged, "etc/app/config"), file);
    assert_eq!(
        kubo.cid_at(&merged, "etc/app/added"),
        kubo.cid_at(&directory, "added")
    );

    let mounts = [freeze(&kubo, &file, "/new/deep/config").await];
    let target_only = compose_frozen(&kubo, None, &mounts).await.unwrap();
    assert_eq!(
        target_only,
        compose_frozen(&kubo, None, &mounts).await.unwrap()
    );
    for path in ["", "new", "new/deep"] {
        let cid = if path.is_empty() {
            target_only.clone()
        } else {
            kubo.cid_at(&target_only, path)
        };
        assert_eq!(kubo.node(&cid).data.unwrap().as_ref(), &[0x08, 0x01]);
    }
    assert_eq!(kubo.cid_at(&target_only, "new/deep/config"), file);
}

#[tokio::test]
async fn targeted_order_replacement_and_explicit_obstruction_removal_match_kubo() {
    let Some(kubo) = Kubo::start() else { return };
    let old = kubo.add_file(b"old", "1", "true");
    let new = kubo.add_file(b"new", "1", "true");
    let directory = kubo.add(&[("keep", "kept")]);
    let root_layer = kubo.add(&[("a", "root wins"), ("outside", "outside")]);

    // A root operation participates at its exact place in the operation stream.
    let mounts = [
        freeze(&kubo, &old, "/a").await,
        freeze(&kubo, &root_layer, "/").await,
        freeze(&kubo, &new, "/a").await,
    ];
    let root = compose_frozen(&kubo, None, &mounts).await.unwrap();
    assert_eq!(kubo.cid_at(&root, "a"), new);
    assert_eq!(
        kubo.cli(&["cat", &format!("/ipfs/{root}/outside")]),
        "outside"
    );
    let mounts = [
        freeze(&kubo, &root_layer, "/").await,
        freeze(&kubo, &new, "/a").await,
        freeze(&kubo, &root_layer, "/").await,
    ];
    let root = compose_frozen(&kubo, None, &mounts).await.unwrap();
    assert_eq!(root, root_layer);

    let blocked = [
        freeze(&kubo, &old, "/a").await,
        freeze(&kubo, &new, "/a/b").await,
    ];
    assert!(compose_frozen(&kubo, None, &blocked).await.is_err());
    let repaired = [
        freeze(&kubo, &old, "/a").await,
        freeze(&kubo, &directory, "/a").await,
        freeze(&kubo, &new, "/a/b").await,
    ];
    let root = compose_frozen(&kubo, None, &repaired).await.unwrap();
    assert_eq!(kubo.cid_at(&root, "a/b"), new);
    assert_eq!(kubo.cli(&["cat", &format!("/ipfs/{root}/a/keep")]), "kept");
    let child_then_parent = [
        freeze(&kubo, &new, "/a/b").await,
        freeze(&kubo, &directory, "/a").await,
    ];
    assert_eq!(
        compose_frozen(&kubo, None, &child_then_parent)
            .await
            .unwrap(),
        root
    );
    let replaced = compose_frozen(&kubo, Some(&root), &[freeze(&kubo, &old, "/a").await])
        .await
        .unwrap();
    assert_eq!(
        kubo.tree(&replaced).await,
        BTreeMap::from([("a".to_owned(), old)])
    );

    let empty = kubo.add(&[]);
    let empty_target = compose_frozen(&kubo, None, &[freeze(&kubo, &empty, "/empty").await])
        .await
        .unwrap();
    assert_eq!(kubo.cid_at(&empty_target, "empty"), empty);
    assert_eq!(
        kubo.tree(&empty_target).await,
        BTreeMap::from([("empty/".to_owned(), "directory".to_owned())])
    );
}

#[cfg(unix)]
#[tokio::test]
async fn targeted_symlink_is_replaced_at_target_but_blocks_descent_and_standalone_mounts() {
    let Some(kubo) = Kubo::start() else { return };
    let directory = tempfile::tempdir().unwrap();
    std::os::unix::fs::symlink("elsewhere", directory.path().join("alias")).unwrap();
    let base = kubo.cli(&[
        "add",
        "-Qr",
        "--cid-version=1",
        directory.path().to_str().unwrap(),
    ]);
    let symlink = kubo.link(&base, "alias").cid;
    let (_cancel_tx, mut cancel) = tokio::sync::watch::channel(false);
    assert!(inspect_source(symlink, &kubo.client(), &mut cancel)
        .await
        .is_err());
    let file = kubo.add_file(b"replacement", "1", "true");
    let blocked = [freeze(&kubo, &file, "/alias/child").await];
    assert!(compose_frozen(&kubo, Some(&base), &blocked).await.is_err());
    let root = compose_frozen(&kubo, Some(&base), &[freeze(&kubo, &file, "/alias").await])
        .await
        .unwrap();
    assert_eq!(kubo.cid_at(&root, "alias"), file);
    assert_eq!(
        kubo.cli(&["cat", &format!("/ipfs/{root}/alias")]),
        "replacement"
    );
    let source = kubo.add(&[("child", "directory replacement")]);
    let root = compose_frozen(
        &kubo,
        Some(&base),
        &[freeze(&kubo, &source, "/alias").await],
    )
    .await
    .unwrap();
    assert_eq!(kubo.cid_at(&root, "alias"), source);
}

#[tokio::test]
async fn standalone_raw_and_chunked_files_keep_exact_cids_and_measured_tsize() {
    let Some(kubo) = Kubo::start() else { return };
    let small = b"standalone raw payload".to_vec();
    // Deliberately repeat a complete chunk: cumulative Tsize counts occurrences,
    // whereas a unique-block sum would undercount this fixture.
    let large: Vec<u8> = (0..(2 * 262_144 + 71))
        .map(|index| (index % 256) as u8)
        .collect();
    for (version, raw, content) in [
        ("1", "true", &small),
        ("1", "true", &large),
        ("0", "false", &large),
    ] {
        let cid: Cid = kubo.add_file(content, version, raw).parse().unwrap();
        assert_eq!(
            cid.version(),
            if version == "0" {
                cid::Version::V0
            } else {
                cid::Version::V1
            }
        );
        assert_eq!(
            cid.codec(),
            if content.len() < 262_144 { 0x55 } else { 0x70 }
        );
        if cid.codec() == 0x70 {
            let node = kubo.node(&cid.to_string());
            assert_eq!(node.links.len(), 3);
            assert_eq!(node.links[0].cid, node.links[1].cid);
        }
        let (_cancel_tx, mut cancel) = tokio::sync::watch::channel(false);
        let source = inspect_source(cid, &kubo.client(), &mut cancel)
            .await
            .unwrap();
        assert_eq!(source.cid(), cid);
        assert_eq!(source.kind(), SourceKind::File);
        let size = kubo.measured_dag_size(cid);
        assert_eq!(source.dag_size(), size);
        if cid.codec() == 0x55 {
            assert_eq!(size, content.len() as u64);
        } else {
            assert!(size > content.len() as u64);
        }
        assert!(FrozenMount::new(source.clone(), GuestTarget::root()).is_err());
        let operation = FrozenMount::new(source, GuestTarget::new("/exact-name").unwrap()).unwrap();
        let root = compose_frozen(&kubo, None, &[operation]).await.unwrap();
        let link = kubo.link(&root, "exact-name");
        assert_eq!(link.cid, cid, "serialized link must preserve CID version");
        assert_eq!(link.size, Some(size));
        assert_eq!(
            kubo.bytes(&["cat", &format!("/ipfs/{root}/exact-name")]),
            *content
        );
        kubo.cli(&["pin", "rm", &cid.to_string()]);
        kubo.cli(&["repo", "gc"]);
        kubo.cli(&["pin", "ls", "--type=recursive", &root]);
        assert_eq!(
            kubo.bytes(&["cat", &format!("/ipfs/{root}/exact-name")]),
            *content
        );
    }
}

#[tokio::test]
async fn targeted_composition_acknowledgements_remain_readable_during_and_after_gc() {
    let Some(kubo) = Kubo::start() else { return };
    let base = kubo.add(&[("keep/file", "head content")]);
    let directory = kubo.add(&[("config", "frozen content")]);
    let mut operations = Vec::new();
    for index in 0..12 {
        let target = format!("/etc/app-{index}");
        operations.push((target.clone(), freeze(&kubo, &directory, &target).await));
    }
    let repo = kubo.repo.path().to_owned();
    let api = kubo.api.clone();
    // Run GC independently of the async test runtime while CAR ingestion and
    // recursive-pin acknowledgement run. The isolated source pins remain held.
    let collector = std::thread::spawn(move || {
        for _ in 0..12 {
            Kubo::checked(
                Kubo::command(&repo)
                    .arg(format!("--api={api}"))
                    .args(["repo", "gc"])
                    .output()
                    .unwrap(),
            );
        }
    });
    let mut results = Vec::new();
    for (target, operation) in operations {
        // A different target requires fresh generated blocks for every import.
        let root = compose_frozen(&kubo, Some(&base), &[operation])
            .await
            .unwrap();
        assert_eq!(
            kubo.cli(&["cat", &format!("/ipfs/{root}{target}/config")]),
            "frozen content"
        );
        results.push((root, target));
    }
    collector.join().unwrap();
    kubo.cli(&["pin", "rm", &base, &directory]);
    kubo.cli(&["repo", "gc"]);
    for (root, target) in results {
        kubo.cli(&["pin", "ls", "--type=recursive", &root]);
        assert_eq!(
            kubo.cli(&["cat", &format!("/ipfs/{root}{target}/config")]),
            "frozen content"
        );
        assert_eq!(
            kubo.cli(&["cat", &format!("/ipfs/{root}/keep/file")]),
            "head content"
        );
    }
}
