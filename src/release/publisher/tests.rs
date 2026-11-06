use super::*;
use sha2::{Digest, Sha256};

fn root(bytes: &[u8]) -> Cid {
    Cid::new_v1(
        0x70,
        cid::multihash::Multihash::wrap(0x12, &Sha256::digest(bytes)).unwrap(),
    )
}

#[test]
fn request_binds_id_to_exact_immutable_reference() {
    let request = Request::new(root(b"one"));
    let mut bytes = serde_json::to_vec(&request).unwrap();
    bytes.push(b'\n');
    let parsed = Request::parse(&bytes).unwrap();
    assert_eq!(parsed.root().unwrap(), root(b"one"));
    let mut wrong = request;
    wrong.descriptor = root(b"two").to_string();
    assert!(Request::parse(&serde_json::to_vec(&wrong).unwrap()).is_err());
}

#[test]
fn request_rejects_unknown_fields_versions_and_noncanonical_references() {
    let request = Request::new(root(b"one"));
    let mut value = serde_json::to_value(&request).unwrap();
    value["key"] = "other".into();
    assert!(Request::parse(&serde_json::to_vec(&value).unwrap()).is_err());
    value.as_object_mut().unwrap().remove("key");
    value["version"] = 2.into();
    assert!(Request::parse(&serde_json::to_vec(&value).unwrap()).is_err());
    assert!(Request::parse(&vec![b' '; MAX_REQUEST_BYTES + 1]).is_err());
    assert!(
        Request::parse(br#"{"version":1,"version":1,"request_id":"x","descriptor":"x"}"#).is_err()
    );
}

#[test]
fn committed_request_is_bound_to_channel_generation_and_root() {
    let name = libp2p::identity::Keypair::generate_ed25519()
        .public()
        .to_peer_id();
    let channel = ww::ipns::canonical_name(name);
    let intent = PublicationIntent {
        previous: None,
        next: Publication {
            generation: 1,
            root: root(b"one"),
        },
    };
    let announcement = Announcement {
        version: 1,
        domain: "release-test".into(),
        channel: channel.clone(),
        key: "ww-release".into(),
        publication: intent.next.clone(),
        descriptor: Cid::new_v1(0x71, *root(b"descriptor").hash()),
        release_sequence: 12,
        source_revision: "a".repeat(40),
    };
    let raw = serde_json::to_vec(&announcement).unwrap();
    let decoded = Announcement::decode(&raw).unwrap();
    decoded
        .matches(&intent, "release-test", &channel, "ww-release")
        .unwrap();
    let mut wrong = intent.clone();
    wrong.next.generation += 1;
    assert!(decoded
        .matches(&wrong, "release-test", &channel, "ww-release")
        .is_err());
    assert!(decoded
        .matches(&intent, "another-domain", &channel, "ww-release")
        .is_err());
    assert!(decoded
        .matches(&intent, "release-test", &channel, "other")
        .is_err());
}

#[test]
fn announcement_rejects_corruption_and_unknown_schema() {
    assert!(Announcement::decode(b"{}").is_err());
    assert!(Announcement::decode(&vec![0; ipfs::release::MAX_ANNOUNCEMENT_BYTES + 1]).is_err());
}

#[cfg(unix)]
#[tokio::test]
async fn kubo_unix_transport_checks_identity_before_announcing() {
    let dir = tempfile::tempdir().unwrap();
    let socket = dir.path().join("api.sock");
    let listener = tokio::net::UnixListener::bind(&socket).unwrap();
    let name = libp2p::identity::Keypair::generate_ed25519()
        .public()
        .to_peer_id();
    let channel = ww::ipns::canonical_name(name);
    let candidate = root(b"one");
    let expected_name = name.to_string();
    let server = tokio::spawn(async move {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let mut requests = Vec::new();
        for body in [
            format!(r#"{{"Keys":[{{"Name":"ww-release","Id":"{expected_name}"}}]}}"#),
            format!(r#"{{"Name":"{expected_name}","Value":"/ipfs/{candidate}"}}"#),
        ] {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut raw = Vec::new();
            loop {
                let byte = stream.read_u8().await.unwrap();
                raw.push(byte);
                if raw.ends_with(b"\r\n\r\n") {
                    break;
                }
            }
            requests.push(String::from_utf8(raw).unwrap());
            stream
                .write_all(
                    format!(
                        "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                        body.len()
                    )
                    .as_bytes(),
                )
                .await
                .unwrap();
        }
        requests
    });
    let api = KuboApi::new(
        &format!("/unix{}", socket.display()),
        Duration::from_secs(2),
    )
    .unwrap();
    api.announce("ww-release", &channel, candidate)
        .await
        .unwrap();
    let requests = server.await.unwrap();
    assert!(requests[0].starts_with("POST /api/v0/key/list"));
    assert!(requests[1].starts_with("POST /api/v0/name/publish?"));
    assert!(requests[1].contains("key=ww-release"));
    assert!(requests[1].contains("resolve=true"));
    assert!(!requests[1].contains("sequence="));
}

#[cfg(unix)]
#[tokio::test]
async fn wrong_key_identity_prevents_announcement() {
    let dir = tempfile::tempdir().unwrap();
    let socket = dir.path().join("api.sock");
    let listener = tokio::net::UnixListener::bind(&socket).unwrap();
    let expected = libp2p::identity::Keypair::generate_ed25519()
        .public()
        .to_peer_id();
    let other = libp2p::identity::Keypair::generate_ed25519()
        .public()
        .to_peer_id();
    let server = tokio::spawn(async move {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let (mut stream, _) = listener.accept().await.unwrap();
        let mut raw = Vec::new();
        loop {
            raw.push(stream.read_u8().await.unwrap());
            if raw.ends_with(b"\r\n\r\n") {
                break;
            }
        }
        let body = format!(r#"{{"Keys":[{{"Name":"ww-release","Id":"{other}"}}]}}"#);
        stream
            .write_all(
                format!(
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                )
                .as_bytes(),
            )
            .await
            .unwrap();
        assert!(
            tokio::time::timeout(Duration::from_millis(100), listener.accept())
                .await
                .is_err()
        );
    });
    let api = KuboApi::new(
        &format!("/unix{}", socket.display()),
        Duration::from_secs(2),
    )
    .unwrap();
    assert!(api
        .announce(
            "ww-release",
            &ww::ipns::canonical_name(expected),
            root(b"one")
        )
        .await
        .is_err());
    server.await.unwrap();
}

#[cfg(unix)]
#[tokio::test]
async fn lost_acknowledgement_retries_same_root_without_a_sequence_override() {
    let dir = tempfile::tempdir().unwrap();
    let socket = dir.path().join("api.sock");
    let listener = tokio::net::UnixListener::bind(&socket).unwrap();
    let name = libp2p::identity::Keypair::generate_ed25519()
        .public()
        .to_peer_id();
    let candidate = root(b"one");
    let server = tokio::spawn(async move {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let mut requests = Vec::new();
        for body in [
            Some(format!(
                r#"{{"Keys":[{{"Name":"ww-release","Id":"{name}"}}]}}"#
            )),
            None, // The remote operation can have completed before this EOF.
            Some(format!(
                r#"{{"Keys":[{{"Name":"ww-release","Id":"{name}"}}]}}"#
            )),
            Some(format!(
                r#"{{"Name":"{name}","Value":"/ipfs/{candidate}"}}"#
            )),
        ] {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut raw = Vec::new();
            loop {
                raw.push(stream.read_u8().await.unwrap());
                if raw.ends_with(b"\r\n\r\n") {
                    break;
                }
            }
            requests.push(String::from_utf8(raw).unwrap());
            if let Some(body) = body {
                stream.write_all(format!("HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).as_bytes()).await.unwrap();
            }
        }
        requests
    });
    let api = KuboApi::new(
        &format!("/unix{}", socket.display()),
        Duration::from_secs(2),
    )
    .unwrap();
    let channel = ww::ipns::canonical_name(name);
    assert!(api
        .announce("ww-release", &channel, candidate)
        .await
        .is_err());
    api.announce("ww-release", &channel, candidate)
        .await
        .unwrap();
    let requests = server.await.unwrap();
    assert_eq!(requests[1], requests[3]);
    assert!(!requests[3].contains("sequence="));
}

#[cfg(unix)]
#[tokio::test]
async fn invalid_or_oversized_acknowledgements_remain_uncertain() {
    for bad_body in [
        r#"{"Name":"invalid","Value":"/ipfs/wrong"}"#.to_string(),
        "x".repeat(MAX_RESPONSE_BYTES + 1),
    ] {
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("api.sock");
        let listener = tokio::net::UnixListener::bind(&socket).unwrap();
        let name = libp2p::identity::Keypair::generate_ed25519()
            .public()
            .to_peer_id();
        let server = tokio::spawn(async move {
            use tokio::io::{AsyncReadExt, AsyncWriteExt};
            for body in [
                format!(r#"{{"Keys":[{{"Name":"ww-release","Id":"{name}"}}]}}"#),
                bad_body,
            ] {
                let (mut stream, _) = listener.accept().await.unwrap();
                let mut raw = Vec::new();
                loop {
                    raw.push(stream.read_u8().await.unwrap());
                    if raw.ends_with(b"\r\n\r\n") {
                        break;
                    }
                }
                let _ = stream.write_all(format!("HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).as_bytes()).await;
            }
        });
        let api = KuboApi::new(
            &format!("/unix{}", socket.display()),
            Duration::from_secs(2),
        )
        .unwrap();
        assert!(api
            .announce("ww-release", &ww::ipns::canonical_name(name), root(b"one"))
            .await
            .is_err());
        server.await.unwrap();
    }
}

fn authenticated(name: libp2p::PeerId, sequence: u64) -> AuthenticatedRelease {
    use base64::Engine;
    use ed25519_dalek::{Signer, SigningKey};
    use ww::release::descriptor::{
        Artifact, DeploymentArtifact, Descriptor, HostArtifact, Provenance, COMPATIBILITY_PROFILE,
    };
    let file = |path: &str| Artifact {
        path: path.into(),
        root: Cid::new_v1(0x55, *root(path.as_bytes()).hash()),
        sha256: Sha256::digest(path.as_bytes()).into(),
        size: path.len() as u64,
    };
    let provenance = Provenance {
        repository: "https://github.com/wetware/ww".into(),
        revision: "a".repeat(40),
        builder: "test".into(),
    };
    let descriptor = Descriptor {
        channel: ww::ipns::canonical_name(name),
        sequence,
        version: "test".into(),
        source: provenance.clone(),
        hosts: vec![HostArtifact {
            target: "x86_64-unknown-linux-gnu".into(),
            artifact: file("bin/ww/linux/x86_64/ww"),
            provenance,
            compatibility: COMPATIBILITY_PROFILE.into(),
        }],
        deployment: DeploymentArtifact {
            root: root(b"fhs"),
            kernel: file("fhs/boot/kernel.wasm"),
            status: file("fhs/bin/status.wasm"),
            compatibility: COMPATIBILITY_PROFILE.into(),
        },
    };
    let mut manifest = format!(
        "# sha256\n{}  RELEASE-DESCRIPTOR\n",
        hex::encode(Sha256::digest(descriptor.binding().unwrap()))
    );
    for artifact in descriptor
        .hosts
        .iter()
        .map(|h| &h.artifact)
        .chain([&descriptor.deployment.kernel, &descriptor.deployment.status])
    {
        manifest.push_str(&format!(
            "{}  {}\n",
            hex::encode(artifact.sha256),
            artifact.path
        ));
    }
    let signing = SigningKey::from_bytes(&[7; 32]);
    let mut der = vec![
        0x30, 0x2a, 0x30, 0x05, 0x06, 0x03, 0x2b, 0x65, 0x70, 0x03, 0x21, 0x00,
    ];
    der.extend_from_slice(signing.verifying_key().as_bytes());
    let keys = TrustedKeys::from_bytes(
        format!(
            "{}\n",
            base64::engine::general_purpose::STANDARD.encode(der)
        )
        .as_bytes(),
    )
    .unwrap();
    AuthenticatedRelease::authenticate(
        &keys,
        manifest.as_bytes(),
        &signing.sign(manifest.as_bytes()).to_bytes(),
        &descriptor.binding().unwrap(),
        descriptor.cid().unwrap(),
        &descriptor.encode().unwrap(),
    )
    .unwrap()
}

#[tokio::test]
async fn signed_stale_release_is_rejected_before_any_kubo_operation() {
    let name = libp2p::identity::Keypair::generate_ed25519()
        .public()
        .to_peer_id();
    let config = Config {
        version: 1,
        domain: "release-test".into(),
        channel: ww::ipns::canonical_name(name),
        key: "ww-release".into(),
        journal: "/unused".into(),
        trusted_keys: "/unused-keys".into(),
        operation_timeout_seconds: 1,
    };
    let api = KuboApi::new("http://127.0.0.1:1", Duration::from_secs(1)).unwrap();
    let previous = Announcement {
        version: 1,
        domain: config.domain.clone(),
        channel: config.channel.clone(),
        key: config.key.clone(),
        publication: Publication {
            generation: 2,
            root: root(b"previous"),
        },
        descriptor: Cid::new_v1(0x71, *root(b"old-desc").hash()),
        release_sequence: 8,
        source_revision: "a".repeat(40),
    };
    for sequence in [7, 8] {
        let release = authenticated(name, sequence);
        let intent = PublicationIntent {
            previous: Some(previous.publication.clone()),
            next: Publication {
                generation: 3,
                root: root(b"candidate"),
            },
        };
        let adapter = KuboPublisher {
            api: &api,
            config: &config,
            release: &release,
            root: intent.next.root,
            previous: Some(previous.clone()),
        };
        let error = adapter.prepare(&intent).await.unwrap_err();
        assert!(
            error.to_string().contains("release sequence must advance"),
            "{error:#}"
        );
    }
}
