//! THROWAWAY SPIKE — amended Graph 4 ownership model (PR-1b.0 preflight).
//! Minimal analogues only: no parser, analyzer, evaluator, effects, or Glia
//! syntax. Delete freely.
//!
//! Model under test:
//! - `Defs` owns top-level definitions; `inherited` chains to a frozen prelude.
//! - Callables (`Fn`/`Macro`) and evaluator-owned caps carry a per-VALUE
//!   `OwnerRef { Strong | Weak }`.
//! - `rest_for(owner, v)`: copies v with every owner ref THAT POINTS AT
//!   `owner` (ptr-eq) downgraded to Weak; containers rebuilt; stops at
//!   Atom and Cap inners. Foreign owners untouched (rule F1).
//! - `escape_with(owner, v)`: symmetric upgrade using `owner` as the live
//!   witness (never bare Weak::upgrade). A weak ref to a DIFFERENT owner at
//!   an escape boundary is an invariant violation -> explicit fault.
//! - CapturedEnv holds lexical captures only; self-owned nested values are
//!   rested at capture; `for_call` escapes them with the callable's witness.

pub mod cells; // Spike B — binding-cells competitor

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::rc::{Rc, Weak};
use std::sync::atomic::{AtomicUsize, Ordering};

/// Global drop counter for `Defs` (test probe).
pub static DEFS_DROPS: AtomicUsize = AtomicUsize::new(0);
/// Node-visit counter for the last transform (bench probe).
pub static NODES_VISITED: AtomicUsize = AtomicUsize::new(0);

// ---------------------------------------------------------------------------
// Values
// ---------------------------------------------------------------------------

#[derive(Clone, Debug)]
pub enum ToyVal {
    Int(i64),
    Str(String),
    List(Vec<ToyVal>),
    /// Assoc-list map (stands in for ValMap; keys may be any ToyVal).
    MapV(Vec<(ToyVal, ToyVal)>),
    SetV(Vec<ToyVal>),
    Fn(FnValue),
    Macro(FnValue),
    Cap(CapValue),
    Atom(Rc<RefCell<ToyVal>>),
}

/// Identity bearer: shared code + captured lexical env (stands in for the
/// production `Rc<CapturedEnv>` identity). Strength conversion NEVER clones
/// this Rc's contents — only the outer FnValue wrapper.
#[derive(Debug)]
pub struct FnCode {
    pub id: usize,
    pub captured: RefCell<CapturedEnv>,
}

#[derive(Default, Debug)]
pub struct CapturedEnv {
    /// Lexical captures only. Self-owned nested callables are RESTED here.
    pub slots: Vec<(String, ToyVal)>,
}

#[derive(Clone, Debug)]
pub struct FnValue {
    pub code: Rc<FnCode>,
    pub owner: OwnerRef,
}

impl FnValue {
    /// Language identity = captured-env pointer (production: Rc::ptr_eq(env)).
    pub fn same_identity(&self, other: &FnValue) -> bool {
        Rc::ptr_eq(&self.code, &other.code)
    }
    pub fn identity_hash(&self) -> usize {
        Rc::as_ptr(&self.code) as usize
    }
}

/// Evaluator-owned capability: sealed inner (opaque to transforms) plus a
/// per-VALUE owner ref (Sol required change #4).
#[derive(Clone, Debug)]
pub struct CapValue {
    pub inner: Rc<CapInner>,
    pub owner: Option<OwnerRef>,
}

#[derive(Debug)]
pub struct CapInner {
    /// Sealed at construction with RESTED method values.
    pub methods: Vec<(String, ToyVal)>,
}

#[derive(Clone, Debug)]
pub enum OwnerRef {
    Strong(Rc<Defs>),
    Weak(Weak<Defs>),
}

impl OwnerRef {
    pub fn is_strong(&self) -> bool {
        matches!(self, OwnerRef::Strong(_))
    }
}

// ---------------------------------------------------------------------------
// Defs
// ---------------------------------------------------------------------------

#[derive(Debug)]
pub struct Binding {
    pub value: ToyVal,
    pub has_resting_owner_refs: bool,
}

#[derive(Debug)]
pub struct Defs {
    pub bindings: RefCell<HashMap<String, Binding>>,
    pub inherited: Option<Rc<Defs>>,
    pub frozen: Cell<bool>,
}

impl Drop for Defs {
    fn drop(&mut self) {
        DEFS_DROPS.fetch_add(1, Ordering::Relaxed);
    }
}

#[derive(Debug, PartialEq)]
pub enum OwnershipFault {
    UnmatchedWeak,
    FrozenMutation,
}

impl Defs {
    pub fn new(inherited: Option<Rc<Defs>>) -> Rc<Defs> {
        Rc::new(Defs {
            bindings: RefCell::new(HashMap::new()),
            inherited,
            frozen: Cell::new(false),
        })
    }

    pub fn freeze(&self) {
        self.frozen.set(true);
    }

    /// Define/redefine (last write wins). Stores a RESTED copy.
    pub fn define(self: &Rc<Self>, name: &str, value: ToyVal) -> Result<(), OwnershipFault> {
        self.define_ref(name, &value)
    }

    /// Borrow-based define: caller retains (and tears down) the original.
    /// Needed for adversarially deep values, whose recursive `Drop` would
    /// otherwise overflow inside this call — a property of any recursive
    /// value type, independent of the (iterative) transforms.
    pub fn define_ref(self: &Rc<Self>, name: &str, value: &ToyVal) -> Result<(), OwnershipFault> {
        if self.frozen.get() {
            return Err(OwnershipFault::FrozenMutation);
        }
        let (rested, has_resting) = rest_for(self, value);
        self.bindings.borrow_mut().insert(
            name.to_string(),
            Binding {
                value: rested,
                has_resting_owner_refs: has_resting,
            },
        );
        Ok(())
    }

    /// Chain lookup; upgrades resting refs against the owning Defs.
    pub fn lookup(self: &Rc<Self>, name: &str) -> Result<Option<ToyVal>, OwnershipFault> {
        if let Some(b) = self.bindings.borrow().get(name) {
            let v = if b.has_resting_owner_refs {
                escape_with(self, &b.value)?
            } else {
                b.value.clone()
            };
            return Ok(Some(v));
        }
        match &self.inherited {
            Some(parent) => parent.lookup(name),
            None => Ok(None),
        }
    }

    /// Own bindings only, escaped — the export snapshot.
    pub fn local_bindings(self: &Rc<Self>) -> Result<Vec<(String, ToyVal)>, OwnershipFault> {
        let mut out = Vec::new();
        for (k, b) in self.bindings.borrow().iter() {
            let v = if b.has_resting_owner_refs {
                escape_with(self, &b.value)?
            } else {
                b.value.clone()
            };
            out.push((k.clone(), v));
        }
        out.sort_by(|(a, _), (b, _)| a.cmp(b));
        Ok(out)
    }
}

// ---------------------------------------------------------------------------
// Constructors (the "escaped strong" entry points)
// ---------------------------------------------------------------------------

static NEXT_ID: AtomicUsize = AtomicUsize::new(1);

/// Fresh callable: Strong(current owner); captured self-owned values RESTED.
pub fn make_fn(owner: &Rc<Defs>, captured: Vec<(String, ToyVal)>) -> FnValue {
    let rested: Vec<(String, ToyVal)> = captured
        .into_iter()
        .map(|(k, v)| {
            let (r, _) = rest_for(owner, &v);
            (k, r)
        })
        .collect();
    FnValue {
        code: Rc::new(FnCode {
            id: NEXT_ID.fetch_add(1, Ordering::Relaxed),
            captured: RefCell::new(CapturedEnv { slots: rested }),
        }),
        owner: OwnerRef::Strong(owner.clone()),
    }
}

/// `for_call`: escape captured values with the callable's own witness.
/// Fails explicitly if the callable's owner ref is an unmatched weak.
pub fn for_call(f: &FnValue) -> Result<Vec<(String, ToyVal)>, OwnershipFault> {
    let witness = match &f.owner {
        OwnerRef::Strong(o) => o.clone(),
        OwnerRef::Weak(w) => w.upgrade().ok_or(OwnershipFault::UnmatchedWeak)?,
    };
    f.code
        .captured
        .borrow()
        .slots
        .iter()
        .map(|(k, v)| Ok((k.clone(), escape_with(&witness, v)?)))
        .collect()
}

/// Fresh evaluator-owned cap: methods rested against the owner; the outer
/// cap carries the strong witness (Sol #4).
pub fn make_owned_cap(owner: &Rc<Defs>, methods: Vec<(String, ToyVal)>) -> CapValue {
    let rested = methods
        .into_iter()
        .map(|(k, v)| {
            let (r, _) = rest_for(owner, &v);
            (k, r)
        })
        .collect();
    CapValue {
        inner: Rc::new(CapInner { methods: rested }),
        owner: Some(OwnerRef::Strong(owner.clone())),
    }
}

/// Cap method dispatch: escape the method with the cap's witness.
pub fn cap_dispatch(cap: &CapValue, method: &str) -> Result<Option<ToyVal>, OwnershipFault> {
    let witness = match &cap.owner {
        Some(OwnerRef::Strong(o)) => o.clone(),
        Some(OwnerRef::Weak(w)) => w.upgrade().ok_or(OwnershipFault::UnmatchedWeak)?,
        None => {
            // Owner-free cap: methods must be owner-free too.
            return Ok(cap
                .inner
                .methods
                .iter()
                .find(|(k, _)| k == method)
                .map(|(_, v)| v.clone()));
        }
    };
    match cap.inner.methods.iter().find(|(k, _)| k == method) {
        Some((_, v)) => Ok(Some(escape_with(&witness, v)?)),
        None => Ok(None),
    }
}

/// Attenuation wrapper: fresh cap sharing the base's methods, TRANSFERRING
/// the owner witness (Sol #5).
pub fn attenuate(base: &CapValue, allow: &[&str]) -> CapValue {
    let methods = base
        .inner
        .methods
        .iter()
        .filter(|(k, _)| allow.contains(&k.as_str()))
        .cloned()
        .collect();
    CapValue {
        inner: Rc::new(CapInner { methods }),
        owner: base.owner.clone(),
    }
}

// ---------------------------------------------------------------------------
// Iterative transforms (the centralized barrier)
// ---------------------------------------------------------------------------

enum Mode {
    Rest,
    Escape,
}

/// Copy `v` with self-owned callable/cap owner refs downgraded to weak.
/// Returns (copy, any_resting_ref_present).
pub fn rest_for(owner: &Rc<Defs>, v: &ToyVal) -> (ToyVal, bool) {
    let flag = Cell::new(false);
    let out = transform(owner, v, Mode::Rest, &flag).expect("rest is infallible");
    (out, flag.get())
}

/// Copy `v` with weak refs to `owner` upgraded via the live witness.
/// A weak ref to any OTHER owner is an invariant violation.
pub fn escape_with(owner: &Rc<Defs>, v: &ToyVal) -> Result<ToyVal, OwnershipFault> {
    let flag = Cell::new(false);
    transform(owner, v, Mode::Escape, &flag)
}

/// Iterative post-order rebuild — no Rust recursion (Sol #7 / P1-1).
fn transform(
    owner: &Rc<Defs>,
    root: &ToyVal,
    mode: Mode,
    resting_flag: &Cell<bool>,
) -> Result<ToyVal, OwnershipFault> {
    enum Task<'a> {
        Enter(&'a ToyVal),
        BuildList(usize),
        BuildMap(usize),
        BuildSet(usize),
    }
    let mut work: Vec<Task> = vec![Task::Enter(root)];
    let mut out: Vec<ToyVal> = Vec::new();

    let convert = |r: &OwnerRef| -> Result<OwnerRef, OwnershipFault> {
        match (&mode, r) {
            (Mode::Rest, OwnerRef::Strong(o)) if Rc::ptr_eq(o, owner) => {
                resting_flag.set(true);
                Ok(OwnerRef::Weak(Rc::downgrade(o)))
            }
            (Mode::Rest, OwnerRef::Weak(w)) if Weak::as_ptr(w) == Rc::as_ptr(owner) => {
                // Already resting (idempotent).
                resting_flag.set(true);
                Ok(r.clone())
            }
            (Mode::Escape, OwnerRef::Weak(w)) => {
                if Weak::as_ptr(w) == Rc::as_ptr(owner) {
                    // Witness-based upgrade: never bare Weak::upgrade.
                    Ok(OwnerRef::Strong(owner.clone()))
                } else {
                    // Weak ref to a different owner at an escape boundary:
                    // invariant violation -> explicit fault.
                    Err(OwnershipFault::UnmatchedWeak)
                }
            }
            _ => Ok(r.clone()), // foreign strong / non-matching: untouched (F1)
        }
    };

    while let Some(task) = work.pop() {
        match task {
            Task::Enter(v) => {
                NODES_VISITED.fetch_add(1, Ordering::Relaxed);
                match v {
                    ToyVal::Int(_) | ToyVal::Str(_) => out.push(v.clone()),
                    // Opaque stops: shared identity, never rewritten.
                    ToyVal::Atom(a) => out.push(ToyVal::Atom(a.clone())),
                    ToyVal::Fn(f) => out.push(ToyVal::Fn(FnValue {
                        code: f.code.clone(),
                        owner: convert(&f.owner)?,
                    })),
                    ToyVal::Macro(f) => out.push(ToyVal::Macro(FnValue {
                        code: f.code.clone(),
                        owner: convert(&f.owner)?,
                    })),
                    ToyVal::Cap(c) => out.push(ToyVal::Cap(CapValue {
                        inner: c.inner.clone(), // sealed; not traversed
                        owner: match &c.owner {
                            Some(r) => Some(convert(r)?),
                            None => None,
                        },
                    })),
                    ToyVal::List(xs) => {
                        work.push(Task::BuildList(xs.len()));
                        for x in xs.iter().rev() {
                            work.push(Task::Enter(x));
                        }
                    }
                    ToyVal::SetV(xs) => {
                        work.push(Task::BuildSet(xs.len()));
                        for x in xs.iter().rev() {
                            work.push(Task::Enter(x));
                        }
                    }
                    ToyVal::MapV(ps) => {
                        work.push(Task::BuildMap(ps.len()));
                        for (k, val) in ps.iter().rev() {
                            work.push(Task::Enter(val));
                            work.push(Task::Enter(k)); // keys transformed too
                        }
                    }
                }
            }
            Task::BuildList(n) => {
                let items = out.split_off(out.len() - n);
                out.push(ToyVal::List(items));
            }
            Task::BuildSet(n) => {
                let items = out.split_off(out.len() - n);
                out.push(ToyVal::SetV(items));
            }
            Task::BuildMap(n) => {
                let flat = out.split_off(out.len() - 2 * n);
                let mut pairs = Vec::with_capacity(n);
                let mut it = flat.into_iter();
                while let (Some(k), Some(v)) = (it.next(), it.next()) {
                    pairs.push((k, v));
                }
                out.push(ToyVal::MapV(pairs));
            }
        }
    }
    debug_assert_eq!(out.len(), 1);
    Ok(out.pop().unwrap())
}

// ---------------------------------------------------------------------------
// Helpers for tests/benches
// ---------------------------------------------------------------------------

/// Count strength states of callables/caps in a value (diagnostics).
pub fn count_refs(v: &ToyVal) -> (usize, usize) {
    let mut strong = 0;
    let mut weak = 0;
    let mut stack = vec![v];
    while let Some(v) = stack.pop() {
        match v {
            ToyVal::Fn(f) | ToyVal::Macro(f) => match f.owner {
                OwnerRef::Strong(_) => strong += 1,
                OwnerRef::Weak(_) => weak += 1,
            },
            ToyVal::Cap(c) => match &c.owner {
                Some(OwnerRef::Strong(_)) => strong += 1,
                Some(OwnerRef::Weak(_)) => weak += 1,
                None => {}
            },
            ToyVal::List(xs) | ToyVal::SetV(xs) => stack.extend(xs.iter()),
            ToyVal::MapV(ps) => {
                for (k, val) in ps {
                    stack.push(k);
                    stack.push(val);
                }
            }
            _ => {}
        }
    }
    (strong, weak)
}
pub mod crossowner;
