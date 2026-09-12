//! T1 constructive child-authority confinement harness.
//!
//! Ordinary `cargo test` runs the characterization tests, closed confinement
//! regressions, and the mandatory Cap'n Proto fork gate. The former T4 and T5
//! expected-red cases are ordinary green regressions. Ordinary children now
//! receive one typed `Membrane` capability. Fixed platform authority occupies
//! typed graft fields; arbitrary application capabilities occupy `extras`.

#[path = "support/ticked_executor.rs"]
mod ticked_executor;

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

use ticked_executor::TickedExecutor;

const CAPNP_FORK_REVISION: &str = "c6eecf42da63296e5bf628251935cf5af09d80be";
const USE_PREBUILT_AUTHORITY_PROBE_ENV: &str = "WW_USE_PREBUILT_AUTHORITY_PROBE";
const KNOWN_CID: &str = "bafkreibm6jg3ux5quy7flfgn5gmxk5ubm6yur3apcu3to3d6tmjzptm2ye";

fn fixed_epoch_zero_guard() -> authority::EpochGuard {
    authority::EpochGuard::fixed(authority::Epoch::zero())
}

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

fn require_probe_wasm(
    wasm: PathBuf,
    use_prebuilt: bool,
    build: impl FnOnce() -> Result<(), String>,
) -> PathBuf {
    if !use_prebuilt {
        build().unwrap_or_else(|error| panic!("{error}"));
    }

    let metadata = std::fs::metadata(&wasm).unwrap_or_else(|error| {
        panic!(
            "required authority-probe artifact {} is missing: {error}",
            wasm.display()
        )
    });
    assert!(
        metadata.is_file(),
        "required authority-probe artifact is not a file: {}",
        wasm.display()
    );
    assert!(
        metadata.len() > 0,
        "required authority-probe artifact is empty: {}",
        wasm.display()
    );
    wasm
}

fn probe_wasm() -> &'static PathBuf {
    static PROBE: OnceLock<PathBuf> = OnceLock::new();
    PROBE.get_or_init(|| {
        let root = Path::new(env!("CARGO_MANIFEST_DIR"));
        let target = root.join("target/authority-probe");
        let wasm = target.join("wasm32-wasip3/release/authority_probe.wasm");
        let use_prebuilt = std::env::var(USE_PREBUILT_AUTHORITY_PROBE_ENV).as_deref() == Ok("1");
        require_probe_wasm(wasm, use_prebuilt, || {
            let status = Command::new("make")
                .current_dir(root)
                .arg("authority-probe")
                .status()
                .map_err(|error| format!("failed to launch authority-probe build: {error}"))?;
            if status.success() {
                Ok(())
            } else {
                Err(format!("authority-probe build failed with {status}"))
            }
        })
    })
}

fn probe_bytes() -> Vec<u8> {
    std::fs::read(probe_wasm()).expect("read authority-probe WASM")
}

#[test]
fn default_probe_mode_builds_before_accepting_the_artifact() {
    let temp = tempfile::tempdir().expect("create probe tempdir");
    let wasm = temp.path().join("authority_probe.wasm");
    let build_called = Cell::new(false);

    let selected = require_probe_wasm(wasm.clone(), false, || {
        build_called.set(true);
        std::fs::write(&wasm, b"fresh authority probe").map_err(|error| error.to_string())
    });

    assert!(build_called.get(), "default mode did not invoke the build");
    assert_eq!(selected, wasm);
}

#[test]
fn default_probe_mode_rejects_a_failed_build_and_missing_output() {
    let temp = tempfile::tempdir().expect("create probe tempdir");
    let wasm = temp.path().join("authority_probe.wasm");
    std::fs::write(&wasm, b"stale authority probe").expect("write stale probe");

    let failure = std::panic::catch_unwind(|| {
        require_probe_wasm(wasm.clone(), false, || {
            Err("intentional build failure".into())
        })
    });

    assert!(failure.is_err(), "build failure accepted a stale artifact");

    std::fs::remove_file(&wasm).expect("remove stale probe");
    let failure = std::panic::catch_unwind(|| require_probe_wasm(wasm, false, || Ok(())));
    assert!(failure.is_err(), "missing build output was accepted");
}

#[test]
fn prebuilt_probe_mode_accepts_a_nonempty_artifact_without_building() {
    let temp = tempfile::tempdir().expect("create probe tempdir");
    let wasm = temp.path().join("authority_probe.wasm");
    std::fs::write(&wasm, b"validated authority probe").expect("write prebuilt probe");

    let selected = require_probe_wasm(wasm.clone(), true, || {
        panic!("prebuilt mode invoked the build")
    });

    assert_eq!(selected, wasm);
}

#[test]
fn prebuilt_probe_mode_rejects_missing_and_empty_artifacts() {
    let temp = tempfile::tempdir().expect("create probe tempdir");
    let missing = temp.path().join("missing.wasm");
    let empty = temp.path().join("empty.wasm");
    std::fs::write(&empty, []).expect("write empty probe");

    for wasm in [missing, empty] {
        let failure = std::panic::catch_unwind(|| {
            require_probe_wasm(wasm, true, || panic!("prebuilt mode invoked the build"))
        });
        assert!(failure.is_err(), "invalid prebuilt artifact was accepted");
    }
}

#[derive(Default)]
struct BackendCounts {
    http: Cell<u32>,
    ipfs: Cell<u32>,
}

struct Harness {
    executor: system_capnp::executor::Client,
    backend_counts: Rc<BackendCounts>,
    backend_url: String,
    _epoch_tx: watch::Sender<authority::Epoch>,
    _ticked: TickedExecutor,
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

    let epoch = authority::Epoch {
        seq: 1,
        head: b"t1".to_vec(),
        root: None,
    };
    let (epoch_tx, epoch_rx) = watch::channel(epoch);
    let guard = authority::EpochGuard {
        issued_seq: 1,
        receiver: epoch_rx.clone(),
    };
    // Authority probes are production P3 Cells. Retain the ExecutorPool so
    // its worker advances the shared Engine epoch for the harness lifetime.
    let ticked = TickedExecutor::new();
    let runtime = create_runtime_client(
        false,
        guard,
        Some(ticked.engine()),
        None,
        CachePolicy::Shared,
    );
    let executor = load_executor(&runtime, wasm).await;
    Harness {
        executor,
        backend_counts,
        backend_url,
        _epoch_tx: epoch_tx,
        _ticked: ticked,
    }
}

async fn fixed_epoch_zero_executor(
    wasm: &[u8],
) -> (system_capnp::executor::Client, TickedExecutor) {
    let ticked = TickedExecutor::new();
    let runtime = create_runtime_client(
        false,
        fixed_epoch_zero_guard(),
        Some(ticked.engine()),
        None,
        CachePolicy::Isolated,
    );
    (load_executor(&runtime, wasm).await, ticked)
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

#[derive(Clone)]
struct FixedAuthority {
    peer_id: Option<Vec<u8>>,
    stat: Option<system_capnp::stat::Client>,
    stream_listener: Option<system_capnp::stream_listener::Client>,
    stream_dialer: Option<system_capnp::stream_dialer::Client>,
    vat_listener: Option<system_capnp::vat_listener::Client>,
    vat_dialer: Option<system_capnp::vat_client::Client>,
    http_listener: Option<system_capnp::http_listener::Client>,
    http_dialer: Option<ww::http_capnp::http_client::Client>,
    routing_finder: Option<ww::routing_capnp::finder::Client>,
    routing_announcer: Option<ww::routing_capnp::announcer::Client>,
    runtime: Option<system_capnp::runtime::Client>,
    authority: Option<ww::auth_capnp::authority::Client>,
    identity: Option<ww::auth_capnp::identity::Client>,
    ipfs: Option<system_capnp::ipfs::Client>,
}

impl Default for FixedAuthority {
    fn default() -> Self {
        Self {
            peer_id: Some(b"test-peer".to_vec()),
            stat: None,
            stream_listener: None,
            stream_dialer: None,
            vat_listener: None,
            vat_dialer: None,
            http_listener: None,
            http_dialer: None,
            routing_finder: None,
            routing_announcer: None,
            runtime: None,
            authority: None,
            identity: None,
            ipfs: None,
        }
    }
}

#[derive(Clone)]
struct TestGraftBuilder {
    fixed: FixedAuthority,
    extras: ww::rpc::NamedCapabilities,
    grafts: Option<Rc<Cell<u32>>>,
}

impl authority::GraftBuilder for TestGraftBuilder {
    fn build(
        &self,
        _guard: &authority::EpochGuard,
        mut builder: system_capnp::membrane::graft_results::Builder<'_>,
    ) -> Result<(), capnp::Error> {
        let fixed = &self.fixed;
        if let Some(grafts) = &self.grafts {
            let graft = grafts.get() + 1;
            grafts.set(graft);
            builder.set_peer_id(format!("stateful-peer-{graft}").as_bytes());
        } else if let Some(peer_id) = &fixed.peer_id {
            builder.set_peer_id(peer_id);
        }
        if let Some(stat) = &fixed.stat {
            builder.set_stat(stat.clone());
        }
        if fixed.stream_listener.is_some()
            || fixed.stream_dialer.is_some()
            || fixed.vat_listener.is_some()
            || fixed.vat_dialer.is_some()
            || fixed.http_listener.is_some()
            || fixed.http_dialer.is_some()
        {
            let mut network = builder.reborrow().init_network();
            if fixed.stream_listener.is_some() || fixed.stream_dialer.is_some() {
                let mut stream = network.reborrow().init_stream();
                if let Some(listener) = &fixed.stream_listener {
                    stream.set_listener(listener.clone());
                }
                if let Some(dialer) = &fixed.stream_dialer {
                    stream.set_dialer(dialer.clone());
                }
            }
            if fixed.vat_listener.is_some() || fixed.vat_dialer.is_some() {
                let mut vat = network.reborrow().init_vat();
                if let Some(listener) = &fixed.vat_listener {
                    vat.set_listener(listener.clone());
                }
                if let Some(dialer) = &fixed.vat_dialer {
                    vat.set_dialer(dialer.clone());
                }
            }
            if fixed.http_listener.is_some() || fixed.http_dialer.is_some() {
                let mut http = network.init_http();
                if let Some(listener) = &fixed.http_listener {
                    http.set_listener(listener.clone());
                }
                if let Some(dialer) = &fixed.http_dialer {
                    http.set_dialer(dialer.clone());
                }
            }
        }
        if fixed.routing_finder.is_some() || fixed.routing_announcer.is_some() {
            let mut routing = builder.reborrow().init_routing();
            if let Some(finder) = &fixed.routing_finder {
                routing.set_finder(finder.clone());
            }
            if let Some(announcer) = &fixed.routing_announcer {
                routing.set_announcer(announcer.clone());
            }
        }
        if let Some(runtime) = &fixed.runtime {
            builder.set_runtime(runtime.clone());
        }
        if let Some(authority) = &fixed.authority {
            builder.set_authority(authority.clone());
        }
        if let Some(identity) = &fixed.identity {
            builder.set_identity(identity.clone());
        }
        if let Some(ipfs) = &fixed.ipfs {
            builder.set_ipfs(ipfs.clone());
        }
        let extras = builder.reborrow().init_extras(self.extras.len() as u32);
        ww::rpc::encode_exports(&self.extras, extras)
    }
}

fn test_membrane(
    fixed: FixedAuthority,
    grants: &[Grant],
) -> Result<system_capnp::membrane::Client, capnp::Error> {
    let extras = ww::rpc::NamedCapabilities::try_from_pairs(
        grants
            .iter()
            .map(|grant| (grant.name.clone(), grant.cap.clone())),
    )?;
    let (_epoch_tx, epoch_rx) = watch::channel(authority::Epoch::zero());
    Ok(capnp_rpc::new_client(authority::MembraneServer::new(
        epoch_rx,
        TestGraftBuilder {
            fixed,
            extras,
            grafts: None,
        },
    )))
}

fn stateful_test_membrane(grafts: Rc<Cell<u32>>) -> system_capnp::membrane::Client {
    let (_epoch_tx, epoch_rx) = watch::channel(authority::Epoch::zero());
    capnp_rpc::new_client(authority::MembraneServer::new(
        epoch_rx,
        TestGraftBuilder {
            fixed: FixedAuthority::default(),
            extras: ww::rpc::NamedCapabilities::default(),
            grafts: Some(grafts),
        },
    ))
}

async fn spawn_probe(
    executor: &system_capnp::executor::Client,
    mode: &str,
    env: &[(&str, &str)],
    grants: &[Grant],
) -> Result<system_capnp::process::Client, capnp::Error> {
    spawn_probe_with_authority(executor, mode, env, FixedAuthority::default(), grants).await
}

async fn spawn_probe_with_authority(
    executor: &system_capnp::executor::Client,
    mode: &str,
    env: &[(&str, &str)],
    fixed: FixedAuthority,
    grants: &[Grant],
) -> Result<system_capnp::process::Client, capnp::Error> {
    let membrane = test_membrane(fixed, grants)?;
    spawn_probe_with_membrane(executor, mode, env, membrane).await
}

async fn spawn_probe_with_membrane(
    executor: &system_capnp::executor::Client,
    mode: &str,
    env: &[(&str, &str)],
    membrane: system_capnp::membrane::Client,
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
    request.get().set_membrane(membrane);
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
    probe_report_with_authority(executor, mode, env, FixedAuthority::default(), grants).await
}

async fn probe_report_with_authority(
    executor: &system_capnp::executor::Client,
    mode: &str,
    env: &[(&str, &str)],
    fixed: FixedAuthority,
    grants: &[Grant],
) -> Value {
    // CI runners execute many real-WASM cases concurrently. Descendant probes
    // perform additional nested spawns and can legitimately cross 30 seconds
    // under CPU contention even though each RPC remains live. Bound the whole
    // probe lifecycle instead of timing only stdout after an unbounded spawn.
    let bytes = tokio::time::timeout(std::time::Duration::from_secs(120), async {
        let process = spawn_probe_with_authority(executor, mode, env, fixed, grants)
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

struct CidExecutor {
    cid: String,
}

#[allow(refining_impl_trait)]
impl system_capnp::executor::Server for CidExecutor {
    fn cid(
        self: capnp::capability::Rc<Self>,
        _params: system_capnp::executor::CidParams,
        mut results: system_capnp::executor::CidResults,
    ) -> Promise<(), capnp::Error> {
        results.get().set_cid(&self.cid);
        Promise::ok(())
    }
}

#[derive(Default)]
struct NetworkCallCounts {
    stream_listener: Cell<u32>,
    stream_dialer: Cell<u32>,
    vat_listener: Cell<u32>,
    vat_dialer: Cell<u32>,
    http_listener: Cell<u32>,
}

struct RecordingStreamListener {
    calls: Rc<NetworkCallCounts>,
}

#[allow(refining_impl_trait)]
impl system_capnp::stream_listener::Server for RecordingStreamListener {
    fn listen(
        self: capnp::capability::Rc<Self>,
        params: system_capnp::stream_listener::ListenParams,
        _results: system_capnp::stream_listener::ListenResults,
    ) -> Promise<(), capnp::Error> {
        let params = capnp_rpc::pry!(params.get());
        if !params.has_membrane() {
            return Promise::err(capnp::Error::failed("missing delegated Membrane".into()));
        }
        capnp_rpc::pry!(params.get_executor());
        let protocol = capnp_rpc::pry!(capnp_rpc::pry!(params.get_protocol()).to_str());
        if protocol != "authority-probe" {
            return Promise::err(capnp::Error::failed("unexpected stream protocol".into()));
        }
        self.calls
            .stream_listener
            .set(self.calls.stream_listener.get() + 1);
        Promise::ok(())
    }
}

struct OneByteStream;

#[allow(refining_impl_trait)]
impl system_capnp::byte_stream::Server for OneByteStream {
    fn read(
        self: capnp::capability::Rc<Self>,
        _params: system_capnp::byte_stream::ReadParams,
        mut results: system_capnp::byte_stream::ReadResults,
    ) -> Promise<(), capnp::Error> {
        results.get().set_data(b"x");
        Promise::ok(())
    }
}

struct RecordingStreamDialer {
    calls: Rc<NetworkCallCounts>,
}

#[allow(refining_impl_trait)]
impl system_capnp::stream_dialer::Server for RecordingStreamDialer {
    fn dial(
        self: capnp::capability::Rc<Self>,
        params: system_capnp::stream_dialer::DialParams,
        mut results: system_capnp::stream_dialer::DialResults,
    ) -> Promise<(), capnp::Error> {
        let params = capnp_rpc::pry!(params.get());
        if capnp_rpc::pry!(params.get_peer()) != b"authority-probe-peer" {
            return Promise::err(capnp::Error::failed("unexpected stream peer".into()));
        }
        let protocol = capnp_rpc::pry!(capnp_rpc::pry!(params.get_protocol()).to_str());
        if protocol != "authority-probe" {
            return Promise::err(capnp::Error::failed("unexpected stream protocol".into()));
        }
        self.calls
            .stream_dialer
            .set(self.calls.stream_dialer.get() + 1);
        results
            .get()
            .set_stream(capnp_rpc::new_client(OneByteStream));
        Promise::ok(())
    }
}

struct RecordingVatListener {
    calls: Rc<NetworkCallCounts>,
}

#[allow(refining_impl_trait)]
impl system_capnp::vat_listener::Server for RecordingVatListener {
    fn serve_raw(
        self: capnp::capability::Rc<Self>,
        params: system_capnp::vat_listener::ServeRawParams,
        _results: system_capnp::vat_listener::ServeRawResults,
    ) -> Promise<(), capnp::Error> {
        let params = capnp_rpc::pry!(params.get());
        capnp_rpc::pry!(params
            .get_cap()
            .get_as_capability::<capnp::capability::Client>());
        let protocol = capnp_rpc::pry!(capnp_rpc::pry!(params.get_protocol()).to_str());
        if protocol != "authority-probe" {
            return Promise::err(capnp::Error::failed("unexpected vat protocol".into()));
        }
        self.calls
            .vat_listener
            .set(self.calls.vat_listener.get() + 1);
        Promise::ok(())
    }
}

struct RecordingVatDialer {
    calls: Rc<NetworkCallCounts>,
}

#[allow(refining_impl_trait)]
impl system_capnp::vat_client::Server for RecordingVatDialer {
    fn dial(
        self: capnp::capability::Rc<Self>,
        params: system_capnp::vat_client::DialParams,
        mut results: system_capnp::vat_client::DialResults,
    ) -> Promise<(), capnp::Error> {
        let params = capnp_rpc::pry!(params.get());
        if capnp_rpc::pry!(params.get_peer()) != b"authority-probe-peer" {
            return Promise::err(capnp::Error::failed("unexpected vat peer".into()));
        }
        let protocol = capnp_rpc::pry!(capnp_rpc::pry!(params.get_protocol()).to_str());
        if protocol != "authority-probe" {
            return Promise::err(capnp::Error::failed("unexpected vat protocol".into()));
        }
        self.calls.vat_dialer.set(self.calls.vat_dialer.get() + 1);
        let executor: system_capnp::executor::Client = capnp_rpc::new_client(CidExecutor {
            cid: "network-returned-executor".into(),
        });
        results
            .get()
            .init_cap()
            .set_as_capability(executor.client.hook);
        Promise::ok(())
    }
}

struct RecordingHttpListener {
    calls: Rc<NetworkCallCounts>,
}

#[allow(refining_impl_trait)]
impl system_capnp::http_listener::Server for RecordingHttpListener {
    fn listen(
        self: capnp::capability::Rc<Self>,
        params: system_capnp::http_listener::ListenParams,
        _results: system_capnp::http_listener::ListenResults,
    ) -> Promise<(), capnp::Error> {
        let params = capnp_rpc::pry!(params.get());
        if !params.has_membrane() {
            return Promise::err(capnp::Error::failed("missing delegated Membrane".into()));
        }
        capnp_rpc::pry!(params.get_executor());
        let prefix = capnp_rpc::pry!(capnp_rpc::pry!(params.get_prefix()).to_str());
        if prefix != "/authority-probe" {
            return Promise::err(capnp::Error::failed("unexpected HTTP prefix".into()));
        }
        self.calls
            .http_listener
            .set(self.calls.http_listener.get() + 1);
        Promise::ok(())
    }
}

struct CountingRuntime {
    calls: Rc<Cell<u32>>,
    cid: String,
}

#[allow(refining_impl_trait)]
impl system_capnp::runtime::Server for CountingRuntime {
    fn load(
        self: capnp::capability::Rc<Self>,
        _params: system_capnp::runtime::LoadParams,
        mut results: system_capnp::runtime::LoadResults,
    ) -> Promise<(), capnp::Error> {
        self.calls.set(self.calls.get() + 1);
        let executor: system_capnp::executor::Client = capnp_rpc::new_client(CidExecutor {
            cid: self.cid.clone(),
        });
        results.get().set_executor(executor);
        Promise::ok(())
    }
}

fn counting_runtime(cid: &str) -> (Grant, Rc<Cell<u32>>) {
    let calls = Rc::new(Cell::new(0));
    let runtime: system_capnp::runtime::Client = capnp_rpc::new_client(CountingRuntime {
        calls: calls.clone(),
        cid: cid.to_owned(),
    });
    (
        Grant {
            name: String::new(),
            cap: runtime.client,
        },
        calls,
    )
}

struct DropTrackedRuntime {
    dropped: Rc<Cell<bool>>,
}

impl Drop for DropTrackedRuntime {
    fn drop(&mut self) {
        self.dropped.set(true);
    }
}

impl system_capnp::runtime::Server for DropTrackedRuntime {}

struct GatedDropTrackedRuntime {
    dropped: Rc<Cell<bool>>,
    started: Arc<tokio::sync::Notify>,
    release: Arc<tokio::sync::Notify>,
}

struct PendingRequestDropGuard {
    dropped: Rc<Cell<bool>>,
}

impl Drop for PendingRequestDropGuard {
    fn drop(&mut self) {
        self.dropped.set(true);
    }
}

struct CancellationTrackedRuntime {
    request_dropped: Rc<Cell<bool>>,
    started: Arc<tokio::sync::Notify>,
}

struct GatedCidExecutor {
    started: Arc<tokio::sync::Notify>,
    release: Arc<tokio::sync::Notify>,
}

#[allow(refining_impl_trait)]
impl system_capnp::executor::Server for GatedCidExecutor {
    async fn cid(
        self: capnp::capability::Rc<Self>,
        _params: system_capnp::executor::CidParams,
        mut results: system_capnp::executor::CidResults,
    ) -> Result<(), capnp::Error> {
        self.started.notify_one();
        self.release.notified().await;
        results.get().set_cid(KNOWN_CID);
        Ok(())
    }
}

#[allow(refining_impl_trait)]
impl system_capnp::runtime::Server for CancellationTrackedRuntime {
    async fn load(
        self: capnp::capability::Rc<Self>,
        _params: system_capnp::runtime::LoadParams,
        _results: system_capnp::runtime::LoadResults,
    ) -> Result<(), capnp::Error> {
        let _request_drop_guard = PendingRequestDropGuard {
            dropped: self.request_dropped.clone(),
        };
        self.started.notify_one();
        std::future::pending::<()>().await;
        unreachable!("the pending request must end only when its Future is dropped")
    }
}

impl Drop for GatedDropTrackedRuntime {
    fn drop(&mut self) {
        self.dropped.set(true);
    }
}

#[allow(refining_impl_trait)]
impl system_capnp::runtime::Server for GatedDropTrackedRuntime {
    async fn load(
        self: capnp::capability::Rc<Self>,
        _params: system_capnp::runtime::LoadParams,
        mut results: system_capnp::runtime::LoadResults,
    ) -> Result<(), capnp::Error> {
        self.started.notify_one();
        self.release.notified().await;
        let executor: system_capnp::executor::Client = capnp_rpc::new_client(CidExecutor {
            cid: "record-pinned".into(),
        });
        results.get().set_executor(executor);
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
    delegated: system_capnp::runtime::Client,
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

fn names(report: &Value, delivery: &str) -> Vec<String> {
    report[delivery]
        .as_array()
        .unwrap_or_else(|| panic!("missing {delivery} names: {report}"))
        .iter()
        .map(|value| value.as_str().expect("cap name").to_owned())
        .collect()
}

fn present_authority_pointers(report: &Value) -> Vec<&'static str> {
    assert_eq!(
        report["ok"], true,
        "typed authority inspection failed: {report}"
    );
    let detail = &report["detail"];
    [
        ("stat", &detail["stat"]),
        (
            "network.stream.listener",
            &detail["network"]["stream"]["listener"],
        ),
        (
            "network.stream.dialer",
            &detail["network"]["stream"]["dialer"],
        ),
        (
            "network.vat.listener",
            &detail["network"]["vat"]["listener"],
        ),
        ("network.vat.dialer", &detail["network"]["vat"]["dialer"]),
        (
            "network.http.listener",
            &detail["network"]["http"]["listener"],
        ),
        ("network.http.dialer", &detail["network"]["http"]["dialer"]),
        ("routing.finder", &detail["routing"]["finder"]),
        ("routing.announcer", &detail["routing"]["announcer"]),
        ("runtime", &detail["runtime"]),
        ("authority", &detail["authority"]),
        ("identity", &detail["identity"]),
        ("ipfs", &detail["ipfs"]),
    ]
    .into_iter()
    .filter_map(|(path, value)| {
        value
            .as_bool()
            .unwrap_or_else(|| panic!("missing boolean presence for {path}: {report}"))
            .then_some(path)
    })
    .collect()
}

fn check_no_active_authority_pointers(report: &Value) -> Result<(), Vec<&'static str>> {
    let present = present_authority_pointers(report);
    if present.is_empty() {
        Ok(())
    } else {
        Err(present)
    }
}

fn assert_no_active_authority_pointers(report: &Value, context: &str) {
    if let Err(present) = check_no_active_authority_pointers(report) {
        panic!("{context} exposed typed authority pointers: {present:?}");
    }
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
        let identity = "alias-target";
        let (grant, calls) = counting_runtime(identity);
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
                        && entry["cid"] == identity
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
fn repeated_membrane_graft_is_extra_name_idempotent() {
    let wasm = probe_bytes();
    let local = tokio::task::LocalSet::new();
    local.block_on(&tokio::runtime::Runtime::new().unwrap(), async move {
        let harness = harness(&wasm).await;
        let report = probe_report(&harness.executor, "enumerate", &[], &[]).await;
        assert_eq!(names(&report, "first"), names(&report, "second"));
    });
}

#[test]
fn executor_spawn_forwards_the_exact_stateful_membrane_through_real_wasm() {
    let wasm = probe_bytes();
    let local = tokio::task::LocalSet::new();
    local.block_on(&tokio::runtime::Runtime::new().unwrap(), async move {
        let harness = harness(&wasm).await;

        let no_graft_calls = Rc::new(Cell::new(0));
        let no_graft = spawn_probe_with_membrane(
            &harness.executor,
            "no-graft",
            &[],
            stateful_test_membrane(no_graft_calls.clone()),
        )
        .await
        .expect("spawn no-graft probe");
        let stdout = no_graft
            .stdout_request()
            .send()
            .promise
            .await
            .expect("no-graft stdout")
            .get()
            .expect("no-graft stdout results")
            .get_stream()
            .expect("no-graft stdout stream");
        let output = read_all(stdout).await.expect("read no-graft output");
        let report: Value = serde_json::from_slice(&output).expect("no-graft JSON");
        assert_eq!(report["ok"], true);
        assert_eq!(
            no_graft_calls.get(),
            0,
            "Executor.spawn must not graft the supplied Membrane"
        );

        let graft_calls = Rc::new(Cell::new(0));
        let stateful = spawn_probe_with_membrane(
            &harness.executor,
            "stateful-graft",
            &[],
            stateful_test_membrane(graft_calls.clone()),
        )
        .await
        .expect("spawn stateful-graft probe");
        let stdout = stateful
            .stdout_request()
            .send()
            .promise
            .await
            .expect("stateful-graft stdout")
            .get()
            .expect("stateful-graft stdout results")
            .get_stream()
            .expect("stateful-graft stdout stream");
        let output = read_all(stdout).await.expect("read stateful-graft output");
        let report: Value = serde_json::from_slice(&output).expect("stateful-graft JSON");
        assert_eq!(report["ok"], true);
        assert_eq!(
            report["peer_ids"],
            serde_json::json!([b"stateful-peer-1", b"stateful-peer-2"])
        );
        assert_eq!(
            graft_calls.get(),
            2,
            "repeated child graft calls must reach the supplied Membrane"
        );
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
        let (mut grant, calls) = counting_runtime("parent");
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
fn request_owned_guest_server_future_can_call_back_into_the_host() {
    let wasm = probe_bytes();
    let local = tokio::task::LocalSet::new();
    local.block_on(&tokio::runtime::Runtime::new().unwrap(), async move {
        let harness = harness(&wasm).await;
        let process = spawn_probe(&harness.executor, "reentrant-callback", &[], &[])
            .await
            .expect("spawn reentrant P3 probe");
        let response = process
            .bootstrap_request()
            .send()
            .promise
            .await
            .expect("read reentrant probe bootstrap");
        let listener_cap = response
            .get()
            .expect("bootstrap results")
            .get_cap()
            .get_as_capability::<capnp::capability::Client>()
            .expect("bootstrap capability");
        let listener = system_capnp::vat_listener::Client::new(listener_cap.hook);
        let (callback, calls) = counting_runtime("reentrant-callback");

        let mut request = listener.serve_raw_request();
        request
            .get()
            .init_cap()
            .set_as_capability(callback.cap.hook);
        request.get().set_protocol("test-only");
        tokio::time::timeout(std::time::Duration::from_secs(5), request.send().promise)
            .await
            .expect("request-owned reentrant callback timed out")
            .expect("request-owned reentrant callback failed");
        assert_eq!(calls.get(), 1, "guest did not call the host capability");

        process
            .kill_request()
            .send()
            .promise
            .await
            .expect("kill reentrant probe");
        let wait = process
            .wait_request()
            .send()
            .promise
            .await
            .expect("wait for reentrant probe");
        assert_eq!(wait.get().expect("wait results").get_exit_code(), 137);
    });
}

#[test]
fn promised_and_broken_references_preserve_behavior_through_membrane_extras() {
    let wasm = probe_bytes();
    let local = tokio::task::LocalSet::new();
    local.block_on(&tokio::runtime::Runtime::new().unwrap(), async move {
        let harness = harness(&wasm).await;
        let promised_calls = Rc::new(Cell::new(0));
        let promised_cid = "promised-runtime".to_owned();
        let promised_calls_server = promised_calls.clone();
        let promised: system_capnp::runtime::Client = capnp_rpc::new_future_client(async move {
            tokio::task::yield_now().await;
            Ok(capnp_rpc::new_client(CountingRuntime {
                calls: promised_calls_server,
                cid: promised_cid,
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
            "promised reference did not resolve through Membrane.extras: {promised_report}"
        );
        assert_eq!(promised_calls.get(), 1);

        let broken: system_capnp::runtime::Client = capnp_rpc::new_future_client(async {
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
fn attenuated_runtime_recursively_denies_returned_executor_through_real_wasm() {
    let wasm = probe_bytes();
    let local = tokio::task::LocalSet::new();
    local.block_on(&tokio::runtime::Runtime::new().unwrap(), async move {
        let harness = harness(&wasm).await;
        let calls = Rc::new(Cell::new(0));
        let runtime: system_capnp::runtime::Client = capnp_rpc::new_client(CountingRuntime {
            calls: calls.clone(),
            cid: "attenuated".into(),
        });
        let policy = membrane::MethodProfile::<system_capnp::runtime::Client>::new()
            .allow_method(system_capnp::runtime::Client::load_request)
            .expect("capture Runtime.load method")
            .build();
        let attenuated = membrane::membrane(runtime, Rc::new(policy));

        let report = probe_report_with_authority(
            &harness.executor,
            "attenuated",
            &[],
            FixedAuthority {
                runtime: Some(attenuated),
                ..FixedAuthority::default()
            },
            &[],
        )
        .await;
        assert_eq!(report["ok"], true, "attenuation probe failed: {report}");
        assert_eq!(report["detail"]["extras"], serde_json::json!([]));
        assert_eq!(report["detail"]["executor_returned"], true);
        assert!(
            report["detail"]["cid_denied"]
                .as_str()
                .is_some_and(|error| error.contains(membrane::DENIED_MARKER)),
            "recursive Executor denial must retain its stable class: {report}"
        );
        assert!(
            report["detail"]["shutdown_denied"]
                .as_str()
                .is_some_and(|error| error.contains(membrane::DENIED_MARKER)),
            "Runtime denial must retain its stable class: {report}"
        );
        assert_eq!(
            calls.get(),
            1,
            "only the allowed Runtime.load reached the server"
        );
    });
}

#[test]
fn typed_runtime_and_executor_extra_remain_distinct() {
    let wasm = probe_bytes();
    let local = tokio::task::LocalSet::new();
    local.block_on(&tokio::runtime::Runtime::new().unwrap(), async move {
        let harness = harness(&wasm).await;
        let runtime = create_runtime_client(
            false,
            fixed_epoch_zero_guard(),
            None,
            None,
            CachePolicy::Isolated,
        );
        let report = probe_report_with_authority(
            &harness.executor,
            "trusted-lattice",
            &[("WW_PROBE_IMAGE", "runtime-selected-image")],
            FixedAuthority {
                runtime: Some(runtime),
                ..FixedAuthority::default()
            },
            &[Grant {
                name: "bound-executor".into(),
                cap: harness.executor.clone().client,
            }],
        )
        .await;
        assert_eq!(report["ok"], true, "multi-grant probe failed: {report}");
        assert_eq!(
            report["detail"]["extras"],
            serde_json::json!(["bound-executor"])
        );
        assert_eq!(report["detail"]["runtime"], true);
        assert_eq!(report["detail"]["different_images"], true);
    });
}

#[test]
fn late_delegation_uses_explicit_conduit_without_mutating_birth_set() {
    let wasm = probe_bytes();
    let local = tokio::task::LocalSet::new();
    local.block_on(&tokio::runtime::Runtime::new().unwrap(), async move {
        let harness = harness(&wasm).await;
        let (mut delegated_grant, delegated_calls) = counting_runtime("late-x");
        delegated_grant.name = "delegated-x".into();
        let delegated = system_capnp::runtime::Client::new(delegated_grant.cap.hook);
        let conduit_calls = Rc::new(Cell::new(0));
        let vat_client: system_capnp::vat_client::Client = capnp_rpc::new_client(LateVatClient {
            delegated,
            calls: conduit_calls.clone(),
        });
        let report = probe_report(
            &harness.executor,
            "late-delegation",
            &[],
            &[Grant {
                name: "mailbox".into(),
                cap: vat_client.client,
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
            "Membrane.graft() must retain the immutable birth extras"
        );
        assert_eq!(
            report["detail"]["delegated_cid"],
            serde_json::json!("late-x")
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
                .is_some_and(|error| error.contains("runtime is withheld")),
            "withheld typed runtime must be a clear guest-level failure: {absent}"
        );

        let present = probe_report_with_authority(
            &harness.executor,
            "invoke",
            &[("WW_PROBE_CAP", "runtime")],
            FixedAuthority {
                runtime: Some(create_runtime_client(
                    false,
                    fixed_epoch_zero_guard(),
                    None,
                    None,
                    CachePolicy::Isolated,
                )),
                ..FixedAuthority::default()
            },
            &[],
        )
        .await;
        assert_eq!(
            present["ok"], true,
            "an explicitly granted Runtime must retain normal Cap'n Proto behavior: {present}"
        );
    });
}

#[test]
fn listener_presence_check_rejects_a_leak_without_bound_executor() {
    let wasm = probe_bytes();
    let local = tokio::task::LocalSet::new();
    local.block_on(&tokio::runtime::Runtime::new().unwrap(), async move {
        let harness = harness(&wasm).await;
        let calls = Rc::new(NetworkCallCounts::default());
        let fixed = FixedAuthority {
            stream_listener: Some(capnp_rpc::new_client(RecordingStreamListener {
                calls: calls.clone(),
            })),
            ..FixedAuthority::default()
        };

        let behavior = probe_report_with_authority(
            &harness.executor,
            "invoke",
            &[("WW_PROBE_CAP", "stream-listener")],
            fixed.clone(),
            &[],
        )
        .await;
        assert_eq!(
            behavior["ok"], false,
            "the behavior probe must lack its bound-executor dependency: {behavior}"
        );
        assert!(
            behavior["error"]
                .as_str()
                .is_some_and(|error| error.contains("bound-executor")),
            "the behavior probe must fail after finding the leaked listener: {behavior}"
        );
        assert_eq!(calls.stream_listener.get(), 0);

        let presence =
            probe_report_with_authority(&harness.executor, "inspect-authority", &[], fixed, &[])
                .await;
        assert_eq!(
            check_no_active_authority_pointers(&presence),
            Err(vec!["network.stream.listener"]),
            "the pointer-presence check must reject the leaked listener"
        );
    });
}

#[test]
fn typed_network_capabilities_are_callable_through_real_wasm() {
    let wasm = probe_bytes();
    let local = tokio::task::LocalSet::new();
    local.block_on(&tokio::runtime::Runtime::new().unwrap(), async move {
        let harness = harness(&wasm).await;
        let calls = Rc::new(NetworkCallCounts::default());
        let fixed = FixedAuthority {
            stream_listener: Some(capnp_rpc::new_client(RecordingStreamListener {
                calls: calls.clone(),
            })),
            stream_dialer: Some(capnp_rpc::new_client(RecordingStreamDialer {
                calls: calls.clone(),
            })),
            vat_listener: Some(capnp_rpc::new_client(RecordingVatListener {
                calls: calls.clone(),
            })),
            vat_dialer: Some(capnp_rpc::new_client(RecordingVatDialer {
                calls: calls.clone(),
            })),
            http_listener: Some(capnp_rpc::new_client(RecordingHttpListener {
                calls: calls.clone(),
            })),
            http_dialer: Some(capnp_rpc::new_client(
                ww::rpc::http_client::EpochGuardedHttpProxy::new(
                    vec!["*".into()],
                    fixed_epoch_zero_guard(),
                ),
            )),
            ..FixedAuthority::default()
        };
        let executor: system_capnp::executor::Client = capnp_rpc::new_client(CidExecutor {
            cid: "bound-network-executor".into(),
        });
        let bound_executor = Grant {
            name: "bound-executor".into(),
            cap: executor.client,
        };

        for name in [
            "stream-listener",
            "stream-dialer",
            "vat-listener",
            "vat-dialer",
            "http-listener",
            "http-dialer",
        ] {
            let report = probe_report_with_authority(
                &harness.executor,
                "invoke",
                &[
                    ("WW_PROBE_CAP", name),
                    ("WW_PROBE_HTTP_URL", &harness.backend_url),
                ],
                fixed.clone(),
                std::slice::from_ref(&bound_executor),
            )
            .await;
            assert_eq!(
                report["ok"], true,
                "typed network capability {name} was not callable: {report}"
            );
            assert_eq!(report["detail"]["rpc_reached"], true);
            if name == "stream-dialer" {
                assert_eq!(report["detail"]["read_bytes"], 1);
            }
            if name == "vat-dialer" {
                assert_eq!(
                    report["detail"]["executor_cid"],
                    "network-returned-executor"
                );
            }
        }

        assert_eq!(calls.stream_listener.get(), 1);
        assert_eq!(calls.stream_dialer.get(), 1);
        assert_eq!(calls.vat_listener.get(), 1);
        assert_eq!(calls.vat_dialer.get(), 1);
        assert_eq!(calls.http_listener.get(), 1);
        assert_eq!(
            harness.backend_counts.http.get(),
            1,
            "typed HTTP dialer must reach the test-local backend exactly once"
        );
    });
}

#[test]
fn parent_local_drop_does_not_revoke_membrane_pinned_authority() {
    let wasm = probe_bytes();
    let local = tokio::task::LocalSet::new();
    local.block_on(&tokio::runtime::Runtime::new().unwrap(), async move {
        let harness = harness(&wasm).await;
        let dropped = Rc::new(Cell::new(false));
        let started = Arc::new(tokio::sync::Notify::new());
        let release = Arc::new(tokio::sync::Notify::new());
        let runtime: system_capnp::runtime::Client =
            capnp_rpc::new_client(GatedDropTrackedRuntime {
                dropped: dropped.clone(),
                started: started.clone(),
                release: release.clone(),
            });
        let grant = Grant {
            name: "tracked".into(),
            cap: runtime.client,
        };

        let call_started = started.notified();
        let process = spawn_probe(
            &harness.executor,
            "invoke",
            &[("WW_PROBE_CAP", "tracked")],
            std::slice::from_ref(&grant),
        )
        .await
        .expect("spawn membrane-pinned child");
        call_started.await;

        drop(grant);
        assert!(
            !dropped.get(),
            "dropping the parent's local reference must not revoke the child's Membrane"
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
        let output = read_all(stdout).await.expect("read membrane-pinned probe");
        let report: Value = serde_json::from_slice(&output).expect("membrane-pinned JSON");
        assert_eq!(
            report["ok"], true,
            "membrane-pinned invocation failed: {report}"
        );
        assert_eq!(report["detail"]["cid"], serde_json::json!("record-pinned"));
    });
}

#[test]
fn process_kill_tears_down_store_with_pending_real_p3_rpc_request() {
    let wasm = probe_bytes();
    let local = tokio::task::LocalSet::new();
    local.block_on(&tokio::runtime::Runtime::new().unwrap(), async move {
        let eof_fallbacks_before = ww::launcher::rpc_eof_fallback_count();
        let harness = harness(&wasm).await;
        let request_dropped = Rc::new(Cell::new(false));
        let started = Arc::new(tokio::sync::Notify::new());
        let runtime: system_capnp::runtime::Client =
            capnp_rpc::new_client(CancellationTrackedRuntime {
                request_dropped: request_dropped.clone(),
                started: started.clone(),
            });
        let grant = Grant {
            name: "tracked".into(),
            cap: runtime.client,
        };

        let call_started = started.notified();
        let process = spawn_probe(
            &harness.executor,
            "invoke",
            &[("WW_PROBE_CAP", "tracked")],
            std::slice::from_ref(&grant),
        )
        .await
        .expect("spawn P3 request-cancellation probe");
        call_started.await;
        drop(grant);

        tokio::time::timeout(
            std::time::Duration::from_secs(5),
            process.kill_request().send().promise,
        )
        .await
        .expect("process.kill exceeded the cancellation bound")
        .expect("process.kill RPC failed");

        let wait = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            process.wait_request().send().promise,
        )
        .await
        .expect("P3 process teardown exceeded the cancellation bound")
        .expect("process.wait RPC failed");
        assert_eq!(
            wait.get().expect("wait results").get_exit_code(),
            137,
            "process.kill must select the production killed-process outcome"
        );

        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            while !request_dropped.get() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("the pending host RPC Future survived process teardown");
        assert_eq!(
            ww::launcher::rpc_eof_fallback_count(),
            eof_fallbacks_before,
            "normal Store teardown must let the host RpcSystem observe EOF before fallback"
        );
    });
}

#[test]
fn epoch_revocation_rejects_an_in_flight_production_p3_capability_call() {
    let wasm = probe_bytes();
    let local = tokio::task::LocalSet::new();
    local.block_on(&tokio::runtime::Runtime::new().unwrap(), async move {
        let harness = harness(&wasm).await;
        let epoch = authority::Epoch {
            seq: 1,
            head: b"revocable-host".to_vec(),
            root: None,
        };
        let (epoch_tx, epoch_rx) = watch::channel(epoch);
        let guard = authority::EpochGuard {
            issued_seq: 1,
            receiver: epoch_rx,
        };
        let registry = ww::dispatcher::server::new_registry();
        let http_listener: system_capnp::http_listener::Client = capnp_rpc::new_client(
            ww::rpc::http_listener::HttpListenerImpl::new(guard, registry.clone()),
        );

        let started = Arc::new(tokio::sync::Notify::new());
        let release = Arc::new(tokio::sync::Notify::new());
        let executor: system_capnp::executor::Client = capnp_rpc::new_client(GatedCidExecutor {
            started: started.clone(),
            release: release.clone(),
        });
        let grants = [Grant {
            name: "bound-executor".into(),
            cap: executor.client,
        }];

        let call_started = started.notified();
        let process = spawn_probe_with_authority(
            &harness.executor,
            "epoch-http-listen",
            &[],
            FixedAuthority {
                http_listener: Some(http_listener),
                ..FixedAuthority::default()
            },
            &grants,
        )
        .await
        .expect("spawn epoch-revocation P3 probe");
        tokio::time::timeout(std::time::Duration::from_secs(5), call_started)
            .await
            .expect("HttpListener CID preflight did not become pending");

        epoch_tx.send_replace(authority::Epoch {
            seq: 2,
            head: b"replacement-host".to_vec(),
            root: None,
        });
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
        let output = tokio::time::timeout(std::time::Duration::from_secs(5), read_all(stdout))
            .await
            .expect("epoch-revocation P3 probe timed out")
            .expect("read epoch-revocation probe");
        let report: Value = serde_json::from_slice(&output).expect("epoch-revocation JSON");
        assert_eq!(
            report["ok"], false,
            "stale call unexpectedly succeeded: {report}"
        );
        assert!(
            report["error"]
                .as_str()
                .is_some_and(|error| error.contains("staleEpoch")),
            "pending call did not fail with staleEpoch: {report}"
        );
        assert!(
            registry.read().expect("route registry lock").is_empty(),
            "stale in-flight listen call installed a route"
        );
    });
}

#[test]
fn child_exit_releases_membrane_owned_extra_references() {
    let wasm = probe_bytes();
    let local = tokio::task::LocalSet::new();
    local.block_on(&tokio::runtime::Runtime::new().unwrap(), async move {
        let harness = harness(&wasm).await;
        let dropped = Rc::new(Cell::new(false));
        let runtime: system_capnp::runtime::Client = capnp_rpc::new_client(DropTrackedRuntime {
            dropped: dropped.clone(),
        });
        let grant = Grant {
            name: "tracked".into(),
            cap: runtime.client,
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
            "child exit must release Membrane and RPC references even while the Process handle remains"
        );
    });
}

#[test]
fn current_minimal_membrane_substrate_characterization() {
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
        let ticked = TickedExecutor::new();
        let runtime = create_runtime_client_with_pinset(
            false,
            fixed_epoch_zero_guard(),
            Some(ticked.engine()),
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
        let authority = probe_report(&executor, "inspect-authority", &[], &[]).await;
        assert_no_active_authority_pointers(&authority, "path-only CID reader Membrane");

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
fn minimal_membrane_child_receives_only_required_peer_metadata() {
    let wasm = probe_bytes();
    let local = tokio::task::LocalSet::new();
    local.block_on(&tokio::runtime::Runtime::new().unwrap(), async move {
        let harness = harness(&wasm).await;
        let peer = probe_report(
            &harness.executor,
            "invoke",
            &[("WW_PROBE_CAP", "peer-id")],
            &[],
        )
        .await;
        assert_eq!(peer["ok"], true, "required peerId was unavailable: {peer}");
        assert_eq!(peer["detail"]["peer_id"], serde_json::json!(b"test-peer"));

        let authority = probe_report(&harness.executor, "inspect-authority", &[], &[]).await;
        assert_no_active_authority_pointers(&authority, "minimal Membrane");
        assert_eq!(
            harness.backend_counts.http.get(),
            0,
            "minimal authority must not reach the test-local HTTP backend"
        );
        assert_eq!(
            harness.backend_counts.ipfs.get(),
            0,
            "minimal authority must not reach the test-local IPFS backend"
        );
    });
}

#[test]
fn minimal_membrane_child_cannot_discover_or_announce_providers() {
    let wasm = probe_bytes();
    let local = tokio::task::LocalSet::new();
    local.block_on(&tokio::runtime::Runtime::new().unwrap(), async move {
        let harness = harness(&wasm).await;
        for mode in ["routing-finder", "routing-announcer"] {
            let report = probe_report(&harness.executor, mode, &[], &[]).await;
            assert_ne!(
                report["ok"], true,
                "minimal Membrane child used {mode} authority: {report}"
            );
        }
    });
}

#[test]
fn finder_only_can_discover_but_cannot_use_announcer() {
    let wasm = probe_bytes();
    let local = tokio::task::LocalSet::new();
    local.block_on(&tokio::runtime::Runtime::new().unwrap(), async move {
        let harness = harness(&wasm).await;
        let routing = ww::rpc::routing::LocalProviderRouting::new();
        routing.provide_as(
            KNOWN_CID,
            ww::rpc::PeerInfo {
                peer_id: vec![1, 2, 3],
                addrs: vec![vec![4, 5, 6]],
            },
        );
        let finder: ww::routing_capnp::finder::Client = capnp_rpc::new_client(routing.finder());
        let report = probe_report_with_authority(
            &harness.executor,
            "routing-finder",
            &[],
            FixedAuthority {
                routing_finder: Some(finder),
                ..FixedAuthority::default()
            },
            &[],
        )
        .await;
        assert_eq!(report["ok"], true, "Finder probe failed: {report}");
        assert_eq!(report["detail"]["find_providers"], true);
        assert_eq!(report["detail"]["providers"], 1);
        assert_eq!(report["detail"]["done"], true);
        assert_eq!(report["detail"]["announcer_withheld"], true);
    });
}

#[test]
fn announcer_only_can_announce_but_cannot_use_finder() {
    let wasm = probe_bytes();
    let local = tokio::task::LocalSet::new();
    local.block_on(&tokio::runtime::Runtime::new().unwrap(), async move {
        let harness = harness(&wasm).await;
        let routing = ww::rpc::routing::LocalProviderRouting::new();
        let announcer: ww::routing_capnp::announcer::Client =
            capnp_rpc::new_client(routing.announcer());
        let report = probe_report_with_authority(
            &harness.executor,
            "routing-announcer",
            &[],
            FixedAuthority {
                routing_announcer: Some(announcer),
                ..FixedAuthority::default()
            },
            &[],
        )
        .await;
        assert_eq!(report["ok"], true, "Announcer probe failed: {report}");
        assert_eq!(report["detail"]["provide"], true);
        assert_eq!(report["detail"]["finder_withheld"], true);
    });
}

#[test]
fn finder_and_announcer_are_available_only_when_both_are_explicitly_granted() {
    let wasm = probe_bytes();
    let local = tokio::task::LocalSet::new();
    local.block_on(&tokio::runtime::Runtime::new().unwrap(), async move {
        let harness = harness(&wasm).await;
        let routing = ww::rpc::routing::LocalProviderRouting::new();
        routing.provide_as(
            KNOWN_CID,
            ww::rpc::PeerInfo {
                peer_id: vec![1, 2, 3],
                addrs: vec![vec![4, 5, 6]],
            },
        );
        let finder: ww::routing_capnp::finder::Client = capnp_rpc::new_client(routing.finder());
        let announcer: ww::routing_capnp::announcer::Client =
            capnp_rpc::new_client(routing.announcer());
        let report = probe_report_with_authority(
            &harness.executor,
            "routing-both",
            &[],
            FixedAuthority {
                routing_finder: Some(finder),
                routing_announcer: Some(announcer),
                ..FixedAuthority::default()
            },
            &[],
        )
        .await;
        assert_eq!(
            report["ok"], true,
            "combined routing probe failed: {report}"
        );
        assert_eq!(report["detail"]["provide"], true);
        assert_eq!(report["detail"]["find_providers"], true);
        assert_eq!(report["detail"]["providers"], 1);
        assert_eq!(report["detail"]["done"], true);
        assert_eq!(
            report["detail"]["typed_fields"],
            serde_json::json!(["finder", "announcer"])
        );
    });
}

#[test]
fn membrane_extras_equal_supplied_set() {
    let wasm = probe_bytes();
    let local = tokio::task::LocalSet::new();
    local.block_on(&tokio::runtime::Runtime::new().unwrap(), async move {
        let harness = harness(&wasm).await;
        let (mut only, _calls) = counting_runtime("only");
        only.name = "only-grant".into();
        let report = probe_report(&harness.executor, "enumerate", &[], &[only]).await;
        assert_eq!(names(&report, "first"), vec!["only-grant"]);
        assert_eq!(names(&report, "second"), vec!["only-grant"]);
    });
}

#[test]
fn path_like_extra_labels_round_trip_through_real_wasm() {
    let wasm = probe_bytes();
    let local = tokio::task::LocalSet::new();
    local.block_on(&tokio::runtime::Runtime::new().unwrap(), async move {
        let harness = harness(&wasm).await;
        let (grant, _calls) = counting_runtime("wire");
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
fn ordinary_child_bootstrap_separates_typed_authority_from_dynamic_extras() {
    let wasm = probe_bytes();
    let local = tokio::task::LocalSet::new();
    local.block_on(&tokio::runtime::Runtime::new().unwrap(), async move {
        let harness = harness(&wasm).await;
        let (mut extra, _calls) = counting_runtime("extra");
        extra.name = "application-cap".into();

        let peer = probe_report_with_authority(
            &harness.executor,
            "invoke",
            &[("WW_PROBE_CAP", "peer-id")],
            FixedAuthority {
                peer_id: Some(b"typed-peer".to_vec()),
                ..FixedAuthority::default()
            },
            std::slice::from_ref(&extra),
        )
        .await;
        assert_eq!(peer["ok"], true, "typed peerId was not usable: {peer}");
        assert_eq!(peer["detail"]["peer_id"], serde_json::json!(b"typed-peer"));

        let extras = probe_report_with_authority(
            &harness.executor,
            "enumerate",
            &[],
            FixedAuthority {
                peer_id: Some(b"typed-peer".to_vec()),
                ..FixedAuthority::default()
            },
            &[extra],
        )
        .await;
        assert_eq!(names(&extras, "first"), vec!["application-cap"]);
        assert_eq!(names(&extras, "second"), vec!["application-cap"]);
    });
}

#[test]
fn restricted_executor_cannot_amplify_descendant() {
    let wasm = probe_bytes();
    let local = tokio::task::LocalSet::new();
    local.block_on(&tokio::runtime::Runtime::new().unwrap(), async move {
        let harness = harness(&wasm).await;
        let (mut narrow, calls) = counting_runtime("descendant-narrow");
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
        assert_no_active_authority_pointers(
            &report["detail"]["omitted"],
            "restricted Executor descendant",
        );
    });
}

#[test]
fn fixed_epoch_zero_runtime_has_no_raw_runtime_bootstrap_fallback() {
    let wasm = probe_bytes();
    let local = tokio::task::LocalSet::new();
    local.block_on(&tokio::runtime::Runtime::new().unwrap(), async move {
        let (executor, _ticked) = fixed_epoch_zero_executor(&wasm).await;
        let report = probe_report(&executor, "raw-runtime", &[], &[]).await;
        assert_ne!(
            report["ok"], true,
            "alternate constructor exposed usable raw Runtime: {report}"
        );
    });
}
