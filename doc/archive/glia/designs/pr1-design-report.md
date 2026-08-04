# PR-1 Design Report — Glia control-state extraction + cap identity encapsulation

Branch: `glia-control-extraction` @ f1365b6 (origin/master). Status: DESIGN CHECKPOINT — no source edited.
Scope authority: `doc/designs/value-contract.md` §2 (Removed from `Val`, PR-1 row of §11).

---

## 1. Usage inventory (repository-grounded)

### 1.1 The three control variants in `Val` (`crates/glia/src/lib.rs:215`)

| Variant | Definition | Propagation channel |
|---|---|---|
| `Val::Recur(Vec<Val>)` | lib.rs:248 | **Ok channel** — returned as a *value* from body eval; this is exactly how it leaks into collections (`(loop [] [(recur)])` builds `Vector([Recur])`). |
| `Val::Effect { effect_type: String, data: Box<Val> }` | lib.rs:259 | **Err channel** — the unhandled-effect carrier. |
| `Val::Resume(Box<Val>)` | lib.rs:277 | **Err channel** — handler short-circuit sentinel. |

#### Producers (all inside `crates/glia`)

- `Val::Recur`: `eval.rs:1235` (`eval_recur`, legacy Val path), `eval.rs:2051` (`Expr::Recur`). Consumed by the rebind loops in `invoke_fn` (`eval.rs:952`) and `Expr::Loop` (`eval.rs:2010`); stray sentinels converted to `error::internal("recur", "not in tail position")` only at the two toplevel guards (`eval.rs:2847`, `eval.rs:2867`).
- `Val::Effect`: exactly two production sites — `perform_dispatch` no-match arm (`eval.rs:3255`, `effect_type` = keyword string or `"cap:{name}"`) and `perform_cap_value` exhausted-inner arm (`eval.rs:2831`). One test-only construction outside glia: `std/caps/src/mcp_adapter.rs:381`.
- `Val::Resume`: exactly one production site — the resume `NativeFn` closure in `effect::make_resume_fn` (`effect.rs:185`). Kernel test stub reproduces it (`std/kernel/src/lib.rs:3005`).

#### Consumers

- `with-effect-handler` state machine matches `Err(Val::Resume(_))` at `eval.rs:2308` (sync-native handler arm) and `eval.rs:2358` (fn/async handler arm) to flip back to polling the suspended body.
- `perform_cap_value` retries on `Err(Val::Effect { effect_type == format!("cap:{name}") })` (`eval.rs:2780`) to fall through from stack interposition to the cap's intrinsic handler.
- `error::unwrap_thrown` (`error.rs:537`) peels the `glia.exception` carrier. Callers: `std/shell/src/lib.rs:229`, `src/cli/shell.rs:1112`, `std/kernel/src/lib.rs:1898` (+test :3897), `std/caps/src/mcp_adapter.rs:150,170`. **All four production sites use the identical `unwrap_thrown(&e).unwrap_or(&e)` display dance**; they differ only in the final format string (`"[{tag}] {msg}"` in both shells, `"error: [{tag}] {msg}\n"` in kernel, hint-suffixed in mcp_adapter).
- **No embedder has a dedicated `Val::Effect` match arm.** Unhandled non-exception effects reach users via `Display` fallback as literally `#<effect :{type} {data}>` (lib.rs:495). This string must be preserved.
- Value-space arms that drop with the variants: `PartialEq`/`Hash` sentinel arms (lib.rs:404-406, 448), `Display` (`#<recur>` :488, `#<effect ...>` :495, `#<resume ...>` :501), `Debug` (:316-325), `type` builtin (`eval.rs:1404,1406,1410` — today exposes `:recur`/`:effect`/`:resume`), `error::val_type_name` (error.rs:569-574), `is_authority_free` (eval.rs:384), free-var walk + `analyze` Const arms (`expr.rs:163-169, 352-357`).
- `valmap.rs`, `pattern.rs`, `oneshot.rs`: zero control references (no tripwires exist yet; §8 of the contract).

### 1.2 `Result<Val, Val>` control flows

- The universal evaluator signature (`eval`, `eval_expr`, `eval_toplevel*`, `invoke_fn`, builtins). The `Err` lane multiplexes three meanings: structured error map, `Val::Effect` carrier, `Val::Resume` sentinel.
- **Load-bearing semantic fact:** plain-`Err` errors (evaluator, builtin, native, `Dispatch::call`) are **not catchable by `(try ...)`**; only `(throw ...)`-performed errors are (throw/try are prelude macros over `perform :glia.exception` / `with-effect-handler`, `prelude.glia:53-137`). The two error lanes must remain distinct after extraction.
- `oneshot::Receiver: Future<Output = Result<Val, Val>>` — `Err` = `continuation_abandoned()` when the sender drops.
- `effect::HostEffectFuture = Pin<Box<dyn Future<Output = Result<HostEffectResult, Val>>>>` — `Err` is a pure error (e.g. `protocol_mode_unavailable` in MCP mode, asserted un-peeled at `src/cli/shell.rs:2172`).
- Cap effect handlers in kernel/caps/cli **return `Err(Val::Resume(..)) on success** via `call_resume` (`std/kernel/src/lib.rs:813`, `std/caps/src/lib.rs:243`, duplicate `call_resume_local` at `src/cli/shell.rs:774`) — the central control inversion outside glia.

### 1.3 `Dispatch::call` (`eval.rs:250`)

Five impls outside glia; **all** return only plain error `Val`s from `call`, never control:

| Impl | Site | Notes |
|---|---|---|
| `KernelDispatch` | `std/kernel/src/lib.rs:666` | Only impl overriding `reify_attenuation` (→ `attenuate::reify`), `validate_cell_grant` (destructures `Val::Cap{inner}`, requires capnp client), `report_warning`. |
| `ShellDispatch` | `std/shell/src/lib.rs:69` | table lookup, miss → string error. |
| `NoopDispatch` | `std/shell/src/lib.rs:325` | prelude-load stub. |
| `LocalShellDispatch` | `src/cli/shell.rs:159` | table lookup. |
| 2× `NoopDispatch` | `std/caps/src/lib.rs:374, :411` | import-time stubs (byte-identical duplicates). |

`crates/rpc/src/dispatch.rs` and `src/dispatcher/server.rs` are **false positives** — HTTP/capnp dispatch, zero glia coupling.

### 1.4 `NativeFnImpl` / `AsyncNativeFnImpl` (`lib.rs:204, :209`)

Producer shapes outside glia:
- **Cap effect handlers** (arity-2 `(data, resume)`, success = `call_resume` → `Err(Val::Resume)`): kernel `make_authority_handler`:964, `make_host_handler`:1018, `make_runtime_handler`:1324, `make_routing_handler`:1512; attenuate `make_gated_handler`:151 (forwards resume opaquely; `map_membrane_denial`:200 passes `Val::Resume` through unchanged); caps `make_import_handler`:302, `make_host_handler`:477, `make_routing_handler`:570; cli `make_host_handler_local`:781, `make_routing_handler_local`:876.
- **Method tables / builtins** (plain `Result`, no resume): kernel process/executor methods (:366-:455), `make_schema_builtin`:1999, `make_doc_builtin`:2026, `make_help_builtin`:2074.
- Error mix: kernel ~72 `glia::error::*` vs ~95 `Val::from(String)`; `std/caps` is 100% plain strings. Both must keep compiling cheaply after the Err-type change.
- In-glia natives: `make_resume_fn` (effect.rs:165), identity resume in `perform_cap_value` (eval.rs:2791).

### 1.5 Effects, suspension, resumption, boundaries

- Suspension: `perform_dispatch` (eval.rs:3210) writes `(target, data, oneshot::Sender)` into the matching `HandlerContext.slot` and awaits the `Receiver`; the `with-effect-handler` poll loop (eval.rs:2233-2371) and the host-effects poll loop (eval.rs:2946-2985) service the slots.
- Host boundary: `HostEffect`/`HostEffectResult{Resume,Exit}`/`HostEffectHandler` (effect.rs:126-148) — already a correct non-`Val` boundary type; `EvalOutcome{Value,Exit}` (eval.rs:2875). Three near-identical `[load, stdout, exit]` keyword frames: `std/shell:126`, `src/cli/shell.rs:1171`, `std/kernel:745`.
- Shell/MCP boundaries consume `Result<EvalOutcome, Val>` (`std/shell:205`, `src/cli/shell.rs:1189→1106/486`, `std/kernel:759→1815/1887`). Kernel init.d logs `Err` with plain `{e}` Display, no peel (`kernel:1830`).

### 1.6 Capability identity surface

- `Val::Cap { name, schema_cid, cap_id: u64, inner: Rc<dyn Any> }` — public variant fields; `make_cap` (lib.rs:100) is the intended mint, but nothing prevents literal construction with a chosen `cap_id` (the forgery hole). `next_cap_id` (lib.rs:53) is `pub`.
- `EffectTarget::Cap { name, schema_cid, cap_id: u64 }` (effect.rs:63) — `matches()` compares caps **by `cap_id` only** (effect.rs:77). Constructed only inside glia (eval.rs:2176 `with-effect-handler`, :2767 `perform_cap_value`); external code constructs only `EffectTarget::Keyword`.
- **No code outside `crates/glia` reads or writes `cap_id`** (single comment mention, kernel:4311). External destructures read only `name`, `schema_cid`, `inner`:
  - kernel: `collect_forwardable_caps`:227, spawn-caps:541, `validate_cell_grant`:689, authority-guard:974, serve-vat:1218, serve-raw-vat:1262, `unwrap_cap_arg`:1988 (the only site reading name+schema_cid together) — all downcast `inner` to capnp clients / `GliaCapInner` / `AttenuatedCapInner` / `HandledCapInner` / `MembranedCap`.
  - attenuate: `reify`:213 destructure; `membraned_cap_of`:47 double-downcast; re-mint via `make_cap`:301 with **fresh id** (this freshness is what makes attenuated caps escape ambient parent handlers — must be preserved).
  - caps/cli/std-shell: construction only (`make_cap` at caps:268, std-shell:311, cli:682/686), no destructuring.
- `next_cap_id` is never called outside glia. All external `Val::Cap` production goes through `make_cap`.

---

## 2. Proposed exact types

### 2.1 Crate-private evaluator control (`crates/glia`)

```rust
/// Why evaluation did not produce a value. Never representable as a Val.
pub(crate) enum Control {
    /// Raw-lane structured error (evaluator/builtin/native/Dispatch).
    /// NOT catchable by `try` — preserves today's two-lane semantics.
    Raise(Val),
    /// An effect found no matching handler frame; unwinding to the boundary.
    Unhandled(EffectRequest),
    /// A handler body short-circuited via `resume`.
    Resume(Val),
}
```

**Recur is deliberately NOT in `Control`.** Two shapes were compared:

- **(A) `Control::Recur(Vec<Val>)`** (the sketch in the prompt): recur unwinds the Err lane until the nearest `loop`/fn frame catches it. Failure case: today's inert-garbage programs become *live jumps* — `(loop [] [(recur)])` currently returns `[#<recur>]`; under (A) the recur raised inside the vector literal propagates through `?` to the loop frame and **silently becomes an infinite loop**. Non-tail recur would work "by accident" everywhere, which is neither today's behavior nor Clojure's.
- **(B) `Result<Flow, Control>`** with a crate-private value channel:

```rust
/// Evaluator-internal result of one expression: a value, or a lexical
/// recur travelling (only) to the nearest loop/fn tail frame.
pub(crate) enum Flow {
    Value(Val),
    Recur(Vec<Val>),
}
pub(crate) type EvalResult = Result<Flow, Control>;

impl Flow {
    /// Demand a value in a non-tail position; a Recur here is the
    /// "recur not in tail position" structured error (today's toplevel
    /// guard, applied uniformly at every value-demanding site).
    pub(crate) fn into_value(self) -> Result<Val, Control>;
}
```

**Recommendation: (B).** Tail sites (loop body, fn body, do/let/if/match tails, `with-effect-handler` body) propagate `Flow`; every other site calls `into_value()` — and forgetting one is a **compile error** (`Flow` ≠ `Val`), whereas under (A) a forgotten interception silently changes semantics. Deliberate observable change either way: recur in non-tail position becomes a structured `:glia.error/internal` "recur not in tail position" error instead of an inert `#<recur>` value. (B) makes it exactly that error at every site; (A) cannot.

### 2.2 Public/native signaling (narrowest surface)

```rust
// crates/glia/src/lib.rs
/// The only things a native function or embedder can signal instead of
/// returning a value: raise a structured error, or propagate a resumption.
/// Lexical recur and effect synthesis are deliberately absent.
#[derive(Clone, Debug)]
pub enum Signal {
    Raise(Val),
    Resume(Val),
}
impl From<Val> for Signal      { /* Raise */ }   // keeps `?` on error-helper returns working
impl From<String> for Signal   { /* Raise(Val::from(s)) */ }
impl From<&str> for Signal     { /* ditto */ }
impl From<GliaError> for Signal{ /* Raise(e.into()) */ }

pub type NativeFnImpl = Rc<dyn Fn(&[Val]) -> Result<Val, Signal>>;
pub type AsyncNativeFnImpl =
    Rc<dyn Fn(Vec<Val>) -> Pin<Box<dyn Future<Output = Result<Val, Signal>>>>>;
```

- `Signal::Resume` is required (not merely convenient): every kernel/caps/cli cap handler terminates in `call_resume`, which must propagate the resume continuation's signal through the handler's own return. Repository evidence (§1.4) shows natives need exactly Raise + Resume, nothing more. Recur stays unsynthesizable by construction.
- `Dispatch::call` migrates to the same type: `-> Pin<Box<dyn Future<Output = Result<Val, Signal>> + 'a>>`. All five impls only ever `Raise` (§1.3), so migration is mechanical (`Err(Val::from(..))` compiles unchanged via `From`).
- `reify_attenuation` and `validate_cell_grant` stay on `Result<_, Val>`: they are synchronous validation hooks with no resumption path (evidence: kernel-only overrides, errors only).
- `effect::HostEffectFuture` keeps `Result<HostEffectResult, Val>` — its Err is a pure error today (MCP-mode refusals are asserted un-peeled, `src/cli/shell.rs:2172`); no change needed.

### 2.3 Effect suspension + boundary reporting

```rust
// crates/glia/src/effect.rs
/// An effect and its payload, as carried to a handler or the boundary.
#[derive(Clone, Debug)]
pub struct EffectRequest {
    pub target: EffectTarget,
    pub data: Val,
}
impl EffectRequest {
    /// Legacy wire/display tag: the keyword, or "cap:{name}".
    pub fn effect_type(&self) -> String;
}

// crates/glia/src/eval.rs — the embedder boundary type
/// How a top-level evaluation failed, as seen by embedders.
#[derive(Clone, Debug)]
pub enum EvalError {
    /// Raw-lane structured error (not catchable by `try`).
    Raise(Val),
    /// Unhandled effect, including an unhandled `throw`
    /// (target = `:glia.exception`, data = the thrown error).
    Unhandled(EffectRequest),
}
impl fmt::Display for EvalError {
    // Raise(v)      → "{v}"                      (byte-identical to today)
    // Unhandled(r)  → "#<effect :{type} {data}>" (byte-identical to lib.rs:495)
}
impl EvalError {
    /// The payload embedders inspect with `error::message`/`type_tag`:
    /// thrown error data, or the raised error value; None for
    /// non-exception unhandled effects (display falls back to `{self}`).
    pub fn payload(&self) -> Option<&Val>;
}

// crates/glia/src/error.rs — migrated in place, same name
pub fn unwrap_thrown(err: &EvalError) -> Option<&Val>;  // peels :glia.exception
```

Public entry-point signatures become:

```rust
pub fn eval_toplevel(..)                 -> .. Result<Val, EvalError> ..;
pub fn eval_toplevel_expr(..)            -> .. Result<Val, EvalError> ..;
pub fn eval_toplevel_with_host_effects(..) -> .. Result<EvalOutcome, EvalError> ..;
```

`eval` / `eval_expr` have **zero external callers** (verified) → demote to `pub(crate)` so `Flow`/`Control` never leak into the public surface.

### 2.4 User-thrown error payloads — unchanged

`GliaError` / the `:glia.error/...` map schema / `error::user` stay exactly as-is. A thrown error remains an ordinary `Val` performed at `:glia.exception`; only the *carrier* at the boundary changes from `Val::Effect` to `EvalError::Unhandled(EffectRequest)`.

### 2.5 Lexical recur — `Flow::Recur(Vec<Val>)` (crate-private, §2.1)

Caught only by `Expr::Loop` and `invoke_fn` rebind loops; toplevel guards keep producing the same error for a recur that reaches them (now impossible except via a bare `(recur ...)` at toplevel).

### 2.6 Opaque capability identity

```rust
// crates/glia/src/lib.rs
/// Opaque capability instance identity. Minted only inside make_cap;
/// no public constructor, no access to the raw counter.
#[derive(Clone, PartialEq, Eq, Hash, Debug)]
pub struct CapId(u64);          // field private; next_cap_id() becomes private

/// Payload of `Val::Cap`. Fields private; `make_cap` is the only mint.
#[derive(Clone)]
pub struct CapHandle {
    name: String,
    schema_cid: String,
    id: CapId,
    inner: Rc<dyn Any>,
}
impl CapHandle {
    pub fn name(&self) -> &str;
    pub fn schema_cid(&self) -> &str;
    pub fn id(&self) -> &CapId;
    /// Read-only access for the downcast/export operations existing code
    /// performs (capnp clients, GliaCapInner, AttenuatedCapInner,
    /// HandledCapInner→MembranedCap). Reading confers no minting power.
    pub fn inner(&self) -> &Rc<dyn Any>;
    /// The controlled path into effect targeting.
    pub fn effect_target(&self) -> EffectTarget;
}

// Val::Cap(CapHandle)   — single-field variant; matching stays possible,
//                          construction requires make_cap.

pub fn make_cap(name: impl Into<String>, schema_cid: impl Into<String>,
                inner: Rc<dyn Any>) -> Val;   // unchanged signature; mints fresh CapId
```

```rust
// crates/glia/src/effect.rs
pub enum EffectTarget {
    Keyword(String),                          // freely constructible (host frames)
    Cap { name: String, schema_cid: String, id: CapId },
}
```

Covering the **whole identity path**: forging `EffectTarget::Cap` now requires a `CapId`, and a `CapId` is obtainable only from a `CapHandle` you already hold (`h.id().clone()` / `h.effect_target()`) — equivalent authority to holding the cap, which is the capability model's intent. `matches()` compares `CapId` (derived `PartialEq`). Attenuation freshness (attenuate.rs:301) is preserved automatically since `make_cap` always mints. If reviewers want belt-and-braces, the `Cap` variant can carry a private-field `CapTarget` struct instead; the evidence (zero external constructions) says the simpler shape suffices.

Kernel accessor migration is mechanical: `let Val::Cap { inner, .. } = v` → `let Val::Cap(h) = v; let inner = h.inner();`; `unwrap_cap_arg` returns `(h.name(), h.schema_cid(), h.inner())`.

---

## 3. Migration map (producers/consumers → new form)

| Site | Today | After |
|---|---|---|
| `eval_recur` / `Expr::Recur` | `Ok(Val::Recur(v))` | `Ok(Flow::Recur(v))` |
| loop/fn rebind loops | match `Ok(Val::Recur)` | match `Ok(Flow::Recur)` |
| every non-tail eval site | collects sentinel as value | `.into_value()?` (compile-enforced) |
| `perform_dispatch` no-match | `Err(Val::Effect{..})` | `Err(Control::Unhandled(req))` |
| `perform_cap_value` retry match | string compare `"cap:{name}"` | `req.target` CapId == current cap id (strictly equivalent: the request was just built from this cap) |
| `make_resume_fn` closure | `Err(Val::Resume(v))` | `Err(Signal::Resume(v))` |
| with-effect-handler machine ×2 | `Err(Val::Resume(_))` | `Err(Control::Resume(_))` |
| oneshot `Receiver::Output` | `Result<Val, Val>` | `Result<Val, Control>` (abandon → `Raise(continuation_abandoned())`) |
| natives/`Dispatch::call` err lane | `Err(Val)` | `Err(Signal)`; `From` impls keep `Err(Val::from(..))` / `?` compiling |
| kernel/caps/cli `call_resume` | `Result<Val, Val>` | `Result<Val, Signal>` (dedupe the cli duplicate is optional, out of scope) |
| toplevel guards | `Ok(Val::Recur)` → error | `Flow::Recur` → same error |
| boundary formatters ×4 | `unwrap_thrown(&e).unwrap_or(&e)` | `e.payload()` + `format!("{e}")` fallback — byte-identical output |
| `type` builtin / `val_type_name` / Display / Debug / Eq / Hash / `is_authority_free` / expr.rs arms | sentinel arms | deleted (unrepresentable) |
| `Val::Cap{..}` destructures (kernel ×7, attenuate ×2, tests) | field access | `CapHandle` accessors |
| `EffectTarget::Cap` constructions (glia ×2) | struct literal with `cap_id` | via `CapHandle::effect_target()` |

Dependent packages migrated in this PR: `crates/glia`, `std/kernel` (+`attenuate.rs`), `std/caps` (+`mcp_adapter.rs`), `std/shell`, `src/cli/shell.rs`. Verified non-consumers: `crates/rpc`, `src/dispatcher`, `tests/`, examples.

## 4. Deliberate observable changes (to pin with tests)

1. Recur in non-tail position → structured error `[glia.error/internal] recur not in tail position` instead of an inert `#<recur>` value (`(loop [] [(recur)])`, `(f (recur))`).
2. `(type ...)` can no longer yield `:recur`/`:effect`/`:resume` (values unrepresentable).
3. Rust API: three `Val` variants removed; `NativeFnImpl`/`AsyncNativeFnImpl`/`Dispatch::call` err type = `Signal`; toplevel entry points return `EvalError`; `Val::Cap` payload opaque; `next_cap_id` private; `unwrap_thrown` re-typed.
4. Everything else byte-identical at user boundaries: shell/REPL/init.d strings, MCP error text + JSON `data`, `#<effect ...>` display for unhandled effects, exit behavior, try/catch/or-else/guard/try-resume semantics, one-shot resume violations, attenuation freshness.

## 5. Open questions for review

1. `EffectTarget::Cap` public fields with opaque `CapId` vs fully sealed `CapTarget` struct — evidence says the former suffices; confirm.
2. `Signal` naming and location (`glia::Signal` vs `glia::eval::Signal`).
3. Whether to keep `Clone` on `EvalError` (cheap, useful for tests) — proposed yes.
4. The prompt attachment was truncated at §2 of the checkpoint spec (mid `EvalControl` example); any further checkpoint sections (§3+) should be supplied before implementation begins.
