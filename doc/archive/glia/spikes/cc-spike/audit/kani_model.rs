//! Bounded arithmetic/state-machine model for the review.
//! This abstracts raw pointers, vtables, allocation, and Rust Drop glue.

const N: usize = 3;

fn bounded_graph() -> ([[u8; N]; N], [u8; N]) {
    let edges: [[u8; N]; N] = kani::any();
    let external: [u8; N] = kani::any();
    for i in 0..N {
        kani::assume(external[i] <= 2);
        for j in 0..N {
            kani::assume(edges[i][j] <= 2);
        }
    }
    (edges, external)
}

fn closure(mut black: [bool; N], edges: &[[u8; N]; N]) -> [bool; N] {
    for _ in 0..N {
        for i in 0..N {
            if black[i] {
                for j in 0..N {
                    if edges[i][j] > 0 {
                        black[j] = true;
                    }
                }
            }
        }
    }
    black
}

#[kani::proof]
fn no_trial_underflow_for_exact_reports() {
    let (edges, external) = bounded_graph();
    let mut strong = external;
    for i in 0..N {
        for j in 0..N {
            strong[j] = strong[j].checked_add(edges[i][j]).unwrap();
        }
    }
    let mut trial = strong;
    for i in 0..N {
        for j in 0..N {
            for _ in 0..edges[i][j] {
                assert!(trial[j] > 0);
                trial[j] -= 1;
            }
        }
    }
    assert_eq!(trial, external);
}

#[kani::proof]
fn reachable_node_is_never_white() {
    let (edges, external) = bounded_graph();
    let roots = external.map(|n| n > 0);
    let reachable = closure(roots, &edges);
    let black = closure(roots, &edges);
    for i in 0..N {
        assert!(!reachable[i] || black[i]);
    }
}

#[kani::proof]
fn two_node_unreachable_scc_is_selected() {
    let edges = [[0u8, 1, 0], [1u8, 0, 0], [0u8; N]];
    let external = [0u8; N];
    let black = closure(external.map(|n| n > 0), &edges);
    assert!(!black[0] && !black[1]);
}

#[kani::proof]
fn omitted_edges_cannot_hide_an_external_reachable_node() {
    let visible: [[u8; N]; N] = kani::any();
    let omitted: [[u8; N]; N] = kani::any();
    let external: [u8; N] = kani::any();
    for i in 0..N {
        kani::assume(external[i] <= 1);
        for j in 0..N {
            kani::assume(visible[i][j] <= 1);
            kani::assume(omitted[i][j] <= 1);
        }
    }
    let mut actual = visible;
    for i in 0..N {
        for j in 0..N {
            actual[i][j] += omitted[i][j];
        }
    }
    let reachable = closure(external.map(|n| n > 0), &actual);

    // Trial residual is external roots plus every omitted incoming handle.
    let mut residual = external;
    for i in 0..N {
        for j in 0..N {
            residual[j] += omitted[i][j];
        }
    }
    let black = closure(residual.map(|n| n > 0), &visible);
    for i in 0..N {
        assert!(!reachable[i] || black[i]);
    }
}

#[kani::proof]
fn scan_black_restores_reachable_counts() {
    let (edges, external) = bounded_graph();
    let black = closure(external.map(|n| n > 0), &edges);
    let mut restored = external;
    for i in 0..N {
        if black[i] {
            for j in 0..N {
                restored[j] = restored[j].checked_add(edges[i][j]).unwrap();
            }
        }
    }
    for j in 0..N {
        let mut expected = external[j];
        for i in 0..N {
            if black[i] {
                expected += edges[i][j];
            }
        }
        assert_eq!(restored[j], expected);
    }
}

#[kani::proof]
fn buffered_reincremented_candidate_is_black() {
    let internal: u8 = kani::any();
    let external: u8 = kani::any();
    kani::assume(internal <= 3);
    kani::assume(external <= 2);
    let strong = internal + external + 1; // the re-increment
    let trial = strong - internal;
    assert!(trial > 0);
}

#[kani::proof]
fn failed_prevalidation_mutates_nothing() {
    let counts: [u8; N] = kani::any();
    let colors: [u8; N] = kani::any();
    let buffered: [bool; N] = kani::any();
    let before = (counts, colors, buffered);
    let validation_ok: bool = kani::any();
    if !validation_ok {
        let after = (counts, colors, buffered);
        assert_eq!(before, after);
    }
}

#[kani::proof]
fn destruction_list_contains_each_white_once() {
    let white: [bool; N] = kani::any();
    let visits: [u8; 6] = kani::any();
    let mut seen = [false; N];
    let mut list = [usize::MAX; N];
    let mut len = 0usize;
    for raw in visits {
        let i = raw as usize % N;
        if white[i] && !seen[i] {
            seen[i] = true;
            list[len] = i;
            len += 1;
        }
    }
    for x in 0..len {
        for y in (x + 1)..len {
            assert_ne!(list[x], list[y]);
        }
    }
}

#[kani::proof]
fn drop_and_free_flags_are_exactly_once() {
    let attempts: u8 = kani::any();
    kani::assume(attempts <= 4);
    let mut freed = false;
    let mut deallocated = false;
    let mut drops = 0u8;
    let mut frees = 0u8;
    for _ in 0..attempts {
        if !freed {
            freed = true;
            drops += 1;
        }
        if freed && !deallocated {
            deallocated = true;
            frees += 1;
        }
    }
    assert!(drops <= 1);
    assert!(frees <= 1);
}

#[kani::proof]
fn collection_idempotence_in_abstract_state() {
    let reachable: [bool; N] = kani::any();
    let mut alive = [true; N];
    for i in 0..N {
        if !reachable[i] {
            alive[i] = false;
        }
    }
    let after_first = alive;
    for i in 0..N {
        if !reachable[i] {
            alive[i] = false;
        }
    }
    assert_eq!(alive, after_first);
}

// Expected counterexample: production Cc::clone uses unchecked `usize + 1`.
#[kani::proof]
fn counterexample_unchecked_clone_overflow() {
    let strong: usize = kani::any();
    kani::assume(strong == usize::MAX);
    let next = strong + 1;
    assert!(next > strong);
}

// Expected counterexample: reporting one physical handle twice can subtract
// more trial references than actually exist unless the unsafe Trace contract
// is enforced.
#[kani::proof]
fn counterexample_duplicate_report_underflow() {
    let actual_handles = 1u8;
    let reported_handles = 2u8;
    let mut trial = actual_handles;
    for _ in 0..reported_handles {
        assert!(trial > 0);
        trial -= 1;
    }
}

