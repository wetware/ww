//! Prerequisite spikes for #302: thread-per-subsystem runtime.
//!
//! These tests validate the core assumptions of the Pingora-inspired
//! threading model before committing to the full harness implementation.
//!
//! Spike 1: Fuel + LocalSet interleaving
//! Spike 2: Cap'n Proto RPC on shared LocalSet
//! Spike 3: WASM compilation off the executor thread
//!
//! Requires pre-built echo WASM at examples/echo/bin/echo.wasm.
//! Build: make echo

use std::sync::Arc;
use std::time::Instant;

use tokio::sync::mpsc;

use ww::launcher::create_runtime_client;
use ww::rpc::{CachePolicy, NetworkState};
use ww::system_capnp;

const ECHO_WASM_PATH: &str = "examples/echo/bin/echo.wasm";

fn load_echo_wasm() -> Option<Vec<u8>> {
    std::fs::read(ECHO_WASM_PATH).ok()
}

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

/// Load echo WASM via runtime.load() and spawn a cell, returning its Process capability.
async fn spawn_echo_cell(
    runtime: &system_capnp::runtime::Client,
    wasm: &[u8],
    label: &str,
) -> system_capnp::process::Client {
    // runtime.load(wasm) → Executor
    let mut load_req = runtime.load_request();
    load_req.get().set_wasm(wasm);
    let load_resp = load_req.send().promise.await.expect("runtime.load failed");
    let executor = load_resp.get().unwrap().get_executor().unwrap();

    // executor.spawn(args, env) → Process
    let mut req = executor.spawn_request();
    {
        let mut env = req.get().init_env(1);
        env.set(0, format!("WW_LABEL={label}").as_str());
    }
    let resp = req.send().promise.await.expect("executor.spawn failed");
    resp.get().unwrap().get_process().unwrap()
}

/// Write data to a process's stdin via ByteStream RPC.
async fn write_stdin(process: &system_capnp::process::Client, data: &[u8]) {
    let stdin_resp = process.stdin_request().send().promise.await.unwrap();
    let stdin = stdin_resp.get().unwrap().get_stream().unwrap();
    let mut write_req = stdin.write_request();
    write_req.get().set_data(data);
    write_req.send().promise.await.unwrap();
    // Close stdin so the echo cell sees EOF and writes output.
    stdin.close_request().send().promise.await.unwrap();
}

/// Read all data from a process's stdout via ByteStream RPC.
async fn read_stdout(process: &system_capnp::process::Client) -> Vec<u8> {
    let stdout_resp = process.stdout_request().send().promise.await.unwrap();
    let stdout = stdout_resp.get().unwrap().get_stream().unwrap();
    let mut result = Vec::new();
    loop {
        let mut read_req = stdout.read_request();
        read_req.get().set_max_bytes(64 * 1024);
        let read_resp = read_req.send().promise.await.unwrap();
        let chunk = read_resp.get().unwrap().get_data().unwrap();
        if chunk.is_empty() {
            break;
        }
        result.extend_from_slice(chunk);
    }
    result
}

// =========================================================================
// Spike 1: Fuel + LocalSet interleaving
// =========================================================================
//
// Validates that two WASM cells spawned on the same LocalSet interleave
// execution via fuel-based cooperative yielding. Both cells are echo cells
// that read stdin and write to stdout. If fuel yields work correctly, both
// cells make progress concurrently on a single thread.

#[tokio::test]
async fn spike1_two_cells_interleave_on_shared_localset() {
    let wasm = match load_echo_wasm() {
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

            // Spawn two echo cells on the same LocalSet.
            let cell_a = spawn_echo_cell(&runtime, &wasm, "cell-A").await;
            let cell_b = spawn_echo_cell(&runtime, &wasm, "cell-B").await;

            // Write to both cells concurrently. If one cell blocked the
            // LocalSet without yielding, the other would never receive
            // its stdin data (deadlock or timeout).
            let msg_a = b"hello from A";
            let msg_b = b"hello from B";

            let (result_a, result_b) = tokio::join!(
                async {
                    write_stdin(&cell_a, msg_a).await;
                    read_stdout(&cell_a).await
                },
                async {
                    write_stdin(&cell_b, msg_b).await;
                    read_stdout(&cell_b).await
                },
            );

            assert_eq!(result_a, msg_a, "cell A should echo its input");
            assert_eq!(result_b, msg_b, "cell B should echo its input");
        })
        .await;
}

// =========================================================================
// Spike 2: Cap'n Proto RPC on shared LocalSet
// =========================================================================
//
// Validates that two independent Cap'n Proto RPC systems can coexist on
// the same LocalSet. Both cells export a bootstrap capability (Process)
// and serve RPC concurrently. This is the same setup that would exist
// on an executor worker thread running multiple cells.

#[tokio::test]
async fn spike2_two_rpc_systems_on_shared_localset() {
    let wasm = match load_echo_wasm() {
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

            // Spawn two cells — each gets its own RPC system (VatNetwork +
            // RpcSystem) running on the same outer LocalSet.
            let cell_a = spawn_echo_cell(&runtime, &wasm, "rpc-A").await;
            let cell_b = spawn_echo_cell(&runtime, &wasm, "rpc-B").await;

            // Interleave RPC calls to both cells. Each call goes through
            // Cap'n Proto promise pipelining on the shared LocalSet.
            let msg_a = b"rpc message A";
            let msg_b = b"rpc message B";

            // Write to A, then B, then read from both. This exercises
            // the RPC scheduling: write_a's promise must resolve while
            // B's RPC system is also active on the same LocalSet.
            write_stdin(&cell_a, msg_a).await;
            write_stdin(&cell_b, msg_b).await;

            let (result_a, result_b) = tokio::join!(read_stdout(&cell_a), read_stdout(&cell_b),);

            assert_eq!(result_a, msg_a, "cell A RPC round-trip failed");
            assert_eq!(result_b, msg_b, "cell B RPC round-trip failed");

            // Verify we can make additional RPC calls after the first batch.
            // This confirms the RPC systems are still alive and serving.
            let wait_a = cell_a.wait_request().send().promise.await;
            let wait_b = cell_b.wait_request().send().promise.await;
            assert!(wait_a.is_ok(), "cell A should still be reachable via RPC");
            assert!(wait_b.is_ok(), "cell B should still be reachable via RPC");
        })
        .await;
}

// =========================================================================
// Spike 3: WASM compilation off the executor thread
// =========================================================================
//
// Measures Component::from_binary compilation time and validates that
// compilation can happen on a dedicated thread, with only the compiled
// Component (or pre-instantiated module) sent back to the executor.
//
// This doesn't require a running cell — just wasmtime compilation.

#[tokio::test]
async fn spike3_wasm_compilation_off_thread() {
    let wasm = match load_echo_wasm() {
        Some(w) => w,
        None => {
            eprintln!("SKIP: echo WASM not built (run `make echo`)");
            return;
        }
    };

    // Measure inline compilation time (current approach).
    let mut config = wasmtime::Config::new();
    config.consume_fuel(true);
    let engine = Arc::new(wasmtime::Engine::new(&config).unwrap());

    let inline_start = Instant::now();
    let _component = wasmtime::component::Component::from_binary(&engine, &wasm).unwrap();
    let inline_duration = inline_start.elapsed();
    eprintln!(
        "spike3: inline compilation took {:?} (this blocks the executor thread)",
        inline_duration
    );

    // Validate off-thread compilation: compile on a background thread,
    // send the serialized module back via oneshot.
    let engine2 = engine.clone();
    let wasm2 = wasm.clone();
    let (tx, rx) = tokio::sync::oneshot::channel();

    let offthread_start = Instant::now();
    std::thread::spawn(move || {
        let component = wasmtime::component::Component::from_binary(&engine2, &wasm2).unwrap();
        // Serialize the compiled component so it can be deserialized on
        // the executor thread without re-compiling.
        let serialized = component.serialize().unwrap();
        let _ = tx.send(serialized);
    });

    let serialized = rx.await.expect("compilation thread panicked");
    let offthread_duration = offthread_start.elapsed();
    eprintln!(
        "spike3: off-thread compile + serialize took {:?}",
        offthread_duration
    );

    // Deserialize on the "executor thread" (this thread).
    let deser_start = Instant::now();
    // SAFETY: We just serialized this component from the same engine
    // configuration. In production, we'd validate the engine config matches.
    let _deserialized =
        unsafe { wasmtime::component::Component::deserialize(&engine, &serialized) }.unwrap();
    let deser_duration = deser_start.elapsed();
    eprintln!(
        "spike3: deserialize on executor thread took {:?} (this is what cells would pay)",
        deser_duration
    );

    // The key assertion: deserialization should be much faster than compilation.
    // Compilation is O(WASM size), deserialization is O(native code size) with
    // just mmap. For a typical cell, expect 10-100x speedup.
    assert!(
        deser_duration < inline_duration,
        "deserialize ({:?}) should be faster than compile ({:?})",
        deser_duration,
        inline_duration,
    );

    eprintln!(
        "spike3: speedup = {:.1}x (compile {:?} vs deserialize {:?})",
        inline_duration.as_secs_f64() / deser_duration.as_secs_f64().max(0.000001),
        inline_duration,
        deser_duration,
    );
}

// =========================================================================
// Bonus: Validate current_thread runtime works for the executor model
// =========================================================================
//
// Runs the same two-cell test but on an explicit current_thread runtime
// inside a std::thread, simulating the actual executor worker thread
// topology from the design doc.

#[test]
fn spike_bonus_executor_worker_thread_model() {
    let wasm = match load_echo_wasm() {
        Some(w) => w,
        None => {
            eprintln!("SKIP: echo WASM not built (run `make echo`)");
            return;
        }
    };

    let handle = std::thread::spawn(move || {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();

        let local = tokio::task::LocalSet::new();

        rt.block_on(local.run_until(async {
            let runtime = setup_runtime();

            let cell_a = spawn_echo_cell(&runtime, &wasm, "worker-A").await;
            let cell_b = spawn_echo_cell(&runtime, &wasm, "worker-B").await;

            let msg_a = b"worker thread A";
            let msg_b = b"worker thread B";

            let (result_a, result_b) = tokio::join!(
                async {
                    write_stdin(&cell_a, msg_a).await;
                    read_stdout(&cell_a).await
                },
                async {
                    write_stdin(&cell_b, msg_b).await;
                    read_stdout(&cell_b).await
                },
            );

            assert_eq!(result_a, msg_a);
            assert_eq!(result_b, msg_b);
        }));
    });

    handle.join().expect("executor worker thread panicked");
}
