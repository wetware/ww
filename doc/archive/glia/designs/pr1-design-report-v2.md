# PR-1 Design Report v2 — exceptions-as-effects, Fault lane, control extraction

Branch: `glia-control-extraction` @ f1365b6. Status: REVISED DESIGN CHECKPOINT — no source edited.
Supersedes `.context/pr1-design-report.md` §2.1–2.3 (the `Raise` lanes); §1 inventory and §2.6 cap-identity design of v1 remain valid and are incorporated by reference.

---

## 1. Current `throw`/`try` mechanics (verified)

1. **`throw` performs `:glia.exception`** — YES. `(defmacro throw [data] `(perform :glia.exception ~data))` (`prelude.glia:73-74`).
2. **`try` installs an effect handler for it** — YES. `try` expands to `(with-effect-handler :glia.exception (fn [err] ...) expr)` (`prelude.glia:106-118`); `try-catches` dispatches on `:glia.error/type`, re-performing `:glia.exception` when no clause matches (`prelude.glia:85`).
3. **Does the handler receive a live resumption?** — NO, for ordinary `try`. The generated handler is **arity-1**. `with-effect-handler` checks handler arity (`eval.rs:2253-2258`): only a 2-arity handler gets a resume fn; for 1-arity handlers the machine executes `drop(resume_tx)` **before** invoking the handler (`eval.rs:2284`).
4. **Discard or withhold?** — Structurally **withheld**: the sender is dropped before the handler runs; the handler cannot resume even maliciously.
5. **Can another handler intercept `:glia.exception` and resume?** — YES, by design: `try-resume` installs an arity-2 handler and documents `(resume value)` continuing the throwing computation (`prelude.glia:130-139`, tests around `eval.rs:7055`).
6. **Abortive by convention or structurally enforced?** — Structurally enforced for `try` (arity-1 ⇒ sender dropped); resumability is opt-in via handler arity. The *effect machinery* is uniformly resumable; abortiveness is a property of the installed handler.
7. **Unnecessary continuation capture?** — Minimal: `perform_dispatch` always allocates a oneshot pair (`eval.rs:3237`), and for 1-arity handlers the sender is dropped unused. Cost is one `Rc<Slot>` allocation per dispatched exception; no semantic capture. Not worth changing in PR-1.
8. **Pinned observable semantics (tests):** `try_ok/try_err/try_catch_string/nested_try` (`eval.rs:6489-6523`), guard catchable-by-tag (`eval.rs:6575`), try-resume resumption + one-shot enforcement (`eval.rs:6842`, `7055`, `effect.rs:229-317`), unhandled throw escapes as `glia.exception` carrier (`eval.rs:7179`, `7201`), unhandled cap effect fails closed with structured carrier (`eval.rs:7539`), handler-stack dynamics (`eval.rs:7794`, `7813`), MCP envelope peeling (`mcp_adapter.rs:379`), MCP-mode typed refusals asserted **un-peeled** (`src/cli/shell.rs:2159-2183`), shell exit as control outcome (`cli/shell.rs:2147`).

**Conclusion: the preferred surface model ("exceptions use the general effect machinery; ordinary `try` handles them abortively") is exactly what is implemented today for `throw`n exceptions. No contradiction found.** The defect is only that evaluator/builtin/native failures bypass this machinery on a parallel raw `Err(Val)` lane.

---

## 2. Complete exception / fault / control classification

Legend: "today" = current behavior. All raw-lane errors are uncatchable by `try` today; EXCEPTION-classified rows become catchable (the intended correction).

| Category | Repository evidence | Today | Class |
|---|---|---|---|
| wrong arity | `error::arity` ×56 in glia (e.g. `builtin_get` eval.rs:1656; `resume` effect.rs:171); kernel attenuate.rs:166 | raw Err | **EXCEPTION** |
| wrong operand/argument type | `error::type_mismatch` ×62 in glia (e.g. eval.rs:1668 vector index not int); kernel :541, :1994 | raw Err | **EXCEPTION** |
| calling a non-callable | eval.rs:3015 (`call head` not a symbol), eval.rs:2743 (`defcap method` not a function), invoke of non-fn handler eval.rs:2337 | raw Err | **EXCEPTION** |
| index out of bounds | `(get [1 2] 99)` → `Nil`; negative → `Nil` (eval.rs:1660-1666). No throwing index builtin exists | `Nil` | **NOT AN ERROR** (preserve) |
| malformed schema/configuration | kernel `parse_cell_spec` → `invalid_cell_spec` (:228-:343); glia `cell_error` eval.rs:404 | raw Err | **EXCEPTION** |
| missing required schema field | kernel parse_cell_spec (missing `:wasm`/`:grants`) | raw Err | **EXCEPTION** |
| unknown schema field | kernel parse_cell_spec unknown-key rejection | raw Err | **EXCEPTION** |
| failed builtin validation | division by zero eval.rs:1811/1823; NaN comparison eval.rs:1844-1850; `match` no clause eval.rs:2154; fn/let/loop shape errors eval.rs:747-1164; handler-depth limit eval.rs:2194 | raw Err | **EXCEPTION** (NaN-comparison *behavior* change is PR-2; here it just becomes catchable) |
| failed native validation | kernel executor `:spawn` caps :541, serve-vat :1218; caps import path checks (caps:318-337) | raw Err | **EXCEPTION** |
| unavailable external resource | kernel `http-client not available` :1303; ~60 RPC `.map_err(\|e\| Val::from(e.to_string()))` sites; Dispatch miss `command not found` (kernel:672, shells) | raw Err | **EXCEPTION** |
| capability-operation failure | `permission_denied` (glia eval.rs:2811; attenuate.rs:174-187, :242); tags `cap-call`/`rpc`/`epoch-expired` (constructors exist; producers in kernel RPC paths) | raw Err | **EXCEPTION** |
| invalid / non-tail `recur` | toplevel guards eval.rs:2847/2867; everywhere else the sentinel is currently *inert stored data* | raw Err / inert `#<recur>` | **FAULT** (malformed control state; preserves today's uncatchability; prevents both storage and accidental transfer) |
| abandoned continuation | oneshot Sender::Drop → `continuation_abandoned` delivered to a suspended `perform` (oneshot.rs, defensive path) | raw Err | **FAULT** (protocol violation on a computation that is being discarded; preserves uncatchability) — approval item |
| already-resumed continuation | `resume` called twice → `continuation_already_resumed` (effect.rs:179) | raw Err into live handler code | **EXCEPTION** (program-level protocol misuse; handler can recover) |
| malformed internal AST/expression state | analyze structural errors (expr.rs:368-627, `Result<_, String>`); syntax-quote misuse eval.rs:3042-3045; pattern.rs:285 destructure failure | raw Err | **EXCEPTION** (deterministic program-structure failures, not evaluator bugs) |
| impossible evaluator branch | no *firing* site today (a few `unreachable!()`); the lane exists for future invariants | panic / n/a | **FAULT** (reserved) |
| embedder/host failure | HostEffect handler `Err`: MCP `protocol_mode_unavailable` (cli:1159/1167), kernel `write_stdout` failure (:737); import-poll `Pending` invariants (caps:402/446); `invalid resume function` (kernel:816, caps:246, cli:777) | raw Err, already bypasses guest frames | **FAULT** |
| ordinary missing map key | `(get {:a 1} :b)` → `Nil` (eval.rs:1659) | `Nil` | **NOT AN ERROR** (preserve) |
| arbitrary key of unexpected type | `(get {:a 1} 3.14)` → lookup by value equality, `Nil` if absent | `Nil` | **NOT AN ERROR** (preserve) |
| fuel exhausted | tag `glia.error/fuel-exhausted` has **zero producers** — metering lives in `crates/cell` at the wasm layer | n/a | **FAULT** (reserved; metering must bypass guest handlers) |
| tail-position `recur`; effect suspension/resumption; `Resume` sentinel; unhandled-effect carrier | eval.rs loop/fn rebinds; perform/with-effect-handler machinery | Ok-channel sentinel / Err-channel sentinels | **CONTROL** |
| parse/read errors | `error::parse` ×3; cli:1134 | produced *outside* eval (read time), boundary data | EXCEPTION *payload*, raised pre-frame — effectively boundary-only; no behavior change |

Catchability is classified by intended Glia semantics, not Rust return type, per instruction. Net: the overwhelming majority of today's raw lane is EXCEPTION; FAULT is a short, enumerated list (non-tail recur, abandoned continuation, host/embedder failures, resume-protocol invariant, reserved impossible-branch/fuel).

---

## 3. Revised exact types

### 3.1 Crate-private (`crates/glia`)

```rust
/// Evaluator-internal result of one expression.
pub(crate) enum Flow {
    Value(Val),
    /// Lexical recur unwinding to the nearest loop/fn tail frame.
    Recur(Vec<Val>),
}

/// Non-value, non-recur unwinding. Exceptions NEVER travel here —
/// they are performed as the `:glia.exception` effect.
pub(crate) enum Control {
    /// Unrecoverable runtime fault; bypasses all Glia handlers.
    Fault(Fault),
    /// An effect that found no matching handler, unwinding to the boundary
    /// (includes unhandled exceptions: target `:glia.exception`).
    Unhandled(EffectRequest),
    /// Handler short-circuit from `resume`.
    Resume(Val),
}

pub(crate) type EvalResult = Result<Flow, Control>;

impl Flow {
    /// Demand a value in non-tail position. Recur here is a Fault
    /// ("recur not in tail position") — compile-enforced at every
    /// value-demanding site because Flow ≠ Val.
    pub(crate) fn into_value(self) -> Result<Val, Control>;
}

/// Raise a catchable exception from inside the evaluator: dispatch the
/// payload as the `:glia.exception` effect on the CURRENT handler stack.
/// Ok(v) = a resuming handler supplied v as the value of the failing
/// expression; Err(Unhandled) = no handler, unwinds to the boundary.
async fn throw(env: &Env, payload: Val) -> Result<Val, Control> {
    perform_dispatch(&env.handler_stack,
        EffectTarget::Keyword(error::EXCEPTION_EFFECT.into()), payload).await
}
```

Sync helpers (builtins, `analyze`, `pattern`, cell validation — ~150 raise sites) **keep their `Result<T, Val>` signatures unchanged**; the `Val` is now an *exception payload*, converted by `throw(env, payload)` at the ~10–15 async chokepoints where eval invokes them (builtin dispatch, native invocation, `Dispatch::call`, analyze/pattern call sites, special-form arms). This keeps glia-internal churn bounded and puts exception dispatch at the dynamically correct point (the failing expression).

### 3.2 Public boundary types

```rust
/// Unrecoverable runtime failure. Not a Val; cannot be caught or observed
/// by Glia code. Payload is a structured error map (same schema as
/// exceptions) so boundary formatting is preserved.
#[derive(Clone, Debug)]
pub struct Fault(Val);            // field private
impl Fault {
    pub fn payload(&self) -> &Val;
}
impl fmt::Display for Fault {     // == today's bare-Err display of the map

/// An effect and its payload as carried to the boundary.
#[derive(Clone, Debug)]
pub struct EffectRequest { pub target: EffectTarget, pub data: Val }
impl EffectRequest {
    /// Legacy tag: the keyword, or "cap:{name}".
    pub fn effect_type(&self) -> String;
}

/// How a top-level evaluation failed, as seen by embedders.
/// ONE escaped-effect arm — no special exception error type.
#[derive(Clone, Debug)]
pub enum EvalError {
    Fault(Fault),
    Unhandled(EffectRequest),
}
impl EvalError {
    /// Successor of `error::unwrap_thrown`: the thrown payload iff this is
    /// an unhandled `:glia.exception`.
    pub fn thrown(&self) -> Option<&Val>;
    /// The structured payload for message/type_tag inspection:
    /// thrown data, or fault payload. None for other escaped effects.
    pub fn payload(&self) -> Option<&Val>;
}
impl fmt::Display for EvalError {
    // Fault(f)                        → "{f.payload()}"        (== today)
    // Unhandled(:glia.exception, d)   → "{d}"                  (peeled)
    // Unhandled(other)                → "#<effect :{ty} {data}>" (== today)
}
// error::unwrap_thrown(err: &EvalError) -> Option<&Val> kept as a shim.
```

Terminology discipline (§6 of the prompt): **exception** = a Glia error value travelling the `:glia.exception` effect; **escaped effect** = `EvalError::Unhandled(EffectRequest)`; **fault** = `Fault`. The bare word `Error` appears only in `EvalError`, the boundary sum of the latter two.

Public entry points: `eval_toplevel`/`eval_toplevel_expr` → `Result<Val, EvalError>`; `eval_toplevel_with_host_effects` → `Result<EvalOutcome, EvalError>`. `eval`/`eval_expr` → `pub(crate)` (zero external callers, verified).

### 3.3 Exception payload (§6)

Existing `GliaError` → `Val::Map` (`:glia.error/*` schema) serves **directly** as the exception payload — no wrapper/tag. Rationale: `try`'s dispatch already keys on `:glia.error/type` (prelude:114-116); `ex-info` user errors already flow through `GliaError::User`; all tags, data, messages, and boundary formatting preserved verbatim. A wrapper would break `(catch :glia.error/arity-mismatch ...)` for free wins we don't need.

### 3.4 Answers to the §3 prompt questions

- **Builtin wrong-type error:** returns `Err(error::type_mismatch(...))` exactly as today; the invoking chokepoint runs `throw(env, payload)` → catchable.
- **Native validation error:** returns `Err(NativeSignal::throw(...))` (or `Err(val.into())` — same thing); invocation chokepoint dispatches identically.
- **Evaluator invariant fault:** `return Err(Control::Fault(Fault::new(error::internal(...))))` in-crate (helper macro `fault!(ctx, msg)`); bypasses `perform_dispatch` entirely.
- **Unhandled exception → embedder:** `throw` finds no frame → `Err(Control::Unhandled(req{target: :glia.exception}))` → `EvalError::Unhandled` → `thrown()` peels.
- **Unhandled non-exception effect → embedder:** same arm, different target; Display preserves `#<effect ...>`.
- **One escaped-effect type at the boundary:** YES — `EvalError::Unhandled(EffectRequest)` covers both; no exception-specific error variant exists.

---

## 4. Native and Dispatch API (revised)

```rust
/// Opaque signal from native code / Dispatch. Constructed only via
/// semantic constructors; no public pattern matching.
pub struct NativeSignal(pub(crate) NativeSignalKind);
pub(crate) enum NativeSignalKind { Throw(Val), Resume(Val), Fault(Fault) }

impl NativeSignal {
    /// Recoverable exception — dispatched through `:glia.exception`,
    /// catchable by `try`. The default for ordinary native failures.
    pub fn throw(err: impl Into<Val>) -> Self;
    /// Trusted-runtime invariant violation — bypasses all Glia handlers.
    pub fn fault(err: impl Into<Val>) -> Self;
    // NO public resume(); pub(crate) only (make_resume_fn).
    // NO recur — unrepresentable.
}
impl From<Val> for NativeSignal    { /* throw */ }
impl From<String> for NativeSignal { /* throw(Val::from(s)) */ }
impl From<&str>  for NativeSignal  { /* ditto */ }
impl From<GliaError> for NativeSignal { /* throw(e.into()) */ }

pub type NativeFnImpl =
    Rc<dyn Fn(&[Val]) -> Result<Val, NativeSignal>>;
pub type AsyncNativeFnImpl =
    Rc<dyn Fn(Vec<Val>) -> Pin<Box<dyn Future<Output = Result<Val, NativeSignal>>>>>;

// Dispatch::call — same signal type, same conversion chokepoint:
fn call<'a>(&'a self, name: &'a str, args: &'a [Val])
    -> Pin<Box<dyn Future<Output = Result<Val, NativeSignal>> + 'a>>;
```

Chokepoint conversion (uniform for builtins/natives/Dispatch): `Ok(v)` → `Flow::Value(v)`; `Err(Throw(v))` → `throw(env, v).await`; `Err(Resume(v))` → `Err(Control::Resume(v))`; `Err(Fault(f))` → `Err(Control::Fault(f))`.

Answers:
1. **Ordinary native failures → exception effects?** YES (via `throw`/`From` default).
2. **Should native code construct `Fault` directly?** Yes, narrowly — kernel needs it for protocol invariants (`invalid resume function`) and host-write failures. Note `fault` is a *de-escalation*, not an authority grant: it strictly reduces what guest code can observe/recover; a hostile native gains nothing over `throw`.
3. **Restrict faults to trusted APIs?** Rust module privacy cannot distinguish kernel from other embedders; the restriction is the constructor's documented contract plus the audit (§2) enumerating legitimate sites. Acceptable given (2).
4. **Does native code need to construct `Resume`?** NO. Natives only *propagate* it: the resume fn they call returns `Err(NativeSignal(Resume))`, which they pass through opaquely (existing `call_resume` helpers keep working verbatim). The constructor stays `pub(crate)`.
5. **`Dispatch::call`:** identical signal type; all five impls only ever throw (verified §1.3 of v1), so migration is `From`-mechanical. `reify_attenuation`/`validate_cell_grant` stay `Result<_, Val>` (synchronous validation; payload = exception).
6. **~95+ `Err(Val::from(...))` sites:** the `From` impls make them compile unchanged with **throw** as the audited default (they are validation/RPC/resource failures = EXCEPTION per §2). The FAULT exceptions to the default are explicitly enumerated and opt in via `NativeSignal::fault`: kernel `call_resume` invalid-resume (:816), caps `call_resume` (:246), cli `call_resume_local` (:777), caps import-poll `Pending` invariants (:402/:446), kernel `write_stdout` failure path (:737 — via HostEffect Err, see below). Nothing is blindly reclassified.

`HostEffectFuture` keeps `Result<HostEffectResult, Val>`; its `Err` converts to `Control::Fault` at the host-effects poll loop (embedder/host failure class — preserves today's bypass-guest-handlers behavior and the un-peeled MCP test assertions via `Fault::payload`).

---

## 5. `recur`: `Flow::Recur` vs private abortive effect

| Criterion | A: `Flow::Recur` | B: evaluator-private effect |
|---|---|---|
| Tail-position enforcement | `into_value()` at every non-tail site; forgetting one is a **Rust compile error** | requires analyzer tail checking, else non-tail recur *dispatches to the nearest loop* — becomes active control transfer (the exact forbidden outcome) |
| Accidental user interception | impossible (crate-private enum) | preventable with unforgeable target, but handler-stack walk still occurs |
| Continuation capture | none | oneshot allocation + suspension per `recur` |
| Performance | zero-cost enum on the hot loop path | handler frame push/pop per loop iteration + async dispatch per recur |
| Complexity | small; rebind loops match one arm | reuses effect machinery but entangles loops with the handler stack |
| Analyzer/evaluator split | runtime enforcement now; static tail analysis can be layered later without API change | forces analysis work into PR-1 to be sound |
| Rust exhaustiveness | full (`match Flow`) | none (runtime target matching) |
| Non-tail recur becomes active transfer? | structurally impossible | yes, unless statically prevented |
| Future unified control-effect model | crate-private; freely re-implementable later | commits loops to the effect system now |

**Recommendation: A (`Flow::Recur`), exactly matching the stated bias** — semantically documented as evaluator-private abortive control, implemented as specialized flow. Repository evidence supports it: loops are the evaluator's hot path, and B's soundness precondition (static tail-position analysis) is real analyzer work that PR-1 shouldn't absorb. Non-tail recur maps to **Fault** (§2), preserving today's uncatchable "recur not in tail position" while eliminating both sentinel storage and accidental transfer.

---

## 6. Behavioral-preservation argument

Preserved byte-for-byte:
- `throw`/`try`/`try-resume`/`or-else`/`guard` semantics — machinery untouched.
- Boundary strings for **all** current error paths: previously-raw errors arrive as unhandled exceptions; the four peel-formatters (`std/shell:229`, `cli:1112`, `kernel:1898`, `mcp_adapter:150/170`) produce identical `[tag] msg` output via `payload()`; fault payloads display as today's bare maps; non-exception escaped effects keep `#<effect :{ty} {data}>`.
- MCP JSON `data` objects; MCP-mode refusals still un-peeled-inspectable (`Fault::payload`).
- One-shot resume enforcement; unhandled-cap-effect fail-closed carrier; `EvalOutcome::Exit`; map/vector `get` → `Nil` semantics.

Deliberate, intended changes (the point of the correction):
1. EXCEPTION-classified evaluator/builtin/native failures become **catchable by `try`** (previously uncatchable).
2. The same failures become **resumable under `try-resume`** — a required consequence of unification (an exception *is* an effect), conspicuously flagged: this extends resumability beyond `throw`n errors. Alternative (withhold the continuation for internal raises) is possible but creates two observably different `:glia.exception` dispatches. **Approval required.**
3. Non-tail `recur` → Fault at every non-tail site (today: inert `#<recur>` debris in the value space; toplevel-only error).
4. `(type ...)` can no longer return `:recur`/`:effect`/`:resume`; the three variants leave `Val`.
5. One formatter delta: kernel **init.d** logs `format!("{e}")` without peeling (`kernel:1830`). Unifying the lanes forces one of its two current strings to change; recommended `EvalError` Display peels exceptions, so previously-raw errors keep their string and previously-`throw`n unhandled errors change from `#<effect :glia.exception {err}>` to `{err}` in init.d logs only. **Approval required** (or init.d adopts the standard peel formatter — adjacent fix).
6. Rust API surface (variants removed, signal types, `EvalError`, cap encapsulation per v1 §2.6).

---

## 7. Migration scope (by crate, approximate call sites)

| Crate/file | Scope | Est. sites |
|---|---|---|
| `crates/glia` | `Val` variant removal + arm deletion (~15 arms); `Flow`/`Control`/`throw` plumbing through `eval.rs` internals; ~10–15 chokepoint conversions; 2 toplevel guards; `effect.rs` resume fn + oneshot Err type; `error.rs` unwrap_thrown shim + `EvalError`; `expr.rs`/`pattern.rs` unchanged signatures (payload lane); cap `CapId`/`CapHandle` (v1 §2.6). **~150 raise sites unchanged in place.** | ~80 non-test edits + **150–250 test assertions** (largest single cost) |
| `std/kernel` | `NativeFnImpl` closures (~15), `Dispatch` impl, `call_resume` → fault opt-in, formatter :1898, `Val::Cap` accessor migration ×7, host-effects unchanged | 86 `Err(` sites mostly `From`-silent; ~25 real edits + tests |
| `std/kernel/attenuate.rs` | `reify` signature intact; gated-handler Err type; cap accessors ×2 | ~8 |
| `std/caps` | 2 NoopDispatch, `call_resume`, 2 fault opt-ins, handler closures | 34 `Err(` mostly silent; ~15 real edits |
| `src/cli/shell.rs` | 2 handler factories, `call_resume_local`, 2 formatters, MCP arms | 37 `Err(` mostly silent; ~15 real edits |
| `std/shell` | 2 Dispatch impls, formatter, eval-arm match | ~10 |
| `std/caps/mcp_adapter.rs` | `val_to_mcp_error_text/data` re-typed to `&EvalError` (or thin adapters) | ~4 |

## 8. Recommended sequencing: **Option A** (bounded)

The exception normalization adds only the chokepoint `throw` dispatch on top of work Option B does anyway (the Err-type change to every native/Dispatch signature is the bulk, and it happens in both options). Option B would freeze `Raise` into `NativeSignal`/`EvalError` — a public API we already believe is wrong — then delete it in the next PR and migrate all four embedder crates' formatters and the full test surface **twice**. Option A's boundary: no effect-machinery changes, no analyzer work, no collections/printer/callable work, FAULT list short and enumerated. If review finds it too large, the only coherent split that avoids freezing a disposable API is: PR-1a = glia-internal (`Flow`/`Control`/`throw` + `Val` variant removal + boundary types), PR-1b = embedder migration + cap encapsulation — but they cannot compile independently across the four packages without shims, so A as one reviewable PR remains the recommendation.

## 9. Required tests

1. `(get {:a 1} :b)` → `nil`; `(get [1] 5)` → `nil` — not exceptions (regression pins).
2. `(try (foo 1 2 3) (catch :glia.error/arity-mismatch e :caught))` → `:caught`.
3. `(try (+ 1 "a") (catch :glia.error/type-mismatch e :caught))` → `:caught`.
4. Kernel: `(try (host :listen {bad-spec}) (catch :glia.error/invalid-cell-spec e ...))` catches; native string-error catchable via wildcard.
5. `(try (throw (ex-info "x" {:type :t})) (catch :t e e))` — unchanged.
6. Abortiveness: side-effect after a throwing expression inside `try` body does not run; `try` handler has no resume (structural: arity-1).
7. `(with-effect-handler :e (fn [d r] (r 42)) (perform :e nil))` → `42` — ordinary effects resumable.
8. Fault bypass: non-tail recur inside `(try ... (catch _ e :caught))` is NOT caught; reaches embedder as `EvalError::Fault` with "recur not in tail position".
9. `(loop [] [(recur)])` → Fault (not `[#<recur>]`, not an infinite loop); `(f (recur))` → Fault.
10. `(loop [x 0] (if (< x 3) (recur (+ x 1)) x))` → `3`; fn-recur and variadic-recur arity checks unchanged.
11. Boundary: unhandled `throw` → embedder prints `[tag] msg` (all four formatters); unhandled `(perform :net {...})` → `#<effect :net {...}>`; MCP `data` JSON unchanged; MCP-mode `:stdout`/`:exit` refusals keep tag `glia.error/protocol-mode-unavailable` via `Fault::payload`.
12. One-shot: second `(resume ...)` → `:glia.error/continuation-already-resumed`, now catchable inside the handler (`try` around the second resume call).
13. try-resume over a builtin error (`(try-resume (fn [e r] (r 0)) (+ 1 "a"))` → `0`) — pins decision §11.2 if approved, or pins the withholding behavior if not.
14. Cap identity: `EffectTarget` forging impossible (compile-fail doc-test or API review); attenuated cap still escapes ambient parent handler (fresh id).

## 10. Drift report

**REQUIRED CONSEQUENCE** — control extraction (`Flow`/`Control`); exceptions-as-effects chokepoint dispatch; `Fault` lane + enumerated fault sites; `NativeSignal` + `Dispatch::call` migration; `EvalError`/`EffectRequest` boundary; resume-fn signal change; boundary formatter migration in the four packages; test migration; cap `CapId`/`CapHandle` encapsulation (authorized scope §D); builtin-error resumability *as machinery consequence* (flagged, §11.2).
**ADJACENT FIX — APPROVAL REQUIRED** — dedupe the three `call_resume` copies into `glia::effect::call_resume`; dedupe the three identical host-effects blocks; init.d adopting the standard peel formatter; retagging miscategorized `glia.error/internal` uses (match-no-clause, division-by-zero → better tags); `std/caps` adopting structured errors over plain strings.
**DRIFT — DO NOT IMPLEMENT** — generalized effect-system rewrite (machinery untouched); *new* resumable-exception affordances beyond what the existing machinery yields (new syntax, restartable conditions, resumption policies); callable changes (PR-4); persistent collections (PR-2); printer/reader (PR-3); error-message wording cleanup; map-lookup semantics changes (`get` stays `Nil`); NaN-comparison behavior change (PR-2 contract item); static tail-position analysis in the analyzer; Python-style compatibility anything.

## 11. Decisions requiring approval

1. **Option A** (exception normalization inside PR-1) — recommended.
2. **Resumability of evaluator/builtin/native exceptions under `try-resume`** — recommended ALLOW (uniform semantics; well-defined: resuming supplies the failing expression's value). Alternative: withhold continuation for internal raises (two observably different exception dispatches).
3. **Non-tail `recur` = FAULT** (uncatchable) — recommended; preserves today's uncatchability, matches "malformed control state".
4. **Abandoned continuation = FAULT** vs EXCEPTION — recommended FAULT (defensive path, preserves uncatchability).
5. **`NativeSignal::fault` public** (documented trusted-runtime contract; de-escalation argument) — recommended yes.
6. **`EvalError` Display peels exceptions** → single string delta at kernel init.d logs for previously-thrown unhandled errors (§6.5) — recommended yes.
7. Handler-depth-limit and division-by-zero become catchable EXCEPTIONs (tag cleanup deferred) — recommended yes (falls out of classification; called out for visibility).
