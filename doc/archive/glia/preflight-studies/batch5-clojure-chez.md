All tracing is complete. Both repos fetched at pinned SHAs; large files (Compiler.java, syntax.ss, gc.c, mkgc.ss, cpnanopass.ss, cpprim.ss, interpret.ss) were WebFetch-truncated, so I downloaded the identical pinned raw URLs in full and verified every quote against the pinned bytes. Below are the records.

---

# SYSTEM 1: CLOJURE

**Pinned revision:** `2cea69253b9ed27436e93632b9a27c2f99088a49` (HEAD 2026-07-28, via api.github.com). Repository active.
**Verification method note:** Var.java and Namespace.java verified fully via WebFetch. Compiler.java was truncated by WebFetch mid-file (WebFetch directly verified DefExpr.eval and the DefExpr parser fragment); all other Compiler.java quotes were verified against the full file downloaded from the same pinned raw URL (`raw.githubusercontent.com/clojure/clojure/2cea6925.../src/jvm/clojure/lang/Compiler.java`, 9687 lines).

---

### Record C1 — What `def` creates and where it lives

- **System:** Clojure
- **Repository:** https://github.com/clojure/clojure
- **Repository status:** active
- **Revision or commit:** 2cea69253b9ed27436e93632b9a27c2f99088a49
- **Files and symbols:** [Namespace.java](https://github.com/clojure/clojure/blob/2cea69253b9ed27436e93632b9a27c2f99088a49/src/jvm/clojure/lang/Namespace.java) (`mappings`, `intern`), [Var.java](https://github.com/clojure/clojure/blob/2cea69253b9ed27436e93632b9a27c2f99088a49/src/jvm/clojure/lang/Var.java) (`root`, `Unbound`), [Compiler.java L8052-8097](https://github.com/clojure/clojure/blob/2cea69253b9ed27436e93632b9a27c2f99088a49/src/jvm/clojure/lang/Compiler.java#L8052) (`lookupVar`), DefExpr.eval
- **Runtime path traced:** `(def x e)` → `DefExpr.Parser` → `lookupVar(sym, true)` → `currentNS().intern(sym)` → new `Var` stored in namespace map → `DefExpr.eval()` → `var.bindRoot(init.eval())`.
- **Observed implementation:** Namespace holds `transient final AtomicReference<IPersistentMap> mappings` (name→Var map, CAS-swapped). `Namespace.intern`: `while((o = map.valAt(sym)) == null) { if(v == null) v = new Var(this, sym); ... mappings.compareAndSet(map, newMap); }`. Var: `volatile Object root; volatile boolean dynamic = false; ... public final Symbol sym; public final Namespace ns;`. A fresh Var starts unbound: root is an `Unbound extends AFn` sentinel whose `throwArity` raises `"Attempting to call unbound fn: " + v`. `DefExpr.eval()`: `if(initProvided) { var.bindRoot(init.eval()); }`. `lookupVar` with `internNew` interns even with *no* init (`(declare f)` path): `//introduce a new var in the current ns if(internNew) var = currentNS().intern(...)`.
- **Semantic intent from docs/papers:** clojure.org/reference/vars: "Vars provide a mechanism to refer to a mutable storage location that can be dynamically rebound."
- **What is verified:** All quoted code at pinned SHA. def = intern-cell-then-bind, two separable steps; the cell exists (and is callable-but-throwing) before any value.
- **What is inferred:** None material.
- **Relevant invariant:** A top-level name denotes a *stable heap cell created at intern time*; interning is idempotent (`isInternedMapping` short-circuit) so re-`def` reuses the same cell forever.
- **Consequence for Glia:** This is the strongest precedent for name→Rc<Binding{current, version}> in Defs: Clojure's whole top-level model is "intern the cell first, bind later." Glia's `defs: name→Val` cannot represent "declared but unbound" without a sentinel Val, and cannot make re-definition idempotent on identity.
- **Confidence:** high
- **Classification:** CHALLENGES CURRENT DESIGN (raw-value Defs variant); CONFIRMS the late-bound-top-level half of the design.

---

### Record C2 — How compiled fn bodies resolve a global

- **System:** Clojure
- **Repository:** https://github.com/clojure/clojure
- **Repository status:** active
- **Revision or commit:** 2cea6925...
- **Files and symbols:** Compiler.java: `VarExpr` ([L635-682](https://github.com/clojure/clojure/blob/2cea69253b9ed27436e93632b9a27c2f99088a49/src/jvm/clojure/lang/Compiler.java#L635)), `ObjExpr.emitVarValue` (L5762-5774), `ObjExpr.emitVar` (L5753), `InvokeExpr.parse` (L4338-4377), `InvokeExpr.emitArgsAndCall` (L4324); Var.java `getRawRoot`, `deref`.
- **Runtime path traced:** symbol → `VarExpr(var)` at compile time (the Var *object* is resolved once, at compile time, via lookupVar) → bytecode: load Var from the class's constant table → `getRawRoot()`/`get()` virtual call → `invokeinterface IFn.invoke` on the result. Every call re-reads the cell.
- **Observed implementation:** `VarExpr.emit`: `objx.emitVarValue(gen,var);`. `emitVarValue`: `if(!v.isDynamic()) { emitConstant(gen, i); gen.invokeVirtual(VAR_TYPE, varGetRawMethod); } else { ... varGetMethod }` where `varGetRawMethod = Method.getMethod("Object getRawRoot()")`. Var: `final public Object getRawRoot(){ return root; }` (volatile read, no thread-binding check); `deref()` checks `getThreadBinding()` first. Invocation: `gen.invokeInterface(IFN_TYPE, new Method("invoke", ...))`. Escape hatch: with `:direct-linking`, `InvokeExpr.parse` bypasses the cell — `if(!v.isDynamic() && !RT.booleanCast(RT.get(v.meta(), redefKey, false))) { ... StaticInvokeExpr.parse(...) }` — a static call to the fn class, opt-out via `^:redef`/`^:dynamic`.
- **Semantic intent from docs/papers:** Clojure 1.8 release notes describe direct linking as trading redefinability for performance — confirming the cell-read is understood as the *cost of* redefinability.
- **What is verified:** All quotes at pinned SHA.
- **What is inferred:** That `emitConstant` materializes the Var in the class constant pool (static fields set in `<clinit>` via RT.var) — standard, seen in surrounding code (`clinitgen.invokeVirtual(VAR_TYPE, ...getRawRoot())` at L5228) but I did not trace `emitConstants` line-by-line.
- **Relevant invariant:** Name→cell resolution happens ONCE (compile time); cell→value resolution happens EVERY call. The name is not consulted at runtime at all.
- **Consequence for Glia:** Splits Glia's "late-bound through Defs" into two designs: (a) per-call *name lookup* in Defs (hash lookup each call), vs (b) per-call *cell read* with the Rc<Binding> resolved once at closure creation. Clojure demonstrates (b) — cheap volatile read — and shows the opt-out (direct linking) is what you add when even cell-reads are too hot. With raw name→Val Defs, Glia is forced into (a) forever.
- **Confidence:** high
- **Classification:** CHALLENGES CURRENT DESIGN (raw-value storage); suggests binding cells with one-time name resolution.

---

### Record C3 — Local capture and recursion (closed-overs, thisName, letfn)

- **System:** Clojure
- **Repository:** https://github.com/clojure/clojure
- **Repository status:** active
- **Revision or commit:** 2cea6925...
- **Files and symbols:** Compiler.java: `ObjExpr.closes` (L4742), closed-over field emission (L4929-4958), ctor (L5007-5025), closure-construction `ObjExpr.emit` (L5607-5634), `emitLocal` (L5669-5689), `LocalBinding` (L6528), `fn.thisName` (L4590), `FnMethod.parse` this-registration (L5961), `LetFnExpr.emit` (L6846-6870), `emitLetFnInits` (L5580-5605), `closeOver` (L8116).
- **Runtime path traced:** free lexical var → `closeOver` adds LocalBinding to `objx.closes` → compiled fn class gets one instance field per closed-over → at closure-creation site the *enclosing* scope's values are loaded and passed to the ctor (`for(ISeq s = RT.seq(closesExprs)...) objx.emitLocal(...); gen.invokeConstructor(...)`) → ctor does `putField`. Reads inside the body: `if(closes.containsKey(lb)) { gen.loadThis(); gen.getField(objtype, lb.name, ...) }`.
- **Observed implementation:** Capture is **by value, flat, at construction time**; fields are plain (comment: `cv.visitField(0 //+ (oneTimeUse ? 0 : ACC_FINAL)` with note `//todo - only enable this non-private+writability for letfns where we need it`). Self-recursion needs no cell: `fn.thisName = nm.name;` and in FnMethod.parse `if(objx.thisName != null) registerLocal(Symbol.intern(objx.thisName), null, null,false);` — the name binds to JVM local 0, the closure object itself. Mutual *local* recursion (letfn): `LetFnExpr.emit` first stores nulls, then constructs each fn, then **patches closed-over fields after construction**: `fe.emitLetFnInits(gen, objx, lbset)` → `objx.emitLocal(gen, lb, false); gen.putField(objtype, lb.name, OBJECT_TYPE);` — the cycle knot is tied by mutating the already-constructed closures' capture fields.
- **Semantic intent from docs/papers:** Locals are immutable per clojure.org (no set! on locals), which is what makes by-value snapshot correct.
- **What is verified:** All quotes at pinned SHA.
- **What is inferred:** Nothing material.
- **Relevant invariant:** Value-snapshot capture is sound ⇔ lexicals are immutable; recursion never depends on capture — self-recursion uses object identity ("this"), local mutual recursion uses post-construction patching, top-level mutual recursion uses the Var cell (declare → unbound Var → calls go through cell, Record C1/C2).
- **Consequence for Glia:** Directly CONFIRMS Glia's "closures capture lexical values by snapshot." Also gives Glia the complete recursion decision table: named self-recursion can be self-reference (no cell); `letfn`-style local mutual recursion in a snapshot world requires either a patch-after-construct step (interior mutability at construction only) or routing through Defs cells; top-level mutual recursion falls out free from late binding.
- **Confidence:** high
- **Classification:** CONFIRMS CURRENT DESIGN (snapshot capture); REQUIRES A SPIKE only if Glia wants letfn-style *local* mutual recursion.

---

### Record C4 — Redefinition: root swap, rev bump, effect on existing closures

- **System:** Clojure
- **Repository:** https://github.com/clojure/clojure
- **Repository status:** active
- **Revision or commit:** 2cea6925...
- **Files and symbols:** Var.java `bindRoot`, `alterRoot`, `rev`; Compiler.java `emitVarValue` (consumer side).
- **Runtime path traced:** re-`def`/`alter-var-root` → `bindRoot`/`alterRoot` → in-place swap of `volatile Object root` → all existing compiled call sites observe it on next `getRawRoot()`.
- **Observed implementation:** `synchronized public void bindRoot(Object root){ validate(...); Object oldroot = this.root; this.root = root; ++rev; alterMeta(dissoc, RT.list(macroKey)); notifyWatches(oldroot,this.root); }` with `static public volatile int rev = 0;` — a **global version counter** bumped on every root mutation anywhere. `alterRoot` likewise does `this.root = newRoot; ++rev;`. Closures holding the fn value itself (e.g. passed as an argument) keep the *old* value; only cell-mediated references update.
- **Semantic intent from docs/papers:** Vars documented as supporting interactive redefinition ("REPL-driven development") without recompiling callers.
- **What is verified:** Quotes at pinned SHA. `rev` is written on every rebind; it is `static` (VM-global), not per-Var.
- **What is inferred:** Consumers of `rev` are outside these files (e.g. MultiFn/protocol cache invalidation); I did not trace readers, only the write barrier.
- **Relevant invariant:** Redefinition = mutation of the cell, never replacement of the map entry; visibility to existing code is exactly the set of references that go *through* the cell.
- **Consequence for Glia:** For Glia's open question this is the crux: with name→Rc<Binding{current, version}>, re-def is `binding.current = v; binding.version += 1` and every escaped closure that resolved the cell sees it — matching Clojure. With raw name→Val, re-def replaces a map entry, and any value that was *copied out* (export maps! Glia exports are "ordinary maps of the module's own bindings") is permanently stale. Note Clojure's version counter is global, not per-cell — a cheaper alternative to Glia's proposed per-Binding `version` if versions are only used for cache invalidation.
- **Confidence:** high
- **Classification:** CHALLENGES CURRENT DESIGN (specifically challenges value-copy *exports* combined with visible redefinition).

---

### Record C5 — refer/aliasing and ns-unmap: names vs cells

- **System:** Clojure
- **Repository:** https://github.com/clojure/clojure
- **Repository status:** active
- **Revision or commit:** 2cea6925...
- **Files and symbols:** Namespace.java `reference(Symbol, Object)`, `unmap`, `findInternedVar`, `getMapping`, `referenceClass`.
- **Runtime path traced:** `(refer ...)`/`(use ...)` → `Namespace.reference(sym, var)` → the *importing* namespace's mappings gets `sym → the identical Var object` owned by the exporting namespace (`findInternedVar` checks `((Var) o).ns == this` to distinguish owned vs referred). `ns-unmap` → `unmap` → `map.without(sym)`.
- **Observed implementation:** `reference`: `while((o = map.valAt(sym)) == null) { IPersistentMap newMap = map.assoc(sym, val); mappings.compareAndSet(map, newMap); ... }` — stores the Var itself, no copy, no wrapper. `unmap`: `while(map.containsKey(sym)) { IPersistentMap newMap = map.without(sym); mappings.compareAndSet(map, newMap); }` — removes only the name→cell edge. The Var object survives; compiled classes that captured it as a constant keep calling through it; `var.ns`/`var.sym` still name it.
- **Semantic intent from docs/papers:** clojure.org/reference/namespaces: namespaces are "mappings from symbols to Vars" — the docs literally define the module map as name→cell.
- **What is verified:** Quotes at pinned SHA.
- **What is inferred:** The "compiled classes keep working after unmap" behavior follows from C2 (constant-pool Var reference) + this code; not separately executed.
- **Relevant invariant:** Import/export = *sharing of cell identity* across module maps. Unbinding a name never invalidates existing references; it only stops new name resolutions.
- **Consequence for Glia:** Answers the key question's aliasing clause: aliasing coherence (importer sees exporter's redefs) is a CELL property — it cannot be recovered from raw values plus late name-lookup unless every import re-looks-up in the *exporting* Defs by name (i.e., exports become `(Rc<Defs>, name)` pairs — which is just a two-word spelling of a binding cell). Glia's "exports = ordinary maps of the module's own bindings" gives frozen-at-export semantics; that's a defensible *choice*, but it forecloses Clojure-style live aliasing, and the Rc<Defs> in the export pair is exactly the "escaped → Strong" upgrade Glia already plans.
- **Confidence:** high
- **Classification:** CHALLENGES CURRENT DESIGN if live re-export is wanted; otherwise FUTURE TRACK.

---

### Clojure — key-question verdict (evidence-based)

Depends on the **CELL** (stable location): (1) redefinition visibility to already-compiled/escaped code (C2+C4: constant-pool Var + `getRawRoot` per call); (2) cross-namespace aliasing coherence (C5: `reference` stores the same Var); (3) survive-unmap behavior of existing code (C5); (4) dynamic binding/`with-redefs`/watch machinery (`deref`'s TBox check, `notifyWatches`); (5) intern-before-bind `declare` (C1: Unbound sentinel in a real cell). Depends only on **late lookup by name**: top-level forward references and mutual recursion *within an interpreter that resolves names at call time* — Clojure itself does NOT use per-call name lookup (names resolve to cells once, at compile time), so in Clojure even recursion runs through cells, but the recursion behavior per se would also be satisfied by per-call name lookup. Self-recursion needs neither (C3: `thisName` = self-reference).

### Clojure — runtime/binding matrix row

| column | Clojure |
|---|---|
| system | Clojure 1.13-master @ 2cea6925 |
| lexical capture representation | flat by-value copy into `final`-ish JVM instance fields at closure-construction (`ObjExpr.closes` → ctor `putField`) |
| persistent-definition representation | `Var` heap cell (`volatile Object root`) interned in namespace |
| raw value vs binding cell | **binding cell** (Var), with global `static volatile int rev` version counter |
| top-level late binding | name→cell once at compile; cell→value (`getRawRoot()`) every call; opt-out = direct linking |
| recursion mechanism | self: local-0 self reference (`thisName`); local mutual: post-construction field patching (letfn); top-level mutual: unbound-Var cell + late cell read |
| module instance representation | `Namespace` object: `AtomicReference<IPersistentMap>` mappings + aliases, CAS-updated persistent maps |
| export representation | shared cell identity — importer maps name → exporter's Var object (no copy) |
| callable identity | fn = fresh JVM class instance; Var itself implements IFn by forwarding `invoke() { return fn().invoke(); }` |
| lifetime mechanism | JVM tracing GC; Vars pinned by namespace map + class constant pools |
| cycle handling | free (tracing GC); letfn cycles created by construct-then-patch |
| native boundary | Java interop bypasses Vars entirely (static/virtual dispatch); `referenceClass` maps names to Class objects, not cells |
| WASM implications | model presumes cheap volatile reads + GC; on WASM (no GC in Glia) cells must be Rc'd and cycles handled manually |
| transferable lesson | resolve name→cell once, read cell per call; make redefinition = cell mutation + version bump; make import = cell sharing |

---

# SYSTEM 2: CHEZ SCHEME

**Pinned revision:** `814fa4e063665ef24a48a530ad5534c386c46501` (HEAD 2026-06-10, via api.github.com). Repository active.
**Verification method note:** cmacros.ss layouts verified via WebFetch and re-verified against the full pinned file. syntax.ss (10559 lines), interpret.ss, gc.c, mkgc.ss, cpnanopass.ss, cpprim.ss, prims.ss quotes verified against full files downloaded from the pinned raw URLs.

---

### Record Z1 — Top-level bindings are symbol value slots (the symbol IS the cell)

- **System:** Chez Scheme
- **Repository:** https://github.com/cisco/ChezScheme
- **Repository status:** active
- **Revision or commit:** 814fa4e063665ef24a48a530ad5534c386c46501
- **Files and symbols:** [s/cmacros.ss L1452](https://github.com/cisco/ChezScheme/blob/814fa4e063665ef24a48a530ad5534c386c46501/s/cmacros.ss#L1452) (symbol layout), [s/syntax.ss L386, L588-610, L6908-6960](https://github.com/cisco/ChezScheme/blob/814fa4e063665ef24a48a530ad5534c386c46501/s/syntax.ss#L6908), [s/cpprim.ss L3365-3377](https://github.com/cisco/ChezScheme/blob/814fa4e063665ef24a48a530ad5534c386c46501/s/cpprim.ss#L3365), s/prims.ss L1334-1342.
- **Runtime path traced:** top-level `define` → expander makes `(make-binding 'global label)` and emits `define-top-level-value-hook label ...` where `(define define-top-level-value-hook $set-top-level-value!)` → variable reference expands via `build-global-reference` → `$top-level-value` primcall → open-coded to a memory load off the symbol.
- **Observed implementation:** Symbol heap layout: `(define-primitive-structure-disps symbol type-symbol ([ptr value] [ptr pvalue] [ptr plist] [ptr name] ... [ptr splist] [ptr hash]))` — slot 0 is the binding cell. `build-global-reference`: `(build-primcall ae (if (or safe? (fx= (optimize-level) 3)) 3 2) '$top-level-value `(quote ,name))`; assignment: `'$set-top-level-value!`. Open-coding in cpprim.ss: `(bind #t ([t (%mref ,e ,(constant symbol-value-disp))]) `(if ,(%type-check mask-unbound sunbound ,t) ,(build-libcall ... $top-level-value e) ,t))` — one load + unbound-sentinel check. Public `top-level-value` (syntax.ss L6908+) resolves id→label through the environment's ribcage, then `case (binding-type b)`: `[(global immutable-global) (#2%$top-level-value (binding-value b))] [(library-global) (invoke-loaded-library (car (binding-value b))) (#2%$top-level-value (cdr (binding-value b)))]`.
- **Semantic intent from docs/papers:** CSUG documents `top-level-value`/`set-top-level-value!` as operating on locations in an environment; classic Lisp "value cell" design.
- **What is verified:** All quotes at pinned SHA, including the machine-level `symbol-value-disp` load.
- **What is inferred:** That interaction-environment labels for ordinary toplevels are the symbols themselves (strongly indicated by `define-top-level-value-hook label` with symbol labels and gensym uids for libraries; not chased through every ribcage case).
- **Relevant invariant:** There is exactly one canonical cell per top-level variable (a symbol/gensym value slot), referenced from arbitrarily many code objects; unbound is an in-cell sentinel (`$unbound-object`), checked at safe optimize levels and *skipped* (`#3%`) at unsafe.
- **Consequence for Glia:** Second independent mature system converging on stable cells + in-cell unbound sentinel for top level. Also a cheap design trick: Glia's `Rc<Binding>` is Chez's "symbol"; interning name→cell in Defs once and letting closures hold `Rc<Binding>` reproduces the one-load access path (`RefCell` borrow instead of `%mref`).
- **Confidence:** high
- **Classification:** CHALLENGES CURRENT DESIGN (raw-value Defs variant); CONFIRMS late-bound top level.

---

### Record Z2 — Flat closures capture VALUES; assigned variables are boxed (assignment conversion)

- **System:** Chez Scheme
- **Repository:** https://github.com/cisco/ChezScheme
- **Repository status:** active
- **Revision or commit:** 814fa4e0...
- **Files and symbols:** cmacros.ss L1527 (closure layout), [s/cpnanopass.ss L831-873](https://github.com/cisco/ChezScheme/blob/814fa4e063665ef24a48a530ad5534c386c46501/s/cpnanopass.ss#L831) (`np-convert-assignments`), [s/interpret.ss](https://github.com/cisco/ChezScheme/blob/814fa4e063665ef24a48a530ad5534c386c46501/s/interpret.ss) (`ip2-closure` L556, ip2-body "consers" L606-643, set! L385-397).
- **Runtime path traced:** compiler pipeline (L10852 order): `np-convert-assignments` boxes mutated locals → later closure passes capture plain values. Interpreter mirrors it independently.
- **Observed implementation:** Closure object: `(define-primitive-structure-disps closure type-closure ([ptr code] [ptr data 0]))` — code pointer + inline flat array of free-variable *values*. `np-convert-assignments`: refs `[,x (if (uvar-assigned? x) (%primcall #f #f car ,x) x)]`, writes `[(set! ,x ,[e]) (%primcall #f #f set-car! ,x ,e)]`, binding sites wrap in `(cons t (quote ,unbound-object))`. Interpreter identically: comment `; process the body and wrap in consers for assigned variables`, capture `[(closure) ($rt lambda () (vector-ref cp i))]` for unassigned vs `(car (vector-ref cp i))` for assigned; `ip2-closure` copies free values into a fresh vector (`(vector ($rt x1) ($rt x2))`).
- **Semantic intent from docs/papers:** Dybvig, "The Development of Chez Scheme" (ICFP '06): flat closures + assignment conversion so closures can copy values and share only explicit boxes.
- **What is verified:** All quotes at pinned SHA, in both compiled and interpreted pipelines.
- **What is inferred:** None material.
- **Relevant invariant:** Capture-by-value is made sound by a *pre-pass* that gives every mutated variable a one-word cell; immutable variables never get cells. Locations are the exception, not the rule.
- **Consequence for Glia:** Strongly CONFIRMS Glia's snapshot capture for lexicals, and gives the precise refinement: if Glia ever adds `set!`-able locals, don't abandon snapshots — box only assigned variables (Rc<RefCell<Val>> per assigned var), keep everything else by value. Also supports the hybrid answer for Defs: cells only where mutation/redefinition must be observed.
- **Confidence:** high
- **Classification:** CONFIRMS CURRENT DESIGN

---

### Record Z3 — letrec/mutual recursion: allocate closures, then patch slots (cycles are real)

- **System:** Chez Scheme
- **Repository:** https://github.com/cisco/ChezScheme
- **Repository status:** active
- **Revision or commit:** 814fa4e0...
- **Files and symbols:** cpnanopass.ss `np-expand-closures` L1948-2027 (`create-bindings`, `create-inits`), mkgc.ss L272-311 (closure trace rule, `code-flag-mutable-closure`), cpnanopass.ss L6573.
- **Runtime path traced:** `(closures ([x (free...) le] ...) body)` → `create-bindings` allocates every closure and sets only the code slot (`(set! ,(%mref ,(closure-name c) ,(constant closure-code-disp)) (label-ref ...))`) → `create-inits` then fills free slots: `(set! ,(%mref ,(closure-name c) ,i) ,(build-free-ref (car x*)))` — since all names are already bound to allocated closures, mutually recursive closures embed direct pointers to each other.
- **Observed implementation:** As quoted; the resulting object graph is genuinely cyclic. GC side (mkgc.ss): closures are normally swept as pure (`(trace-pure-ptrs closure-data len)`) unless `(& (code-type code) (<< code-flag-mutable-closure code-flags-offset))`, in which case impure `(trace-ptrs closure-data len)`; the mutable-closure flag at L6573 is set for `$make-wrapper-procedure`-style objects whose data slot can be updated later.
- **Semantic intent from docs/papers:** Standard fix-point closure allocation ("closure hoisting/knot-tying"); Waddell/Sarkar/Dybvig "Fixing Letrec" governs which bindings get this treatment.
- **What is verified:** Quotes at pinned SHA.
- **What is inferred:** That interpreter letrec reduces to the same shape via cpletrec (`(set! cpletrec-ran? #t)` observed in interpret.ss; pass itself not traced).
- **Relevant invariant:** Recursion among closures is object-graph cyclicity, tolerated because collection is tracing; "immutable" closures are still *initialized by mutation* in a construction window.
- **Consequence for Glia:** GC-DIFFERENCE with a transferable half: the construct-then-patch idiom needs only construction-window interior mutability (same as Clojure letfn), which Rc/RefCell supports — but the *resulting Rc cycle leaks* in Glia without weak edges. This is precisely the leak class Glia's OwnerRef Weak-when-resting rule targets; Chez shows the cycle is unavoidable if closures point at each other directly, and avoidable if mutual recursion is routed through Defs cells instead (cell→closure strong, closure→cell weak-or-strong per the write barrier).
- **Confidence:** high (code), medium (interpreter reduction detail)
- **Classification:** NOT TRANSFERABLE — GC DIFFERENCE (direct cyclic closures); the construction idiom itself REQUIRES A SPIKE if Glia wants local letrec.

---

### Record Z4 — Libraries: uid-labeled cells, immutable exported globals, invoke-on-demand

- **System:** Chez Scheme
- **Repository:** https://github.com/cisco/ChezScheme
- **Repository status:** active
- **Revision or commit:** 814fa4e0...
- **Files and symbols:** syntax.ss: binding-type table L930 (`library-global (uid . sym) immutable library variable`), L2768 (`(cons `(,label . ,(make-binding 'library-global (cons library-uid label))) env*)`), `install-library` module L4602-4756, top-level-value/set-top-level-value! cases L6918/L6951, boot-time value copying L5893-5900.
- **Runtime path traced:** library compile → each exported variable gets a gensym uid-label whose value slot holds the value after the library's invoke code runs → importer's environment maps id → `library-global (library-uid . label)` binding → reference = `(invoke-loaded-library (car ...))` then `$top-level-value` of the label; assignment: `[(library-global) ($oops 'set-top-level-value! "cannot assign immutable variable ~s" sym)]`. `install-library` only registers path/uid → libdesc (`(record-loaded-library path uid) (when desc (put-library-descriptor uid desc))`); invoke/visit code installed separately (`install-library/rt-code` sets `rtdesc-invoke-code-set!`).
- **Observed implementation:** As quoted. Also observed the *copy* variant used only for bootstrapping the system module: `($set-top-level-value! s ($top-level-value (binding-value label)))` (L5895) — value copying between cells exists but only where semantics are frozen by definition.
- **Semantic intent from docs/papers:** R6RS library semantics: exports immutable, instantiation on demand — the code implements exactly that.
- **What is verified:** Quotes at pinned SHA.
- **What is inferred:** Exact contents of libdesc/ctdesc/rtdesc records (accessors observed; record definitions in expand-lang.ss not fetched).
- **Relevant invariant:** Module export = (module-uid, cell-label) indirection, NOT a value copy; immutability from outside is enforced at the access path, not by copying; laziness (invoke-on-first-reference) is possible *because* the export is an indirection.
- **Consequence for Glia:** Nuances Glia's "exports = ordinary maps of the module's own bindings": Chez shows value-copy exports are the *bootstrap special case*, while the real mechanism is cell indirection with an enforced no-write rule — i.e., Glia can get "frozen exports" semantics without copies (Rc<Binding> + deny-write capability), preserving redefinition visibility as a policy knob instead of a structural impossibility. The `(uid . label)` pair also mirrors Glia's `OwnerRef` + name design; note Chez's export edge is strong and keeps the library alive — matching Glia's "Strong when escaped" rule.
- **Confidence:** high
- **Classification:** CHALLENGES CURRENT DESIGN (export-as-value-copy); CONFIRMS the escaped→Strong ownership upgrade.

---

### Record Z5 — Interpreter global calls: per-call cell read for user globals, snapshot for primitives

- **System:** Chez Scheme
- **Repository:** https://github.com/cisco/ChezScheme
- **Repository status:** active
- **Revision or commit:** 814fa4e0...
- **Files and symbols:** interpret.ss `docall-sym` L193-201, primref cases L253, L408-425.
- **Runtime path traced:** interpreted call to a top-level name → `docall-sym`: `(let ([t0 (#3%$top-level-value sym)] [t1 ($rt e1)] ...) (unless (procedure? t0) (unbound-or-non-procedure sym t0)) (t0 t1 ...))` — the cell is read at **every call**. Interpreted primref call → `(let ((e ($top-level-value (primref-name pr)))) ($rt lambda () (e ($rt x1))))` — the cell is read **once at closure-build time** and the value snapshotted.
- **Observed implementation:** As quoted; also plain primref value: `[,pr (let ((fun ($top-level-value (primref-name pr)))) ($rt lambda () fun))]`.
- **Semantic intent from docs/papers:** Primitives are stable-by-contract (at optimize level ≥2 the compiler inlines them outright — `build-primitive-reference` uses `lookup-primref` unless `$suppress-primitive-inlining`), so their snapshot is safe; user globals must remain redefinable.
- **What is verified:** Quotes at pinned SHA.
- **What is inferred:** None.
- **Relevant invariant:** One system, two binding policies, chosen by *stability class of the definition*: frozen namespace → snapshot values; live namespace → cell read per use.
- **Consequence for Glia:** Direct precedent for Glia's exact architecture split: the frozen shared prelude Defs can be snapshotted/raw-valued (Chez primrefs), while module-own Defs should be cell-mediated (Chez user globals). I.e., the answer to "raw values vs cells" can legitimately be *both*, keyed on frozen-ness — which Glia's design already distinguishes.
- **Confidence:** high
- **Classification:** CONFIRMS CURRENT DESIGN (frozen prelude snapshot) + CHALLENGES (raw values for live module Defs).

---

### Record Z6 — Weak pairs & ephemerons in the collector

- **System:** Chez Scheme
- **Repository:** https://github.com/cisco/ChezScheme
- **Repository status:** active
- **Revision or commit:** 814fa4e0...
- **Files and symbols:** cmacros.ss L1440 (ephemeron layout `([ptr car] [ptr cdr] [ptr prev-ref] [ptr next])` with comment "`prev-ref` and `next` are used by the GC"), s/prims.ss L57 (`weak-cons` → `s_weak_cons` foreign alloc), mkgc.ss trace rules L164-197 (`space-weakpair`: `(try-double-pair copy pair-car trace pair-cdr ...)`; `space-ephemeron`: `(add-ephemeron-to-pending)`), [c/gc.c](https://github.com/cisco/ChezScheme/blob/814fa4e063665ef24a48a530ad5534c386c46501/c/gc.c) `resweep_weak_pairs` L1869, `forward_or_bwp`, `check_ephemeron` L2717.
- **Runtime path traced:** weak pair car is *not* traced during the copy pass (`copy pair-car` vs `trace pair-cdr`); after sweeping, `resweep_weak_pairs` walks weakpair segments and `forward_or_bwp` either forwards the car or kills it: `if (FORWARDEDP(p, si)) { *pp = GET_FWDADDRESS(p); } else { *pp = Sbwp_object; }`. Ephemerons queue on their key's segment (`add_ephemeron_to_pending`); `check_ephemeron` re-traces car/cdr only once the key proves reachable (`relocate_impure(&INITCDR(pe), from_g)` after `new_marked`/`FORWARDEDP`).
- **Observed implementation:** As quoted; gc.c header comment: "the collector queues pending guardians and ephemerons on the segment where the [key resides] ... [when the key proves] to be reachable (i.e., copied or marked), the guardian/ephemeron is [processed]".
- **Semantic intent from docs/papers:** Dybvig/Bruggeman/Eby "Guardians in a Generation-Based Garbage Collector" (PLDI '93); ephemerons per Hayes to fix the key-in-value leak of plain weak pairs.
- **What is verified:** Quotes at pinned SHA.
- **What is inferred:** None material.
- **Relevant invariant:** Weakness is resolved *after* reachability is globally known (post-pass), so weak refs never resurrect; ephemerons express "value alive iff key alive", which pairwise weak/strong edges cannot.
- **Consequence for Glia:** Mostly GC-DIFFERENCE: Rust `Weak::upgrade` gives Glia the weak-pair half for free (dead → None ≈ `#!bwp`), and upgrades are decided instantaneously rather than by post-pass — fine in single-threaded Rc. But note the structural warning: Glia's OwnerRef Weak(Weak<Defs>) is a plain weak edge; if a Defs is kept alive only *through the value that rests in it* (key-in-value shape), Rc has no ephemeron analog and the manual write barrier is the only defense — worth a targeted test in the PR-1 suite.
- **Confidence:** high
- **Classification:** NOT TRANSFERABLE — GC DIFFERENCE (mechanism); FUTURE TRACK (ephemeron-shaped leak test for OwnerRef).

---

### Chez — runtime/binding matrix row

| column | Chez Scheme |
|---|---|
| system | Chez Scheme 10.x master @ 814fa4e0 |
| lexical capture representation | flat closure `{code, data[n]}`; free-variable **values** copied at construction; assigned vars pre-boxed (`cons` cells / assignment conversion) |
| persistent-definition representation | symbol/gensym **value slot** = the cell (`symbol` layout slot 0); unbound = in-cell sentinel |
| raw value vs binding cell | binding cell for user globals; raw snapshot/inlining for primitives (stability-class split) |
| top-level late binding | every reference/call loads `%mref sym symbol-value-disp` (safe mode adds unbound check; `#3%` skips it) |
| recursion mechanism | letrec: allocate-all-then-init-slots (real cycles); top-level mutual: through symbol cells |
| module instance representation | libdesc record registered by uid (`install-library`); invoke code run on first reference |
| export representation | `library-global (library-uid . label)` indirection to a gensym cell; immutable from outside; never a value copy (except boot-time system module) |
| callable identity | closure object (code ptr + data); wrapper procedures carry arity mask; `code-flag-mutable-closure` marks patchable ones |
| lifetime mechanism | generational copying/marking tracing GC (BiBOP segments) |
| cycle handling | free (tracing); weak pairs resolved by post-pass resweep (`forward_or_bwp` → `#!bwp`); ephemerons via pending queues keyed on segments |
| native boundary | `foreign-procedure` (e.g. `weak-cons` itself); locks/immobility for pointers handed to C |
| WASM implications | whole design assumes relocating tracing GC + raw address arithmetic; the *policy* layer (cells for live names, snapshots for frozen) ports; the mechanism doesn't |
| transferable lesson | box only what mutates; make export = indirection + write-prohibition, not copy; split binding policy by frozen vs live namespace |

---

## Cross-system answer to the key open question (evidence summary, no recommendation)

Both systems, independently and with different GC regimes, converge on **stable binding cells for live top-level definitions** (Var; symbol value slot) and **raw values only for frozen strata** (Clojure direct linking of non-`^:redef` vars; Chez primref snapshot/inlining). Behaviors that the traced code shows depend on the CELL: redefinition visibility to escaped/compiled code, aliasing/re-export coherence, declare-before-define with idempotent identity, survive-unmap of existing callers, and O(1) per-use access without name hashing. Behaviors needing only late lookup by name: top-level forward reference and mutual recursion, and unmap-blocks-new-resolution. Behaviors needing neither: self-recursion (both systems use self-reference), lexical capture (both snapshot values; Chez boxes only assigned vars).

Working files (pinned downloads used for verification, held in a temporary local study directory and not archived): `Compiler.java`, `cmacros.ss`, `syntax.ss`, `interpret.ss`, `gc.c`, `mkgc.ss`, `cpnanopass.ss`, `cpprim.ss`, and `prims.ss`.
