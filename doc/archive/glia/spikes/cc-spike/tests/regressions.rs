//! Historical cycle-collector bug classes (spec §14) + borrow-failure
//! rollback (§4) + async retention (§10).

use cc_spike::cc::{collect_cycles, suspects, Cc};
use cc_spike::model::*;
use std::cell::RefCell;

fn fnv(c: MClosure) -> MVal {
    MVal::Fn(c)
}

// extra-free / pre-compensation: whites referencing whites must not
// double-drop or underflow during the destruction cascade (the classic
// bacon-rajan `extra_free` shape: Env ↔ closures).
#[test]
fn r01_extra_free_shape() {
    let d = Drops::default();
    let a = defs(&d);
    let g = closure(&a, vec![], MFnBody::Raw(vec![]), &d);
    let f = closure(&a, vec![("g".into(), fnv(g.clone()))], MFnBody::Raw(vec![]), &d);
    define(&a, "f", fnv(f));
    define(&a, "g", fnv(g));
    drop(a);
    collect_cycles();
    assert_eq!(d.count(), 3, "defs + two captures, each exactly once");
}

// scan_black restoration: an external root reaching INTO a candidate cycle
// must restore counts correctly and survive.
#[test]
fn r02_scan_black_restoration() {
    let d = Drops::default();
    let a = defs(&d);
    let f = closure(&a, vec![], MFnBody::Raw(vec![]), &d);
    define(&a, "f", fnv(f.clone()));
    let external = fnv(f); // external root into the cycle
    // Buffer the owner as a suspect via a clone/drop pulse.
    let pulse = a.clone();
    drop(pulse);
    collect_cycles();
    assert_eq!(d.count(), 0, "externally reachable cycle fully restored");
    drop(external);
    drop(a);
    collect_cycles();
    assert_eq!(d.count(), 2);
}

// duplicate buffering: many clone/drop pulses buffer an object once.
#[test]
fn r03_duplicate_buffering() {
    let d = Drops::default();
    let a = defs(&d);
    define(&a, "f", fnv(closure(&a, vec![], MFnBody::Raw(vec![]), &d)));
    let before = suspects();
    for _ in 0..100 {
        let c = a.clone();
        drop(c);
    }
    assert!(
        suspects() - before <= 1,
        "buffered flag deduplicates suspects"
    );
    drop(a);
    collect_cycles();
    assert_eq!(d.count(), 2);
}

// buffered object re-incremented before collection: purge discards it.
#[test]
fn r04_reincrement_before_collect() {
    let d = Drops::default();
    let a = defs(&d);
    let pulse = a.clone();
    drop(pulse); // buffered as Purple
    let keep = a.clone(); // re-increment → Black
    let stats = collect_cycles();
    assert_eq!(stats.collected, 0, "re-blackened suspect purged, not traced");
    drop(keep);
    drop(a);
    assert_eq!(d.count(), 1, "acyclic death at zero");
    collect_cycles(); // purge the parked allocation (quiescence)
}

// buffer containing already-dead entries: purge deallocates safely.
#[test]
fn r05_dead_buffer_entries() {
    let d = Drops::default();
    {
        let a = defs(&d);
        let pulse = a.clone();
        drop(pulse); // buffered
        drop(a); // dies at zero; value dropped; allocation parked for purge
    }
    assert_eq!(d.count(), 1, "value destroyed at zero");
    collect_cycles(); // purge deallocates the parked allocation (Miri-checked)
}

// cycle with an outgoing edge to a rooted object: the rooted target
// survives; the cycle dies; the cascade releases the target's count.
#[test]
fn r06_cycle_with_outgoing_rooted_edge() {
    let d = Drops::default();
    let rooted = Cc::new(MAtom {
        value: RefCell::new(MVal::Int(7)),
        drops: d.clone(),
    });
    let a = defs(&d);
    define(
        &a,
        "f",
        fnv(closure(
            &a,
            vec![("r".into(), MVal::Atom(rooted.clone()))],
            MFnBody::Raw(vec![]),
            &d,
        )),
    );
    drop(a);
    collect_cycles();
    assert_eq!(d.count(), 2, "cycle reclaimed; rooted atom survives");
    assert!(matches!(&*rooted.value.borrow(), MVal::Int(7)));
    drop(rooted);
    assert_eq!(d.count(), 3);
    collect_cycles(); // purge the parked buffered allocation (quiescence)
}

// multiple cycles sharing nodes + nested SCCs are covered by proofs
// p10/p11; here: drops that release additional Cc handles mid-collection.
#[test]
#[cfg_attr(miri, ignore)] // 1k-cascade scale case: algorithm covered by r01-r06/r08
fn r07_drop_releases_more_handles() {
    let d = Drops::default();
    let a = defs(&d);
    // The cycle holds an acyclic side-chain: destroying the cycle cascades
    // through 1,000 additional releases inside the destruction pump.
    let mut chain = MVal::Int(0);
    for _ in 0..1_000 {
        chain = MVal::Atom(Cc::new(MAtom {
            value: RefCell::new(chain),
            drops: d.clone(),
        }));
    }
    define(
        &a,
        "f",
        fnv(closure(&a, vec![("chain".into(), chain)], MFnBody::Raw(vec![]), &d)),
    );
    drop(a);
    collect_cycles();
    assert_eq!(d.count(), 1_002, "cycle + full cascade, exactly once each");
}

// collection immediately after collection (empty + non-empty buffers).
#[test]
fn r08_back_to_back_collections() {
    let d = Drops::default();
    let a = defs(&d);
    define(&a, "f", fnv(closure(&a, vec![], MFnBody::Raw(vec![]), &d)));
    drop(a);
    let s1 = collect_cycles();
    let s2 = collect_cycles();
    let s3 = collect_cycles();
    assert_eq!((s1.collected, s2.collected, s3.collected), (2, 0, 0));
    assert_eq!(d.count(), 2);
}

// §4: borrow failure during tracing → abort with ZERO mutation; the graph
// remains valid and collectable later.
#[test]
fn r09_borrow_failure_aborts_and_recovers() {
    let d = Drops::default();
    let a = defs(&d);
    let atom = Cc::new(MAtom {
        value: RefCell::new(MVal::Int(0)),
        drops: d.clone(),
    });
    *atom.value.borrow_mut() = fnv(closure(&a, vec![], MFnBody::Raw(vec![]), &d));
    define(&a, "cell", MVal::Atom(atom.clone()));
    let pulse = a.clone();
    drop(pulse); // buffer the owner
    {
        let _live_borrow = atom.value.borrow_mut(); // non-safepoint state
        // Debug builds fail loudly (caller bug); release builds return an
        // abort. Both leave the collector state fully valid — proven by
        // the successful collection below.
        let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(collect_cycles));
        if cfg!(debug_assertions) {
            assert!(outcome.is_err(), "debug: loud failure at non-safepoint");
        } else {
            let stats = outcome.expect("release: safe abort");
            assert!(stats.aborted, "collection must abort, not proceed");
            assert_eq!(stats.collected, 0, "nothing reclaimed on abort");
        }
        assert_eq!(d.count(), 0, "no drops on abort");
    }
    // Safepoint restored: the SAME suspects collect fine.
    drop(atom);
    drop(a);
    let stats = collect_cycles();
    assert!(!stats.aborted);
    assert_eq!(d.count(), 3, "full recovery after aborted attempt");
    collect_cycles(); // purge any parked allocations (quiescence)
}

// threshold-triggered collection after (simulated) cancellation: dropping
// a "future"'s captured state buffers suspects; maybe_collect fires.
#[test]
fn r10_threshold_after_cancellation() {
    use cc_spike::cc::maybe_collect;
    let d = Drops::default();
    let a = defs(&d);
    define(&a, "f", fnv(closure(&a, vec![], MFnBody::Raw(vec![]), &d)));
    let future_state = vec![MVal::Atom(Cc::new(MAtom {
        value: RefCell::new(a.bindings.borrow()["f"].clone()),
        drops: d.clone(),
    }))];
    drop(a);
    drop(future_state); // "cancelled future" releases its retained state
    let stats = maybe_collect(1).expect("threshold reached");
    assert!(!stats.aborted);
    assert_eq!(d.count(), 3, "cancellation debris collected at safepoint");
    collect_cycles(); // purge any parked allocations (quiescence)
}

// §10: async retention — a suspended hand-rolled future is the SOLE root;
// collection must not touch it; resume works; post-drop it collects.
#[test]
fn r11_async_suspension_retention() {
    use std::future::Future;
    use std::pin::Pin;
    use std::task::{Context, Poll, Waker};

    struct TwoStep {
        state: u8,
        held: Option<MVal>,
    }
    impl Future for TwoStep {
        type Output = i64;
        fn poll(mut self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<i64> {
            match self.state {
                0 => {
                    self.state = 1;
                    Poll::Pending // suspend while HOLDING the only root
                }
                _ => {
                    let Some(MVal::Fn(f)) = self.held.take() else {
                        panic!("callable survived suspension");
                    };
                    // "Invoke": read through the capture allocation.
                    let n = f.captured.slots.len() as i64;
                    Poll::Ready(n)
                }
            }
        }
    }

    let d = Drops::default();
    let a = defs(&d);
    define(
        &a,
        "f",
        fnv(closure(&a, vec![("x".into(), MVal::Int(1))], MFnBody::Raw(vec![]), &d)),
    );
    let escaped = a.bindings.borrow()["f"].clone();
    let mut fut = Box::pin(TwoStep {
        state: 0,
        held: Some(escaped),
    });
    drop(a); // future now holds the only external root

    let waker = Waker::noop();
    let mut cx = Context::from_waker(waker);
    assert_eq!(fut.as_mut().poll(&mut cx), Poll::Pending);

    collect_cycles(); // safepoint while suspended
    assert_eq!(d.count(), 0, "future-held graph fully retained (no rooting API)");

    assert_eq!(fut.as_mut().poll(&mut cx), Poll::Ready(1), "resume + use");
    drop(fut);
    collect_cycles();
    assert_eq!(d.count(), 2, "reclaimed after the future drops");
}

// §10 cancellation variant: drop the future while suspended.
#[test]
fn r12_async_cancellation() {
    let d = Drops::default();
    let a = defs(&d);
    define(&a, "f", fnv(closure(&a, vec![], MFnBody::Raw(vec![]), &d)));
    let escaped = a.bindings.borrow()["f"].clone();
    let holder: Vec<MVal> = vec![escaped]; // stands in for captured future state
    drop(a);
    collect_cycles();
    assert_eq!(d.count(), 0, "suspended state retains");
    drop(holder); // cancellation
    collect_cycles();
    assert_eq!(d.count(), 2, "cancelled future's graph collected");
    collect_cycles(); // purge (quiescence)
}
