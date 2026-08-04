# PR-1b — module export boundary design (design only; no code edited)

Companion to `.context/pr1-import-design-estimate.md` and `.context/pr1b-cache-audit.md`. Locked B3 direction incorporated.

## 1. Current environment/frame topology (verified)

- `Env { frames: Vec<Frame>, handler_stack, root_frame_is_lexical }` (`eval.rs:48`); `Frame = HashMap<String, Val>`. **Flat frame vector — there is no parent-Env linkage or child-env API**; lookup walks frames innermost-out; `push_frame`/`pop_frame` manage lexical scopes.
- **`def` writes to the ROOT frame**: `set_root` inserts into `frames[0]` (`eval.rs:110-114`). All definition forms go through it: `eval_def` (:928), `eval_defmacro` (:1342), `Expr::Def` (:2189), `Expr::DefMacro` (:2762), `defcap` (:2833). Macro/function definition does not behave differently.
- Prelude bindings and module top-level definitions therefore land in the **same frame** (`frames[0]` of the import env) — provenance is not recoverable from frame structure.
- `Env::bindings()` (`eval.rs:140-150`) merges all frames inner-over-outer, no provenance. Production callers: the import result construction (`caps:469`) and `compute_cap_status` (`eval.rs:564`, untouched by this design).
- Module redefinition of a prelude name is an **overwrite** of the shared root entry within that env; nested lexical bindings (`let`/`fn` params) live in pushed frames that pop before export and use `set`, not `set_root` — they cannot leak (temporary frames are gone when `bindings()` runs; under the new model they are additionally never logged).
- Defs inside function bodies write the **closure's own captured-env root**, not the module env (`Env::for_call` builds a separate Env; pinned by the ww#574 comment in `test.glia:15-19`) — they never were module exports.
- Modules cannot observe or mutate importer frames today (fresh `Env`) and still cannot under B3 (fresh lexical env; only the handler *stack* is shared).
- **No code relies on prelude names in module maps** (§6 below).

## 2. Existing export behavior

`import` returns `Env::bindings()` of the whole import env: module defs **plus the entire prelude** (`defn`, `when`, `try`, `map`, `+`, … as live `Val::Macro`/`Val::Fn` values). Accidental: the caps import tests assert only module-defined keys (the `len == 2` test seeds the cache directly and never evaluates a module); docs/examples access only module-defined names (`(core :identity)`, `(t :assert=)`, `(p :audit)`); `ww/test`/`ww/policy` export closures/macros/an atom under their own names. Classification per §6 of the prompt: **no dependency found — current prelude inclusion is accidental behavior**, surfaced and dropped.

## 3. Export-model comparison

| Model | Verdict |
|---|---|
| **A** module-owned top-level frame | **Defeated by `def` semantics**: with prelude in a parent frame and a pushed child frame, `set_root` still targets `frames[0]` (the prelude frame). Making `def` target a different frame is an environment redesign (excluded). |
| **B** snapshot before/after | Zero core changes: record `(name → value)` after prelude+setup, diff after module eval (new names, or values that differ — `Fn`/`Macro` compare by captured-env pointer, so redefinitions are detected reliably). Edge: a module that rebinds a name to the *identical* value (`(def + +)`) exports nothing for it. |
| **C** definition tracking at `set_root` | **Exact provenance**: an optional crate-private def-log on `Env` (`Option<Rc<RefCell<BTreeSet<String>>>>`); `set_root` records the name when the log is armed. Armed only for module-form evaluation (after prelude load and import-cap binding, so neither is ever logged). Conditional defs (`(when c (def x 1))`) log correctly (macro expansions evaluate in the module env); in-fn defs are unlogged by construction (`for_call` envs carry no log). Cost: one field + two lines in `set_root`, zero cost outside imports. |
| **D** keep merged bindings | Rejected — the complection this decision removes. |

Both B and C give identical shadowing, nested-scope, macro, capability, future-export-syntax, and B3/per-import compatibility. **Recommended: C** — exact by construction, no value-equality edge, and the def-log is the natural seam a future explicit-export form would use. (B is the fallback if touching `Env` is unwanted.)

## 4. Recommended model — semantics

> An imported module exports exactly the names bound by top-level definition forms (`def`, `defn`, `defmacro`, `defcap`) during evaluation of the module's own forms, each mapped to its final value.

- Example from the prompt holds exactly: `{:answer 42 :add-one #<fn>}`; no `:defn :try :when :map :+`.
- No export syntax, no visibility modifiers, no whitelists — as directed.

## 5. Shadowing / redefinition semantics (exact)

- `(def try 42)` in a module: overwrites the **module instance's own** root entry (each import gets a fresh prelude frame, so no shared parent exists to mutate — "parent unchanged" holds across instances by construction); later module code sees 42; closures defined *before* the redefinition keep the original via their captured snapshots (layered-shadowing behavior in practice); export map contains `{:try 42}`.
- `(def map my-map)` — same: exported, instance-local.
- `(def x 1) (def x 2)` → log contains `x` once; export reads the final root value → `{:x 2}`. ✓ matches the expected export.
- Nested lexical locals: never logged, never exported.

## 6. Closures, macros, atoms, capabilities

Adopted rule: **any value bound at module top level is exported, regardless of type** — no eligibility filtering.

1. Capability acquired at init (`(def http (perform host :http-client))`) → exported, **intentional**: under B3 it is the *importer's own* authority, scoped to this module instance (per-import instantiation; the cache audit removed the cross-caller channel).
2. Atoms → exported, intentional; **fresh per import** (ww/test's registry).
3./4. Exported closures keep full access to prelude/parent bindings: closures own self-contained captured envs (slim `capture_closure` on the analyzed path; full snapshot on the raw path) — returning only the module-owned map **cannot** break captures, and nothing else retains the import env.
4. Macros → safe: same captured-env mechanism (ww/policy's `attenuate-handler`).
5. Nothing exported is tied to the discarded import evaluation: all `Val`s are owned; resume continuations are consumed during init; handler frames pushed by module code pop via existing guards.
6. Per-import instantiation delivers fresh identity for atoms/closures/caps as intended (pinned by tests 8/9 in §9).

## 7. Combined B3 + export-boundary flow (exact)

```rust
// Env (crate-private additions)
pub struct Env { frames, handler_stack, root_frame_is_lexical,
    /// Records names written by set_root while armed. Import-only.
    def_log: Option<Rc<RefCell<BTreeSet<String>>>>,   // None everywhere else
}
impl Env {
    pub(crate) fn for_module(hs: &HandlerStack) -> Self;          // fresh root, shared stack, no log
    pub(crate) fn arm_def_log(&mut self) -> Rc<RefCell<BTreeSet<String>>>;
    pub fn set_root(&mut self, name: String, val: Val) {
        if let Some(log) = &self.def_log { log.borrow_mut().insert(name.clone()); }
        …existing insert…
    }
}

// eval_import (crate-private, in perform_cap_value's intrinsic arm)
async fn eval_import<D: Dispatch>(loader, chain, payload, env, dispatch) -> Result<Val, Control> {
    let ModuleSource { resolved, text } = /* loader.load(path).await — Err → throw (catchable) */;
    /* chain check: resolved ∈ chain → throw import-cycle (catchable) */
    let mut module_env = Env::for_module(&env.handler_stack);
    load_prelude_forms(&mut module_env, dispatch).await?;          // implementation context
    module_env.set_root("import".into(),
        make_import_cap_chained(loader.clone(), chain + [resolved])); // pre-log: not exported
    let log = module_env.arm_def_log();                             // exports start HERE
    for form in read_many(&text)? /* Err → throw */ {
        eval(&form, &mut module_env, dispatch).await?.into_value("module form")?;
        // B3: effects suspend/resume at the original site; exceptions
        // dispatch on the shared stack; faults propagate untouched.
    }
    Ok(module_exports(&module_env, &log.borrow()))                  // replaces Env::bindings()
}

/// Log names → final root-frame values, sorted, as a Val::Map.
fn module_exports(env: &Env, names: &BTreeSet<String>) -> Val;
```

`ModuleLoader`/`ModuleSource`/`make_import_cap(loader)`/`ImportInner{loader, chain}` are unchanged from the cache audit (§7 there). Export selection is orthogonal to cycle chains, nested-import cap binding, handler-stack sharing, module-local handlers, and source caching — verified no interference (the import cap and prelude are bound pre-arming; nested imports return their own logged exports).

## 8. Scope and sequencing

A **bounded consequence** of the module-instantiation model — stays in PR-1b. Delta over the existing PR-1b estimate: glia only — `Env` +~10 lines, `arm_def_log`/`for_module` +~15, `module_exports` +~15, `eval_import` uses it (no extra); no new crates, no public API surface beyond what B3 already adds (`bindings()` remains for `compute_cap_status`; caps' call site disappears with `make_import_handler`). Diff delta ≈ +45 production, +120 tests. No environment redesign needed; if model A had been forced, that would have required one — avoided by C. Implementation order within PR-1b: Env additions → `eval_import` (already sequenced) → export tests.

## 9. Tests (concrete)

1. Module uses `defn`/`when`/`+` internally; export map lacks `:defn :when :+` and every prelude name (assert exact key set).
2. `(def answer 42)(defn add-one [x] (+ x 1))` → exactly `{:answer 42 :add-one #<fn>}`; `((get m :add-one) 41)` → 42 post-import.
3. `(def try 42)` module → export `{:try 42}`; a *second* import of a different module still has working prelude `try` (no cross-instance mutation).
4. `(def x 1)(def x 2)` → `{:x 2}`.
5. `(let [tmp 9] (def y tmp))` → exports `{:y 9}`, no `:tmp`.
6. Exported closure referencing both a prelude fn and a module binding works after import.
7. Exported macro (`ww/policy`-style) expands correctly post-import.
8. Two imports of ww/test → distinct `:*tests*` atoms (fresh identity per import).
9. Module acquiring a cap at init exports it; a second import under a handler resuming a *different* cap exports that one (instance scoping).
10. Nested import: inner module's map contains only inner-owned names.
11. Diamond imports → independent dependency instances (distinct atoms).
12. Cycle detection unaffected (A→B→A still throws the catchable cycle exception).
13. `ww/test` + `ww/policy` end-to-end through real import: registry, stub-handler, audit flows work with module-only maps (deliberate update of any assumption found).
14. Loader `load` called once for two imports (source cache) while exports/atoms differ by identity.
15. One wasm-backed import round through the `shell_e2e` harness.

## 10. Drift report

**REQUIRED CONSEQUENCE**: def-log provenance + `module_exports` + `for_module`/`arm_def_log` + tests above (direct consequence of the locked export rule and B3 instantiation).
**ADJACENT — APPROVAL REQUIRED**: none new (the previously flagged macro-staging fix and export filtering questions are absorbed/settled by this decision).
**DRIFT — DO NOT IMPLEMENT**: explicit export syntax, public/private declarations, manifests, whitelists, type-based eligibility, def-target changes (`def` keeps writing the root frame), generalized environment cleanup, callable/PR-2/printer/macro-staging work.

## 11. Decisions still requiring approval

1. Model **C** (def-log at `set_root`) over B (snapshot-diff) — C touches `Env` with one crate-private optional field; B is zero-touch but has the redefine-to-identical-value edge.
2. `defcap` at module top level counts as an export (it goes through `set_root`) — proposed yes.
3. The `import` binding inside modules and prelude names are implementation context, never exported (bound pre-arming) — proposed yes (consistent with the target rule).
4. Prior PR-1b sign-offs assumed carried: per-import instantiation replacing "idempotent re-import", `IMPORT_CACHE` removal, chain-based cycles (this design keeps all three; the "prelude-in-map quirk" question from the cache audit is now *resolved* by this decision instead of preserved).
