// Included in upgrade_cmd::tests to reuse the signed-release and HTTP fixtures.

fn channel_record(
    key: &libp2p::identity::Keypair,
    root: cid::Cid,
    sequence: u64,
    lifetime: i64,
) -> Vec<u8> {
    rust_ipns::Record::new(
        key,
        format!("/ipfs/{root}"),
        chrono::Utc::now() + chrono::Duration::seconds(lifetime),
        sequence,
        Duration::from_secs(300),
    )
    .unwrap()
    .encode()
    .unwrap()
}

async fn channel_select(
    fixture: &Fixture,
    key: &libp2p::identity::Keypair,
    record: Option<Vec<u8>>,
    home: &Path,
    scope: Scope,
) -> (Result<SelectedRelease>, Vec<String>) {
    let name = ww::ipns::canonical_name(key.public().to_peer_id());
    let mut routes = fixture.routes.clone();
    if let Some(record) = record {
        routes.insert(format!("/routing/v1/ipns/{name}"), record);
    }
    let server = Server::start(routes).await;
    let mut options = args(fixture, &server, home);
    options.release = None;
    options.ipns_routing_url = server.endpoint.clone();
    let result = select_upgrade_release(&options, scope, home, Some(&name)).await;
    let requests = server.requests.lock().unwrap().clone();
    (result, requests)
}

#[tokio::test]
async fn channel_advances_and_preserves_independent_floors_across_reopen() {
    let home = tempfile::tempdir().unwrap();
    let key = libp2p::identity::Keypair::generate_ed25519();
    let name = key.public().to_peer_id();
    let first = Fixture::channel(platform().unwrap(), b"first Host", name, 1);
    let second = Fixture::channel(platform().unwrap(), b"second Host", name, 2);
    for (fixture, sequence) in [(&first, 10), (&second, 20)] {
        let (selected, requests) = channel_select(
            fixture,
            &key,
            Some(channel_record(&key, fixture.root, sequence, 3600)),
            home.path(),
            Scope::Metadata,
        )
        .await;
        let selected = selected.unwrap();
        assert_eq!(selected.root, fixture.root);
        assert!(selected.host.is_none());
        assert!(!requests.contains(&format!(
            "cat:/ipfs/{}",
            fixture.descriptor.hosts[0].artifact.root
        )));
    }
    // Each call opens a new Consumer. Neither freshness floor can be bypassed
    // by increasing only the other sequence.
    for (fixture, sequence, expected) in [
        (&first, 10, "IPNS sequence regressed"),
        (&first, 21, "release sequence regressed"),
        (&second, 19, "IPNS sequence regressed"),
    ] {
        let (result, _) = channel_select(
            fixture,
            &key,
            Some(channel_record(&key, fixture.root, sequence, 3600)),
            home.path(),
            Scope::Metadata,
        )
        .await;
        let error = format!("{:#}", result.err().unwrap());
        assert!(error.contains(expected), "{error}");
    }
    // A valid transport refresh can retain the same signed release.
    assert!(channel_select(
        &second,
        &key,
        Some(channel_record(&key, second.root, 22, 3600)),
        home.path(),
        Scope::Host,
    )
    .await
    .0
    .is_ok());
    assert!(!home.path().join(".ww/bin/ww").exists());
}

#[tokio::test]
async fn channel_rejects_conflicting_roots_and_descriptors_at_equal_sequences() {
    let home = tempfile::tempdir().unwrap();
    let key = libp2p::identity::Keypair::generate_ed25519();
    let name = key.public().to_peer_id();
    let first = Fixture::channel(platform().unwrap(), b"first Host", name, 2);
    let conflict = Fixture::channel(platform().unwrap(), b"different Host", name, 2);
    channel_select(
        &first,
        &key,
        Some(channel_record(&key, first.root, 20, 3600)),
        home.path(),
        Scope::Metadata,
    )
    .await
    .0
    .unwrap();
    for (sequence, expected) in [
        (20, "IPNS equivocation"),
        (21, "release sequence equivocation"),
    ] {
        let result = channel_select(
            &conflict,
            &key,
            Some(channel_record(&key, conflict.root, sequence, 3600)),
            home.path(),
            Scope::Metadata,
        )
        .await
        .0;
        let error = format!("{:#}", result.err().unwrap());
        assert!(error.contains(expected), "{error}");
    }
}

#[tokio::test]
async fn explicit_historical_selection_does_not_weaken_channel_freshness() {
    let home = tempfile::tempdir().unwrap();
    let key = libp2p::identity::Keypair::generate_ed25519();
    let name = key.public().to_peer_id();
    let first = Fixture::channel(platform().unwrap(), b"first Host", name, 1);
    let second = Fixture::channel(platform().unwrap(), b"second Host", name, 2);
    channel_select(
        &second,
        &key,
        Some(channel_record(&key, second.root, 20, 3600)),
        home.path(),
        Scope::Metadata,
    )
    .await
    .0
    .unwrap();
    let server = Server::start(first.routes.clone()).await;
    let options = args(&first, &server, home.path());
    // A deliberately invalid configured name must not affect immutable selection.
    let historical =
        select_upgrade_release(&options, Scope::Host, home.path(), Some("invalid-name"))
            .await
            .unwrap();
    assert_eq!(historical.root, first.root);
    assert_eq!(historical.host.as_deref(), Some(&b"first Host"[..]));
    let result = channel_select(
        &first,
        &key,
        Some(channel_record(&key, first.root, 21, 3600)),
        home.path(),
        Scope::Metadata,
    )
    .await
    .0;
    assert!(format!("{:#}", result.err().unwrap()).contains("release sequence regressed"));
}

#[tokio::test]
async fn invalid_channel_records_never_fetch_release_content_or_change_installed_host() {
    let key = libp2p::identity::Keypair::generate_ed25519();
    let fixture = Fixture::channel(
        platform().unwrap(),
        b"candidate Host",
        key.public().to_peer_id(),
        1,
    );
    let wrong = libp2p::identity::Keypair::generate_ed25519();
    for record in [
        None,
        Some(vec![0xff; 32]),
        Some(vec![0; ww::ipns::MAX_RECORD_SIZE + 1]),
        Some(channel_record(&key, fixture.root, 1, -60)),
        Some(channel_record(&wrong, fixture.root, 1, 3600)),
    ] {
        let home = tempfile::tempdir().unwrap();
        installed_fixture(home.path(), &fixture, b"installed Host");
        let (result, requests) =
            channel_select(&fixture, &key, record, home.path(), Scope::Host).await;
        assert!(result.is_err());
        assert!(
            requests
                .iter()
                .all(|request| request.starts_with("/routing/")),
            "{requests:?}"
        );
        assert_eq!(
            std::fs::read(home.path().join(".ww/bin/ww")).unwrap(),
            b"installed Host"
        );
        assert!(!home.path().join(".ww/host-upgrade.json").exists());
    }
}

#[tokio::test]
async fn channel_release_authentication_and_artifact_failures_preserve_installed_host() {
    let key = libp2p::identity::Keypair::generate_ed25519();
    for corrupt_artifact in [false, true] {
        let mut fixture = Fixture::channel(
            platform().unwrap(),
            b"candidate Host",
            key.public().to_peer_id(),
            1,
        );
        let route = if corrupt_artifact {
            format!("cat:/ipfs/{}", fixture.descriptor.hosts[0].artifact.root)
        } else {
            format!("cat:/ipfs/{}/CHECKSUMS.txt.sig", fixture.root)
        };
        if corrupt_artifact {
            fixture.routes.remove(&route);
        } else {
            fixture.routes.get_mut(&route).unwrap()[0] ^= 1;
        }
        let home = tempfile::tempdir().unwrap();
        installed_fixture(home.path(), &fixture, b"installed Host");
        assert!(channel_select(
            &fixture,
            &key,
            Some(channel_record(&key, fixture.root, 1, 3600)),
            home.path(),
            Scope::Host
        )
        .await
        .0
        .is_err());
        assert_eq!(
            std::fs::read(home.path().join(".ww/bin/ww")).unwrap(),
            b"installed Host"
        );
        assert!(!home.path().join(".ww/host-upgrade.json").exists());
    }
}

#[tokio::test]
async fn channel_wrong_platform_fails_before_payload_download() {
    let key = libp2p::identity::Keypair::generate_ed25519();
    let target = if platform().unwrap() == "aarch64-apple-darwin" {
        "x86_64-unknown-linux-gnu"
    } else {
        "aarch64-apple-darwin"
    };
    let fixture = Fixture::channel(target, b"candidate Host", key.public().to_peer_id(), 1);
    let home = tempfile::tempdir().unwrap();
    let (result, requests) = channel_select(
        &fixture,
        &key,
        Some(channel_record(&key, fixture.root, 1, 3600)),
        home.path(),
        Scope::Host,
    )
    .await;
    assert!(result.is_err());
    assert!(!requests.contains(&format!(
        "cat:/ipfs/{}",
        fixture.descriptor.hosts[0].artifact.root
    )));
}

#[tokio::test]
async fn channel_unavailable_routing_preserves_installed_host() {
    let key = libp2p::identity::Keypair::generate_ed25519();
    let name = ww::ipns::canonical_name(key.public().to_peer_id());
    let fixture = Fixture::channel(
        platform().unwrap(),
        b"candidate Host",
        key.public().to_peer_id(),
        1,
    );
    let server = Server::start(fixture.routes.clone()).await;
    let home = tempfile::tempdir().unwrap();
    installed_fixture(home.path(), &fixture, b"installed Host");
    let mut options = args(&fixture, &server, home.path());
    options.release = None;
    // The fixture's default routing URL uses the closed loopback port 1.
    let result = select_upgrade_release(&options, Scope::Host, home.path(), Some(&name)).await;
    assert!(result.is_err());
    assert!(server.requests.lock().unwrap().is_empty());
    assert_eq!(
        std::fs::read(home.path().join(".ww/bin/ww")).unwrap(),
        b"installed Host"
    );
}

#[tokio::test]
async fn upgrade_check_discovers_channel_and_persists_floor_without_replacement() {
    let key = libp2p::identity::Keypair::generate_ed25519();
    let name = ww::ipns::canonical_name(key.public().to_peer_id());
    let fixture = Fixture::channel(
        platform().unwrap(),
        b"authenticated Host",
        key.public().to_peer_id(),
        2,
    );
    let mut routes = fixture.routes.clone();
    routes.insert(
        format!("/routing/v1/ipns/{name}"),
        channel_record(&key, fixture.root, 20, 3600),
    );
    let server = Server::start(routes).await;
    let home = tempfile::tempdir().unwrap();
    installed_fixture(home.path(), &fixture, b"authenticated Host");
    let mut release = args(&fixture, &server, home.path());
    release.release = None;
    release.ipns_routing_url = server.endpoint.clone();
    let options = UpgradeArgs {
        release,
        release_ipns: Some(name),
        rollback: false,
        recover_only: false,
        timeout_secs: None,
        check: true,
        json: false,
        repair_baseline: false,
        acknowledge_missing_rollback: false,
    };
    let report = execute_at(&options, home.path()).await.unwrap();
    assert!(matches!(
        report.outcome,
        CommandOutcome::Native(Outcome::AlreadyInstalled)
    ));
    assert!(report.changed_paths.is_empty());
    assert_eq!(
        std::fs::read(home.path().join(".ww/bin/ww")).unwrap(),
        b"authenticated Host"
    );
    assert!(!home.path().join(".ww/host-upgrade.json").exists());
    assert!(!home.path().join(".ww/bin/.ww-host-upgrade").exists());
    assert!(!server.requests.lock().unwrap().contains(&format!(
        "cat:/ipfs/{}",
        fixture.descriptor.hosts[0].artifact.root
    )));
    let consumer = ww::release::descriptor::Consumer::open(
        &home.path().join(".ww/release-consumer"),
        key.public().to_peer_id(),
        &keyring(&fixture.key),
    )
    .unwrap();
    assert_eq!(consumer.accepted_release_sequence(), Some(2));
    assert_eq!(consumer.accepted_ipns_sequence(), Some(20));
}

#[tokio::test]
async fn upgrade_default_channel_wrong_signer_preserves_installed_host() {
    let key = libp2p::identity::Keypair::generate_ed25519();
    let wrong = libp2p::identity::Keypair::generate_ed25519();
    let name = ww::ipns::canonical_name(key.public().to_peer_id());
    let fixture = Fixture::channel(
        platform().unwrap(),
        b"authenticated Host",
        key.public().to_peer_id(),
        2,
    );
    let mut routes = fixture.routes.clone();
    routes.insert(
        format!("/routing/v1/ipns/{name}"),
        channel_record(&wrong, fixture.root, 20, 3600),
    );
    let server = Server::start(routes).await;
    let home = tempfile::tempdir().unwrap();
    installed_fixture(home.path(), &fixture, b"authenticated Host");
    let mut release = args(&fixture, &server, home.path());
    release.release = None;
    release.ipns_routing_url = server.endpoint.clone();
    let options = UpgradeArgs {
        release,
        release_ipns: Some(name),
        rollback: false,
        recover_only: false,
        timeout_secs: None,
        check: false,
        json: false,
        repair_baseline: false,
        acknowledge_missing_rollback: false,
    };
    let error = execute_at(&options, home.path()).await.err().unwrap();
    assert!(
        format!("{error:#}").contains("authenticate release IPNS record"),
        "{error:#}"
    );
    assert_eq!(
        std::fs::read(home.path().join(".ww/bin/ww")).unwrap(),
        b"authenticated Host"
    );
    assert!(!home.path().join(".ww/host-upgrade.json").exists());
    assert!(!home.path().join(".ww/release-consumer").exists());
}

#[tokio::test]
async fn channel_consumer_directory_refuses_symlinks_and_nonprivate_permissions() {
    use std::os::unix::fs::PermissionsExt;
    let key = libp2p::identity::Keypair::generate_ed25519();
    let fixture = Fixture::channel(
        platform().unwrap(),
        b"candidate Host",
        key.public().to_peer_id(),
        1,
    );
    for symlink in [false, true] {
        let home = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let directory = home.path().join(".ww/release-consumer");
        std::fs::create_dir(home.path().join(".ww")).unwrap();
        if symlink {
            std::os::unix::fs::symlink(outside.path(), &directory).unwrap();
        } else {
            std::fs::create_dir(&directory).unwrap();
            std::fs::set_permissions(&directory, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        let (result, requests) = channel_select(
            &fixture,
            &key,
            Some(channel_record(&key, fixture.root, 1, 3600)),
            home.path(),
            Scope::Host,
        )
        .await;
        let error = format!("{:#}", result.err().unwrap());
        if !symlink {
            assert!(error.contains("must be private"), "{error}");
        }
        assert!(
            requests
                .iter()
                .all(|request| request.starts_with("/routing/")),
            "{requests:?}"
        );
        assert_eq!(std::fs::read_dir(outside.path()).unwrap().count(), 0);
        assert_eq!(std::fs::read_dir(&directory).unwrap().count(), 0);
    }
}
