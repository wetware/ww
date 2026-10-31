//! Candidate-only report of actual executable-instantiation duration.
//! This probe uses the new collector outside the shared comparison harness.
#![cfg(all(not(target_arch = "wasm32"), unix))]

#[path = "lifecycle_service.rs"]
mod service;

use std::sync::Arc;

#[test]
#[ignore = "candidate-only lifecycle boundary report; requires shared lock and AC monitoring"]
fn measure_observed_instantiation() {
    if cfg!(debug_assertions) {
        panic!("qualification requires release mode");
    }
    let artifact = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("target/authority-probe/wasm32-wasip3/release/authority_probe.wasm");
    let bytes = std::fs::read(&artifact).expect("real authority-probe fixture");
    let (runtime, mut publisher) = cell::engine::runtime_engine().unwrap();
    let component = Arc::new(cell::engine::compile_component(&runtime.engine(), &bytes).unwrap());
    let metrics = cell::engine::wasmtime_cache_metrics();
    let executor = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    println!(
        "{}",
        serde_json::json!({
            "benchmark": "lifecycle_observed_metadata",
            "artifact": artifact,
            "warmups": service::WARMUPS, "samples": service::SAMPLES,
            "boundary": "candidate collector duration delta around executable CommandPre::instantiate_async",
            "excluded": "preparation, fuel setup/settlement, command execution, Store destruction, filesystem cleanup",
            "comparison": "candidate-only descriptive durations; no baseline isolated duration claim",
        })
    );
    executor.block_on(async {
        tokio::time::timeout(std::time::Duration::from_secs(150), async {
            for sample in 0..service::WARMUPS + service::SAMPLES {
                let prepared =
                    service::prepare(cell::Program::Precompiled(component.clone()), &runtime).await;
                let before = metrics.lifecycle_snapshot().instantiation;
                service::execute(prepared, &mut publisher).await;
                let after = metrics.lifecycle_snapshot().instantiation;
                assert_eq!(after.success - before.success, 1);
                assert_eq!(after.error, before.error);
                assert_eq!(after.timeout, before.timeout);
                assert_eq!(after.duration.count - before.duration.count, 1);
                println!(
                    "{}",
                    serde_json::json!({
                        "benchmark": "lifecycle_observed", "case": "instantiate_observed",
                        "sample": sample, "warmup": sample < service::WARMUPS,
                        "operations": 1,
                        "wall_ns": after.duration.sum_nanos - before.duration.sum_nanos,
                    })
                );
            }
        })
        .await
        .expect("observed-instantiation campaign deadline");
    });
}
