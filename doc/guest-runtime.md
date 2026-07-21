# Guest Runtime

This document specifies the async runtime that drives WASM guest
processes (Glia shell, cells, kernel).  It complements
[rpc-transport.md](rpc-transport.md) (transport plumbing) and
[architecture.md](architecture.md) (capability flow).

Primary code references:
- `std/system/src/lib.rs` — poll loop, WASI stream adapters
- `crates/glia/src/eval.rs` — async evaluator, effect handler state machine
- `crates/glia/src/effect.rs` — effect types, handler stack, resume function
- `crates/glia/src/oneshot.rs` — single-threaded oneshot channel

## Design principles

1. **Single-threaded, hand-rolled, no external async runtime.**
   The guest runs as single-threaded WASM (wasm32-wasip2).  There is no
   tokio, async-std, or executor crate.  The runtime is a hand-written
   poll loop using `std::task::{Context, Poll, Waker}` and WASI poll.
   This gives maximal control over scheduling and keeps the binary small.

2. **`Rc<RefCell<T>>` everywhere — no `Arc`, no `Mutex`.**
   Single-threaded means no thread safety overhead.  All interior
   mutability uses `RefCell`.  All shared ownership uses `Rc`.

3. **One poll loop to rule them all.**
   `poll_loop()` is the single event loop that drives both the capnp-rpc
   state machine and user futures.  Every guest entry point
   (`system::run`, `system::serve`, `system::serve_stdio`) delegates to
   it.  There is exactly one implementation of the poll/flush/block
   cycle — no duplicated loops.

4. **Effects are the concurrency primitive.**
   Glia's `perform`/`with-effect-handler`/`resume` mechanism is the
   only control-flow abstraction.  There is no `spawn`, no task pool,
   no concurrent evaluation.  An effect suspends the body, dispatches
   to a handler (which may `.await` an RPC promise), and resumes the
   body when the handler calls `resume`.  This is structured concurrency
   without a scheduler.

5. **All evaluation is async.**
   Every `eval*` function returns `Pin<Box<dyn Future>>`.  Handlers can
   be sync (`NativeFn`), async (`AsyncNativeFn`), or user-defined (`Fn`).
   The evaluator is a single cooperative coroutine — no parallelism,
   no preemption.

## The poll loop

`poll_loop` in `std/system/src/lib.rs` is the guest's event loop:

```
fn poll_loop<T>(
    rpc_system, pollables,
    poll_work: impl FnMut(&mut Context) -> Poll<T>,
) -> Option<T>
```

Returns `Some(T)` when `poll_work` completes, `None` if RPC closes first.

Each iteration:

```
1. Reset WRITE_OCCURRED flag
2. Poll RPC system        (deliver inbound messages)
3. Poll user work          (run Glia evaluation / application logic)
4. Poll RPC system again   (flush outbound messages queued by step 3)
5. Block on WASI poll      (reader + writer, or reader + idle timeout)
```

The **double-poll** (steps 2 + 4) is critical: user work in step 3 may
queue outbound RPC calls.  Without step 4, those calls are never flushed
before `wasi_poll` blocks, causing deadlock.  See
[rpc-transport.md](rpc-transport.md) for deadlock analysis.

### Waker strategy

The loop uses `Waker::noop()` — a no-op waker from the standard library.
Wakers are irrelevant here because the loop polls unconditionally on
every iteration; forward progress comes from WASI poll unblocking, not
from waker notifications.  The only place wakers matter is inside Glia's
oneshot channel (see below), where they enable the effect handler state
machine to re-poll the body after a resume.

### WASI poll blocking

When the loop makes no progress and has no pending writes, it blocks on
`wasi_poll::poll([reader, idle_timeout])` with a 100ms safety timeout.
The timeout guards against missed wakeups from wasmtime's
`AsyncReadStream` background worker.  See the `IDLE_POLL_TIMEOUT_NS`
comment in `std/system/src/lib.rs` for details.

### WRITE_OCCURRED flag

A thread-local `Cell<bool>` set by `StreamWriter::poll_write`.  Tracks
whether any data was written during the current poll cycle so the loop
knows whether to include the writer pollable in the WASI poll set.
This replaced a racy `pollable.ready()` check that caused deadlocks.

## The effect system

Glia's effect system bridges evaluation and Cap'n Proto RPC.

### Types (`crates/glia/src/effect.rs`)

- **`EffectTarget`** — what the effect targets: `Keyword(String)` for
  environmental effects, or `Cap { name, schema_cid, cap_id }` for
  capability-scoped effects.  Caps match by per-instance `cap_id`; the
  schema CID remains type/introspection metadata, not authority identity.

- **`EffectSlot`** — shared state between `perform` and the handler loop.
  Holds `Option<(EffectTarget, Val, oneshot::Sender)>`.

- **`HandlerContext`** — one frame on the handler stack: a slot and a
  target.  Each `with-effect-handler` pushes one.

- **`HostEffectHandler`** — an embedding-installed async handler value. It is
  valid only in `with-effect-handler`; it is not an ordinary callable value and
  `apply` rejects it. This preserves `perform` as the evaluator's sole route
  into application-visible host work.

- **`HandlerStack`** — `Rc<RefCell<Vec<Rc<RefCell<HandlerContext>>>>>`.
  Dynamic scope, not lexical.  Walked newest-first by `perform`.

### Dispatch flow

When Glia code calls `(perform cap :method args...)`:

1. `perform_dispatch` walks the handler stack (newest → oldest),
   looking for a `HandlerContext` whose target matches.
2. Creates a oneshot channel `(tx, rx)`.
3. Writes `(target, data, tx)` into the matching handler's `EffectSlot`.
4. Awaits `rx` — the body suspends here.

The `with-effect-handler` state machine (`eval.rs`) polls the body.
When the body returns `Poll::Pending` and the slot has a pending effect:

5. The state machine transitions to `HandlerState::Handling`.
6. It invokes the handler function with `(data, resume_fn)`.
7. The handler does its work (e.g. `.await`s an RPC promise), then
   calls `(resume result)`.
8. `resume` sends `result` through the oneshot channel and returns
   `Err(Val::Resume(val))` to short-circuit the handler's eval chain.
9. The state machine detects `Resume`, transitions back to
   `HandlerState::Polling`, wakes the context, and the body resumes
   with the value from the oneshot.

The same dispatch applies to standard environmental effects:
`(perform :load path)`, `(perform :stdout value)`, and `(perform :exit nil)`.
Their concrete handlers belong to the embedding. Kernel and terminal handlers
load files, write terminal output, and end the process; the local shell returns
an exit sentinel to its outer loop; MCP rejects stdout and exit so its JSON-RPC
transport remains valid. These are semantic routing choices, not capability
authority checks: WASI sandbox/preopen policy and membrane configuration remain
the enforcement mechanisms for non-Glia guests and RPC capabilities.

### Handler stack discipline

Handlers are **popped before dispatch** and **pushed after completion**.
This ensures that if a handler itself calls `perform`, the effect goes
to an outer handler — not recursively to itself.

Max depth is 64 (`MAX_HANDLER_DEPTH`).

## The oneshot channel

`crates/glia/src/oneshot.rs` — a zero-dependency, single-threaded
oneshot channel that backs the effect resume mechanism.

- Uses only `std::task::{Context, Poll, Waker}` and `std::rc`/`std::cell`.
- `Sender` is not `Clone` — one-shot enforced by move semantics.
- `Sender::drop` signals abandonment so the receiver can detect abort
  (handler didn't call `resume`).
- `Receiver` implements `Future<Output = Result<Val, Val>>`.

## WASI stream adapters

`StreamReader` and `StreamWriter` implement `futures::io::AsyncRead` and
`futures::io::AsyncWrite` over WASI input/output streams.  These are
required by capnp-rpc's `VatNetwork`.

The `futures` crate dependency exists solely for these trait impls —
the rest of the runtime uses only `std::future` and `std::task`.

## Entry points

| Function | Purpose |
|----------|---------|
| `system::run(f)` | Bootstrap host cap, run `f` to completion, drive RPC. |
| `system::serve(bootstrap, f)` | Same as `run`, but also exports `bootstrap` to host. |
| `system::serve_stdio(bootstrap)` | Export cap over WASI stdin/stdout (no Membrane). |

All three delegate to `poll_loop`.

## Non-goals

- **No `spawn` / concurrent tasks.**  The runtime is intentionally
  sequential.  If concurrent evaluation is needed in the future, it
  should be added as an effect (`(spawn expr)`) with a task set in the
  poll loop — not by pulling in an external async runtime.

- **No tokio / async-std.**  The WASM target doesn't support OS I/O
  primitives these runtimes require.  The hand-rolled loop integrates
  directly with WASI poll, which is the correct abstraction for
  wasm32-wasip2.

- **No timers (yet).**  The idle timeout is internal to the poll loop.
  User-facing timeouts would be a natural extension via WASI
  `subscribe-duration` pollables, exposed as an effect.
