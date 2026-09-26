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
    let runtime_engine = pool.runtime_engine();
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
                    runtime_engine,
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
                req.get()
                    .set_membrane(authority::membrane_client(epoch_rx, b"test-peer"));
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

async fn drive_with_manual_epochs<F: std::future::Future>(
    future: F,
    publisher: &mut cell::engine::EpochPublisher,
) -> F::Output {
    let mut future = Box::pin(future);
    for _ in 0..1_000 {
        match tokio::time::timeout(std::time::Duration::from_millis(10), &mut future).await {
            Ok(output) => return output,
            Err(_) => {
                publisher.tick();
            }
        }
    }
    panic!("manual epoch driver exceeded 1,000 ticks")
}

async fn spawn_manual_oneshot_greeter(
    wasm: Vec<u8>,
) -> (
    greeter_capnp::greeter::Client,
    ww::system_capnp::process::Client,
    cell::engine::EpochPublisher,
    FuelObserver,
    watch::Sender<authority::Epoch>,
) {
    let (runtime_engine, mut publisher) =
        cell::engine::runtime_engine().expect("manual runtime engine");
    let epoch = authority::Epoch {
        seq: 1,
        head: vec![],
        root: None,
    };
    let (epoch_tx, epoch_rx) = watch::channel(epoch);
    let guard = authority::EpochGuard {
        issued_seq: 1,
        receiver: epoch_rx.clone(),
    };
    let fuel_observer = FuelObserver::default();
    let runtime = ww::launcher::create_runtime_client_with_fuel_observer(
        false,
        guard,
        runtime_engine,
        None,
        CachePolicy::Isolated,
        fuel_observer.clone(),
    );

    let mut load = runtime.load_request();
    load.get().set_wasm(&wasm);
    let executor = load
        .send()
        .promise
        .await
        .expect("load one-shot discovery guest")
        .get()
        .expect("load results")
        .get_executor()
        .expect("one-shot Executor");

    let mut spawn = executor.spawn_request();
    spawn
        .get()
        .set_membrane(authority::membrane_client(epoch_rx, b"test-peer"));
    {
        let mut env = spawn.get().init_env(1);
        env.set(0, "WW_PEER_ID=deadbeefcafebabe");
    }
    {
        let mut policy = spawn.get().init_fuel_policy().init_oneshot();
        policy.set_total_budget(10_000_000);
        policy.set_max_per_epoch(10_000);
        policy.set_min_per_epoch(0);
    }
    let process = spawn
        .send()
        .promise
        .await
        .expect("spawn one-shot discovery guest")
        .get()
        .expect("spawn results")
        .get_process()
        .expect("one-shot Process");

    let bootstrap =
        drive_with_manual_epochs(process.bootstrap_request().send().promise, &mut publisher)
            .await
            .expect("bootstrap one-shot discovery guest");
    let generic = bootstrap
        .get()
        .expect("bootstrap results")
        .get_cap()
        .get_as_capability::<capnp::capability::Client>()
        .expect("bootstrap capability");
    let greeter = greeter_capnp::greeter::Client { client: generic };
    (greeter, process, publisher, fuel_observer, epoch_tx)
}

#[tokio::test]
async fn guest_rpc_queues_while_oneshot_store_is_suspended_and_resumes_on_tick() {
    let wasm = load_discovery_wasm();
    tokio::task::LocalSet::new()
        .run_until(async move {
            let (greeter, _process, mut publisher, fuel_observer, _epoch_tx) =
                spawn_manual_oneshot_greeter(wasm).await;
            // Bootstrap can consume its epoch and resume only far enough to
            // return the capability. Start the request under test in a fresh
            // published epoch so its suspension is independently observable.
            publisher.tick();
            let next_suspension = fuel_observer.suspension_count() + 1;
            let large_name = "suspended-request-".repeat(16 * 1024);
            let mut first_request = greeter.greet_request();
            first_request.get().set_name(&large_name);
            let mut first = Box::pin(first_request.send().promise);
            tokio::select! {
                () = fuel_observer.wait_for_suspension(next_suspension) => {}
                result = &mut first => match result {
                    Ok(_) => panic!("large guest RPC completed before suspension"),
                    Err(error) => panic!("large guest RPC failed before suspension: {error}"),
                },
            }

            let mut second_request = greeter.greet_request();
            second_request.get().set_name("queued-while-suspended");
            let mut second = Box::pin(second_request.send().promise);
            assert!(
                tokio::time::timeout(std::time::Duration::from_millis(50), &mut second)
                    .await
                    .is_err(),
                "queued guest RPC completed while the Store was suspended"
            );

            let ticker = tokio::task::spawn_local(async move {
                let mut interval = tokio::time::interval(std::time::Duration::from_millis(5));
                loop {
                    interval.tick().await;
                    publisher.tick();
                }
            });
            let (first, second) = tokio::time::timeout(std::time::Duration::from_secs(60), async {
                tokio::join!(first, second)
            })
            .await
            .expect("queued guest RPCs did not resume");
            ticker.abort();
            first.expect("large guest RPC failed after resume");
            second.expect("queued guest RPC failed after resume");
        })
        .await;
}

#[tokio::test]
async fn epoch_replacement_kill_drops_a_suspended_store_with_pending_guest_rpc() {
    let wasm = load_discovery_wasm();
    tokio::task::LocalSet::new()
        .run_until(async move {
            let (greeter, process, mut publisher, fuel_observer, epoch_tx) =
                spawn_manual_oneshot_greeter(wasm).await;
            publisher.tick();
            let next_suspension = fuel_observer.suspension_count() + 1;
            let mut request = greeter.greet_request();
            request.get().set_name("kill-suspended-".repeat(16 * 1024));
            let mut pending = Box::pin(request.send().promise);
            tokio::select! {
                () = fuel_observer.wait_for_suspension(next_suspension) => {}
                result = &mut pending => match result {
                    Ok(_) => panic!("guest RPC completed before suspension"),
                    Err(error) => panic!("guest RPC failed before suspension: {error}"),
                },
            }

            epoch_tx.send_replace(authority::Epoch {
                seq: 2,
                head: b"replacement-root".to_vec(),
                root: None,
            });

            process
                .kill_request()
                .send()
                .promise
                .await
                .expect("kill suspended one-shot Process");
            let wait = tokio::time::timeout(
                std::time::Duration::from_secs(5),
                process.wait_request().send().promise,
            )
            .await
            .expect("suspended Process teardown exceeded timeout")
            .expect("wait for suspended Process");
            assert_eq!(wait.get().expect("wait results").get_exit_code(), 137);
            assert!(
                tokio::time::timeout(std::time::Duration::from_secs(5), &mut pending)
                    .await
                    .expect("pending guest RPC survived Store teardown")
                    .is_err()
            );
        })
        .await;
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
