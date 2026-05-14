//! End-to-end integration test: HttpServer + Runtime/Executor + echo cell.
//!
//! Validates the full pipeline:
//!   1. Load echo WASM binary
//!   2. Set up Cap'n Proto RPC (in-memory, no network)
//!   3. Get Runtime from membrane graft
//!   4. Runtime.load(wasm) → Executor
//!   5. Executor.spawn(args, env) → Process
//!   6. Write to Process.stdin, read from Process.stdout
//!   7. Verify echo round-trip
//!
//! Also validates HttpServer::handle() (Mode A: per-request spawn).
//!
//! Run: cargo run --example echo_handler_e2e

use tokio::sync::mpsc;

use ww::launcher::create_runtime_client;
use ww::rpc::{CachePolicy, NetworkState};
use ww::system_capnp;

const ECHO_WASM: &[u8] = include_bytes!("echo/bin/echo.wasm");

/// Create a Runtime client for testing (no network, no epoch guard).
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

#[tokio::main(flavor = "current_thread")]
async fn main() {
    // Initialize tracing for visibility
    tracing_subscriber::fmt()
        .with_env_filter("info")
        .with_target(false)
        .init();

    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            println!("Echo Cell E2E Test");
            println!("=====================\n");

            let runtime = setup_runtime();

            // Load echo WASM via runtime.load() → Executor
            let mut load_req = runtime.load_request();
            load_req.get().set_wasm(ECHO_WASM);
            let load_resp = load_req.send().promise.await.unwrap();
            let executor = load_resp.get().unwrap().get_executor().unwrap();

            // ─── Test 1: Executor.spawn() ───
            println!("--- Test 1: Executor.spawn() ---");
            {
                let spawn_resp = executor.spawn_request().send().promise.await.unwrap();
                let process = spawn_resp.get().unwrap().get_process().unwrap();

                // Write to stdin
                let stdin_resp = process.stdin_request().send().promise.await.unwrap();
                let stdin = stdin_resp.get().unwrap().get_stream().unwrap();
                let mut write_req = stdin.write_request();
                write_req.get().set_data(b"hello from spawn");
                write_req.send().promise.await.unwrap();
                stdin.close_request().send().promise.await.unwrap();

                // Read from stdout
                let stdout_resp = process.stdout_request().send().promise.await.unwrap();
                let stdout = stdout_resp.get().unwrap().get_stream().unwrap();
                let mut read_req = stdout.read_request();
                read_req.get().set_max_bytes(65536);
                let read_resp = read_req.send().promise.await.unwrap();
                let data = read_resp.get().unwrap().get_data().unwrap();

                assert_eq!(data, b"hello from spawn", "spawn echo failed");
                println!("  [OK] Echo round-trip: 'hello from spawn'");

                // Wait for exit
                let wait_resp = process.wait_request().send().promise.await.unwrap();
                let exit_code = wait_resp.get().unwrap().get_exit_code();
                assert_eq!(exit_code, 0, "expected exit code 0");
                println!("  [OK] Exit code: {exit_code}");
            }

            // ─── Test 2: Executor is reusable (multiple spawns) ───
            println!("\n--- Test 2: Executor (multiple spawns) ---");
            {
                // Spawn first instance
                let spawn_resp = executor.spawn_request().send().promise.await.unwrap();
                let process = spawn_resp.get().unwrap().get_process().unwrap();

                let stdin_resp = process.stdin_request().send().promise.await.unwrap();
                let stdin = stdin_resp.get().unwrap().get_stream().unwrap();
                let mut write_req = stdin.write_request();
                write_req.get().set_data(b"hello from Executor");
                write_req.send().promise.await.unwrap();
                stdin.close_request().send().promise.await.unwrap();

                let stdout_resp = process.stdout_request().send().promise.await.unwrap();
                let stdout = stdout_resp.get().unwrap().get_stream().unwrap();
                let mut read_req = stdout.read_request();
                read_req.get().set_max_bytes(65536);
                let read_resp = read_req.send().promise.await.unwrap();
                let data = read_resp.get().unwrap().get_data().unwrap();

                assert_eq!(data, b"hello from Executor");
                println!("  [OK] Echo round-trip: 'hello from Executor'");

                let wait_resp = process.wait_request().send().promise.await.unwrap();
                assert_eq!(wait_resp.get().unwrap().get_exit_code(), 0);
                println!("  [OK] Exit code: 0");

                // Spawn second instance (verifies Executor is reusable)
                let spawn_resp2 = executor.spawn_request().send().promise.await.unwrap();
                let process2 = spawn_resp2.get().unwrap().get_process().unwrap();

                let stdin_resp2 = process2.stdin_request().send().promise.await.unwrap();
                let stdin2 = stdin_resp2.get().unwrap().get_stream().unwrap();
                let mut write_req2 = stdin2.write_request();
                write_req2.get().set_data(b"second spawn");
                write_req2.send().promise.await.unwrap();
                stdin2.close_request().send().promise.await.unwrap();

                let stdout_resp2 = process2.stdout_request().send().promise.await.unwrap();
                let stdout2 = stdout_resp2.get().unwrap().get_stream().unwrap();
                let mut read_req2 = stdout2.read_request();
                read_req2.get().set_max_bytes(65536);
                let read_resp2 = read_req2.send().promise.await.unwrap();
                let data2 = read_resp2.get().unwrap().get_data().unwrap();

                assert_eq!(data2, b"second spawn");
                println!("  [OK] Second spawn echo: 'second spawn'");

                let wait_resp2 = process2.wait_request().send().promise.await.unwrap();
                assert_eq!(wait_resp2.get().unwrap().get_exit_code(), 0);
                println!("  [OK] Second spawn exit code: 0");
            }

            // ─── Test 3: HttpServer.handle() (Mode A) ───
            println!("\n--- Test 3: HttpServer.handle() (per-request spawn) ---");
            {
                let server = ww::dispatcher::HttpServer::new(executor.clone());

                let (response, exit_code) = server
                    .handle(b"hello via HttpServer".to_vec())
                    .await
                    .unwrap();

                assert_eq!(response, b"hello via HttpServer");
                assert_eq!(exit_code, 0);
                println!("  [OK] HttpServer echo: 'hello via HttpServer'");
                println!("  [OK] Exit code: {exit_code}");

                // Second request (new spawn)
                let (response2, exit_code2) =
                    server.handle(b"second request".to_vec()).await.unwrap();
                assert_eq!(response2, b"second request");
                assert_eq!(exit_code2, 0);
                println!("  [OK] Second request echo: 'second request'");
            }

            println!("\n=====================");
            println!("ALL TESTS PASSED");
        })
        .await;
}
