//! Integration tests for stdin-as-shutdown-signal semantics.
//!
//! Closing stdin causes a spawned cell to see EOF and exit. This proves the
//! WASI stdin pipe delivers the close signal end-to-end for byte adapters that
//! spawn cells.
//!
//! Requires a pre-built echo WASM binary:
//!   make echo

use tokio::sync::mpsc;

use ww::launcher::create_runtime_client;
use ww::rpc::{CachePolicy, NetworkState};
use ww::system_capnp;

const ECHO_WASM_PATH: &str = "examples/echo/bin/echo.wasm";

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
/// This is the fundamental mechanism stream and HTTP adapters rely on for
/// spawned-cell shutdown.
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
