//! Bounded, opt-in lifecycle measurements using APIs present at baseline 0ee7af3e.
//! Copy this file unchanged into the baseline checkout. Run both release binaries
//! under the shared build/benchmark lock and continuous external AC monitoring.
//! Each sample sums individually timed operations; preparation, assertions, and
//! filesystem completion waits occur outside each operation's timed interval.
#![cfg(not(target_arch = "wasm32"))]

mod support;

use std::process::Command;
use std::sync::Arc;
use std::time::{Duration, Instant};

use cell::engine::{compile_component, runtime_engine, wasmtime_cache_metrics, RuntimeEngine};
use cell::{Builder, Program};
use wasmtime::component::Component;

const PROBE_MODE: &str = "WW_LIFECYCLE_OVERHEAD_MODE";
const WARMUPS: usize = 3;
const SAMPLES: usize = 31;
const PROCESS_DEADLINE: Duration = Duration::from_secs(180);
const COMPILE_OPERATIONS: usize = 16;
const WARM_COMPILE_OPERATIONS: usize = 64;
const INVALID_COMPILE_OPERATIONS: usize = 2048;
const INSTANTIATION_OPERATIONS: usize = 64;
const STORE_OPERATIONS: usize = 32;

const COMMAND: &str = r#"
    (component
      (core func $task-return (canon task.return (result (result))))
      (core module $command
        (import "" "task-return" (func $task-return (param i32)))
        ;; START_HOOK
        (func (export "run")
          i32.const 0
          call $task-return))
      (core instance $instance
        (instantiate $command
          (with "" (instance
            (export "task-return" (func $task-return))))))
      (func $run async (result (result))
        (canon lift (core func $instance "run") async))
      (instance (export (interface "wasi:cli/run@0.3.0"))
        (export "run" (func $run))))
"#;

#[test]
#[ignore = "opt-in lifecycle measurements; externally enforced performance gates"]
fn measure_lifecycle_operations() {
    let deadline = Instant::now() + PROCESS_DEADLINE;
    for mode in ["disabled", "enabled"] {
        let directory = tempfile::tempdir().expect("private compilation cache");
        let mut command = Command::new(std::env::current_exe().unwrap());
        command
            .args([
                "--exact",
                "lifecycle_overhead_probe",
                "--ignored",
                "--nocapture",
                "--test-threads=1",
            ])
            .env(PROBE_MODE, mode)
            .env_remove(cell::engine::CWASM_DIR_ENV)
            .env_remove(cell::engine::CWASM_CACHE_MAX_BYTES_ENV);
        if mode == "enabled" {
            command.env(cell::engine::CWASM_DIR_ENV, directory.path());
        }
        // Inherit output so the JSON samples cannot fill a subprocess pipe.
        let mut child = command.spawn().expect("isolated lifecycle benchmark");
        loop {
            if let Some(status) = child.try_wait().unwrap() {
                assert!(
                    status.success(),
                    "{mode} lifecycle benchmark failed: {status}"
                );
                break;
            }
            if Instant::now() >= deadline {
                let _ = child.kill();
                let _ = child.wait();
                panic!("{mode} lifecycle benchmark exceeded {PROCESS_DEADLINE:?}");
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    }
}

fn label() -> String {
    std::env::var("WW_LIFECYCLE_BENCH_LABEL").unwrap_or_else(|_| "unspecified".into())
}

fn report(case: &str, mode: &str, sample: usize, operations: usize, elapsed: Duration) {
    println!(
        "{}",
        serde_json::json!({
            "benchmark": "lifecycle", "label": label(), "case": case,
            "cache_mode": mode, "sample": sample, "warmup": sample < WARMUPS,
            "operations": operations, "unit": "operation",
            "elapsed_ns": elapsed.as_nanos(),
            "ns_per_operation": elapsed.as_nanos() as f64 / operations as f64,
        })
    );
}

fn measure_compile(runtime: &RuntimeEngine, mode: &str, wasm: &[u8], invalid: bool) {
    let operations = if invalid {
        INVALID_COMPILE_OPERATIONS
    } else if mode == "enabled" {
        WARM_COMPILE_OPERATIONS
    } else {
        COMPILE_OPERATIONS
    };
    let case = if invalid {
        "compile_invalid"
    } else if mode == "enabled" {
        "compile_warm_cache"
    } else {
        "compile_disabled_cache"
    };
    let engine = runtime.engine();
    for sample in 0..WARMUPS + SAMPLES {
        let mut elapsed = Duration::ZERO;
        for _ in 0..operations {
            let start = Instant::now();
            let result = compile_component(&engine, wasm);
            elapsed += start.elapsed();
            assert_eq!(result.is_err(), invalid, "unexpected {case} result");
            // Component/error destruction is outside compilation timing.
            drop(result);
        }
        report(case, mode, sample, operations, elapsed);
    }
}

fn prepare_builder(runtime: &RuntimeEngine, component: &Arc<Component>) -> Builder {
    let (builder, _handles) = Builder::ordinary(
        Program::Precompiled(Arc::clone(component)),
        tokio::io::empty(),
        tokio::io::sink(),
        tokio::io::sink(),
        support::filesystem_ownership(),
    );
    builder.with_runtime_engine(runtime.clone())
}

async fn measure_process(runtime: &RuntimeEngine, component: &Arc<Component>, case: &str) {
    let build_drop = case == "store_build_drop";
    let operations = if build_drop {
        STORE_OPERATIONS
    } else {
        INSTANTIATION_OPERATIONS
    };
    for sample in 0..WARMUPS + SAMPLES {
        let mut elapsed = Duration::ZERO;
        for _ in 0..operations {
            let builder = prepare_builder(runtime, component);
            let completion = builder.filesystem_completion();
            if build_drop {
                // Includes Builder::build, Store construction, linker validation,
                // and Store drop. Ownership creation and cleanup completion are
                // excluded. This control never executes guest instantiation.
                let start = Instant::now();
                let result = builder.build().await;
                let succeeded = result.is_ok();
                drop(result);
                elapsed += start.elapsed();
                assert!(succeeded, "precompiled Store construction failed");
            } else {
                let proc = builder.build().await.expect("prepare benchmark process");
                // Proc::run consumes the Store. Success includes the no-op
                // wasi:cli/run call and Store drop; failure includes start-trap
                // propagation and Store drop. This is not a pure Wasmtime timer.
                let start = Instant::now();
                let result = proc.run().await;
                elapsed += start.elapsed();
                if case == "instantiate_start_trap" {
                    let error = result.expect_err("guest start must trap");
                    assert!(format!("{error:#}").contains("unreachable"));
                } else {
                    result.expect("no-op P3 command");
                }
            }
            completion.wait().await.expect("filesystem cleanup");
        }
        report(case, "disabled", sample, operations, elapsed);
    }
}

#[test]
#[ignore = "subprocess entry point for measure_lifecycle_operations"]
fn lifecycle_overhead_probe() {
    let Ok(mode) = std::env::var(PROBE_MODE) else {
        return;
    };
    assert!(matches!(mode.as_str(), "disabled" | "enabled"));
    println!(
        "{}",
        serde_json::json!({
            "benchmark": "lifecycle_metadata", "label": label(),
            "cache_mode": mode, "debug_assertions": cfg!(debug_assertions),
            "warmups": WARMUPS, "samples": SAMPLES,
            "process_deadline_seconds": PROCESS_DEADLINE.as_secs(),
            "timing": "sum_of_individually_timed_operations",
            "instantiation_includes": "Proc::run, no-op command or start trap, Store drop",
            "store_includes": "Builder::build, pre-instantiation validation, Store drop",
            "filesystem_completion": "outside_timing",
        })
    );
    let (runtime, _publisher) = runtime_engine().expect("production runtime engine");
    assert_eq!(wasmtime_cache_metrics().snapshot().state.as_str(), mode);
    let bytes = wat::parse_str(COMMAND).expect("native P3 command");
    if mode == "enabled" {
        compile_component(&runtime.engine(), &bytes).expect("populate compilation cache");
        let before = wasmtime_cache_metrics().snapshot();
        compile_component(&runtime.engine(), &bytes).expect("verify warm compilation cache");
        assert_eq!(wasmtime_cache_metrics().snapshot().hits, before.hits + 1);
    }
    let before = wasmtime_cache_metrics().snapshot();
    measure_compile(&runtime, &mode, &bytes, false);
    let after = wasmtime_cache_metrics().snapshot();
    if mode == "enabled" {
        assert_eq!(
            after.hits - before.hits,
            ((WARMUPS + SAMPLES) * WARM_COMPILE_OPERATIONS) as u64,
            "every measured warm compilation must hit the persistent cache"
        );
    }
    measure_compile(&runtime, &mode, b"not a component", true);
    if mode == "enabled" {
        return;
    }

    // Fixture compilation is outside process timings and bypasses the canonical
    // accounting API. Timed process cases always use Program::Precompiled.
    let component = Arc::new(Component::from_binary(&runtime.engine(), &bytes).unwrap());
    let trap = COMMAND.replace(";; START_HOOK", "(func $start unreachable) (start $start)");
    let trap_bytes = wat::parse_str(trap).expect("trapping P3 start fixture");
    let trap_component = Arc::new(Component::from_binary(&runtime.engine(), &trap_bytes).unwrap());
    let local = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    local.block_on(async {
        measure_process(&runtime, &component, "instantiate_success").await;
        measure_process(&runtime, &trap_component, "instantiate_start_trap").await;
        measure_process(&runtime, &component, "store_build_drop").await;
    });
}
