//! Review-only operation-sequence differential model.

use cc_spike::cc::{collect_cycles, Cc, Trace, TraceAbort, Tracer};
use proptest::prelude::*;
use std::cell::{Cell, RefCell};
use std::collections::HashSet;
use std::rc::Rc;

struct Node {
    visible: RefCell<Vec<Cc<Node>>>,
    opaque: RefCell<Vec<Cc<Node>>>,
    drops: Rc<Cell<usize>>,
}

unsafe impl Trace for Node {
    fn trace(&self, tracer: &mut Tracer) -> Result<(), TraceAbort> {
        let visible = self.visible.try_borrow().map_err(|_| TraceAbort)?;
        for edge in visible.iter() {
            tracer.edge(edge);
        }
        // `opaque` deliberately models the documented host boundary.
        Ok(())
    }
}

impl Drop for Node {
    fn drop(&mut self) {
        self.drops.set(self.drops.get() + 1);
    }
}

fn handle<'a>(roots: &'a [Vec<Cc<Node>>], hosts: &'a [Vec<Cc<Node>>], futures: &'a [Vec<Cc<Node>>], i: usize) -> Option<&'a Cc<Node>> {
    roots[i]
        .first()
        .or_else(|| hosts[i].first())
        .or_else(|| futures[i].first())
}

fn reachable(
    alive: &[bool],
    visible: &[Vec<usize>],
    opaque: &[Vec<usize>],
    roots: &[Vec<Cc<Node>>],
    hosts: &[Vec<Cc<Node>>],
    futures: &[Vec<Cc<Node>>],
) -> HashSet<usize> {
    let mut seen = HashSet::new();
    let mut stack = Vec::new();
    for i in 0..alive.len() {
        if alive[i] && (!roots[i].is_empty() || !hosts[i].is_empty() || !futures[i].is_empty()) {
            stack.push(i);
        }
    }
    while let Some(i) = stack.pop() {
        if !alive[i] || !seen.insert(i) {
            continue;
        }
        stack.extend(visible[i].iter().copied());
        stack.extend(opaque[i].iter().copied());
    }
    seen
}

fn run_sequence(bytes: &[u8], allow_opaque: bool) -> Result<(), TestCaseError> {
    const N: usize = 6;
    let drops: Vec<Rc<Cell<usize>>> = (0..N).map(|_| Rc::new(Cell::new(0))).collect();
    let initial: Vec<Cc<Node>> = (0..N)
        .map(|i| {
            Cc::new(Node {
                visible: RefCell::new(Vec::new()),
                opaque: RefCell::new(Vec::new()),
                drops: drops[i].clone(),
            })
        })
        .collect();
    let mut roots: Vec<Vec<Cc<Node>>> = initial.into_iter().map(|n| vec![n]).collect();
    let mut hosts: Vec<Vec<Cc<Node>>> = (0..N).map(|_| Vec::new()).collect();
    let mut futures: Vec<Vec<Cc<Node>>> = (0..N).map(|_| Vec::new()).collect();
    let mut visible = vec![Vec::<usize>::new(); N];
    let mut opaque = vec![Vec::<usize>::new(); N];
    let mut alive = vec![true; N];

    for chunk in bytes.chunks(3) {
        let op = chunk[0] % 12;
        let a = chunk.get(1).copied().unwrap_or(0) as usize % N;
        let b = chunk.get(2).copied().unwrap_or(0) as usize % N;
        match op {
            0 => {
                if let Some(h) = handle(&roots, &hosts, &futures, a).cloned() {
                    roots[a].push(h);
                }
            }
            1 => {
                roots[a].pop();
            }
            2 => {
                if let (Some(source), Some(target)) = (
                    handle(&roots, &hosts, &futures, a).cloned(),
                    handle(&roots, &hosts, &futures, b).cloned(),
                ) {
                    source.visible.borrow_mut().push(target);
                    visible[a].push(b);
                }
            }
            3 => {
                if let Some(source) = handle(&roots, &hosts, &futures, a).cloned() {
                    if source.visible.borrow_mut().pop().is_some() {
                        visible[a].pop();
                    }
                }
            }
            4 => {
                if let Some(h) = handle(&roots, &hosts, &futures, a).cloned() {
                    hosts[a].push(h);
                }
            }
            5 => {
                hosts[a].pop();
            }
            6 => {
                if let Some(h) = handle(&roots, &hosts, &futures, a).cloned() {
                    futures[a].push(h);
                }
            }
            7 => {
                futures[a].pop();
            }
            8 => {
                let expected = reachable(&alive, &visible, &opaque, &roots, &hosts, &futures);
                collect_cycles();
                for i in 0..N {
                    prop_assert!(drops[i].get() <= 1, "node {} dropped more than once", i);
                    prop_assert!(!expected.contains(&i) || drops[i].get() == 0, "reachable node {} was collected", i);
                    if !allow_opaque {
                        prop_assert_eq!(drops[i].get(), usize::from(!expected.contains(&i)), "fully traced live set differs at node {}", i);
                    }
                    if drops[i].get() == 1 {
                        alive[i] = false;
                        visible[i].clear();
                        opaque[i].clear();
                    }
                }
                let before: Vec<usize> = drops.iter().map(|d| d.get()).collect();
                let second = collect_cycles();
                let after: Vec<usize> = drops.iter().map(|d| d.get()).collect();
                if !allow_opaque {
                    prop_assert_eq!(before, after, "fully traced collection was not idempotent");
                    prop_assert_eq!(second.collected, 0, "second fully traced collection selected new garbage without a mutation");
                } else {
                    // Dropping an unreachable source can remove an omitted
                    // edge and expose a second conservative candidate wave.
                    // That may require another collection, but neither wave
                    // may reclaim a node reachable in the reference graph.
                    for i in 0..N {
                        prop_assert!(!expected.contains(&i) || drops[i].get() == 0, "reachable node {} was collected by a conservative follow-up", i);
                    }
                }
            }
            9 if allow_opaque => {
                if let (Some(source), Some(target)) = (
                    handle(&roots, &hosts, &futures, a).cloned(),
                    handle(&roots, &hosts, &futures, b).cloned(),
                ) {
                    source.opaque.borrow_mut().push(target);
                    opaque[a].push(b);
                }
            }
            10 if allow_opaque => {
                if let Some(source) = handle(&roots, &hosts, &futures, a).cloned() {
                    if source.opaque.borrow_mut().pop().is_some() {
                        opaque[a].pop();
                    }
                }
            }
            _ => {
                if let Some(h) = handle(&roots, &hosts, &futures, a) {
                    let pulse = h.clone();
                    drop(pulse);
                }
            }
        }
    }

    Ok(())
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 4096, max_shrink_iters: 20_000, ..ProptestConfig::default() })]

    #[test]
    fn operation_sequences_match_reference(bytes in prop::collection::vec(any::<u8>(), 1..192)) {
        run_sequence(&bytes, false)?;
    }

    #[test]
    fn opaque_operation_sequences_never_free_reachable(bytes in prop::collection::vec(any::<u8>(), 1..192)) {
        run_sequence(&bytes, true)?;
    }
}
