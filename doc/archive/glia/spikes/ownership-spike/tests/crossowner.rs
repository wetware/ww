//! Sol Review 2 reconciliation proofs: both P1 graphs under the reviewed
//! SHALLOW model (leak demonstrated) and the candidate-B DEEP CoW model
//! (reclamation + identity preservation), plus the irreducible
//! multi-owner-mutual class and metrics.

use ownership_spike::crossowner::*;
use std::rc::Rc;

fn probe(o: &Rc<XOwner>) -> std::rc::Weak<XOwner> {
    Rc::downgrade(o)
}

/// Build the P1-A graph: B-owned factory closure f whose capture holds the
/// A-owned callable g; A stores f. Returns (f-as-stored-input, g).
fn p1a_graph(a: &Rc<XOwner>, b: &Rc<XOwner>) -> (XVal, XFn) {
    let g = make_fn(a, vec![], vec![]);
    // B's factory output: fn() -> g()  — g arrives ESCAPED (strong A).
    let f = make_fn(b, vec![("g".into(), XVal::Fn(g.clone()))], vec![]);
    (XVal::Fn(f), g)
}

// ── P1-A cross-owner factory back-edge ──

#[test]
fn p1a_shallow_leaks_both_owners() {
    let a = XOwner::new();
    let b = XOwner::new();
    let (pa, pb) = (probe(&a), probe(&b));
    let (f, g) = p1a_graph(&a, &b);
    define_shallow(&a, "f", f);
    drop(g);
    drop(a);
    drop(b);
    assert!(pa.upgrade().is_some(), "SHALLOW: A leaks (Sol P1-A)");
    assert!(pb.upgrade().is_some(), "SHALLOW: B leaks (Sol P1-A)");
}

#[test]
fn p1a_deep_reclaims_both_owners() {
    let a = XOwner::new();
    let b = XOwner::new();
    let (pa, pb) = (probe(&a), probe(&b));
    let (f, g) = p1a_graph(&a, &b);
    define_deep(&a, "f", f);

    // Lifecycle: stored f's capture now rests g for A; f's outer owner
    // stays Strong(B) (foreign, preserved). Invocation still works via the
    // lookup-escape → activation path.
    let looked = lookup_deep(&a, "f").unwrap().expect("f resolves");
    let XVal::Fn(lf) = &looked else { panic!("fn") };
    let slots = activate(lf).expect("activation escapes");
    assert_eq!(slots.len(), 1, "capture intact");

    drop(slots);
    drop(looked);
    drop(g);
    drop(a);
    assert!(
        pa.upgrade().is_none(),
        "DEEP: A reclaims once its env drops (self-cycle closed)"
    );
    // B was held only through A's stored f — freed with A.
    drop(b);
    assert!(pb.upgrade().is_none(), "DEEP: B reclaims after A");
}

// ── P1-B body-hidden owner-bearing value ──

#[test]
fn p1b_shallow_leaks_owner() {
    let a = XOwner::new();
    let pa = probe(&a);
    let g = make_fn(&a, vec![], vec![]);
    // Macro-injected: f's BODY embeds the live g (escaped, Strong(A)).
    let f = XVal::Fn(XFn {
        // simulate a macro-produced callable constructed under A whose
        // body payload embeds g — bypassing construction normalization,
        // exactly like Expr::Const embedding in production.
        ident: Rc::new(()),
        captured: Rc::new(vec![]),
        body: Rc::new(vec![XVal::Fn(g.clone())]),
        owner: XOwnerRef::Strong(Rc::clone(&a)),
    });
    define_shallow(&a, "f", f);
    drop(g);
    drop(a);
    assert!(pa.upgrade().is_some(), "SHALLOW: body-hidden edge leaks A");
}

#[test]
fn p1b_deep_reclaims_owner() {
    let a = XOwner::new();
    let pa = probe(&a);
    let g = make_fn(&a, vec![], vec![]);
    let f = XVal::Fn(XFn {
        ident: Rc::new(()),
        captured: Rc::new(vec![]),
        body: Rc::new(vec![XVal::Fn(g.clone())]),
        owner: XOwnerRef::Strong(Rc::clone(&a)),
    });
    define_deep(&a, "f", f);
    // Escaped copy restores the body value through the witness.
    let looked = lookup_deep(&a, "f").unwrap().expect("f resolves");
    let XVal::Fn(lf) = &looked else { panic!("fn") };
    assert!(
        matches!(&lf.body[0], XVal::Fn(bg) if matches!(&bg.owner, XOwnerRef::Strong(o) if Rc::ptr_eq(o, &a))),
        "escaped body value is strong again"
    );
    drop(looked);
    drop(g);
    drop(a);
    assert!(pa.upgrade().is_none(), "DEEP: body edge rested → A reclaims");
}

// ── Identity/alias preservation under CoW ──

#[test]
fn identity_and_aliases_preserved_through_cow() {
    let a = XOwner::new();
    let b = XOwner::new();
    let (f, _g) = p1a_graph(&a, &b);
    let XVal::Fn(original) = &f else { panic!() };
    let alias = original.clone(); // escaped alias held elsewhere

    define_deep(&a, "f", f.clone());
    let looked = lookup_deep(&a, "f").unwrap().unwrap();
    let XVal::Fn(lf) = &looked else { panic!() };

    // Identity token: alias == stored/looked-up copy.
    assert!(Rc::ptr_eq(&alias.ident, &lf.ident), "identity preserved");
    // CoW: the looked-up copy's capture Rc DIFFERS from the alias's
    // (rewritten at store), while the alias's own capture is untouched.
    assert!(
        !Rc::ptr_eq(&alias.captured, &lf.captured),
        "storage rewrote a fresh capture (copy-on-write)"
    );
    assert!(
        matches!(&alias.captured[0].1, XVal::Fn(g) if matches!(&g.owner, XOwnerRef::Strong(_))),
        "alias's capture still strong — aliases unaffected"
    );
}

// ── The irreducible class: genuine mutual multi-owner cycle ──

#[test]
fn mutual_multi_owner_cycle_is_positionally_irreducible() {
    let a = XOwner::new();
    let b = XOwner::new();
    let (pa, pb) = (probe(&a), probe(&b));

    // A-owned callable, B-owned callable — each stored in the OTHER owner.
    // Every owner edge is FOREIGN; F1 (and escapee semantics) forbid
    // weakening any of them. Deep resting changes nothing.
    let fa = make_fn(&a, vec![], vec![]);
    let fb = make_fn(&b, vec![], vec![]);
    define_deep(&a, "their", XVal::Fn(fb));
    define_deep(&b, "their", XVal::Fn(fa));
    drop(a);
    drop(b);
    assert!(
        pa.upgrade().is_some() && pb.upgrade().is_some(),
        "mutual cross-owner storage cycles are NOT closable by any \
         positional rule — this is the named residual class"
    );
}

// ── Metrics: transitions and invalid-state check ──

#[test]
fn cow_transition_counts_and_no_invalid_states() {
    let a = XOwner::new();
    let b = XOwner::new();
    let (f, _g) = p1a_graph(&a, &b);

    // Store: exactly one capture rewrite (the interior g rests).
    let XVal::Fn(before) = &f else { panic!() };
    let cap_before = Rc::clone(&before.captured);
    define_deep(&a, "f", f);
    let stored = {
        let bindings = a.bindings.borrow();
        let (v, resting) = bindings.get("f").cloned().unwrap();
        assert!(resting, "stored entry flagged");
        v
    };
    let XVal::Fn(sf) = &stored else { panic!() };
    assert!(!Rc::ptr_eq(&cap_before, &sf.captured), "one CoW at store");
    // Stored state: outer owner strong-foreign(B); interior g weak(A) —
    // the only weak position is inside owner-storage, as required.
    assert!(matches!(&sf.owner, XOwnerRef::Strong(o) if Rc::ptr_eq(o, &b)));
    assert!(
        matches!(&sf.captured[0].1, XVal::Fn(g) if g_resting_for(g, &a)),
        "interior g rests for A in storage"
    );

    // Lookup: exactly one CoW back (escape).
    let looked = lookup_deep(&a, "f").unwrap().unwrap();
    let XVal::Fn(lf) = &looked else { panic!() };
    assert!(!Rc::ptr_eq(&sf.captured, &lf.captured), "one CoW at lookup");
    assert!(
        matches!(&lf.captured[0].1, XVal::Fn(g) if matches!(&g.owner, XOwnerRef::Strong(o) if Rc::ptr_eq(o, &a))),
        "escaped interior fully strong — no weak value ever escapes"
    );
}

fn g_resting_for(g: &XFn, owner: &Rc<XOwner>) -> bool {
    matches!(&g.owner, XOwnerRef::Weak(w) if std::rc::Weak::as_ptr(w) == Rc::as_ptr(owner))
}
