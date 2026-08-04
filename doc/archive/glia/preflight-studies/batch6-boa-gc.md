All source was fetched at a pinned revision and mirrored in a temporary local study directory (not archived) for exact line counts and grep metrics. Raw-link base for every citation below: `https://raw.githubusercontent.com/boa-dev/boa/4fc75c6ae9d85f2b8065c6716f88e9b35318438c/`.

---

System: boa_gc — thread-local, non-moving, stop-the-world mark-sweep GC with ephemeron support and "root counting by subtraction"
Repository: https://github.com/boa-dev/boa (crate `core/gc`, plus derive in `core/macros`, integration in `core/engine`)
Revision: `4fc75c6ae9d85f2b8065c6716f88e9b35318438c` (HEAD of default branch as of fetch on 2026-08-03; commit dated 2026-07-26, "fix: disable comfy-table terminal features (#5462)")
Files/symbols: `core/gc/src/lib.rs` (BoaGc, Allocator, Collector, force_collect), `core/gc/src/trace.rs` (Trace, Finalize, Tracer, empty_trace!, custom_trace!), `core/gc/src/cell.rs` (GcRefCell, BorrowFlag, GcRef/GcRefMut), `core/gc/src/internals/{gc_header.rs,gc_box.rs,vtable.rs,ephemeron_box.rs,weak_map_box.rs}`, `core/gc/src/pointers/{gc.rs,ephemeron.rs,weak.rs,weak_map.rs}`, `core/macros/src/lib.rs` (derive_trace), `core/engine/src/{object/jsobject.rs,value/inner/nan_boxed.rs,native_function/mod.rs,bigint.rs,symbol.rs}`

Mechanism:
Every `Gc::new` boxes a `GcBox<T> { header: GcHeader, vtable: &'static VTable, value: T }` (`repr(C)`) individually on the Rust heap and pushes the type-erased `NonNull<GcBox<NonTraceable>>` into a `thread_local! BOA_GC: RefCell<BoaGc>` registry (`strongs: Vec<GcErasedPointer>`; ephemerons and weak maps in separate vecs). The header packs two `Cell<u32>`s: `ref_count` (incremented by `Gc::new`/`clone`, decremented by `Drop`→`Finalize::finalize`) and `non_root_count` with the mark bit as its top bit. There is NO rooting API and no stack scanning: at collection start, `trace_non_roots` walks every heap object and increments `non_root_count` on every `Gc` handle found *inside* the heap; an object is a root iff `non_root_count < ref_count` (some handle lives outside the heap — Rust stack, Context, VM registers). Mark then BFS-traces from roots through a `Tracer { queue: VecDeque }` (non-recursive, added after a real stack-overflow bug), using per-type static VTables (`trace_fn/trace_non_roots_fn/run_finalizer_fn/drop_fn/type_id/size`) built by a const-generic workaround. Phases: Mark → Finalize unreachables → re-Mark (finalizers may resurrect) → Sweep (retain marked, `drop_fn` unmarked, under a `DropGuard` that makes `finalizer_safe()` false so `Gc::drop` doesn't touch dead pointers). Ephemerons (`WeakGc<T>` = `Ephemeron<T, ()>`) are traced to a fixpoint over a pending list; value traced only if key marked; dead ephemerons get `finalize_and_clear()` so `upgrade()` returns `None`. Collections trigger inside allocation when `bytes_allocated > threshold` (initial 1 MiB, grown to keep usage ≤ 70%). Non-moving, non-generational, non-incremental, single-threaded by construction (`Gc` carries `PhantomData<Rc<T>>`, heap is `thread_local!`).

What problem it solves: true cycle collection for the JS object graph in safe-Rust-hosted code, with zero mutator-side rooting bookkeeping (no handle scopes, no write barriers), weak refs/WeakMap with correct ephemeron semantics, and spec-style finalization — while keeping strings, bigints, symbols, and source text OUTSIDE the traced heap as plain Rc/Arc/interned data.

What Glia could borrow:
- The rooting trick is the headline: `is_rooted() = non_root_count < ref_count`. It composes exactly with an Rc-shaped handle (Gc clone/drop = refcount ops, same as today's `Rc<Defs>`), and eliminates Graph 4's manual Strong/Weak OwnerRef barrier in a stage-2 design — escape analysis becomes automatic: a callable is "escaped" precisely when a handle to it exists outside the traced heap, computed at collect time, not maintained at mutation time. This is the strongest fit for option (C) (Rc + cycle collector) and for option (B)'s root discovery.
- The partition precedent: Boa traces ONLY the potentially-cyclic object graph. `JsString` is a custom refcounted/interned pointer with `empty_trace` (`trace.rs:564-574`), `JsBigInt { inner: Rc<RawBigInt> }` and Arc-based `JsSymbol` are `#[boa_gc(unsafe_empty_trace)]`, and Boa actively moved `CompileTimeEnvironment` (PR #3025) and `SourceText` (PR #4293) from `Gc` to `Rc`. This validates Glia option B's "GC only executable objects, big immutable data stays Rust-owned".
- The derive ergonomics: `#[derive(Trace, Finalize)]` + `#[unsafe_ignore_trace]` escape hatch + the derive force-implements `Drop` (calling `finalize` only when `finalizer_safe()`) so users cannot write a custom Drop that touches dead Gc pointers.
- `Tracer` queue (iterative, no recursion), the Mark→Finalize→re-Mark→Sweep resurrection-safe ordering, and `finalizer_safe()`/`DropGuard` as a cheap sweep-reentrancy guard.
- Ephemeron-based `WeakGc` gives weak refs without a second header word.

What does not transfer:
- The rooting scheme requires that ALL internal references to GC'd objects live inside traced `GcBox`es — it's all-or-nothing over the executable-object graph. A half-converted Glia (some `Rc<Defs>` handles held in untraced Rust structures that are themselves inside the heap conceptually) would misclassify roots. Conversion of Defs/envs/callables to Gc must be total within the traced region.
- `trace_non_roots` + mark + sweep all walk the entire heap every collection — O(live heap) pauses even when garbage is tiny; no incrementality, no generations. Nothing here helps option (D) (BEAM-style per-process generational heaps); the `thread_local!` singleton heap is also the opposite of per-process isolation (Boa issue #5186 flags it as blocking multi-agent support).
- Capability discipline: `Finalize::finalize(&self)` receives full `&self` and may resurrect (the second mark pass exists because of this) — finalizers are authority-bearing and would need restriction in a cap-secure Lisp (a revoked capability graph could self-revive). Also `unsafe trait Trace` puts every hand-written impl in the TCB: "An incorrect implementation of the trait can result in heap overflows, data corruption, use-after-free, or Undefined Behaviour in general."
- The `NativeFunction` closure story is a wart, not a model: safe API only for `Copy` closures; anything capturing must pass captures separately as `T: Trace` through an `unsafe fn`, citing rust-gc#50. Glia's capability method graphs would hit exactly this edge.

Complexity introduced: core crate 3,542 LOC (lib 556 / trace 589 / cell 595 / internals 637 / pointers 1,165, incl. ~150 embedded test lines) + ~20 KB separate `src/test/` + ~145 LOC of proc-macro derive. Unsafe: 178 `unsafe` tokens in core src — 86 `unsafe {}` blocks, 44 `unsafe fn`, 36 `unsafe impl` — and 118 `// SAFETY:` comments; essentially every module except `weak.rs` is unsafe-bearing. Mutator obligations: every field of every heap type must be `Trace` (derive or unsafe manual impl); interior mutability only via `GcRefCell` (whose borrow flag gates tracing); no custom `Drop`; `Gc` deref during sweep is guarded only by `debug_assert!(finalizer_safe())`. Embedder obligations: implement `Trace`+`Finalize` (+`JsData`/`NativeObject` in engine) for native data; closures capturing traced data must use the unsafe captures API. Bug classes that actually hit it: trace-recursion stack overflow (#1848 → fixed by iterative Tracer #3508), root-count overflow/saturation UAF hazard (#4936/#4950/#4951), threshold integer-division truncation (#4701/#4702), finalizer reentrancy panic `BorrowMutError` in JsMap MapLock (#5337), sweep `drop_fn` raw-fn-pointer ACE surface on VTable corruption (#5187), GC allocation hurting startup (#3896); API still churning in 2026 (MutationContext #5458/#5460, `'gc` lifetime #5446).
Confidence: High. All mechanism claims are quoted from source at the pinned sha (using the temporary local mirror noted above); LOC/unsafe counts are exact `wc`/`grep` over that mirror; bug history from GitHub issue-title search (45 hits, sampled 2 in full), so the bug ledger is representative rather than exhaustive.

---

## 1. Managed pointer types

`core/gc/src/pointers/gc.rs` (…/core/gc/src/pointers/gc.rs):

```rust
/// A garbage-collected pointer type over an immutable value.
pub struct Gc<T: Trace + ?Sized + 'static> {
    pub(crate) inner_ptr: NonNull<GcBox<T>>,
    pub(crate) marker: PhantomData<Rc<T>>,   // !Send + !Sync, Rc-variance
}
```

`GcBox` layout, `core/gc/src/internals/gc_box.rs`:

```rust
#[derive(Debug)]
#[repr(C)]
pub struct GcBox<T: Trace + ?Sized + 'static> {
    pub(crate) header: GcHeader,
    pub(crate) vtable: &'static VTable,
    value: T,
}
```

Header = two u32 cells, `core/gc/src/internals/gc_header.rs`:

```rust
const MARK_MASK: u32 = 1 << (u32::BITS - 1);
const NON_ROOTS_MASK: u32 = !MARK_MASK;

pub(crate) struct GcHeader {
    ref_count: Cell<u32>,
    non_root_count: Cell<u32>,   // 31 bits count + top bit = mark flag
}
```

So per-object overhead = 8 bytes header + 8 bytes vtable pointer + Box allocation + one `Vec` slot in the registry. `Gc` is immutable-only (`Deref`, no `DerefMut`); mutation goes through `GcRefCell` (see below). There is also `GcErased` (type-erased `Gc<NonTraceable>` with `type_id()`/`downcast()` via the custom VTable) and `Gc::new_cyclic` (allocates an empty `EphemeronBox`, runs the closure with a dead `WeakGc`, then `set`s key+value — quoted in gc.rs:185-206).

`GcRefCell` (`core/gc/src/cell.rs`) is a RefCell clone with a `Cell<BorrowFlag>` (`WRITING = !0`, `UNUSED = 0`, else reading count); `borrow()`/`borrow_mut()` return `GcRef<'_,T>`/`GcRefMut<'_,T>` guards holding `NonNull<T>` + a flag-restoring `BorrowGcRef` drop guard. Borrow/mutation does NOT touch rooting at all in the current design (older comments about rooting in the SAFETY notes are vestigial) — the flag's only GC interaction is that tracing skips a cell in `Writing` state.

## 2. The tracing API

`core/gc/src/trace.rs`:

```rust
/// # Safety
/// - An incorrect implementation of the trait can result in heap overflows, data corruption,
///   use-after-free, or Undefined Behaviour in general.
/// - Calling any of the functions marked as `unsafe` outside of the context of the garbage collector
///   can result in Undefined Behaviour.
pub unsafe trait Trace: Finalize {
    /// Marks all contained `Gc`s.
    unsafe fn trace(&self, tracer: &mut Tracer);
    /// Trace handles located in GC heap, and mark them as non root.
    unsafe fn trace_non_roots(&self);
    /// Runs [`Finalize::finalize`] on this object and all contained subobjects.
    fn run_finalizer(&self);
}

pub trait Finalize {
    fn finalize(&self) {}
}
```

A hand-written impl must forward all three methods to every field that may transitively contain a `Gc`; missing a field in `trace` = UAF, missing it in `trace_non_roots` = false root (leak is the "safe" failure; the dangerous direction is over-counting, which the header saturates against). Helper macros:

```rust
#[macro_export]
macro_rules! empty_trace {
    () => {
        #[inline] unsafe fn trace(&self, _tracer: &mut $crate::Tracer) {}
        #[inline] unsafe fn trace_non_roots(&self) {}
        #[inline] fn run_finalizer(&self) { $crate::Finalize::finalize(self) }
    };
}
```

`custom_trace!(this, mark, {...})` generates all three methods from one body (trace.rs:125-159). `Trace` for `Gc` itself is the recursion cutoff:

```rust
unsafe impl<T: Trace + ?Sized> Trace for Gc<T> {
    unsafe fn trace(&self, tracer: &mut Tracer) { tracer.enqueue(self.as_erased_pointer()); }
    unsafe fn trace_non_roots(&self) { self.inner().inc_non_root_count(); }
    fn run_finalizer(&self) { Finalize::finalize(self); }
}
```

The derive (`core/macros/src/lib.rs:294-433`) emits the three methods over all fields (skipping `#[unsafe_ignore_trace]` ones), supports `#[boa_gc(empty_trace)]` (requires `Self: Copy`), `unsafe_empty_trace`, `unsafe_no_drop`, and crucially force-implements `Drop`:

```rust
// We also implement drop to prevent unsafe drop implementations on this
// type and encourage people to use Finalize. This implementation will
// call `Finalize::finalize` if it is safe to do so.
fn drop(&mut self) {
    if ::boa_gc::finalizer_safe() { ::boa_gc::Finalize::finalize(self); }
}
```

`Tracer` is a plain `VecDeque<GcErasedPointer>` worklist; `trace_until_empty` pops, skips marked, sets mark bit, calls the vtable `trace_fn` (trace.rs:43-56). Blanket impls exist for primitives, `String`, `str`, `Rc<str>`, fn pointers, tuples ≤12, `Box`, `Vec`, maps/sets, `Cell<T: Default>` (take/mark/set trick), `OnceCell`, `Cow<'static>`, etc.

## 3. Rooting

No explicit roots. Trace the whole path:

- `GcHeader::new()` → `ref_count: 1, non_root_count: 0` (gc_header.rs:22-27).
- `Gc::clone` → `inc_ref_count()`; `Gc::drop` → `if finalizer_safe() { Finalize::finalize(self) }` and `impl Finalize for Gc` is `dec_ref_count()` (gc.rs:320-373). So the refcount is maintained at mutator speed, like `Rc`.
- Each collection begins with `Collector::trace_non_roots(gc)` (lib.rs:280-298): for every object in `strongs`, call its vtable `trace_non_roots_fn`, which recurses through the value and does `inc_non_root_count()` on each embedded `Gc` handle.
- Root test, gc_header.rs:

```rust
/// This only gives valid result if the we have run through the
/// tracing non roots phase.
pub(crate) fn is_rooted(&self) -> bool {
    self.non_root_count() < self.ref_count()
}
```

- `mark_heap` (lib.rs:301-417) enqueues every `is_rooted()` node and drains the tracer; unmarked+unrooted go to `strong_dead`. Sweep resets counts: `node_ref.header.unmark(); node_ref.reset_non_root_count();` (lib.rs:456-458).
- `inc_non_root_count` saturates at `ref_count` and `inc_ref_count` panics above `NON_ROOTS_MAX` — both are explicit UAF guards added after issue #4936/#4950 ("This prevents `is_rooted()` from returning false on live objects, which would cause a UAF", gc_header.rs:43-47).

History: this replaced runtime root/unroot flag maintenance in PR #3109 "Find roots when running GC rather than runtime". So: no header root bit, no root/unroot in Trace, no stack scanning, no handle scopes — the Rust stack and engine structs root objects simply by holding a `Gc` whose refcount exceeds its in-heap reference count.

## 4. Weak references and ephemerons

`WeakGc<T>` is literally an ephemeron with unit value (`pointers/weak.rs`):

```rust
#[derive(Debug, Trace, Finalize)]
#[repr(transparent)]
pub struct WeakGc<T: Trace + ?Sized + 'static> { inner: Ephemeron<T, ()> }
...
pub fn upgrade(&self) -> Option<Gc<T>> { self.inner.key() }
```

`EphemeronBox` (`internals/ephemeron_box.rs`) holds `header: GcHeader` + `UnsafeCell<Option<Data{ key: NonNull<GcBox<K>>, value: V }>>`. `Ephemeron`'s own `Trace::trace` only marks its box, never the key ("we want to stop tracing through weakly held pointers", ephemeron.rs:128-137). During `mark_heap`, `ErasedEphemeronBox::trace` traces the value only if the box is marked AND the key's `GcBox` is marked:

```rust
let is_key_marked = key.is_marked();
if is_key_marked {
    unsafe { data.value.trace(tracer) }
}
is_key_marked
```

Unresolved ephemerons sit in `pending_ephemerons` and the loop in lib.rs:383-403 retries until a fixpoint (correct transitive ephemeron semantics — key-marked-via-another-ephemeron cases converge). Dead ones get `finalize_and_clear()` → `(*self.data.get()).take()`, so surviving `WeakGc` handles remain valid allocations whose `upgrade()` returns `None`; the box itself is swept when its own refcount/marks say so. `upgrade` resurrects safely by `inc_ref_count` on the key then `Gc::from_raw` (ephemeron.rs:56-68). `WeakMap` is a third registry (`weak_maps`) of `WeakMapBox` holding a `WeakGc` to a `GcRefCell<RawWeakMap>`; after sweep, `clear_dead_entries()` drops entries with dead keys (lib.rs:256-273, weak_map_box.rs).

## 5. Finalization

- `Finalize::finalize(&self)` — shared reference, no `&mut`, no consuming. It is the sanctioned Drop replacement; the derive's forced `Drop` calls it only when `finalizer_safe()` (i.e., not during sweep/dump), and `Collector::finalize` calls the vtable `run_finalizer_fn` (which runs `finalize` on the object and recursively on all sub-objects) on every unreachable node *before* sweeping.
- Resurrection is explicitly supported — lib.rs:209-220:

```rust
/// This collector currently functions in four main phases
/// Mark -> Finalize -> Mark -> Sweep
/// 1. Mark nodes as reachable.
/// 2. Finalize the unreachable nodes.
/// 3. Mark again because `Finalize::finalize` can potentially resurrect dead nodes.
/// 4. Sweep and drop all dead nodes.
```

- Order: iteration order of the `strongs` vec (allocation order among the unreachable set), then dead ephemerons; unspecified as a contract.
- Authority: yes, finalizers are arbitrary user code with full `&self` access to captured state and can allocate, clone `Gc`s (resurrect), or touch cells — Boa hit a real crash from exactly that (#5337, MapLock finalizer `BorrowMutError`). For Glia, this is the ambient-authority hazard to design out.

## 6. Host/native integration

`JsObject` (`core/engine/src/object/jsobject.rs:60-84`):

```rust
#[derive(Trace, Finalize)]
#[boa_gc(unsafe_no_drop)]
pub struct JsObject<T: NativeObject = ErasedObjectData> {
    inner: Gc<VTableObject<T>>,
}

#[derive(Trace, Finalize)]
pub(crate) struct VTableObject<T: NativeObject + ?Sized> {
    #[unsafe_ignore_trace]
    vtable: &'static InternalObjectMethods,
    object: GcRefCell<Object<T>>,
}
```

`NativeFunction` (`core/engine/src/native_function/mod.rs`): either a plain fn pointer (untraced) or `Gc<dyn TraceableClosure>`:

```rust
enum Inner {
    PointerFn(NativeFunctionPointer),
    Closure(Gc<dyn TraceableClosure>),
}

#[derive(Trace, Finalize)]
struct Closure<F, T> where F: Fn(&JsValue,&[JsValue],&T,&mut Context)->JsResult<JsValue>, T: Trace {
    #[unsafe_ignore_trace]
    f: F,          // the closure itself is never traced
    captures: T,   // traced captures passed separately
}
```

Safe constructors (`from_copy_closure`, `from_copy_closure_with_captures`) require `F: Copy` so `f` can't smuggle a `Gc`; the general constructors are `unsafe`:

```rust
/// Passing a closure that contains a captured variable that needs to be traced by the garbage
/// collector could cause an use after free, memory corruption or other kinds of **Undefined
/// Behaviour**. See <https://github.com/Manishearth/rust-gc/issues/50> ...
pub unsafe fn from_closure_with_captures<F, T>(closure: F, captures: T) -> Self
```

Embedder obligations: custom object data implements `Finalize + Trace + JsData` (usually via derive); anything capturing engine values in native code must either be `Copy` or route captures through `T: Trace`.

## 7. Collection triggers and phases

Trigger is allocation-time (`Allocator::alloc_gc` → `manage_state`, lib.rs:188-201):

```rust
fn manage_state(gc: &mut BoaGc) {
    if gc.runtime.bytes_allocated > gc.config.threshold {
        Collector::collect(gc);
        if gc.runtime.bytes_allocated > gc.config.threshold / 100 * gc.config.used_space_percentage {
            gc.config.threshold = gc.runtime.bytes_allocated / gc.config.used_space_percentage * 100;
        }
    }
}
```

Defaults: `threshold: 1_048_576` ("Start at 1MB, the nursary size for V8 is ~1-8MB"), `used_space_percentage: 70`. Plus `force_collect()`. Phases per §3/§5: full-heap `trace_non_roots` pass → mark (queue-driven BFS from computed roots, plus the ephemeron fixpoint loop) → finalize unreachables → full re-mark → sweep both vecs (`Vec::retain`, `drop_fn` per dead node, byte accounting) → weak-map entry purge → `shrink_to(len >> 2)` on the registries. Not incremental, not concurrent, not generational; pause is O(live heap + dead set) and every collection pays the O(all objects) non-root counting pass twice-marked in the worst case. `bytes_allocated` is tracked from static `size_of` in the VTable, so it under-counts owned side allocations (a `Vec` inside a GC'd value counts only as 3 words).

## 8. Moving vs non-moving; heap layout; allocation path

Strictly non-moving. There is no heap arena at all: each object is an individual `Box` allocation (`Box::into_raw(Box::new(GcBox{...}))`, lib.rs:131-146), and the "heap" is three `Vec`s of erased pointers (`strongs`, `weaks`, `weak_maps`) in a `thread_local!` singleton (previously an intrusive linked list; changed to `Vec` in PR #3493). Pointers are stable forever, so `&T` from `Gc::deref` and `NonNull<GcBox<K>>` ephemeron keys stay valid; type identity is preserved via the hand-rolled static `VTable` (`internals/vtable.rs`, built with the const-promotion workaround `trait HasVTable { const VTABLE: &'static VTable; ... }` because `GcBox<T>` must be `repr(C)`-castable to `GcBox<NonTraceable>`). No bump allocation, no size classes, no compaction — allocation throughput and locality are exactly `Box::new` + `Vec::push`.

## 9. WASM support

The collector core has zero wasm-specific code. The only wasm cfg in the crate is skipping impls for OS types (trace.rs:219-225):

```rust
#[cfg(not(target_family = "wasm"))]
simple_empty_finalize_trace![std::fs::File, std::fs::FileType, std::net::TcpStream, std::net::UdpSocket];
```

(plus `target_has_atomic` gates for atomics impls). Because the entire heap is `thread_local!` (`BOA_GC`, `GC_DROPPING`) and `Gc` is `!Send`/`!Sync` via `PhantomData<Rc<T>>`, the design is single-threaded by construction and works unchanged on single-threaded wasm32 (Boa ships a wasm playground). The flip side is recorded in issue #5186: supporting ECMAScript shared-memory agents "Requires replacing `thread_local!` GC".

## 10. Cost ledger

- LOC (exact, pinned rev): core src 3,542 lines — lib.rs 556, trace.rs 589, cell.rs 595, internals 637 (gc_header 220 incl. ~85 test lines, gc_box 85, vtable 100, ephemeron_box 181, weak_map_box 39, mod 12), pointers 1,165 (gc 459, weak_map 451, ephemeron 163, weak 78, mod 14). Separate `src/test/` ≈ 20 KB. Derive machinery ≈ 145 lines in boa_macros. Dependencies: just `boa_macros` + `hashbrown` (+ optional trace impls).
- Unsafe: 178 `unsafe` tokens in core src = 86 `unsafe {}` blocks + 44 `unsafe fn` + 36 `unsafe impl`; 118 `// SAFETY:` comments. Highest density: trace.rs (52), lib.rs (36), pointers/gc.rs (24). Only `pointers/weak.rs` and the two `mod.rs` are unsafe-free.
- Bug classes from the tracker (title-search, 45 hits): recursive-trace stack overflow on long reference chains (#1848 → iterative `Tracer`, PR #3508); root/non-root count overflow enabling `is_rooted()==false` on live objects → UAF (#4936, #4950, #4951 — now saturating + panic-on-overflow); integer-division truncation in threshold growth (#4701/#4702); finalizer-vs-borrow reentrancy panic (#5337); sweep executing `drop_fn` through an unprotected raw fn pointer flagged as an ACE surface under VTable corruption (#5187); GC allocation hurting startup time (#3896); plus active architectural churn in 2026 (MutationContext threading #5458/#5460, `'gc` lifetime on core types #5446, public config API #5250/#5251).
- Mutator/embedder obligations: total `Trace` coverage over the traced region, `GcRefCell` for all mutation, no custom `Drop`, unsafe API for non-Copy native closures, and remembering that `Gc::deref` during sweep is only debug-asserted.

## GC-managed vs plain Rust data (JsValue/JsObject boundary)

Default `JsValue` is a 64-bit NaN-boxed word (`core/engine/src/value/inner/nan_boxed.rs`; enum fallback `EnumBasedValue { Null, Undefined, Boolean, Integer32(i32), Float64(f64), BigInt(JsBigInt), Object(JsObject), Symbol(JsSymbol), String(JsString) }` behind feature `jsvalue-enum`). Its `Trace` impl traces exactly one variant:

```rust
unsafe impl Trace for NanBoxedValue {
    custom_trace! {this, mark, {
        if let Some(o) = this.as_object() { mark(&o); }
    }}
}
```

Numbers/bools are inline; `JsString` is a bespoke refcounted/interned pointer (`ptr: NonNull<JsStringVTable>`, static-string support, `Box::leak` for heap strings — `core/string/src/lib.rs`) with `empty_trace`; `JsBigInt { inner: Rc<RawBigInt> }` and Arc-backed `JsSymbol` are `#[boa_gc(unsafe_empty_trace)]`. Only the `JsObject` graph — the thing that can form cycles — lives in the traced heap, and Boa has repeatedly migrated acyclic engine data out of it (`Rc` for CompileTimeEnvironments #3025, SourceText #4293). That is a direct, working precedent for Glia's "large immutable data outside any traced heap" premise.
