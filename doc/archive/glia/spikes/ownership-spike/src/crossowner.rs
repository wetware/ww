//! Sol Review 2 reconciliation model: the two P1 ownership graphs under
//! (a) SHALLOW positional resting — amended Graph 4 exactly as reviewed
//!     (callable = leaf; only its own outer owner reference rewritten), and
//! (b) DEEP copy-on-write resting with a SPLIT IDENTITY TOKEN (candidate B):
//!     the barrier recurses into captured slots and executable-payload
//!     values with F1 ptr-eq scoping; rewrites produce NEW capture/body Rcs
//!     (copy-on-write) while a dedicated identity token is never replaced.
//!
//! Self-contained on purpose: the reviewed Stage-C spike stays untouched.

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::{Rc, Weak};

/// Minimal definition owner.
#[derive(Default)]
pub struct XOwner {
    pub bindings: RefCell<HashMap<String, (XVal, bool)>>,
}

impl XOwner {
    pub fn new() -> Rc<XOwner> {
        Rc::new(XOwner::default())
    }
}

#[derive(Clone)]
pub enum XOwnerRef {
    Strong(Rc<XOwner>),
    Weak(Weak<XOwner>),
}

impl XOwnerRef {
    fn rested(&self, owner: &Rc<XOwner>) -> XOwnerRef {
        match self {
            XOwnerRef::Strong(o) if Rc::ptr_eq(o, owner) => XOwnerRef::Weak(Rc::downgrade(o)),
            other => other.clone(),
        }
    }
    fn escaped(&self, witness: &Rc<XOwner>) -> Result<XOwnerRef, XFault> {
        match self {
            XOwnerRef::Weak(w) if Weak::as_ptr(w) == Rc::as_ptr(witness) => {
                Ok(XOwnerRef::Strong(Rc::clone(witness)))
            }
            XOwnerRef::Weak(_) => Err(XFault::UnmatchedWitness),
            XOwnerRef::Strong(o) => Ok(XOwnerRef::Strong(Rc::clone(o))),
        }
    }
    fn is_resting_for(&self, owner: &Rc<XOwner>) -> bool {
        matches!(self, XOwnerRef::Weak(w) if Weak::as_ptr(w) == Rc::as_ptr(owner))
    }
}

#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub enum XFault {
    UnmatchedWitness,
}

#[derive(Clone)]
pub enum XVal {
    Int(i64),
    List(Vec<XVal>),
    Fn(XFn),
}

/// Candidate-B callable shape: identity token split from the rewritable
/// capture/body payloads.
#[derive(Clone)]
pub struct XFn {
    /// IDENTITY ANCHOR: never replaced by any transform. Aliases share it;
    /// separately constructed callables get fresh tokens.
    pub ident: Rc<()>,
    /// Lexical capture — copy-on-write under the deep model.
    pub captured: Rc<Vec<(String, XVal)>>,
    /// Executable-payload constants (models macro-injected runtime values
    /// inside FnBody/Expr) — copy-on-write under the deep model.
    pub body: Rc<Vec<XVal>>,
    pub owner: XOwnerRef,
}

pub fn make_fn(owner: &Rc<XOwner>, captured: Vec<(String, XVal)>, body: Vec<XVal>) -> XFn {
    // Construction-time normalization (both models): self-owned interior
    // values rest immediately (deep, CoW-free since freshly built).
    let captured = captured
        .into_iter()
        .map(|(k, v)| (k, deep_rest(owner, &v).0))
        .collect();
    let body = body.into_iter().map(|v| deep_rest(owner, &v).0).collect();
    XFn {
        ident: Rc::new(()),
        captured: Rc::new(captured),
        body: Rc::new(body),
        owner: XOwnerRef::Strong(Rc::clone(owner)),
    }
}

// ───────────────────────── SHALLOW (as reviewed) ─────────────────────────

/// Amended Graph 4 exactly as Stage C shipped: the callable is a leaf; only
/// its own outer owner reference is rewritten. Captured slots and body
/// payloads are NOT traversed.
pub fn shallow_rest(owner: &Rc<XOwner>, v: &XVal) -> (XVal, bool) {
    match v {
        XVal::Int(_) => (v.clone(), false),
        XVal::List(xs) => {
            let mut resting = false;
            let items = xs
                .iter()
                .map(|x| {
                    let (x, r) = shallow_rest(owner, x);
                    resting |= r;
                    x
                })
                .collect();
            (XVal::List(items), resting)
        }
        XVal::Fn(f) => {
            let rested = f.owner.rested(owner);
            let resting = rested.is_resting_for(owner);
            (
                XVal::Fn(XFn {
                    ident: Rc::clone(&f.ident),
                    captured: Rc::clone(&f.captured), // leaf: shared, untouched
                    body: Rc::clone(&f.body),         // leaf: shared, untouched
                    owner: rested,
                }),
                resting,
            )
        }
    }
}

pub fn define_shallow(owner: &Rc<XOwner>, name: &str, v: XVal) {
    let (v, resting) = shallow_rest(owner, &v);
    owner
        .bindings
        .borrow_mut()
        .insert(name.to_string(), (v, resting));
}

// ─────────────────── DEEP copy-on-write (candidate B) ────────────────────

/// Candidate B: the barrier recurses into captured slots and body payloads
/// with the SAME F1 ptr-eq scoping (only refs to the storing owner weaken;
/// every foreign owner stays strong). Rewrites are copy-on-write: a new
/// capture/body Rc only when something inside changed; the identity token
/// is always preserved. Returns (value, subtree-has-resting, changed).
pub fn deep_rest(owner: &Rc<XOwner>, v: &XVal) -> (XVal, bool) {
    let (v, resting, _changed) = deep_rest_inner(owner, v);
    (v, resting)
}

fn deep_rest_inner(owner: &Rc<XOwner>, v: &XVal) -> (XVal, bool, bool) {
    match v {
        XVal::Int(_) => (v.clone(), false, false),
        XVal::List(xs) => {
            let mut resting = false;
            let mut changed = false;
            let items = xs
                .iter()
                .map(|x| {
                    let (x, r, c) = deep_rest_inner(owner, x);
                    resting |= r;
                    changed |= c;
                    x
                })
                .collect();
            (XVal::List(items), resting, changed)
        }
        XVal::Fn(f) => {
            let rested_owner = f.owner.rested(owner);
            let owner_resting = rested_owner.is_resting_for(owner);
            let owner_changed = owner_resting && !f.owner.is_resting_for(owner);

            let mut interior_resting = false;
            let mut cap_changed = false;
            let new_captured: Vec<(String, XVal)> = f
                .captured
                .iter()
                .map(|(k, x)| {
                    let (x, r, c) = deep_rest_inner(owner, x);
                    interior_resting |= r;
                    cap_changed |= c;
                    (k.clone(), x)
                })
                .collect();
            let mut body_changed = false;
            let new_body: Vec<XVal> = f
                .body
                .iter()
                .map(|x| {
                    let (x, r, c) = deep_rest_inner(owner, x);
                    interior_resting |= r;
                    body_changed |= c;
                    x
                })
                .collect();

            let captured = if cap_changed {
                Rc::new(new_captured) // copy-on-write
            } else {
                Rc::clone(&f.captured) // untouched: aliases unaffected
            };
            let body = if body_changed {
                Rc::new(new_body)
            } else {
                Rc::clone(&f.body)
            };
            (
                XVal::Fn(XFn {
                    ident: Rc::clone(&f.ident), // identity NEVER replaced
                    captured,
                    body,
                    owner: rested_owner,
                }),
                owner_resting || interior_resting,
                owner_changed || cap_changed || body_changed,
            )
        }
    }
}

pub fn deep_escape(witness: &Rc<XOwner>, v: &XVal) -> Result<XVal, XFault> {
    Ok(deep_escape_inner(witness, v)?.0)
}

fn deep_escape_inner(witness: &Rc<XOwner>, v: &XVal) -> Result<(XVal, bool), XFault> {
    match v {
        XVal::Int(_) => Ok((v.clone(), false)),
        XVal::List(xs) => {
            let mut changed = false;
            let mut items = Vec::with_capacity(xs.len());
            for x in xs {
                let (x, c) = deep_escape_inner(witness, x)?;
                changed |= c;
                items.push(x);
            }
            Ok((XVal::List(items), changed))
        }
        XVal::Fn(f) => {
            let was_resting = f.owner.is_resting_for(witness);
            let escaped_owner = f.owner.escaped(witness)?;
            let mut cap_changed = false;
            let mut new_captured = Vec::with_capacity(f.captured.len());
            for (k, x) in f.captured.iter() {
                let (x, c) = deep_escape_inner(witness, x)?;
                cap_changed |= c;
                new_captured.push((k.clone(), x));
            }
            let mut body_changed = false;
            let mut new_body = Vec::with_capacity(f.body.len());
            for x in f.body.iter() {
                let (x, c) = deep_escape_inner(witness, x)?;
                body_changed |= c;
                new_body.push(x);
            }
            let captured = if cap_changed {
                Rc::new(new_captured)
            } else {
                Rc::clone(&f.captured)
            };
            let body = if body_changed {
                Rc::new(new_body)
            } else {
                Rc::clone(&f.body)
            };
            Ok((
                XVal::Fn(XFn {
                    ident: Rc::clone(&f.ident),
                    captured,
                    body,
                    owner: escaped_owner,
                }),
                was_resting || cap_changed || body_changed,
            ))
        }
    }
}

pub fn define_deep(owner: &Rc<XOwner>, name: &str, v: XVal) {
    let (v, resting) = deep_rest(owner, &v);
    owner
        .bindings
        .borrow_mut()
        .insert(name.to_string(), (v, resting));
}

pub fn lookup_deep(owner: &Rc<XOwner>, name: &str) -> Result<Option<XVal>, XFault> {
    let entry = {
        let b = owner.bindings.borrow();
        b.get(name).cloned()
    };
    match entry {
        None => Ok(None),
        Some((v, false)) => Ok(Some(v)),
        Some((v, true)) => Ok(Some(deep_escape(owner, &v)?)),
    }
}

/// Activation (both models): witness from the callable's owner; captured
/// slots escaped through it.
pub fn activate(f: &XFn) -> Result<Vec<(String, XVal)>, XFault> {
    let witness = match &f.owner {
        XOwnerRef::Strong(o) => Rc::clone(o),
        XOwnerRef::Weak(_) => return Err(XFault::UnmatchedWitness),
    };
    let mut out = Vec::with_capacity(f.captured.len());
    for (k, v) in f.captured.iter() {
        out.push((k.clone(), deep_escape(&witness, v)?));
    }
    Ok(out)
}
