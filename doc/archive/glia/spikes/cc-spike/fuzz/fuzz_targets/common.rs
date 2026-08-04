use cc_spike::cc::{collect_cycles, Cc, Trace, TraceAbort, Tracer};
use std::cell::{Cell, RefCell};
use std::collections::HashSet;
use std::rc::Rc;

pub struct Node {
    visible: RefCell<Vec<Cc<Node>>>,
    opaque: RefCell<Vec<Cc<Node>>>,
    drops: Rc<Cell<usize>>,
}

unsafe impl Trace for Node {
    fn trace(&self, tracer: &mut Tracer) -> Result<(), TraceAbort> {
        let edges = self.visible.try_borrow().map_err(|_| TraceAbort)?;
        for edge in edges.iter() {
            tracer.edge(edge);
        }
        Ok(())
    }
}

impl Drop for Node {
    fn drop(&mut self) {
        self.drops.set(self.drops.get() + 1);
    }
}

fn handle<'a>(roots: &'a [Vec<Cc<Node>>], hosts: &'a [Vec<Cc<Node>>], slots: &'a [Vec<Cc<Node>>], i: usize) -> Option<&'a Cc<Node>> {
    roots[i].first().or_else(|| hosts[i].first()).or_else(|| slots[i].first())
}

fn reachable(
    alive: &[bool],
    visible: &[Vec<usize>],
    opaque: &[Vec<usize>],
    roots: &[Vec<Cc<Node>>],
    hosts: &[Vec<Cc<Node>>],
    slots: &[Vec<Cc<Node>>],
) -> HashSet<usize> {
    let mut seen = HashSet::new();
    let mut work = Vec::new();
    for i in 0..alive.len() {
        if alive[i] && (!roots[i].is_empty() || !hosts[i].is_empty() || !slots[i].is_empty()) {
            work.push(i);
        }
    }
    while let Some(i) = work.pop() {
        if !alive[i] || !seen.insert(i) {
            continue;
        }
        work.extend(visible[i].iter().copied());
        work.extend(opaque[i].iter().copied());
    }
    seen
}

pub fn run_sequence(data: &[u8], allow_opaque: bool) {
    const N: usize = 8;
    let drops: Vec<Rc<Cell<usize>>> = (0..N).map(|_| Rc::new(Cell::new(0))).collect();
    let nodes: Vec<Cc<Node>> = (0..N)
        .map(|i| Cc::new(Node {
            visible: RefCell::new(Vec::new()),
            opaque: RefCell::new(Vec::new()),
            drops: drops[i].clone(),
        }))
        .collect();
    let mut roots: Vec<Vec<Cc<Node>>> = nodes.into_iter().map(|n| vec![n]).collect();
    let mut hosts: Vec<Vec<Cc<Node>>> = (0..N).map(|_| Vec::new()).collect();
    let mut slots: Vec<Vec<Cc<Node>>> = (0..N).map(|_| Vec::new()).collect();
    let mut visible = vec![Vec::<usize>::new(); N];
    let mut opaque = vec![Vec::<usize>::new(); N];
    let mut alive = vec![true; N];

    for chunk in data.chunks(3).take(96) {
        let op = chunk[0] % 13;
        let a = chunk.get(1).copied().unwrap_or(0) as usize % N;
        let b = chunk.get(2).copied().unwrap_or(0) as usize % N;
        match op {
            0 => if let Some(h) = handle(&roots, &hosts, &slots, a).cloned() { roots[a].push(h); },
            1 => { roots[a].pop(); }
            2 => if let (Some(source), Some(target)) = (
                handle(&roots, &hosts, &slots, a).cloned(),
                handle(&roots, &hosts, &slots, b).cloned(),
            ) {
                source.visible.borrow_mut().push(target);
                visible[a].push(b);
            },
            3 => if let Some(source) = handle(&roots, &hosts, &slots, a).cloned() {
                if source.visible.borrow_mut().pop().is_some() { visible[a].pop(); }
            },
            4 => if let Some(h) = handle(&roots, &hosts, &slots, a).cloned() { hosts[a].push(h); },
            5 => { hosts[a].pop(); }
            6 => if let Some(h) = handle(&roots, &hosts, &slots, a).cloned() { slots[a].push(h); },
            7 => { slots[a].pop(); }
            8 => {
                let expected = reachable(&alive, &visible, &opaque, &roots, &hosts, &slots);
                collect_cycles();
                for i in 0..N {
                    assert!(drops[i].get() <= 1);
                    assert!(!expected.contains(&i) || drops[i].get() == 0);
                    if !allow_opaque {
                        assert_eq!(drops[i].get(), usize::from(!expected.contains(&i)));
                    }
                    if drops[i].get() == 1 {
                        alive[i] = false;
                        visible[i].clear();
                        opaque[i].clear();
                    }
                }
                if !allow_opaque {
                    let before: Vec<_> = drops.iter().map(|d| d.get()).collect();
                    assert_eq!(collect_cycles().collected, 0);
                    assert_eq!(before, drops.iter().map(|d| d.get()).collect::<Vec<_>>());
                }
            }
            9 if allow_opaque => if let (Some(source), Some(target)) = (
                handle(&roots, &hosts, &slots, a).cloned(),
                handle(&roots, &hosts, &slots, b).cloned(),
            ) {
                source.opaque.borrow_mut().push(target);
                opaque[a].push(b);
            },
            10 if allow_opaque => if let Some(source) = handle(&roots, &hosts, &slots, a).cloned() {
                if source.opaque.borrow_mut().pop().is_some() { opaque[a].pop(); }
            },
            11 => if let Some(source) = handle(&roots, &hosts, &slots, a).cloned() {
                // libFuzzer aborts the process from its panic hook, so this
                // general operation-sequence target cannot exercise the
                // collector's intentional debug panic for a failed borrow.
                // Still perturb candidate buffering here; borrow-abort
                // recovery is covered by the dedicated native regression.
                let pulse = source.clone();
                drop(pulse);
                let borrow = source.visible.borrow_mut();
                drop(borrow);
            },
            _ => if let Some(h) = handle(&roots, &hosts, &slots, a) {
                let pulse = h.clone();
                drop(pulse);
            },
        }
    }
}
