//! Product-level lifecycle qualification using APIs available at baseline 49099f53.
//! Run the same release executable source on both revisions, under the shared
//! build/benchmark lock with external AC monitoring and a 180-second deadline.
#![cfg(all(not(target_arch = "wasm32"), unix))]

mod support;

use std::sync::Arc;
use std::time::{Duration, Instant};

use cell::engine::{compile_component, runtime_engine, EpochPublisher, RuntimeEngine};
use cell::{Builder, Proc, Program};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

pub(crate) const WARMUPS: usize = 3;
pub(crate) const SAMPLES: usize = 31;
const OUTPUT_LIMIT: u64 = 64 * 1024;

struct Usage {
    cpu_ns: u64,
    peak_rss_bytes: u64,
}

fn usage() -> Usage {
    let mut value = std::mem::MaybeUninit::<libc::rusage>::uninit();
    // SAFETY: getrusage writes a complete rusage on success; check before reading.
    let status = unsafe { libc::getrusage(libc::RUSAGE_SELF, value.as_mut_ptr()) };
    assert_eq!(status, 0, "getrusage failed");
    // SAFETY: the successful call above initialized the structure.
    let value = unsafe { value.assume_init() };
    let nanos = |time: libc::timeval| {
        u64::try_from(time.tv_sec).unwrap() * 1_000_000_000
            + u64::try_from(time.tv_usec).unwrap() * 1_000
    };
    let rss = u64::try_from(value.ru_maxrss).unwrap();
    Usage {
        cpu_ns: nanos(value.ru_utime) + nanos(value.ru_stime),
        // Darwin returns bytes; Linux and other supported Unix targets use KiB.
        peak_rss_bytes: if cfg!(target_os = "macos") {
            rss
        } else {
            rss * 1024
        },
    }
}

pub(crate) struct Measurement {
    wall_ns: u128,
    cpu_ns: u64,
    peak_rss_bytes: u64,
}

impl Measurement {
    fn finish(start: Instant, before: Usage) -> Self {
        let wall_ns = start.elapsed().as_nanos();
        let after = usage();
        Self {
            wall_ns,
            cpu_ns: after.cpu_ns.checked_sub(before.cpu_ns).unwrap(),
            peak_rss_bytes: after.peak_rss_bytes,
        }
    }

    fn report(self, case: &str, sample: usize) {
        self.report_with_preparation(case, sample, None);
    }

    fn report_with_preparation(self, case: &str, sample: usize, preparation_wall_ns: Option<u128>) {
        println!(
            "{}",
            serde_json::json!({
                "benchmark": "lifecycle_service", "case": case,
                "label": std::env::var("WW_LIFECYCLE_SERVICE_LABEL")
                    .unwrap_or_else(|_| "unspecified".into()),
                "sample": sample, "warmup": sample < WARMUPS,
                "operations": 1, "wall_ns": self.wall_ns,
                "cpu_ns": self.cpu_ns, "peak_rss_bytes": self.peak_rss_bytes,
                "preparation_wall_ns": preparation_wall_ns,
            })
        );
    }
}

pub(crate) struct Prepared {
    proc: Proc,
    completion: cache::FsCompletion,
    stdout: tokio::io::DuplexStream,
    transport: cell::p3::HostTransport,
}

pub(crate) async fn prepare(program: Program, runtime: &RuntimeEngine) -> Prepared {
    let (stdout, writer) = tokio::io::duplex(OUTPUT_LIMIT as usize);
    let (builder, mut handles) = Builder::ordinary(
        program,
        tokio::io::empty(),
        writer,
        tokio::io::sink(),
        support::filesystem_ownership(),
    );
    let completion = builder.filesystem_completion();
    let transport = handles.take_host_stream().unwrap();
    let proc = builder
        .with_runtime_engine(runtime.clone())
        .with_args(vec!["authority-probe".into(), "no-graft".into()])
        .build()
        .await
        .expect("prepare compiled Rust native P3 command");
    Prepared {
        proc,
        completion,
        stdout,
        transport,
    }
}

pub(crate) async fn execute(prepared: Prepared, publisher: &mut EpochPublisher) -> Measurement {
    let Prepared {
        proc,
        completion,
        stdout,
        mut transport,
    } = prepared;
    let guest = async {
        let before = usage();
        let start = Instant::now();
        let run = proc.run();
        tokio::pin!(run);
        let mut interval = tokio::time::interval(Duration::from_millis(10));
        let result = loop {
            tokio::select! {
                result = &mut run => break result,
                _ = interval.tick() => { publisher.tick(); }
            }
        };
        let measurement = Measurement::finish(start, before);
        result.expect("real P3 command succeeds");
        measurement
    };
    let transport_io = async {
        let mut discarded = Vec::new();
        (&mut transport)
            .take(OUTPUT_LIMIT + 1)
            .read_to_end(&mut discarded)
            .await
            .unwrap();
        assert!(discarded.len() <= OUTPUT_LIMIT as usize);
        transport.shutdown().await.unwrap();
    };
    let output = async {
        let mut bytes = Vec::new();
        stdout
            .take(OUTPUT_LIMIT + 1)
            .read_to_end(&mut bytes)
            .await
            .unwrap();
        assert!(bytes.len() <= OUTPUT_LIMIT as usize);
        bytes
    };
    let (measurement, (), output) = tokio::time::timeout(Duration::from_secs(10), async {
        tokio::join!(guest, transport_io, output)
    })
    .await
    .expect("compiled P3 command deadline");
    let output: serde_json::Value = serde_json::from_slice(&output).expect("P3 stdout JSON");
    assert_eq!(output["mode"], "no-graft");
    assert_eq!(output["ok"], true);
    tokio::time::timeout(Duration::from_secs(10), completion.wait())
        .await
        .expect("filesystem cleanup deadline")
        .expect("filesystem cleanup succeeds");
    measurement
}

#[test]
#[ignore = "prospective service qualification; run with shared lock and AC monitoring"]
fn measure_lifecycle_service() {
    if cfg!(debug_assertions) {
        panic!("qualification requires release mode");
    }
    assert!(
        std::env::var_os(cell::engine::CWASM_DIR_ENV).is_none(),
        "uncached qualification requires an unset WW_CWASM_DIR"
    );
    let artifact = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("target/authority-probe/wasm32-wasip3/release/authority_probe.wasm");
    let bytes =
        std::fs::read(&artifact).expect("build or restore the real authority-probe fixture");
    let (runtime, mut publisher) = runtime_engine().unwrap();
    assert_eq!(
        cell::engine::wasmtime_cache_metrics()
            .snapshot()
            .state
            .as_str(),
        "disabled"
    );
    let executor = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    // Host-wide resources are already running when a Cell is launched in service.
    let _resources = support::filesystem_resources();
    println!(
        "{}",
        serde_json::json!({
            "benchmark": "lifecycle_service_metadata", "artifact": artifact,
            "artifact_bytes": bytes.len(), "warmups": WARMUPS, "samples": SAMPLES,
            "operations_per_sample": 1, "cache_mode": "disabled",
            "launch_includes": "byte clone or Arc clone, I/O setup, filesystem ownership, Builder::build, Proc::run, stdout validation, transport drain, filesystem cleanup",
            "launch_excludes": "fixture disk read, process/runtime/Host-wide filesystem owner startup",
            "preparation_wall_ns": "report-only prepare() interval: bounded I/O and filesystem ownership setup plus Builder::build; includes compilation for launch_bytes; excludes input byte/Arc clone",
            "proc_run_includes": "initial fuel, executable instantiation, real guest command, Store drop; concurrent I/O draining may overlap",
            "proc_run_excludes": "compilation, preparation, output validation, filesystem completion wait",
            "cpu": "process user+system getrusage delta; includes filesystem worker threads",
            "memory": "process lifetime peak RSS, not per-operation live memory",
        })
    );
    let engine = runtime.engine();
    for sample in 0..WARMUPS + SAMPLES {
        let before = usage();
        let start = Instant::now();
        let result = compile_component(&engine, &bytes);
        let measurement = Measurement::finish(start, before);
        drop(result.expect("compile real Rust P3 component"));
        measurement.report("compile_uncached", sample);
    }
    let component = Arc::new(compile_component(&runtime.engine(), &bytes).unwrap());
    executor.block_on(async {
        tokio::time::timeout(Duration::from_secs(150), async {
            for sample in 0..WARMUPS + SAMPLES {
                let before = usage();
                let start = Instant::now();
                let program = Program::Bytes(bytes.clone());
                let prepare_start = Instant::now();
                let prepared = prepare(program, &runtime).await;
                let preparation_wall_ns = prepare_start.elapsed().as_nanos();
                execute(prepared, &mut publisher).await;
                Measurement::finish(start, before).report_with_preparation(
                    "launch_bytes",
                    sample,
                    Some(preparation_wall_ns),
                );
            }
            for sample in 0..WARMUPS + SAMPLES {
                let before = usage();
                let start = Instant::now();
                let program = Program::Precompiled(component.clone());
                let prepare_start = Instant::now();
                let prepared = prepare(program, &runtime).await;
                let preparation_wall_ns = prepare_start.elapsed().as_nanos();
                execute(prepared, &mut publisher).await;
                Measurement::finish(start, before).report_with_preparation(
                    "launch_precompiled",
                    sample,
                    Some(preparation_wall_ns),
                );
            }
            for sample in 0..WARMUPS + SAMPLES {
                let prepared = prepare(Program::Precompiled(component.clone()), &runtime).await;
                execute(prepared, &mut publisher)
                    .await
                    .report("proc_run", sample);
            }
        })
        .await
        .expect("service campaign deadline");
    });
}
