# PR-1 Final Implementation Contract

Branch: `glia-control-extraction` @ f1365b6. Status: FINAL PRE-CODE CHECKPOINT.
Builds on `.context/pr1-design-report.md` (v1: inventory, cap identity) and `.context/pr1-design-report-v2.md` (v2: exceptions-as-effects, Fault, classification). Approved in principle: Option A; exceptions via existing `:glia.exception` machinery; faults outside `try`; `Flow::Recur`; uniform `try-resume` semantics.

---

## 1. init.d output — RESOLVED: Option A (adopt peeled formatting)

**Producer:** `run_initd` error arm, `std/kernel/src/lib.rs:1831`:
`log::error!("init.d: {name}: form {}: {e}", i + 1)` — routed to the kernel's `StderrLogger` (`lib.rs:118-144`, WASI stderr, default max level Warn so `error!` is emitted). The sibling `init.d: {e}` at `lib.rs:2377` logs `run_initd`'s own I/O-level failure, not an eval error — unaffected.

**Exact current output** for a failing form (name `05-status`, form 2):
- raw-lane error (e.g. type mismatch):
  `init.d: 05-status: form 2: {:glia.error/type :glia.error/type-mismatch :glia.error/message "..." ...}`
- unhandled `(throw ...)`:
  `init.d: 05-status: form 2: #<effect :glia.exception {:glia.error/type ... :glia.error/message "..."}>`
- unhandled non-exception effect: `init.d: 05-status: form 2: #<effect :net {...}>`

**Exact proposed output** (peeled `EvalError` Display):
- both former cases: `init.d: 05-status: form 2: {:glia.error/type ... :glia.error/message "..."}`
- non-exception effect: unchanged `#<effect :net {...}>`
- fault: `init.d: 05-status: form 2: {payload map}` (same as today's raw display)

**Consumers:** none. Repo-wide search finds no test, script, parser, or tooling matching these strings (one prose mention in CHANGELOG). Human-facing logging only.

**Information content:** the only loss is the `#<effect :glia.exception …>` wrapper distinguishing "thrown" from "raw" — a distinction the approved unification erases semantically (post-change both *are* unhandled exceptions). No other information changes.

**Decision: A.** Consistent peeled formatting everywhere. No unrelated messages touched.

## 2. Fault taxonomy — single channel, kind field

One operational uncatchable channel (`Control::Fault(Fault)` internally, `EvalError::Fault(Fault)` at the boundary). Taxonomy is a field, not a second channel:

```rust
/// Category of an uncatchable fault.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum FaultKind {
    /// Structurally invalid program control — e.g. recur outside tail position.
    Language,
    /// Evaluator/continuation invariant failure or embedder/host failure.
    Runtime,
}

/// Unrecoverable failure; bypasses all Glia handlers. Payload is a
/// structured error map (same schema as exceptions) for boundary display.
#[derive(Clone, Debug)]
pub struct Fault { kind: FaultKind, payload: Val }   // fields private
impl Fault {
    pub fn kind(&self) -> FaultKind;
    pub fn payload(&self) -> &Val;
    pub(crate) fn language(payload: Val) -> Self;
    pub(crate) fn runtime(payload: Val) -> Self;
}
impl fmt::Display for Fault { /* == payload Display */ }
```

**Payload tags:**
- **Language fault:** new tag `glia.error/invalid-recur` (new `tag::INVALID_RECUR` + `GliaError::InvalidRecur { context }` arm; message keeps today's wording "recur not in tail position"). Deliberate tag change from today's `glia.error/internal` at the (rarely-hit) toplevel guard; every other language-fault site is *new* enforcement (previously inert sentinel debris), so no string regression.
- **Runtime faults:** existing tags preserved at their sites, no retagging: `glia.error/internal` (invalid resume fn — kernel:816/caps:246/cli:777, import-poll `Pending` — caps:402/446, reserved impossible branches), `glia.error/continuation-abandoned` (oneshot abandon), `glia.error/protocol-mode-unavailable` (MCP host refusals, minted by the embedder), plus whatever payload a HostEffect handler returns.

`NativeSignal::fault(err)` mints **Runtime** only — embedders cannot produce language faults (those are the evaluator's judgment about program structure).

## 3. Handler-depth limit — RESOLVED: catchable EXCEPTION

**Why it exists:** `MAX_HANDLER_DEPTH = 64` (`effect.rs:84`) — the doc comment states it "prevents pathological nesting from causing unbounded walk cost": `perform_dispatch` walks the handler stack linearly per dispatch, so the cap bounds worst-case per-effect cost.

**Failure mode prevented:** cost amplification, not memory/stack exhaustion and not metering integrity.

**Why catching is safe:** the check fires at `with-effect-handler` entry **before** the frame is pushed (`eval.rs:2193-2200`) — no partial state, nothing corrupted, the stack is exactly as it was. Dispatching the exception itself pushes no frames, and any outer `try` was installed at depth < 64, so the catch path operates entirely within the cap. Catching cannot defeat the limit (a retry still cannot exceed 64).

**Why it is not fuel/stack-analogous:** fuel protects the *host's* metering integrity against untrusted guests and must be unforgeable and uncatchable; native stack exhaustion is detected mid-flight with unwinding state. The depth cap is a deterministic precondition check on guest-visible policy. Retry after backing off (restructuring nesting, releasing frames) is meaningful.

**Decision:** catchable **EXCEPTION**, keeping today's `glia.error/internal` tag and message for PR-1 (string-preserving; retagging is a listed adjacent fix). Existing test at `eval.rs:8062-8083` keeps passing at the boundary and gains a catchability companion. Division by zero: catchable exception, as approved.

## 4. Final exact types and signatures

### crates/glia — crate-private

```rust
pub(crate) enum Flow { Value(Val), Recur(Vec<Val>) }

pub(crate) enum Control {
    Fault(Fault),
    Unhandled(EffectRequest),   // includes unhandled :glia.exception
    Resume(Val),
}
pub(crate) type EvalResult = Result<Flow, Control>;

impl Flow {
    /// Non-tail value demand; Recur here → Control::Fault(language, invalid-recur).
    pub(crate) fn into_value(self) -> Result<Val, Control>;
}

/// Dispatch a catchable exception on the current handler stack.
/// Ok(v) = a resuming handler supplied the failing expression's value.
async fn throw(env: &Env, payload: Val) -> Result<Val, Control>;
```

Internal signatures: `eval_expr` family → `Result<Flow, Control>`; sync helpers (builtins, analyze, pattern, cell validation, ~150 raise sites) keep `Result<T, Val>` (Val = exception payload) with conversion via `throw` at the ~10–15 async chokepoints; `perform_dispatch` → `Result<Val, Control>`; oneshot `Receiver::Output` → `Result<Val, Control>` (abandon → `Fault::runtime(continuation_abandoned())`); with-effect-handler machine matches `Err(Control::Resume(_))`; `perform_cap_value` retry matches `Control::Unhandled` whose target is the current cap's id; HostEffect handler `Err(v)` → `Control::Fault(Fault::runtime(v))`; `eval`/`eval_expr` demoted to `pub(crate)`.

### crates/glia — public

```rust
// lib.rs
pub enum FaultKind { Language, Runtime }
pub struct Fault { .. }                                  // §2 above

pub struct NativeSignal(pub(crate) NativeSignalKind);
pub(crate) enum NativeSignalKind { Throw(Val), Resume(Val), Fault(Fault) }
impl NativeSignal {
    pub fn throw(err: impl Into<Val>) -> Self;
    pub fn fault(err: impl Into<Val>) -> Self;           // → Fault::runtime
    pub(crate) fn resume(val: Val) -> Self;              // make_resume_fn only
}
impl From<Val> for NativeSignal;                          // → throw
impl From<String> for NativeSignal;
impl From<&str> for NativeSignal;
impl From<GliaError> for NativeSignal;

pub type NativeFnImpl = Rc<dyn Fn(&[Val]) -> Result<Val, NativeSignal>>;
pub type AsyncNativeFnImpl =
    Rc<dyn Fn(Vec<Val>) -> Pin<Box<dyn Future<Output = Result<Val, NativeSignal>>>>>;

// Cap identity (v1 §2.6, unchanged)
pub struct CapId(u64);                                   // opaque; no public mint
pub struct CapHandle { name, schema_cid, id, inner }     // fields private
impl CapHandle {
    pub fn name(&self) -> &str;
    pub fn schema_cid(&self) -> &str;
    pub fn id(&self) -> &CapId;
    pub fn inner(&self) -> &Rc<dyn Any>;
    pub fn effect_target(&self) -> EffectTarget;
}
// Val: -Recur, -Effect, -Resume; Cap(CapHandle)
pub fn make_cap(..) -> Val;                              // unchanged signature; only mint
// next_cap_id: private

// effect.rs
pub enum EffectTarget { Keyword(String), Cap { name: String, schema_cid: String, id: CapId } }
pub struct EffectRequest { pub target: EffectTarget, pub data: Val }
impl EffectRequest { pub fn effect_type(&self) -> String; }   // keyword | "cap:{name}"
// HostEffect / HostEffectResult / HostEffectHandler / HostEffectFuture: unchanged

// eval.rs
pub trait Dispatch {
    fn call<'a>(&'a self, name: &'a str, args: &'a [Val])
        -> Pin<Box<dyn Future<Output = Result<Val, NativeSignal>> + 'a>>;
    fn reify_attenuation(&self, cap: &Val, allow: &BTreeSet<String>)
        -> Option<Result<Val, Val>> { None }             // unchanged (payload = exception)
    fn validate_cell_grant(&self, name: &str, cap: &Val) -> Result<(), Val> { Ok(()) }
    fn report_warning(&self, warning: &str) {}
}

pub enum EvalError { Fault(Fault), Unhandled(EffectRequest) }
impl EvalError {
    pub fn thrown(&self) -> Option<&Val>;    // unhandled :glia.exception payload
    pub fn payload(&self) -> Option<&Val>;   // thrown data | fault payload
}
impl fmt::Display for EvalError;             // peels exceptions; faults = payload;
                                             // other effects = "#<effect :{ty} {data}>"

pub fn eval_toplevel(..)                   -> .. Result<Val, EvalError> ..;
pub fn eval_toplevel_expr(..)              -> .. Result<Val, EvalError> ..;
pub fn eval_toplevel_with_host_effects(..) -> .. Result<EvalOutcome, EvalError> ..;
// EvalOutcome { Value(Val), Exit }: unchanged

// error.rs
pub mod tag { .. pub const INVALID_RECUR: &str = "glia.error/invalid-recur"; }
pub enum GliaError { .. InvalidRecur { context: String }, .. }   // message: "recur not in tail position"
pub fn unwrap_thrown(err: &EvalError) -> Option<&Val>;           // re-typed shim
```

### Embedder deltas (kernel, caps, std/shell, cli, mcp_adapter)

Per v2 §7; classification defaults: `From` → throw everywhere, explicit `NativeSignal::fault` at exactly the five enumerated runtime-invariant sites (kernel:816, caps:246, cli:777, caps:402, caps:446); `write_stdout` and MCP refusals reach `Fault` via the HostEffect Err path with no signature change to `HostEffectFuture`.

## 5. Final approved decision list

| # | Decision | Status |
|---|---|---|
| 1 | Option A — exception normalization inside PR-1, no temporary Raise API | **APPROVED** (user) |
| 2 | `try-resume` applies uniformly to evaluator/native exceptions | **APPROVED** (user) |
| 3 | `Flow::Recur` for lexical recur; non-tail recur = Language fault | **APPROVED** (user: Flow::Recur; fault classification per v2 §11.3 — carried into contract) |
| 4 | init.d adopts peeled formatting (Option A) | RESOLVED this checkpoint — recommend approve |
| 5 | Fault = single channel + `FaultKind { Language, Runtime }`; `glia.error/invalid-recur` tag for language faults; runtime tags unchanged | RESOLVED this checkpoint — recommend approve |
| 6 | Handler-depth limit = catchable exception (tag unchanged) | RESOLVED this checkpoint — recommend approve |
| 7 | Division by zero = catchable exception | **APPROVED** (user) |
| 8 | Abandoned continuation = Runtime fault | carried from v2 §11.4 (recommended, not separately vetoed) |
| 9 | `NativeSignal::fault` public (Runtime only; de-escalation) | carried from v2 §11.5 |
| 10 | Cap identity: `CapId`/`CapHandle`, `EffectTarget::Cap` carries `CapId` | v1 §2.6, within authorized scope §D |

Adjacent fixes remain parked pending separate approval (call_resume/host-effects dedup, internal-tag cleanup, caps structured errors). Drift list of v2 §10 unchanged.

## 6. Blockers

None. All semantic questions are resolved; the implementation is mechanical from this contract. One note, not a blocker: the original PR-1 prompt attachment was truncated (§3+ of its checkpoint spec never arrived), but the two follow-up prompts supersede it — flagging only in case that section contained additional deliverables beyond what has now been specified.
