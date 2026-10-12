use std::sync::Arc;

use ww_cache::{
    AdmissionKind, CellLimits, CellResources, ForegroundTransaction, HostFsOwner, HostLimits,
    HostResources,
};

fn limits(logical: usize, backend: usize, roots: usize) -> HostLimits {
    HostLimits::new(logical, backend, roots).unwrap()
}

fn start_host(limits: HostLimits) -> (HostFsOwner, Arc<HostResources>) {
    HostFsOwner::start(tokio::runtime::Handle::current(), limits).unwrap()
}

#[test]
fn limits_reject_zero_and_backend_totals_without_a_cleanup_lane() {
    assert!(HostLimits::new(0, 2, 1).is_err());
    assert!(HostLimits::new(1, 0, 1).is_err());
    assert!(HostLimits::new(1, 1, 1).is_err());
    assert!(HostLimits::new(1, 2, 0).is_err());
    assert!(CellLimits::new(0).is_err());
}

#[tokio::test]
async fn cell_then_host_logical_admission_is_immediate_and_rolls_back_partials() {
    let (owner, host) = start_host(limits(1, 2, 1));
    let first = CellResources::new(Arc::clone(&host), CellLimits::new(1).unwrap());
    let second = CellResources::new(Arc::clone(&host), CellLimits::new(1).unwrap());

    let held = first.try_logical_io().unwrap();
    let rejected = second.try_logical_io().unwrap_err();
    assert_eq!(rejected.kind(), AdmissionKind::HostLogical);

    // The failed Host reservation must return the second Cell's local debit.
    drop(held);
    let second_first = second.try_logical_io().unwrap();
    drop(second_first);
    let second_again = second.try_logical_io().unwrap();
    drop(second_again);

    owner.shutdown().await.unwrap();
}

#[tokio::test]
async fn two_cells_share_one_host_logical_ceiling() {
    let (owner, host) = start_host(limits(2, 2, 1));
    let first = CellResources::new(Arc::clone(&host), CellLimits::new(1).unwrap());
    let second = CellResources::new(Arc::clone(&host), CellLimits::new(1).unwrap());

    let first_held = first.try_logical_io().unwrap();
    let second_held = second.try_logical_io().unwrap();
    assert_eq!(
        first.try_logical_io().unwrap_err().kind(),
        AdmissionKind::CellLogical
    );
    assert_eq!(
        CellResources::new(Arc::clone(&host), CellLimits::new(1).unwrap())
            .try_logical_io()
            .unwrap_err()
            .kind(),
        AdmissionKind::HostLogical
    );

    drop((first_held, second_held));
    owner.shutdown().await.unwrap();
}

#[tokio::test]
async fn backend_total_reserves_exactly_one_cleanup_lane() {
    let (owner, host) = start_host(limits(4, 2, 1));

    let foreground = host.try_foreground().unwrap();
    assert_eq!(
        host.try_foreground().unwrap_err().kind(),
        AdmissionKind::ForegroundProducer
    );
    drop(foreground);
    assert!(host.try_foreground().is_ok());

    owner.shutdown().await.unwrap();
}

#[tokio::test]
async fn cleanup_obligations_are_bounded_before_a_root_exists() {
    let (owner, host) = start_host(limits(4, 2, 1));
    let (_scope, root_owner, finalizer, completion) = host.try_filesystem_scope().unwrap();

    let error = match host.try_filesystem_scope() {
        Ok(_) => panic!("K+1 root unexpectedly admitted"),
        Err(error) => error,
    };
    assert_eq!(error.kind(), AdmissionKind::CleanupObligation);

    drop(root_owner);
    assert_eq!(finalizer.run().await, Ok(()));
    assert_eq!(completion.wait().await, Ok(()));
    owner.shutdown().await.unwrap();
}

#[tokio::test]
async fn elected_owner_adopts_preacquired_receipts_without_retaining_waiter_logical() {
    let (owner, host) = start_host(limits(1, 2, 2));
    let (origin, origin_owner, origin_finalizer, origin_completion) =
        host.try_filesystem_scope().unwrap();
    let (cache, cache_owner, cache_finalizer, cache_completion) =
        host.try_filesystem_scope().unwrap();

    let waiter_logical = host.try_logical_io().unwrap();
    let receipts = vec![origin.try_work().unwrap(), cache.try_work().unwrap()];
    // This is the only step intended to occur inside the cache mutex after
    // dedup elects an owner.
    let producer = host.try_foreground().unwrap();
    let transaction = ForegroundTransaction::from_parts(producer, receipts).unwrap();

    drop(waiter_logical);
    let replacement_waiter = host.try_logical_io().unwrap();
    assert_eq!(
        host.try_foreground().unwrap_err().kind(),
        AdmissionKind::ForegroundProducer
    );
    drop(replacement_waiter);

    drop((origin_owner, cache_owner));
    drop(transaction);
    assert!(host.try_foreground().is_ok());
    assert_eq!(origin_finalizer.run().await, Ok(()));
    assert_eq!(cache_finalizer.run().await, Ok(()));
    assert_eq!(origin_completion.wait().await, Ok(()));
    assert_eq!(cache_completion.wait().await, Ok(()));
    owner.shutdown().await.unwrap();
}

#[tokio::test]
async fn host_close_rejects_new_operations_and_roots_immediately() {
    let (owner, host) = start_host(limits(2, 2, 1));
    owner.close_admission();

    assert_eq!(
        host.try_logical_io().unwrap_err().kind(),
        AdmissionKind::Closed
    );
    let root_error = match host.try_filesystem_scope() {
        Ok(_) => panic!("closed Host unexpectedly admitted a root"),
        Err(error) => error,
    };
    assert_eq!(root_error.kind(), AdmissionKind::Closed);
    owner.shutdown().await.unwrap();
}

#[tokio::test]
async fn logical_permit_cannot_cross_host_ownership() {
    let (first_owner, first) = start_host(limits(2, 2, 1));
    let (second_owner, second) = start_host(limits(2, 2, 1));
    let logical = first.try_logical_io().unwrap();

    assert!(first.validate_logical(&logical).is_ok());
    assert_eq!(
        second.validate_logical(&logical).unwrap_err().kind(),
        AdmissionKind::Closed
    );

    drop(logical);
    first_owner.shutdown().await.unwrap();
    second_owner.shutdown().await.unwrap();
}
