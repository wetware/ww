//! Authenticated image-update execution against canonical FHS payloads.
use ed25519_dalek::{Signer, SigningKey};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::path::Path;
use std::sync::{Arc, Mutex};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use ww::installation_state::Receipt;
use ww::release::descriptor::{
    platform_path, Artifact, DeploymentArtifact, Descriptor, HostArtifact, Provenance,
    COMPATIBILITY_PROFILE, SOURCE_REPOSITORY,
};

fn cid(codec: u64, bytes: &[u8]) -> cid::Cid {
    cid::Cid::new_v1(
        codec,
        cid::multihash::Multihash::wrap(0x12, &Sha256::digest(bytes)).unwrap(),
    )
}

fn file(path: &str, bytes: &[u8]) -> Artifact {
    Artifact {
        path: path.into(),
        root: cid(0x55, bytes),
        sha256: Sha256::digest(bytes).into(),
        size: bytes.len() as u64,
    }
}

fn keyring(key: &SigningKey) -> Vec<u8> {
    // Release trust uses one base64-encoded Ed25519 SPKI DER key per line.
    let mut der = vec![
        0x30, 0x2a, 0x30, 0x05, 0x06, 0x03, 0x2b, 0x65, 0x70, 0x03, 0x21, 0x00,
    ];
    der.extend_from_slice(key.verifying_key().as_bytes());
    const ALPHABET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut encoded = Vec::new();
    for block in der.chunks(3) {
        let value = (u32::from(block[0]) << 16)
            | (u32::from(*block.get(1).unwrap_or(&0)) << 8)
            | u32::from(*block.get(2).unwrap_or(&0));
        encoded.extend([
            ALPHABET[(value >> 18) as usize],
            ALPHABET[((value >> 12) & 63) as usize],
            if block.len() > 1 {
                ALPHABET[((value >> 6) & 63) as usize]
            } else {
                b'='
            },
            if block.len() > 2 {
                ALPHABET[(value & 63) as usize]
            } else {
                b'='
            },
        ]);
    }
    encoded.push(b'\n');
    encoded
}

struct Fixture {
    key: SigningKey,
    root: cid::Cid,
    routes: BTreeMap<String, Vec<u8>>,
    descriptor: Descriptor,
}

impl Fixture {
    fn new(target: &str, kernel_bytes: &[u8], status_bytes: &[u8]) -> Self {
        let key = SigningKey::from_bytes(&[7; 32]);
        let name = libp2p::identity::Keypair::generate_ed25519()
            .public()
            .to_peer_id();
        let provenance = Provenance {
            repository: SOURCE_REPOSITORY.into(),
            revision: "a".repeat(40),
            builder: "test".into(),
        };
        let host = file(platform_path(target).unwrap(), b"authenticated Host");
        let kernel = file("fhs/boot/kernel.wasm", kernel_bytes);
        let status = file("fhs/bin/status.wasm", status_bytes);
        let descriptor = Descriptor {
            channel: ww::ipns::canonical_name(name),
            sequence: 3,
            version: "0.1.1".into(),
            source: provenance.clone(),
            hosts: vec![HostArtifact {
                target: target.into(),
                artifact: host.clone(),
                provenance,
                compatibility: COMPATIBILITY_PROFILE.into(),
            }],
            deployment: DeploymentArtifact {
                root: cid(0x70, b"FHS graph"),
                kernel: kernel.clone(),
                status: status.clone(),
                compatibility: COMPATIBILITY_PROFILE.into(),
            },
        };
        let binding = descriptor.binding().unwrap();
        let mut manifest = format!(
            "# sha256\n{}  RELEASE-DESCRIPTOR\n",
            hex::encode(Sha256::digest(&binding))
        );
        for artifact in [&host, &kernel, &status] {
            manifest.push_str(&format!(
                "{}  {}\n",
                hex::encode(artifact.sha256),
                artifact.path
            ));
        }
        let manifest = manifest.into_bytes();
        let descriptor_cid = descriptor.cid().unwrap();
        // One plain UnixFS directory link. This is protocol fixture data;
        // production root parsing uses verify_transport_root.
        let mut link = vec![0x0a, descriptor_cid.to_bytes().len() as u8];
        link.extend(descriptor_cid.to_bytes());
        link.extend([0x12, 18]);
        link.extend(b"release-descriptor");
        let mut root_bytes = vec![0x0a, 2, 8, 1, 0x12, link.len() as u8];
        root_bytes.extend(link);
        let root = cid(0x70, &root_bytes);
        let routes = BTreeMap::from([
            (format!("cat:/ipfs/{root}/CHECKSUMS.txt"), manifest.clone()),
            (
                format!("cat:/ipfs/{root}/CHECKSUMS.txt.sig"),
                key.sign(&manifest).to_bytes().to_vec(),
            ),
            (format!("cat:/ipfs/{root}/RELEASE-DESCRIPTOR"), binding),
            (
                format!("block/get:{descriptor_cid}"),
                descriptor.encode().unwrap(),
            ),
            (format!("block/get:{root}"), root_bytes),
            (
                format!("cat:/ipfs/{}", host.root),
                b"authenticated Host".to_vec(),
            ),
            (format!("cat:/ipfs/{}", kernel.root), kernel_bytes.to_vec()),
            (format!("cat:/ipfs/{}", status.root), status_bytes.to_vec()),
        ]);
        Self {
            key,
            root,
            routes,
            descriptor,
        }
    }
}

struct Server {
    endpoint: String,
    requests: Arc<Mutex<Vec<String>>>,
    task: tokio::task::JoinHandle<()>,
}

fn platform() -> &'static str {
    match (std::env::consts::OS, std::env::consts::ARCH) {
        ("macos", "aarch64") => "aarch64-apple-darwin",
        ("macos", "x86_64") => "x86_64-apple-darwin",
        ("linux", "aarch64") => "aarch64-unknown-linux-gnu",
        ("linux", "x86_64") if cfg!(target_env = "musl") => "x86_64-unknown-linux-musl",
        ("linux", "x86_64") => "x86_64-unknown-linux-gnu",
        _ => panic!("unsupported release test platform"),
    }
}

async fn update(home: &Path, server: &Server, fixture: &Fixture, check: bool) -> serde_json::Value {
    let mut command = tokio::process::Command::new(home.join(".ww/bin/ww"));
    for (key, _) in std::env::vars_os() {
        if key.to_string_lossy().starts_with("WW_") || key.to_string_lossy().starts_with("IPFS_") {
            command.env_remove(key);
        }
    }
    command
        .env("HOME", home)
        .args(["image", "update", "--json", "--release"])
        .arg(fixture.root.to_string())
        .arg("--ipfs-url")
        .arg(&server.endpoint)
        .arg("--trusted-release-keys")
        .arg(home.join("release-keys"));
    if check {
        command.arg("--check");
    }
    command.kill_on_drop(true);
    let output = tokio::time::timeout(std::time::Duration::from_secs(90), command.output())
        .await
        .expect("bounded image update")
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).unwrap()
}

#[tokio::test]
async fn signed_image_update_changes_only_baseline_and_retains_complete_recovery() {
    let home = tempfile::tempdir().unwrap();
    let root = home.path().join(".ww");
    let repo = Path::new(env!("CARGO_MANIFEST_DIR"));
    let kernel = std::fs::read(repo.join("std/kernel/bin/main.wasm"))
        .expect("build canonical payloads with make fhs");
    let status = std::fs::read(repo.join("std/status/bin/status.wasm")).unwrap();
    let mut next_status = status.clone();
    // Valid component custom section; the resulting artifact has a distinct identity.
    next_status.extend_from_slice(&[0, 4, 3, b'7', b'4', b'0']);
    let fixture = Fixture::new(platform(), &kernel, &next_status);
    let server = Server::start(fixture.routes.clone()).await;
    std::fs::write(home.path().join("release-keys"), keyring(&fixture.key)).unwrap();
    let software = ["bin/ww", "fhs/boot/kernel.wasm", "fhs/bin/status.wasm"];
    for name in software {
        std::fs::create_dir_all(root.join(name).parent().unwrap()).unwrap();
    }
    std::fs::copy(env!("CARGO_BIN_EXE_ww"), root.join(software[0])).unwrap();
    std::fs::write(root.join(software[1]), &kernel).unwrap();
    std::fs::write(root.join(software[2]), &status).unwrap();
    let private = [
        "identity",
        "etc/ns/ww",
        "ipns-watermark",
        "installed-release.json",
        "data/private",
    ];
    for name in private {
        let path = root.join(name);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, name.as_bytes()).unwrap();
    }
    let receipt = Receipt {
        version: 2,
        complete: true,
        previous: None,
        hashes: std::array::from_fn(|i| {
            if i < 3 {
                blake3::hash(&std::fs::read(root.join(software[i])).unwrap())
                    .to_hex()
                    .to_string()
            } else {
                format!("legacy-provenance-{i}")
            }
        }),
    };
    let original_receipt = receipt.encode().unwrap();
    std::fs::write(root.join("installation.json"), &original_receipt).unwrap();
    let preflight = update(home.path(), &server, &fixture, true).await;
    assert_eq!(preflight["check"], true);
    assert_eq!(preflight["activation"], false);
    assert_eq!(std::fs::read(root.join(software[2])).unwrap(), status);
    assert_eq!(
        std::fs::read(root.join("installation.json")).unwrap(),
        original_receipt
    );
    assert!(!root.join("image-update.json").exists());

    let applied = update(home.path(), &server, &fixture, false).await;
    assert_eq!(applied["activation"], false);
    assert_eq!(applied["release"], fixture.root.to_string());
    assert_eq!(applied["previous_fhs"]["status_blake3"], receipt.hashes[2]);
    assert_eq!(
        applied["fhs"]["status_blake3"],
        blake3::hash(&next_status).to_hex().as_str()
    );
    assert_eq!(std::fs::read(root.join(software[1])).unwrap(), kernel);
    assert_eq!(std::fs::read(root.join(software[2])).unwrap(), next_status);
    let after = Receipt::load(&root).unwrap();
    assert_eq!(after.hashes[0], receipt.hashes[0]);
    assert_eq!(after.hashes[3..], receipt.hashes[3..]);
    assert_eq!(
        blake3::hash(&std::fs::read(root.join(software[0])).unwrap())
            .to_hex()
            .as_str(),
        receipt.hashes[0]
    );
    for name in private {
        assert_eq!(std::fs::read(root.join(name)).unwrap(), name.as_bytes());
    }
    assert!(!root.join("daemon.json").exists());
    assert!(!root.join("image-update.json").exists());
    assert!(root.join("image-update-retained.json").is_file());
    for hash in [
        &receipt.hashes[1],
        &receipt.hashes[2],
        &after.hashes[1],
        &after.hashes[2],
    ] {
        assert!(root.join(".fhs-recovery").join(hash).is_file());
    }
    assert!(!server.requests.lock().unwrap().contains(&format!(
        "cat:/ipfs/{}",
        fixture.descriptor.hosts[0].artifact.root
    )));
    let repeated = update(home.path(), &server, &fixture, false).await;
    assert_eq!(repeated["changed_paths"], serde_json::json!([]));

    // Replay a durable interrupted transaction with unavailable target bytes.
    // Both preflight and execution must identify the prior generation and must
    // recover before requesting the supplied release again.
    std::fs::copy(
        root.join("image-update-retained.json"),
        root.join("image-update.json"),
    )
    .unwrap();
    std::fs::remove_file(root.join(".fhs-recovery").join(&after.hashes[2])).unwrap();
    let requests_before = server.requests.lock().unwrap().len();
    let recovery_check = update(home.path(), &server, &fixture, true).await;
    assert!(recovery_check["release"].is_null());
    assert_eq!(recovery_check["fhs"]["status_blake3"], receipt.hashes[2]);
    assert_eq!(std::fs::read(root.join(software[2])).unwrap(), next_status);
    let recovered = update(home.path(), &server, &fixture, false).await;
    assert!(recovered["release"].is_null());
    assert_eq!(recovered["fhs"]["status_blake3"], receipt.hashes[2]);
    assert_eq!(recovered["recovery"], "restored previous FHS baseline");
    assert_eq!(std::fs::read(root.join(software[2])).unwrap(), status);
    assert_eq!(server.requests.lock().unwrap().len(), requests_before);
}
impl Drop for Server {
    fn drop(&mut self) {
        self.task.abort();
    }
}
impl Server {
    async fn start(routes: BTreeMap<String, Vec<u8>>) -> Self {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = format!("http://{}", listener.local_addr().unwrap());
        let requests = Arc::new(Mutex::new(Vec::new()));
        let recorded = requests.clone();
        let task = tokio::spawn(async move {
            loop {
                let (mut socket, _) = listener.accept().await.unwrap();
                let mut request = Vec::new();
                loop {
                    let mut buffer = [0; 4096];
                    let count = socket.read(&mut buffer).await.unwrap();
                    assert_ne!(count, 0);
                    request.extend_from_slice(&buffer[..count]);
                    if request.windows(4).any(|window| window == b"\r\n\r\n") {
                        break;
                    }
                }
                let request = String::from_utf8(request).unwrap();
                let target = request
                    .lines()
                    .next()
                    .unwrap()
                    .split_whitespace()
                    .nth(1)
                    .unwrap();
                let url = reqwest::Url::parse(&format!("http://localhost{target}")).unwrap();
                let arg = url
                    .query_pairs()
                    .find(|(key, _)| key == "arg")
                    .unwrap()
                    .1
                    .into_owned();
                let key = format!("{}:{arg}", url.path().strip_prefix("/api/v0/").unwrap());
                recorded.lock().unwrap().push(key.clone());
                let (status, body) = match routes.get(&key) {
                    Some(body) => ("200 OK", body.as_slice()),
                    None => ("404 Not Found", &b"missing"[..]),
                };
                socket
                    .write_all(
                        format!(
                            "HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                            body.len()
                        )
                        .as_bytes(),
                    )
                    .await
                    .unwrap();
                socket.write_all(body).await.unwrap();
            }
        });
        Self {
            endpoint,
            requests,
            task,
        }
    }
}
