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
use cell::proc::FuelObserver;
use tokio::sync::watch;
use tokio_util::compat::TokioAsyncReadCompatExt;

use ww::greeter_capnp;
use ww::rpc::CachePolicy;
use ww::services::{ExecutorPool, SpawnRequest};

const DISCOVERY_WASM_PATH: &str = "examples/discovery/bin/discovery.wasm";

fn load_discovery_wasm() -> Vec<u8> {
    let bytes = std::fs::read(DISCOVERY_WASM_PATH).unwrap_or_else(|error| {
        panic!(
            "required WASM artifact {DISCOVERY_WASM_PATH} is missing: {error}; run `make discovery` before `cargo test`"
        )
    });
    assert!(
        !bytes.is_empty(),
        "required WASM artifact {DISCOVERY_WASM_PATH} is empty"
    );
    bytes
}

/// Spawn a discovery cell on the executor pool and return a Greeter client.
///
/// Creates a duplex stream: one end goes to the cell (via the worker thread),
/// the other end stays on the test thread for the capnp client.
async fn spawn_greeter_on_pool(
    pool: &ExecutorPool,
    wasm: Vec<u8>,
) -> (greeter_capnp::greeter::Client, FuelObserver) {
    let (test_end, cell_end) = tokio::io::duplex(64 * 1024);
    let engine = pool.engine();
    let fuel_observer = FuelObserver::default();
    let worker_fuel_observer = fuel_observer.clone();

    pool.spawn(SpawnRequest {
        name: "discovery-test".into(),
        factory: Box::new(move |_shutdown| {
            Box::pin(async move {
                // Create runtime on the worker thread (capnp clients are !Send).
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
                let runtime = ww::launcher::create_runtime_client_with_fuel_observer(
                    false,
                    guard,
                    Some(engine),
                    None,
                    CachePolicy::Shared,
                    worker_fuel_observer,
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
                        let cap = resp
                            .get()
                            .unwrap()
                            .get_cap()
                            .get_as_capability::<capnp::capability::Client>()
                            .unwrap();

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

    (greeter, fuel_observer)
}

#[tokio::test]
async fn spaced_requests_on_the_ticked_engine_retain_fuel_for_a_large_response() {
    let wasm = load_discovery_wasm();

    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let (_shutdown_tx, shutdown_rx) = watch::channel(());
            let pool = ExecutorPool::new(1, shutdown_rx);
            let (greeter, fuel_observer) = spawn_greeter_on_pool(&pool, wasm).await;

            for request_number in 0..60 {
                tokio::time::sleep(std::time::Duration::from_millis(25)).await;
                let name = format!("spaced-request-{request_number}");
                let mut request = greeter.greet_request();
                request.get().set_name(&name);
                let response = tokio::time::timeout(
                    std::time::Duration::from_secs(60),
                    request.send().promise,
                )
                .await
                .unwrap_or_else(|_| panic!("spaced greet {request_number} timed out"))
                .unwrap_or_else(|error| panic!("spaced greet {request_number} failed: {error}"));
                let greeting = response
                    .get()
                    .expect("spaced greet results")
                    .get_greeting()
                    .expect("spaced greeting text")
                    .to_str()
                    .expect("spaced greeting UTF-8");
                assert!(
                    greeting.contains(&name),
                    "spaced greet {request_number} returned an unexpected response"
                );
            }

            let fuel_trajectory = fuel_observer.observations();
            assert!(
                fuel_trajectory.len() >= 30,
                "spaced traffic observed too few production epoch callbacks: {fuel_trajectory:?}"
            );
            assert!(
                fuel_trajectory
                    .iter()
                    .any(|sample| sample.measured_consumption > 0),
                "production epoch callbacks did not measure guest work: {fuel_trajectory:?}"
            );
            let steady_start = fuel_trajectory.len().saturating_sub(20);
            let steady = &fuel_trajectory[steady_start..];
            assert!(
                steady.iter().any(|sample| {
                    sample.host_calls_this_epoch == 0
                        && sample.measured_consumption > 0
                        && sample.budget >= 9_000_000
                }),
                "steady traffic did not exercise a consuming unmarked epoch with a retained high budget: {steady:?}"
            );
            assert!(
                steady.iter().all(|sample| sample.budget >= 9_000_000),
                "I/O-bound production fuel trajectory decayed: {steady:?}"
            );
            assert!(
                steady.last().is_some_and(|sample| sample.avg_ratio < 100),
                "I/O-bound production fuel ratio stayed high: {steady:?}"
            );

            let name = "x".repeat(256 * 1024);
            let mut request = greeter.greet_request();
            request.get().set_name(&name);
            let response =
                tokio::time::timeout(std::time::Duration::from_secs(60), request.send().promise)
                    .await
                    .expect("large greet after spaced traffic timed out")
                    .expect("large greet after spaced traffic failed");
            let greeting = response
                .get()
                .expect("large greet results")
                .get_greeting()
                .expect("large greeting text");

            assert!(
                greeting.len() > 64 * 1024,
                "response did not exceed the bounded P3 transport capacity"
            );
            assert!(
                greeting
                    .to_str()
                    .expect("large greeting UTF-8")
                    .contains(&name),
                "large greeting did not preserve the request payload"
            );
        })
        .await;
}

#[tokio::test]
async fn test_discovery_cell_greet() {
    let wasm = load_discovery_wasm();

    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let (_shutdown_tx, shutdown_rx) = watch::channel(());
            let pool = ExecutorPool::new(1, shutdown_rx);
            let (greeter, _fuel_observer) = spawn_greeter_on_pool(&pool, wasm).await;

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
async fn concurrent_rpc_calls_complete_while_application_clock_is_pending() {
    let wasm = load_discovery_wasm();

    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let (_shutdown_tx, shutdown_rx) = watch::channel(());
            let pool = ExecutorPool::new(1, shutdown_rx);
            let (greeter, _fuel_observer) = spawn_greeter_on_pool(&pool, wasm).await;

            // The discovery guest's application branch is suspended on a P3
            // monotonic-clock Future. These requests enter one live guest
            // RpcSystem before that independent clock completes.
            let requests = ["Alice", "Bob", "Charlie"].map(|name| {
                let mut request = greeter.greet_request();
                request.get().set_name(name);
                async move {
                    let response = tokio::time::timeout(
                        std::time::Duration::from_secs(60),
                        request.send().promise,
                    )
                    .await
                    .expect("concurrent greet timed out")
                    .expect("concurrent greet RPC failed");
                    (name, response)
                }
            });

            for (name, resp) in futures::future::join_all(requests).await {
                let greeting = resp
                    .get()
                    .unwrap()
                    .get_greeting()
                    .unwrap()
                    .to_str()
                    .unwrap();

                assert!(
                    greeting.contains(&format!("Hello, {name}!")),
                    "unexpected concurrent greeting for {name}: {greeting}"
                );
            }
        })
        .await;
}

#[tokio::test]
async fn response_larger_than_p3_transport_capacity_completes() {
    let wasm = load_discovery_wasm();

    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let (_shutdown_tx, shutdown_rx) = watch::channel(());
            let pool = ExecutorPool::new(1, shutdown_rx);
            let (greeter, _fuel_observer) = spawn_greeter_on_pool(&pool, wasm).await;
            let name = "x".repeat(256 * 1024);

            let mut request = greeter.greet_request();
            request.get().set_name(&name);
            let response =
                tokio::time::timeout(std::time::Duration::from_secs(60), request.send().promise)
                    .await
                    .expect("large greet response timed out")
                    .expect("large greet RPC failed");
            let greeting = response
                .get()
                .expect("large greet results")
                .get_greeting()
                .expect("large greeting text");

            assert!(
                greeting.len() > 64 * 1024,
                "response did not exceed the bounded P3 transport capacity"
            );
            assert!(
                greeting
                    .to_str()
                    .expect("large greeting UTF-8")
                    .contains(&name),
                "large greeting did not preserve the request payload"
            );
        })
        .await;
}
