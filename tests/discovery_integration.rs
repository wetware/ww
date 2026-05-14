//! Integration test: discovery cell spawn + Greeter RPC round-trip.
//!
//! Validates the host-side chain that VatListener uses internally:
//!   runtime.load(wasm) → executor.spawn() → process.bootstrap() → Greeter cap → greet()
//!
//! No args = cell mode (default). No libp2p networking required.
//! Uses in-memory RPC over duplex streams, with the WASM cell running on an
//! ExecutorPool worker thread (matching prod topology) to avoid deadlocks.
//!
//! Requires a pre-built discovery WASM at `examples/discovery/bin/discovery.wasm`.
//! Build:  make discovery

use capnp_rpc::rpc_twoparty_capnp::Side;
use capnp_rpc::twoparty::VatNetwork;
use capnp_rpc::RpcSystem;
use tokio::sync::{mpsc, watch};
use tokio_util::compat::TokioAsyncReadCompatExt;

use ww::greeter_capnp;
use ww::rpc::{CachePolicy, NetworkState};
use ww::services::{ExecutorPool, SpawnRequest};

const DISCOVERY_WASM_PATH: &str = "examples/discovery/bin/discovery.wasm";

/// Skip the test if the WASM binary hasn't been built.
fn load_discovery_wasm() -> Option<Vec<u8>> {
    std::fs::read(DISCOVERY_WASM_PATH).ok()
}

/// Spawn a discovery cell on the executor pool and return a Greeter client.
///
/// Creates a duplex stream: one end goes to the cell (via the worker thread),
/// the other end stays on the test thread for the capnp client.
async fn spawn_greeter_on_pool(
    pool: &ExecutorPool,
    wasm: Vec<u8>,
) -> greeter_capnp::greeter::Client {
    let (test_end, cell_end) = tokio::io::duplex(64 * 1024);

    pool.spawn(SpawnRequest {
        name: "discovery-test".into(),
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
                    None, // no signing key
                    Some(stream_control),
                    None,
                    None,
                    CachePolicy::Shared,
                    ww::ipfs::HttpClient::new("http://localhost:5001".into()),
                    Vec::new(), // no outbound HTTP access
                );

                // Load WASM via runtime to get an Executor.
                let mut load_req = runtime.load_request();
                load_req.get().set_wasm(&wasm);
                let load_resp = load_req.send().promise.await.unwrap();
                let executor = load_resp.get().unwrap().get_executor().unwrap();

                // Spawn the cell in cell mode (no args = default).
                let mut req = executor.spawn_request();
                {
                    let mut env = req.get().init_env(1);
                    env.set(0, "WW_PEER_ID=deadbeefcafebabe");
                }
                let spawn_resp = req.send().promise.await.unwrap();
                let process = spawn_resp.get().unwrap().get_process().unwrap();

                let bootstrap_resp = tokio::time::timeout(
                    std::time::Duration::from_secs(60),
                    process.bootstrap_request().send().promise,
                )
                .await;

                match bootstrap_resp {
                    Ok(Ok(resp)) => {
                        let cap: capnp::capability::Client =
                            resp.get().unwrap().get_cap().get_as_capability().unwrap();

                        // Bridge the bootstrap cap to the duplex stream so the
                        // test thread can use it.
                        let (reader, writer) = tokio::io::split(cell_end);
                        let network = VatNetwork::new(
                            reader.compat(),
                            tokio_util::compat::TokioAsyncWriteCompatExt::compat_write(writer),
                            Side::Server,
                            Default::default(),
                        );
                        let rpc = RpcSystem::new(Box::new(network), Some(cap));
                        let _ = rpc.await;
                    }
                    _ => {
                        eprintln!("  [worker] bootstrap failed/timed out");
                    }
                }
            })
        }),
        result_tx: None,
    })
    .map_err(|_| ())
    .expect("pool rejected spawn");

    // Set up the test-side capnp client over the duplex.
    let (test_read, test_write) = tokio::io::split(test_end);
    let test_network = VatNetwork::new(
        test_read.compat(),
        tokio_util::compat::TokioAsyncWriteCompatExt::compat_write(test_write),
        Side::Client,
        Default::default(),
    );
    let mut test_rpc = RpcSystem::new(Box::new(test_network), None);
    let greeter: greeter_capnp::greeter::Client = test_rpc.bootstrap(Side::Server);

    // Drive the test-side RPC in the background.
    tokio::task::spawn_local(async move {
        let _ = test_rpc.await;
    });

    // Yield to let the RPC task start.
    tokio::task::yield_now().await;

    greeter
}

#[tokio::test]
async fn test_discovery_cell_greet() {
    let wasm = match load_discovery_wasm() {
        Some(w) => w,
        None => {
            eprintln!("SKIP: discovery WASM not built (run `make discovery`)");
            return;
        }
    };

    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let (_shutdown_tx, shutdown_rx) = watch::channel(());
            let pool = ExecutorPool::new(1, shutdown_rx);
            let greeter = spawn_greeter_on_pool(&pool, wasm).await;

            // Call greet() and verify the response.
            // Generous timeout: debug-mode wasmtime compilation of the
            // discovery component can take 5–10s and may run alongside
            // other integration tests (cargo test runs in parallel).
            let mut req = greeter.greet_request();
            req.get().set_name("integration-test");
            let resp = tokio::time::timeout(std::time::Duration::from_secs(60), req.send().promise)
                .await
                .expect("greet timed out")
                .expect("greet RPC failed");
            let greeting = resp
                .get()
                .unwrap()
                .get_greeting()
                .unwrap()
                .to_str()
                .unwrap();

            assert!(
                greeting.contains("Hello, integration-test!"),
                "unexpected greeting: {greeting}"
            );
            assert!(
                greeting.contains("I'm"),
                "greeting should include peer identity: {greeting}"
            );
            // The peer ID we passed was "deadbeefcafebabe" (hex),
            // so short_id should show the last 8 hex chars.
            assert!(
                greeting.contains("cafebabe"),
                "greeting should contain short peer ID: {greeting}"
            );
        })
        .await;
}

#[tokio::test]
async fn test_discovery_cell_greet_multiple() {
    let wasm = match load_discovery_wasm() {
        Some(w) => w,
        None => {
            eprintln!("SKIP: discovery WASM not built (run `make discovery`)");
            return;
        }
    };

    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let (_shutdown_tx, shutdown_rx) = watch::channel(());
            let pool = ExecutorPool::new(1, shutdown_rx);
            let greeter = spawn_greeter_on_pool(&pool, wasm).await;

            // Multiple calls on the same cell should all succeed.
            // First call covers wasmtime compilation (can take 5–10s in debug
            // builds, longer under cargo's parallel test load); subsequent
            // calls should be fast but share the same budget.
            for name in &["Alice", "Bob", "Charlie"] {
                let mut req = greeter.greet_request();
                req.get().set_name(name);
                let resp =
                    tokio::time::timeout(std::time::Duration::from_secs(60), req.send().promise)
                        .await
                        .expect("greet timed out")
                        .expect("greet RPC failed");
                let greeting = resp
                    .get()
                    .unwrap()
                    .get_greeting()
                    .unwrap()
                    .to_str()
                    .unwrap();

                assert!(
                    greeting.contains(&format!("Hello, {name}!")),
                    "unexpected greeting for {name}: {greeting}"
                );
            }
        })
        .await;
}
