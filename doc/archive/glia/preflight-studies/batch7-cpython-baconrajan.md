All evidence gathered from pinned revisions. Composing the final report.

---

# SOURCE-FIRST STUDY: PRODUCTION RC CYCLE COLLECTORS

## SYSTEM 1: CPython generational cycle collector

**System:** CPython reference-cycle garbage collector (default GIL build)
**Repository:** https://github.com/python/cpython
**Revision:** PRIMARY pinned `5918085bb6f4a3a48193cacb9bb99b044d4e0452` (committed 2026-08-03T06:52:26Z, HEAD of main). SECONDARY pinned tag `v3.11.9` for the classic pre-reorg `Modules/gcmodule.c` (post-3.12 reorg the collector lives in `Python/gc.c`; `Modules/gcmodule.c` at HEAD is only the 595-line Python-module shim; free-threaded build has separate `Python/gc_free_threading.c`).
**Files/symbols:**
- `Include/internal/pycore_interp_structs.h` — `PyGC_Head`, `struct gc_generation`, `GC_GENERATION_INIT`
- `Include/internal/pycore_gc.h` — `_PyObject_GC_TRACK/_PyObject_GC_UNTRACK/_PyObject_GC_IS_TRACKED`, `_PyGC_PREV_MASK_*`
- `Python/gc.c` (HEAD) / `Modules/gcmodule.c` (v3.11.9) — `update_refs`, `visit_decref`, `subtract_refs`, `move_unreachable`, `deduce_unreachable`, `handle_weakrefs`/`handle_weakref_callbacks`+`clear_weakrefs`, `finalize_garbage`, `handle_resurrected_objects`, `delete_garbage`, `handle_legacy_finalizers`, `gc_collect_main`, `gc_collect_generations`, `gc_freeze_impl`, `_PyObject_GC_Link`
- `Objects/funcobject.c` — `func_traverse`/`func_clear`; `Objects/dictobject.c` — `dict_traverse`/`dict_tp_clear`; `Objects/tupleobject.c` — `_PyTuple_MaybeUntrack`
- `Include/objimpl.h` — `Py_VISIT`; `Doc/c-api/gcsupport.rst`, `Doc/c-api/typeobj.rst`, `InternalDocs/garbage_collector.md`

**Mechanism:** Every collectable object is allocated with a two-word `PyGC_Head` PREPENDED to the PyObject (`gc_alloc` mallocs `presize + basicsize`, object pointer = `mem + presize`). The head is a doubly-linked intrusive list node threading every tracked object into one of 3 generation lists; the low 2 bits of `_gc_prev` are flag bits (FINALIZED, COLLECTING) freed up by alignment, and `_gc_next == 0` means "untracked". Types opt in via `Py_TPFLAGS_HAVE_GC` + a mandatory `tp_traverse` (edge enumerator) and, if mutable, `tp_clear` (edge breaker). Constructors call `PyObject_GC_Track` after fields are initialized; deallocators call `PyObject_GC_UnTrack` before fields are invalidated. Collection is trial deletion over a whole generation: `update_refs` copies the true refcount into the head (reusing `_gc_prev` as scratch, temporarily degrading the list to singly-linked); `subtract_refs` runs every object's `tp_traverse` with `visit_decref`, canceling internal edges; anything with a residual count > 0 is externally referenced; `move_unreachable` partitions with a re-marking fixpoint (`visit_reachable` rescues false negatives). Then: weakrefs to condemned objects are cleared (callbacks of *reachable* weakrefs invoked; condemned ones suppressed), PEP 442 `tp_finalize` runs exactly once per object (FINALIZED bit), resurrection is re-detected by a second full `deduce_unreachable` pass, and finally `delete_garbage` calls each survivor's `tp_clear` to break the cycle and let ordinary refcounting free everything. Legacy `tp_del` cycles are quarantined in `gc.garbage` forever. Trigger: gen0 allocation counter over threshold (700 in 3.11; 2000 at HEAD) — at HEAD it only sets an eval-breaker bit and the collection runs at the next interpreter-loop safepoint. Full collections additionally gated by the 25% `long_lived_pending/long_lived_total` heuristic to avoid quadratic behavior.

**What problem it solves:** exactly Glia's problem — pure refcounting (CPython's primary mechanism) leaks cyclic garbage; the collector finds cycles *within refcounted heaps* without a global mark from roots, by exploiting the identity "external refs = refcount − internal refs", so it never needs to know stack/register roots.

**What Glia could borrow:**
- The trial-deletion identity itself: no root scanning needed — decisive for an embedded Rust interpreter where the "roots" are arbitrary host-side `Val` handles. Anything with residual count > 0 is treated as rooted; host-held `Rc`s are automatically roots.
- The two-function contract (`trace` = enumerate strong edges exactly; `clear` = drop strong edges, object must remain *valid* but not *usable*) — this is precisely what Glia's `Val` enum variants would each need; a Rust enum makes exhaustive `trace` much easier to get right than C.
- Deferred triggering via allocation counter + safepoint bit (`_PyObject_GC_Link` → `_Py_ScheduleGC` → eval-breaker) — maps directly onto Glia's eval loop; never collect mid-primitive.
- "Track late, untrack when provably acyclic" (tuple untracking) — Glia analog: don't register leaf values, atoms holding only scalars, capability objects whose closures capture nothing cyclic.
- `gc.freeze` / permanent generation — natural fit for Glia's immutable initial-authority records and std env: move known-immortal graphs out of every scan.
- The weakref-ordering discipline (clear observers of condemned objects BEFORE running any clear/finalize) if Glia ever adds weak observers.

**What does not transfer:** the header-prepended-by-allocator trick (Rust can't put an intrusive header before an `Rc` allocation you don't control); `_gc_prev`-as-scratch pointer tagging; generational thresholds tuned for CPython allocation rates; legacy-finalizer quarantine (`gc.garbage`); PEP 442 resurrection double-pass is only needed if user-visible finalizers can run arbitrary code — Glia `Drop` impls are Rust-side and can't resurrect (no way to smuggle `&self` out into a new strong ref from safe drop glue, if Glia forbids collector-aware Drop).

**Complexity introduced (per-type obligations, header cost, tuning, bug classes):**
- Per-type: allocate via GC allocator, `tp_traverse`, `tp_clear`, track after init / untrack before teardown, weakref-list handling before clear, finalize-once bit. Six distinct obligations per container type.
- Header: 2 machine words per object, always, plus pre-header (`presize`) alignment; heap layout owned by the GC.
- Tuning: 3 thresholds + 25% full-GC ratio (issue #4074) + per-type untrack heuristics that were *removed* when mutator-side bookkeeping outweighed GC savings (dict lazy tracking removed in 3.14, GH-127010) — evidence that these optimizations are a maintenance treadmill.
- Bug classes with 20+ years of scar tissue: object left tracked during `tp_dealloc` → double-free (update_refs assert comment); missing/incomplete `tp_traverse` → weakref callback runs on cleared objects → crash (bpo-38006, gh-91636); weakref cleared too early breaks type-cache invalidation → segfault (GH-135552); untracked containers participating in cycles → uncollectable leaks; `tp_clear` set that doesn't jointly break every cycle → permanent leak.

**Confidence:** High. All quotes read directly from raw files at the two pinned revisions; line numbers verified locally.

### Numbered trace, with quotes

**1. GC header + track/untrack.**
`pycore_interp_structs.h` L159-169 (https://github.com/python/cpython/blob/5918085bb6f4a3a48193cacb9bb99b044d4e0452/Include/internal/pycore_interp_structs.h#L159):
```c
/* GC information is stored BEFORE the object structure. */
typedef struct {
    // Tagged pointer to next object in the list.
    // 0 means the object is not tracked
    _Py_ALIGNED_DEF(_PyObject_MIN_ALIGNMENT, uintptr_t) _gc_next;

    // Tagged pointer to previous object in the list.
    // Lowest two bits are used for flags documented later.
    // Those bits are made available by the struct's minimum alignment.
    uintptr_t _gc_prev;
} PyGC_Head;
```
Flag bits, `pycore_gc.h` L115-122: `#define _PyGC_PREV_MASK_FINALIZED ((uintptr_t)1)` / `#define _PyGC_PREV_MASK_COLLECTING ((uintptr_t)2)` / `#define _PyGC_PREV_SHIFT 2` / `#define _PyGC_PREV_MASK (((uintptr_t) -1) << _PyGC_PREV_SHIFT)`.
`_PyObject_GC_TRACK`, `pycore_gc.h` L194-235 (https://github.com/python/cpython/blob/5918085bb6f4a3a48193cacb9bb99b044d4e0452/Include/internal/pycore_gc.h#L207):
```c
/* Tell the GC to track this object.
 *
 * The object must not be tracked by the GC.
 *
 * NB: While the object is tracked by the collector, it must be safe to call the
 * ob_traverse method. */
static inline void _PyObject_GC_TRACK(...)
{
    _PyObject_ASSERT_FROM(op, !_PyObject_GC_IS_TRACKED(op),
                          "object already tracked by the garbage collector", ...);
    PyGC_Head *gc = _Py_AS_GC(op);
    _PyObject_ASSERT_FROM(op,
                          (gc->_gc_prev & _PyGC_PREV_MASK_COLLECTING) == 0,
                          "object is in generation which is garbage collected", ...);
    struct _gc_runtime_state *gcstate = &_PyInterpreterState_GET()->gc;
    PyGC_Head *generation0 = gcstate->generation0;
    PyGC_Head *last = (PyGC_Head*)(generation0->_gc_prev);
    _PyGCHead_SET_NEXT(last, gc);
    _PyGCHead_SET_PREV(gc, last);
    _PyGCHead_SET_NEXT(gc, generation0);
    generation0->_gc_prev = (uintptr_t)gc;
    gcstate->heap_size++;
}
```
`_PyObject_GC_UNTRACK` (L247-271) unlinks and does `gc->_gc_next = 0; gc->_gc_prev &= _PyGC_PREV_MASK_FINALIZED;` with the note "This may be called while GC. So _PyGC_PREV_MASK_COLLECTING must be cleared. But _PyGC_PREV_MASK_FINALIZED bit is kept." Tracked test is just `return (gc->_gc_next != 0);` (L73-79). Debug build warns "Object of type %s is not untracked before destruction" in `PyObject_GC_Del` (v3.11.9 gcmodule.c L2349-2358).
Key point confirmed: participation = per-object intrusive header + allocator cooperation + two lifecycle calls placed correctly by every type author.

**2. Edge enumeration: the two-function contract.**
`Py_VISIT` (`Include/objimpl.h` L193-200):
```c
#define Py_VISIT(op)                                                    \
    do {                                                                \
        if (op) {                                                       \
            int vret = visit(_PyObject_CAST(op), arg);                  \
            if (vret)                                                   \
                return vret;                                            \
        }                                                               \
    } while (0)
```
`func_traverse` (`Objects/funcobject.c` L1242-1260, pinned HEAD) — note it visits *every* PyObject field including the closure:
```c
func_traverse(PyObject *self, visitproc visit, void *arg)
{
    PyFunctionObject *f = _PyFunction_CAST(self);
    Py_VISIT(f->func_code);
    Py_VISIT(f->func_globals);
    Py_VISIT(f->func_builtins);
    Py_VISIT(f->func_module);
    Py_VISIT(f->func_defaults);
    Py_VISIT(f->func_kwdefaults);
    Py_VISIT(f->func_doc);
    Py_VISIT(f->func_name);
    Py_VISIT(f->func_dict);
    Py_VISIT(f->func_closure);
    Py_VISIT(f->func_annotations);
    Py_VISIT(f->func_annotate);
    Py_VISIT(f->func_typeparams);
    Py_VISIT(f->func_qualname);
    return 0;
}
```
`func_clear` (L1181-1203) `Py_CLEAR`s the same reference fields (`func_module/defaults/kwdefaults/doc/dict/closure/annotations/annotate/typeparams`, plus decref of globals/builtins). `dict_traverse` (`Objects/dictobject.c` L5088-5118) walks `dk_nentries` visiting `me_value` (and `me_key` for non-unicode tables); `dict_tp_clear` is just `PyDict_Clear(op); return 0;` (L5120-5125).
Doc contract (`Doc/c-api/gcsupport.rst` L26-40): "Constructors for container types must conform to two rules: 1. The memory for the object must be allocated using PyObject_GC_New or PyObject_GC_NewVar. 2. Once all the fields which may contain references to other containers are initialized, it must call PyObject_GC_Track." and symmetrically UnTrack-before-invalidate + GC_Del. `typeobj.rst` L1681-1684: **"Taken together, all tp_clear functions in the system must combine to break all reference cycles. This is subtle, and if in any doubt supply a tp_clear function."** Traversal purity (gcsupport.rst L305-309): "The traversal function must not have any side effects. Implementations may not modify the reference counts of any Python objects nor create or destroy any Python objects, directly or indirectly." Skip rule (L320-326): "The Py_VISIT call may be skipped for those members that provably cannot participate in reference cycles."

**3. Algorithm.** (all from pinned v3.11.9 `Modules/gcmodule.c`; identical structure at HEAD `Python/gc.c`)
`update_refs` L415-443 (https://github.com/python/cpython/blob/v3.11.9/Modules/gcmodule.c#L415):
```c
/* Set all gc_refs = ob_refcnt.  After this, gc_refs is > 0 and
 * PREV_MASK_COLLECTING bit is set for all objects in containers. */
static void
update_refs(PyGC_Head *containers)
{
    PyGC_Head *gc = GC_NEXT(containers);
    for (; gc != containers; gc = GC_NEXT(gc)) {
        gc_reset_refs(gc, Py_REFCNT(FROM_GC(gc)));
        /* Python's cyclic gc should never see an incoming refcount
         * of 0:  if something decref'ed to 0, it should have been
         * deallocated immediately at that time. ... */
        _PyObject_ASSERT(FROM_GC(gc), gc_get_refs(gc) != 0);
    }
}
```
`visit_decref` + `subtract_refs` L446-482:
```c
static int
visit_decref(PyObject *op, void *parent)
{
    ...
    if (_PyObject_IS_GC(op)) {
        PyGC_Head *gc = AS_GC(op);
        /* We're only interested in gc_refs for objects in the
         * generation being collected, which can be recognized
         * because only they have positive gc_refs. */
        if (gc_is_collecting(gc)) {
            gc_decref(gc);
        }
    }
    return 0;
}
/* Subtract internal references from gc_refs.  After this, gc_refs is >= 0
 * for all objects in containers ... The ones with gc_refs > 0 are directly
 * reachable from outside containers, and so can't be collected. */
static void
subtract_refs(PyGC_Head *containers)
{
    traverseproc traverse;
    PyGC_Head *gc = GC_NEXT(containers);
    for (; gc != containers; gc = GC_NEXT(gc)) {
        PyObject *op = FROM_GC(gc);
        traverse = Py_TYPE(op)->tp_traverse;
        (void) traverse(op, (visitproc)visit_decref, op);
    }
}
```
`move_unreachable` L545-629, invariant comment: "Invariants: all objects 'to the left' of us in young are reachable (directly or indirectly) from outside the young list as it was at entry." Zero-count objects are moved to `unreachable` under `NEXT_MASK_UNREACHABLE`; when a later traversal proves one reachable, `visit_reachable` (L485-543) moves it *back*: "This had gc_refs = 0 when move_unreachable got to it, but turns out it's reachable after all. Move it back to move_unreachable's 'young' list, and move_unreachable will eventually get to it again." The `deduce_unreachable` doc (L1064-1090) states the whole 3-step recipe: "1. Copy all reference counts to a different field (gc_prev is used to hold this copy to save memory). 2. Traverse all objects in 'base' and visit all referred objects using 'tp_traverse' and for every visited object, subtract 1 to the reference count... 3. Identify all unreachable objects (the ones with 0 reference count) and move them to the 'unreachable' list." Plus the famous reversal-optimization comment (L1105-1129): "It 'sounds slick' to move the unreachable objects, until you think about it... this dance leaves the objects in order C, B, A... On all _subsequent_ scans, none of them will move... this can save an unbounded number of moves across an unbounded number of later collections."
Ordering in `gc_collect_main` (L1177-1344): merge younger gens → `deduce_unreachable` → `untrack_tuples` (+`untrack_dicts` on full collections only, "to avoid quadratic dict build-up. See issue #14775") → promote reachable to `old` → `move_legacy_finalizers` (tp_del objects + everything reachable from them quarantined) → `handle_weakrefs` → `finalize_garbage` (PEP 442, finalize-once via FINALIZED bit: `if (!_PyGCHead_FINALIZED(gc) && (finalize = Py_TYPE(op)->tp_finalize) != NULL) { _PyGCHead_SET_FINALIZED(gc); Py_INCREF(op); finalize(op); ... }`) → `handle_resurrected_objects` (a *second* `deduce_unreachable` over the condemned set, because finalizers may have resurrected) → `delete_garbage`:
```c
/* Break reference cycles by clearing the containers involved.  This is
 * tricky business as the lists can be changing and we don't know which
 * objects may be freed.  It is possible I screwed something up here. */
static void
delete_garbage(...)
{
    while (!gc_list_is_empty(collectable)) {
        ...
        if ((clear = Py_TYPE(op)->tp_clear) != NULL) {
            Py_INCREF(op);
            (void) clear(op);
            ...
        }
        if (GC_NEXT(collectable) == gc) {
            /* object is still alive, move it, it may die later */
            gc_clear_collecting(gc);
            gc_list_move(gc, old);
        }
    }
}
```
→ `handle_legacy_finalizers` appends `tp_del` cycles to `gc.garbage`: "Handle uncollectable garbage (cycles with tp_del slots, and stuff reachable only from such cycles)... The programmer has to deal with this if they insist on creating this type of structure."

**4. Triggers, freeze, untrack.**
Thresholds v3.11.9 `pycore_runtime_init.h` L56-64: `.gc = { .enabled = 1, .generations = { { .threshold = 700, }, { .threshold = 10, }, { .threshold = 10, }, }, }`. At pinned HEAD (`pycore_interp_structs.h` L277-284) gen0 grew: `.generations = { { .threshold = 2000, }, { .threshold = 10, }, { .threshold = 10, }, }`. Trigger site v3.11.9 (`_PyObject_GC_Link`, L2252-2273): `gcstate->generations[0].count++; if (count > threshold && enabled && threshold && !collecting && !_PyErr_Occurred(tstate)) { gcstate->collecting = 1; gc_collect_generations(tstate); gcstate->collecting = 0; }` — synchronous. At HEAD the same site instead calls `_Py_ScheduleGC(tstate)` which sets `_PY_GC_SCHEDULED_BIT` on the eval breaker; the eval loop later calls `_Py_RunGC` (gc.c L1964-2003) — collection deferred to a safepoint. `gc_collect_generations` picks the *oldest* generation over threshold and applies the full-GC damper (L1414-1454): "we only trigger a full collection if the ratio long_lived_pending / long_lived_total is above a given value (hardwired to 25%)... 'each full garbage collection is more and more costly as the number of objects grows, but we do fewer and fewer of them'" (Martin von Löwis heuristic, issue #4074).
`gc.freeze` (v3.11.9 L1904-1913): `for (int i = 0; i < NUM_GENERATIONS; ++i) { gc_list_merge(GEN_HEAD(gcstate, i), &gcstate->permanent_generation.head); gcstate->generations[i].count = 0; }` — objects parked in a never-scanned permanent generation (designed for pre-fork CoW; equally useful for immortal runtime structure).
Untrack: `_PyTuple_MaybeUntrack` (tupleobject.c L137-157) untracks a tuple during GC iff every element `!_PyObject_GC_MAY_BE_TRACKED(elt)`; pycore_gc.h L275-314 note: "all tuples except the empty tuple are tracked when created. During garbage collection it is determined whether any surviving tuples can be untracked... It may take more than one cycle to untrack a tuple." `InternalDocs/garbage_collector.md` (HEAD) records the reversal for dicts: "Dictionaries are always tracked from creation and are not untracked by the garbage collector. Earlier versions (up to 3.13) used lazy tracking... That machinery was removed in 3.14 (GH-127010) because the per-set-item cost of checking the tracking invariant outweighed the savings on full collections."

**5. Mutator obligations for every new container type** (from gcsupport.rst L15-53 + code): (1) set `Py_TPFLAGS_HAVE_GC`; (2) allocate through `PyObject_GC_New/NewVar` so the header exists; (3) call `PyObject_GC_Track` only after all traversable fields are initialized; (4) implement `tp_traverse` visiting every field that may hold a container, side-effect-free; (5) implement `tp_clear` if mutable, such that all clears jointly break every cycle; (6) in `tp_dealloc`: `PyObject_GC_UnTrack` FIRST ("before any of the fields used by the tp_traverse handler become invalid"), call `PyObject_ClearWeakRefs` if weakref-able, then free with `PyObject_GC_Del`; (7) respect finalize-once if defining `tp_finalize`. Warning quote (gcsupport.rst L42-45): "If a type adds the Py_TPFLAGS_HAVE_GC, then it *must* implement at least a tp_traverse handler or explicitly use one from its subclass or subclasses."

**6. Weakrefs + finalizers.** Classic rule (v3.11.9 handle_weakrefs L742-771): "Clear all weakrefs to the objects in unreachable... we cannot invoke any callbacks until all weakrefs to unreachable objects are cleared, lest the callback resurrect an unreachable object via a still-active weakref." Callback policy (L820-847): if the weakref itself is condemned trash, never call its callback ("It may be catastrophic to call it. If the callback is also in cyclic trash (CT)... the callback could resurrect insane objects"); only callbacks on *externally reachable* weakrefs run. HEAD refined this after GH-135552 (gc.c L768-806): "First, weakrefs to the unreachable set of objects must be cleared before we start calling `tp_clear`. If we don't, those weakrefs can reveal unreachable objects to Python-level code and that is not safe. Many objects are not usable after `tp_clear` has been called and could even cause crashes if accessed (see bpo-38006 and gh-91636 as example bugs)... Now, we delay the clear of weakrefs without callbacks until *after* finalizers have been executed." Resurrection: `finalize_garbage` runs `tp_finalize` once per object, then `handle_resurrected_objects` (L1143-1173) re-runs the entire trial-deletion on the condemned set and merges resurrected objects back into the old generation.

**7. Costs and bug classes — representative quotes.**
- Double-dealloc from staying tracked during teardown (update_refs comment, L424-441): "a tp_dealloc routine left a gc-aware object tracked during its teardown phase... gc can trigger then, and may see the still-tracked dying object... In a release build, an actual double deallocation occurred, which leads to corruption of the allocator's internal bookkeeping pointers."
- Hidden edges (missing tp_traverse) → crash, v3.11.9 L780-789: "One way this can happen is if some container objects do not implement tp_traverse. Then, wr_object can be outside the unreachable set but can be deallocated as a result of breaking the reference cycle. If we don't clear the weakref, the callback will run and potentially cause a crash. See bpo-38006 for one example."
- `delete_garbage`: "This is tricky business as the lists can be changing and we don't know which objects may be freed. It is possible I screwed something up here." (shipped in production for two decades with that comment.)
- Joint-clear obligation: the typeobj.rst "Taken together, all tp_clear functions in the system must combine to break all reference cycles. This is subtle..." — the closest CPython comes to "traverse must expose exactly what clear breaks": traverse under-reporting leaks cycles (or crashes via the bpo-38006 path); clear under-breaking leaks; tuple gets away with no tp_clear only because "it's possible to prove that no reference cycle can be composed entirely of tuples. Therefore the tp_clear functions of other types are responsible for clearing any cycles containing tuples" (typeobj.rst L1684-1687) — i.e., correctness is a *whole-system* proof, not a per-type one.
- Performance bug classes fixed by tuning: quadratic full-GC (#4074 → 25% ratio), quadratic dict untracking (#14775 → full collections only), untrack bookkeeping cost exceeding benefit (GH-127010 → feature deleted).

---

## SYSTEM 2: bacon-rajan-cc

**System:** `bacon_rajan_cc` v0.4.0 — synchronous (stop-the-world, thread-local) Bacon-Rajan 2001 cycle collection over an Rc-like pointer
**Repository:** https://github.com/fitzgen/bacon-rajan-cc
**Revision:** pinned `e8f6ff5f8a54d14767bcd1e36ca71a539791f290` (2023-10-09T19:16:50Z, "Add a function to free dead roots without doing a full cycle collection") — this is HEAD; **last commit is ~2.75 years old as of 2026-08-03**.
**Files/symbols:** `src/lib.rs` (`Cc<T>`, `Weak<T>`, `CcBox`, `CcBoxData`, `Color`, `Drop`/`Clone`/`Deref`), `src/trace.rs` (`Trace`, `Tracer`, ~60 std impls), `src/collect.rs` (`ROOTS`, `add_root`, `collect_cycles`, `mark_roots`/`scan_roots`/`collect_roots`, `free_dead_roots`), `src/cc_box_ptr.rs` (`CcBoxPtr`, `Dropable`, `free`).

**Mechanism:** `Cc<T: Trace>` is a structural clone of `Rc<T>` whose heap box carries two extra fields beyond strong/weak: `buffered: Cell<bool>` and `color: Cell<Color>` (`CcBoxData`, lib.rs L215-222; colors Black/Gray/White/Purple per the paper, lib.rs L191-213). The collector is driven from the pointer's own `Drop`: on decrement, if strong hits 0 → `release()` (drop value now; defer dealloc if buffered); else → `possible_root()` colors the node Purple and pushes a type-erased `NonNull<dyn CcBoxPtr>` into a `thread_local!` `ROOTS` buffer (lib.rs L356-371, collect.rs L16-24). `Clone` recolors Black. Collection is only ever explicit: `collect_cycles() { mark_roots(); scan_roots(); collect_roots(); }` (collect.rs L209-213). `mark_roots` does trial deletion: for each Purple root, `mark_gray` recursively traces the subgraph decrementing every child's strong count once (L224-273). `scan_roots`: nodes still strong>0 get `scan_black` (recolor + re-increment children); strong==0 nodes go White (L278-311). `collect_roots` gathers White nodes into a Vec first — an explicit deviation from the paper: "Collecting the nodes into this Vec is a difference from the original Bacon-Rajan paper. We need this because we have destructors and running them during traversal will cause cycles to be broken which ruins the rest of our traversal" (L319-322) — then pre-compensates counts before dropping ("during trial deletion the reference count was already decremented so we'll end up decrementing twice. To avoid that, we increment the count just before calling drop() so that it balances out. This is another difference from the original paper caused by having destructors that we need to run", L349-353), runs `drop_in_place` on each value, and deallocates in a second pass guarded by weak counts. Edge enumeration is the `Trace` trait: `pub trait Trace { fn trace(&self, tracer: &mut Tracer); }` with `Tracer = dyn FnMut(NonNull<dyn CcBoxPtr>)`; `Cc<T>` traces itself as one edge (`tracer(self._ptr)`), `Weak<T>` traces nothing ("Weak references should not be traced", lib.rs L915-919). `Deref` panics if `strong_count()==0`: "Invalid access during cycle collection" (L530-540) — a runtime guard against touching condemned siblings from `Drop`.

**What problem it solves:** brings CPython-class cycle collection to plain single-threaded Rust `Rc` code with deterministic Drop otherwise intact — the exact "Option C" shape: keep all references strong, buffer possible roots on decrement, collect on demand.

**What Glia could borrow:**
- The whole architectural shape: `Cc<T>` as a drop-in `Rc` replacement inside `Val`'s payloads; `Trace` derived/hand-written per participating payload (closure captures, definition owners, capability method tables, atoms); `collect_cycles()` called from the shell/eval loop at safepoints, plus `number_of_roots_buffered()` as the threshold signal ("Part of choosing a convenient time might be when the number of potential cycle roots reaches some critical threshold", collect.rs L29-36) and `free_dead_roots()` as the cheap non-tracing fast path.
- Root-buffering-on-decrement means only the *suspect* subgraph is traced, not the whole heap — much better fit than CPython's scan-a-generation for a small interpreter heap, and it needs no allocator-owned object list.
- Its destructor adaptations (collect-then-drop Vec; count pre-compensation; weak-guard on dealloc; `Deref` panic guard) are exactly the Rust-specific correctness patches Glia would otherwise rediscover the hard way.
- The `extra_free` regression test (lib.rs L1399-1467) is literally Glia's Graph-4 shape — `struct Env { closures: Vec<Cc<RefCell<Clos>>>, next: Option<Cc<Env>> }` / `struct Clos { env: Cc<Env> }` — with an in-tree comment recording the historical premature-free: "collect_cycles(); // <- incorrectly? frees env_a. mark_gray decrements env_a and does not reinstate (it's the root of the black region). collect_white frees circular_env, which decrements env_a again - to zero and frees it..." Adopt this test verbatim.

**What does not transfer:** no generations, no automatic trigger, no allocation-count integration (all trivial to add on top); no finalizer/resurrection story beyond ordinary `Drop` (fine for Glia, which has no user-level finalizers); the crate's `Trace` impl for `RefCell` panics if the cell is mutably borrowed when collection runs ("We'll panic if we can't borrow. I'm not sure if we have a better option.", trace.rs L254-259) — Glia must guarantee collection only at points where no `RefCell` borrows are live (eval-loop safepoint gives this for free); crate is effectively unmaintained (no commits since 2023-10; version 0.4.0; zero dependencies; README self-describes as "Currently only stop-the-world, not concurrent" and points to a bugzilla link for future incremental work) — treat as reference implementation to study/fork, not a dependency; no known production deployment (README's own "Alternatives" list — rust-gc, shredder, cactusref, etc. — signals experimental ecosystem status).

**Complexity introduced:** per-type: one `Trace` impl (single function, vs CPython's six obligations — no track/untrack because `Cc::new`/`Drop` do it implicitly; no `tp_clear` because collect_roots drops values directly and the panic-guarded `Deref` substitutes for "cleared object validity"). Header: +2 fields per box (`buffered` + `color`, ~2 bytes padded to a word alongside strong/weak) and every `Cc` decrement pays a color/buffer check; potential-root pushes cost a Vec push + a lingering buffer entry until next collect. Trigger tuning is exported to the embedder entirely. Bug classes: forgotten trace edge → uncollected cycle (leak); *over*-traced edge (tracing a non-owned pointer, tracing the same field twice, or tracing through a `Weak`) → double-decrement in `mark_gray` → premature free → UB (the `extra_free`/`test_double_visit_scan_black` history shows even the crate authors hit count-balance bugs in `scan_black`); `Deref` during collection → panic instead of UB (in a member's `Drop`); `RefCell` borrowed at collect time → panic.

**Confidence:** High for mechanism and quotes (entire crate read at pinned revision, it's ~2,500 lines total). Medium on "no production users" (negative claim; based on README, staleness, and absence of any known dependent runtime — not exhaustively verified against crates.io reverse-deps).

### Numbered trace

**1. `Cc<T>` + `Trace`.** lib.rs L286-290: `pub struct Cc<T: 'static + Trace> { _ptr: NonNull<CcBox<T>> }`; box L277-281 `struct CcBox<T: Trace> { value: T, data: CcBoxData }` with `CcBoxData { strong: Cell<usize>, weak: Cell<usize>, buffered: Cell<bool>, color: Cell<Color> }`. trace.rs L14-27 (https://github.com/fitzgen/bacon-rajan-cc/blob/e8f6ff5f8a54d14767bcd1e36ca71a539791f290/src/trace.rs#L20):
```rust
pub type Tracer<'a> = dyn FnMut(NonNull<dyn CcBoxPtr>) + 'a;

/// A trait that informs cycle collector how to find memory that is owned by a
/// `Trace` instance and managed by the cycle collector.
pub trait Trace {
    /// Invoke the `Tracer` on each of the `CcBoxPtr`s owned by this `Trace`
    /// instance.
    ///
    /// Failing to invoke the tracer on every owned `CcBoxPtr` can lead to
    /// leaking cycles.
    fn trace(&self, tracer: &mut Tracer);
}
```
Forgetting a field = **leak, not unsoundness** (under-decrement in `mark_gray` keeps the cycle looking externally referenced → `scan_black` restores it). The unsound direction is *over*-reporting: tracing an edge you don't own (or twice) double-decrements during trial deletion → premature `drop_value`/`free` → UB. Tracing through `Weak` is defined away: `impl<T: Trace> Trace for Weak<T> { fn trace(&self, _tracer: &mut Tracer) { // Weak references should not be traced. } }` (lib.rs L915-919).

**2. Phases + trigger.** Decrement hook, lib.rs L569-580:
```rust
fn drop(&mut self) {
    unsafe {
        if self.data().strong() > 0 {
            self.data().dec_strong();
            if self.data().strong() == 0 {
                self.release();
            } else {
                self.possible_root();
            }
        }
    }
}
```
`possible_root` L356-371: colors Purple, sets `buffered`, `collect::add_root(ptr)` into `thread_local!(static ROOTS: RefCell<Vec<NonNull<dyn CcBoxPtr>>>)`. Collection is **manual only** — `pub fn collect_cycles() { mark_roots(); scan_roots(); collect_roots(); }`; no threshold exists in the crate; docs delegate: "You may wish to do this when the roots buffer reaches a certain size, when memory is low, or at opportune moments within your application" (collect.rs L123-127). `mark_roots` L224-273 (mark_gray: `cc_box_ptr.trace(&mut |t| { t.data().dec_strong(); mark_gray(t); })`; also frees Black strong==0 leftovers in the buffer), `scan_roots` L278-311 (scan/scan_black exactly per paper, with re-increment in scan_black), `collect_roots` L317-377 (two-pass: drop values with count pre-compensation, then dealloc under weak-count guard). `free_dead_roots` L107-121 is the O(buffer) non-tracing purge added in the final commit.

**3. Integration constraints.** RefCell: supported and pervasive in the crate's own tests (`list_cycle`, `self_cycle` build genuine `Cc<RefCell<...>>` cycles and collect them), but `Trace for RefCell` does `self.borrow().trace(tracer)` and will panic on a live mutable borrow — collection must run at borrow-free safepoints. Drop: user `Drop` impls run for cycle members (deterministic Rust drops preserved); during collection a member's `Drop` that derefs a condemned sibling hits the `panic!("Invalid access during cycle collection")` guard rather than UB; `Cc` handles inside dropped values are protected by the `if self.data().strong() > 0` check in `Drop`. Everything is `!Send` (Cell + thread_local) — single-thread by construction; no OS/timing dependencies, so wasm32 compatible as-is. Maintenance: last commit 2023-10-09 (pinned above), v0.4.0, no dependencies; no evidence of production adoption — README positions it among experimental GC crates and its stated future work (incremental collection) never happened.

---

## SYNTHESIS

**(a) Can a cycle collector be retrofitted onto types built on `std::rc::Rc`?** No — both systems show participation is decided *at allocation time* and lives *in the pointer/box*, and each needs one thing `std::rc::Rc` cannot give. CPython's design requires the collector to (i) enumerate every participating object (the intrusive `PyGC_Head` list — "GC information is stored BEFORE the object structure", allocated by `gc_alloc` as `presize + basicsize`) and (ii) read true refcounts (`update_refs`: `gc_reset_refs(gc, Py_REFCNT(...))`). Bacon-Rajan instead requires (iii) a hook on every decrement (`Cc::drop` → `possible_root`) plus per-box `color`/`buffered` state. `Rc` exposes `strong_count` but has no decrement hook, no intrusive enumeration, no spare header bits, and its `RcBox` layout is not extensible. You could approximate a CPython-style collector over raw `Rc` with a side registry of `Weak` handles + a `Trace` trait, but you'd still have to (1) register/unregister every object manually (CPython's track/untrack bug class, now without debug asserts), (2) keep gc_refs scratch in side tables (cache-hostile), and (3) — the real killer — break discovered cycles without `tp_clear`: with plain `Rc` you cannot drop a value that other `Rc`s still point at; you'd need every strong edge behind `RefCell<Option<...>>` so a clear-pass can sever them, which is a bigger type-system perturbation than the weak-at-rest barrier it replaces. Conclusion for Glia Option C: adopt a `Cc`-shaped pointer (fork of bacon-rajan-cc or in-tree equivalent) for the participating payloads; do not attempt to collect over unmodified `std::rc::Rc`. Note the migration is total within the participating object graph: one `Rc` edge hidden inside a `Cc` graph is an untraceable edge (see (b)).

**(b) Can opaque host objects participate without hand-written traverse? What if they hide edges?** They cannot, in either system, and both systems document the failure modes. CPython: a type that stores references but has no `tp_traverse` simply may not be part of cycles; if it is, `subtract_refs` under-subtracts, the cycle appears externally referenced, and it leaks forever — and the failure escalates beyond leaks: the handle_weakrefs comment records crashes, "One way this can happen is if some container objects do not implement tp_traverse. Then, wr_object can be outside the unreachable set but can be deallocated as a result of breaking the reference cycle... the callback will run and potentially cause a crash. See bpo-38006" (v3.11.9 gcmodule.c L780-789); HEAD generalizes: "Normally, the object should also be part of the unreachable set but that's not true in the case of incomplete or missing `tp_traverse` methods" (gc.c L798-805). The doc-level rule is the gcsupport/typeobj pair: traverse must visit every member that may participate ("The Py_VISIT call may be skipped [only] for those members that provably cannot participate in reference cycles") and "all tp_clear functions in the system must combine to break all reference cycles." bacon-rajan is identical in kind: "Failing to invoke the tracer on every owned CcBoxPtr can lead to leaking cycles" — hidden edge = leak (safe); fabricated/duplicated edge = double-decrement = use-after-free (unsafe). For Glia this means: a Rust closure captured in a native fn, or a `dyn Any` payload, that owns `Cc` values must either get a hand-written `trace` (make `Trace` a supertrait of the native-fn/capability object trait so the compiler forces it) or be *structurally barred* from owning participating values (e.g., native closures may only capture through a traced wrapper, or only hold `Weak`). "Trust the host object to not form cycles" is exactly CPython's untracked-container rule, and CPython's history shows it fails silently as leaks and loudly as crashes at the weakref/teardown boundary.

**(c) Single-threaded / WASM compatibility.** Excellent for both models. CPython's collector is stop-the-world under the GIL — "Garbage collection is a 'stop-the-world' operation: even in free threading builds, only one thread state is attached when tp_traverse handlers run" (gcsupport.rst) — and its HEAD trigger (allocation counter → `_Py_ScheduleGC` sets an eval-breaker bit → collect at the next interpreter safepoint) is exactly the pattern a single-threaded WASM-hosted Glia eval loop wants: no signals, no threads, no timers. bacon-rajan-cc is *aggressively* single-threaded (`Cell` counts, `thread_local!` root buffer, `!Send` pointers) and dependency-free; `collect_cycles()` is a plain synchronous call — it compiles and runs on `wasm32-unknown-unknown` semantics with zero adaptation. The only WASM-relevant care point is recursion: both `mark_gray`/`scan`/`collect_white` (bacon-rajan) and `tp_traverse` chains recurse over the object graph on the native stack; deep Glia structures would need an explicit worklist to avoid stack overflow under WASM's fixed stack (CPython bounds this differently — its traversal is iterative over generation lists with only per-object `tp_traverse` calls, one of the quiet benefits of the list-based design).

**Bottom-line cost model for Glia Option C, from the evidence:** per-object: +2 words (CPython header) or +2 fields in the box (bacon-rajan); per-type: 1 mandatory hand-written/derived `Trace` (bacon-rajan) vs 6 lifecycle obligations (CPython) — the Rust shape is materially cheaper because allocation/track/clear collapse into `Cc::new`/`Drop`/direct-drop; per-decrement: color check + possible buffer push (bacon-rajan) vs free (CPython, which pays with whole-generation scans instead); systemic: the correctness obligation is global, not local ("all tp_clear functions in the system must combine..."), hidden edges in host objects are the dominant hazard, and every historical bug in both codebases (bpo-38006, gh-91636, GH-135552, `extra_free`, `test_double_visit_scan_black`) sits at exactly the boundaries Glia has: closures↔environments, weak observers, and teardown-order.