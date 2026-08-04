# PR-1 — Sol reconciliation v2 (revised semantic model; design only, no code edited)

Supersedes `.context/pr1-sol-reconciliation.md`. Inputs: updated locked model (2026-08-02), implemented tree on `glia-control-extraction`.

## 1. Revised semantic taxonomy

| Class | Meaning | Transport |
|---|---|---|
| VALUE | ordinary result | `Flow::Value` |
| EXCEPTION | recoverable program condition; runtime stays trustworthy | performed as `:glia.exception`; `try` catches, `try-resume` resumes |
| OTHER EFFECT | ordinary effect request | existing machinery; unhandled → `EvalError::Unhandled` |
| PRIVATE CONTROL | lexical `recur` (tail-only), resume short-circuit | `Flow::Recur` / `Control::Resume`, crate-private |
| INTERNAL FAULT | runtime cannot safely promise continued execution (panic-analog) | `Control::Fault` → `EvalError::Fault`, bypasses all handlers |

`FaultKind::{Language, Runtime}` is **dropped** — no repository evidence supports two operational classes (see §3: after reclassification, only two producer families remain and both mean the same operational thing: "supervisor, take over"). `glia.error/invalid-recur` becomes an **exception tag**.

## 2. Continuation-protocol table

Context: continuations are created by `perform_dispatch` (oneshot pair; tx wrapped by `make_resume_fn`, rx awaited at the perform site). The handler machine owns delivery; the guest sees only the resume `NativeFn`. In kernel/cli, `{cap}-handler` natives are ordinary env bindings, so guests can invoke them directly with arbitrary "resume" arguments (verified: kernel:2336, cli:690-698; std/shell hides handlers inside `HandledCapInner`).

| Failure | Creator/owner | Guest-reachable? Reproducer | Legal? Runtime OK after? | Classification |
|---|---|---|---|---|
| Already resumed (double resume) | tx taken on 1st call | YES — native or stashed resume fn called twice | illegal use; healthy | **EXCEPTION** (`continuation-already-resumed`) — already implemented |
| Handler returns without resuming (abortive) | machine drops body | YES — every `try` | legal | **NOT AN ERROR** — already implemented |
| Resume after handler scope exits / stray resume | guest stashes resume fn in an atom, calls it later: `(def s (atom nil))`, try-resume handler `(reset! s r)`, later `((deref s) 1)` | YES | illegal use; healthy (send to dead slot is a no-op; the Resume signal then unwinds) | **EXCEPTION** — new: convert body-originated `Control::Resume` at the handler machine and at `seal` into a thrown `continuation-scope` exception (today: Runtime fault at seal only) |
| Invalid resume function (non-NativeFn passed to a handler native) | kernel/caps/cli `call_resume` | YES — `(host-handler (list :id) 42)` | illegal use; healthy | **EXCEPTION** (today: `NativeSignal::fault` — reclassify; the resume protocol is not unforgeable from the guest because handlers are plain bindings) |
| Invalid resume value/arity | `make_resume_fn` arity check | YES | illegal use; healthy | **EXCEPTION** — already implemented |
| Sender dropped w/o send → rx abandoned | machine internals | Believed **unreachable**: abortive path drops the body future before re-polling it; Handling state never polls the body; cancellation drops both ends unpolled | n/a | **INTERNAL FAULT** (defensive; keep `continuation-abandoned` payload) |
| Receiver dropped, then send | stashed-resume case | YES (same stash reproducer) | send is a silent no-op; flows into stray-resume above | folds into **EXCEPTION** (stray resume) |
| Native resume propagation | native returns the resume fn's signal | YES — every cap handler | legal | **PRIVATE CONTROL** — unchanged |
| Resume across import/module scope | module resume stashed, called by importer | YES (post-§4 import design) | illegal use; healthy | **EXCEPTION** (same stray-resume conversion) |

## 3. Reclassification of every current fault producer

| Site (file/symbol) | Reproducer | Guest-reachable | Runtime OK after | Old | **New** |
|---|---|---|---|---|---|
| `Flow::into_value` non-tail recur (eval.rs, ~40 chokepoints) | `(loop [] [(recur)])` | YES (documented ops) | yes | Language fault | **EXCEPTION** `glia.error/invalid-recur` — dispatched on the current stack at each chokepoint (macro-based, no new future layer; see §9) |
| `seal` top-level stray recur | `(recur 1)` at REPL | YES | yes | Language fault | **EXCEPTION** (dispatched at toplevel before sealing; boundary shows `[glia.error/invalid-recur]`) |
| `seal` escaped Resume | stash-resume reproducer (§2) | YES | yes | Runtime fault | **EXCEPTION** (continuation-scope) |
| `perform_dispatch` rx-abandoned | believed unreachable (§2) | no | n/a | Runtime fault | **INTERNAL FAULT** |
| Host-effect handler `Err` (eval.rs host loop) — includes MCP `:stdout`/`:exit` refusals (cli:1158-1169) and kernel `write_stdout` failure (kernel:737) | `(perform :stdout "x")` under MCP mode | YES (documented `perform`) | yes — guest could catch and degrade (skip printing, fall back) | Runtime fault | **EXCEPTION**, delivered at the suspended perform site (new oneshot error delivery; §9 patch 3) — *approval item 1* |
| kernel/caps/cli `call_resume` invalid-resume (`NativeSignal::fault` ×3) | `(import-handler (list "x") 42)` | YES | yes | Runtime fault | **EXCEPTION** |
| caps import-poll `Pending` ×2 (`NativeSignal::fault`) | not reachable from Glia given the import sandbox (sync NoopDispatch, no async natives) | no | evaluator fine, but import machinery invariant broke | Runtime fault | **INTERNAL FAULT** (kept; the one remaining embedder `fault` user) |
| Impossible evaluator branch / malformed internal Expr state | no current producer | no | no | reserved | **INTERNAL FAULT** (reserved) |
| Wrong arity/type, division-by-zero, depth limit, unbound symbol, cap denials, `match` no-clause | — | YES | yes | exception | **EXCEPTION** (unchanged) |
| Import not found / malformed module source / module init failure / escaped module effect | `(perform import "missing")` etc. | YES | yes | stringified throw (lane-destroying) | **EXCEPTION / OTHER EFFECT** per §4 |

Result: faults shrink to exactly (a) evaluator/continuation-machinery invariants and (b) the import single-poll invariant — one operational class. Hence `Fault { payload: Val }` with no kind split (§5).

## 4. Import propagation — exact design

Imports are normal effectful operations (no evidence of intentional isolation was found — the fresh `Env` provides *lexical* isolation only, and the current stringification is the accident PR-0/PR-1 inherited).

**Chosen design: A′ — structured re-dispatch into the importer's dynamic context at the settle chokepoint**, with design B (threading the importer's `HandlerStack` into the native) rejected for PR-1: natives deliberately do not receive evaluator context, and adding a context-passing channel is a handler-API redesign (excluded). C variants (deferred evaluation, suspension multiplexing) require a guest eval facility or a multiplexed continuation protocol — out of scope.

```rust
// glia — public constructor unchanged from v1 design:
impl NativeSignal {
    /// Re-inject a nested evaluation's boundary error into the CALLER's
    /// dynamic context, preserving structure. Requires already holding an
    /// `EvalError` (only a completed evaluation yields one); guest code has
    /// no path to this API, so faults cannot be forged from Glia.
    pub fn forward(err: crate::eval::EvalError) -> Self;
}
pub(crate) enum NativeSignalKind { Throw(Val), Resume(Val), Fault(Fault), Forward(Box<EvalError>) }

// settle_native gains:
Err(NativeSignal(NativeSignalKind::Forward(e))) => match *e {
    // Faults stay faults — bypass everything.
    EvalError::Fault(f) => Err(Control::Fault(Box::new(f))),
    // Escaped exceptions AND ordinary effects RE-DISPATCH on the caller's
    // (importer's) live handler stack: `try` around the import catches
    // module-init exceptions; importer effect handlers receive module
    // effects with original EffectTarget + data. Unhandled again → escapes
    // structurally, as before.
    EvalError::Unhandled(req) => perform_dispatch(hs, req.target, req.data).await,
},
```

- Import target not found / unreadable / non-UTF-8 / parse failure → `NativeSignal::throw(structured payload)` → catchable (`ImportError`-style fallback works: `(try (perform import "x") (catch _ e fallback))`).
- Module-context: exception payloads that are maps get an assoc'd `:glia.import/module "<resolved>"` key (additive, lane-preserving); non-map payloads and effect/fault lanes are untouched, with the path logged instead.
- **Documented limitation** (*approval item 2*): re-dispatch happens after the module's own continuation is discarded, so a *resumptive* importer handler resumes the **import expression**, not the module's interior perform site. Abortive handling (the dominant pattern) is exact. Full interior resumption requires design B's context threading — deferred.
- caps changes: the four poll arms use `forward` (Pending arms stay `fault`); the two import `NoopDispatch`es unchanged.

## 5. Simplified Fault API

```rust
/// The runtime cannot safely promise continued execution of this Glia
/// computation. Panic-analog from the guest's perspective: bypasses `try`
/// and `try-resume`, escapes to the embedder/supervisor for log /
/// terminate / restart. Not a Rust panic: structured escape keeps the
/// Wetware layer in charge.
#[derive(Clone, Debug)]
pub struct Fault { payload: Val }          // no kind field
impl Fault { pub fn payload(&self) -> &Val }
impl Display for Fault                      // = payload display
```

- `Fault` stays **public** (embedders match `EvalError::Fault` and inspect `payload()` for logging/supervision — kernel init.d, shells, MCP).
- Constructors: `pub(crate) Fault::new` + public `NativeSignal::fault(...)` retained — caps' import-Pending invariant is a genuine trusted-embedder fault producer, and Rust module privacy cannot scope "trusted" more narrowly than the documented contract. (`fault` remains a de-escalation: strictly less guest-visible than `throw`.)
- Payload tags remaining on the fault lane: `glia.error/continuation-abandoned`, `glia.error/internal` (import-Pending, reserved invariants). `glia.error/invalid-recur` **moves to the exception lane**.
- No `panic!` replacement — structured escape preferred, as directed.

## 6. Macro staging — verification and deferred issue

**A (source-level boundary):** verified — a malformed operand (e.g. `(try (let [x] x) …)` at toplevel) is rejected during `eval_toplevel`'s whole-form analysis, before the `try` macro (a runtime env lookup) ever expands or installs its handler. Under the locked model this stays outside lexical `try`. With Language faults dropped, its lane is: **pre-execution analysis failure → thrown as an exception at the toplevel dynamic position** — where no handlers exist yet by construction, so it reaches the boundary uncaught; an enclosing *runtime* re-analysis (macro expansion, defcap, future eval op) raises catchably (*approval item 3* pins this single-lane reading — it removes the need for any pre-execution special case while preserving "lexical try does not catch it" observationally).
**B (general staging bug):** confirmed and recorded — `analyze_list`'s generic arm (`expr.rs:390-399`) eagerly analyzes all operands before knowing the head resolves to a macro. Reproducer: `(defmacro m [x] 1)` then `(m (let [y] y))` → analysis failure although `m` never uses `x` as an expression; via the legacy raw pipeline the same call succeeds → **pipeline divergence**. Semantic impact: macros cannot take non-expression operand syntax in the analyzed pipeline. Smallest principled future fix: in the generic-call arm, analyze operands *lazily* (store raw, analyze on first non-macro resolution) or re-analyze from `raw_args` when the head resolves to a macro — an analyzer staging change. **ADJACENT FIX — APPROVAL REQUIRED**; not needed for control extraction; not fixed in PR-1.
No guest-callable runtime `eval`/`read`/`analyze` exists today (verified); documented, none added.

## 7. Sealed capability API (unchanged from v1 reconciliation, now locked)

Redacted manual `Debug` for `CapId` (`CapId(..)`); no ctor/accessor; monotonic counter kept (process-local private table key; possession-not-secrecy principle + capnp import/export-index analogy documented at `CapId`/`CapTarget`). `CapHandle::id()` removed (zero production consumers). `EffectTarget::Cap(CapTarget)` with all-private fields, minted only by `CapHandle::effect_target()`; `EffectRequest.target` private with `EffectRequest::keyword(name, data)` (*approval item 4*) + `effect_type()`/`data`; matching uses `CapId` internally; attenuation mints fresh identity. Compile-fail doctests: forging from guessed integers, debug output, escaped requests, and cloned metadata all fail to compile.

## 8. Required test changes

Corrections: rename `unbound_symbol_call_is_catchable_by_try` → runtime-arity pin; `fault_bypasses_try_non_tail_recur` + `non_tail_recur_cannot_become_stored_data_or_transfer` **flip** to catchable-exception pins (still asserting no stored sentinel / no active transfer: the loop must NOT spin, the vector must NOT contain debris — the failing expression raises); `recur_outside_loop` → exception tag pin; handler-depth test actually catches; second-resume pin corrected (Glia-fn handlers cannot sequence a second resume — native-only, pinned in effect.rs); cli MCP test flips from fault-kind assert to catchable-exception assert (pending approval 1).
Additions per §8 of the prompt: arity/type catch pins (exist); non-tail recur catchable + inspectable tag; double-resume catchable; abortive-no-resume-is-valid pin; injected `NativeSignal::fault` native bypasses `try` AND `try-resume`; import: not-found caught, module-exception caught, module-effect handled by importer handler, injected module fault bypasses; malformed-source-before-expansion stays outside lexical try; stray-resume exception; `try` abortive / `try-resume` resumptive+one-shot (exist); cap-forgery compile-fail; attenuation fresh identity (exists); constrained-stack (2 MiB thread) deep exception/effect program; one deep-nesting form through the wasmtime-backed `shell_e2e`.

## 9. Smallest correction sequence

1. **Recur reclassification**: `into_value` chokepoints → `value_of!` macro dispatching `invalid-recur` as an exception (no new future layer — stack budget preserved); toplevel recur + stray resume → exceptions; drop `FaultKind`, collapse `Fault { payload }`; flip the recur/fault tests.
2. **Continuation reclassification**: 3× `call_resume` fault→throw; keep import-Pending faults; effect.rs pins unchanged.
3. **Host-effect error delivery** (gated on approval 1): oneshot slot carries `Result<Val, Val>` (`Sender::send_err`); host loop delivers handler errors to the suspended perform, which throws them catchably; abandoned stays fault; cli/kernel host closures unchanged otherwise; MCP tests flip.
4. **Import propagation**: `NativeSignal::forward` + re-dispatching settle arm; caps arms use it; `:glia.import/module` context key; import test suite.
5. **Sealed cap API** + compile-fail tests + `EffectRequest::keyword` migration (2 external sites).
6. **Smaller corrections**: remove `thrown()` (kernel test helpers → `map_throw` capture); remove 4 `PartialEq` derives + mechanical `assert_eq!(r, Ok(v))` → `assert_eq!(r.unwrap(), v)` sweep (~240 sites); keep Control boxing.
7. Constrained-stack + wasm stress tests; boundary-output tests; CHANGELOG migration entry + principle comments.
8. Full re-verification (workspace + std tests, clippy, fmt, wasm32-wasip2).

## 10. Drift report

**REQUIRED CONSEQUENCE**: everything in §9 (all explicitly directed or forced by the revised taxonomy).
**ADJACENT — APPROVAL REQUIRED**: eager-operand macro staging fix (§6B, deferred); `glia.error/analysis` tag for analysis-failure payloads (currently legacy `{:type :internal}` maps); design-B import context threading (full interior resumption).
**DRIFT — DO NOT IMPLEMENT**: PR-2 collections/float; callable changes; printer/reader; handler-API redesign (arity protocol locked); static tail analysis; analyzer/macro rewrite; random cap IDs; `panic!`-based faults; unrelated cleanup (std/shell's 9 pre-existing clippy warnings stay).

## 11. Decisions still requiring approval

1. **Host-effect handler failures → catchable exceptions** delivered at the perform site (covers MCP `:stdout`/`:exit` refusals and kernel stdout-write failure; requires the small oneshot error-delivery extension; reverses the earlier fault classification and flips two cli tests). Criteria-driven recommendation: approve.
2. **Import resumption limitation** under design A′: resumptive importer handlers resume the import expression, not the module interior (abortive handling exact). Accept documented limitation now; design B deferred.
3. **Pre-execution analysis failures** = exceptions raised at the toplevel dynamic position (no handlers exist there → observationally uncatchable by lexical `try`, no special lane needed). Confirm this single-lane reading.
4. `EffectRequest::keyword` public constructor (needed once `target` is sealed).
5. Stray-resume conversion points (handler-machine body arm + `seal`) — confirm the reproducer-driven EXCEPTION classification.
6. rx-abandoned stays an INTERNAL FAULT on unreachability grounds — confirm.
7. Import path-context via additive `:glia.import/module` key on map-shaped exception payloads (logging elsewhere) — confirm.
