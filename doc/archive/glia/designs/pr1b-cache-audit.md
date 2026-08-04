# PR-1b — module cache semantics and authority audit (design only; no code edited)

Companion to `.context/pr1-import-design-estimate.md`. B3 approved directionally; this resolves the final blocker.

## 1. Current cache topology

- **What is cached:** the fully evaluated module bindings map (`Val::Map`). `Env::bindings()` (`eval.rs:140`) merges **all frames**, so the cached map contains the module's defs **plus every prelude binding** (`defn`, `when`, `try`, … as `Val::Macro`/`Val::Fn` values) — an existing quirk, not PR-1b's doing.
- **Key:** resolved path string (`/lib/{path}.glia` normalization).
- **Store/lifetime:** `IMPORT_CACHE` is a `thread_local` `RefCell<HashMap<String, Val>>` (`caps:261-264`) — lives for the thread ≈ the runtime instance (cli process; one wasm kernel/shell cell instance). A separate `LOAD_CACHE` caches raw **bytes** per resolved path (also thread-local).
- **Scope / who shares:** everything evaluated in that runtime instance shares entries — all forms, all sessions served by that instance. Not per-import-capability, not per-Dispatch. `clear_import_cache()` exists (cli calls it at runtime build; caps tests).
- **Second-import behavior today:** same `Val::Map` **by identity** — same atoms, same closures. Doc comment commits to this: *"Caches the map for idempotent re-import"* (`caps:299-302`).

## 2. Values that can enter cached module results

Today, module init runs in an **authority vacuum** (fresh empty handler stack + `NoopDispatch`), so cached maps can contain only: data, **closures** (capturing the module env; `is_cap_free = true` by construction), **macros** (ww/policy's `attenuate-handler`), and **atoms** — ww/test caches `*tests*`, a live mutable registry, shared by identity across every later import in the runtime instance (deliberate per ww#574, *within* one instance). No capabilities can enter today — not by policy, but because acquisition is impossible during init.

**Under B3 with the proposed evaluated-value cache, that guarantee evaporates:** module init sees the caller's live handler stack and Dispatch, so `(def http (perform host :http-client))` at module top level acquires a **capability from the first importer** and the cache would hand that cap (or closures capturing it, or atoms containing it) to every later importer — including one running under an attenuated or absent `host`. Resumptions can't be cached (one-shot, consumed during init), but caller-scoped values (whatever the first caller's handlers resumed with) can.

## 3. Authority / cross-caller risk analysis

| Risk | Today | B3 + cache-evaluated-Val (policy A) |
|---|---|---|
| Cap acquired from first caller reused by later caller | impossible (vacuum) | **yes — confinement breach**; bypasses attenuation differences between callers |
| Caller-scoped resumed values frozen into the map | impossible | yes (whatever `:lookup`-style handlers supplied at first init) |
| Mutable identity (atoms) shared across callers | yes, within a runtime instance (deliberate for ww/test) | same, but now across *authority contexts* too |
| Closure identity shared | yes (idempotent re-import) | same |
| `is_authority_free` constraining the cache | **not used** — nothing constrains caching today; safety is purely the vacuum | would need to become load-bearing (policy C) — and it is insufficient anyway: it walks atom *contents* and closure flags, but cannot express "no shared mutable identity" (an authority-free atom is still a cross-caller channel) |

Answers 7/8 explicitly: yes, under B3 module initialization can acquire authority from the first caller, and yes, it can appear in exported bindings or captured closures. This is the blocker's core, and it rules out policy A.

## 4. Existing semantic commitments

- *"Caches the map for idempotent re-import"* (`caps:301`) — the only explicit commitment: second import returns the same value. Motivated by ww/test's registry atom (ww#574 comment at `test.glia:15-19` — persistence "across invocations" of the module's *closures*, which the returned map itself provides; cross-*import* identity is incidental to that need).
- Usage pattern in every doc/example: **import once, bind, pass the map** (`(def t (perform import "ww/test"))`) — no example re-imports expecting shared state.
- No doc claims once-only initialization effects, no doc claims module singletons.
- Clojure/Python `require`/`import`-once singletons presuppose an ambient global namespace and ambient authority. Glia has neither: the capability model's own rule — *authority and identity flow by possession* — maps modules onto **per-import instantiation** (functor-style, as in capability-secure module systems): if two parties should share one module instance, someone who possesses the map hands it to them.

## 5. Cache-policy comparison

| Policy | Authority isolation | Module identity | Init effects | Perf | Closure identity | Nested/cycles | Invalidation | Complexity |
|---|---|---|---|---|---|---|---|---|
| **A** cache evaluated Val per loader | **broken** (§3) | singleton per runtime | once (first caller's context — worst of both) | best | shared | ok | manual clear | low |
| **B** cache source/parsed only; evaluate per caller | **sound** — each import acquires only its own caller's authority | instance per import | per import, in the importing context (predictable) | re-eval per import; I/O already amortized by `LOAD_CACHE`; std modules are ~100 lines — negligible at import frequency | fresh per import | natural; cycles per chain | text cache keyed by path; trivial | low |
| **C** cache only when recursively authority-free | partial — `is_authority_free` doesn't capture shared-mutable-identity (authority-free atoms still leak state cross-caller); needs a new stricter predicate; two observable regimes depending on module contents (confusing) | mixed | mixed | good | mixed | ok | subtle | **high** |
| **D** cache per caller/Dispatch context | sound in principle; but "caller context" has no stable key (handler stacks mutate per form; Dispatch is a borrow) | per context | per context | poor hit rate | per context | ok | unclear | high, ill-defined |
| **E** no cache at all | sound | instance per import | per import | same as B minus text memoization | fresh | natural | n/a | lowest |
| **F** B + optional embedder-side *value* pinning (an embedder that owns one authority context may pre-import and bind maps itself) | sound (explicit, possession-based) | embedder's choice | embedder's choice | best where used | embedder's choice | ok | embedder's | = B |

"Cache hits skip effects" is **not** desirable under B3: it silently reorders whose handlers run init effects. Policies B/E make init effects a per-import, current-context matter — no hidden first-caller privilege.

## 6. Recommended policy for PR-1b: **B (source-cache only; evaluate per import)** — which is E plus text memoization, and F comes free

- Delete `IMPORT_CACHE` (the evaluated-value cache) and `clear_import_cache`; keep byte/text caching inside the loader (existing `LOAD_CACHE` mechanics).
- Modules are **instantiated per import** in the importer's dynamic context. Sharing an instance is explicit: import once, pass the map (possession semantics — one rule for values, capabilities, and now modules).
- **Deliberate commitment change** (sign-off item 1): the `caps:301` "idempotent re-import" line is replaced by documented per-import instantiation. Audit of ww/test and ww/policy shows no usage depends on cross-import identity — both are import-once-bind. ww/test's registry persistence lives in the map the importer holds, untouched.
- The prelude-bindings-in-map quirk is kept as-is for parity (ADJACENT: filter exports to module-defined names later).

## 7. Exact `ModuleLoader` API after this policy

```rust
// crates/glia/src/lib.rs
/// Resolved module source. Loaders may memoize text by resolved path;
/// evaluated module values are never cached by the runtime — modules are
/// instantiated per import in the importer's dynamic context.
pub struct ModuleSource {
    pub resolved: String,   // for logging and cycle detection
    pub text: String,
}

/// Embedder hook for module resolution and loading. Pure I/O: no
/// evaluation-result storage, no cycle state (cycles are tracked by the
/// evaluator per active import chain).
pub trait ModuleLoader {
    fn load<'a>(
        &'a self,
        path: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<ModuleSource, Val>> + 'a>>; // Err = exception payload
}

pub fn make_import_cap(loader: Rc<dyn ModuleLoader>) -> Val;
```

`ModuleSource::Cached`, `store`, `begin`, `finish` are all gone — the trait shrank to one method. caps implements it on `LoadRuntime` (resolution + text memoization); `IMPORT_CACHE`/`clear_import_cache`/`make_import_handler` deleted.

## 8. Cycle / concurrency design: **per active import chain, by construction**

```rust
pub(crate) struct ImportInner {
    loader: Rc<dyn ModuleLoader>,
    /// Resolved paths of the imports currently in progress ON THIS CHAIN.
    /// Root caps start empty; eval_import binds a child import cap into the
    /// module env carrying chain + [resolved].
    chain: Rc<Vec<String>>,
}
```

`eval_import` checks `resolved ∈ chain` → throws a structured import-cycle exception (catchable); otherwise evaluates the module with a child import cap whose chain is extended. Properties: cycle state travels with the recursion itself — **no loader-global "loading" flag**, so two concurrent independent evaluations (interleaved futures awaiting I/O) importing the same module can never be mislabeled as a cycle; they simply both instantiate it, which is exactly policy B's semantics. No locks, no cross-task state, nothing to leak on cancellation (the chain is owned by dropped futures).

## 9. Required tests (additions to the PR-1b suite)

1. Per-import instantiation: import ww/test twice → two distinct registries (mutating one leaves the other empty); pins the commitment change.
2. Init effects per import: module performs `:probe` at top level → importer's handler observes one perform per import, under each import's own handlers.
3. Authority isolation: importer A's handler resumes a cap into the module; importer B (handler absent) imports the same module → B's import raises unhandled/exception, and A's cap is nowhere reachable from B.
4. Text-cache soundness: loader `load` called once for two imports of the same path (memoized I/O) while still yielding independent instances.
5. Cycle: A imports B imports A → structured import-cycle exception, catchable by `try`; chain unwound cleanly.
6. Not-a-cycle: two interleaved evaluations import the same module concurrently → both succeed.
7. Diamond: A imports B and C, both import D → D instantiated twice (policy B pinned), no cycle error.
8. Existing §8 estimate tests (42-case, shadowing, fault bypass, one-shot, cancellation, stack/wasm) unchanged.

## 10. Remaining blocker

None structural. Sign-off items: (1) replace the "idempotent re-import" commitment with per-import instantiation (policy B) — including that repeated imports re-run init effects; (2) delete `IMPORT_CACHE`/`clear_import_cache` (API removal, cli + caps tests touch it); (3) chain-carried cycle detection with catchable import-cycle exception; (4) keep the prelude-bindings-in-map quirk for parity (filtering deferred as ADJACENT).
