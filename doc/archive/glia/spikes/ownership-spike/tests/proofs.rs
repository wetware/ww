//! The 17 required proofs for amended Graph 4 (+ Spike B comparisons).
use ownership_spike::cells::*;
use ownership_spike::*;
use std::rc::{Rc, Weak};

fn weak_probe(d: &Rc<Defs>) -> Weak<Defs> {
    Rc::downgrade(d)
}

/// P1: plain function storage does not cycle — owner freed when env drops.
#[test]
fn p01_plain_function_storage_no_cycle() {
    let m = Defs::new(None);
    let probe = weak_probe(&m);
    let f = make_fn(&m, vec![]);
    m.define("f", ToyVal::Fn(f)).unwrap();
    assert_eq!(Rc::strong_count(&m), 1, "stored copy must not hold M");
    drop(m);
    assert!(probe.upgrade().is_none(), "M must free with no escapees");
}

/// P2: named recursion (fn references itself by late lookup) doesn't leak.
#[test]
fn p02_named_recursion_no_leak() {
    let m = Defs::new(None);
    let probe = weak_probe(&m);
    // fact references itself through the owner at call time; its stored
    // copy is weak, so no cycle exists.
    let fact = make_fn(&m, vec![]);
    m.define("fact", ToyVal::Fn(fact)).unwrap();
    // Simulated recursive call: lookup fact through the live owner.
    let escaped = m.lookup("fact").unwrap().unwrap();
    let ToyVal::Fn(f) = &escaped else { panic!() };
    assert!(f.owner.is_strong());
    // Recursion step: resolve self again via for_call witness.
    let _captured = for_call(f).unwrap();
    drop(escaped);
    assert_eq!(Rc::strong_count(&m), 1);
    drop(m);
    assert!(probe.upgrade().is_none());
}

/// P3: mutual recursion doesn't leak; extracting only one works.
#[test]
fn p03_mutual_recursion_no_leak() {
    let m = Defs::new(None);
    let probe = weak_probe(&m);
    m.define("f", ToyVal::Fn(make_fn(&m, vec![]))).unwrap();
    m.define("g", ToyVal::Fn(make_fn(&m, vec![]))).unwrap();
    let f = m.lookup("f").unwrap().unwrap(); // only f escapes
    drop(m);
    // f alone keeps M alive; g resolvable through f's witness.
    let ToyVal::Fn(fv) = &f else { panic!() };
    let OwnerRef::Strong(m_alive) = &fv.owner else { panic!("escaped f must be strong") };
    assert!(matches!(
        m_alive.lookup("g").unwrap().unwrap(),
        ToyVal::Fn(_)
    ));
    drop(f);
    assert!(probe.upgrade().is_none(), "M frees after last escapee");
}

/// P4: same-owner callable inside captured lexical values doesn't leak.
/// (Sol P0-2: (def f (let [g (fn [] 1)] (fn [] (g)))))
#[test]
fn p04_same_owner_capture_no_leak() {
    let m = Defs::new(None);
    let probe = weak_probe(&m);
    let g = make_fn(&m, vec![]); // fresh: Strong(M)
    let f = make_fn(&m, vec![("g".into(), ToyVal::Fn(g))]); // capture rests g
    {
        let cap = f.code.captured.borrow();
        let (s, w) = count_refs(&cap.slots[0].1);
        assert_eq!((s, w), (0, 1), "captured same-owner g must rest weak");
    }
    m.define("f", ToyVal::Fn(f)).unwrap();
    assert_eq!(Rc::strong_count(&m), 1, "no cycle via capture");
    // Escaped f can still call g via for_call escape.
    let ToyVal::Fn(fe) = m.lookup("f").unwrap().unwrap() else { panic!() };
    let captured = for_call(&fe).unwrap();
    let (s, _) = count_refs(&captured[0].1);
    assert_eq!(s, 1, "for_call must escape captured g");
    drop(captured); // escaped captures are escapees: they hold M too
    drop(fe);
    drop(m);
    assert!(probe.upgrade().is_none());
}

/// P5: foreign-owner captured callables remain alive (strong, untouched).
#[test]
fn p05_foreign_capture_stays_strong() {
    let m1 = Defs::new(None);
    let m2 = Defs::new(None);
    let probe2 = weak_probe(&m2);
    let foreign = make_fn(&m2, vec![]);
    let f = make_fn(&m1, vec![("dep".into(), ToyVal::Fn(foreign))]);
    m1.define("f", ToyVal::Fn(f)).unwrap();
    drop(m2); // importer's capture must keep M2 alive
    assert!(probe2.upgrade().is_some(), "foreign owner retained via capture");
    drop(m1);
    assert!(probe2.upgrade().is_none());
}

/// P6: nested imported-module maps preserve foreign owners (F1).
#[test]
fn p06_nested_module_map_preserves_foreign_owner() {
    let m2 = Defs::new(None); // inner module
    let probe2 = weak_probe(&m2);
    m2.define("f", ToyVal::Fn(make_fn(&m2, vec![]))).unwrap();
    let export2 = ToyVal::MapV(
        m2.local_bindings()
            .unwrap()
            .into_iter()
            .map(|(k, v)| (ToyVal::Str(k), v))
            .collect(),
    );
    drop(m2); // only the export map holds M2 now
    assert!(probe2.upgrade().is_some());

    let m1 = Defs::new(None); // parent module stores the child map
    m1.define("dep", export2).unwrap();
    assert!(
        probe2.upgrade().is_some(),
        "F1: define must NOT weaken foreign Strong(M2)"
    );
    // And the stored map still yields a callable f.
    let dep = m1.lookup("dep").unwrap().unwrap();
    let ToyVal::MapV(pairs) = &dep else { panic!() };
    assert!(matches!(&pairs[0].1, ToyVal::Fn(fv) if fv.owner.is_strong()));
    drop(dep);
    drop(m1);
    assert!(probe2.upgrade().is_none());
}

/// P7: owned capability methods do not cycle.
#[test]
fn p07_owned_cap_methods_no_cycle() {
    let m = Defs::new(None);
    let probe = weak_probe(&m);
    let ping = make_fn(&m, vec![]);
    let cap = make_owned_cap(&m, vec![("ping".into(), ToyVal::Fn(ping))]);
    // Methods rested inside the sealed inner:
    let (s, w) = count_refs(&cap.inner.methods[0].1);
    assert_eq!((s, w), (0, 1));
    m.define("svc", ToyVal::Cap(cap)).unwrap();
    assert_eq!(Rc::strong_count(&m), 1, "defcap must not self-cycle");
    drop(m);
    assert!(probe.upgrade().is_none());
}

/// P8: exported capabilities survive after the module env drops.
#[test]
fn p08_exported_cap_survives() {
    let m = Defs::new(None);
    let probe = weak_probe(&m);
    let cap = make_owned_cap(&m, vec![("ping".into(), ToyVal::Fn(make_fn(&m, vec![])))]);
    m.define("svc", ToyVal::Cap(cap)).unwrap();
    let escaped = m.lookup("svc").unwrap().unwrap();
    drop(m);
    assert!(probe.upgrade().is_some(), "escaped cap carries the witness");
    let ToyVal::Cap(c) = &escaped else { panic!() };
    let method = cap_dispatch(c, "ping").unwrap().unwrap();
    assert!(matches!(method, ToyVal::Fn(fv) if fv.owner.is_strong()));
    drop(escaped);
    assert!(probe.upgrade().is_none());
}

/// P9: attenuated capability wrappers preserve lifetime.
#[test]
fn p09_attenuation_preserves_lifetime() {
    let m = Defs::new(None);
    let probe = weak_probe(&m);
    let cap = make_owned_cap(
        &m,
        vec![
            ("ping".into(), ToyVal::Fn(make_fn(&m, vec![]))),
            ("admin".into(), ToyVal::Fn(make_fn(&m, vec![]))),
        ],
    );
    let escaped = {
        let stored = ToyVal::Cap(cap);
        m.define("svc", stored).unwrap();
        m.lookup("svc").unwrap().unwrap()
    };
    let ToyVal::Cap(base) = &escaped else { panic!() };
    let thin = attenuate(base, &["ping"]);
    drop(escaped);
    drop(m);
    assert!(probe.upgrade().is_some(), "attenuated wrapper carries owner");
    assert!(cap_dispatch(&thin, "ping").unwrap().is_some());
    assert!(cap_dispatch(&thin, "admin").unwrap().is_none());
    drop(thin);
    assert!(probe.upgrade().is_none());
}

/// P10: ordinary maps require no hidden owner metadata.
#[test]
fn p10_ordinary_map_no_hidden_metadata() {
    let m = Defs::new(None);
    let probe = weak_probe(&m);
    m.define("f", ToyVal::Fn(make_fn(&m, vec![]))).unwrap();
    m.define("answer", ToyVal::Int(42)).unwrap();
    let export = ToyVal::MapV(
        m.local_bindings()
            .unwrap()
            .into_iter()
            .map(|(k, v)| (ToyVal::Str(k), v))
            .collect(),
    );
    drop(m);
    // The map is a plain value; lifetime rides its Fn values only.
    assert!(probe.upgrade().is_some());
    let ToyVal::MapV(pairs) = export else { panic!() };
    let f = pairs
        .iter()
        .find(|(k, _)| matches!(k, ToyVal::Str(s) if s == "f"))
        .unwrap()
        .1
        .clone();
    drop(pairs); // map dropped, extracted callable remains
    assert!(probe.upgrade().is_some());
    drop(f);
    assert!(probe.upgrade().is_none());
}

/// P11: data-only owner frees immediately when the active env drops.
#[test]
fn p11_data_only_owner_frees_immediately() {
    let m = Defs::new(None);
    let probe = weak_probe(&m);
    m.define("x", ToyVal::Int(1)).unwrap();
    let export = m.local_bindings().unwrap();
    drop(m);
    assert!(probe.upgrade().is_none(), "data-only module frees at once");
    drop(export);
}

/// P12: the last escaped owner-bearing value controls reclamation.
#[test]
fn p12_last_escapee_controls_reclamation() {
    let m = Defs::new(None);
    let probe = weak_probe(&m);
    m.define("f", ToyVal::Fn(make_fn(&m, vec![]))).unwrap();
    m.define("g", ToyVal::Fn(make_fn(&m, vec![]))).unwrap();
    let f = m.lookup("f").unwrap().unwrap();
    let g = m.lookup("g").unwrap().unwrap();
    drop(m);
    assert!(probe.upgrade().is_some());
    drop(f);
    assert!(probe.upgrade().is_some(), "g still holds M");
    drop(g);
    assert!(probe.upgrade().is_none());
}

/// P13: unmatched weak values fail explicitly (never silently).
#[test]
fn p13_unmatched_weak_fails_explicitly() {
    let m1 = Defs::new(None);
    let m2 = Defs::new(None);
    // Forge an invalid state: a value resting-weak against m1, escaped
    // under m2's witness.
    let f = make_fn(&m1, vec![]);
    let (rested, _) = rest_for(&m1, &ToyVal::Fn(f));
    let err = escape_with(&m2, &rested).unwrap_err();
    assert_eq!(err, OwnershipFault::UnmatchedWeak);
    // And frozen mutation faults too.
    let p = Defs::new(None);
    p.freeze();
    assert_eq!(
        p.define("x", ToyVal::Int(1)).unwrap_err(),
        OwnershipFault::FrozenMutation
    );
}

/// P14: deep containers do not recurse on the Rust stack (transform is
/// iterative). NOTE: Vec-drop of deep values recurses in Rust, so teardown
/// here is iterative too — a pre-existing property of any recursive value
/// type (production Glia included), separate from the transforms.
#[test]
fn p14_deep_containers_iterative() {
    fn drop_iteratively(mut v: ToyVal) {
        let mut stack = Vec::new();
        loop {
            match v {
                ToyVal::List(mut xs) => {
                    if let Some(next) = xs.pop() {
                        stack.extend(xs.into_iter());
                        v = next;
                        continue;
                    }
                }
                _ => {}
            }
            match stack.pop() {
                Some(next) => v = next,
                None => break,
            }
        }
    }
    let m = Defs::new(None);
    let mut v = ToyVal::Fn(make_fn(&m, vec![]));
    const DEPTH: usize = 200_000;
    for _ in 0..DEPTH {
        v = ToyVal::List(vec![v]);
    }
    // 200k-deep: recursive transforms would overflow any thread stack.
    m.define_ref("deep", &v).unwrap();
    drop_iteratively(v);
    let out = m.lookup("deep").unwrap().unwrap();
    let (s, w) = count_refs(&out);
    assert_eq!((s, w), (1, 0));
    drop_iteratively(out);
    // Tear down the stored copy iteratively as well before Defs drops.
    let stored = m.bindings.borrow_mut().remove("deep").unwrap();
    drop_iteratively(stored.value);
}

/// P15: equality/hash identity unchanged by rest/escape.
#[test]
fn p15_identity_preserved() {
    let m = Defs::new(None);
    let f = make_fn(&m, vec![]);
    let id = f.identity_hash();
    m.define("f", ToyVal::Fn(f.clone())).unwrap();
    let a = m.lookup("f").unwrap().unwrap();
    let b = m.lookup("f").unwrap().unwrap();
    let (ToyVal::Fn(fa), ToyVal::Fn(fb)) = (&a, &b) else { panic!() };
    assert!(fa.same_identity(fb), "(= (get m :f) (get m :f))");
    assert!(fa.same_identity(&f), "(= f g) aliasing");
    assert_eq!(fa.identity_hash(), id, "hash by captured-env pointer stable");
}

/// P16: map keys containing callables remain valid through transforms.
#[test]
fn p16_callable_map_keys() {
    let m = Defs::new(None);
    let key_fn = make_fn(&m, vec![]);
    let key_id = key_fn.identity_hash();
    let map = ToyVal::MapV(vec![(ToyVal::Fn(key_fn), ToyVal::Int(7))]);
    m.define("m", map).unwrap();
    let out = m.lookup("m").unwrap().unwrap();
    let ToyVal::MapV(pairs) = out else { panic!() };
    let ToyVal::Fn(k) = &pairs[0].0 else { panic!() };
    assert_eq!(k.identity_hash(), key_id, "key identity/hash preserved");
    assert!(k.owner.is_strong(), "escaped key upgraded");
}

/// P17: accepted atom cycles are isolated and measurable.
#[test]
fn p17_atom_cycle_isolated_and_measured() {
    use std::cell::RefCell;
    let before = DEFS_DROPS.load(std::sync::atomic::Ordering::Relaxed);
    {
        let m = Defs::new(None);
        // Atom containing a Strong(M) callable, stored in M: the one
        // accepted cycle class (transform stops at atoms).
        let atom = Rc::new(RefCell::new(ToyVal::Fn(make_fn(&m, vec![]))));
        m.define("a", ToyVal::Atom(atom)).unwrap();
        // Leak expected on drop of m: cycle keeps M alive.
        let probe = weak_probe(&m);
        drop(m);
        assert!(probe.upgrade().is_some(), "accepted atom cycle leaks M");
        // Breaking the cycle by clearing the atom releases M.
        let m_alive = probe.upgrade().unwrap();
        let ToyVal::Atom(a) = m_alive
            .bindings
            .borrow()
            .get("a")
            .unwrap()
            .value
            .clone()
        else {
            panic!()
        };
        *a.borrow_mut() = ToyVal::Int(0);
        drop(m_alive);
        assert!(probe.upgrade().is_none(), "clearing the atom frees M");
    }
    let after = DEFS_DROPS.load(std::sync::atomic::Ordering::Relaxed);
    assert_eq!(after - before, 1, "exactly this Defs reclaimed");
}

// ---------------------------------------------------------------------------
// Spike B comparisons (cells competitor)
// ---------------------------------------------------------------------------

/// B1: cells give O(1) late lookup, but the anchor needs the SAME barrier:
/// stored values must still rest, escapes must still upgrade.
#[test]
fn b1_cells_same_barrier_shape() {
    let d = Defs2::new();
    let f = make_fn2(&d, &["fact"]);
    d.define("fact", CVal::Fn(f));
    // Stored copy rested (anchor weak) — identical obligation to Graph 4.
    assert_eq!(Rc::strong_count(&d), 1, "cells do not remove the rest step");
    let escaped = d.lookup("fact").unwrap();
    let CVal::Fn(fe) = &escaped else { panic!() };
    assert!(matches!(fe.anchor, CAnchor::Strong(_)));
    // Recursion resolves via witnessed cell deref (no name hash).
    assert!(resolve_at_call(fe, "fact").is_ok());
    drop(escaped);
    let probe = Rc::downgrade(&d);
    drop(d);
    assert!(probe.upgrade().is_none());
}

/// B2: cells survive redefinition with stable identity + versions
/// (the genuine advantage: per-cell versioning for capability caching).
#[test]
fn b2_cells_versioning_advantage() {
    let d = Defs2::new();
    let f = make_fn2(&d, &["x"]);
    d.define("x", CVal::Int(1));
    let v1 = d.cell("x").version.get();
    d.define("x", CVal::Int(2));
    let v2 = d.cell("x").version.get();
    assert!(v2 > v1, "redefinition bumps the cell version");
    // Late binding through the cell: existing closure sees the new value.
    let escaped = escape_anchor(&d, &CVal::Fn(f.clone()));
    let CVal::Fn(fe) = &escaped else { panic!() };
    match resolve_at_call(fe, "x").unwrap() {
        CVal::Int(2) => {}
        _ => panic!("late binding through cell"),
    }
}

/// B3: cells do NOT eliminate container rewriting: a closure inside a list
/// stored in its own Defs2 still needs the positional anchor rest.
#[test]
fn b3_cells_still_need_container_rewrite() {
    let d = Defs2::new();
    let f = make_fn2(&d, &[]);
    d.define("fns", CVal::List(vec![CVal::Fn(f)]));
    assert_eq!(
        Rc::strong_count(&d),
        1,
        "container walk still required with cells"
    );
}
