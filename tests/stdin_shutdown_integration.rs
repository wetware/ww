//! Integration tests for stdin-as-shutdown-signal semantics.
//!
//! Two levels of coverage:
//!
//! 1. **Mechanism test:** closing stdin causes an echo cell to see EOF and exit.
//!    Proves the WASI stdin pipe actually delivers the close signal end-to-end.
//!
//! 2. **VatListener bridge test:** `handle_vat_connection` closes stdin when the
//!    peer disconnects. Uses an in-memory duplex stream instead of libp2p, calling
//!    the real function with the generic stream parameter.
//!
//! Both tests require pre-built WASM binaries:
//!   make echo discovery

use capnp_rpc::rpc_twoparty_capnp::Side;
use capnp_rpc::twoparty::VatNetwork;
use capnp_rpc::RpcSystem;
use tokio::sync::mpsc;
use tokio_util::compat::TokioAsyncReadCompatExt;

use ww::launcher::create_runtime_client;
use ww::rpc::{CachePolicy, NetworkState};
use ww::system_capnp;

const ECHO_WASM_PATH: &str = "examples/echo/bin/echo.wasm";
const DISCOVERY_WASM_PATH: &str = "examples/discovery/bin/discovery.wasm";

fn load_wasm(path: &str) -> Option<Vec<u8>> {
    std::fs::read(path).ok()
}

/// Create a Runtime client for testing (no epoch guard, no network).
fn setup_runtime() -> system_capnp::runtime::Client {
    let network_state = NetworkState::new();
    let (swarm_tx, _swarm_rx) = mpsc::channel(16);

    create_runtime_client(
        network_state,
        swarm_tx,
        false,
        None,
        None,
        None,
        None,
        None,
        None,
        CachePolicy::Shared,
        ww::ipfs::HttpClient::new("http://localhost:5001".into()),
        Vec::new(),
    )
}

// ---------------------------------------------------------------------------
// Test 1: Mechanism — stdin close causes cell exit
// ---------------------------------------------------------------------------

/// Spawn an echo cell, close stdin without writing anything, verify it exits.
///
/// The echo binary reads stdin until EOF, then exits 0. Closing the host-side
/// stdin ByteStream should deliver EOF to the WASI guest, causing a clean exit.
/// This is the fundamental mechanism that VatListener relies on for vat cell
/// shutdown.
#[tokio::test]
async fn test_stdin_close_exits_echo_cell() {
    let wasm = match load_wasm(ECHO_WASM_PATH) {
        Some(w) => w,
        None => {
            eprintln!("SKIP: echo WASM not built (run `make echo`)");
            return;
        }
    };

    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let runtime = setup_runtime();

            // Load echo WASM via runtime.load() → Executor, then spawn.
            let mut load_req = runtime.load_request();
            load_req.get().set_wasm(&wasm);
            let load_resp = load_req.send().promise.await.expect("runtime.load failed");
            let executor = load_resp.get().unwrap().get_executor().unwrap();

            let mut spawn_req = executor.spawn_request();
            spawn_req.get().init_args(0);
            spawn_req.get().init_env(0);
            let resp = spawn_req
                .send()
                .promise
                .await
                .expect("executor.spawn failed");
            let process = resp.get().unwrap().get_process().unwrap();

            // Get stdin handle.
            let stdin_resp = process.stdin_request().send().promise.await.unwrap();
            let stdin = stdin_resp.get().unwrap().get_stream().unwrap();

            // Close stdin immediately — no bytes written.
            // This is the shutdown signal: <-chan struct{} semantics.
            stdin
                .close_request()
                .send()
                .promise
                .await
                .expect("stdin close failed");

            // Wait for the cell to exit. It should see EOF and exit 0.
            let wait_resp = tokio::time::timeout(
                std::time::Duration::from_secs(10),
                process.wait_request().send().promise,
            )
            .await
            .expect("cell did not exit within 10s after stdin close")
            .expect("wait RPC failed");

            let exit_code = wait_resp.get().unwrap().get_exit_code();
            assert_eq!(exit_code, 0, "echo cell should exit 0 on stdin EOF");
        })
        .await;
}

// ---------------------------------------------------------------------------
// Test 2: VatListener bridge — peer disconnect triggers stdin close
// ---------------------------------------------------------------------------

/// Call `handle_vat_connection` with an in-memory duplex stream and a
/// discovery cell. Verify:
///   1. The RPC bridge works (peer can bootstrap and call Greeter.greet).
///   2. When the peer disconnects, handle_vat_connection returns Ok
///      (having closed stdin to signal the cell).
#[tokio::test]
async fn test_vat_connection_closes_stdin_on_peer_disconnect() {
    let wasm = match load_wasm(DISCOVERY_WASM_PATH) {
        Some(w) => w,
        None => {
            eprintln!("SKIP: discovery WASM not built (run `make discovery`)");
            return;
        }
    };

    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let runtime = setup_runtime();

            // Create an in-memory duplex: one end for handle_vat_connection
            // (the "host bridge"), the other for the simulated peer.
            let (peer_stream, bridge_stream) = tokio::io::duplex(8 * 1024);

            // Load WASM via runtime.load() → Executor, then spawn via handle_vat_connection.
            let wasm_clone = wasm.clone();
            let runtime_clone = runtime.clone();
            let bridge_handle = tokio::task::spawn_local(async move {
                // Load wasm via runtime.load() to get an Executor.
                let mut load_req = runtime_clone.load_request();
                load_req.get().set_wasm(&wasm_clone);
                let load_resp = load_req.send().promise.await.unwrap();
                let executor = load_resp.get().unwrap().get_executor().unwrap();
                let expected_schema_cid =
                    ww::rpc::schema_cid(membrane::schema_registry::HOST_SCHEMA);
                ww::rpc::vat_listener::handle_vat_connection_spawn(
                    executor,
                    Vec::new(), // no extra caps in test
                    // Convert tokio duplex → futures-io via compat layer.
                    bridge_stream.compat(),
                    "test-protocol-cid",
                    &expected_schema_cid,
                )
                .await
            });

            // Peer side: set up a Cap'n Proto client that bootstraps from
            // the bridge to get the cell's exported Greeter capability.
            let (peer_read, peer_write) = tokio::io::split(peer_stream);
            let peer_network = VatNetwork::new(
                peer_read.compat(),
                tokio_util::compat::TokioAsyncWriteCompatExt::compat_write(peer_write),
                Side::Client,
                Default::default(),
            );
            let mut peer_rpc = RpcSystem::new(Box::new(peer_network), None);
            let greeter: ww::greeter_capnp::greeter::Client = peer_rpc.bootstrap(Side::Server);
            let peer_rpc_handle = tokio::task::spawn_local(async move {
                let _ = peer_rpc.await;
            });

            // Verify the bridge works: call Greeter.greet() through
            // the full two-RPC-system chain.
            let mut req = greeter.greet_request();
            req.get().set_name("shutdown-test");
            let resp = tokio::time::timeout(std::time::Duration::from_secs(15), req.send().promise)
                .await
                .expect("greet timed out (cell may not have exported bootstrap)")
                .expect("greet RPC failed");

            let greeting = resp
                .get()
                .unwrap()
                .get_greeting()
                .unwrap()
                .to_str()
                .unwrap();
            assert!(
                greeting.contains("Hello, shutdown-test!"),
                "unexpected greeting: {greeting}"
            );

            // Simulate peer disconnect: abort the peer RPC system.
            // This drops the VatNetwork, closing the duplex stream,
            // which causes handle_vat_connection's peer_rpc to complete.
            // handle_vat_connection should then close stdin and return Ok.
            drop(greeter);
            peer_rpc_handle.abort();

            // Wait for handle_vat_connection to finish.
            let result = tokio::time::timeout(std::time::Duration::from_secs(10), bridge_handle)
                .await
                .expect("handle_vat_connection did not return within 10s after peer disconnect")
                .expect("bridge task panicked");

            assert!(
                result.is_ok(),
                "handle_vat_connection should return Ok after peer disconnect, got: {:?}",
                result.err()
            );
        })
        .await;
}
