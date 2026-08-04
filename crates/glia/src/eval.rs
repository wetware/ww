//! Evaluator for Glia expressions.
//!
//! Resolution order for list forms:
//! 1. Special forms (`def`, `if`, `do`, `let`, `fn`, `quote`, `defmacro`) — unevaluated args
//! 2. Macro expansion — if head resolves to `Val::Macro`, expand with raw args then re-eval
//! 3. Env lookup — if head resolves to `Val::Fn`, invoke the closure
//! 4. Built-in functions (`+`, `list`, `cons`, `apply`, etc.) — eval args, call builtin
//! 5. Generic dispatch — eval args, delegate to [`Dispatch`]
//!
//! Non-list values are self-evaluating (returned as-is), except symbols
//! which are looked up in [`Env`] (unbound symbols pass through).
//!
//! Capability dispatch (host, executor, ipfs, etc.) is provided by the
//! caller via the [`Dispatch`] trait — the evaluator itself is host-agnostic.

use core::future::Future;
use core::pin::Pin;
use core::sync::atomic::{AtomicU64, Ordering};
use core::task::Poll;
use std::cell::{Cell, RefCell};
use std::collections::{BTreeSet, HashMap};
use std::rc::Rc;

use crate::effect::{self, HandlerStack};
use crate::error;
use crate::expr::FnBody;
use crate::{
    make_cap, oneshot, AttenuatedCapInner, Fault, FnArity, GliaCapInner, HandledCapInner,
    NativeSignal, Val, ValMap,
};

/// Monotonic counter for `gensym`.
static GENSYM_COUNTER: AtomicU64 = AtomicU64::new(0);

// ---------------------------------------------------------------------------
// Env — lexical scope chain
// ---------------------------------------------------------------------------

/// A lexical environment: a stack of frames where each frame maps names to values.
///
/// Lookup walks from the innermost (last) frame outward.  `push_frame` /
/// `pop_frame` create and destroy child scopes (used by future `let` / `fn`
/// special forms).
///
/// The `handler_stack` is dynamic scope for the effect system.
/// Closures and macros use the caller's handler stack at invocation time,
/// not the handler stack captured at definition time.
#[derive(Debug, Clone)]
pub struct Env {
    frames: Vec<Frame>,
    handler_stack: HandlerStack,
    /// Whether the outermost frame came from lexical closure capture.
    ///
    /// The embedding's root frame is ambient process context and should not
    /// trigger the transitional cell warning. A closure snapshot, however,
    /// contains bindings that the old `cell` behavior would have captured.
    root_frame_is_lexical: bool,
    /// Persistent definition owner for this environment. The `def` family
    /// writes here (never to lexical frames); lookup falls through to it
    /// after the frames, giving top-level names late binding.
    defs: Rc<Defs>,
    /// Top-level definition privilege.
    ///
    /// An env-level invariant set at construction and never toggled: `true`
    /// only for embedder/REPL/module roots (`Env::new`), `false` for every
    /// call-derived env (`Env::for_call`) and lexical capture. Enforced by
    /// [`Env::define`].
    defining: bool,
}

impl Default for Env {
    /// Default creates an Env with one root frame (same as `Env::new()`).
    fn default() -> Self {
        Self::new()
    }
}

type Frame = std::collections::HashMap<String, Val>;

impl Env {
    /// Create a new, empty environment with a single root frame.
    pub fn new() -> Self {
        Self {
            frames: vec![Frame::new()],
            handler_stack: effect::new_handler_stack(),
            root_frame_is_lexical: false,
            defs: Defs::new(None),
            defining: true,
        }
    }

    /// Resolve a name: lexical frames innermost-outward, then the
    /// persistent definition owner and its inherited chain.
    ///
    /// Top-level names are LATE-BOUND: every resolution consults the live
    /// `Defs` state, so redefinition is visible to existing closures and
    /// named/mutual recursion resolve naturally.
    /// Errors are RELEASE-CHECKED internal faults (the uncatchable
    /// evaluator lane — never a panic, never a guest exception): an
    /// ownership-invariant breach during definition-owner resolution is a
    /// runtime bug to surface, not a condition to mask.
    pub fn get(&self, name: &str) -> Result<Option<Val>, Box<Fault>> {
        if let Some(v) = self.get_lexical(name) {
            return Ok(Some(v.clone()));
        }
        self.defs
            .lookup(name)
            .map_err(|f| own_invariant_fault("lookup", name, f))
    }

    /// Lexical-frames-only lookup (no definition-owner fallthrough).
    ///
    /// Used by closure capture: free variables that resolve to persistent
    /// definitions must NOT be snapshotted — they stay late-bound through
    /// the owner.
    fn get_lexical(&self, name: &str) -> Option<&Val> {
        for frame in self.frames.iter().rev() {
            if let Some(v) = frame.get(name) {
                return Some(v);
            }
        }
        None
    }

    /// Bind `name` to `val` in the innermost (current) frame.
    pub fn set(&mut self, name: String, val: Val) {
        if let Some(frame) = self.frames.last_mut() {
            frame.insert(name, val);
        }
    }

    /// Push a new empty child frame (enters a new scope).
    pub fn push_frame(&mut self) {
        self.frames.push(Frame::new());
    }

    /// Pop the innermost frame (exits a scope).  The root frame cannot be popped.
    pub fn pop_frame(&mut self) {
        if self.frames.len() > 1 {
            self.frames.pop();
        }
    }

    /// Bind `name` to `val` in the root (outermost) frame.
    /// Used by `def` — definitions are always global, like Clojure's `def`.
    pub fn set_root(&mut self, name: String, val: Val) {
        if let Some(frame) = self.frames.first_mut() {
            frame.insert(name, val);
        }
    }

    /// Capability-valued bindings introduced by non-root lexical scopes.
    ///
    /// Used only by the transitional `with`/`let` migration warning. Root
    /// capabilities are intentionally excluded so a legitimate top-level
    /// zero-grant `cell` does not warn merely because the embedding has caps.
    fn scoped_cap_names(&self) -> Vec<String> {
        let mut seen = std::collections::HashSet::new();
        let mut caps = Vec::new();
        let first_scoped_frame = usize::from(!self.root_frame_is_lexical);
        for frame in self.frames.iter().skip(first_scoped_frame).rev() {
            for (name, val) in frame {
                if seen.insert(name.clone()) && matches!(val, Val::Cap(_)) {
                    caps.push(name.clone());
                }
            }
        }
        caps.sort();
        caps
    }

    /// Collect all visible LEXICAL bindings (inner overrides outer) as
    /// `(name, val)` pairs. Persistent definitions are NOT included.
    ///
    /// NOTE: superseded by [`Env::local_bindings`] for module exports —
    /// a module exports its own persistent definitions, not its lexical
    /// frames. Retained for embedder introspection of ambient context.
    #[must_use]
    pub fn bindings(&self) -> Vec<(String, Val)> {
        let mut merged = Frame::new();
        for frame in &self.frames {
            for (k, v) in frame {
                merged.insert(k.clone(), v.clone());
            }
        }
        let mut bindings: Vec<(String, Val)> = merged.into_iter().collect();
        bindings.sort_by(|(left, _), (right, _)| left.cmp(right));
        bindings
    }

    /// This environment's OWN persistent definitions, sorted by name.
    ///
    /// The module-export primitive: inherited (prelude) names and lexical
    /// frame bindings are excluded — a module exports exactly what it
    /// defined. Prefer this over [`Env::bindings`] for exports.
    /// Errors are release-checked internal faults (see [`Env::get`]).
    pub fn local_bindings(&self) -> Result<Vec<(String, Val)>, Box<Fault>> {
        self.defs
            .local_bindings()
            .map_err(|f| own_invariant_fault("export", "<module>", f))
    }

    /// The persistent definition owner (crate-internal plumbing).
    pub(crate) fn defs(&self) -> &Rc<Defs> {
        &self.defs
    }

    /// Replace this environment's definition owner with a fresh child of
    /// `parent`. Used by prelude loading: the env's subsequent definitions
    /// go to the child while `parent`'s names stay visible via inherited
    /// lookup.
    pub(crate) fn adopt_inherited_defs(&mut self, parent: Rc<Defs>) {
        self.defs = Defs::new(Some(parent));
    }

    /// The single checked definition operation. All def-family paths
    /// converge here (raw/analyzed `def` and `defmacro`, `defn` via macro
    /// expansion, `defcap`'s final binding, REPL/module top level).
    ///
    /// Order: (1) top-level privilege — `Err(DefineError::NotTopLevel)`,
    /// caller throws catchable `glia.error/def-not-top-level`, no mutation;
    /// (2) frozen owner — internal fault, no mutation; (3) storage through
    /// `Defs` (last-write-wins, version bump).
    pub(crate) fn define(&self, name: String, val: Val) -> Result<(), DefineError> {
        if !self.defining {
            return Err(DefineError::NotTopLevel);
        }
        self.defs.define(name, val).map_err(DefineError::Own)
    }

    /// Create a new Env for callable activation.
    ///
    /// The call env's root frame is the closure's lexical capture with any
    /// same-owner slots ESCAPED through the callable's live owner witness
    /// (flag-gated: pure captures copy directly). Late binding flows from
    /// `defs` being the callable's DEFINING owner; activation never carries
    /// definition privilege; the handler stack is the CALLER's (dynamic
    /// scope).
    ///
    /// A callable whose owner reference is not live at activation is a
    /// release-checked internal fault: every legitimate invocation path
    /// receives values that escaped through lookup/args, which restore the
    /// strong owner reference.
    pub(crate) fn for_call(
        closure: &crate::Closure,
        caller_hs: &HandlerStack,
    ) -> Result<Self, Box<Fault>> {
        let witness = match &closure.owner {
            OwnerRef::Strong(o) => Rc::clone(o),
            OwnerRef::Weak(_) => {
                return Err(own_invariant_fault(
                    "activation",
                    "<callable>",
                    own::OwnFault::UnmatchedWeak,
                ))
            }
        };
        let root: Frame = if closure.captured.has_resting {
            let mut escaped = Frame::with_capacity(closure.captured.slots.len());
            for (k, v) in &closure.captured.slots {
                let v = own::escape_with(&witness, v)
                    .map_err(|f| own_invariant_fault("activation", k, f))?;
                escaped.insert(k.clone(), v);
            }
            escaped
        } else {
            closure.captured.slots.clone()
        };
        Ok(Self {
            frames: vec![root, Frame::new()], // capture + param frame
            handler_stack: caller_hs.clone(),
            root_frame_is_lexical: true,
            defs: witness,
            defining: false,
        })
    }
}

// ---------------------------------------------------------------------------
// Defs — persistent definition ownership (PR-1b.0)
// ---------------------------------------------------------------------------

/// A persistent definition owner: where `def`-family bindings belong.
///
/// Separates "where do persistent names live?" (this type) from "what locals
/// are in scope?" (`Env`'s lexical frames). Top-level names are late-bound
/// through the owner, which gives named/mutual recursion and REPL
/// redefinition their semantics. Modules get a fresh `Defs` inheriting the
/// shared frozen prelude; exports enumerate only the local bindings.
///
/// Stage A: structurally present but semantically inert (definitions still
/// go to the root lexical frame). Stage B activates it.
pub struct Defs {
    /// Local persistent bindings. Stored values are normalized by the
    /// crate-private ownership barrier (see `own`); everything read back out
    /// through [`Defs::lookup`] is ordinary, fully-usable values.
    bindings: RefCell<HashMap<String, Binding>>,
    /// Inherited owner chain (the shared frozen prelude for modules).
    inherited: Option<Rc<Defs>>,
    /// Frozen owners reject definition; the prelude freezes after load.
    frozen: Cell<bool>,
    /// Bumped on every definition; authority analysis (Stage E) uses it to
    /// invalidate any cached view of live definition state.
    version: Cell<u64>,
}

/// Failure modes of the checked definition operation ([`Env::define`]).
pub(crate) enum DefineError {
    /// No top-level definition privilege: the def site throws the catchable
    /// `glia.error/def-not-top-level` exception. No mutation occurred.
    NotTopLevel,
    /// Ownership-layer failure (frozen owner; invariant breach): surfaced
    /// as an internal fault. No mutation occurred.
    Own(own::OwnFault),
}

/// Map a crate-private ownership failure onto the public uncatchable fault
/// lane with NEUTRAL vocabulary (no guest- or embedder-visible
/// weak/strong/resting terms; "definition owner" is language semantics).
#[cold]
#[inline(never)]
fn own_invariant_fault(op: &str, name: &str, f: own::OwnFault) -> Box<Fault> {
    let detail = match f {
        own::OwnFault::UnmatchedWeak => "definition-owner witness mismatch",
        own::OwnFault::FrozenMutation => "mutation of a frozen definition owner",
    };
    Box::new(Fault::runtime(error::internal(
        op,
        format!("{detail} at '{name}'"),
    )))
}

/// Resolve a name through [`Env::get`], surfacing ownership faults on the
/// evaluator's uncatchable `Control::Fault` lane. The resolution-site form
/// of the release-checked plumbing: never a panic, never a catchable throw.
fn resolve(env: &Env, name: &str) -> Result<Option<Val>, Control> {
    env.get(name).map_err(Control::Fault)
}

/// One persistent binding plus its barrier metadata.
struct Binding {
    value: Val,
    /// Fast-path summary: whether `value` holds any barrier-normalized
    /// self-references. `false` lets lookup skip the deep transform.
    has_resting_owner_refs: bool,
}

impl std::fmt::Debug for Defs {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Defs")
            .field("bindings", &self.bindings.borrow().len())
            .field("inherited", &self.inherited.is_some())
            .field("frozen", &self.frozen.get())
            .finish()
    }
}

#[allow(dead_code)] // wired into definition/lookup paths in Stage B
impl Defs {
    /// Create a fresh owner, optionally inheriting an existing chain.
    pub(crate) fn new(inherited: Option<Rc<Defs>>) -> Rc<Defs> {
        Rc::new(Defs {
            bindings: RefCell::new(HashMap::new()),
            inherited,
            frozen: Cell::new(false),
            version: Cell::new(0),
        })
    }

    /// Intern a persistent definition. Fails on frozen owners.
    pub(crate) fn define(self: &Rc<Self>, name: String, value: Val) -> Result<(), own::OwnFault> {
        if self.frozen.get() {
            return Err(own::OwnFault::FrozenMutation);
        }
        let (value, has_resting_owner_refs) = own::rest_for(self, &value);
        self.bindings.borrow_mut().insert(
            name,
            Binding {
                value,
                has_resting_owner_refs,
            },
        );
        self.version.set(self.version.get() + 1);
        Ok(())
    }

    /// Resolve a name through this owner and its inherited chain.
    pub(crate) fn lookup(self: &Rc<Self>, name: &str) -> Result<Option<Val>, own::OwnFault> {
        let local = {
            let bindings = self.bindings.borrow();
            bindings
                .get(name)
                .map(|b| (b.value.clone(), b.has_resting_owner_refs))
        };
        if let Some((value, needs_escape)) = local {
            let value = if needs_escape {
                own::escape_with(self, &value)?
            } else {
                value
            };
            return Ok(Some(value));
        }
        match &self.inherited {
            Some(parent) => parent.lookup(name),
            None => Ok(None),
        }
    }

    /// Enumerate this owner's LOCAL bindings (inherited names excluded) as
    /// fully-usable values. The module-export primitive.
    pub(crate) fn local_bindings(self: &Rc<Self>) -> Result<Vec<(String, Val)>, own::OwnFault> {
        let entries: Vec<(String, Val, bool)> = {
            let bindings = self.bindings.borrow();
            bindings
                .iter()
                .map(|(k, b)| (k.clone(), b.value.clone(), b.has_resting_owner_refs))
                .collect()
        };
        let mut out = Vec::with_capacity(entries.len());
        for (name, value, needs_escape) in entries {
            let value = if needs_escape {
                own::escape_with(self, &value)?
            } else {
                value
            };
            out.push((name, value));
        }
        out.sort_by(|(left, _), (right, _)| left.cmp(right));
        Ok(out)
    }

    /// Permanently reject further definition (prelude, closed modules).
    pub(crate) fn freeze(&self) {
        self.frozen.set(true);
    }

    pub(crate) fn is_frozen(&self) -> bool {
        self.frozen.get()
    }

    /// Definition-state version for live authority analysis (Stage E).
    pub(crate) fn version(&self) -> u64 {
        self.version.get()
    }
}

/// Lexical-only closure capture.
///
/// Closures snapshot their free lexical values here and carry their
/// definition owner separately (inside [`crate::Closure`]), which is what
/// breaks the routine `Defs → fn → captured Env → Defs` cycle. Contains
/// lexical values ONLY — never a `Defs`, never a handler stack. Same-owner
/// executable values in the slots are normalized (rested) at capture time;
/// [`Env::for_call`] restores them at activation via the callable's owner
/// witness.
pub(crate) struct CapturedEnv {
    slots: Frame,
    /// Fast-path summary: whether any slot holds barrier-normalized
    /// self-references (RC-mechanism metadata; skips the activation
    /// transform when false).
    has_resting: bool,
}

impl CapturedEnv {
    /// Full-frame capture (raw pipeline; the old `snapshot` semantics):
    /// merge every lexical frame, inner shadowing outer, then normalize
    /// same-owner executable slots for storage inside the closure.
    pub(crate) fn capture_all(env: &Env) -> Self {
        let mut merged = Frame::new();
        for frame in &env.frames {
            for (k, v) in frame {
                merged.insert(k.clone(), v.clone());
            }
        }
        let (slots, has_resting) = own::rest_frame_for(&env.defs, &merged);
        CapturedEnv { slots, has_resting }
    }

    /// Slim capture (analyzed pipeline; the old `capture_closure`
    /// semantics): every frames-resident macro PLUS the closure's free
    /// lexical variables. Persistent definitions are NOT copied — they
    /// stay late-bound through the owner. See the macro-scan rationale on
    /// the call sites: eval-time expansion must find frames-resident
    /// macros that free-variable analysis alone would drop.
    pub(crate) fn capture_free(env: &Env, free_vars: BTreeSet<&String>) -> Self {
        let mut captured = Frame::new();
        // Oldest → newest so inner scopes shadow outer ones, matching
        // lexical lookup.
        for frame in &env.frames {
            for (k, v) in frame {
                if matches!(v, Val::Macro { .. }) {
                    captured.insert(k.clone(), v.clone());
                }
            }
        }
        for name in free_vars {
            if let Some(value) = env.get_lexical(name) {
                captured.insert(name.clone(), value.clone());
            }
        }
        let (slots, has_resting) = own::rest_frame_for(&env.defs, &captured);
        CapturedEnv { slots, has_resting }
    }

    #[cfg(test)]
    pub(crate) fn from_slots(slots: Frame) -> Self {
        CapturedEnv {
            slots,
            has_resting: false,
        }
    }

    #[cfg(test)]
    pub(crate) fn get(&self, name: &str) -> Option<&Val> {
        self.slots.get(name)
    }

    #[cfg(test)]
    pub(crate) fn len(&self) -> usize {
        self.slots.len()
    }
}

pub(crate) use own::OwnerRef;

/// Crate-private RC ownership barrier (amended Graph 4).
///
/// JURISDICTION (normative): this machinery governs only local executable
/// value graphs. Its transforms recurse ONLY into the four transparent
/// containers (`List`, `Vector`, `Map`, `Set`); every other `Val` variant is
/// a barrier-inert leaf — all durable data (`Nil`..`Bytes`), atoms (opaque
/// interior), native/host payloads, and any future durable handle. It must
/// never traverse the transitive contents of durable or lazily-backed data.
///
/// AUTHORITY (normative): pointer strength has no authority meaning. A dead
/// weak owner at an escape boundary is an internal fault — never revocation,
/// never attenuation, never guest-visible semantics.
///
/// The five ownership choke points (definition storage; lookup/export
/// enumeration; capture + call activation; capability sealing/dispatch/
/// attenuation; module export construction) are the ONLY call sites. Nothing
/// outside `eval` can name these helpers; `OwnerRef` construction happens
/// only here.
mod own {
    use super::{Defs, Frame};
    use crate::{Val, ValMap};
    use std::rc::{Rc, Weak};

    /// Private owner reference carried by callables (Stage C) and
    /// evaluator-owned caps (Stage D). RC-specific; deleted wholesale under
    /// any future GC (see the recorded deletion inventory).
    #[derive(Clone)]
    pub(crate) enum OwnerRef {
        Strong(Rc<Defs>),
        Weak(Weak<Defs>),
    }

    /// Deliberately opaque: ownership state never appears in any output,
    /// including internal debug formatting.
    impl std::fmt::Debug for OwnerRef {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            write!(f, "OwnerRef(..)")
        }
    }

    #[allow(dead_code)] // wired into the leaf hooks in Stages C/D
    impl OwnerRef {
        /// Positional normalization for storage inside `owner`'s own
        /// subtree: self-references release their keep-alive; foreign
        /// owners are untouched.
        pub(super) fn rested(&self, owner: &Rc<Defs>) -> OwnerRef {
            match self {
                OwnerRef::Strong(o) if Rc::ptr_eq(o, owner) => OwnerRef::Weak(Rc::downgrade(o)),
                other => other.clone(),
            }
        }

        /// Witness-based restoration on escape. Never a bare upgrade: the
        /// caller must hold the live owner `Rc` (the witness); a resting
        /// reference that does not match it is an internal fault.
        pub(super) fn escaped_with(&self, witness: &Rc<Defs>) -> Result<OwnerRef, OwnFault> {
            match self {
                OwnerRef::Weak(w) if Weak::as_ptr(w) == Rc::as_ptr(witness) => {
                    Ok(OwnerRef::Strong(Rc::clone(witness)))
                }
                OwnerRef::Weak(_) => Err(OwnFault::UnmatchedWeak),
                OwnerRef::Strong(o) => Ok(OwnerRef::Strong(Rc::clone(o))),
            }
        }

        pub(super) fn is_resting_for(&self, owner: &Rc<Defs>) -> bool {
            matches!(self, OwnerRef::Weak(w) if Weak::as_ptr(w) == Rc::as_ptr(owner))
        }
    }

    /// Internal ownership faults. Surfaced at the choke points (Stage B+)
    /// through the evaluator's Fault channel — never guest-catchable, never
    /// authority-meaningful.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub(crate) enum OwnFault {
        /// A resting reference met an escape boundary without its owner's
        /// witness. Invariant breach: report, don't mask.
        #[allow(dead_code)] // constructed by the Stage C/D leaf hooks
        UnmatchedWeak,
        /// A definition was attempted on a frozen owner.
        FrozenMutation,
    }

    /// Work item for the iterative transforms. Containers are decomposed
    /// onto an explicit stack; depth never grows the Rust call stack.
    enum Task<'a> {
        Enter(&'a Val),
        BuildList(usize),
        BuildVector(usize),
        BuildSet(usize),
        /// Carries the source map: pair count for reassembly, plus identity
        /// — an unchanged map is reproduced by an O(1) structural-sharing
        /// clone of the original, which also preserves reader
        /// `literal_pairs` provenance (quoted maps are code-as-data; their
        /// duplicate-key evidence must survive definition storage).
        BuildMap(&'a ValMap),
    }

    /// Transform result entry: the rebuilt value, whether the subtree holds
    /// resting self-references, and whether anything changed (unchanged
    /// containers are rebuilt from child outputs — never recursively
    /// cloned — so arbitrarily deep values stay stack-safe).
    type Out = (Val, bool, bool);

    /// Normalize `v` for storage inside `owner`'s own subtree.
    ///
    /// Returns the normalized value and the `has_resting_owner_refs`
    /// summary. Exhaustive over `Val`: adding a variant forces an explicit
    /// leaf-or-container decision here (gate condition 2).
    pub(super) fn rest_for(owner: &Rc<Defs>, v: &Val) -> (Val, bool) {
        let mut work: Vec<Task<'_>> = vec![Task::Enter(v)];
        let mut out: Vec<Out> = Vec::new();
        while let Some(task) = work.pop() {
            match task {
                Task::Enter(v) => {
                    let done = enter(v, &mut work, &mut out, |val| Ok(rest_leaf(owner, val)));
                    done.expect("rest leaves are infallible");
                }
                other => build(other, &mut out),
            }
        }
        debug_assert_eq!(out.len(), 1);
        let (value, resting, _changed) = out.pop().expect("transform yields one root");
        (value, resting)
    }

    /// Rest one non-container leaf: callables' owner references release
    /// their keep-alive when stored inside their OWN owner's subtree; the
    /// callable's interior capture is untouched (it was normalized at its
    /// own construction — the barrier rewrites owner edges, never recurses
    /// into captures). Caps become owner-aware in Stage D.
    fn rest_leaf(owner: &Rc<Defs>, v: &Val) -> Out {
        match v {
            Val::Fn {
                arities,
                closure,
                is_cap_free,
                cap_violation,
            } => {
                let rested = closure.owner.rested(owner);
                let changed = rested.is_resting_for(owner) && !closure.owner.is_resting_for(owner);
                let resting = rested.is_resting_for(owner);
                (
                    Val::Fn {
                        arities: arities.clone(),
                        closure: crate::Closure {
                            captured: Rc::clone(&closure.captured),
                            owner: rested,
                        },
                        is_cap_free: *is_cap_free,
                        cap_violation: cap_violation.clone(),
                    },
                    resting,
                    changed,
                )
            }
            Val::Macro {
                arities,
                closure,
                is_cap_free,
                cap_violation,
            } => {
                let rested = closure.owner.rested(owner);
                let changed = rested.is_resting_for(owner) && !closure.owner.is_resting_for(owner);
                let resting = rested.is_resting_for(owner);
                (
                    Val::Macro {
                        arities: arities.clone(),
                        closure: crate::Closure {
                            captured: Rc::clone(&closure.captured),
                            owner: rested,
                        },
                        is_cap_free: *is_cap_free,
                        cap_violation: cap_violation.clone(),
                    },
                    resting,
                    changed,
                )
            }
            other => (other.clone(), false, false),
        }
    }

    /// Restore `v` read out of `witness`'s subtree to fully-escaped form.
    pub(super) fn escape_with(witness: &Rc<Defs>, v: &Val) -> Result<Val, OwnFault> {
        let mut work: Vec<Task<'_>> = vec![Task::Enter(v)];
        let mut out: Vec<Out> = Vec::new();
        while let Some(task) = work.pop() {
            match task {
                Task::Enter(v) => enter(v, &mut work, &mut out, |val| escape_leaf(witness, val))?,
                other => build(other, &mut out),
            }
        }
        debug_assert_eq!(out.len(), 1);
        let (value, _resting, _changed) = out.pop().expect("transform yields one root");
        Ok(value)
    }

    /// Escape one non-container leaf: resting owner references are
    /// restored through the live witness; a resting reference that does
    /// not match the witness is the release-checked invariant fault.
    fn escape_leaf(witness: &Rc<Defs>, v: &Val) -> Result<Out, OwnFault> {
        match v {
            Val::Fn {
                arities,
                closure,
                is_cap_free,
                cap_violation,
            } => {
                let was_resting = closure.owner.is_resting_for(witness);
                let escaped = closure.owner.escaped_with(witness)?;
                Ok((
                    Val::Fn {
                        arities: arities.clone(),
                        closure: crate::Closure {
                            captured: Rc::clone(&closure.captured),
                            owner: escaped,
                        },
                        is_cap_free: *is_cap_free,
                        cap_violation: cap_violation.clone(),
                    },
                    false,
                    was_resting,
                ))
            }
            Val::Macro {
                arities,
                closure,
                is_cap_free,
                cap_violation,
            } => {
                let was_resting = closure.owner.is_resting_for(witness);
                let escaped = closure.owner.escaped_with(witness)?;
                Ok((
                    Val::Macro {
                        arities: arities.clone(),
                        closure: crate::Closure {
                            captured: Rc::clone(&closure.captured),
                            owner: escaped,
                        },
                        is_cap_free: *is_cap_free,
                        cap_violation: cap_violation.clone(),
                    },
                    false,
                    was_resting,
                ))
            }
            other => Ok((other.clone(), false, false)),
        }
    }

    /// Decompose one value: containers push build markers + children;
    /// everything else goes through the leaf hook. The match is exhaustive
    /// by design — NO wildcard arm.
    fn enter<'a>(
        v: &'a Val,
        work: &mut Vec<Task<'a>>,
        out: &mut Vec<Out>,
        leaf: impl Fn(&Val) -> Result<Out, OwnFault>,
    ) -> Result<(), OwnFault> {
        match v {
            // Transparent local containers — the ONLY recursion points.
            Val::List(xs) => {
                work.push(Task::BuildList(xs.len()));
                for x in xs {
                    work.push(Task::Enter(x));
                }
            }
            Val::Vector(xs) => {
                work.push(Task::BuildVector(xs.len()));
                for x in xs {
                    work.push(Task::Enter(x));
                }
            }
            Val::Set(xs) => {
                work.push(Task::BuildSet(xs.len()));
                for x in xs {
                    work.push(Task::Enter(x));
                }
            }
            Val::Map(m) => {
                work.push(Task::BuildMap(m));
                for (k, val) in m.iter() {
                    work.push(Task::Enter(k));
                    work.push(Task::Enter(val));
                }
            }
            // Durable data: barrier-inert leaves (jurisdiction rule).
            Val::Nil
            | Val::Bool(_)
            | Val::Int(_)
            | Val::Float(_)
            | Val::Str(_)
            | Val::Sym(_)
            | Val::Keyword(_)
            | Val::Bytes(_) => out.push(leaf(v)?),
            // Atoms: opaque interior — the barrier never looks inside
            // (accepted leak class; see the ownership ledger).
            Val::Atom(_) => out.push(leaf(v)?),
            // Host trust boundary: never traversed.
            Val::NativeFn { .. } | Val::AsyncNativeFn { .. } => out.push(leaf(v)?),
            // Owner-bearing leaves: the hook rewrites `Closure::owner`
            // (callables now; caps in Stage D). The barrier NEVER recurses
            // into a callable's interior capture.
            Val::Fn { .. } | Val::Macro { .. } => out.push(leaf(v)?),
            Val::Cap(_) => out.push(leaf(v)?),
        }
        Ok(())
    }

    /// Reassemble one container from its transformed children.
    ///
    /// ORDERING INVARIANT: `enter` pushes children onto the work stack in
    /// source order, so they are PROCESSED in reverse and their outputs sit
    /// on the output stack with the FIRST child on top. Popping therefore
    /// yields source order directly — no reversal. (An earlier draft
    /// reversed here; the round-trip tests masked it because rest + escape
    /// applied compensating reversals. One-way tests now pin this.)
    fn build(task: Task<'_>, out: &mut Vec<Out>) {
        match task {
            Task::Enter(_) => unreachable!("Enter handled by caller"),
            Task::BuildList(n) | Task::BuildVector(n) | Task::BuildSet(n) => {
                let mut resting = false;
                let mut changed = false;
                let mut items = Vec::with_capacity(n);
                for _ in 0..n {
                    let (v, r, c) = out.pop().expect("child output present");
                    resting |= r;
                    changed |= c;
                    items.push(v);
                }
                // Vec-backed containers are always rebuilt from child
                // outputs (never `clone()`d wholesale: recursive Vec clone
                // would consume Rust stack on deep values).
                let rebuilt = match task {
                    Task::BuildList(_) => Val::List(items),
                    Task::BuildVector(_) => Val::Vector(items),
                    Task::BuildSet(_) => Val::Set(items),
                    _ => unreachable!(),
                };
                out.push((rebuilt, resting, changed));
            }
            Task::BuildMap(source) => {
                let mut resting = false;
                let mut changed = false;
                let mut kvs = Vec::with_capacity(source.len());
                for _ in 0..source.len() {
                    // Each pair was pushed (key, value); the key's output
                    // is on top (see the ordering invariant above).
                    let (key, kr, kc) = out.pop().expect("map key present");
                    let (val, vr, vc) = out.pop().expect("map value present");
                    resting |= kr | vr;
                    changed |= kc | vc;
                    kvs.push((key, val));
                }
                let rebuilt = if changed {
                    // Owner-bearing contents were rewritten: this is a
                    // runtime map (reader literals are pure data and never
                    // change), so dropping `literal_pairs` is correct.
                    Val::Map(ValMap::from_pairs(kvs))
                } else {
                    // Unchanged: O(1) structural-sharing clone preserves
                    // identity, normalization, and reader provenance.
                    Val::Map(source.clone())
                };
                out.push((rebuilt, resting, changed));
            }
        }
    }

    /// Stage D shell: seal an evaluator-owned cap inner's contents for
    /// `owner` (methods/base/handler rested; the outer cap carries the
    /// witness). Wired at `defcap` construction and define-time rebuild of
    /// the known inner types.
    #[allow(dead_code)] // wired in Stage D
    pub(super) fn seal_cap_inner(owner: &Rc<Defs>, contents: &Val) -> (Val, bool) {
        rest_for(owner, contents)
    }

    /// Stage D shell: attenuation transfers the base capability's owner
    /// lifetime to the derived capability. Lifetime only — no authority
    /// meaning.
    #[allow(dead_code)] // wired in Stage D
    pub(super) fn transfer_owner(base: &OwnerRef) -> OwnerRef {
        base.clone()
    }

    #[allow(dead_code)] // capture normalization lands in Stage C
    pub(super) fn rest_frame_for(owner: &Rc<Defs>, slots: &Frame) -> (Frame, bool) {
        let mut resting = false;
        let mut out = Frame::with_capacity(slots.len());
        for (k, v) in slots {
            let (v, r) = rest_for(owner, v);
            resting |= r;
            out.insert(k.clone(), v);
        }
        (out, resting)
    }
}

// ---------------------------------------------------------------------------
// Dispatch — external command routing
// ---------------------------------------------------------------------------

/// Trait for dispatching evaluated calls to external handlers.
///
/// The kernel (or any host) implements this to route capability calls
/// like `(host id)`, `(ipfs cat ...)`, etc.
pub trait Dispatch {
    /// Invoke the command `name` with already-evaluated `args`.
    ///
    /// Takes `&self` (not `&mut self`) — implementations use interior mutability
    /// for any mutable state. This enables sharing dispatch between body and
    /// handler futures in the effect system's state machine.
    ///
    /// The error channel carries a [`NativeSignal`]: ordinary failures are
    /// catchable exceptions (`Err(Val::from(..))` still compiles via `From`);
    /// trusted invariant violations use [`NativeSignal::fault`].
    fn call<'a>(
        &'a self,
        name: &'a str,
        args: &'a [Val],
    ) -> Pin<Box<dyn Future<Output = Result<Val, NativeSignal>> + 'a>>;

    /// Offer the embedder the chance to reify `(attenuate cap methods)` into
    /// real boundary enforcement before glia falls back to evaluator-local
    /// attenuation.
    ///
    /// The wetware kernel overrides this for capnp-backed caps: it wraps the
    /// underlying client hook in a `wetware-membrane` allowlist so the attenuation
    /// travels with the capability across process/vat boundaries, and returns
    /// a [`HandledCapInner`]-backed cap. Returning `None` (the default)
    /// means "not mine" — glia then applies the local [`AttenuatedCapInner`]
    /// path, which is only interposition within this evaluator (sound for
    /// caps that cannot cross a boundary, such as `defcap` caps).
    fn reify_attenuation(
        &self,
        cap: &Val,
        allow_methods: &BTreeSet<String>,
    ) -> Option<Result<Val, Val>> {
        let _ = (cap, allow_methods);
        None
    }

    /// Embedding-specific exportability check for a `cell` grant.
    ///
    /// Glia rejects non-capabilities and evaluator-local `defcap` values
    /// itself. Embeddings may additionally reject capability wrappers that
    /// cannot be encoded for their process boundary.
    fn validate_cell_grant(&self, name: &str, cap: &Val) -> Result<(), Val> {
        let _ = (name, cap);
        Ok(())
    }

    /// Report a non-blocking transitional authoring warning.
    fn report_warning(&self, warning: &str) {
        let _ = warning;
    }
}

// ---------------------------------------------------------------------------
// Control — evaluator-internal flow and unwinding
// ---------------------------------------------------------------------------

/// Evaluator-internal result of one expression: a value, or a lexical
/// `recur` unwinding to the nearest loop/fn tail frame. Crate-private, so
/// controls are unrepresentable as values and unreachable from natives.
#[derive(Debug, Clone)]
pub(crate) enum Flow {
    Value(Val),
    Recur(Vec<Val>),
}

impl Flow {
    /// Demand a value in a non-tail position. A `Recur` arriving here is
    /// structurally invalid control — a language fault, bypassing all Glia
    /// handlers. `context` names the position that demanded the value.
    pub(crate) fn into_value(self, context: &str) -> Result<Val, Control> {
        match self {
            Flow::Value(v) => Ok(v),
            Flow::Recur(_) => Err(Control::Fault(Box::new(Fault::language(
                error::invalid_recur(context),
            )))),
        }
    }
}

impl From<Val> for Flow {
    fn from(v: Val) -> Self {
        Flow::Value(v)
    }
}

/// Non-value, non-recur unwinding. Exceptions never travel this channel —
/// they are performed as the `:glia.exception` effect via [`throw`], so the
/// only error-shaped arm here is the uncatchable fault.
#[derive(Debug, Clone)]
pub(crate) enum Control {
    /// Unrecoverable runtime fault; bypasses all Glia handlers.
    /// Boxed to keep the hot `Result<Flow, Control>` return small.
    Fault(Box<Fault>),
    /// An effect that found no matching handler, unwinding to the boundary
    /// (includes unhandled exceptions: target `:glia.exception`). Boxed for
    /// the same size reason.
    Unhandled(Box<effect::EffectRequest>),
    /// Handler short-circuit from `resume`.
    Resume(Val),
}

/// Dispatch `payload` as a catchable `:glia.exception` exception on the
/// current handler stack. `Ok(v)` means a resuming handler supplied `v` as
/// the value of the failing expression; `Err(Control::Unhandled)` means no
/// handler was in scope and the exception unwinds to the boundary.
pub(crate) async fn throw(hs: &HandlerStack, payload: Val) -> Result<Val, Control> {
    perform_dispatch(
        hs,
        effect::EffectTarget::Keyword(error::EXCEPTION_EFFECT.into()),
        payload,
    )
    .await
}

/// Settle a native/Dispatch invocation: values pass through, throws are
/// dispatched as exceptions, resume and fault signals unwind as control.
async fn settle_native(hs: &HandlerStack, r: Result<Val, NativeSignal>) -> Result<Val, Control> {
    use crate::NativeSignalKind;
    match r {
        Ok(v) => Ok(v),
        Err(NativeSignal(NativeSignalKind::Throw(payload))) => throw(hs, payload).await,
        Err(NativeSignal(NativeSignalKind::Resume(v))) => Err(Control::Resume(v)),
        Err(NativeSignal(NativeSignalKind::Fault(f))) => Err(Control::Fault(Box::new(f))),
    }
}

/// Unwrap a `Result<T, Val>` whose `Err` is an exception payload: dispatch
/// it on the current handler stack. If a handler resumes, the resumed value
/// becomes the value of the ENCLOSING expression (early return from the
/// surrounding function). Usable in functions returning
/// `Result<Val, Control>` or `Result<Flow, Control>`.
macro_rules! try_throw {
    ($env:expr, $r:expr) => {
        match $r {
            Ok(x) => x,
            Err(payload) => {
                return throw(&$env.handler_stack, payload).await.map(Into::into);
            }
        }
    };
}

// ---------------------------------------------------------------------------
// EvalError — the embedder boundary
// ---------------------------------------------------------------------------

/// How a top-level evaluation failed, as seen by embedders. One structured
/// escaped-effect arm covers unhandled exceptions and unhandled ordinary
/// effects alike; there is no exception-specific error variant.
#[derive(Clone, Debug, PartialEq)]
pub enum EvalError {
    /// Unrecoverable runtime fault (bypassed all Glia handlers).
    Fault(Fault),
    /// An effect that reached the boundary with no matching handler —
    /// including an unhandled `throw` (target `:glia.exception`).
    Unhandled(effect::EffectRequest),
}

impl EvalError {
    /// The thrown payload iff this is an unhandled `:glia.exception`.
    /// Successor of `error::unwrap_thrown`.
    pub fn thrown(&self) -> Option<&Val> {
        match self {
            EvalError::Unhandled(req)
                if matches!(&req.target,
                    effect::EffectTarget::Keyword(k) if k == error::EXCEPTION_EFFECT) =>
            {
                Some(&req.data)
            }
            _ => None,
        }
    }

    /// The structured payload embedders inspect with `error::message` /
    /// `error::type_tag`: thrown error data, or fault payload. `None` for
    /// non-exception escaped effects (display falls back to `{self}`).
    pub fn payload(&self) -> Option<&Val> {
        match self {
            EvalError::Fault(f) => Some(f.payload()),
            other => other.thrown(),
        }
    }
}

impl core::fmt::Display for EvalError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            // Faults display as their structured payload, exactly like the
            // pre-extraction bare error map.
            EvalError::Fault(fault) => write!(f, "{}", fault.payload()),
            // Unhandled exceptions display peeled (the payload map);
            // other escaped effects keep the legacy carrier form.
            EvalError::Unhandled(req) => match self.thrown() {
                Some(payload) => write!(f, "{payload}"),
                None => write!(f, "#<effect :{} {}>", req.effect_type(), req.data),
            },
        }
    }
}

impl std::error::Error for EvalError {}

/// Convert an internal evaluation result into the embedder boundary form.
/// `EvalError` is a cold boundary type (constructed at most once per
/// top-level evaluation), so its by-value size is not a concern.
#[allow(clippy::result_large_err)]
fn seal(r: Result<Flow, Control>) -> Result<Val, EvalError> {
    match r {
        Ok(Flow::Value(v)) => Ok(v),
        Ok(Flow::Recur(_)) => Err(EvalError::Fault(Fault::language(error::invalid_recur(
            "top level",
        )))),
        Err(Control::Fault(fault)) => Err(EvalError::Fault(*fault)),
        Err(Control::Unhandled(req)) => Err(EvalError::Unhandled(*req)),
        Err(Control::Resume(val)) => Err(EvalError::Fault(Fault::runtime(error::internal(
            "resume",
            format!("resume signal escaped to the top level (value: {val})"),
        )))),
    }
}

// ---------------------------------------------------------------------------
// Evaluator
// ---------------------------------------------------------------------------

/// Returns true if `val` is logically truthy (Clojure model).
/// Only `nil` and `false` are falsy — everything else is truthy,
/// including `0`, empty string, and empty collections.
fn is_truthy(val: &Val) -> bool {
    !matches!(val, Val::Nil | Val::Bool(false))
}

fn cap_descriptor_bytes(name: &str, schema_cid: &str, methods: &BTreeSet<String>) -> Vec<u8> {
    let mut method_vec: Vec<&str> = methods.iter().map(String::as_str).collect();
    method_vec.sort_unstable();
    format!(
        "glia.cap.v1\nname={name}\nschema={schema_cid}\nmethods={}\n",
        method_vec.join(",")
    )
    .into_bytes()
}

fn parse_allow_methods(value: &Val) -> Result<BTreeSet<String>, Val> {
    let items = match value {
        Val::Vector(v) | Val::List(v) => v,
        other => {
            return Err(error::type_mismatch(
                "attenuate allow-methods",
                "vector or list",
                other,
            ))
        }
    };

    let mut allow = BTreeSet::new();
    for item in items {
        match item {
            Val::Keyword(k) => {
                allow.insert(k.clone());
            }
            other => return Err(error::type_mismatch("attenuate method", "keyword", other)),
        }
    }
    Ok(allow)
}

fn is_authority_free(value: &Val) -> bool {
    // Atoms make the value graph potentially CYCLIC (`(reset! a a)` is
    // expressible), so the traversal tracks the atoms on the current path:
    // revisiting one is a back-edge whose contents are already under
    // inspection — recursing again would loop forever (RefCell permits
    // nested shared borrows, so this is a stack overflow, not a panic).
    fn walk(value: &Val, visiting: &mut Vec<*const RefCell<Val>>) -> bool {
        match value {
            // An atom is only as authority-free as its current contents.
            Val::Atom(a) => {
                let ptr = Rc::as_ptr(a);
                if visiting.contains(&ptr) {
                    // Back-edge: adds no values not already being checked.
                    return true;
                }
                visiting.push(ptr);
                let result = match a.try_borrow() {
                    Ok(inner) => walk(&inner, visiting),
                    // Mutably borrowed elsewhere: contents unknowable —
                    // fail closed (may hold a cap).
                    Err(_) => false,
                };
                visiting.pop();
                result
            }
            Val::Nil
            | Val::Bool(_)
            | Val::Int(_)
            | Val::Float(_)
            | Val::Str(_)
            | Val::Sym(_)
            | Val::Keyword(_)
            | Val::Bytes(_) => true,
            Val::List(items) | Val::Vector(items) | Val::Set(items) => {
                items.iter().all(|v| walk(v, visiting))
            }
            Val::Map(m) => m
                .iter()
                .all(|(k, v)| walk(k, visiting) && walk(v, visiting)),
            Val::Fn { is_cap_free, .. } | Val::Macro { is_cap_free, .. } => *is_cap_free,
            Val::NativeFn { .. } | Val::AsyncNativeFn { .. } | Val::Cap(_) => false,
        }
    }
    walk(value, &mut Vec::new())
}

fn compute_cap_status(captured: &CapturedEnv) -> (bool, Option<String>) {
    // Deterministic report order (matches the old sorted-bindings walk).
    let mut names: Vec<&String> = captured.slots.keys().collect();
    names.sort();
    for name in names {
        if !is_authority_free(&captured.slots[name]) {
            return (false, Some(name.clone()));
        }
    }
    (true, None)
}

// ---------------------------------------------------------------------------
// Explicit cell grants
// ---------------------------------------------------------------------------

fn cell_error(message: impl Into<String>) -> Val {
    error::internal("cell", message)
}

fn cell_call_grants_index(raw_args: &[Val]) -> Result<Option<usize>, Val> {
    match raw_args {
        [] => Err(error::arity("cell", "1 or 3", 0)),
        [_wasm] => Ok(None),
        [_wasm, Val::Keyword(keyword)] if keyword == "grants" => Err(cell_error(
            "cell — missing grant map after :grants; use (cell image :grants {})",
        )),
        [_wasm, Val::Keyword(keyword)] => Err(cell_error(format!(
            "cell — unknown keyword :{keyword}; supported keyword: :grants"
        ))),
        [_wasm, _] => Err(cell_error(
            "cell — malformed keyword arguments; use (cell image) or (cell image :grants grant-map)",
        )),
        [_wasm, Val::Keyword(keyword), _grants] if keyword == "grants" => Ok(Some(2)),
        [_wasm, Val::Keyword(keyword), _] => Err(cell_error(format!(
            "cell — unknown keyword :{keyword}; supported keyword: :grants"
        ))),
        [_wasm, other, _] => Err(error::type_mismatch(
            "cell option",
            "keyword :grants",
            other,
        )),
        _ => Err(cell_error(format!(
            "cell — malformed keyword arguments: expected (cell image) or (cell image :grants grant-map), got {} arguments",
            raw_args.len()
        ))),
    }
}

fn validate_literal_grant_duplicates(raw_grants: &Val) -> Result<(), Val> {
    let Val::Map(map) = raw_grants else {
        return Ok(());
    };
    let Some(pairs) = map.literal_pairs() else {
        return Ok(());
    };

    let mut first_sites = HashMap::<String, usize>::new();
    for (index, (key, _)) in pairs.iter().enumerate() {
        let Val::Keyword(name) = key else {
            continue;
        };
        if let Some(first) = first_sites.insert(name.clone(), index + 1) {
            return Err(cell_error(format!(
                "duplicate grant name \"{name}\": first defined at grant-map entry {first}, again at entry {}; grant names must be unique",
                index + 1
            )));
        }
    }
    Ok(())
}

fn cell_wasm(value: Val) -> Result<Vec<u8>, Val> {
    match value {
        Val::Bytes(bytes) => Ok(bytes),
        other => Err(error::type_mismatch(
            "cell first arg (wasm)",
            "bytes",
            &other,
        )),
    }
}

fn grant_value_error(name: &str, value: &Val) -> Val {
    cell_error(format!(
        "grant \"{name}\" expected a capability, got {}; use a capability value in :grants, for example :{name} {name}-cap",
        error::val_type_name(value)
    ))
}

fn validate_glia_grant(name: &str, value: &Val) -> Result<(), Val> {
    let Val::Cap(handle) = value else {
        return Err(grant_value_error(name, value));
    };
    if handle.inner().downcast_ref::<GliaCapInner>().is_some()
        || handle
            .inner()
            .downcast_ref::<AttenuatedCapInner>()
            .is_some()
    {
        return Err(cell_error(format!(
            "grant \"{name}\" is a Glia-native capability that cannot yet cross a cell boundary; use a Cap'n Proto-backed capability or follow the defcap-export work"
        )));
    }
    Ok(())
}

fn build_explicit_cell<D: Dispatch>(
    wasm: Vec<u8>,
    entries: Vec<(Val, Val)>,
    dispatch: &D,
) -> Result<Val, Val> {
    let mut grants = std::collections::BTreeMap::<String, Val>::new();
    for (key, value) in entries {
        let name = match key {
            Val::Keyword(name) => name,
            other => {
                return Err(error::type_mismatch(
                    "cell :grants map key",
                    "keyword",
                    &other,
                ))
            }
        };
        if grants.contains_key(&name) {
            return Err(cell_error(format!(
                "duplicate grant name \"{name}\"; grant names must be unique"
            )));
        }
        validate_glia_grant(&name, &value)?;
        dispatch.validate_cell_grant(&name, &value)?;
        grants.insert(name, value);
    }
    let grants = ValMap::from_pairs(
        grants
            .into_iter()
            .map(|(name, value)| (Val::Keyword(name), value))
            .collect(),
    );
    Ok(Val::Map(ValMap::from_pairs(vec![
        (
            Val::Keyword(crate::cell_spec::TYPE_KEY.into()),
            Val::Keyword(crate::cell_spec::TYPE_TAG.into()),
        ),
        (
            Val::Keyword(crate::cell_spec::WASM_KEY.into()),
            Val::Bytes(wasm),
        ),
        (
            Val::Keyword(crate::cell_spec::GRANTS_KEY.into()),
            Val::Map(grants),
        ),
    ])))
}

fn report_legacy_cell_capture<D: Dispatch>(env: &Env, dispatch: &D) {
    let names = env.scoped_cap_names();
    if names.is_empty() {
        return;
    }
    let bindings = names.join(", ");
    let rewrite = names
        .iter()
        .map(|name| format!(":{name} {name}"))
        .collect::<Vec<_>>()
        .join(" ");
    dispatch.report_warning(&format!(
        "transitional cell grant migration warning: lexical capability capture was removed; scoped capability binding(s) {bindings} are not granted to this child. Rewrite as (cell image :grants {{{rewrite}}})"
    ));
}

async fn eval_cell_expr<D: Dispatch>(
    args: &[Expr],
    raw_args: &[Val],
    env: &mut Env,
    dispatch: &D,
) -> Result<Val, Control> {
    let grants_index = try_throw!(env, cell_call_grants_index(raw_args));
    if let Some(index) = grants_index {
        try_throw!(env, validate_literal_grant_duplicates(&raw_args[index]));
    }

    let wasm_val = eval_expr(&args[0], env, dispatch)
        .await?
        .into_value("cell wasm argument")?;
    let wasm = try_throw!(env, cell_wasm(wasm_val));
    let Some(index) = grants_index else {
        report_legacy_cell_capture(env, dispatch);
        return Ok(try_throw!(
            env,
            build_explicit_cell(wasm, Vec::new(), dispatch)
        ));
    };

    let entries = match &args[index] {
        Expr::Map(pairs) => {
            let mut entries = Vec::with_capacity(pairs.len());
            for (key, value) in pairs {
                entries.push((
                    eval_expr(key, env, dispatch)
                        .await?
                        .into_value("cell :grants key")?,
                    eval_expr(value, env, dispatch)
                        .await?
                        .into_value("cell :grants value")?,
                ));
            }
            entries
        }
        grants_expr => match eval_expr(grants_expr, env, dispatch)
            .await?
            .into_value("cell :grants")?
        {
            Val::Map(map) => map.iter().map(|(k, v)| (k.clone(), v.clone())).collect(),
            other => {
                let payload = error::type_mismatch("cell :grants", "map", &other);
                return throw(&env.handler_stack, payload).await;
            }
        },
    };
    Ok(try_throw!(
        env,
        build_explicit_cell(wasm, entries, dispatch)
    ))
}

async fn eval_cell_raw<D: Dispatch>(
    raw_args: &[Val],
    env: &mut Env,
    dispatch: &D,
) -> Result<Val, Control> {
    let grants_index = try_throw!(env, cell_call_grants_index(raw_args));
    if let Some(index) = grants_index {
        try_throw!(env, validate_literal_grant_duplicates(&raw_args[index]));
    }

    let wasm_val = eval(&raw_args[0], env, dispatch)
        .await?
        .into_value("cell wasm argument")?;
    let wasm = try_throw!(env, cell_wasm(wasm_val));
    let Some(index) = grants_index else {
        report_legacy_cell_capture(env, dispatch);
        return Ok(try_throw!(
            env,
            build_explicit_cell(wasm, Vec::new(), dispatch)
        ));
    };

    let entries = match &raw_args[index] {
        Val::Map(map) if map.literal_pairs().is_some() => {
            let pairs = map.literal_pairs().expect("checked literal pairs");
            let mut entries = Vec::with_capacity(pairs.len());
            for (key, value) in pairs {
                entries.push((
                    eval(key, env, dispatch)
                        .await?
                        .into_value("cell :grants key")?,
                    eval(value, env, dispatch)
                        .await?
                        .into_value("cell :grants value")?,
                ));
            }
            entries
        }
        grants_expr => match eval(grants_expr, env, dispatch)
            .await?
            .into_value("cell :grants")?
        {
            Val::Map(map) => map.iter().map(|(k, v)| (k.clone(), v.clone())).collect(),
            other => {
                let payload = error::type_mismatch("cell :grants", "map", &other);
                return throw(&env.handler_stack, payload).await;
            }
        },
    };
    Ok(try_throw!(
        env,
        build_explicit_cell(wasm, entries, dispatch)
    ))
}

/// Evaluate a function/macro body, dispatching on `FnBody` variant.
///
/// `Analyzed` bodies are evaluated via `eval_expr` (no re-analysis).
/// `Raw` bodies are analyzed first, then evaluated (one-time cost for
/// macro-produced closures).
async fn eval_fn_body<'a, D: Dispatch>(
    body: &'a FnBody,
    env: &'a mut Env,
    dispatch: &'a D,
) -> Result<Flow, Control> {
    // Only the last form is in tail position (it may recur); intermediate
    // forms demand values.
    match body {
        FnBody::Raw(forms) => {
            let Some((last, init)) = forms.split_last() else {
                return Ok(Flow::Value(Val::Nil));
            };
            for form in init {
                eval(form, env, dispatch).await?.into_value("body form")?;
            }
            eval(last, env, dispatch).await
        }
        FnBody::Analyzed(exprs) => {
            let Some((last, init)) = exprs.split_last() else {
                return Ok(Flow::Value(Val::Nil));
            };
            for expr in init {
                eval_expr(expr, env, dispatch)
                    .await?
                    .into_value("body form")?;
            }
            eval_expr(last, env, dispatch).await
        }
    }
}

/// Evaluate arguments: recursively evaluate nested lists, look up symbols
/// in env (pass through if unbound), and return non-list/non-sym values as-is.
///
/// Used by the generic dispatch path and future fn invocation.
async fn eval_args<'a, D: Dispatch>(
    raw_args: &'a [Val],
    env: &'a mut Env,
    dispatch: &'a D,
) -> Result<Vec<Val>, Control> {
    let mut args = Vec::with_capacity(raw_args.len());
    for a in raw_args {
        match a {
            Val::List(_) => args.push(
                eval(a, env, dispatch)
                    .await?
                    .into_value("function argument")?,
            ),
            Val::Sym(s) => match resolve(env, s)? {
                Some(v) => args.push(v.clone()),
                None => args.push(a.clone()),
            },
            other => args.push(other.clone()),
        }
    }
    Ok(args)
}

// ---------------------------------------------------------------------------
// Special forms — each receives RAW (unevaluated) args
// ---------------------------------------------------------------------------

/// `(def name value)` — evaluate value, bind name in root frame.
async fn eval_def<'a, D: Dispatch>(
    args: &'a [Val],
    env: &'a mut Env,
    dispatch: &'a D,
) -> Result<Val, Control> {
    if args.is_empty() || args.len() > 2 {
        return throw(&env.handler_stack, error::arity("def", "1-2", args.len())).await;
    }
    let name = match &args[0] {
        Val::Sym(s) => s.clone(),
        other => {
            let payload = error::type_mismatch("def", "symbol", other);
            return throw(&env.handler_stack, payload).await;
        }
    };
    let val = match args.get(1) {
        Some(expr) => eval(expr, env, dispatch).await?.into_value("def value")?,
        None => Val::Nil,
    };
    define_or_throw(env, name, val).await
}

/// Route one definition through the checked operation ([`Env::define`]),
/// surfacing failures on the evaluator's channels: a privilege violation
/// throws the catchable `glia.error/def-not-top-level` BEFORE any mutation;
/// ownership-layer failures (frozen owner) are internal faults. Returns the
/// defined value — or the handler's resume value if a thrown privilege
/// error was resumed.
async fn define_or_throw(env: &Env, name: String, val: Val) -> Result<Val, Control> {
    match env.define(name.clone(), val.clone()) {
        Ok(()) => Ok(val),
        Err(DefineError::NotTopLevel) => {
            throw(&env.handler_stack, error::def_not_top_level(&name)).await
        }
        Err(DefineError::Own(fault)) => {
            Err(Control::Fault(Box::new(Fault::runtime(error::internal(
                "def",
                format!("definition of '{name}' rejected by ownership layer: {fault:?}"),
            )))))
        }
    }
}

/// `(if test then)` or `(if test then else)` — lazy eval of branches.
async fn eval_if<'a, D: Dispatch>(
    args: &'a [Val],
    env: &'a mut Env,
    dispatch: &'a D,
) -> Result<Flow, Control> {
    if args.len() < 2 || args.len() > 3 {
        let payload = error::arity("if", "2-3", args.len());
        return throw(&env.handler_stack, payload).await.map(Into::into);
    }
    let test_val = eval(&args[0], env, dispatch)
        .await?
        .into_value("if condition")?;
    if is_truthy(&test_val) {
        eval(&args[1], env, dispatch).await
    } else if args.len() == 3 {
        eval(&args[2], env, dispatch).await
    } else {
        Ok(Flow::Value(Val::Nil))
    }
}

/// `(do forms...)` — evaluate sequentially, return last.
async fn eval_do<'a, D: Dispatch>(
    args: &'a [Val],
    env: &'a mut Env,
    dispatch: &'a D,
) -> Result<Flow, Control> {
    let Some((last, init)) = args.split_last() else {
        return Ok(Flow::Value(Val::Nil));
    };
    for form in init {
        eval(form, env, dispatch).await?.into_value("do form")?;
    }
    eval(last, env, dispatch).await
}

/// `(let [bindings...] body...)` — local scope with sequential bindings.
async fn eval_let<'a, D: Dispatch>(
    args: &'a [Val],
    env: &'a mut Env,
    dispatch: &'a D,
) -> Result<Flow, Control> {
    let bindings = match args.first() {
        Some(Val::Vector(v)) => v,
        Some(other) => {
            let payload = error::type_mismatch("let", "vector of bindings", other);
            return throw(&env.handler_stack, payload).await.map(Into::into);
        }
        None => {
            let payload = error::arity("let", "at least 1", 0);
            return throw(&env.handler_stack, payload).await.map(Into::into);
        }
    };
    if bindings.len() % 2 != 0 {
        let payload = error::internal("let", "bindings must be pairs (even number of forms)");
        return throw(&env.handler_stack, payload).await.map(Into::into);
    }

    env.push_frame();

    // Evaluate bindings and body in a block so we always pop the frame,
    // even if evaluation unwinds mid-binding or mid-body.
    let result = async {
        for pair in bindings.chunks(2) {
            let name = match &pair[0] {
                Val::Sym(s) => s.clone(),
                other => {
                    let payload = error::type_mismatch("let binding name", "symbol", other);
                    return throw(&env.handler_stack, payload).await.map(Into::into);
                }
            };
            let val = eval(&pair[1], env, dispatch)
                .await?
                .into_value("let binding")?;
            env.set(name, val);
        }

        // Body forms (implicit do); last form is in tail position.
        let body = &args[1..];
        let Some((last, init)) = body.split_last() else {
            return Ok(Flow::Value(Val::Nil));
        };
        for form in init {
            eval(form, env, dispatch)
                .await?
                .into_value("let body form")?;
        }
        eval(last, env, dispatch).await
    }
    .await;

    env.pop_frame();
    result
}

/// Parse a parameter vector into an FnArity.
/// Handles `[x y]` (fixed) and `[x & rest]` (variadic).
fn parse_params(param_vec: &[Val], body: &[Val]) -> Result<FnArity, Val> {
    let mut params = Vec::new();
    let mut variadic = None;
    let mut i = 0;
    while i < param_vec.len() {
        match &param_vec[i] {
            Val::Sym(s) if s == "&" => {
                // Next symbol is the variadic rest param
                i += 1;
                match param_vec.get(i) {
                    Some(Val::Sym(rest_name)) => {
                        if variadic.is_some() {
                            return Err(error::internal("fn", "only one & rest param allowed"));
                        }
                        variadic = Some(rest_name.clone());
                    }
                    _ => return Err(error::internal("fn", "expected symbol after &")),
                }
                if i + 1 < param_vec.len() {
                    return Err(error::internal("fn", "nothing allowed after & rest param"));
                }
            }
            Val::Sym(s) => params.push(s.clone()),
            other => return Err(error::type_mismatch("fn parameter", "symbol", other)),
        }
        i += 1;
    }
    Ok(FnArity {
        params,
        variadic,
        body: FnBody::Raw(body.to_vec()),
    })
}

/// `(fn [params] body...)` or `(fn ([params] body...) ([params] body...))` — create a closure.
fn eval_fn(args: &[Val], env: &Env) -> Result<Val, Val> {
    if args.is_empty() {
        return Err(error::arity("fn", "at least 1", 0));
    }

    let arities = match &args[0] {
        // Single-arity: (fn [x y] body...)
        Val::Vector(params) => {
            let arity = parse_params(params, &args[1..])?;
            vec![arity]
        }
        // Multi-arity: (fn ([x] body1) ([x y] body2) ...)
        Val::List(_) => {
            let mut result = Vec::new();
            for arg in args {
                match arg {
                    Val::List(items) if !items.is_empty() => {
                        let param_vec = match &items[0] {
                            Val::Vector(v) => v,
                            other => {
                                return Err(error::type_mismatch(
                                    "fn multi-arity clause",
                                    "vector of params",
                                    other,
                                ))
                            }
                        };
                        result.push(parse_params(param_vec, &items[1..])?);
                    }
                    other => return Err(error::type_mismatch("fn arity clause", "list", other)),
                }
            }
            // Check for overlapping arities (same fixed param count, ignoring variadic)
            let mut seen_counts = std::collections::HashSet::new();
            let mut has_variadic = false;
            for a in &result {
                if a.variadic.is_some() {
                    if has_variadic {
                        return Err(error::internal("fn", "only one variadic arity allowed"));
                    }
                    has_variadic = true;
                } else if !seen_counts.insert(a.params.len()) {
                    return Err(error::internal(
                        "fn",
                        format!("duplicate arity for {} args", a.params.len()),
                    ));
                }
            }
            result
        }
        other => {
            return Err(error::type_mismatch(
                "fn",
                "[params] or arity clauses",
                other,
            ))
        }
    };

    // Raw fn path: no FnArityExpr, no free-vars data → full-frame capture;
    // the cap-status walk below sees every captured binding. Slim captures
    // only apply to the analyzed pipeline (expr::Expr::Fn).
    let captured = CapturedEnv::capture_all(env);
    let (is_cap_free, cap_violation) = compute_cap_status(&captured);
    Ok(Val::Fn {
        arities,
        closure: crate::Closure {
            captured: Rc::new(captured),
            owner: OwnerRef::Strong(Rc::clone(&env.defs)),
        },
        is_cap_free,
        cap_violation,
    })
}

/// Invoke a Val::Fn with evaluated arguments. Matches arity and evaluates body.
async fn invoke_fn<'a, D: Dispatch>(
    arities: &'a [FnArity],
    closure: &'a crate::Closure,
    args: &[Val],
    dispatch: &'a D,
    caller_hs: HandlerStack,
) -> Result<Val, Control> {
    // Find matching arity: prefer exact fixed-arity match over variadic.
    // This ensures (fn ([x y] ...) ([x & rest] ...)) called with 2 args
    // picks the fixed 2-arity, not the variadic.
    let matched = arities
        .iter()
        .find(|a| a.variadic.is_none() && args.len() == a.params.len())
        .or_else(|| {
            arities
                .iter()
                .find(|a| a.variadic.is_some() && args.len() >= a.params.len())
        });
    let arity = match matched {
        Some(a) => a,
        None => {
            let expected: Vec<String> = arities
                .iter()
                .map(|a| {
                    if a.variadic.is_some() {
                        format!("{}+", a.params.len())
                    } else {
                        a.params.len().to_string()
                    }
                })
                .collect();
            let payload = error::arity("fn", &expected.join(" or "), args.len());
            return throw(&caller_hs, payload).await;
        }
    };

    // Build fn environment: captured env + new frame with param bindings.
    // Uses Env::for_call to avoid infinite recursion from Env::clone when
    // closures capture their own scope.
    let mut fn_env = Env::for_call(closure, &caller_hs).map_err(Control::Fault)?;

    // Bind positional params
    for (name, val) in arity.params.iter().zip(args.iter()) {
        fn_env.set(name.clone(), val.clone());
    }

    // Bind variadic rest param
    if let Some(rest_name) = &arity.variadic {
        let rest_args: Vec<Val> = args[arity.params.len()..].to_vec();
        fn_env.set(rest_name.clone(), Val::List(rest_args));
    }

    // Number of expected recur args: fixed params + (1 if variadic)
    let recur_arity = arity.params.len() + usize::from(arity.variadic.is_some());

    // Evaluate body (implicit do) with recur support.
    // If the body's tail yields a recur, re-bind params and loop — same
    // semantics as loop/recur but targeting the enclosing fn.
    let result = async {
        loop {
            let result = eval_fn_body(&arity.body, &mut fn_env, dispatch).await?;

            match result {
                Flow::Recur(new_vals) => {
                    if new_vals.len() != recur_arity {
                        let payload =
                            error::arity("recur", &recur_arity.to_string(), new_vals.len());
                        return throw(&fn_env.handler_stack, payload).await;
                    }
                    // Re-bind fixed params
                    for (name, val) in arity.params.iter().zip(new_vals.iter()) {
                        fn_env.set(name.clone(), val.clone());
                    }
                    // Re-bind variadic rest param.
                    // Recur passes fixed_params + 1 args; the last arg IS the
                    // new variadic collection (not individual elements to collect).
                    if let Some(rest_name) = &arity.variadic {
                        let rest_val = new_vals[arity.params.len()].clone();
                        fn_env.set(rest_name.clone(), rest_val);
                    }
                    // continue — re-evaluate body with new bindings
                }
                Flow::Value(v) => return Ok(v),
            }
        }
    }
    .await;

    fn_env.pop_frame();
    result
}

/// Invoke a closure like [`invoke_fn`] but force the handler stack used inside
/// the function body. Used by defcap method dispatch to preserve the caller's
/// handler stack rather than the definition-time stack.
async fn invoke_fn_with_handler_stack<'a, D: Dispatch>(
    arities: &'a [FnArity],
    closure: &'a crate::Closure,
    args: &[Val],
    dispatch: &'a D,
    handler_stack: HandlerStack,
) -> Result<Val, Control> {
    invoke_fn(arities, closure, args, dispatch, handler_stack).await
}

/// Parse macro/fn arity definitions from raw Val args.
///
/// Shared by `eval_defmacro` (old path) and `eval_expr` DefMacro handler.
/// `fn_args` is `[params, body...]` or `[(arity1) (arity2) ...]`.
fn parse_macro_arities(fn_args: &[Val]) -> Result<Vec<FnArity>, Val> {
    if fn_args.is_empty() {
        return Err(error::arity("defmacro", "at least 1", 0));
    }
    match &fn_args[0] {
        // Single-arity: [x y] body...
        Val::Vector(params) => {
            let arity = parse_params(params, &fn_args[1..])?;
            Ok(vec![arity])
        }
        // Multi-arity: ([x] body1) ([x y] body2) ...
        Val::List(_) => {
            let mut result = Vec::new();
            for arg in fn_args {
                match arg {
                    Val::List(items) if !items.is_empty() => {
                        let param_vec = match &items[0] {
                            Val::Vector(v) => v,
                            other => {
                                return Err(error::type_mismatch(
                                    "defmacro multi-arity clause",
                                    "vector of params",
                                    other,
                                ))
                            }
                        };
                        result.push(parse_params(param_vec, &items[1..])?);
                    }
                    other => {
                        return Err(error::type_mismatch("defmacro arity clause", "list", other))
                    }
                }
            }
            let mut seen_counts = std::collections::HashSet::new();
            let mut has_variadic = false;
            for a in &result {
                if a.variadic.is_some() {
                    if has_variadic {
                        return Err(error::internal(
                            "defmacro",
                            "only one variadic arity allowed",
                        ));
                    }
                    has_variadic = true;
                } else if !seen_counts.insert(a.params.len()) {
                    return Err(error::internal(
                        "defmacro",
                        format!("duplicate arity for {} args", a.params.len()),
                    ));
                }
            }
            Ok(result)
        }
        other => Err(error::type_mismatch(
            "defmacro",
            "[params] or arity clauses",
            other,
        )),
    }
}

/// `(defmacro name [params] body...)` — define a macro in the root frame.
///
/// Like `fn` but the resulting `Val::Macro` receives unevaluated args;
/// the body evaluates in the captured env and the result is re-evaluated
/// in the caller's env.
async fn eval_defmacro(args: &[Val], env: &mut Env) -> Result<Val, Control> {
    if args.is_empty() {
        let payload = error::arity("defmacro", "at least 2", 0);
        return throw(&env.handler_stack, payload).await;
    }
    let name = match &args[0] {
        Val::Sym(s) => s.clone(),
        other => {
            let payload = error::type_mismatch("defmacro name", "symbol", other);
            return throw(&env.handler_stack, payload).await;
        }
    };
    let fn_args = &args[1..];
    if fn_args.is_empty() {
        let payload = error::arity("defmacro", "at least 2", 1);
        return throw(&env.handler_stack, payload).await;
    }
    let arities = try_throw!(env, parse_macro_arities(fn_args));
    // Raw macro path: no free-vars data → full-frame capture (see the raw
    // fn path).
    let captured = CapturedEnv::capture_all(env);
    let (is_cap_free, cap_violation) = compute_cap_status(&captured);
    let val = Val::Macro {
        arities,
        closure: crate::Closure {
            captured: Rc::new(captured),
            owner: OwnerRef::Strong(Rc::clone(&env.defs)),
        },
        is_cap_free,
        cap_violation,
    };
    define_or_throw(env, name, val).await
}

/// Invoke a macro: like invoke_fn but receives raw (unevaluated) args.
/// The macro body evaluates in the captured env; the result is a new form
/// that the caller will re-evaluate in their own env.
async fn invoke_macro<'a, D: Dispatch>(
    arities: &'a [FnArity],
    closure: &'a crate::Closure,
    raw_args: &[Val],
    dispatch: &'a D,
    caller_hs: HandlerStack,
) -> Result<Val, Control> {
    // Find matching arity (same logic as invoke_fn)
    let matched = arities
        .iter()
        .find(|a| a.variadic.is_none() && raw_args.len() == a.params.len())
        .or_else(|| {
            arities
                .iter()
                .find(|a| a.variadic.is_some() && raw_args.len() >= a.params.len())
        });
    let arity = match matched {
        Some(a) => a,
        None => {
            let expected: Vec<String> = arities
                .iter()
                .map(|a| {
                    if a.variadic.is_some() {
                        format!("{}+", a.params.len())
                    } else {
                        a.params.len().to_string()
                    }
                })
                .collect();
            let payload = error::arity("macro", &expected.join(" or "), raw_args.len());
            return throw(&caller_hs, payload).await;
        }
    };

    // Build macro environment: captured env + new frame with raw arg bindings
    let mut macro_env = Env::for_call(closure, &caller_hs).map_err(Control::Fault)?;

    // Bind positional params to RAW (unevaluated) args
    for (name, val) in arity.params.iter().zip(raw_args.iter()) {
        macro_env.set(name.clone(), val.clone());
    }

    // Bind variadic rest param
    if let Some(rest_name) = &arity.variadic {
        let rest_args: Vec<Val> = raw_args[arity.params.len()..].to_vec();
        macro_env.set(rest_name.clone(), Val::List(rest_args));
    }

    // Evaluate body (implicit do) in the macro's captured env. The
    // expansion must be a value: a macro body has no recur target.
    let result = async {
        eval_fn_body(&arity.body, &mut macro_env, dispatch)
            .await?
            .into_value("macro body")
    }
    .await;

    macro_env.pop_frame();
    result
}

/// `(loop [bindings...] body...)` — tail-recursive iteration.
///
/// Bindings are sequential (like `let`).  Body forms are evaluated in
/// an implicit `do`.  If the tail yields a recur, the bindings are
/// replaced and the body re-evaluated; otherwise the result is returned.
async fn eval_loop<'a, D: Dispatch>(
    args: &'a [Val],
    env: &'a mut Env,
    dispatch: &'a D,
) -> Result<Val, Control> {
    let bindings = match args.first() {
        Some(Val::Vector(v)) => v,
        Some(other) => {
            let payload = error::type_mismatch("loop", "vector of bindings", other);
            return throw(&env.handler_stack, payload).await;
        }
        None => {
            let payload = error::arity("loop", "at least 1", 0);
            return throw(&env.handler_stack, payload).await;
        }
    };
    if bindings.len() % 2 != 0 {
        let payload = error::internal("loop", "bindings must be pairs (even number of forms)");
        return throw(&env.handler_stack, payload).await;
    }

    let binding_names: Vec<String> = try_throw!(
        env,
        bindings
            .chunks(2)
            .map(|pair| match &pair[0] {
                Val::Sym(s) => Ok(s.clone()),
                other => Err(error::type_mismatch("loop binding name", "symbol", other)),
            })
            .collect::<Result<Vec<_>, Val>>()
    );

    let num_bindings = binding_names.len();

    env.push_frame();

    let result = async {
        // Evaluate initial bindings sequentially (each sees previous ones).
        for pair in bindings.chunks(2) {
            let name = match &pair[0] {
                Val::Sym(s) => s.clone(),
                _ => unreachable!(), // already validated above
            };
            let val = eval(&pair[1], env, dispatch)
                .await?
                .into_value("loop binding")?;
            env.set(name, val);
        }

        let body = &args[1..];
        loop {
            // Evaluate body forms (implicit do); only the last is in tail
            // position and may recur.
            let result = match body.split_last() {
                None => Flow::Value(Val::Nil),
                Some((last, init)) => {
                    for form in init {
                        eval(form, env, dispatch)
                            .await?
                            .into_value("loop body form")?;
                    }
                    eval(last, env, dispatch).await?
                }
            };

            match result {
                Flow::Recur(new_vals) => {
                    if new_vals.len() != num_bindings {
                        let payload =
                            error::arity("recur", &num_bindings.to_string(), new_vals.len());
                        return throw(&env.handler_stack, payload).await;
                    }
                    for (name, val) in binding_names.iter().zip(new_vals) {
                        env.set(name.clone(), val);
                    }
                    // continue loop — re-evaluate body
                }
                Flow::Value(v) => return Ok(v),
            }
        }
    }
    .await;

    env.pop_frame();
    result
}

/// `(recur args...)` — evaluate args and return a `Recur` sentinel.
///
/// Only meaningful inside `loop` body (tail position).  If it escapes
/// to the top level, `eval_toplevel` converts it to an error.
async fn eval_recur<'a, D: Dispatch>(
    args: &'a [Val],
    env: &'a mut Env,
    dispatch: &'a D,
) -> Result<Flow, Control> {
    let evaled = eval_args(args, env, dispatch).await?;
    Ok(Flow::Recur(evaled))
}

// ---------------------------------------------------------------------------
// Higher-order built-in functions (need async dispatch for fn invocation)
// ---------------------------------------------------------------------------

/// Dispatch `map`, `filter`, or `reduce` — these invoke user closures.
async fn eval_hof<'a, D: Dispatch>(
    name: &str,
    args: &[Val],
    env: &'a mut Env,
    dispatch: &'a D,
) -> Result<Val, Control> {
    match name {
        "map" => {
            if args.len() != 2 {
                let payload = error::arity("map", "2", args.len());
                return throw(&env.handler_stack, payload).await;
            }
            let (arities, closure) = try_throw!(env, extract_fn("map", &args[0]));
            let items = try_throw!(env, extract_seq("map", &args[1]));
            let mut result = Vec::with_capacity(items.len());
            for item in items {
                let val = invoke_fn(
                    &arities,
                    &closure,
                    std::slice::from_ref(item),
                    dispatch,
                    env.handler_stack.clone(),
                )
                .await?;
                result.push(val);
            }
            Ok(Val::List(result))
        }
        "filter" => {
            if args.len() != 2 {
                let payload = error::arity("filter", "2", args.len());
                return throw(&env.handler_stack, payload).await;
            }
            let (arities, closure) = try_throw!(env, extract_fn("filter", &args[0]));
            let items = try_throw!(env, extract_seq("filter", &args[1]));
            let mut result = Vec::new();
            for item in items {
                let val = invoke_fn(
                    &arities,
                    &closure,
                    std::slice::from_ref(item),
                    dispatch,
                    env.handler_stack.clone(),
                )
                .await?;
                let keep = !matches!(val, Val::Nil | Val::Bool(false));
                if keep {
                    result.push(item.clone());
                }
            }
            Ok(Val::List(result))
        }
        "reduce" => {
            if args.len() < 2 || args.len() > 3 {
                let payload = error::arity("reduce", "2-3", args.len());
                return throw(&env.handler_stack, payload).await;
            }
            let (arities, closure) = try_throw!(env, extract_fn("reduce", &args[0]));
            let (mut acc, items) = if args.len() == 3 {
                (
                    args[1].clone(),
                    try_throw!(env, extract_seq("reduce", &args[2])),
                )
            } else {
                let items = try_throw!(env, extract_seq("reduce", &args[1]));
                if items.is_empty() {
                    let payload = error::type_mismatch(
                        "reduce",
                        "non-empty collection (or pass an init value)",
                        &Val::List(vec![]),
                    );
                    return throw(&env.handler_stack, payload).await;
                }
                (items[0].clone(), &items[1..])
            };
            for item in items {
                acc = invoke_fn(
                    &arities,
                    &closure,
                    &[acc, item.clone()],
                    dispatch,
                    env.handler_stack.clone(),
                )
                .await?;
            }
            Ok(acc)
        }
        _ => unreachable!(),
    }
}

/// Extract a `Val::Fn` into its arities and closure, or error.
fn extract_fn(caller: &str, val: &Val) -> Result<(Vec<FnArity>, crate::Closure), Val> {
    match val {
        Val::Fn {
            arities, closure, ..
        } => Ok((arities.clone(), closure.clone())),
        other => Err(error::type_mismatch(caller, "function", other)),
    }
}

/// Extract a sequence (list/vector/nil) into a slice reference.
fn extract_seq<'a>(caller: &str, val: &'a Val) -> Result<&'a [Val], Val> {
    match val {
        Val::Nil => Ok(&[]),
        Val::List(v) | Val::Vector(v) => Ok(v.as_slice()),
        other => Err(error::type_mismatch(caller, "collection", other)),
    }
}

// ---------------------------------------------------------------------------
// Built-in functions
// ---------------------------------------------------------------------------

/// Check whether `name` is a built-in function. If so, run it on the
/// already-evaluated `args` and return `Some(result)`.
/// Returns `None` if `name` is not a built-in — the caller should fall
/// through to host dispatch.
fn eval_builtin(name: &str, args: &[Val]) -> Option<Result<Val, Val>> {
    match name {
        // --- Collections ---
        "list" => Some(Ok(Val::List(args.to_vec()))),
        "cons" => Some(builtin_cons(args)),
        "first" => Some(builtin_first(args)),
        "rest" => Some(builtin_rest(args)),
        "count" => Some(builtin_count(args)),
        "vec" => Some(builtin_vec(args)),
        "get" => Some(builtin_get(args)),
        "assoc" => Some(builtin_assoc(args)),
        "conj" => Some(builtin_conj(args)),
        "concat" => Some(builtin_concat(args)),

        // --- Arithmetic ---
        "+" => Some(builtin_add(args)),
        "-" => Some(builtin_sub(args)),
        "*" => Some(builtin_mul(args)),
        "/" => Some(builtin_div(args)),
        "mod" => Some(builtin_mod(args)),

        // --- Comparison ---
        "=" => Some(builtin_eq(args)),
        "<" => Some(builtin_lt(args)),
        ">" => Some(builtin_gt(args)),
        "<=" => Some(builtin_le(args)),
        ">=" => Some(builtin_ge(args)),

        // --- Type ---
        "type" => {
            if args.len() != 1 {
                return Some(Err(error::arity("type", "1", args.len())));
            }
            let kw = match &args[0] {
                Val::Nil => "nil",
                Val::Bool(_) => "bool",
                Val::Int(_) => "int",
                Val::Float(_) => "float",
                Val::Str(_) => "str",
                Val::Sym(_) => "sym",
                Val::Keyword(_) => "keyword",
                Val::List(_) => "list",
                Val::Vector(_) => "vector",
                Val::Map(_) => "map",
                Val::Set(_) => "set",
                Val::Bytes(_) => "bytes",
                Val::Atom(_) => "atom",
                Val::Fn { .. } => "fn",
                Val::Macro { .. } => "macro",
                Val::NativeFn { .. } => "native-fn",
                Val::AsyncNativeFn { .. } => "async-native-fn",
                Val::Cap(_) => "cap",
            };
            Some(Ok(Val::Keyword(kw.into())))
        }
        "nil?" => {
            if args.len() != 1 {
                return Some(Err(error::arity("nil?", "1", args.len())));
            }
            Some(Ok(Val::Bool(matches!(args[0], Val::Nil))))
        }
        "some?" => {
            if args.len() != 1 {
                return Some(Err(error::arity("some?", "1", args.len())));
            }
            Some(Ok(Val::Bool(!matches!(args[0], Val::Nil))))
        }
        "map?" => {
            if args.len() != 1 {
                return Some(Err(error::arity("map?", "1", args.len())));
            }
            Some(Ok(Val::Bool(matches!(args[0], Val::Map(_)))))
        }
        "cell?" => {
            if args.len() != 1 {
                return Some(Err(error::arity("cell?", "1", args.len())));
            }
            Some(Ok(Val::Bool(crate::is_cell_tagged(&args[0]))))
        }
        "empty?" => {
            if args.len() != 1 {
                return Some(Err(error::arity("empty?", "1", args.len())));
            }
            let empty = match &args[0] {
                Val::Nil => true,
                Val::List(v) | Val::Vector(v) | Val::Set(v) => v.is_empty(),
                Val::Map(m) => m.is_empty(),
                Val::Str(s) => s.is_empty(),
                other => return Some(Err(error::type_mismatch("empty?", "collection", other))),
            };
            Some(Ok(Val::Bool(empty)))
        }
        "contains?" => Some(builtin_contains(args)),

        // --- Strings ---
        "str" => {
            let mut buf = String::new();
            for arg in args {
                use std::fmt::Write;
                let _ = match arg {
                    Val::Str(s) => write!(buf, "{s}"),
                    Val::Nil => write!(buf, ""),
                    other => write!(buf, "{other}"),
                };
            }
            Some(Ok(Val::Str(buf)))
        }
        "name" => {
            if args.len() != 1 {
                return Some(Err(error::arity("name", "1", args.len())));
            }
            match &args[0] {
                Val::Keyword(k) => Some(Ok(Val::Str(k.clone()))),
                Val::Sym(s) => Some(Ok(Val::Str(s.clone()))),
                other => Some(Err(error::type_mismatch(
                    "name",
                    "keyword or symbol",
                    other,
                ))),
            }
        }
        // --- Other ---
        "atom" => {
            if args.len() != 1 {
                return Some(Err(error::arity("atom", "1", args.len())));
            }
            Some(Ok(Val::Atom(Rc::new(RefCell::new(args[0].clone())))))
        }

        "deref" => {
            if args.len() != 1 {
                return Some(Err(error::arity("deref", "1", args.len())));
            }
            match &args[0] {
                Val::Atom(a) => Some(Ok(a.borrow().clone())),
                other => Some(Err(error::type_mismatch("deref", "atom", other))),
            }
        }

        "reset!" => {
            if args.len() != 2 {
                return Some(Err(error::arity("reset!", "2", args.len())));
            }
            match &args[0] {
                Val::Atom(a) => {
                    let new_val = args[1].clone();
                    *a.borrow_mut() = new_val.clone();
                    Some(Ok(new_val))
                }
                other => Some(Err(error::type_mismatch("reset!", "atom", other))),
            }
        }

        "gensym" => {
            if !args.is_empty() {
                return Some(Err(error::arity("gensym", "0", args.len())));
            }
            let n = GENSYM_COUNTER.fetch_add(1, Ordering::Relaxed) + 1;
            Some(Ok(Val::Sym(format!("G__{n}"))))
        }

        "ex-info" => {
            if args.len() != 2 {
                return Some(Err(crate::error::arity("ex-info", "2", args.len())));
            }
            let msg = match &args[0] {
                Val::Str(s) => s.clone(),
                other => return Some(Err(crate::error::type_mismatch("ex-info", "string", other))),
            };
            let user_map = match &args[1] {
                Val::Map(m) => m.clone(),
                other => {
                    return Some(Err(crate::error::type_mismatch(
                        "ex-info second arg",
                        "map",
                        other,
                    )))
                }
            };
            // Use the user's :type as the canonical dispatch tag.
            // Missing :type → empty Val::Str (uncatchable by tag, only
            // by wildcard) — defensive default.
            let type_tag = user_map
                .get(&Val::Keyword("type".into()))
                .cloned()
                .unwrap_or_else(|| Val::Str(String::new()));
            // Extras = user's map minus the keys ex-info canonicalizes.
            let extras = user_map
                .dissoc(&Val::Keyword("type".into()))
                .dissoc(&Val::Keyword("message".into()));
            Some(Ok(crate::error::user(type_tag, msg, extras)))
        }

        _ => None, // not a built-in
    }
}

fn builtin_contains(args: &[Val]) -> Result<Val, Val> {
    if args.len() != 2 {
        return Err(error::arity("contains?", "2", args.len()));
    }
    let found = match &args[0] {
        Val::Map(m) => m.contains_key(&args[1]),
        Val::Set(items) => items.iter().any(|v| v == &args[1]),
        Val::Vector(v) => match &args[1] {
            Val::Int(i) => *i >= 0 && (*i as usize) < v.len(),
            other => return Err(error::type_mismatch("contains? vector key", "int", other)),
        },
        other => {
            return Err(error::type_mismatch(
                "contains?",
                "map, set, or vector",
                other,
            ))
        }
    };
    Ok(Val::Bool(found))
}

// --- Collection built-ins ---

fn builtin_cons(args: &[Val]) -> Result<Val, Val> {
    if args.len() != 2 {
        return Err(error::arity("cons", "2", args.len()));
    }
    let tail = match &args[1] {
        Val::List(v) | Val::Vector(v) => v,
        other => {
            return Err(error::type_mismatch(
                "cons second arg",
                "list or vector",
                other,
            ))
        }
    };
    let mut result = Vec::with_capacity(1 + tail.len());
    result.push(args[0].clone());
    result.extend_from_slice(tail);
    Ok(Val::List(result))
}

fn builtin_first(args: &[Val]) -> Result<Val, Val> {
    if args.len() != 1 {
        return Err(error::arity("first", "1", args.len()));
    }
    match &args[0] {
        Val::Nil => Ok(Val::Nil),
        Val::List(v) | Val::Vector(v) => Ok(v.first().cloned().unwrap_or(Val::Nil)),
        other => Err(error::type_mismatch("first", "collection", other)),
    }
}

fn builtin_rest(args: &[Val]) -> Result<Val, Val> {
    if args.len() != 1 {
        return Err(error::arity("rest", "1", args.len()));
    }
    match &args[0] {
        Val::Nil => Ok(Val::List(vec![])),
        Val::List(v) | Val::Vector(v) => {
            if v.is_empty() {
                Ok(Val::List(vec![]))
            } else {
                Ok(Val::List(v[1..].to_vec()))
            }
        }
        other => Err(error::type_mismatch("rest", "collection", other)),
    }
}

fn builtin_count(args: &[Val]) -> Result<Val, Val> {
    if args.len() != 1 {
        return Err(error::arity("count", "1", args.len()));
    }
    let n = match &args[0] {
        Val::Nil => 0,
        Val::List(v) | Val::Vector(v) | Val::Set(v) => v.len(),
        Val::Map(m) => m.len(),
        Val::Str(s) => s.chars().count(),
        other => return Err(error::type_mismatch("count", "collection or nil", other)),
    };
    Ok(Val::Int(n as i64))
}

fn builtin_vec(args: &[Val]) -> Result<Val, Val> {
    if args.len() != 1 {
        return Err(error::arity("vec", "1", args.len()));
    }
    match &args[0] {
        Val::Nil => Ok(Val::Vector(vec![])),
        Val::List(v) => Ok(Val::Vector(v.clone())),
        Val::Vector(_) => Ok(args[0].clone()),
        other => Err(error::type_mismatch("vec", "list or vector", other)),
    }
}

fn builtin_get(args: &[Val]) -> Result<Val, Val> {
    if args.len() != 2 {
        return Err(error::arity("get", "2", args.len()));
    }
    match &args[0] {
        Val::Map(m) => Ok(m.get(&args[1]).cloned().unwrap_or(Val::Nil)),
        Val::Vector(v) => match &args[1] {
            Val::Int(i) => {
                if *i < 0 {
                    Ok(Val::Nil)
                } else {
                    Ok(v.get(*i as usize).cloned().unwrap_or(Val::Nil))
                }
            }
            other => Err(error::type_mismatch("get vector index", "int", other)),
        },
        Val::Nil => Ok(Val::Nil),
        other => Err(error::type_mismatch("get", "map or vector", other)),
    }
}

fn builtin_assoc(args: &[Val]) -> Result<Val, Val> {
    if args.is_empty() || !(args.len() - 1).is_multiple_of(2) {
        return Err(error::arity(
            "assoc",
            "map + key-value pairs (odd total)",
            args.len(),
        ));
    }
    let mut m = match &args[0] {
        Val::Map(m) => m.clone(),
        other => return Err(error::type_mismatch("assoc first arg", "map", other)),
    };
    for chunk in args[1..].chunks(2) {
        m = m.assoc(chunk[0].clone(), chunk[1].clone());
    }
    Ok(Val::Map(m))
}

fn builtin_conj(args: &[Val]) -> Result<Val, Val> {
    if args.len() < 2 {
        return Err(error::arity("conj", "at least 2", args.len()));
    }
    match &args[0] {
        Val::Vector(v) => {
            let mut result = v.clone();
            result.extend_from_slice(&args[1..]);
            Ok(Val::Vector(result))
        }
        Val::List(v) => {
            // Clojure: conj on lists PREPENDS each item
            let mut result = v.clone();
            for item in &args[1..] {
                result.insert(0, item.clone());
            }
            Ok(Val::List(result))
        }
        Val::Map(m) => {
            let mut result = m.clone();
            for item in &args[1..] {
                match item {
                    Val::Vector(pair) if pair.len() == 2 => {
                        result = result.assoc(pair[0].clone(), pair[1].clone());
                    }
                    other => {
                        return Err(error::type_mismatch(
                            "conj map entry",
                            "[key val] vector",
                            other,
                        ))
                    }
                }
            }
            Ok(Val::Map(result))
        }
        other => Err(error::type_mismatch("conj", "collection", other)),
    }
}

fn builtin_concat(args: &[Val]) -> Result<Val, Val> {
    let mut result = Vec::new();
    for arg in args {
        match arg {
            Val::Nil => {}
            Val::List(v) | Val::Vector(v) => result.extend(v.iter().cloned()),
            other => return Err(error::type_mismatch("concat", "sequence or nil", other)),
        }
    }
    Ok(Val::List(result))
}

// --- Arithmetic helpers ---

/// Extract a numeric pair, promoting to Float if mixed.
enum NumPair {
    Ints(i64, i64),
    Floats(f64, f64),
}

fn num_pair(a: &Val, b: &Val) -> Result<NumPair, Val> {
    match (a, b) {
        (Val::Int(x), Val::Int(y)) => Ok(NumPair::Ints(*x, *y)),
        (Val::Float(x), Val::Float(y)) => Ok(NumPair::Floats(*x, *y)),
        (Val::Int(x), Val::Float(y)) => Ok(NumPair::Floats(*x as f64, *y)),
        (Val::Float(x), Val::Int(y)) => Ok(NumPair::Floats(*x, *y as f64)),
        _ => Err(error::type_mismatch("arithmetic", "number pair", a)),
    }
}

fn builtin_add(args: &[Val]) -> Result<Val, Val> {
    let mut acc = Val::Int(0);
    for a in args {
        acc = match num_pair(&acc, a)? {
            NumPair::Ints(x, y) => Val::Int(x + y),
            NumPair::Floats(x, y) => Val::Float(x + y),
        };
    }
    Ok(acc)
}

fn builtin_sub(args: &[Val]) -> Result<Val, Val> {
    if args.is_empty() {
        return Err(error::arity("-", "at least 1", 0));
    }
    if args.len() == 1 {
        return match &args[0] {
            Val::Int(n) => Ok(Val::Int(-n)),
            Val::Float(n) => Ok(Val::Float(-n)),
            other => Err(error::type_mismatch("-", "number", other)),
        };
    }
    let mut acc = args[0].clone();
    for a in &args[1..] {
        acc = match num_pair(&acc, a)? {
            NumPair::Ints(x, y) => Val::Int(x - y),
            NumPair::Floats(x, y) => Val::Float(x - y),
        };
    }
    Ok(acc)
}

fn builtin_mul(args: &[Val]) -> Result<Val, Val> {
    let mut acc = Val::Int(1);
    for a in args {
        acc = match num_pair(&acc, a)? {
            NumPair::Ints(x, y) => Val::Int(x * y),
            NumPair::Floats(x, y) => Val::Float(x * y),
        };
    }
    Ok(acc)
}

fn builtin_div(args: &[Val]) -> Result<Val, Val> {
    if args.len() != 2 {
        return Err(error::arity("/", "2", args.len()));
    }
    match num_pair(&args[0], &args[1])? {
        NumPair::Ints(_, 0) => Err(error::internal("/", "division by zero")),
        NumPair::Ints(x, y) => Ok(Val::Int(x / y)),
        NumPair::Floats(_, 0.0) => Err(error::internal("/", "division by zero")),
        NumPair::Floats(x, y) => Ok(Val::Float(x / y)),
    }
}

fn builtin_mod(args: &[Val]) -> Result<Val, Val> {
    if args.len() != 2 {
        return Err(error::arity("mod", "2", args.len()));
    }
    match num_pair(&args[0], &args[1])? {
        NumPair::Ints(_, 0) => Err(error::internal("mod", "division by zero")),
        NumPair::Ints(x, y) => Ok(Val::Int(x % y)),
        NumPair::Floats(_, 0.0) => Err(error::internal("mod", "division by zero")),
        NumPair::Floats(x, y) => Ok(Val::Float(x % y)),
    }
}

// --- Comparison built-ins ---

fn builtin_eq(args: &[Val]) -> Result<Val, Val> {
    if args.len() != 2 {
        return Err(error::arity("=", "2", args.len()));
    }
    Ok(Val::Bool(args[0] == args[1]))
}

fn numeric_cmp(a: &Val, b: &Val) -> Result<std::cmp::Ordering, Val> {
    match (a, b) {
        (Val::Int(x), Val::Int(y)) => Ok(x.cmp(y)),
        (Val::Float(x), Val::Float(y)) => x
            .partial_cmp(y)
            .ok_or_else(|| error::internal("comparison", "NaN")),
        (Val::Int(x), Val::Float(y)) => (*x as f64)
            .partial_cmp(y)
            .ok_or_else(|| error::internal("comparison", "NaN")),
        (Val::Float(x), Val::Int(y)) => x
            .partial_cmp(&(*y as f64))
            .ok_or_else(|| error::internal("comparison", "NaN")),
        _ => Err(error::type_mismatch("comparison", "number pair", a)),
    }
}

fn builtin_lt(args: &[Val]) -> Result<Val, Val> {
    if args.len() != 2 {
        return Err(error::arity("<", "2", args.len()));
    }
    Ok(Val::Bool(numeric_cmp(&args[0], &args[1])?.is_lt()))
}

fn builtin_gt(args: &[Val]) -> Result<Val, Val> {
    if args.len() != 2 {
        return Err(error::arity(">", "2", args.len()));
    }
    Ok(Val::Bool(numeric_cmp(&args[0], &args[1])?.is_gt()))
}

fn builtin_le(args: &[Val]) -> Result<Val, Val> {
    if args.len() != 2 {
        return Err(error::arity("<=", "2", args.len()));
    }
    Ok(Val::Bool(!numeric_cmp(&args[0], &args[1])?.is_gt()))
}

fn builtin_ge(args: &[Val]) -> Result<Val, Val> {
    if args.len() != 2 {
        return Err(error::arity(">=", "2", args.len()));
    }
    Ok(Val::Bool(!numeric_cmp(&args[0], &args[1])?.is_lt()))
}

// ---------------------------------------------------------------------------
// Expr-based evaluation (new pipeline)
// ---------------------------------------------------------------------------

use crate::expr::{self, Expr};

/// Evaluate an analyzed Expr in the given environment.
pub(crate) fn eval_expr<'a, D: Dispatch>(
    expr: &'a Expr,
    env: &'a mut Env,
    dispatch: &'a D,
) -> Pin<Box<dyn Future<Output = Result<Flow, Control>> + 'a>> {
    Box::pin(async move {
        match expr {
            Expr::Const(v) => Ok(Flow::Value(v.clone())),

            Expr::Sym(s) => match resolve(env, s)? {
                Some(v) => Ok(Flow::Value(v.clone())),
                None => Ok(Flow::Value(Val::Sym(s.clone()))),
            },

            Expr::Def { name, value } => {
                let val = eval_expr(value, env, dispatch)
                    .await?
                    .into_value("def value")?;
                define_or_throw(env, name.clone(), val)
                    .await
                    .map(Flow::Value)
            }

            Expr::If { test, then, else_ } => {
                let test_val = eval_expr(test, env, dispatch)
                    .await?
                    .into_value("if condition")?;
                if is_truthy(&test_val) {
                    eval_expr(then, env, dispatch).await
                } else {
                    eval_expr(else_, env, dispatch).await
                }
            }

            Expr::Do { body } => {
                let Some((last, init)) = body.split_last() else {
                    return Ok(Flow::Value(Val::Nil));
                };
                for e in init {
                    eval_expr(e, env, dispatch).await?.into_value("do form")?;
                }
                eval_expr(last, env, dispatch).await
            }

            Expr::Let { bindings, body } => {
                env.push_frame();
                let result = async {
                    for (binding, val_expr) in bindings {
                        let val = eval_expr(val_expr, env, dispatch)
                            .await?
                            .into_value("let binding")?;
                        match binding {
                            crate::pattern::LetBinding::Simple(name) => {
                                env.set(name.clone(), val);
                            }
                            crate::pattern::LetBinding::Destructure(pat) => {
                                try_throw!(
                                    env,
                                    crate::pattern::bind_pattern(
                                        pat,
                                        &val,
                                        "let",
                                        &mut |name, v| {
                                            env.set(name.to_string(), v);
                                        }
                                    )
                                );
                            }
                        }
                    }
                    let Some((last, init)) = body.split_last() else {
                        return Ok(Flow::Value(Val::Nil));
                    };
                    for e in init {
                        eval_expr(e, env, dispatch)
                            .await?
                            .into_value("let body form")?;
                    }
                    eval_expr(last, env, dispatch).await
                }
                .await;
                env.pop_frame();
                result
            }

            Expr::Quote(val) => Ok(Flow::Value(val.clone())),

            Expr::Fn { arities } => {
                // Convert FnArityExpr → FnArity with FnBody::Analyzed
                let free_vars: BTreeSet<&String> = arities
                    .iter()
                    .flat_map(|arity| arity.free_vars.iter())
                    .collect();
                let captured = CapturedEnv::capture_free(env, free_vars);
                let (is_cap_free, cap_violation) = compute_cap_status(&captured);
                let fn_arities: Vec<FnArity> = arities
                    .iter()
                    .map(|a| FnArity {
                        params: a.params.clone(),
                        variadic: a.variadic.clone(),
                        body: FnBody::Analyzed(a.body.clone()),
                    })
                    .collect();
                Ok(Flow::Value(Val::Fn {
                    arities: fn_arities,
                    closure: crate::Closure {
                        captured: Rc::new(captured),
                        owner: OwnerRef::Strong(Rc::clone(&env.defs)),
                    },
                    is_cap_free,
                    cap_violation,
                }))
            }

            Expr::Loop { bindings, body } => {
                env.push_frame();
                // Evaluate initial bindings — track binding specs for recur
                let mut binding_specs: Vec<crate::pattern::LetBinding> =
                    Vec::with_capacity(bindings.len());
                for (binding, val_expr) in bindings {
                    let val = eval_expr(val_expr, env, dispatch)
                        .await?
                        .into_value("loop binding")?;
                    match binding {
                        crate::pattern::LetBinding::Simple(name) => {
                            env.set(name.clone(), val);
                            binding_specs.push(crate::pattern::LetBinding::Simple(name.clone()));
                        }
                        crate::pattern::LetBinding::Destructure(pat) => {
                            try_throw!(
                                env,
                                crate::pattern::bind_pattern(pat, &val, "loop", &mut |name, v| {
                                    env.set(name.to_string(), v);
                                })
                            );
                            binding_specs
                                .push(crate::pattern::LetBinding::Destructure(pat.clone()));
                        }
                    }
                }
                let num_bindings = binding_specs.len();

                let result = async {
                    loop {
                        // Only the last body form is in tail position.
                        let result = match body.split_last() {
                            None => Flow::Value(Val::Nil),
                            Some((last, init)) => {
                                for e in init {
                                    eval_expr(e, env, dispatch)
                                        .await?
                                        .into_value("loop body form")?;
                                }
                                eval_expr(last, env, dispatch).await?
                            }
                        };
                        match result {
                            Flow::Recur(new_vals) => {
                                if new_vals.len() != num_bindings {
                                    let payload = error::arity(
                                        "recur",
                                        &num_bindings.to_string(),
                                        new_vals.len(),
                                    );
                                    return throw(&env.handler_stack, payload)
                                        .await
                                        .map(Into::into);
                                }
                                // Re-bind: re-apply patterns for destructuring bindings
                                for (spec, val) in binding_specs.iter().zip(new_vals) {
                                    match spec {
                                        crate::pattern::LetBinding::Simple(name) => {
                                            env.set(name.clone(), val);
                                        }
                                        crate::pattern::LetBinding::Destructure(pat) => {
                                            try_throw!(
                                                env,
                                                crate::pattern::bind_pattern(
                                                    pat,
                                                    &val,
                                                    "recur",
                                                    &mut |name, v| {
                                                        env.set(name.to_string(), v);
                                                    },
                                                )
                                            );
                                        }
                                    }
                                }
                            }
                            Flow::Value(v) => return Ok(Flow::Value(v)),
                        }
                    }
                }
                .await;
                env.pop_frame();
                result
            }

            Expr::Recur { args } => {
                let mut evaled = Vec::with_capacity(args.len());
                for a in args {
                    evaled.push(
                        eval_expr(a, env, dispatch)
                            .await?
                            .into_value("recur argument")?,
                    );
                }
                Ok(Flow::Recur(evaled))
            }

            Expr::Perform { target, args } => {
                let target_val = eval_expr(target, env, dispatch)
                    .await?
                    .into_value("perform target")?;
                let mut evaled_args = Vec::with_capacity(args.len());
                for a in args {
                    evaled_args.push(
                        eval_expr(a, env, dispatch)
                            .await?
                            .into_value("perform argument")?,
                    );
                }

                // Build EffectTarget + data payload from the two perform forms.
                let (effect_target, data_val) = match &target_val {
                    // (perform :keyword data) — keyword/environmental effect
                    Val::Keyword(s) => {
                        if evaled_args.len() != 1 {
                            let payload = error::arity(
                                "perform (keyword effect)",
                                "1 data arg",
                                evaled_args.len(),
                            );
                            return throw(&env.handler_stack, payload).await.map(Into::into);
                        }
                        (
                            effect::EffectTarget::Keyword(s.clone()),
                            evaled_args.into_iter().next().unwrap(),
                        )
                    }
                    // (perform cap :method args...) — cap-targeted effect
                    Val::Cap(_) => {
                        return perform_cap_value(&target_val, &evaled_args, env, dispatch)
                            .await
                            .map(Flow::Value)
                    }
                    other => {
                        let payload =
                            error::type_mismatch("perform target", "keyword or cap", other);
                        return throw(&env.handler_stack, payload).await.map(Into::into);
                    }
                };

                // Stack walk: find the matching handler frame.
                perform_dispatch(&env.handler_stack, effect_target, data_val)
                    .await
                    .map(Flow::Value)
            }

            Expr::PerformStar { target, payload } => {
                // Apply-style perform: the payload list's elements are the
                // args `perform` would take. Lets a generic handler delegate
                // its `(method args...)` payload without knowing the arity.
                let target_val = eval_expr(target, env, dispatch)
                    .await?
                    .into_value("perform* target")?;
                let payload_val = eval_expr(payload, env, dispatch)
                    .await?
                    .into_value("perform* payload")?;
                let items: Vec<Val> = match payload_val {
                    Val::List(v) | Val::Vector(v) => v,
                    other => {
                        let payload =
                            error::type_mismatch("perform* payload", "list or vector", &other);
                        return throw(&env.handler_stack, payload).await.map(Into::into);
                    }
                };
                match &target_val {
                    Val::Cap(_) => perform_cap_value(&target_val, &items, env, dispatch)
                        .await
                        .map(Flow::Value),
                    Val::Keyword(s) => {
                        if items.len() != 1 {
                            let payload = error::arity(
                                "perform* (keyword effect)",
                                "payload of 1 element",
                                items.len(),
                            );
                            return throw(&env.handler_stack, payload).await.map(Into::into);
                        }
                        perform_dispatch(
                            &env.handler_stack,
                            effect::EffectTarget::Keyword(s.clone()),
                            items.into_iter().next().unwrap(),
                        )
                        .await
                        .map(Flow::Value)
                    }
                    other => {
                        let payload =
                            error::type_mismatch("perform* target", "keyword or cap", other);
                        throw(&env.handler_stack, payload).await.map(Into::into)
                    }
                }
            }

            Expr::Match { expr, clauses } => {
                // Evaluate the scrutinee
                let value = eval_expr(expr, env, dispatch)
                    .await?
                    .into_value("match scrutinee")?;

                // Try each clause in order (linear, first match wins)
                for (pattern, body) in clauses {
                    if let Some(bindings) = crate::pattern::match_pattern(pattern, &value) {
                        // Push new frame with pattern bindings
                        env.push_frame();
                        for (name, val) in bindings {
                            env.set(name, val);
                        }
                        // Clause bodies are in tail position.
                        let result = eval_expr(body, env, dispatch).await;
                        env.pop_frame();
                        return result;
                    }
                }

                // No clause matched — catchable exception
                let payload = error::internal("match", format!("no clause matched value {value}"));
                throw(&env.handler_stack, payload).await.map(Into::into)
            }

            Expr::WithEffectHandler {
                target,
                handler,
                body,
            } => {
                // Evaluate target and handler BEFORE pushing context.
                let target_val = eval_expr(target, env, dispatch)
                    .await?
                    .into_value("with-effect-handler target")?;
                let handler_val = eval_expr(handler, env, dispatch)
                    .await?
                    .into_value("with-effect-handler handler")?;

                let effect_target = match &target_val {
                    Val::Keyword(s) => effect::EffectTarget::Keyword(s.clone()),
                    Val::Cap(h) => h.effect_target(),
                    other => {
                        let payload = error::type_mismatch(
                            "with-effect-handler target",
                            "keyword or cap",
                            other,
                        );
                        return throw(&env.handler_stack, payload).await.map(Into::into);
                    }
                };

                // Depth check — fires BEFORE the frame is pushed, so state is
                // consistent and the exception is catchable by outer handlers.
                let hs = env.handler_stack.clone();
                let caller_hs = hs.clone();
                if hs.borrow().len() >= effect::MAX_HANDLER_DEPTH {
                    let payload = error::internal(
                        "with-effect-handler",
                        format!(
                            "handler stack depth limit ({}) exceeded",
                            effect::MAX_HANDLER_DEPTH
                        ),
                    );
                    return throw(&env.handler_stack, payload).await.map(Into::into);
                }

                // Create handler context with the target.
                let ctx = Rc::new(RefCell::new(effect::HandlerContext {
                    slot: Rc::new(RefCell::new(effect::EffectSlot::new())),
                    target: effect_target,
                }));
                hs.borrow_mut().push(ctx.clone());
                let _guard = HandlerFrameGuard {
                    stack: hs.clone(),
                    context: ctx.clone(),
                };

                // Create body future. The last body form is in tail
                // position (a loop enclosing this form may catch its recur).
                let mut body_fut = {
                    let body = body.clone();
                    Box::pin(async move {
                        let Some((last, init)) = body.split_last() else {
                            return Ok(Flow::Value(Val::Nil));
                        };
                        for e in init {
                            eval_expr(e, env, dispatch)
                                .await?
                                .into_value("with-effect-handler body form")?;
                        }
                        eval_expr(last, env, dispatch).await
                    })
                };

                // State machine: alternate between polling body and handling effects.
                enum HandlerState<'b> {
                    Polling,
                    Handling(Pin<Box<dyn Future<Output = Result<Val, Control>> + 'b>>),
                }
                let mut state = HandlerState::Polling;

                let result: Result<Flow, Control> = std::future::poll_fn(|cx| {
                    loop {
                        match &mut state {
                            HandlerState::Polling => {
                                match body_fut.as_mut().poll(cx) {
                                    Poll::Ready(result) => return Poll::Ready(result),
                                    Poll::Pending => {
                                        let pending = ctx.borrow().slot.borrow_mut().pending.take();
                                        match pending {
                                            Some((_target, data, resume_tx)) => {
                                                // Dispatch to handler based on its type.
                                                match &handler_val {
                                                    Val::Fn {
                                                        arities, closure, ..
                                                    } => {
                                                        // Pop before handle (handler's performs go to outer handlers).
                                                        hs.borrow_mut().pop();

                                                        let has_2_arity = arities.iter().any(|a| {
                                                            (a.variadic.is_none()
                                                                && a.params.len() == 2)
                                                                || (a.variadic.is_some()
                                                                    && a.params.len() <= 2)
                                                        });
                                                        let owned_arities = arities.clone();
                                                        let owned_closure = closure.clone();

                                                        let handler_fut: Pin<
                                                            Box<
                                                                dyn Future<
                                                                        Output = Result<
                                                                            Val,
                                                                            Control,
                                                                        >,
                                                                    > + '_,
                                                            >,
                                                        > = if has_2_arity {
                                                            let resume_fn =
                                                                effect::make_resume_fn(resume_tx);
                                                            let args = vec![data, resume_fn];
                                                            let handler_hs = caller_hs.clone();
                                                            Box::pin(async move {
                                                                invoke_fn(
                                                                    &owned_arities,
                                                                    &owned_closure,
                                                                    &args,
                                                                    dispatch,
                                                                    handler_hs,
                                                                )
                                                                .await
                                                            })
                                                        } else {
                                                            drop(resume_tx);
                                                            let args = vec![data];
                                                            let handler_hs = caller_hs.clone();
                                                            Box::pin(async move {
                                                                invoke_fn(
                                                                    &owned_arities,
                                                                    &owned_closure,
                                                                    &args,
                                                                    dispatch,
                                                                    handler_hs,
                                                                )
                                                                .await
                                                            })
                                                        };

                                                        state = HandlerState::Handling(handler_fut);
                                                        continue;
                                                    }
                                                    Val::NativeFn { func, .. } => {
                                                        hs.borrow_mut().pop();
                                                        let resume_fn =
                                                            effect::make_resume_fn(resume_tx);
                                                        let func = func.clone();
                                                        let handler_hs = caller_hs.clone();
                                                        let handler_fut: Pin<
                                                            Box<
                                                                dyn Future<
                                                                        Output = Result<
                                                                            Val,
                                                                            Control,
                                                                        >,
                                                                    > + '_,
                                                            >,
                                                        > = Box::pin(async move {
                                                            let args = [data, resume_fn];
                                                            settle_native(&handler_hs, func(&args))
                                                                .await
                                                        });
                                                        state = HandlerState::Handling(handler_fut);
                                                        continue;
                                                    }
                                                    Val::AsyncNativeFn { func, .. } => {
                                                        hs.borrow_mut().pop();
                                                        let resume_fn =
                                                            effect::make_resume_fn(resume_tx);
                                                        let func = func.clone();
                                                        let handler_hs = caller_hs.clone();
                                                        let handler_fut: Pin<
                                                            Box<
                                                                dyn Future<
                                                                        Output = Result<
                                                                            Val,
                                                                            Control,
                                                                        >,
                                                                    > + '_,
                                                            >,
                                                        > = Box::pin(async move {
                                                            let fut = func(vec![data, resume_fn]);
                                                            settle_native(&handler_hs, fut.await)
                                                                .await
                                                        });
                                                        state = HandlerState::Handling(handler_fut);
                                                        continue;
                                                    }
                                                    other => {
                                                        // Pop so the exception cannot dispatch
                                                        // back into this broken frame.
                                                        hs.borrow_mut().pop();
                                                        drop(resume_tx);
                                                        let payload = error::type_mismatch(
                                                            "with-effect-handler handler",
                                                            "function",
                                                            other,
                                                        );
                                                        let handler_hs = caller_hs.clone();
                                                        let handler_fut: Pin<
                                                            Box<
                                                                dyn Future<
                                                                        Output = Result<
                                                                            Val,
                                                                            Control,
                                                                        >,
                                                                    > + '_,
                                                            >,
                                                        > = Box::pin(async move {
                                                            throw(&handler_hs, payload).await
                                                        });
                                                        state = HandlerState::Handling(handler_fut);
                                                        continue;
                                                    }
                                                }
                                            }
                                            None => return Poll::Pending,
                                        }
                                    }
                                }
                            }
                            HandlerState::Handling(handler_fut) => {
                                match handler_fut.as_mut().poll(cx) {
                                    Poll::Pending => return Poll::Pending,
                                    Poll::Ready(result) => {
                                        hs.borrow_mut().push(ctx.clone());
                                        match result {
                                            Err(Control::Resume(_)) => {
                                                state = HandlerState::Polling;
                                                cx.waker().wake_by_ref();
                                                return Poll::Pending;
                                            }
                                            other => return Poll::Ready(other.map(Flow::Value)),
                                        }
                                    }
                                }
                            }
                        }
                    }
                })
                .await;

                result
            }

            Expr::DefMacro { name, raw_args } => {
                // raw_args contains [params, body...] — no name (already extracted).
                let arities = try_throw!(env, parse_macro_arities(raw_args));
                let captured = CapturedEnv::capture_all(env);
                let (is_cap_free, cap_violation) = compute_cap_status(&captured);
                let val = Val::Macro {
                    arities,
                    closure: crate::Closure {
                        captured: Rc::new(captured),
                        owner: OwnerRef::Strong(Rc::clone(&env.defs)),
                    },
                    is_cap_free,
                    cap_violation,
                };
                define_or_throw(env, name.clone(), val)
                    .await
                    .map(Flow::Value)
            }

            Expr::Call {
                head,
                args,
                raw_args,
            } => {
                // Special form: (defcap name :method fn ...)
                if head == "defcap" {
                    if raw_args.len() < 3 {
                        let payload = error::arity("defcap", "at least 3", raw_args.len());
                        return throw(&env.handler_stack, payload).await.map(Into::into);
                    }
                    let name = match &raw_args[0] {
                        Val::Sym(s) => s.clone(),
                        other => {
                            let payload = error::type_mismatch("defcap name", "symbol", other);
                            return throw(&env.handler_stack, payload).await.map(Into::into);
                        }
                    };
                    if (raw_args.len() - 1) % 2 != 0 {
                        let payload = error::internal(
                            "defcap",
                            "method definitions must be keyword/function pairs",
                        );
                        return throw(&env.handler_stack, payload).await.map(Into::into);
                    }

                    let mut methods = HashMap::new();
                    let mut method_names = BTreeSet::new();
                    for pair in raw_args[1..].chunks(2) {
                        let method_name = match &pair[0] {
                            Val::Keyword(k) => k.clone(),
                            other => {
                                let payload =
                                    error::type_mismatch("defcap method name", "keyword", other);
                                return throw(&env.handler_stack, payload).await.map(Into::into);
                            }
                        };
                        let method_expr =
                            try_throw!(env, expr::analyze(&pair[1]).map_err(Val::from));
                        let method_val = eval_expr(&method_expr, env, dispatch)
                            .await?
                            .into_value("defcap method")?;
                        if !matches!(
                            method_val,
                            Val::Fn { .. } | Val::NativeFn { .. } | Val::AsyncNativeFn { .. }
                        ) {
                            let payload = error::type_mismatch(
                                "defcap method value",
                                "function",
                                &method_val,
                            );
                            return throw(&env.handler_stack, payload).await.map(Into::into);
                        }
                        method_names.insert(method_name.clone());
                        methods.insert(method_name, method_val);
                    }

                    let schema_cid = "glia:defcap:v1".to_string();
                    let descriptor = cap_descriptor_bytes(&name, &schema_cid, &method_names);
                    let cap = make_cap(
                        name.clone(),
                        schema_cid,
                        Rc::new(GliaCapInner {
                            methods,
                            descriptor,
                        }),
                    );
                    let cap = define_or_throw(env, name, cap).await?;
                    return Ok(Flow::Value(cap));
                }

                // Special form: (attenuate cap [:method ...])
                if head == "attenuate" {
                    if args.len() != 2 {
                        let payload = error::arity("attenuate", "2", args.len());
                        return throw(&env.handler_stack, payload).await.map(Into::into);
                    }
                    let cap_val = eval_expr(&args[0], env, dispatch)
                        .await?
                        .into_value("attenuate cap")?;
                    let allow_val = eval_expr(&args[1], env, dispatch)
                        .await?
                        .into_value("attenuate methods")?;
                    let mut allow_methods = try_throw!(env, parse_allow_methods(&allow_val));

                    // Boundary-crossing caps first: the embedder may reify the
                    // attenuation into real enforcement (the kernel wraps
                    // capnp-backed caps in a hook-level membrane). `None`
                    // means "not mine" — fall through to the evaluator-local
                    // interposition path below.
                    if let Some(reified) = dispatch.reify_attenuation(&cap_val, &allow_methods) {
                        return Ok(Flow::Value(try_throw!(env, reified)));
                    }

                    let (name, schema_cid, base, nested_allow): (
                        String,
                        String,
                        Val,
                        Option<BTreeSet<String>>,
                    ) = match &cap_val {
                        Val::Cap(h) => {
                            if let Some(inner_att) = h.inner().downcast_ref::<AttenuatedCapInner>()
                            {
                                (
                                    h.name().to_string(),
                                    h.schema_cid().to_string(),
                                    inner_att.base.clone(),
                                    Some(inner_att.allow_methods.clone()),
                                )
                            } else {
                                (
                                    h.name().to_string(),
                                    h.schema_cid().to_string(),
                                    cap_val.clone(),
                                    None,
                                )
                            }
                        }
                        other => {
                            let payload = error::type_mismatch("attenuate first arg", "cap", other);
                            return throw(&env.handler_stack, payload).await.map(Into::into);
                        }
                    };

                    if let Some(existing) = nested_allow {
                        allow_methods = allow_methods.intersection(&existing).cloned().collect();
                    }

                    let descriptor = cap_descriptor_bytes(&name, &schema_cid, &allow_methods);
                    return Ok(Flow::Value(make_cap(
                        name,
                        schema_cid,
                        Rc::new(AttenuatedCapInner {
                            base,
                            allow_methods,
                            descriptor,
                        }),
                    )));
                }

                // 1. Check for macro expansion
                if let Some(Val::Macro {
                    arities, closure, ..
                }) = resolve(env, head)?
                {
                    let arities = arities.clone();
                    let closure = closure.clone();
                    let expanded = invoke_macro(
                        &arities,
                        &closure,
                        raw_args,
                        dispatch,
                        env.handler_stack.clone(),
                    )
                    .await?;
                    // Re-analyze and eval the expanded form (tail position:
                    // the expansion may recur into an enclosing loop).
                    let analyzed = try_throw!(env, expr::analyze(&expanded).map_err(Val::from));
                    return eval_expr(&analyzed, env, dispatch).await;
                }

                // 2. Check env for fn or native-fn
                if let Some(Val::Fn {
                    arities, closure, ..
                }) = resolve(env, head)?
                {
                    let arities = arities.clone();
                    let closure = closure.clone();
                    let evaled_args = eval_expr_args(args, env, dispatch).await?;
                    return invoke_fn(
                        &arities,
                        &closure,
                        &evaled_args,
                        dispatch,
                        env.handler_stack.clone(),
                    )
                    .await
                    .map(Flow::Value);
                }
                if let Some(Val::NativeFn { func, .. }) = resolve(env, head)? {
                    let func = func.clone();
                    let evaled_args = eval_expr_args(args, env, dispatch).await?;
                    return settle_native(&env.handler_stack, func(&evaled_args))
                        .await
                        .map(Flow::Value);
                }
                if let Some(Val::AsyncNativeFn { func, .. }) = resolve(env, head)? {
                    let func = func.clone();
                    let evaled_args = eval_expr_args(args, env, dispatch).await?;
                    let result = func(evaled_args).await;
                    return settle_native(&env.handler_stack, result)
                        .await
                        .map(Flow::Value);
                }

                // 3b. `cell` needs the unevaluated grant-map syntax for
                // duplicate detection, so it owns argument evaluation.
                if head == "cell" {
                    return eval_cell_expr(args, raw_args, env, dispatch)
                        .await
                        .map(Flow::Value);
                }

                // 3. Evaluate args for remaining paths
                let evaled_args = eval_expr_args(args, env, dispatch).await?;

                // 4. HOF builtins
                if head == "map" || head == "filter" || head == "reduce" {
                    return eval_hof(head, &evaled_args, env, dispatch)
                        .await
                        .map(Flow::Value);
                }

                // 5. Sync builtins
                if let Some(result) = eval_builtin(head, &evaled_args) {
                    return Ok(Flow::Value(try_throw!(env, result)));
                }

                // 6. Generic dispatch
                let result = dispatch.call(head, &evaled_args).await;
                settle_native(&env.handler_stack, result)
                    .await
                    .map(Flow::Value)
            }

            Expr::Apply { args } => {
                let evaled = eval_expr_args(args, env, dispatch).await?;
                if evaled.len() < 2 {
                    let payload = error::arity("apply", "at least 2", evaled.len());
                    return throw(&env.handler_stack, payload).await.map(Into::into);
                }
                let func = &evaled[0];
                let last = &evaled[evaled.len() - 1];
                let trailing = match last {
                    Val::List(v) | Val::Vector(v) => v.clone(),
                    other => {
                        let payload =
                            error::type_mismatch("apply last arg", "list or vector", other);
                        return throw(&env.handler_stack, payload).await.map(Into::into);
                    }
                };
                let mut spread = evaled[1..evaled.len() - 1].to_vec();
                spread.extend(trailing);

                match func {
                    Val::Sym(fname) => {
                        if let Some(Val::Fn {
                            arities, closure, ..
                        }) = resolve(env, fname)?
                        {
                            let arities = arities.clone();
                            let closure = closure.clone();
                            return invoke_fn(
                                &arities,
                                &closure,
                                &spread,
                                dispatch,
                                env.handler_stack.clone(),
                            )
                            .await
                            .map(Flow::Value);
                        }
                        if let Some(Val::NativeFn { func, .. }) = resolve(env, fname)? {
                            let func = func.clone();
                            return settle_native(&env.handler_stack, func(&spread))
                                .await
                                .map(Flow::Value);
                        }
                        if let Some(Val::AsyncNativeFn { func, .. }) = resolve(env, fname)? {
                            let result = func.clone()(spread).await;
                            return settle_native(&env.handler_stack, result)
                                .await
                                .map(Flow::Value);
                        }
                        if let Some(result) = eval_builtin(fname, &spread) {
                            return Ok(Flow::Value(try_throw!(env, result)));
                        }
                        let result = dispatch.call(fname, &spread).await;
                        settle_native(&env.handler_stack, result)
                            .await
                            .map(Flow::Value)
                    }
                    Val::Fn {
                        arities, closure, ..
                    } => {
                        let arities = arities.clone();
                        let closure = closure.clone();
                        invoke_fn(
                            &arities,
                            &closure,
                            &spread,
                            dispatch,
                            env.handler_stack.clone(),
                        )
                        .await
                        .map(Flow::Value)
                    }
                    Val::NativeFn { func, .. } => settle_native(&env.handler_stack, func(&spread))
                        .await
                        .map(Flow::Value),
                    Val::AsyncNativeFn { func, .. } => {
                        let result = func(spread).await;
                        settle_native(&env.handler_stack, result)
                            .await
                            .map(Flow::Value)
                    }
                    other => {
                        let payload =
                            error::type_mismatch("apply first arg", "symbol or fn", other);
                        throw(&env.handler_stack, payload).await.map(Into::into)
                    }
                }
            }

            Expr::Vector(exprs) => {
                let mut items = Vec::with_capacity(exprs.len());
                for e in exprs {
                    items.push(
                        eval_expr(e, env, dispatch)
                            .await?
                            .into_value("vector element")?,
                    );
                }
                Ok(Flow::Value(Val::Vector(items)))
            }

            Expr::Map(pairs) => {
                let mut items = Vec::with_capacity(pairs.len());
                for (k, v) in pairs {
                    items.push((
                        eval_expr(k, env, dispatch).await?.into_value("map key")?,
                        eval_expr(v, env, dispatch).await?.into_value("map value")?,
                    ));
                }
                Ok(Flow::Value(Val::Map(ValMap::from_pairs(items))))
            }

            Expr::Set(exprs) => {
                let mut items = Vec::with_capacity(exprs.len());
                for e in exprs {
                    items.push(
                        eval_expr(e, env, dispatch)
                            .await?
                            .into_value("set element")?,
                    );
                }
                Ok(Flow::Value(Val::Set(items)))
            }
        }
    })
}

/// Evaluate a list of Expr args into Vec<Val>.
async fn eval_expr_args<'a, D: Dispatch>(
    args: &'a [Expr],
    env: &'a mut Env,
    dispatch: &'a D,
) -> Result<Vec<Val>, Control> {
    let mut result = Vec::with_capacity(args.len());
    for a in args {
        result.push(
            eval_expr(a, env, dispatch)
                .await?
                .into_value("function argument")?,
        );
    }
    Ok(result)
}

fn cap_method_and_args(args: &[Val], ctx: &'static str) -> Result<(String, Vec<Val>), Val> {
    let method = match args.first() {
        Some(Val::Keyword(k)) => k.clone(),
        Some(other) => return Err(error::type_mismatch(ctx, "keyword method", other)),
        None => return Err(error::arity(ctx, "at least 1", 0)),
    };
    Ok((method, args[1..].to_vec()))
}

async fn invoke_cap_method_value<'a, D: Dispatch>(
    method_val: Val,
    args: &[Val],
    env: &'a mut Env,
    dispatch: &'a D,
) -> Result<Val, Control> {
    match method_val {
        Val::Fn {
            arities, closure, ..
        } => {
            invoke_fn_with_handler_stack(
                &arities,
                &closure,
                args,
                dispatch,
                env.handler_stack.clone(),
            )
            .await
        }
        Val::NativeFn { func, .. } => settle_native(&env.handler_stack, func(args)).await,
        Val::AsyncNativeFn { func, .. } => {
            let result = func(args.to_vec()).await;
            settle_native(&env.handler_stack, result).await
        }
        other => {
            let payload = error::type_mismatch("defcap method", "function", &other);
            throw(&env.handler_stack, payload).await
        }
    }
}

async fn perform_cap_value<'a, D: Dispatch>(
    cap: &Val,
    args: &[Val],
    env: &'a mut Env,
    dispatch: &'a D,
) -> Result<Val, Control> {
    let mut current = cap.clone();
    let payload = args.to_vec();

    loop {
        let Val::Cap(handle) = &current else {
            let payload = error::type_mismatch("perform target", "cap", &current);
            return throw(&env.handler_stack, payload).await;
        };
        let handle = handle.clone();

        let effect_target = handle.effect_target();
        match perform_dispatch(
            &env.handler_stack,
            effect_target.clone(),
            Val::List(payload.clone()),
        )
        .await
        {
            Ok(value) => return Ok(value),
            // Our own dispatch came back unhandled — fall through to the
            // cap's intrinsic behavior below.
            Err(Control::Unhandled(req)) if req.target.matches(&effect_target) => {}
            Err(err) => return Err(err),
        }

        // A cap that carries its own handler: invoke it directly with the
        // stack-handler protocol `(payload resume)`, `resume` bound to the
        // identity continuation. The handler stack above keeps interposition
        // priority; this is the cap's intrinsic behavior when nothing
        // interposes.
        if let Some(handled) = handle.inner().downcast_ref::<HandledCapInner>() {
            let handler = handled.handler.clone();
            let resume = Val::NativeFn {
                name: "resume".into(),
                func: Rc::new(|args: &[Val]| {
                    args.first()
                        .cloned()
                        .ok_or_else(|| NativeSignal::throw(error::arity("resume", "1", 0)))
                }),
            };
            return invoke_cap_method_value(
                handler,
                &[Val::List(payload.clone()), resume],
                env,
                dispatch,
            )
            .await;
        }

        if let Some(attenuated) = handle.inner().downcast_ref::<AttenuatedCapInner>() {
            let (method, _) = try_throw!(
                env,
                cap_method_and_args(&payload, "perform (attenuated cap)")
            );
            if !attenuated.allow_methods.contains(&method) {
                let payload = error::permission_denied(
                    &format!(
                        "method :{method} denied by attenuation policy on '{}'",
                        handle.name()
                    ),
                    None,
                );
                return throw(&env.handler_stack, payload).await;
            }
            current = attenuated.base.clone();
            continue;
        }

        if let Some(glia_cap) = handle.inner().downcast_ref::<GliaCapInner>() {
            let (method, method_args) =
                try_throw!(env, cap_method_and_args(&payload, "perform (defcap)"));
            let method_val = match glia_cap.methods.get(&method).cloned() {
                Some(v) => v,
                None => {
                    let payload = error::permission_denied(
                        &format!(
                            "method :{method} is not available on capability '{}'",
                            handle.name()
                        ),
                        None,
                    );
                    return throw(&env.handler_stack, payload).await;
                }
            };
            return invoke_cap_method_value(method_val, &method_args, env, dispatch).await;
        }

        return Err(Control::Unhandled(Box::new(effect::EffectRequest {
            target: effect_target,
            data: Val::List(payload),
        })));
    }
}

/// Top-level Expr evaluation boundary: seals internal control into
/// [`EvalError`] and converts a stray top-level recur into a language fault.
pub fn eval_toplevel_expr<'a, D: Dispatch>(
    expr: &'a Expr,
    env: &'a mut Env,
    dispatch: &'a D,
) -> Pin<Box<dyn Future<Output = Result<Val, EvalError>> + 'a>> {
    Box::pin(async move { seal(eval_expr(expr, env, dispatch).await) })
}

/// Top-level evaluation wrapper.
///
/// Analyzes the Val into an Expr, then evaluates it, sealing internal
/// control into [`EvalError`] at the boundary. Analysis failures are
/// catchable exceptions raised at the top-level dynamic position.
pub fn eval_toplevel<'a, D: Dispatch>(
    val: &'a Val,
    env: &'a mut Env,
    dispatch: &'a D,
) -> Pin<Box<dyn Future<Output = Result<Val, EvalError>> + 'a>> {
    Box::pin(async move {
        let analyzed = match expr::analyze(val) {
            Ok(expr) => expr,
            Err(msg) => {
                let hs = env.handler_stack.clone();
                return seal(throw(&hs, Val::from(msg)).await.map(Flow::Value));
            }
        };
        seal(eval_expr(&analyzed, env, dispatch).await)
    })
}

/// Result of an embedding-owned top-level evaluation.
#[derive(Clone, Debug)]
pub enum EvalOutcome {
    Value(Val),
    Exit,
}

/// Removes embedding-owned frames even when the enclosing evaluation future is
/// cancelled. Guest `with-effect-handler` frames use `HandlerFrameGuard` for
/// the same cancellation-safe cleanup.
struct HostFrameGuard {
    stack: HandlerStack,
    contexts: Vec<Rc<RefCell<effect::HandlerContext>>>,
}

impl Drop for HostFrameGuard {
    fn drop(&mut self) {
        self.stack
            .borrow_mut()
            .retain(|frame| !self.contexts.iter().any(|ctx| Rc::ptr_eq(frame, ctx)));
    }
}

/// Removes a guest-owned handler frame if evaluation exits through an error,
/// an embedding abort, or cancellation before `with-effect-handler` reaches
/// its ordinary cleanup path.
struct HandlerFrameGuard {
    stack: HandlerStack,
    context: Rc<RefCell<effect::HandlerContext>>,
}

impl Drop for HandlerFrameGuard {
    fn drop(&mut self) {
        self.stack
            .borrow_mut()
            .retain(|frame| !Rc::ptr_eq(frame, &self.context));
    }
}

/// Evaluate a form with Rust-owned default keyword-effect frames.
///
/// The frames are dynamic but never guest-visible: they are installed before
/// evaluation, so ordinary Glia `with-effect-handler` frames remain newer and
/// therefore interpose first.  This mirrors the existing handler poll loop,
/// including genuine-pending propagation, while allowing `:exit` to abort by
/// dropping the suspended body future rather than smuggling a sentinel through
/// guest evaluation.
pub fn eval_toplevel_with_host_effects<'a, D: Dispatch>(
    val: &'a Val,
    env: &'a mut Env,
    dispatch: &'a D,
    host_effects: &'a [effect::HostEffect],
) -> Pin<Box<dyn Future<Output = Result<EvalOutcome, EvalError>> + 'a>> {
    Box::pin(async move {
        let hs = env.handler_stack.clone();
        let contexts: Vec<Rc<RefCell<effect::HandlerContext>>> = host_effects
            .iter()
            .map(|effect| {
                Rc::new(RefCell::new(effect::HandlerContext {
                    slot: Rc::new(RefCell::new(effect::EffectSlot::new())),
                    target: effect.target.clone(),
                }))
            })
            .collect();
        hs.borrow_mut().extend(contexts.iter().cloned());
        let _guard = HostFrameGuard {
            stack: hs,
            contexts: contexts.clone(),
        };

        let mut body = eval_toplevel(val, env, dispatch);
        let mut handling: Option<(crate::oneshot::Sender, effect::HostEffectFuture)> = None;

        let result = std::future::poll_fn(|cx| loop {
            if let Some((_, future)) = handling.as_mut() {
                match future.as_mut().poll(cx) {
                    Poll::Pending => return Poll::Pending,
                    Poll::Ready(Ok(effect::HostEffectResult::Resume(value))) => {
                        let (tx, _) = handling.take().expect("host handler state");
                        tx.send(value);
                        cx.waker().wake_by_ref();
                        continue;
                    }
                    Poll::Ready(Ok(effect::HostEffectResult::Exit)) => {
                        return Poll::Ready(Ok(EvalOutcome::Exit));
                    }
                    // A host-effect handler failure is an embedder fault: it
                    // aborts evaluation and bypasses guest handlers.
                    Poll::Ready(Err(err)) => {
                        return Poll::Ready(Err(EvalError::Fault(Fault::runtime(err))))
                    }
                }
            }

            match body.as_mut().poll(cx) {
                Poll::Ready(Ok(value)) => return Poll::Ready(Ok(EvalOutcome::Value(value))),
                Poll::Ready(Err(err)) => return Poll::Ready(Err(err)),
                Poll::Pending => {
                    let pending = contexts.iter().enumerate().rev().find_map(|(index, ctx)| {
                        ctx.borrow()
                            .slot
                            .borrow_mut()
                            .pending
                            .take()
                            .map(|(_, data, tx)| (index, data, tx))
                    });
                    match pending {
                        Some((index, data, tx)) => {
                            let future = (host_effects[index].handler)(data);
                            handling = Some((tx, future));
                            continue;
                        }
                        None => return Poll::Pending,
                    }
                }
            }
        })
        .await;

        result
    })
}

/// Evaluate a Glia expression.
///
/// Resolution order:
/// 1. Special forms — matched by name, receive unevaluated args
/// 2. Macro expansion — if head is Val::Macro in env, expand + re-eval
/// 3. Env lookup — if head resolves to Val::Fn, invoke it
/// 4. Built-in functions — eval args, call builtin
/// 5. `apply` — special handling (re-dispatches)
/// 6. Generic path — eval args, delegate to Dispatch (capability calls)
///
/// Non-list values are self-evaluating (returned as-is), except symbols
/// which are looked up in `env` (unbound symbols pass through for Dispatch).
pub(crate) fn eval<'a, D: Dispatch>(
    expr: &'a Val,
    env: &'a mut Env,
    dispatch: &'a D,
) -> Pin<Box<dyn Future<Output = Result<Flow, Control>> + 'a>> {
    Box::pin(async move {
        match expr {
            Val::List(items) if items.is_empty() => Ok(Flow::Value(Val::Nil)),
            Val::List(items) => {
                let head = match &items[0] {
                    Val::Sym(s) => s.as_str(),
                    other => {
                        let payload = error::type_mismatch("call head", "symbol", other);
                        return throw(&env.handler_stack, payload).await.map(Into::into);
                    }
                };
                let raw_args = &items[1..];

                // --- Special forms (unevaluated args) ---
                match head {
                    "def" => return eval_def(raw_args, env, dispatch).await.map(Flow::Value),
                    "if" => return eval_if(raw_args, env, dispatch).await,
                    "do" => return eval_do(raw_args, env, dispatch).await,
                    "let" => return eval_let(raw_args, env, dispatch).await,
                    "quote" => {
                        if raw_args.len() != 1 {
                            let payload = error::arity("quote", "1", raw_args.len());
                            return throw(&env.handler_stack, payload).await.map(Into::into);
                        }
                        return Ok(Flow::Value(raw_args[0].clone()));
                    }

                    "fn" => return Ok(Flow::Value(try_throw!(env, eval_fn(raw_args, env)))),

                    "loop" => return eval_loop(raw_args, env, dispatch).await.map(Flow::Value),
                    "recur" => return eval_recur(raw_args, env, dispatch).await,

                    "defmacro" => {
                        return eval_defmacro(raw_args, env).await.map(Flow::Value);
                    }

                    // Reader markers — raised if they escape syntax-quote
                    "unquote" => {
                        let payload = error::internal("unquote", "~ not inside syntax-quote");
                        return throw(&env.handler_stack, payload).await.map(Into::into);
                    }
                    "splice-unquote" => {
                        let payload =
                            error::internal("splice-unquote", "~@ not inside syntax-quote");
                        return throw(&env.handler_stack, payload).await.map(Into::into);
                    }

                    _ => {} // fall through to macro / fn / builtins / dispatch
                }

                // --- Macro expansion: if head resolves to a macro, expand + eval ---
                if let Some(Val::Macro {
                    arities, closure, ..
                }) = resolve(env, head)?
                {
                    let arities = arities.clone();
                    let closure = closure.clone();
                    // Macro receives RAW (unevaluated) args, body runs in captured env
                    let expanded = invoke_macro(
                        &arities,
                        &closure,
                        raw_args,
                        dispatch,
                        env.handler_stack.clone(),
                    )
                    .await?;
                    // Re-evaluate the expanded form in the CALLER's env
                    // (tail position: the expansion may recur).
                    return eval(&expanded, env, dispatch).await;
                }

                // --- Env lookup: if head resolves to a fn, invoke it ---
                if let Some(Val::Fn {
                    arities, closure, ..
                }) = resolve(env, head)?
                {
                    let arities = arities.clone();
                    let closure = closure.clone();
                    let args = eval_args(raw_args, env, dispatch).await?;
                    return invoke_fn(
                        &arities,
                        &closure,
                        &args,
                        dispatch,
                        env.handler_stack.clone(),
                    )
                    .await
                    .map(Flow::Value);
                }

                // --- Built-in: apply (needs re-dispatch, so handled here) ---
                if head == "apply" {
                    let args = eval_args(raw_args, env, dispatch).await?;
                    if args.len() < 2 {
                        let payload = error::arity("apply", "at least 2", args.len());
                        return throw(&env.handler_stack, payload).await.map(Into::into);
                    }
                    // First arg is the function (symbol or Val::Fn)
                    let func = &args[0];
                    // Last arg must be a collection; middle args are prepended
                    let last = &args[args.len() - 1];
                    let trailing = match last {
                        Val::List(v) | Val::Vector(v) => v.clone(),
                        other => {
                            let payload =
                                error::type_mismatch("apply last arg", "list or vector", other);
                            return throw(&env.handler_stack, payload).await.map(Into::into);
                        }
                    };
                    let mut spread = args[1..args.len() - 1].to_vec();
                    spread.extend(trailing);

                    // Re-dispatch: if func is a symbol, check env for Val::Fn first,
                    // then try builtins, then dispatch.
                    match func {
                        Val::Sym(fname) => {
                            if let Some(Val::Fn {
                                arities, closure, ..
                            }) = resolve(env, fname)?
                            {
                                let arities = arities.clone();
                                let closure = closure.clone();
                                return invoke_fn(
                                    &arities,
                                    &closure,
                                    &spread,
                                    dispatch,
                                    env.handler_stack.clone(),
                                )
                                .await
                                .map(Flow::Value);
                            }
                            if let Some(result) = eval_builtin(fname, &spread) {
                                return Ok(Flow::Value(try_throw!(env, result)));
                            }
                            let result = dispatch.call(fname, &spread).await;
                            return settle_native(&env.handler_stack, result)
                                .await
                                .map(Flow::Value);
                        }
                        Val::Fn {
                            arities, closure, ..
                        } => {
                            let arities = arities.clone();
                            let closure = closure.clone();
                            return invoke_fn(
                                &arities,
                                &closure,
                                &spread,
                                dispatch,
                                env.handler_stack.clone(),
                            )
                            .await
                            .map(Flow::Value);
                        }
                        other => {
                            let payload =
                                error::type_mismatch("apply first arg", "symbol or fn", other);
                            return throw(&env.handler_stack, payload).await.map(Into::into);
                        }
                    }
                }

                // --- Built-in: cell (explicit grants only) ---
                if head == "cell" {
                    return eval_cell_raw(raw_args, env, dispatch)
                        .await
                        .map(Flow::Value);
                }

                // --- Higher-order builtins (need env + dispatch for fn invocation) ---
                if head == "map" || head == "filter" || head == "reduce" {
                    let args = eval_args(raw_args, env, dispatch).await?;
                    return eval_hof(head, &args, env, dispatch).await.map(Flow::Value);
                }

                // --- Built-in functions ---
                let args = eval_args(raw_args, env, dispatch).await?;
                if let Some(result) = eval_builtin(head, &args) {
                    return Ok(Flow::Value(try_throw!(env, result)));
                }

                // --- Generic path: eval args, then dispatch to host ---
                let result = dispatch.call(head, &args).await;
                settle_native(&env.handler_stack, result)
                    .await
                    .map(Flow::Value)
            }
            // Symbol lookup.
            Val::Sym(s) => match resolve(env, s)? {
                Some(v) => Ok(Flow::Value(v.clone())),
                None => Ok(Flow::Value(Val::Sym(s.clone()))),
            },
            // Self-evaluating forms.
            other => Ok(Flow::Value(other.clone())),
        }
    })
}

// ---------------------------------------------------------------------------
// Shared perform dispatch — stack walk
// ---------------------------------------------------------------------------

/// Walk the handler stack (newest → oldest) looking for a frame whose target
/// matches `effect_target`. Write to that frame's slot and await the oneshot.
///
/// Used by both `Expr::Perform` and (in future) `Val::List` fallback dispatch.
async fn perform_dispatch(
    handler_stack: &effect::HandlerStack,
    effect_target: effect::EffectTarget,
    data: Val,
) -> Result<Val, Control> {
    // Walk stack in reverse (newest first) to find a matching handler.
    // Unified: both keyword and cap effects use EffectTarget::matches().
    let matching_ctx = {
        let stack = handler_stack.borrow();
        stack
            .iter()
            .rev()
            .find(|ctx| ctx.borrow().target.matches(&effect_target))
            .cloned()
    };

    if effect::trace_enabled() {
        eprintln!(
            "[glia:effect] perform {:?} (stack depth {}, matched: {})",
            effect_target,
            handler_stack.borrow().len(),
            matching_ctx.is_some()
        );
    }

    match matching_ctx {
        Some(ctx) => {
            let (tx, rx) = oneshot::channel();
            ctx.borrow_mut().slot.borrow_mut().pending = Some((effect_target, data, tx));
            // The receiver resolves with the resumed value, or an
            // abandonment error if the handler dropped the continuation —
            // a runtime fault (protocol violation on a computation that is
            // being discarded).
            rx.await
                .map_err(|abandoned| Control::Fault(Box::new(Fault::runtime(abandoned))))
        }
        None => {
            // No matching handler — propagate as unhandled effect. Under
            // GLIA_TRACE_EFFECTS, dump the handler stack so the reader can
            // see what WAS in scope (G3 diagnostic).
            if effect::trace_enabled() {
                eprintln!(
                    "[glia:effect] {}",
                    effect::format_unhandled_diagnostic(&effect_target, &handler_stack.borrow())
                );
            }
            Err(Control::Unhandled(Box::new(effect::EffectRequest {
                target: effect_target,
                data,
            })))
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    #![allow(clippy::result_large_err)] // EvalError-returning test helpers

    use super::*;

    /// A trivial dispatcher that records calls and returns nil.
    /// Uses RefCell for interior mutability (Dispatch takes &self).
    pub(crate) struct RecordingDispatch {
        calls: RefCell<Vec<(String, Vec<Val>)>>,
        warnings: RefCell<Vec<String>>,
    }

    impl RecordingDispatch {
        pub(crate) fn new() -> Self {
            Self {
                calls: RefCell::new(Vec::new()),
                warnings: RefCell::new(Vec::new()),
            }
        }
    }

    impl Dispatch for RecordingDispatch {
        fn call<'a>(
            &'a self,
            name: &'a str,
            args: &'a [Val],
        ) -> Pin<Box<dyn Future<Output = Result<Val, NativeSignal>> + 'a>> {
            self.calls
                .borrow_mut()
                .push((name.to_string(), args.to_vec()));
            Box::pin(core::future::ready(Ok(Val::Nil)))
        }

        fn report_warning(&self, warning: &str) {
            self.warnings.borrow_mut().push(warning.to_string());
        }
    }

    /// Helper to run an async eval in a blocking context.
    pub(crate) fn eval_blocking(
        expr: &Val,
        env: &mut Env,
        dispatch: &RecordingDispatch,
    ) -> Result<Val, EvalError> {
        // We can use a trivial executor since our futures are purely synchronous.
        pollster_eval(eval_toplevel(expr, env, dispatch))
    }

    /// Structured payload of a boundary error (thrown data or fault payload).
    pub(crate) fn err_payload(e: &EvalError) -> &Val {
        e.payload().expect("boundary error should carry a payload")
    }

    /// Run the LEGACY raw-Val evaluator to the boundary (seal applied), for
    /// tests that deliberately exercise the non-analyzed path.
    pub(crate) fn eval_raw_blocking<D: Dispatch>(
        expr: &Val,
        env: &mut Env,
        dispatch: &D,
    ) -> Result<Val, EvalError> {
        pollster_eval(async { seal(eval(expr, env, dispatch).await) })
    }

    /// Minimal single-future poll-to-completion (no tokio needed).
    pub(crate) fn pollster_eval<F: Future<Output = T>, T>(mut fut: F) -> T {
        use core::task::{Context, Poll, RawWaker, RawWakerVTable, Waker};

        fn dummy_raw_waker() -> RawWaker {
            fn no_op(_: *const ()) {}
            fn clone(p: *const ()) -> RawWaker {
                RawWaker::new(p, &VTABLE)
            }
            const VTABLE: RawWakerVTable = RawWakerVTable::new(clone, no_op, no_op, no_op);
            RawWaker::new(core::ptr::null(), &VTABLE)
        }

        let waker = unsafe { Waker::from_raw(dummy_raw_waker()) };
        let mut cx = Context::from_waker(&waker);
        // SAFETY: we never move the future after pinning.
        let mut fut = unsafe { Pin::new_unchecked(&mut fut) };
        // Loop: the effect system's state machine uses wake_by_ref() + Pending
        // to re-schedule itself after handler resume. In single-threaded sync
        // context, just re-poll immediately.
        let mut polls = 0u32;
        loop {
            match fut.as_mut().poll(&mut cx) {
                Poll::Ready(val) => return val,
                Poll::Pending => {
                    polls += 1;
                    if polls > 10_000 {
                        panic!("future stuck in Pending after 10000 polls — likely deadlock");
                    }
                    continue;
                }
            }
        }
    }

    // --- Env tests ---

    #[test]
    fn env_get_set() {
        let mut env = Env::new();
        assert!(env.get("x").unwrap().is_none());
        env.set("x".into(), Val::Int(42));
        assert_eq!(env.get("x").unwrap(), Some(Val::Int(42)));
    }

    #[test]
    fn env_child_scope_shadows() {
        let mut env = Env::new();
        env.set("x".into(), Val::Int(1));
        env.push_frame();
        env.set("x".into(), Val::Int(2));
        assert_eq!(env.get("x").unwrap(), Some(Val::Int(2)));
        env.pop_frame();
        assert_eq!(env.get("x").unwrap(), Some(Val::Int(1)));
    }

    #[test]
    fn env_child_sees_parent() {
        let mut env = Env::new();
        env.set("x".into(), Val::Int(1));
        env.push_frame();
        assert_eq!(env.get("x").unwrap(), Some(Val::Int(1)));
        env.pop_frame();
    }

    #[test]
    fn env_pop_root_is_noop() {
        let mut env = Env::new();
        env.set("x".into(), Val::Int(1));
        env.pop_frame(); // should not panic or lose the root
        assert_eq!(env.get("x").unwrap(), Some(Val::Int(1)));
    }

    // --- eval tests ---

    #[test]
    fn eval_self_evaluating() {
        let mut env = Env::new();
        let d = RecordingDispatch::new();

        assert_eq!(eval_blocking(&Val::Int(42), &mut env, &d), Ok(Val::Int(42)));
        assert_eq!(
            eval_blocking(&Val::Str("hi".into()), &mut env, &d),
            Ok(Val::Str("hi".into()))
        );
        assert_eq!(eval_blocking(&Val::Nil, &mut env, &d), Ok(Val::Nil));
        assert_eq!(
            eval_blocking(&Val::Bool(true), &mut env, &d),
            Ok(Val::Bool(true))
        );
        assert!(d.calls.borrow().is_empty());
    }

    #[test]
    fn eval_symbol_lookup() {
        let mut env = Env::new();
        let d = RecordingDispatch::new();
        env.set("x".into(), Val::Int(99));

        assert_eq!(
            eval_blocking(&Val::Sym("x".into()), &mut env, &d),
            Ok(Val::Int(99))
        );
        // Unbound symbols pass through
        assert_eq!(
            eval_blocking(&Val::Sym("unknown".into()), &mut env, &d),
            Ok(Val::Sym("unknown".into()))
        );
    }

    #[test]
    fn eval_empty_list() {
        let mut env = Env::new();
        let d = RecordingDispatch::new();
        assert_eq!(
            eval_blocking(&Val::List(vec![]), &mut env, &d),
            Ok(Val::Nil)
        );
    }

    #[test]
    fn eval_dispatches_call() {
        let mut env = Env::new();
        let d = RecordingDispatch::new();

        let expr = Val::List(vec![Val::Sym("host".into()), Val::Sym("id".into())]);
        let result = eval_blocking(&expr, &mut env, &d);
        assert_eq!(result, Ok(Val::Nil));
        assert_eq!(d.calls.borrow().len(), 1);
        assert_eq!(d.calls.borrow()[0].0, "host");
        assert_eq!(d.calls.borrow()[0].1, vec![Val::Sym("id".into())]);
    }

    #[test]
    fn eval_nested_list_evaluated_first() {
        let mut env = Env::new();

        // A dispatcher that returns Val::Bytes for "ipfs" and Val::Nil for "host".
        struct TestDispatch;
        impl Dispatch for TestDispatch {
            fn call<'a>(
                &'a self,
                name: &'a str,
                _args: &'a [Val],
            ) -> Pin<Box<dyn Future<Output = Result<Val, NativeSignal>> + 'a>> {
                let result = match name {
                    "ipfs" => Ok(Val::Bytes(vec![1, 2, 3])),
                    "host" => Ok(Val::Nil),
                    _ => Err(NativeSignal::throw(error::unbound_symbol(name, None))),
                };
                Box::pin(core::future::ready(result))
            }
        }

        let d = TestDispatch;
        // (host listen "chess" (ipfs cat "bin/x.wasm"))
        let expr = Val::List(vec![
            Val::Sym("host".into()),
            Val::Sym("listen".into()),
            Val::Str("chess".into()),
            Val::List(vec![
                Val::Sym("ipfs".into()),
                Val::Sym("cat".into()),
                Val::Str("bin/x.wasm".into()),
            ]),
        ]);
        let result = eval_raw_blocking(&expr, &mut env, &d);
        assert_eq!(result, Ok(Val::Nil));
    }

    #[test]
    fn eval_non_symbol_head_errors() {
        let mut env = Env::new();
        let d = RecordingDispatch::new();
        let expr = Val::List(vec![Val::Int(42)]);
        let result = eval_blocking(&expr, &mut env, &d);
        assert!(result.is_err());
    }

    // --- Env: set_root + snapshot ---

    #[test]
    fn env_set_root_writes_outermost() {
        let mut env = Env::new();
        env.push_frame();
        env.set_root("x".into(), Val::Int(42));
        env.pop_frame();
        // x should still be visible in the root frame
        assert_eq!(env.get("x").unwrap(), Some(Val::Int(42)));
    }

    #[test]
    fn capture_all_merges_frames() {
        let mut env = Env::new();
        env.set("x".into(), Val::Int(1));
        env.set("y".into(), Val::Int(2));
        env.push_frame();
        env.set("x".into(), Val::Int(10)); // shadow x
        env.set("z".into(), Val::Int(3));

        let captured = CapturedEnv::capture_all(&env);
        assert_eq!(captured.get("x"), Some(&Val::Int(10))); // inner wins
        assert_eq!(captured.get("y"), Some(&Val::Int(2))); // from outer
        assert_eq!(captured.get("z"), Some(&Val::Int(3))); // from inner
    }

    // --- def ---

    #[test]
    fn def_binds_in_root() {
        let mut env = Env::new();
        let d = RecordingDispatch::new();
        // (def x 42)
        let expr = Val::List(vec![
            Val::Sym("def".into()),
            Val::Sym("x".into()),
            Val::Int(42),
        ]);
        let result = eval_blocking(&expr, &mut env, &d);
        assert_eq!(result, Ok(Val::Int(42)));
        assert_eq!(env.get("x").unwrap(), Some(Val::Int(42)));
    }

    #[test]
    fn def_evals_value() {
        let mut env = Env::new();
        let d = RecordingDispatch::new();
        // (def x (do 1 2 3))
        let expr = Val::List(vec![
            Val::Sym("def".into()),
            Val::Sym("x".into()),
            Val::List(vec![
                Val::Sym("do".into()),
                Val::Int(1),
                Val::Int(2),
                Val::Int(3),
            ]),
        ]);
        let result = eval_blocking(&expr, &mut env, &d);
        assert_eq!(result, Ok(Val::Int(3)));
        assert_eq!(env.get("x").unwrap(), Some(Val::Int(3)));
    }

    #[test]
    fn def_non_symbol_errors() {
        let mut env = Env::new();
        let d = RecordingDispatch::new();
        // (def 42 "oops")
        let expr = Val::List(vec![
            Val::Sym("def".into()),
            Val::Int(42),
            Val::Str("oops".into()),
        ]);
        let result = eval_blocking(&expr, &mut env, &d);
        assert!(result.is_err());
        assert!(err_contains(err_payload(&result.unwrap_err()), "def"));
    }

    #[test]
    fn def_inside_let_writes_root() {
        let mut env = Env::new();
        let d = RecordingDispatch::new();
        // (let [a 1] (def b 2))
        let expr = Val::List(vec![
            Val::Sym("let".into()),
            Val::Vector(vec![Val::Sym("a".into()), Val::Int(1)]),
            Val::List(vec![
                Val::Sym("def".into()),
                Val::Sym("b".into()),
                Val::Int(2),
            ]),
        ]);
        eval_blocking(&expr, &mut env, &d).unwrap();
        // b should be visible at root level (not just inside let)
        assert_eq!(env.get("b").unwrap(), Some(Val::Int(2)));
    }

    // --- if ---

    #[test]
    fn if_true_branch() {
        let mut env = Env::new();
        let d = RecordingDispatch::new();
        // (if true "yes" "no")
        let expr = Val::List(vec![
            Val::Sym("if".into()),
            Val::Bool(true),
            Val::Str("yes".into()),
            Val::Str("no".into()),
        ]);
        assert_eq!(
            eval_blocking(&expr, &mut env, &d),
            Ok(Val::Str("yes".into()))
        );
    }

    #[test]
    fn if_false_branch() {
        let mut env = Env::new();
        let d = RecordingDispatch::new();
        // (if false "yes" "no")
        let expr = Val::List(vec![
            Val::Sym("if".into()),
            Val::Bool(false),
            Val::Str("yes".into()),
            Val::Str("no".into()),
        ]);
        assert_eq!(
            eval_blocking(&expr, &mut env, &d),
            Ok(Val::Str("no".into()))
        );
    }

    #[test]
    fn if_nil_is_falsy() {
        let mut env = Env::new();
        let d = RecordingDispatch::new();
        // (if nil "yes" "no")
        let expr = Val::List(vec![
            Val::Sym("if".into()),
            Val::Nil,
            Val::Str("yes".into()),
            Val::Str("no".into()),
        ]);
        assert_eq!(
            eval_blocking(&expr, &mut env, &d),
            Ok(Val::Str("no".into()))
        );
    }

    #[test]
    fn if_zero_is_truthy() {
        let mut env = Env::new();
        let d = RecordingDispatch::new();
        // (if 0 "yes" "no")
        let expr = Val::List(vec![
            Val::Sym("if".into()),
            Val::Int(0),
            Val::Str("yes".into()),
            Val::Str("no".into()),
        ]);
        assert_eq!(
            eval_blocking(&expr, &mut env, &d),
            Ok(Val::Str("yes".into()))
        );
    }

    #[test]
    fn if_empty_string_truthy() {
        let mut env = Env::new();
        let d = RecordingDispatch::new();
        // (if "" "yes" "no")
        let expr = Val::List(vec![
            Val::Sym("if".into()),
            Val::Str("".into()),
            Val::Str("yes".into()),
            Val::Str("no".into()),
        ]);
        assert_eq!(
            eval_blocking(&expr, &mut env, &d),
            Ok(Val::Str("yes".into()))
        );
    }

    #[test]
    fn if_no_else_returns_nil() {
        let mut env = Env::new();
        let d = RecordingDispatch::new();
        // (if false "yes")
        let expr = Val::List(vec![
            Val::Sym("if".into()),
            Val::Bool(false),
            Val::Str("yes".into()),
        ]);
        assert_eq!(eval_blocking(&expr, &mut env, &d), Ok(Val::Nil));
    }

    #[test]
    fn if_wrong_arg_count() {
        let mut env = Env::new();
        let d = RecordingDispatch::new();
        // (if)
        let expr = Val::List(vec![Val::Sym("if".into())]);
        assert!(eval_blocking(&expr, &mut env, &d).is_err());
        // (if a b c d)
        let expr = Val::List(vec![
            Val::Sym("if".into()),
            Val::Bool(true),
            Val::Int(1),
            Val::Int(2),
            Val::Int(3),
        ]);
        assert!(eval_blocking(&expr, &mut env, &d).is_err());
    }

    #[test]
    fn if_only_evals_taken_branch() {
        let mut env = Env::new();
        let d = RecordingDispatch::new();
        // (if true (host "taken") (host "not-taken"))
        // Only "taken" branch should dispatch; "not-taken" should NOT.
        let expr = Val::List(vec![
            Val::Sym("if".into()),
            Val::Bool(true),
            Val::List(vec![Val::Sym("host".into()), Val::Str("taken".into())]),
            Val::List(vec![Val::Sym("host".into()), Val::Str("not-taken".into())]),
        ]);
        eval_blocking(&expr, &mut env, &d).unwrap();
        assert_eq!(d.calls.borrow().len(), 1);
        assert_eq!(d.calls.borrow()[0].1, vec![Val::Str("taken".into())]);
    }

    // --- do ---

    #[test]
    fn do_returns_last() {
        let mut env = Env::new();
        let d = RecordingDispatch::new();
        // (do 1 2 3)
        let expr = Val::List(vec![
            Val::Sym("do".into()),
            Val::Int(1),
            Val::Int(2),
            Val::Int(3),
        ]);
        assert_eq!(eval_blocking(&expr, &mut env, &d), Ok(Val::Int(3)));
    }

    #[test]
    fn do_empty() {
        let mut env = Env::new();
        let d = RecordingDispatch::new();
        // (do)
        let expr = Val::List(vec![Val::Sym("do".into())]);
        assert_eq!(eval_blocking(&expr, &mut env, &d), Ok(Val::Nil));
    }

    #[test]
    fn do_single() {
        let mut env = Env::new();
        let d = RecordingDispatch::new();
        // (do 42)
        let expr = Val::List(vec![Val::Sym("do".into()), Val::Int(42)]);
        assert_eq!(eval_blocking(&expr, &mut env, &d), Ok(Val::Int(42)));
    }

    // --- let ---

    #[test]
    fn let_basic() {
        let mut env = Env::new();
        let d = RecordingDispatch::new();
        // (let [x 1] x)
        let expr = Val::List(vec![
            Val::Sym("let".into()),
            Val::Vector(vec![Val::Sym("x".into()), Val::Int(1)]),
            Val::Sym("x".into()),
        ]);
        assert_eq!(eval_blocking(&expr, &mut env, &d), Ok(Val::Int(1)));
    }

    #[test]
    fn let_shadow() {
        let mut env = Env::new();
        let d = RecordingDispatch::new();
        env.set("x".into(), Val::Int(1));
        // (let [x 2] x)
        let expr = Val::List(vec![
            Val::Sym("let".into()),
            Val::Vector(vec![Val::Sym("x".into()), Val::Int(2)]),
            Val::Sym("x".into()),
        ]);
        assert_eq!(eval_blocking(&expr, &mut env, &d), Ok(Val::Int(2)));
        // After let, x should be back to 1
        assert_eq!(env.get("x").unwrap(), Some(Val::Int(1)));
    }

    #[test]
    fn let_sequential_binding() {
        let mut env = Env::new();
        let d = RecordingDispatch::new();
        // (let [x 1 y x] y) — y sees x from earlier binding
        let expr = Val::List(vec![
            Val::Sym("let".into()),
            Val::Vector(vec![
                Val::Sym("x".into()),
                Val::Int(1),
                Val::Sym("y".into()),
                Val::Sym("x".into()),
            ]),
            Val::Sym("y".into()),
        ]);
        assert_eq!(eval_blocking(&expr, &mut env, &d), Ok(Val::Int(1)));
    }

    #[test]
    fn let_implicit_do() {
        let mut env = Env::new();
        let d = RecordingDispatch::new();
        // (let [x 1] 10 20 x) — multiple body forms, returns last
        let expr = Val::List(vec![
            Val::Sym("let".into()),
            Val::Vector(vec![Val::Sym("x".into()), Val::Int(1)]),
            Val::Int(10),
            Val::Int(20),
            Val::Sym("x".into()),
        ]);
        assert_eq!(eval_blocking(&expr, &mut env, &d), Ok(Val::Int(1)));
    }

    #[test]
    fn let_odd_bindings_error() {
        let mut env = Env::new();
        let d = RecordingDispatch::new();
        // (let [x] x) — odd number of binding forms
        let expr = Val::List(vec![
            Val::Sym("let".into()),
            Val::Vector(vec![Val::Sym("x".into())]),
            Val::Sym("x".into()),
        ]);
        let result = eval_blocking(&expr, &mut env, &d);
        assert!(result.is_err());
        assert!(err_contains(err_payload(&result.unwrap_err()), "pairs"));
    }

    #[test]
    fn let_non_vector_error() {
        let mut env = Env::new();
        let d = RecordingDispatch::new();
        // (let (x 1) x) — list instead of vector
        let expr = Val::List(vec![
            Val::Sym("let".into()),
            Val::List(vec![Val::Sym("x".into()), Val::Int(1)]),
            Val::Sym("x".into()),
        ]);
        let result = eval_blocking(&expr, &mut env, &d);
        assert!(result.is_err());
        assert!(err_contains(err_payload(&result.unwrap_err()), "vector"));
    }

    // --- quote ---

    #[test]
    fn quote_symbol() {
        let mut env = Env::new();
        let d = RecordingDispatch::new();
        env.set("x".into(), Val::Int(99));
        // (quote x) — should NOT look up x
        let expr = Val::List(vec![Val::Sym("quote".into()), Val::Sym("x".into())]);
        assert_eq!(eval_blocking(&expr, &mut env, &d), Ok(Val::Sym("x".into())));
    }

    #[test]
    fn quote_list() {
        let mut env = Env::new();
        let d = RecordingDispatch::new();
        // (quote (+ 1 2)) — should NOT evaluate the list
        let inner = Val::List(vec![Val::Sym("+".into()), Val::Int(1), Val::Int(2)]);
        let expr = Val::List(vec![Val::Sym("quote".into()), inner.clone()]);
        assert_eq!(eval_blocking(&expr, &mut env, &d), Ok(inner));
        assert!(d.calls.borrow().is_empty()); // no dispatch happened
    }

    // --- fn ---

    #[test]
    fn fn_single_arity_call() {
        let mut env = Env::new();
        let d = RecordingDispatch::new();
        // (def f (fn [x] x))
        let def_expr = Val::List(vec![
            Val::Sym("def".into()),
            Val::Sym("f".into()),
            Val::List(vec![
                Val::Sym("fn".into()),
                Val::Vector(vec![Val::Sym("x".into())]),
                Val::Sym("x".into()),
            ]),
        ]);
        eval_blocking(&def_expr, &mut env, &d).unwrap();
        // (f 42)
        let call_expr = Val::List(vec![Val::Sym("f".into()), Val::Int(42)]);
        let result = eval_blocking(&call_expr, &mut env, &d);
        assert_eq!(result, Ok(Val::Int(42)));
    }

    #[test]
    fn fn_multi_arity() {
        let mut env = Env::new();
        let d = RecordingDispatch::new();
        // (def f (fn ([x] x) ([x y] y)))
        let def_expr = Val::List(vec![
            Val::Sym("def".into()),
            Val::Sym("f".into()),
            Val::List(vec![
                Val::Sym("fn".into()),
                Val::List(vec![
                    Val::Vector(vec![Val::Sym("x".into())]),
                    Val::Sym("x".into()),
                ]),
                Val::List(vec![
                    Val::Vector(vec![Val::Sym("x".into()), Val::Sym("y".into())]),
                    Val::Sym("y".into()),
                ]),
            ]),
        ]);
        eval_blocking(&def_expr, &mut env, &d).unwrap();
        // (f 1) → 1
        let call1 = Val::List(vec![Val::Sym("f".into()), Val::Int(1)]);
        assert_eq!(eval_blocking(&call1, &mut env, &d), Ok(Val::Int(1)));
        // (f 1 2) → 2
        let call2 = Val::List(vec![Val::Sym("f".into()), Val::Int(1), Val::Int(2)]);
        assert_eq!(eval_blocking(&call2, &mut env, &d), Ok(Val::Int(2)));
    }

    #[test]
    fn fn_variadic() {
        let mut env = Env::new();
        let d = RecordingDispatch::new();
        // (def f (fn [x & rest] rest))
        let def_expr = Val::List(vec![
            Val::Sym("def".into()),
            Val::Sym("f".into()),
            Val::List(vec![
                Val::Sym("fn".into()),
                Val::Vector(vec![
                    Val::Sym("x".into()),
                    Val::Sym("&".into()),
                    Val::Sym("rest".into()),
                ]),
                Val::Sym("rest".into()),
            ]),
        ]);
        eval_blocking(&def_expr, &mut env, &d).unwrap();
        // (f 1 2 3) → (2 3)
        let call = Val::List(vec![
            Val::Sym("f".into()),
            Val::Int(1),
            Val::Int(2),
            Val::Int(3),
        ]);
        assert_eq!(
            eval_blocking(&call, &mut env, &d),
            Ok(Val::List(vec![Val::Int(2), Val::Int(3)]))
        );
    }

    #[test]
    fn fn_closure_captures_env() {
        let mut env = Env::new();
        let d = RecordingDispatch::new();
        // (def x 10)
        let def_x = Val::List(vec![
            Val::Sym("def".into()),
            Val::Sym("x".into()),
            Val::Int(10),
        ]);
        eval_blocking(&def_x, &mut env, &d).unwrap();
        // (def f (fn [] x))
        let def_f = Val::List(vec![
            Val::Sym("def".into()),
            Val::Sym("f".into()),
            Val::List(vec![
                Val::Sym("fn".into()),
                Val::Vector(vec![]),
                Val::Sym("x".into()),
            ]),
        ]);
        eval_blocking(&def_f, &mut env, &d).unwrap();
        // (def x 20) — rebind x
        let def_x2 = Val::List(vec![
            Val::Sym("def".into()),
            Val::Sym("x".into()),
            Val::Int(20),
        ]);
        eval_blocking(&def_x2, &mut env, &d).unwrap();
        // SEMANTIC — late binding (PR-1b.0): (f) → 20. Top-level names
        // resolve through the live definition owner at call time, so
        // redefinition is visible to existing closures. (Supersedes the old
        // snapshot semantics, which returned 10 here.)
        let call = Val::List(vec![Val::Sym("f".into())]);
        assert_eq!(eval_blocking(&call, &mut env, &d), Ok(Val::Int(20)));
    }

    #[test]
    fn fn_closure_empty_free_vars_captures_nothing() {
        let mut env = Env::new();
        let d = RecordingDispatch::new();

        let closure = eval_str("(fn [] 1)", &mut env, &d).unwrap();
        let Val::Fn { closure, .. } = closure else {
            panic!("expected function");
        };
        assert_eq!(closure.captured.len(), 0);
    }

    #[test]
    fn fn_closure_single_capture_is_slim() {
        let mut env = Env::new();
        let d = RecordingDispatch::new();
        env.set("x".into(), Val::Int(7));
        env.set("y".into(), Val::Int(9));

        let closure = eval_str("(fn [] x)", &mut env, &d).unwrap();
        let Val::Fn { closure, .. } = closure else {
            panic!("expected function");
        };
        assert_eq!(closure.captured.len(), 1);
        assert_eq!(closure.captured.get("x"), Some(&Val::Int(7)));
        assert!(closure.captured.get("y").is_none());
    }

    #[test]
    fn fn_closure_multi_arity_union_capture() {
        let mut env = Env::new();
        let d = RecordingDispatch::new();
        env.set("x".into(), Val::Int(1));
        env.set("y".into(), Val::Int(2));
        env.set("z".into(), Val::Int(3));

        let closure = eval_str("(fn ([a] x) ([a b] y))", &mut env, &d).unwrap();
        let Val::Fn { closure, .. } = closure else {
            panic!("expected function");
        };
        assert_eq!(closure.captured.len(), 2);
        assert_eq!(closure.captured.get("x"), Some(&Val::Int(1)));
        assert_eq!(closure.captured.get("y"), Some(&Val::Int(2)));
        assert!(closure.captured.get("z").is_none());
    }

    #[test]
    fn fn_closure_over_closure_still_works_with_slim_env() {
        let mut env = Env::new();
        let d = RecordingDispatch::new();
        let inner = eval_str("(let [x 42 outer (fn [] (fn [] x))] (outer))", &mut env, &d).unwrap();
        let Val::Fn { closure, .. } = &inner else {
            panic!("expected function");
        };
        assert_eq!(closure.captured.len(), 1);
        assert_eq!(closure.captured.get("x"), Some(&Val::Int(42)));
        env.set("inner".into(), inner);
        assert_eq!(
            eval_str("(inner)", &mut env, &d),
            Ok(Val::Int(42)),
            "nested closure should remain functional"
        );
    }

    #[test]
    fn fn_closure_identity_preservation_with_slim_envs() {
        let mut env = Env::new();
        let d = RecordingDispatch::new();

        let f = eval_str("(fn [] 1)", &mut env, &d).unwrap();
        let f_clone = f.clone();
        let g = eval_str("(fn [] 1)", &mut env, &d).unwrap();

        assert_eq!(f, f_clone, "cloned closure should preserve Rc identity");
        assert_ne!(f, g, "separate evaluations should produce distinct Rc envs");
    }

    #[test]
    fn raw_fn_path_keeps_full_snapshot() {
        let mut env = Env::new();
        let d = RecordingDispatch::new();
        env.set("x".into(), Val::Int(1));
        env.set("y".into(), Val::Int(2));

        let raw = Val::List(vec![
            Val::Sym("fn".into()),
            Val::Vector(vec![]),
            Val::Sym("x".into()),
        ]);
        let closure = eval_raw_blocking(&raw, &mut env, &d).unwrap();
        let Val::Fn { closure, .. } = closure else {
            panic!("expected function");
        };
        assert_eq!(closure.captured.get("x"), Some(&Val::Int(1)));
        assert_eq!(closure.captured.get("y"), Some(&Val::Int(2)));
    }

    fn fn_cap_status(value: Val) -> (bool, Option<String>) {
        let Val::Fn {
            is_cap_free,
            cap_violation,
            ..
        } = value
        else {
            panic!("expected fn value");
        };
        (is_cap_free, cap_violation)
    }

    fn macro_cap_status(value: Val) -> (bool, Option<String>) {
        let Val::Macro {
            is_cap_free,
            cap_violation,
            ..
        } = value
        else {
            panic!("expected macro value");
        };
        (is_cap_free, cap_violation)
    }

    #[test]
    fn fn_cap_status_no_captures() {
        let mut env = Env::new();
        let d = RecordingDispatch::new();
        let status = fn_cap_status(eval_str("(fn [] 1)", &mut env, &d).unwrap());
        assert_eq!(status, (true, None));
    }

    #[test]
    fn fn_cap_status_int_capture() {
        let mut env = Env::new();
        let d = RecordingDispatch::new();
        env.set("x".into(), Val::Int(1));
        let status = fn_cap_status(eval_str("(fn [] x)", &mut env, &d).unwrap());
        assert_eq!(status, (true, None));
    }

    #[test]
    fn fn_cap_status_cap_capture() {
        let mut env = Env::new();
        let d = RecordingDispatch::new();
        env.set("db".into(), make_cap("db", "cid:db", Rc::new(())));
        let status = fn_cap_status(eval_str("(fn [] db)", &mut env, &d).unwrap());
        assert_eq!(status, (false, Some("db".into())));
    }

    #[test]
    fn fn_cap_status_zero_grant_cell_capture_is_authority_free() {
        // Deliberate PR-0 flip: a zero-grant cell spec is ordinary data and
        // carries no authority (same classification as the bytes it wraps).
        let mut env = cell_test_env();
        let d = RecordingDispatch::new();
        let cell = eval_str("(cell image)", &mut env, &d).unwrap();
        env.set("c".into(), cell);
        let status = fn_cap_status(eval_str("(fn [] c)", &mut env, &d).unwrap());
        assert_eq!(status, (true, None));
    }

    #[test]
    fn fn_cap_status_cap_bearing_cell_capture() {
        let mut env = cell_test_env();
        let d = RecordingDispatch::new();
        let cell = eval_str("(cell image :grants {:db db})", &mut env, &d).unwrap();
        env.set("c".into(), cell);
        let status = fn_cap_status(eval_str("(fn [] c)", &mut env, &d).unwrap());
        assert_eq!(status, (false, Some("c".into())));
    }

    #[test]
    fn fn_cap_status_native_fn_capture() {
        let mut env = Env::new();
        let d = RecordingDispatch::new();
        env.set(
            "n".into(),
            Val::NativeFn {
                name: "n".into(),
                func: Rc::new(|_| Ok(Val::Nil)),
            },
        );
        let status = fn_cap_status(eval_str("(fn [] n)", &mut env, &d).unwrap());
        assert_eq!(status, (false, Some("n".into())));
    }

    #[test]
    fn fn_cap_status_capture_cap_free_fn() {
        let mut env = Env::new();
        let d = RecordingDispatch::new();
        let helper = eval_str("(fn [] 1)", &mut env, &d).unwrap();
        env.set("helper".into(), helper);
        let status = fn_cap_status(eval_str("(fn [] helper)", &mut env, &d).unwrap());
        assert_eq!(status, (true, None));
    }

    #[test]
    fn fn_cap_status_capture_cap_bearing_fn() {
        let mut env = Env::new();
        let d = RecordingDispatch::new();
        env.set("db".into(), make_cap("db", "cid:db", Rc::new(())));
        let helper = eval_str("(fn [] db)", &mut env, &d).unwrap();
        env.set("helper".into(), helper);
        let status = fn_cap_status(eval_str("(fn [] helper)", &mut env, &d).unwrap());
        assert_eq!(status, (false, Some("helper".into())));
    }

    #[test]
    fn fn_cap_status_capture_cap_free_macro() {
        let mut env = Env::new();
        let d = RecordingDispatch::new();
        eval_str("(defmacro m [] 42)", &mut env, &d).unwrap();
        let status = fn_cap_status(eval_str("(fn [] m)", &mut env, &d).unwrap());
        assert_eq!(status, (true, None));
    }

    #[test]
    fn fn_cap_status_capture_cap_bearing_macro() {
        // STAGE E MARKER: `m` is a persistent definition now (defs-resident,
        // late-bound), so the construction-time snapshot no longer sees it
        // and cap status under-reports — the documented Stage B→E gap. The
        // bit is advisory lint only (authority = possession, enforced by the
        // membrane, never by this flag). Stage E's live analysis restores
        // (false, Some("m")) by resolving free names through the owner.
        let mut env = Env::new();
        let d = RecordingDispatch::new();
        env.set("db".into(), make_cap("db", "cid:db", Rc::new(())));
        eval_str("(defmacro m [] db)", &mut env, &d).unwrap();
        let status = fn_cap_status(eval_str("(fn [] m)", &mut env, &d).unwrap());
        assert_eq!(status, (true, None));
    }

    #[test]
    fn fn_cap_violation_is_deterministic_by_binding_name() {
        let mut env = Env::new();
        let d = RecordingDispatch::new();
        env.set("z_cap".into(), make_cap("z", "cid:z", Rc::new(())));
        env.set("a_cap".into(), make_cap("a", "cid:a", Rc::new(())));
        let status = fn_cap_status(eval_str("(fn [] (list z_cap a_cap))", &mut env, &d).unwrap());
        assert_eq!(status, (false, Some("a_cap".into())));
    }

    #[test]
    fn macro_cap_status_no_captures() {
        let mut env = Env::new();
        let d = RecordingDispatch::new();
        let status = macro_cap_status(eval_str("(defmacro m [] 1)", &mut env, &d).unwrap());
        assert_eq!(status, (true, None));
    }

    #[test]
    fn macro_cap_status_int_capture() {
        let mut env = Env::new();
        let d = RecordingDispatch::new();
        env.set("x".into(), Val::Int(1));
        let status = macro_cap_status(eval_str("(defmacro m [] x)", &mut env, &d).unwrap());
        assert_eq!(status, (true, None));
    }

    #[test]
    fn macro_cap_status_cap_capture() {
        let mut env = Env::new();
        let d = RecordingDispatch::new();
        env.set("db".into(), make_cap("db", "cid:db", Rc::new(())));
        let status = macro_cap_status(eval_str("(defmacro m [] db)", &mut env, &d).unwrap());
        assert_eq!(status, (false, Some("db".into())));
    }

    #[test]
    fn macro_cap_status_zero_grant_cell_capture_is_authority_free() {
        // Deliberate PR-0 flip, mirroring the fn cap-status change. The cell
        // is built in a scratch env so the macro's env snapshot holds only
        // the (authority-free) spec, not the builder caps.
        let mut scratch = cell_test_env();
        let d = RecordingDispatch::new();
        let cell = eval_str("(cell image)", &mut scratch, &d).unwrap();
        let mut env = Env::new();
        env.set("c".into(), cell);
        let status = macro_cap_status(eval_str("(defmacro m [] c)", &mut env, &d).unwrap());
        assert_eq!(status, (true, None));
    }

    #[test]
    fn macro_cap_status_cap_bearing_cell_capture() {
        let mut env = cell_test_env();
        let d = RecordingDispatch::new();
        let cell = eval_str("(cell image :grants {:db db})", &mut env, &d).unwrap();
        env.set("c".into(), cell);
        let status = macro_cap_status(eval_str("(defmacro m [] c)", &mut env, &d).unwrap());
        assert_eq!(status, (false, Some("c".into())));
    }

    #[test]
    fn macro_cap_status_native_fn_capture() {
        let mut env = Env::new();
        let d = RecordingDispatch::new();
        env.set(
            "n".into(),
            Val::NativeFn {
                name: "n".into(),
                func: Rc::new(|_| Ok(Val::Nil)),
            },
        );
        let status = macro_cap_status(eval_str("(defmacro m [] n)", &mut env, &d).unwrap());
        assert_eq!(status, (false, Some("n".into())));
    }

    #[test]
    fn macro_cap_status_capture_cap_free_fn() {
        let mut env = Env::new();
        let d = RecordingDispatch::new();
        let helper = eval_str("(fn [] 1)", &mut env, &d).unwrap();
        env.set("helper".into(), helper);
        let status = macro_cap_status(eval_str("(defmacro m [] helper)", &mut env, &d).unwrap());
        assert_eq!(status, (true, None));
    }

    #[test]
    fn macro_cap_status_capture_cap_bearing_fn() {
        let mut env = Env::new();
        let d = RecordingDispatch::new();
        env.set("db".into(), make_cap("db", "cid:db", Rc::new(())));
        let helper = eval_str("(fn [] db)", &mut env, &d).unwrap();
        env.set("helper".into(), helper);
        let status = macro_cap_status(eval_str("(defmacro m [] helper)", &mut env, &d).unwrap());
        assert_eq!(status, (false, Some("db".into())));
    }

    #[test]
    fn macro_cap_status_capture_cap_free_macro() {
        let mut env = Env::new();
        let d = RecordingDispatch::new();
        eval_str("(defmacro helper [] 42)", &mut env, &d).unwrap();
        let status = macro_cap_status(eval_str("(defmacro m [] helper)", &mut env, &d).unwrap());
        assert_eq!(status, (true, None));
    }

    #[test]
    fn macro_cap_status_capture_cap_bearing_macro() {
        let mut env = Env::new();
        let d = RecordingDispatch::new();
        env.set("db".into(), make_cap("db", "cid:db", Rc::new(())));
        eval_str("(defmacro helper [] db)", &mut env, &d).unwrap();
        let status = macro_cap_status(eval_str("(defmacro m [] helper)", &mut env, &d).unwrap());
        assert_eq!(status, (false, Some("db".into())));
    }

    #[test]
    fn closure_hash_and_eq_ignore_cap_status_fields() {
        use std::collections::hash_map::DefaultHasher;
        use std::hash::{Hash, Hasher};

        let shared = Rc::new(CapturedEnv::from_slots(Frame::new()));
        let owner = Defs::new(None);
        let left = Val::Fn {
            arities: vec![],
            closure: crate::Closure {
                captured: Rc::clone(&shared),
                owner: super::own::OwnerRef::Strong(Rc::clone(&owner)),
            },
            is_cap_free: true,
            cap_violation: None,
        };
        let right = Val::Fn {
            arities: vec![],
            closure: crate::Closure {
                captured: shared,
                owner: super::own::OwnerRef::Strong(owner),
            },
            is_cap_free: false,
            cap_violation: Some("db".into()),
        };

        assert_eq!(left, right);
        let mut lh = DefaultHasher::new();
        left.hash(&mut lh);
        let mut rh = DefaultHasher::new();
        right.hash(&mut rh);
        assert_eq!(lh.finish(), rh.finish());
    }

    #[test]
    fn raw_fn_path_cap_status_scans_full_snapshot() {
        let mut env = Env::new();
        let d = RecordingDispatch::new();
        env.set("db".into(), make_cap("db", "cid:db", Rc::new(())));
        env.set("x".into(), Val::Int(1));

        let raw = Val::List(vec![
            Val::Sym("fn".into()),
            Val::Vector(vec![]),
            Val::Sym("x".into()),
        ]);
        let status = fn_cap_status(eval_raw_blocking(&raw, &mut env, &d).unwrap());
        assert_eq!(status, (false, Some("db".into())));
    }

    #[test]
    fn fn_arity_mismatch() {
        let mut env = Env::new();
        let d = RecordingDispatch::new();
        // (def f (fn [x y] x))
        let def_expr = Val::List(vec![
            Val::Sym("def".into()),
            Val::Sym("f".into()),
            Val::List(vec![
                Val::Sym("fn".into()),
                Val::Vector(vec![Val::Sym("x".into()), Val::Sym("y".into())]),
                Val::Sym("x".into()),
            ]),
        ]);
        eval_blocking(&def_expr, &mut env, &d).unwrap();
        // (f 1) — wrong arity
        let call = Val::List(vec![Val::Sym("f".into()), Val::Int(1)]);
        let err = eval_blocking(&call, &mut env, &d).unwrap_err();
        assert_eq!(
            error::type_tag(err_payload(&err)),
            Some(error::tag::ARITY),
            "got: {err}"
        );
    }

    #[test]
    fn fn_duplicate_arity_errors() {
        let mut env = Env::new();
        let d = RecordingDispatch::new();
        // (fn ([x] x) ([y] y)) — two 1-arg arities
        let expr = Val::List(vec![
            Val::Sym("fn".into()),
            Val::List(vec![
                Val::Vector(vec![Val::Sym("x".into())]),
                Val::Sym("x".into()),
            ]),
            Val::List(vec![
                Val::Vector(vec![Val::Sym("y".into())]),
                Val::Sym("y".into()),
            ]),
        ]);
        let err = eval_blocking(&expr, &mut env, &d).unwrap_err();
        assert!(
            err_contains(err_payload(&err), "duplicate arity"),
            "got: {err}"
        );
    }

    #[test]
    fn fn_implicit_do_body() {
        let mut env = Env::new();
        let d = RecordingDispatch::new();
        // (def f (fn [x] 1 2 x)) — body has multiple forms, returns last
        let def_expr = Val::List(vec![
            Val::Sym("def".into()),
            Val::Sym("f".into()),
            Val::List(vec![
                Val::Sym("fn".into()),
                Val::Vector(vec![Val::Sym("x".into())]),
                Val::Int(1),
                Val::Int(2),
                Val::Sym("x".into()),
            ]),
        ]);
        eval_blocking(&def_expr, &mut env, &d).unwrap();
        let call = Val::List(vec![Val::Sym("f".into()), Val::Int(99)]);
        assert_eq!(eval_blocking(&call, &mut env, &d), Ok(Val::Int(99)));
    }

    #[test]
    fn fn_no_params_errors() {
        let mut env = Env::new();
        let d = RecordingDispatch::new();
        // (fn) — no params at all
        let expr = Val::List(vec![Val::Sym("fn".into())]);
        assert!(eval_blocking(&expr, &mut env, &d).is_err());
    }

    // --- loop / recur ---

    #[test]
    fn loop_returns_non_recur() {
        let mut env = Env::new();
        let d = RecordingDispatch::new();
        // (loop [x 42] x)
        let expr = Val::List(vec![
            Val::Sym("loop".into()),
            Val::Vector(vec![Val::Sym("x".into()), Val::Int(42)]),
            Val::Sym("x".into()),
        ]);
        assert_eq!(eval_blocking(&expr, &mut env, &d), Ok(Val::Int(42)));
    }

    #[test]
    fn loop_recur_once() {
        let mut env = Env::new();
        let d = RecordingDispatch::new();
        // (loop [x true] (if x (recur false) "done"))
        let expr = Val::List(vec![
            Val::Sym("loop".into()),
            Val::Vector(vec![Val::Sym("x".into()), Val::Bool(true)]),
            Val::List(vec![
                Val::Sym("if".into()),
                Val::Sym("x".into()),
                Val::List(vec![Val::Sym("recur".into()), Val::Bool(false)]),
                Val::Str("done".into()),
            ]),
        ]);
        assert_eq!(
            eval_blocking(&expr, &mut env, &d),
            Ok(Val::Str("done".into()))
        );
    }

    #[test]
    fn loop_recur_multiple_bindings() {
        let mut env = Env::new();
        let d = RecordingDispatch::new();
        // (loop [a 1 b 2] (if a (recur false 3) b))
        let expr = Val::List(vec![
            Val::Sym("loop".into()),
            Val::Vector(vec![
                Val::Sym("a".into()),
                Val::Int(1),
                Val::Sym("b".into()),
                Val::Int(2),
            ]),
            Val::List(vec![
                Val::Sym("if".into()),
                Val::Sym("a".into()),
                Val::List(vec![
                    Val::Sym("recur".into()),
                    Val::Bool(false),
                    Val::Int(3),
                ]),
                Val::Sym("b".into()),
            ]),
        ]);
        assert_eq!(eval_blocking(&expr, &mut env, &d), Ok(Val::Int(3)));
    }

    #[test]
    fn loop_sequential_bindings() {
        let mut env = Env::new();
        let d = RecordingDispatch::new();
        // (loop [a 1 b a] b) — b sees a=1
        let expr = Val::List(vec![
            Val::Sym("loop".into()),
            Val::Vector(vec![
                Val::Sym("a".into()),
                Val::Int(1),
                Val::Sym("b".into()),
                Val::Sym("a".into()),
            ]),
            Val::Sym("b".into()),
        ]);
        assert_eq!(eval_blocking(&expr, &mut env, &d), Ok(Val::Int(1)));
    }

    #[test]
    fn recur_wrong_arity() {
        let mut env = Env::new();
        let d = RecordingDispatch::new();
        // (loop [x 1 y 2] (recur 3))
        let expr = Val::List(vec![
            Val::Sym("loop".into()),
            Val::Vector(vec![
                Val::Sym("x".into()),
                Val::Int(1),
                Val::Sym("y".into()),
                Val::Int(2),
            ]),
            Val::List(vec![Val::Sym("recur".into()), Val::Int(3)]),
        ]);
        let err = eval_blocking(&expr, &mut env, &d).unwrap_err();
        assert_eq!(
            error::type_tag(err_payload(&err)),
            Some(error::tag::ARITY),
            "got: {err}"
        );
    }

    #[test]
    fn recur_outside_loop() {
        let mut env = Env::new();
        let d = RecordingDispatch::new();
        // (recur 1) at top level — a LANGUAGE FAULT: structurally invalid
        // control, bypasses all handlers, tagged glia.error/invalid-recur.
        let expr = Val::List(vec![Val::Sym("recur".into()), Val::Int(1)]);
        let err = eval_blocking(&expr, &mut env, &d).unwrap_err();
        assert!(
            matches!(&err, EvalError::Fault(f) if f.kind() == crate::FaultKind::Language),
            "got: {err:?}"
        );
        assert_eq!(
            error::type_tag(err_payload(&err)),
            Some(error::tag::INVALID_RECUR),
            "got: {err}"
        );
        assert!(
            error::message(err_payload(&err)).unwrap().contains("recur"),
            "got: {err}"
        );
    }

    #[test]
    fn loop_non_vector_bindings() {
        let mut env = Env::new();
        let d = RecordingDispatch::new();
        // (loop (x 1) x) — list instead of vector
        let expr = Val::List(vec![
            Val::Sym("loop".into()),
            Val::List(vec![Val::Sym("x".into()), Val::Int(1)]),
            Val::Sym("x".into()),
        ]);
        let err = eval_blocking(&expr, &mut env, &d).unwrap_err();
        assert!(err_contains(err_payload(&err), "vector"), "got: {err}");
    }

    #[test]
    fn loop_odd_bindings() {
        let mut env = Env::new();
        let d = RecordingDispatch::new();
        // (loop [x] x) — odd number of binding forms
        let expr = Val::List(vec![
            Val::Sym("loop".into()),
            Val::Vector(vec![Val::Sym("x".into())]),
            Val::Sym("x".into()),
        ]);
        let err = eval_blocking(&expr, &mut env, &d).unwrap_err();
        assert!(err_contains(err_payload(&err), "pairs"), "got: {err}");
    }

    // =========================================================================
    // Built-in function tests
    // =========================================================================

    /// Helper: parse + eval a string expression.
    /// Wrap a raw payload as the boundary form of an unhandled exception.
    fn boundary_thrown(payload: Val) -> EvalError {
        EvalError::Unhandled(effect::EffectRequest {
            target: effect::EffectTarget::Keyword(error::EXCEPTION_EFFECT.into()),
            data: payload,
        })
    }

    pub(crate) fn eval_str(
        input: &str,
        env: &mut Env,
        d: &RecordingDispatch,
    ) -> Result<Val, EvalError> {
        let expr =
            crate::read(input).map_err(|e| boundary_thrown(error::parse(None, e.to_string())))?;
        eval_blocking(&expr, env, d)
    }

    /// Extract the grant entries of a cell-spec map, sorted by name for
    /// deterministic assertions (the spec's grants map is unordered; wire
    /// ordering is pinned at the kernel boundary).
    fn cell_caps(value: Val) -> Vec<(String, Val)> {
        assert!(
            crate::is_cell_tagged(&value),
            "expected cell-tagged map, got {value}"
        );
        let Val::Map(map) = value else {
            unreachable!("is_cell_tagged accepted a non-map");
        };
        let Some(Val::Map(grants)) = map.get(&Val::Keyword(crate::cell_spec::GRANTS_KEY.into()))
        else {
            panic!("cell builtin produced a spec without a grants map");
        };
        let mut caps: Vec<(String, Val)> = grants
            .iter()
            .map(|(k, v)| match k {
                Val::Keyword(name) => (name.clone(), v.clone()),
                other => panic!("non-keyword grant key {other}"),
            })
            .collect();
        caps.sort_by(|a, b| a.0.cmp(&b.0));
        caps
    }

    fn cell_test_env() -> Env {
        let mut env = Env::new();
        env.set("image".into(), Val::Bytes(vec![0, 97, 115, 109]));
        env.set("db".into(), make_cap("database", "cid:db", Rc::new(())));
        env.set(
            "logger".into(),
            make_cap("logger", "cid:logger", Rc::new(())),
        );
        env
    }

    #[test]
    fn duplicate_ordinary_map_literal_does_not_evaluate_discarded_value() {
        let mut env = Env::new();
        let d = RecordingDispatch::new();
        let result = eval_str("(get {:a missing :a 2} :a)", &mut env, &d).unwrap();
        assert_eq!(result, Val::Int(2));
    }

    #[test]
    fn cell_without_grants_has_zero_authority() {
        let mut env = cell_test_env();
        let d = RecordingDispatch::new();
        let caps = cell_caps(eval_str("(cell image)", &mut env, &d).unwrap());
        assert!(caps.is_empty());
        assert!(d.warnings.borrow().is_empty());
    }

    #[test]
    fn cell_with_empty_grant_map_has_zero_authority() {
        let mut env = cell_test_env();
        let d = RecordingDispatch::new();
        let caps = cell_caps(eval_str("(cell image :grants {})", &mut env, &d).unwrap());
        assert!(caps.is_empty());
    }

    #[test]
    fn cell_explicit_grants_are_renamed() {
        // Deterministic (alphabetic) wire ordering is now pinned at the
        // kernel boundary by parse_cell_spec; here the spec's grants map
        // just carries the renamed entries.
        let mut env = cell_test_env();
        let d = RecordingDispatch::new();
        let caps = cell_caps(
            eval_str(
                "(cell image :grants {:z-log logger :app-db db})",
                &mut env,
                &d,
            )
            .unwrap(),
        );
        assert_eq!(
            caps.iter()
                .map(|(name, _)| name.as_str())
                .collect::<Vec<_>>(),
            vec!["app-db", "z-log"]
        );
        assert!(matches!(
            &caps[0].1,
            Val::Cap(h) if h.name() == "database"
        ));
    }

    #[test]
    fn cell_returns_tagged_map_data() {
        // PR-0: (cell ...) returns ordinary tagged immutable data.
        let mut env = cell_test_env();
        let d = RecordingDispatch::new();
        let cell = eval_str("(cell image :grants {:db db})", &mut env, &d).unwrap();
        env.set("c".into(), cell.clone());

        assert!(crate::is_cell_tagged(&cell));
        assert_eq!(
            eval_str("(type c)", &mut env, &d).unwrap(),
            Val::Keyword("map".into())
        );
        assert_eq!(
            eval_str("(get c :ww/type)", &mut env, &d).unwrap(),
            Val::Keyword("cell".into())
        );
        assert_eq!(
            eval_str("(type (get c :wasm))", &mut env, &d).unwrap(),
            Val::Keyword("bytes".into())
        );
        // Transformable as ordinary data; the tag survives transformation
        // (cell? is tag-only) and activation re-validates the full spec.
        assert_eq!(
            eval_str("(cell? (assoc c :extra 1))", &mut env, &d).unwrap(),
            Val::Bool(true)
        );
    }

    #[test]
    fn cell_predicate_accepts_cell_specs_and_rejects_non_cells() {
        let mut env = cell_test_env();
        let d = RecordingDispatch::new();
        let cell = eval_str("(cell image)", &mut env, &d).unwrap();
        env.set("c".into(), cell);

        // cell? is a tag-only predicate: a map whose :ww/type is :cell.
        // Malformed-but-tagged specs satisfy it; activation is where they
        // fail (pinned kernel-side in parse_cell_spec tests).
        for accepted in [
            "(cell? c)",
            "(cell? {:ww/type :cell :wasm (get c :wasm) :grants {}})",
            "(cell? {:ww/type :cell})",
            "(cell? {:ww/type :cell :wasm \"not-bytes\" :grants {}})",
            "(cell? {:ww/type :cell :wasm (get c :wasm) :grants {\"db\" 1}})",
            "(cell? {:ww/type :cell :wasm (get c :wasm) :grants {} :extra 1})",
            "(cell? (assoc c :anything 42))",
        ] {
            assert_eq!(
                eval_str(accepted, &mut env, &d).unwrap(),
                Val::Bool(true),
                "should accept: {accepted}"
            );
        }
        for rejected in [
            "(cell? nil)",
            "(cell? 42)",
            "(cell? {})",
            "(cell? [:ww/type :cell])",
            "(cell? {:ww/type :atom :wasm (get c :wasm) :grants {}})",
            "(cell? {:ww/type \"cell\"})",
        ] {
            assert_eq!(
                eval_str(rejected, &mut env, &d).unwrap(),
                Val::Bool(false),
                "should reject: {rejected}"
            );
        }
        let arity_err = eval_str("(cell?)", &mut env, &d).unwrap_err();
        assert_eq!(
            error::type_tag(err_payload(&arity_err)),
            Some(error::tag::ARITY)
        );
    }

    #[test]
    fn cell_accepts_programmatically_built_grant_map() {
        let mut env = cell_test_env();
        let db = env.get("db").unwrap().unwrap().clone();
        env.set(
            "bundle".into(),
            Val::Map(ValMap::from_pairs(vec![(
                Val::Keyword("renamed".into()),
                db,
            )])),
        );
        let d = RecordingDispatch::new();
        let caps = cell_caps(eval_str("(cell image :grants bundle)", &mut env, &d).unwrap());
        assert_eq!(caps.len(), 1);
        assert_eq!(caps[0].0, "renamed");
    }

    #[test]
    fn cell_rejects_non_capability_before_construction() {
        let mut env = cell_test_env();
        let d = RecordingDispatch::new();
        let err = eval_str("(cell image :grants {:db 42})", &mut env, &d).unwrap_err();
        let message = error::message(err_payload(&err)).unwrap();
        assert!(
            message.contains("grant \"db\" expected a capability, got int"),
            "got: {message}"
        );
    }

    #[test]
    fn cell_rejects_glia_native_defcap_with_grant_name() {
        let mut env = cell_test_env();
        let d = RecordingDispatch::new();
        eval_str(
            "(defcap local-logger :write (fn [message] message))",
            &mut env,
            &d,
        )
        .unwrap();
        let err =
            eval_str("(cell image :grants {:logger local-logger})", &mut env, &d).unwrap_err();
        let message = error::message(err_payload(&err)).unwrap();
        assert!(message.contains("grant \"logger\""), "got: {message}");
        assert!(
            message.contains("Glia-native capability that cannot yet cross a cell boundary"),
            "got: {message}"
        );
        assert!(message.contains("defcap-export"), "got: {message}");
    }

    #[test]
    fn cell_duplicate_literal_grant_reports_both_source_entries() {
        let mut env = cell_test_env();
        let d = RecordingDispatch::new();
        let err = eval_str("(cell image :grants {:db db :db logger})", &mut env, &d).unwrap_err();
        let message = error::message(err_payload(&err)).unwrap();
        assert!(
            message.contains("duplicate grant name \"db\""),
            "got: {message}"
        );
        assert!(message.contains("entry 1"), "got: {message}");
        assert!(message.contains("entry 2"), "got: {message}");
        assert!(
            message.contains("grant names must be unique"),
            "got: {message}"
        );
    }

    #[test]
    fn cell_rejects_malformed_and_unknown_keyword_arguments() {
        let mut env = cell_test_env();
        let d = RecordingDispatch::new();
        let missing =
            eval_str("(cell image :grants)", &mut env, &d).expect_err("missing map must fail");
        assert!(error::message(err_payload(&missing))
            .unwrap()
            .contains("missing grant map after :grants"));

        let unknown =
            eval_str("(cell image :inherit {})", &mut env, &d).expect_err("unknown keyword");
        assert!(error::message(err_payload(&unknown))
            .unwrap()
            .contains("unknown keyword :inherit"));

        let malformed =
            eval_str("(cell image {} {})", &mut env, &d).expect_err("malformed options");
        assert!(error::message(err_payload(&malformed))
            .unwrap()
            .contains("keyword :grants"));

        let string_name = eval_str("(cell image :grants {\"db\" db})", &mut env, &d)
            .expect_err("grant names must be keywords");
        assert!(error::message(err_payload(&string_name))
            .unwrap()
            .contains("cell :grants map key"));
    }

    #[test]
    fn analyzed_and_raw_cell_paths_have_identical_grant_behavior() {
        let form = crate::read("(cell image :grants {:renamed db :log logger})").unwrap();
        let mut analyzed_env = cell_test_env();
        let analyzed_dispatch = RecordingDispatch::new();
        let analyzed =
            cell_caps(eval_blocking(&form, &mut analyzed_env, &analyzed_dispatch).unwrap());

        let mut raw_env = cell_test_env();
        let raw_dispatch = RecordingDispatch::new();
        let raw = cell_caps(eval_raw_blocking(&form, &mut raw_env, &raw_dispatch).unwrap());

        assert_eq!(
            analyzed
                .iter()
                .map(|(name, _)| name.clone())
                .collect::<Vec<_>>(),
            raw.iter().map(|(name, _)| name.clone()).collect::<Vec<_>>()
        );

        let malformed = crate::read("(cell image :inherit {})").unwrap();
        let analyzed_error =
            eval_blocking(&malformed, &mut analyzed_env, &analyzed_dispatch).unwrap_err();
        let raw_error = eval_raw_blocking(&malformed, &mut raw_env, &raw_dispatch).unwrap_err();
        assert_eq!(
            error::message(err_payload(&analyzed_error)),
            error::message(err_payload(&raw_error))
        );

        let programmatic = crate::read("(cell image :grants bundle)").unwrap();
        let db = raw_env.get("db").unwrap().unwrap().clone();
        raw_env.set(
            "bundle".into(),
            Val::Map(ValMap::from_pairs(vec![(
                Val::Keyword("programmatic".into()),
                db,
            )])),
        );
        let raw_programmatic =
            cell_caps(eval_raw_blocking(&programmatic, &mut raw_env, &raw_dispatch).unwrap());
        assert_eq!(raw_programmatic.len(), 1);
        assert_eq!(raw_programmatic[0].0, "programmatic");
    }

    #[test]
    fn lexical_capabilities_are_not_captured_and_legacy_with_warns() {
        let mut env = cell_test_env();
        let mut d = RecordingDispatch::new();
        pollster_eval(crate::load_prelude(&mut env, &mut d));
        let caps =
            cell_caps(eval_str("(with [status-host db] (cell image))", &mut env, &d).unwrap());
        assert!(caps.is_empty());
        let warnings = d.warnings.borrow();
        assert_eq!(warnings.len(), 1);
        assert!(
            warnings[0].contains("lexical capability capture was removed"),
            "got: {}",
            warnings[0]
        );
        assert!(warnings[0].contains("status-host"), "got: {}", warnings[0]);
        assert!(
            warnings[0].contains(":grants {:status-host status-host}"),
            "got: {}",
            warnings[0]
        );
    }

    #[test]
    fn explicit_or_legitimate_zero_grant_cells_do_not_warn() {
        let mut env = cell_test_env();
        let mut d = RecordingDispatch::new();
        pollster_eval(crate::load_prelude(&mut env, &mut d));

        eval_str("(cell image)", &mut env, &d).unwrap();
        eval_str(
            "(with [status-host db] (cell image :grants {:host status-host}))",
            &mut env,
            &d,
        )
        .unwrap();
        assert!(d.warnings.borrow().is_empty());
    }

    #[test]
    fn closure_captured_capability_triggers_legacy_warning() {
        let mut env = cell_test_env();
        let mut d = RecordingDispatch::new();
        pollster_eval(crate::load_prelude(&mut env, &mut d));

        eval_str(
            "(def spawn
               (let [status-host db]
                 (fn [] (do status-host (cell image)))))",
            &mut env,
            &d,
        )
        .unwrap();
        let caps = cell_caps(eval_str("(spawn)", &mut env, &d).unwrap());
        assert!(caps.is_empty());
        let warnings = d.warnings.borrow();
        assert_eq!(warnings.len(), 1);
        assert!(warnings[0].contains("status-host"), "got: {}", warnings[0]);
        assert!(
            warnings[0].contains(":grants {:status-host status-host}"),
            "got: {}",
            warnings[0]
        );
    }

    #[test]
    fn host_effect_frame_resumes_without_guest_binding() {
        let mut env = Env::new();
        let d = RecordingDispatch::new();
        let form = crate::read("(perform :load \"x\")").unwrap();
        let handler: effect::HostEffectHandler = Rc::new(|data| {
            Box::pin(async move {
                assert_eq!(data, Val::Str("x".into()));
                Ok(effect::HostEffectResult::Resume(Val::Bytes(vec![1, 2])))
            })
        });
        let effects = [effect::HostEffect {
            target: effect::EffectTarget::Keyword("load".into()),
            handler,
        }];
        let result = pollster_eval(eval_toplevel_with_host_effects(
            &form, &mut env, &d, &effects,
        ));
        assert!(matches!(result, Ok(EvalOutcome::Value(Val::Bytes(ref b))) if b == &vec![1, 2]));
        assert!(env.get("load-handler").unwrap().is_none());
    }

    #[test]
    fn host_effect_exit_aborts_without_guest_value() {
        let mut env = Env::new();
        let d = RecordingDispatch::new();
        let form = crate::read("(do (perform :exit nil) 42)").unwrap();
        let handler: effect::HostEffectHandler =
            Rc::new(|_| Box::pin(async { Ok(effect::HostEffectResult::Exit) }));
        let effects = [effect::HostEffect {
            target: effect::EffectTarget::Keyword("exit".into()),
            handler,
        }];
        assert!(matches!(
            pollster_eval(eval_toplevel_with_host_effects(
                &form, &mut env, &d, &effects
            )),
            Ok(EvalOutcome::Exit)
        ));
    }

    #[test]
    fn host_effect_abort_cleans_guest_handler_frames() {
        let mut env = Env::new();
        let d = RecordingDispatch::new();
        let form = crate::read(
            "(with-effect-handler :outer (fn [value resume] (resume value)) (perform :load \"x\"))",
        )
        .unwrap();
        let handler: effect::HostEffectHandler =
            Rc::new(|_| Box::pin(async { Err(Val::from("load failed")) }));
        let effects = [effect::HostEffect {
            target: effect::EffectTarget::Keyword("load".into()),
            handler,
        }];

        assert!(pollster_eval(eval_toplevel_with_host_effects(
            &form, &mut env, &d, &effects
        ))
        .is_err());
        assert!(
            env.handler_stack.borrow().is_empty(),
            "host-effect abort must not leak guest handler frames"
        );
    }

    #[test]
    fn host_effect_exit_cleans_guest_handler_frames() {
        let mut env = Env::new();
        let d = RecordingDispatch::new();
        let form = crate::read(
            "(with-effect-handler :outer (fn [value resume] (resume value)) (perform :exit nil))",
        )
        .unwrap();
        let handler: effect::HostEffectHandler =
            Rc::new(|_| Box::pin(async { Ok(effect::HostEffectResult::Exit) }));
        let effects = [effect::HostEffect {
            target: effect::EffectTarget::Keyword("exit".into()),
            handler,
        }];

        assert!(matches!(
            pollster_eval(eval_toplevel_with_host_effects(
                &form, &mut env, &d, &effects
            )),
            Ok(EvalOutcome::Exit)
        ));
        assert!(
            env.handler_stack.borrow().is_empty(),
            "host-effect exit must not leak guest handler frames"
        );
    }

    #[test]
    fn guest_handler_interposes_before_host_effect_frame() {
        let mut env = Env::new();
        let d = RecordingDispatch::new();
        let form = crate::read(
            "(with-effect-handler :stdout (fn [value resume] (resume :guest)) (perform :stdout \"x\"))",
        )
        .unwrap();
        let handler: effect::HostEffectHandler = Rc::new(|_| {
            Box::pin(async {
                Ok(effect::HostEffectResult::Resume(Val::Keyword(
                    "host".into(),
                )))
            })
        });
        let effects = [effect::HostEffect {
            target: effect::EffectTarget::Keyword("stdout".into()),
            handler,
        }];
        assert!(matches!(
            pollster_eval(eval_toplevel_with_host_effects(&form, &mut env, &d, &effects)),
            Ok(EvalOutcome::Value(Val::Keyword(ref value))) if value == "guest"
        ));
    }

    /// Check if an error Val contains a substring in its :message field or Display output.
    fn err_contains(err: &Val, needle: &str) -> bool {
        // Check :message field in map
        if let Val::Map(m) = err {
            for (k, v) in m.iter() {
                if let (Val::Keyword(key), Val::Str(msg)) = (k, v) {
                    if key == "message" && msg.contains(needle) {
                        return true;
                    }
                }
            }
        }
        // Fallback: check Display output
        format!("{err}").contains(needle)
    }

    // --- list ---

    #[test]
    fn builtin_list_empty() {
        let mut env = Env::new();
        let d = RecordingDispatch::new();
        assert_eq!(eval_str("(list)", &mut env, &d), Ok(Val::List(vec![])));
    }

    #[test]
    fn builtin_list_with_args() {
        let mut env = Env::new();
        let d = RecordingDispatch::new();
        assert_eq!(
            eval_str("(list 1 2 3)", &mut env, &d),
            Ok(Val::List(vec![Val::Int(1), Val::Int(2), Val::Int(3)]))
        );
    }

    // --- cons ---

    #[test]
    fn builtin_cons_onto_list() {
        let mut env = Env::new();
        let d = RecordingDispatch::new();
        assert_eq!(
            eval_str("(cons 1 (list 2 3))", &mut env, &d),
            Ok(Val::List(vec![Val::Int(1), Val::Int(2), Val::Int(3)]))
        );
    }

    #[test]
    fn builtin_cons_wrong_args() {
        let mut env = Env::new();
        let d = RecordingDispatch::new();
        assert!(eval_str("(cons 1)", &mut env, &d).is_err());
    }

    #[test]
    fn builtin_cons_non_collection() {
        let mut env = Env::new();
        let d = RecordingDispatch::new();
        assert!(eval_str("(cons 1 2)", &mut env, &d).is_err());
    }

    // --- first ---

    #[test]
    fn builtin_first_of_list() {
        let mut env = Env::new();
        let d = RecordingDispatch::new();
        assert_eq!(
            eval_str("(first (list 1 2 3))", &mut env, &d),
            Ok(Val::Int(1))
        );
    }

    #[test]
    fn builtin_first_of_empty() {
        let mut env = Env::new();
        let d = RecordingDispatch::new();
        assert_eq!(eval_str("(first (list))", &mut env, &d), Ok(Val::Nil));
    }

    #[test]
    fn builtin_first_of_nil() {
        let mut env = Env::new();
        let d = RecordingDispatch::new();
        assert_eq!(eval_str("(first nil)", &mut env, &d), Ok(Val::Nil));
    }

    #[test]
    fn builtin_first_wrong_type() {
        let mut env = Env::new();
        let d = RecordingDispatch::new();
        assert!(eval_str("(first 42)", &mut env, &d).is_err());
    }

    // --- rest ---

    #[test]
    fn builtin_rest_of_list() {
        let mut env = Env::new();
        let d = RecordingDispatch::new();
        assert_eq!(
            eval_str("(rest (list 1 2 3))", &mut env, &d),
            Ok(Val::List(vec![Val::Int(2), Val::Int(3)]))
        );
    }

    #[test]
    fn builtin_rest_of_empty() {
        let mut env = Env::new();
        let d = RecordingDispatch::new();
        assert_eq!(
            eval_str("(rest (list))", &mut env, &d),
            Ok(Val::List(vec![]))
        );
    }

    #[test]
    fn builtin_rest_of_nil() {
        let mut env = Env::new();
        let d = RecordingDispatch::new();
        assert_eq!(eval_str("(rest nil)", &mut env, &d), Ok(Val::List(vec![])));
    }

    #[test]
    fn builtin_rest_wrong_type() {
        let mut env = Env::new();
        let d = RecordingDispatch::new();
        assert!(eval_str("(rest 42)", &mut env, &d).is_err());
    }

    // --- count ---

    #[test]
    fn builtin_count_list() {
        let mut env = Env::new();
        let d = RecordingDispatch::new();
        assert_eq!(
            eval_str("(count (list 1 2 3))", &mut env, &d),
            Ok(Val::Int(3))
        );
    }

    #[test]
    fn builtin_count_nil() {
        let mut env = Env::new();
        let d = RecordingDispatch::new();
        assert_eq!(eval_str("(count nil)", &mut env, &d), Ok(Val::Int(0)));
    }

    #[test]
    fn builtin_count_string_chars() {
        let mut env = Env::new();
        let d = RecordingDispatch::new();
        // Unicode: each emoji is one char
        assert_eq!(
            eval_str(r#"(count "hello")"#, &mut env, &d),
            Ok(Val::Int(5))
        );
    }

    #[test]
    fn builtin_count_wrong_type() {
        let mut env = Env::new();
        let d = RecordingDispatch::new();
        assert!(eval_str("(count 42)", &mut env, &d).is_err());
    }

    // --- vec ---

    #[test]
    fn builtin_vec_from_list() {
        let mut env = Env::new();
        let d = RecordingDispatch::new();
        assert_eq!(
            eval_str("(vec (list 1 2))", &mut env, &d),
            Ok(Val::Vector(vec![Val::Int(1), Val::Int(2)]))
        );
    }

    #[test]
    fn builtin_vec_from_nil() {
        let mut env = Env::new();
        let d = RecordingDispatch::new();
        assert_eq!(eval_str("(vec nil)", &mut env, &d), Ok(Val::Vector(vec![])));
    }

    #[test]
    fn builtin_vec_wrong_type() {
        let mut env = Env::new();
        let d = RecordingDispatch::new();
        assert!(eval_str("(vec 42)", &mut env, &d).is_err());
    }

    // --- get ---

    #[test]
    fn builtin_get_map() {
        let mut env = Env::new();
        let d = RecordingDispatch::new();
        assert_eq!(
            eval_str("(get {:a 1 :b 2} :b)", &mut env, &d),
            Ok(Val::Int(2))
        );
    }

    #[test]
    fn builtin_get_map_missing() {
        let mut env = Env::new();
        let d = RecordingDispatch::new();
        assert_eq!(eval_str("(get {:a 1} :z)", &mut env, &d), Ok(Val::Nil));
    }

    #[test]
    fn builtin_get_vector() {
        let mut env = Env::new();
        let d = RecordingDispatch::new();
        assert_eq!(
            eval_str("(get [10 20 30] 1)", &mut env, &d),
            Ok(Val::Int(20))
        );
    }

    #[test]
    fn builtin_get_nil() {
        let mut env = Env::new();
        let d = RecordingDispatch::new();
        assert_eq!(eval_str("(get nil :a)", &mut env, &d), Ok(Val::Nil));
    }

    #[test]
    fn builtin_get_wrong_type() {
        let mut env = Env::new();
        let d = RecordingDispatch::new();
        assert!(eval_str("(get 42 0)", &mut env, &d).is_err());
    }

    // --- assoc ---

    #[test]
    fn builtin_assoc_add_key() {
        let mut env = Env::new();
        let d = RecordingDispatch::new();
        assert_eq!(
            eval_str("(assoc {:a 1} :b 2)", &mut env, &d),
            Ok(Val::Map(ValMap::from_pairs(vec![
                (Val::Keyword("a".into()), Val::Int(1)),
                (Val::Keyword("b".into()), Val::Int(2)),
            ])))
        );
    }

    #[test]
    fn builtin_assoc_update_key() {
        let mut env = Env::new();
        let d = RecordingDispatch::new();
        assert_eq!(
            eval_str("(assoc {:a 1} :a 99)", &mut env, &d),
            Ok(Val::Map(ValMap::from_pairs(vec![(
                Val::Keyword("a".into()),
                Val::Int(99)
            )])))
        );
    }

    #[test]
    fn builtin_assoc_wrong_args() {
        let mut env = Env::new();
        let d = RecordingDispatch::new();
        // Even number of args (map + 1 key, no value)
        assert!(eval_str("(assoc {:a 1} :b)", &mut env, &d).is_err());
    }

    // --- conj ---

    #[test]
    fn builtin_conj_vector_appends() {
        let mut env = Env::new();
        let d = RecordingDispatch::new();
        assert_eq!(
            eval_str("(conj [1 2] 3)", &mut env, &d),
            Ok(Val::Vector(vec![Val::Int(1), Val::Int(2), Val::Int(3)]))
        );
    }

    #[test]
    fn builtin_conj_list_prepends() {
        let mut env = Env::new();
        let d = RecordingDispatch::new();
        assert_eq!(
            eval_str("(conj (list 2 3) 1)", &mut env, &d),
            Ok(Val::List(vec![Val::Int(1), Val::Int(2), Val::Int(3)]))
        );
    }

    #[test]
    fn builtin_conj_map() {
        let mut env = Env::new();
        let d = RecordingDispatch::new();
        assert_eq!(
            eval_str("(conj {:a 1} [:b 2])", &mut env, &d),
            Ok(Val::Map(ValMap::from_pairs(vec![
                (Val::Keyword("a".into()), Val::Int(1)),
                (Val::Keyword("b".into()), Val::Int(2)),
            ])))
        );
    }

    #[test]
    fn builtin_conj_too_few_args() {
        let mut env = Env::new();
        let d = RecordingDispatch::new();
        assert!(eval_str("(conj [1])", &mut env, &d).is_err());
    }

    // --- Arithmetic ---

    #[test]
    fn builtin_add_ints() {
        let mut env = Env::new();
        let d = RecordingDispatch::new();
        assert_eq!(eval_str("(+ 1 2 3)", &mut env, &d), Ok(Val::Int(6)));
    }

    #[test]
    fn builtin_add_empty() {
        let mut env = Env::new();
        let d = RecordingDispatch::new();
        assert_eq!(eval_str("(+)", &mut env, &d), Ok(Val::Int(0)));
    }

    #[test]
    fn builtin_add_float_promotion() {
        let mut env = Env::new();
        let d = RecordingDispatch::new();
        assert_eq!(eval_str("(+ 1 2.0)", &mut env, &d), Ok(Val::Float(3.0)));
    }

    #[test]
    fn builtin_add_non_number() {
        let mut env = Env::new();
        let d = RecordingDispatch::new();
        assert!(eval_str(r#"(+ 1 "a")"#, &mut env, &d).is_err());
    }

    #[test]
    fn builtin_sub_two() {
        let mut env = Env::new();
        let d = RecordingDispatch::new();
        assert_eq!(eval_str("(- 10 3)", &mut env, &d), Ok(Val::Int(7)));
    }

    #[test]
    fn builtin_sub_negate() {
        let mut env = Env::new();
        let d = RecordingDispatch::new();
        assert_eq!(eval_str("(- 5)", &mut env, &d), Ok(Val::Int(-5)));
    }

    #[test]
    fn builtin_sub_empty_error() {
        let mut env = Env::new();
        let d = RecordingDispatch::new();
        assert!(eval_str("(-)", &mut env, &d).is_err());
    }

    #[test]
    fn builtin_mul_ints() {
        let mut env = Env::new();
        let d = RecordingDispatch::new();
        assert_eq!(eval_str("(* 2 3 4)", &mut env, &d), Ok(Val::Int(24)));
    }

    #[test]
    fn builtin_mul_empty() {
        let mut env = Env::new();
        let d = RecordingDispatch::new();
        assert_eq!(eval_str("(*)", &mut env, &d), Ok(Val::Int(1)));
    }

    #[test]
    fn builtin_div_ints() {
        let mut env = Env::new();
        let d = RecordingDispatch::new();
        assert_eq!(eval_str("(/ 10 3)", &mut env, &d), Ok(Val::Int(3)));
    }

    #[test]
    fn builtin_div_by_zero() {
        let mut env = Env::new();
        let d = RecordingDispatch::new();
        assert!(eval_str("(/ 10 0)", &mut env, &d).is_err());
    }

    #[test]
    fn builtin_div_wrong_args() {
        let mut env = Env::new();
        let d = RecordingDispatch::new();
        assert!(eval_str("(/ 1)", &mut env, &d).is_err());
    }

    #[test]
    fn builtin_mod_ints() {
        let mut env = Env::new();
        let d = RecordingDispatch::new();
        assert_eq!(eval_str("(mod 10 3)", &mut env, &d), Ok(Val::Int(1)));
    }

    #[test]
    fn builtin_mod_by_zero() {
        let mut env = Env::new();
        let d = RecordingDispatch::new();
        assert!(eval_str("(mod 10 0)", &mut env, &d).is_err());
    }

    // --- Comparison ---

    #[test]
    fn builtin_eq_true() {
        let mut env = Env::new();
        let d = RecordingDispatch::new();
        assert_eq!(eval_str("(= 1 1)", &mut env, &d), Ok(Val::Bool(true)));
    }

    #[test]
    fn builtin_eq_false() {
        let mut env = Env::new();
        let d = RecordingDispatch::new();
        assert_eq!(eval_str("(= 1 2)", &mut env, &d), Ok(Val::Bool(false)));
    }

    #[test]
    fn builtin_eq_wrong_args() {
        let mut env = Env::new();
        let d = RecordingDispatch::new();
        assert!(eval_str("(= 1)", &mut env, &d).is_err());
    }

    #[test]
    fn builtin_lt_true() {
        let mut env = Env::new();
        let d = RecordingDispatch::new();
        assert_eq!(eval_str("(< 1 2)", &mut env, &d), Ok(Val::Bool(true)));
    }

    #[test]
    fn builtin_lt_false() {
        let mut env = Env::new();
        let d = RecordingDispatch::new();
        assert_eq!(eval_str("(< 2 1)", &mut env, &d), Ok(Val::Bool(false)));
    }

    #[test]
    fn builtin_gt_true() {
        let mut env = Env::new();
        let d = RecordingDispatch::new();
        assert_eq!(eval_str("(> 2 1)", &mut env, &d), Ok(Val::Bool(true)));
    }

    #[test]
    fn builtin_le_equal() {
        let mut env = Env::new();
        let d = RecordingDispatch::new();
        assert_eq!(eval_str("(<= 2 2)", &mut env, &d), Ok(Val::Bool(true)));
    }

    #[test]
    fn builtin_ge_equal() {
        let mut env = Env::new();
        let d = RecordingDispatch::new();
        assert_eq!(eval_str("(>= 2 2)", &mut env, &d), Ok(Val::Bool(true)));
    }

    #[test]
    fn builtin_comparison_mixed_numeric() {
        let mut env = Env::new();
        let d = RecordingDispatch::new();
        assert_eq!(eval_str("(< 1 2.5)", &mut env, &d), Ok(Val::Bool(true)));
    }

    #[test]
    fn builtin_comparison_non_number() {
        let mut env = Env::new();
        let d = RecordingDispatch::new();
        assert!(eval_str(r#"(< 1 "a")"#, &mut env, &d).is_err());
    }

    // --- gensym ---

    #[test]
    fn builtin_gensym() {
        let mut env = Env::new();
        let d = RecordingDispatch::new();
        let r1 = eval_str("(gensym)", &mut env, &d).unwrap();
        let r2 = eval_str("(gensym)", &mut env, &d).unwrap();
        // Each gensym returns a unique symbol
        match (&r1, &r2) {
            (Val::Sym(s1), Val::Sym(s2)) => {
                assert!(s1.starts_with("G__"));
                assert!(s2.starts_with("G__"));
                assert_ne!(s1, s2);
            }
            _ => panic!("gensym should return Sym, got {r1} and {r2}"),
        }
    }

    #[test]
    fn builtin_gensym_no_args() {
        let mut env = Env::new();
        let d = RecordingDispatch::new();
        assert!(eval_str("(gensym 1)", &mut env, &d).is_err());
    }

    // --- apply ---

    #[test]
    fn builtin_apply_builtin_fn() {
        let mut env = Env::new();
        let d = RecordingDispatch::new();
        assert_eq!(
            eval_str("(apply + (list 1 2 3))", &mut env, &d),
            Ok(Val::Int(6))
        );
    }

    #[test]
    fn builtin_apply_user_fn() {
        let mut env = Env::new();
        let d = RecordingDispatch::new();
        eval_str("(def f (fn [x y] (+ x y)))", &mut env, &d).unwrap();
        assert_eq!(
            eval_str("(apply f (list 3 4))", &mut env, &d),
            Ok(Val::Int(7))
        );
    }

    #[test]
    fn builtin_apply_with_middle_args() {
        let mut env = Env::new();
        let d = RecordingDispatch::new();
        // (apply + 1 2 (list 3)) → (+ 1 2 3) → 6
        assert_eq!(
            eval_str("(apply + 1 2 (list 3))", &mut env, &d),
            Ok(Val::Int(6))
        );
    }

    #[test]
    fn builtin_apply_too_few_args() {
        let mut env = Env::new();
        let d = RecordingDispatch::new();
        assert!(eval_str("(apply +)", &mut env, &d).is_err());
    }

    #[test]
    fn builtin_apply_non_collection_last() {
        let mut env = Env::new();
        let d = RecordingDispatch::new();
        assert!(eval_str("(apply + 1 2)", &mut env, &d).is_err());
    }

    #[test]
    fn builtin_apply_fn_value() {
        let mut env = Env::new();
        let d = RecordingDispatch::new();
        // apply with a fn value (not symbol)
        eval_str("(def f (fn [x] (+ x 1)))", &mut env, &d).unwrap();
        assert_eq!(eval_str("(apply f [10])", &mut env, &d), Ok(Val::Int(11)));
    }

    // --- Integration: builtins with special forms ---

    #[test]
    fn builtin_in_let() {
        let mut env = Env::new();
        let d = RecordingDispatch::new();
        assert_eq!(
            eval_str("(let [x (+ 1 2)] (* x 10))", &mut env, &d),
            Ok(Val::Int(30))
        );
    }

    #[test]
    fn builtin_in_fn() {
        let mut env = Env::new();
        let d = RecordingDispatch::new();
        eval_str("(def add (fn [a b] (+ a b)))", &mut env, &d).unwrap();
        assert_eq!(eval_str("(add 3 4)", &mut env, &d), Ok(Val::Int(7)));
    }

    #[test]
    fn builtin_nested() {
        let mut env = Env::new();
        let d = RecordingDispatch::new();
        assert_eq!(
            eval_str("(+ (* 2 3) (- 10 4))", &mut env, &d),
            Ok(Val::Int(12))
        );
    }

    // =========================================================================
    // defmacro tests
    // =========================================================================

    #[test]
    fn defmacro_basic() {
        let mut env = Env::new();
        let d = RecordingDispatch::new();
        // Define a macro that returns a constant form
        eval_str("(defmacro m [] 42)", &mut env, &d).unwrap();
        assert_eq!(eval_str("(m)", &mut env, &d), Ok(Val::Int(42)));
    }

    #[test]
    fn defmacro_receives_unevaluated_args() {
        let mut env = Env::new();
        let d = RecordingDispatch::new();
        // Macro that receives a form and quotes it (returns it without eval)
        // (defmacro identity-form [x] x) — returns the raw form
        eval_str("(defmacro identity-form [x] x)", &mut env, &d).unwrap();
        // (identity-form 42) → eval(42) → 42
        assert_eq!(
            eval_str("(identity-form 42)", &mut env, &d),
            Ok(Val::Int(42))
        );
    }

    #[test]
    fn defmacro_expansion_is_re_evaluated() {
        let mut env = Env::new();
        let d = RecordingDispatch::new();
        // Macro that constructs a (+ 1 2) form using list and quote
        eval_str(r#"(defmacro add12 [] (list (quote +) 1 2))"#, &mut env, &d).unwrap();
        // (add12) → expands to (+ 1 2) → evaluates to 3
        assert_eq!(eval_str("(add12)", &mut env, &d), Ok(Val::Int(3)));
    }

    #[test]
    fn defmacro_stored_in_root() {
        let mut env = Env::new();
        let d = RecordingDispatch::new();
        // Define macro inside a let — should still be in root
        eval_str("(let [x 1] (defmacro m [] 99))", &mut env, &d).unwrap();
        assert_eq!(eval_str("(m)", &mut env, &d), Ok(Val::Int(99)));
    }

    #[test]
    fn defmacro_no_name_errors() {
        let mut env = Env::new();
        let d = RecordingDispatch::new();
        assert!(eval_str("(defmacro)", &mut env, &d).is_err());
    }

    #[test]
    fn defmacro_no_params_errors() {
        let mut env = Env::new();
        let d = RecordingDispatch::new();
        assert!(eval_str("(defmacro m)", &mut env, &d).is_err());
    }

    #[test]
    fn defmacro_non_symbol_name_errors() {
        let mut env = Env::new();
        let d = RecordingDispatch::new();
        assert!(eval_str("(defmacro 42 [] nil)", &mut env, &d).is_err());
    }

    #[test]
    fn defmacro_variadic() {
        let mut env = Env::new();
        let d = RecordingDispatch::new();
        // Macro with variadic args — wraps everything in a list call
        eval_str(
            "(defmacro wrap [& forms] (cons (quote list) forms))",
            &mut env,
            &d,
        )
        .unwrap();
        // (wrap 1 2 3) → expands to (list 1 2 3) → (1 2 3)
        assert_eq!(
            eval_str("(wrap 1 2 3)", &mut env, &d),
            Ok(Val::List(vec![Val::Int(1), Val::Int(2), Val::Int(3)]))
        );
    }

    // --- Integration: defmacro + builtins ---

    #[test]
    fn defmacro_uses_builtins_to_construct_forms() {
        let mut env = Env::new();
        let d = RecordingDispatch::new();
        // A "when" macro: (when test body...) → (if test (do body...) nil)
        eval_str(
            r#"(defmacro when [test & body]
                (list (quote if) test (cons (quote do) body) nil))"#,
            &mut env,
            &d,
        )
        .unwrap();
        assert_eq!(
            eval_str("(when true (+ 1 2))", &mut env, &d),
            Ok(Val::Int(3))
        );
        assert_eq!(eval_str("(when false (+ 1 2))", &mut env, &d), Ok(Val::Nil));
    }

    #[test]
    fn defmacro_unless_integration() {
        let mut env = Env::new();
        let d = RecordingDispatch::new();
        // (unless test body...) → (if test nil (do body...))
        eval_str(
            r#"(defmacro unless [test & body]
                (list (quote if) test nil (cons (quote do) body)))"#,
            &mut env,
            &d,
        )
        .unwrap();
        assert_eq!(
            eval_str("(unless false 42)", &mut env, &d),
            Ok(Val::Int(42))
        );
        assert_eq!(eval_str("(unless true 42)", &mut env, &d), Ok(Val::Nil));
    }

    #[test]
    fn defmacro_with_gensym() {
        let mut env = Env::new();
        let d = RecordingDispatch::new();
        // Macro that uses gensym to avoid name collisions
        // This just tests that gensym can be called from a macro body
        eval_str("(defmacro test-gensym [] (do (gensym) 42))", &mut env, &d).unwrap();
        assert_eq!(eval_str("(test-gensym)", &mut env, &d), Ok(Val::Int(42)));
    }

    // --- concat builtin tests ---

    #[test]
    fn builtin_concat_two_lists() {
        let mut env = Env::new();
        let d = RecordingDispatch::new();
        assert_eq!(
            eval_str("(concat (list 1 2) (list 3 4))", &mut env, &d),
            Ok(Val::List(vec![
                Val::Int(1),
                Val::Int(2),
                Val::Int(3),
                Val::Int(4),
            ]))
        );
    }

    #[test]
    fn builtin_concat_empty() {
        let mut env = Env::new();
        let d = RecordingDispatch::new();
        assert_eq!(eval_str("(concat)", &mut env, &d), Ok(Val::List(vec![])));
    }

    #[test]
    fn builtin_concat_with_nil() {
        let mut env = Env::new();
        let d = RecordingDispatch::new();
        assert_eq!(
            eval_str("(concat (list 1) nil (list 2))", &mut env, &d),
            Ok(Val::List(vec![Val::Int(1), Val::Int(2)]))
        );
    }

    #[test]
    fn builtin_concat_with_vector() {
        let mut env = Env::new();
        let d = RecordingDispatch::new();
        assert_eq!(
            eval_str("(concat [1 2] (list 3))", &mut env, &d),
            Ok(Val::List(vec![Val::Int(1), Val::Int(2), Val::Int(3)]))
        );
    }

    #[test]
    fn builtin_concat_non_seq_error() {
        let mut env = Env::new();
        let d = RecordingDispatch::new();
        assert!(eval_str("(concat 42)", &mut env, &d).is_err());
    }

    // --- Syntax-quote integration tests ---

    #[test]
    fn syntax_quote_when_macro() {
        let mut env = Env::new();
        let d = RecordingDispatch::new();
        eval_str(
            "(defmacro when [test & body] `(if ~test (do ~@body) nil))",
            &mut env,
            &d,
        )
        .unwrap();
        assert_eq!(eval_str("(when true 1 2 3)", &mut env, &d), Ok(Val::Int(3)));
        assert_eq!(eval_str("(when false 1 2 3)", &mut env, &d), Ok(Val::Nil));
    }

    #[test]
    fn syntax_quote_simple_expansion() {
        let mut env = Env::new();
        let d = RecordingDispatch::new();
        // Syntax-quote in a let produces a data structure
        assert_eq!(
            eval_str("(let [x 42] `(+ ~x 1))", &mut env, &d),
            Ok(Val::List(vec![
                Val::Sym("+".into()),
                Val::Int(42),
                Val::Int(1),
            ]))
        );
    }

    #[test]
    fn syntax_quote_splice_expansion() {
        let mut env = Env::new();
        let d = RecordingDispatch::new();
        assert_eq!(
            eval_str("(let [xs (list 1 2 3)] `(+ ~@xs))", &mut env, &d,),
            Ok(Val::List(vec![
                Val::Sym("+".into()),
                Val::Int(1),
                Val::Int(2),
                Val::Int(3),
            ]))
        );
    }

    #[test]
    fn syntax_quote_unless_macro() {
        let mut env = Env::new();
        let d = RecordingDispatch::new();
        eval_str(
            "(defmacro unless [test & body] `(if ~test nil (do ~@body)))",
            &mut env,
            &d,
        )
        .unwrap();
        assert_eq!(
            eval_str("(unless false 1 2 3)", &mut env, &d),
            Ok(Val::Int(3))
        );
        assert_eq!(eval_str("(unless true 1 2 3)", &mut env, &d), Ok(Val::Nil));
    }

    #[test]
    fn syntax_quote_preserves_keywords() {
        let mut env = Env::new();
        let d = RecordingDispatch::new();
        // Keywords are self-evaluating — should pass through syntax-quote
        assert_eq!(
            eval_str("`(:a ~(+ 1 2))", &mut env, &d),
            Ok(Val::List(vec![Val::Keyword("a".into()), Val::Int(3)]))
        );
    }

    #[test]
    fn unquote_outside_syntax_quote_errors() {
        let mut env = Env::new();
        let d = RecordingDispatch::new();
        let result = eval_str("(unquote x)", &mut env, &d);
        assert!(result.is_err());
        assert!(err_contains(
            err_payload(&result.unwrap_err()),
            "not inside syntax-quote"
        ));
    }

    #[test]
    fn splice_unquote_outside_syntax_quote_errors() {
        let mut env = Env::new();
        let d = RecordingDispatch::new();
        let result = eval_str("(splice-unquote x)", &mut env, &d);
        assert!(result.is_err());
        assert!(err_contains(
            err_payload(&result.unwrap_err()),
            "not inside syntax-quote"
        ));
    }

    // Prelude tests
    // =========================================================================

    /// Helper: load the prelude then parse + eval a string expression.
    fn prelude_eval(input: &str) -> Result<Val, EvalError> {
        let mut env = Env::new();
        let d = RecordingDispatch::new();
        // Load prelude forms into the environment
        let prelude_forms = crate::read_many(crate::PRELUDE)
            .map_err(|e| boundary_thrown(Val::from(format!("prelude parse: {e}"))))?;
        for form in &prelude_forms {
            eval_blocking(form, &mut env, &d)?;
        }
        // Now eval the test expression
        eval_str(input, &mut env, &d)
    }

    #[test]
    fn prelude_not_true() {
        assert_eq!(prelude_eval("(not true)"), Ok(Val::Bool(false)));
    }

    #[test]
    fn prelude_not_false() {
        assert_eq!(prelude_eval("(not false)"), Ok(Val::Bool(true)));
    }

    #[test]
    fn prelude_not_nil() {
        assert_eq!(prelude_eval("(not nil)"), Ok(Val::Bool(true)));
    }

    #[test]
    fn prelude_not_truthy() {
        // Non-nil, non-false values are truthy → not returns false
        assert_eq!(prelude_eval("(not 42)"), Ok(Val::Bool(false)));
    }

    #[test]
    fn prelude_when_true() {
        assert_eq!(prelude_eval("(when true 1 2 3)"), Ok(Val::Int(3)));
    }

    #[test]
    fn prelude_when_false() {
        assert_eq!(prelude_eval("(when false 1 2 3)"), Ok(Val::Nil));
    }

    #[test]
    fn prelude_when_not_false() {
        assert_eq!(prelude_eval("(when-not false 42)"), Ok(Val::Int(42)));
    }

    #[test]
    fn prelude_when_not_true() {
        assert_eq!(prelude_eval("(when-not true 42)"), Ok(Val::Nil));
    }

    #[test]
    fn prelude_and_empty() {
        assert_eq!(prelude_eval("(and)"), Ok(Val::Bool(true)));
    }

    #[test]
    fn prelude_and_single() {
        assert_eq!(prelude_eval("(and 42)"), Ok(Val::Int(42)));
    }

    #[test]
    fn prelude_and_two_truthy() {
        assert_eq!(prelude_eval("(and 1 2)"), Ok(Val::Int(2)));
    }

    #[test]
    fn prelude_and_short_circuit() {
        assert_eq!(prelude_eval("(and false 2)"), Ok(Val::Bool(false)));
    }

    #[test]
    fn prelude_and_nil_short_circuit() {
        assert_eq!(prelude_eval("(and nil 2)"), Ok(Val::Nil));
    }

    #[test]
    fn prelude_or_empty() {
        assert_eq!(prelude_eval("(or)"), Ok(Val::Nil));
    }

    #[test]
    fn prelude_or_single() {
        assert_eq!(prelude_eval("(or 42)"), Ok(Val::Int(42)));
    }

    #[test]
    fn prelude_or_first_truthy() {
        assert_eq!(prelude_eval("(or 1 2)"), Ok(Val::Int(1)));
    }

    #[test]
    fn prelude_or_skip_nil() {
        assert_eq!(prelude_eval("(or nil 2)"), Ok(Val::Int(2)));
    }

    #[test]
    fn prelude_or_skip_false_nil() {
        assert_eq!(prelude_eval("(or false nil 3)"), Ok(Val::Int(3)));
    }

    #[test]
    fn prelude_cond_basic() {
        assert_eq!(prelude_eval("(cond false 1 true 2)"), Ok(Val::Int(2)));
    }

    #[test]
    fn prelude_cond_default() {
        assert_eq!(prelude_eval("(cond false 1 42)"), Ok(Val::Int(42)));
    }

    #[test]
    fn prelude_cond_empty() {
        assert_eq!(prelude_eval("(cond)"), Ok(Val::Nil));
    }

    #[test]
    fn prelude_cond_first_match() {
        assert_eq!(prelude_eval("(cond true 1 true 2)"), Ok(Val::Int(1)));
    }

    #[test]
    fn prelude_defn_basic() {
        assert_eq!(
            prelude_eval("(do (defn add [a b] (+ a b)) (add 1 2))"),
            Ok(Val::Int(3))
        );
    }

    #[test]
    fn prelude_defn_multi_body() {
        assert_eq!(
            prelude_eval("(do (defn f [x] 1 2 (+ x 10)) (f 5))"),
            Ok(Val::Int(15))
        );
    }

    // =========================================================================
    // fn recur tests (#225)
    // =========================================================================

    #[test]
    fn fn_recur_factorial() {
        let mut env = Env::new();
        let d = RecordingDispatch::new();
        // Define factorial with recur
        eval_str(
            "(def factorial (fn [n acc] (if (= n 0) acc (recur (- n 1) (* acc n)))))",
            &mut env,
            &d,
        )
        .unwrap();
        assert_eq!(eval_str("(factorial 5 1)", &mut env, &d), Ok(Val::Int(120)));
    }

    #[test]
    fn fn_recur_countdown() {
        let mut env = Env::new();
        let d = RecordingDispatch::new();
        eval_str(
            r#"(def countdown (fn [n] (if (= n 0) "done" (recur (- n 1)))))"#,
            &mut env,
            &d,
        )
        .unwrap();
        assert_eq!(
            eval_str("(countdown 100)", &mut env, &d),
            Ok(Val::Str("done".into()))
        );
    }

    #[test]
    fn fn_recur_no_recur_regression() {
        // Normal fn without recur must still work
        let mut env = Env::new();
        let d = RecordingDispatch::new();
        eval_str("(def add (fn [a b] (+ a b)))", &mut env, &d).unwrap();
        assert_eq!(eval_str("(add 3 4)", &mut env, &d), Ok(Val::Int(7)));
    }

    #[test]
    fn fn_recur_wrong_arity() {
        let mut env = Env::new();
        let d = RecordingDispatch::new();
        eval_str("(def f (fn [a b] (recur 1)))", &mut env, &d).unwrap();
        let err = eval_str("(f 1 2)", &mut env, &d).unwrap_err();
        assert!(err_contains(err_payload(&err), "expected 2"), "got: {err}");
    }

    #[test]
    fn fn_recur_variadic() {
        let mut env = Env::new();
        let d = RecordingDispatch::new();
        // Variadic fn that sums via recur: acc + first of rest, recur with rest
        eval_str(
            "(def sum-all (fn [acc & nums] (if (= (count nums) 0) acc (recur (+ acc (first nums)) (rest nums)))))",
            &mut env,
            &d,
        )
        .unwrap();
        // sum-all 0 1 2 3 → 6
        // Note: recur with variadic expects fixed_params + 1 args (the rest becomes a list)
        assert_eq!(eval_str("(sum-all 0 1 2 3)", &mut env, &d), Ok(Val::Int(6)));
    }

    #[test]
    fn fn_recur_single_iteration() {
        let mut env = Env::new();
        let d = RecordingDispatch::new();
        eval_str("(def once (fn [x] (if x (recur false) 42)))", &mut env, &d).unwrap();
        assert_eq!(eval_str("(once true)", &mut env, &d), Ok(Val::Int(42)));
    }

    // =========================================================================
    // Stdlib tests (#202)
    // =========================================================================

    #[test]
    fn stdlib_type_int() {
        let mut env = Env::new();
        let d = RecordingDispatch::new();
        assert_eq!(
            eval_str("(type 42)", &mut env, &d),
            Ok(Val::Keyword("int".into()))
        );
    }

    #[test]
    fn stdlib_type_nil() {
        let mut env = Env::new();
        let d = RecordingDispatch::new();
        assert_eq!(
            eval_str("(type nil)", &mut env, &d),
            Ok(Val::Keyword("nil".into()))
        );
    }

    #[test]
    fn stdlib_type_fn() {
        let mut env = Env::new();
        let d = RecordingDispatch::new();
        assert_eq!(
            eval_str("(type (fn [x] x))", &mut env, &d),
            Ok(Val::Keyword("fn".into()))
        );
    }

    #[test]
    fn stdlib_nil_pred() {
        let mut env = Env::new();
        let d = RecordingDispatch::new();
        assert_eq!(eval_str("(nil? nil)", &mut env, &d), Ok(Val::Bool(true)));
        assert_eq!(eval_str("(nil? 0)", &mut env, &d), Ok(Val::Bool(false)));
    }

    #[test]
    fn stdlib_some_pred() {
        let mut env = Env::new();
        let d = RecordingDispatch::new();
        assert_eq!(eval_str("(some? nil)", &mut env, &d), Ok(Val::Bool(false)));
        assert_eq!(eval_str("(some? 0)", &mut env, &d), Ok(Val::Bool(true)));
    }

    #[test]
    fn stdlib_empty_pred() {
        let mut env = Env::new();
        let d = RecordingDispatch::new();
        assert_eq!(eval_str("(empty? nil)", &mut env, &d), Ok(Val::Bool(true)));
        assert_eq!(
            eval_str("(empty? (list))", &mut env, &d),
            Ok(Val::Bool(true))
        );
        assert_eq!(
            eval_str("(empty? (list 1))", &mut env, &d),
            Ok(Val::Bool(false))
        );
    }

    #[test]
    fn stdlib_contains_map() {
        let mut env = Env::new();
        let d = RecordingDispatch::new();
        assert_eq!(
            eval_str("(contains? {:a 1 :b 2} :a)", &mut env, &d),
            Ok(Val::Bool(true))
        );
        assert_eq!(
            eval_str("(contains? {:a 1} :z)", &mut env, &d),
            Ok(Val::Bool(false))
        );
    }

    #[test]
    fn stdlib_str_empty() {
        let mut env = Env::new();
        let d = RecordingDispatch::new();
        assert_eq!(eval_str("(str)", &mut env, &d), Ok(Val::Str("".into())));
    }

    #[test]
    fn stdlib_str_concat() {
        let mut env = Env::new();
        let d = RecordingDispatch::new();
        assert_eq!(
            eval_str(r#"(str "hello" " " "world")"#, &mut env, &d),
            Ok(Val::Str("hello world".into()))
        );
    }

    #[test]
    fn stdlib_str_nil_empty() {
        let mut env = Env::new();
        let d = RecordingDispatch::new();
        assert_eq!(
            eval_str(r#"(str "a" nil "b")"#, &mut env, &d),
            Ok(Val::Str("ab".into()))
        );
    }

    #[test]
    fn stdlib_name_keyword() {
        let mut env = Env::new();
        let d = RecordingDispatch::new();
        assert_eq!(
            eval_str("(name :foo)", &mut env, &d),
            Ok(Val::Str("foo".into()))
        );
    }

    #[test]
    fn stdlib_name_symbol() {
        let mut env = Env::new();
        let d = RecordingDispatch::new();
        assert_eq!(
            eval_str("(name 'bar)", &mut env, &d),
            Ok(Val::Str("bar".into()))
        );
    }

    #[test]
    fn stdlib_map_basic() {
        let mut env = Env::new();
        let d = RecordingDispatch::new();
        eval_str("(def inc (fn [x] (+ x 1)))", &mut env, &d).unwrap();
        assert_eq!(
            eval_str("(map inc (list 1 2 3))", &mut env, &d),
            Ok(Val::List(vec![Val::Int(2), Val::Int(3), Val::Int(4)]))
        );
    }

    #[test]
    fn stdlib_map_empty() {
        let mut env = Env::new();
        let d = RecordingDispatch::new();
        eval_str("(def f (fn [x] x))", &mut env, &d).unwrap();
        assert_eq!(
            eval_str("(map f (list))", &mut env, &d),
            Ok(Val::List(vec![]))
        );
    }

    #[test]
    fn stdlib_filter_basic() {
        let mut env = Env::new();
        let d = RecordingDispatch::new();
        eval_str("(def pos? (fn [x] (> x 0)))", &mut env, &d).unwrap();
        assert_eq!(
            eval_str("(filter pos? (list -1 0 1 2 -3))", &mut env, &d),
            Ok(Val::List(vec![Val::Int(1), Val::Int(2)]))
        );
    }

    #[test]
    fn stdlib_reduce_with_init() {
        let mut env = Env::new();
        let d = RecordingDispatch::new();
        eval_str("(def add (fn [a b] (+ a b)))", &mut env, &d).unwrap();
        assert_eq!(
            eval_str("(reduce add 0 (list 1 2 3))", &mut env, &d),
            Ok(Val::Int(6))
        );
    }

    #[test]
    fn stdlib_reduce_no_init() {
        let mut env = Env::new();
        let d = RecordingDispatch::new();
        eval_str("(def add (fn [a b] (+ a b)))", &mut env, &d).unwrap();
        assert_eq!(
            eval_str("(reduce add (list 1 2 3))", &mut env, &d),
            Ok(Val::Int(6))
        );
    }

    #[test]
    fn stdlib_reduce_empty_no_init_errors() {
        let mut env = Env::new();
        let d = RecordingDispatch::new();
        eval_str("(def f (fn [a b] a))", &mut env, &d).unwrap();
        assert!(eval_str("(reduce f (list))", &mut env, &d).is_err());
    }

    #[test]
    fn stdlib_reduce_empty_with_init() {
        let mut env = Env::new();
        let d = RecordingDispatch::new();
        eval_str("(def add (fn [a b] (+ a b)))", &mut env, &d).unwrap();
        assert_eq!(
            eval_str("(reduce add 100 (list))", &mut env, &d),
            Ok(Val::Int(100))
        );
    }

    // =========================================================================
    // Effect system tests (#205)
    // =========================================================================

    /// Helper: load prelude then eval — needed for try/throw macros
    fn effects_eval(input: &str) -> Result<Val, EvalError> {
        let mut env = Env::new();
        let d = RecordingDispatch::new();
        let prelude_forms = crate::read_many(crate::PRELUDE)
            .map_err(|e| boundary_thrown(error::parse(Some("prelude.glia"), e.to_string())))?;
        for form in &prelude_forms {
            eval_blocking(form, &mut env, &d)?;
        }
        eval_str(input, &mut env, &d)
    }

    // --- perform / with-handler primitives ---

    #[test]
    fn perform_without_handler_propagates() {
        let mut env = Env::new();
        let d = RecordingDispatch::new();
        let result = eval_str("(perform :fail 42)", &mut env, &d);
        assert!(result.is_err());
    }

    #[test]
    fn with_handler_catches_effect() {
        let mut env = Env::new();
        let d = RecordingDispatch::new();
        let result = eval_str(
            "(with-effect-handler :fail (fn [error] (+ error 1)) (perform :fail 42))",
            &mut env,
            &d,
        );
        assert_eq!(result, Ok(Val::Int(43)));
    }

    #[test]
    fn with_handler_passes_through_on_no_effect() {
        let mut env = Env::new();
        let d = RecordingDispatch::new();
        assert_eq!(
            eval_str(
                "(with-effect-handler :fail (fn [error] 0) (+ 1 2))",
                &mut env,
                &d
            ),
            Ok(Val::Int(3))
        );
    }

    #[test]
    fn with_handler_unmatched_effect_propagates() {
        let mut env = Env::new();
        let d = RecordingDispatch::new();
        let result = eval_str(
            "(with-effect-handler :other (fn [error] 0) (perform :fail 42))",
            &mut env,
            &d,
        );
        assert!(result.is_err());
    }

    #[test]
    fn nested_handlers() {
        let mut env = Env::new();
        let d = RecordingDispatch::new();
        // Inner handler catches :fail, outer catches :other
        assert_eq!(
            eval_str(
                "(with-effect-handler :other (fn [error] 99) (with-effect-handler :fail (fn [error] (+ error 10)) (perform :fail 5)))",
                &mut env,
                &d
            ),
            Ok(Val::Int(15))
        );
    }

    // --- try / throw macros (prelude) ---

    #[test]
    fn throw_basic() {
        let result = effects_eval("(throw 42)");
        assert!(result.is_err());
    }

    #[test]
    fn try_ok() {
        // No throw → try returns the body's value directly.
        assert_eq!(effects_eval("(try (+ 1 2))"), Ok(Val::Int(3)));
    }

    #[test]
    fn try_err() {
        // Wildcard catch binds the thrown value verbatim.
        let result = effects_eval(r#"(try (throw {:type :test}) (catch _ e e))"#).unwrap();
        // Plain-map throw isn't catchable by tag (no :glia.error/type),
        // but wildcard sees the map verbatim.
        if let Val::Map(m) = &result {
            assert_eq!(
                m.get(&Val::Keyword("type".into())),
                Some(&Val::Keyword("test".into()))
            );
        } else {
            panic!("expected map, got {result:?}");
        }
    }

    #[test]
    fn try_catch_string() {
        // Strings flow through wildcard catch verbatim.
        let result = effects_eval(r#"(try (throw "just a string") (catch _ e e))"#).unwrap();
        assert_eq!(result, Val::Str("just a string".into()));
    }

    #[test]
    fn nested_try() {
        // Inner catch handles, outer never sees the throw.
        let result =
            effects_eval("(try (try (throw 1) (catch _ e e)) (catch _ e (+ e 100)))").unwrap();
        assert_eq!(result, Val::Int(1));
    }

    // --- ex-info ---

    #[test]
    fn ex_info_basic() {
        let mut env = Env::new();
        let d = RecordingDispatch::new();
        let result = eval_str(r#"(ex-info "bad input" {:type :invalid})"#, &mut env, &d).unwrap();
        if let Val::Map(m) = &result {
            assert_eq!(
                m.get(&Val::Keyword("message".into())),
                Some(&Val::Str("bad input".into()))
            );
            assert_eq!(
                m.get(&Val::Keyword("type".into())),
                Some(&Val::Keyword("invalid".into()))
            );
        } else {
            panic!("expected map, got {result:?}");
        }
    }

    #[test]
    fn ex_info_wrong_args() {
        let mut env = Env::new();
        let d = RecordingDispatch::new();
        assert!(eval_str("(ex-info)", &mut env, &d).is_err());
    }

    // --- or-else ---

    #[test]
    fn or_else_ok() {
        assert_eq!(effects_eval("(or-else (+ 1 2) 0)"), Ok(Val::Int(3)));
    }

    #[test]
    fn or_else_err() {
        assert_eq!(effects_eval("(or-else (throw 42) 0)"), Ok(Val::Int(0)));
    }

    // --- guard ---

    #[test]
    fn guard_pass() {
        assert_eq!(effects_eval("(guard true {:type :fail})"), Ok(Val::Nil));
    }

    #[test]
    fn guard_fail() {
        // ex-info now stamps :glia.error/type from :type, so a guard
        // failure is catchable by the user's tag.
        let result = effects_eval(
            r#"(try (guard false (ex-info "nope" {:type :fail}))
                    (catch :fail e (get e :glia.error/message)))"#,
        )
        .unwrap();
        assert_eq!(result, Val::Str("nope".into()));
    }

    // --- existing error format ---

    #[test]
    fn internal_error_is_structured() {
        let mut env = Env::new();
        let d = RecordingDispatch::new();
        // Division by zero should produce a structured error
        let err = eval_str("(/ 1 0)", &mut env, &d).unwrap_err();
        assert!(err_contains(err_payload(&err), "division by zero"));
    }

    // --- effect edge cases ---

    #[test]
    fn perform_non_keyword_type() {
        let mut env = Env::new();
        let d = RecordingDispatch::new();
        let result = eval_str(r#"(perform 42 "data")"#, &mut env, &d);
        assert!(result.is_err());
        assert!(err_contains(err_payload(&result.unwrap_err()), "keyword"));
    }

    #[test]
    fn perform_nil_data() {
        let mut env = Env::new();
        let d = RecordingDispatch::new();
        let result = eval_str(
            "(with-effect-handler :test (fn [data] data) (perform :test nil))",
            &mut env,
            &d,
        );
        assert_eq!(result, Ok(Val::Nil));
    }

    #[test]
    fn perform_in_loop() {
        let mut env = Env::new();
        let d = RecordingDispatch::new();
        let result = eval_str(
            "(with-effect-handler :done (fn [data] data) (loop [i 0] (if (= i 3) (perform :done i) (recur (+ i 1)))))",
            &mut env,
            &d,
        );
        assert_eq!(result, Ok(Val::Int(3)));
    }

    #[test]
    fn handler_missing_key() {
        // with-effect-handler requires a keyword or cap target
        let mut env = Env::new();
        let d = RecordingDispatch::new();
        let result = eval_str("(with-effect-handler (perform :test 42))", &mut env, &d);
        assert!(result.is_err());
    }

    #[test]
    fn handler_not_function() {
        let mut env = Env::new();
        let d = RecordingDispatch::new();
        let result = eval_str(
            r#"(with-effect-handler :test 42 (perform :test "data"))"#,
            &mut env,
            &d,
        );
        assert!(result.is_err());
        assert!(err_contains(err_payload(&result.unwrap_err()), "function"));
    }

    #[test]
    fn handler_multi_body() {
        let mut env = Env::new();
        let d = RecordingDispatch::new();
        let result = eval_str(
            "(with-effect-handler :test (fn [data] data) (def x 1) (perform :test (+ x 1)))",
            &mut env,
            &d,
        );
        assert_eq!(result, Ok(Val::Int(2)));
    }

    #[test]
    fn handler_throws_effect() {
        let mut env = Env::new();
        let d = RecordingDispatch::new();
        let result = eval_str(
            "(with-effect-handler :outer (fn [error] (+ error 100)) (with-effect-handler :fail (fn [error] (perform :outer error)) (perform :fail 5)))",
            &mut env,
            &d,
        );
        assert_eq!(result, Ok(Val::Int(105)));
    }

    #[test]
    fn ex_info_non_string_msg() {
        let mut env = Env::new();
        let d = RecordingDispatch::new();
        let result = eval_str("(ex-info 42 {})", &mut env, &d);
        assert!(result.is_err());
    }

    #[test]
    fn ex_info_non_map_data() {
        let mut env = Env::new();
        let d = RecordingDispatch::new();
        let result = eval_str(r#"(ex-info "msg" [1 2])"#, &mut env, &d);
        assert!(result.is_err());
    }

    // --- prelude macro edge cases ---

    #[test]
    fn try_multiple_body() {
        // Multi-form bodies must wrap in `do` under the new shape
        // (try takes one EXPR + zero or more catch clauses).
        assert_eq!(effects_eval("(try (do 1 2 3))"), Ok(Val::Int(3)));
    }

    #[test]
    fn throw_nil() {
        let result = effects_eval("(try (throw nil) (catch _ e e))").unwrap();
        assert_eq!(result, Val::Nil);
    }

    #[test]
    fn throw_int() {
        let result = effects_eval("(try (throw 42) (catch _ e e))").unwrap();
        assert_eq!(result, Val::Int(42));
    }

    #[test]
    fn throw_vector() {
        let result = effects_eval("(try (throw [1 2 3]) (catch _ e e))").unwrap();
        assert_eq!(
            result,
            Val::Vector(vec![Val::Int(1), Val::Int(2), Val::Int(3)])
        );
    }

    #[test]
    fn guard_truthy_int() {
        assert_eq!(effects_eval("(guard 42 {:type :fail})"), Ok(Val::Nil));
    }

    #[test]
    fn guard_truthy_string() {
        assert_eq!(effects_eval(r#"(guard "hi" {:type :fail})"#), Ok(Val::Nil));
    }

    #[test]
    fn or_else_nested() {
        assert_eq!(
            effects_eval("(or-else (or-else (throw 1) (throw 2)) 3)"),
            Ok(Val::Int(3))
        );
    }

    #[test]
    fn try_deeply_nested() {
        // Each layer catches via wildcard; thrown value bubbles up
        // through the catches, still equal to 1.
        let result =
            effects_eval("(try (try (try (throw 1) (catch _ e e)) (catch _ e e)) (catch _ e e))")
                .unwrap();
        assert_eq!(result, Val::Int(1));
    }

    #[test]
    fn guard_with_ex_info() {
        // The thrown ex-info has both :glia.error/message (canonical)
        // and :message (back-compat) populated.
        let err =
            effects_eval(r#"(try (guard false (ex-info "nope" {:type :fail})) (catch _ e e))"#)
                .unwrap();
        if let Val::Map(m) = &err {
            assert_eq!(
                m.get(&Val::Keyword("glia.error/message".into())),
                Some(&Val::Str("nope".into()))
            );
            assert_eq!(
                m.get(&Val::Keyword("message".into())),
                Some(&Val::Str("nope".into()))
            );
            assert_eq!(
                m.get(&Val::Keyword("glia.error/type".into())),
                Some(&Val::Keyword("fail".into()))
            );
        } else {
            panic!("expected map, got {err:?}");
        }
    }

    // =========================================================================
    // Resume / continuation tests (#247)
    // =========================================================================

    #[test]
    fn resume_basic() {
        // Handler resumes with 42, body continues: (+ 10 42) = 52
        let mut env = Env::new();
        let d = RecordingDispatch::new();
        let result = eval_str(
            "(with-effect-handler :foo (fn [data resume] (resume 42)) (+ 10 (perform :foo 0)))",
            &mut env,
            &d,
        );
        assert_eq!(result, Ok(Val::Int(52)));
    }

    #[test]
    fn resume_with_data() {
        // Handler receives data and resumes with data + 1
        let mut env = Env::new();
        let d = RecordingDispatch::new();
        let result = eval_str(
            "(with-effect-handler :inc (fn [data resume] (resume (+ data 1))) (perform :inc 41))",
            &mut env,
            &d,
        );
        assert_eq!(result, Ok(Val::Int(42)));
    }

    #[test]
    fn abort_1arg_handler() {
        // 1-arg handler = abort semantics (backward compat)
        let mut env = Env::new();
        let d = RecordingDispatch::new();
        let result = eval_str(
            "(with-effect-handler :foo (fn [data] 99) (+ 10 (perform :foo 0)))",
            &mut env,
            &d,
        );
        assert_eq!(result, Ok(Val::Int(99)));
    }

    #[test]
    fn abort_2arg_handler_no_resume() {
        // 2-arg handler that doesn't call resume = abort
        let mut env = Env::new();
        let d = RecordingDispatch::new();
        let result = eval_str(
            "(with-effect-handler :foo (fn [data resume] 99) (+ 10 (perform :foo 0)))",
            &mut env,
            &d,
        );
        assert_eq!(result, Ok(Val::Int(99)));
    }

    #[test]
    fn resume_oneshot_violation() {
        // Calling resume twice should error
        let mut env = Env::new();
        let d = RecordingDispatch::new();
        let result = eval_str(
            "(with-effect-handler :foo (fn [data resume] (resume 1) (resume 2)) (perform :foo 0))",
            &mut env,
            &d,
        );
        // The second resume should error (one-shot violated)
        // But the first resume short-circuits via the resume signal, so (resume 2) is never
        // reached: the signal propagates up and the handler resumes the body with 1.
        // with-handler catches Resume and resumes the body. Body returns 1. Result: Ok(1).
        assert_eq!(result, Ok(Val::Int(1)));
    }

    #[test]
    fn resume_nested_handlers() {
        // Inner handler resumes, outer handler not triggered
        let mut env = Env::new();
        let d = RecordingDispatch::new();
        let result = eval_str(
            "(with-effect-handler :outer (fn [data] 0) (with-effect-handler :inner (fn [data resume] (resume 42)) (+ 10 (perform :inner 0))))",
            &mut env,
            &d,
        );
        assert_eq!(result, Ok(Val::Int(52)));
    }

    #[test]
    fn resume_different_value_types() {
        let mut env = Env::new();
        let d = RecordingDispatch::new();
        // Resume with nil
        assert_eq!(
            eval_str(
                "(with-effect-handler :foo (fn [data resume] (resume nil)) (perform :foo 0))",
                &mut env,
                &d,
            ),
            Ok(Val::Nil)
        );
        // Resume with string
        let result = eval_str(
            r#"(with-effect-handler :foo (fn [data resume] (resume "hello")) (perform :foo 0))"#,
            &mut env,
            &d,
        );
        assert_eq!(result, Ok(Val::Str("hello".into())));
    }

    #[test]
    fn resume_try_throw_interaction() {
        // try/throw still compose with the resume state machine —
        // wildcard catch sees the thrown value verbatim.
        assert_eq!(
            effects_eval("(try (throw 42) (catch _ e e))"),
            Ok(Val::Int(42))
        );
    }

    #[test]
    fn resume_in_loop() {
        // perform inside a loop body, handler resumes, loop continues
        let mut env = Env::new();
        let d = RecordingDispatch::new();
        let result = eval_str(
            "(with-effect-handler :step (fn [data resume] (resume (+ data 1))) (loop [i 0] (if (= i 3) i (recur (perform :step i)))))",
            &mut env,
            &d,
        );
        assert_eq!(result, Ok(Val::Int(3)));
    }

    #[test]
    fn resume_multiple_sequential_performs() {
        // Body performs twice — each gets its own resume
        let mut env = Env::new();
        let d = RecordingDispatch::new();
        let result = eval_str(
            "(with-effect-handler :inc (fn [data resume] (resume (+ data 10))) (+ (perform :inc 1) (perform :inc 2)))",
            &mut env,
            &d,
        );
        // (perform :inc 1) → resume(11), (perform :inc 2) → resume(12), total = 23
        assert_eq!(result, Ok(Val::Int(23)));
    }

    #[test]
    fn perform_without_handler_still_errors() {
        // No handler context → unhandled effect
        let mut env = Env::new();
        let d = RecordingDispatch::new();
        let result = eval_str("(perform :foo 42)", &mut env, &d);
        assert!(result.is_err());
    }

    #[test]
    fn resume_unmatched_effect_propagates() {
        // Handler for :bar doesn't match :foo
        let mut env = Env::new();
        let d = RecordingDispatch::new();
        let result = eval_str(
            "(with-effect-handler :bar (fn [data resume] (resume 0)) (perform :foo 42))",
            &mut env,
            &d,
        );
        assert!(result.is_err());
    }

    #[test]
    fn resume_body_no_effect_passes_through() {
        // Body doesn't perform — result passes through
        let mut env = Env::new();
        let d = RecordingDispatch::new();
        let result = eval_str(
            "(with-effect-handler :foo (fn [data resume] (resume 0)) (+ 1 2))",
            &mut env,
            &d,
        );
        assert_eq!(result, Ok(Val::Int(3)));
    }

    #[test]
    fn resume_handler_map_eval_before_push() {
        // Handler closures don't see the current handler context
        // (they're evaluated before the context is pushed)
        let mut env = Env::new();
        let d = RecordingDispatch::new();
        // This test just verifies the handler works — the ordering guarantee
        // is architectural (tested in the spike).
        let result = eval_str(
            "(with-effect-handler :foo (fn [data resume] (resume 100)) (perform :foo 0))",
            &mut env,
            &d,
        );
        assert_eq!(result, Ok(Val::Int(100)));
    }

    // =========================================================================
    // G1 — resumable-effects semantics lock-in
    //
    // These tests pin down the observable guarantees the resumable-effects
    // model depends on. They assert existing behavior; they must not require
    // any runtime change.
    // =========================================================================

    #[test]
    fn abort_without_resume_skips_body_after_perform() {
        // A handler that returns WITHOUT calling `resume` aborts the suspended
        // body: the code *after* the `perform` never runs. We make that
        // observable with a second, distinct effect that would only fire if
        // execution continued past the first `perform`.
        let mut env = Env::new();
        let d = RecordingDispatch::new();
        let result = eval_str(
            "(with-effect-handler :probe (fn [d] :probe-ran)
               (with-effect-handler :abort (fn [d] :aborted)
                 (do (perform :abort 0) (perform :probe 0))))",
            &mut env,
            &d,
        );
        // If the body were resumed, (perform :probe 0) would run and the result
        // would be :probe-ran. Because the :abort handler never resumes, the
        // do-block is discarded and we get the handler's value instead.
        assert_eq!(result, Ok(Val::Keyword("aborted".into())));
    }

    #[test]
    fn resume_continues_at_exact_perform_site_in_nested_expr() {
        // `resume` returns control to the precise position of the `perform`
        // inside a larger expression: the surrounding arithmetic sees the
        // resumed value in place. (+ 1 (* 10 (perform :x 0))) with resume 5
        // must evaluate as (+ 1 (* 10 5)) = 51.
        let mut env = Env::new();
        let d = RecordingDispatch::new();
        let result = eval_str(
            "(with-effect-handler :x (fn [d resume] (resume 5)) (+ 1 (* 10 (perform :x 0))))",
            &mut env,
            &d,
        );
        assert_eq!(result, Ok(Val::Int(51)));
    }

    #[test]
    fn handler_reperform_same_effect_forwards_to_next_outer_handler() {
        // Handler forwarding: a handler frame is popped before it runs, so a
        // handler that re-performs the *same* effect reaches the NEXT outer
        // handler rather than recursing into itself (which would loop forever).
        let mut env = Env::new();
        let d = RecordingDispatch::new();
        let result = eval_str(
            "(with-effect-handler :log (fn [d] :outer)
               (with-effect-handler :log (fn [d] (perform :log d))
                 (perform :log 0)))",
            &mut env,
            &d,
        );
        // Inner handler catches :log, re-performs :log; because its own frame
        // is already popped, the re-perform lands on the outer handler → :outer.
        assert_eq!(result, Ok(Val::Keyword("outer".into())));
    }

    #[test]
    fn async_native_handler_resumes_body() {
        // An async native handler that calls the provided `resume` continuation
        // must resume the suspended body with the sent value, exactly like a
        // synchronous handler. (+ 10 (perform :inc 41)) with resume(41+1) = 52.
        let mut env = Env::new();
        let d = RecordingDispatch::new();
        env.set(
            "async-resume".into(),
            Val::AsyncNativeFn {
                name: "async-resume".into(),
                func: Rc::new(|args: Vec<Val>| {
                    let data = args[0].clone();
                    let resume = args[1].clone();
                    Box::pin(async move {
                        if let Val::NativeFn { func, .. } = &resume {
                            let next = match data {
                                Val::Int(n) => Val::Int(n + 1),
                                other => other,
                            };
                            // Returns the resume signal; the handler state
                            // machine translates that into a body resume.
                            func(&[next])
                        } else {
                            Err(NativeSignal::throw("async-resume: bad resume"))
                        }
                    })
                }),
            },
        );
        let result = eval_str(
            "(with-effect-handler :inc async-resume (+ 10 (perform :inc 41)))",
            &mut env,
            &d,
        );
        assert_eq!(result, Ok(Val::Int(52)));
    }

    #[test]
    fn native_fn_display() {
        let nf = Val::NativeFn {
            name: "test".into(),
            func: Rc::new(|_: &[Val]| Ok(Val::Nil)),
        };
        assert_eq!(format!("{nf}"), "#<native-fn test>");
    }

    #[test]
    fn native_fn_equality() {
        let func: crate::NativeFnImpl = Rc::new(|_: &[Val]| Ok(Val::Nil));
        let a = Val::NativeFn {
            name: "test".into(),
            func: func.clone(),
        };
        let b = Val::NativeFn {
            name: "test".into(),
            func: func.clone(),
        };
        assert_eq!(a, b); // same Rc → equal
        let c = Val::NativeFn {
            name: "test".into(),
            func: Rc::new(|_: &[Val]| Ok(Val::Nil)),
        };
        assert_ne!(a, c); // different Rc → not equal
    }

    #[test]
    fn try_resume_macro() {
        // try-resume catches error and resumes
        let result = effects_eval("(try-resume (fn [err resume] (resume 42)) (throw :oops))");
        assert_eq!(result, Ok(Val::Int(42)));
    }

    #[test]
    fn try_resume_macro_abort() {
        // try-resume with recovery fn that doesn't resume → abort
        let result = effects_eval("(try-resume (fn [err resume] 99) (throw :oops))");
        assert_eq!(result, Ok(Val::Int(99)));
    }

    #[test]
    fn try_resume_macro_no_error() {
        // try-resume with no error — body result passes through
        let result = effects_eval("(try-resume (fn [error resume] 0) (+ 1 2))");
        assert_eq!(result, Ok(Val::Int(3)));
    }

    // =========================================================================
    // try / catch — multi-clause dispatch + re-throw semantics
    // =========================================================================

    #[test]
    fn catch_multiple_clauses_first_match_wins() {
        // Three catches; only :foo matches the thrown :foo error.
        let result = effects_eval(
            r#"(try (throw (ex-info "boom" {:type :foo}))
                 (catch :bar e :took-bar)
                 (catch :foo e :took-foo)
                 (catch _    e :took-wild))"#,
        )
        .unwrap();
        assert_eq!(result, Val::Keyword("took-foo".into()));
    }

    #[test]
    fn catch_non_matching_falls_through_to_wildcard() {
        let result = effects_eval(
            r#"(try (throw (ex-info "boom" {:type :unknown}))
                 (catch :bar e :took-bar)
                 (catch _    e :took-wild))"#,
        )
        .unwrap();
        assert_eq!(result, Val::Keyword("took-wild".into()));
    }

    #[test]
    fn catch_non_matching_no_wildcard_rethrows_to_outer_try() {
        // Inner try has only :bar; the :foo throw propagates to outer try
        // which catches via wildcard.
        let result = effects_eval(
            r#"(try (try (throw (ex-info "boom" {:type :foo}))
                     (catch :bar e :inner-bar))
                 (catch _ e (get e :glia.error/type)))"#,
        )
        .unwrap();
        assert_eq!(result, Val::Keyword("foo".into()));
    }

    #[test]
    fn rethrow_inside_catch_body_propagates_to_outer_try() {
        // Inner catch matches and re-throws a different error; outer try
        // catches the re-throw. The inner handler must NOT loop on its own
        // re-throw (commitment 2 of the eng review: popped-handler-skip).
        let result = effects_eval(
            r#"(try (try (throw (ex-info "first" {:type :first}))
                     (catch :first e (throw (ex-info "second" {:type :second}))))
                 (catch :second e (get e :glia.error/type))
                 (catch _       e :wrong))"#,
        )
        .unwrap();
        assert_eq!(result, Val::Keyword("second".into()));
    }

    #[test]
    fn unhandled_throw_escapes_as_glia_exception_effect() {
        // No try in scope — throw escapes as an unhandled-exception carrier with
        // effect_type = "glia.exception". Outer callers (kernel, MCP,
        // shell) rely on this contract.
        let err = effects_eval("(throw (ex-info \"escape\" {:type :foo}))").unwrap_err();
        match &err {
            EvalError::Unhandled(req) => {
                assert_eq!(req.effect_type(), error::EXCEPTION_EFFECT);
                // The data is the inner structured error map.
                assert_eq!(error::type_tag(&req.data), Some("foo"));
            }
            other => panic!("expected unhandled exception, got {other:?}"),
        }
        // unwrap_thrown peels the carrier for outer callers.
        let inner = error::unwrap_thrown(&err).expect("should peel");
        assert_eq!(error::type_tag(inner), Some("foo"));
    }

    #[test]
    fn rethrow_with_no_outer_try_escapes_as_effect() {
        // Single try, only matches :a. Throwing :b means the dispatcher
        // re-throws; with no outer try, the re-throw escapes as
        // an escaped effect — same contract as a direct unhandled throw.
        let err = effects_eval(
            r#"(try (throw (ex-info "x" {:type :b}))
                 (catch :a e :ignored))"#,
        )
        .unwrap_err();
        let inner = error::unwrap_thrown(&err).expect("should peel");
        assert_eq!(error::type_tag(inner), Some("b"));
    }

    // =========================================================================
    // match — pattern matching tests
    // =========================================================================

    #[test]
    fn match_literal_first_clause() {
        let mut env = Env::new();
        let d = RecordingDispatch::new();
        let result = eval_str("(match 42 42 :yes _ :no)", &mut env, &d);
        assert_eq!(result, Ok(Val::Keyword("yes".into())));
    }

    #[test]
    fn match_literal_second_clause() {
        let mut env = Env::new();
        let d = RecordingDispatch::new();
        let result = eval_str("(match 99 42 :a 99 :b _ :c)", &mut env, &d);
        assert_eq!(result, Ok(Val::Keyword("b".into())));
    }

    #[test]
    fn match_wildcard_default() {
        let mut env = Env::new();
        let d = RecordingDispatch::new();
        let result = eval_str("(match 7 42 :no _ :yes)", &mut env, &d);
        assert_eq!(result, Ok(Val::Keyword("yes".into())));
    }

    #[test]
    fn match_no_clause_errors() {
        let mut env = Env::new();
        let d = RecordingDispatch::new();
        let result = eval_str("(match 7 42 :no)", &mut env, &d);
        assert!(result.is_err());
        assert!(err_contains(
            err_payload(&result.unwrap_err()),
            "no clause matched"
        ));
    }

    #[test]
    fn match_bind_visible_in_body() {
        let mut env = Env::new();
        let d = RecordingDispatch::new();
        let result = eval_str("(match 42 x (+ x 1))", &mut env, &d);
        assert_eq!(result, Ok(Val::Int(43)));
    }

    #[test]
    fn match_nil_literal() {
        let mut env = Env::new();
        let d = RecordingDispatch::new();
        let result = eval_str("(match nil nil :yes _ :no)", &mut env, &d);
        assert_eq!(result, Ok(Val::Keyword("yes".into())));
    }

    #[test]
    fn match_keyword_literal() {
        let mut env = Env::new();
        let d = RecordingDispatch::new();
        let result = eval_str("(match :ok :ok :yes _ :no)", &mut env, &d);
        assert_eq!(result, Ok(Val::Keyword("yes".into())));
    }

    #[test]
    fn match_vector_pattern() {
        let mut env = Env::new();
        let d = RecordingDispatch::new();
        let result = eval_str("(match [1 2] [a b] (+ a b) _ 0)", &mut env, &d);
        assert_eq!(result, Ok(Val::Int(3)));
    }

    #[test]
    fn match_vector_wrong_length() {
        let mut env = Env::new();
        let d = RecordingDispatch::new();
        let result = eval_str("(match [1 2 3] [a b] :two _ :other)", &mut env, &d);
        assert_eq!(result, Ok(Val::Keyword("other".into())));
    }

    #[test]
    fn match_map_pattern() {
        let mut env = Env::new();
        let d = RecordingDispatch::new();
        let result = eval_str(
            r#"(match {:name "Alice" :age 30} {:name name} name _ "unknown")"#,
            &mut env,
            &d,
        );
        assert_eq!(result, Ok(Val::Str("Alice".into())));
    }

    #[test]
    fn match_evaluated_scrutinee() {
        let mut env = Env::new();
        let d = RecordingDispatch::new();
        let result = eval_str("(match (+ 1 2) 3 :yes _ :no)", &mut env, &d);
        assert_eq!(result, Ok(Val::Keyword("yes".into())));
    }

    #[test]
    fn match_with_effect_normal_return() {
        let mut env = Env::new();
        let d = RecordingDispatch::new();
        let result = eval_str(
            "(match (+ 1 2) result result (effect :fail error) :caught)",
            &mut env,
            &d,
        );
        assert_eq!(result, Ok(Val::Int(3)));
    }

    #[test]
    fn match_with_effect_abort() {
        let mut env = Env::new();
        let d = RecordingDispatch::new();
        let result = eval_str(
            "(match (perform :fail 42) result result (effect :fail error) (+ error 1))",
            &mut env,
            &d,
        );
        assert_eq!(result, Ok(Val::Int(43)));
    }

    #[test]
    fn match_with_effect_resume() {
        let mut env = Env::new();
        let d = RecordingDispatch::new();
        let result = eval_str(
            "(match (+ 10 (perform :inc 5)) result result (effect :inc data resume) (resume (+ data 10)))",
            &mut env,
            &d,
        );
        // perform :inc 5 → handler resumes with 15 → body evaluates (+ 10 15) = 25
        assert_eq!(result, Ok(Val::Int(25)));
    }

    #[test]
    fn match_effect_unmatched_propagates() {
        let mut env = Env::new();
        let d = RecordingDispatch::new();
        let result = eval_str(
            "(match (perform :foo 42) result result (effect :bar data) data)",
            &mut env,
            &d,
        );
        // :foo doesn't match :bar, propagates out — no outer handler → error
        assert!(result.is_err());
    }

    #[test]
    fn match_nested_pattern() {
        let mut env = Env::new();
        let d = RecordingDispatch::new();
        let result = eval_str("(match [1 [2 3]] [a [b c]] (+ a b c) _ 0)", &mut env, &d);
        assert_eq!(result, Ok(Val::Int(6)));
    }

    #[test]
    fn match_odd_clauses_errors() {
        let mut env = Env::new();
        let d = RecordingDispatch::new();
        let result = eval_str("(match 42 :a)", &mut env, &d);
        assert!(result.is_err());
    }

    // =========================================================================
    // Destructuring tests
    // =========================================================================

    #[test]
    fn let_vector_destructure() {
        let mut env = Env::new();
        let d = RecordingDispatch::new();
        let result = eval_str("(let [[a b] [1 2]] (+ a b))", &mut env, &d);
        assert_eq!(result, Ok(Val::Int(3)));
    }

    #[test]
    fn let_map_destructure() {
        let mut env = Env::new();
        let d = RecordingDispatch::new();
        let result = eval_str(r#"(let [{:name name} {:name "Alice"}] name)"#, &mut env, &d);
        assert_eq!(result, Ok(Val::Str("Alice".into())));
    }

    #[test]
    fn let_destructure_mismatch_errors() {
        let mut env = Env::new();
        let d = RecordingDispatch::new();
        let result = eval_str("(let [[a b] 42] (+ a b))", &mut env, &d);
        assert!(result.is_err());
        assert!(err_contains(
            err_payload(&result.unwrap_err()),
            "destructuring failed"
        ));
    }

    #[test]
    fn let_nested_destructure() {
        let mut env = Env::new();
        let d = RecordingDispatch::new();
        let result = eval_str("(let [[a [b c]] [1 [2 3]]] (+ a b c))", &mut env, &d);
        assert_eq!(result, Ok(Val::Int(6)));
    }

    #[test]
    fn let_vector_rest() {
        let mut env = Env::new();
        let d = RecordingDispatch::new();
        let result = eval_str("(let [[a & rest] [1 2 3]] rest)", &mut env, &d);
        assert_eq!(result, Ok(Val::List(vec![Val::Int(2), Val::Int(3)])));
    }

    #[test]
    fn let_simple_still_works() {
        // Ensure simple let bindings are unaffected
        let mut env = Env::new();
        let d = RecordingDispatch::new();
        let result = eval_str("(let [x 1 y 2] (+ x y))", &mut env, &d);
        assert_eq!(result, Ok(Val::Int(3)));
    }

    #[test]
    fn loop_destructure_basic() {
        let mut env = Env::new();
        let d = RecordingDispatch::new();
        // Destructure in loop, recur re-matches
        let result = eval_str(
            "(loop [[a b] [0 0]] (if (= a 3) b (recur [(+ a 1) (+ b 10)])))",
            &mut env,
            &d,
        );
        assert_eq!(result, Ok(Val::Int(30)));
    }

    #[test]
    fn loop_destructure_recur_mismatch() {
        let mut env = Env::new();
        let d = RecordingDispatch::new();
        // recur with non-vector when loop expects [a b]
        let result = eval_str("(loop [[a b] [0 0]] (recur 42))", &mut env, &d);
        assert!(result.is_err());
    }

    #[test]
    fn loop_simple_still_works() {
        let mut env = Env::new();
        let d = RecordingDispatch::new();
        let result = eval_str("(loop [i 0] (if (= i 5) i (recur (+ i 1))))", &mut env, &d);
        assert_eq!(result, Ok(Val::Int(5)));
    }

    // -----------------------------------------------------------------------
    // Cap-targeted effect handler tests
    // -----------------------------------------------------------------------

    /// Helper: create a capability with a unique instance identity.
    fn make_test_cap(name: &str, marker: i32) -> Val {
        make_cap(name, format!("test-cid-{name}"), Rc::new(marker))
    }

    #[test]
    fn perform_cap_basic() {
        // Cap-targeted perform dispatches to the correct handler.
        let mut env = Env::new();
        let d = RecordingDispatch::new();
        let cap = make_test_cap("executor", 1);
        env.set("my-cap".into(), cap);
        // Handler receives data (a list of [:method args...]) and returns it.
        let result = eval_str(
            "(with-effect-handler my-cap (fn [data resume] (resume data)) (perform my-cap :run 42))",
            &mut env,
            &d,
        );
        assert_eq!(
            result,
            Ok(Val::List(vec![Val::Keyword("run".into()), Val::Int(42)]))
        );
    }

    #[test]
    fn perform_cap_different_cid_no_match() {
        // Different test caps have different instance identities, so they do not match.
        let mut env = Env::new();
        let d = RecordingDispatch::new();
        let cap1 = make_test_cap("executor", 1);
        let cap2 = make_test_cap("ipfs", 2);
        env.set("cap1".into(), cap1);
        env.set("cap2".into(), cap2);
        // Handler installed for cap1 (executor CID), perform on cap2 (ipfs CID) — no match.
        let result = eval_str(
            "(with-effect-handler cap1 (fn [data] :handled) (perform cap2 :run 0))",
            &mut env,
            &d,
        );
        assert!(result.is_err());
    }

    #[test]
    fn perform_cap_same_cid_different_id_no_match() {
        // Same schema CID, different cap instances — does NOT match.
        let mut env = Env::new();
        let d = RecordingDispatch::new();
        let cap1 = make_test_cap("executor", 1);
        let cap2 = make_test_cap("executor", 2);
        env.set("cap1".into(), cap1);
        env.set("cap2".into(), cap2);
        // Handler installed for cap1, perform on cap2 — no match due to cap_id mismatch.
        let result = eval_str(
            "(with-effect-handler cap1 (fn [data] :handled) (perform cap2 :run 0))",
            &mut env,
            &d,
        );
        assert!(result.is_err());
    }

    #[test]
    fn perform_cap_same_instance_matches() {
        let mut env = Env::new();
        let d = RecordingDispatch::new();
        let cap1 = make_test_cap("executor", 1);
        let cap2 = cap1.clone();
        env.set("cap1".into(), cap1);
        env.set("cap2".into(), cap2);
        let result = eval_str(
            "(with-effect-handler cap1 (fn [data] :handled) (perform cap2 :run 0))",
            &mut env,
            &d,
        );
        assert_eq!(result, Ok(Val::Keyword("handled".into())));
    }

    #[test]
    fn unhandled_cap_effect_fails_closed_with_structured_carrier() {
        // An unhandled capability-targeted effect must fail CLOSED and surface a
        // structured effect carrier (EvalError::Unhandled), never a plain string. This is
        // what lets outer callers pattern-match / unwrap the failure instead of
        // string-scraping.
        let mut env = Env::new();
        let d = RecordingDispatch::new();
        let cap = make_test_cap("executor", 1);
        env.set("my-cap".into(), cap);
        let result = eval_str("(perform my-cap :run 42)", &mut env, &d);
        match result {
            Err(EvalError::Unhandled(req)) => {
                assert_eq!(req.effect_type(), "cap:executor");
                // The carrier retains the effect payload as structured data.
                assert!(matches!(req.data, Val::List(_)));
            }
            other => panic!("expected structured unhandled-effect carrier, got {other:?}"),
        }
    }

    #[test]
    fn defcap_define_and_perform() {
        let mut env = Env::new();
        let d = RecordingDispatch::new();
        let result = eval_str(
            "(do (defcap directory :lookup (fn [name] name))
                 (perform directory :lookup \"service\"))",
            &mut env,
            &d,
        );
        assert_eq!(result, Ok(Val::Str("service".into())));
    }

    #[test]
    fn defcap_perform_hits_cap_handler_before_method_table() {
        let mut env = Env::new();
        let d = RecordingDispatch::new();
        let result = eval_str(
            "(do (defcap directory :lookup (fn [name] :backend))
                 (with-effect-handler directory
                   (fn [data] :handled)
                   (perform directory :lookup \"service\")))",
            &mut env,
            &d,
        );
        assert_eq!(result, Ok(Val::Keyword("handled".into())));
    }

    #[test]
    fn defcap_unknown_method_denied() {
        let mut env = Env::new();
        let d = RecordingDispatch::new();
        let result = eval_str(
            "(do (defcap directory :lookup (fn [name] name))
                 (perform directory :announce \"x\"))",
            &mut env,
            &d,
        );
        assert!(result.is_err());
        assert!(err_contains(
            err_payload(&result.unwrap_err()),
            "not available"
        ));
    }

    #[test]
    fn attenuate_allow_and_deny() {
        let mut env = Env::new();
        let d = RecordingDispatch::new();
        let cap = make_test_cap("svc", 1);
        env.set("svc".into(), cap.clone());
        let ok = eval_str(
            "(with-effect-handler svc (fn [data] :ok)
               (let [svc-ro (attenuate svc [:run])]
                 (perform svc-ro :run 1)))",
            &mut env,
            &d,
        );
        assert_eq!(ok, Ok(Val::Keyword("ok".into())));

        let denied = eval_str(
            "(with-effect-handler svc (fn [data] :ok)
               (let [svc-ro (attenuate svc [:run])]
                 (perform svc-ro :write 1)))",
            &mut env,
            &d,
        );
        assert!(denied.is_err());
        assert!(err_contains(err_payload(&denied.unwrap_err()), "denied"));
    }

    #[test]
    fn attenuate_nested_intersection() {
        let mut env = Env::new();
        let d = RecordingDispatch::new();
        let cap = make_test_cap("svc", 1);
        env.set("svc".into(), cap);
        let denied = eval_str(
            "(with-effect-handler svc (fn [data] :ok)
               (let [a1 (attenuate svc [:run])
                     a2 (attenuate a1 [:write])]
                 (perform a2 :run 1)))",
            &mut env,
            &d,
        );
        assert!(denied.is_err());
        assert!(err_contains(err_payload(&denied.unwrap_err()), "denied"));
    }

    /// A dispatcher that reifies every attenuation into a sentinel cap,
    /// standing in for the kernel's membrane reification.
    struct ReifyingDispatch;

    impl Dispatch for ReifyingDispatch {
        fn call<'a>(
            &'a self,
            _name: &'a str,
            _args: &'a [Val],
        ) -> Pin<Box<dyn Future<Output = Result<Val, NativeSignal>> + 'a>> {
            Box::pin(core::future::ready(Ok(Val::Nil)))
        }

        fn reify_attenuation(
            &self,
            cap: &Val,
            allow_methods: &BTreeSet<String>,
        ) -> Option<Result<Val, Val>> {
            let Val::Cap(h) = cap else {
                return Some(Err(Val::from("reify: not a cap")));
            };
            // Mark reification observable: rename + carry the allow set count.
            Some(Ok(make_cap(
                format!("{}-reified", h.name()),
                format!("methods-{}", allow_methods.len()),
                Rc::new(()),
            )))
        }
    }

    #[test]
    fn attenuate_offers_reification_to_dispatch_first() {
        let mut env = Env::new();
        let d = ReifyingDispatch;
        let cap = make_test_cap("svc", 1);
        env.set("svc".into(), cap);
        let expr = crate::read("(attenuate svc [:run :write])").unwrap();
        let result = pollster_eval(eval_toplevel(&expr, &mut env, &d)).unwrap();
        match result {
            Val::Cap(h) => {
                assert_eq!(h.name(), "svc-reified", "embedder reification must win");
                assert_eq!(
                    h.schema_cid(),
                    "methods-2",
                    "allow set must reach the embedder"
                );
            }
            other => panic!("expected reified cap, got {other:?}"),
        }
    }

    #[test]
    fn cell_preserves_reified_attenuation_in_explicit_grant() {
        let mut env = Env::new();
        let d = ReifyingDispatch;
        env.set("image".into(), Val::Bytes(vec![0, 97, 115, 109]));
        env.set("svc".into(), make_test_cap("svc", 1));
        let expr = crate::read(
            "(let [svc-ro (attenuate svc [:run])]
               (cell image :grants {:restricted svc-ro}))",
        )
        .unwrap();
        let result = pollster_eval(eval_toplevel(&expr, &mut env, &d)).unwrap();
        let caps = cell_caps(result);
        assert_eq!(caps.len(), 1);
        assert_eq!(caps[0].0, "restricted");
        match &caps[0].1 {
            Val::Cap(h) => {
                assert_eq!(h.name(), "svc-reified");
                assert_eq!(h.schema_cid(), "methods-1");
            }
            other => panic!("expected reified cap, got {other}"),
        }
    }

    #[test]
    fn attenuate_default_dispatch_falls_back_to_local_interposition() {
        // RecordingDispatch keeps the default None reify — the local
        // AttenuatedCapInner path must be taken (covered behaviorally by
        // attenuate_allow_and_deny; here we pin the inner representation).
        let mut env = Env::new();
        let d = RecordingDispatch::new();
        let cap = make_test_cap("svc", 1);
        env.set("svc".into(), cap);
        let result = eval_str("(attenuate svc [:run])", &mut env, &d).unwrap();
        match result {
            Val::Cap(h) => {
                assert!(
                    h.inner().downcast_ref::<AttenuatedCapInner>().is_some(),
                    "default dispatch must produce local AttenuatedCapInner"
                );
            }
            other => panic!("expected cap, got {other:?}"),
        }
    }

    fn make_handled_cap(name: &str) -> Val {
        // Handler follows the stack-handler protocol (payload resume): it
        // resumes with a keyword proving the carried handler ran.
        let handler = Val::NativeFn {
            name: "carried-handler".into(),
            func: Rc::new(|args: &[Val]| {
                assert!(
                    matches!(args.first(), Some(Val::List(_))),
                    "handler payload must be the (:method args...) list"
                );
                match args.get(1) {
                    Some(Val::NativeFn { func, .. }) => func(&[Val::Keyword("intrinsic".into())]),
                    other => Err(NativeSignal::throw(format!(
                        "expected resume fn, got {other:?}"
                    ))),
                }
            }),
        };
        make_cap(
            name,
            "test-cid",
            Rc::new(HandledCapInner {
                handler,
                export: Rc::new(()),
                descriptor: Vec::new(),
            }),
        )
    }

    #[test]
    fn handled_cap_perform_invokes_carried_handler() {
        let mut env = Env::new();
        let d = RecordingDispatch::new();
        env.set("hc".into(), make_handled_cap("hc"));
        let result = eval_str("(perform hc :anything 1 2)", &mut env, &d);
        assert_eq!(result, Ok(Val::Keyword("intrinsic".into())));
    }

    #[test]
    fn handled_cap_stack_handler_interposes_first() {
        // Dynamic-scope interposition keeps priority over the cap's own
        // handler: with-effect-handler on the cap instance wins.
        let mut env = Env::new();
        let d = RecordingDispatch::new();
        env.set("hc".into(), make_handled_cap("hc"));
        let result = eval_str(
            "(with-effect-handler hc (fn [data resume] (resume :interposed))
               (perform hc :anything))",
            &mut env,
            &d,
        );
        assert_eq!(result, Ok(Val::Keyword("interposed".into())));
    }

    #[test]
    fn fn_invocation_uses_caller_handler_stack_not_definition_stack() {
        let mut env = Env::new();
        let d = RecordingDispatch::new();
        let result = eval_str(
            "(do
               (def f
                 (with-effect-handler :log (fn [msg] :inner)
                   (fn [msg] (perform :log msg))))
               (f \"x\"))",
            &mut env,
            &d,
        );
        assert!(matches!(
            &result,
            Err(EvalError::Unhandled(req)) if req.effect_type() == "log"
        ));
    }

    #[test]
    fn macro_invocation_uses_caller_handler_stack_not_definition_stack() {
        let mut env = Env::new();
        let d = RecordingDispatch::new();
        let result = eval_str(
            "(do
               (with-effect-handler :log (fn [msg] :inner)
                 (defmacro m [x] (list (quote perform) :log x)))
               (m \"x\"))",
            &mut env,
            &d,
        );
        assert!(matches!(
            &result,
            Err(EvalError::Unhandled(req)) if req.effect_type() == "log"
        ));
    }

    #[test]
    fn perform_cap_no_handler() {
        // No handler installed for cap → unhandled effect error.
        let mut env = Env::new();
        let d = RecordingDispatch::new();
        let cap = make_test_cap("ipfs", 1);
        env.set("my-cap".into(), cap);
        let result = eval_str("(perform my-cap :cat \"/foo\")", &mut env, &d);
        assert!(result.is_err());
    }

    #[test]
    fn perform_keyword_still_works() {
        // Existing keyword performs are unchanged.
        let mut env = Env::new();
        let d = RecordingDispatch::new();
        let result = eval_str(
            "(with-effect-handler :fail (fn [data] (+ data 1)) (perform :fail 42))",
            &mut env,
            &d,
        );
        assert_eq!(result, Ok(Val::Int(43)));
    }

    #[test]
    fn with_effect_handler_non_cap_target_errors() {
        // Non-Cap first arg → error.
        let mut env = Env::new();
        let d = RecordingDispatch::new();
        let result = eval_str(
            "(with-effect-handler 42 (fn [data] data) :body)",
            &mut env,
            &d,
        );
        assert!(result.is_err());
        if let Err(err) = &result {
            assert!(err_contains(err_payload(err), "cap"));
        }
    }

    #[test]
    fn with_effect_handler_non_fn_handler_errors() {
        // Non-Fn second arg → error at perform time (handler is still a value).
        let mut env = Env::new();
        let d = RecordingDispatch::new();
        let cap = make_test_cap("x", 1);
        env.set("my-cap".into(), cap);
        let result = eval_str(
            "(with-effect-handler my-cap 42 (perform my-cap :m 0))",
            &mut env,
            &d,
        );
        assert!(result.is_err());
    }

    #[test]
    fn effect_handler_cap_shadows_outer() {
        // Inner handler for same cap wins.
        let mut env = Env::new();
        let d = RecordingDispatch::new();
        let cap = make_test_cap("exec", 1);
        env.set("my-cap".into(), cap);
        let result = eval_str(
            "(with-effect-handler my-cap (fn [data] :outer) (with-effect-handler my-cap (fn [data] :inner) (perform my-cap :m 0)))",
            &mut env,
            &d,
        );
        assert_eq!(result, Ok(Val::Keyword("inner".into())));
    }

    #[test]
    fn effect_handler_cap_attenuation_forward() {
        // Inner handler delegates to outer via perform on same cap.
        // Pop-before-handle makes this work: inner is popped, perform hits outer.
        let mut env = Env::new();
        let d = RecordingDispatch::new();
        let cap = make_test_cap("exec", 1);
        env.set("my-cap".into(), cap);
        let result = eval_str(
            "(with-effect-handler my-cap (fn [data resume] (resume :forwarded)) (with-effect-handler my-cap (fn [data resume] (perform my-cap :delegated 0)) (perform my-cap :m 0)))",
            &mut env,
            &d,
        );
        assert_eq!(result, Ok(Val::Keyword("forwarded".into())));
    }

    #[test]
    fn effect_handler_cap_attenuation_block() {
        // Inner handler blocks disallowed method — returns error without forwarding.
        let mut env = Env::new();
        let d = RecordingDispatch::new();
        let cap = make_test_cap("exec", 1);
        env.set("my-cap".into(), cap);
        let result = eval_str(
            "(with-effect-handler my-cap (fn [data resume] (resume :full-authority)) (with-effect-handler my-cap (fn [data] :blocked) (perform my-cap :m 0)))",
            &mut env,
            &d,
        );
        // Inner handler aborts (1-arg, no resume) → returns :blocked, body is abandoned.
        assert_eq!(result, Ok(Val::Keyword("blocked".into())));
    }

    #[test]
    fn mixed_stack_walk() {
        // Keyword handler + cap handler on same stack, correct dispatch.
        let mut env = Env::new();
        let d = RecordingDispatch::new();
        let cap = make_test_cap("exec", 1);
        env.set("my-cap".into(), cap);
        // Install keyword handler for :fail, then cap handler for my-cap.
        // Keyword perform should hit keyword handler; cap perform should hit cap handler.
        let result = eval_str(
            "(with-effect-handler :fail (fn [data] :keyword-handled) (with-effect-handler my-cap (fn [data] :cap-handled) (perform my-cap :m 0)))",
            &mut env,
            &d,
        );
        assert_eq!(result, Ok(Val::Keyword("cap-handled".into())));
    }

    #[test]
    fn mixed_stack_keyword_through_cap() {
        // Cap handler is on the stack but keyword perform goes to keyword handler.
        let mut env = Env::new();
        let d = RecordingDispatch::new();
        let cap = make_test_cap("exec", 1);
        env.set("my-cap".into(), cap);
        let result = eval_str(
            "(with-effect-handler :fail (fn [data] :keyword-handled) (with-effect-handler my-cap (fn [data] :cap-handled) (perform :fail 0)))",
            &mut env,
            &d,
        );
        assert_eq!(result, Ok(Val::Keyword("keyword-handled".into())));
    }

    #[test]
    fn perform_cap_resume_value() {
        // Cap handler resumes with a transformed value.
        let mut env = Env::new();
        let d = RecordingDispatch::new();
        let cap = make_test_cap("math", 1);
        env.set("my-cap".into(), cap);
        let result = eval_str(
            "(with-effect-handler my-cap (fn [data resume] (resume 100)) (+ 1 (perform my-cap :compute 0)))",
            &mut env,
            &d,
        );
        assert_eq!(result, Ok(Val::Int(101)));
    }

    #[test]
    fn perform_target_must_be_keyword_or_cap() {
        // Passing a string as target should error.
        let mut env = Env::new();
        let d = RecordingDispatch::new();
        let result = eval_str("(perform \"not-valid\" 42)", &mut env, &d);
        assert!(result.is_err());
        if let Err(err) = &result {
            assert!(err_contains(err_payload(err), "keyword or cap"));
        }
    }

    #[test]
    fn async_native_fn_basic() {
        // AsyncNativeFn should be callable and its result awaited.
        let mut env = Env::new();
        let d = RecordingDispatch::new();
        env.set(
            "afn".into(),
            Val::AsyncNativeFn {
                name: "afn".into(),
                func: Rc::new(|args: Vec<Val>| {
                    Box::pin(core::future::ready(Ok(Val::Int(
                        if let Val::Int(n) = &args[0] {
                            n + 100
                        } else {
                            -1
                        },
                    ))))
                }),
            },
        );
        let result = eval_str("(afn 5)", &mut env, &d);
        assert_eq!(result, Ok(Val::Int(105)));
    }

    #[test]
    fn async_native_fn_error() {
        // AsyncNativeFn returning Err should propagate as an error.
        let mut env = Env::new();
        let d = RecordingDispatch::new();
        env.set(
            "fail-async".into(),
            Val::AsyncNativeFn {
                name: "fail-async".into(),
                func: Rc::new(|_args: Vec<Val>| {
                    Box::pin(core::future::ready(Err(NativeSignal::throw("async boom"))))
                }),
            },
        );
        let result = eval_str("(fail-async 1)", &mut env, &d);
        assert!(result.is_err());
    }

    #[test]
    fn async_native_fn_in_effect_handler() {
        // AsyncNativeFn used as a cap handler should work correctly.
        let mut env = Env::new();
        let d = RecordingDispatch::new();
        let cap = make_test_cap("svc", 1);
        env.set("my-cap".into(), cap.clone());
        env.set(
            "async-handler".into(),
            Val::AsyncNativeFn {
                name: "async-handler".into(),
                func: Rc::new(|_args: Vec<Val>| {
                    // Handler returns 999 directly.
                    Box::pin(core::future::ready(Ok(Val::Int(999))))
                }),
            },
        );
        let result = eval_str(
            "(with-effect-handler my-cap async-handler (perform my-cap :ping 1))",
            &mut env,
            &d,
        );
        assert_eq!(result, Ok(Val::Int(999)));
    }

    #[test]
    fn handler_depth_limit() {
        // Exceeding MAX_HANDLER_DEPTH should error.
        // We pre-fill the handler stack to near the limit, then one more push should fail.
        let mut env = Env::new();
        let d = RecordingDispatch::new();
        let cap = make_test_cap("x", 1);
        env.set("my-cap".into(), cap.clone());

        // Pre-fill handler stack to the limit.
        let cap_target = match &cap {
            Val::Cap(h) => h.effect_target(),
            _ => unreachable!(),
        };
        for _ in 0..effect::MAX_HANDLER_DEPTH {
            let ctx = Rc::new(RefCell::new(effect::HandlerContext {
                slot: Rc::new(RefCell::new(effect::EffectSlot::new())),
                target: cap_target.clone(),
            }));
            env.handler_stack.borrow_mut().push(ctx);
        }

        // One more with-effect-handler should hit the depth limit.
        let result = eval_str(
            "(with-effect-handler my-cap (fn [data] data) :body)",
            &mut env,
            &d,
        );
        assert!(result.is_err());
        if let Err(err) = &result {
            assert!(err_contains(err_payload(err), "depth limit"));
        }
    }

    // -----------------------------------------------------------------
    // perform* — apply-style perform
    // -----------------------------------------------------------------

    #[test]
    fn perform_star_keyword_effect() {
        let mut env = Env::new();
        let d = RecordingDispatch::new();
        let result = eval_str(
            "(with-effect-handler :x (fn [d resume] (resume (+ d 1)))
               (perform* :x [41]))",
            &mut env,
            &d,
        );
        assert_eq!(result, Ok(Val::Int(42)));
    }

    #[test]
    fn perform_star_cap_delegates_payload() {
        let mut env = Env::new();
        let d = RecordingDispatch::new();
        let result = eval_str(
            "(do (defcap svc :add (fn [a b] (+ a b)))
                 (perform* svc (list :add 2 3)))",
            &mut env,
            &d,
        );
        assert_eq!(result, Ok(Val::Int(5)));
    }

    #[test]
    fn perform_star_rejects_non_list_payload() {
        let mut env = Env::new();
        let d = RecordingDispatch::new();
        let result = eval_str("(perform* :x 41)", &mut env, &d);
        assert!(result.is_err());
    }

    #[test]
    fn perform_star_in_handler_skips_self() {
        // The load-bearing property for interposition handlers: a handler
        // delegating via perform* must reach the cap's own behavior, not
        // recurse into itself.
        let mut env = Env::new();
        let d = RecordingDispatch::new();
        let result = eval_str(
            "(do (defcap svc :echo (fn [x] x))
                 (with-effect-handler svc (fn [data resume] (resume (perform* svc data)))
                   (perform svc :echo 7)))",
            &mut env,
            &d,
        );
        assert_eq!(result, Ok(Val::Int(7)));
    }

    // -----------------------------------------------------------------
    // ww/policy module (std/lib/ww/policy.glia) — the P1 handlers
    // -----------------------------------------------------------------

    const POLICY_GLIA: &str = include_str!("../../../std/lib/ww/policy.glia");

    /// Load prelude + the ww/policy module source, then eval `input` in the
    /// same env. Mirrors what `(perform import "ww/policy")` provides.
    fn policy_eval(input: &str) -> Result<Val, EvalError> {
        let mut env = Env::new();
        let d = RecordingDispatch::new();
        for src in [crate::PRELUDE, POLICY_GLIA] {
            let forms = crate::read_many(src)
                .map_err(|e| boundary_thrown(Val::from(format!("parse: {e}"))))?;
            for form in &forms {
                eval_blocking(form, &mut env, &d)?;
            }
        }
        eval_str(input, &mut env, &d)
    }

    #[test]
    fn policy_audit_logs_then_delegates() {
        let result = policy_eval(
            "(do (defcap svc :echo (fn [x] x))
                 (def *log* (atom (list)))
                 (let [out (with-effect-handler svc
                             (audit svc (fn [d] (reset! *log* (concat (deref *log*) (list d)))))
                             (perform svc :echo 7))]
                   (list out (count (deref *log*)))))",
        )
        .unwrap();
        assert_eq!(result, Val::List(vec![Val::Int(7), Val::Int(1)]));
    }

    #[test]
    fn policy_mock_stubs_and_fails_closed() {
        let stubbed = policy_eval(
            "(do (defcap svc :echo (fn [x] x))
                 (with-effect-handler svc (mock {:echo 99 :add (fn [a b] (+ a b))})
                   (list (perform svc :echo 7) (perform svc :add 2 3))))",
        )
        .unwrap();
        assert_eq!(stubbed, Val::List(vec![Val::Int(99), Val::Int(5)]));

        let unstubbed = policy_eval(
            "(do (defcap svc :echo (fn [x] x))
                 (with-effect-handler svc (mock {:echo 99})
                   (perform svc :other 1)))",
        );
        assert!(unstubbed.is_err());
        assert!(err_contains(
            err_payload(&unstubbed.unwrap_err()),
            "not stubbed"
        ));
    }

    #[test]
    fn policy_retry_retries_until_success() {
        // The defcap fails twice (atom-backed counter), then succeeds;
        // retry 3 absorbs both failures.
        let result = policy_eval(
            "(do (def *fails* (atom 0))
                 (defcap svc :flaky (fn []
                   (if (< (deref *fails*) 2)
                     (do (reset! *fails* (+ (deref *fails*) 1))
                         (throw (ex-info \"transient\" {:type :glia.error/internal})))
                     :recovered)))
                 (with-effect-handler svc (retry svc 3)
                   (perform svc :flaky)))",
        )
        .unwrap();
        assert_eq!(result, Val::Keyword("recovered".into()));
    }

    #[test]
    fn policy_retry_exhausts_and_rethrows() {
        let result = policy_eval(
            "(do (defcap svc :flaky (fn []
                   (throw (ex-info \"always down\" {:type :glia.error/internal}))))
                 (with-effect-handler svc (retry svc 2)
                   (perform svc :flaky)))",
        );
        assert!(result.is_err());
        assert!(err_contains(
            err_payload(&result.unwrap_err()),
            "always down"
        ));
    }

    #[test]
    fn policy_budget_denies_after_n() {
        let ok = policy_eval(
            "(do (defcap svc :echo (fn [x] x))
                 (with-effect-handler svc (budget svc 2)
                   (list (perform svc :echo 1) (perform svc :echo 2))))",
        )
        .unwrap();
        assert_eq!(ok, Val::List(vec![Val::Int(1), Val::Int(2)]));

        let over = policy_eval(
            "(do (defcap svc :echo (fn [x] x))
                 (with-effect-handler svc (budget svc 2)
                   (do (perform svc :echo 1)
                       (perform svc :echo 2)
                       (perform svc :echo 3))))",
        );
        assert!(over.is_err());
        assert!(err_contains(err_payload(&over.unwrap_err()), "budget"));
    }

    #[test]
    fn policy_attenuate_handler_is_attenuate_sugar() {
        // With the default dispatch (no embedder reification) this exercises
        // the local fallback path; the kernel reifies the same surface into
        // a membrane. Allowed method works, unlisted method fails closed.
        let allowed = policy_eval(
            "(do (defcap svc :echo (fn [x] x) :zap (fn [] :boom))
                 (attenuate-handler ro svc [:echo]
                   (perform ro :echo 7)))",
        )
        .unwrap();
        assert_eq!(allowed, Val::Int(7));

        let denied = policy_eval(
            "(do (defcap svc :echo (fn [x] x) :zap (fn [] :boom))
                 (attenuate-handler ro svc [:echo]
                   (perform ro :zap)))",
        );
        assert!(denied.is_err());
        assert!(err_contains(err_payload(&denied.unwrap_err()), "denied"));
    }

    /// Was the retry blocker: closures dropped macro-expansion globals
    /// (`try` captured, `try-catches` not). Fixed by Env::capture_closure
    /// (#572); kept as the policy-context regression alongside the capture
    /// tests below.
    #[test]
    fn closure_calling_try_macro_resolves_try_catches() {
        // Minimal, handler-independent repro.
        assert_eq!(
            policy_eval("(do (defn f [] (try (throw 1) (catch _ e e))) (f))"),
            Ok(Val::Int(1)),
        );
    }

    #[test]
    fn def_inside_closure_does_not_mutate_caller_root() {
        // SEMANTIC — def inside a function throws the catchable
        // `glia.error/def-not-top-level` BEFORE any mutation (PR-1b.0;
        // supersedes the old silently-scoped def). Cross-invocation state
        // needs an atom.
        let err =
            prelude_eval("(do (def *n* 1) (defn bump [] (def *n* 9)) (bump) *n*)").unwrap_err();
        assert!(
            err_contains(err_payload(&err), "def-not-top-level"),
            "expected def-not-top-level, got: {err}"
        );
        // Catchable, and the definition never happened: *n* is unchanged.
        assert_eq!(
            prelude_eval(
                "(do (def *n* 1) (defn bump [] (def *n* 9)) \
                 (try (bump) (catch :glia.error/def-not-top-level e :caught)) *n*)"
            ),
            Ok(Val::Int(1))
        );
    }

    // -----------------------------------------------------------------
    // atoms — evaluator-local mutable cells
    // -----------------------------------------------------------------

    #[test]
    fn atom_deref_reset_roundtrip() {
        assert_eq!(
            prelude_eval("(let [a (atom 1)] (reset! a 5) (deref a))"),
            Ok(Val::Int(5))
        );
    }

    #[test]
    fn atom_state_survives_across_closure_invocations() {
        // The property budget/rate-limit handlers depend on: a captured atom
        // is shared, not cloned, across calls.
        assert_eq!(
            prelude_eval(
                "(let [a (atom 0)
                       bump (fn [] (reset! a (+ (deref a) 1)))]
                   (bump) (bump) (bump)
                   (deref a))"
            ),
            Ok(Val::Int(3))
        );
    }

    #[test]
    fn self_referential_atom_does_not_hang_cap_status() {
        // (reset! a a) makes the value graph cyclic; defining a closure over
        // it triggers compute_cap_status -> is_authority_free, which must
        // terminate via cycle detection (naive recursion stack-overflows:
        // RefCell allows nested shared borrows, so it is not a borrow panic).
        let r = prelude_eval(
            "(let [a (atom nil)]
               (reset! a a)
               (let [f (fn [] a)]
                 (f)
                 :done))",
        );
        assert_eq!(r, Ok(Val::Keyword("done".into())));
    }

    #[test]
    fn atom_equality_is_identity() {
        assert_eq!(
            prelude_eval("(let [a (atom 1) b (atom 1)] (list (= a a) (= a b)))"),
            Ok(Val::List(vec![Val::Bool(true), Val::Bool(false)]))
        );
    }

    #[test]
    fn atom_deref_type_errors_are_structured() {
        let r = prelude_eval("(deref 42)");
        assert!(r.is_err());
        assert!(err_contains(err_payload(&r.unwrap_err()), "atom"));
    }

    // -----------------------------------------------------------------
    // Closure capture must retain macros reachable through expansion.
    // Regression: closures capture free vars, computed on the un-expanded
    // body, so a macro (`try`) that expands to reference another global
    // (`try-catches`) previously left that global uncaptured and the
    // expansion fell through to host dispatch (nil). See capture_closure.
    // -----------------------------------------------------------------

    #[test]
    fn closure_calling_try_resolves_expansion_globals() {
        // Body catches: without the fix this returns nil.
        assert_eq!(
            prelude_eval("(do (defn f [] (try (throw 1) (catch _ e e))) (f))"),
            Ok(Val::Int(1)),
        );
        // Body does not throw: success value flows through.
        assert_eq!(
            prelude_eval("(do (defn g [] (try 7 (catch _ e e))) (g))"),
            Ok(Val::Int(7)),
        );
    }

    #[test]
    fn closure_macro_expanding_to_macro_chain() {
        // `when-not` -> `if`/`do`; and a user macro expanding to `try`. The
        // closure must resolve every macro reached through expansion.
        assert_eq!(
            prelude_eval(
                "(do (defmacro guarded [v] (list (quote try) v (list (quote catch) (quote _) (quote e) :caught)))
                     (defn f [] (guarded (throw 1)))
                     (f))"
            ),
            Ok(Val::Keyword("caught".into())),
        );
    }

    #[test]
    fn nested_closures_retain_macros() {
        // A closure returning a closure that uses `try`; the inner closure is
        // created while the outer runs, and must still capture `try-catches`.
        assert_eq!(
            prelude_eval(
                "(do (defn mk [] (fn [] (try (throw 9) (catch _ e e)))) (def inner (mk)) (inner))"
            ),
            Ok(Val::Int(9)),
        );
    }

    #[test]
    fn try_inside_effect_handler_body_catches() {
        // The case that surfaced the bug (ww/policy retry): a `try` around a
        // delegating perform, inside an effect handler.
        let r = prelude_eval(
            "(with-effect-handler :e
               (fn [d resume] (resume (try (throw 42) (catch _ x x))))
               (perform :e 0))",
        );
        assert_eq!(r, Ok(Val::Int(42)));
    }

    // -----------------------------------------------------------------
    // ww/test module (std/lib/ww/test.glia) — the framework must actually
    // register and run tests (regression for wetware/ww#574: the old
    // def-from-closure registry silently registered nothing, so run-tests
    // always reported "0 tests, all pass").
    // -----------------------------------------------------------------

    // =====================================================================
    // PR-1 contract tests — exception/fault/control separation
    // =====================================================================

    #[test]
    fn missing_map_key_is_nil_not_exception() {
        let mut env = Env::new();
        let d = RecordingDispatch::new();
        assert_eq!(eval_str("(get {:a 1} :b)", &mut env, &d), Ok(Val::Nil));
        assert_eq!(eval_str("(get {:a 1} 3.5)", &mut env, &d), Ok(Val::Nil));
        assert_eq!(eval_str("(get [1 2] 99)", &mut env, &d), Ok(Val::Nil));
        assert_eq!(eval_str("(get [1 2] -1)", &mut env, &d), Ok(Val::Nil));
    }

    #[test]
    fn wrong_arity_is_catchable_by_try() {
        let r = effects_eval(
            "(let [f (fn [x] x)] (try (f 1 2 3) (catch :glia.error/arity-mismatch e :caught)))",
        );
        assert_eq!(r, Ok(Val::Keyword("caught".into())));
    }

    #[test]
    fn wrong_type_is_catchable_by_try() {
        let r = effects_eval("(try (+ 1 \"a\") (catch :glia.error/type-mismatch e :caught))");
        assert_eq!(r, Ok(Val::Keyword("caught".into())));
    }

    #[test]
    fn division_by_zero_is_catchable_by_try() {
        let r = effects_eval("(try (/ 1 0) (catch _ e :caught))");
        assert_eq!(r, Ok(Val::Keyword("caught".into())));
    }

    #[test]
    fn unbound_symbol_call_is_catchable_by_try() {
        // Calling through an unbound head reaches Dispatch; RecordingDispatch
        // returns nil, so exercise a builtin structural error instead:
        // analysis error from a malformed let is a catchable exception when
        // raised inside a fn body analyzed at call time.
        let r = effects_eval("(try (get) (catch :glia.error/arity-mismatch e :caught))");
        assert_eq!(r, Ok(Val::Keyword("caught".into())));
    }

    #[test]
    fn native_error_is_catchable_by_try() {
        // A native fn raising a plain string error is catchable (wildcard).
        let mut env = Env::new();
        let d = RecordingDispatch::new();
        env.set(
            "boom".into(),
            Val::NativeFn {
                name: "boom".into(),
                func: Rc::new(|_| Err(NativeSignal::throw("native boom"))),
            },
        );
        let prelude_forms = crate::read_many(crate::PRELUDE).unwrap();
        for form in &prelude_forms {
            eval_blocking(form, &mut env, &d).unwrap();
        }
        let r = eval_str("(try (boom) (catch _ e :caught))", &mut env, &d);
        assert_eq!(r, Ok(Val::Keyword("caught".into())));
    }

    #[test]
    fn try_is_abortive_side_effects_after_throw_do_not_run() {
        let r = effects_eval(
            "(let [a (atom 0)]
               (try (do (throw 1) (reset! a 99)) (catch _ e nil))
               (deref a))",
        );
        assert_eq!(r, Ok(Val::Int(0)));
    }

    #[test]
    fn try_resume_resumes_builtin_error_with_replacement_value() {
        // Approved: exceptions are uniformly resumable; resuming supplies
        // the failing expression's value.
        let r = effects_eval("(try-resume (fn [e resume] (resume 0)) (+ 1 (+ 2 \"a\")))");
        assert_eq!(r, Ok(Val::Int(1)));
    }

    #[test]
    fn ordinary_effects_remain_resumable() {
        let r = effects_eval(
            "(with-effect-handler :e (fn [d resume] (resume (+ d 1))) (+ 10 (perform :e 1)))",
        );
        assert_eq!(r, Ok(Val::Int(12)));
    }

    #[test]
    fn fault_bypasses_try_non_tail_recur() {
        // Non-tail recur is a LANGUAGE FAULT — not catchable, reaches the
        // boundary even through a wildcard catch.
        let r = effects_eval("(try (loop [x 0] (f (recur 1))) (catch _ e :caught))");
        match r {
            Err(EvalError::Fault(f)) => {
                assert_eq!(f.kind(), crate::FaultKind::Language);
                assert_eq!(
                    error::type_tag(f.payload()),
                    Some(error::tag::INVALID_RECUR)
                );
            }
            other => panic!("expected language fault, got {other:?}"),
        }
    }

    #[test]
    fn non_tail_recur_cannot_become_stored_data_or_transfer() {
        let mut env = Env::new();
        let d = RecordingDispatch::new();
        // Stored-data pathology: previously returned [#<recur>].
        let r = eval_str("(loop [] [(recur)])", &mut env, &d);
        assert!(
            matches!(&r, Err(EvalError::Fault(f)) if f.kind() == crate::FaultKind::Language),
            "vector-literal recur must fault, got {r:?}"
        );
        // Argument-position pathology: previously passed #<recur> as a value.
        let r2 = eval_str("(loop [] (count (recur)))", &mut env, &d);
        assert!(
            matches!(&r2, Err(EvalError::Fault(f)) if f.kind() == crate::FaultKind::Language),
            "argument-position recur must fault, got {r2:?}"
        );
        // Non-last do position: previously skipped past the sentinel.
        let r3 = eval_str("(loop [] (do (recur) 5))", &mut env, &d);
        assert!(
            matches!(&r3, Err(EvalError::Fault(f)) if f.kind() == crate::FaultKind::Language),
            "non-tail do recur must fault, got {r3:?}"
        );
    }

    #[test]
    fn ordinary_tail_recur_still_works() {
        let mut env = Env::new();
        let d = RecordingDispatch::new();
        assert_eq!(
            eval_str("(loop [x 0] (if (< x 3) (recur (+ x 1)) x))", &mut env, &d),
            Ok(Val::Int(3))
        );
        // fn-targeted recur
        assert_eq!(
            eval_str(
                "(let [f (fn [n acc] (if (= n 0) acc (recur (- n 1) (* acc n))))] (f 5 1))",
                &mut env,
                &d
            ),
            Ok(Val::Int(120))
        );
    }

    #[test]
    fn handler_depth_limit_is_catchable() {
        // Exceeding MAX_HANDLER_DEPTH is a resource-guard EXCEPTION: the
        // check fires before the frame is pushed, so state is consistent
        // and outer handlers may recover.
        let mut env = Env::new();
        let d = RecordingDispatch::new();
        for _ in 0..effect::MAX_HANDLER_DEPTH {
            let ctx = Rc::new(RefCell::new(effect::HandlerContext {
                slot: Rc::new(RefCell::new(effect::EffectSlot::new())),
                target: effect::EffectTarget::Keyword("filler".into()),
            }));
            env.handler_stack.borrow_mut().push(ctx);
        }
        let prelude_forms = crate::read_many(crate::PRELUDE).unwrap();
        // Prelude must load without handler frames interfering; load into a
        // fresh env then copy the handler stack trick is overkill — instead
        // assert the boundary form directly (no try available here).
        drop(prelude_forms);
        let r = eval_str("(with-effect-handler :x (fn [d] d) 1)", &mut env, &d);
        match r {
            Err(EvalError::Unhandled(req)) => {
                assert_eq!(req.effect_type(), error::EXCEPTION_EFFECT);
                assert_eq!(error::type_tag(&req.data), Some(error::tag::INTERNAL));
            }
            other => panic!("expected catchable depth-limit exception, got {other:?}"),
        }
    }

    #[test]
    fn boundary_display_preserves_legacy_strings() {
        let mut env = Env::new();
        let d = RecordingDispatch::new();
        // Unhandled non-exception effect: legacy carrier string.
        let err = eval_str("(perform :net {:x 1})", &mut env, &d).unwrap_err();
        assert_eq!(format!("{err}"), "#<effect :net {:x 1}>");
        // Unhandled exception: peeled payload map (same as legacy raw-error
        // display for previously-raw errors).
        let err2 = eval_str("(+ 1 \"a\")", &mut env, &d).unwrap_err();
        let display = format!("{err2}");
        assert!(
            display.starts_with('{') && display.contains(":glia.error/type"),
            "expected peeled payload map, got: {display}"
        );
    }

    #[test]
    fn one_shot_second_resume_is_catchable_inside_handler() {
        let r = effects_eval(
            "(with-effect-handler :e
               (fn [d resume]
                 (do (resume 1)
                     :unreachable))
               (+ 10 (perform :e 0)))",
        );
        // First resume short-circuits the handler body; the body computes.
        // The one-shot structural pins (second resume raises the
        // continuation-already-resumed exception) live in effect.rs tests.
        assert_eq!(r, Ok(Val::Int(11)));
    }

    const WWTEST_GLIA: &str = include_str!("../../../std/lib/ww/test.glia");

    /// Run a deep-evaluation test body on an explicitly budgeted stack:
    /// 2 MiB in RELEASE — the real budget gate, matching the wasm guest
    /// commitment — and 4 MiB in debug, where async eval frames are several
    /// times larger (debug-only headroom; the release/wasm gates remain the
    /// binding measurement). Panics (failed assertions) propagate through
    /// `join`, so wrapped tests fail normally.
    fn on_budget_stack(f: impl FnOnce() + Send + 'static) {
        let stack = if cfg!(debug_assertions) {
            4 * 1024 * 1024
        } else {
            2 * 1024 * 1024
        };
        let handle = std::thread::Builder::new()
            .name("glia-budget-stack".into())
            .stack_size(stack)
            .spawn(f)
            .expect("spawn budget-stack thread");
        if let Err(panic) = handle.join() {
            std::panic::resume_unwind(panic);
        }
    }

    fn wwtest_eval(input: &str) -> Result<Val, EvalError> {
        let mut env = Env::new();
        let d = RecordingDispatch::new();
        for src in [crate::PRELUDE, WWTEST_GLIA] {
            let forms = crate::read_many(src)
                .map_err(|e| boundary_thrown(Val::from(format!("parse: {e}"))))?;
            for form in &forms {
                eval_blocking(form, &mut env, &d)?;
            }
        }
        let expr =
            crate::read(input).map_err(|e| boundary_thrown(error::parse(None, e.to_string())))?;
        let stdout: effect::HostEffectHandler =
            Rc::new(|_data| Box::pin(async { Ok(effect::HostEffectResult::Resume(Val::Nil)) }));
        let effects = [effect::HostEffect {
            target: effect::EffectTarget::Keyword("stdout".into()),
            handler: stdout,
        }];
        match pollster_eval(eval_toplevel_with_host_effects(
            &expr, &mut env, &d, &effects,
        ))? {
            EvalOutcome::Value(value) => Ok(value),
            EvalOutcome::Exit => Err(boundary_thrown(Val::from("unexpected exit in ww/test"))),
        }
    }

    #[test]
    fn wwtest_deftest_actually_registers() {
        on_budget_stack(wwtest_deftest_actually_registers_body);
    }

    fn wwtest_deftest_actually_registers_body() {
        let n = wwtest_eval(
            "(do (deftest \"t1\" (fn [] (assert= 1 1)))
                 (deftest \"t2\" (fn [] (assert-true true)))
                 (count (deref *tests*)))",
        );
        assert_eq!(n, Ok(Val::Int(2)));
    }

    #[test]
    fn wwtest_run_tests_executes_and_reports() {
        on_budget_stack(wwtest_run_tests_executes_and_reports_body);
    }

    fn wwtest_run_tests_executes_and_reports_body() {
        // One passing, one failing test: run-tests must EXECUTE both and
        // report accurate counts (previously always {:passed 0 :failed 0}).
        let r = wwtest_eval(
            "(do (deftest \"passes\" (fn [] (assert= 1 1)))
                 (deftest \"fails\"  (fn [] (assert= 1 2)))
                 (let [report (run-tests)]
                   (list (get report :passed) (get report :failed))))",
        )
        .unwrap();
        assert_eq!(r, Val::List(vec![Val::Int(1), Val::Int(1)]));
    }

    #[test]
    fn wwtest_assert_throws_and_reset() {
        // Deepest macro tower in the suite (deftest + assert-throws + try
        // expansion): runs on the budgeted stack (4 MiB debug / 2 MiB
        // release — the release size IS the budget gate).
        on_budget_stack(wwtest_assert_throws_and_reset_body);
    }

    fn wwtest_assert_throws_and_reset_body() {
        let r = wwtest_eval(
            "(do (deftest \"throws-ok\" (fn [] (assert-throws (fn [] (throw 1)))))
                 (let [report (run-tests)
                       _ (reset-tests)]
                   (list (get report :passed) (count (deref *tests*)))))",
        )
        .unwrap();
        assert_eq!(r, Val::List(vec![Val::Int(1), Val::Int(0)]));
    }

    // -----------------------------------------------------------------
    // ww/test capability harness (G4): stub-handler + recorder, with a
    // replay-style record/verify example.
    // -----------------------------------------------------------------

    #[test]
    fn wwtest_stub_handler_answers_and_fails_closed() {
        on_budget_stack(wwtest_stub_handler_answers_and_fails_closed_body);
    }

    fn wwtest_stub_handler_answers_and_fails_closed_body() {
        let ok = wwtest_eval(
            "(do (defcap svc :lookup (fn [k] :real))
                 (with-effect-handler svc (stub-handler {:lookup \"stubbed\" :add (fn [a b] (+ a b))})
                   (list (perform svc :lookup \"x\") (perform svc :add 2 3))))",
        )
        .unwrap();
        assert_eq!(ok, Val::List(vec![Val::Str("stubbed".into()), Val::Int(5)]));

        let denied = wwtest_eval(
            "(do (defcap svc :lookup (fn [k] :real))
                 (with-effect-handler svc (stub-handler {:lookup 1})
                   (perform svc :other)))",
        );
        assert!(denied.is_err());
        assert!(err_contains(
            err_payload(&denied.unwrap_err()),
            "not stubbed"
        ));
    }

    #[test]
    fn wwtest_recorder_replay_style_verification() {
        on_budget_stack(wwtest_recorder_replay_style_verification_body);
    }

    fn wwtest_recorder_replay_style_verification_body() {
        // Replay-style example: run the code under test with a recording
        // interposer, then verify the exact call sequence AND that the
        // delegated results were real.
        let r = wwtest_eval(
            "(do (defcap svc :add (fn [a b] (+ a b)) :neg (fn [x] (- 0 x)))
                 (let [r (recorder svc)
                       out (with-effect-handler svc (get r :handler)
                             (list (perform svc :add 1 2) (perform svc :neg 7)))]
                   (list out (deref (get r :calls)))))",
        )
        .unwrap();
        let expected_out = Val::List(vec![Val::Int(3), Val::Int(-7)]);
        let expected_calls = Val::List(vec![
            Val::List(vec![Val::Keyword("add".into()), Val::Int(1), Val::Int(2)]),
            Val::List(vec![Val::Keyword("neg".into()), Val::Int(7)]),
        ]);
        assert_eq!(r, Val::List(vec![expected_out, expected_calls]));
    }
}

/// PR-1b.0 ownership-barrier mechanism tests. Everything here exercises the
/// crate-private machinery introduced in Stage A, before any production path
/// calls it. These tests are RC-implementation-specific: under a future GC
/// migration this module is deleted with the barrier (see the recorded
/// deletion inventory); the SEMANTIC suites elsewhere survive.
#[cfg(test)]
mod ownership_tests {
    use super::own::{self, OwnerRef};
    use super::{tests, Defs, Env, Frame};
    use crate::{Val, ValMap};
    use std::rc::Rc;

    // RC-MECHANISM: rest weakens only matching-owner references; foreign
    // owners stay strong.
    #[test]
    fn rest_weakens_only_matching_owner() {
        let a = Defs::new(None);
        let b = Defs::new(None);
        let own_ref = OwnerRef::Strong(Rc::clone(&a));
        let foreign = OwnerRef::Strong(Rc::clone(&b));

        let rested_own = own_ref.rested(&a);
        assert!(rested_own.is_resting_for(&a), "self-reference must rest");

        let rested_foreign = foreign.rested(&a);
        assert!(
            !rested_foreign.is_resting_for(&a),
            "foreign owner must not rest"
        );
        assert!(
            matches!(&rested_foreign, OwnerRef::Strong(o) if Rc::ptr_eq(o, &b)),
            "foreign strong reference preserved"
        );
    }

    // RC-MECHANISM: escape restores matching references via the witness.
    #[test]
    fn escape_restores_matching_reference() {
        let a = Defs::new(None);
        let rested = OwnerRef::Strong(Rc::clone(&a)).rested(&a);
        let escaped = rested.escaped_with(&a).expect("matching witness escapes");
        assert!(
            matches!(&escaped, OwnerRef::Strong(o) if Rc::ptr_eq(o, &a)),
            "escape restores a strong reference to the same owner"
        );
    }

    // RC-MECHANISM: repeated rest/escape is idempotent.
    #[test]
    fn rest_and_escape_idempotent() {
        let a = Defs::new(None);
        let r1 = OwnerRef::Strong(Rc::clone(&a)).rested(&a);
        let r2 = r1.rested(&a);
        assert!(r2.is_resting_for(&a), "second rest is a no-op");

        let e1 = r2.escaped_with(&a).unwrap();
        let e2 = e1.escaped_with(&a).unwrap();
        assert!(
            matches!(&e2, OwnerRef::Strong(o) if Rc::ptr_eq(o, &a)),
            "second escape is a no-op"
        );
    }

    // RC-MECHANISM: an unmatched weak witness faults — explicitly, not as
    // revocation, not as a guest error.
    #[test]
    fn unmatched_weak_witness_faults() {
        let a = Defs::new(None);
        let other = Defs::new(None);
        let rested = OwnerRef::Strong(Rc::clone(&a)).rested(&a);
        assert_eq!(
            rested.escaped_with(&other).unwrap_err(),
            own::OwnFault::UnmatchedWeak
        );
    }

    // RC-MECHANISM: resting releases the keep-alive; the owner frees when
    // the last strong reference drops even while a rested reference exists.
    #[test]
    fn rested_reference_does_not_keep_owner_alive() {
        let a = Defs::new(None);
        let probe = Rc::downgrade(&a);
        let rested = OwnerRef::Strong(Rc::clone(&a)).rested(&a);
        drop(a);
        assert!(
            probe.upgrade().is_none(),
            "rested reference must not keep the owner alive"
        );
        // And escaping it afterwards is the fault case, not a resurrection.
        let dead = Defs::new(None);
        assert!(rested.escaped_with(&dead).is_err());
    }

    // RC-MECHANISM: deep traversal is iterative — container depth must not
    // consume Rust call stack. (Construction and teardown are also kept
    // iterative: recursive Clone/Drop of deep Vec-backed values is a
    // pre-existing property of `Val` independent of the barrier.)
    #[test]
    fn deep_traversal_is_iterative() {
        let owner = Defs::new(None);
        const DEPTH: usize = 100_000;
        let mut v = Val::Int(0);
        for _ in 0..DEPTH {
            v = Val::List(vec![v]);
        }

        let (rested, resting) = own::rest_for(&owner, &v);
        assert!(!resting, "pure data holds no resting refs");
        let escaped = own::escape_with(&owner, &rested).unwrap();

        // Verify depth survived, iteratively.
        let mut depth = 0usize;
        let mut cur = &escaped;
        while let Val::List(xs) = cur {
            depth += 1;
            cur = &xs[0];
        }
        assert_eq!(depth, DEPTH);

        // Iterative teardown of all three deep trees.
        for val in [v, rested, escaped] {
            let mut stack = vec![val];
            while let Some(x) = stack.pop() {
                if let Val::List(xs) = x {
                    stack.extend(xs);
                }
            }
        }
    }

    // RC-MECHANISM: scalar and `Bytes` values are barrier-inert leaves —
    // the transform neither recurses into them nor flags them.
    #[test]
    fn scalars_and_bytes_are_leaves() {
        let owner = Defs::new(None);
        let leaves = vec![
            Val::Nil,
            Val::Bool(true),
            Val::Int(42),
            Val::Float(1.5),
            Val::Str("s".into()),
            Val::Sym("x".into()),
            Val::Keyword("k".into()),
            Val::Bytes(vec![0u8; 4096]),
            Val::Atom(Rc::new(std::cell::RefCell::new(Val::Int(1)))),
        ];
        for leaf in leaves {
            let (rested, resting) = own::rest_for(&owner, &leaf);
            assert!(!resting);
            assert_eq!(format!("{rested:?}"), format!("{leaf:?}"));
            let escaped = own::escape_with(&owner, &rested).unwrap();
            assert_eq!(format!("{escaped:?}"), format!("{leaf:?}"));
        }
    }

    // RC-MECHANISM: identity-keyed map entries survive the transforms —
    // atom keys (identity-compared, like callables) still resolve after a
    // rest/escape round trip. Callable keys get their direct equivalent in
    // Stage C when `Closure` carries the identity anchor.
    #[test]
    fn map_identity_keys_preserved_through_transforms() {
        let owner = Defs::new(None);
        let atom_key = Val::Atom(Rc::new(std::cell::RefCell::new(Val::Int(7))));
        let m = Val::Map(ValMap::from_pairs(vec![
            (atom_key.clone(), Val::Str("hit".into())),
            (Val::Keyword("plain".into()), Val::Int(1)),
        ]));

        let (rested, resting) = own::rest_for(&owner, &m);
        assert!(!resting);
        let escaped = own::escape_with(&owner, &rested).unwrap();

        let Val::Map(out) = &escaped else {
            panic!("map survives transforms");
        };
        assert_eq!(out.len(), 2);
        assert_eq!(out.get(&atom_key), Some(&Val::Str("hit".into())));
    }

    // RC-MECHANISM: nested containers rebuild correctly in source order.
    #[test]
    fn nested_container_structure_preserved() {
        let owner = Defs::new(None);
        let v = Val::Vector(vec![
            Val::Int(1),
            Val::List(vec![Val::Int(2), Val::Int(3)]),
            Val::Set(vec![Val::Int(4)]),
            Val::Map(ValMap::from_pairs(vec![(
                Val::Keyword("k".into()),
                Val::Vector(vec![Val::Int(5), Val::Int(6)]),
            )])),
        ]);
        let (rested, _) = own::rest_for(&owner, &v);
        let escaped = own::escape_with(&owner, &rested).unwrap();
        assert_eq!(format!("{escaped:?}"), format!("{v:?}"));
    }

    // RC-MECHANISM: the inert Stage A `Defs` — define/lookup round trip on
    // data takes the fast path; frozen owners reject definition.
    #[test]
    fn defs_define_lookup_and_freeze() {
        let d = Defs::new(None);
        let v0 = d.version();
        d.define("answer".into(), Val::Int(42)).unwrap();
        assert!(d.version() > v0, "definition bumps the version");
        assert_eq!(d.lookup("answer").unwrap(), Some(Val::Int(42)));
        assert_eq!(d.lookup("missing").unwrap(), None);

        // Inherited chain resolves parent names; local shadows win.
        let child = Defs::new(Some(Rc::clone(&d)));
        assert_eq!(child.lookup("answer").unwrap(), Some(Val::Int(42)));
        child.define("answer".into(), Val::Int(1)).unwrap();
        assert_eq!(child.lookup("answer").unwrap(), Some(Val::Int(1)));
        assert_eq!(d.lookup("answer").unwrap(), Some(Val::Int(42)));

        // local_bindings enumerates LOCAL names only.
        let locals = child.local_bindings().unwrap();
        assert_eq!(locals.len(), 1);
        assert_eq!(locals[0].0, "answer");

        d.freeze();
        assert!(d.is_frozen());
        assert_eq!(
            d.define("later".into(), Val::Int(0)).unwrap_err(),
            own::OwnFault::FrozenMutation
        );
    }

    // RC-MECHANISM: ONE-WAY transform assertions — rest_for alone and
    // escape_with alone must preserve collection order and map pairing.
    // (Round-trip tests masked an ordering defect because rest + escape
    // applied compensating reversals; these pin each direction separately.)
    #[test]
    fn one_way_transforms_preserve_structure() {
        let owner = Defs::new(None);
        let vector = Val::Vector(vec![Val::Int(1), Val::Int(2), Val::Int(3)]);
        let (rested, _) = own::rest_for(&owner, &vector);
        assert_eq!(format!("{rested:?}"), format!("{vector:?}"));
        let escaped = own::escape_with(&owner, &vector).unwrap();
        assert_eq!(format!("{escaped:?}"), format!("{vector:?}"));

        let map = Val::Map(ValMap::from_pairs(vec![
            (Val::Keyword("k".into()), Val::Int(1)),
            (Val::Keyword("j".into()), Val::Int(2)),
        ]));
        for transformed in [
            own::rest_for(&owner, &map).0,
            own::escape_with(&owner, &map).unwrap(),
        ] {
            let Val::Map(m) = &transformed else {
                panic!("map survives");
            };
            assert_eq!(m.get(&Val::Keyword("k".into())), Some(&Val::Int(1)));
            assert_eq!(m.get(&Val::Keyword("j".into())), Some(&Val::Int(2)));
        }

        let nested = Val::List(vec![
            Val::Int(1),
            Val::Vector(vec![Val::Int(2), Val::Int(3)]),
            Val::Map(ValMap::from_pairs(vec![(
                Val::Keyword("k".into()),
                Val::Vector(vec![Val::Int(4), Val::Int(5)]),
            )])),
        ]);
        let (rested, _) = own::rest_for(&owner, &nested);
        assert_eq!(format!("{rested:?}"), format!("{nested:?}"));
    }

    // RC-MECHANISM: direct Defs storage — define + lookup preserves
    // collection structure (no inverse-bug cancellation possible).
    #[test]
    fn defs_storage_preserves_collections() {
        let d = Defs::new(None);
        d.define(
            "xs".into(),
            Val::Vector(vec![Val::Int(1), Val::Int(2), Val::Int(3)]),
        )
        .unwrap();
        assert_eq!(
            d.lookup("xs").unwrap(),
            Some(Val::Vector(vec![Val::Int(1), Val::Int(2), Val::Int(3)]))
        );
        d.define(
            "m".into(),
            Val::Map(ValMap::from_pairs(vec![(
                Val::Keyword("k".into()),
                Val::Int(1),
            )])),
        )
        .unwrap();
        let Some(Val::Map(m)) = d.lookup("m").unwrap() else {
            panic!("stored map resolves");
        };
        assert_eq!(m.get(&Val::Keyword("k".into())), Some(&Val::Int(1)));
    }

    // RC-MECHANISM: reader map-literal provenance (duplicate-key evidence)
    // survives the ownership transforms for unchanged maps.
    #[test]
    fn map_literal_provenance_survives_transforms() {
        let owner = Defs::new(None);
        let map_val = crate::read("{:a 1 :a 2}").expect("reader accepts duplicate keys");
        let Val::Map(m) = &map_val else {
            panic!("reader yields a map");
        };
        assert!(m.literal_pairs().is_some(), "reader attaches provenance");

        let (rested, _) = own::rest_for(&owner, &map_val);
        let Val::Map(rm) = &rested else {
            panic!("map survives rest");
        };
        assert_eq!(
            rm.literal_pairs().map(<[_]>::len),
            Some(2),
            "unchanged maps keep duplicate-key provenance through rest"
        );
        let escaped = own::escape_with(&owner, &rested).unwrap();
        let Val::Map(em) = &escaped else {
            panic!("map survives escape");
        };
        assert_eq!(em.literal_pairs().map(<[_]>::len), Some(2));
    }

    // RC-MECHANISM — STAGE C EXIT CRITERION (Sol R1 §3): the routine
    // `Defs → callable → Env → Defs` strong cycle is REMOVED. Defining a
    // closure into its own owner no longer keeps that owner alive.
    #[test]
    fn defs_strong_cycle_removed() {
        let mut env = Env::new();
        let d = tests::RecordingDispatch::new();
        let probe = Rc::downgrade(env.defs());
        tests::eval_str("(def f (fn [] 1))", &mut env, &d).unwrap();
        drop(env);
        assert!(
            probe.upgrade().is_none(),
            "defining a self-owned closure must not leak the owner"
        );
    }

    // RC-MECHANISM: named recursion does not leak the owner.
    #[test]
    fn named_recursion_no_leak() {
        let mut env = Env::new();
        let d = tests::RecordingDispatch::new();
        let probe = Rc::downgrade(env.defs());
        tests::eval_str(
            "(def fact (fn [n] (if (< n 2) 1 (* n (fact (- n 1))))))",
            &mut env,
            &d,
        )
        .unwrap();
        assert_eq!(tests::eval_str("(fact 5)", &mut env, &d), Ok(Val::Int(120)));
        drop(env);
        assert!(
            probe.upgrade().is_none(),
            "recursive definition must not leak"
        );
    }

    // RC-MECHANISM: mutual recursion does not leak the owner.
    #[test]
    fn mutual_recursion_no_leak() {
        let mut env = Env::new();
        let d = tests::RecordingDispatch::new();
        let probe = Rc::downgrade(env.defs());
        tests::eval_str(
            "(def is-even (fn [n] (if (= n 0) true (is-odd (- n 1)))))",
            &mut env,
            &d,
        )
        .unwrap();
        tests::eval_str(
            "(def is-odd (fn [n] (if (= n 0) false (is-even (- n 1)))))",
            &mut env,
            &d,
        )
        .unwrap();
        assert_eq!(
            tests::eval_str("(is-even 4)", &mut env, &d),
            Ok(Val::Bool(true))
        );
        drop(env);
        assert!(probe.upgrade().is_none(), "mutual recursion must not leak");
    }

    // RC-MECHANISM: a same-owner callable nested in another callable's
    // capture (Sol's canonical program) neither breaks activation nor
    // leaks — captured slots rest at capture and escape at activation.
    #[test]
    fn nested_same_owner_capture_no_leak() {
        let mut env = Env::new();
        let d = tests::RecordingDispatch::new();
        let probe = Rc::downgrade(env.defs());
        tests::eval_str("(def f (let [g (fn [] 1)] (fn [] (g))))", &mut env, &d).unwrap();
        assert_eq!(tests::eval_str("(f)", &mut env, &d), Ok(Val::Int(1)));
        drop(env);
        assert!(
            probe.upgrade().is_none(),
            "nested same-owner capture must not leak"
        );
    }

    // RC-MECHANISM: a foreign-owner callable stored in another owner stays
    // strong — the foreign module remains alive exactly as long as a
    // holder exists.
    #[test]
    fn foreign_owner_callable_stays_alive() {
        let mut env_a = Env::new();
        let d = tests::RecordingDispatch::new();
        let probe_a = Rc::downgrade(env_a.defs());
        tests::eval_str("(def f (fn [] 41))", &mut env_a, &d).unwrap();
        let escaped_f = env_a.get("f").unwrap().expect("f resolves");

        let env_b = Env::new();
        env_b.defs().define("af".into(), escaped_f).unwrap();
        drop(env_a);
        assert!(
            probe_a.upgrade().is_some(),
            "foreign holder keeps the defining owner alive"
        );
        drop(env_b);
        assert!(
            probe_a.upgrade().is_none(),
            "dropping the last holder frees the foreign owner"
        );
    }

    // RC-MECHANISM: the LAST escaped closure controls owner reclamation;
    // exported closures survive after the originating env and export map
    // drop. Exact strong/weak counts at each transition point.
    #[test]
    fn exact_counts_and_last_escapee_reclamation() {
        let mut env = Env::new();
        let d = tests::RecordingDispatch::new();
        let probe = Rc::downgrade(env.defs());

        // Construction + definition storage: only the env holds the owner
        // strongly (the stored closure RESTS); the resting binding holds
        // one weak reference (plus the probe).
        tests::eval_str("(def f (fn [] 7))", &mut env, &d).unwrap();
        assert_eq!(Rc::strong_count(env.defs()), 1, "definition storage");
        assert_eq!(Rc::weak_count(env.defs()), 2, "resting ref + probe");

        // Lookup: the escaped copy holds the owner strongly.
        let looked_up = env.get("f").unwrap().expect("f resolves");
        assert_eq!(Rc::strong_count(env.defs()), 2, "lookup escapee");

        // Invocation: transient — counts return to the pre-call state.
        let out = tests::eval_str("(f)", &mut env, &d);
        assert_eq!(out, Ok(Val::Int(7)));
        assert_eq!(Rc::strong_count(env.defs()), 2, "invocation is transient");

        // Export: one more strong holder per exported copy.
        let exports = env.local_bindings().unwrap();
        assert_eq!(Rc::strong_count(env.defs()), 3, "export escapee");

        // Drop the env: escapees keep the owner alive.
        let defs_probe = probe.clone();
        drop(env);
        assert!(defs_probe.upgrade().is_some(), "escapees keep owner alive");

        // Drop the export map: one escapee left.
        drop(exports);
        assert!(defs_probe.upgrade().is_some(), "last escapee still holds");

        // Final escape drop: the owner is reclaimed.
        drop(looked_up);
        assert!(
            defs_probe.upgrade().is_none(),
            "last escapee drop reclaims the owner"
        );
    }

    // RC-MECHANISM: identity and hash anchor on the captured-env pointer —
    // preserved through define (rest) and lookup (escape); callable map
    // keys stay valid.
    #[test]
    fn identity_hash_preserved_through_define_lookup() {
        use std::collections::hash_map::DefaultHasher;
        use std::hash::{Hash, Hasher};

        let mut env = Env::new();
        let d = tests::RecordingDispatch::new();
        tests::eval_str("(def f (fn [] 1))", &mut env, &d).unwrap();
        let a = env.get("f").unwrap().expect("f resolves");
        let b = env.get("f").unwrap().expect("f resolves");
        assert_eq!(a, b, "escaped copies of one closure compare equal");
        let hash = |v: &Val| {
            let mut h = DefaultHasher::new();
            v.hash(&mut h);
            h.finish()
        };
        assert_eq!(hash(&a), hash(&b), "hashes stable through rest/escape");

        // Callable map key: a map keyed by the closure resolves through a
        // separately escaped copy.
        let m = Val::Map(crate::ValMap::from_pairs(vec![(
            a.clone(),
            Val::Str("hit".into()),
        )]));
        env.defs().define("m".into(), m).unwrap();
        let Some(Val::Map(m2)) = env.get("m").unwrap() else {
            panic!("map resolves");
        };
        assert_eq!(m2.get(&b), Some(&Val::Str("hit".into())));

        // Separately evaluated fn forms remain DISTINCT identities.
        tests::eval_str("(def g (fn [] 1))", &mut env, &d).unwrap();
        let g = env.get("g").unwrap().expect("g resolves");
        assert_ne!(a, g, "separate evaluations are distinct closures");
    }

    // RC-MECHANISM: CapturedEnv is lexical-only storage (no Defs inside —
    // enforced by construction; this pins the accessor surface).
    #[test]
    fn captured_env_is_lexical_slots_only() {
        let mut slots = Frame::new();
        slots.insert("x".into(), Val::Int(9));
        let captured = super::CapturedEnv::from_slots(slots);
        assert_eq!(captured.len(), 1);
        assert_eq!(captured.get("x"), Some(&Val::Int(9)));
        assert_eq!(captured.get("y"), None);
    }
}

/// PR-1b.0 Stage B definition-semantics tests. All `// SEMANTIC`: these pin
/// language behavior (definition ownership, late binding, the top-level
/// gate, prelude sharing) and survive any future memory-model migration.
#[cfg(test)]
mod stage_b_semantics {
    use super::tests::RecordingDispatch;
    use super::{Env, EvalError};
    use crate::Val;

    #[allow(clippy::result_large_err)] // test helper; EvalError is the boundary type
    fn eval_seq(env: &mut Env, d: &RecordingDispatch, forms: &[&str]) -> Result<Val, EvalError> {
        let mut last = Val::Nil;
        for src in forms {
            last = super::tests::eval_str(src, env, d)?;
        }
        Ok(last)
    }

    /// Env with the REAL shared-prelude lifecycle (memoized, frozen,
    /// inherited) — the module/REPL initialization shape.
    fn module_env(d: &mut RecordingDispatch) -> Env {
        let mut env = Env::new();
        super::tests::pollster_eval(crate::load_prelude(&mut env, d));
        env
    }

    // SEMANTIC — late binding, canonical program.
    #[test]
    fn late_binding_canonical() {
        let mut d = RecordingDispatch::new();
        let mut env = module_env(&mut d);
        let out = eval_seq(
            &mut env,
            &d,
            &["(def x 1)", "(defn f [] x)", "(def x 2)", "(f)"],
        );
        assert_eq!(out, Ok(Val::Int(2)));
    }

    // SEMANTIC — named recursion.
    #[test]
    fn named_recursion() {
        let mut d = RecordingDispatch::new();
        let mut env = module_env(&mut d);
        let out = eval_seq(
            &mut env,
            &d,
            &[
                "(defn fact [n] (if (< n 2) 1 (* n (fact (- n 1)))))",
                "(fact 5)",
            ],
        );
        assert_eq!(out, Ok(Val::Int(120)));
    }

    // SEMANTIC — mutual recursion (top-level forward reference).
    #[test]
    fn mutual_recursion() {
        let mut d = RecordingDispatch::new();
        let mut env = module_env(&mut d);
        let out = eval_seq(
            &mut env,
            &d,
            &[
                "(defn is-even [n] (if (= n 0) true (is-odd (- n 1))))",
                "(defn is-odd [n] (if (= n 0) false (is-even (- n 1))))",
                // Guest recursion depth is bounded by the host Rust stack
                // (async eval frames): ≈5 in debug builds, hundreds in
                // release (measured: 100 passes, 1000 overflows at the
                // 2 MiB default). `recur` remains the unbounded-iteration
                // construct. Depth budget is a Stage G measurement item.
                "(is-even 4)",
            ],
        );
        assert_eq!(out, Ok(Val::Bool(true)));
        #[cfg(not(debug_assertions))]
        assert_eq!(
            eval_seq(&mut env, &d, &["(is-even 100)"]),
            Ok(Val::Bool(true))
        );
    }

    // SEMANTIC — repeated definition: last write wins.
    #[test]
    fn repeated_definition_last_wins() {
        let mut d = RecordingDispatch::new();
        let mut env = module_env(&mut d);
        let out = eval_seq(&mut env, &d, &["(def x 1)", "(def x 2)", "x"]);
        assert_eq!(out, Ok(Val::Int(2)));
    }

    // SEMANTIC — def inside a function: catchable
    // glia.error/def-not-top-level; no mutation occurs.
    #[test]
    fn def_inside_fn_is_catchable() {
        let mut d = RecordingDispatch::new();
        let mut env = module_env(&mut d);
        let out = eval_seq(
            &mut env,
            &d,
            &[
                "(defn install [] (def hidden 1))",
                "(try (install) (catch :glia.error/def-not-top-level e :caught))",
            ],
        );
        assert_eq!(out, Ok(Val::Keyword("caught".into())));
        // The definition never happened.
        assert!(env
            .local_bindings()
            .unwrap()
            .iter()
            .all(|(n, _)| n != "hidden"));
    }

    // SEMANTIC — a function called during module initialization cannot
    // define (call envs never carry definition privilege).
    #[test]
    fn fn_called_during_module_init_cannot_define() {
        let mut d = RecordingDispatch::new();
        let mut env = module_env(&mut d);
        let err = eval_seq(
            &mut env,
            &d,
            &["(defn setup [] (def installed 1))", "(setup)"],
        )
        .unwrap_err();
        assert!(
            err.to_string().contains("def-not-top-level"),
            "expected def-not-top-level, got: {err}"
        );
    }

    // SEMANTIC — a top-level macro may expand to `def` (the expansion
    // evaluates in the caller's top-level environment).
    #[test]
    fn top_level_macro_expanding_to_def_succeeds() {
        let mut d = RecordingDispatch::new();
        let mut env = module_env(&mut d);
        let out = eval_seq(
            &mut env,
            &d,
            &[
                "(defmacro def-two [n] (list (quote def) n 2))",
                "(def-two y)",
                "y",
            ],
        );
        assert_eq!(out, Ok(Val::Int(2)));
    }

    // SEMANTIC — a macro BODY attempting `def` fails (expansion computation
    // runs without definition privilege).
    #[test]
    fn macro_body_attempting_def_fails() {
        let mut d = RecordingDispatch::new();
        let mut env = module_env(&mut d);
        let err = eval_seq(&mut env, &d, &["(defmacro bad [] (def z 9))", "(bad)"]).unwrap_err();
        assert!(
            err.to_string().contains("def-not-top-level"),
            "expected def-not-top-level, got: {err}"
        );
    }

    // SEMANTIC — prelude names resolve inside a module via inherited
    // lookup.
    #[test]
    fn prelude_names_resolve_in_module() {
        let mut d = RecordingDispatch::new();
        let mut env = module_env(&mut d);
        let out = eval_seq(&mut env, &d, &["(when true 42)"]);
        assert_eq!(out, Ok(Val::Int(42)));
    }

    // SEMANTIC — prelude names are absent from local_bindings; only owned
    // definitions appear (embedder root-frame bindings excluded too).
    #[test]
    fn local_bindings_owned_definitions_only() {
        let mut d = RecordingDispatch::new();
        let mut env = module_env(&mut d);
        assert!(
            env.local_bindings().unwrap().is_empty(),
            "fresh module owns no definitions"
        );
        // Embedder ambient context (root lexical frame) is not a module
        // definition.
        env.set("ambient".into(), Val::Int(0));
        eval_seq(&mut env, &d, &["(def answer 42)"]).unwrap();
        let names: Vec<String> = env
            .local_bindings()
            .unwrap()
            .into_iter()
            .map(|(n, _)| n)
            .collect();
        assert_eq!(names, vec!["answer".to_string()]);
    }

    // SEMANTIC — local shadowing of a prelude name: resolves locally,
    // exports locally, never mutates the shared prelude, never affects a
    // sibling environment.
    #[test]
    fn prelude_shadowing_is_local() {
        let mut d = RecordingDispatch::new();
        let mut env_a = module_env(&mut d);
        let out = eval_seq(
            &mut env_a,
            &d,
            &["(defmacro when [test & body] :shadowed)", "(when true 1)"],
        );
        assert_eq!(out, Ok(Val::Keyword("shadowed".into())));
        let names: Vec<String> = env_a
            .local_bindings()
            .unwrap()
            .into_iter()
            .map(|(n, _)| n)
            .collect();
        assert_eq!(names, vec!["when".to_string()], "shadow exports locally");

        // Sibling environment inherits the UNTOUCHED shared prelude.
        let mut env_b = module_env(&mut d);
        let out_b = eval_seq(&mut env_b, &d, &["(when true 1)"]);
        assert_eq!(out_b, Ok(Val::Int(1)), "sibling sees the real prelude");
    }

    // SEMANTIC — REPL shape: definitions persist across top-level
    // evaluations and observe late redefinition.
    #[test]
    fn repl_definitions_persist_and_late_bind() {
        let mut d = RecordingDispatch::new();
        let mut env = module_env(&mut d);
        assert_eq!(
            eval_seq(&mut env, &d, &["(def n 1)", "(defn get-n [] n)"]).map(|_| ()),
            Ok(())
        );
        assert_eq!(eval_seq(&mut env, &d, &["(get-n)"]), Ok(Val::Int(1)));
        assert_eq!(
            eval_seq(&mut env, &d, &["(def n 5)", "(get-n)"]),
            Ok(Val::Int(5))
        );
    }

    // SEMANTIC — lexical behavior unchanged: `let` shadows definitions
    // lexically; closures still snapshot lexical locals.
    #[test]
    fn lexical_semantics_unchanged() {
        let mut d = RecordingDispatch::new();
        let mut env = module_env(&mut d);
        assert_eq!(
            eval_seq(&mut env, &d, &["(def x 1)", "(let [x 5] x)"]),
            Ok(Val::Int(5))
        );
        assert_eq!(eval_seq(&mut env, &d, &["x"]), Ok(Val::Int(1)));
        // Lexical capture is still a snapshot.
        assert_eq!(
            eval_seq(&mut env, &d, &["(def g (let [y 7] (fn [] y)))", "(g)"]),
            Ok(Val::Int(7))
        );
    }

    // SEMANTIC — a closure survives and works after its defining
    // environment drops (the exported-value lifetime guarantee).
    #[test]
    fn closure_survives_defining_env_drop() {
        let mut d = RecordingDispatch::new();
        let mut env_a = module_env(&mut d);
        eval_seq(&mut env_a, &d, &["(def x 41)", "(defn f [] (+ x 1))"]).unwrap();
        let f = env_a.get("f").unwrap().expect("f resolves");
        drop(env_a);

        let mut env_b = module_env(&mut d);
        env_b.set("imported-f".into(), f);
        assert_eq!(
            eval_seq(&mut env_b, &d, &["(imported-f)"]),
            Ok(Val::Int(42)),
            "closure keeps its defining owner (late-bound x) after env drop"
        );
    }

    // SEMANTIC — a macro survives after its defining environment drops.
    #[test]
    fn macro_survives_defining_env_drop() {
        let mut d = RecordingDispatch::new();
        let mut env_a = module_env(&mut d);
        eval_seq(&mut env_a, &d, &["(defmacro answer [] 42)"]).unwrap();
        let m = env_a.get("answer").unwrap().expect("macro resolves");
        drop(env_a);

        let mut env_b = module_env(&mut d);
        env_b.set("imported-answer".into(), m);
        assert_eq!(
            eval_seq(&mut env_b, &d, &["(imported-answer)"]),
            Ok(Val::Int(42))
        );
    }

    // SEMANTIC — defined collections round-trip unchanged (Sol R1 change 1:
    // guest-visible reproducers for the ordering defect).
    #[test]
    fn defined_collections_round_trip() {
        let mut d = RecordingDispatch::new();
        let mut env = module_env(&mut d);
        assert_eq!(
            eval_seq(&mut env, &d, &["(def xs [1 2 3])", "xs"]),
            Ok(Val::Vector(vec![Val::Int(1), Val::Int(2), Val::Int(3)]))
        );
        assert_eq!(
            eval_seq(&mut env, &d, &["(def m {:k 1})", "(get m :k)"]),
            Ok(Val::Int(1))
        );
        assert_eq!(
            eval_seq(&mut env, &d, &["(def zs (list 1 2 3))", "zs"]),
            Ok(Val::List(vec![Val::Int(1), Val::Int(2), Val::Int(3)]))
        );
    }

    // SEMANTIC — a quoted map literal keeps its reader provenance
    // (duplicate-key evidence for later grant validation) through
    // definition storage.
    #[test]
    fn quoted_map_provenance_survives_definition() {
        let mut d = RecordingDispatch::new();
        let mut env = module_env(&mut d);
        eval_seq(&mut env, &d, &["(def form (quote {:a 1 :a 2}))"]).unwrap();
        let Some(Val::Map(m)) = env.get("form").unwrap() else {
            panic!("stored quoted map resolves");
        };
        assert_eq!(
            m.literal_pairs().map(<[_]>::len),
            Some(2),
            "duplicate-key provenance survives define/lookup"
        );
    }

    // SEMANTIC — definition-privilege matrix: every def-family form is
    // denied without top-level privilege, in every callable context.
    #[test]
    fn privilege_matrix_defmacro_and_defn_inside_fn() {
        let mut d = RecordingDispatch::new();
        let mut env = module_env(&mut d);
        // Analyzed pipeline: defmacro inside a fn body.
        let out = eval_seq(
            &mut env,
            &d,
            &[
                "(defn h [] (defmacro mm [] 1))",
                "(try (h) (catch :glia.error/def-not-top-level e :caught))",
            ],
        );
        assert_eq!(out, Ok(Val::Keyword("caught".into())));
        // defn (macro expansion to def) inside a fn body.
        let out = eval_seq(
            &mut env,
            &d,
            &[
                "(defn outer [] (defn inner [] 1))",
                "(try (outer) (catch :glia.error/def-not-top-level e :caught))",
            ],
        );
        assert_eq!(out, Ok(Val::Keyword("caught".into())));
    }

    // SEMANTIC — raw (non-analyzed) pipeline: def inside a raw-constructed
    // closure is denied through the same checked operation.
    #[test]
    fn privilege_matrix_raw_pipeline_def_denied() {
        let mut d = RecordingDispatch::new();
        let mut env = module_env(&mut d);
        // Build (def h (fn [] (defmacro mm [] 1))) as raw Vals — bypasses
        // the analyzer, exercising the raw defmacro path inside a call.
        let def_h = Val::List(vec![
            Val::Sym("def".into()),
            Val::Sym("h".into()),
            Val::List(vec![
                Val::Sym("fn".into()),
                Val::Vector(vec![]),
                Val::List(vec![
                    Val::Sym("defmacro".into()),
                    Val::Sym("mm".into()),
                    Val::Vector(vec![]),
                    Val::Int(1),
                ]),
            ]),
        ]);
        super::tests::eval_raw_blocking(&def_h, &mut env, &d).unwrap();
        let err =
            super::tests::eval_raw_blocking(&Val::List(vec![Val::Sym("h".into())]), &mut env, &d)
                .unwrap_err();
        let payload = super::tests::err_payload(&err);
        assert_eq!(
            crate::error::type_tag(payload),
            Some(crate::error::tag::DEF_NOT_TOP_LEVEL),
            "raw pipeline must deny with the exact stable tag, got: {payload}"
        );
    }

    // SEMANTIC — defcap: allowed at top level; denied inside a function.
    #[test]
    fn privilege_matrix_defcap() {
        let mut d = RecordingDispatch::new();
        let mut env = module_env(&mut d);
        assert!(eval_seq(
            &mut env,
            &d,
            &["(defcap logger :write (fn [message] message))"]
        )
        .is_ok());
        let out = eval_seq(
            &mut env,
            &d,
            &[
                "(defn make-cap-later [] (defcap sneaky :m (fn [x] x)))",
                "(try (make-cap-later) (catch :glia.error/def-not-top-level e :caught))",
            ],
        );
        assert_eq!(out, Ok(Val::Keyword("caught".into())));
    }

    // SEMANTIC — def inside an effect handler is denied; the try-resume /
    // with-effect-handler BODY at top level retains privilege (it is the
    // top-level evaluation), and privilege is intact after resumption.
    #[test]
    fn privilege_matrix_handlers_and_resumption() {
        let mut d = RecordingDispatch::new();
        let mut env = module_env(&mut d);
        // Handler body (a call) cannot define; its throw surfaces at the
        // perform site and is catchable there.
        let out = eval_seq(
            &mut env,
            &d,
            &["(try (with-effect-handler :e (fn [v resume] (def bad 1)) \
                        (perform :e 1)) \
                      (catch :glia.error/def-not-top-level e :caught))"],
        );
        assert_eq!(out, Ok(Val::Keyword("caught".into())));
        assert!(env
            .local_bindings()
            .unwrap()
            .iter()
            .all(|(n, _)| n != "bad"));

        // Resumptive handler: BOTH sides of the suspension in ONE
        // top-level body — a definition before `perform` and one after the
        // resumption, proving privilege is intact across suspend/resume.
        let out = eval_seq(
            &mut env,
            &d,
            &[
                "(with-effect-handler :r (fn [v resume] (resume 10)) \
                   (do (def before 1) (def after (+ (perform :r 1) 1))))",
                "before",
            ],
        );
        assert_eq!(out, Ok(Val::Int(1)));
        assert_eq!(eval_seq(&mut env, &d, &["after"]), Ok(Val::Int(11)));

        // A RESUMED function remains unprivileged: definition attempted
        // after its suspension point still throws.
        let out = eval_seq(
            &mut env,
            &d,
            &[
                "(defn suspended [] (do (perform :r2 1) (def bad2 1)))",
                "(try (with-effect-handler :r2 (fn [v resume] (resume 0)) \
                        (suspended)) \
                      (catch :glia.error/def-not-top-level e :caught))",
            ],
        );
        assert_eq!(out, Ok(Val::Keyword("caught".into())));
        assert!(env
            .local_bindings()
            .unwrap()
            .iter()
            .all(|(n, _)| n != "bad2"));
    }

    // SEMANTIC — def inside a cap method body is denied.
    #[test]
    fn privilege_matrix_cap_method() {
        let mut d = RecordingDispatch::new();
        let mut env = module_env(&mut d);
        let out = eval_seq(
            &mut env,
            &d,
            &[
                "(defcap c :m (fn [x] (def bad 1)))",
                "(try (perform c :m 1) (catch :glia.error/def-not-top-level e :caught))",
            ],
        );
        assert_eq!(out, Ok(Val::Keyword("caught".into())));
    }

    // SEMANTIC — a frozen-owner definition is an evaluator FAULT: wildcard
    // `try` cannot catch it and it reaches the boundary as EvalError::Fault.
    #[test]
    fn frozen_owner_definition_faults_uncatchably() {
        let mut d = RecordingDispatch::new();
        let env_template = module_env(&mut d);
        let prelude = std::rc::Rc::clone(env_template.defs());
        drop(env_template);
        // An env whose OWN owner is frozen (constructed host-side; guests
        // cannot reach this state through the language today).
        let mut env = Env::new();
        env.defs().freeze();
        let _ = prelude;
        let err = eval_seq(&mut env, &d, &["(try (def x 1) (catch _ e :caught))"]).unwrap_err();
        assert!(
            matches!(err, EvalError::Fault(_)),
            "frozen-owner definition must fault past wildcard try, got: {err}"
        );
    }

    // SEMANTIC — named recursion under a pinned 2 MiB stack budget
    // (regression coverage for evaluator frame growth; measurement item
    // from Sol R1 §12 — not TCO, not trampolining).
    #[test]
    fn named_recursion_2mib_stack_budget() {
        // Subprocess-isolated: a Rust stack overflow ABORTS the process
        // rather than unwinding through JoinHandle::join, so the
        // constrained-stack probe runs in a CHILD test process and the
        // parent asserts its exit status. (Deeper WASM measurement stays a
        // Stage G item.)
        const CHILD_FLAG: &str = "GLIA_RECURSION_BUDGET_CHILD";
        if std::env::var(CHILD_FLAG).is_ok() {
            let depth = if cfg!(debug_assertions) { 3 } else { 50 };
            let handle = std::thread::Builder::new()
                .name("glia-recursion-budget".into())
                .stack_size(2 * 1024 * 1024)
                .spawn(move || -> Result<(), String> {
                    // Vals are thread-local (Rc); assert inside and return
                    // a Send-able verdict.
                    let mut d = RecordingDispatch::new();
                    let mut env = module_env(&mut d);
                    match eval_seq(
                        &mut env,
                        &d,
                        &[
                            "(defn down [n] (if (= n 0) 0 (down (- n 1))))",
                            &format!("(down {depth})"),
                        ],
                    ) {
                        Ok(Val::Int(0)) => Ok(()),
                        other => Err(format!("unexpected result: {other:?}")),
                    }
                })
                .expect("spawn budget thread");
            handle
                .join()
                .expect("no overflow")
                .expect("recursion at budget");
            return;
        }
        let exe = std::env::current_exe().expect("test binary path");
        let status = std::process::Command::new(exe)
            .args([
                "--exact",
                "eval::stage_b_semantics::named_recursion_2mib_stack_budget",
                "--test-threads=1",
            ])
            .env(CHILD_FLAG, "1")
            .status()
            .expect("spawn child test process");
        assert!(
            status.success(),
            "recursion-at-budget child process failed (an overflow aborts): {status}"
        );
    }
}

/// Release-checked ownership-fault plumbing tests (Stage C): a GENUINE
/// mismatched resting reference — a callable rested for one owner, planted
/// in a different owner's storage — must surface on the uncatchable fault
/// lane end to end. Never a panic, never a guest exception.
#[cfg(test)]
mod fault_plumbing_tests {
    use super::tests::{eval_str, RecordingDispatch};
    use super::{own, Binding, Env, EvalError};

    /// Build an env whose defs contains a binding holding a callable that
    /// RESTS for a FOREIGN owner (the genuine invariant breach).
    fn poisoned_env() -> Env {
        let mut foreign = Env::new();
        let d = RecordingDispatch::new();
        let f = eval_str("(fn [] 1)", &mut foreign, &d).unwrap();
        let (rested, resting) = own::rest_for(foreign.defs(), &f);
        assert!(resting, "self-owned callable rests for its owner");

        let env = Env::new();
        env.defs().bindings.borrow_mut().insert(
            "poisoned".into(),
            Binding {
                value: rested,
                has_resting_owner_refs: true,
            },
        );
        env
    }

    // RC-MECHANISM: an unmatched resting owner at LOOKUP is a
    // release-checked internal fault — never a panic — with no ownership
    // vocabulary in the fault text.
    #[test]
    fn lookup_unmatched_witness_faults_not_panics() {
        let env = poisoned_env();
        let err = env.get("poisoned").expect_err("must fault, not succeed");
        let text = format!("{}", err.payload());
        assert!(
            !text.contains("weak") && !text.contains("Weak") && !text.contains("resting"),
            "no ownership vocabulary in fault text: {text}"
        );
    }

    // RC-MECHANISM: an unmatched resting owner at EXPORT is the same
    // release-checked fault.
    #[test]
    fn export_unmatched_witness_faults_not_panics() {
        let env = poisoned_env();
        env.local_bindings().expect_err("must fault, not succeed");
    }

    // RC-MECHANISM: the fault flows through evaluation as EvalError::Fault
    // and wildcard `try` CANNOT intercept it.
    #[test]
    fn lookup_fault_is_uncatchable_through_eval() {
        let mut env = poisoned_env();
        let d = RecordingDispatch::new();
        let err = eval_str("(try poisoned (catch _ e :caught))", &mut env, &d)
            .expect_err("fault must escape the wildcard catch");
        assert!(
            matches!(err, EvalError::Fault(_)),
            "expected the uncatchable fault lane, got: {err}"
        );
    }
}
