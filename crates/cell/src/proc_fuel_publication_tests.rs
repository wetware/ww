// Included inside proc::tests. These tests execute the production P3 linker,
// settlement hooks, and consuming Proc::run ownership path.

const P3_PENDING_PUBLICATION_COMMAND: &str =
    include_str!("../../../tests/fixtures/fuel-publication-component.wat");

#[tokio::test]
async fn fuel_publication_store_moves_keep_one_pending_owner_until_drop() {
    let (runtime, _publisher) = crate::engine::runtime_engine().unwrap();
    let metrics = runtime.fuel_metrics();
    let mut store = fuel_metrics_store(&runtime, FuelEstimator::new(INITIAL_FUEL));
    install_fuel_slice_in_store(&mut store, INITIAL_FUEL).unwrap();
    let remaining = store.get_fuel().unwrap() - 37;
    store.set_fuel(remaining).unwrap();
    settle_and_apply_fuel_boundary(
        &mut store.as_context_mut(),
        remaining,
        FuelBoundary::HostReturn,
    )
    .await
    .unwrap();
    assert_eq!(metrics.snapshot().scheduled_settled_units, 0);

    let store = std::thread::spawn(move || store).join().unwrap();
    assert_eq!(metrics.snapshot().scheduled_settled_units, 0);
    std::thread::spawn(move || drop(store)).join().unwrap();
    assert_eq!(metrics.snapshot().scheduled_settled_units, 37);
    assert_eq!(metrics.snapshot().oneshot_settled_units, 0);
    drop(runtime);
    assert_eq!(metrics.snapshot().scheduled_settled_units, 37);
}

#[tokio::test]
async fn fuel_publication_pending_p3_store_flushes_only_on_owner_cancellation() {
    for oneshot in [false, true] {
        let (runtime, _publisher) = crate::engine::runtime_engine().unwrap();
        let metrics = runtime.fuel_metrics();
        let observer = FuelObserver::default();
        let estimator = if oneshot {
            FuelEstimator::new_oneshot(100_000, 100_000, 0)
        } else {
            FuelEstimator::new(INITIAL_FUEL)
        };
        let proc = fuel_metrics_process(
            runtime,
            estimator,
            P3_PENDING_PUBLICATION_COMMAND,
            observer.clone(),
        )
        .await;
        let mut run = Box::pin(proc.run());
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            tokio::select! {
                result = &mut run => panic!("pending P3 command returned: {result:?}"),
                () = async {
                    while observer.updates().len() < 8 {
                        tokio::task::yield_now().await;
                    }
                } => {}
            }
        })
        .await
        .expect("eight marked returns before the pending P3 wait");

        let updates = observer.updates();
        assert_eq!(updates.len(), 8);
        assert_eq!(
            updates.iter().map(|update| update.consumed).sum::<u64>(),
            46
        );
        let active = metrics.snapshot();
        assert_eq!(active.scheduled_settled_units, 0);
        assert_eq!(active.oneshot_settled_units, 0);
        assert_eq!(
            if oneshot {
                active.oneshot.grant
            } else {
                active.scheduled.grant
            },
            9
        );

        // This future owns Proc and its Store. Dropping an arbitrary borrowed
        // guest-call future would not establish the same destruction boundary.
        drop(run);
        let terminal = metrics.snapshot();
        assert_eq!(
            terminal.scheduled_settled_units,
            if oneshot { 0 } else { 46 }
        );
        assert_eq!(terminal.oneshot_settled_units, if oneshot { 46 } else { 0 });
        assert_eq!(terminal.scheduled, active.scheduled);
        assert_eq!(terminal.oneshot, active.oneshot);
    }
}

#[tokio::test]
async fn fuel_publication_successful_p3_completion_publishes_partial_batch_once() {
    let command = P3_PENDING_PUBLICATION_COMMAND.replace("i64.const 60000000000 call $wait", "");
    let (runtime, _publisher) = crate::engine::runtime_engine().unwrap();
    let metrics = runtime.fuel_metrics();
    let observer = FuelObserver::default();
    let proc = fuel_metrics_process(
        runtime,
        FuelEstimator::new(INITIAL_FUEL),
        &command,
        observer.clone(),
    )
    .await;
    tokio::time::timeout(std::time::Duration::from_secs(5), proc.run())
        .await
        .expect("finite P3 command deadline")
        .expect("finite P3 command");
    let updates = observer.updates();
    assert_eq!(updates.len(), 8);
    assert_eq!(
        updates.iter().map(|update| update.consumed).sum::<u64>(),
        46
    );
    let terminal = metrics.snapshot();
    assert_eq!(terminal.scheduled_settled_units, 46);
    assert_eq!(terminal.oneshot_settled_units, 0);
    assert_eq!(terminal.scheduled.grant, 9);
}

#[tokio::test]
async fn fuel_publication_failed_p3_instantiation_flushes_committed_settlements() {
    // Running the loop as a start function accounts for two additional units
    // at its first host return. Pin both entry paths before comparing Drop's
    // published total with the independently recorded authoritative updates.
    for (start, expected) in [(false, 46), (true, 48)] {
        let mut command = P3_PENDING_PUBLICATION_COMMAND
            .replace("i64.const 60000000000 call $wait", "unreachable");
        if start {
            command = command.replace(";; START_HOOK", "(start $run)");
        }
        let (runtime, _publisher) = crate::engine::runtime_engine().unwrap();
        let metrics = runtime.fuel_metrics();
        let observer = FuelObserver::default();
        let proc = fuel_metrics_process(
            runtime,
            FuelEstimator::new(INITIAL_FUEL),
            &command,
            observer.clone(),
        )
        .await;
        let error = tokio::time::timeout(std::time::Duration::from_secs(5), proc.run())
            .await
            .expect("trapping P3 command deadline")
            .expect_err("guest must trap after the marked returns");
        assert!(format!("{error:#}").contains("unreachable"));
        let updates = observer.updates();
        assert_eq!(updates.len(), 8);
        assert_eq!(updates[0].consumed, if start { 6 } else { 4 });
        assert!(updates[1..].iter().all(|update| update.consumed == 6));
        assert_eq!(
            updates.iter().map(|update| update.consumed).sum::<u64>(),
            expected
        );
        let terminal = metrics.snapshot();
        assert_eq!(terminal.scheduled_settled_units, expected);
        assert_eq!(terminal.oneshot_settled_units, 0);
        assert_eq!(terminal.scheduled.grant, 9);
    }
}
