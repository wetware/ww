//! E2E tests for the shell cell using the ExecutorPool.
//!
//! Spawns a real shell cell WASM on a worker thread (matching prod topology),
//! communicates via Cap'n Proto RPC over an in-memory duplex. No deadlocks
//! because the cell's membrane RPC runs on the worker's own tokio runtime.
//!
//! Requires pre-built WASM: `make shell`

use anyhow::Result;
use capnp_rpc::rpc_twoparty_capnp::Side;
use capnp_rpc::twoparty::VatNetwork;
use capnp_rpc::RpcSystem;
use tokio::sync::{mpsc, watch};
use tokio_util::compat::TokioAsyncReadCompatExt;

use ww::rpc::{CachePolicy, NetworkState};
use ww::services::{ExecutorPool, SpawnRequest};
use ww::shell_capnp;

fn shell_wasm_exists() -> bool {
    std::path::Path::new("std/shell/bin/shell.wasm").exists()
}

/// Spawn a shell cell on the executor pool and return a Shell client.
///
/// Creates a duplex stream: one end goes to the cell on the worker thread,
/// the other end stays on the test thread for the capnp client.
async fn spawn_shell_on_pool(pool: &ExecutorPool) -> Result<shell_capnp::shell::Client> {
    // Duplex: cell_end goes to the worker, test_end stays here.
    let (test_end, cell_end) = tokio::io::duplex(64 * 1024);

    // Read the WASM bytes (Send) to move into the factory.
    let wasm = std::fs::read("std/shell/bin/shell.wasm")
        .map_err(|e| anyhow::anyhow!("failed to read shell WASM: {e}"))?;

    pool.spawn(SpawnRequest {
        name: "shell-test".into(),
        factory: Box::new(move |_shutdown| {
            Box::pin(async move {
                // Create runtime on the worker thread (capnp clients are !Send).
                let network_state = NetworkState::new();
                let (swarm_tx, _swarm_rx) = mpsc::channel(16);
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

                let runtime = ww::launcher::create_runtime_client(
                    network_state,
                    swarm_tx,
                    false,
                    Some(guard),
                    Some(epoch_rx),
                    None,
                    Some(stream_control),
                    None,
                    None,
                    CachePolicy::Shared,
                    ww::ipfs::HttpClient::new("http://localhost:5001".into()),
                    Vec::new(),
                );

                eprintln!("  [worker] loading WASM ({} bytes)", wasm.len());
                // Load WASM via runtime to get an Executor.
                let mut load_req = runtime.load_request();
                load_req.get().set_wasm(&wasm);
                let load_resp = load_req.send().promise.await.unwrap();
                let executor = load_resp.get().unwrap().get_executor().unwrap();
                eprintln!("  [worker] executor obtained, spawning cell");

                // Spawn the cell via executor.spawn()
                let spawn_resp = executor.spawn_request().send().promise.await.unwrap();
                let process = spawn_resp.get().unwrap().get_process().unwrap();
                eprintln!("  [worker] process spawned, waiting for bootstrap");

                let bootstrap_resp = tokio::time::timeout(
                    std::time::Duration::from_secs(10),
                    process.bootstrap_request().send().promise,
                )
                .await;
                eprintln!("  [worker] bootstrap result: {:?}", bootstrap_resp.is_ok());

                match bootstrap_resp {
                    Ok(Ok(resp)) => {
                        let cap = resp
                            .get()
                            .unwrap()
                            .get_cap()
                            .get_as_capability::<capnp::capability::Client>()
                            .unwrap();
                        eprintln!("  [worker] got bootstrap cap, bridging to duplex");

                        let (reader, writer) = tokio::io::split(cell_end);
                        let network = VatNetwork::new(
                            reader.compat(),
                            tokio_util::compat::TokioAsyncWriteCompatExt::compat_write(writer),
                            Side::Server,
                            Default::default(),
                        );
                        let rpc = RpcSystem::new(Box::new(network), Some(cap));
                        let _ = rpc.await;
                        eprintln!("  [worker] bridge ended");
                    }
                    _ => {
                        eprintln!("  [worker] bootstrap failed/timed out");
                    }
                }
            })
        }),
        result_tx: None,
    })
    .map_err(|_| anyhow::anyhow!("pool rejected spawn"))?;

    // Set up the test-side capnp client over the duplex.
    let (test_read, test_write) = tokio::io::split(test_end);
    let test_network = VatNetwork::new(
        test_read.compat(),
        tokio_util::compat::TokioAsyncWriteCompatExt::compat_write(test_write),
        Side::Client,
        Default::default(),
    );
    let mut test_rpc = RpcSystem::new(Box::new(test_network), None);
    let shell: shell_capnp::shell::Client = test_rpc.bootstrap(Side::Server);

    // Drive the test-side RPC in the background.
    // RpcSystem is !Send so must use spawn_local.
    tokio::task::spawn_local(async move {
        eprintln!("  [test] RPC system started");
        match test_rpc.await {
            Ok(_) => eprintln!("  [test] RPC system ended cleanly"),
            Err(e) => eprintln!("  [test] RPC system error: {e}"),
        }
    });

    // Yield to let the RPC task start.
    tokio::task::yield_now().await;

    Ok(shell)
}

/// Helper: eval a Glia expression via the Shell RPC.
async fn eval(shell: &shell_capnp::shell::Client, text: &str) -> (String, bool) {
    let mut req = shell.eval_request();
    req.get().set_text(text);
    let resp = tokio::time::timeout(std::time::Duration::from_secs(15), req.send().promise)
        .await
        .expect("eval timed out")
        .expect("eval RPC failed");
    let result: shell_capnp::shell::eval_results::Reader<'_> = resp.get().unwrap();
    let text = result
        .get_result()
        .unwrap()
        .to_str()
        .unwrap_or("(invalid UTF-8)")
        .to_string();
    let is_error = result.get_is_error();
    (text, is_error)
}

/// Wait for shell to be ready by polling with eval("nil").
async fn wait_ready(shell: &shell_capnp::shell::Client) {
    for i in 0..100 {
        let mut req = shell.eval_request();
        req.get().set_text("nil");
        if let Ok(Ok(resp)) =
            tokio::time::timeout(std::time::Duration::from_secs(5), req.send().promise).await
        {
            let result: shell_capnp::shell::eval_results::Reader<'_> = resp.get().unwrap();
            let text = result.get_result().unwrap().to_str().unwrap_or("");
            if !result.get_is_error() || !text.contains("not ready") {
                return;
            }
        }
        if i > 0 && i % 10 == 0 {
            eprintln!("  waiting for shell to initialize... ({i} polls)");
        }
    }
    panic!("shell cell never became ready");
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "current_thread")]
async fn test_shell_eval_arithmetic() {
    if !shell_wasm_exists() {
        eprintln!("SKIP: shell WASM not built (run `make shell`)");
        return;
    }

    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let (_shutdown_tx, shutdown_rx) = watch::channel(());
            let pool = ExecutorPool::new(1, shutdown_rx);
            let shell = spawn_shell_on_pool(&pool).await.expect("spawn shell");

            wait_ready(&shell).await;

            let (result, is_error) = eval(&shell, "(+ 1 2)").await;
            assert!(!is_error, "arithmetic should not error: {result}");
            assert_eq!(result, "3");
        })
        .await;
}

#[tokio::test]
async fn test_shell_eval_state_persistence() {
    if !shell_wasm_exists() {
        eprintln!("SKIP: shell WASM not built (run `make shell`)");
        return;
    }
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let (_shutdown_tx, shutdown_rx) = watch::channel(());
            let pool = ExecutorPool::new(1, shutdown_rx);
            let shell = spawn_shell_on_pool(&pool).await.expect("spawn shell");
            wait_ready(&shell).await;

            let (_, is_error) = eval(&shell, "(def x 42)").await;
            assert!(!is_error, "def should not error");

            let (result, is_error) = eval(&shell, "x").await;
            assert!(!is_error, "x lookup should not error: {result}");
            assert_eq!(result, "42");
        })
        .await;
}

#[tokio::test]
async fn test_shell_eval_parse_error() {
    if !shell_wasm_exists() {
        eprintln!("SKIP: shell WASM not built (run `make shell`)");
        return;
    }
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let (_shutdown_tx, shutdown_rx) = watch::channel(());
            let pool = ExecutorPool::new(1, shutdown_rx);
            let shell = spawn_shell_on_pool(&pool).await.expect("spawn shell");
            wait_ready(&shell).await;

            let (result, is_error) = eval(&shell, "(+ 1").await;
            assert!(is_error, "unmatched paren should be a parse error");
            assert!(
                result.contains("parse error"),
                "should mention parse: {result}"
            );
        })
        .await;
}

#[tokio::test]
async fn test_shell_eval_exit_sentinel() {
    if !shell_wasm_exists() {
        eprintln!("SKIP: shell WASM not built (run `make shell`)");
        return;
    }
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let (_shutdown_tx, shutdown_rx) = watch::channel(());
            let pool = ExecutorPool::new(1, shutdown_rx);
            let shell = spawn_shell_on_pool(&pool).await.expect("spawn shell");
            wait_ready(&shell).await;

            let (result, is_error) = eval(&shell, "(perform :exit nil)").await;
            assert!(!is_error, "exit effect should not be an error");
            assert!(result.is_empty(), "exit result: {result}");
        })
        .await;
}

#[tokio::test]
async fn test_shell_eval_prelude_macros() {
    if !shell_wasm_exists() {
        eprintln!("SKIP: shell WASM not built (run `make shell`)");
        return;
    }
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let (_shutdown_tx, shutdown_rx) = watch::channel(());
            let pool = ExecutorPool::new(1, shutdown_rx);
            let shell = spawn_shell_on_pool(&pool).await.expect("spawn shell");
            wait_ready(&shell).await;

            let (result, is_error) = eval(&shell, "(when true 42)").await;
            assert!(!is_error, "when macro should work: {result}");
            assert_eq!(result, "42");

            let (_, is_error) = eval(&shell, "(defn double [x] (* x 2))").await;
            assert!(!is_error, "defn should not error");

            let (result, is_error) = eval(&shell, "(double 21)").await;
            assert!(!is_error, "calling defn'd function: {result}");
            assert_eq!(result, "42");
        })
        .await;
}

#[tokio::test]
async fn test_shell_eval_empty_input() {
    if !shell_wasm_exists() {
        eprintln!("SKIP: shell WASM not built (run `make shell`)");
        return;
    }
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let (_shutdown_tx, shutdown_rx) = watch::channel(());
            let pool = ExecutorPool::new(1, shutdown_rx);
            let shell = spawn_shell_on_pool(&pool).await.expect("spawn shell");
            wait_ready(&shell).await;

            let (result, is_error) = eval(&shell, "").await;
            assert!(!is_error, "empty input should not error");
            assert!(result.is_empty(), "empty input result: '{result}'");
        })
        .await;
}

// ---------------------------------------------------------------------------
// Membrane capability tests — exercise the real WASM→RPC→Host path
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_shell_host_id() {
    if !shell_wasm_exists() {
        eprintln!("SKIP: shell WASM not built (run `make shell`)");
        return;
    }
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let (_shutdown_tx, shutdown_rx) = watch::channel(());
            let pool = ExecutorPool::new(1, shutdown_rx);
            let shell = spawn_shell_on_pool(&pool).await.expect("spawn shell");
            wait_ready(&shell).await;

            let (result, is_error) = eval(&shell, "(perform host :id)").await;
            assert!(!is_error, "host :id should not error: {result}");
            // Result is a bs58-encoded peer ID string — non-empty.
            assert!(!result.is_empty(), "peer ID should not be empty");
        })
        .await;
}

#[tokio::test]
async fn test_shell_host_addrs() {
    if !shell_wasm_exists() {
        eprintln!("SKIP: shell WASM not built (run `make shell`)");
        return;
    }
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let (_shutdown_tx, shutdown_rx) = watch::channel(());
            let pool = ExecutorPool::new(1, shutdown_rx);
            let shell = spawn_shell_on_pool(&pool).await.expect("spawn shell");
            wait_ready(&shell).await;

            let (result, is_error) = eval(&shell, "(perform host :addrs)").await;
            assert!(!is_error, "host :addrs should not error: {result}");
            // In test mode (no swarm), addrs returns an empty list.
            assert!(
                result == "()" || result.starts_with('('),
                "addrs should be a list: {result}"
            );
        })
        .await;
}

#[tokio::test]
async fn test_shell_host_peers() {
    if !shell_wasm_exists() {
        eprintln!("SKIP: shell WASM not built (run `make shell`)");
        return;
    }
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let (_shutdown_tx, shutdown_rx) = watch::channel(());
            let pool = ExecutorPool::new(1, shutdown_rx);
            let shell = spawn_shell_on_pool(&pool).await.expect("spawn shell");
            wait_ready(&shell).await;

            let (result, is_error) = eval(&shell, "(perform host :peers)").await;
            assert!(!is_error, "host :peers should not error: {result}");
            // In test mode (no swarm), peers returns an empty list.
            assert!(
                result == "()" || result.starts_with('('),
                "peers should be a list: {result}"
            );
        })
        .await;
}
