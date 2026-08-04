# PR-1b.0 — definition-ownership resolution (design only; no code/docs edited)

Resolves the five open questions over the locked structural model. Companion: `.context/pr1b-definition-ownership.md`.

## 1. `def` inside functions — **Model B: top-level-only definitions**

Repository search: **zero uses** of def-family forms inside function bodies in any `.glia` source (std/lib, examples, prelude) or test program. The single historical attempt is ww#574 — module registration from inside `deftest` — which *silently failed* under the current model and was replaced by the atom idiom (`test.glia:15-20`). Classification: one workaround-generating accident; no intentional uses; no test artifacts; otherwise no dependency found.

Evaluation highlights (full grid in-line): Model A (define-anywhere, lexical owner) matches Clojure precedent but creates a standing wart — the export map is a completion-time snapshot while the owner stays live, so post-import calls silently diverge the two views — and gives every function the ambient power to mutate its module's definition surface (against the possession model; bad for AI-authored code). Model B seals module surfaces at completion (map ≡ owner forever), keeps REPL ergonomics intact (the REPL *is* top level: definitions, hot redefinition, conditional defs via top-level macro expansion all work), needs no analyzer work (a `defining: bool` on `Env`, cleared by `for_call`; violation → structured catchable `glia.error/def-not-top-level` exception, consistent with the staging model), and drastically simplifies authority accounting: module owners are effectively frozen after evaluation, so closure cap-status is stable. Recursion never needed def-inside-fn. Clojure compatibility explicitly does not outweigh this (per the decision standard). **Recommend B.** (Function-body state = `let`, atoms, capabilities — the existing idioms.)

## 2. Exact ownership graph — **Graph 4: resting-weak / escaping-strong owner references**

Graph 1 (strong closure→owner) is rejected by the decision standard: every interned function would cycle — recursion is routine. Graph 2 (pure weak) fails survival: nothing strong remains after import returns, so exported closures die. Graph 3 reintroduces either a special module value or the same cycle via `Rc<ModuleInstance>`. **Graph 4** breaks the dilemma by making the owner reference *positional*:

```rust
pub(crate) enum OwnerRef { Strong(Rc<Defs>), Weak(std::rc::Weak<Defs>) }

// lib.rs — no external code constructs/destructures these (verified):
Val::Fn    { arities, env: Rc<Env> /* lexical-only */, owner: OwnerRef, is_cap_free, cap_violation }
Val::Macro { …same… }

pub(crate) struct Defs {
    bindings: RefCell<HashMap<String, Binding>>,   // Binding { value: Val, has_callables: bool }
    inherited: Option<Rc<Defs>>,                   // module → shared prelude → none
}
pub struct Env { frames: Vec<Frame>, defs: Rc<Defs>, handler_stack, defining: bool }
```

- **`Defs::define` downgrades** the *stored copy*'s `OwnerRef` to `Weak` (walking containers — maps/vectors/lists/sets are rebuilt with per-`Val` downgraded copies; the walk stops at `Atom`/`Cap` boundaries). **`Defs::lookup`/`local_bindings` upgrade on read** (the owner is trivially alive during any read through it), gated by the per-binding `has_callables` flag so plain-data reads pay nothing.
- Consequences, point by point: ordinary top-level functions create **no cycle**; **named recursion requires no cycle** (resting copy is weak; the executing copy is an upgraded escapee); exported closures and macros carry `Strong` (export snapshot = `local_bindings`, upgraded) and **survive the map being dropped**; a closure can outlive the module map — the escaped values themselves are what strongly own the module's `Defs`; the export map need not secretly retain anything (its *values* do, transparently); weak upgrade cannot fail in legitimate use (every read path goes through a live owner); data-only module owners free immediately after import; module unloading = dropping the last escaped value (supported for free); REPL owners are process-lived via the session `Env`'s strong `defs`; nested imports chain child-owner → prelude (never the parent module) — acyclic; cancellation drops the eval future and with it the only strong handles minus escapees; slim capture gets *cheaper* (top-level names are no longer copied — lexical frees only); raw-path snapshot likewise captures lexicals only; callable identity keeps using the captured lexical-env `Rc` (PR-4 unaffected).
- **Honest residual**: values that cannot be rebuilt at intern time — `Atom`s — can still smuggle a `Strong` closure back into its own owner (`(def a (atom nil))` + `(reset! a (fn [] …))`). That is the *existing, accepted, non-routine* atom-cycle class, unchanged and documented; routine definitions never cycle. Cap `inner` payloads are opaque `Rc<dyn Any>` and unaffected.

## 3. Closure and macro capture semantics (pinned)

A closure/macro captures exactly two things: **lexical locals by value/snapshot** (as today — Q1 yes) and **its definition owner by `OwnerRef`** (Q2: top-level names resolve late through `Defs`). Handler stack: not captured (caller's stack at invocation, unchanged). Dispatch: never captured. Answers 3–10: redefinition **is** visible to existing closures (late binding — intended, pinned); `(def x 1)(defn f [] x)(def x 2)(f)` → **2**; `fact` resolves itself through the owner → factorial works; mutual recursion works with no placeholders (both interned before first call); macros capture ownership identically and expansion-time lookups are late-bound (this *strengthens* the current "capture all macros" workaround — `try`'s expansion finds `try-catches` through the owner); an exported closure retains the module owner after the map is dropped (Q8 yes), exported macros likewise (Q9 yes); callable identity remains deferred to PR-4 (Q10 yes).

## 4. Shared prelude owner — **A (shared, immutable-by-construction), in PR-1b.0**

Inspection: the prelude defines **only macros** — no atoms, no load-time effects (`perform` appears only inside quoted expansion templates), no capabilities, no per-module state → safe to share by identity; stable prelude macro identity across modules also helps PR-4. Design: `load_prelude` keeps its public signature but memoizes a `Rc<Defs>` per thread (OnceCell) and attaches it as `inherited`; modules and sessions inherit it; module definitions shadow it locally. Mutation is prevented **by construction, not by type**: `define` writes only the current owner; no API path mutates `inherited`; the type is crate-private, and only the runtime holds the prelude handle — stated plainly per your instruction (a sealed two-type variant is noted as an option, not needed now). B (fresh per module) re-evaluates ~40 macro definitions per import and duplicates identity for no isolation gain (macros are pure); C (cached forms, fresh values) is B with less parsing. **A lands in PR-1b.0** — building per-module prelude owners first would be disposable work.

## 5. Module lifetime and exports

The required scenario works with **ordinary maps and no hidden state**:

```
(def imported (import "m"))   ; map values carry Strong owner refs
(def f (imported :f))         ; f itself strongly retains m's Defs
; imported dropped
(f)                           ; resolves module defs (late), recursive names,
                              ; prelude (owner→inherited), lexical captures
```

What strongly owns the module's `Defs` after import: **each escaped callable value**, individually and transparently — the map merely holds those values. No hidden owner handle, no per-closure special retention scheme beyond the uniform `OwnerRef`, and no `ModuleValue` wrapper. There is **no contradiction to surface**: ordinary values express the lifetime because the lifetime *is* value reachability under Graph 4.

## 6. Exact `Defs` API and invariants

```rust
impl Defs {
    pub(crate) fn new(inherited: Option<Rc<Defs>>) -> Rc<Defs>;
    /// Chain walk: own bindings, then inherited (read-only). Upgrades
    /// OwnerRefs in the returned copy when the binding has callables.
    pub(crate) fn lookup(self: &Rc<Self>, name: &str) -> Option<Val>;
    /// Writes ONLY own bindings (define and redefine; last write wins).
    /// Downgrades stored callables' OwnerRefs (container walk, stops at
    /// Atom/Cap). There is no API that writes through `inherited`.
    pub(crate) fn define(self: &Rc<Self>, name: String, value: Val);
    /// Own bindings only, upgraded — the export snapshot, `bindings()`
    /// replacement, and cap-status input.
    pub(crate) fn local_bindings(self: &Rc<Self>) -> Vec<(String, Val)>;
}
```

Invariants: shadowing = own entry wins in lookup, parent untouched; single-threaded `Rc`/`RefCell` (matches the runtime); Debug prints names only; module code cannot mutate the prelude because *no code path* mutates `inherited` and Model B removes any post-completion define path from module-produced closures; `Env.defining` is the sole gate for the def-family (true at module/REPL top level and through top-level macro expansion; false inside `for_call`).

## 7. Interaction with PR-1 / PR-1b

PR-1 finishes and merges **independently** (control extraction is orthogonal; the stack/allocation fixes remain valid — `Flow`/`Control` untouched). PR-1b.0 must land **before** B3 import work; import tests wait for it. Public API deltas in PR-1b.0: `Val::Fn`/`Val::Macro` gain the `owner` field (verified: no construction or destructuring outside glia — internal in practice), `Env` gains `defs`/`defining` (constructors unchanged), `load_prelude` signature unchanged, `bindings()` retired in favor of `local_bindings` for its two callers. B3 becomes **simpler**: module = `Defs::new(Some(prelude))` + shared handler stack; exports = `local_bindings()`; the def-log never exists. **Sequence: PR-1 → PR-1b.0 (ownership) → PR-1b (B3 import).**

## 8. Scope and migration estimate

Glia-only. `lib.rs`: `OwnerRef` + two variant fields + `Defs` (~120 lines); `eval.rs`: lookup/def paths, capture fns (slim/snapshot/filter/for_call), `compute_cap_status` over `local_bindings` + live owner chain, the `defining` gate, prelude memoization (~+350/−200); production total ≈ **+550/−250**, one compile stage, embedders untouched. Tests: ~25 existing updated (capture-count pins, redefinition pins, def-inside-fn expectations) + ~30 new (below), including `Weak`/`Rc::strong_count` lifetime probes. Perf: one `Rc<Defs>` probe per top-level miss; downgrade/upgrade walks gated by `has_callables`; capture cheaper. Risk: medium — concentrated in the two walk functions; the 682-test suite plus the new lifetime tests fence it.

## 9. Required tests

(1) named recursion (`fact 5` → 120 — currently *broken*, becomes the flagship fix); (2) mutual recursion, no placeholders; (3) late binding: `(def x 1)(defn f [] x)(def x 2)(f)` → 2; (4) lexical locals stay snapshot-captured (`let`-captured value unaffected by later rebinding); (5) module `(def try 42)` exports `{:try 42}`, prelude lookup in a sibling module unchanged; (6) `(def x 1)(def x 2)` → `{:x 2}`; (7) def-inside-fn raises `glia.error/def-not-top-level`, catchable, and `(when c (def x …))` at top level still works; (8) REPL persistence across forms; (9) redefinition visibility pinned (intended semantics replacing the accidental snapshot pins at eval.rs:4487-4541); (10) exported closure works after map dropped (and after importer env dropped); (11) exported macro likewise; (12) owner freed when last escapee drops — `Weak<Defs>` observer + `Rc::strong_count`; (13) plain `(defn f [] 1)` module: owner freed right after import when only data escapes; (14) recursive fns don't leak (weak resting refs — measured via weak/strong counts); (15) prelude shared: same macro identity across two module instances; two sessions share one prelude owner; (16) exports ≡ `local_bindings` only (no prelude names — exact key set); (17) nested imports have distinct owners; (18) diamond imports → independent owners; (19) cap/atom exports instance-scoped; (20) constrained-stack + wasm rounds stay green.

## 10. Documentation updates (proposed, not applied)

`doc/designs/definition-ownership.md` (new): the standing structural-ownership rule verbatim; the invariant; Model B top-level-only rule with the `defining` gate and the ww#574 history; Graph 4 with the resting-weak/escaping-strong diagram and the atom residual; capture = lexical-snapshot + owner-ref; late-binding semantics with the §3 examples; shared prelude owner and why it is immutable by construction; rejected designs (def-log/snapshot-diff — historical reconstruction; strong-owner graph — routine cycles; ModuleValue — hidden lifetime state). `doc/designs/macro-staging.md` (new): the deferred track as previously outlined. `doc/designs/value-contract.md`: PR-1b.0 row in the §11 roadmap; note that callable identity input (`env` Rc) is unchanged for PR-4. `doc/architecture.md`: "Design rules" section with the standing rule; a paragraph on definition ownership linking the design doc. CHANGELOG (with PR-1b.0): Rust API notes (`Val::Fn`/`Macro` field, `bindings()` → `local_bindings`) and the language-semantics notes (recursion works; late binding; def is top-level-only; module exports are owned definitions only).

## 11. Drift report

**REQUIRED CONSEQUENCE**: Model B def gate + structured error tag; `Defs`/`OwnerRef`/Graph 4 mechanics; capture rewrite; shared prelude owner; test suite incl. lifetime probes; the doc set above.
**ADJACENT — APPROVAL REQUIRED**: sealed two-type `Defs` variant (type-level prelude immutability) — optional hardening; `glia.error/analysis` tag (still parked); macro-staging track (unchanged).
**DRIFT — DO NOT IMPLEMENT**: namespace aliases/refers, explicit exports, visibility modifiers, general GC/Weak sweeps beyond Graph 4, callable identity work, macro-staging fix, PR-2/printer work, unrelated cleanup.

## 12. Decisions still requiring approval

1. **Model B — top-level-only definitions** with runtime enforcement and catchable `glia.error/def-not-top-level` (evidence: zero uses; sealed module surfaces; possession-model fit).
2. **Graph 4** resting-weak/escaping-strong owner refs, including the define-time container-downgrade walk and the documented atom residual (the only remaining — non-routine — cycle class).
3. Late binding for top-level names as pinned intended semantics (test 9 flips the accidental snapshot pins).
4. Shared immutable-by-construction prelude owner in PR-1b.0 (not deferred).
5. `bindings()` → `local_bindings` retirement (touches `compute_cap_status` semantics: cap-status now reads the live owner chain at check time).
6. Sequence PR-1 → PR-1b.0 → PR-1b confirmed.
