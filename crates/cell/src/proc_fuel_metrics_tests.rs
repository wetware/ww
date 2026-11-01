// Included inside proc::tests so these tests exercise the private production
// application paths without introducing test hooks into the scheduler.

fn fuel_metrics_poll<F: Future>(future: Pin<&mut F>) -> Poll<F::Output> {
    future.poll(&mut Context::from_waker(std::task::Waker::noop()))
}

fn fuel_metrics_store(
    runtime: &crate::engine::RuntimeEngine,
    estimator: FuelEstimator,
) -> Store<ComponentRunStates> {
    Store::new(
        &runtime.engine(),
        component_test_state_with_runtime(estimator, None, runtime),
    )
}

async fn fuel_metrics_process(
    runtime: crate::engine::RuntimeEngine,
    estimator: FuelEstimator,
    wat: &str,
    observer: FuelObserver,
) -> Proc {
    let (builder, _handles) = Builder::ordinary(
        Program::Bytes(wat::parse_str(wat).expect("fuel metrics component")),
        tokio::io::empty(),
        tokio::io::sink(),
        tokio::io::sink(),
        FilesystemOwnership::test(),
    );
    builder
        .with_runtime_engine(runtime)
        .with_fuel_estimator(estimator)
        .with_fuel_observer(observer)
        .build()
        .await
        .expect("prepare fuel metrics process")
}

#[tokio::test]
async fn fuel_metrics_initial_scheduled_grant_precedes_start_trap() {
    let (runtime, _publisher) = crate::engine::runtime_engine().unwrap();
    let metrics = runtime.fuel_metrics();
    let proc = fuel_metrics_process(
        runtime,
        FuelEstimator::new(INITIAL_FUEL),
        P3_TRAPPING_START_COMMAND,
        FuelObserver::default(),
    )
    .await;
    assert_eq!(metrics.snapshot().scheduled.grant, 0);
    let error = proc.run().await.expect_err("fixture start must trap");
    assert!(format!("{error:#}").contains("unreachable"));
    let snapshot = metrics.snapshot();
    assert_eq!(snapshot.scheduled.grant, 1);
    assert_eq!(snapshot.scheduled.suspend, 0);
    assert_eq!(snapshot.scheduled.exhausted, 0);
    assert_eq!(snapshot.oneshot.grant, 0);
}

#[tokio::test]
async fn fuel_metrics_initial_oneshot_grants_are_clamped_before_instantiation() {
    for (total, epoch, expected) in [(1_337, 10_000, 1_337), (25_000, 12_500, 12_500)] {
        let (runtime, _publisher) = crate::engine::runtime_engine().unwrap();
        let metrics = runtime.fuel_metrics();
        let proc = fuel_metrics_process(
            runtime,
            FuelEstimator::new_oneshot(total, epoch, 0),
            P3_TRAPPING_START_COMMAND,
            FuelObserver::default(),
        )
        .await;
        assert_eq!(proc.store.data().fuel_estimator.quantum, INITIAL_FUEL);
        assert_eq!(
            proc.store.data().fuel_estimator.current_decision(),
            FuelDecision::Grant(expected)
        );
        assert_eq!(proc.store.get_fuel().unwrap(), 0);
        assert_eq!(metrics.snapshot().oneshot.grant, 0);
        let error = proc.run().await.expect_err("fixture start must trap");
        assert!(format!("{error:#}").contains("unreachable"));
        let snapshot = metrics.snapshot();
        assert_eq!(snapshot.oneshot.grant, 1);
        assert_eq!(snapshot.oneshot.suspend, 0);
        assert_eq!(snapshot.oneshot.exhausted, 0);
        assert_eq!(snapshot.scheduled.grant, 0);
    }
}

#[tokio::test]
async fn fuel_metrics_zero_total_commits_exhaustion_before_start() {
    let (runtime, _publisher) = crate::engine::runtime_engine().unwrap();
    let metrics = runtime.fuel_metrics();
    let proc = fuel_metrics_process(
        runtime,
        FuelEstimator::new_oneshot(0, 10_000, 0),
        P3_TRAPPING_START_COMMAND,
        FuelObserver::default(),
    )
    .await;
    assert_eq!(metrics.snapshot().oneshot.exhausted, 0);
    let error = proc.run().await.expect_err("zero authority must terminate");
    let message = format!("{error:#}");
    assert!(message.contains("one-shot total fuel authority exhausted"));
    assert!(!message.contains("unreachable"));
    let snapshot = metrics.snapshot();
    assert_eq!(snapshot.oneshot.grant, 0);
    assert_eq!(snapshot.oneshot.suspend, 0);
    assert_eq!(snapshot.oneshot.exhausted, 1);
}

#[tokio::test]
async fn fuel_metrics_installation_separates_quantum_authority_and_reserve() {
    let (runtime, _publisher) = crate::engine::runtime_engine().unwrap();
    let metrics = runtime.fuel_metrics();
    let mut store = fuel_metrics_store(&runtime, FuelEstimator::new_oneshot(1_337, 10_000, 0));
    install_fuel_slice_in_store(&mut store, 1_337).unwrap();
    assert_eq!(metrics.snapshot().oneshot.grant, 1);
    let estimator = &store.data().fuel_estimator;
    assert_eq!(estimator.quantum, INITIAL_FUEL);
    assert_eq!(estimator.authority_grant, 1_337);
    assert_eq!(estimator.physical_installed, 1_337 + YIELD_RESERVE);
    assert_eq!(store.get_fuel().unwrap(), 1_337 + YIELD_RESERVE);
    assert_eq!(estimator.total_remaining, Some(1_337));
    assert_eq!(estimator.epoch_remaining, Some(10_000));
    drop(store);
    drop(runtime);
    assert_eq!(
        metrics.snapshot().oneshot.grant,
        1,
        "Store drop erased a grant"
    );
}

#[tokio::test]
async fn fuel_metrics_repeated_wait_polls_count_one_suspension_and_one_resumed_grant() {
    let (runtime, mut publisher) = crate::engine::runtime_engine().unwrap();
    let metrics = runtime.fuel_metrics();
    let mut store = fuel_metrics_store(&runtime, FuelEstimator::new_oneshot(25_000, 10_000, 0));
    install_fuel_slice_in_store(&mut store, 10_000).unwrap();
    store.set_fuel(YIELD_RESERVE).unwrap();
    {
        let mut context = store.as_context_mut();
        let mut apply = std::pin::pin!(settle_and_apply_fuel_boundary(
            &mut context,
            YIELD_RESERVE,
            FuelBoundary::Slice,
        ));
        for _ in 0..4 {
            assert!(fuel_metrics_poll(apply.as_mut()).is_pending());
            let snapshot = metrics.snapshot();
            assert_eq!(snapshot.oneshot.grant, 1);
            assert_eq!(snapshot.oneshot.suspend, 1);
            assert_eq!(snapshot.oneshot.exhausted, 0);
        }
        // Watch notifications coalesce; three notifications restore one allowance.
        publisher.tick();
        publisher.tick();
        publisher.tick();
        match fuel_metrics_poll(apply.as_mut()) {
            Poll::Ready(Ok(())) => {}
            result => panic!("epoch publication did not resume the grant: {result:?}"),
        }
    }
    assert_eq!(store.data().opened_epoch, 3);
    assert_eq!(store.data().fuel_estimator.total_remaining, Some(15_000));
    assert_eq!(store.data().fuel_estimator.epoch_remaining, Some(10_000));
    let snapshot = metrics.snapshot();
    assert_eq!(snapshot.oneshot.grant, 2);
    assert_eq!(snapshot.oneshot.suspend, 1);
    assert_eq!(snapshot.oneshot.exhausted, 0);
}

#[tokio::test]
async fn fuel_metrics_epoch_opening_supersedes_settlement_suspend() {
    let (runtime, mut publisher) = crate::engine::runtime_engine().unwrap();
    let metrics = runtime.fuel_metrics();
    let observer = FuelObserver::default();
    let mut store = fuel_metrics_store(&runtime, FuelEstimator::new_oneshot(25_000, 10_000, 0));
    store.data_mut().fuel_observer = Some(observer.clone());
    install_fuel_slice_in_store(&mut store, 10_000).unwrap();
    store.set_fuel(YIELD_RESERVE).unwrap();
    publisher.tick();
    publisher.tick();
    publisher.tick();
    // Settlement exhausts the old epoch, but opening epoch 3 replaces Suspend.
    settle_and_apply_fuel_boundary(
        &mut store.as_context_mut(),
        YIELD_RESERVE,
        FuelBoundary::Slice,
    )
    .await
    .unwrap();
    assert_eq!(store.data().opened_epoch, 3);
    assert_eq!(observer.observations().len(), 1);
    assert_eq!(observer.updates().len(), 1);
    assert_eq!(observer.updates()[0].decision, FuelDecision::Grant(10_000));
    let snapshot = metrics.snapshot();
    assert_eq!(snapshot.oneshot.grant, 2);
    assert_eq!(snapshot.oneshot.suspend, 0);
    assert_eq!(snapshot.oneshot.exhausted, 0);
}

#[tokio::test]
async fn fuel_metrics_pending_epoch_supersedes_suspend_in_apply_loop() {
    let (runtime, mut publisher) = crate::engine::runtime_engine().unwrap();
    let metrics = runtime.fuel_metrics();
    let mut store = fuel_metrics_store(&runtime, FuelEstimator::new_oneshot(25_000, 10_000, 0));
    install_fuel_slice_in_store(&mut store, 10_000).unwrap();
    store.set_fuel(YIELD_RESERVE).unwrap();
    let decision = store
        .data_mut()
        .fuel_estimator
        .settle_and_decide(YIELD_RESERVE, FuelBoundary::Slice)
        .decision;
    assert_eq!(decision, FuelDecision::Suspend);
    // Models a tick between the settlement check and decision application.
    publisher.tick();
    apply_fuel_decision(&mut store.as_context_mut(), decision)
        .await
        .unwrap();
    let snapshot = metrics.snapshot();
    assert_eq!(snapshot.oneshot.grant, 2);
    assert_eq!(snapshot.oneshot.suspend, 0);
    assert_eq!(snapshot.oneshot.exhausted, 0);
}

#[tokio::test]
async fn fuel_metrics_epoch_opening_installs_only_the_final_proposed_grant() {
    let (runtime, mut publisher) = crate::engine::runtime_engine().unwrap();
    let metrics = runtime.fuel_metrics();
    let observer = FuelObserver::default();
    let mut store = fuel_metrics_store(&runtime, FuelEstimator::new_oneshot(50_000, 10_000, 0));
    store.data_mut().fuel_observer = Some(observer.clone());
    install_fuel_slice_in_store(&mut store, 10_000).unwrap();
    // Match the existing mid-chunk regression's tightened next-epoch authority.
    // Old-epoch settlement proposes 9,000; opening the epoch replaces it with 1,337.
    store.data_mut().fuel_estimator.epoch_limit = Some(1_337);
    store.set_fuel(YIELD_RESERVE + 9_000).unwrap();
    publisher.tick();
    settle_and_apply_fuel_boundary(
        &mut store.as_context_mut(),
        YIELD_RESERVE + 9_000,
        FuelBoundary::HostReturn,
    )
    .await
    .unwrap();
    let estimator = &store.data().fuel_estimator;
    assert_eq!(estimator.total_remaining, Some(49_000));
    assert_eq!(estimator.epoch_remaining, Some(1_337));
    assert_eq!(estimator.authority_grant, 1_337);
    assert_eq!(store.get_fuel().unwrap(), YIELD_RESERVE + 1_337);
    assert_eq!(observer.updates().len(), 1);
    assert_eq!(observer.updates()[0].decision, FuelDecision::Grant(1_337));
    let snapshot = metrics.snapshot();
    assert_eq!(snapshot.oneshot.grant, 2);
    assert_eq!(snapshot.oneshot.suspend, 0);
    assert_eq!(snapshot.oneshot.exhausted, 0);
}

#[tokio::test]
async fn fuel_metrics_stale_ready_notification_and_closed_clock_do_not_suspend() {
    let (runtime, mut publisher) = crate::engine::runtime_engine().unwrap();
    let metrics = runtime.fuel_metrics();
    let mut store = fuel_metrics_store(&runtime, FuelEstimator::new_oneshot(25_000, 10_000, 0));
    publisher.tick();
    assert_eq!(store.data_mut().open_latest_epoch(), Some(1));
    // open_latest_epoch reads the publication without marking its watch version seen.
    // changed() first returns the stale publication immediately, then the closure.
    drop(publisher);
    install_fuel_slice_in_store(&mut store, 10_000).unwrap();
    store.set_fuel(YIELD_RESERVE).unwrap();
    let error = settle_and_apply_fuel_boundary(
        &mut store.as_context_mut(),
        YIELD_RESERVE,
        FuelBoundary::Slice,
    )
    .await
    .expect_err("closed clock must terminate the Cell");
    assert!(error
        .to_string()
        .contains("epoch clock closed while one-shot Cell was suspended"));
    let snapshot = metrics.snapshot();
    assert_eq!(snapshot.oneshot.grant, 1);
    assert_eq!(snapshot.oneshot.suspend, 0);
    assert_eq!(snapshot.oneshot.exhausted, 0);
}

#[tokio::test]
async fn fuel_metrics_stale_ready_notification_then_pending_counts_only_once() {
    let (runtime, mut publisher) = crate::engine::runtime_engine().unwrap();
    let metrics = runtime.fuel_metrics();
    let mut store = fuel_metrics_store(&runtime, FuelEstimator::new_oneshot(25_000, 10_000, 0));
    publisher.tick();
    assert_eq!(store.data_mut().open_latest_epoch(), Some(1));
    install_fuel_slice_in_store(&mut store, 10_000).unwrap();
    store.set_fuel(YIELD_RESERVE).unwrap();
    {
        let mut context = store.as_context_mut();
        let mut apply = std::pin::pin!(settle_and_apply_fuel_boundary(
            &mut context,
            YIELD_RESERVE,
            FuelBoundary::Slice,
        ));
        assert!(fuel_metrics_poll(apply.as_mut()).is_pending());
        assert!(fuel_metrics_poll(apply.as_mut()).is_pending());
        assert_eq!(metrics.snapshot().oneshot.suspend, 1);
        // Closing an already suspended clock retains the suspension and its error.
        drop(publisher);
        let Poll::Ready(Err(error)) = fuel_metrics_poll(apply.as_mut()) else {
            panic!("closed suspended clock did not return its error");
        };
        assert!(error
            .to_string()
            .contains("epoch clock closed while one-shot Cell was suspended"));
    }
    let snapshot = metrics.snapshot();
    assert_eq!(snapshot.oneshot.grant, 1);
    assert_eq!(snapshot.oneshot.suspend, 1);
    assert_eq!(snapshot.oneshot.exhausted, 0);
}

#[tokio::test]
async fn fuel_metrics_cooperative_budget_yield_is_not_an_epoch_suspension() {
    // A spawned task receives a finite cooperative budget. Consume it without
    // yielding, so watch::changed must return Pending before checking authority.
    tokio::spawn(async {
        let (runtime, mut publisher) = crate::engine::runtime_engine().unwrap();
        let metrics = runtime.fuel_metrics();
        let mut store = fuel_metrics_store(&runtime, FuelEstimator::new_oneshot(25_000, 10_000, 0));
        install_fuel_slice_in_store(&mut store, 10_000).unwrap();
        store.set_fuel(YIELD_RESERVE).unwrap();
        for _ in 0..1_024 {
            if !tokio::task::coop::has_budget_remaining() {
                break;
            }
            tokio::task::coop::consume_budget().await;
        }
        assert!(
            !tokio::task::coop::has_budget_remaining(),
            "fixture did not exhaust the task cooperative budget"
        );
        {
            let mut context = store.as_context_mut();
            let mut apply = std::pin::pin!(settle_and_apply_fuel_boundary(
                &mut context,
                YIELD_RESERVE,
                FuelBoundary::Slice,
            ));
            assert!(fuel_metrics_poll(apply.as_mut()).is_pending());
            assert_eq!(metrics.snapshot().oneshot.suspend, 0);
            publisher.tick();
            // The next task poll gets a fresh budget and sees the published
            // epoch immediately, so this decision never waits for authority.
            tokio::task::yield_now().await;
            apply.await.unwrap();
        }
        let snapshot = metrics.snapshot();
        assert_eq!(snapshot.oneshot.grant, 2);
        assert_eq!(snapshot.oneshot.suspend, 0);
        assert_eq!(snapshot.oneshot.exhausted, 0);
    })
    .await
    .expect("cooperative budget fixture panicked");
}

#[tokio::test]
async fn fuel_metrics_hook_rearms_and_ignored_returns_do_not_grant() {
    let (runtime, _publisher) = crate::engine::runtime_engine().unwrap();
    let metrics = runtime.fuel_metrics();
    let mut store = fuel_metrics_store(&runtime, FuelEstimator::new(25_000));
    install_fuel_slice_in_store(&mut store, 25_000).unwrap();
    store.set_fuel(YIELD_RESERVE + 15_000).unwrap();
    // The first callback rearms an intermediate yield; the repeated callback
    // no longer meets the rearmed boundary. Neither installs new authority.
    for _ in 0..3 {
        FuelCallHook::<false>
            .handle_call_event(store.as_context_mut(), CallHook::ReturningFromHost)
            .await
            .unwrap();
        assert_eq!(metrics.snapshot().scheduled.grant, 1);
    }
    store.set_fuel(YIELD_RESERVE).unwrap();
    FuelCallHook::<false>
        .handle_call_event(store.as_context_mut(), CallHook::ReturningFromHost)
        .await
        .unwrap();
    assert_eq!(metrics.snapshot().scheduled.grant, 2);
    for _ in 0..3 {
        FuelCallHook::<false>
            .handle_call_event(store.as_context_mut(), CallHook::ReturningFromHost)
            .await
            .unwrap();
    }
    assert_eq!(metrics.snapshot().scheduled.grant, 2);
    assert_eq!(store.data().fuel_estimator.host_calls_this_epoch, 0);
}

#[tokio::test]
async fn fuel_metrics_each_marked_host_return_installs_one_grant() {
    let (runtime, _publisher) = crate::engine::runtime_engine().unwrap();
    let metrics = runtime.fuel_metrics();
    let mut store = fuel_metrics_store(&runtime, FuelEstimator::new(INITIAL_FUEL));
    install_fuel_slice_in_store(&mut store, INITIAL_FUEL).unwrap();
    for count in 1..=4 {
        FuelCallHook::<false>
            .handle_call_event(store.as_context_mut(), CallHook::CallingHost)
            .await
            .unwrap();
        store.data_mut().mark_host_call();
        FuelCallHook::<false>
            .handle_call_event(store.as_context_mut(), CallHook::ReturningFromHost)
            .await
            .unwrap();
        assert_eq!(metrics.snapshot().scheduled.grant, count + 1);
    }
    assert_eq!(store.data().fuel_estimator.host_calls_this_epoch, 4);
    assert_eq!(metrics.snapshot().scheduled.suspend, 0);
    assert_eq!(metrics.snapshot().scheduled.exhausted, 0);
}

#[tokio::test]
async fn fuel_metrics_failed_installations_preserve_estimator_order_without_grants() {
    let (runtime, _publisher) = crate::engine::runtime_engine().unwrap();
    let metrics = runtime.fuel_metrics();
    let mut config = wasmtime::Config::new();
    config.consume_fuel(false);
    let engine = wasmtime::Engine::new(&config).unwrap();
    for context_installer in [false, true] {
        let mut store = Store::new(
            &engine,
            component_test_state_with_runtime(
                FuelEstimator::new_oneshot(1_337, 10_000, 0),
                None,
                &runtime,
            ),
        );
        let error = if context_installer {
            install_fuel_slice_in_context(&mut store.as_context_mut(), 1_337)
        } else {
            install_fuel_slice_in_store(&mut store, 1_337)
        }
        .expect_err("disabled Wasmtime fuel must reject installation");
        assert!(error.to_string().contains("fuel is not configured"));
        let estimator = &store.data().fuel_estimator;
        assert_eq!(estimator.authority_grant, 1_337);
        assert_eq!(estimator.physical_installed, 1_337 + YIELD_RESERVE);
        assert_eq!(estimator.quantum, INITIAL_FUEL);
        assert_eq!(estimator.total_remaining, Some(1_337));
        assert_eq!(estimator.epoch_remaining, Some(10_000));
        assert_eq!(metrics.snapshot().oneshot.grant, 0);
    }
}

#[tokio::test]
async fn fuel_metrics_real_p3_suspends_resumes_and_exhausts() {
    let (runtime, mut publisher) = crate::engine::runtime_engine().unwrap();
    let metrics = runtime.fuel_metrics();
    let observer = FuelObserver::default();
    let proc = fuel_metrics_process(
        runtime,
        FuelEstimator::new_oneshot(25_000, 10_000, 0),
        include_str!("../../../tests/fixtures/spinning-component.wat"),
        observer.clone(),
    )
    .await;
    let mut run = Box::pin(proc.run());
    for occurrence in 1..=2 {
        let mut suspended = false;
        for _ in 0..100 {
            assert!(fuel_metrics_poll(run.as_mut()).is_pending());
            if observer.suspension_count() == occurrence {
                suspended = true;
                break;
            }
        }
        assert!(
            suspended,
            "real P3 process did not reach suspension {occurrence}"
        );
        let snapshot = metrics.snapshot();
        assert_eq!(snapshot.oneshot.grant, occurrence as u64);
        assert_eq!(snapshot.oneshot.suspend, occurrence as u64);
        assert_eq!(snapshot.oneshot.exhausted, 0);
        publisher.tick();
    }
    let error = tokio::time::timeout(std::time::Duration::from_secs(5), run)
        .await
        .expect("real P3 process did not terminate")
        .expect_err("total authority must exhaust");
    assert!(format!("{error:#}").contains("one-shot total fuel authority exhausted"));
    let snapshot = metrics.snapshot();
    assert_eq!(snapshot.oneshot.grant, 3);
    assert_eq!(snapshot.oneshot.suspend, 2);
    assert_eq!(snapshot.oneshot.exhausted, 1);
}

#[tokio::test]
async fn fuel_metrics_cancelling_suspended_p3_does_not_invent_resumption() {
    let (runtime, mut publisher) = crate::engine::runtime_engine().unwrap();
    let metrics = runtime.fuel_metrics();
    let observer = FuelObserver::default();
    let proc = fuel_metrics_process(
        runtime,
        FuelEstimator::new_oneshot(25_000, 10_000, 0),
        include_str!("../../../tests/fixtures/spinning-component.wat"),
        observer.clone(),
    )
    .await;
    let mut run = Box::pin(proc.run());
    let mut suspended = false;
    for _ in 0..100 {
        assert!(fuel_metrics_poll(run.as_mut()).is_pending());
        if observer.suspension_count() == 1 {
            suspended = true;
            break;
        }
    }
    assert!(suspended, "real P3 process did not suspend");
    drop(run);
    publisher.tick();
    let snapshot = metrics.snapshot();
    assert_eq!(snapshot.oneshot.grant, 1);
    assert_eq!(snapshot.oneshot.suspend, 1);
    assert_eq!(snapshot.oneshot.exhausted, 0);
}
