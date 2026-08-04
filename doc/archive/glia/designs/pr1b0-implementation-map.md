# PR-1b.0 implementation map — final pre-implementation checkpoint

Status: COMPLETE. Inputs: amended contract, preflight report, memory-model study, Sol handoff, ownership resolution, spike, study batches. No production source or docs modified. Nothing committed. All line numbers verified against the working tree (PR-1 applied, uncommitted) on 2026-08-03.

## 1. Executive implementation strategy

Seven stages (A–G), ordered so that behavior flips happen at exactly two points and compile-broken intervals at exactly one. Stage A adds every new type UNCALLED (compiles green, zero behavior change). Stage B routes definitions through `Defs` — the first behavior flip (late binding, recursion, top-level gate), still with the OLD closure representation. Stage C is the single flag-day: `Val::Fn`/`Val::Macro` payload change — **fully contained in crates/glia** (verified: zero `Val::Fn`/`Val::Macro` token matches in std/kernel, std/caps, std/shell, src/cli; external code only pattern-matches through helpers or uses `..`). Stage D caps, Stage E authority (second small flag-day: field deletion), Stage F audit, Stage G tests/perf/wasm/docs. The spike crate (`.context/spike/ownership-spike/`) is the reference model: its `rest_for`/`escape_with`/`Defs` port to production nearly verbatim (Val has more leaf variants; ValMap instead of assoc Vec).

Two structural devices minimize risk: (1) all Graph 4 mechanism lives in one private submodule `eval::own` whose only exports are five named helper functions — the privacy proof for §4 is the module boundary itself; (2) the new `Closure` payload struct is public-but-opaque (all fields `pub(crate)`), which is the only way to put an `OwnerRef` inside the public `Val` enum without exposing it (approval item 1, §16).

## 2. Current-state inventory (file, symbol, role → change, risk)

| # | File:line | Symbol | Current role | Change | Risk |
|---|---|---|---|---|---|
| 1 | eval.rs:49 | `pub struct Env { frames: Vec<Frame>, handler_stack, root_frame_is_lexical }` | lexical + namespace + capture conflated | gains `defs: Rc<Defs>`, `defining: bool`; frames become lexical-only | HIGH (touches everything) |
| 2 | eval.rs:67 | `type Frame = HashMap<String, Val>` | frame representation | unchanged (lexical frames keep it; `CapturedEnv` reuses the map shape) | LOW |
| 3 | eval.rs:71 | `Env::new` | root env, single frame | + fresh standalone `Defs`, `defining: true` | LOW |
| 4 | eval.rs:225 | `Env::for_call(captured: &Rc<Env>, caller_hs)` | rebuilds call env from captured `Rc<Env>` | signature → `for_call(closure: &Closure, caller_hs)`; escapes captured slots via owner witness; `defining: false` | HIGH |
| 5 | eval.rs:154/173/197 | `snapshot` / `filter_to` / `capture_closure` | three capture helpers (callers: 1127, 1334, 2754 snapshot; 2263 capture_closure; filter_to — no production callers) | converge into `CapturedEnv::capture(&Env, free_vars, owner)` with rest-in-capture normalization (Sol P0-2); `filter_to` deleted | MED |
| 6 | eval.rs:90 | `Env::set` | lexical binding write | unchanged (lexical only, never defs) | LOW |
| 7 | eval.rs:110 | `Env::set_root` | writes frames[0] — today the def sink AND prelude sink | def-family stops using it; retained for embedder root-frame seeding (cli/kernel bind caps into frames[0]) | MED |
| 8 | eval.rs:140 | `Env::bindings()` | merges ALL frames (prelude leaks into exports via caps:469) | retained deprecated for one release; new `Env::local_bindings()` = defs-local escaped snapshot; caps:469 call site swapped (approval item 2) | MED |
| 9 | eval.rs:80 | `Env::get` | frames-only lookup | frames → `defs` chain (escape_with on defs hits) | HIGH |
| 10 | eval.rs:915 | `eval_def` (raw def path) | writes via set_root | routes to `Env::define` | MED |
| 11 | eval.rs:2185, 3472 | `Expr::Def` eval + analyzed "def" head | analyzed def paths | route to `Env::define` | MED |
| 12 | eval.rs:1249, 2751, 3489 | raw + analyzed defmacro | macro definition | route to `Env::define` | MED |
| 13 | eval.rs:2772-2837 | defcap eval (GliaCapInner built at 2828) | cap interning | methods rested; owner witness on handle; routes to `Env::define` | HIGH |
| 14 | eval.rs:1127-1133, 1334-1340, 2263-2277, 2754-2760 | fn/macro construction (4 sites incl. `snapshot()` + `compute_cap_status`) | builds `Val::Fn { arities, env, is_cap_free, cap_violation }` | builds `Val::Fn { arities, closure: Closure }`; cap-status fields kept until Stage E | HIGH |
| 15 | eval.rs:1138/1233/1349 | `invoke_fn` / `invoke_fn_with_handler_stack` / `invoke_macro` | call paths via for_call | consume `Closure`; escape at activation | HIGH |
| 16 | eval.rs:2588, 3054, 3159, 3603 | resume-fn / internal fn builders | construct Val::Fn directly | same payload change | MED |
| 17 | lib.rs:433-445 | `Val::Fn` / `Val::Macro` variants | `{arities, env: Rc<Env>, is_cap_free, cap_violation}` | `{arities, closure: Closure}` (opaque struct) | HIGH |
| 18 | lib.rs:555-556, 596-597 | callable eq/hash (env ptr) | identity = `Rc::ptr_eq(env)` | identity = `Rc::ptr_eq(closure.captured)` — same per-creation granularity, MUST be observably unchanged | HIGH |
| 19 | lib.rs:243 | `CapHandle { name, schema_cid, id, inner }` | sealed cap identity | + `owner: Option<OwnerRef>` (private field; no accessor) | MED |
| 20 | lib.rs:197/212/231 | `GliaCapInner` / `AttenuatedCapInner` / `HandledCapInner` | evaluator/kernel cap inners (`methods`/`base`/`handler` hold Vals) | contents rested for matching owner at construction (2828, 2898) or at define-time inner rebuild (kernel-built 5222 — no kernel change needed, see §4 cat 4) | HIGH |
| 21 | eval.rs:517/563 | `is_authority_free` / `compute_cap_status` | construction-time cached bit (STALE under late binding — Sol P1) | live, check-time analysis through owner chain + `Defs.version`; fields deleted Stage E | HIGH |
| 22 | lib.rs:798-812 | `PRELUDE` + `load_prelude(&mut Env, dispatch)` | evals 14 defmacro forms into env per call (callers: cli/shell.rs:705, kernel:2421, tests) | memoized `Rc<Defs>` (thread_local OnceCell), frozen after first build; signature unchanged — sets `env.defs = child_of(prelude)` | MED |
| 23 | src/cli/shell.rs:680,705; std/shell:281; kernel:2421 | REPL/module entry points | `Env::new()` + `load_prelude` | unchanged source; get `defining: true` + prelude inheritance for free | LOW |
| 24 | std/caps/src/lib.rs:264/306/346/469/477 | `IMPORT_CACHE`, `make_import_handler`, `import_env.bindings()` | legacy import; exports = merged bindings (prelude leaks) | ONLY change: line 469 `bindings()` → `local_bindings()`; cache and handler untouched (B3 is PR-1b) | LOW |
| 25 | Makefile wasm32-wasip2; std/caps guest tests | WASM-facing paths | PR-1 verified at 2 MiB stack | re-verify Stage G (iterative transforms keep stack flat) | MED |

## 3. Exact final type shapes

```rust
// ── eval.rs (module `own` is private: `mod own { … }` inside eval) ──

pub(crate) enum OwnerRef {                    // RC-SPECIFIC — never leaves eval::own except inside Closure/CapHandle
    Strong(Rc<Defs>),
    Weak(std::rc::Weak<Defs>),
}

pub struct Defs {                             // pub type, all fields private
    bindings: RefCell<HashMap<String, Binding>>, // container: LANGUAGE SEMANTICS; stored values rested: RC-SPECIFIC
    inherited: Option<Rc<Defs>>,              // LANGUAGE SEMANTICS (prelude chain)
    frozen: Cell<bool>,                       // LANGUAGE SEMANTICS
    version: Cell<u64>,                       // GC-NEUTRAL RUNTIME (authority invalidation)
}

struct Binding {
    value: Val,                               // rested copy — RC-SPECIFIC state within
    has_resting_owner_refs: bool,             // RC-SPECIFIC (fast-path flag)
}

pub(crate) struct CapturedEnv {               // LANGUAGE SEMANTICS (lexical snapshot); slot values rested: RC-SPECIFIC
    slots: HashMap<String, Val>,              // same Frame shape; keys = free vars only
}

#[derive(Debug, Clone)]
pub struct Env {
    frames: Vec<Frame>,                       // LANGUAGE SEMANTICS (lexical only now)
    defs: Rc<Defs>,                           // LANGUAGE SEMANTICS (definition owner)
    handler_stack: HandlerStack,              // LANGUAGE SEMANTICS (existing, unchanged)
    defining: bool,                           // LANGUAGE SEMANTICS (top-level privilege; env-invariant, never a toggle — async-safe)
    root_frame_is_lexical: bool,              // existing transitional-warning flag, unchanged (ADJACENT to retire later)
}

// ── lib.rs ──

pub struct Closure {                          // public-but-opaque: ALL fields pub(crate); Debug prints "Closure"
    pub(crate) captured: Rc<eval::CapturedEnv>, // LANGUAGE SEMANTICS (identity anchor + lexical snapshot)
    pub(crate) owner: eval::OwnerRef,           // RC-SPECIFIC
}

Val::Fn    { arities: Vec<FnArity>, closure: Closure }   // is_cap_free/cap_violation DELETED (Stage E)
Val::Macro { arities: Vec<FnArity>, closure: Closure }

pub struct CapHandle {
    name: String,                             // LANGUAGE SEMANTICS
    schema_cid: String,                       // LANGUAGE SEMANTICS
    id: CapId,                                // LANGUAGE SEMANTICS
    inner: Rc<dyn Any>,                       // LANGUAGE SEMANTICS + MIGRATION HAZARD (opaque host payloads)
    owner: Option<eval::OwnerRef>,            // RC-SPECIFIC (private; None for host/capnp caps)
}

// NativeFnImpl / AsyncNativeFnImpl: FROZEN as-is — MIGRATION HAZARD, documented, no growth.
// Frozen prelude storage (lib.rs): thread_local! { static PRELUDE_DEFS: OnceCell<Rc<eval::Defs>> }  // GC-NEUTRAL (BEAM literal-area seat)
```

Identity/equality: `(Val::Fn{closure: a,..}, Val::Fn{closure: b,..}) => Rc::ptr_eq(&a.captured, &b.captured)`; hash = captured ptr. Same granularity as today (one `Rc` minted per fn-form evaluation; rest/escape clone the enclosing `Val` but never replace the `captured` Rc) — spike p15 is the proof obligation.

## 4. Ownership choke-point map (the five categories — closed set)

| Cat | Site (symbol) | Helper called | Witness | Action | Weak location | Fault behavior | Tests |
|---|---|---|---|---|---|---|---|
| 1 Definition storage | `Env::define` (new; called from eval.rs 915, 2185, 3472, 1249, 2751, 3489, 2772/defcap, REPL forms) | `own::rest_for(&env.defs, v)` | `env.defs` (live Rc) | RESTS self-owned refs; bumps `version`; sets flag | `Defs.bindings` values | pre-checks: `!defining` → catchable `glia.error/def-not-top-level`; `frozen` → internal Fault | SEM: late-bind/recursion/gate/freeze; MECH: p01/p02/p05 ports |
| 2 Lookup/export enumeration | `Env::get` (eval.rs:80, defs-chain arm) and `Env::local_bindings` (new; replaces caps:469 use of `bindings()`) | `own::escape_with(&defs, v)` | the defs Rc being read | ESCAPES (flag-gated fast path) | none (output all-strong) | unmatched weak → internal Fault (release-checked) | SEM: closure/macro/cap survival, export snapshot; MECH: p11/p12, weak-location probes |
| 3 Capture + activation | `CapturedEnv::capture` (replaces snapshot@1127/1334/2754, capture_closure@2263) and `Env::for_call` (225) | capture: `own::rest_for(owner, slot)` per slot; for_call: `own::escape_with(witness, slot)` | at capture: the defining env's `defs`; at call: `closure.owner` upgraded via witness rules | capture RESTS self-owned; for_call ESCAPES | `CapturedEnv.slots` | dead weak owner at activation → internal Fault | SEM: nested closure programs; MECH: p04 port, p13, p15 |
| 4 Cap sealing/dispatch/attenuation | defcap build (2828); dispatch (654, 3152, 3259); glia attenuation (2898); define-time inner rebuild for kernel-built inners (kernel:5222 constructs `AttenuatedCapInner` — glia handles it when the cap is def'd: `rest_for`'s Cap arm downcasts the three KNOWN inners and rebuilds with rested matching-owner contents; unknown `Rc<dyn Any>` = leaf/trust boundary — no kernel change) | `own::seal_cap_inner` (construction), `own::escape_with` (dispatch via outer witness), `own::transfer_owner` (attenuation) | outer `CapHandle.owner` | seal RESTS methods/base/handler; dispatch ESCAPES; attenuation TRANSFERS | sealed inner contents | unmatched → internal Fault | SEM: defcap/attenuate/dispatch; MECH: p07/p08/p09 ports |
| 5 Module/export construction | same `local_bindings` helper as cat 2, consumed at caps:469 (legacy import) and future PR-1b `eval_import` | `own::escape_with` | module env's `defs` | ESCAPES | none | as cat 2 | SEM: module-owned exports, prelude excluded; MECH: last-escapee reclamation |

**Privacy proof:** `OwnerRef`, `rest_for`, `escape_with`, `seal_cap_inner`, `transfer_owner`, and the `Binding` flag all live in `mod own` (private to `eval`). The only items exported from it are the five helpers (`pub(super)`) plus the `OwnerRef` type (`pub(crate)`, needed inside `Closure`/`CapHandle` fields). No constructor for `OwnerRef` is visible outside `own` — variants are constructed only by the helpers; `Closure`/`CapHandle` construction goes through crate-internal builders. Compile-time: any new call site would need `eval::own::…` which does not resolve outside `eval`. A `#[cfg(test)]` re-export feeds the RC-MECHANISM tests.

## 5. Definition-path map

All eight paths converge on **one checked operation**: `Env::define(&mut self, name: String, val: Val) -> Result<(), NativeSignal>`:
(1) gate: `!self.defining` → `NativeSignal::throw(glia.error/def-not-top-level …)` (thrown BEFORE any mutation); (2) freeze: `self.defs.frozen` → `NativeSignal::fault(…)`; (3) `defs.define(name, rest_for(&defs, val))` + flag + `version += 1`.

- raw `def` → `eval_def` (915) → `Env::define`
- analyzed `def` → `Expr::Def` (2185) and head-dispatch (3472) → `Env::define`
- raw `defmacro` (1249) / analyzed `Expr::DefMacro` (2751) / head (3489) → build macro Val → `Env::define`
- `defn` → prelude macro (prelude.glia:49) expands to `def` → caller-env def path (above)
- `defcap` (2772) → build sealed cap → `Env::define`
- top-level macro expansion → `invoke_macro` (1349) evaluates the EXPANSION in the caller's env, which at top level has `defining: true` → definition succeeds (locked macro rule); the macro's own BODY runs in a `for_call` env with `defining: false`, so a definition attempted *during expansion computation* correctly throws
- REPL: cli/shell.rs:680 and std/shell:281 use `Env::new()` (`defining: true`) → every top-level form defines directly
- module init (kernel:2421): `Env::new()` + `load_prelude` → `defining: true` for the module's top-level forms; any function CALLED during init runs under `for_call` → `false`

**Async/cancellation:** `defining` is a constructor-set invariant of each `Env` value, never toggled at runtime (Sol P1 requirement). There is no set-then-restore window, so suspension at any await point or cancellation of any future cannot leave privilege in a wrong state — the env that resumes is the env that suspended.

## 6. Staged implementation sequence

**Stage A — structural types (uncalled).** Files: eval.rs (+`mod own` with OwnerRef/rest_for/escape_with/seal/transfer ported from spike, iterative), +`Defs`, +`CapturedEnv`; lib.rs (+`Closure` struct unused, +PRELUDE_DEFS cell unused); Env gains `defs`+`defining` fields with today's behavior preserved (get/set untouched; defs created but unread). Diff ≈ +500/−0. Compile breaks: none (Env literal sites: new/default/snapshot/filter_to/for_call updated in-place). Tests before continuing: whole suite green unchanged + new `own` unit tests (transform laws on Val). Rollback: delete `mod own` + 2 fields.

**Stage B — definition semantics.** Route the 8 paths through `Env::define`; `Env::get` defs-chain fallthrough; `local_bindings`; `load_prelude` memoize+freeze; kernel/cli untouched at source level. Diff ≈ +250/−80 (eval.rs, lib.rs). Compile breaks: none. **Behavior flip 1** (late binding, recursion, def-inside-fn now throws, prelude no longer in frames[0]). Existing tests asserting old behavior updated per §8 differential map (inventory them in the same change). Tests: SEMANTIC group 1 (late binding, named+mutual recursion, gate, prelude shadow/isolation, freeze fault). **Sol Review 1.** Rollback: def paths back to set_root (one function each).

**Stage C — functions and macros (THE flag-day).** lib.rs variant change (17/18 above); eval.rs construction sites (1127-1133, 1334-1340, 2263-2277, 2754-2760, 2588, 3054, 3159, 3603), invoke paths (1138/1233/1349), for_call (225), env-binding pattern sites (1614, 2930, 3016, 3531, 3577, 4481, 4498), compute_cap_status adapted to `&CapturedEnv` (fields retained until E). Diff ≈ +300/−200, all crates/glia (verified: no external Val::Fn/Macro tokens). Expected compile-broken interval: one editing session; sequence lib.rs first, then eval.rs top-to-bottom; `cargo check` gates. Tests: full suite + SEMANTIC group 2 (closure survival, capture semantics unchanged for locals) + MECH p04/p13/p15/p16 ports. Rollback: revert the C commit-range (stash checkpoint before starting).

**Stage D — capabilities.** lib.rs `CapHandle.owner` + make_cap internal builder; defcap seal (2828 → seal_cap_inner); dispatch escape (654/3152/3259); attenuation transfer (2898) + define-time inner rebuild for known inners (covers kernel:5222 without kernel changes). Diff ≈ +200/−60 (glia only). Tests: SEMANTIC caps group + MECH p07/p08/p09. **Sol Review 2 after D.**

**Stage E — live authority.** Delete `is_cap_free`/`cap_violation` fields (small flag-day: 556, 1128-33, 1332-40, 2264-77, 2755-60 + tests 4589-4646); rewrite `is_authority_free`(517)/`compute_cap_status`(563) as check-time traversal: captured slots + free top-level names resolved live through the owner chain (visited-set cycle guard; `Defs.version` recorded for future memoization — no memo in PR-1b.0). Diff ≈ +120/−100. Tests: authority data→cap flip, cap→data flip, zero-grant cell capture (existing 4638) preserved.

**Stage F — full ownership audit.** Re-run the §9 preflight storage audit against real code; grep-audit `own::` confinement; weak-location probes on every category; remove `filter_to`; snapshot() has no remaining callers → delete. Diff ≈ +40/−60.

**Stage G — tests, performance, WASM, docs.** Tag all tests; production benches (§9 plan); wasm32-wasip2 + std/caps guest tests + 2 MiB stack verification; documentation per §11 map; CHANGELOG/migration notes. **Sol Review 3.**

Total estimate: ≈ +1,410/−500 across crates/glia (lib.rs, eval.rs, expr.rs docs) + 1 line std/caps + 0 kernel/cli/shell — consistent with Sol's revised +900/−350 core estimate plus tests/benches.

## 7. Test map (→ stage, file)

SEMANTIC (tag `// SEMANTIC`) — eval.rs test mod unless noted:
late binding (B); named recursion (B); mutual recursion (B); top-level-only def incl. macro-expansion-allowed + fn-during-init-denied (B); prelude shadowing/isolation/no-export (B); module-owned exports via local_bindings (B; std/caps test for import-map contents); closure survival after env drop (C); macro survival (C); cap survival (D); attenuation lifetime (D); authority data→cap and cap→data (E); foreign-owner composition/nested module maps (C/D — F1 regression); durable-leaf jurisdiction: big `Bytes` in local map is O(1) leaf (F, §9 bench-asserted); WASM semantics = full wwtest suite on wasm target (G).

RC-MECHANISM (tag `// RC-MECHANISM`) — eval.rs `mod ownership_tests` with `cfg(test)` access to `own`:
no routine owner cycles / exact reclamation after final escape (B/C ← spike p01, p02, p11, p12); nested capture resting (C ← p04); weak-location invariant probes (F ← props law 4); unmatched witness fault (C ← p13 + props law 3); rest/escape idempotence (A ← props law 1); identity/hash preservation (C ← p15); callable map keys (C ← p16); atom accepted-cycle isolated/breakable + cross-module ephemeron-shape variant from preflight §4 finding 8 (F ← p17 + new); deep iterative traversal 100k+ (A ← p14); strong/weak count probes at each choke point (B–D).

Direct spike→production equivalents: p01–p17 all port; props laws 1–4 port as targeted cases (full proptest harness optional — ADJACENT).

## 8. Differential behavior map

Intentional changes (with canonical programs):
1. `(def x 1) (defn f [] x) (def x 2) (f)` — before `1`, after `2` (late binding).
2. `(defn fact [n] (if (< n 2) 1 (* n (fact (- n 1))))) (fact 5)` — before type error (name unresolved), after `120`.
3. Mutual recursion `(defn even? [n] (if (= n 0) true (odd? (- n 1))))` + odd? — before broken, after works.
4. `(defn install [] (def x 1)) (install)` — before silent evaporation, after catchable `glia.error/def-not-top-level`.
5. Import map of a module defining only `answer` — before contains prelude macros + answer, after `{:answer 42}` only.
6. Module shadowing `(defmacro when …)` locally — before mutates shared frame[0] view, after local override, prelude untouched.
7. Authority: `(def x 1) (defn f [] x) (def x <cap>)` — `f`'s cap-status now reflects the live redefinition.

Must NOT change: lexical locals still snapshot (`(let [y 1] (fn [] y))` unaffected by later local rebinding — there is none); `recur`; try/throw/try-resume + effect protocol; callable equality/hash granularity (fresh identity per fn-form evaluation; `=` on the same closure value true, on re-evaluated form false); atom identity semantics; NativeFn/AsyncNativeFn behavior; printing/Debug output for guest-visible values; `set` lexical semantics; cap identity (`CapId`) semantics; PR-1 exception/fault taxonomy.

## 9. Performance plan (provisional stop thresholds set NOW, before measurement)

Production benches (criterion-free timing tests, `--release`, marked `#[ignore]` for normal runs): pure-data fast path lookup ≤ 2.0× plain clone (spike: 1.19); deep local container 100k define+lookup ≤ 25 ms, no stack growth; 10k-node container with 1 closure: affected lookup ≤ 1 ms (spike 0.058); 1k duplicate closures: linear, ≤ 0.1 ms/iter; foreign-owner values: no transform cost beyond clone; export snapshot of 1k-binding module ≤ 5 ms; repeated lookup of same flagged binding: no degradation across 10k reads; live authority analysis of a 100-binding module callable ≤ 1 ms; 10 MB `Bytes` leaf in a defined map: define/lookup cost independent of payload size (≤ 10 µs overhead vs empty map) — the jurisdiction bench; mixed map {closure, 10 MB Bytes}: cost = closure cost only; WASM: wwtest green at the existing 2 MiB stack budget, no new memory-limit failures. Breach of any threshold → stop condition (§13), not silent acceptance. No CHAMP work.

## 10. Public API / migration surface

Changes: `Val::Fn`/`Val::Macro` payload (workspace-internal; zero external users verified); new public-opaque `Closure` (no public methods except `Debug`; approval 1); `CapHandle` private field added (no accessor); `Env::local_bindings` added pub (caps needs it); `Env::bindings` kept + doc-deprecated; `Env::snapshot`/`filter_to` removed (no external users); `load_prelude` signature unchanged; `Defs` public type, fully private fields, no public constructor (reachable only via Env). Confirmed: `OwnerRef` never public; no weak/strong words in any error, Debug, or doc-visible text (`Closure` Debug prints `Closure`, `CapHandle` Debug unchanged); collections untouched (no Rc/Weak awareness); `NativeFnImpl`/`AsyncNativeFnImpl` frozen; the one host-visible change (caps:469 one-line swap) is the approved export-boundary consequence.

## 11. Documentation map (written in Stage G)

`doc/designs/value-domains.md` (NEW): domains A/B/C + variant table, identity principle, certification-not-type + fails-closed rule, domain C descriptors, no-transparent-lazy-container rule, non-goals. No RC content.
`doc/designs/definition-ownership.md` (NEW): normative semantics (Defs, late binding, gate, exports, frozen prelude) — memory-model-free; then fenced "Current implementation: RC ownership barrier" (Graph 4, ledger, choke points); Graph 4 jurisdiction verbatim; RC deletion inventory; SEMANTIC/RC-MECHANISM test taxonomy; memory-model revisit triggers.
`doc/designs/macro-staging.md` (NEW, short): records the known eager-analysis staging bug + why PR-1b.0 does not fix it (parked; needed later for canonical code identity).
`doc/designs/value-contract.md` (EDIT): value-domains cross-ref; PR-2 inputs (summary bits, canonical float); PR-3 canonical-form note.
`doc/architecture.md` (EDIT): "Memory model: local executable graphs and durable data" subsection (3 paragraphs incl. authority/pointer-strength separation; release ≠ revoke).
CHANGELOG/migration notes: the seven §8 behavior changes with before/after programs.
Future skeletons: `portable-callables.md`, `process-memory.md` per memory-model study §13.

## 12. Sol review checkpoints

Review 1 (after B) — type graph: eval.rs `Defs`/`Binding`/`CapturedEnv`(unused yet)/`Env` fields/`Env::define`/`Env::get`/`local_bindings`/`mod own` surface; lib.rs `load_prelude`+`PRELUDE_DEFS`+`Closure`(unused). Questions: field-level cycle possibilities, privacy, defining invariant, freeze ordering.
Review 2 (after D) — ownership mechanics: all §4 sites by line; `CapturedEnv::capture`; `for_call`; the three inner rebuilds; equality/hash impls (lib.rs 555/596 region); weak-location probe tests. Questions: F1 scoping, nested captures, foreign preservation, attenuation transfer, identity stability.
Review 3 (final diff): storage-site audit table vs code; Stage E authority accounting; bench results vs §9 thresholds; wasm results; §10 API table; docs; drift ledger.

## 13. Hard stop conditions

The prompt's thirteen, verbatim adopted, plus repository-specific: (14) any external crate found constructing `Val::Fn`/`Val::Macro` or reading their fields (breaks Stage C containment — re-verify at C start); (15) `snapshot()`/`filter_to()` found load-bearing for effect/handler machinery in a way `CapturedEnv` cannot express; (16) prelude found to define non-macro values at freeze time (breaks macro-only certification — currently 14/14 defmacro, verified); (17) `root_frame_is_lexical` cell-warning path conflicting with the capture change; (18) any §9 threshold breach; (19) `Env::bindings()` deprecation found to break an embedder path other than caps:469.

## 14. Drift ledger

REQUIRED PR-1B.0 SEMANTIC: Defs, define gate, late binding, recursion, frozen prelude, local_bindings export semantics, defining invariant, live authority model.
REQUIRED PR-1B.0 RC MECHANISM: mod own (OwnerRef, rest_for, escape_with, seal, transfer), Closure/CapHandle owner fields, flag, capture normalization, for_call escape, inner rebuilds.
REQUIRED FUTURE-GC ABSTRACTION: named choke-point helpers, own-module privacy, frozen native aliases, test tags, deletion inventory, opaque Closure.
REQUIRED DOCUMENTATION/TEST: §7 map, §11 docs, CHANGELOG.
ADJACENT — PARK: proptest harness port; authority-status memoization (version-validated); retiring root_frame_is_lexical; Env::bindings final removal; pinned-durable-bytes accounting.
DRIFT — DO NOT IMPLEMENT (confirmed exclusions): B3 import; IMPORT_CACHE removal (no compile dependency exists — verified caps compiles against local_bindings swap alone); PR-2 collections; durable handles; portable callables; effect declarations; remote caps; GC/cycle collection/process heaps; namespaces; explicit exports; macro-staging fix (documented only); callable identity redesign; callable-map syntax; unrelated cleanup.

## 15. Estimated files/diff by stage

| Stage | Files | Est. diff | Compile-broken | Behavior flip |
|---|---|---|---|---|
| A | eval.rs, lib.rs | +500/−0 | none | none |
| B | eval.rs, lib.rs | +250/−80 | none | YES (defs semantics) |
| C | lib.rs, eval.rs | +300/−200 | one bounded session | capture internals only |
| D | lib.rs, eval.rs | +200/−60 | none | none guest-visible |
| E | lib.rs, eval.rs | +120/−100 | brief (field deletion) | authority accuracy |
| F | eval.rs | +40/−60 | none | none |
| G | tests/benches/docs, std/caps (1 line) | +400 (tests/docs) | none | none |

## 16. Decisions requiring Louis's approval

1. The `Closure` public-opaque struct (`Val::Fn { arities, closure: Closure }`, all fields `pub(crate)`) — the minimal shape that keeps `OwnerRef` private inside a public enum; NOT the excluded callable redesign (arities stay in place; identity semantics unchanged).
2. The one-line external change: std/caps lib.rs:469 `bindings()` → `local_bindings()` — lands the approved export boundary structurally in PR-1b.0 (prelude stops leaking into import maps) while leaving IMPORT_CACHE/B3 for PR-1b.
3. Updating the existing tests that assert the seven old behaviors in §8 (they are the differential map made executable).
4. The §9 provisional thresholds as binding stop conditions.
5. Stage G writes the §11 documents (first production-doc edits of this arc).
6. Begin Stage A on approval of 1–5.
