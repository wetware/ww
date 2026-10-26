//! Opt-in measurements using only APIs available at baseline 400ec876.
//! Copy this file unchanged into that checkout and use the same release profile.
//! No FuelObserver is installed inside timed work.
#![cfg(not(target_arch = "wasm32"))]

mod support;

use std::process::Command;
use std::sync::{Arc, Barrier};
use std::time::{Duration, Instant};

use cell::engine::{compile_component, runtime_engine, RuntimeEngine};
use cell::proc::{FuelEstimator, FuelObserver};
use cell::{Builder, Proc, Program};
use wasmtime::component::Component;

const PROBE: &str = "WW_FUEL_OVERHEAD_PROBE";
const WARMUPS: usize = 3;
const SAMPLES: usize = 31;
const DEADLINE: Duration = Duration::from_secs(10);

#[test]
#[ignore = "opt-in real P3 fuel measurements; no machine-dependent assertions"]
fn measure_fuel_operations() {
    let mut child = Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "fuel_overhead_probe",
            "--ignored",
            "--nocapture",
            "--test-threads=1",
        ])
        .env(PROBE, "1")
        .env_remove(cell::engine::CWASM_DIR_ENV)
        .env_remove(cell::engine::CWASM_CACHE_MAX_BYTES_ENV)
        .spawn()
        .expect("isolated fuel measurement process");
    let deadline = Instant::now() + Duration::from_secs(180);
    loop {
        if let Some(status) = child.try_wait().unwrap() {
            assert!(status.success(), "fuel measurement failed: {status}");
            break;
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            panic!("fuel measurement exceeded 180 seconds");
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

#[derive(Clone, Copy, Debug)]
enum Work {
    Compute,
    Io,
    Boundaries,
    Oneshot,
}

impl Work {
    fn name(self) -> &'static str {
        match self {
            Self::Compute => "scheduled_compute",
            Self::Io => "scheduled_io",
            Self::Boundaries => "scheduled_host_boundaries",
            Self::Oneshot => "oneshot_suspend_resume",
        }
    }

    fn operations(self) -> usize {
        match self {
            Self::Compute => 4_000_000,
            Self::Io => 32,
            Self::Boundaries => 100_000,
            Self::Oneshot => 2_000,
        }
    }

    fn unit(self) -> &'static str {
        match self {
            Self::Compute | Self::Oneshot => "guest_loop_iteration",
            Self::Io => "async_timer_wait",
            Self::Boundaries => "host_return",
        }
    }
}

fn label() -> String {
    std::env::var("WW_FUEL_BENCH_LABEL").unwrap_or_else(|_| "unspecified".into())
}

async fn prepare(
    runtime: &RuntimeEngine,
    component: &Arc<Component>,
    work: Work,
    observer: Option<FuelObserver>,
) -> (Proc, cache::FsCompletion) {
    let (mut builder, _handles) = Builder::ordinary(
        Program::Precompiled(Arc::clone(component)),
        tokio::io::empty(),
        tokio::io::sink(),
        tokio::io::sink(),
        support::filesystem_ownership(),
    );
    builder = builder.with_runtime_engine(runtime.clone());
    if matches!(work, Work::Oneshot) {
        // More cumulative authority than the finite guest needs, with several
        // epoch limits per execution. The periodic publisher resumes the guest.
        builder = builder.with_fuel_estimator(FuelEstimator::new_oneshot(1_000_000, 2_000, 2_000));
    }
    if let Some(observer) = observer {
        builder = builder.with_fuel_observer(observer);
    }
    let completion = builder.filesystem_completion();
    (builder.build().await.expect("prepare P3 guest"), completion)
}

#[test]
#[ignore = "subprocess entry point for measure_fuel_operations"]
fn fuel_overhead_probe() {
    if std::env::var(PROBE).as_deref() != Ok("1") {
        return;
    }
    println!(
        "{}",
        serde_json::json!({
            "benchmark": "fuel_metadata", "label": label(),
            "debug_assertions": cfg!(debug_assertions),
            "warmups": WARMUPS, "samples": SAMPLES,
            "epoch_ms": cell::sched::EPOCH_TICK_MS,
            "timed_observer": false,
        })
    );
    // Compile every fixture before any timing or worker creation.
    let (runtime, mut publisher) = runtime_engine().expect("production runtime engine");
    let cases: Vec<_> = [Work::Compute, Work::Io, Work::Boundaries, Work::Oneshot]
        .into_iter()
        .map(|work| {
            let bytes = wat::parse_str(fixture(work)).expect("benchmark WAT");
            let component = compile_component(&runtime.engine(), &bytes).expect("compile P3 guest");
            (work, Arc::new(component))
        })
        .collect();

    // An observer-free timed case must not silently stop exercising suspension.
    // Verify its exact fixture and policy with a deterministic, untimed preflight.
    let local = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    local.block_on(async {
        let observer = FuelObserver::default();
        let (proc, completion) = prepare(
            &runtime,
            &cases[3].1,
            Work::Oneshot,
            Some(observer.clone()),
        )
        .await;
        let mut task = Box::pin(proc.run());
        let drive = async {
            for suspension in 1..=2 {
                tokio::select! {
                    result = &mut task => panic!("one-shot exited before suspension {suspension}: {result:?}"),
                    () = observer.wait_for_suspension(suspension) => {},
                }
                publisher.tick();
            }
            loop {
                tokio::select! {
                    result = &mut task => { result.expect("one-shot resumed and completed"); break; },
                    () = tokio::time::sleep(Duration::from_millis(1)) => { publisher.tick(); },
                }
            }
        };
        tokio::time::timeout(DEADLINE, drive).await.expect("one-shot preflight deadline");
        drop(task);
        completion.wait().await.expect("preflight filesystem cleanup");
        println!("{}", serde_json::json!({
            "benchmark": "fuel_preflight", "label": label(),
            "oneshot_suspensions": observer.suspension_count(),
        }));
    });
    drop(local);

    let (stop_tx, stop_rx) = std::sync::mpsc::channel();
    let ticker = std::thread::spawn(move || {
        while stop_rx
            .recv_timeout(Duration::from_millis(cell::sched::EPOCH_TICK_MS))
            .is_err()
        {
            publisher.tick();
        }
    });
    for workers in [1, 4] {
        for (work, component) in &cases {
            measure_case(&runtime, component, *work, workers);
        }
    }
    stop_tx.send(()).unwrap();
    ticker.join().unwrap();
}

fn measure_case(runtime: &RuntimeEngine, component: &Arc<Component>, work: Work, workers: usize) {
    // Threads and their runtimes persist across samples. One real Cell per
    // thread shares the runtime engine and (when instrumented) its collector.
    let barrier = Arc::new(Barrier::new(workers));
    let samples = std::thread::scope(|scope| {
        let tasks: Vec<_> = (0..workers)
            .map(|_| {
                let barrier = Arc::clone(&barrier);
                scope.spawn(move || {
                    let local = tokio::runtime::Builder::new_current_thread()
                        .enable_all()
                        .build()
                        .unwrap();
                    local.block_on(async {
                        let mut samples = Vec::new();
                        for _ in 0..WARMUPS + SAMPLES {
                            let (proc, completion) = prepare(runtime, component, work, None).await;
                            barrier.wait();
                            let start = Instant::now();
                            tokio::time::timeout(DEADLINE, proc.run())
                                .await
                                .expect("P3 benchmark execution deadline")
                                .expect("P3 benchmark execution");
                            samples.push(start.elapsed());
                            barrier.wait();
                            completion.wait().await.expect("sample filesystem cleanup");
                        }
                        samples
                    })
                })
            })
            .collect();
        tasks
            .into_iter()
            .map(|task| task.join().unwrap())
            .collect::<Vec<_>>()
    });
    for sample in WARMUPS..WARMUPS + SAMPLES {
        let per_worker: Vec<_> = samples.iter().map(|row| row[sample].as_nanos()).collect();
        let elapsed_ns = *per_worker.iter().max().unwrap();
        let operations = workers * work.operations();
        println!(
            "{}",
            serde_json::json!({
                "benchmark": "fuel", "label": label(), "case": work.name(),
                "workers": workers, "sample": sample - WARMUPS,
                "operations": operations, "unit": work.unit(),
                "elapsed_ns": elapsed_ns, "worker_elapsed_ns": per_worker,
                "ns_per_operation": elapsed_ns as f64 / operations as f64,
                "operations_per_second": operations as f64 * 1e9 / elapsed_ns as f64,
            })
        );
    }
}

fn fixture(work: Work) -> String {
    let (imports, lower, core_import, export, body) = match work {
        Work::Compute | Work::Oneshot => ("", "", "", "", ""),
        Work::Boundaries => (
            r#"(import "wasi:clocks/monotonic-clock@0.3.0" (instance $clock
                (export "now" (func (result u64)))))"#,
            r#"(core func $host (canon lower (func $clock "now")))"#,
            r#"(import "" "host" (func $host (result i64)))"#,
            r#"(export "host" (func $host))"#,
            "call $host drop",
        ),
        Work::Io => (
            r#"(import "wasi:clocks/monotonic-clock@0.3.0" (instance $clock
                (export "wait-for" (func async (param "how-long" u64)))))"#,
            r#"(core func $host (canon lower (func $clock "wait-for")))"#,
            r#"(import "" "host" (func $host (param i64)))"#,
            r#"(export "host" (func $host))"#,
            "i64.const 1 call $host",
        ),
    };
    format!(
        r#"(component
          {imports}
          {lower}
          (core func $done (canon task.return (result (result))))
          (core module $guest
            (import "" "done" (func $done (param i32)))
            {core_import}
            (func (export "run") (local $left i32)
              i32.const {iterations} local.set $left
              (loop $loop
                {body}
                local.get $left i32.const 1 i32.sub local.tee $left br_if $loop)
              i32.const 0 call $done))
          (core instance $instance (instantiate $guest
            (with "" (instance (export "done" (func $done)) {export}))))
          (func $run async (result (result))
            (canon lift (core func $instance "run") async))
          (instance (export (interface "wasi:cli/run@0.3.0"))
            (export "run" (func $run))))"#,
        iterations = work.operations(),
    )
}
