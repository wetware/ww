//! Model types structurally mirroring the production Glia graph — all edge
//! classes that defeated Graph 4 are representable: owner→closure storage,
//! closure→owner references, lexical captures (same-owner and foreign),
//! runtime values hidden in executable bodies (Const/Quote/raw args),
//! pattern map keys, atoms, capability method/base/handler payloads,
//! handler/oneshot retention, and opaque native payloads.

use crate::cc::{Cc, Trace, TraceAbort, Tracer};
use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::rc::Rc;

/// Shared drop counter for exactly-once destruction proofs.
#[derive(Clone, Default)]
pub struct Drops(pub Rc<Cell<usize>>);
impl Drops {
    pub fn count(&self) -> usize {
        self.0.get()
    }
    fn bump(&self) {
        self.0.set(self.0.get() + 1);
    }
}

pub struct MDefs {
    pub bindings: RefCell<HashMap<String, MVal>>,
    pub drops: Drops,
}
impl Drop for MDefs {
    fn drop(&mut self) {
        self.drops.bump();
    }
}

pub struct MCapturedEnv {
    /// Immutable after construction, like production `CapturedEnv`.
    pub slots: Vec<(String, MVal)>,
    pub drops: Drops,
}
impl Drop for MCapturedEnv {
    fn drop(&mut self) {
        self.drops.bump();
    }
}

pub struct MAtom {
    pub value: RefCell<MVal>,
    pub drops: Drops,
}
impl Drop for MAtom {
    fn drop(&mut self) {
        self.drops.bump();
    }
}

pub struct MCapInner {
    pub methods: RefCell<Vec<(String, MVal)>>,
    pub base: RefCell<Option<MVal>>,
    pub handler: RefCell<Option<MVal>>,
    pub drops: Drops,
}
impl Drop for MCapInner {
    fn drop(&mut self) {
        self.drops.bump();
    }
}

#[derive(Clone)]
pub struct MClosure {
    pub captured: Cc<MCapturedEnv>,
    pub owner: Cc<MDefs>,
    pub arity: MFnArity,
}
impl MClosure {
    /// Callable identity = the capture allocation pointer (stable,
    /// non-moving, independent of counts/colors).
    pub fn id(&self) -> usize {
        self.captured.ptr_id()
    }
}

#[derive(Clone)]
pub struct MFnArity {
    pub body: MFnBody,
}

#[derive(Clone)]
pub enum MFnBody {
    Raw(Vec<MVal>),
    Analyzed(Vec<MExpr>),
}

#[derive(Clone)]
pub enum MExpr {
    Const(MVal),
    Quote(MVal),
    CallRaw(Vec<MVal>),
    Sub(Vec<MExpr>),
    Match(Vec<(MPattern, MExpr)>),
}

#[derive(Clone)]
pub enum MPattern {
    Literal(MVal),
    MapKeys(Vec<(MVal, MPattern)>),
}

/// Opaque host payload: its interior may hold participating values, but it
/// is DELIBERATELY untraced — the collector form of the host trust
/// boundary. Hidden edges may retain garbage; they can never cause
/// premature collection (omission only under-subtracts).
pub struct OpaqueHost {
    pub hidden: RefCell<Option<MVal>>,
}

#[derive(Clone)]
pub enum MVal {
    Int(i64),
    /// Durable/IPFS-backed data: collector-inert opaque leaf.
    Durable(Rc<Vec<u8>>),
    List(Vec<MVal>),
    Map(Vec<(MVal, MVal)>),
    Fn(MClosure),
    Atom(Cc<MAtom>),
    Cap(Cc<MCapInner>),
    Native(Rc<OpaqueHost>),
}

/// Iterative within-object edge walk (no Rust recursion even for deeply
/// nested plain containers/expressions inside ONE allocation).
pub fn trace_mval(root: &MVal, t: &mut Tracer) -> Result<(), TraceAbort> {
    enum W<'a> {
        V(&'a MVal),
        E(&'a MExpr),
        P(&'a MPattern),
        B(&'a MFnBody),
    }
    let mut work: Vec<W<'_>> = vec![W::V(root)];
    while let Some(item) = work.pop() {
        match item {
            W::V(v) => match v {
                MVal::Int(_) | MVal::Durable(_) => {}
                // Deliberate trust boundary: never traced.
                MVal::Native(_) => {}
                MVal::List(xs) => work.extend(xs.iter().map(W::V)),
                MVal::Map(ps) => {
                    for (k, val) in ps {
                        work.push(W::V(k));
                        work.push(W::V(val));
                    }
                }
                MVal::Fn(c) => {
                    t.edge(&c.captured);
                    t.edge(&c.owner);
                    work.push(W::B(&c.arity.body));
                }
                MVal::Atom(a) => t.edge(a),
                MVal::Cap(c) => t.edge(c),
            },
            W::B(b) => match b {
                MFnBody::Raw(vs) => work.extend(vs.iter().map(W::V)),
                MFnBody::Analyzed(es) => work.extend(es.iter().map(W::E)),
            },
            W::E(e) => match e {
                MExpr::Const(v) | MExpr::Quote(v) => work.push(W::V(v)),
                MExpr::CallRaw(vs) => work.extend(vs.iter().map(W::V)),
                MExpr::Sub(es) => work.extend(es.iter().map(W::E)),
                MExpr::Match(clauses) => {
                    for (p, e) in clauses {
                        work.push(W::P(p));
                        work.push(W::E(e));
                    }
                }
            },
            W::P(p) => match p {
                MPattern::Literal(v) => work.push(W::V(v)),
                MPattern::MapKeys(ps) => {
                    for (k, sub) in ps {
                        work.push(W::V(k));
                        work.push(W::P(sub));
                    }
                }
            },
        }
    }
    Ok(())
}

// SAFETY: each impl enumerates every owned participating edge exactly once
// per call via the shared iterative walk; RefCell contents via try_borrow;
// no side effects.
unsafe impl Trace for MDefs {
    fn trace(&self, t: &mut Tracer) -> Result<(), TraceAbort> {
        let b = self.bindings.try_borrow().map_err(|_| TraceAbort)?;
        for v in b.values() {
            trace_mval(v, t)?;
        }
        Ok(())
    }
}
unsafe impl Trace for MCapturedEnv {
    fn trace(&self, t: &mut Tracer) -> Result<(), TraceAbort> {
        for (_, v) in &self.slots {
            trace_mval(v, t)?;
        }
        Ok(())
    }
}
unsafe impl Trace for MAtom {
    fn trace(&self, t: &mut Tracer) -> Result<(), TraceAbort> {
        let v = self.value.try_borrow().map_err(|_| TraceAbort)?;
        trace_mval(&v, t)
    }
}
unsafe impl Trace for MCapInner {
    fn trace(&self, t: &mut Tracer) -> Result<(), TraceAbort> {
        let m = self.methods.try_borrow().map_err(|_| TraceAbort)?;
        for (_, v) in m.iter() {
            trace_mval(v, t)?;
        }
        let b = self.base.try_borrow().map_err(|_| TraceAbort)?;
        if let Some(v) = &*b {
            trace_mval(v, t)?;
        }
        let h = self.handler.try_borrow().map_err(|_| TraceAbort)?;
        if let Some(v) = &*h {
            trace_mval(v, t)?;
        }
        Ok(())
    }
}

// ── construction helpers ──

pub fn defs(drops: &Drops) -> Cc<MDefs> {
    Cc::new(MDefs {
        bindings: RefCell::new(HashMap::new()),
        drops: drops.clone(),
    })
}

pub fn closure(owner: &Cc<MDefs>, slots: Vec<(String, MVal)>, body: MFnBody, drops: &Drops) -> MClosure {
    MClosure {
        captured: Cc::new(MCapturedEnv {
            slots,
            drops: drops.clone(),
        }),
        owner: owner.clone(),
        arity: MFnArity { body },
    }
}

pub fn define(owner: &Cc<MDefs>, name: &str, v: MVal) {
    owner.bindings.borrow_mut().insert(name.to_string(), v);
}
