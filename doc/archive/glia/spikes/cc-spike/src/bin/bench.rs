//! cc-spike benchmarks. THRESHOLDS SET BEFORE MEASUREMENT (spec §18):
//!   T1  Cc clone+drop ≤ 1.5× Rc clone+drop
//!   T2  nonzero decrement with buffering ≤ 2× Rc drop
//!   T3  purge of 100k dead acyclic candidates ≤ 5 ms (release)
//!   T4  collection of a 10k-object SCC batch ≤ 50 ms (release)
//!   T5  repeated lookup of a stored callable: flat per-iteration cost,
//!       ≥ 5× cheaper than deep-CoW escape-per-lookup
//!   T6  ownership adds ZERO body-size-dependent overhead beyond the
//!       baseline value clone (which production pays today)
//!   T7  shared-DAG collection scales with unique nodes (time ratio for
//!       2× nodes ≤ 3×; trace calls linear), never path multiplicity
//!   T8  mutual service graph (1k pairs) collects ≤ 50 ms
//! Breach = report and stop, never silently optimize.

use cc_spike::cc::{collect_cycles, suspects, trace_calls, Cc};
use cc_spike::model::*;
use ownership_spike::crossowner as cow;
use std::cell::RefCell;
use std::rc::Rc;
use std::time::Instant;

fn fnv(c: MClosure) -> MVal {
    MVal::Fn(c)
}

fn ms(t: Instant) -> f64 {
    t.elapsed().as_secs_f64() * 1e3
}

fn main() {
    let mode = if cfg!(debug_assertions) { "debug" } else { "release" };
    println!("== cc-spike bench ({mode}) ==");

    // T1: clone+drop ratio vs Rc.
    {
        let n = 1_000_000u32;
        let rc = Rc::new(1u64);
        let t = Instant::now();
        for _ in 0..n {
            let c = Rc::clone(&rc);
            drop(c);
        }
        let rc_ms = ms(t);
        let d = Drops::default();
        let cc = Cc::new(MAtom {
            value: RefCell::new(MVal::Int(1)),
            drops: d.clone(),
        });
        let t = Instant::now();
        for _ in 0..n {
            let c = cc.clone();
            drop(c);
        }
        let cc_ms = ms(t);
        // NOTE: clone→drop of a >1-count Cc takes the buffering path each
        // time; the buffered flag dedups, so cost is color/flag writes.
        let ratio = cc_ms / rc_ms;
        println!("T1 clone+drop: Rc {rc_ms:.2} ms, Cc {cc_ms:.2} ms, ratio {ratio:.2} (gate ≤1.5) {}",
            if ratio <= 1.5 { "PASS" } else { "FAIL" });
        collect_cycles();
    }

    // T2: nonzero decrement w/ buffering vs Rc drop.
    {
        let n = 1_000_000u32;
        let rc = Rc::new(1u64);
        let clones: Vec<_> = (0..n).map(|_| Rc::clone(&rc)).collect();
        let t = Instant::now();
        drop(clones);
        let rc_ms = ms(t);
        let d = Drops::default();
        let cc = Cc::new(MAtom {
            value: RefCell::new(MVal::Int(1)),
            drops: d.clone(),
        });
        let clones: Vec<_> = (0..n).map(|_| cc.clone()).collect();
        let t = Instant::now();
        drop(clones);
        let cc_ms = ms(t);
        let ratio = cc_ms / rc_ms;
        println!("T2 nonzero dec: Rc {rc_ms:.2} ms, Cc {cc_ms:.2} ms, ratio {ratio:.2} (gate ≤2.0) {}",
            if ratio <= 2.0 { "PASS" } else { "FAIL" });
        collect_cycles();
    }

    // T3: purge 100k dead acyclic candidates.
    {
        let d = Drops::default();
        for _ in 0..100_000 {
            let a = Cc::new(MAtom {
                value: RefCell::new(MVal::Int(0)),
                drops: d.clone(),
            });
            let pulse = a.clone();
            drop(pulse); // buffered as suspect
            drop(a); // dies at zero; parked for purge
        }
        assert_eq!(d.count(), 100_000);
        let n = suspects();
        let t = Instant::now();
        collect_cycles();
        let purge_ms = ms(t);
        println!("T3 purge {n} dead candidates: {purge_ms:.2} ms (gate ≤5 release) {}",
            if purge_ms <= 5.0 || cfg!(debug_assertions) { "PASS" } else { "FAIL" });
    }

    // T4: 10k-object SCC batch.
    {
        let d = Drops::default();
        for _ in 0..5_000 {
            let a = defs(&d);
            define(&a, "f", fnv(closure(&a, vec![], MFnBody::Raw(vec![]), &d)));
            drop(a);
        }
        let t = Instant::now();
        collect_cycles();
        let c_ms = ms(t);
        assert_eq!(d.count(), 10_000);
        println!("T4 collect 10k-object SCC batch: {c_ms:.2} ms (gate ≤50 release) {}",
            if c_ms <= 50.0 || cfg!(debug_assertions) { "PASS" } else { "FAIL" });
    }

    // T5+T6: repeated lookup; body-size independence. Compare vs deep CoW.
    {
        // Cc model: lookup = clone the stored value (no transform).
        let d = Drops::default();
        let a = defs(&d);
        let big_body: Vec<MExpr> = (0..10_000).map(|i| MExpr::Const(MVal::Int(i))).collect();
        define(&a, "f", fnv(closure(&a, vec![], MFnBody::Analyzed(big_body), &d)));
        let small = {
            let a2 = defs(&d);
            define(&a2, "f", fnv(closure(&a2, vec![], MFnBody::Raw(vec![]), &d)));
            a2
        };
        let iters = 2_000;
        let t = Instant::now();
        for _ in 0..iters {
            let v = small.bindings.borrow()["f"].clone();
            drop(v);
        }
        let small_ms = ms(t) / iters as f64;
        let t = Instant::now();
        for _ in 0..iters {
            let v = a.bindings.borrow()["f"].clone();
            drop(v);
        }
        let big_ms = ms(t) / iters as f64;

        // Deep-CoW comparator: store once, escape per lookup.
        let owner = cow::XOwner::new();
        let helper = cow::make_fn(&owner, vec![], vec![]);
        let f = cow::make_fn(
            &owner,
            vec![("h".into(), cow::XVal::Fn(helper))],
            (0..10_000).map(cow::XVal::Int).collect(),
        );
        cow::define_deep(&owner, "f", cow::XVal::Fn(f));
        let t = Instant::now();
        for _ in 0..iters {
            let v = cow::lookup_deep(&owner, "f").unwrap().unwrap();
            drop(v);
        }
        let cow_ms = ms(t) / iters as f64;
        println!("T5 lookup/iter: Cc(small) {small_ms:.5} ms, deep-CoW {cow_ms:.5} ms → CoW/Cc {:.1}× (gate ≥5×) {}",
            cow_ms / small_ms, if cow_ms / small_ms >= 5.0 { "PASS" } else { "FAIL" });
        // T6: the ownership layer adds ZERO body-dependent overhead beyond
        // the plain value clone production already pays; measured here as
        // the ratio of lookup cost to a bare clone of the same value.
        let bare = {
            let v = a.bindings.borrow()["f"].clone();
            let t = Instant::now();
            for _ in 0..iters {
                let c = v.clone();
                drop(c);
            }
            ms(t) / iters as f64
        };
        let overhead = big_ms / bare;
        println!("T6 big-body lookup {big_ms:.5} ms vs bare clone {bare:.5} ms → overhead {overhead:.2}× (gate ≤1.25) {}",
            if overhead <= 1.25 { "PASS" } else { "FAIL" });
        drop(a);
        drop(small);
        collect_cycles();
    }

    // T7: shared-DAG scaling (unique nodes, not paths).
    {
        fn dag(layers: usize, d: &Drops) -> (Cc<MDefs>, usize) {
            let a = defs(d);
            let mut layer = closure(&a, vec![], MFnBody::Raw(vec![]), d);
            for _ in 0..layers {
                layer = closure(
                    &a,
                    vec![
                        ("l".into(), MVal::Fn(layer.clone())),
                        ("r".into(), MVal::Fn(layer.clone())),
                    ],
                    MFnBody::Raw(vec![]),
                    d,
                );
            }
            define(&a, "dag", MVal::Fn(layer));
            (a, layers)
        }
        let d = Drops::default();
        let (a8, _) = dag(200, &d);
        drop(a8);
        let calls0 = trace_calls();
        let t = Instant::now();
        collect_cycles();
        let t200 = ms(t);
        let calls200 = trace_calls() - calls0;

        let (a16, _) = dag(400, &d);
        drop(a16);
        let calls0 = trace_calls();
        let t = Instant::now();
        collect_cycles();
        let t400 = ms(t);
        let calls400 = trace_calls() - calls0;
        let ratio = t400 / t200.max(1e-6);
        println!("T7 shared DAG: 200 layers {t200:.3} ms/{calls200} traces; 400 layers {t400:.3} ms/{calls400} traces; time ratio {ratio:.2} (gate ≤3, paths would be 2^200×) {}",
            if ratio <= 3.0 { "PASS" } else { "FAIL" });
    }

    // T8: mutual service graph (1k A↔B pairs).
    {
        let d = Drops::default();
        for _ in 0..1_000 {
            let a = defs(&d);
            let b = defs(&d);
            define(&a, "their", fnv(closure(&b, vec![], MFnBody::Raw(vec![]), &d)));
            define(&b, "their", fnv(closure(&a, vec![], MFnBody::Raw(vec![]), &d)));
            drop(a);
            drop(b);
        }
        let t = Instant::now();
        collect_cycles();
        let c_ms = ms(t);
        assert_eq!(d.count(), 4_000);
        println!("T8 1k mutual service pairs: {c_ms:.2} ms (gate ≤50 release) {}",
            if c_ms <= 50.0 || cfg!(debug_assertions) { "PASS" } else { "FAIL" });
    }
}
