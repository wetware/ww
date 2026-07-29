//! T1 constructive child-authority confinement harness.
//!
//! Ordinary `cargo test` runs the characterization tests, closed confinement
//! regressions, and the mandatory Cap'n Proto fork gate. The remaining
//! cross-tranche expected-red case is isolated:
//!
//! ```text
//! cargo test --test child_authority_confinement t1_expected_red -- --ignored --nocapture
//! ```
//!
//! T4 owns implicit Glia lexical capture. T5 owns removal of the temporary
//! child-side `Membrane.graft()` compatibility shape.

use std::cell::Cell;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::rc::Rc;
use std::sync::OnceLock;

use capnp::capability::Promise;
use serde_json::Value;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::watch;

use ww::launcher::create_runtime_client;
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
    let bytes = tokio::time::timeout(std::time::Duration::from_secs(30), read_all(stdout))
        .await
        .expect("authority probe stdout timeout")
        .expect("read authority probe stdout");
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
        let report = probe_report(
            &harness.executor,
            "substrate",
            &[("T1_VISIBLE_ENV", "present")],
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

        // Current ExecutorImpl supplies neither CidTree nor cache mode. These
        // are current-negative observations, not the future substrate contract.
        assert!(report["filesystem"]["root_entries"]["error"].is_string());
        assert!(report["filesystem"]["cid_enumeration"]["error"].is_string());
        assert!(report["filesystem"]["scratch"]["error"].is_string());
        assert!(report["filesystem"]["known_cid_read"].is_null());
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
            &[":grants {:http-client http-client}", ":spawn :caps {}"][..],
        ),
        (
            "examples/snap-hello-rs/glia/register.glia",
            &[":grants {}"][..],
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
#[ignore = "T1 expected red (T5): ordinary children still expose InitialAuthorityRecord through the compatibility Membrane.graft() interface"]
fn t1_expected_red_child_bootstrap_has_no_membrane_graft_compatibility_shape() {
    let source = std::fs::read_to_string(
        Path::new(env!("CARGO_MANIFEST_DIR")).join("crates/rpc/src/graft.rs"),
    )
    .expect("graft source");
    assert!(
        !source.contains("impl membrane_capnp::membrane::Server for InitialAuthorityBootstrap"),
        "T5 must replace the temporary child Membrane.graft() compatibility interface"
    );
}

#[test]
fn restricted_executor_cannot_amplify_descendant() {
    let wasm = probe_bytes();
    let local = tokio::task::LocalSet::new();
    local.block_on(&tokio::runtime::Runtime::new().unwrap(), async move {
        let harness = harness(&wasm).await;
        let report = probe_report(
            &harness.executor,
            "descendant",
            &[("WW_PROBE_HTTP_URL", &harness.backend_url)],
            &[Grant {
                name: "restricted-executor".into(),
                cap: harness.executor.clone().client,
            }],
        )
        .await;
        assert_eq!(
            report["ok"], true,
            "descendant probe itself failed: {report}"
        );
        let leaked = report["detail"]["descendant"]["usable"]
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
