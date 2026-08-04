All source traced. Here is the raw structured report.

---

# PINNED REVISIONS

- RustPython/RustPython @ `9dff4d3a99cd06cea8062ec8b3bff4cd3f712163` (committed 2026-08-03T07:00:04Z) — verified via `api.github.com/repos/RustPython/RustPython/commits?per_page=1`
- kyren/gc-arena @ `ffc3821d6e525c0f2eab3c9ebf73de228892b188` (committed 2026-07-05, pushed 2026-08-03) — verified same way
- ruffle-rs/ruffle @ `5b6f666567449f02cc4c211ab6dc2b510bc094d0` (committed 2026-08-02T12:46:04Z) — verified same way
- NOTE: production Ruffle does NOT build against gc-arena HEAD; its workspace `Cargo.toml` (line 62) pins `gc-arena = { git = "https://github.com/kyren/gc-arena.git", rev = "75671ae03f53718357b741ed4027560f14e90836", features = ["enum-map", "hashbrown", "indexmap", "slotmap", "smallvec"] }`. I verified `pub unsafe trait Collect<'gc>` exists at that rev too (src/collect.rs line 23), so quotes from HEAD are API-representative.

---

# SYSTEM 1

**System:** RustPython — refcounted Python VM in Rust

**Repository:** https://github.com/RustPython/RustPython

**Revision:** `9dff4d3a99cd06cea8062ec8b3bff4cd3f712163` (2026-08-03). Historical comparison point: tag `2025-11-10-main-55` = `9792001703ae`.

**Files/symbols:** `crates/common/src/refcount.rs` (RefCount), `crates/common/src/atomic.rs` (PyAtomic/Radium), `crates/vm/src/object/core.rs` (PyInner, PyObject, PyObjectRef, PyRef, drop_slow, default_dealloc), `crates/vm/src/object/payload.rs` (PyPayload), `crates/vm/src/object/traverse.rs` (MaybeTraverse/Traverse), `crates/vm/src/object/traverse_object.rs` (PyObjVTable), `crates/vm/src/gc_state.rs` (cycle collector), `crates/vm/src/stdlib/gc.rs` (gc module), `crates/vm/src/builtins/function.rs` (PyFunction).

**Mechanism:** Every Python object is a heap `PyInner<T>` headed by a custom refcount word (`RefCount`: strong+weak counts plus destructed/published/leaked flag bits packed into one usize), a static vtable pointer (`&'static PyObjVTable` with `dealloc`/`trace`/`clear` fn pointers monomorphized per payload type), plus — since 2026 — three GC header fields: `gc_bits: PyAtomic<u8>`, `gc_generation: PyAtomic<u8>`, and intrusive doubly-linked-list `gc_pointers`. `PyObjectRef`/`PyRef<T>` are `repr(transparent)` `NonNull` wrappers whose Clone/Drop do inc/dec; Drop at zero dispatches `drop_slow` → vtable dealloc → `__del__`, weakref clearing, GC-untrack, `tp_clear`-style child extraction, freelist reuse, memory free. Atomicity is compile-time switchable: `PyAtomic<T>` is `radium::Radium` — real atomics with `feature = "threading"`, plain `Cell<T>` without; `Send`/`Sync` for PyObjectRef/PyRef are only implemented under `cfg(feature = "threading")`. Cycles: for ~7 years the `gc` module was a stub (`collect()` returned literal 0) and cycles leaked permanently; a real CPython-style backup cycle collector landed 2026-01-31 (#6910, `gc_state.rs`): 3 generations with thresholds, stop-the-world safepoint barrier, snapshot refcounts, subtract internal edges (via per-type `Traverse`), BFS from externally-referenced survivors, finalize/`__del__` with resurrection detection, then break cycles by `clear`-extracting child refs. Traversal is per-payload: `unsafe trait Traverse { fn traverse(&self, ...); fn clear(&mut self, out: &mut Vec<PyObjectRef>); }` gated by `const HAS_TRAVERSE`/`HAS_CLEAR` compiled into the vtable.

**What problem it solves:** deterministic drops and simple FFI with Rust payloads, while (belatedly) fixing the classic module→function→globals-dict leak that pure refcounting cannot reclaim.

**What Glia could borrow:** (a) the empirical verdict itself — a major Rust runtime shipped Rc-style counting without cycle collection for 7 years, accumulated open leak issues (#2380, #3504), rejected a STW-collector PR (#4180) in 2024, and finally paid for a full traverse/clear machinery anyway — i.e., Glia's option C converges on the same `Traverse` infrastructure option B needs; (b) the vtable-gated `HAS_TRAVERSE`/`HAS_CLEAR` pattern (zero cost for atoms/leaf types; only executable/container types pay); (c) `NEW_REF_UNTRACKED` lazy tracking (frames start untracked, tracked only on escape — direct analogue of Graph 4's weak-while-resting/strong-when-escaped OwnerRef); (d) `clear()` extracting child `PyObjectRef`s into a Vec and dropping them outside locks (mirrors Glia's manual barrier that must break definition-owner↔closure Rc cycles without reentrant drops — see also the deferred-drop queue in refcount.rs); (e) the `leaked`/intern bit for immortal objects.

**What does not transfer:** all atomics/QSBR/published-bit machinery, stop-the-world safepoints, per-generation locks — Glia is single-threaded Rc/RefCell; the freelist husk-reuse; CPython-compat semantics (`__del__` resurrection, gc.garbage).

**Complexity introduced:** every container-ish payload must hand-uphold an unsafe `Traverse` contract ("call traverse_fn at most once per owned ref; DO NOT clone a PyObjectRef in traverse"); dealloc is a 5-stage unsafe pipeline (finalizer, weakrefs, untrack under intrusive-list correctness, clear, freelist aliasing rules); the collector needs a running-frame exclusion hack because frames are real objects; three header bytes + two intrusive pointers added to every object.

**Confidence:** High — all claims quoted from pinned source; history cross-checked via tags and the GitHub commits/issues API.

## Trace answers — RustPython

**1. Object ownership — custom refcount, neither Rc nor Arc.**
https://raw.githubusercontent.com/RustPython/RustPython/9dff4d3a99cd06cea8062ec8b3bff4cd3f712163/crates/common/src/refcount.rs
```rust
/// State layout (usize):
/// 64-bit: [1 bit: destructed] [1 bit: published] [1 bit: leaked] [30 bits: weak_count] [31 bits: strong_count]
pub struct RefCount {
    state: PyAtomic<usize>,
}
```
`inc()` aborts on overflow; `dec()` returns true at zero unless `leaked`; `safe_inc()` is a CAS loop that fails on destructed/zero (used for weak upgrade under free-threading).
https://raw.githubusercontent.com/RustPython/RustPython/9dff4d3a99cd06cea8062ec8b3bff4cd3f712163/crates/vm/src/object/core.rs
```rust
#[repr(C)]
pub(super) struct PyInner<T> {
    pub(super) ref_count: RefCount,
    pub(super) vtable: &'static PyObjVTable,
    /// GC bits for free-threading (like ob_gc_bits)
    pub(super) gc_bits: PyAtomic<u8>,
    pub(super) gc_generation: PyAtomic<u8>,
    /// Intrusive linked list pointers for GC generational tracking
    pub(super) gc_pointers: Pointers<PyObject>,
    pub(super) typ: PyAtomicRef<PyType>, // __class__ member
    pub(super) payload: T,
}
...
#[repr(transparent)]
pub struct PyObjectRef { ptr: NonNull<PyObject> }
...
#[repr(transparent)]
pub struct PyObject(PyInner<Erased>);
...
#[repr(transparent)]
pub struct PyRef<T> { ptr: NonNull<Py<T>> }

impl<T> Drop for PyRef<T> {
    fn drop(&mut self) {
        if self.0.ref_count.dec() {
            unsafe { PyObject::drop_slow(self.ptr.cast::<PyObject>()) }
        }
    }
}
```

**2. Cycle handling — historically a no-op; NOW a real collector.**
Historical stub, still shipping at tag 2025-11-10 (`stdlib/src/gc.rs` @ `9792001703ae`, https://raw.githubusercontent.com/RustPython/RustPython/9792001703ae/stdlib/src/gc.rs):
```rust
#[pyfunction]
fn collect(_args: FuncArgs, _vm: &VirtualMachine) -> i32 { 0 }
#[pyfunction]
fn isenabled(_args: FuncArgs, _vm: &VirtualMachine) -> bool { false }
#[pyfunction]
fn enable(_args: FuncArgs, vm: &VirtualMachine) -> PyResult {
    Err(vm.new_not_implemented_error(""))
}
```
So through late 2025: cycles leaked, gc module lied (`collect() == 0`). Issue-tracker evidence: #2380 "memory leaks" (opened 2020-12, still open), #3504 "Memory leak ?" (opened 2021-12, still open); PR #4180 "(READY FOR REVIEW) Garbage collect: A stop-the-world cycle collector" (opened 2022-09) — closed UNMERGED 2024-02-20 (API: `merged: false`).
Current pinned HEAD: full generational backup collector, `crates/vm/src/gc_state.rs` (first commit `714d1ce58b48`, 2026-01-31, "gc module internal structure and API (#6910)"; 19 commits since). Algorithm (https://raw.githubusercontent.com/RustPython/RustPython/9dff4d3a99cd06cea8062ec8b3bff4cd3f712163/crates/vm/src/gc_state.rs):
```rust
// Step 2: Build gc_refs map (copy reference counts)
// Step 3: Subtract internal references
for child_ptr in referent_ptrs {
    let gc_ptr = GcPtr(child_ptr);
    if collecting.contains(&gc_ptr)
        && let Some(refs) = gc_refs.get_mut(&gc_ptr)
    { *refs = refs.saturating_sub(1); }
}
// Step 4: Find reachable objects (gc_refs > 0) and traverse from them
// Step 5: Find unreachable objects
let unreachable: Vec<GcPtr> = collecting.difference(&reachable).copied().collect();
// Step 6: Finalize unreachable objects and handle resurrection
```
plus a `CollectStopTheWorld` RAII barrier ("parks every other thread for the pointer-reading phases"). The new `stdlib/gc.rs` implements real `collect/get_objects/get_referents/get_referrers/is_tracked/freeze/thresholds`. Auto-collection triggers at bytecode safepoints ("Auto-collection is deferred to a bytecode safepoint (see `maybe_collect`)").

**3. Native payload integration.**
`PyPayload` (https://raw.githubusercontent.com/RustPython/RustPython/9dff4d3a99cd06cea8062ec8b3bff4cd3f712163/crates/vm/src/object/payload.rs):
```rust
pub trait PyPayload: MaybeTraverse + PyThreadingConstraint + Sized + 'static {
    const PAYLOAD_TYPE_ID: core::any::TypeId = core::any::TypeId::of::<Self>();
    fn class(ctx: &Context) -> &'static Py<PyType>;
    const NEW_REF_UNTRACKED: bool = false;
    const HAS_FREELIST: bool = false;
    ...
}
```
The Rust payload lives inline as `PyInner<T>.payload: T`; dict/slots/weakref-list are prefix-allocated *before* the object at negative offsets (`ObjExt`, `ext_ref()` uses `with_exposed_provenance` with negative offset). Dropping is owned by the per-type vtable: `PyObjVTable { typeid, dealloc: unsafe fn(*mut PyObject), debug, trace: Option<...>, clear: Option<...> }` (traverse_object.rs), where `dealloc = default_dealloc::<T>` runs `__del__` → weakrefs → GC-untrack → `clear_fn` edge extraction → freelist-or-free; the payload's normal Rust `Drop` runs when the box is finally destroyed. Native functions are just payloads too (builtin function types implement PyPayload; threading constraint `Send + Sync` only under the feature).

**4. The classic cycle shape exists.**
`crates/vm/src/builtins/function.rs` (https://raw.githubusercontent.com/RustPython/RustPython/9dff4d3a99cd06cea8062ec8b3bff4cd3f712163/crates/vm/src/builtins/function.rs):
```rust
#[pyclass(module = false, name = "function", traverse = "manual")]
pub struct PyFunction {
    code: PyAtomicRef<PyCode>,
    globals: PyDictRef,
    builtins: PyObjectRef,
    pub(crate) closure: Option<PyRef<PyTuple<PyCellRef>>>,
    ...
}
unsafe impl Traverse for PyFunction {
    fn traverse(&self, tracer_fn: &mut TraverseFn<'_>) {
        self.globals.traverse(tracer_fn);
        if let Some(closure) = self.closure.as_ref() { ... }
```
A module's `__dict__` (the globals dict) strongly holds each function; each function strongly holds `globals: PyDictRef` back — exactly the module-dict→function→globals-dict cycle; the `clear` comment confirms CPython semantics: "Note: globals, builtins, code are NOT cleared (required to be non-NULL)". Frames add more cycle shapes; `PyFunction`'s Traverse is exactly what the new collector subtracts over.

**5. Threading/ownership boundary.**
`crates/common/src/atomic.rs`: `pub type PyAtomic<T> = <T as PyAtomicScalar>::Radium;` with
```rust
#[cfg(feature = "threading")]
macro_rules! atomic_ty { ($i:ty, $atomic:ty) => { $atomic }; }
#[cfg(not(feature = "threading"))]
macro_rules! atomic_ty { ($i:ty, $atomic:ty) => { core::cell::Cell<$i> }; }
```
and in object/core.rs:
```rust
cfg_select! {
    feature = "threading" => {
        unsafe impl Send for PyObjectRef {}
        unsafe impl Sync for PyObjectRef {}
    }
    _ => {}
}
```
`crates/vm/Cargo.toml`: `threading = ["rustpython-common/threading"]`; common: `threading = ["parking_lot", "std"]`. So the same codebase is "Rc-like" (Cell counters, no Send/Sync) single-threaded and "Arc-like" multi-threaded; the payload bound flips via `PyThreadingConstraint` (`Send + Sync` vs empty). The 2026 collector additionally carries free-threading gear (gc_bits "like ob_gc_bits", QSBR, published bit) that only exists for the threaded build.

---

# SYSTEM 2

**System:** gc-arena (branded-lifetime incremental tracing GC) as shipped in production by Ruffle

**Repository:** https://github.com/kyren/gc-arena ; production consumer https://github.com/ruffle-rs/ruffle

**Revision:** gc-arena `ffc3821d6e525c0f2eab3c9ebf73de228892b188`; Ruffle `5b6f666567449f02cc4c211ab6dc2b510bc094d0` (Ruffle pins gc-arena git rev `75671ae03f53718357b741ed4027560f14e90836`).

**Files/symbols:** gc-arena: `src/arena.rs` (Arena, Rootable, Rootable!, mutate, collect_debt, MarkedArena), `src/gc.rs` (Gc, Gc::write, is_dead), `src/collect.rs` (Collect/Trace), `src/context.rs` (Mutation, Finalization, Phase, backward/forward_barrier), `src/barrier.rs` (Write, unlock), `src/lock.rs` (Lock/RefLock/GcRefLock), `src/gc_weak.rs` (GcWeak), `src/metrics.rs` (Pacing, allocation_debt), `src/no_drop.rs` + `derive/src/lib.rs` (no_drop rule), `src/static_collect.rs` (Static), `src/collect_impl.rs` (Rc/Arc impls), `src/lib.rs` (`#![no_std]`). Ruffle: `Cargo.toml` (pin), `core/src/player.rs` (GcRoot, GcRootData, GcArena, enter_arena/enter_arena_mut, collect_debt call), `core/src/context.rs` (UpdateContext), `core/common/src/tag_utils.rs` (SwfMovie/SwfSlice), `core/src/display_object/movie_clip.rs`, `core/src/loader.rs`, `core/src/avm2/activation.rs`, `core/src/avm2/object.rs`, `web/Cargo.toml`.

**Mechanism:** One arena = one root type + a `Context`. All GC pointers are `Gc<'gc, T>`: `Copy`, plain `NonNull`, branded by an invariant lifetime (`Invariant<'a> = PhantomData<Cell<&'a ()>>`). Access happens only inside `arena.mutate(|mc: &'gc Mutation<'gc>, root: &'gc Root<'gc, R>| ...)` where `'gc` is a fresh higher-ranked lifetime per call ("generativity"), so `Gc` can never escape, cross arenas, or enter TLS — dangling is impossible by construction, and outside `mutate` the collector knows everything is rooted or garbage. Reachability comes from the unsafe `Collect<'gc>` trait (`trace` must visit every owned Gc/GcWeak); the derive is safe and forces `#[collect(no_drop)]` (conflicting-impl trick `impl<T: Drop> __MustNotImplDrop for T`), `require_static`, or `unsafe_drop`. Interior mutability is fenced: `Cell/RefCell` only implement Collect for `'static` contents; GC-visible mutation goes through `Lock/RefLock` (Cell/RefCell wrappers) unlocked only via `Gc::write(mc, gc) -> &'gc Write<T>`, which fires an explicit backward write barrier ("IF marking AND parent black AND child white, re-gray the parent"); forward barriers also exist. Collection is incremental mark-sweep, driven manually by the embedder between mutations: allocation accrues "debt", `collect_debt()` runs Mark/Sweep in slices per `Pacing { sleep_factor, min_sleep, mark_factor, trace_factor, keep_factor, drop_factor, free_factor }` (default sleep_factor 0.5, min_sleep 256; a STOP_THE_WORLD preset exists). Phases: Sleep → Mark → (Marked; `MarkedArena::finalize` gives a `Finalization<'gc>` where `Gc::is_dead`/`GcWeak::is_dead`/resurrect enable weak-table finalization) → Sweep → Sleep. `GcWeak` is a non-keeping pointer with `upgrade(mc)`. No threads required: crate is `#![no_std]`, no Send/Sync anywhere in the pointer types — hence wasm32 works (Ruffle's whole web build).

**What problem it solves:** safe, zero-per-pointer-overhead cycle-collecting GC for a scripting-language object graph embedded in Rust, with incremental pauses and compile-time-enforced rooting — no dangling, no rooting bugs, no runtime read barriers.

**What Glia could borrow (option B is exactly Ruffle's production shape):** put only the executable graph in the arena — Ruffle's arena holds interpreters, display tree, closures, scope chains (GcRootData), while big immutable data (`SwfMovie { data: Vec<u8>, ... }`) stays in `Arc` outside and is referenced from inside via `#[collect(require_static)]`/`Static` fields (`SwfSlice { pub movie: Arc<SwfMovie>, start, end }` inside GC'd `MovieClipShared`). `Rc<T: Collect>` even implements Collect by tracing through, and `Rc<T: 'static>` fields work via require_static — so Glia's Rust-owned Rc data can sit inside arena objects untraced. The single-root pattern (GcRootData = the "definition owner universe"), `DynamicRootSet` for handles that must live outside the arena (this replaces Graph 4's OwnerRef Strong/Weak escape mechanism wholesale: an escaped callable = a dynamic root; a resting one = an ordinary traced edge; no manual cycle barrier needed at all), `Lock`/`Write` for barrier discipline, `MarkedArena::finalize` + GcWeak for capability-revocation-style weak maps, and debt-based pacing hooked at the REPL/turn boundary (Ruffle: one `self.gc_arena.borrow_mut().collect_debt()` per tick).

**What does not transfer:** the `'gc` brand infects every type and function signature in the interpreter (Ruffle: `Activation<'a, 'gc: 'a>`, `Result<Value<'gc>, Error<'gc>>` everywhere) — retrofitting onto Glia's existing Rc/RefCell `Expr`/`Effect`/`Cell` types is a whole-crate rewrite, not an increment; `Gc` cannot be held across `.await` or stored in capability callbacks — Ruffle's loaders hold `Arc<Mutex<Player>>` + a plain `LoaderHandle` slotmap key and re-enter via `player.lock().unwrap().update(|uc| ...)` for every touch of GC data, and Glia's cap-method graphs crossing async/RPC membranes would need the same handle indirection; per-object Drop that touches other GC objects is forbidden (no_drop), so Drop-based capability revocation patterns must move to finalize-phase sweeps.

**Complexity introduced:** mutator obligations — every mutation of a traced object requires `Gc::write`/`unlock()` (forgetting the barrier is unsafe-only, safe API makes it unrepresentable, but ergonomically every field write becomes `Gc::write(mc, obj).field.unlock().set(...)` or `field!` projections); every type in the graph needs `#[derive(Collect)]` with correct static/traced split; API infection — `&Mutation<'gc>` must thread through every allocating function; unsafe surface — the crate itself is dense unsafe (lifetime transmutes in `Arena::mutate`, `Write::assume`), but consumers stay safe except at async/escape boundaries (Ruffle's `enter_arena` does its own `unsafe { &*(root.deref() as *const _) }` lifetime conflation).

**Confidence:** High — all core claims quoted from pinned files; the one caveat is Ruffle builds a slightly older gc-arena rev (`75671ae`) than gc-arena HEAD, with the trait shape verified identical.

## Trace answers — gc-arena / Ruffle

**1. Arena model / branding.**
https://raw.githubusercontent.com/kyren/gc-arena/ffc3821d6e525c0f2eab3c9ebf73de228892b188/src/arena.rs
```rust
pub trait Rootable<'a> { type Root: ?Sized + 'a; }
...
pub struct Arena<R> where R: for<'a> Rootable<'a> {
    context: Box<Context>,
    root: Root<'static, R>,
}
...
pub fn mutate<F, T>(&self, f: F) -> T
where F: for<'gc> FnOnce(&'gc Mutation<'gc>, &'gc Root<'gc, R>) -> T,
```
Doc comment, same file: "the root type is branded by a unique, invariant lifetime `'gc` which ensures that `Gc` pointers must be contained inside the root object hierarchy and cannot escape the arena callbacks or be smuggled inside another arena."
https://raw.githubusercontent.com/kyren/gc-arena/ffc3821d6e525c0f2eab3c9ebf73de228892b188/src/gc.rs
```rust
pub struct Gc<'gc, T: ?Sized + 'gc> {
    pub(crate) ptr: NonNull<GcBoxInner<T>>,
    pub(crate) _invariant: Invariant<'gc>,
}
impl<'gc, T: ?Sized + 'gc> Copy for Gc<'gc, T> {}
```
`src/types.rs` line 258: `pub(crate) type Invariant<'a> = PhantomData<Cell<&'a ()>>;` (invariance is what makes the brand unforgeable). Allocation only via `Gc::new(mc: &Mutation<'gc>, t: T)`. `Mutation<'gc>` is `#[repr(transparent)]` over `Context` + `Invariant<'gc>` (src/context.rs line 19).

**2. Collect trait / no-Drop rule.**
https://raw.githubusercontent.com/kyren/gc-arena/ffc3821d6e525c0f2eab3c9ebf73de228892b188/src/collect.rs
```rust
///   1. `Collect::trace` *must* trace over *every* `Gc` and `GcWeak` pointer held inside this type.
///   2. Held `Gc` and `GcWeak` pointers must not be accessed inside `Drop::drop` since during drop
///      any such pointer may be dangling.
///   3. Internal mutability *must* not be used to adopt new `Gc` or `GcWeak` pointers without
///      calling appropriate write barrier operations during the same arena mutation.
pub unsafe trait Collect<'gc> {
    const NEEDS_TRACE: bool = true;
    fn trace<T: Trace<'gc>>(&self, cc: &mut T) {}
}
```
Derive enforcement (`src/no_drop.rs` whole file):
```rust
pub trait __MustNotImplDrop {}
#[allow(drop_bounds)]
impl<T: Drop> __MustNotImplDrop for T {}
```
derive/src/lib.rs lines 266-270 emit `gen impl ::gc_arena::__MustNotImplDrop for @Self {}` under `#[collect(no_drop)]`; modes are `require_static`, `no_drop`, `unsafe_drop` ("Such `Drop` impls must *not* access garbage collected pointers during `Drop::drop`").

**3. Write barriers.**
https://raw.githubusercontent.com/kyren/gc-arena/ffc3821d6e525c0f2eab3c9ebf73de228892b188/src/gc.rs
```rust
/// This triggers an unrestricted *backwards* write barrier on this pointer, meaning that it is
/// guaranteed that this pointer can safely adopt *any* arbitrary child pointers (until the next
/// time that collection is triggered).
pub fn write(mc: &Mutation<'gc>, gc: Self) -> &'gc Write<T> {
    unsafe {
        mc.backward_barrier(Gc::erase(gc), None);
        Write::assume(gc.as_ref())
    }
}
```
`src/context.rs` `Mutation::backward_barrier` doc: "IF we are in the marking phase AND the `parent` pointer is colored black AND the `child` (if given) is colored white, then change the `parent` color to gray and enqueue it for tracing." `forward_barrier` is the dual (grays the child). `src/barrier.rs`: `#[repr(transparent)] pub struct Write<T: ?Sized>` with `pub fn unlock(&self) -> &T::Unlocked where T: Unlock` ("a `&Write<T>` implies that a write barrier was triggered on the parent `Gc`"). `src/lock.rs`: `Lock<T>`(Cell) / `RefLock<T>`(RefCell) wrappers, aliases `GcLock`/`GcRefLock`; raw `Cell`/`RefCell` only get Collect when contents are `'static` (src/collect_impl.rs: `unsafe impl<'gc, T> Collect<'gc> for RefCell<T> where T: 'static`). Mutator obligation: every safe mutation path is `Gc::write` (or `field!` projection or `Write::from_mut`) then `unlock()`.

**4. Single root; Ruffle's root.**
https://raw.githubusercontent.com/ruffle-rs/ruffle/5b6f666567449f02cc4c211ab6dc2b510bc094d0/core/src/player.rs
```rust
#[derive(Collect)]
#[collect(no_drop)]
struct GcRoot<'gc> {
    avm2_callstack: GcRefLock<'gc, CallStack<'gc>>,
    data: GcRefLock<'gc, GcRootData<'gc>>,
}
...
#[derive(Collect)]
#[collect(no_drop)]
struct GcRootData<'gc> {
    library: Library<'gc>,
    stage: Stage<'gc>,
    mouse_data: MouseData<'gc>,
    drag_object: Option<DragObject<'gc>>,
    avm1: Avm1<'gc>,
    avm2: Avm2<'gc>,
    action_queue: ActionQueue<'gc>,
    interner: AvmStringInterner<'gc>,
    load_manager: LoadManager<'gc>,
    avm1_shared_objects: HashMap<String, Object<'gc>>,
    avm2_shared_objects: HashMap<String, SharedObjectObject<'gc>>,
    unbound_text_fields: Vec<EditText<'gc>>,
    timers: Timers<'gc>,
    current_context_menu: Option<ContextMenuState<'gc>>,
    external_interface: ExternalInterface<'gc>,
    audio_manager: AudioManager<'gc>,
    stream_manager: StreamManager<'gc>,
    sockets: Sockets<'gc>,
    net_connections: NetConnections<'gc>,
    local_connections: LocalConnections<'gc>,
    orphan_manager: OrphanManager<'gc>,
    /// Dynamic root for allowing handles to GC objects to exist outside of the GC.
    dynamic_root: DynamicRootSet<'gc>,
    post_frame_callbacks: Vec<PostFrameCallback<'gc>>,
}
...
type GcArena = gc_arena::Arena<Rootable![GcRoot<'_>]>;
```
and `Player { ..., gc_arena: Rc<RefCell<GcArena>>, ... }` — production proof of use.

**5. Collection: pacing, phases, finalization, weak, pauses.**
`src/metrics.rs`:
```rust
pub struct Pacing {
    pub sleep_factor: f64,
    pub min_sleep: usize,
    pub mark_factor: f64,
    pub trace_factor: f64,
    pub keep_factor: f64,
    pub drop_factor: f64,
    pub free_factor: f64,
}
pub const DEFAULT: Pacing = Pacing {
    sleep_factor: 0.5, min_sleep: 256,
    mark_factor: 0.1, trace_factor: 0.4, keep_factor: 0.05,
    drop_factor: 0.2, free_factor: 0.3,
};
```
plus `STOP_THE_WORLD` preset (all work factors 0.0). Debt: "Any allocation that occurs during a collection cycle will incur 'debt' ... paid off by running the collection algorithm some amount of time proportional to the debt." `src/context.rs`: `enum Phase { Mark, Sweep, Sleep, Drop }`; `do_collection` loops `mark_one`/`sweep_one` units until debt paid (`RunUntil::PayDebt`) — pause length is embedder-tunable and bounded by debt, not heap size. Arena surface: `collect_debt()`, `mark_debt()`, `finish_marking()`, `cycle_debt()`, `finish_cycle()`; `mark_debt()` returns `MarkedArena` whose `finalize` gives `&Finalization<'gc>` — with `Gc::is_dead` (`matches!(inner.header.color(), GcColor::White | GcColor::WhiteWeak)`), `GcWeak::is_dead`, and resurrection ("Manually marks a dead `Gc` pointer as reachable and keeps it alive"). No per-object finalizer callbacks; value `Drop` runs at sweep but must not touch Gc (rule 2). `GcWeak` (src/gc_weak.rs): `upgrade(self, mc) -> Option<Gc<'gc,T>>`, `is_dropped`, `is_dead(fc)`. Ruffle pumps GC once per frame tick — core/src/player.rs line 2389: `self.gc_arena.borrow_mut().collect_debt();`.

**6. Interop: non-GC data outside, referenced from inside.**
Big immutable buffer outside: `core/common/src/tag_utils.rs`:
```rust
pub struct SwfMovie {
    header: HeaderExt,
    /// Uncompressed SWF data.
    data: Vec<u8>,
    ...
}
pub struct SwfSlice { pub movie: Arc<SwfMovie>, pub start: usize, pub end: usize }
```
Referenced from a traced object: `core/src/display_object/movie_clip.rs`:
```rust
#[derive(Collect)]           // (attribute context from file)
struct MovieClipShared<'gc> {
    cell: RefCell<MovieClipSharedMut>,
    id: CharacterId,
    swf: SwfSlice,                       // Arc<SwfMovie> — untraced, 'static
    #[collect(require_static)]
    preload_progress: PreloadProgress,
    exported_name: Lock<Option<AvmString<'gc>>>,
    avm2_class: Lock<Option<Avm2ClassObject<'gc>>>,
    ...
}
```
Backend handles inside GC'd data: `MovieClipData` has `audio_stream: Cell<Option<SoundInstanceHandle>>` and `#[collect(require_static)] drawing: OnceCell<Box<RefCell<Drawing>>>`. The renderer/audio/etc. live in `Player` (outside): `renderer: Box<dyn RenderBackend>, audio: Box<dyn AudioBackend>, navigator: Box<dyn NavigatorBackend>, ...` and are *lent* into the arena each mutate via `UpdateContext<'gc>` (core/src/context.rs): `pub gc_context: &'gc Mutation<'gc>`, `pub renderer: &'gc mut dyn RenderBackend`, `pub audio: &'gc mut dyn AudioBackend`, `pub root_swf: &'gc mut Arc<SwfMovie>`. Also gc-arena itself: `static_collect!` for primitives/String, `Static<T: 'static>` wrapper, and `unsafe impl<'gc, T> Collect<'gc> for Rc<T> where T: ?Sized + Collect<'gc>` (collect_impl.rs line 225) — Rust-owned data plugs in with `NEEDS_TRACE = false` when `'static`.

**7. Costs.**
Signature infection — core/src/player.rs:
```rust
fn enter_arena_mut<F, T>(&mut self, f: F) -> T
where
    F: for<'gc> FnOnce(&'gc Mutation<'gc>, &'gc mut GcRootData<'gc>, &'gc mut Self) -> T,
```
(and its `enter_arena` sibling contains consumer-side unsafe: "SAFETY: The 'gc lifetime is generative, and can be soundly conflated with the lifetime of shorter borrows"). Every interpreter type is double-lifetimed — core/src/avm2/activation.rs: `pub struct Activation<'a, 'gc: 'a>`; typical method (core/src/avm2/object.rs):
```rust
fn call_property_local(
    self,
    multiname: &Multiname<'gc>,
    arguments: FunctionArgs<'_, 'gc>,
    activation: &mut Activation<'_, 'gc>,
) -> Result<Value<'gc>, Error<'gc>>
```
Also the 23-tuple `GcRootData::update_context_params(&mut self) -> (Stage<'gc>, &mut Library<'gc>, ...)` splitting borrows for UpdateContext. Async boundary — core/src/loader.rs `movie_loader`: the future captures `player: Arc<Mutex<Player>>` and a plain slotmap `LoaderHandle` (`pub struct LoadManager<'gc>(SlotMap<LoaderHandle, MovieLoader<'gc>>)`), and after each `.await` re-enters with `player.lock().unwrap().update(|uc| { let clip = match uc.load_manager.get_loader(handle) { Some(MovieLoader { target_clip, .. }) => *target_clip, ... } })` — no `Gc` ever crosses an await; `DynamicRootSet` in the root is the sanctioned escape hatch ("allowing handles to GC objects to exist outside of the GC").

**8. WASM.**
gc-arena `src/lib.rs` line 1: `#![no_std]` (+ optional `std` feature); no `Send`/`Sync` bounds or thread usage in Gc/Arena/Context (single-threaded by design — context.rs notes the type is `!Sync`). Ruffle's `web/Cargo.toml`: `crate-type = ["cdylib", "rlib"]` with `wasm-bindgen`, `wasm-bindgen-futures`, `tracing-wasm`, depending on `ruffle_core` — the entire arena (GcRoot above) runs in the browser on wasm32-unknown-unknown, which has no threads. Ruffle even special-cases wasm in its own code where needed (cf. RustPython's `elapsed_secs` cfg for wasm32 — same pattern).

---

## Cross-cutting verdict data for Glia

1. **RustPython is the negative result for "Rc without cycle collection" at scale:** 7 years of shipped leaks (open issues #2380, #3504), one rejected collector PR (#4180, closed unmerged 2024), then capitulation in 2026 into a full traverse/clear/generation infrastructure bolted onto refcounting — i.e., option C's end-state contains all of option B's tracing machinery *plus* refcount overhead, STW safepoints, and resurrection semantics.
2. **gc-arena/Ruffle is the positive precedent for option B exactly as scoped:** executable/control graph traced (interpreters, display objects, closures, scope chains), large immutable data Rust-owned outside via `Arc` + `require_static`/`Static`/`Rc` impls, embedder-paced incremental collection at turn boundaries, `DynamicRootSet` subsuming the OwnerRef Strong/Weak escape design, `GcWeak` + finalize for revocation tables. Its price is total: the `'gc` brand rewrites every signature, and async/capability boundaries force handle indirection (Ruffle's `Arc<Mutex<Player>>` + slotmap-key pattern).
3. RustPython's `NEW_REF_UNTRACKED` (lazy GC-tracking on escape, used by frames) is the closest existing analogue to Graph 4's weak-while-resting/strong-when-escaped callables, and its `clear()`-extract-then-drop pattern is the closest analogue to Graph 4's manual cycle barrier.