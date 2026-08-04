All fetches complete. Below are the source-first records. Everything quoted was read from the pinned revisions listed; layer attribution (expander / BC / CS glue / Chez) is stated per Racket record.

# Pinned revisions

- **Racket**: `racket/racket` @ `2706d5c2e41da655d37cc4738c26325daf3c9512` (commit date 2026-08-03T00:01:08Z, via api.github.com/repos/racket/racket/commits?per_page=1)
- **Lua**: `lua/lua` @ `7579fc9d7ed90240487251dfb69168f8e64e9294` (commit date 2026-07-23T16:58:30Z, "variable scoping in repeat-until", via api.github.com/repos/lua/lua/commits?per_page=1)

All file links below are `https://raw.githubusercontent.com/<org>/<repo>/<sha>/<path>`.

---

## RACKET RECORDS

### R1 — Expander: namespace records and per-phase `definitions`

- **System:** Racket — layer (a) expander (racket/src/expander)
- **Repository:** https://github.com/racket/racket
- **Repository status:** active upstream; fetched raw at pinned sha
- **Revision or commit:** 2706d5c2e41da655d37cc4738c26325daf3c9512
- **Files and symbols:** `racket/src/expander/namespace/namespace.rkt` — `(struct namespace ...)`, `(struct definitions ...)`, `namespace->definitions`, `namespace-set-variable!`, `namespace-get-variable`; `racket/src/expander/namespace/registry.rkt` — `(struct module-registry (declarations lock-box))`
- **Runtime path traced:** top-level/module variable definition and lookup through a namespace at a given phase level.
- **Observed implementation (short quotes):**
  - `(struct namespace (... phase-level-to-definitions ; phase-level -> definitions [shared for the same module instance] ... module-instances) ...)`
  - `(struct definitions (variables ; linklet instance` / `transformers) ; sym -> val` `#:authentic)`
  - `namespace->definitions` lazily does `(definitions (make-instance (namespace->name p-ns) p-ns) (make-hasheq))` and caches per phase-level.
  - `(define (namespace-set-variable! ns phase-level name val [as-constant? #f]) ... (instance-set-variable-value! (definitions-variables d) name val ...))`
  - `(module-registry (make-hasheq) (box #f))` — resolved-module-path → module.
- **Semantic intent from docs/papers:** namespaces are phase-indexed views over a module instance's definitions (Racket Reference "Namespaces"; Flatt, "Binding as Sets of Scopes" for the surrounding model).
- **What is verified:** runtime *variables* live in a linklet **instance** (name → variable cell object; see R3), while *transformers* (macros) live in a plain mutable `hasheq`. One `definitions` record per (module instance, phase level), lazily created, shared across same-instance namespaces.
- **What is inferred:** nothing material; all quoted.
- **Relevant invariant:** value-bindings and macro-bindings have different representations; only value-bindings get cells.
- **Consequence for Glia:** `Defs` ≈ the `definitions` record: an owner object holding name→binding for one instantiation scope. Racket splits "cells for values" from "plain map for expander-only bindings"; Glia can keep macro/special tables as ordinary maps regardless of the raw-value-vs-cell decision.
- **Confidence:** high (direct struct quotes)
- **Classification:** CONFIRMS CURRENT DESIGN
- Link: https://raw.githubusercontent.com/racket/racket/2706d5c2e41da655d37cc4738c26325daf3c9512/racket/src/expander/namespace/namespace.rkt

### R2 — Expander: top-level binding is deferred to evaluation time

- **System:** Racket — layer (a) expander
- **Repository / status / revision:** as R1
- **Files and symbols:** `racket/src/expander/expand/bind-top.rkt` — `as-expand-time-top-level-bindings`, `top-level-bind-scope`, `select-defined-syms-and-bind!/ctx`
- **Runtime path traced:** expansion of `(define-values (x) ...)` at the REPL/top level.
- **Observed implementation (short quotes):** the function adds a distinct `top-level-bind-scope` to definition ids (`(add-scope id top-level-bind-scope)`); file comments state that `x` needs "a binding to give the definition an expand-time meaning" but "the permanent binding should happen when the `define-values` form is evaluated", using "a distinct scope that effectively hides the binding from tasks other than expansion".
- **Semantic intent from docs/papers:** top-level is deliberately late-bound; expansion must not commit the binding.
- **What is verified:** the expander creates only a scoped, expansion-visible binding; the durable binding is installed by evaluating the definition (which writes the instance variable per R1/R3).
- **What is inferred:** interaction with redefinition semantics (not traced further).
- **Relevant invariant:** compile references by name against the namespace; resolution to a value happens at reference/definition evaluation time.
- **Consequence for Glia:** top-level late binding through the `Defs` owner (lookup at call/reference time, not capture time) is the standard, verified Racket approach.
- **Confidence:** high
- **Classification:** CONFIRMS CURRENT DESIGN
- Link: https://raw.githubusercontent.com/racket/racket/2706d5c2e41da655d37cc4738c26325daf3c9512/racket/src/expander/expand/bind-top.rkt

### R3 — CS glue: linklet `variable` records = binding cells with weak owner backlink

- **System:** Racket — layer (c) Racket CS glue (racket/src/cs), Scheme code running on Chez
- **Repository / status / revision:** as R1
- **Files and symbols:** `racket/src/cs/linklet.sls` — `define-record variable`, `variable-set!`, `variable-ref`, `variable-ref/no-check`, `make-instance`
- **Runtime path traced:** linklet instantiation → per-name variable creation → read/write of a module-level/top-level variable.
- **Observed implementation (short quotes):**
  ```scheme
  (define-record variable (val
                           name
                           source-name
                           constance  ; #f (mutable), 'constant, or 'consistent (always the same shape)
                           inst-box)) ; weak pair with instance in `car`
  ```
  `variable-ref` checks `(eq? v variable-undefined)` and raises; `make-instance` builds an eq hashtable and per name does `(eq-hashtable-set! raw-ht (car content) (make-variable (cadr content) name name constance inst-box))` where `inst-box` is `(weak-cons inst #f)`.
- **Semantic intent from docs/papers:** linklet model (Flatt et al., "Rebuilding Racket on Chez Scheme", ICFP 2019): instances map symbols to variables; imports link to the exporter's variable.
- **What is verified:** in Racket CS, every module-level/top-level binding is a **first-class mutable cell** (`variable` record), stored in a sym→variable hashtable per instance; unbound = sentinel checked at reference; **each cell carries a WEAK backlink to its owning instance** (`inst-box` weak pair).
- **What is inferred:** import wiring shares the identical `variable` object across instances (strongly implied by the record shape and linklet docs; the exact linking code path was not quoted).
- **Relevant invariant:** cell identity is stable across redefinition; owner backlink is weak so an escaped cell does not keep its instance alive (GC-backed).
- **Consequence for Glia:** direct precedent for the `name→Rc<Binding>` cell option in `Defs`, including a weak cell→owner backlink structurally identical to `OwnerRef::Weak`. Both production Racket runtimes chose cells (see R5) for late-bound definitions while closures snapshot values (R4). This is the strongest single data point on the open question: cells for `Defs`, raw snapshots for lexical capture.
- **Confidence:** high
- **Classification:** CONFIRMS CURRENT DESIGN (and resolves the open raw-value-vs-cell question toward cells for late-bound names)
- Link: https://raw.githubusercontent.com/racket/racket/2706d5c2e41da655d37cc4738c26325daf3c9512/racket/src/cs/linklet.sls

### R4 — BC: flat closures copy VALUES; mutation via compiler-inserted boxes

- **System:** Racket — layer (b) Racket BC runtime (racket/src/bc)
- **Repository / status / revision:** as R1
- **Files and symbols:** `racket/src/bc/src/schpriv.h` — `Scheme_Lambda` (`closure_size`, `closure_map`), `Scheme_Closure`, `SCHEME_CLOSURE_ENV`; `racket/src/bc/src/eval.c` — `scheme_make_closure` (line ~2003), `bangboxenv_execute` (line ~1949)
- **Runtime path traced:** evaluation of a `lambda` → closure allocation → capture; `set!` on a captured local.
- **Observed implementation (short quotes):**
  ```c
  typedef struct Scheme_Closure {
    Scheme_Object so;
    Scheme_Lambda *code;
    Scheme_Object *vals[mzFLEX_ARRAY_DECL];
  } Scheme_Closure;
  ```
  `mzshort *closure_map; /* after resolve pass: contains closure_size elements mapping closed-over var to stack positions. ... */`
  In `scheme_make_closure`: `dest = closure->vals; map = data->closure_map; /* Copy data into the closure: */ while (i--) { dest[i] = runstack[map[i]]; }`
  `bangboxenv_execute`: `/* A bangboxenv step is inserted by the compilation of 'lambda' and 'let' forms where an argument or bindings is set!ed in the body. */ ... bb = scheme_make_envunbox(MZ_RUNSTACK[pos]); MZ_RUNSTACK[pos] = bb;`
- **Semantic intent from docs/papers:** classic flat-closure + assignment conversion (Dybvig; Cardelli/Rabbit lineage): copy immutable values, box anything mutated so all sharers alias one cell.
- **What is verified:** BC closures are flat arrays of captured **values** copied off the runstack at closure-creation time; mutable locals are converted to boxes *before* capture, so the "value" captured is the box.
- **What is inferred:** letrec specifics (not traced).
- **Relevant invariant:** capture-by-snapshot is semantics-preserving iff every mutated-and-shared binding has been converted to a cell first.
- **Consequence for Glia:** Glia's snapshot capture of lexical values is exactly BC's model for immutable locals. If Glia locals can be mutated after capture (or two closures must observe each other's writes), it needs assignment conversion to `Rc<RefCell<Val>>` cells — a compiler/analysis decision, not a change to the snapshot mechanism.
- **Confidence:** high
- **Classification:** CONFIRMS CURRENT DESIGN
- Links: https://raw.githubusercontent.com/racket/racket/2706d5c2e41da655d37cc4738c26325daf3c9512/racket/src/bc/src/schpriv.h , https://raw.githubusercontent.com/racket/racket/2706d5c2e41da655d37cc4738c26325daf3c9512/racket/src/bc/src/eval.c

### R5 — BC: toplevel buckets are cells with a weak-by-default / strong-when-flagged home link

- **System:** Racket — layer (b) Racket BC runtime
- **Repository / status / revision:** as R1
- **Files and symbols:** `racket/src/bc/include/scheme.h` — `Scheme_Bucket`; `racket/src/bc/src/eval.c` — `global_lookup` macro in the `scheme_toplevel_type` case, `unbound_global`; `racket/src/bc/src/schpriv.h` — `GLOB_STRONG_HOME_LINK`, `Scheme_Bucket_With_Home`; `racket/src/bc/src/linklet.c` — `scheme_get_bucket_home`, `scheme_set_bucket_home`, `make_bucket`, `scheme_instance_variable_bucket`
- **Runtime path traced:** bytecode reference to a top-level/module variable → prefix array → bucket → value; bucket↔instance ownership.
- **Observed implementation (short quotes):**
  ```c
  typedef struct Scheme_Bucket { Scheme_Object so; void *val; char *key; } Scheme_Bucket;
  ```
  ```c
  #define global_lookup(prefix, _obj, tmp) \
    tmp = RUNSTACK[SCHEME_TOPLEVEL_DEPTH(_obj)]; \
    tmp = ((Scheme_Prefix *)tmp)->a[SCHEME_TOPLEVEL_POS(_obj)]; \
    tmp = (Scheme_Object *)(SCHEME_VAR_BUCKET(tmp))->val; \
    if (!tmp) { ... unbound_global(_obj); return NULL; } ...
  ```
  ```c
  /* whether home_link is strong or weak: */
  #define GLOB_STRONG_HOME_LINK 4
  typedef struct { Scheme_Bucket_With_Ref_Id bucket;
    Scheme_Object *home_link; /* weak to Scheme_Instance *, except when GLOB_STRONG_HOME_LINK */
  } Scheme_Bucket_With_Home;
  ```
  `scheme_get_bucket_home`: `if (flags & GLOB_STRONG_HOME_LINK) return (Scheme_Instance *)l; else return (Scheme_Instance *)SCHEME_WEAK_BOX_VAL(l);`
  Also `#define GLOB_IS_LINKED 128 /* Linked from other (cannot be undefined) */`.
- **Semantic intent from docs/papers:** buckets are the mutable cells behind top-level/module variables; late binding + unbound-at-use errors; the home link exists so a bucket can name its defining instance without (usually) keeping it alive.
- **What is verified:** (i) definitions are cells (`bucket->val`), dereferenced at every reference, with the unbound check at use time; (ii) the cell→owner backlink is **weak by default and strong only when explicitly flagged** at bucket-creation time, and imports mark buckets `GLOB_IS_LINKED`.
- **What is inferred:** the policy for when BC sets `GLOB_STRONG_HOME_LINK` beyond the `#f`-keyed self bucket seen in `scheme_instance_variable_bucket` (only that one site quoted).
- **Relevant invariant:** a cell may outlive its owner (weak link returns NULL) — safe only because a tracing GC keeps the *cell itself* alive via whoever holds it.
- **Consequence for Glia:** this is the closest production analogue of the amended-Graph-4 `OwnerRef{Weak at rest | Strong when escaped}` — but note the difference: BC's strong/weak choice is fixed at creation by role and the GC guarantees no dangling cell; Glia's design flips the same bit dynamically via a manual write barrier with no collector backstop. The *shape* is confirmed; the *manual flipping discipline* has no precedent here and still needs its own validation.
- **Confidence:** high for mechanism; medium for policy coverage
- **Classification:** CONFIRMS CURRENT DESIGN (mechanism); the manual dynamic flip itself REQUIRES A SPIKE
- Links: https://raw.githubusercontent.com/racket/racket/2706d5c2e41da655d37cc4738c26325daf3c9512/racket/src/bc/src/linklet.c , https://raw.githubusercontent.com/racket/racket/2706d5c2e41da655d37cc4738c26325daf3c9512/racket/src/bc/include/scheme.h

### R6 — BC: weak tables/ephemerons are GC services

- **System:** Racket — layer (b) BC runtime, GC-dependent
- **Repository / status / revision:** as R1
- **Files and symbols:** `racket/src/bc/src/hash.c` (lines ~797–860) — `SCHEME_BT_KIND_WEAK/LATE/EPHEMERON`, `GC_malloc_weak_box`, `scheme_weak_reference_indirect`; `racket/src/bc/include/scheme.h` — `SCHEME_hash_weak_ptr`, `SCHEME_hash_late_weak_ptr`, `SCHEME_hash_ephemeron_ptr`; `Scheme_Bucket_Table.weak /* 1 => normal weak, 2 => late weak */`
- **Runtime path traced:** creation of weak buckets in weak hash tables.
- **Observed implementation (short quotes):** `kb = GC_malloc_weak_box((void *)key, (void **)bucket, (void **)&bucket->val ..., (table->weak == SCHEME_BT_KIND_LATE));` — weakness (including ephemeron key→value liveness coupling) is implemented by the collector, not by table code.
- **Semantic intent:** avoid table-induced retention; ephemerons break the key-in-value cycle problem.
- **What is verified:** all weak-binding behavior in BC bottoms out in GC primitives.
- **What is inferred:** none.
- **Relevant invariant:** weak semantics require a collector that nulls references at a well-defined time.
- **Consequence for Glia:** none directly; Glia's `Weak` is `rc::Weak`, whose upgrade-fails-after-drop semantics are deterministic and simpler than GC weak boxes, but there is no ephemeron analogue if a key-value liveness coupling is ever needed.
- **Confidence:** high
- **Classification:** NOT TRANSFERABLE — GC DIFFERENCE
- Link: https://raw.githubusercontent.com/racket/racket/2706d5c2e41da655d37cc4738c26325daf3c9512/racket/src/bc/src/hash.c

*(Layer (d) note: actual CS closure layout is inside Chez Scheme (racket/ChezScheme repo), not inspected here; no claims made about it.)*

---

## LUA RECORDS

### L1 — `UpVal`: captured LOCATION that becomes a captured VALUE when closed

- **System:** Lua (5.5-work, upstream master)
- **Repository:** https://github.com/lua/lua
- **Repository status:** active upstream mirror; fetched raw at pinned sha
- **Revision or commit:** 7579fc9d7ed90240487251dfb69168f8e64e9294
- **Files and symbols:** `lobject.h` — `UpVal`, `LClosure`, `CClosure`, `ClosureHeader`; `lfunc.h` — `upisopen`, `uplevel`
- **Runtime path traced:** representation of a captured variable over its lifetime.
- **Observed implementation (short quotes):**
  ```c
  typedef struct UpVal {
    CommonHeader;
    union {
      TValue *p;  /* points to stack or to its own value */
      ptrdiff_t offset;  /* used while the stack is being reallocated */
    } v;
    union {
      struct {  /* (when open) */
        struct UpVal *next;  /* linked list */
        struct UpVal **previous;
      } open;
      TValue value;  /* the value (when closed) */
    } u;
  } UpVal;
  ```
  ```c
  typedef struct LClosure { ClosureHeader; struct Proto *p; UpVal *upvals[1]; /* list of upvalues */ } LClosure;
  typedef struct CClosure { ClosureHeader; lua_CFunction f; TValue upvalue[1]; /* list of upvalues */ } CClosure;
  ```
  `#define upisopen(up) ((up)->v.p != &(up)->u.value)`
- **Semantic intent from docs/papers:** Ierusalimschy, de Figueiredo, Celes, "Closures: the implementation of Lua 5.0" — upvalues capture the variable (location) while it is on the stack, then migrate the value into the upvalue on scope exit.
- **What is verified:** a Lua closure captures a pointer to a cell (`UpVal*`); the cell aliases the stack slot while open and owns the value when closed — all reads/writes go through the single indirection `uv->v.p` in both states. C closures, by contrast, embed `TValue`s **by value** (no cells).
- **What is inferred:** none.
- **Relevant invariant:** access code is state-agnostic: `v.p` always points at the current home of the value.
- **Consequence for Glia:** Lua demonstrates that location-capture is only needed to let closures observe *post-capture mutation* of enclosing locals; Glia's snapshot capture is exactly Lua's "closed" state taken eagerly. If Glia locals are immutable after capture, snapshot ≡ Lua semantics. If shared mutation between sibling closures is ever wanted, the cell (`Rc<RefCell<Val>>` ≈ closed `UpVal`) is the unit to introduce — not whole-environment capture.
- **Confidence:** high
- **Classification:** CONFIRMS CURRENT DESIGN
- Link: https://raw.githubusercontent.com/lua/lua/7579fc9d7ed90240487251dfb69168f8e64e9294/lobject.h

### L2 — Upvalue identity: one cell per stack slot, shared, then closed in place

- **System:** Lua
- **Repository / status / revision:** as L1
- **Files and symbols:** `lfunc.c` — `luaF_findupval`, `newupval`, `luaF_closeupval`, `luaF_unlinkupval`, `luaF_close`; `lvm.c` — `pushclosure`, `OP_CLOSURE`, `OP_CLOSE`, `OP_GETUPVAL`, `OP_SETUPVAL`
- **Runtime path traced:** closure creation over a live local; scope exit closing the upvalue.
- **Observed implementation (short quotes):**
  - `luaF_findupval` walks `L->openupval` sorted by stack level: `if (uplevel(p) == level) return p;` else `return newupval(L, level, pp);` — **at most one UpVal per stack slot**.
  - `pushclosure`: `if (uv[i].instack) ncl->upvals[i] = luaF_findupval(L, base + uv[i].idx); else ncl->upvals[i] = encup[uv[i].idx];` — non-local captures **reuse the enclosing closure's cells** (flat closure of cell pointers).
  - `luaF_closeupval`: `setobj(L, slot, uv->v.p); /* move value to upvalue slot */ uv->v.p = slot; /* now current value lives here */` plus `luaC_barrier(L, uv, slot)`.
  - `OP_SETUPVAL`: `setobj(L, uv->v.p, s2v(ra)); luaC_barrier(L, uv, s2v(ra));`
- **Semantic intent from docs/papers:** same Lua 5.0 paper: sharing identity of the *variable*, not the value, across all closures capturing it.
- **What is verified:** cell dedup by stack level; sharing preserved through close (all holders keep the same `UpVal*`, whose interior redirects); one fresh `LClosure` per `OP_CLOSURE` execution (no closure cache in `pushclosure` at this revision).
- **What is inferred:** none.
- **Relevant invariant:** dedup happens at capture time (linear list keyed by location); closing never changes cell identity, only where the value lives.
- **Consequence for Glia:** if binding cells (`Rc<Binding>`) are adopted in `Defs`, the Lua invariant to copy is *stable cell identity across state changes* (redefinition ≈ re-close): holders never re-resolve, the cell's interior changes. This is also the argument for cells over raw values wherever Glia wants redefinition to be visible to prior capturers.
- **Confidence:** high
- **Classification:** CONFIRMS CURRENT DESIGN
- Links: https://raw.githubusercontent.com/lua/lua/7579fc9d7ed90240487251dfb69168f8e64e9294/lfunc.c , https://raw.githubusercontent.com/lua/lua/7579fc9d7ed90240487251dfb69168f8e64e9294/lvm.c

### L3 — `_ENV`: globals are ordinary table lookups through a captured cell

- **System:** Lua
- **Repository / status / revision:** as L1
- **Files and symbols:** `lparser.c` — `mainfunc` (creates upvalue 0 named `LUA_ENV`, `env->instack = 1; env->idx = 0;`), `buildglobal` (`luaK_indexed(fs, var, &key); /* 'var' represents _ENV[varname] */`); `lvm.c` — `OP_GETTABUP`, `OP_SETTABUP`
- **Runtime path traced:** compilation and execution of a global variable read/write.
- **Observed implementation (short quotes):**
  - `mainfunc` comment: `/* compiles the main function, which is a regular vararg function with an upvalue named LUA_ENV */`
  - `OP_GETTABUP`: `TValue *upval = cl->upvals[GETARG_B(i)]->v.p; ... luaV_fastget(upval, key, s2v(ra), luaH_getshortstr, tag);`
  - `OP_SETTABUP`: `TValue *upval = cl->upvals[GETARG_A(i)]->v.p; ... luaV_fastset(upval, key, rc, hres, luaH_psetshortstr);`
- **Semantic intent from docs/papers:** Lua 5.2+ manual: free names are sugar for `_ENV.name`; there is no global scope in the VM, only a conventionally-threaded upvalue holding a table.
- **What is verified:** every "global" access is a runtime map lookup (late binding, per access) through a captured cell whose value is the environment table; environments are first-class and swappable per closure.
- **What is inferred:** module convention (`require` returning a table) — from the manual, not traced in C.
- **Relevant invariant:** the *cell* gives which environment; the *map* gives which binding; neither is resolved before use.
- **Consequence for Glia:** validates the proposed combination exactly: ordinary-map module exports + top-level late binding via lookup in a `Defs`-like owner, with the closure holding only a reference to that owner (Glia's `OwnerRef` plays the role of the `_ENV` upvalue slot). Lua shows this costs one hash lookup per global access — the reason to consider caching `Rc<Binding>` cells per callsite if profiling ever demands it.
- **Confidence:** high
- **Classification:** CONFIRMS CURRENT DESIGN
- Link: https://raw.githubusercontent.com/lua/lua/7579fc9d7ed90240487251dfb69168f8e64e9294/lparser.c

### L4 — GC: closure/upvalue traversal and cycle handling

- **System:** Lua
- **Repository / status / revision:** as L1
- **Files and symbols:** `lgc.c` — `traverseLclosure`, `traverseCclosure`, `remarkupvals`, `reallymarkobject` (`LUA_VUPVAL` case), `twups` list
- **Runtime path traced:** GC marking of closures, open upvalues, and the closure→`_ENV`→closure cycle.
- **Observed implementation (short quotes):**
  - `traverseLclosure`: `markobjectN(g, cl->p); for (...) { UpVal *uv = cl->upvals[i]; markobjectN(g, uv); }`
  - `reallymarkobject`: `if (upisopen(uv)) set2gray(uv); /* open upvalues are kept gray */ else set2black(uv); markvalue(g, uv->v.p);`
  - Comment: "Open upvalues are already indirectly linked through their respective threads in the 'twups' list, so they don't go to the gray list; nevertheless, they are kept gray to avoid barriers, as their values will be revisited by the thread or by 'remarkupvals'."
  - `remarkupvals` walks `g->twups`, and for unmarked threads: `if (!iswhite(uv)) { ... markvalue(g, uv->v.p); /* mark its value */ }`
- **Semantic intent from docs/papers:** incremental tri-color GC where an open upvalue's value slot is also a stack slot owned by a possibly-dead thread; cycles (e.g. a function stored in `_ENV` closing over `_ENV`) are handled by ordinary tracing.
- **What is verified:** the closure graph is cyclic by construction in every nontrivial Lua program, and only the tracing collector makes that safe; open upvalues get bespoke gray/twups treatment because their storage aliases a stack.
- **What is inferred:** none.
- **Relevant invariant (transferable part):** the *access* representation (one indirection through `v.p`) is fully decoupled from the *lifetime* machinery; all GC complexity is confined to marking.
- **Consequence for Glia:** the cycle Lua's GC absorbs (callable stored in environment ↔ environment referenced by callable) is exactly the cycle Glia's weak-at-rest `OwnerRef` must break by hand, since Rc cannot collect it. The location-vs-value capture lesson transfers; the cycle-tolerance does not — every place Lua may freely store a closure into the table it captured is, in Glia, a mandatory write-barrier site. That enumeration (all paths by which a callable can come to rest in its own `Defs`, including via containers and other closures) is the correctness surface of the manual barrier.
- **Confidence:** high
- **Classification:** NOT TRANSFERABLE — GC DIFFERENCE (with the location-vs-value and cycle-enumeration lessons extracted above)
- Link: https://raw.githubusercontent.com/lua/lua/7579fc9d7ed90240487251dfb69168f8e64e9294/lgc.c

---

## RUNTIME/BINDING MATRIX

**Racket** (layer annotated; sha 2706d5c2…):
- lexical capture representation: BC — flat closure, values copied from runstack via `closure_map` (`Scheme_Closure.vals`); mutable locals pre-boxed by `bangboxenv` (verified). CS — Chez flat closures (layer d, not inspected; no claim).
- persistent-definition representation: linklet instance = sym→cell table (CS `variable` record; BC `Scheme_Bucket_With_Home`), wrapped per phase by expander `definitions`.
- raw value vs binding cell: **cells** for top-level/module definitions; **raw values** in closures except set!-ed locals (boxed cells).
- top-level late binding: yes — cell deref per reference, unbound checked at use (`global_lookup` / `variable-undefined`); expander defers permanent binding to evaluation (bind-top.rkt).
- recursion mechanism: top-level/module recursion via late-bound cells (verified); local letrec not traced.
- module instance representation: expander `module-instance` (namespace + phase-level state + data-box); runtime instance = sym→variable/bucket table.
- export representation: expander `module.provides` = phase-level → sym → binding; runtime imports link to exporter's cell (`GLOB_IS_LINKED`).
- callable identity: fresh `Scheme_Closure` per closure creation; empty closures shareable (`scheme_malloc_empty_closure`).
- lifetime mechanism: tracing GC (BC gc2 / Chez).
- cycle handling: GC; retention tuned with weak backlinks (bucket `home_link`, variable `inst-box` weak pair).
- native boundary: C primitives and buckets are C structs shared directly with Scheme objects (BC).
- WASM implications: both runtimes presuppose tracing GC + weak boxes; a no-GC port must replace weak-owner-backlink semantics explicitly (what OwnerRef does).
- transferable lesson: **cells with weak owner backlinks for late-bound definitions; value snapshots for lexical capture; strongness of the owner link is an explicit per-cell bit.**

**Lua** (sha 7579fc9…):
- lexical capture representation: per-variable `UpVal` cell; open = pointer into stack (location), closed = owns `TValue`; `LClosure` holds `UpVal*` array.
- persistent-definition representation: none in VM; globals are fields of the `_ENV` table.
- raw value vs binding cell: locals raw on stack; captured locals become cells (UpVal); globals are map entries (neither raw-in-closure nor per-name cells); C-closure upvalues are raw embedded values.
- top-level late binding: yes — every global access is a table lookup at execution (`OP_GETTABUP`/`OP_SETTABUP`).
- recursion mechanism: global recursion via late `_ENV` lookup (verified mechanics); local recursion via upvalue capture patterns (not traced).
- module instance representation: a table (convention, manual; not traced in C).
- export representation: table fields.
- callable identity: fresh `LClosure` per `OP_CLOSURE` execution (no cache in `pushclosure` at this revision); upvalue cells deduped by stack level (`luaF_findupval`).
- lifetime mechanism: incremental/generational tracing GC.
- cycle handling: tracing GC; open upvalues kept gray + `twups`/`remarkupvals` atomic-phase fixup (GC-internal).
- native boundary: `CClosure` embeds upvalues **by value** — the C boundary silently switches from location-capture to value-capture.
- WASM implications: interpreter brings its own GC; model is self-contained, no linear-memory weak-ref dependency.
- transferable lesson: **capture the location only while mutation must be observable, then "close" into an owned cell without changing cell identity; environments as captured first-class maps give late binding for free.**

---

**Cross-cutting raw observations for the Glia open question** (no recommendation, data only): (1) Racket CS `variable`, Racket BC bucket, and Lua closed `UpVal` are all per-name/per-variable heap cells with stable identity across mutation; (2) both Racket runtimes attach the owner backlink to the **cell**, weak by default, strong by explicit flag set at creation — Glia's proposal attaches it to the **callable** and flips it dynamically; no inspected system flips strength at runtime; (3) all three systems put raw values (not cells) inside closures whenever the binding is immutable-after-capture; (4) every inspected system relies on tracing GC to make the environment↔callable cycle safe — none demonstrates the manual write-barrier discipline, so that specific mechanism has no precedent in these codebases and remains spike territory.