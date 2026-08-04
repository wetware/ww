All four systems traced from pinned source. Below are the records.

---

# SOURCE-FIRST IMPLEMENTATION STUDY — Steel / Rhai / Rune / Gluon

Pinned revisions (via `api.github.com/repos/<r>/commits?per_page=1`, retrieved 2026-08-03):
- Steel `mattwparas/steel` @ `3a418c9ea586c1862a8c3a49d6a998436afc8957` (2026-07-18)
- Rhai `rhaiscript/rhai` @ `950b724b8f1db8404588d6b0f398878b2c91f8ec` (2026-07-18)
- Rune `rune-rs/rune` @ `54fe7cb8eaa3603932a09f678af9d07f39a8d796` (2026-07-29)
- Gluon `gluon-lang/gluon` @ `418c6b7de22b244746bfd0570f9fcfd6d738e542` (2026-07-10)

---

## STEEL

### Record S1 — Closure representation and capture-by-snapshot

System: Steel
Repository: https://github.com/mattwparas/steel
Repository status: active (last commit 2026-07-18)
Revision or commit: 3a418c9ea586c1862a8c3a49d6a998436afc8957
Files and symbols: `crates/steel-core/src/values/functions.rs` (`ByteCodeLambda`, `captures: CaptureVec`, `set_captures`), `crates/steel-core/src/steel_vm/vm.rs` (`handle_new_start_closure`, `OpCode::COPYCAPTURESTACK`, `OpCode::COPYCAPTURECLOSURE`), `crates/steel-core/src/rvals.rs` (`SteelVal::Closure(Gc<ByteCodeLambda>)`)
Runtime path traced: `NEWSCLOSURE` opcode → `handle_new_start_closure(offset)` → per-capture instruction loop → clone prototype from `function_interner.closure_interner` → `prototype.set_captures(captures)`.
Observed implementation (short quotes):
```rust
pub struct ByteCodeLambda {
    pub(crate) id: u32,
    pub(crate) body_exp: StandardShared<[DenseInstruction]>,
    pub(crate) arity: u16,
    pub(crate) is_multi_arity: bool,
    pub(crate) captures: CaptureVec,
    ...
}
```
```rust
(OpCode::COPYCAPTURESTACK, n) => {
    let value = self.thread.stack[n.to_usize() + offset].clone();
    captures.push(value);
}
(OpCode::COPYCAPTURECLOSURE, n) => {
    let value = guard.function.captures()[n.to_usize()].clone();
    captures.push(value);
}
```
Semantic intent from docs: source comments only — "body of the function with identifiers yet to be bound"; capture instructions patch a shared interned prototype per instantiation.
What is verified: captures are a flat vector of `SteelVal` values copied at closure-creation time, either from the stack or from the enclosing closure's own capture vector (flat closure conversion, no environment chain). Closure = `Gc<ByteCodeLambda>` (Rc-family, see S3).
What is inferred: the compiler resolves capture indices statically (only `COPYCAPTURESTACK`/`COPYCAPTURECLOSURE` reach the VM; anything else panics: "Something went wrong in closure construction!").
Relevant invariant: a closure never holds a live scope; it holds a value snapshot; immutable captured values are structurally shared via Rc clone.
Consequence for Glia: direct precedent for "closures capture lexical values by snapshot" in an Rc-based Lisp with no environment chain — including the two-source copy (stack vs enclosing captures), matching Glia's centralized rest/escape rewriting shape.
Confidence: high
Classification: CONFIRMS CURRENT DESIGN

### Record S2 — Flat global env, per-call late binding

System: Steel
Repository: https://github.com/mattwparas/steel
Repository status: active
Revision or commit: 3a418c9ea586c1862a8c3a49d6a998436afc8957
Files and symbols: `crates/steel-core/src/env.rs` (`Env`, `repl_lookup_idx`, `repl_define_idx`), `crates/steel-core/src/steel_vm/vm.rs` (`handle_call_global`), `crates/steel-core/src/values/closed.rs` (`GlobalSlotRecycler`)
Runtime path traced: `CALLGLOBAL` opcode → `handle_call_global(index, payload)` → `global_env.repl_lookup_idx(index)` → dispatch.
Observed implementation (short quotes):
```rust
pub struct Env {
    #[cfg(not(feature = "sync"))]
    pub(crate) bindings_vec: Vec<SteelVal>,
    ...
}
pub fn repl_lookup_idx(&self, idx: usize) -> SteelVal {
    self.bindings_vec[idx].clone()
}
```
```rust
fn handle_call_global(&mut self, index: usize, payload_size: usize) -> Result<()> {
    let func = self.thread.global_env.repl_lookup_idx(index);
    self.handle_global_function_call(func, payload_size)
}
```
Semantic intent from docs: comment in `handle_call_global`: "TODO: Lazily fetch the function. Avoid cloning where relevant." `GlobalSlotRecycler` (closed.rs:72) reclaims dead global slots.
What is verified: globals are raw values in a flat index-addressed vector; every global call re-reads the slot → top-level redefinition takes effect on the next call (late binding). `repl_define_idx` overwrites the slot in place.
What is inferred: recursion at top level works purely through this slot re-read (a self-call compiles to `CALLGLOBAL` of its own slot); no self-capture needed.
Relevant invariant: global slot identity (index) is stable; the value in it is replaceable; captured copies of old values are unaffected by redefinition.
Consequence for Glia: confirms `Defs` as name→raw-Val owner with per-call late lookup at top level, and confirms that raw values (not cells) suffice for top-level late binding when calls go through the definition owner. Also gives an answer to top-level recursion without cells.
Confidence: high
Classification: CONFIRMS CURRENT DESIGN

### Record S3 — Rc values + mark&sweep heap of mutable cells; weak user handles

System: Steel
Repository: https://github.com/mattwparas/steel
Repository status: active
Revision or commit: 3a418c9ea586c1862a8c3a49d6a998436afc8957
Files and symbols: `crates/steel-core/src/gc.rs` (`pub struct Gc<T: ?Sized + 'static>(pub(crate) Shared<T>)`, `pub type Shared<T> = Rc<T>`), `crates/steel-core/src/values/closed.rs` (`Heap`, `FreeList`, `HeapRef`, `HeapAllocated`, `FreeList::allocate`), `crates/steel-core/src/steel_vm/vm.rs` (`ALLOC`/`READALLOC`/`SETALLOC` handlers)
Runtime path traced: mutable-captured variable → `ALLOC`-family opcode → `Heap::allocate` → `FreeList::allocate` → heap keeps strong `StandardShared`, script gets `HeapRef` (weak) → periodic mark&sweep (`mark_and_sweep_queue`) drops unreachable strong entries.
Observed implementation (short quotes):
```rust
pub struct Heap {
    count: usize,
    mark_and_sweep_queue: Vec<SteelVal>,
    ...
    memory_free_list: FreeList<SteelVal>,
    vector_free_list: FreeList<Vec<SteelVal>>,
}
pub struct HeapRef<T: HeapAble> {
    pub(crate) inner: WeakShared<MutContainer<HeapAllocated<T>>>,
}
pub struct HeapAllocated<T: ...> {
    pub(crate) reachable: bool,
    pub(crate) finalizer: bool,
    pub(crate) value: T,
}
// FreeList::allocate:
let weak_ptr = StandardShared::downgrade(guard);
... HeapRef { inner: weak_ptr }
```
`SteelVal` variants: `Boxed(GcMut<SteelVal>)`, `HeapAllocated(HeapRef<SteelVal>)`.
Semantic intent from docs: doc comment on weak boxes (closed.rs:796): "when the garbage collector can prove that the value of a weak box is only reachable through weak references, the weak box value will always return #false."
What is verified: Steel is hybrid: ordinary values are `Gc<T>` = `Rc<T>`; only mutable cells (and mutable vectors) live in a mark&sweep-managed heap where the *heap owns the strong reference* and all program-visible handles are `Weak`. Cycles through mutable cells are therefore collectable; the tracing walk is rooted at stack, frames, globals.
What is inferred: cycles composed purely of Rc values without heap cells are impossible or leak (immutable data can't be made cyclic without a mutable cell; mutation is routed to the heap), so the design is leak-sound by construction.
Relevant invariant: "strong ownership lives in exactly one owner (the heap); everything the program holds is weak" — mutation and cyclicity are quarantined to owner-managed storage.
Consequence for Glia: this is the strongest external precedent for the OwnerRef Weak/Strong split generalized to *all* mutable cells: Glia's `Defs` owning strong values with per-callable weak resting references is the same ownership inversion Steel uses for its heap. But Steel needed a real mark&sweep pass over that owner to reclaim cell cycles — if Glia adopts binding cells (open question), it inherits exactly this need or must accept leaks. Raw-value snapshots avoid it.
Confidence: high
Classification: REQUIRES A SPIKE (cell-cycle reclamation cost if Glia chooses binding cells; raw-value path CONFIRMS)

### Record S4 — Modules: compile-time name mangling into one global env; native boundary

System: Steel
Repository: https://github.com/mattwparas/steel
Repository status: active
Revision or commit: 3a418c9ea586c1862a8c3a49d6a998436afc8957
Files and symbols: `crates/steel-core/src/compiler/modules.rs` (`ModuleManager` line 249, `MANGLER_SEPARATOR = "__%#__"` line 92, `NameMangler::mangle_vars`), `crates/steel-core/src/steel_vm/register_fn.rs` (`pub trait RegisterFn<FN, ARGS, RET> { fn register_fn(&mut self, name: &'static str, func: FN) -> &mut Self; }`), `crates/steel-core/src/rvals.rs` (`FuncV(FunctionSignature)`, `BoxedFunction(Gc<BoxedDynFunction>)`, `MutFunc(MutFunctionSignature)`)
Runtime path traced: `require` → `ModuleManager` compiles module AST → `name_mangler.mangle_vars(&mut module_ast)` → mangled defines emitted into the *same* flat global `Env`; there is no runtime module object.
Observed implementation (short quotes):
```rust
pub(crate) const MANGLER_SEPARATOR: &str = "__%#__";
...
let mut name_mangler = NameMangler::new(globals, prefix);
name_mangler.mangle_vars(&mut module_ast);
mangled_asts.append(&mut module_ast);
```
Semantic intent from docs: none beyond code; modules are a compiler-phase construct.
What is verified: Steel has no per-module runtime instance and no module lifetime problem — module bindings are ordinary global slots under mangled names; exported closures capture nothing module-specific because the module *is* the global env. Native functions are separate `SteelVal` variants holding plain fn pointers / boxed dyn fns, registered by name into globals.
What is inferred: module identity/privacy is enforced only at compile time (mangling), not at runtime — a runtime value that leaks a mangled name has full access.
Relevant invariant: flattening definitions into one owner eliminates the exported-closure-keeps-module-alive question entirely, at the cost of a single ambient namespace.
Consequence for Glia: challenges the necessity of per-module `Defs` owners for the *lifetime* problem — but Steel's ambient flat namespace is exactly what a capability-secure Lisp cannot adopt (no per-module authority boundary). Glia keeps per-module owners for confinement, not for lifetime; worth stating that trade explicitly in the design doc.
Confidence: high (Steel behavior), medium (transfer analysis)
Classification: CHALLENGES CURRENT DESIGN (lifetime motivation only; capability motivation stands)

---

## RHAI

### Record R1 — Scope is parallel name/value vectors; linear reverse search

System: Rhai
Repository: https://github.com/rhaiscript/rhai
Repository status: active (last commit 2026-07-18)
Revision or commit: 950b724b8f1db8404588d6b0f398878b2c91f8ec
Files and symbols: `src/types/scope.rs` (`Scope`, `search`)
Runtime path traced: variable access → AST-cached index if available, else `Scope::search(name)` reverse linear scan.
Observed implementation (short quotes):
```rust
pub struct Scope<'a> {
    values: ThinVec<Dynamic>,
    names: ThinVec<ImmutableString>,
    aliases: ThinVec<StaticVec<ImmutableString>>,
    ...
}
pub(crate) fn search(&self, name: &str) -> Option<usize> {
    self.names.iter().rev().position(|key| name == key)
        .map(|i| self.len() - 1 - i)
}
```
Semantic intent from docs: "Always search a Scope in reverse order" (shadowing).
What is verified: no environment chain, no hashing for locals — one flat scope per evaluation; repeated lookups are O(n) string compares unless the parser cached a `NonZeroUsize` offset (visible in `Stmt::Share(... Option<NonZeroUsize>)`).
What is inferred: perf depends on parse-time index caching; the interpreter falls back to name search when the resolver may have changed scope shape (`global.always_search_scope = true`).
Relevant invariant: names are for resolution; indices are the runtime identity when statically known.
Consequence for Glia: confirms compiling name → slot/index against a `Defs` owner rather than runtime name lookup; Rhai shows the cost of keeping names authoritative at runtime.
Confidence: high
Classification: CONFIRMS CURRENT DESIGN

### Record R2 — No true closures: pure fn defs + Share statement + curried Rc<RefCell> cells

System: Rhai
Repository: https://github.com/rhaiscript/rhai
Repository status: active
Revision or commit: 950b724b8f1db8404588d6b0f398878b2c91f8ec
Files and symbols: `src/ast/script_fn.rs` (`ScriptFuncDef` — has `body`, `name`, `params`, no capture field), `src/eval/stmt.rs` (`Stmt::Share` at line 979), `src/types/dynamic.rs` (`Union::Shared(crate::Shared<crate::Locked<Dynamic>>, Tag, AccessMode)`, `into_shared`), `src/lib.rs` (`pub use alloc::rc::Rc as Shared; pub use std::cell::RefCell as Locked;` non-sync), `src/types/fn_ptr.rs` (`FnPtr { name, curry: ThinVec<Dynamic>, env: Option<Shared<EncapsulatedEnviron>>, typ }`), `src/func/call.rs` line 756+
Runtime path traced: closure literal → parser rewrites to anonymous fn with captured names prepended as params → `Stmt::Share` converts each captured scope slot: `*val = val.take().into_shared()` → shared cell curried into `FnPtr` → call: `curry.iter_mut().chain(call_args.iter_mut())`.
Observed implementation (short quotes):
```rust
Stmt::Share(x) => { ...
    let val = scope.get_mut_by_index(index);
    if !val.is_shared() {
        *val = val.take().into_shared();
    }
}
```
```rust
pub fn into_shared(self) -> Self {
    ... Self(Union::Shared(crate::Locked::new(self).into(), DEFAULT_TAG_VALUE, _access))
}
```
Semantic intent from docs (rhai.rs/book/language/fn-closure.html, fetched 2026-08-03): "The captured variables are automatically converted into **reference-counted shared values** (`Rc<RefCell<Dynamic>>`...)"; "The shared value is then curried into the function pointer itself"; on cycles: "Rhai avoids this by clone-copying most data values, so reference loops are hard to create."
What is verified: capture = promote the *original binding* to a shared cell in place, then alias it via curry. Script function bodies themselves stay environment-free; the "environment" is an argument list.
What is inferred: leaks are possible if a shared cell transitively holds its own FnPtr (Rc cycle, no collector); Rhai relies on value-semantics cloning making this rare, and the book does not warn about it.
Relevant invariant: sharing is opt-in per binding and visible in the IR (`Share`); everything not shared is a plain value.
Consequence for Glia: this is the pure "binding cells" pole of Glia's open question, implemented with the same Rc/RefCell substrate and no GC. It works, but only because Rhai's data model is clone-heavy and closures are rare; a Lisp where closures are pervasive cannot count on "cycles are hard to create." Supports Glia's snapshot-by-default with cells only where mutation demands (and Steel S3 shows what cells cost).
Confidence: high
Classification: CHALLENGES CURRENT DESIGN (viable cell-based alternative) / informs raw-vs-cell open question

### Record R3 — Module = value map + hash-keyed function table; EncapsulatedEnviron merged per call

System: Rhai
Repository: https://github.com/rhaiscript/rhai
Repository status: active
Revision or commit: 950b724b8f1db8404588d6b0f398878b2c91f8ec
Files and symbols: `src/module/mod.rs` (`Module`), `src/ast/ast.rs` (`EncapsulatedEnviron`), `src/func/script.rs` (`call_script_fn` env merge at "Merge in encapsulated environment, if any")
Runtime path traced: calling a fn that came from a module/AST → `call_script_fn(..., _env: Option<&EncapsulatedEnviron>, ...)` → push env's `imports` and `lib` into `GlobalRuntimeState`, swap `global.constants` for the env's constants; restored after the call.
Observed implementation (short quotes):
```rust
pub struct Module {
    modules: BTreeMap<Identifier, SharedModule>,
    variables: BTreeMap<Identifier, Dynamic>,
    all_variables: Option<StraightHashMap<Dynamic>>,
    functions: Option<StraightHashMap<(RhaiFunc, Box<FuncMetadata>)>>,
    all_functions: Option<StraightHashMap<RhaiFunc>>,
    ...
}
pub struct EncapsulatedEnviron {
    pub lib: crate::StaticVec<crate::SharedModule>,
    pub imports: crate::ThinVec<(ImmutableString, crate::SharedModule)>,
    pub constants: Option<crate::eval::SharedGlobalConstants>,
}
```
Semantic intent from docs: `all_*` fields are "Flattened collection ... including those in sub-modules" — precomputed resolution caches.
What is verified: a module is an ordinary map of `Dynamic` variables plus a u64-hash-keyed function table (`StraightHashMap` keyed by pre-hashed name+arity). Fn pointers escaping a module carry `env: Option<Shared<EncapsulatedEnviron>>` — an Rc that keeps the source module(s) alive and is spliced into the caller's global state for the duration of the call.
What is inferred: function identity across modules is the hash, not an address; collisions are handled by the bloom-filter/dynamic-params machinery.
Relevant invariant: an escaped callable carries an owning handle to its defining module environment and temporarily *becomes* the resolution context when invoked.
Consequence for Glia: direct precedent for "exported closures keep module owner alive" — Rhai's `FnPtr.env: Shared<EncapsulatedEnviron>` is Glia's `OwnerRef::Strong` for escaped callables, including the swap-in-owner-on-call semantics. Confirms ordinary-map module exports too (`variables: BTreeMap<Identifier, Dynamic>`).
Confidence: high
Classification: CONFIRMS CURRENT DESIGN

### Record R4 — Script fn call: args moved into scope, per-call resolution with caches

System: Rhai
Repository: https://github.com/rhaiscript/rhai
Repository status: active
Revision or commit: 950b724b8f1db8404588d6b0f398878b2c91f8ec
Files and symbols: `src/func/script.rs` (`call_script_fn`), `src/func/call.rs` (`KEYWORD_FN_PTR_CALL` branch, `fn_resolution_caches`)
Runtime path traced: call → stack-depth check (`global.level > self.max_call_levels()` → `ErrorStackOverflow`) → `scope.extend(fn_def.params.iter().cloned().zip(args.iter_mut().map(|v| v.take())))` → eval body → `ERR::Return(x, ..) => Ok(x)`.
Observed implementation (short quotes):
```rust
// Put arguments into scope as variables
scope.extend(fn_def.params.iter().cloned().zip(args.iter_mut().map(|v| {
    v.take()
})));
```
Semantic intent from docs: header comment: "All function arguments not in the first position are always passed by value and thus consumed."
What is verified: recursion is by-name re-resolution every call (hash lookup with `Caches::fn_resolution_caches`), bounded by `max_call_levels` (Rust-stack recursion, no trampolining/TCO). Late binding is total: redefining a function between calls changes behavior.
What is inferred: cost of total late binding is mitigated only by caching; deep recursion is a hard error, not a tail-call.
Relevant invariant: callable identity = (hashed name, arity) at each call site, resolved against current state.
Consequence for Glia: confirms top-level late binding is a mainstream embeddable-language choice; also a warning: Glia's single-threaded Rc interpreter should decide explicitly between Rust-stack recursion with depth guard (Rhai) vs its own frames — Rhai shows the guard-based approach is acceptable for embedding.
Confidence: high
Classification: CONFIRMS CURRENT DESIGN

---

## RUNE

### Record U1 — Closures: environment snapshot as Box<[Value]>, delivered as a tuple argument

System: Rune
Repository: https://github.com/rune-rs/rune
Repository status: active (last commit 2026-07-29)
Revision or commit: 54fe7cb8eaa3603932a09f678af9d07f39a8d796
Files and symbols: `crates/rune/src/runtime/vm.rs` (`op_closure` line 2781, `op_environment` line 1383), `crates/rune/src/runtime/function.rs` (`FunctionImpl`, `Inner::FnClosureOffset`, `environment: Box<[Value]>`), `crates/rune/src/runtime/unit.rs` (`UnitFn::Offset { captures: Option<usize> }`)
Runtime path traced: closure literal → `op_closure(hash, addr, count, out)` → look up `UnitFn::Offset { captures: Some(n) }` in unit → clone `count` stack values into `Box<[Value]>` → `Function::from_vm_closure(context, unit, globals, offset, call, args, environment, hash)`. Call → environment passed as extra `OwnedTuple` arg → body executes `op_environment` to spread the tuple into registers.
Observed implementation (short quotes):
```rust
let environment = self.stack.slice_at(addr, count)?;
let environment = environment.iter().cloned()
    .try_collect::<alloc::Vec<Value>>()?;
let environment = environment.try_into_boxed_slice()?;
```
```rust
Inner::FnClosureOffset(closure) => {
    let environment = closure.environment.try_clone()?;
    let environment = OwnedTuple::try_from(environment)?;
    closure.fn_offset.call(args, (environment,))?
}
```
Semantic intent from docs: function.rs header: "Closures (which might or might not capture their environment)."
What is verified: captures are a by-value snapshot (clone of `Value` handles) taken at creation; the closure body receives them as a hidden first tuple argument, not via any scope pointer.
What is inferred: mutation of a captured variable is only shared if the captured `Value` is itself a shared object (Rune values are handles); plain data snapshots diverge.
Relevant invariant: closure = code offset + owned value snapshot; zero references into any evaluation scope.
Consequence for Glia: second independent confirmation (with Steel) that snapshot capture into a flat owned vector is the standard for Rust-hosted, non-GC languages; the "environment as synthetic argument" trick is compatible with Glia's centralized escape transforms.
Confidence: high
Classification: CONFIRMS CURRENT DESIGN

### Record U2 — Ownership: non-atomic refcount + access flags; no cycle collection

System: Rune
Repository: https://github.com/rune-rs/rune
Repository status: active
Revision or commit: 54fe7cb8eaa3603932a09f678af9d07f39a8d796
Files and symbols: `crates/rune/src/runtime/value.rs` (`Repr { Inline(Inline), Dynamic(AnySequence<Arc<Rtti>, Value>), Any(AnyObj) }`), `crates/rune/src/runtime/shared.rs` (`Shared<T> { shared: NonNull<AnyObjData>, ... }`), `crates/rune/src/runtime/any_obj.rs` (`AnyObjData`)
Runtime path traced: every heap value → `AnyObjData` allocation → `inc`/`dec` refcount on clone/drop; borrow discipline via `access.try_shared()/try_exclusive()`.
Observed implementation (short quotes):
```rust
#[repr(C)]
pub(super) struct AnyObjData<T = ()> {
    pub(super) access: Access,
    pub(super) count: Cell<usize>,
    pub(super) vtable: &'static AnyObjVtable,
    pub(super) data: T,
}
```
Semantic intent from docs: `dec` assertion comment: "Reference count of zero should only happen if Shared is incorrectly implemented."
What is verified: Rune's runtime ownership is hand-rolled `Rc`-equivalent (`Cell<usize>` count) plus runtime borrow flags (an RefCell-equivalent generalized to guards). There is no tracing GC anywhere in `crates/rune/src/runtime` (no mark/sweep files exist in the pinned tree).
What is inferred: reference cycles (e.g., a closure stored into an object it captured) leak; Rune accepts this, same as vanilla Rc.
Relevant invariant: dynamic borrow checking (Access) substitutes for `RefCell`, and refcounting substitutes for GC — identical constraint envelope to Glia.
Consequence for Glia: Rune is the closest constraint-match to Glia (refcount, no GC, dynamic borrows) and it ships without any cycle story; this validates "leak-on-cycle is acceptable if the language design steers cycles into owner-managed structures" — which is exactly what the Weak resting OwnerRef is for.
Confidence: high (implementation), medium (leak-acceptance being deliberate — inferred from absence)
Classification: CONFIRMS CURRENT DESIGN

### Record U3 — Unit/Context: hash-keyed compiled functions; escaped callables own their world

System: Rune
Repository: https://github.com/rune-rs/rune
Repository status: active
Revision or commit: 54fe7cb8eaa3603932a09f678af9d07f39a8d796
Files and symbols: `crates/rune/src/runtime/unit.rs` (`Unit`, `Logic { functions: hash::Map<UnitFn>, ... }`), `crates/rune/src/module/module.rs` (`Module { items, associated, types, ... }`), `crates/rune/src/runtime/runtime_context.rs` (`RuntimeContext { functions: hash::Map<FunctionHandler>, constants, ... }`), `crates/rune/src/runtime/function.rs` (`FnOffset { context: Arc<RuntimeContext>, unit: Arc<Unit>, globals: V::Globals, offset, call, args, hash }`)
Runtime path traced: native `Module::function(name, f)` → install into `Context` → `RuntimeContext` hash map of type-erased `FunctionHandler`s; script fns compiled into `Unit.functions: hash::Map<UnitFn>`; a first-class `Function` escaping the VM embeds `Arc<RuntimeContext>` + `Arc<Unit>`.
Observed implementation (short quotes):
```rust
struct FnOffset<V> where V: FnValue {
    context: Arc<RuntimeContext>,
    /// The unit where the function resides.
    unit: Arc<Unit>,
    /// The storage for static items declared by the unit.
    globals: V::Globals,
    ...
}
```
Semantic intent from docs: Inner::FnOffset comment: "This also captures the context and unit it belongs to allow for external calls."
What is verified: callable identity is a type `Hash`; modules have no runtime instances (they compile into Context/Unit); every escaped callable strongly owns its code (`Arc<Unit>`) and its native world (`Arc<RuntimeContext>`).
What is inferred: nothing in a Unit points back at a `Function` value, so unit/context Arcs cannot cycle — strong ownership by callables is safe here by construction.
Relevant invariant: escaped callable ⇒ strong owner of everything needed to run it later, and that ownership is acyclic by stratification (code/context never reference values).
Consequence for Glia: "exported closures keep module owner alive" confirmed a second time; also gives the acyclicity argument Glia needs: strong OwnerRef from callable to `Defs` is safe iff `Defs` never strongly owns callables that escaped... which Glia's `Defs` (name→Val) violates — a `Defs` entry can hold a closure whose OwnerRef points back at that `Defs`. Rune avoids this by keeping code (Unit) value-free; Glia cannot. The Weak-resting/Strong-escaped split is therefore not paranoia, it is required, and "escaped" must mean "left the ownership subtree of its own Defs."
Confidence: high
Classification: CONFIRMS CURRENT DESIGN (and sharpens the escape criterion)

### Record U4 — Globals: Rc-shared slot block carried by escaped functions (OwnerRef analog)

System: Rune
Repository: https://github.com/rune-rs/rune
Repository status: active
Revision or commit: 54fe7cb8eaa3603932a09f678af9d07f39a8d796
Files and symbols: `crates/rune/src/runtime/globals.rs` (`Globals`, `GlobalsInner`), `crates/rune/src/runtime/function.rs` (`FnValue` trait and its `Globals` associated type)
Runtime path traced: unit declares statics → `Globals` built per-VM: `GlobalsInner { unit: Arc<Unit>, slots: RefCell<Box<[Value]>> }` behind `Option<Rc<GlobalsInner>>` → `op_closure`/`Function::from_vm_closure` passes `self.globals.clone()` into the escaped `Function` → later external calls construct `Vm::new(...).with_globals(V::globals(&self.globals))`.
Observed implementation (short quotes):
```rust
pub struct Globals {
    /// The handle is non-atomically counted, since the storage holds [`Value`]s
    /// and can therefore never be shared across threads anyway.
    inner: Option<Rc<GlobalsInner>>,
}
pub(crate) struct GlobalsInner {
    unit: Arc<Unit>,
    /// One slot per static declared by the unit. ...
    slots: RefCell<Box<[Value]>>,
}
```
Semantic intent from docs: `FnValue` doc: "A [`Function`] carries the [`Globals`] of the virtual machine which produced it, so that calling it from the outside still observes the same statics."
What is verified: mutable persistent definitions (statics) are a fixed slot block (`RefCell<Box<[Value]>>`) owned via `Rc`; each escaped callable carries a strong handle so external invocation observes the same definition state.
What is inferred: since `slots` holds `Value`s which can hold `Function`s which hold `Rc<GlobalsInner>`, a static that stores a closure creates an Rc cycle → leak. Rune ships this anyway (comment acknowledges Rc, not cycles).
Relevant invariant: definition storage identity is preserved across the escape boundary by a strong owner handle on the callable — precisely amended-Graph-4's `OwnerRef::Strong(Rc<Defs>)`.
Consequence for Glia: strongest single confirmation found in this study: an actively-developed comparable independently converged on "slot block behind Rc + RefCell, callable carries the owner handle when it escapes." Rune's version is Strong-always and demonstrably leaks on the closure-stored-in-static pattern — validating Glia's refinement (Weak while resting in own Defs, Strong only when escaped).
Confidence: high
Classification: CONFIRMS CURRENT DESIGN

---

## GLUON

### Record G1 — Real mark&sweep tracing GC with generation tree

System: Gluon
Repository: https://github.com/gluon-lang/gluon
Repository status: low activity (last commit 2026-07-10; historically slow-moving)
Revision or commit: 418c6b7de22b244746bfd0570f9fcfd6d738e542
Files and symbols: `vm/src/gc.rs` (`pub struct Gc` line 236, `collect` line 1278, `sweep` line 1308, `mark`, `GcHeader { marked, generation }`, `unsafe trait DataDef`)
Runtime path traced: `alloc` past `collect_limit` → `Gc::collect(roots)` → `roots.trace(self)` marks → `sweep()` walks the intrusive `values: Option<AllocPtr>` linked list freeing unmarked → `collect_limit = 2 * allocated_memory`.
Observed implementation (short quotes):
```rust
/// A mark and sweep garbage collector.
pub struct Gc {
    /// Linked list of all objects allocted by this garbage collector.
    values: Option<AllocPtr>,
    allocated_memory: usize,
    collect_limit: usize,
    memory_limit: usize,
    ...
    generation: Generation,
}
pub unsafe fn collect<R>(&mut self, roots: R) where R: Trace + CollectScope {
    roots.scope(self, |self_| {
        roots.trace(self_);
        self_.sweep();
        self_.collect_limit = 2 * self_.allocated_memory;
    })
}
```
Semantic intent from docs: in-source: "A mark and sweep garbage collector"; long comment on the generation tree ("Generations 2 can share values with anything above them in the tree...") governing cross-thread value sharing.
What is verified: Gluon HAS a tracing mark&sweep GC (per-thread GCs arranged in a generation tree, global gen-0 GC in `GlobalVmState { pub gc: Mutex<Gc> }`); cycles are reclaimed.
What is inferred: none needed.
Relevant invariant: with tracing GC, cyclic value graphs are a non-issue and the language exploits that (see G2).
Consequence for Glia: baseline calibration — Gluon's freedoms (cyclic letrec, closures stored anywhere) come from the collector Glia deliberately does not have; do not import Gluon patterns without checking this dependency.
Confidence: high
Classification: NOT TRANSFERABLE — GC DIFFERENCE

### Record G2 — ClosureData upvar array; recursion via allocate-dummy-then-patch (cyclic closures)

System: Gluon
Repository: https://github.com/gluon-lang/gluon
Repository status: low activity
Revision or commit: 418c6b7de22b244746bfd0570f9fcfd6d738e542
Files and symbols: `vm/src/value.rs` (`ClosureData { function: GcPtr<BytecodeFunction>, upvars: Array<Value> }`, `ValueRepr::Closure(GcPtr<ClosureData>)`), `vm/src/thread.rs` (`MakeClosure`, `NewClosure`, `CloseClosure`, `PushUpVar` at lines 2399–2456)
Runtime path traced: non-recursive: `MakeClosure` copies `upvars` stack values into the GC allocation. Recursive/letrec: `NewClosure` allocates with dummy upvars (`ClosureInitDef(@func, upvars as usize)`) → bindings evaluated (can reference the closure) → `CloseClosure(n)` patches: `closure.as_mut().upvars.iter_mut().zip(&self.stack[start..])` overwrites dummies. Access: `PushUpVar(i)` indexes the array.
Observed implementation (short quotes):
```rust
pub struct ClosureData {
    pub function: GcPtr<BytecodeFunction>,
    pub(crate) upvars: Array<Value>,
}
```
```rust
NewClosure { function_index, upvars } => {
    // Use dummy variables until it is filled
    ... construct_gc!(ClosureInitDef(@func, upvars as usize)) ...
}
CloseClosure(n) => { ...
    // it has just been allocated and havent even had its upvars set yet
    for (var, value) in closure.as_mut().upvars.iter_mut().zip(&self.stack[start..]) {
        *var = value.clone_unrooted();
    }
}
```
Semantic intent from docs: comments in the handlers as quoted.
What is verified: captures are by-value snapshots into a GC array (same shape as Steel/Rune), but mutually recursive closures are made *genuinely cyclic* by post-allocation patching — a closure's upvar can be the closure itself.
What is inferred: this exact letrec strategy requires either tracing GC or acceptance of Rc cycles; under Glia it would leak per recursive definition group.
Relevant invariant: recursion implemented as cyclic capture, not late binding.
Consequence for Glia: the strongest argument for Glia's alternative: top-level/letrec recursion should go through the `Defs` owner (late binding through the definition slot, per Steel S2) rather than self-capture — self-capture is precisely the pattern Glia's no-GC substrate cannot afford. If Glia ever wants Scheme-style local `letrec` closures, this is the spike to run: owner-mediated weak self-reference vs Gluon-style patching.
Confidence: high
Classification: NOT TRANSFERABLE — GC DIFFERENCE (mechanism); REQUIRES A SPIKE (Glia letrec design)

### Record G3 — Module globals resolved at link time and baked in as upvars (early binding)

System: Gluon
Repository: https://github.com/gluon-lang/gluon
Repository status: low activity
Revision or commit: 418c6b7de22b244746bfd0570f9fcfd6d738e542
Files and symbols: `vm/src/vm.rs` (`new_bytecode` lines 53–83, `GlobalVmState { env: parking_lot::RwLock<Globals> }` line 201, `pub struct Globals { pub type_infos: TypeInfos }` line 291, `trait VmEnv { fn get_global(&self, name: &str) -> Option<RootedGlobal>; }`), `src/import.rs` (`Import<I = DefaultImporter>`, `self.importer.import(compiler, vm, &modulename).await`, error "The importer found a cyclic dependency when loading files")
Runtime path traced: module compiled → `CompiledModule { module_globals, function }` → `new_bytecode`: each referenced global name resolved once via `env.get_global(index.definition_name())` → resulting values become the upvars of the module's top-level closure (`ClosureDataDef(&bytecode_function, globals.iter()...)`).
Observed implementation (short quotes):
```rust
let globals = module_globals.into_iter().map(|index| {
    env.get_global(index.definition_name())
        .expect("ICE: Global is missing from environment").value
}).collect::<Vec<_>>();
... gc.alloc(ClosureDataDef(&bytecode_function, globals.iter().map(|v| v.get_value())))
```
Semantic intent from docs: import machinery is a compiler-database query (`query::{AsyncCompilation, Compilation, CompilerDatabase}`) — compiled modules are cached; cyclic imports are a hard error.
What is verified: Gluon has NO top-level late binding: cross-module references are resolved to values exactly once at module link time and snapshotted as upvars. Redefinition does not propagate. Modules are values produced by typed compilation, cached by the query database.
What is inferred: this choice is coupled to static typing (a global's type is fixed at check time; swapping values at runtime would bypass the checker).
Relevant invariant: definition lookup happens at a well-defined phase boundary (link), never during execution.
Consequence for Glia: the counter-model to Glia's late binding — but its motivation is the type system, not memory. For a dynamically-typed capability Lisp, per-call lookup through `Defs` (Steel/Rhai style) remains the right default; note only that Gluon shows a frozen-at-link snapshot semantics is coherent, which is effectively what Glia's "frozen shared prelude" already is for the prelude layer.
Confidence: high
Classification: NOT TRANSFERABLE — STATIC TYPE-SYSTEM DIFFERENCE (with partial confirmation of frozen-prelude layering)

### Record G4 — Native boundary: ExternFunction as GC-managed value

System: Gluon
Repository: https://github.com/gluon-lang/gluon
Repository status: low activity
Revision or commit: 418c6b7de22b244746bfd0570f9fcfd6d738e542
Files and symbols: `vm/src/value.rs` (`ExternFunction` line 1097, `ValueRepr::Function(GcPtr<ExternFunction>)`, `PartialApplicationData { function: Callable, args: Array<Value> }`), `vm/src/thread.rs` (`call_function_with_upvars` line 2657, `get_global<T: Getable + VmType>` line 850 with `check_signature`)
Runtime path traced: Rust fn registered → `ExternFunction { id: Symbol, args: VmIndex, function: extern "C" fn(&Thread) -> Status }` allocated on the GC heap → uniform `Callable` dispatch; over/under-application handled by `PartialApplication` allocation. Host extraction: `get_global` → type check (`check_signature`) → `T::from_value`.
Observed implementation (short quotes):
```rust
pub struct ExternFunction {
    pub id: Symbol,
    pub args: VmIndex,
    pub function: extern "C" fn(&Thread) -> Status,
}
```
Semantic intent from docs: none beyond signatures; the `extern "C" fn(&Thread) -> Status` shape forces natives to pull args off the VM stack themselves.
What is verified: natives are first-class GC values indistinguishable from closures at call sites; currying uniformity is bought with `PartialApplicationData` heap allocations; the host boundary re-checks types at extraction.
What is inferred: nothing significant.
Relevant invariant: one `Callable` dispatch union covering native/closure/partial-application.
Consequence for Glia: neutral confirmation that natives-as-plain-values (Glia already does this via its Val/effect model) is fine; the partial-application allocation trick is only needed for auto-currying semantics Glia doesn't have.
Confidence: high
Classification: NOT RELEVANT (Glia already equivalent)

---

## RUNTIME/BINDING MATRIX

| field | Steel | Rhai | Rune | Gluon |
|---|---|---|---|---|
| lexical capture representation | flat `CaptureVec` of `SteelVal` copied at NEWSCLOSURE (stack or parent-capture source) | none in fn def; captured names become params, shared cells curried into `FnPtr.curry` | `Box<[Value]>` snapshot at `op_closure`, delivered as hidden tuple arg | `upvars: Array<Value>` in GC-heap `ClosureData` |
| persistent-definition representation | flat `Env.bindings_vec: Vec<SteelVal>` indexed by interned slot | `Scope{names, values}` + `Module.variables: BTreeMap<Identifier, Dynamic>` | `GlobalsInner.slots: RefCell<Box<[Value]>>` behind `Rc`, one slot per unit static | query-database globals; resolved via `VmEnv::get_global(name)` |
| raw value vs binding cell | raw values; cells (`HeapAllocated`/`HeapRef`) only for mutated captures | raw until `Stmt::Share` promotes binding in place to `Rc<RefCell<Dynamic>>` | raw `Value` handles (interior sharing only if value itself is shared object) | raw values (immutable language; `Reference` cells are library types) |
| top-level late binding | yes — `CALLGLOBAL` re-reads slot per call | yes — name+arity hash resolved per call (cached) | statics read slot at runtime; fn-by-hash fixed at compile | no — globals snapshotted into module closure upvars at link |
| recursion mechanism | global-slot re-read (late binding); mutable-cell box for local letrec | per-call name re-resolution; depth-capped Rust stack | fn hash lookup in Unit (fixed offsets) | cyclic closure via NewClosure/CloseClosure patch (needs GC) |
| module instance representation | none at runtime — compile-time mangling (`__%#__`) into one Env | `Shared<Module>` (Rc) with variables map + fn hash table | none — compiled into `Arc<Unit>` + `Arc<RuntimeContext>` | module = typed value; compiled module cached in CompilerDatabase |
| export representation | mangled global slots | `variables: BTreeMap<Identifier, Dynamic>` + hash-keyed fns | items by `Hash` in Unit/Context | record fields of module value |
| callable identity | `Gc<ByteCodeLambda>` (ptr) + interned prototype `id: u32` | (hashed name, arity) `u64` | type `Hash` + (unit, offset) | `GcPtr<ClosureData>` / `GcPtr<ExternFunction>` ptr |
| lifetime mechanism | Rc (`Gc<T>=Rc<T>`) + heap-owned strong / user-weak for cells | `Rc`/`Arc` everywhere (`Shared`) | hand-rolled non-atomic refcount (`AnyObjData.count: Cell<usize>`) | tracing mark&sweep per-thread GC, generation tree |
| cycle handling | mark&sweep over cell heap reclaims cell cycles; pure-Rc cycles impossible by construction | none; "clone-copying most data values, so reference loops are hard to create" (book) | none; cycles leak | full GC reclaim |
| native boundary | `RegisterFn` trait → `FuncV`/`BoxedFunction`/`MutFunc` SteelVal variants | `Module::set_native_fn` → `RhaiFunc` in hash table | `Module::function` builder → `FunctionHandler` in `RuntimeContext` by hash | `ExternFunction { extern "C" fn(&Thread) -> Status }` as GC value |
| WASM implications | no wasm-specific files in pinned tree (not traced) | first-class: `wasm-bindgen` feature, `wasm` category, `web-time` dep in Cargo.toml | dedicated `crates/rune-wasm` crate in tree | none in pinned tree |
| transferable lesson | snapshot captures + late-bound global slots + owner-holds-strong/user-holds-weak cells is a proven no-GC Lisp recipe | binding-cell capture works on Rc/RefCell but relies on clone-heavy data culture to dodge cycles | escaped callables carrying `Rc` owner handle to definition slots = OwnerRef, field-tested; Strong-always leaks on closure-in-static | recursion-as-cyclic-capture and link-time global snapshots both require what Glia lacks (GC / static types) |

---

## PINNED LINKS

Steel (3a418c9):
- https://raw.githubusercontent.com/mattwparas/steel/3a418c9ea586c1862a8c3a49d6a998436afc8957/crates/steel-core/src/values/closed.rs
- https://raw.githubusercontent.com/mattwparas/steel/3a418c9ea586c1862a8c3a49d6a998436afc8957/crates/steel-core/src/values/functions.rs
- https://raw.githubusercontent.com/mattwparas/steel/3a418c9ea586c1862a8c3a49d6a998436afc8957/crates/steel-core/src/env.rs
- https://raw.githubusercontent.com/mattwparas/steel/3a418c9ea586c1862a8c3a49d6a998436afc8957/crates/steel-core/src/gc.rs
- https://raw.githubusercontent.com/mattwparas/steel/3a418c9ea586c1862a8c3a49d6a998436afc8957/crates/steel-core/src/steel_vm/vm.rs
- https://raw.githubusercontent.com/mattwparas/steel/3a418c9ea586c1862a8c3a49d6a998436afc8957/crates/steel-core/src/compiler/modules.rs
- https://raw.githubusercontent.com/mattwparas/steel/3a418c9ea586c1862a8c3a49d6a998436afc8957/crates/steel-core/src/steel_vm/register_fn.rs
- https://raw.githubusercontent.com/mattwparas/steel/3a418c9ea586c1862a8c3a49d6a998436afc8957/crates/steel-core/src/rvals.rs

Rhai (950b724):
- https://raw.githubusercontent.com/rhaiscript/rhai/950b724b8f1db8404588d6b0f398878b2c91f8ec/src/types/scope.rs
- https://raw.githubusercontent.com/rhaiscript/rhai/950b724b8f1db8404588d6b0f398878b2c91f8ec/src/ast/script_fn.rs
- https://raw.githubusercontent.com/rhaiscript/rhai/950b724b8f1db8404588d6b0f398878b2c91f8ec/src/func/script.rs
- https://raw.githubusercontent.com/rhaiscript/rhai/950b724b8f1db8404588d6b0f398878b2c91f8ec/src/types/dynamic.rs
- https://raw.githubusercontent.com/rhaiscript/rhai/950b724b8f1db8404588d6b0f398878b2c91f8ec/src/module/mod.rs
- https://raw.githubusercontent.com/rhaiscript/rhai/950b724b8f1db8404588d6b0f398878b2c91f8ec/src/types/fn_ptr.rs
- https://raw.githubusercontent.com/rhaiscript/rhai/950b724b8f1db8404588d6b0f398878b2c91f8ec/src/eval/stmt.rs
- https://raw.githubusercontent.com/rhaiscript/rhai/950b724b8f1db8404588d6b0f398878b2c91f8ec/src/ast/ast.rs
- https://raw.githubusercontent.com/rhaiscript/rhai/950b724b8f1db8404588d6b0f398878b2c91f8ec/src/func/call.rs
- Docs: https://rhai.rs/book/language/fn-closure.html (fetched 2026-08-03)

Rune (54fe7cb):
- https://raw.githubusercontent.com/rune-rs/rune/54fe7cb8eaa3603932a09f678af9d07f39a8d796/crates/rune/src/runtime/vm.rs
- https://raw.githubusercontent.com/rune-rs/rune/54fe7cb8eaa3603932a09f678af9d07f39a8d796/crates/rune/src/runtime/function.rs
- https://raw.githubusercontent.com/rune-rs/rune/54fe7cb8eaa3603932a09f678af9d07f39a8d796/crates/rune/src/runtime/shared.rs
- https://raw.githubusercontent.com/rune-rs/rune/54fe7cb8eaa3603932a09f678af9d07f39a8d796/crates/rune/src/runtime/any_obj.rs
- https://raw.githubusercontent.com/rune-rs/rune/54fe7cb8eaa3603932a09f678af9d07f39a8d796/crates/rune/src/runtime/value.rs
- https://raw.githubusercontent.com/rune-rs/rune/54fe7cb8eaa3603932a09f678af9d07f39a8d796/crates/rune/src/runtime/unit.rs
- https://raw.githubusercontent.com/rune-rs/rune/54fe7cb8eaa3603932a09f678af9d07f39a8d796/crates/rune/src/runtime/globals.rs
- https://raw.githubusercontent.com/rune-rs/rune/54fe7cb8eaa3603932a09f678af9d07f39a8d796/crates/rune/src/runtime/runtime_context.rs
- https://raw.githubusercontent.com/rune-rs/rune/54fe7cb8eaa3603932a09f678af9d07f39a8d796/crates/rune/src/module/module.rs

Gluon (418c6b7):
- https://raw.githubusercontent.com/gluon-lang/gluon/418c6b7de22b244746bfd0570f9fcfd6d738e542/vm/src/gc.rs
- https://raw.githubusercontent.com/gluon-lang/gluon/418c6b7de22b244746bfd0570f9fcfd6d738e542/vm/src/value.rs
- https://raw.githubusercontent.com/gluon-lang/gluon/418c6b7de22b244746bfd0570f9fcfd6d738e542/vm/src/thread.rs
- https://raw.githubusercontent.com/gluon-lang/gluon/418c6b7de22b244746bfd0570f9fcfd6d738e542/vm/src/vm.rs
- https://raw.githubusercontent.com/gluon-lang/gluon/418c6b7de22b244746bfd0570f9fcfd6d738e542/src/import.rs
- https://raw.githubusercontent.com/gluon-lang/gluon/418c6b7de22b244746bfd0570f9fcfd6d738e542/vm/src/api/function.rs

Method note: all quotes were taken from files downloaded directly from raw.githubusercontent.com at the pinned SHAs (temporary local copies, not archived); commit pins were retrieved from the GitHub commits API on 2026-08-03; the single docs citation (Rhai book) was fetched via WebFetch and is marked as docs, not source.
