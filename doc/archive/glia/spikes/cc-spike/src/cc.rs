//! Collector-aware reference counting with iterative trial-deletion cycle
//! collection (Bacon–Rajan-style; algorithmic reference only, independent
//! implementation).
//!
//! Design decisions (deviations from the sketch, each explained):
//! - `Trace::trace` is FALLIBLE (`Result<(), TraceAbort>`): participating
//!   `RefCell` contents trace via `try_borrow`; a failed borrow aborts the
//!   collection attempt. Combined with PRE-VALIDATION (a read-only borrow
//!   check over the candidate-reachable set BEFORE any count/color
//!   mutation), an abort reclaims nothing and mutates nothing — the
//!   strongest possible rollback: there is nothing to roll back.
//! - `CcBox.value` is `ManuallyDrop<T>`: value destruction and memory
//!   deallocation are separate exactly-once events (`freed` flag guards the
//!   value; deallocation is guarded by `buffered`/queue discipline).
//! - A DESTRUCTION TRAMPOLINE (thread-local queue + pump flag) makes ALL
//!   value destruction iterative: deep chains and SCC teardown never
//!   consume recursive Rust stack.
//! - White destruction uses three balancing steps so cascading `Cc` drops
//!   are exact: (1) EDGE-RESTORE: every white's outgoing edges are
//!   re-incremented (undoing trial subtraction) so the value-drop cascade
//!   decrements are balanced for whites AND for surviving black children;
//!   (2) a VEC-GUARD +1 per white keeps every white ≥ 1 during the
//!   cascade; (3) the final guard release brings each white to exactly 0
//!   for deallocation.
//!
//! No finalizers. No resurrection. No weak handles. No guest-visible state.
//! `Cc` is `!Send + !Sync`. All unsafe code lives in this module.

use std::cell::{Cell, RefCell};
use std::collections::HashSet;
use std::marker::PhantomData;
use std::mem::ManuallyDrop;
use std::ptr::NonNull;
use std::rc::Rc;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Color {
    /// In use (or proven reachable). Acyclic fast path never leaves Black.
    Black,
    /// Possible cycle root (buffered suspect).
    Purple,
    /// Trial-deletion in progress.
    Gray,
    /// Trial-deletion candidate garbage.
    White,
}

/// Returned by `Trace` when a participating `RefCell` is mutably borrowed.
/// Reaching this DURING mutation phases is impossible by construction
/// (pre-validation runs first); reaching it during pre-validation aborts
/// the whole collection with zero side effects.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TraceAbort;

/// SAFETY CONTRACT (normative — see spike report §4):
/// An implementation must enumerate EVERY owned participating edge (each
/// `Cc` handle owned by `self`) EXACTLY ONCE per call, deterministically,
/// and do nothing else: no guest code, no host callbacks, no allocation of
/// participating objects, no cloning of `Cc` handles, no graph mutation,
/// no drops. `RefCell` contents are visited via `try_borrow`, returning
/// `Err(TraceAbort)` on failure. Duplicate edges are a correctness bug
/// (over-subtraction → premature collection) and are detected in debug
/// builds; omitted edges on PARTICIPATING types are a correctness bug that
/// degrades to a leak; deliberately omitted edges are allowed only at the
/// documented opaque-host trust boundary.
pub unsafe trait Trace {
    fn trace(&self, tracer: &mut Tracer) -> Result<(), TraceAbort>;
}

/// Edge sink handed to `Trace` implementations.
pub struct Tracer<'a> {
    sink: &'a mut dyn FnMut(ErasedCc),
    #[cfg(debug_assertions)]
    seen: HashSet<usize>,
}

impl<'a> Tracer<'a> {
    fn new(sink: &'a mut dyn FnMut(ErasedCc)) -> Self {
        Tracer {
            sink,
            #[cfg(debug_assertions)]
            seen: HashSet::new(),
        }
    }

    /// Report one owned participating edge.
    ///
    /// CONTRACT NUANCE (caught by the detector itself during development):
    /// duplication is per-HANDLE, not per-target. Two distinct owned `Cc`
    /// handles referring to the same allocation are two edges — each
    /// contributed +1 to the target and each must subtract once. Only
    /// tracing the SAME handle twice over-subtracts; the debug detector
    /// therefore keys on the handle's address.
    pub fn edge<T: Trace + 'static>(&mut self, c: &Cc<T>) {
        let erased = c.erased();
        #[cfg(debug_assertions)]
        {
            let handle_addr = c as *const Cc<T> as usize;
            assert!(
                self.seen.insert(handle_addr),
                "same trace handle reported twice by one object (correctness \
                 bug: would over-subtract during trial deletion)"
            );
        }
        (self.sink)(erased);
    }
}

struct CcVTable {
    /// Trace the value's edges. SAFETY: ptr must be a live, non-freed
    /// allocation of the vtable's type.
    trace: unsafe fn(ErasedCc, &mut Tracer) -> Result<(), TraceAbort>,
    /// Drop the value in place (exactly once; caller manages `freed`).
    drop_value: unsafe fn(ErasedCc),
    /// Free the allocation (exactly once; value must already be dropped).
    dealloc: unsafe fn(ErasedCc),
}

struct CcMeta {
    strong: Cell<usize>,
    color: Cell<Color>,
    buffered: Cell<bool>,
    freed: Cell<bool>,
    vt: &'static CcVTable,
}

#[repr(C)]
struct CcBox<T> {
    meta: CcMeta,
    value: ManuallyDrop<T>,
}

/// Type-erased handle: `repr(C)` guarantees `meta` at offset 0 for every
/// `T`, so an erased pointer can always reach the metadata; typed access
/// goes through the vtable recorded at allocation.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct ErasedCc(NonNull<CcBox<()>>);

impl ErasedCc {
    fn meta<'a>(&self) -> &'a CcMeta {
        // SAFETY: constructed only from live CcBox allocations; repr(C)
        // puts CcMeta first for every instantiation; deallocation removes
        // every ErasedCc from all collector structures first (queue/buffer
        // discipline below).
        unsafe { &(*self.0.as_ptr()).meta }
    }
    pub fn addr(&self) -> usize {
        self.0.as_ptr() as usize
    }
}

pub struct Cc<T: Trace + 'static> {
    ptr: NonNull<CcBox<T>>,
    _not_send: PhantomData<Rc<T>>,
}

// ── thread-local collector state ──
thread_local! {
    /// Observability: total `trace` invocations (scaling proofs — collection
    /// must scale with unique objects, never path multiplicity).
    static TRACE_CALLS: Cell<usize> = const { Cell::new(0) };
    /// Suspected cycle roots (deduplicated via the `buffered` flag).
    static ROOTS: RefCell<Vec<ErasedCc>> = const { RefCell::new(Vec::new()) };
    /// Destruction trampoline queue + pump flag: all value destruction is
    /// driven from ONE flat loop, so deep chains/SCCs never recurse.
    static DESTROY_QUEUE: RefCell<Vec<ErasedCc>> = const { RefCell::new(Vec::new()) };
    static PUMPING: Cell<bool> = const { Cell::new(false) };
}

impl<T: Trace + 'static> Cc<T> {
    pub fn new(value: T) -> Self {
        let boxed = Box::new(CcBox {
            meta: CcMeta {
                strong: Cell::new(1),
                color: Cell::new(Color::Black),
                buffered: Cell::new(false),
                freed: Cell::new(false),
                vt: Self::vtable(),
            },
            value: ManuallyDrop::new(value),
        });
        Cc {
            // SAFETY: Box::into_raw is non-null.
            ptr: unsafe { NonNull::new_unchecked(Box::into_raw(boxed)) },
            _not_send: PhantomData,
        }
    }

    fn vtable() -> &'static CcVTable {
        trait HasVt: Trace + Sized + 'static {
            const VT: CcVTable = CcVTable {
                trace: |e, tracer| {
                    // SAFETY (vtable dispatch): `e` was created from a
                    // `CcBox<Self>` — the vtable pointer is written exactly
                    // once at allocation from `Self::vtable()`, so the cast
                    // recovers the true type. Value not freed: callers gate
                    // on `!freed`. The shared place is projected to the
                    // value field only (whole-box references would interact
                    // with meta borrows).
                    let b = e.0.as_ptr() as *const CcBox<Self>;
                    let v: &ManuallyDrop<Self> = unsafe { &(*b).value };
                    v.trace(tracer)
                },
                drop_value: |e| {
                    // SAFETY: same provenance/type argument as `trace`;
                    // called exactly once per allocation (guarded by the
                    // `freed` flag at every call site). The mutable place
                    // is projected to the VALUE FIELD ONLY through the raw
                    // pointer — materializing `&mut CcBox` here would
                    // invalidate live `&CcMeta` references held by callers
                    // (pump/dec), which Miri flags as UB.
                    let b = e.0.as_ptr() as *mut CcBox<Self>;
                    unsafe { ManuallyDrop::drop(&mut (*b).value) };
                },
                dealloc: |e| {
                    // SAFETY: value already dropped (freed=true), no
                    // remaining collector references (queue/buffer purged),
                    // allocation created by Box::new in Cc::new — so
                    // reconstituting the Box frees it exactly once.
                    drop(unsafe { Box::from_raw(e.0.as_ptr() as *mut CcBox<Self>) });
                },
            };
        }
        impl<T: Trace + Sized + 'static> HasVt for T {}
        &<T as HasVt>::VT
    }

    fn erased(&self) -> ErasedCc {
        ErasedCc(self.ptr.cast())
    }

    /// Stable identity for the lifetime of the allocation (non-moving).
    pub fn ptr_id(&self) -> usize {
        self.ptr.as_ptr() as usize
    }

    pub fn ptr_eq(a: &Self, b: &Self) -> bool {
        a.ptr == b.ptr
    }

    pub fn strong_count(&self) -> usize {
        self.erased().meta().strong.get()
    }
}

impl<T: Trace + 'static> std::ops::Deref for Cc<T> {
    type Target = T;
    fn deref(&self) -> &T {
        // SAFETY: a live `Cc` handle implies strong ≥ 1 and value not
        // freed (freed only ever becomes true when strong reaches 0 or
        // the object was proven unreachable garbage, in which case no
        // `Cc` handle exists).
        unsafe { &(*self.ptr.as_ptr()).value }
    }
}

impl<T: Trace + 'static> Clone for Cc<T> {
    fn clone(&self) -> Self {
        let e = self.erased();
        let m = e.meta();
        // DELIBERATE DEVIATION from Bacon–Rajan: increment does NOT
        // re-blacken. Blacken-on-increment is a candidate-pruning
        // optimization in the paper, not a soundness requirement — a live
        // object left Purple/buffered is simply re-proven reachable by
        // mark/scan (scan_black restores it). The trade buys a count-only
        // clone hot path (T1); the cost is bounded wasted trial work for
        // hot objects, once per collection, deduplicated by `buffered`.
        m.strong.set(m.strong.get() + 1);
        Cc {
            ptr: self.ptr,
            _not_send: PhantomData,
        }
    }
}

impl<T: Trace + 'static> Drop for Cc<T> {
    fn drop(&mut self) {
        dec(self.erased());
    }
}

/// Decrement: zero → enqueue for flat destruction; nonzero → buffer as a
/// possible cycle root (unless its value is already destroyed).
fn dec(e: ErasedCc) {
    let m = e.meta();
    let s = m.strong.get();
    debug_assert!(s > 0, "decrement below zero");
    m.strong.set(s - 1);
    if s - 1 == 0 {
        enqueue_release(e);
        pump();
    } else if !m.freed.get() && !m.buffered.get() {
        // First suspicion buffers the object; further decrements are
        // count-only (dedup by `buffered`). Color is written once here.
        m.color.set(Color::Purple);
        m.buffered.set(true);
        ROOTS.with(|r| r.borrow_mut().push(e));
    }
}

fn enqueue_release(e: ErasedCc) {
    DESTROY_QUEUE.with(|q| q.borrow_mut().push(e));
}

/// The single flat destruction loop. Value drops cascade further `dec`s;
/// zeros re-enter the queue instead of recursing.
fn pump() {
    if PUMPING.with(|p| p.get()) {
        return;
    }
    PUMPING.with(|p| p.set(true));
    loop {
        let next = DESTROY_QUEUE.with(|q| q.borrow_mut().pop());
        let Some(e) = next else { break };
        let m = e.meta();
        debug_assert_eq!(m.strong.get(), 0, "release with live references");
        if !m.freed.get() {
            m.freed.set(true);
            // SAFETY: strong == 0, not yet freed → exactly-once drop.
            unsafe { (m.vt.drop_value)(e) };
        }
        // Re-derive after the value drop: no reference outlives a vtable
        // call that mutates the allocation.
        let m = e.meta();
        if !m.buffered.get() {
            // SAFETY: value dropped, not referenced by the roots buffer,
            // queue entry consumed → exactly-once deallocation.
            unsafe { (m.vt.dealloc)(e) };
        }
        // else: the roots buffer still references this allocation; the
        // next collection's purge deallocates it.
    }
    PUMPING.with(|p| p.set(false));
}

/// Trace an object's edges into a callback. Returns Err only on borrow
/// failure (participating RefCell mutably borrowed).
fn trace_edges(e: ErasedCc, f: &mut dyn FnMut(ErasedCc)) -> Result<(), TraceAbort> {
    TRACE_CALLS.with(|c| c.set(c.get() + 1));
    let m = e.meta();
    debug_assert!(!m.freed.get(), "tracing a destroyed value");
    let mut tracer = Tracer::new(f);
    // SAFETY: not freed (asserted); vtable matches allocation type.
    unsafe { (m.vt.trace)(e, &mut tracer) }
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct CollectStats {
    pub candidates: usize,
    pub collected: usize,
    pub aborted: bool,
}

/// Cycle collection at an explicit safepoint. Iterative trial deletion:
/// purge → pre-validate (borrow check, NO mutation) → mark(subtract) →
/// scan(restore/condemn) → destroy whites (edge-restore, guard, drop,
/// release).
pub fn collect_cycles() -> CollectStats {
    let mut stats = CollectStats::default();

    // Phase 0a: take the buffer; purge dead/stale entries.
    let taken: Vec<ErasedCc> = ROOTS.with(|r| std::mem::take(&mut *r.borrow_mut()));
    let mut roots: Vec<ErasedCc> = Vec::with_capacity(taken.len());
    for e in taken {
        let m = e.meta();
        m.buffered.set(false);
        if m.freed.get() {
            // Value already destroyed; the buffer held the allocation.
            debug_assert_eq!(m.strong.get(), 0);
            // SAFETY: freed, unbuffered, out of every collector structure.
            unsafe { (m.vt.dealloc)(e) };
        } else if m.color.get() == Color::Purple && m.strong.get() > 0 {
            roots.push(e);
        }
        // Black/re-incremented entries: simply unbuffered.
    }
    stats.candidates = roots.len();
    if roots.is_empty() {
        return stats;
    }

    // Phase 0b: PRE-VALIDATION — walk everything reachable from the
    // candidates and confirm every participating object is traceable
    // (no live mutable borrows). NOTHING has been mutated yet, so a
    // failure aborts with zero side effects and full future correctness.
    {
        let mut visited: HashSet<usize> = HashSet::new();
        let mut stack: Vec<ErasedCc> = roots.clone();
        while let Some(e) = stack.pop() {
            if !visited.insert(e.addr()) {
                continue;
            }
            let mut push = |c: ErasedCc| stack.push(c);
            if trace_edges(e, &mut push).is_err() {
                // Abort: NOTHING has been mutated. Re-buffer the candidates
                // FIRST so that even the debug-mode loud failure below
                // unwinds out of a fully valid collector state.
                for r in roots {
                    let m = r.meta();
                    if !m.buffered.get() {
                        m.buffered.set(true);
                        ROOTS.with(|b| b.borrow_mut().push(r));
                    }
                }
                stats.aborted = true;
                // Debug: fail loudly (a non-safepoint collection is a
                // caller bug). Release: return the abort safely.
                debug_assert!(
                    stats.aborted && false,
                    "collect_cycles at a non-safepoint (live borrow)"
                );
                return stats;
            }
        }
    }

    // Phase 1: MARK — iterative gray marking with trial subtraction.
    // Each edge (parent → child) decrements the child exactly once.
    for &r in &roots {
        if r.meta().color.get() != Color::Gray {
            let mut stack = vec![r];
            while let Some(e) = stack.pop() {
                let m = e.meta();
                if m.color.get() == Color::Gray {
                    continue;
                }
                m.color.set(Color::Gray);
                let mut on_child = |c: ErasedCc| {
                    let cm = c.meta();
                    cm.strong.set(cm.strong.get() - 1);
                    stack.push(c);
                };
                // Pre-validated: cannot fail.
                let _ = trace_edges(e, &mut on_child);
            }
        }
    }

    // Phase 2: SCAN — externally referenced grays (strong > 0) re-blacken
    // and RESTORE their outgoing edge counts; the rest condemn to White.
    for &r in &roots {
        let mut stack = vec![r];
        while let Some(e) = stack.pop() {
            let m = e.meta();
            match m.color.get() {
                Color::Gray => {
                    if m.strong.get() > 0 {
                        scan_black(e);
                    } else {
                        m.color.set(Color::White);
                        let mut push = |c: ErasedCc| stack.push(c);
                        let _ = trace_edges(e, &mut push);
                    }
                }
                _ => {}
            }
        }
    }

    // Phase 3: gather whites (dedup by flipping to Black on first visit).
    let mut whites: Vec<ErasedCc> = Vec::new();
    for &r in &roots {
        let mut stack = vec![r];
        while let Some(e) = stack.pop() {
            let m = e.meta();
            if m.color.get() == Color::White {
                m.color.set(Color::Black);
                whites.push(e);
                let mut push = |c: ErasedCc| stack.push(c);
                let _ = trace_edges(e, &mut push);
            }
        }
    }
    stats.collected = whites.len();
    if whites.is_empty() {
        return stats;
    }

    // Phase 4: DESTROY.
    // 4a EDGE-RESTORE: undo trial subtraction for every edge ORIGINATING
    //    from a white, so the coming value-drop cascade decrements are
    //    exactly balanced (for white AND surviving black children).
    for &w in &whites {
        let mut bump = |c: ErasedCc| {
            let cm = c.meta();
            cm.strong.set(cm.strong.get() + 1);
        };
        let _ = trace_edges(w, &mut bump);
    }
    // 4b VEC-GUARD: keep every white ≥ 1 while values drop.
    for &w in &whites {
        let m = w.meta();
        m.strong.set(m.strong.get() + 1);
        // Mark destroyed BEFORE any value drop so cascade decrements never
        // re-buffer a dying white as a new suspect.
        m.freed.set(true);
    }
    // 4c VALUE DROPS: flat — cascades pump through the destruction queue.
    PUMPING.with(|p| p.set(true)); // suppress nested pumping…
    for &w in &whites {
        // SAFETY: freed was just set by us and the value not yet dropped
        // (whites were live before this phase); exactly-once by phase
        // structure.
        unsafe { (w.meta().vt.drop_value)(w) };
    }
    // 4d GUARD RELEASE: each white returns to exactly 0 and deallocates
    //    through the uniform release path.
    for &w in &whites {
        let m = w.meta();
        let s = m.strong.get();
        debug_assert!(s >= 1, "white lost its guard");
        m.strong.set(s - 1);
        if s - 1 == 0 {
            enqueue_release(w);
        } else {
            // Still guarded by a queued cascade release; it will reach 0
            // through the queue.
            debug_assert!(false, "white above guard after balanced cascade");
        }
    }
    PUMPING.with(|p| p.set(false));
    pump(); // …and drain everything in one flat loop.

    stats
}

/// Iterative scan_black: re-blacken and restore child counts.
fn scan_black(start: ErasedCc) {
    let mut stack = vec![start];
    while let Some(e) = stack.pop() {
        let m = e.meta();
        if m.color.get() == Color::Black {
            continue;
        }
        m.color.set(Color::Black);
        let mut on_child = |c: ErasedCc| {
            let cm = c.meta();
            cm.strong.set(cm.strong.get() + 1);
            if cm.color.get() != Color::Black {
                stack.push(c);
            }
        };
        let _ = trace_edges(e, &mut on_child);
    }
}

/// Number of buffered suspects (threshold trigger input; test observability).
pub fn suspects() -> usize {
    ROOTS.with(|r| r.borrow().len())
}

/// Explicit safepoint helper: collect when the suspect buffer exceeds a
/// threshold (production intent: called at evaluator turn boundaries).
pub fn maybe_collect(threshold: usize) -> Option<CollectStats> {
    if suspects() >= threshold {
        Some(collect_cycles())
    } else {
        None
    }
}

/// Observability for scaling proofs.
pub fn trace_calls() -> usize {
    TRACE_CALLS.with(|c| c.get())
}
