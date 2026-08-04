# PR-1 — Sol review reconciliation (design only; no code edited)

Inputs: locked language model (2026-08-02), `.context/pr1-final-contract.md`, implemented tree on `glia-control-extraction`.

## 1. Reconciliation of Sol findings

| Finding | Verdict | Resolution |
|---|---|---|
| 1. Import lane destruction (stringify+rethrow in caps import) | **CONFIRMED — fix** | `NativeSignal::forward(EvalError)` opaque forwarding arm (§3); no stringification; lanes, `FaultKind`, targets, payloads survive verbatim |
| 2. Analyzer failure inside `try` not catchable | **REJECTED as defect** under locked semantics, with the required verification done (§2): the failure occurs during whole-form analysis in `eval_toplevel`, before `try` (a runtime macro) ever expands or installs its handler — a pre-execution Language fault. Misleading test renamed + boundary pinned (§5). One genuine adjacent bug surfaced separately: eager operand analysis (§2b) |
| 3. Capability identity insufficiently sealed | **CONFIRMED — fix completely** | Sealed API in §4: redacted `CapId` Debug, no numeric accessor, `CapHandle::id()` removed (zero production consumers verified), `EffectTarget::Cap` payload privately-fielded, `EffectRequest.target` private, compile-fail tests |
| `NativeSignal::thrown()` public | **CONFIRMED — remove** | Kernel test helpers (its only users, both `#[cfg(test)]`) switch to capture-via-`map_throw` (kept API); no new surface |
| `map_throw()` | keep (attenuation needs it) — as directed |
| Unapproved `PartialEq` derives | **CONFIRMED — remove** from `Fault`/`EvalError`/`EffectTarget`/`EffectRequest`; no production semantic requirement exists (`EffectTarget::matches` is the semantic comparator; `CapId`'s `PartialEq/Eq/Hash` and `FaultKind`'s derives are contract-specified and stay). ~240 test asserts migrate mechanically `assert_eq!(r, Ok(v))` → `assert_eq!(r.unwrap(), v)` |
| Boxed cold Control arms | keep — as directed |
| Misleading tests | corrected per §5 |

## 2. Macro staging verification

**(a) Do macros receive genuinely raw forms?** At invocation, yes: both pipelines pass `raw_args` (unevaluated `Val`s) to `invoke_macro`, the expansion is then analyzed (`eval.rs:2925`) and evaluated in the caller's env — the standard Lisp rule holds at the invocation step.

**(b) General staging bug — CONFIRMED, surfaced separately.** `analyze_list`'s generic arm (`expr.rs:390-399`) **eagerly analyzes every operand** (`raw_args.iter().map(analyze)…?`) before `Expr::Call` is built — i.e., before anyone knows the head resolves to a macro (macro-ness is a runtime env lookup). A macro whose operand is not an analyzable Glia expression never runs, even though it would have received the operand raw. The legacy `Val` path does *not* pre-analyze, so the two pipelines diverge for such operands. This is distinct from finding 2 (whose operand is ordinary code that is genuinely malformed). Fix would defer operand analysis for potentially-macro heads — an analyzer staging change: **ADJACENT FIX — APPROVAL REQUIRED**, not in PR-1.

**(c) Runtime-analysis carve-out.** No guest-callable `eval`/`read`/`analyze`/`macroexpand` builtin exists today (verified) — there is no runtime path that would expose *whole-form* analysis as a catchable operation, and none is added. Analysis does, however, run at three in-evaluator sites; the proposed lane boundary (one approval item, §10.1):

- `eval_toplevel` whole-form analysis (`eval.rs:3306`) — genuinely pre-execution → becomes **Language fault** (today: catchable throw — must change to honor the locked model).
- Macro-expansion analysis (`eval.rs:2925`) and defcap method analysis (`eval.rs:2804`) — occur mid-evaluation, after enclosing handlers are installed → remain **catchable exceptions** under the "analysis as a runtime operation" carve-out. This also keeps the analyzed and legacy pipelines convergent at runtime (legacy structural validation — e.g. `let` pairs check — is runtime validation and stays catchable).

## 3. Import lane preservation — exact design

```rust
// crates/glia/src/lib.rs
pub(crate) enum NativeSignalKind {
    Throw(Val),
    Resume(Val),
    Fault(Fault),
    /// Lane-preserving re-injection of a nested evaluation's boundary error.
    Forward(Box<crate::eval::EvalError>),
}

impl NativeSignal {
    /// Forward a boundary error from a nested evaluation (e.g. module
    /// import) preserving lane identity: faults stay faults with their
    /// FaultKind, escaped effects stay escaped effects with their target
    /// and payload. Requires already HOLDING an `EvalError`, which only a
    /// completed evaluation yields — this exposes no fault/effect
    /// constructors beyond that.
    pub fn forward(err: crate::eval::EvalError) -> Self;
}

// crates/glia/src/eval.rs — settle_native gains one arm:
Err(NativeSignal(NativeSignalKind::Forward(e))) => match *e {
    EvalError::Fault(f)      => Err(Control::Fault(Box::new(f))),
    // Escaped stays escaped: NOT re-dispatched on the importer's stack,
    // preserving today's uncatchability of module-boundary errors while
    // restoring their structure.
    EvalError::Unhandled(req) => Err(Control::Unhandled(Box::new(req))),
},
```

`std/caps` import handler: the four poll arms (`prelude error`, `eval error in {resolved}`, both `Pending` arms unchanged as faults) replace `Err(NativeSignal::throw(format!("import: … {e}")))` with `Err(NativeSignal::forward(e))`. Path context moves to a `log::debug!`-level note or is dropped (payload must survive unchanged; no wrapping). Design (2) (moving import outside the signal abstraction) rejected: the import handler must remain an `AsyncNativeFn` reachable from `perform`, so (1) is strictly narrower.

## 4. Sealed capability API — exact shape

```rust
// lib.rs
#[derive(Clone, PartialEq, Eq, Hash)]        // Debug now manual
pub struct CapId(u64);                        // private repr, no ctor, no accessor
impl fmt::Debug for CapId {                   // redacted: never prints the counter
    fn fmt(&self, f) { f.write_str("CapId(..)") }
}
// CapHandle: id() REMOVED (zero production consumers — verified);
//            name()/schema_cid()/inner()/effect_target() remain.

// effect.rs
pub enum EffectTarget {
    Keyword(String),                          // freely constructible (host frames)
    Cap(CapTarget),                           // payload sealed
}
/// Cap-targeted effect address. All fields private; minted only by
/// `CapHandle::effect_target()`. Authority comes from possession of a
/// capability reference, not from secrecy of its internal identifier —
/// but possession is exactly what private fields enforce.
#[derive(Clone)]
pub struct CapTarget { name: String, schema_cid: String, id: CapId }
impl CapTarget { pub fn name(&self) -> &str; }        // display only
// matches() compares CapId internally, unchanged semantics.

pub struct EffectRequest {
    target: EffectTarget,                     // PRIVATE — an escaped request no
    pub data: Val,                            // longer yields a matching target
}
impl EffectRequest {
    /// Keyword-targeted carrier (parse-error wrapping, embedder tests).
    /// No cap-targeted constructor exists.
    pub fn keyword(name: impl Into<String>, data: Val) -> Self;
    pub fn effect_type(&self) -> String;      // keyword | "cap:{name}"
}
```

Migration of the two external `EffectRequest{..}` literals (cli parse-error wrap, mcp_adapter test) to `EffectRequest::keyword(...)`; glia-internal constructions unaffected (crate-private field access). Counter IDs stay (process-local table keys; no trust/serialization boundary crossing found — they never leave the process, are not readable from Glia, and are redacted from Debug; documented alongside the capnp import/export-index analogy per the clarification). Attenuation keeps minting fresh identity (unchanged). Compile-fail doctests (```compile_fail) pin that outside code cannot: construct `CapId` (private field), construct `EffectTarget::Cap` (private-field `CapTarget`), extract a target from an escaped `EffectRequest` (private field), or use debug output (redacted).

## 5. Test corrections and additions

Corrections:
- `unbound_symbol_call_is_catchable_by_try` → rename `runtime_arity_failure_is_catchable_by_try`; delete its misleading analysis-error comment (it actually tests `(get)` runtime arity — correct behavior, wrong story).
- `one_shot_second_resume_is_catchable_inside_handler` → replace with a comment-corrected pin: a Glia-fn handler cannot sequence a second `resume` (the first short-circuits the handler body — asserted), and the one-shot violation is reachable only from native handlers, pinned by the existing `effect.rs` `make_resume_fn_second_call_*` tests (which stay).
- `handler_depth_limit_is_catchable` → actually catch: install a bottom `try` frame, pre-fill to the limit, assert the depth exception lands in the catch (today's test only asserts the boundary shape).

Additions:
- Pre-execution boundary pin: `(try (let [x] x) (catch _ e :caught))` at toplevel → **Language fault**, uncatchable (with the §2c lane change); paired with the runtime-catchable arity/type pins.
- Import lane preservation: module that (a) throws uncaught → importer boundary sees `Unhandled(:glia.exception)` with the original payload; (b) performs an unhandled `:custom` effect → `Unhandled(:custom)`, payload intact; (c) contains a non-tail recur → `Fault(Language)`, `invalid-recur` tag intact.
- Fault bypass of **both** `try` and `try-resume` (current test covers `try` only).
- Boundary-output additions per Sol (init.d-shaped peeled string, MCP data object for forwarded module errors).
- Cap sealing compile-fail doctests (§4).

## 6. Stack/WASM regression plan

- Host constrained-stack pin: a glia test spawning `std::thread::Builder::new().stack_size(2 << 20)` running the deepest known program shape (the ww/test `run-tests` + failing-assert scenario that exposed the regression, plus a deterministic ~40-level nested-`try` form). Pins evaluator depth at master's 2 MiB budget; fails loudly if a future change regresses it.
- WASM representative stress: extend an existing wasmtime-backed e2e (`tests/shell_e2e.rs` evaluates through the real wasm kernel) with one deep-nesting + throwing/catching form, exercising the new dispatch path under the wasm guest stack. No new harness.

## 7. Migration documentation plan

`CHANGELOG.md` entry for the source-breaking Rust API with an old→new table: `Val::{Recur,Effect,Resume}` removal; `Val::Cap { .. }` patterns → `Val::Cap(handle)` + accessors; `NativeFnImpl`/`AsyncNativeFnImpl`/`Dispatch::call` `Err(Val)` → `Err(NativeSignal)` (`From` keeps `Err(Val::from(..))`/`?` compiling; `NativeSignal::fault` for invariants; `forward` for nested evals); `eval_toplevel*` → `EvalError` with `payload()`/`unwrap_thrown` recipes for the standard formatter dance; `EffectTarget::Cap` sealing; removed items (`next_cap_id`, `CapHandle::id`, `thrown()`, `PartialEq` derives). Doc-comment note added at `CapId`/`CapTarget` documenting the possession-not-secrecy principle and the deliberate non-conflation with capnp connection-scoped table indexes.

## 8. Smallest patch sequence

1. Toplevel analysis failures → Language fault + the two boundary pins (§2c, §5) — smallest semantic change first, isolates the one approval-sensitive behavior.
2. Sealed capability API (§4) + external constructor migration + compile-fail doctests.
3. `NativeSignal`: remove `thrown()` (kernel helpers → `map_throw` capture); add `Forward` arm + `forward()`; caps import forwarding; import-lane tests.
4. Remove `PartialEq` derives; mechanical test-assert migration.
5. Remaining test corrections/additions (§5) + constrained-stack regression; wasm e2e stress.
6. CHANGELOG migration entry + principle comments.
7. Full re-verification (workspace + std crates tests, clippy, fmt, wasm32-wasip2).

## 9. Drift report

**REQUIRED CONSEQUENCE** — everything in patches 1–7 above: lane-preserving `forward`, sealed `CapId`/`CapTarget`/`EffectRequest`, `thrown()` removal, `PartialEq` removal + test migration, toplevel-analysis Language fault, test corrections/additions, stack/wasm regressions, migration docs (all explicitly directed).
**ADJACENT FIX — APPROVAL REQUIRED** — (a) eager operand analysis before macro resolution (§2b): fixing requires deferring operand analysis for potentially-macro heads; also entangled with the analyzed/legacy divergence for non-expression operands. (b) A proper `glia.error/analysis` tag for analysis-fault payloads (they currently carry legacy `{:type :internal}` string maps).
**DRIFT — DO NOT IMPLEMENT** — analyzer rewrite / static tail analysis; handler arity-protocol redesign (locked); special-casing `try`; new eval/read facility; random cap IDs (no boundary-crossing evidence); PR-2+ collection/float/callable/printer work; helper dedup; unrelated cleanup (std/shell's 9 pre-existing clippy warnings stay).

## 10. Decisions still requiring approval

1. **Runtime-analysis boundary** (§2c): toplevel whole-form analysis → Language fault; macro-expansion and defcap analysis (mid-evaluation, handlers installed) remain catchable exceptions. This is the minimal reading consistent with both the "pre-execution fault" rule and the "runtime analysis is catchable" carve-out, and keeps legacy/analyzed pipelines convergent. Alternative (all analysis failures fault) diverges the pipelines and makes some in-`try` failures uncatchable after handler installation.
2. **Module-boundary catchability**: `forward` deliberately does NOT re-dispatch a module's unhandled exception on the importer's handler stack (preserves today's uncatchability, restores structure). Approve, or request importer-side catchability (a semantic expansion).
3. Whether to fix the eager-operand-analysis staging bug in PR-1 or defer (recommend: defer, separate PR).
4. `EffectRequest::keyword` public constructor (needed once `target` is sealed, for the two external keyword-carrier sites) — narrowest replacement; confirm.
5. Path-context handling in import forwarding: payload preserved verbatim, resolved-path context dropped (or logged) rather than wrapped — confirm.
