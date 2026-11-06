use super::*;

#[test]
fn release_sequence_orders_attempts_without_overtaking_a_later_run() {
    assert!(release_sequence(5, 1).unwrap() < release_sequence(5, 2).unwrap());
    assert!(release_sequence(5, 65535).unwrap() < release_sequence(6, 1).unwrap());
    assert_eq!(release_sequence(1, 1).unwrap(), 65537);
}

#[test]
fn release_sequence_counter_exhaustion_fails_closed() {
    for (run, attempt) in [(0, 1), (-1, 1), (1, 0), (1, 65536), (1 << 47, 1)] {
        assert!(release_sequence(run, attempt).is_err(), "{run}, {attempt}");
    }
    assert_eq!(
        release_sequence((1 << 47) - 1, 65535).unwrap(),
        (1 << 63) - 1
    );
}

#[test]
fn release_metadata_preserves_actual_producer_and_rejects_missing_receipt() {
    let work = tempfile::tempdir().unwrap();
    let tree = work.path().join("tree");
    let artifacts = work.path().join("artifacts");
    fs::create_dir(&tree).unwrap();
    fs::write(tree.join("VERSION"), "1.0\n").unwrap();
    fs::write(tree.join("REVISION"), format!("{}\n", "b".repeat(40))).unwrap();
    let mut last_receipt = PathBuf::new();
    for (platform, path) in [
        ("linux-x86_64", "linux/x86_64"),
        ("linux-aarch64", "linux/aarch64"),
        ("macos-aarch64", "macos/aarch64"),
        ("macos-x86_64", "macos/x86_64"),
        ("linux-musl-x86_64", "linux/musl-x86_64"),
    ] {
        let artifact = artifacts.join(format!("ww-{platform}"));
        fs::create_dir_all(&artifact).unwrap();
        let binary = tree.join("bin/ww").join(path).join("ww");
        fs::create_dir_all(binary.parent().unwrap()).unwrap();
        fs::write(&binary, b"native Host").unwrap();
        fs::write(artifact.join("REVISION"), format!("{}\n", "a".repeat(40))).unwrap();
        fs::write(
            artifact.join("SHA256SUMS"),
            format!("{}  ww\n", hex::encode(Sha256::digest(b"native Host"))),
        )
        .unwrap();
        last_receipt = artifact.join("PROVENANCE.json");
        fs::write(
            &last_receipt,
            serde_json::to_vec(&serde_json::json!({
                "repository": "https://github.com/wetware/ww",
                "revision": "a".repeat(40),
                "builder": "original-build",
            }))
            .unwrap(),
        )
        .unwrap();
    }
    let result = metadata(
        &artifacts,
        &tree,
        "fixture".into(),
        1,
        "packaging-build".into(),
    )
    .unwrap();
    assert_eq!(result.source.builder, "packaging-build");
    assert_eq!(result.source.revision, "b".repeat(40));
    assert_eq!(result.hosts.len(), 5);
    assert!(result
        .hosts
        .values()
        .all(|host| { host.builder == "original-build" && host.revision == "a".repeat(40) }));
    fs::remove_file(last_receipt).unwrap();
    let error = metadata(
        &artifacts,
        &tree,
        "fixture".into(),
        1,
        "packaging-build".into(),
    )
    .unwrap_err();
    assert!(error.to_string().contains("invalid producer receipt"));
}

#[tokio::test]
async fn streamed_file_exceeds_one_stage_without_exceeding_stage_memory_bound() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("large");
    let mut file = std::fs::File::create(&path).unwrap();
    use std::io::Write;
    for i in 0u8..72 {
        file.write_all(&vec![i; CHUNK_BYTES]).unwrap();
    }
    let mut builder = Builder::new(&temp.path().join("spool")).unwrap();
    let info = builder.file(&path).unwrap();
    let package = builder.finish(info.link.cid).unwrap();
    assert!(package.stage_count() >= 3);
    for stage in &package.index.stages {
        let charge: usize = stage
            .blocks
            .iter()
            .map(|cid| package.read_block(cid).unwrap().len() + 64)
            .sum();
        assert!(charge <= STAGE_BYTES);
    }
    let mut count = 0;
    package
        .visit_file(info.link.cid, |bytes| {
            count += bytes.len();
            Ok(())
        })
        .unwrap();
    assert_eq!(count, 72 * CHUNK_BYTES);
    struct LoseSecondAck {
        backend: Backend,
        calls: std::sync::atomic::AtomicUsize,
    }
    #[async_trait::async_trait]
    impl Retention for LoseSecondAck {
        fn domain(&self) -> &str {
            "package-test"
        }
        async fn import(&self, root: &Cid, blocks: &[(Cid, Vec<u8>)]) -> Result<()> {
            self.backend.import(root, blocks).await?;
            ensure!(
                self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst) != 1,
                "lost second acknowledgement"
            );
            Ok(())
        }
    }
    let backend = LoseSecondAck {
        backend: Backend::default(),
        calls: std::sync::atomic::AtomicUsize::new(0),
    };
    let journal = tempfile::tempdir().unwrap();
    let mut owner = Owner::open(journal.path(), "package-test").unwrap();
    let generation = owner.begin(package.root(), None).unwrap();
    assert!(package
        .retain(&mut owner, generation, &backend)
        .await
        .is_err());
    assert_eq!(owner.releases().unwrap()[0].stages.len(), 1);
    assert!(owner.releases().unwrap()[0].pending_stage.is_some());
    drop(owner);
    let mut owner = Owner::open(journal.path(), "package-test").unwrap();
    package
        .retain(&mut owner, generation, &backend)
        .await
        .unwrap();
    assert_eq!(
        owner.releases().unwrap()[0].state,
        ipfs::release::ReleaseState::Retained
    );
}

use base64::{engine::general_purpose::STANDARD, Engine as _};
use ed25519_dalek::{Signer, SigningKey};
use std::sync::Mutex;

fn provenance(revision: char) -> Provenance {
    Provenance {
        repository: "https://github.com/wetware/ww".into(),
        revision: revision.to_string().repeat(40),
        builder: "github-actions".into(),
    }
}
fn keyring(key: &SigningKey) -> TrustedKeys {
    let mut der = vec![
        0x30, 0x2a, 0x30, 0x05, 0x06, 0x03, 0x2b, 0x65, 0x70, 0x03, 0x21, 0x00,
    ];
    der.extend_from_slice(key.verifying_key().as_bytes());
    TrustedKeys::from_bytes(format!("{}\n", STANDARD.encode(der)).as_bytes()).unwrap()
}
fn fixture(root: &Path) -> (PathBuf, PathBuf, SigningKey, Descriptor) {
    let tree = root.join("tree");
    let spool = root.join("spool");
    fs::create_dir_all(tree.join("fhs/boot")).unwrap();
    fs::create_dir_all(tree.join("fhs/bin")).unwrap();
    fs::write(tree.join("fhs/boot/kernel.wasm"), b"fixture kernel").unwrap();
    fs::write(tree.join("fhs/bin/status.wasm"), b"fixture status").unwrap();
    fs::write(tree.join("VERSION"), b"0.1.1\n").unwrap();
    fs::write(tree.join("REVISION"), format!("{}\n", "b".repeat(40))).unwrap();
    let targets = [
        "aarch64-apple-darwin",
        "aarch64-unknown-linux-gnu",
        "x86_64-apple-darwin",
        "x86_64-unknown-linux-gnu",
        "x86_64-unknown-linux-musl",
    ];
    for (index, target) in targets.iter().enumerate() {
        let path = tree.join(ww::release::descriptor::platform_path(target).unwrap());
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, format!("fixture host {index}")).unwrap();
    }
    let mut builder = Builder::new(&spool).unwrap();
    builder.scan(&tree, "", 0, true).unwrap();
    let descriptor = Descriptor {
        channel: ww::ipns::canonical_name(
            libp2p::identity::Keypair::ed25519_from_bytes([42; 32])
                .unwrap()
                .public()
                .to_peer_id(),
        ),
        sequence: 12,
        version: "0.1.1".into(),
        source: provenance('b'),
        hosts: targets
            .iter()
            .enumerate()
            .map(|(index, target)| {
                let path = ww::release::descriptor::platform_path(target).unwrap();
                HostArtifact {
                    target: (*target).into(),
                    artifact: builder.files[path].artifact(path),
                    provenance: provenance(if index == 0 { 'a' } else { 'b' }),
                    compatibility: COMPATIBILITY_PROFILE.into(),
                }
            })
            .collect(),
        deployment: DeploymentArtifact {
            root: builder.directories["fhs"].cid,
            kernel: builder.files["fhs/boot/kernel.wasm"].artifact("fhs/boot/kernel.wasm"),
            status: builder.files["fhs/bin/status.wasm"].artifact("fhs/bin/status.wasm"),
            compatibility: COMPATIBILITY_PROFILE.into(),
        },
    };
    fs::write(tree.join(DESCRIPTOR_FILE), descriptor.encode().unwrap()).unwrap();
    fs::write(tree.join(BINDING_PATH), descriptor.binding().unwrap()).unwrap();
    let binding = builder.file(&tree.join(BINDING_PATH)).unwrap();
    builder.files.insert(BINDING_PATH.into(), binding);
    let mut manifest = String::from("# sha256\n");
    for (path, file) in &builder.files {
        manifest.push_str(&format!("{}  {path}\n", hex::encode(file.sha256)));
    }
    let key = SigningKey::from_bytes(&[7; 32]);
    fs::write(tree.join("CHECKSUMS.txt"), &manifest).unwrap();
    fs::write(
        tree.join("CHECKSUMS.txt.sig"),
        key.sign(manifest.as_bytes()).to_bytes(),
    )
    .unwrap();
    (tree, spool, key, descriptor)
}

#[test]
fn signed_multiplatform_envelope_preserves_paths_and_reused_build_provenance() {
    let directory = tempfile::tempdir().unwrap();
    let (tree, spool, key, descriptor) = fixture(directory.path());
    let package = finalize(&tree, &spool, &keyring(&key)).unwrap();
    assert_eq!(
        package.resolve(DESCRIPTOR_LINK_NAME).unwrap(),
        descriptor.cid().unwrap()
    );
    let release = package.authenticate(&keyring(&key)).unwrap();
    assert_eq!(release.descriptor().hosts.len(), 5);
    assert_eq!(
        release.descriptor().hosts[0].provenance.revision,
        "a".repeat(40)
    );
    assert_eq!(release.descriptor().source.revision, "b".repeat(40));
    for host in &descriptor.hosts {
        assert_eq!(
            package.resolve(&host.artifact.path).unwrap(),
            host.artifact.root
        );
        let selection = release.select(&host.target, COMPATIBILITY_PROFILE).unwrap();
        selection
            .verify_host(&package.file_bytes(&host.artifact.path, 1024).unwrap())
            .unwrap();
    }
    let reopened = Package::open(&spool).unwrap();
    assert_eq!(reopened.root(), package.root());
    reopened.authenticate(&keyring(&key)).unwrap();
}

#[test]
fn signed_package_rejects_changed_payload_signature_binding_and_descriptor() {
    for target in [
        "bin/ww/linux/x86_64/ww",
        "CHECKSUMS.txt.sig",
        BINDING_PATH,
        DESCRIPTOR_FILE,
        "VERSION",
    ] {
        let directory = tempfile::tempdir().unwrap();
        let (tree, spool, key, _) = fixture(directory.path());
        let mut data = fs::read(tree.join(target)).unwrap();
        data[0] ^= 1;
        fs::write(tree.join(target), data).unwrap();
        assert!(
            finalize(&tree, &spool, &keyring(&key)).is_err(),
            "accepted changed {target}"
        );
    }
}

#[test]
fn package_rejects_missing_blocks_symlinks_and_spool_inside_tree() {
    let directory = tempfile::tempdir().unwrap();
    let (tree, spool, key, descriptor) = fixture(directory.path());
    let package = finalize(&tree, &spool, &keyring(&key)).unwrap();
    fs::remove_file(
        spool
            .join("blocks")
            .join(descriptor.hosts[0].artifact.root.to_string()),
    )
    .unwrap();
    assert!(package.authenticate(&keyring(&key)).is_err());
    assert!(Package::open(&spool).is_err());
    assert!(finalize(&tree, &tree.join("private-spool"), &keyring(&key)).is_err());
    #[cfg(unix)]
    {
        std::os::unix::fs::symlink(tree.join("VERSION"), tree.join("alias")).unwrap();
        assert!(finalize(&tree, &spool, &keyring(&key)).is_err());
    }
}

#[derive(Default)]
struct Backend(Mutex<BTreeSet<Cid>>);
#[async_trait::async_trait]
impl Retention for Backend {
    fn domain(&self) -> &str {
        "package-test"
    }
    async fn import(&self, root: &Cid, blocks: &[(Cid, Vec<u8>)]) -> Result<()> {
        let supplied: BTreeSet<_> = blocks.iter().map(|(cid, _)| *cid).collect();
        let mut retained = self.0.lock().unwrap();
        for (cid, bytes) in blocks {
            for child in links(cid, bytes)? {
                ensure!(
                    supplied.contains(&child) || retained.contains(&child),
                    "missing retained descendant"
                );
            }
        }
        retained.insert(*root);
        Ok(())
    }
}

#[tokio::test]
async fn staged_multiplatform_release_requires_complete_retention() {
    let directory = tempfile::tempdir().unwrap();
    let (tree, spool, key, _) = fixture(directory.path());
    let package = finalize(&tree, &spool, &keyring(&key)).unwrap();
    let journal = tempfile::tempdir().unwrap();
    let mut owner = Owner::open(journal.path(), "package-test").unwrap();
    let generation = owner.begin(package.root(), None).unwrap();
    assert!(owner.finish_retention(generation).is_err());
    package
        .retain(&mut owner, generation, &Backend::default())
        .await
        .unwrap();
    assert_eq!(
        owner.releases().unwrap()[0].state,
        ipfs::release::ReleaseState::Retained
    );
    package
        .retain(&mut owner, generation, &Backend::default())
        .await
        .unwrap();
    assert!(owner.current().unwrap().is_none());
}

#[tokio::test]
async fn remote_acquisition_reconstructs_authenticated_graph_and_stages() {
    use axum::{
        extract::{Query, State},
        http::StatusCode,
        routing::post,
        Router,
    };
    use std::sync::Arc;
    async fn block(
        State(spool): State<Arc<PathBuf>>,
        Query(query): Query<BTreeMap<String, String>>,
    ) -> (StatusCode, Vec<u8>) {
        match query
            .get("arg")
            .and_then(|s| s.parse::<Cid>().ok())
            .and_then(|cid| read_block(&spool, &cid).ok())
        {
            Some(bytes) => (StatusCode::OK, bytes),
            None => (StatusCode::NOT_FOUND, Vec::new()),
        }
    }
    let directory = tempfile::tempdir().unwrap();
    let (tree, spool, key, _) = fixture(directory.path());
    let package = finalize(&tree, &spool, &keyring(&key)).unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let app = Router::new()
        .route("/api/v0/block/get", post(block))
        .with_state(Arc::new(spool));
    let server = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    let client = ipfs::HttpClient::new(format!("http://{address}"));
    let fetched = Package::fetch(&client, package.root(), &directory.path().join("download"))
        .await
        .unwrap();
    assert_eq!(fetched.root(), package.root());
    assert_eq!(fetched.stage_count(), package.stage_count());
    fetched.authenticate(&keyring(&key)).unwrap();
    server.abort();
}

/// Export deterministic, nonproduction fixtures for the standalone publisher's
/// disposable Kubo qualification. No repository or infrastructure key is read.
#[test]
#[ignore = "writes an explicitly requested local integration fixture"]
fn export_disposable_publisher_fixture() {
    let root = PathBuf::from(
        std::env::var_os("WW_RELEASE_FIXTURE_DIRECTORY").expect("fixture directory required"),
    );
    assert!(!root.exists(), "fixture directory must not exist");
    fs::create_dir_all(&root).unwrap();
    let (tree, spool, key, descriptor) = fixture(&root);
    finalize(&tree, &spool, &keyring(&key)).unwrap();
    let mut public = vec![
        0x30, 0x2a, 0x30, 0x05, 0x06, 0x03, 0x2b, 0x65, 0x70, 0x03, 0x21, 0x00,
    ];
    public.extend_from_slice(key.verifying_key().as_bytes());
    fs::write(
        root.join("trusted-release-keys"),
        format!("{}\n", STANDARD.encode(public)),
    )
    .unwrap();
    let discovery = libp2p::identity::Keypair::ed25519_from_bytes([42; 32]).unwrap();
    fs::write(
        root.join("test-discovery-key.protobuf"),
        discovery.to_protobuf_encoding().unwrap(),
    )
    .unwrap();
    fs::write(
        root.join("metadata.json"),
        serde_json::to_vec_pretty(&Metadata {
            channel: descriptor.channel,
            sequence: descriptor.sequence,
            version: descriptor.version,
            source: descriptor.source,
            hosts: descriptor
                .hosts
                .into_iter()
                .map(|host| (host.target, host.provenance))
                .collect(),
        })
        .unwrap(),
    )
    .unwrap();
}
