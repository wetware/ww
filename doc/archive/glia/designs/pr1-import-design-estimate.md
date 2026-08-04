# Import semantics — full-context design estimate (design only; no code edited)

Companion to `.context/pr1-sol-reconciliation-v2.md`. Concrete test case throughout: module body `(+ 1 (perform :lookup :x))`, importer handler resumes `:lookup` with 41, expected 42 **inside the module**.

## 1. Current import topology (exact)

1. Embedders wrap every top-level form: `(with-effect-handler import import-handler …)` — caps `wrap_with_handlers` (`std/caps/src/lib.rs:861-867`, list `["import","routing","host"]`), kernel's own copy (`std/kernel/src/lib.rs:1766-1767`, list `["import","routing","runtime","host"]`). `import` is a cap from `caps::make_import_cap()` (unit inner — pure identity token); `import-handler` is `caps::make_import_handler(load_runtime)`, an `AsyncNativeFn` (cli binds both at `src/cli/shell.rs:690/699`; std/shell embeds the handler in a `HandledCapInner` at `std/shell/src/lib.rs:324`; kernel wires both from caps).
2. `(perform import "core")` → `Expr::Perform` → `perform_cap_value` (`crates/glia/src/eval.rs`) → `perform_dispatch` walks the guest stack → matches the wrapper frame → writes `(target, data, tx)` into the frame's slot → the guest computation suspends on `rx.await`.
3. The import frame's `with-effect-handler` machine sees body-Pending + pending slot → pops its frame → builds `resume` via `make_resume_fn(tx)` → enters `HandlerState::Handling(import-handler future)`; **while Handling it never polls its body**.
4. Inside the handler future (`std/caps/src/lib.rs:302-480`): resolve path → `load_runtime.load(...).await` (async I/O fine — Pending propagates to the host executor) → UTF-8 → `read_many` → **`Env::new()` — fresh lexical env AND fresh, empty `HandlerStack`** (`lib.rs:372`) → prelude then module forms evaluated via `eval_toplevel`/`eval_toplevel_expr` under `NoopDispatch`, each **polled exactly once with a noop waker** (Pending → fault).
5. **Continuation loss point:** a module-body `(perform :lookup :x)` calls `perform_dispatch` against the module's *empty* stack → the no-match arm returns `Err(Control::Unhandled)` immediately — nothing ever suspends; the `(+ 1 _)` frame unwinds and is destroyed as the error propagates out of the nested `eval_toplevel_expr`, which seals it into `EvalError` (`eval.rs:seal`). The handler then stringifies it (`"import: eval error in {resolved}: {e}"`). The continuation is not "discarded later" — it never exists past this point.
6. On success: bindings → map → `IMPORT_CACHE` (thread-local, keyed by resolved path, `lib.rs:264`) → `call_resume(resume, map)` → resume signal → `settle_native` → `Control::Resume` → machine re-polls body → guest's `rx` yields the map.

Note the second structural obstacle beyond the fresh stack: even if the module shared the importer's stack, its suspensions would park inside the handler future while every servicing machine lives in the **parked body** the Handling state never polls — a deadlock. Any design that keeps module evaluation inside a native handler future must also rewire slot servicing.

## 2. A′ vs B on the concrete example

**A′ (forward + redispatch):** module eval completes-with-`Unhandled(:lookup)` (continuation already gone, §1.5) → `settle_native` re-dispatches on the importer's stack → the `:lookup` handler runs, `(resume 41)` → `perform_dispatch` returns `Ok(41)` **at the settle chokepoint** → 41 becomes the value of the whole import expression. **Observable result: `(perform import "m")` evaluates to `41`.** The module map is lost, the module never computes `(+ 1 …)`. For abortive handling A′ is exact; for resumptive handling it is semantically wrong, not merely limited.

**B (module evaluated under the importer's live dynamic context):** the module's perform finds the importer's `:lookup` frame, suspends *in place*, the handler resumes 41 into the module's own oneshot, the module computes 42 and finishes; the import returns the module map. **Observable: 42 at the original site; import yields the bindings map.** Full per-dimension comparison in §5's table (exceptions, effects, resumption point, nesting, one-shot ownership, shadowing, stack/heap, cancellation, async, caching, faults).

## 3. Existing semantic commitments

Searched docs, tests, examples, comments. Found: (a) `"Evaluate in a fresh Env (isolated scope)"` (`caps:371`) — a **lexical**-isolation comment only; (b) `"{name}: not available during import"` NoopDispatch and the single-poll `"imports are strictly synchronous"` machinery — implementation conveniences, nowhere documented as semantics; (c) affirmative evidence that modules are expected to interoperate with the caller's effect world: `ww/test` (the flagship imported module) performs `:stdout` and `perform*` against caller caps from its functions, and `ww/policy`'s documented usage wraps caller effects. **No commitment to abortive/effect-masking imports exists anywhere. Explicitly: there is no repository evidence for the abortive-boundary option.**

## 4. Exact design for B — chosen shape **B3: import reified as evaluator-integrated cap behavior**

The decisive move: module evaluation must run in **body position at the perform site** (same task, same poll chain), not inside a handler future — that dissolves both obstacles in §1 (fresh stack; Handling-state deadlock) with **zero changes** to `AsyncNativeFnImpl`, `Dispatch::call`, `NativeSignal`, `Control`, `EffectRequest`, `settle_native`, the handler machines, the continuation transport, or the shell boundaries.

```rust
// crates/glia/src/lib.rs (new, public)
/// Source produced by an embedder's module loader.
pub enum ModuleSource {
    /// Already-evaluated module map (cache hit): returned as-is,
    /// no effects re-performed.
    Cached(Val),
    /// Module source text plus its resolved path (for logging/cycles).
    Source { resolved: String, text: String },
}

/// Embedder hook for module resolution, loading, caching, and cycle
/// detection. All I/O stays embedder-side; the evaluator never touches
/// the filesystem.
pub trait ModuleLoader {
    fn load<'a>(&'a self, path: &'a str)
        -> Pin<Box<dyn Future<Output = Result<ModuleSource, Val>> + 'a>>; // Err = exception payload
    fn store(&self, resolved: &str, module: Val);
    /// Cycle guard: mark/unmark in-progress; Err = import-cycle payload.
    fn begin(&self, resolved: &str) -> Result<(), Val>;
    fn finish(&self, resolved: &str);
}

/// Import capability: identity token whose intrinsic behavior is
/// evaluator-integrated module loading. Mint via this constructor.
pub fn make_import_cap(loader: Rc<dyn ModuleLoader>) -> Val; // inner = ImportInner{loader}
pub(crate) struct ImportInner { pub(crate) loader: Rc<dyn ModuleLoader> }
```

```rust
// crates/glia/src/eval.rs — perform_cap_value gains one intrinsic arm,
// checked AFTER the handler-stack walk (guest interposition keeps priority):
if let Some(import) = handle.inner().downcast_ref::<ImportInner>() {
    return eval_import(import.loader.clone(), &payload, env, dispatch).await;
}

/// Crate-private. Runs in body position: same task, same handler stack.
async fn eval_import<D: Dispatch>(
    loader: Rc<dyn ModuleLoader>, payload: &[Val], env: &mut Env, dispatch: &D,
) -> Result<Val, Control> {
    // path extraction errors, loader.load Err, begin() cycle Err,
    // UTF-8/read_many failures → throw(&env.handler_stack, …)  [catchable]
    // ModuleSource::Cached(v) → Ok(v)
    // Fresh LEXICAL env sharing the importer's DYNAMIC stack:
    let mut module_env = Env::for_module(&env.handler_stack);   // new pub(crate) ctor
    load_prelude_forms(&mut module_env, dispatch).await?;        // reuses PRELUDE, in-crate
    module_env.set("import".into(), make_import_cap(loader.clone())); // nested imports
    for form in forms {
        // Plain in-context evaluation: exceptions dispatch on the shared
        // stack (importer's try catches); effects suspend AT THIS SITE and
        // resume here; faults propagate as Control::Fault untouched.
        eval(form, &mut module_env, dispatch).await?.into_value("module form")?;
    }
    let module = bindings_map(&module_env);
    loader.store(&resolved, module.clone());
    Ok(module)
}
```

Answers to the ten §3 questions: (1) yes — the module `Env` **shares** the importer's `HandlerStack` `Rc` (crate-internal constructor; no borrow gymnastics); (2) no separate child-context type is needed; (3) yes — the module reuses the continuation machinery *directly* because it runs in the same task at the perform site; (4) `EvalError::Unhandled` doesn't disappear — under B3 module evaluation never crosses an `EvalError` boundary at all (plain `Control` propagation; truly-unhandled effects escape structurally as today); (5) yes — same future/task, inline; (6) moot — there is no async native import handler anymore; the loader's I/O future awaits inline in body position; (7) the "evaluate within current context" entry point exists but is (8) **crate-private** (`eval_import`); the only new public surface is `ModuleLoader` + `ModuleSource` + the `make_import_cap` signature; (9) nested imports are recursive inline evaluation — the module env gets the import cap bound; `begin/finish` gives cycle detection (import-cycle → catchable exception); handler frames pushed by module code pop via the existing guards, no duplicated state; (10) moot under B3 — the deadlock question is exactly why B1 loses.

Embedder migration: caps implements `ModuleLoader` on `LoadRuntime` (resolution + `IMPORT_CACHE` + cycle set move behind it) and **deletes `make_import_handler`** (~170 lines, including both import `NoopDispatch`es, the single-poll hack, and every stringification site — Sol's finding evaporates rather than being patched); cli/kernel/std-shell mint the cap with the loader and drop `"import"` from their wrapper lists; `NativeSignal::forward` is **dropped from the plan** (superseded).

## 5. Alternatives

| | Semantics | Complexity | Public API | Perf | Cancellation | Composability | Future-proofing |
|---|---|---|---|---|---|---|---|
| **B1** shared live context via scoped TLS + natives | correct **only** with a second change: machines must poll body *and* handler in Handling state (else §1's deadlock); poll-ordering subtleties | high; touches every machine + a per-poll scoped-context wrapper | `AsyncNativeFnImpl` untouched but new context API | extra poll passes | murky (two live poll paths) | leaks a general "natives may re-enter eval" pattern | poor |
| **B2** continuation bridge (keep the nested module future alive across `Unhandled`) | correct if built | highest: requires reifying/parking the module future at the no-match point and re-entering it after outer handling — a new continuation transport | new bridge types | ok | hard (parked futures own oneshot halves) | narrow | poor |
| **B3** evaluator-integrated import (chosen) | **correct by construction** — body position | lowest that is correct: one trait, one intrinsic arm, deletions elsewhere | +`ModuleLoader`/`ModuleSource`, `make_import_cap(loader)`; −`make_import_handler` | no new machinery on hot paths; I/O awaited inline | trivial — dropping the eval future drops the module future inside it; frame guards already Drop-clean | modules are ordinary evaluation: shadowing, nesting, `try`, host frames all compose for free | good — same shape a future `(import …)` special form or guest `eval` would want |
| **B4 = A′** forward + redispatch | **wrong** for resumption (§2: returns 41, loses the module) | low | small | fine | fine | abortive-only | dead end |

## 6. Lanes under B3 (each travels existing types, unchanged)

Import-not-found / unreadable / non-UTF-8 / parse failure / cycle → `throw(hs, payload)` → catchable (`ImportError`-style fallback works). Module-thrown exception → dispatched on the shared stack at the throw site → importer's `try` catches the **exact structured payload**. Ordinary module effect → suspends at the original module site; importer handlers abort or **resume in place** (the 41→42 case). Internal fault → `Control::Fault` propagates through `eval_import` untouched → bypasses everything. Payloads/targets/data: never wrapped, never stringified. Module-path context: `log`-level only (no payload injection — resolving the earlier open question in favor of logging).

## 7. Scope and migration estimate

| Dimension | Estimate |
|---|---|
| Crates | glia, caps, ww(cli), kernel, std/shell (5) |
| Files | ~7 production (glia lib+eval; caps lib; cli shell; kernel lib; std/shell lib) + tests |
| Production call sites | glia: 1 new intrinsic arm + ~130-line `eval_import` + trait/ctor ~70 lines; caps: +~90 (loader impl), **−~170** (handler deleted); embedder wiring ~10–20 lines each ×3 |
| Public API breaks | `caps::make_import_handler` removed; `caps::make_import_cap()` → takes a loader (or moves to glia); guest-visible `import-handler` binding disappears in cli/kernel |
| New types | `ModuleLoader`, `ModuleSource`, `ImportInner` (private), `Env::for_module` (crate-private) |
| Test migration | caps import tests rewritten against the loader (~10 tests); + new suite §8 (~350–450 lines); existing glia/kernel suites unaffected |
| Diff size | ≈ +550/−350 production+wiring, +400 tests |
| Compile-break stages | 2: (i) glia additions (green, unused); (ii) caps loader + handler deletion + 3 embedder rewires in one stage (cross-crate window) |
| Sequence | glia trait/arm+tests → caps loader → embedders → test suite → verification |
| Review risk | **medium**: evaluator gains an I/O-adjacent arm (mitigated: all I/O behind the trait; the arm is straight-line); recursion/cycle handling; deliberate behavior deltas (below) |

Deliberate behavior deltas (each needs sign-off, §10): modules evaluate under the **caller's `Dispatch`** (the `NoopDispatch` masking goes away — consistent with "ordinary effectful evaluation"); module-init effects now reach importer handlers including host frames (`:load`, `:stdout`); nested imports become possible (bound cap + cycle exception; today they silently can't work); cache-hit returns the map without re-performing effects (Python-like, pinned by test); `import-handler` bindings removed.

Drift classification: B3 core + tests + wiring = **REQUIRED CONSEQUENCE** (of the locked import semantics). The five behavior deltas = called out individually for approval within it. Anything touching the machines/`AsyncNativeFnImpl` (B1/B2 machinery), guest `eval` facility, `(import …)` special form = **DRIFT — DO NOT IMPLEMENT**.

## 8. Required tests (concrete)

1. `(try (perform import "missing") (catch _ e (perform import "fallback")))` → fallback module map; error payload carries a structured tag.
2. Module `(throw (ex-info "boom" {:type :mod}))`; importer `(try (perform import "m") (catch :mod e e))` → exact payload map.
3. Module `(def answer (+ 1 (perform :lookup :x)))`; importer installs resumptive `:lookup`→41 → `(get (perform import "m") :answer)` = **42**.
4. Abortive: importer's 1-arity `:custom` handler returns `:handled` while module performs `:custom` → pinned: the with-effect-handler **body** (containing the import) aborts to `:handled`; module map not produced.
5. Nested: module A imports module B; B performs `:lookup`; outer importer's handler resumes → value lands at B's original site (via A's map).
6. Shadowing: module installs its own `:lookup` handler around its perform → module-local handler wins (innermost on the shared stack).
7. Injected `NativeSignal::fault` inside a module-called native → bypasses importer `try` and effect handlers → `EvalError::Fault`.
8. One-shot: importer handler stashes resume, calls twice during module suspension → second raises `continuation-already-resumed`.
9. Cancellation: drop the eval future mid-module-suspension → no leaked frames (`handler_stack` empty afterward) — Rust-side assertion.
10. Constrained-stack (2 MiB thread) import+resume path; one wasm-backed import/effect/resume round through the existing `shell_e2e` harness.

## 9. Risks

Evaluator-core surface growth (bounded by the loader trait); the two-crate compile-break window; behavior deltas above (mainly `NoopDispatch` removal — modules gain dispatch access, which is the *point* of the locked semantics but is the most observable change); nested-import cycles (guarded, tested); cache coherence across embedders (per-loader, as today's per-thread cache).

## 10. Recommendation and approvals

**Recommendation: implement full resumptive import semantics via B3, as an immediate stacked PR-1b, before PR-1 merges is not required — sequence PR-1 → PR-1b, reviewed together.** Rationale: PR-1's current import behavior is exactly master's (the stringification predates PR-1), so PR-1 introduces no regression; B3 is self-contained with its own reviewable surface and deletes the code Sol flagged rather than patching it. The abortive-boundary option is rejected — §3 found zero supporting evidence and affirmative counter-evidence. If you prefer a single PR, B3 folds into PR-1 at ≈+950 additional diff.

Approvals still required:
1. B3 + PR-1b sequencing (or fold into PR-1).
2. Modules evaluate under the caller's `Dispatch` (NoopDispatch masking removed).
3. Nested imports enabled (import cap bound in module env; cycle → catchable exception).
4. Cache semantics pinned: first import runs effects; cache hits return the map silently.
5. Removal of `make_import_handler` / guest-visible `import-handler` bindings.
6. `NativeSignal::forward` dropped from the plan (superseded by B3).
7. Module-path context via logging only (no payload injection).
