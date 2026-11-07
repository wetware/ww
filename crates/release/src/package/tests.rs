use super::*;

use base64::{engine::general_purpose::STANDARD, Engine as _};
use ed25519_dalek::{Signer, SigningKey};

#[path = "transport_tests.rs"]
mod transport;

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
        let path = tree.join(crate::descriptor::platform_path(target).unwrap());
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, format!("fixture host {index}")).unwrap();
    }
    let mut builder = Builder::new(&spool).unwrap();
    builder.scan(&tree, "", 0, true).unwrap();
    let descriptor = Descriptor {
        channel: ipfs::ipns::canonical_name(
            libp2p_identity::Keypair::ed25519_from_bytes([42; 32])
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
                let path = crate::descriptor::platform_path(target).unwrap();
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

#[test]
fn package_bytes_match_the_original_qualified_fixture() {
    // Captured by the unchanged original package fixture/exporter at
    // 9aff997fee4514e4a594a5f0f93bd92ae8d9ccb3 before the package extraction.
    let directory = tempfile::tempdir().unwrap();
    let (tree, spool, key, descriptor) = fixture(directory.path());
    let package = finalize(&tree, &spool, &keyring(&key)).unwrap();
    assert_eq!(
        package.root().to_string(),
        "bafybeifyyh6o4ng7sumde5mq5xtx47vi336xriqk6guja4hbd6hwlmbzne"
    );
    assert_eq!(
        descriptor.cid().unwrap().to_string(),
        "bafyreighlxdzzruy7ejfjsojjydtfcywm6d2ctepqyc3irruelj2nvfc7e"
    );
    for (name, digest) in [
        (
            "CHECKSUMS.txt",
            "146d36eb748979e668d9124c42ca971fb3b2866541ce2d5e4eb4d825bd8acfc6",
        ),
        (
            "CHECKSUMS.txt.sig",
            "26041e16e8857f791b4d97ff4fad340c6e26e4d7dbf4dbdbbd5f6a4331526529",
        ),
    ] {
        assert_eq!(
            hex::encode(Sha256::digest(fs::read(tree.join(name)).unwrap())),
            digest
        );
    }
    let mut blocks: Vec<_> = fs::read_dir(spool.join("blocks"))
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .collect();
    blocks.sort();
    assert_eq!(blocks.len(), 27);
    let mut digest = Sha256::new();
    for block in blocks {
        digest.update(block.file_name().unwrap().to_str().unwrap().as_bytes());
        digest.update([0]);
        digest.update(fs::read(block).unwrap());
    }
    assert_eq!(
        hex::encode(digest.finalize()),
        "6fe72b1d8691c6fbfd257164c6ffb3594d52b6bf17fb1bbf9d71aa5ce60e7b52"
    );
    assert_eq!(Package::open(&spool).unwrap().root(), package.root());
}

#[test]
fn graph_byte_budget_is_inclusive_and_shared_blocks_are_charged_once() {
    let directory = tempfile::tempdir().unwrap();
    let mut builder = Builder::new(directory.path()).unwrap();
    let leaf = builder.block(0x55, b"payload").unwrap().cid;
    let mut graph = GraphValidation {
        total: MAX_GRAPH_BYTES - 7,
        ..Default::default()
    };
    graph.walk(directory.path(), leaf, 0).unwrap();
    assert_eq!(graph.total, MAX_GRAPH_BYTES);
    graph.walk(directory.path(), leaf, 1).unwrap();
    assert_eq!(graph.total, MAX_GRAPH_BYTES);
    let other = builder.block(0x55, b"x").unwrap().cid;
    assert!(graph.walk(directory.path(), other, 0).is_err());
}

#[test]
fn graph_block_and_traversal_budgets_fail_before_accepting_excess_work() {
    let directory = tempfile::tempdir().unwrap();
    let mut builder = Builder::new(directory.path()).unwrap();
    let leaf = builder.block(0x55, b"payload").unwrap().cid;
    let mut graph = GraphValidation::default();
    for index in 0..MAX_BLOCKS - 1 {
        graph.visited.insert(Cid::new_v1(
            0x55,
            cid::multihash::Multihash::wrap(0x12, &Sha256::digest(index.to_le_bytes())).unwrap(),
        ));
    }
    graph.walk(directory.path(), leaf, 0).unwrap();
    let other = builder.block(0x55, b"other").unwrap().cid;
    assert!(graph.walk(directory.path(), other, 0).is_err());

    let mut graph = GraphValidation {
        visits: MAX_BLOCKS * 4 - 1,
        ..Default::default()
    };
    graph.walk(directory.path(), leaf, 0).unwrap();
    assert!(graph.walk(directory.path(), leaf, 0).is_err());
}

#[tokio::test]
async fn remote_acquisition_reconstructs_authenticated_graph() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let directory = tempfile::tempdir().unwrap();
    let (tree, spool, key, _) = fixture(directory.path());
    let package = finalize(&tree, &spool, &keyring(&key)).unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        loop {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut request = Vec::new();
            while !request.windows(4).any(|bytes| bytes == b"\r\n\r\n") {
                let mut buffer = [0; 1024];
                let count = stream.read(&mut buffer).await.unwrap();
                assert!(count > 0 && request.len() + count <= 4096);
                request.extend_from_slice(&buffer[..count]);
            }
            let request = std::str::from_utf8(&request).unwrap();
            let path = request.split_whitespace().nth(1).unwrap();
            let cid: Cid = path
                .strip_prefix("/api/v0/block/get?arg=")
                .unwrap()
                .parse()
                .unwrap();
            let bytes = read_block(&spool, &cid).unwrap();
            let header = format!(
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                bytes.len()
            );
            stream.write_all(header.as_bytes()).await.unwrap();
            stream.write_all(&bytes).await.unwrap();
        }
    });
    let client = ipfs::HttpClient::new(format!("http://{address}"));
    let destination = directory.path().join("download");
    let fetched = Package::fetch(&client, package.root(), &destination)
        .await
        .unwrap();
    assert_eq!(fetched.root(), package.root());
    fetched.authenticate(&keyring(&key)).unwrap();
    Package::open(&destination)
        .unwrap()
        .authenticate(&keyring(&key))
        .unwrap();
    server.abort();
}

/// Export deterministic, nonproduction fixtures for the standalone publisher's
/// disposable Kubo qualification. No repository or infrastructure key is read.
#[test]
#[ignore = "writes an explicitly requested local integration fixture"]
fn export_disposable_package_fixture() {
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
    let discovery = libp2p_identity::Keypair::ed25519_from_bytes([42; 32]).unwrap();
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
