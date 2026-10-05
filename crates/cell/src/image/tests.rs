use super::*;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::str::FromStr;
use std::sync::{Arc, Mutex};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

#[derive(Clone, Debug, PartialEq, Eq)]
struct Request {
    endpoint: String,
    query: Vec<(String, String)>,
}

impl Request {
    fn args(&self) -> Vec<&str> {
        self.query
            .iter()
            .filter(|(key, _)| key == "arg")
            .map(|(_, value)| value.as_str())
            .collect()
    }
}

// Decode the HTTP request independently of the client's URL builder. Splitting
// before decoding also exposes injected arguments instead of hiding them.
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

struct FakeKubo {
    client: ipfs::BootClient,
    requests: Arc<Mutex<Vec<Request>>>,
    server: tokio::task::JoinHandle<()>,
}

impl Drop for FakeKubo {
    fn drop(&mut self) {
        self.server.abort();
    }
}

impl FakeKubo {
    async fn new(
        overlays: HashMap<String, Vec<Value>>,
        directories: HashMap<String, Vec<Value>>,
        root_hash: String,
    ) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let requests = Arc::new(Mutex::new(Vec::new()));
        let recorded = Arc::clone(&requests);
        let server = tokio::spawn(async move {
            loop {
                let (mut stream, _) = listener.accept().await.unwrap();
                let mut header = Vec::new();
                while !header.windows(4).any(|window| window == b"\r\n\r\n") {
                    let mut buffer = [0; 1024];
                    let count = stream.read(&mut buffer).await.unwrap();
                    assert_ne!(count, 0, "request ended before HTTP headers");
                    header.extend_from_slice(&buffer[..count]);
                }
                let header = String::from_utf8(header).unwrap();
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
                        .filter(|pair| !pair.is_empty())
                        .map(|pair| {
                            let (key, value) = pair.split_once('=').unwrap_or((pair, ""));
                            (decode_component(key), decode_component(value))
                        })
                        .collect(),
                };
                let path = request.args().first().copied().unwrap_or("");
                let body = match request.endpoint.as_str() {
                    "ls" => json!({"Objects": [{"Links": overlays.get(path).cloned().unwrap_or_default()}]}),
                    "files/ls" => json!({"Entries": directories.get(path).or_else(|| directories.get("*")).cloned().unwrap_or_default()}),
                    "files/stat" => json!({"Hash": root_hash, "Size": 0, "Type": "directory"}),
                    "files/cp" | "files/rm" | "files/mkdir" | "pin/add" => json!({}),
                    endpoint => panic!("unexpected Kubo endpoint: {endpoint}"),
                }.to_string();
                recorded.lock().unwrap().push(request);
                let response = format!("HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len());
                stream.write_all(response.as_bytes()).await.unwrap();
            }
        });
        Self {
            client: ipfs::BootClient::new(ipfs::HttpClient::new(format!("http://{address}")), 0, 1),
            requests,
            server,
        }
    }

    fn requests(&self) -> Vec<Request> {
        self.requests.lock().unwrap().clone()
    }

    fn mutations(&self) -> Vec<Request> {
        self.requests()
            .into_iter()
            .filter(|request| {
                matches!(
                    request.endpoint.as_str(),
                    "files/cp" | "files/rm" | "files/mkdir" | "pin/add"
                )
            })
            .collect()
    }
}

fn cid(label: &str) -> String {
    let hash = cid::multihash::Multihash::<64>::wrap(0x00, label.as_bytes()).unwrap();
    Cid::new_v1(0x55, hash).to_string()
}

fn entry(name: &str, hash: &str, kind: u32) -> Value {
    json!({"Name": name, "Hash": hash, "Size": 0, "Type": kind})
}

async fn merge(kubo: &FakeKubo, overlay: &str) -> Result<()> {
    tokio::time::timeout(
        Duration::from_secs(3),
        merge_overlay_recursive(&kubo.client, "/workspace", overlay),
    )
    .await
    .expect("merge must complete")
}

#[tokio::test]
async fn unsafe_overlay_names_fail_before_any_mutation_in_that_directory() {
    for name in [
        "",
        ".",
        "..",
        "../escape",
        "dir/child",
        "/absolute",
        "a\0b",
        "a\nb",
        "a\rb",
        "a\tb",
        "a\u{7f}b",
    ] {
        for nested in [false, true] {
            let overlay = format!("/ipfs/{}", cid("overlay"));
            let listing_path = if nested {
                format!("{overlay}/directory")
            } else {
                overlay.clone()
            };
            let mut overlays = HashMap::from([(
                listing_path,
                vec![
                    entry("valid-before-hostile", &cid("valid"), 2),
                    entry(name, &cid("hostile"), 2),
                ],
            )]);
            let mut directories = HashMap::new();
            if nested {
                overlays.insert(
                    overlay.clone(),
                    vec![entry("directory", &cid("directory"), 1)],
                );
                directories.insert(
                    "/workspace".into(),
                    vec![entry("directory", &cid("existing"), 1)],
                );
            }
            let kubo = FakeKubo::new(overlays, directories, cid("merged")).await;
            assert!(
                merge(&kubo, &overlay).await.is_err(),
                "accepted {name:?}, nested={nested}"
            );
            assert!(
                kubo.mutations().is_empty(),
                "mutated for {name:?}, nested={nested}: {:?}",
                kubo.requests()
            );
        }
    }
}

#[tokio::test]
async fn malformed_overlay_hashes_fail_before_copy_or_replacement_removal() {
    let valid = cid("valid");
    let mut trailing = Cid::from_str(&valid).unwrap().to_bytes();
    trailing.push(0);
    let trailing = format!(
        "f{}",
        trailing
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>()
    );
    let hashes = [
        trailing,
        String::new(),
        "not-a-cid".into(),
        format!("{valid}/child"),
        format!("/ipfs/{valid}"),
        format!("{valid}?arg=/escape"),
        format!("{valid}&arg=/escape"),
        format!("{valid}#fragment"),
    ];
    for hash in hashes {
        for existing_kind in [None, Some(0), Some(1)] {
            let overlay = format!("/ipfs/{}", cid("overlay"));
            let overlays = HashMap::from([(
                overlay.clone(),
                vec![
                    entry("valid-before-hostile", &valid, 2),
                    entry("target", &hash, 1),
                ],
            )]);
            let directories = existing_kind
                .map(|kind| {
                    HashMap::from([("/workspace".into(), vec![entry("target", &valid, kind)])])
                })
                .unwrap_or_default();
            let kubo = FakeKubo::new(overlays, directories, cid("merged")).await;
            assert!(
                merge(&kubo, &overlay).await.is_err(),
                "accepted {hash:?}, existing={existing_kind:?}"
            );
            assert!(
                kubo.mutations().is_empty(),
                "mutated for {hash:?}: {:?}",
                kubo.requests()
            );
        }
    }
}

#[tokio::test]
async fn nested_malformed_hash_fails_before_replacement_removal() {
    let overlay = format!("/ipfs/{}", cid("overlay"));
    let directory = entry("directory", &cid("directory"), 1);
    let kubo = FakeKubo::new(
        HashMap::from([
            (overlay.clone(), vec![directory.clone()]),
            (
                format!("{overlay}/directory"),
                vec![entry("target", "/ipfs/../../escape", 2)],
            ),
        ]),
        HashMap::from([
            ("/workspace".into(), vec![directory]),
            (
                "/workspace/directory".into(),
                vec![entry("target", &cid("old"), 0)],
            ),
        ]),
        cid("merged"),
    )
    .await;
    assert!(merge(&kubo, &overlay).await.is_err());
    assert!(kubo.mutations().is_empty(), "{:?}", kubo.requests());
}

#[tokio::test]
async fn punctuation_and_unicode_names_survive_recursive_requests_exactly() {
    let overlay = format!("/ipfs/{}", cid("overlay"));
    let directory_name = "dir?arg=escape&recursive=false=#%+ 雪";
    let file_name = "file?arg=escape&arg=other=#%2F%2e%2e+ e\u{301}é\\literal\u{85}";
    let directory = entry(directory_name, &cid("directory"), 1);
    let child_path = format!("/workspace/{directory_name}/{file_name}");
    let child_hash = cid("child");
    let kubo = FakeKubo::new(
        HashMap::from([
            (overlay.clone(), vec![directory.clone()]),
            (
                format!("{overlay}/{directory_name}"),
                vec![entry(file_name, &child_hash.to_uppercase(), 2)],
            ),
        ]),
        HashMap::from([
            ("/workspace".into(), vec![directory]),
            (
                format!("/workspace/{directory_name}"),
                vec![entry(file_name, &cid("old"), 0)],
            ),
        ]),
        cid("merged"),
    )
    .await;
    merge(&kubo, &overlay).await.unwrap();
    let mutations = kubo.mutations();
    assert_eq!(mutations.len(), 2, "{mutations:?}");
    assert_eq!(mutations[0].endpoint, "files/rm");
    assert_eq!(mutations[0].args(), [child_path.as_str()]);
    assert_eq!(
        mutations[0].query,
        [
            ("arg".into(), child_path.clone()),
            ("recursive".into(), "true".into())
        ]
    );
    assert_eq!(mutations[1].endpoint, "files/cp");
    assert_eq!(
        mutations[1].args(),
        [format!("/ipfs/{child_hash}"), child_path]
    );
    assert_eq!(mutations[1].query.len(), 2);
    let requests = kubo.requests();
    let ls = requests
        .iter()
        .filter(|request| request.endpoint == "ls")
        .collect::<Vec<_>>();
    assert_eq!(ls.len(), 2);
    assert_eq!(ls[1].args(), [format!("{overlay}/{directory_name}")]);
    assert_eq!(ls[1].query.len(), 1);
    let nested_listing = requests
        .iter()
        .find(|request| {
            request.endpoint == "files/ls"
                && request.args() == [format!("/workspace/{directory_name}")]
        })
        .unwrap();
    assert!(nested_listing
        .query
        .iter()
        .all(|(key, _)| key == "arg" || key == "long"));
}

#[tokio::test]
async fn recursive_merge_adds_children_and_replaces_both_type_conflicts() {
    let overlay = format!("/ipfs/{}", cid("overlay"));
    let kubo = FakeKubo::new(
        HashMap::from([
            (
                overlay.clone(),
                vec![
                    entry("shared", &cid("directory"), 1),
                    entry("was-file", &cid("new-directory"), 1),
                    entry("was-directory", &cid("new-file"), 2),
                ],
            ),
            (
                format!("{overlay}/shared"),
                vec![entry("added", &cid("added"), 2)],
            ),
        ]),
        HashMap::from([(
            "/workspace".into(),
            vec![
                entry("shared", &cid("old-directory"), 1),
                entry("was-file", &cid("old-file"), 0),
                entry("was-directory", &cid("old-directory"), 1),
            ],
        )]),
        cid("merged"),
    )
    .await;
    merge(&kubo, &overlay).await.unwrap();
    let operations = kubo
        .mutations()
        .into_iter()
        .map(|request| {
            (
                request.endpoint.clone(),
                request
                    .args()
                    .into_iter()
                    .map(str::to_owned)
                    .collect::<Vec<_>>(),
            )
        })
        .collect::<Vec<_>>();
    assert_eq!(
        operations,
        vec![
            (
                "files/cp".into(),
                vec![
                    format!("/ipfs/{}", cid("added")),
                    "/workspace/shared/added".into()
                ]
            ),
            ("files/rm".into(), vec!["/workspace/was-file".into()]),
            (
                "files/cp".into(),
                vec![
                    format!("/ipfs/{}", cid("new-directory")),
                    "/workspace/was-file".into()
                ]
            ),
            ("files/rm".into(), vec!["/workspace/was-directory".into()]),
            (
                "files/cp".into(),
                vec![
                    format!("/ipfs/{}", cid("new-file")),
                    "/workspace/was-directory".into()
                ]
            ),
        ]
    );
}

#[tokio::test]
async fn invalid_root_cids_fail_before_namespace_creation_or_pinning() {
    let valid = cid("valid");
    for invalid in [
        "not-a-cid".to_owned(),
        format!("/ipfs/{valid}"),
        format!("{valid}/child"),
        format!("{valid}?arg=escape"),
    ] {
        for roots in [
            vec![invalid.clone()],
            vec![valid.clone(), invalid.clone()],
            vec![invalid.clone(), valid.clone()],
        ] {
            let kubo = FakeKubo::new(HashMap::new(), HashMap::new(), cid("merged")).await;
            let (_tx, mut cancel) = tokio::sync::watch::channel(false);
            assert!(
                dag_merge(&roots, &kubo.client, &mut cancel).await.is_err(),
                "accepted {roots:?}"
            );
            assert!(
                kubo.requests().is_empty(),
                "requested Kubo for {roots:?}: {:?}",
                kubo.requests()
            );
        }
    }
}

#[tokio::test]
async fn single_layer_pins_and_returns_canonical_cid() {
    let root = cid("root");
    let kubo = FakeKubo::new(HashMap::new(), HashMap::new(), cid("unused")).await;
    let (_tx, mut cancel) = tokio::sync::watch::channel(false);
    assert_eq!(
        dag_merge(&[root.to_uppercase()], &kubo.client, &mut cancel)
            .await
            .unwrap(),
        root
    );
    let requests = kubo.requests();
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].endpoint, "pin/add");
    assert_eq!(requests[0].args(), [root]);
}

// Overflowing varints in the CID version and multihash length, respectively.
const OVERFLOWING_CID_VERSION: &str = "f8180808080808080800255000178";
const OVERFLOWING_MULTIHASH_LENGTH: &str = "f0155008180808080808080800278";

async fn assert_overflowing_root_rejected(hash: &str) {
    for roots in [
        vec![hash.to_owned()],
        vec![cid("base"), hash.to_owned()],
        vec![hash.to_owned(), cid("overlay")],
    ] {
        let kubo = FakeKubo::new(HashMap::new(), HashMap::new(), cid("merged")).await;
        let (_tx, mut cancel) = tokio::sync::watch::channel(false);
        let result = dag_merge(&roots, &kubo.client, &mut cancel).await;
        assert!(
            kubo.requests().is_empty(),
            "malformed root caused Kubo requests: {:?}",
            kubo.requests()
        );
        assert!(result.is_err(), "accepted malformed root: {roots:?}");
    }
}

async fn assert_overflowing_overlay_rejected(hash: &str) {
    let overlay = format!("/ipfs/{}", cid("overlay"));
    let kubo = FakeKubo::new(
        HashMap::from([(overlay.clone(), vec![entry("target", hash, 2)])]),
        HashMap::from([(
            "/workspace".into(),
            vec![entry("target", &cid("existing"), 0)],
        )]),
        cid("merged"),
    )
    .await;
    let result = merge(&kubo, &overlay).await;
    assert!(
        kubo.mutations().is_empty(),
        "malformed overlay caused mutation before rejection: {:?}",
        kubo.mutations()
    );
    assert!(result.is_err(), "accepted malformed overlay hash: {hash}");
}

#[tokio::test]
async fn overflowing_cid_version_root_fails_before_kubo_requests() {
    assert_overflowing_root_rejected(OVERFLOWING_CID_VERSION).await;
}

#[tokio::test]
async fn overflowing_multihash_length_root_fails_before_kubo_requests() {
    assert_overflowing_root_rejected(OVERFLOWING_MULTIHASH_LENGTH).await;
}

#[tokio::test]
async fn overflowing_cid_version_overlay_fails_before_replacement_removal() {
    assert_overflowing_overlay_rejected(OVERFLOWING_CID_VERSION).await;
}

#[tokio::test]
async fn overflowing_multihash_length_overlay_fails_before_replacement_removal() {
    assert_overflowing_overlay_rejected(OVERFLOWING_MULTIHASH_LENGTH).await;
}

#[tokio::test]
async fn merge_accepts_cidv0_and_alternate_cidv1_encodings() {
    use cid::multibase::{encode, Base};

    let hash = cid::multihash::Multihash::<64>::wrap(0x12, &[0xff; 32]).unwrap();
    let v0 = Cid::new_v0(hash).unwrap();
    let v1 = Cid::new_v1(0x70, hash);
    let base64 = encode(Base::Base64, v1.to_bytes());
    assert!(base64.contains('/'), "exercise a slash inside a valid CID");
    let mut encodings = vec![(v0.to_string(), v0.to_string())];
    for base in [
        Base::Base16Lower,
        Base::Base32Upper,
        Base::Base36Lower,
        Base::Base58Btc,
        Base::Base64,
        Base::Base64Pad,
        Base::Base64Url,
    ] {
        encodings.push((encode(base, v1.to_bytes()), v1.to_string()));
    }

    for (encoded, canonical) in encodings {
        let overlay = cid("overlay");
        let kubo = FakeKubo::new(
            HashMap::from([(
                format!("/ipfs/{overlay}"),
                vec![
                    entry("cidv0-file", &v0.to_string(), 2),
                    entry("alternate-file", &encoded, 2),
                ],
            )]),
            HashMap::new(),
            encoded.clone(),
        )
        .await;
        let (_tx, mut cancel) = tokio::sync::watch::channel(false);
        let root = dag_merge(&[encoded.clone(), overlay], &kubo.client, &mut cancel)
            .await
            .unwrap_or_else(|error| panic!("rejected valid CID {encoded:?}: {error:#}"));
        assert_eq!(root, canonical);

        let requests = kubo.requests();
        let sources = requests
            .iter()
            .filter(|request| request.endpoint == "files/cp")
            .map(|request| request.args()[0])
            .collect::<Vec<_>>();
        assert_eq!(
            sources,
            [
                format!("/ipfs/{canonical}"),
                format!("/ipfs/{v0}"),
                format!("/ipfs/{canonical}"),
            ]
        );
        let pin = requests
            .iter()
            .find(|request| request.endpoint == "pin/add")
            .unwrap();
        assert_eq!(pin.args(), [canonical]);
    }
}

#[tokio::test]
async fn merged_root_hash_is_validated_before_pinning() {
    for hash in [
        "not-a-cid".to_owned(),
        format!("/ipfs/{}", cid("merged")),
        format!("{}/child", cid("merged")),
    ] {
        let kubo = FakeKubo::new(HashMap::new(), HashMap::new(), hash.clone()).await;
        let (_tx, mut cancel) = tokio::sync::watch::channel(false);
        assert!(
            dag_merge(&[cid("base"), cid("overlay")], &kubo.client, &mut cancel)
                .await
                .is_err(),
            "accepted {hash:?}"
        );
        assert!(!kubo
            .requests()
            .iter()
            .any(|request| request.endpoint == "pin/add"));
    }
}

#[tokio::test]
async fn layers_apply_left_to_right_and_canonicalize_root_cids() {
    let base = cid("base");
    let first = cid("first");
    let second = cid("second");
    let merged = cid("merged");
    let kubo = FakeKubo::new(
        HashMap::from([
            (
                format!("/ipfs/{first}"),
                vec![entry("target", &cid("first-file"), 2)],
            ),
            (
                format!("/ipfs/{second}"),
                vec![entry("target", &cid("second-file"), 2)],
            ),
        ]),
        HashMap::from([("*".into(), vec![entry("target", &cid("base-file"), 0)])]),
        merged.to_uppercase(),
    )
    .await;
    let (_tx, mut cancel) = tokio::sync::watch::channel(false);
    assert_eq!(
        dag_merge(
            &[
                base.to_uppercase(),
                first.to_uppercase(),
                second.to_uppercase()
            ],
            &kubo.client,
            &mut cancel
        )
        .await
        .unwrap(),
        merged
    );
    let requests = kubo.requests();
    let copies = requests
        .iter()
        .filter(|request| request.endpoint == "files/cp")
        .collect::<Vec<_>>();
    assert_eq!(
        copies
            .iter()
            .map(|request| request.args()[0])
            .collect::<Vec<_>>(),
        [
            format!("/ipfs/{base}"),
            format!("/ipfs/{}", cid("first-file")),
            format!("/ipfs/{}", cid("second-file"))
        ]
    );
    assert_eq!(copies[1].args()[1], copies[2].args()[1]);
    let removals = requests
        .iter()
        .filter(|request| request.endpoint == "files/rm" && request.args() == [copies[1].args()[1]])
        .collect::<Vec<_>>();
    assert_eq!(
        removals.len(),
        2,
        "each later file replaces the earlier entry"
    );
    let pin = requests
        .iter()
        .position(|request| request.endpoint == "pin/add")
        .unwrap();
    assert_eq!(requests[pin].args(), [merged]);
    let cleanup = requests
        .iter()
        .rposition(|request| request.endpoint == "files/rm")
        .unwrap();
    assert!(pin < cleanup);
}
