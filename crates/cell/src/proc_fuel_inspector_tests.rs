// Included inside proc::tests. Independent witnesses are Store::get_fuel,
// configured authority, and the existing application counters, not FuelObserver.

fn inspector_attach_store(
    store: &mut Store<ComponentRunStates>,
) -> crate::fuel_inspector::FuelInspector {
    let (owner, inspector) = crate::fuel_inspector::Owner::new();
    store.data_mut().fuel_inspector = Some(Box::new(owner));
    store.call_hook_async(FuelCallHook::<true>);
    inspector
}

async fn inspector_process(
    runtime: crate::engine::RuntimeEngine,
    estimator: FuelEstimator,
    component: &str,
) -> (Proc, crate::fuel_inspector::FuelInspector) {
    let (builder, _handles) = Builder::ordinary(
        Program::Bytes(wat::parse_str(component).expect("inspector component")),
        tokio::io::empty(),
        tokio::io::sink(),
        tokio::io::sink(),
        FilesystemOwnership::test(),
    );
    let mut process = builder
        .with_runtime_engine(runtime)
        .with_fuel_estimator(estimator)
        .build()
        .await
        .expect("prepare inspected process");
    let inspector = process.fuel_inspector().expect("attach inspector");
    assert!(
        process.fuel_inspector().is_err(),
        "attachment must be unique"
    );
    (process, inspector)
}

#[tokio::test]
async fn inspector_scheduled_ewma_and_physical_fuel_have_independent_witnesses() {
    use crate::fuel_inspector::{FuelInspectionPolicy, FuelState};

    for (consumed, expected_ratio, expected_quantum) in
        [(0, 0, MAX_FUEL), (INITIAL_FUEL, RATIO_SCALE, MIN_FUEL)]
    {
        let (runtime, _publisher) = crate::engine::runtime_engine().unwrap();
        let mut store = fuel_metrics_store(&runtime, FuelEstimator::new(INITIAL_FUEL));
        let inspector = inspector_attach_store(&mut store);
        install_fuel_slice_in_store_with_inspector::<true>(&mut store, INITIAL_FUEL).unwrap();
        let physical_before = store.get_fuel().unwrap();
        store.set_fuel(physical_before - consumed).unwrap();
        settle_and_apply_fuel_boundary_with_inspector::<true>(
            &mut store.as_context_mut(),
            physical_before - consumed,
            FuelBoundary::HostReturn,
        )
        .await
        .unwrap();

        let snapshot = capture_fuel_snapshot(&store.as_context_mut());
        assert_eq!(snapshot.identity, inspector.identity());
        assert_eq!(snapshot.policy, FuelInspectionPolicy::Scheduled);
        assert_eq!(snapshot.state, FuelState::Running);
        assert!(snapshot.ewma_initialized);
        assert_eq!(snapshot.avg_ratio, expected_ratio);
        assert_eq!(snapshot.desired_quantum, expected_quantum);
        assert_eq!(snapshot.min_quantum, MIN_FUEL);
        assert_eq!(snapshot.max_quantum, MAX_FUEL);
        assert_eq!(snapshot.authority_grant, expected_quantum);
        assert_eq!(snapshot.total_remaining, None);
        assert_eq!(snapshot.epoch_remaining, None);
        assert_eq!(snapshot.physical_remaining, Some(store.get_fuel().unwrap()));
        assert_eq!(snapshot.yield_reserve, YIELD_RESERVE);
        let settlement = snapshot.last_settlement.unwrap();
        assert_eq!(settlement.consumed, consumed);
        assert_eq!(settlement.physical_before, physical_before);
        assert_eq!(settlement.physical_remaining, physical_before - consumed);
        let grant = snapshot.last_applied_grant.unwrap();
        assert_eq!(grant.authority, expected_quantum);
        assert_eq!(grant.physical, store.get_fuel().unwrap());
        assert_eq!(grant.physical - grant.authority, YIELD_RESERVE);
    }
}

#[tokio::test]
async fn inspector_coalesced_epochs_preserve_overshoot_and_reject_provisional_suspend() {
    use crate::fuel_inspector::FuelState;

    let (runtime, mut publisher) = crate::engine::runtime_engine().unwrap();
    let metrics = runtime.fuel_metrics();
    let mut store = fuel_metrics_store(&runtime, FuelEstimator::new_oneshot(25_000, 10_000, 0));
    let _inspector = inspector_attach_store(&mut store);
    install_fuel_slice_in_store_with_inspector::<true>(&mut store, 10_000).unwrap();
    let physical_before = store.get_fuel().unwrap();
    store.set_fuel(YIELD_RESERVE - 500).unwrap();
    for _ in 0..3 {
        let epoch = publisher.tick();
        store.data_mut().note_epoch_callback(epoch);
    }
    settle_and_apply_fuel_boundary_with_inspector::<true>(
        &mut store.as_context_mut(),
        YIELD_RESERVE - 500,
        FuelBoundary::Slice,
    )
    .await
    .unwrap();

    let snapshot = capture_fuel_snapshot(&store.as_context_mut());
    assert_eq!(snapshot.state, FuelState::Running);
    assert_eq!(snapshot.total_remaining, Some(14_500));
    assert_eq!(snapshot.epoch_remaining, Some(10_000));
    assert_eq!(snapshot.opened_epoch, 3);
    assert_eq!(snapshot.pending_epoch, None);
    assert_eq!(snapshot.published_epoch, 3);
    assert_eq!(snapshot.last_settlement.unwrap().epoch, 0);
    assert_eq!(snapshot.last_settlement.unwrap().consumed, 10_500);
    assert_eq!(physical_before - (YIELD_RESERVE - 500), 10_500);
    assert_eq!(snapshot.last_applied_grant.unwrap().opened_epoch, 3);
    assert_eq!(snapshot.last_applied_grant.unwrap().authority, 10_000);
    assert_eq!(store.get_fuel().unwrap(), YIELD_RESERVE + 10_000);
    assert_eq!(metrics.snapshot().oneshot.suspend, 0);
    assert_eq!(metrics.snapshot().oneshot_settled_units, 10_500);
    assert_eq!(
        oneshot_ledgers(&store.data().fuel_estimator),
        (14_500, 10_000)
    );
}

#[tokio::test]
async fn inspector_exhaustion_preserves_partial_grant_and_physical_overshoot() {
    use crate::fuel_inspector::FuelState;

    let (runtime, _publisher) = crate::engine::runtime_engine().unwrap();
    let mut store = fuel_metrics_store(&runtime, FuelEstimator::new_oneshot(1_337, 10_000, 0));
    let _inspector = inspector_attach_store(&mut store);
    install_fuel_slice_in_store_with_inspector::<true>(&mut store, 1_337).unwrap();
    store.set_fuel(YIELD_RESERVE - 500).unwrap();
    let error = settle_and_apply_fuel_boundary_with_inspector::<true>(
        &mut store.as_context_mut(),
        YIELD_RESERVE - 500,
        FuelBoundary::Slice,
    )
    .await
    .expect_err("full physical consumption exhausts partial authority");
    assert!(error.to_string().contains("total fuel authority exhausted"));
    let snapshot = capture_fuel_snapshot(&store.as_context_mut());
    assert_eq!(snapshot.state, FuelState::Exhausted);
    assert_eq!(snapshot.total_remaining, Some(0));
    assert_eq!(snapshot.epoch_remaining, Some(8_163));
    assert_eq!(snapshot.authority_grant, 0);
    assert_eq!(snapshot.physical_remaining, Some(store.get_fuel().unwrap()));
    assert_eq!(snapshot.last_settlement.unwrap().consumed, 1_837);
    let applied = snapshot.last_applied_grant.unwrap();
    assert_eq!(applied.authority, 1_337);
    assert_eq!(applied.desired_quantum, INITIAL_FUEL);
    assert_eq!(applied.physical, YIELD_RESERVE + 1_337);
    assert_eq!(
        runtime.fuel_metrics().snapshot().oneshot_settled_units,
        1_837
    );
}

#[tokio::test]
async fn inspector_failed_installation_does_not_fabricate_applied_grant() {
    use crate::fuel_inspector::{InspectError, MAX_INSPECTION_TIMEOUT};

    let (runtime, _publisher) = crate::engine::runtime_engine().unwrap();
    let mut config = wasmtime::Config::new();
    config.consume_fuel(false);
    let engine = wasmtime::Engine::new(&config).unwrap();
    let mut estimator = FuelEstimator::new_oneshot(25_000, 10_000, 0);
    estimator.install_authority_grant(10_000);
    let mut store = Store::new(
        &engine,
        component_test_state_with_runtime(estimator, None, &runtime),
    );
    let inspector = inspector_attach_store(&mut store);
    let mut request = Box::pin(inspector.inspect(MAX_INSPECTION_TIMEOUT));
    assert!(fuel_metrics_poll(request.as_mut()).is_pending());
    settle_and_apply_fuel_boundary_with_inspector::<true>(
        &mut store.as_context_mut(),
        YIELD_RESERVE + 9_000,
        FuelBoundary::HostReturn,
    )
    .await
    .expect_err("engine without metering rejects installation");
    assert!(fuel_metrics_poll(request.as_mut()).is_pending());
    assert!(store.get_fuel().is_err());
    let owner = store.data().fuel_inspector.as_ref().unwrap();
    assert_eq!(owner.last_applied_grant, None);
    assert_eq!(owner.last_settlement.unwrap().consumed, 1_000);
    assert_eq!(
        oneshot_ledgers(&store.data().fuel_estimator),
        (24_000, 9_000)
    );
    assert_eq!(runtime.fuel_metrics().snapshot().oneshot.grant, 0);
    drop(store);
    assert_eq!(request.await, Err(InspectError::OwnerStopped));
}

#[tokio::test]
async fn inspector_request_does_not_settle_unaccounted_physical_consumption() {
    use crate::fuel_inspector::MAX_INSPECTION_TIMEOUT;

    let (runtime, _publisher) = crate::engine::runtime_engine().unwrap();
    let mut store = fuel_metrics_store(&runtime, FuelEstimator::new_oneshot(25_000, 10_000, 0));
    let inspector = inspector_attach_store(&mut store);
    install_fuel_slice_in_store_with_inspector::<true>(&mut store, 10_000).unwrap();
    store.set_fuel(YIELD_RESERVE + 9_500).unwrap();
    let before = runtime.fuel_metrics().snapshot();
    let mut request = Box::pin(inspector.inspect(MAX_INSPECTION_TIMEOUT));
    assert!(fuel_metrics_poll(request.as_mut()).is_pending());
    service_fuel_inspection(&mut store.as_context_mut());
    let snapshot = request.await.unwrap();
    assert_eq!(snapshot.total_remaining, Some(25_000));
    assert_eq!(snapshot.epoch_remaining, Some(10_000));
    assert_eq!(
        snapshot.physical_accounting_baseline,
        YIELD_RESERVE + 10_000
    );
    assert_eq!(snapshot.physical_remaining, Some(YIELD_RESERVE + 9_500));
    assert_eq!(snapshot.last_settlement, None);
    assert_eq!(snapshot.authority_grant, 10_000);
    assert_eq!(store.get_fuel().unwrap(), YIELD_RESERVE + 9_500);
    assert_eq!(
        oneshot_ledgers(&store.data().fuel_estimator),
        (25_000, 10_000)
    );
    assert_eq!(runtime.fuel_metrics().snapshot(), before);
}

#[tokio::test]
async fn inspector_nested_frames_and_unflushed_epoch_return_do_not_create_settlements() {
    let (runtime, mut publisher) = crate::engine::runtime_engine().unwrap();
    let mut store = fuel_metrics_store(&runtime, FuelEstimator::new_oneshot(25_000, 10_000, 0));
    let _inspector = inspector_attach_store(&mut store);
    install_fuel_slice_in_store_with_inspector::<true>(&mut store, 10_000).unwrap();
    let hook = FuelCallHook::<true>;
    hook.handle_call_event(store.as_context_mut(), CallHook::CallingHost)
        .await
        .unwrap();
    store.data_mut().mark_host_call();
    hook.handle_call_event(store.as_context_mut(), CallHook::CallingHost)
        .await
        .unwrap();
    let epoch = publisher.tick();
    store.data_mut().note_epoch_callback(epoch);
    store.set_fuel(YIELD_RESERVE + 9_000).unwrap();
    hook.handle_call_event(store.as_context_mut(), CallHook::ReturningFromHost)
        .await
        .unwrap();
    let pending = capture_fuel_snapshot(&store.as_context_mut());
    assert_eq!(pending.last_settlement, None);
    assert_eq!(pending.total_remaining, Some(25_000));
    assert_eq!(pending.opened_epoch, 0);
    assert_eq!(pending.pending_epoch, Some(1));
    hook.handle_call_event(store.as_context_mut(), CallHook::ReturningFromHost)
        .await
        .unwrap();
    let settled = capture_fuel_snapshot(&store.as_context_mut());
    assert_eq!(settled.last_settlement.unwrap().consumed, 1_000);
    assert_eq!(settled.total_remaining, Some(24_000));
    assert_eq!(settled.opened_epoch, 1);
    assert!(store.data().host_call_frames.is_empty());
}

#[tokio::test]
async fn inspector_compiled_p3_explains_clamp_and_live_epoch_suspension() {
    use crate::fuel_inspector::{FuelState, InspectError, MAX_INSPECTION_TIMEOUT};

    let bytes = compiled_routing_key_probe_bytes();
    let (runtime, mut publisher) = crate::engine::runtime_engine().unwrap();
    let engine = runtime.engine();
    let component = Component::new(&engine, bytes).unwrap();
    let mut linker = Linker::new(&engine);
    add_p3_wasi_to_linker(&mut linker).unwrap();
    add_routing_key_to_linker(&mut linker).unwrap();
    let mut store = fuel_metrics_store(&runtime, FuelEstimator::new_oneshot(25_000, 10_000, 0));
    let inspector = inspector_attach_store(&mut store);
    install_epoch_fuel_callback(&mut store);
    store.set_epoch_deadline(1);
    install_fuel_slice_in_store_with_inspector::<true>(&mut store, 10_000).unwrap();
    let initial = capture_fuel_snapshot(&store.as_context_mut());
    let grant = initial.last_applied_grant.unwrap();
    assert_eq!(grant.desired_quantum, INITIAL_FUEL);
    assert_eq!(grant.authority, 10_000);
    assert_eq!(grant.epoch_remaining, Some(10_000));
    assert_eq!(grant.total_remaining, Some(25_000));
    assert_eq!(grant.physical, store.get_fuel().unwrap());
    assert_eq!(grant.physical - grant.authority, YIELD_RESERVE);
    let instance = linker
        .instantiate_async(&mut store, &component)
        .await
        .unwrap();
    let probe = instance
        .get_typed_func::<(), (String,)>(&mut store, "probe")
        .unwrap();

    let mut completed = 0;
    loop {
        let mut call = Box::pin(probe.call_async(&mut store, ()));
        match tokio::time::timeout(std::time::Duration::from_millis(10), &mut call).await {
            Ok(result) => {
                result.expect("compiled probe before epoch exhaustion");
                completed += 1;
                assert!(completed < 10_000, "compiled probe never suspends");
            }
            Err(_) => {
                let suspended = tokio::select! {
                    result = inspector.inspect(MAX_INSPECTION_TIMEOUT) => result.unwrap(),
                    result = &mut call => panic!("epoch wait ended without publication: {result:?}"),
                };
                assert_eq!(suspended.state, FuelState::Suspended);
                assert_eq!(suspended.epoch_remaining, Some(0));
                assert_eq!(suspended.authority_grant, 0);
                assert!(suspended.total_remaining.unwrap() > 0);
                assert_eq!(runtime.fuel_metrics().snapshot().oneshot.suspend, 1);
                let debit = 25_000 - suspended.total_remaining.unwrap();
                assert!(debit >= 10_000);
                publisher.tick();
                tokio::time::timeout(std::time::Duration::from_secs(1), &mut call)
                    .await
                    .expect("compiled probe resumes")
                    .expect("compiled probe result after epoch opening");
                break;
            }
        }
    }
    assert!(
        completed > 1,
        "compiled probe must exercise repeated host calls"
    );
    let resumed = capture_fuel_snapshot(&store.as_context_mut());
    assert_eq!(resumed.state, FuelState::Running);
    assert_eq!(resumed.opened_epoch, 1);
    assert_eq!(
        resumed.total_remaining,
        store.data().fuel_estimator.total_remaining
    );
    assert_eq!(resumed.physical_remaining, Some(store.get_fuel().unwrap()));
    assert!(resumed.last_applied_grant.unwrap().authority <= 10_000);
    assert_eq!(resumed.last_applied_grant.unwrap().opened_epoch, 1);
    assert!(resumed.captured_at >= initial.captured_at);
    drop(store);
    assert_eq!(
        inspector.inspect(MAX_INSPECTION_TIMEOUT).await,
        Err(InspectError::OwnerStopped)
    );
    assert_eq!(initial.last_applied_grant.unwrap().authority, 10_000);
}

#[tokio::test]
async fn inspector_zero_budget_reports_exhaustion_before_guest_start() {
    use crate::fuel_inspector::{FuelState, InspectError, MAX_INSPECTION_TIMEOUT};

    let (runtime, _publisher) = crate::engine::runtime_engine().unwrap();
    let metrics = runtime.fuel_metrics();
    let (process, inspector) = inspector_process(
        runtime,
        FuelEstimator::new_oneshot(0, 10_000, 0),
        P3_TRAPPING_START_COMMAND,
    )
    .await;
    let mut request = Box::pin(inspector.inspect(MAX_INSPECTION_TIMEOUT));
    assert!(fuel_metrics_poll(request.as_mut()).is_pending());
    let error = process
        .run()
        .await
        .expect_err("zero budget rejects start execution");
    assert!(error.to_string().contains("total fuel authority exhausted"));
    let snapshot = request.await.unwrap();
    assert_eq!(snapshot.state, FuelState::Exhausted);
    assert_eq!(snapshot.total_remaining, Some(0));
    assert_eq!(snapshot.last_settlement, None);
    assert_eq!(snapshot.last_applied_grant, None);
    assert_eq!(snapshot.physical_remaining, Some(0));
    assert_eq!(metrics.snapshot().oneshot.grant, 0);
    assert_eq!(metrics.snapshot().oneshot.exhausted, 1);
    assert_eq!(
        inspector.inspect(MAX_INSPECTION_TIMEOUT).await,
        Err(InspectError::OwnerStopped)
    );
}

#[tokio::test]
async fn inspector_cancelled_suspension_drops_owner_without_new_grant() {
    use crate::fuel_inspector::{FuelState, InspectError, MAX_INSPECTION_TIMEOUT};

    let (runtime, _publisher) = crate::engine::runtime_engine().unwrap();
    let metrics = runtime.fuel_metrics();
    let mut store = fuel_metrics_store(&runtime, FuelEstimator::new_oneshot(25_000, 10_000, 0));
    let inspector = inspector_attach_store(&mut store);
    install_fuel_slice_in_store_with_inspector::<true>(&mut store, 10_000).unwrap();
    store.set_fuel(YIELD_RESERVE).unwrap();
    {
        let mut context = store.as_context_mut();
        let mut wait = Box::pin(settle_and_apply_fuel_boundary_with_inspector::<true>(
            &mut context,
            YIELD_RESERVE,
            FuelBoundary::Slice,
        ));
        assert!(fuel_metrics_poll(wait.as_mut()).is_pending());
        let snapshot = tokio::select! {
            result = inspector.inspect(MAX_INSPECTION_TIMEOUT) => result.unwrap(),
            result = &mut wait => panic!("suspension ended before epoch: {result:?}"),
        };
        assert_eq!(snapshot.state, FuelState::Suspended);
        assert_eq!(snapshot.total_remaining, Some(15_000));
        assert_eq!(snapshot.epoch_remaining, Some(0));
        assert_eq!(snapshot.last_applied_grant.unwrap().authority, 10_000);
    }
    assert_eq!(oneshot_ledgers(&store.data().fuel_estimator), (15_000, 0));
    assert_eq!(metrics.snapshot().oneshot.grant, 1);
    assert_eq!(metrics.snapshot().oneshot_settled_units, 10_000);
    drop(store);
    assert_eq!(
        inspector.inspect(MAX_INSPECTION_TIMEOUT).await,
        Err(InspectError::OwnerStopped)
    );
}

#[tokio::test]
async fn inspector_default_and_enabled_processes_preserve_suspend_resume_and_cancellation() {
    use crate::fuel_inspector::{FuelState, InspectError, MAX_INSPECTION_TIMEOUT};

    async fn run_case(inspect: bool, cancel: bool) -> crate::metrics::FuelSnapshot {
        let (runtime, mut publisher) = crate::engine::runtime_engine().unwrap();
        let metrics = runtime.fuel_metrics();
        let (builder, _handles) = Builder::ordinary(
            Program::Bytes(wat::parse_str(P3_COUNTED_START_COMMAND).unwrap()),
            tokio::io::empty(),
            tokio::io::sink(),
            tokio::io::sink(),
            FilesystemOwnership::test(),
        );
        let mut process = builder
            .with_runtime_engine(runtime)
            .with_fuel_estimator(FuelEstimator::new_oneshot(25_000, 12_500, 0))
            .build()
            .await
            .unwrap();
        let inspector = inspect.then(|| process.fuel_inspector().unwrap());
        let mut run = Box::pin(process.run());
        tokio::time::timeout(std::time::Duration::from_secs(1), async {
            tokio::select! {
                result = &mut run => panic!("counted start completed before epoch opening: {result:?}"),
                () = async {
                    while metrics.snapshot().oneshot.suspend == 0 {
                        tokio::task::yield_now().await;
                    }
                } => {}
            }
        })
        .await
        .expect("counted start reached the authority boundary");
        let before = metrics.snapshot();
        assert_eq!(before.oneshot.grant, 1);
        assert_eq!(before.oneshot.suspend, 1);
        assert_eq!(before.oneshot.exhausted, 0);
        assert!(before.oneshot_settled_units >= 12_500);
        assert!(before.oneshot_settled_units < 25_000);
        if let Some(inspector) = &inspector {
            let snapshot = tokio::select! {
                result = inspector.inspect(MAX_INSPECTION_TIMEOUT) => result.unwrap(),
                result = &mut run => panic!("epoch wait ended without a tick: {result:?}"),
            };
            assert_eq!(snapshot.state, FuelState::Suspended);
            assert_eq!(snapshot.epoch_remaining, Some(0));
            assert_eq!(
                snapshot.total_remaining,
                Some(25_000 - before.oneshot_settled_units)
            );
            assert_eq!(metrics.snapshot(), before, "inspection changed accounting");
        }
        if cancel {
            drop(run);
            assert_eq!(
                metrics.snapshot(),
                before,
                "cancellation created a fuel decision"
            );
        } else {
            publisher.tick();
            tokio::time::timeout(std::time::Duration::from_secs(1), run)
                .await
                .expect("counted start resumed")
                .expect("guest command returned success");
            let completed = metrics.snapshot();
            assert_eq!(completed.oneshot.grant, 3);
            assert_eq!(completed.oneshot.suspend, 1);
            assert_eq!(completed.oneshot.exhausted, 0);
            assert!(completed.oneshot_settled_units > before.oneshot_settled_units);
            assert!(completed.oneshot_settled_units < 25_000);
        }
        if let Some(inspector) = inspector {
            assert_eq!(
                inspector.inspect(MAX_INSPECTION_TIMEOUT).await,
                Err(InspectError::OwnerStopped)
            );
        }
        metrics.snapshot()
    }

    for cancel in [false, true] {
        let baseline = run_case(false, cancel).await;
        let inspected = run_case(true, cancel).await;
        assert_eq!(
            inspected, baseline,
            "inspection changed the guest's fuel outcomes"
        );
    }
}

#[tokio::test]
async fn inspector_independent_requester_times_out_without_owner_progress() {
    use crate::fuel_inspector::{InspectError, MAX_INSPECTION_TIMEOUT};

    let (runtime, _publisher) = crate::engine::runtime_engine().unwrap();
    let mut store = fuel_metrics_store(&runtime, FuelEstimator::new_oneshot(25_000, 10_000, 0));
    let inspector = inspector_attach_store(&mut store);
    install_fuel_slice_in_store_with_inspector::<true>(&mut store, 10_000).unwrap();
    let before_fuel = store.get_fuel().unwrap();
    let before_metrics = runtime.fuel_metrics().snapshot();
    let remote = inspector.clone();
    let result = std::thread::spawn(move || {
        tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .build()
            .unwrap()
            .block_on(remote.inspect(std::time::Duration::from_millis(20)))
    })
    .join()
    .expect("independent request runtime");
    assert_eq!(result, Err(InspectError::Timeout));
    assert_eq!(store.get_fuel().unwrap(), before_fuel);
    assert_eq!(
        oneshot_ledgers(&store.data().fuel_estimator),
        (25_000, 10_000)
    );
    assert_eq!(runtime.fuel_metrics().snapshot(), before_metrics);
    assert_eq!(
        inspector.inspect(MAX_INSPECTION_TIMEOUT).await,
        Err(InspectError::Busy)
    );
    service_fuel_inspection(&mut store.as_context_mut());
    assert_eq!(runtime.fuel_metrics().snapshot(), before_metrics);
    assert_eq!(store.get_fuel().unwrap(), before_fuel);
    drop(store);
    assert_eq!(
        inspector.inspect(MAX_INSPECTION_TIMEOUT).await,
        Err(InspectError::OwnerStopped)
    );
}

#[tokio::test]
async fn inspector_host_epoch_publication_does_not_claim_an_opened_allowance() {
    use crate::fuel_inspector::MAX_INSPECTION_TIMEOUT;

    let (runtime, mut publisher) = crate::engine::runtime_engine().unwrap();
    let mut store = fuel_metrics_store(&runtime, FuelEstimator::new_oneshot(25_000, 10_000, 0));
    let inspector = inspector_attach_store(&mut store);
    install_fuel_slice_in_store_with_inspector::<true>(&mut store, 10_000).unwrap();
    // A real earlier settlement already spent part of epoch zero's allowance.
    store.set_fuel(YIELD_RESERVE + 6_000).unwrap();
    settle_and_apply_fuel_boundary_with_inspector::<true>(
        &mut store.as_context_mut(),
        YIELD_RESERVE + 6_000,
        FuelBoundary::HostReturn,
    )
    .await
    .unwrap();
    let before = store.get_fuel().unwrap();
    assert_eq!(publisher.tick(), 1);
    assert_eq!(publisher.tick(), 2);
    let mut request = Box::pin(inspector.inspect(MAX_INSPECTION_TIMEOUT));
    assert!(fuel_metrics_poll(request.as_mut()).is_pending());
    service_fuel_inspection(&mut store.as_context_mut());
    let snapshot = request.await.unwrap();
    assert_eq!(snapshot.published_epoch, 2);
    assert_eq!(snapshot.opened_epoch, 0);
    assert_eq!(snapshot.pending_epoch, None);
    assert_eq!(snapshot.total_remaining, Some(21_000));
    assert_eq!(snapshot.epoch_remaining, Some(6_000));
    assert_eq!(snapshot.last_applied_grant.unwrap().opened_epoch, 0);
    assert_eq!(store.get_fuel().unwrap(), before);
    assert_eq!(
        oneshot_ledgers(&store.data().fuel_estimator),
        (21_000, 6_000)
    );
}
