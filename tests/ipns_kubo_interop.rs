//! Rust/IPNS interoperability against an isolated Kubo 0.33 repository.
//!
//! Set `WW_TEST_REQUIRE_KUBO=1` for the merge-gate mode. That mode fails when
//! the exact Kubo binary is unavailable. Other local test runs skip cleanly.

use std::path::Path;
use std::process::{Child, Command, Output, Stdio};
use std::time::{Duration, Instant};

use chrono::Utc;
use cid::multihash::Multihash;
use cid::Cid;
use libp2p::identity::Keypair;
use rust_ipns::Record;

const REQUIRED_KUBO: &str = "0.33.0";

struct Kubo {
    _child: KuboChild,
    repository: tempfile::TempDir,
    api: String,
    routing_url: String,
}

struct KuboChild(Child);

impl Drop for KuboChild {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

impl Kubo {
    fn command(repository: &Path) -> Command {
        let mut command = Command::new("ipfs");
        command.env("IPFS_PATH", repository);
        command
    }

    fn output(repository: &Path, args: &[&str]) -> Output {
        Self::command(repository)
            .args(args)
            .output()
            .unwrap_or_else(|error| panic!("run ipfs {args:?}: {error}"))
    }

    fn require_success(output: Output, operation: &str) -> String {
        assert!(
            output.status.success(),
            "{operation} failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8(output.stdout)
            .unwrap_or_else(|error| panic!("{operation} output was not UTF-8: {error}"))
            .trim()
            .to_string()
    }

    fn start() -> Self {
        let version = Command::new("ipfs")
            .args(["version", "--number"])
            .output()
            .expect("WW_TEST_REQUIRE_KUBO=1 requires ipfs on PATH");
        let version = Self::require_success(version, "ipfs version");
        assert_eq!(
            version, REQUIRED_KUBO,
            "interoperability Kubo version drift"
        );

        let repository = tempfile::tempdir().expect("create isolated Kubo repository");
        Self::require_success(
            Self::output(repository.path(), &["init", "--profile=test"]),
            "isolated ipfs init",
        );
        Self::require_success(
            Self::output(
                repository.path(),
                &["config", "--json", "Gateway.ExposeRoutingAPI", "true"],
            ),
            "enable Kubo Routing V1",
        );
        for (key, value) in [
            ("Addresses.API", "/ip4/127.0.0.1/tcp/0"),
            ("Addresses.Gateway", "/ip4/127.0.0.1/tcp/0"),
        ] {
            Self::require_success(
                Self::output(repository.path(), &["config", key, value]),
                &format!("assign an ephemeral Kubo {key} listener"),
            );
        }
        Self::require_success(
            Self::output(
                repository.path(),
                &[
                    "config",
                    "--json",
                    "Addresses.Swarm",
                    r#"["/ip4/127.0.0.1/tcp/0"]"#,
                ],
            ),
            "assign an ephemeral Kubo Swarm listener",
        );

        let child = KuboChild(
            Self::command(repository.path())
                .args(["daemon", "--offline"])
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
                .expect("start isolated Kubo daemon"),
        );
        let deadline = Instant::now() + Duration::from_secs(30);
        let api_path = repository.path().join("api");
        let gateway_path = repository.path().join("gateway");
        while Instant::now() < deadline {
            if api_path.exists() && gateway_path.exists() {
                let api = std::fs::read_to_string(&api_path).expect("read isolated Kubo API file");
                let probe = Self::command(repository.path())
                    .arg(format!("--api={}", api.trim()))
                    .arg("id")
                    .stdout(Stdio::null())
                    .stderr(Stdio::null())
                    .status()
                    .expect("probe isolated Kubo");
                if probe.success() {
                    let gateway = std::fs::read_to_string(&gateway_path)
                        .expect("read isolated Kubo Gateway file");
                    return Self {
                        _child: child,
                        repository,
                        api: api.trim().to_string(),
                        routing_url: multiaddr_http_url(gateway.trim()),
                    };
                }
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        panic!("isolated Kubo did not become ready within 30 seconds");
    }

    fn cli(&self, args: &[&str]) -> String {
        let mut command = Self::command(self.repository.path());
        command.arg(format!("--api={}", self.api));
        command.args(args);
        Self::require_success(
            command.output().expect("run isolated Kubo command"),
            &format!("ipfs {args:?}"),
        )
    }
}

fn multiaddr_http_url(address: &str) -> String {
    if address.starts_with("http://") || address.starts_with("https://") {
        return address.to_string();
    }
    let parts: Vec<_> = address.split('/').collect();
    match parts.as_slice() {
        ["", "ip4", host, "tcp", port] => format!("http://{host}:{port}"),
        ["", "ip6", host, "tcp", port] => format!("http://[{host}]:{port}"),
        _ => panic!("unsupported Kubo listener multiaddr: {address}"),
    }
}

fn test_cid(byte: u8) -> Cid {
    Cid::new_v1(0x55, Multihash::<64>::wrap(0x00, &[byte]).unwrap())
}

fn required() -> bool {
    std::env::var_os("WW_TEST_REQUIRE_KUBO").is_some()
}

#[tokio::test]
async fn rust_and_kubo_boxo_raw_ipns_interoperate() {
    if !required() {
        eprintln!("skipping Kubo interoperability; set WW_TEST_REQUIRE_KUBO=1");
        return;
    }

    let kubo = Kubo::start();
    let routing = ww::ipns::RoutingClient::new(kubo.routing_url.clone()).unwrap();
    routing.probe().await.unwrap();

    // Rust -> Kubo: Kubo has never seen this private key.
    let rust_key = Keypair::generate_ed25519();
    let rust_name = rust_key.public().to_peer_id();
    let rust_canonical_name = ww::ipns::canonical_name(rust_name);
    let keystore_before = kubo.cli(&["key", "list", "-l"]);
    assert!(!keystore_before.contains(&rust_canonical_name));
    let rust_eol = Utc::now() + chrono::Duration::hours(48);
    let rust_record = Record::new(
        &rust_key,
        format!("/ipfs/{}", test_cid(1)),
        rust_eol,
        17,
        ww::ipns::RECORD_TTL,
    )
    .unwrap();
    let rust_raw = rust_record.encode().unwrap();
    routing.put(rust_name, &rust_raw).await.unwrap();
    let fetched = match routing.get(rust_name).await.unwrap() {
        ww::ipns::Fetch::Record(raw) => raw,
        ww::ipns::Fetch::NotFound => panic!("Kubo lost Rust-signed IPNS record"),
    };
    let verified = ww::ipns::SignedRecord::decode(rust_name, fetched.clone()).unwrap();
    assert_eq!(fetched, rust_raw);
    assert_eq!(verified.sequence(), 17);
    assert_eq!(verified.deployment_cid(), Some(test_cid(1)));
    assert_eq!(verified.ttl(), ww::ipns::RECORD_TTL);
    assert_eq!(verified.eol(), rust_eol);
    let decoded = Record::decode(&fetched).unwrap();
    decoded.verify(rust_name).unwrap();
    assert!(decoded.has_signature_v1());
    assert!(decoded.has_signature_v2());
    let keystore_after = kubo.cli(&["key", "list", "-l"]);
    assert_eq!(keystore_before, keystore_after);
    assert!(!keystore_after.contains(&rust_canonical_name));

    // Kubo/Boxo -> Rust: cover V2-only Ed25519, hybrid Ed25519, and a
    // non-inline RSA signer. Each record is fetched as raw protobuf.
    for (label, key_type, v1_compat, value) in [
        ("ed-v2", "ed25519", "false", test_cid(2)),
        ("ed-hybrid", "ed25519", "true", test_cid(3)),
        ("rsa-v2", "rsa", "false", test_cid(4)),
    ] {
        let mut key_args = vec!["key", "gen", "--type", key_type, "--ipns-base", "base36"];
        if key_type == "rsa" {
            key_args.extend(["--size", "2048"]);
        }
        key_args.push(label);
        let name = kubo.cli(&key_args);
        let published_after = Utc::now() + chrono::Duration::hours(47);
        let published_before = Utc::now() + chrono::Duration::hours(49);
        kubo.cli(&[
            "name",
            "publish",
            "--allow-offline",
            "--resolve=false",
            "--lifetime=48h",
            "--ttl=5m",
            &format!("--v1compat={v1_compat}"),
            &format!("--key={label}"),
            &format!("/ipfs/{value}"),
        ]);

        let peer_id = ww::ipns::parse_name(&name).unwrap();
        assert_eq!(ww::ipns::canonical_name(peer_id), name);
        let raw = match routing.get(peer_id).await.unwrap() {
            ww::ipns::Fetch::Record(raw) => raw,
            ww::ipns::Fetch::NotFound => panic!("Kubo-published record was not routable"),
        };
        let record = Record::decode(&raw).unwrap();
        record.verify(peer_id).unwrap();
        assert_eq!(record.value(), format!("/ipfs/{value}").as_bytes());
        assert_eq!(record.sequence(), 0);
        assert_eq!(record.ttl(), ww::ipns::RECORD_TTL.as_nanos() as u64);
        let eol = record.validity().unwrap();
        assert!(eol > published_after);
        assert!(eol < published_before);
        assert_eq!(record.has_signature_v1(), v1_compat == "true");
        assert!(record.has_signature_v2());
        let wetware = ww::ipns::SignedRecord::decode(peer_id, raw).unwrap();
        assert_eq!(wetware.deployment_cid(), Some(value));
        assert_eq!(wetware.sequence(), 0);
        assert_eq!(wetware.ttl(), ww::ipns::RECORD_TTL);
        assert_eq!(wetware.eol(), eol);
    }
}
