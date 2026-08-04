//! Review-only WASM repetition/allocator-reuse pressure.

use cc_spike::cc::collect_cycles;
use cc_spike::model::{closure, define, defs, Drops, MFnBody, MVal};

#[test]
fn repeated_ten_thousand_object_scc_batches() {
    for round in 0..10 {
        let drops = Drops::default();
        for _ in 0..5_000 {
            let owner = defs(&drops);
            define(
                &owner,
                "f",
                MVal::Fn(closure(&owner, vec![], MFnBody::Raw(vec![]), &drops)),
            );
            drop(owner);
        }
        let stats = collect_cycles();
        assert_eq!(stats.collected, 10_000, "round {round}");
        assert_eq!(drops.count(), 10_000, "round {round}");
        assert_eq!(collect_cycles().collected, 0, "round {round} idempotence");
    }
}
