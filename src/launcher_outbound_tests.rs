use super::*;
use futures::FutureExt;
use std::time::Duration;

// Removing the lifecycle's close before teardown makes these real guest
// outcomes leave admission open even though Process.wait reports completion.
#[tokio::test(flavor = "current_thread")]
async fn real_cell_exit_and_trap_close_outbound_admission_before_cleaned() {
    tokio::task::LocalSet::new()
        .run_until(async {
            for (command, expected_exit) in [("i32.const 0 call $return", 0), ("unreachable", 1)] {
                let registry = rpc::outbound::OutboundRegistry::default();
                let effects = registry.register().unwrap();
                let completion = effects.completion();
                let wasm = wat::parse_str(format!(
                    r#"(component
                (core func $return (canon task.return (result (result))))
                (core module $command
                    (import "" "return" (func $return (param i32)))
                    (func (export "run") {command}))
                (core instance $i (instantiate $command
                    (with "" (instance (export "return" (func $return))))))
                (func $run async (result (result))
                    (canon lift (core func $i "run") async))
                (instance (export (interface "wasi:cli/run@0.3.0"))
                    (export "run" (func $run))))"#
                ))
                .unwrap();
                let (mut builder, mut handles) = Builder::ordinary(
                    Program::Bytes(wasm),
                    io::empty(),
                    io::sink(),
                    io::sink(),
                    tests::test_filesystem_ownership(),
                );
                let (engine, _publisher) = cell::engine::runtime_engine().unwrap();
                builder = builder.with_runtime_engine(engine);
                let filesystem_completion = builder.filesystem_completion();
                let (mut lifecycle, process, _, _, observer) = tests::lifecycle_loss_fixture();
                lifecycle.proc = Some(builder.build().await.unwrap());
                lifecycle.filesystem_completion = filesystem_completion;
                let (reader, writer) = handles.take_host_split().unwrap();
                let (rpc, _) = graft::build_child_membrane_rpc_owned(
                    reader,
                    writer,
                    lifecycle.membrane.as_ref().unwrap().clone(),
                    effects.origin(),
                );
                lifecycle.rpc_task = Some(rpc);
                lifecycle.effects = effects;
                tokio::time::timeout(Duration::from_secs(5), lifecycle.run())
                    .await
                    .unwrap();
                let outcome = process.wait_request().send().promise.await.unwrap();
                assert_eq!(outcome.get().unwrap().get_exit_code(), expected_exit);
                assert_eq!(observer.state(), rpc::CleanupState::Cleaned(expected_exit));
                let snapshot = completion
                    .wait()
                    .now_or_never()
                    .expect("effects completed before Process.wait")
                    .unwrap();
                assert!(snapshot.closed);
                assert_eq!(snapshot.active, 0);
                assert!(!snapshot.failed);
            }
        })
        .await;
}

// The guest is a real running P3 Cell. Native negotiation is held by a test
// future, then its actual worker and join monitor are destroyed. This checks
// failed outbound completion propagation through Process.wait, independently
// of the existing guest-initiated real-libp2p pending-cleanup tests.
#[tokio::test(flavor = "current_thread")]
async fn real_cell_kill_does_not_publish_cleaned_after_outbound_worker_loss() {
    tokio::task::LocalSet::new()
        .run_until(async {
            let registry = rpc::outbound::OutboundRegistry::default();
            let effects = registry.register().unwrap();
            let completion = effects.completion();
            let workers = tokio::task::LocalSet::new();
            let (entered_tx, entered_rx) = oneshot::channel();
            let dial = {
                let _entered = workers.enter();
                rpc::vat_dial::connect_owned::<
                    futures::io::Cursor<Vec<u8>>,
                    capnp::capability::Client,
                    _,
                >(&registry, Some(effects.origin()), async move {
                    entered_tx.send(()).unwrap();
                    std::future::pending().await
                })
                .unwrap()
            };
            workers.run_until(entered_rx).await.unwrap();
            let wasm =
                wat::parse_str(include_str!("../tests/fixtures/spinning-component.wat")).unwrap();
            let fuel = FuelObserver::default();
            let (mut builder, mut handles) = Builder::ordinary(
                Program::Bytes(wasm),
                io::empty(),
                io::sink(),
                io::sink(),
                tests::test_filesystem_ownership(),
            );
            let (engine, _publisher) = cell::engine::runtime_engine().unwrap();
            builder = builder
                .with_runtime_engine(engine)
                .with_fuel_estimator(FuelEstimator::new_oneshot(u64::MAX, 10_000, 0))
                .with_fuel_observer(fuel.clone());
            let filesystem = builder.filesystem_completion();
            let (mut lifecycle, process, _, _, cleanup) = tests::lifecycle_loss_fixture();
            lifecycle.proc = Some(builder.build().await.unwrap());
            lifecycle.filesystem_completion = filesystem.clone();
            let (reader, writer) = handles.take_host_split().unwrap();
            let (rpc, _) = graft::build_child_membrane_rpc_owned(
                reader,
                writer,
                lifecycle.membrane.as_ref().unwrap().clone(),
                effects.origin(),
            );
            lifecycle.rpc_task = Some(rpc);
            lifecycle.effects = effects;
            let owner = tokio::task::spawn_local(lifecycle.run());
            tokio::time::timeout(Duration::from_secs(5), fuel.wait_for_suspension(1))
                .await
                .unwrap();
            assert_eq!(completion.snapshot().active, 1);
            assert!(!completion.snapshot().failed);
            drop(workers);
            assert!(completion.snapshot().failed);
            assert_eq!(completion.snapshot().active, 1);
            assert!(dial.await.is_err());
            process.kill_request().send().promise.await.unwrap();
            tokio::time::timeout(Duration::from_secs(5), owner)
                .await
                .unwrap()
                .unwrap();
            filesystem.wait().await.unwrap();
            assert!(completion.snapshot().closed);
            assert_eq!(
                cleanup.state(),
                rpc::CleanupState::Lost(rpc::LostReason::OwnerDropped)
            );
            assert!(tokio::time::timeout(
                Duration::from_secs(2),
                process.wait_request().send().promise
            )
            .await
            .unwrap()
            .is_err());
        })
        .await;
}
