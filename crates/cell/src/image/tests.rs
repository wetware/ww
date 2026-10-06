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
                            format!(r#"{{"Root":{{"Cid":{{"/":"{root}"}},"PinErrorMsg":""}}}}"#)
                                .into_bytes()
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
            requests
                .iter()
                .all(|request| matches!(request.endpoint.as_str(), "block/get" | "dag/import")),
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
