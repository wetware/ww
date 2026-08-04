//! Deterministic graph proofs (spec §7) + the six Graph 4 failure classes
//! (spec §8). Liveness observable: `Drops` counters — 0 after all roots
//! drop + collect = LEAK; exactly 1 = reclaimed exactly once.

use cc_spike::cc::{collect_cycles, suspects, Cc};
use cc_spike::model::*;

fn fnv(c: MClosure) -> MVal {
    MVal::Fn(c)
}

// 1. Plain acyclic object drops immediately, without collection.
#[test]
fn p01_acyclic_drops_immediately() {
    let d = Drops::default();
    let a = defs(&d);
    define(&a, "x", MVal::Int(1));
    drop(a);
    assert_eq!(d.count(), 1, "no collection required");
}

// 2 + §8a. Simple Defs ↔ closure cycle reclaims.
#[test]
fn p02_simple_owner_cycle_reclaims() {
    let d = Drops::default();
    let a = defs(&d);
    let f = closure(&a, vec![], MFnBody::Raw(vec![]), &d);
    define(&a, "f", fnv(f));
    drop(a);
    assert_eq!(d.count(), 0, "cycle: nothing dropped yet");
    collect_cycles();
    assert_eq!(d.count(), 2, "defs + captured reclaimed");
}

// 3. Named-recursion owner graph reclaims.
#[test]
fn p03_named_recursion_reclaims() {
    let d = Drops::default();
    let a = defs(&d);
    // fact's body references its own name through the owner (late binding):
    // modeled as a body Const referencing the owner-stored value is not
    // needed — the owner edge IS the recursion edge.
    let fact = closure(&a, vec![], MFnBody::Analyzed(vec![MExpr::Sub(vec![])]), &d);
    define(&a, "fact", fnv(fact));
    drop(a);
    collect_cycles();
    assert_eq!(d.count(), 2);
}

// 4. Mutual recursion reclaims.
#[test]
fn p04_mutual_recursion_reclaims() {
    let d = Drops::default();
    let a = defs(&d);
    define(&a, "even", fnv(closure(&a, vec![], MFnBody::Raw(vec![]), &d)));
    define(&a, "odd", fnv(closure(&a, vec![], MFnBody::Raw(vec![]), &d)));
    drop(a);
    collect_cycles();
    assert_eq!(d.count(), 3, "defs + 2 captures");
}

// 5 + §8b. Sol P1-A: foreign-factory routed self-cycle reclaims.
#[test]
fn p05_p1a_foreign_factory_reclaims() {
    let d = Drops::default();
    let a = defs(&d);
    let b = defs(&d);
    let g = closure(&a, vec![], MFnBody::Raw(vec![]), &d);
    // B-owned f captures A-owned g (plus a B-owned h, the state that broke
    // the deep-CoW model).
    let h = closure(&b, vec![], MFnBody::Raw(vec![]), &d);
    let f = closure(
        &b,
        vec![("g".into(), fnv(g.clone())), ("h".into(), fnv(h))],
        MFnBody::Raw(vec![]),
        &d,
    );
    define(&a, "f", fnv(f));
    drop(g);
    drop(a);
    drop(b);
    collect_cycles();
    // a-defs, b-defs, f.captured, g.captured, h.captured
    assert_eq!(d.count(), 5, "both owners + all captures reclaimed");
}

// 6 + §8c. Sol P1-B: callable hidden in executable body reclaims.
#[test]
fn p06_p1b_body_hidden_reclaims() {
    let d = Drops::default();
    let a = defs(&d);
    let g = closure(&a, vec![], MFnBody::Raw(vec![]), &d);
    let f = closure(
        &a,
        vec![],
        MFnBody::Analyzed(vec![
            MExpr::Const(fnv(g.clone())),
            MExpr::Quote(MVal::List(vec![fnv(g.clone())])),
            MExpr::CallRaw(vec![fnv(g.clone())]),
            MExpr::Match(vec![(
                MPattern::MapKeys(vec![(fnv(g.clone()), MPattern::Literal(MVal::Int(1)))]),
                MExpr::Sub(vec![]),
            )]),
        ]),
        &d,
    );
    define(&a, "f", fnv(f));
    drop(g);
    drop(a);
    collect_cycles();
    assert_eq!(d.count(), 3, "owner + both captures despite body/pattern edges");
}

// 7 + §8d. TRUE mutual cross-owner SCC reclaims (the class positional
// rules could never touch).
#[test]
fn p07_true_mutual_scc_reclaims() {
    let d = Drops::default();
    let a = defs(&d);
    let b = defs(&d);
    let fa = closure(&a, vec![], MFnBody::Raw(vec![]), &d);
    let fb = closure(&b, vec![], MFnBody::Raw(vec![]), &d);
    define(&a, "their", fnv(fb));
    define(&b, "their", fnv(fa));
    drop(a);
    drop(b);
    collect_cycles();
    assert_eq!(d.count(), 4, "both owners + both captures");
}

// 8 + §8e. Atom callback-registry SCC (the ww/test `*tests*` shape).
#[test]
fn p08_atom_registry_scc_reclaims() {
    let d = Drops::default();
    let a = defs(&d);
    let registry = Cc::new(MAtom {
        value: std::cell::RefCell::new(MVal::List(vec![])),
        drops: d.clone(),
    });
    let callback = closure(&a, vec![], MFnBody::Raw(vec![]), &d);
    // registry holds A's callback; A holds the registry.
    *registry.value.borrow_mut() = MVal::List(vec![fnv(callback)]);
    define(&a, "registry", MVal::Atom(registry.clone()));
    drop(registry);
    drop(a);
    collect_cycles();
    assert_eq!(d.count(), 3, "owner + atom + capture");
}

// 9 + §8f. Capability service wiring SCC.
#[test]
fn p09_cap_service_scc_reclaims() {
    let d = Drops::default();
    let a = defs(&d);
    let b = defs(&d);
    let service = closure(&a, vec![], MFnBody::Raw(vec![]), &d);
    let cap = Cc::new(MCapInner {
        methods: std::cell::RefCell::new(vec![(
            "call".into(),
            fnv(closure(&b, vec![], MFnBody::Raw(vec![]), &d)),
        )]),
        base: std::cell::RefCell::new(None),
        handler: std::cell::RefCell::new(Some(fnv(service))),
        drops: d.clone(),
    });
    define(&a, "b-cap", MVal::Cap(cap.clone()));
    drop(cap);
    drop(a);
    drop(b);
    collect_cycles();
    assert_eq!(d.count(), 5, "owners + cap inner + both captures");
}

// 10. Nested SCCs reclaim in one pass.
#[test]
fn p10_nested_sccs_reclaim() {
    let d = Drops::default();
    let inner_owner = defs(&d);
    define(
        &inner_owner,
        "self",
        fnv(closure(&inner_owner, vec![], MFnBody::Raw(vec![]), &d)),
    );
    let outer_owner = defs(&d);
    let bridge = closure(
        &outer_owner,
        vec![("inner".into(), MVal::List(vec![fnv(closure(
            &inner_owner,
            vec![],
            MFnBody::Raw(vec![]),
            &d,
        ))]))],
        MFnBody::Raw(vec![]),
        &d,
    );
    define(&outer_owner, "bridge", fnv(bridge));
    drop(inner_owner);
    drop(outer_owner);
    collect_cycles();
    assert_eq!(d.count(), 5, "outer SCC + nested inner SCC all reclaimed");
}

// 11. SCCs sharing nodes reclaim.
#[test]
fn p11_sccs_sharing_nodes_reclaim() {
    let d = Drops::default();
    let shared = defs(&d);
    let a = defs(&d);
    let b = defs(&d);
    // Two cycles through the shared owner.
    define(&a, "s", fnv(closure(&shared, vec![], MFnBody::Raw(vec![]), &d)));
    define(&shared, "a", fnv(closure(&a, vec![], MFnBody::Raw(vec![]), &d)));
    define(&b, "s", fnv(closure(&shared, vec![], MFnBody::Raw(vec![]), &d)));
    define(&shared, "b", fnv(closure(&b, vec![], MFnBody::Raw(vec![]), &d)));
    drop(shared);
    drop(a);
    drop(b);
    collect_cycles();
    assert_eq!(d.count(), 7, "3 owners + 4 captures");
}

// 12 + 13. External escape keeps a cycle alive; dropping it collects.
#[test]
fn p12_p13_external_escape_controls_collection() {
    let d = Drops::default();
    let a = defs(&d);
    let f = closure(&a, vec![], MFnBody::Raw(vec![]), &d);
    define(&a, "f", fnv(f.clone()));
    let escaped = fnv(f);
    drop(a);
    collect_cycles();
    assert_eq!(d.count(), 0, "escapee holds the cycle alive");
    drop(escaped);
    collect_cycles();
    assert_eq!(d.count(), 2, "final escape drop makes it collectible");
}

// 14 + 15. Host-held values are roots; dropping the host vec collects.
#[test]
fn p14_p15_host_vec_roots() {
    let d = Drops::default();
    let a = defs(&d);
    define(&a, "f", fnv(closure(&a, vec![], MFnBody::Raw(vec![]), &d)));
    let host: Vec<MVal> = vec![a.bindings.borrow()["f"].clone()];
    drop(a);
    collect_cycles();
    assert_eq!(d.count(), 0, "host vec roots the graph — no registration API");
    drop(host);
    collect_cycles();
    assert_eq!(d.count(), 2);
}

// 16 + 17. Exactly-once drops; repeated collection idempotent.
#[test]
fn p16_p17_exactly_once_and_idempotent() {
    let d = Drops::default();
    let a = defs(&d);
    define(&a, "f", fnv(closure(&a, vec![], MFnBody::Raw(vec![]), &d)));
    drop(a);
    let s1 = collect_cycles();
    assert_eq!(s1.collected, 2);
    assert_eq!(d.count(), 2, "each object dropped exactly once");
    let s2 = collect_cycles();
    assert_eq!(s2.collected, 0, "second collection reclaims nothing");
    assert_eq!(d.count(), 2, "counters unchanged — no double drops");
}

// 18. Acyclic objects never need cycle tracing to die.
#[test]
fn p18_acyclic_never_traced_to_die() {
    let d = Drops::default();
    let chain: Vec<Cc<MAtom>> = (0..100)
        .map(|_| {
            Cc::new(MAtom {
                value: std::cell::RefCell::new(MVal::Int(0)),
                drops: d.clone(),
            })
        })
        .collect();
    drop(chain);
    assert_eq!(d.count(), 100, "all died at refcount zero, pre-collection");
}

// 19 + 20. Opaque host edges: conservative leak, never premature.
#[test]
fn p19_p20_opaque_host_edges() {
    use std::rc::Rc;
    // 19: cycle THROUGH the untraced edge leaks conservatively.
    let d = Drops::default();
    let a = defs(&d);
    let host = Rc::new(OpaqueHost {
        hidden: std::cell::RefCell::new(None),
    });
    *host.hidden.borrow_mut() = Some(fnv(closure(&a, vec![], MFnBody::Raw(vec![]), &d)));
    define(&a, "native", MVal::Native(host.clone()));
    let cleanup = host.clone(); // test-only reach-back to break the cycle later
    drop(host);
    drop(a);
    collect_cycles();
    assert_eq!(d.count(), 0, "hidden edge → conservative leak (documented)");
    // Epilogue: breaking the opaque edge makes the graph ordinarily
    // collectable again (and keeps the test leak-clean under Miri).
    *cleanup.hidden.borrow_mut() = None;
    drop(cleanup);
    collect_cycles();
    assert_eq!(d.count(), 2, "cycle collects once the hidden edge is broken");

    // 20: a cycle whose ONLY external reference lives inside an opaque
    // host is NEVER prematurely collected — the hidden handle still
    // CONTRIBUTES ITS COUNT, so trial deletion under-subtracts and the
    // node classifies as externally rooted.
    let d2 = Drops::default();
    let a2 = defs(&d2);
    let f2 = closure(&a2, vec![], MFnBody::Raw(vec![]), &d2);
    define(&a2, "f", fnv(f2.clone()));
    let live_host = Rc::new(OpaqueHost {
        hidden: std::cell::RefCell::new(Some(fnv(f2))),
    });
    drop(a2);
    collect_cycles();
    assert_eq!(d2.count(), 0, "reachable-through-host is NEVER freed");
    // Use it after collection — must be fully alive.
    {
        let held = live_host.hidden.borrow();
        let Some(MVal::Fn(alive)) = &*held else {
            panic!("value survived");
        };
        assert!(alive.captured.strong_count() >= 1);
    }
    // Quiescence epilogue.
    *live_host.hidden.borrow_mut() = None;
    drop(live_host);
    collect_cycles();
    assert_eq!(d2.count(), 2, "quiescent at test end");
}

// 21 + 22. Identity/hash on the Cc allocation pointer; map keys.
#[test]
fn p21_p22_identity_hash_map_keys() {
    use std::collections::HashMap;
    let d = Drops::default();
    let a = defs(&d);
    let f = closure(&a, vec![], MFnBody::Raw(vec![]), &d);
    let alias = f.clone();
    assert_eq!(f.id(), alias.id(), "aliases share identity");
    let g = closure(&a, vec![], MFnBody::Raw(vec![]), &d);
    assert_ne!(f.id(), g.id(), "separate allocations are distinct");

    let mut map: HashMap<usize, &'static str> = HashMap::new();
    map.insert(f.id(), "hit");
    define(&a, "f", fnv(f.clone()));
    collect_cycles(); // identity survives collection activity
    assert_eq!(map.get(&alias.id()), Some(&"hit"), "map keys stable");
    assert_eq!(f.id(), alias.id(), "identity unaffected by collection");
    // Quiescence epilogue: everything reclaims once all roots drop.
    drop(g);
    drop(f);
    drop(alias);
    drop(a);
    collect_cycles();
    assert_eq!(d.count(), 3, "full quiescence at test end");
}

// 23. Shared-DAG tracing scales with unique objects, not path counts.
#[test]
#[cfg_attr(miri, ignore)] // scale case: covered natively + on wasm; Miri runs the algorithm at small scale elsewhere
fn p23_shared_dag_scales_by_nodes() {
    let d = Drops::default();
    let a = defs(&d);
    // 2^20 paths over 21 shared layers: each layer's closure captures the
    // SAME next-layer closure twice.
    let mut layer = closure(&a, vec![], MFnBody::Raw(vec![]), &d);
    for _ in 0..20 {
        layer = closure(
            &a,
            vec![
                ("l".into(), fnv(layer.clone())),
                ("r".into(), fnv(layer.clone())),
            ],
            MFnBody::Raw(vec![]),
            &d,
        );
    }
    define(&a, "dag", fnv(layer));
    drop(a);
    let before = cc_spike::cc::trace_calls();
    collect_cycles();
    let calls = cc_spike::cc::trace_calls() - before;
    assert_eq!(d.count(), 22, "owner + 21 captures reclaimed");
    assert!(
        calls < 22 * 6,
        "trace invocations bounded by unique nodes × phases, got {calls}"
    );
}

// 24. Deep chains/SCCs: no recursive Rust stack anywhere.
#[test]
#[cfg_attr(miri, ignore)] // scale case: covered natively + on wasm; Miri runs the algorithm at small scale elsewhere
fn p24_deep_structures_stack_safe() {
    // (a) 100k-long Cc-linked chain destroyed via the flat trampoline.
    let d = Drops::default();
    let mut head = Cc::new(MAtom {
        value: std::cell::RefCell::new(MVal::Int(0)),
        drops: d.clone(),
    });
    for _ in 0..100_000 {
        head = Cc::new(MAtom {
            value: std::cell::RefCell::new(MVal::Atom(head)),
            drops: d.clone(),
        });
    }
    drop(head);
    assert_eq!(d.count(), 100_001, "flat destruction of a deep chain");

    // (b) 100k chain CLOSED into a cycle, collected iteratively.
    let d2 = Drops::default();
    let first = Cc::new(MAtom {
        value: std::cell::RefCell::new(MVal::Int(0)),
        drops: d2.clone(),
    });
    let mut cur = first.clone();
    for _ in 0..100_000 {
        cur = Cc::new(MAtom {
            value: std::cell::RefCell::new(MVal::Atom(cur)),
            drops: d2.clone(),
        });
    }
    *first.value.borrow_mut() = MVal::Atom(cur.clone()); // close the loop
    drop(first);
    drop(cur);
    collect_cycles();
    assert_eq!(d2.count(), 100_001, "iterative collection of a deep SCC");

    // (c) deeply nested PLAIN value inside ONE allocation: TRACING is
    // iterative (the collector property under proof). NOTE: dropping such
    // a value recurses through plain Rust `Drop` — a PRE-EXISTING property
    // of Val-shaped enums (production has it today), unchanged by the
    // collector — so the test dismantles the plain value iteratively
    // before letting it drop, exactly as production callers must.
    let d3 = Drops::default();
    let a3 = defs(&d3);
    let mut v = MVal::Int(0);
    for _ in 0..100_000 {
        v = MVal::List(vec![v]);
    }
    define(&a3, "deep", v);
    define(&a3, "self", fnv(closure(&a3, vec![], MFnBody::Raw(vec![]), &d3)));
    let keep = a3.clone(); // hold the graph: this collect only TRACES
    let stats = collect_cycles();
    assert_eq!(stats.collected, 0, "rooted: trace-only pass at depth 100k");
    // Iterative dismantle of the plain deep value, then reclaim the cycle.
    let deep = keep.bindings.borrow_mut().remove("deep").unwrap();
    let mut stack = vec![deep];
    while let Some(x) = stack.pop() {
        if let MVal::List(xs) = x {
            stack.extend(xs);
        }
    }
    drop(keep);
    drop(a3);
    collect_cycles();
    assert_eq!(d3.count(), 2, "cycle reclaimed after iterative dismantle");
}
