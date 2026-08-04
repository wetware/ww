#![cfg(not(target_family = "wasm"))]
//! Property laws (spec §13) over generated graphs, checked against a
//! reference reachability model.

use cc_spike::cc::{collect_cycles, Cc};
use cc_spike::model::*;
use proptest::prelude::*;
use std::cell::RefCell;
use std::collections::HashSet;
use std::rc::Rc;

/// Generated graph plan: `edges[i]` = children of node i; `opaque[i]` =
/// children reached through an UNTRACED opaque host (edge counts, trace
/// doesn't see it); `roots` = host-held nodes; `decorate[i]` adds a
/// defs+closure pair capturing node i's atom (participation variety).
#[derive(Clone, Debug)]
struct Plan {
    n: usize,
    edges: Vec<Vec<usize>>,
    opaque: Vec<Vec<usize>>,
    roots: Vec<usize>,
    decorate: Vec<bool>,
}

fn plan_strategy(max_n: usize, with_opaque: bool) -> impl Strategy<Value = Plan> {
    (2..max_n).prop_flat_map(move |n| {
        let edges = prop::collection::vec(prop::collection::vec(0..n, 0..4), n);
        let opaque = if with_opaque {
            prop::collection::vec(prop::collection::vec(0..n, 0..2), n).boxed()
        } else {
            Just(vec![Vec::new(); n]).boxed()
        };
        let roots = prop::collection::vec(0..n, 0..3);
        let decorate = prop::collection::vec(any::<bool>(), n);
        (Just(n), edges, opaque, roots, decorate).prop_map(|(n, edges, opaque, roots, decorate)| {
            Plan {
                n,
                edges,
                opaque,
                roots,
                decorate,
            }
        })
    })
}

struct Built {
    nodes: Vec<Cc<MAtom>>,
    drops: Vec<Drops>,
    /// Decoration objects (defs/closures/extra atoms) share these SEPARATE
    /// counters — they are independent garbage and must not pollute the
    /// node-liveness signal.
    deco_drops: Vec<Drops>,
    hosts: Vec<Rc<OpaqueHost>>, // keep opaque hosts alive (they are host-side)
}

fn build(plan: &Plan) -> Built {
    let drops: Vec<Drops> = (0..plan.n).map(|_| Drops::default()).collect();
    let deco_drops: Vec<Drops> = (0..plan.n).map(|_| Drops::default()).collect();
    let nodes: Vec<Cc<MAtom>> = (0..plan.n)
        .map(|i| {
            Cc::new(MAtom {
                value: RefCell::new(MVal::Int(0)),
                drops: drops[i].clone(),
            })
        })
        .collect();
    let mut hosts = Vec::new();
    for i in 0..plan.n {
        let mut children: Vec<MVal> = plan.edges[i]
            .iter()
            .map(|&j| MVal::Atom(nodes[j].clone()))
            .collect();
        for &j in &plan.opaque[i] {
            let host = Rc::new(OpaqueHost {
                hidden: RefCell::new(Some(MVal::Atom(nodes[j].clone()))),
            });
            hosts.push(host.clone());
            children.push(MVal::Native(host));
        }
        if plan.decorate[i] {
            let d = defs(&deco_drops[i]);
            define(
                &d,
                "self",
                MVal::Fn(closure(
                    &d,
                    vec![("cell".into(), MVal::Atom(nodes[i].clone()))],
                    MFnBody::Raw(vec![]),
                    &deco_drops[i],
                )),
            );
            children.push(MVal::Atom(Cc::new(MAtom {
                value: RefCell::new(MVal::List(vec![])),
                drops: deco_drops[i].clone(),
            })));
            // Tie a decoration closure into the node so participation
            // variety travels with the node's lifetime.
            children.push(MVal::Fn(closure(
                &d,
                vec![],
                MFnBody::Raw(vec![]),
                &deco_drops[i],
            )));
            drop(d);
        }
        *nodes[i].value.borrow_mut() = MVal::List(children);
    }
    Built {
        nodes,
        drops,
        deco_drops,
        hosts,
    }
}

/// Reference reachability over ALL edges (traced + opaque).
fn reference_reachable(plan: &Plan) -> HashSet<usize> {
    let mut seen: HashSet<usize> = HashSet::new();
    let mut stack: Vec<usize> = plan.roots.clone();
    while let Some(i) = stack.pop() {
        if seen.insert(i) {
            stack.extend(plan.edges[i].iter().copied());
            stack.extend(plan.opaque[i].iter().copied());
        }
    }
    seen
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 128, ..ProptestConfig::default() })]

    /// Laws 1+2+4: a reachable node is NEVER freed (opaque edges included);
    /// omissions cause retention only; freed nodes dropped exactly once.
    #[test]
    fn law_never_free_reachable(plan in plan_strategy(24, true)) {
        let built = build(&plan);
        let root_handles: Vec<Cc<MAtom>> =
            plan.roots.iter().map(|&i| built.nodes[i].clone()).collect();
        drop(built.nodes);
        collect_cycles();
        collect_cycles();
        let reachable = reference_reachable(&plan);
        for i in 0..plan.n {
            let c = built.drops[i].count();
            if reachable.contains(&i) {
                prop_assert_eq!(c, 0, "reachable node {} freed", i);
            }
            prop_assert!(c <= 1, "node {} dropped more than once", i);
        }
        drop(root_handles);
        drop(built.hosts);
        collect_cycles();
    }

    /// Law 8: with FULLY TRACED graphs (no opaque), collection is EXACT —
    /// freed set == reference-unreachable set.
    #[test]
    fn law_exact_on_fully_traced(plan in plan_strategy(24, false)) {
        let built = build(&plan);
        let root_handles: Vec<Cc<MAtom>> =
            plan.roots.iter().map(|&i| built.nodes[i].clone()).collect();
        drop(built.nodes);
        collect_cycles();
        let reachable = reference_reachable(&plan);
        for i in 0..plan.n {
            prop_assert_eq!(
                built.drops[i].count(),
                usize::from(!reachable.contains(&i)),
                "node {} exactness violated",
                i
            );
        }
        drop(root_handles);
        collect_cycles();
        for i in 0..plan.n {
            prop_assert_eq!(built.drops[i].count(), 1, "node {} end-state", i);
        }
    }

    /// Laws 5+6: clone/drop interleavings between collections preserve
    /// eventual liveness; collection stays idempotent.
    #[test]
    fn law_interleaving_and_idempotence(plan in plan_strategy(16, false), pulses in 0usize..32) {
        let built = build(&plan);
        let root_handles: Vec<Cc<MAtom>> =
            plan.roots.iter().map(|&i| built.nodes[i].clone()).collect();
        // Interleave clone/drop pulses with collections.
        for k in 0..pulses {
            let i = k % plan.n;
            let extra = built.nodes[i].clone();
            if k % 3 == 0 {
                collect_cycles();
            }
            drop(extra);
        }
        drop(built.nodes);
        collect_cycles();
        let s2 = collect_cycles();
        prop_assert_eq!(s2.collected, 0, "idempotent");
        let reachable = reference_reachable(&plan);
        for i in 0..plan.n {
            prop_assert_eq!(
                built.drops[i].count(),
                usize::from(!reachable.contains(&i)),
                "node {} interleaving liveness",
                i
            );
        }
        drop(root_handles);
        collect_cycles();
    }

    /// Law 7: acyclic graphs die at refcount zero without collection.
    #[test]
    fn law_acyclic_dies_without_collection(n in 2usize..24) {
        let drops: Vec<Drops> = (0..n).map(|_| Drops::default()).collect();
        // A straight chain (guaranteed acyclic).
        let mut head = Cc::new(MAtom { value: RefCell::new(MVal::Int(0)), drops: drops[0].clone() });
        for d in drops.iter().skip(1) {
            head = Cc::new(MAtom { value: RefCell::new(MVal::Atom(head)), drops: d.clone() });
        }
        drop(head);
        for (i, d) in drops.iter().enumerate() {
            prop_assert_eq!(d.count(), 1, "chain node {} needed collection", i);
        }
    }

    /// Law 9: an aborted collection (live borrow) reclaims nothing and a
    /// later collection matches the reference exactly.
    #[test]
    fn law_abort_preserves_correctness(plan in plan_strategy(12, false), victim in 0usize..12) {
        let built = build(&plan);
        let victim = victim % plan.n;
        let root_handles: Vec<Cc<MAtom>> =
            plan.roots.iter().map(|&i| built.nodes[i].clone()).collect();
        let pinned = built.nodes[victim].clone();
        drop(built.nodes);
        {
            let _borrow = pinned.value.borrow_mut();
            let before: Vec<usize> = built.drops.iter().map(Drops::count).collect();
            let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(collect_cycles));
            let after: Vec<usize> = built.drops.iter().map(Drops::count).collect();
            prop_assert_eq!(before, after, "abort reclaimed something");
        }
        drop(pinned);
        collect_cycles();
        let mut reachable = reference_reachable(&plan);
        reachable.extend(plan.roots.iter());
        for i in 0..plan.n {
            if reachable.contains(&i) {
                prop_assert_eq!(built.drops[i].count(), 0, "post-abort exactness");
            }
        }
        drop(root_handles);
        collect_cycles();
    }
}
