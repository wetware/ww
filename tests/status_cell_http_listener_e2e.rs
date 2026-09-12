//! End-to-end test for the status cell via the HttpListener dispatch chain.
//!
//! Sibling to `tests/status_cell_e2e.rs`. The other test spawns the WASM
//! directly through Runtime/Executor; this one routes through the full
//! HttpListener path:
//!
//!   HttpListener.listen(executor, "/status", membrane)
//!     └─ registers route in RouteRegistry
//!         └─ dispatch_loop receives CgiRequest via mpsc
//!             └─ spawn_and_run calls executor.spawn(env, membrane)
//!                 └─ WAGI cell grafts membrane, returns JSON
//!
//! The test supplies the narrow peerId+Stat Membrane that status requires.
//! The dispatcher must forward that exact Membrane to each request child.
//!
//! Requires pre-built status WASM: `make -C std/status`.

#[path = "support/ticked_executor.rs"]
mod ticked_executor;

use capnp::capability::Promise;
use tokio::sync::{oneshot, watch};

use ww::dispatcher::server::{new_registry, CgiRequest};
use ww::launcher::create_runtime_client;
use ww::rpc::{CachePolicy, NetworkState};
use ww::system_capnp;

use ticked_executor::TickedExecutor;

const STATUS_WASM_PATH: &str = "std/status/bin/status.wasm";

fn status_wasm_exists() -> bool {
    std::path::Path::new(STATUS_WASM_PATH).exists()
}

fn synth_peer_id_bytes() -> Vec<u8> {
    let kp = libp2p::identity::Keypair::generate_ed25519();
    libp2p::PeerId::from_public_key(&kp.public()).to_bytes()
}

#[derive(Clone)]
struct StatusGraft {
    network_state: NetworkState,
}

struct StatusStat {
    network_state: NetworkState,
    guard: authority::EpochGuard,
}

#[allow(refining_impl_trait)]
impl system_capnp::stat::Server for StatusStat {
    fn snapshot(
        self: capnp::capability::Rc<Self>,
        _params: system_capnp::stat::SnapshotParams,
        mut results: system_capnp::stat::SnapshotResults,
    ) -> Promise<(), capnp::Error> {
        if let Err(error) = self.guard.check() {
            return Promise::err(error);
        }
        let network_state = self.network_state.clone();
        let guard = self.guard.clone();
        Promise::from_future(async move {
            let snapshot = network_state.snapshot().await;
            guard.check()?;
            let mut stat = results.get().init_stat();
            let mut addrs = stat
                .reborrow()
                .init_listen_addrs(snapshot.listen_addrs.len() as u32);
            for (index, address) in snapshot.listen_addrs.iter().enumerate() {
                addrs.set(index as u32, address);
            }
            stat.set_connected_peer_count(snapshot.connected_peer_count);
            Ok(())
        })
    }
}

impl authority::GraftBuilder for StatusGraft {
    fn build(
        &self,
        guard: &authority::EpochGuard,
        mut builder: system_capnp::membrane::graft_results::Builder<'_>,
    ) -> Result<(), capnp::Error> {
        builder.set_peer_id(self.network_state.local_peer_id());
        let stat: system_capnp::stat::Client = capnp_rpc::new_client(StatusStat {
            network_state: self.network_state.clone(),
            guard: guard.clone(),
        });
        builder.set_stat(stat);
        Ok(())
    }
}

#[tokio::test(flavor = "current_thread")]
async fn status_cell_via_http_listener_with_narrow_membrane_returns_status() {
    if !status_wasm_exists() {
        eprintln!("skipping: {STATUS_WASM_PATH} not built (run `make -C std/status` first)");
        return;
    }

    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            // ── Runtime + executor + HttpListener wiring ────────────────
            let peer_id_bytes = synth_peer_id_bytes();
            let network_state = NetworkState::from_peer_id(peer_id_bytes.clone());

            let epoch = authority::Epoch {
                seq: 1,
                head: vec![],
                root: None,
            };
            let (_epoch_tx, epoch_rx) = watch::channel(epoch);
            let guard = authority::EpochGuard {
                issued_seq: 1,
                receiver: epoch_rx.clone(),
            };
            // This dispatch path executes the real status Cell, so it must use
            // the same shared, ticked Engine as production.
            let ticked = TickedExecutor::new();
            let runtime = create_runtime_client(
                false,
                guard.clone(),
                Some(ticked.engine()),
                None,
                CachePolicy::Shared,
            );
            let membrane: system_capnp::membrane::Client = capnp_rpc::new_client(
                authority::MembraneServer::new(epoch_rx, StatusGraft { network_state }),
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

            // Register the route with the status child's narrow Membrane.
            let mut listen_req = listener.listen_request();
            listen_req.get().set_executor(executor);
            listen_req.get().set_prefix("/status");
            listen_req.get().set_membrane(membrane);
            listen_req
                .send()
                .promise
                .await
                .expect("HttpListener.listen with narrow Membrane should succeed");

            // ── Dispatch a CGI request through the registry ─────────────
            let tx = {
                let routes = route_registry.read().expect("registry read lock");
                routes
                    .get("/status")
                    .map(|entry| entry.sender())
                    .expect("route /status should be registered")
            };
            let (response_tx, response_rx) = oneshot::channel();
            let cgi_req = CgiRequest {
                method: "GET".into(),
                path: "/status".into(),
                query: String::new(),
                headers: Vec::new(),
                body: Vec::new(),
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
