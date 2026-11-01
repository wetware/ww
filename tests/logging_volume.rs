//! Source-backed logging volume for ordinary native P3 Cells on ExecutorPool.
//! Run with `cargo test --test logging_volume -- --nocapture` to print measurements.
//! Each capture includes pool startup, sequential Cell admission/build/run/cleanup,
//! and pool shutdown. A zero-Cell control identifies fixed lifecycle overhead.
//! This is a successful local execution workload; it does not model network load,
//! guest stderr, failures, or an application-specific mix of Host calls.
//!
//! Opt-in timing: run the already-built binary with
//! `--exact measure_cell_logging_cost --ignored --nocapture --test-threads=1`.
//! The caller must hold `/tmp/ww-fuel-coordination/build-benchmark.lock` and
//! continuously monitor AC power. Use the same compiler/profile for comparisons.
//! Timing excludes engine/component compilation and shutdown. It includes Cell
//! admission, preparation, execution, filesystem cleanup, and result delivery.
#![cfg(not(target_arch = "wasm32"))]

use std::io::{Read, Seek, SeekFrom};
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::time::{Duration, Instant};

use serde_json::Value;
use tokio::sync::{oneshot, watch};
use ww::services::{ExecutorPool, SpawnRequest};

const PROBE: &str = "WW_LOG_VOLUME_OPERATIONS";
const TIMING: &str = "WW_LOG_VOLUME_TIMING";
const OPERATIONS: usize = 16;
const DEADLINE: Duration = Duration::from_secs(60);
const TRACE_FILTER: &str =
    "ww=trace,atom=trace,cache=trace,cell=trace,ipfs=trace,membrane=trace,rpc=trace,stem=trace";
// The same native P3 no-op command used by tests/lifecycle_overhead.rs. Every
// operation executes wasi:cli/run through the production Cell builder and Proc.
const COMMAND: &str = r#"
    (component
      (core func $task-return (canon task.return (result (result))))
      (core module $command
        (import "" "task-return" (func $task-return (param i32)))
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

struct Capture {
    events: usize,
    bytes: usize,
    largest_event_bytes: usize,
    cost: Option<Cost>,
}

#[derive(Clone, Copy)]
struct Cost {
    wall_ns: u64,
    cpu_ns: Option<u64>,
}

fn capture(format: &str, filter: Option<&str>, operations: usize, timing: bool) -> Capture {
    let mut stderr = tempfile::tempfile().expect("capture file");
    let mut command = Command::new(std::env::current_exe().unwrap());
    command
        .args([
            "--exact",
            "logging_volume_probe",
            "--ignored",
            "--nocapture",
            "--test-threads=1",
        ])
        .env(PROBE, operations.to_string())
        .env(TIMING, if timing { "1" } else { "0" })
        .env("WW_LOG_FORMAT", format)
        .env("NO_COLOR", "1")
        .env_remove("RUST_LOG")
        .env_remove(cell::engine::CWASM_DIR_ENV)
        .env_remove(cell::engine::CWASM_CACHE_MAX_BYTES_ENV)
        .stdout(Stdio::piped())
        .stderr(stderr.try_clone().unwrap());
    if let Some(filter) = filter {
        command.env("RUST_LOG", filter);
    }
    let mut child = command.spawn().expect("logging volume subprocess");
    let deadline = Instant::now() + DEADLINE;
    while child.try_wait().unwrap().is_none() {
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            panic!("logging volume subprocess exceeded {DEADLINE:?}");
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    let output = child.wait_with_output().unwrap();
    stderr.seek(SeekFrom::Start(0)).unwrap();
    let mut logs = String::new();
    stderr.read_to_string(&mut logs).unwrap();
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(
        output.status.success(),
        "logging volume subprocess failed:\n{stdout}\n{logs}"
    );
    let result = stdout
        .lines()
        .find_map(|line| line.strip_prefix("WW_LOG_VOLUME_RESULT="))
        .expect("completed workload report");
    let result: Value = serde_json::from_str(result).unwrap();
    assert_eq!(result["completed"], operations);
    assert_eq!(result["panicked"], 0);
    assert_eq!(result["cancelled"], 0);
    assert!(!logs.contains('\x1b'), "unexpected ANSI in capture");
    assert!(logs.is_empty() || logs.ends_with('\n'));
    for line in logs.lines() {
        // Each event must use stderr, including events from executor threads.
        assert!(!stdout.contains(line), "log event appeared on stdout");
        if format == "json" {
            let event: Value = serde_json::from_str(line).expect("one JSON object per event");
            assert!(event.is_object());
            assert_ne!(
                event["level"], "WARN",
                "unexpected workload warning: {line}"
            );
            assert_ne!(event["level"], "ERROR", "unexpected workload error: {line}");
        }
    }
    Capture {
        events: logs.lines().count(),
        bytes: logs.len(),
        largest_event_bytes: logs.lines().map(|line| line.len() + 1).max().unwrap_or(0),
        cost: timing.then(|| Cost {
            wall_ns: result["wall_ns"].as_u64().expect("workload wall time"),
            cpu_ns: result["cpu_ns"].as_u64(),
        }),
    }
}

#[test]
fn representative_cell_logging_volume_is_bounded() {
    for format in ["json", "compact"] {
        for (level, filter) in [
            ("default_info", None),
            ("warn", Some("warn")),
            ("trace", Some(TRACE_FILTER)),
        ] {
            let control = capture(format, filter, 0, false);
            let workload = capture(format, filter, OPERATIONS, false);
            if level == "trace" {
                assert!(
                    workload.events > control.events,
                    "TRACE must exercise Cell diagnostics"
                );
                // Allow finite startup diagnostics and epoch-boundary variability.
                // A new per-tick/per-hook emitter must not silently amplify output.
                assert!(workload.events <= control.events + OPERATIONS * 24);
            } else {
                assert_eq!(
                    workload.events, control.events,
                    "successful Cells must not add ordinary INFO/WARN events"
                );
            }
            assert!(workload.largest_event_bytes <= 16_384);
            println!(
                "{}",
                serde_json::json!({
                    "measurement": "logging_volume",
                    "workload": "executor_native_p3_noop",
                    "format": format,
                    "level": level,
                    "operations": OPERATIONS,
                    "events": workload.events,
                    "bytes": workload.bytes,
                    "events_per_operation": workload.events as f64 / OPERATIONS as f64,
                    "bytes_per_operation": workload.bytes as f64 / OPERATIONS as f64,
                    "control_events": control.events,
                    "control_bytes": control.bytes,
                    "incremental_events_per_operation":
                        (workload.events as f64 - control.events as f64) / OPERATIONS as f64,
                    "incremental_bytes_per_operation":
                        (workload.bytes as f64 - control.bytes as f64) / OPERATIONS as f64,
                    "largest_event_bytes": workload.largest_event_bytes,
                    "timing": "not_measured",
                    "sink": "regular_file",
                })
            );
        }
    }
}

#[test]
#[ignore = "timed Cell workload; caller must hold shared lock and monitor AC power"]
fn measure_cell_logging_cost() {
    const TIMED_OPERATIONS: usize = 256;
    const WARMUPS: usize = 2;
    const SAMPLES: usize = 7;
    let cases = [
        ("json", "off", Some("off")),
        ("json", "default_info", None),
        ("json", "trace", Some(TRACE_FILTER)),
        ("compact", "off", Some("off")),
        ("compact", "default_info", None),
        ("compact", "trace", Some(TRACE_FILTER)),
    ];
    let mut samples: Vec<Vec<Cost>> = vec![Vec::with_capacity(SAMPLES); cases.len()];
    println!(
        "{}",
        serde_json::json!({
            "measurement": "logging_cost_metadata",
            "workload": "executor_native_p3_noop",
            "operations_per_sample": TIMED_OPERATIONS,
            "warmups": WARMUPS,
            "samples": SAMPLES,
            "debug_assertions": cfg!(debug_assertions),
            "timing": "Cell admission/build/run/filesystem cleanup/result delivery",
            "excluded": "engine/component compilation and shutdown",
            "cpu": "process user plus system time via getrusage where available",
            "allocations": "not_measured",
            "sink": "regular_file",
            "power_and_lock": "externally_enforced",
        })
    );
    for iteration in 0..WARMUPS + SAMPLES {
        // Reverse each pair to reduce a fixed mode-order bias. Every case gets
        // its own process and therefore identical subscriber/cache isolation.
        let order: Vec<usize> = if iteration.is_multiple_of(2) {
            (0..cases.len()).collect()
        } else {
            (0..cases.len()).rev().collect()
        };
        for index in order {
            let (format, level, filter) = cases[index];
            let capture = capture(format, filter, TIMED_OPERATIONS, true);
            let cost = capture.cost.expect("timed capture");
            assert!(capture.largest_event_bytes <= 16_384);
            if level == "off" {
                assert_eq!(capture.events, 0, "disabled logging control");
            }
            println!(
                "{}",
                serde_json::json!({
                    "measurement": "logging_cost_sample",
                    "format": format,
                    "level": level,
                    "iteration": iteration,
                    "warmup": iteration < WARMUPS,
                    "operations": TIMED_OPERATIONS,
                    "wall_ns": cost.wall_ns,
                    "cpu_ns": cost.cpu_ns,
                    "wall_ns_per_operation": cost.wall_ns as f64 / TIMED_OPERATIONS as f64,
                    "cpu_ns_per_operation": cost.cpu_ns.map(|n| n as f64 / TIMED_OPERATIONS as f64),
                    "events_including_startup_and_shutdown": capture.events,
                    "bytes_including_startup_and_shutdown": capture.bytes,
                })
            );
            if iteration >= WARMUPS {
                samples[index].push(cost);
            }
        }
    }
    for (index, (format, level, _)) in cases.iter().enumerate() {
        let summarize = |mut values: Vec<u64>| {
            values.sort_unstable();
            if values.is_empty() {
                return Value::Null;
            }
            let per_operation = |n: u64| n as f64 / TIMED_OPERATIONS as f64;
            serde_json::json!({
                "samples": values.len(),
                "min_ns_per_operation": per_operation(values[0]),
                "median_ns_per_operation": per_operation(values[values.len() / 2]),
                "max_ns_per_operation": per_operation(*values.last().unwrap()),
            })
        };
        println!(
            "{}",
            serde_json::json!({
                "measurement": "logging_cost_summary",
                "format": format,
                "level": level,
                "wall": summarize(samples[index].iter().map(|cost| cost.wall_ns).collect()),
                "cpu": summarize(samples[index].iter().filter_map(|cost| cost.cpu_ns).collect()),
            })
        );
    }
}

fn process_cpu_ns() -> Option<u64> {
    #[cfg(unix)]
    {
        let mut usage = std::mem::MaybeUninit::<libc::rusage>::uninit();
        // SAFETY: getrusage initializes the valid output pointer on success.
        if unsafe { libc::getrusage(libc::RUSAGE_SELF, usage.as_mut_ptr()) } != 0 {
            return None;
        }
        // SAFETY: the successful getrusage call initialized this value.
        let usage = unsafe { usage.assume_init() };
        let nanoseconds = |time: libc::timeval| {
            u64::try_from(time.tv_sec)
                .ok()?
                .checked_mul(1_000_000_000)?
                .checked_add(u64::try_from(time.tv_usec).ok()?.checked_mul(1_000)?)
        };
        nanoseconds(usage.ru_utime)?.checked_add(nanoseconds(usage.ru_stime)?)
    }
    #[cfg(not(unix))]
    None
}

#[test]
#[ignore = "subprocess entry point for representative_cell_logging_volume_is_bounded"]
fn logging_volume_probe() {
    let Ok(operations) = std::env::var(PROBE) else {
        return;
    };
    let operations: usize = operations.parse().unwrap();
    assert!(operations <= 1_024, "bounded workload");
    ww::config::init_tracing();
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    runtime.block_on(async {
        let (owner, resources) = cache::HostFsOwner::start(
            tokio::runtime::Handle::current(),
            cache::HostLimits::new(4_096, 1_025, 1_024).unwrap(),
        )
        .unwrap();
        let (_shutdown, shutdown_rx) = watch::channel(());
        let pool = ExecutorPool::new(1, shutdown_rx, Arc::clone(&resources));
        let runtime_engine = pool.runtime_engine();
        let bytes = wat::parse_str(COMMAND).expect("native P3 fixture");
        let component = Arc::new(
            wasmtime::component::Component::from_binary(&runtime_engine.engine(), &bytes)
                .expect("compile native P3 fixture"),
        );
        let timing = std::env::var(TIMING).as_deref() == Ok("1");
        let cpu_start = if timing { process_cpu_ns() } else { None };
        let wall_start = timing.then(Instant::now);
        for _ in 0..operations {
            let component = Arc::clone(&component);
            let runtime_engine = runtime_engine.clone();
            let resources = Arc::clone(&resources);
            let (result_tx, result_rx) = oneshot::channel();
            assert!(pool
                .spawn(SpawnRequest {
                    name: "logging-volume".into(),
                    factory: Box::new(move |_| {
                        Box::pin(async move {
                            let ownership = cell::FilesystemOwnership::try_new(
                                cache::CellResources::new(resources, cache::CellLimits::default()),
                            )
                            .unwrap();
                            let (builder, _handles) = cell::Builder::ordinary(
                                cell::Program::Precompiled(component),
                                tokio::io::empty(),
                                tokio::io::sink(),
                                tokio::io::sink(),
                                ownership,
                            );
                            let completion = builder.filesystem_completion();
                            builder
                                .with_runtime_engine(runtime_engine)
                                .build()
                                .await
                                .expect("prepare real Cell")
                                .run()
                                .await
                                .expect("run real Cell");
                            completion.wait().await.expect("Cell filesystem cleanup");
                        })
                    }),
                    result_tx: Some(result_tx),
                })
                .is_ok());
            assert_eq!(
                tokio::time::timeout(DEADLINE, result_rx)
                    .await
                    .expect("Cell deadline")
                    .expect("Cell result channel")
                    .expect("Cell executor result"),
                0
            );
        }
        let wall_ns = wall_start.map(|start| start.elapsed().as_nanos());
        let cpu_ns = cpu_start.and_then(|start| process_cpu_ns()?.checked_sub(start));
        let metrics = pool.metrics();
        pool.shutdown_and_join().await;
        owner.shutdown().await.expect("Host filesystem shutdown");
        let snapshot = metrics.snapshot();
        assert_eq!(snapshot.occupied, 0);
        assert_eq!(snapshot.accepted as usize, operations);
        // stdout holds only test harness output and this structured command result.
        println!(
            "\nWW_LOG_VOLUME_RESULT={}",
            serde_json::json!({
                "completed": snapshot.completed,
                "cancelled": snapshot.cancelled,
                "panicked": snapshot.panicked,
                "wall_ns": wall_ns,
                "cpu_ns": cpu_ns,
            })
        );
    });
}
