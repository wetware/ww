# Glia Effect System

## Overview

Glia's error handling and effect system, implemented in two phases:
- **Phase 1 (#205, historical):** `try`/`throw` over a `:fail` effect, `try` returning `{:ok val}` / `{:err data}`.
- **Phase 2 (current):** errors and effects share machinery. `(throw err)` performs `:glia.exception`; `(try EXPR (catch :tag e BODY) ...)` installs a handler that dispatches on `:glia.error/type`. The `Result<Val, Val>` carrier is unchanged at the host boundary; an unhandled throw escapes eval as `Err(Val::Effect{ effect_type: "glia.exception", data: <err> })`.

The design follows **Approach C (Capability-Effect Fusion):** standard Unison-style algebraic effects where the Membrane is the outermost effect handler. Every capability call is a `perform`. The handler stack mirrors the capability chain.

## Motivation

### Errors vs Faults (Hickey's distinction)
- **Errors** = programmer mistakes or bad input (in-process, fix the code)
- **Faults** = things that go wrong in the world (systemic, must respond)

### Why effects, not just try/catch
An effect system generalizes error handling to cover nine additional concerns:

| Concern | Mechanism |
|---------|-----------|
| Error recovery | `throw`/`try` (Phase 1) |
| Capability boundary visibility | `perform` makes proc-exit explicit |
| Concurrency without async/await | Fiber yield/resume via effects |
| Distributed state with policy | Handler decides local vs remote |
| Transparent retry / circuit-breaking | Handler retries, code doesn't know |
| Deterministic replay / debugging | Record effect log, replay for debugging |
| Transactional effect batching | Handler buffers, commits atomically |
| Audit trail | Handler logs every capability access |
| Supervision | Crash = effect, supervisor = handler |
| Resource lifecycle | Handler tracks acquire/release |

All share one structure: **interposing policy at a boundary** — the same thing the Membrane does at the RPC level. Effects are the Membrane's language-level twin.

## Design Decisions

1. **Effects are the ONLY way to interact with the outside world from Glia.** No backdoor calls that bypass the effect system.
2. **One-shot continuations only.** Resume or abort, no cloning (OCaml 5's pragmatic choice).
3. **Dynamic handler lookup.** `perform` walks up the handler stack at runtime (Unison-style, not Koka-style static evidence passing). Glia is dynamically typed.
4. **`throw`/`try` are sugar over `:glia.exception` effect.** Phase 1 used `:fail` and `Expr::Try`/`Expr::Throw` special forms returning `{:ok}/{:err}`. Phase 2 replaces them with prelude macros over `perform`/`with-effect-handler`, dispatching on `:glia.error/type`.

## Phase 1: Error Handling (#205)

### Language additions
- `throw` — `(throw data)` signals an error. `data` is any Val (idiomatically a map with `:type`).
- `try` — `(try expr)` evaluates expr; returns `{:ok val}` or `{:err data}`.
- `try-let` — prelude macro for bind-or-catch.
- `or-else` — prelude macro for default-on-failure.
- `guard` — prelude macro: `(guard test error-data)` throws if test is falsy.
- `ex-info` — builtin: `(ex-info "msg" {:type :foo})` constructs error map with `:message` merged with user data.

### Implementation
- Change `Result<Val, String>` to `Result<Val, Val>` internally
- All existing `Err(format!(...))` sites produce `Val::Map` with `{:type :internal :message "..."}`
- `Dispatch` trait signature changes to `Result<Val, Val>` (cross-crate API break)
- Add `Expr::Throw` and `Expr::Try` to analyzer
- `try` must NOT intercept `Val::Recur` — only `Err(Val)` values

### Examples
```clojure
(try (/ 1 0))
;; => {:err {:type :arithmetic :message "division by zero"}}

(try-let [id (perform host :id)]
  (println "connected:" id)
  (catch e
    (println "failed:" (:type e))))

(throw (ex-info "peer unreachable" {:type :network :peer "QmFoo"}))

(or-else (perform host :id) "unknown")

(guard (> n 0) (ex-info "must be positive" {:type :invalid}))
```

## Phase 2: Full Effect System (Q2)

### Language additions
- `perform` — `(perform :effect-type data)` or `(perform cap :method args...)` suspends computation. Returns the value passed to the handler's `resume`.
- `with-effect-handler` — `(with-effect-handler TARGET handler-fn body...)` installs one handler for `TARGET` (a keyword or a cap). Handler fn receives `(data)` to abort or `(data resume)` to resume. (The map-form `with-handler` in the early examples below was never implemented; one handler per form is the real shape.)

### Capability-effect fusion
- `(perform host :id)` lowers to `(perform :host {:method "id"})`
- Kernel installs Membrane as outermost handler for capability effects
- Authority checks happen in the handler (epoch validation, capability revocation)
- Stale epoch detected → handler re-grafts and resumes transparently

### Handler semantics

The surface form is `(with-effect-handler TARGET handler-fn body...)`, where
`TARGET` is a keyword (environmental effect) or a capability value
(object-scoped effect, matched by instance identity). The handler fn is called
with `(data)` to abort or `(data resume)` to optionally resume. `perform` has
two shapes: `(perform :keyword data)` and `(perform cap :method args...)`.

The following are the guarantees the resumable model rests on. Each is pinned by
a test in `crates/glia/src/eval.rs` / `crates/glia/src/effect.rs`; the test name
is given so the doc and the code stay in lockstep.

- **Abort on return-without-resume.** A handler that returns without calling
  `resume` discards the suspended body — the code after the `perform` never
  runs. (`abort_without_resume_skips_body_after_perform`,
  `abort_2arg_handler_no_resume`, `abort_1arg_handler`)
- **Resume returns to the exact `perform` site.** `resume` continues evaluation
  at the precise position of the `perform` inside the surrounding expression;
  e.g. `(+ 1 (* 10 (perform :x 0)))` resumed with `5` yields `51`.
  (`resume_continues_at_exact_perform_site_in_nested_expr`, `resume_basic`)
- **One-shot continuations.** `resume` can be called at most once; a second call
  is a runtime error — a structured `:glia.error/continuation-already-resumed`
  carrier, not a resume sentinel and not a plain string — so callers route on
  error type. (`make_resume_fn_second_call_is_structured_error`,
  `make_resume_fn_second_call_reports_oneshot_violation`,
  `make_resume_fn_second_call_errors`)
- **Handler forwarding skips self.** The handler frame is popped *before* the
  handler runs, so a handler that re-performs the *same* effect reaches the next
  outer handler rather than recursing into itself.
  (`handler_reperform_same_effect_forwards_to_next_outer_handler`)
- **Fail-closed with a structured carrier.** A `perform` with no matching
  handler surfaces a structured `Val::Effect { effect_type, data }` (for caps,
  `effect_type` is `cap:<name>`), never a plain string, so callers can match /
  unwrap it. (`unhandled_cap_effect_fails_closed_with_structured_carrier`)
- **Async native handlers resume.** A handler backed by an async native function
  resumes the body identically to a synchronous one.
  (`async_native_handler_resumes_body`, `async_native_fn_in_effect_handler`)
- **Dynamic (invocation-time) handler stack.** A closure or macro dispatches
  through the handler stack in force at its *invocation/expansion* site, not the
  one ambient at definition time.
  (`fn_invocation_uses_caller_handler_stack_not_definition_stack`,
  `macro_invocation_uses_caller_handler_stack_not_definition_stack`)

### Handler-abort vs resume cleanup

When a handler **resumes**, the suspended body is polled to completion: control
returns to the exact `perform` site (see above), so any resources the body
acquired before the `perform` are still live and its own scope exits (and runs
whatever cleanup that scope encodes) normally.

When a handler **aborts** — returns without calling `resume` — the suspended
body is *discarded*: the `with-effect-handler` form yields the handler's value
and the code after the `perform` never runs. Cleanup semantics of the abort
path:

- **Continuation resources are released by drop.** The suspended body future is
  dropped, and the one-shot resume channel is dropped without a send. There is
  no unwind-and-run-finalizers step (Glia has no `finally`/`defer`); resource
  release rides on Rust `Drop` of whatever the abandoned future still owned.
  Any effect the body *would* have performed after the `perform` simply does not
  happen, so it acquires nothing further.
- **Capabilities are unaffected by abort.** Aborting a body does not revoke or
  close capabilities the body already holds — capability lifetime is governed by
  the membrane/epoch model, not by continuation liveness. An abort just means the
  post-`perform` code that might have *used* those caps never runs.
- **Resuming a dropped continuation fails closed, structured.** If a suspended
  computation is ever polled after its resume channel was dropped without a send
  (the abandonment path), it surfaces a structured
  `:glia.error/continuation-abandoned` carrier rather than a bare string, so a
  caller can distinguish "handler abandoned me" from an ordinary value.
  (`sender_drop_yields_structured_abandonment_error`) In normal flow an abort
  returns the handler's value directly and the body is never re-polled, so this
  carrier is the defensive rail rather than the common case.

### Phase transition (current state)
`Expr::Try` / `Expr::Throw` were never required: `try`/`throw` are pure prelude macros over `perform` / `with-effect-handler`. The current shape:

```clojure
(defmacro throw [data]
  `(perform :glia.exception ~data))

(defmacro try [expr & catches]
  ;; Dispatcher walks catches, matching on :glia.error/type;
  ;; non-match falls through to the next clause; without a wildcard,
  ;; the dispatcher re-throws.
  ...)
```

The full implementation lives in `crates/glia/src/prelude.glia` and is loaded via `include_str!` at boot. See also `crates/glia/src/error.rs` for the `GliaError` enum, the canonical schema, and `unwrap_thrown` for outer callers.

### Examples
```clojure
;; Testing — mock capabilities
(with-handler
  {:host (fn [req resume] (resume {:id "mock-peer"}))}
  (assert (= (perform host :id) "mock-peer")))

;; Retry on transient failure
(with-handler
  {:fail (fn [err resume]
           (when (= :network (:type err))
             (sleep 1000)
             (resume :retry)))}
  (publish-data))

;; Supervision
(with-handler
  {:fail (fn [err _resume]
           (log "crashed:" err)
           (restart-proc))}
  (run-service))

;; Audit trail
(with-handler
  {:host (fn [req resume]
           (log "host access:" req)
           (resume (perform :host req)))}
  (run-service))
```

## Non-goals

The resumable model is deliberately bounded. It does **not** provide:

- **No persisted handler stacks.** The handler stack is in-memory dynamic scope
  for the duration of an eval; it is not serialized or checkpointed.
- **No cross-peer continuations.** A `resume` continuation is local to the peer
  that suspended it; continuations are not shipped across the network.
- **No multi-shot continuations.** Continuations are one-shot — resume or abort,
  never cloned or resumed more than once.

## Open Questions

1. Should `with-effect-handler` support a `finally` clause?
2. Should capability effects use namespaced keywords (`:ww/host`) to avoid collision?

**Resolved:** `perform` without a matching handler fails closed with a structured
`Val::Effect` carrier (see "Handler semantics"). The Phase 2 continuation
mechanism is a `oneshot` channel pair (`crates/glia/src/oneshot.rs`), not
`tokio::sync::oneshot`.

## De-risk Strategy

Build standard effects (Approach A) first. Wire Membrane as handler second. If the Membrane-as-handler pattern creates problems, fall back to "Membrane handles RPC, effects handle in-proc concerns." No work is lost — the language primitives are identical either way.
