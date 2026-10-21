//! Production composition boundary tests. Filesystem semantics live in composer_tests.rs.
use super::codec::{Directory, Link};
use super::*;
use bytes::Bytes;
use cid::multibase::{encode as multibase, Base};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::sync::Notify;

#[derive(Clone, Debug)]
struct Request {
    endpoint: String,
    query: Vec<(String, String)>,
    body: Vec<u8>,
}

impl Request {
    fn argument(&self) -> &str {
        self.query
            .iter()
            .find(|(key, _)| key == "arg")
            .unwrap()
            .1
            .as_str()
    }
}

fn decode_component(encoded: &str) -> String {
    let mut decoded = Vec::new();
    let mut bytes = encoded.bytes();
    while let Some(byte) = bytes.next() {
        match byte {
            b'+' => decoded.push(b' '),
            b'%' => {
                let high = char::from(bytes.next().unwrap()).to_digit(16).unwrap();
                let low = char::from(bytes.next().unwrap()).to_digit(16).unwrap();
                decoded.push((high * 16 + low) as u8);
            }
            byte => decoded.push(byte),
        }
    }
    String::from_utf8(decoded).unwrap()
}

#[derive(Clone, Copy)]
enum ResponseMode {
    Success,
    Resolve,
    Named(&'static str),
    ReadFailure,
    ImportFailure,
    MissingAcknowledgement,
    Stall(&'static str),
}

struct FakeKubo {
    client: ipfs::BootClient,
    requests: Arc<Mutex<Vec<Request>>>,
    stalled: Arc<Notify>,
    server: tokio::task::JoinHandle<()>,
}

impl Drop for FakeKubo {
    fn drop(&mut self) {
        self.server.abort();
    }
}

impl FakeKubo {
    async fn new(blocks: HashMap<Cid, Vec<u8>>, root: Cid, mode: ResponseMode) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let requests = Arc::new(Mutex::new(Vec::new()));
        let recorded = Arc::clone(&requests);
        let stalled = Arc::new(Notify::new());
        let signal = Arc::clone(&stalled);
        let server = tokio::spawn(async move {
            loop {
                let (mut stream, _) = listener.accept().await.unwrap();
                let mut bytes = Vec::new();
                let header_end = loop {
                    let mut buffer = [0; 4096];
                    let count = stream.read(&mut buffer).await.unwrap();
                    if count == 0 {
                        return;
                    }
                    bytes.extend_from_slice(&buffer[..count]);
                    if let Some(end) = bytes.windows(4).position(|part| part == b"\r\n\r\n") {
                        break end + 4;
                    }
                };
                let header = String::from_utf8_lossy(&bytes[..header_end]).into_owned();
                let length = header
                    .lines()
                    .find_map(|line| {
                        let (key, value) = line.split_once(':')?;
                        key.eq_ignore_ascii_case("content-length")
                            .then(|| value.trim().parse::<usize>().unwrap())
                    })
                    .unwrap_or(0);
                while bytes.len() < header_end + length {
                    let mut buffer = [0; 4096];
                    let count = stream.read(&mut buffer).await.unwrap();
                    if count == 0 {
                        return;
                    }
                    bytes.extend_from_slice(&buffer[..count]);
                }
                let target = header
                    .lines()
                    .next()
                    .unwrap()
                    .split_whitespace()
                    .nth(1)
                    .unwrap();
                let (endpoint, query) = target.split_once('?').unwrap_or((target, ""));
                let request = Request {
                    endpoint: endpoint.strip_prefix("/api/v0/").unwrap().to_owned(),
                    query: query
                        .split('&')
                        .filter(|part| !part.is_empty())
                        .map(|part| {
                            let (key, value) = part.split_once('=').unwrap_or((part, ""));
                            (decode_component(key), decode_component(value))
                        })
                        .collect(),
                    body: bytes[header_end..].to_vec(),
                };
                recorded.lock().unwrap().push(request.clone());
                if matches!(mode, ResponseMode::Stall(endpoint) if endpoint == request.endpoint) {
                    signal.notify_one();
                    std::future::pending::<()>().await;
                }
                let failed = matches!(
                    (mode, request.endpoint.as_str()),
                    (ResponseMode::ReadFailure, "block/get")
                        | (ResponseMode::ImportFailure, "dag/import")
                );
                let status = if failed {
                    "500 Internal Server Error"
                } else {
                    "200 OK"
                };
                let body = if failed {
                    b"backend failure".to_vec()
                } else {
                    match request.endpoint.as_str() {
                        "block/get" => blocks
                            .get(&request.argument().parse::<Cid>().unwrap())
                            .unwrap()
                            .clone(),
                        "dag/import" if matches!(mode, ResponseMode::MissingAcknowledgement) => {
                            b"{}".to_vec()
                        }
                        "dag/import" => {
                            let root =
                                if matches!(mode, ResponseMode::Resolve | ResponseMode::Named(_)) {
                                    blocks
                                        .keys()
                                        .copied()
                                        .chain(std::iter::once(root))
                                        .filter_map(|cid| {
                                            let bytes = cid.to_bytes();
                                            request
                                                .body
                                                .windows(bytes.len())
                                                .position(|window| window == bytes)
                                                .map(|index| (index, cid))
                                        })
                                        .min_by_key(|(index, _)| *index)
                                        .unwrap()
                                        .1
                                } else {
                                    root
                                };
                            format!(r#"{{"Root":{{"Cid":{{"/":"{root}"}},"PinErrorMsg":""}}}}"#)
                                .into_bytes()
                        }
                        "pin/add" => b"{}".to_vec(),
                        "name/resolve" if matches!(mode, ResponseMode::Named(_)) => {
                            let ResponseMode::Named(suffix) = mode else {
                                unreachable!()
                            };
                            format!(r#"{{"Path":"/ipfs/{root}{suffix}"}}"#).into_bytes()
                        }
                        endpoint => panic!("composition requested forbidden endpoint {endpoint}"),
                    }
                };
                let header = format!(
                    "HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    body.len()
                );
                if stream.write_all(header.as_bytes()).await.is_ok() {
                    let _ = stream.write_all(&body).await;
                }
            }
        });
        Self {
            client: ipfs::BootClient::one_attempt(
                ipfs::HttpClient::new(format!("http://{address}")),
                Duration::from_secs(2),
            ),
            requests,
            stalled,
            server,
        }
    }

    fn requests(&self) -> Vec<Request> {
        let requests = self.requests.lock().unwrap().clone();
        assert!(
            requests.iter().all(|request| matches!(
                request.endpoint.as_str(),
                "block/get" | "dag/import" | "name/resolve" | "pin/add"
            )),
            "{requests:?}"
        );
        requests
    }
}

fn directory(entries: &[(&str, &[u8])]) -> (Cid, Vec<u8>) {
    let entries = entries
        .iter()
        .map(|(name, bytes)| {
            let hash = cid::multihash::Multihash::<64>::wrap(0x12, &Sha256::digest(bytes)).unwrap();
            (
                (*name).to_owned(),
                Link {
                    cid: Cid::new_v1(0x55, hash),
                    size: bytes.len() as u64,
                },
            )
        })
        .collect::<BTreeMap<_, _>>();
    let (cid, bytes, _) = codec::encode(&Directory {
        data: Bytes::from_static(&[8, 1]),
        entries,
    })
    .unwrap();
    (cid, bytes)
}

struct MergeFixture {
    layers: Vec<String>,
    blocks: HashMap<Cid, Vec<u8>>,
    root: Cid,
    root_bytes: Vec<u8>,
}

fn fixture() -> MergeFixture {
    let (base, base_bytes) = directory(&[("keep", b"base"), ("target", b"old")]);
    let (overlay, overlay_bytes) = directory(&[("target", b"new")]);
    let (root, root_bytes) = directory(&[("keep", b"base"), ("target", b"new")]);
    MergeFixture {
        layers: vec![base.to_string(), overlay.to_string()],
        blocks: HashMap::from([(base, base_bytes), (overlay, overlay_bytes)]),
        root,
        root_bytes,
    }
}

async fn merge(kubo: &FakeKubo, layers: &[String]) -> Result<String> {
    let (_tx, mut cancel) = tokio::sync::watch::channel(false);
    tokio::time::timeout(
        Duration::from_secs(3),
        dag_merge(layers, &kubo.client, &mut cancel),
    )
    .await
    .expect("composition must complete")
}

#[tokio::test]
async fn typed_merge_preserves_cid_version_and_returns_identity() {
    let (v1, block) = directory(&[]);
    let v0 = Cid::new_v0(*v1.hash()).unwrap();
    for root in [v0, v1] {
        let kubo = FakeKubo::new(
            HashMap::from([(root, block.clone())]),
            root,
            ResponseMode::Success,
        )
        .await;
        let (_cancel_tx, mut cancel) = tokio::sync::watch::channel(false);
        assert_eq!(
            dag_merge_cids(&[root], &kubo.client, &mut cancel)
                .await
                .unwrap(),
            root
        );
    }
}

#[tokio::test]
async fn slash_bearing_root_mount_alias_needs_no_path_resolution() {
    let root = Cid::new_v1(
        0x55,
        cid::multihash::Multihash::<64>::wrap(0x12, &[0xff; 32]).unwrap(),
    );
    let alias = multibase(Base::Base64, root.to_bytes());
    assert!(alias.contains('/'));
    assert_eq!(
        crate::mount::parse_immutable_path(&format!("/ipfs/{alias}")).unwrap(),
        (root, Vec::new())
    );
}

#[tokio::test]
async fn invalid_root_cids_fail_before_any_backend_request() {
    let (root, _) = directory(&[]);
    let valid = root.to_string();
    let mut trailing = root.to_bytes();
    trailing.push(0);
    for invalid in [
        String::new(),
        "not-a-cid".to_owned(),
        format!("/ipfs/{valid}"),
        format!("{valid}/child"),
        format!("{valid}?arg=escape"),
        format!("{valid}&arg=escape"),
        format!("{valid}#fragment"),
        multibase(Base::Base16Lower, trailing),
        "f8180808080808080800255000178".to_owned(),
        "f0155008180808080808080800278".to_owned(),
    ] {
        for roots in [
            vec![invalid.clone()],
            vec![valid.clone(), invalid.clone()],
            vec![invalid.clone(), valid.clone()],
        ] {
            let kubo = FakeKubo::new(HashMap::new(), root, ResponseMode::Success).await;
            assert!(merge(&kubo, &roots).await.is_err(), "accepted {roots:?}");
            assert!(kubo.requests().is_empty());
        }
    }
    let kubo = FakeKubo::new(HashMap::new(), root, ResponseMode::Success).await;
    assert!(merge(&kubo, &[]).await.is_err());
    assert!(kubo.requests().is_empty());
}

#[tokio::test]
async fn single_layer_validates_reuses_and_pins_canonical_cid() {
    // Include a valid bare CID containing '/', which is not an IPFS subpath.
    let (v1, bytes) = (0_u32..100)
        .map(|index| directory(&[("file", &index.to_le_bytes())]))
        .find(|(cid, _)| multibase(Base::Base64, cid.to_bytes()).contains('/'))
        .expect("fixture search must produce a slash-containing Base64 CID");
    let v0 = Cid::new_v0(*v1.hash()).unwrap();
    let mut encodings = vec![(v0.to_string(), v0)];
    for base in [
        Base::Base16Lower,
        Base::Base32Upper,
        Base::Base36Lower,
        Base::Base58Btc,
        Base::Base64,
        Base::Base64Pad,
        Base::Base64Url,
    ] {
        encodings.push((multibase(base, v1.to_bytes()), v1));
    }
    for (encoded, canonical) in encodings {
        let kubo = FakeKubo::new(
            HashMap::from([(canonical, bytes.clone())]),
            canonical,
            ResponseMode::Success,
        )
        .await;
        assert_eq!(
            merge(&kubo, &[encoded]).await.unwrap(),
            canonical.to_string()
        );
        let requests = kubo.requests();
        assert_eq!(
            requests
                .iter()
                .map(|r| r.endpoint.as_str())
                .collect::<Vec<_>>(),
            ["block/get", "dag/import"]
        );
        assert_eq!(requests[0].argument(), canonical.to_string());
        assert_eq!(requests[0].query.len(), 1);
        assert!(requests[1]
            .query
            .contains(&("pin-roots".into(), "true".into())));
        // A reused root sends a CAR header without any block records.
        let multipart = &requests[1].body;
        let start = multipart
            .windows(4)
            .position(|part| part == b"\r\n\r\n")
            .unwrap()
            + 4;
        let end = start
            + multipart[start..]
                .windows(4)
                .position(|part| part == b"\r\n--")
                .unwrap();
        let car = &multipart[start..end];
        assert_eq!(car[0] as usize + 1, car.len());
        let cid_bytes = canonical.to_bytes();
        assert!(car.windows(cid_bytes.len()).any(|part| part == cid_bytes));
    }
}

#[tokio::test]
async fn changed_root_reads_blocks_then_imports_and_confirms_final_pin() {
    let fixture = fixture();
    let kubo = FakeKubo::new(fixture.blocks, fixture.root, ResponseMode::Success).await;
    assert_eq!(
        merge(&kubo, &fixture.layers).await.unwrap(),
        fixture.root.to_string()
    );
    let requests = kubo.requests();
    assert_eq!(
        requests
            .iter()
            .map(|r| r.endpoint.as_str())
            .collect::<Vec<_>>(),
        ["block/get", "block/get", "dag/import"]
    );
    assert_eq!(requests[0].argument(), fixture.layers[0]);
    assert_eq!(requests[1].argument(), fixture.layers[1]);
    assert_eq!(
        requests[2].query,
        [
            ("pin-roots".into(), "true".into()),
            ("stats".into(), "false".into()),
            ("silent".into(), "false".into())
        ]
    );
    assert!(requests[2]
        .body
        .windows(fixture.root_bytes.len())
        .any(|bytes| bytes == fixture.root_bytes));
}

#[tokio::test]
async fn backend_failures_and_missing_pin_acknowledgement_never_return_a_root() {
    for mode in [
        ResponseMode::ReadFailure,
        ResponseMode::ImportFailure,
        ResponseMode::MissingAcknowledgement,
    ] {
        let fixture = fixture();
        let kubo = FakeKubo::new(fixture.blocks, fixture.root, mode).await;
        assert!(merge(&kubo, &fixture.layers).await.is_err());
        let requests = kubo.requests();
        if matches!(mode, ResponseMode::ReadFailure) {
            assert_eq!(requests.len(), 1);
        }
        assert!(
            requests
                .iter()
                .filter(|request| request.endpoint == "dag/import")
                .count()
                <= 1
        );
    }
}

#[tokio::test]
async fn reused_root_requires_successful_import_acknowledgement() {
    for mode in [
        ResponseMode::ImportFailure,
        ResponseMode::MissingAcknowledgement,
    ] {
        let (root, bytes) = directory(&[]);
        let kubo = FakeKubo::new(HashMap::from([(root, bytes)]), root, mode).await;
        assert!(merge(&kubo, &[root.to_string()]).await.is_err());
        assert_eq!(kubo.requests().last().unwrap().endpoint, "dag/import");
    }
}

#[tokio::test]
async fn mismatched_block_bytes_fail_before_import_or_pin() {
    let (root, _) = directory(&[("expected", b"bytes")]);
    let (_, wrong_bytes) = directory(&[("different", b"bytes")]);
    let kubo = FakeKubo::new(
        HashMap::from([(root, wrong_bytes)]),
        root,
        ResponseMode::Success,
    )
    .await;
    assert!(merge(&kubo, &[root.to_string()]).await.is_err());
    let requests = kubo.requests();
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].endpoint, "block/get");
}

#[tokio::test]
async fn presignalled_cancellation_makes_no_requests() {
    let fixture = fixture();
    let kubo = FakeKubo::new(fixture.blocks, fixture.root, ResponseMode::Success).await;
    let (_tx, mut cancel) = tokio::sync::watch::channel(true);
    let error = dag_merge(&fixture.layers, &kubo.client, &mut cancel)
        .await
        .unwrap_err();
    assert!(error.to_string().contains("cancelled"));
    assert!(kubo.requests().is_empty());
}

#[tokio::test]
async fn cancellation_interrupts_reads_and_imports_without_success() {
    for endpoint in ["block/get", "dag/import"] {
        let fixture = fixture();
        let kubo = FakeKubo::new(fixture.blocks, fixture.root, ResponseMode::Stall(endpoint)).await;
        let (tx, mut cancel) = tokio::sync::watch::channel(false);
        let operation = dag_merge(&fixture.layers, &kubo.client, &mut cancel);
        tokio::pin!(operation);
        tokio::select! {
            _ = kubo.stalled.notified() => {},
            result = &mut operation => panic!("{endpoint} completed before cancellation: {result:?}"),
        }
        tx.send(true).unwrap();
        let error = tokio::time::timeout(Duration::from_millis(200), operation)
            .await
            .expect("cancellation must interrupt request")
            .unwrap_err();
        assert!(error.to_string().contains("cancelled"));
        assert_eq!(kubo.requests().last().unwrap().endpoint, endpoint);
    }
}

#[tokio::test]
async fn standalone_http_block_limits_are_typed_but_transport_errors_are_not() {
    let oversized = "x".repeat(codec::MAX_BLOCK_BYTES + 1);
    let cid = Cid::new_v1(
        0x55,
        cid::multihash::Multihash::wrap(0x12, &Sha256::digest(oversized.as_bytes())).unwrap(),
    );
    for (response, deterministic) in [
        (format!("HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", oversized.len()), true),
        (format!("HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n{:x}\r\n{oversized}\r\n0\r\n\r\n", oversized.len()), true),
        ("HTTP/1.1 500 Internal Server Error\r\nContent-Length: 5\r\nConnection: close\r\n\r\nerror".into(), false),
        // An oversized backend error page is not evidence of an oversized DAG.
        ("HTTP/1.1 500 Internal Server Error\r\nContent-Length: 70000\r\nConnection: close\r\n\r\n".into(), false),
        ("HTTP/1.1 200 OK\r\nContent-Length: 5\r\nConnection: close\r\n\r\nx".into(), false),
    ] {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut request = [0; 4096];
            assert!(stream.read(&mut request).await.unwrap() > 0);
            // Limit rejection may close the socket before the whole body arrives.
            let _ = stream.write_all(response.as_bytes()).await;
        });
        let client = ipfs::BootClient::one_attempt(
            ipfs::HttpClient::new(format!("http://{address}")),
            Duration::from_secs(5),
        );
        let (_cancel_tx, mut cancel) = tokio::sync::watch::channel(false);
        let error = inspect_source(cid, &client, &mut cancel).await.unwrap_err();
        if deterministic {
            assert!(matches!(error.downcast_ref::<CompositionError>(), Some(CompositionError::ResourceLimit(_))), "{error:#}");
        } else {
            assert!(error.downcast_ref::<CompositionError>().is_none(), "{error:#}");
        }
        server.await.unwrap();
    }
}

async fn resolve_fixture(kubo: &FakeKubo, mounts: &[Mount], head: bool) -> Result<ResolvedMounts> {
    let (_cancel_tx, mut cancel) = tokio::sync::watch::channel(false);
    resolve_fixture_with_cancel(kubo, mounts, head, &mut cancel).await
}

async fn resolve_fixture_with_cancel(
    kubo: &FakeKubo,
    mounts: &[Mount],
    head: bool,
    cancel: &mut tokio::sync::watch::Receiver<bool>,
) -> Result<ResolvedMounts> {
    let (owner, host) = cache::HostFsOwner::start(
        tokio::runtime::Handle::current(),
        cache::HostLimits::default(),
    )
    .unwrap();
    let (scope, scope_owner, finalizer, _completion) = host.try_filesystem_scope().unwrap();
    let transaction = host
        .try_foreground_transaction(host.try_logical_io().unwrap(), &[&scope])
        .unwrap();
    let result =
        resolve_frozen_mounts_with_cancel(mounts, &kubo.client, &transaction, head, cancel).await;
    drop(transaction);
    drop(scope_owner);
    drop(finalizer);
    owner.shutdown().await.unwrap();
    result
}

#[tokio::test]
async fn resolution_retains_parent_and_selected_and_freezes_exact_operations() {
    let bytes = b"selected bytes";
    let (root, block) = directory(&[("child", bytes)]);
    let selected = Cid::new_v1(
        0x55,
        cid::multihash::Multihash::wrap(0x12, &Sha256::digest(bytes)).unwrap(),
    );
    let kubo = FakeKubo::new(
        HashMap::from([(root, block), (selected, bytes.to_vec())]),
        root,
        ResponseMode::Resolve,
    )
    .await;
    let mounts = crate::mount::parse_args(&[
        format!("/ipfs/{root}/child:/etc/a"),
        format!("{selected}:/etc/a"),
    ])
    .unwrap();
    let resolved = resolve_fixture(&kubo, &mounts, false).await.unwrap();
    assert_eq!(resolved.mounts.len(), 2);
    assert!(resolved
        .mounts
        .iter()
        .all(|mount| mount.source().cid() == selected));
    assert_eq!(resolved.mounts[0].target().to_string(), "/etc/a");
    assert_eq!(resolved.retained_roots, [root, selected, selected]);
    let requests = kubo.requests();
    assert_eq!(
        requests[0].endpoint, "pin/add",
        "parent fetched and retained before traversal"
    );
    assert_eq!(
        requests
            .iter()
            .filter(|request| request.endpoint == "dag/import")
            .count(),
        3
    );
    assert!(!requests.iter().any(|request| request.endpoint == "resolve"));
}

#[tokio::test]
async fn resolution_names_resolve_once_and_append_components_structurally() {
    let bytes = b"selected bytes";
    let (child, child_block) = directory(&[("leaf", bytes)]);
    let selected = Cid::new_v1(
        0x55,
        cid::multihash::Multihash::wrap(0x12, &Sha256::digest(bytes)).unwrap(),
    );
    let (root, root_block, _) = super::codec::encode(&Directory {
        data: Bytes::from_static(&[0x08, 0x01]),
        entries: BTreeMap::from([(
            "child".into(),
            Link {
                cid: child,
                size: child_block.len() as u64 + bytes.len() as u64,
            },
        )]),
    })
    .unwrap();
    let kubo = FakeKubo::new(
        HashMap::from([
            (root, root_block),
            (child, child_block),
            (selected, bytes.to_vec()),
        ]),
        root,
        ResponseMode::Named("/child"),
    )
    .await;
    let mounts = crate::mount::parse_args(&["/ipns/example.com/leaf:/etc/file".into()]).unwrap();
    let resolved = resolve_fixture(&kubo, &mounts, false).await.unwrap();
    assert_eq!(resolved.mounts[0].source().cid(), selected);
    let requests = kubo.requests();
    let names = requests
        .iter()
        .filter(|request| request.endpoint == "name/resolve")
        .collect::<Vec<_>>();
    assert_eq!(names.len(), 1);
    assert_eq!(names[0].argument(), "example.com");
}

#[tokio::test]
async fn resolution_rejects_file_ancestors_and_root_files_and_keeps_acknowledged_pins() {
    let bytes = b"file";
    let (root, block) = directory(&[("file", bytes)]);
    let file = Cid::new_v1(
        0x55,
        cid::multihash::Multihash::wrap(0x12, &Sha256::digest(bytes)).unwrap(),
    );
    for argument in [format!("/ipfs/{root}/file/child:/etc"), file.to_string()] {
        let kubo = FakeKubo::new(
            HashMap::from([(root, block.clone()), (file, bytes.to_vec())]),
            root,
            ResponseMode::Resolve,
        )
        .await;
        let mounts = crate::mount::parse_args(&[argument]).unwrap();
        let error = resolve_fixture(&kubo, &mounts, false).await.unwrap_err();
        let error = error.downcast_ref::<MountResolutionFailure>().unwrap();
        assert_eq!(error.retained_roots.len(), 1);
        assert!(!error.uncertain_publication);
        assert!(!kubo
            .requests()
            .iter()
            .any(|request| request.endpoint == "pin/rm"));
    }
}

#[tokio::test]
async fn resolution_rejects_invalid_batches_before_any_request_and_counts_head() {
    let (root, block) = directory(&[]);
    let kubo = FakeKubo::new(HashMap::from([(root, block)]), root, ResponseMode::Resolve).await;
    let mut mounts = crate::mount::parse_args(&vec![root.to_string(); 128]).unwrap();
    assert!(resolve_fixture(&kubo, &mounts, true).await.is_err());
    mounts.truncate(2);
    mounts[1].source = "/ipns/name/../escape".into();
    assert!(resolve_fixture(&kubo, &mounts, false).await.is_err());
    mounts[1].source = "./relative".into();
    assert!(resolve_fixture(&kubo, &mounts, false).await.is_err());
    assert!(kubo.requests().is_empty());
    assert!(resolve_fixture(&kubo, &[], true)
        .await
        .unwrap()
        .mounts
        .is_empty());
}

#[tokio::test]
async fn resolution_lost_retention_acknowledgement_is_uncertain_without_success() {
    let (root, block) = directory(&[]);
    let kubo = FakeKubo::new(
        HashMap::from([(root, block)]),
        root,
        ResponseMode::MissingAcknowledgement,
    )
    .await;
    let mounts = crate::mount::parse_args(&[root.to_string()]).unwrap();
    let error = resolve_fixture(&kubo, &mounts, false).await.unwrap_err();
    let error = error.downcast_ref::<MountResolutionFailure>().unwrap();
    assert!(error.uncertain_publication);
    assert!(error.retained_roots.is_empty());
    assert_eq!(kubo.requests().len(), 2);
}

#[tokio::test]
async fn resolution_captures_entire_local_batch_before_publishing_any_source() {
    let _serial = super::capture::tests::ASYNC_CAPTURE_TEST.lock().await;
    let temp = tempfile::tempdir().unwrap();
    let directory = temp.path().canonicalize().unwrap();
    std::fs::write(directory.join("first"), b"captured").unwrap();
    let mounts = crate::mount::parse_args(&[
        format!("{}:/first", directory.join("first").display()),
        format!("{}:/missing", directory.join("missing").display()),
    ])
    .unwrap();
    let (root, block) = self::directory(&[]);
    let kubo = FakeKubo::new(HashMap::from([(root, block)]), root, ResponseMode::Resolve).await;
    let error = resolve_fixture(&kubo, &mounts, false).await.unwrap_err();
    assert!(
        error
            .chain()
            .any(|cause| cause.downcast_ref::<rustix::io::Errno>()
                == Some(&rustix::io::Errno::NOENT)),
        "{error:#}"
    );
    assert!(kubo.requests().is_empty());
}

#[tokio::test]
async fn resolution_never_follows_immutable_symlinks() {
    let symlink_block = vec![0x0a, 0x08, 0x08, 0x04, 0x12, 0x04, b'f', b'i', b'l', b'e'];
    let symlink = Cid::new_v1(
        0x70,
        cid::multihash::Multihash::wrap(0x12, &Sha256::digest(&symlink_block)).unwrap(),
    );
    let (root, block, _) = super::codec::encode(&Directory {
        data: Bytes::from_static(&[0x08, 0x01]),
        entries: BTreeMap::from([(
            "link".into(),
            Link {
                cid: symlink,
                size: symlink_block.len() as u64,
            },
        )]),
    })
    .unwrap();
    for suffix in ["link", "link/child"] {
        let kubo = FakeKubo::new(
            HashMap::from([(root, block.clone()), (symlink, symlink_block.clone())]),
            root,
            ResponseMode::Resolve,
        )
        .await;
        let mounts = crate::mount::parse_args(&[format!("/ipfs/{root}/{suffix}:/etc")]).unwrap();
        let error = resolve_fixture(&kubo, &mounts, false).await.unwrap_err();
        assert!(format!("{error:#}").contains("symlink"));
        assert_eq!(
            error
                .downcast_ref::<MountResolutionFailure>()
                .unwrap()
                .retained_roots,
            [root]
        );
    }
}

#[tokio::test]
async fn resolution_cancels_pending_name_lookup_without_frozen_operations() {
    let (root, block) = directory(&[]);
    let kubo = FakeKubo::new(
        HashMap::from([(root, block)]),
        root,
        ResponseMode::Stall("name/resolve"),
    )
    .await;
    let mounts = crate::mount::parse_args(&["/ipns/example.com".into()]).unwrap();
    let (tx, mut cancel) = tokio::sync::watch::channel(false);
    let resolution = resolve_fixture_with_cancel(&kubo, &mounts, false, &mut cancel);
    tokio::pin!(resolution);
    tokio::select! {
        result = &mut resolution => panic!("completed before cancellation: {result:?}"),
        _ = kubo.stalled.notified() => {},
    }
    tx.send(true).unwrap();
    let error = tokio::time::timeout(Duration::from_secs(1), resolution)
        .await
        .unwrap()
        .unwrap_err();
    assert!(format!("{error:#}").contains("cancelled"));
    assert!(error
        .downcast_ref::<MountResolutionFailure>()
        .unwrap()
        .retained_roots
        .is_empty());
    assert_eq!(kubo.requests().len(), 1);
}

#[tokio::test]
async fn resolution_rejects_noncanonical_name_results_before_retention() {
    let (root, block) = directory(&[]);
    let kubo = FakeKubo::new(
        HashMap::from([(root, block)]),
        root,
        ResponseMode::Named("/../escape"),
    )
    .await;
    let mounts = crate::mount::parse_args(&["/ipns/example.com".into()]).unwrap();
    assert!(resolve_fixture(&kubo, &mounts, false).await.is_err());
    assert_eq!(kubo.requests().len(), 1);
}
