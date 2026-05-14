//! End-to-end test for the status cell via the HttpListener dispatch chain.
//!
//! Sibling to `tests/status_cell_e2e.rs`. The other test spawns the WASM
//! directly through Runtime/Executor; this one routes through the full
//! HttpListener path:
//!
//!   HttpListener.listen(executor, "/status", caps)
//!     └─ registers route in RouteRegistry
//!         └─ dispatch_loop receives CgiRequest via mpsc
//!             └─ spawn_and_run calls executor.spawn(env, caps)
//!                 └─ WAGI cell grafts membrane, returns JSON
//!
//! The test additionally seeds a non-empty caps list (kernel emits this
//! when an init.d author wraps `(perform host :listen ...)` in a `with`
//! block — e.g. `(with [(host (perform host :host))] ...)`). The status
//! cell ignores extras and uses the default-graft `host` cap, but the
//! dispatcher path must not drop or corrupt the caps in flight. A
//! regression that breaks caps forwarding (e.g. the prior
//! `// TODO: thread _caps` regression that #429 closed) would surface
//! here as `peer_id: null` or a CGI dispatch failure.
//!
//! Requires pre-built status WASM: `make -C std/status`.

use tokio::sync::{mpsc, oneshot, watch};

use ww::dispatcher::server::{new_registry, CgiRequest};
use ww::launcher::create_runtime_client;
use ww::rpc::{CachePolicy, NetworkState};
use ww::system_capnp;

const STATUS_WASM_PATH: &str = "std/status/bin/status.wasm";

fn status_wasm_exists() -> bool {
    std::path::Path::new(STATUS_WASM_PATH).exists()
}

fn synth_peer_id_bytes() -> Vec<u8> {
    let kp = libp2p::identity::Keypair::generate_ed25519();
    libp2p::PeerId::from_public_key(&kp.public()).to_bytes()
}

/// Stand-in capnp client used as the named cap value in the caps list.
/// Any client works for shape validation — the status cell ignores extras.
struct StubExecutor;

#[allow(refining_impl_trait)]
impl system_capnp::executor::Server for StubExecutor {
    fn spawn(
        self: capnp::capability::Rc<Self>,
        _params: system_capnp::executor::SpawnParams,
        _results: system_capnp::executor::SpawnResults,
    ) -> capnp::capability::Promise<(), capnp::Error> {
        capnp::capability::Promise::err(capnp::Error::failed("stub executor".into()))
    }
}

#[tokio::test(flavor = "current_thread")]
async fn status_cell_via_http_listener_with_extra_caps_returns_non_null_peer_id() {
    if !status_wasm_exists() {
        eprintln!("skipping: {STATUS_WASM_PATH} not built (run `make -C std/status` first)");
        return;
    }

    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            // ── Runtime + executor + HttpListener wiring ────────────────
            let network_state = NetworkState::new();
            let peer_id_bytes = synth_peer_id_bytes();
            network_state.set_local_peer_id(peer_id_bytes.clone()).await;

            let epoch = membrane::Epoch {
                seq: 1,
                head: vec![],
                provenance: membrane::Provenance::Block(0),
            };
            let (_epoch_tx, epoch_rx) = watch::channel(epoch);
            let guard = membrane::EpochGuard {
                issued_seq: 1,
                receiver: epoch_rx.clone(),
            };
            let stream_control = libp2p_stream::Behaviour::new().new_control();

            let (swarm_tx, _swarm_rx) = mpsc::channel(16);
            let runtime = create_runtime_client(
                network_state,
                swarm_tx,
                false,
                Some(guard.clone()),
                Some(epoch_rx.clone()),
                None,
                Some(stream_control),
                None,
                None,
                CachePolicy::Shared,
                ww::ipfs::HttpClient::new("http://localhost:5001".into()),
                Vec::new(),
            );

            // Load the status WASM, get an executor.
            let wasm = std::fs::read(STATUS_WASM_PATH).expect("read status.wasm");
            let mut load_req = runtime.load_request();
            load_req.get().set_wasm(&wasm);
            let load_resp = load_req.send().promise.await.expect("runtime.load");
            let executor = load_resp
                .get()
                .expect("load resp")
                .get_executor()
                .expect("get executor");

            // Construct an HttpListener client backed by an in-process registry.
            let route_registry = new_registry();
            let listener_impl =
                ww::rpc::http_listener::HttpListenerImpl::new(guard, route_registry.clone());
            let listener: system_capnp::http_listener::Client =
                capnp_rpc::new_client(listener_impl);

            // Register the route, with a non-empty caps list (mirrors what
            // the kernel emits for `(with [(extra-cap ...)] (perform host
            // :listen status "/status"))`).
            let mut listen_req = listener.listen_request();
            listen_req.get().set_executor(executor);
            listen_req.get().set_prefix("/status");
            {
                let mut caps_builder = listen_req.get().init_caps(1);
                let mut entry = caps_builder.reborrow().get(0);
                entry.set_name("test-extra");
                let placeholder: system_capnp::executor::Client =
                    capnp_rpc::new_client(StubExecutor);
                entry.init_cap().set_as_capability(placeholder.client.hook);
            }
            listen_req
                .send()
                .promise
                .await
                .expect("HttpListener.listen with caps should succeed");

            // ── Dispatch a CGI request through the registry ─────────────
            let tx = {
                let routes = route_registry.read().expect("registry read lock");
                routes
                    .get("/status")
                    .cloned()
                    .expect("route /status should be registered")
            };
            let (response_tx, response_rx) = oneshot::channel();
            let cgi_req = CgiRequest {
                method: "GET".into(),
                path: "/status".into(),
                query: String::new(),
                headers: Vec::new(),
                body: Vec::new(),
                verified_snap: None,
                response_tx,
            };
            tx.send(cgi_req)
                .await
                .expect("CgiRequest should send through route channel");

            let cgi_resp = tokio::time::timeout(std::time::Duration::from_secs(20), response_rx)
                .await
                .expect("dispatch should respond within 20s")
                .expect("response_rx not dropped");

            assert_eq!(cgi_resp.status, 200, "expected HTTP 200");

            let body = std::str::from_utf8(&cgi_resp.body).expect("UTF-8 body");
            let json: serde_json::Value = serde_json::from_str(body)
                .unwrap_or_else(|e| panic!("response should parse as JSON: {e}\nbody: {body}"));

            assert_eq!(json["status"], "ok");
            assert!(
                json["version"].as_str().is_some_and(|s| !s.is_empty()),
                "version should be a non-empty string"
            );

            let peer_id = json["peer_id"]
                .as_str()
                .unwrap_or_else(|| panic!("peer_id MUST be non-null. body: {body}"));
            assert!(
                peer_id.starts_with("12D") || peer_id.starts_with("Qm"),
                "peer_id should look like a libp2p base58 PeerID, got: {peer_id:?}"
            );
        })
        .await;
}
