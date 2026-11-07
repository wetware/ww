use super::*;
use chrono::Duration;
use ed25519_dalek::{Signer, SigningKey};
use ipld_dagpb::PbLink;
use libp2p_identity::Keypair;
use rust_ipns::Record;

const TARGET: &str = "x86_64-unknown-linux-gnu";
const OTHER_TARGET: &str = "aarch64-apple-darwin";
const HOST: &[u8] = b"native Host executable";
const KERNEL: &[u8] = b"kernel component";
const STATUS: &[u8] = b"status component";

fn block(codec: u64, bytes: &[u8]) -> Cid {
    Cid::new_v1(
        codec,
        Multihash::wrap(0x12, &Sha256::digest(bytes)).unwrap(),
    )
}
fn file(path: &str, bytes: &[u8]) -> Artifact {
    Artifact {
        path: path.into(),
        root: block(0x55, bytes),
        sha256: Sha256::digest(bytes).into(),
        size: bytes.len() as u64,
    }
}
fn provenance(revision: char) -> Provenance {
    Provenance {
        repository: SOURCE_REPOSITORY.into(),
        revision: revision.to_string().repeat(40),
        builder: "github-actions".into(),
    }
}
fn descriptor(name: PeerId, sequence: u64) -> Descriptor {
    let kernel = file("fhs/boot/kernel.wasm", KERNEL);
    let status = file("fhs/bin/status.wasm", STATUS);
    Descriptor {
        channel: canonical_name(name),
        sequence,
        version: "0.1.1".into(),
        source: provenance('b'),
        hosts: vec![
            HostArtifact {
                target: OTHER_TARGET.into(),
                artifact: file(platform_path(OTHER_TARGET).unwrap(), HOST),
                provenance: provenance('a'),
                compatibility: COMPATIBILITY_PROFILE.into(),
            },
            HostArtifact {
                target: TARGET.into(),
                artifact: file(platform_path(TARGET).unwrap(), HOST),
                provenance: provenance('b'),
                compatibility: COMPATIBILITY_PROFILE.into(),
            },
        ],
        deployment: DeploymentArtifact {
            root: block(0x70, b"FHS graph"),
            kernel,
            status,
            compatibility: COMPATIBILITY_PROFILE.into(),
        },
    }
}
fn keyring(key: &SigningKey) -> Vec<u8> {
    let mut der = vec![
        0x30, 0x2a, 0x30, 0x05, 0x06, 0x03, 0x2b, 0x65, 0x70, 0x03, 0x21, 0x00,
    ];
    der.extend_from_slice(key.verifying_key().as_bytes());
    format!("{}\n", STANDARD.encode(der)).into_bytes()
}
fn manifest(descriptor: &Descriptor) -> Vec<u8> {
    let mut hashes = BTreeMap::new();
    hashes.insert(
        BINDING_PATH.to_owned(),
        Sha256::digest(descriptor.binding().unwrap()).into(),
    );
    for artifact in descriptor
        .hosts
        .iter()
        .map(|host| &host.artifact)
        .chain([&descriptor.deployment.kernel, &descriptor.deployment.status])
    {
        hashes.insert(artifact.path.clone(), artifact.sha256);
    }
    let mut output = "# sha256\n".to_string();
    for (path, digest) in hashes {
        output.push_str(&format!("{}  {path}\n", hex::encode(digest)));
    }
    output.into_bytes()
}
fn authenticate(descriptor: &Descriptor, key: &SigningKey) -> AuthenticatedRelease {
    let manifest = manifest(descriptor);
    AuthenticatedRelease::authenticate(
        &TrustedKeys::from_bytes(&keyring(key)).unwrap(),
        &manifest,
        &key.sign(&manifest).to_bytes(),
        &descriptor.binding().unwrap(),
        descriptor.cid().unwrap(),
        &descriptor.encode().unwrap(),
    )
    .unwrap()
}
fn envelope(descriptor: Cid) -> (Cid, Vec<u8>) {
    let bytes = PbNode {
        data: Some(Bytes::from_static(&[0x08, 0x01])),
        links: vec![PbLink {
            cid: descriptor,
            name: Some(DESCRIPTOR_LINK_NAME.into()),
            size: Some(1),
        }],
    }
    .into_bytes();
    (block(0x70, &bytes), bytes.to_vec())
}
fn signed(key: &Keypair, root: Cid, sequence: u64, lifetime: i64) -> Vec<u8> {
    Record::new(
        key,
        format!("/ipfs/{root}"),
        Utc::now() + Duration::seconds(lifetime),
        sequence,
        std::time::Duration::from_secs(5 * 60),
    )
    .unwrap()
    .encode()
    .unwrap()
}
struct Fixture {
    name_key: Keypair,
    release_key: SigningKey,
    directory: tempfile::TempDir,
}
impl Fixture {
    fn new() -> Self {
        Self {
            name_key: Keypair::generate_ed25519(),
            release_key: SigningKey::from_bytes(&[3; 32]),
            directory: tempfile::tempdir().unwrap(),
        }
    }
    fn name(&self) -> PeerId {
        self.name_key.public().to_peer_id()
    }
    fn consumer(&self) -> Result<Consumer> {
        Consumer::open(
            self.directory.path(),
            self.name(),
            &keyring(&self.release_key),
        )
    }
    fn release(&self, sequence: u64) -> AuthenticatedRelease {
        authenticate(&descriptor(self.name(), sequence), &self.release_key)
    }
    fn accept(
        &self,
        consumer: &mut Consumer,
        release_seq: u64,
        ipns_seq: u64,
    ) -> Result<Selection> {
        let release = self.release(release_seq);
        let (root, bytes) = envelope(release.descriptor_cid());
        consumer.accept(
            &signed(&self.name_key, root, ipns_seq, 3600),
            &bytes,
            &release,
            TARGET,
            COMPATIBILITY_PROFILE,
        )
    }
}

#[test]
fn canonical_multiplatform_descriptor_preserves_actual_build_provenance() {
    let fixture = Fixture::new();
    let descriptor = descriptor(fixture.name(), 7);
    let bytes = descriptor.encode().unwrap();
    let decoded = Descriptor::decode(descriptor.cid().unwrap(), &bytes).unwrap();
    assert_eq!(decoded.hosts.len(), 2);
    assert_eq!(decoded.hosts[0].provenance.revision, "a".repeat(40));
    assert_eq!(decoded.source.revision, "b".repeat(40));
    assert_eq!(decoded.encode().unwrap(), bytes);
    assert_eq!(
        binding_cid(&descriptor.binding().unwrap()).unwrap(),
        descriptor.cid().unwrap()
    );
    let map = Map::decode(&bytes).unwrap();
    let Value::Map(deployment) = map.get("deployment").unwrap() else {
        panic!("deployment")
    };
    assert!(matches!(deployment.get("root"), Some(Value::Link(_))));
    let release = authenticate(&descriptor, &fixture.release_key);
    for target in [TARGET, OTHER_TARGET] {
        let selection = release.select(target, COMPATIBILITY_PROFILE).unwrap();
        selection.verify_host(HOST).unwrap();
        selection.verify_deployment(KERNEL, STATUS).unwrap();
    }
    assert!(release.select(TARGET, "unknown-profile").is_err());
    assert!(release
        .select("unknown-target", COMPATIBILITY_PROFILE)
        .is_err());
}

#[test]
fn malformed_binding_aliases_and_unsupported_cids_fail() {
    let fixture = Fixture::new();
    let descriptor = descriptor(fixture.name(), 1);
    let binding = descriptor.binding().unwrap();
    for bad in [
        binding[..binding.len() - 1].to_vec(),
        [binding.as_slice(), b"\n"].concat(),
        [b" ", binding.as_slice()].concat(),
        format!("/ipfs/{}\n", descriptor.cid().unwrap()).into_bytes(),
        format!(
            "{}\n",
            descriptor
                .cid()
                .unwrap()
                .to_string_of_base(cid::multibase::Base::Base32Upper)
                .unwrap()
        )
        .into_bytes(),
        format!("{}\n", block(0x55, b"raw")).into_bytes(),
        vec![b'x'; MAX_BINDING_BYTES + 1],
    ] {
        assert!(
            binding_cid(&bad).is_err(),
            "accepted malformed binding: {bad:?}"
        );
    }
}

#[test]
fn schema_unknown_fields_wrong_types_and_bytes_fail() {
    let fixture = Fixture::new();
    let descriptor = descriptor(fixture.name(), 1);
    let original = descriptor.encode().unwrap();
    let map = Map::decode(&original).unwrap();
    for bad in [
        map.insert("schema", Value::Integer(2)).unwrap(),
        map.insert("sequence", Value::Integer(-1)).unwrap(),
        map.insert("hosts", Value::Text("bad".into())).unwrap(),
        map.insert("unexpected", Value::Null).unwrap(),
        map.remove("channel").unwrap(),
    ] {
        let bytes = bad.encode().unwrap();
        assert!(Descriptor::decode(descriptor_cid(&bytes), &bytes).is_err());
    }
    let mut mutated = original.clone();
    *mutated.last_mut().unwrap() ^= 1;
    assert!(Descriptor::decode(descriptor.cid().unwrap(), &mutated).is_err());
    assert!(Descriptor::decode(block(0x55, &original), &original).is_err());
    assert!(Descriptor::decode(
        descriptor.cid().unwrap(),
        &vec![0; ipfs::map::MAX_BLOCK_BYTES + 1]
    )
    .is_err());
}

#[test]
fn invalid_platform_artifact_provenance_and_compatibility_fail() {
    let fixture = Fixture::new();
    let base = descriptor(fixture.name(), 1);
    let mut variants = vec![];
    let mut d = base.clone();
    d.hosts.push(d.hosts[0].clone());
    variants.push(d);
    let mut d = base.clone();
    d.hosts[0].target = "unsupported".into();
    variants.push(d);
    let mut d = base.clone();
    d.hosts[0].artifact.path = "../ww".into();
    variants.push(d);
    let mut d = base.clone();
    d.hosts[0].artifact.sha256[0] ^= 1;
    variants.push(d);
    let mut d = base.clone();
    d.hosts[0].artifact.size = 0;
    variants.push(d);
    let mut d = base.clone();
    d.hosts[0].provenance.revision = "missing".into();
    variants.push(d);
    let mut d = base.clone();
    d.source.repository = "https://attacker.invalid/ww".into();
    variants.push(d);
    let mut d = base.clone();
    d.source.builder = "".into();
    variants.push(d);
    let mut d = base.clone();
    d.deployment.compatibility = "unknown".into();
    variants.push(d);
    let mut d = base.clone();
    d.hosts[0].compatibility = "unknown".into();
    variants.push(d);
    let mut d = base.clone();
    d.sequence = 0;
    variants.push(d);
    let mut d = base.clone();
    d.sequence = u64::MAX;
    variants.push(d);
    let mut d = base.clone();
    d.channel = format!("/ipns/{}", d.channel);
    variants.push(d);
    let mut d = base.clone();
    d.hosts.clear();
    variants.push(d);
    for d in variants {
        assert!(d.encode().is_err(), "accepted {d:?}");
    }
}

#[test]
fn manifest_signature_binding_and_artifact_tampering_fail() {
    let fixture = Fixture::new();
    let d = descriptor(fixture.name(), 1);
    let bytes = d.encode().unwrap();
    let binding = d.binding().unwrap();
    let manifest = manifest(&d);
    let signature = fixture.release_key.sign(&manifest).to_bytes();
    let keys = TrustedKeys::from_bytes(&keyring(&fixture.release_key)).unwrap();
    for signature in [
        signature[..63].to_vec(),
        vec![0; 64],
        SigningKey::from_bytes(&[9; 32])
            .sign(&manifest)
            .to_bytes()
            .to_vec(),
    ] {
        assert!(AuthenticatedRelease::authenticate(
            &keys,
            &manifest,
            &signature,
            &binding,
            d.cid().unwrap(),
            &bytes
        )
        .is_err());
    }
    let mut wrong_binding = binding.clone();
    wrong_binding[5] ^= 1;
    assert!(AuthenticatedRelease::authenticate(
        &keys,
        &manifest,
        &signature,
        &wrong_binding,
        d.cid().unwrap(),
        &bytes
    )
    .is_err());
    let mut wrong_manifest = manifest.clone();
    wrong_manifest[10] ^= 1;
    let wrong_signature = fixture.release_key.sign(&wrong_manifest).to_bytes();
    assert!(AuthenticatedRelease::authenticate(
        &keys,
        &wrong_manifest,
        &wrong_signature,
        &binding,
        d.cid().unwrap(),
        &bytes
    )
    .is_err());
    let selection = authenticate(&d, &fixture.release_key)
        .select(TARGET, COMPATIBILITY_PROFILE)
        .unwrap();
    assert!(selection.verify_host(b"host").is_err());
    let mut corrupt = HOST.to_vec();
    corrupt[0] ^= 1;
    assert!(selection.verify_host(&corrupt).is_err());
    assert!(selection.verify_deployment(STATUS, KERNEL).is_err());
}

#[test]
fn transport_root_requires_exact_authenticated_descriptor_link() {
    let fixture = Fixture::new();
    let d = descriptor(fixture.name(), 1);
    let (root, bytes) = envelope(d.cid().unwrap());
    verify_transport_root(root, &bytes, d.cid().unwrap()).unwrap();
    assert!(
        verify_transport_root(root, &bytes, descriptor(fixture.name(), 2).cid().unwrap()).is_err()
    );
    assert!(verify_transport_root(root, b"corrupt", d.cid().unwrap()).is_err());
    let empty = PbNode {
        data: Some(Bytes::from_static(&[8, 1])),
        links: vec![],
    }
    .into_bytes();
    assert!(verify_transport_root(block(0x70, &empty), &empty, d.cid().unwrap()).is_err());
    let duplicate = PbNode {
        data: Some(Bytes::from_static(&[8, 1])),
        links: vec![
            PbLink {
                cid: d.cid().unwrap(),
                name: Some(DESCRIPTOR_LINK_NAME.into()),
                size: Some(1),
            },
            PbLink {
                cid: d.cid().unwrap(),
                name: Some(DESCRIPTOR_LINK_NAME.into()),
                size: Some(1),
            },
        ],
    }
    .into_bytes();
    assert!(verify_transport_root(block(0x70, &duplicate), &duplicate, d.cid().unwrap()).is_err());
}

#[test]
fn delayed_older_observation_cannot_regress_either_floor_across_restart() {
    let fixture = Fixture::new();
    let mut consumer = fixture.consumer().unwrap();
    fixture.accept(&mut consumer, 1, 10).unwrap();
    fixture.accept(&mut consumer, 2, 20).unwrap();
    assert!(fixture.accept(&mut consumer, 1, 10).is_err());
    let bytes = std::fs::read(consumer.watermark_path()).unwrap();
    drop(consumer);
    let mut consumer = fixture.consumer().unwrap();
    assert_eq!(consumer.accepted_release_sequence(), Some(2));
    assert_eq!(consumer.accepted_ipns_sequence(), Some(20));
    assert!(
        fixture.accept(&mut consumer, 1, 21).is_err(),
        "new transport sequence cannot replay old release"
    );
    assert!(
        fixture.accept(&mut consumer, 3, 19).is_err(),
        "new release cannot lower transport floor"
    );
    assert_eq!(std::fs::read(consumer.watermark_path()).unwrap(), bytes);
    fixture.accept(&mut consumer, 2, 22).unwrap(); // independent transport refresh
    fixture.accept(&mut consumer, 3, 23).unwrap();
}

#[test]
fn fresh_consumer_can_accept_stale_signed_discovery_without_latest_proof() {
    let fixture = Fixture::new();
    let mut established = fixture.consumer().unwrap();
    fixture.accept(&mut established, 2, 20).unwrap();
    assert!(fixture.accept(&mut established, 1, 10).is_err());
    let fresh_directory = tempfile::tempdir().unwrap();
    let mut fresh = Consumer::open(
        fresh_directory.path(),
        fixture.name(),
        &keyring(&fixture.release_key),
    )
    .unwrap();
    fixture.accept(&mut fresh, 1, 10).unwrap();
    assert_eq!(fresh.accepted_release_sequence(), Some(1));
    // No new-consumer policy claims that sequence 1 is globally latest.
}

#[test]
fn equal_release_sequence_requires_same_descriptor_and_equal_transport_requires_same_root() {
    let fixture = Fixture::new();
    let mut consumer = fixture.consumer().unwrap();
    fixture.accept(&mut consumer, 2, 20).unwrap();
    let mut d = descriptor(fixture.name(), 2);
    d.version = "0.1.2".into();
    let release = authenticate(&d, &fixture.release_key);
    let (root, bytes) = envelope(release.descriptor_cid());
    assert!(consumer
        .accept(
            &signed(&fixture.name_key, root, 21, 3600),
            &bytes,
            &release,
            TARGET,
            COMPATIBILITY_PROFILE
        )
        .is_err());
    assert!(fixture.accept(&mut consumer, 3, 20).is_err());
}

#[test]
fn historical_selection_never_changes_existing_watermark() {
    let fixture = Fixture::new();
    let mut consumer = fixture.consumer().unwrap();
    fixture.accept(&mut consumer, 4, 40).unwrap();
    let before = std::fs::read(consumer.watermark_path()).unwrap();
    let historical = fixture
        .release(1)
        .select(TARGET, COMPATIBILITY_PROFILE)
        .unwrap();
    historical.verify_host(HOST).unwrap();
    assert_eq!(std::fs::read(consumer.watermark_path()).unwrap(), before);
    assert_eq!(consumer.accepted_release_sequence(), Some(4));
}

#[test]
fn consumer_cannot_import_another_release_authenticator_or_channel() {
    let fixture = Fixture::new();
    let mut consumer = fixture.consumer().unwrap();
    let d = descriptor(fixture.name(), 1);
    let release = authenticate(&d, &SigningKey::from_bytes(&[9; 32]));
    let (root, bytes) = envelope(release.descriptor_cid());
    assert!(consumer
        .accept(
            &signed(&fixture.name_key, root, 10, 3600),
            &bytes,
            &release,
            TARGET,
            COMPATIBILITY_PROFILE
        )
        .is_err());
    let other = Keypair::generate_ed25519().public().to_peer_id();
    let release = authenticate(&descriptor(other, 1), &fixture.release_key);
    let (root, bytes) = envelope(release.descriptor_cid());
    assert!(consumer
        .accept(
            &signed(&fixture.name_key, root, 10, 3600),
            &bytes,
            &release,
            TARGET,
            COMPATIBILITY_PROFILE
        )
        .is_err());
    assert_eq!(consumer.accepted_release_sequence(), None);
}

#[test]
fn wrong_signer_expiry_and_incompatible_selection_do_not_advance_state() {
    let fixture = Fixture::new();
    let mut consumer = fixture.consumer().unwrap();
    let release = fixture.release(1);
    let (root, bytes) = envelope(release.descriptor_cid());
    assert!(consumer
        .accept(
            &signed(&Keypair::generate_ed25519(), root, 1, 3600),
            &bytes,
            &release,
            TARGET,
            COMPATIBILITY_PROFILE
        )
        .is_err());
    assert!(consumer
        .accept(
            &signed(&fixture.name_key, root, 1, -1),
            &bytes,
            &release,
            TARGET,
            COMPATIBILITY_PROFILE
        )
        .is_err());
    assert!(consumer
        .accept(
            &signed(&fixture.name_key, root, 1, 3600),
            &bytes,
            &release,
            TARGET,
            "unknown"
        )
        .is_err());
    assert!(!consumer.watermark_path().exists());
}

#[test]
fn consumer_lock_serializes_writers_and_missing_initialized_state_fails_closed() {
    let fixture = Fixture::new();
    let mut consumer = fixture.consumer().unwrap();
    assert!(fixture.consumer().is_err());
    fixture.accept(&mut consumer, 1, 10).unwrap();
    let path = consumer.watermark_path().to_path_buf();
    drop(consumer);
    std::fs::remove_file(path).unwrap();
    assert!(fixture.consumer().is_err());
}

#[test]
fn corrupt_proof_and_oversized_state_fail_closed() {
    let fixture = Fixture::new();
    let mut consumer = fixture.consumer().unwrap();
    fixture.accept(&mut consumer, 1, 10).unwrap();
    let path = consumer.watermark_path().to_path_buf();
    let bytes = std::fs::read(&path).unwrap();
    drop(consumer);
    let mut state: Watermark = serde_json::from_slice(&bytes).unwrap();
    state.descriptor = STANDARD.encode(b"tampered");
    std::fs::write(&path, serde_json::to_vec(&state).unwrap()).unwrap();
    assert!(fixture.consumer().is_err());
    std::fs::write(&path, vec![b' '; MAX_STATE_BYTES + 1]).unwrap();
    assert!(fixture.consumer().is_err());
}

#[test]
fn persistence_failure_poisons_handle_and_cannot_advance_memory() {
    let fixture = Fixture::new();
    let mut consumer = fixture.consumer().unwrap();
    std::fs::create_dir(consumer.watermark_path()).unwrap(); // force atomic rename failure
    assert!(fixture.accept(&mut consumer, 1, 10).is_err());
    assert_eq!(consumer.accepted_release_sequence(), None);
    std::fs::remove_dir(consumer.watermark_path()).unwrap();
    assert!(
        fixture.accept(&mut consumer, 2, 20).is_err(),
        "uncertain persistence requires recovery"
    );
    drop(consumer);
    let mut consumer = fixture.consumer().unwrap();
    fixture.accept(&mut consumer, 2, 20).unwrap();
}

#[test]
fn expired_persisted_records_remain_both_ordering_floors() {
    let fixture = Fixture::new();
    let mut consumer = fixture.consumer().unwrap();
    fixture.accept(&mut consumer, 2, 20).unwrap();
    let path = consumer.watermark_path().to_path_buf();
    let mut state: Watermark = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    let release = fixture.release(2);
    let (root, _) = envelope(release.descriptor_cid());
    state.ipns = STANDARD.encode(signed(&fixture.name_key, root, 20, -1));
    drop(consumer);
    std::fs::write(&path, serde_json::to_vec(&state).unwrap()).unwrap();
    let mut consumer = fixture.consumer().unwrap();
    assert_eq!(consumer.accepted_ipns_sequence(), Some(20));
    assert!(fixture.accept(&mut consumer, 1, 21).is_err());
    assert!(fixture.accept(&mut consumer, 3, 19).is_err());
    fixture.accept(&mut consumer, 3, 21).unwrap();
}

#[test]
fn version_one_descriptor_golden_cid() {
    let name: PeerId = "12D3KooWGuR5BdSqp23UeoeesuwYwW3ebQ9rZ8aVwfWEDU8kvCYJ"
        .parse()
        .unwrap();
    let descriptor = descriptor(name, 7);
    assert_eq!(
        descriptor.cid().unwrap().to_string(),
        "bafyreialiysos4suv6rxesralecyon26yl32wqkqpk2parn4a52vj3mugu"
    );
}

#[test]
fn crashed_consumer_temporary_is_bounded_and_never_recovery_authority() {
    let fixture = Fixture::new();
    let mut consumer = fixture.consumer().unwrap();
    fixture.accept(&mut consumer, 2, 20).unwrap();
    let temporary = consumer.watermark_path().with_extension("next");
    std::fs::write(&temporary, b"incomplete newer proof").unwrap();
    drop(consumer);
    let mut consumer = fixture.consumer().unwrap();
    assert_eq!(consumer.accepted_release_sequence(), Some(2));
    fixture.accept(&mut consumer, 3, 30).unwrap();
    assert!(!temporary.exists());
    assert_eq!(
        std::fs::read_dir(fixture.directory.path()).unwrap().count(),
        2
    );
}

#[test]
fn retired_signing_key_still_proves_floor_but_cannot_authorize_new_discovery() {
    let fixture = Fixture::new();
    let mut consumer = fixture.consumer().unwrap();
    fixture.accept(&mut consumer, 2, 20).unwrap();
    drop(consumer);
    let new_key = SigningKey::from_bytes(&[4; 32]);
    let mut consumer =
        Consumer::open(fixture.directory.path(), fixture.name(), &keyring(&new_key)).unwrap();
    assert_eq!(consumer.accepted_release_sequence(), Some(2));
    assert_eq!(consumer.accepted_ipns_sequence(), Some(20));
    assert!(
        fixture.accept(&mut consumer, 3, 30).is_err(),
        "retired key authorized new discovery"
    );
    let stale = authenticate(&descriptor(fixture.name(), 1), &new_key);
    let (stale_root, stale_block) = envelope(stale.descriptor_cid());
    assert!(consumer
        .accept(
            &signed(&fixture.name_key, stale_root, 21, 60),
            &stale_block,
            &stale,
            TARGET,
            COMPATIBILITY_PROFILE
        )
        .is_err());
    let release = authenticate(&descriptor(fixture.name(), 3), &new_key);
    let (root, block) = envelope(release.descriptor_cid());
    assert!(consumer
        .accept(
            &signed(&fixture.name_key, root, 19, 60),
            &block,
            &release,
            TARGET,
            COMPATIBILITY_PROFILE
        )
        .is_err());
    consumer
        .accept(
            &signed(&fixture.name_key, root, 30, 60),
            &block,
            &release,
            TARGET,
            COMPATIBILITY_PROFILE,
        )
        .unwrap();
    assert_eq!(consumer.accepted_release_sequence(), Some(3));
    drop(consumer);
    let consumer =
        Consumer::open(fixture.directory.path(), fixture.name(), &keyring(&new_key)).unwrap();
    assert_eq!(consumer.accepted_release_sequence(), Some(3));
    assert_eq!(consumer.accepted_ipns_sequence(), Some(30));
}
