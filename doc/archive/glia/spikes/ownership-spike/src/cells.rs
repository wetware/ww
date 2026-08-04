//! SPIKE B — strongest competitor: stable binding cells + owner anchor.
//!
//! Shape: `Defs2` maps name -> Rc<BindCell>; closures capture WEAK cell
//! references for the top-level names they use, plus ONE OwnerRef anchor
//! (weak at rest in own Defs2, strong when escaped). The anchor transitively
//! keeps all the module's cells alive (Defs2 strongly owns its cells), so
//! weak cell derefs are witnessed by the anchor.
//!
//! What this tests: whether cells ELIMINATE any of Graph 4's obligations.
//! Findings (see tests): the anchor still needs the same positional
//! strength barrier — container rewriting, rest/escape, F1 scoping — i.e.
//! cells change LOOKUP mechanics (O(1) deref, per-cell versions) but do NOT
//! reduce ownership transition sites, container reconstruction, or failure
//! modes. Cells also pin one Rc allocation per name and complicate
//! redefinition-vs-remove semantics.

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::rc::{Rc, Weak};
use std::sync::atomic::{AtomicUsize, Ordering};

pub static DEFS2_DROPS: AtomicUsize = AtomicUsize::new(0);

#[derive(Clone)]
pub enum CVal {
    Int(i64),
    List(Vec<CVal>),
    Fn(CFn),
}

#[derive(Clone)]
pub struct CFn {
    pub code: Rc<CCode>,
    /// The single positional-strength field — same barrier as Graph 4.
    pub anchor: CAnchor,
}

pub struct CCode {
    pub id: usize,
    /// Captured top-level names resolve through WEAK cells (stable
    /// locations, Lua-upvalue / Clojure-Var style).
    pub cells: Vec<(String, Weak<BindCell>)>,
}

#[derive(Clone)]
pub enum CAnchor {
    Strong(Rc<Defs2>),
    Weak(Weak<Defs2>),
}

pub struct BindCell {
    pub value: RefCell<CVal>,
    pub version: Cell<u64>,
}

pub struct Defs2 {
    pub cells: RefCell<HashMap<String, Rc<BindCell>>>,
}

impl Drop for Defs2 {
    fn drop(&mut self) {
        DEFS2_DROPS.fetch_add(1, Ordering::Relaxed);
    }
}

#[derive(Debug, PartialEq)]
pub enum CellFault {
    DeadCell,
    DeadAnchor,
}

impl Defs2 {
    pub fn new() -> Rc<Defs2> {
        Rc::new(Defs2 {
            cells: RefCell::new(HashMap::new()),
        })
    }

    /// Intern-or-get a cell (forward references for recursion).
    pub fn cell(self: &Rc<Self>, name: &str) -> Rc<BindCell> {
        self.cells
            .borrow_mut()
            .entry(name.to_string())
            .or_insert_with(|| {
                Rc::new(BindCell {
                    value: RefCell::new(CVal::Int(0)),
                    version: Cell::new(0),
                })
            })
            .clone()
    }

    /// define = write the cell + bump version; the STORED value must still
    /// be rested (anchor weakened) — same barrier as Graph 4.
    pub fn define(self: &Rc<Self>, name: &str, v: CVal) {
        let rested = rest_anchor(self, &v);
        let cell = self.cell(name);
        *cell.value.borrow_mut() = rested;
        cell.version.set(cell.version.get() + 1);
    }

    /// lookup = cell deref + escape (same barrier), but name->cell hashing
    /// happens once at capture; closures deref cells directly afterwards.
    pub fn lookup(self: &Rc<Self>, name: &str) -> Option<CVal> {
        let cell = self.cells.borrow().get(name)?.clone();
        let v = cell.value.borrow().clone();
        Some(escape_anchor(self, &v))
    }
}

/// Positional strength barrier — IDENTICAL SHAPE to Graph 4's transforms.
pub fn rest_anchor(owner: &Rc<Defs2>, v: &CVal) -> CVal {
    match v {
        CVal::Int(_) => v.clone(),
        CVal::List(xs) => CVal::List(xs.iter().map(|x| rest_anchor(owner, x)).collect()),
        CVal::Fn(f) => CVal::Fn(CFn {
            code: f.code.clone(),
            anchor: match &f.anchor {
                CAnchor::Strong(o) if Rc::ptr_eq(o, owner) => CAnchor::Weak(Rc::downgrade(o)),
                other => other.clone(),
            },
        }),
    }
}

pub fn escape_anchor(owner: &Rc<Defs2>, v: &CVal) -> CVal {
    match v {
        CVal::Int(_) => v.clone(),
        CVal::List(xs) => CVal::List(xs.iter().map(|x| escape_anchor(owner, x)).collect()),
        CVal::Fn(f) => CVal::Fn(CFn {
            code: f.code.clone(),
            anchor: match &f.anchor {
                CAnchor::Weak(w) if Weak::as_ptr(w) == Rc::as_ptr(owner) => {
                    CAnchor::Strong(owner.clone())
                }
                other => other.clone(),
            },
        }),
    }
}

static NEXT_ID2: AtomicUsize = AtomicUsize::new(1);

/// Closure creation: capture weak cells for referenced top-level names +
/// strong anchor to the defining owner.
pub fn make_fn2(owner: &Rc<Defs2>, names: &[&str]) -> CFn {
    let cells = names
        .iter()
        .map(|n| ((*n).to_string(), Rc::downgrade(&owner.cell(n))))
        .collect();
    CFn {
        code: Rc::new(CCode {
            id: NEXT_ID2.fetch_add(1, Ordering::Relaxed),
            cells,
        }),
        anchor: CAnchor::Strong(owner.clone()),
    }
}

/// Call-time resolution: witness the anchor, then O(1) cell derefs —
/// no name hashing, no owner-chain walk.
pub fn resolve_at_call(f: &CFn, name: &str) -> Result<CVal, CellFault> {
    let _witness: Rc<Defs2> = match &f.anchor {
        CAnchor::Strong(o) => o.clone(),
        CAnchor::Weak(w) => w.upgrade().ok_or(CellFault::DeadAnchor)?,
    };
    let cell = f
        .code
        .cells
        .iter()
        .find(|(n, _)| n == name)
        .and_then(|(_, w)| w.upgrade())
        .ok_or(CellFault::DeadCell)?;
    let v = cell.value.borrow().clone();
    Ok(v)
}
