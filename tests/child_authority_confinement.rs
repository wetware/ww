//! T1 constructive child-authority confinement harness.
//!
//! Ordinary `cargo test` runs the characterization tests, closed confinement
//! regressions, and the mandatory Cap'n Proto fork gate. The former T4 and T5
//! expected-red cases are ordinary green regressions. Ordinary children now
//! receive their immutable grants through the distinct `InitialGrants`
//! interface.

use std::cell::Cell;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::rc::Rc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, OnceLock};

use capnp::capability::{FromClientHook, Promise};
use serde_json::Value;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::watch;

use ww::launcher::{create_runtime_client, create_runtime_client_with_pinset};
use ww::rpc::CachePolicy;
use ww::system_capnp;

const CAPNP_FORK_REVISION: &str = "c6eecf42da63296e5bf628251935cf5af09d80be";
const SENSITIVE_CAPS: &[&str] = &[
    "host",
    "runtime",
    "routing",
    "authority",
    "identity",
    "ipfs",
    "http-client",
];
const KNOWN_CID: &str = "bafkreibm6jg3ux5quy7flfgn5gmxk5ubm6yur3apcu3to3d6tmjzptm2ye";

fn assert_capnp_rpc_revision(lock: &str, label: &str) {
    let stanza = lock
        .split("[[package]]")
        .find(|stanza| stanza.contains("name = \"capnp-rpc\""))
        .unwrap_or_else(|| panic!("{label} has no capnp-rpc package"));
    assert!(
        stanza.contains(&format!("#{CAPNP_FORK_REVISION}\"")),
        "{label} must resolve capnp-rpc at {CAPNP_FORK_REVISION}: {stanza}"
    );
}

fn fixture_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/authority-probe")
}

fn probe_wasm() -> &'static PathBuf {
    static PROBE: OnceLock<PathBuf> = OnceLock::new();
    PROBE.get_or_init(|| {
        let root = Path::new(env!("CARGO_MANIFEST_DIR"));
        let target = root.join("target/authority-probe");
        let status = Command::new(env!("CARGO"))
            .current_dir(root)
            .env("CARGO_TARGET_DIR", &target)
            .args([
                "build",
                "--locked",
                "--manifest-path",
                "tests/fixtures/authority-probe/Cargo.toml",
                "--target",
                "wasm32-wasip2",
                "--release",
            ])
            .status()
            .expect("launch cargo to build real-WASM authority probe");
        assert!(status.success(), "authority probe build failed");
        let wasm = target.join("wasm32-wasip2/release/authority_probe.wasm");
        assert!(wasm.is_file(), "probe artifact missing: {}", wasm.display());
        wasm
    })
}

fn probe_bytes() -> Vec<u8> {
    std::fs::read(probe_wasm()).expect("read authority-probe WASM")
}

#[derive(Default)]
struct SwarmCounts {
    provide: Cell<u32>,
    find: Cell<u32>,
}

#[derive(Default)]
struct BackendCounts {
    http: Cell<u32>,
    ipfs: Cell<u32>,
}

struct Harness {
    executor: system_capnp::executor::Client,
    counts: Rc<SwarmCounts>,
    backend_counts: Rc<BackendCounts>,
    backend_url: String,
    _epoch_tx: watch::Sender<authority::Epoch>,
}

async fn probe_backend() -> (String, Rc<BackendCounts>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind test-local probe backend");
    let url = format!(
        "http://{}",
        listener.local_addr().expect("probe backend address")
    );
    let counts = Rc::new(BackendCounts::default());
    let server_counts = counts.clone();
    tokio::task::spawn_local(async move {
        loop {
            let (mut stream, _) = listener.accept().await.expect("accept probe backend call");
            let counts = server_counts.clone();
            tokio::task::spawn_local(async move {
                let mut request = Vec::new();
                let mut chunk = [0_u8; 1024];
                loop {
                    let read = stream
                        .read(&mut chunk)
                        .await
                        .expect("read probe backend request");
                    if read == 0 {
                        return;
                    }
                    request.extend_from_slice(&chunk[..read]);
                    if request.windows(4).any(|window| window == b"\r\n\r\n") {
                        break;
                    }
                }
                let first_line = String::from_utf8_lossy(&request);
                let first_line = first_line.lines().next().unwrap_or_default();
                if first_line.contains("/api/v0/") {
                    counts.ipfs.set(counts.ipfs.get() + 1);
                } else if first_line.contains("/authority-probe") {
                    counts.http.set(counts.http.get() + 1);
                }
                stream
                    .write_all(
                        b"HTTP/1.1 200 OK\r\ncontent-length: 1\r\nconnection: close\r\n\r\nx",
                    )
                    .await
                    .expect("write probe backend response");
            });
        }
    });
    (url, counts)
}

async fn harness(wasm: &[u8]) -> Harness {
    let (backend_url, backend_counts) = probe_backend().await;
    let counts = Rc::new(SwarmCounts::default());

    let epoch = authority::Epoch {
        seq: 1,
        head: b"t1".to_vec(),
        provenance: authority::Provenance::Block(1),
    };
    let (epoch_tx, epoch_rx) = watch::channel(epoch);
    let guard = authority::EpochGuard {
        issued_seq: 1,
        receiver: epoch_rx.clone(),
    };
    let runtime = create_runtime_client(false, Some(guard), None, None, CachePolicy::Shared);
    let executor = load_executor(&runtime, wasm).await;
    Harness {
        executor,
        counts,
        backend_counts,
        backend_url,
        _epoch_tx: epoch_tx,
    }
}

async fn raw_fallback_executor(wasm: &[u8]) -> system_capnp::executor::Client {
    let runtime = create_runtime_client(false, None, None, None, CachePolicy::Isolated);
    load_executor(&runtime, wasm).await
}

async fn load_executor(
    runtime: &system_capnp::runtime::Client,
    wasm: &[u8],
) -> system_capnp::executor::Client {
    let mut request = runtime.load_request();
    request.get().set_wasm(wasm);
    request
        .send()
        .promise
        .await
        .expect("runtime.load probe")
        .get()
        .expect("load results")
        .get_executor()
        .expect("probe executor")
}

#[derive(Clone)]
struct Grant {
    name: String,
    cap: capnp::capability::Client,
}

async fn spawn_probe(
    executor: &system_capnp::executor::Client,
    mode: &str,
    env: &[(&str, &str)],
    grants: &[Grant],
) -> Result<system_capnp::process::Client, capnp::Error> {
    let mut request = executor.spawn_request();
    {
        let mut args = request.get().init_args(2);
        args.set(0, "authority-probe");
        args.set(1, mode);
    }
    {
        let mut vars = request.get().init_env(env.len() as u32);
        for (index, (name, value)) in env.iter().enumerate() {
            vars.set(index as u32, format!("{name}={value}"));
        }
    }
    {
        let mut caps = request.get().init_caps(grants.len() as u32);
        for (index, grant) in grants.iter().enumerate() {
            let mut entry = caps.reborrow().get(index as u32);
            entry.set_name(&grant.name);
            entry.init_cap().set_as_capability(grant.cap.clone().hook);
        }
    }
    let response = request.send().promise.await?;
    response.get()?.get_process()
}

async fn read_all(stream: system_capnp::byte_stream::Client) -> Result<Vec<u8>, capnp::Error> {
    let mut output = Vec::new();
    loop {
        let mut request = stream.read_request();
        request.get().set_max_bytes(64 * 1024);
        let response = request.send().promise.await?;
        let bytes = response.get()?.get_data()?;
        if bytes.is_empty() {
            return Ok(output);
        }
        output.extend_from_slice(bytes);
    }
}

async fn probe_report(
    executor: &system_capnp::executor::Client,
    mode: &str,
    env: &[(&str, &str)],
    grants: &[Grant],
) -> Value {
    // CI runners execute many real-WASM cases concurrently. Descendant probes
    // perform additional nested spawns and can legitimately cross 30 seconds
    // under CPU contention even though each RPC remains live. Bound the whole
    // probe lifecycle instead of timing only stdout after an unbounded spawn.
    let bytes = tokio::time::timeout(std::time::Duration::from_secs(120), async {
        let process = spawn_probe(executor, mode, env, grants)
            .await
            .expect("spawn authority probe");
        let stdout = process
            .stdout_request()
            .send()
            .promise
            .await
            .expect("process.stdout")
            .get()
            .expect("stdout results")
            .get_stream()
            .expect("stdout stream");
        read_all(stdout).await.expect("read authority probe stdout")
    })
    .await
    .unwrap_or_else(|_| panic!("authority probe {mode:?} timed out after 120 seconds"));
    let text = String::from_utf8(bytes).expect("probe stdout UTF-8");
    serde_json::from_str(text.trim())
        .unwrap_or_else(|error| panic!("probe emitted invalid JSON ({error}): {text:?}"))
}

struct CountingHost {
    calls: Rc<Cell<u32>>,
    identity: Vec<u8>,
}

#[allow(refining_impl_trait)]
impl system_capnp::host::Server for CountingHost {
    fn id(
        self: capnp::capability::Rc<Self>,
        _params: system_capnp::host::IdParams,
        mut results: system_capnp::host::IdResults,
    ) -> Promise<(), capnp::Error> {
        self.calls.set(self.calls.get() + 1);
        results.get().set_peer_id(&self.identity);
        Promise::ok(())
    }
}

fn counting_host(identity: &[u8]) -> (Grant, Rc<Cell<u32>>) {
    let calls = Rc::new(Cell::new(0));
    let host: system_capnp::host::Client = capnp_rpc::new_client(CountingHost {
        calls: calls.clone(),
        identity: identity.to_vec(),
    });
    (
        Grant {
            name: String::new(),
            cap: host.client,
        },
        calls,
    )
}

struct DropTrackedHost {
    dropped: Rc<Cell<bool>>,
}

impl Drop for DropTrackedHost {
    fn drop(&mut self) {
        self.dropped.set(true);
    }
}

impl system_capnp::host::Server for DropTrackedHost {}

struct GatedDropTrackedHost {
    dropped: Rc<Cell<bool>>,
    started: Arc<tokio::sync::Notify>,
    release: Arc<tokio::sync::Notify>,
}

impl Drop for GatedDropTrackedHost {
    fn drop(&mut self) {
        self.dropped.set(true);
    }
}

#[allow(refining_impl_trait)]
impl system_capnp::host::Server for GatedDropTrackedHost {
    async fn id(
        self: capnp::capability::Rc<Self>,
        _params: system_capnp::host::IdParams,
        mut results: system_capnp::host::IdResults,
    ) -> Result<(), capnp::Error> {
        self.started.notify_one();
        self.release.notified().await;
        results.get().set_peer_id(b"record-pinned");
        Ok(())
    }
}

struct KnownCidPinner {
    cid: cid::Cid,
    bytes: Vec<u8>,
    pins: AtomicUsize,
    fetches: AtomicUsize,
    unpins: AtomicUsize,
}

#[async_trait::async_trait]
impl cache::Pinner for KnownCidPinner {
    async fn pin(&self, cid: &cid::Cid) -> anyhow::Result<()> {
        anyhow::ensure!(cid == &self.cid, "unknown CID");
        self.pins.fetch_add(1, Ordering::Relaxed);
        Ok(())
    }

    async fn unpin(&self, cid: &cid::Cid) -> anyhow::Result<()> {
        anyhow::ensure!(cid == &self.cid, "unknown CID");
        self.unpins.fetch_add(1, Ordering::Relaxed);
        Ok(())
    }

    async fn fetch(&self, cid: &cid::Cid) -> anyhow::Result<Vec<u8>> {
        anyhow::ensure!(cid == &self.cid, "unknown CID");
        self.fetches.fetch_add(1, Ordering::Relaxed);
        Ok(self.bytes.clone())
    }

    async fn size(&self, cid: &cid::Cid) -> anyhow::Result<u64> {
        anyhow::ensure!(cid == &self.cid, "unknown CID");
        Ok(self.bytes.len() as u64)
    }
}

struct LateVatClient {
    delegated: system_capnp::host::Client,
    calls: Rc<Cell<u32>>,
}

#[allow(refining_impl_trait)]
impl system_capnp::vat_client::Server for LateVatClient {
    fn dial(
        self: capnp::capability::Rc<Self>,
        params: system_capnp::vat_client::DialParams,
        mut results: system_capnp::vat_client::DialResults,
    ) -> Promise<(), capnp::Error> {
        let params = capnp_rpc::pry!(params.get());
        let protocol = capnp_rpc::pry!(capnp_rpc::pry!(params.get_protocol()).to_str());
        if protocol != "late-delegation" {
            return Promise::err(capnp::Error::failed(format!(
                "unexpected delegation protocol: {protocol}"
            )));
        }
        self.calls.set(self.calls.get() + 1);
        results
            .get()
            .init_cap()
            .set_as_capability(self.delegated.client.clone().hook);
        Promise::ok(())
    }
}

struct MailboxHost {
    vat_client: system_capnp::vat_client::Client,
}

#[allow(refining_impl_trait)]
impl system_capnp::host::Server for MailboxHost {
    fn network(
        self: capnp::capability::Rc<Self>,
        _params: system_capnp::host::NetworkParams,
        mut results: system_capnp::host::NetworkResults,
    ) -> Promise<(), capnp::Error> {
        results.get().set_vat_client(self.vat_client.clone());
        Promise::ok(())
    }
}

fn names(report: &Value, delivery: &str) -> Vec<String> {
    report[delivery]
        .as_array()
        .unwrap_or_else(|| panic!("missing {delivery} names: {report}"))
        .iter()
        .map(|value| value.as_str().expect("cap name").to_owned())
        .collect()
}

#[test]
fn capnp_fork_gate_same_cap_two_names_survives_redelivery() {
    let root_lock =
        std::fs::read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join("Cargo.lock"))
            .expect("root Cargo.lock");
    let fixture_lock =
        std::fs::read_to_string(fixture_dir().join("Cargo.lock")).expect("probe Cargo.lock");
    assert_capnp_rpc_revision(&root_lock, "host Cargo.lock");
    assert_capnp_rpc_revision(&fixture_lock, "probe Cargo.lock");

    let wasm = probe_bytes();
    let local = tokio::task::LocalSet::new();
    local.block_on(&tokio::runtime::Runtime::new().unwrap(), async move {
        let harness = harness(&wasm).await;
        let identity = vec![0xaa, 0xbb, 0xcc, 0xdd];
        let (grant, calls) = counting_host(&identity);
        let report = probe_report(
            &harness.executor,
            "alias-redelivery",
            &[],
            &[
                Grant {
                    name: "alias-a".into(),
                    cap: grant.cap.clone(),
                },
                Grant {
                    name: "alias-b".into(),
                    cap: grant.cap,
                },
            ],
        )
        .await;

        assert_eq!(
            report["ok"], true,
            "broken-cap or routing anomaly: {report}"
        );
        let observed = report["detail"]["observed"]
            .as_array()
            .expect("alias observations");
        assert_eq!(observed.len(), 4, "two names across two deliveries");
        for delivery in 1..=2 {
            for alias in ["alias-a", "alias-b"] {
                assert!(observed.iter().any(|entry| {
                    entry["delivery"] == delivery
                        && entry["name"] == alias
                        && entry["peer_id"] == serde_json::json!(identity)
                }));
            }
        }
        assert_eq!(
            calls.get(),
            4,
            "the same intended server must observe one call per alias per delivery"
        );
    });
}

#[test]
fn repeated_current_bootstrap_delivery_is_name_idempotent() {
    let wasm = probe_bytes();
    let local = tokio::task::LocalSet::new();
    local.block_on(&tokio::runtime::Runtime::new().unwrap(), async move {
        let harness = harness(&wasm).await;
        let report = probe_report(&harness.executor, "enumerate", &[], &[]).await;
        assert_eq!(names(&report, "first"), names(&report, "second"));
    });
}

#[test]
fn arbitrary_strings_do_not_resolve_without_an_export() {
    let wasm = probe_bytes();
    let local = tokio::task::LocalSet::new();
    local.block_on(&tokio::runtime::Runtime::new().unwrap(), async move {
        let harness = harness(&wasm).await;
        for name in ["", "definitely-not-granted", "host/../../runtime"] {
            let report = probe_report(
                &harness.executor,
                "arbitrary-name",
                &[("WW_PROBE_NAME", name)],
                &[],
            )
            .await;
            assert_eq!(
                report["resolved"], false,
                "strings are not authority: {report}"
            );
        }
    });
}

#[test]
fn probe_can_invoke_a_test_local_parent_capability_when_explicitly_supplied() {
    let wasm = probe_bytes();
    let local = tokio::task::LocalSet::new();
    local.block_on(&tokio::runtime::Runtime::new().unwrap(), async move {
        let harness = harness(&wasm).await;
        let (mut grant, calls) = counting_host(b"parent");
        grant.name = "ambient-parent".into();
        let report = probe_report(
            &harness.executor,
            "invoke",
            &[("WW_PROBE_CAP", "ambient-parent")],
            &[grant],
        )
        .await;
        assert_eq!(report["ok"], true, "parent probe failed: {report}");
        assert_eq!(calls.get(), 1);
    });
}

#[test]
fn promised_and_broken_references_preserve_behavior_through_initial_grants() {
    let wasm = probe_bytes();
    let local = tokio::task::LocalSet::new();
    local.block_on(&tokio::runtime::Runtime::new().unwrap(), async move {
        let harness = harness(&wasm).await;
        let promised_calls = Rc::new(Cell::new(0));
        let promised_identity = b"promised-host".to_vec();
        let promised_calls_server = promised_calls.clone();
        let promised: system_capnp::host::Client = capnp_rpc::new_future_client(async move {
            tokio::task::yield_now().await;
            Ok(capnp_rpc::new_client(CountingHost {
                calls: promised_calls_server,
                identity: promised_identity,
            }))
        });
        let promised_report = probe_report(
            &harness.executor,
            "invoke",
            &[("WW_PROBE_CAP", "ambient-parent")],
            &[Grant {
                name: "ambient-parent".into(),
                cap: promised.client,
            }],
        )
        .await;
        assert_eq!(
            promised_report["ok"], true,
            "promised reference did not resolve through InitialGrants: {promised_report}"
        );
        assert_eq!(promised_calls.get(), 1);

        let broken: system_capnp::host::Client = capnp_rpc::new_future_client(async {
            Err(capnp::Error::failed("broken-ref-probe".into()))
        });
        let broken_report = probe_report(
            &harness.executor,
            "invoke",
            &[("WW_PROBE_CAP", "ambient-parent")],
            &[Grant {
                name: "ambient-parent".into(),
                cap: broken.client,
            }],
        )
        .await;
        assert_eq!(broken_report["ok"], false);
        assert!(
            broken_report["error"]
                .as_str()
                .is_some_and(|error| error.contains("broken-ref-probe")),
            "broken reference must remain observably broken: {broken_report}"
        );
    });
}

#[test]
fn attenuated_grant_allows_id_and_denies_network_through_real_wasm() {
    let wasm = probe_bytes();
    let local = tokio::task::LocalSet::new();
    local.block_on(&tokio::runtime::Runtime::new().unwrap(), async move {
        let harness = harness(&wasm).await;
        let calls = Rc::new(Cell::new(0));
        let host: system_capnp::host::Client = capnp_rpc::new_client(CountingHost {
            calls: calls.clone(),
            identity: b"attenuated".to_vec(),
        });
        let policy = membrane::MethodProfile::<system_capnp::host::Client>::new()
            .allow_method(system_capnp::host::Client::id_request)
            .expect("capture Host.id method")
            .build();
        let attenuated = membrane::membrane(host, Rc::new(policy));

        let report = probe_report(
            &harness.executor,
            "attenuated",
            &[],
            &[Grant {
                name: "attenuated-host".into(),
                cap: attenuated.client,
            }],
        )
        .await;
        assert_eq!(report["ok"], true, "attenuation probe failed: {report}");
        assert_eq!(
            report["detail"]["names"],
            serde_json::json!(["attenuated-host"])
        );
        assert_eq!(
            report["detail"]["peer_id"],
            serde_json::json!(b"attenuated")
        );
        assert!(
            report["detail"]["denied"]
                .as_str()
                .is_some_and(|error| error.contains(membrane::DENIED_MARKER)),
            "denial must retain its stable class: {report}"
        );
        assert_eq!(
            calls.get(),
            1,
            "only the allowed Host.id reached the server"
        );
    });
}

#[test]
fn multi_grant_trusted_constructor_lattice_is_concrete_and_exact() {
    let wasm = probe_bytes();
    let local = tokio::task::LocalSet::new();
    local.block_on(&tokio::runtime::Runtime::new().unwrap(), async move {
        let harness = harness(&wasm).await;
        let runtime = create_runtime_client(false, None, None, None, CachePolicy::Isolated);
        let report = probe_report(
            &harness.executor,
            "trusted-lattice",
            &[("WW_PROBE_IMAGE", "runtime-selected-image")],
            &[
                Grant {
                    name: "runtime".into(),
                    cap: runtime.client,
                },
                Grant {
                    name: "bound-executor".into(),
                    cap: harness.executor.clone().client,
                },
            ],
        )
        .await;
        assert_eq!(report["ok"], true, "multi-grant probe failed: {report}");
        assert_eq!(
            report["detail"]["names"],
            serde_json::json!(["runtime", "bound-executor"])
        );
        assert_eq!(report["detail"]["different_images"], true);
    });
}

#[test]
fn late_delegation_uses_explicit_conduit_without_mutating_birth_set() {
    let wasm = probe_bytes();
    let local = tokio::task::LocalSet::new();
    local.block_on(&tokio::runtime::Runtime::new().unwrap(), async move {
        let harness = harness(&wasm).await;
        let (mut delegated_grant, delegated_calls) = counting_host(b"late-x");
        delegated_grant.name = "delegated-x".into();
        let delegated = system_capnp::host::Client::new(delegated_grant.cap.hook);
        let conduit_calls = Rc::new(Cell::new(0));
        let vat_client: system_capnp::vat_client::Client = capnp_rpc::new_client(LateVatClient {
            delegated,
            calls: conduit_calls.clone(),
        });
        let mailbox: system_capnp::host::Client = capnp_rpc::new_client(MailboxHost { vat_client });

        let report = probe_report(
            &harness.executor,
            "late-delegation",
            &[],
            &[Grant {
                name: "mailbox".into(),
                cap: mailbox.client,
            }],
        )
        .await;
        assert_eq!(report["ok"], true, "late delegation failed: {report}");
        assert_eq!(
            report["detail"]["initial_names"],
            serde_json::json!(["mailbox"])
        );
        assert_eq!(
            report["detail"]["received_later"],
            serde_json::json!(["delegated-x"]),
            "late capabilities must be reported separately from birth grants"
        );
        assert_eq!(
            report["detail"]["current_holdings"],
            serde_json::json!(["mailbox", "delegated-x"]),
            "late delegation changes current holdings through the explicit conduit"
        );
        assert_eq!(
            report["detail"]["after_names"],
            serde_json::json!(["mailbox"]),
            "InitialGrants.get() must remain the immutable birth set"
        );
        assert_eq!(
            report["detail"]["delegated_peer_id"],
            serde_json::json!(b"late-x")
        );
        assert_eq!(conduit_calls.get(), 1);
        assert_eq!(delegated_calls.get(), 1);
    });
}

#[test]
fn runtime_is_available_only_when_explicitly_granted() {
    let wasm = probe_bytes();
    let local = tokio::task::LocalSet::new();
    local.block_on(&tokio::runtime::Runtime::new().unwrap(), async move {
        let harness = harness(&wasm).await;

        let absent = probe_report(
            &harness.executor,
            "invoke",
            &[("WW_PROBE_CAP", "runtime")],
            &[],
        )
        .await;
        assert_eq!(absent["ok"], false, "Runtime must not be ambient: {absent}");
        assert!(
            absent["error"]
                .as_str()
                .is_some_and(|error| error.contains("capability 'runtime' not found")),
            "missing required grant must be a clear guest-level failure: {absent}"
        );

        let present = probe_report(
            &harness.executor,
            "invoke",
            &[("WW_PROBE_CAP", "runtime")],
            &[Grant {
                name: "runtime".into(),
                cap: create_runtime_client(false, None, None, None, CachePolicy::Isolated).client,
            }],
        )
        .await;
        assert_eq!(
            present["ok"], true,
            "an explicitly granted Runtime must retain normal Cap'n Proto behavior: {present}"
        );
    });
}

#[test]
fn parent_local_drop_does_not_revoke_record_pinned_authority() {
    let wasm = probe_bytes();
    let local = tokio::task::LocalSet::new();
    local.block_on(&tokio::runtime::Runtime::new().unwrap(), async move {
        let harness = harness(&wasm).await;
        let dropped = Rc::new(Cell::new(false));
        let started = Arc::new(tokio::sync::Notify::new());
        let release = Arc::new(tokio::sync::Notify::new());
        let host: system_capnp::host::Client = capnp_rpc::new_client(GatedDropTrackedHost {
            dropped: dropped.clone(),
            started: started.clone(),
            release: release.clone(),
        });
        let grant = Grant {
            name: "tracked".into(),
            cap: host.client,
        };

        let call_started = started.notified();
        let process = spawn_probe(
            &harness.executor,
            "invoke",
            &[("WW_PROBE_CAP", "tracked")],
            std::slice::from_ref(&grant),
        )
        .await
        .expect("spawn record-pinned child");
        call_started.await;

        drop(grant);
        assert!(
            !dropped.get(),
            "dropping the parent's local reference must not revoke the child's birth record"
        );
        release.notify_one();

        let stdout = process
            .stdout_request()
            .send()
            .promise
            .await
            .expect("process.stdout")
            .get()
            .expect("stdout results")
            .get_stream()
            .expect("stdout stream");
        let output = read_all(stdout).await.expect("read record-pinned probe");
        let report: Value = serde_json::from_slice(&output).expect("record-pinned JSON");
        assert_eq!(
            report["ok"], true,
            "record-pinned invocation failed: {report}"
        );
        assert_eq!(
            report["detail"]["peer_id"],
            serde_json::json!(b"record-pinned")
        );
    });
}

#[test]
fn child_exit_releases_record_owned_grant_references() {
    let wasm = probe_bytes();
    let local = tokio::task::LocalSet::new();
    local.block_on(&tokio::runtime::Runtime::new().unwrap(), async move {
        let harness = harness(&wasm).await;
        let dropped = Rc::new(Cell::new(false));
        let host: system_capnp::host::Client = capnp_rpc::new_client(DropTrackedHost {
            dropped: dropped.clone(),
        });
        let grant = Grant {
            name: "tracked".into(),
            cap: host.client,
        };

        let process = spawn_probe(
            &harness.executor,
            "enumerate",
            &[],
            std::slice::from_ref(&grant),
        )
            .await
            .expect("spawn tracked child");
        drop(grant);

        let stdout = process
            .stdout_request()
            .send()
            .promise
            .await
            .expect("process.stdout")
            .get()
            .expect("stdout results")
            .get_stream()
            .expect("stdout stream");
        let _ = read_all(stdout).await.expect("drain tracked child output");
        process
            .wait_request()
            .send()
            .promise
            .await
            .expect("wait tracked child");

        assert!(
            dropped.get(),
            "child exit must release record and RPC references even while the Process handle remains"
        );
    });
}

#[test]
fn invalid_grants_are_rejected_before_process_build() {
    let local = tokio::task::LocalSet::new();
    local.block_on(&tokio::runtime::Runtime::new().unwrap(), async move {
        // Runtime.load intentionally defers component compilation when no
        // compile service is configured. If spawn reached ProcBuilder::build,
        // these bytes would fail as invalid WASM.
        let runtime = create_runtime_client(false, None, None, None, CachePolicy::Isolated);
        let executor = load_executor(&runtime, b"not a WebAssembly component").await;
        let (grant, _calls) = counting_host(b"never-started");
        let error = spawn_probe(
            &executor,
            "enumerate",
            &[],
            &[Grant {
                name: String::new(),
                cap: grant.cap,
            }],
        )
        .await
        .err()
        .expect("invalid grant must reject the spawn");
        assert!(
            error.to_string().contains("capability name"),
            "grant validation must win before WASM process build: {error}"
        );
    });
}

#[test]
fn current_empty_grant_substrate_characterization() {
    let wasm = probe_bytes();
    let local = tokio::task::LocalSet::new();
    local.block_on(&tokio::runtime::Runtime::new().unwrap(), async move {
        let harness = harness(&wasm).await;
        let known_path = format!("/ipfs/{KNOWN_CID}");
        let report = probe_report(
            &harness.executor,
            "substrate",
            &[
                ("T1_VISIBLE_ENV", "present"),
                ("WW_PROBE_KNOWN_CID_PATH", &known_path),
            ],
            &[],
        )
        .await;
        assert_eq!(report["mode"], "substrate");
        assert!(report["args"]
            .as_array()
            .expect("args")
            .iter()
            .any(|arg| arg == "substrate"));
        assert!(report["env"]
            .as_array()
            .expect("env")
            .iter()
            .any(|pair| pair[0] == "T1_VISIBLE_ENV" && pair[1] == "present"));
        assert_eq!(report["stdio"]["stdin_terminal"], false);
        assert_eq!(report["stdio"]["stdout_terminal"], false);
        assert!(report["clock"]["monotonic_nanos"].is_u64());
        assert!(report["random_u64"].is_u64());

        assert_eq!(
            report["filesystem"]["root_entries"],
            serde_json::json!([]),
            "byte-loaded children receive a private empty image root; /tmp is a separate preopen"
        );
        assert!(report["filesystem"]["cid_enumeration"]["error"].is_string());
        assert_eq!(
            report["filesystem"]["scratch"], true,
            "the process-private /tmp preopen must be writable"
        );
        assert!(
            report["filesystem"]["known_cid_read"]["error"].is_string(),
            "without explicit cache wiring there is no global-host fallback: {report}"
        );
    });
}

#[test]
fn explicitly_wired_known_cid_read_has_path_only_authority_and_node_effects() {
    let wasm = probe_bytes();
    let local = tokio::task::LocalSet::new();
    local.block_on(&tokio::runtime::Runtime::new().unwrap(), async move {
        let cid: cid::Cid = KNOWN_CID.parse().expect("known CID");
        let bytes = b"known-cid-content".to_vec();
        let pinner = Arc::new(KnownCidPinner {
            cid,
            bytes: bytes.clone(),
            pins: AtomicUsize::new(0),
            fetches: AtomicUsize::new(0),
            unpins: AtomicUsize::new(0),
        });
        let cache = Arc::new(cache::PinsetCache::new(pinner.clone(), 1024).unwrap());
        let runtime = create_runtime_client_with_pinset(
            false,
            None,
            None,
            None,
            CachePolicy::Isolated,
            Some(cache.clone()),
        );
        let executor = load_executor(&runtime, &wasm).await;
        let path = format!("/ipfs/{KNOWN_CID}");
        let report = probe_report(
            &executor,
            "substrate",
            &[("WW_PROBE_KNOWN_CID_PATH", &path)],
            &[],
        )
        .await;

        assert_eq!(
            report["filesystem"]["known_cid_read"],
            serde_json::json!(bytes.len())
        );
        assert!(report["filesystem"]["cid_enumeration"]["error"].is_string());
        assert!(report["filesystem"]["ipfs_mutation"]["error"].is_string());
        assert_eq!(report["filesystem"]["scratch"], true);

        // The substrate grants no RPC capability or locator. The child can
        // only cause a path-based read of the CID it already supplied.
        let authority = probe_report(&executor, "invoke-all", &[], &[]).await;
        assert_eq!(authority["usable"], serde_json::json!([]));

        // This read is not "no node effect": it consumed pin/cache/fetch work
        // and materialized bytes in the host-managed cache.
        assert_eq!(pinner.pins.load(Ordering::Relaxed), 1);
        assert_eq!(pinner.fetches.load(Ordering::Relaxed), 1);
        assert!(cache.staging_dir().join(KNOWN_CID).is_file());
        assert!(cache.probably_cached(&cid));

        let repeated_env = [("WW_PROBE_KNOWN_CID_PATH", path.as_str())];
        let repeat_a = probe_report(&executor, "substrate", &repeated_env, &[]);
        let repeat_b = probe_report(&executor, "substrate", &repeated_env, &[]);
        let (repeat_a, repeat_b) = tokio::join!(repeat_a, repeat_b);
        assert_eq!(
            repeat_a["filesystem"]["known_cid_read"],
            serde_json::json!(bytes.len())
        );
        assert_eq!(
            repeat_b["filesystem"]["known_cid_read"],
            serde_json::json!(bytes.len())
        );
        assert_eq!(
            pinner.pins.load(Ordering::Relaxed),
            1,
            "concurrent repeated reads must reuse the tracked pin"
        );
        assert_eq!(
            pinner.fetches.load(Ordering::Relaxed),
            1,
            "concurrent warm reads must reuse the staged immutable bytes"
        );
    });
}

#[test]
fn writable_tmp_is_private_between_parent_and_descendant() {
    let wasm = probe_bytes();
    let local = tokio::task::LocalSet::new();
    local.block_on(&tokio::runtime::Runtime::new().unwrap(), async move {
        let harness = harness(&wasm).await;
        let report = probe_report(
            &harness.executor,
            "scratch-parent",
            &[],
            &[Grant {
                name: "restricted-executor".into(),
                cap: harness.executor.clone().client,
            }],
        )
        .await;
        assert_eq!(report["ok"], true, "scratch probe failed: {report}");
        assert_eq!(
            report["detail"]["child"]["observed_before_write"], false,
            "a descendant must not observe its parent's /tmp"
        );
        assert_eq!(
            report["detail"]["child"]["write"],
            Value::Null,
            "the descendant must receive its own writable /tmp"
        );
        assert_eq!(
            report["detail"]["parent_after"],
            serde_json::json!(b"parent"),
            "the descendant's write must not mutate the parent's scratch"
        );
    });
}

#[test]
fn current_process_stdio_topology_has_three_host_handles() {
    let wasm = probe_bytes();
    let local = tokio::task::LocalSet::new();
    local.block_on(&tokio::runtime::Runtime::new().unwrap(), async move {
        let harness = harness(&wasm).await;
        let process = spawn_probe(&harness.executor, "substrate", &[], &[])
            .await
            .expect("spawn substrate probe");
        let _stdin = process
            .stdin_request()
            .send()
            .promise
            .await
            .expect("process.stdin")
            .get()
            .expect("stdin results")
            .get_stream()
            .expect("stdin stream");
        let stdout = process
            .stdout_request()
            .send()
            .promise
            .await
            .expect("process.stdout")
            .get()
            .expect("stdout results")
            .get_stream()
            .expect("stdout stream");
        let _stderr = process
            .stderr_request()
            .send()
            .promise
            .await
            .expect("process.stderr")
            .get()
            .expect("stderr results")
            .get_stream()
            .expect("stderr stream");
        let output = read_all(stdout).await.expect("drain substrate stdout");
        assert!(!output.is_empty());
    });
}

#[test]
fn empty_grant_child_cannot_invoke_node_authority() {
    let wasm = probe_bytes();
    let local = tokio::task::LocalSet::new();
    local.block_on(&tokio::runtime::Runtime::new().unwrap(), async move {
        let harness = harness(&wasm).await;
        let mut usable = Vec::new();
        for cap in SENSITIVE_CAPS {
            let report = probe_report(
                &harness.executor,
                "invoke",
                &[
                    ("WW_PROBE_CAP", cap),
                    ("WW_PROBE_HTTP_URL", &harness.backend_url),
                ],
                &[],
            )
            .await;
            if report["ok"] == true {
                usable.push((*cap).to_owned());
            }
        }
        assert_eq!(
            harness.backend_counts.http.get(),
            0,
            "empty authority must not reach the test-local HTTP backend"
        );
        assert_eq!(
            harness.backend_counts.ipfs.get(),
            0,
            "empty authority must not reach the test-local IPFS backend"
        );
        assert!(
            usable.is_empty(),
            "empty-grant child reached usable node authority: {usable:?}"
        );
    });
}

#[test]
fn empty_grant_child_cannot_route_discover_or_publish() {
    let wasm = probe_bytes();
    let local = tokio::task::LocalSet::new();
    local.block_on(&tokio::runtime::Runtime::new().unwrap(), async move {
        let harness = harness(&wasm).await;
        let report = probe_report(&harness.executor, "routing", &[], &[]).await;
        assert_ne!(
            report["ok"], true,
            "empty-grant child used routing authority: {report}"
        );
        assert_eq!(harness.counts.provide.get(), 0);
        assert_eq!(harness.counts.find.get(), 0);
    });
}

#[test]
fn explicitly_granted_routing_remains_concretely_callable() {
    let wasm = probe_bytes();
    let local = tokio::task::LocalSet::new();
    local.block_on(&tokio::runtime::Runtime::new().unwrap(), async move {
        let harness = harness(&wasm).await;
        let routing: ww::routing_capnp::routing::Client =
            capnp_rpc::new_client(ww::rpc::routing::LocalRouting::new());
        let report = probe_report(
            &harness.executor,
            "routing",
            &[],
            &[Grant {
                name: "routing".into(),
                cap: routing.client,
            }],
        )
        .await;
        assert_eq!(report["ok"], true, "routing probe failed: {report}");
        assert_eq!(report["detail"]["provide"], true);
        assert_eq!(report["detail"]["find_providers"], true);
        assert_eq!(report["detail"]["done"], true);
    });
}

#[test]
fn bootstrap_exports_equal_supplied_set() {
    let wasm = probe_bytes();
    let local = tokio::task::LocalSet::new();
    local.block_on(&tokio::runtime::Runtime::new().unwrap(), async move {
        let harness = harness(&wasm).await;
        let (mut only, _calls) = counting_host(b"only");
        only.name = "only-grant".into();
        let report = probe_report(&harness.executor, "enumerate", &[], &[only]).await;
        assert_eq!(names(&report, "first"), vec!["only-grant"]);
        assert_eq!(names(&report, "second"), vec!["only-grant"]);
    });
}

#[test]
fn empty_and_duplicate_wire_names_abort_spawn_but_path_like_labels_are_valid() {
    let wasm = probe_bytes();
    let local = tokio::task::LocalSet::new();
    local.block_on(&tokio::runtime::Runtime::new().unwrap(), async move {
        let harness = harness(&wasm).await;
        let (grant, _calls) = counting_host(b"wire");
        for names in [vec![""], vec!["duplicate", "duplicate"]] {
            let grants: Vec<_> = names
                .iter()
                .map(|name| Grant {
                    name: (*name).to_owned(),
                    cap: grant.cap.clone(),
                })
                .collect();
            let result = spawn_probe(&harness.executor, "enumerate", &[], &grants).await;
            assert!(
                result.is_err(),
                "invalid wire names {names:?} started a child"
            );
        }

        let report = probe_report(
            &harness.executor,
            "enumerate",
            &[],
            &[Grant {
                name: "bad/name".to_owned(),
                cap: grant.cap,
            }],
        )
        .await;
        assert!(
            names(&report, "first").contains(&"bad/name".to_owned()),
            "path-like labels remain valid opaque capability names: {report}"
        );
    });
}

#[test]
fn t1_glia_cell_has_no_implicit_lexical_capability_capture() {
    let source = std::fs::read_to_string(
        Path::new(env!("CARGO_MANIFEST_DIR")).join("crates/glia/src/eval.rs"),
    )
    .expect("Glia evaluator source");
    let captures = source.matches("env.collect_caps()").count();
    assert_eq!(
        captures, 0,
        "Glia cell evaluation still has {captures} implicit lexical-capability capture paths"
    );
}

#[test]
fn t4_migrated_glia_cell_sites_parse() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    for (relative, expected_grants) in [
        (
            "examples/chess/glia/register.glia",
            &[":spawn :caps {}"][..],
        ),
        ("examples/counter/glia/register.glia", &[":grants {}"][..]),
        (
            "examples/discovery/glia/register.glia",
            &[":spawn :caps {}"][..],
        ),
        ("examples/echo/glia/register.glia", &[":grants {}"][..]),
        (
            "examples/oracle/glia/register.glia",
            &[
                ":grants {:http-client http-client}",
                ":caps {\"http-client\" http-client}",
            ][..],
        ),
        (
            "examples/snap-hello-rs/glia/register.glia",
            &[":grants {}"][..],
        ),
        (
            "examples/chess/glia/serve.glia",
            &[
                ":args [\"serve\"]",
                ":caps {\"host\" host \"routing\" routing}",
            ][..],
        ),
        (
            "examples/discovery/glia/serve.glia",
            &[
                ":args [\"serve\"]",
                ":caps {\"host\" host \"routing\" routing}",
            ][..],
        ),
        (
            "examples/oracle/glia/serve.glia",
            &[
                ":args [\"serve\"]",
                ":caps {\"host\" host \"routing\" routing}",
            ][..],
        ),
        (
            "examples/oracle/glia/consume.glia",
            &[
                ":args [\"consume\"]",
                ":caps {\"host\" host \"routing\" routing}",
            ][..],
        ),
        (
            "std/status/etc/init.d/05-status.glia",
            &[":grants {:host host}"][..],
        ),
    ] {
        let source = std::fs::read_to_string(root.join(relative))
            .unwrap_or_else(|error| panic!("read migrated Glia source {relative}: {error}"));
        for expected_grant in expected_grants {
            assert!(
                source.contains(expected_grant),
                "{relative} must retain its reviewed explicit-grant shape: {expected_grant}"
            );
        }
        glia::read_many(&source)
            .unwrap_or_else(|error| panic!("parse migrated Glia source {relative}: {error}"));
    }
}

#[test]
fn ordinary_child_bootstrap_has_no_membrane_graft_compatibility_shape() {
    let source = std::fs::read_to_string(
        Path::new(env!("CARGO_MANIFEST_DIR")).join("crates/rpc/src/graft.rs"),
    )
    .expect("graft source");
    assert!(
        source.contains("impl membrane_capnp::initial_grants::Server for InitialGrantsServer"),
        "ordinary children must be served by the grants-only interface"
    );
    assert!(
        !source.contains("impl membrane_capnp::membrane::Server for InitialAuthorityBootstrap"),
        "the temporary child Membrane.graft() compatibility interface must stay removed"
    );

    let schema =
        std::fs::read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join("capnp/membrane.capnp"))
            .expect("membrane schema");
    let initial_grants = schema
        .split("interface InitialGrants")
        .nth(1)
        .and_then(|rest| rest.split("interface Membrane").next())
        .expect("InitialGrants schema section");
    assert!(
        !initial_grants.contains("graft @"),
        "the grants-only interface must expose no graft operation"
    );
}

#[test]
fn restricted_executor_cannot_amplify_descendant() {
    let wasm = probe_bytes();
    let local = tokio::task::LocalSet::new();
    local.block_on(&tokio::runtime::Runtime::new().unwrap(), async move {
        let harness = harness(&wasm).await;
        let (mut narrow, calls) = counting_host(b"descendant-narrow");
        narrow.name = "narrow".into();
        let report = probe_report(
            &harness.executor,
            "descendant",
            &[("WW_PROBE_HTTP_URL", &harness.backend_url)],
            &[
                Grant {
                    name: "restricted-executor".into(),
                    cap: harness.executor.clone().client,
                },
                narrow,
            ],
        )
        .await;
        assert_eq!(
            report["ok"], true,
            "descendant probe itself failed: {report}"
        );
        assert_eq!(
            report["detail"]["parent_names"],
            serde_json::json!(["restricted-executor", "narrow"])
        );
        assert_eq!(
            report["detail"]["aliases"]["ok"], true,
            "explicitly forwarded descendant aliases must remain callable: {report}"
        );
        assert_eq!(
            calls.get(),
            4,
            "same descendant capability under two names must survive two get() deliveries"
        );
        let leaked = report["detail"]["omitted"]["usable"]
            .as_array()
            .expect("descendant usable capability list");
        assert!(
            leaked.is_empty(),
            "restricted Executor amplified descendant authority: {leaked:?}"
        );
    });
}

#[test]
fn no_epoch_no_stream_has_no_raw_host_fallback() {
    let wasm = probe_bytes();
    let local = tokio::task::LocalSet::new();
    local.block_on(&tokio::runtime::Runtime::new().unwrap(), async move {
        let executor = raw_fallback_executor(&wasm).await;
        let report = probe_report(&executor, "raw-host", &[], &[]).await;
        assert_ne!(
            report["ok"], true,
            "alternate constructor exposed usable raw Host: {report}"
        );
    });
}
