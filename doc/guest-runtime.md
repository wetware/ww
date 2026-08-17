# Guest Runtime

This document specifies the async runtime that drives WASM guest cells and
PID0. It complements
[rpc-transport.md](rpc-transport.md) (transport plumbing) and
[architecture.md](architecture.md) (capability flow). See the
[WASM guest API reference](api/wasm-guest.md) for the interface tables.

Primary code references:
- `std/system/src/lib.rs` — poll loop, WASI stream adapters
- `std/kernel/src/lib.rs` — active PID0 composition and readiness commit

PID0 additionally imports the versioned private
`wetware:kernel-runtime/readiness@1.0.0` interface. The native runtime installs
this interface only on the trusted PID0 linker; ordinary cell linkers omit it.
Its argument-free `kernel-ready` function (`kernel_ready()` in generated Rust)
commits the generation recorded by PID0's process-local graft after composition
completes. Because a WIT host function is not a Cap'n Proto capability value,
it cannot appear in a graft, be delegated to a child, or cross a network
connection.

## Design principles

1. **Single-threaded, hand-rolled, no external async runtime.**
   The guest runs as single-threaded WASM (wasm32-wasip2).  There is no
   tokio, async-std, or executor crate.  The runtime is a hand-written
   poll loop using `std::task::{Context, Poll, Waker}` and WASI poll.
   This gives maximal control over scheduling and keeps the binary small.

2. **One poll loop drives RPC and guest work.**
   `poll_loop()` is the single event loop that drives both the capnp-rpc
   state machine and user futures.  Every guest entry point
   (`system::run`, `system::serve`, `system::serve_stdio`) delegates to
   it.  There is exactly one implementation of the poll/flush/block
   cycle — no duplicated loops.

3. **Guest work and RPC share one cooperative loop.**
   `system::run` polls the guest's async entry future and the Cap'n Proto RPC
   system in the same loop. `system::run_with` can add guest pollables such as
   stdin.

## The poll loop

`poll_loop` in `std/system/src/lib.rs` is the guest's event loop:

```
fn poll_loop<T>(
    rpc_system, pollables, extras,
    poll_work: impl FnMut(&mut Context) -> Poll<T>,
) -> Result<T, PollLoopExit>
```

Returns the guest result when `poll_work` completes. Returns `PollLoopExit` if
RPC closes or fails first.

Each iteration:

```
1. Reset WRITE_OCCURRED flag
2. Poll RPC system        (deliver inbound messages)
3. Poll user work          (run the guest's async entry future)
4. Poll RPC system again   (flush outbound messages queued by step 3)
5. Block on WASI poll      (reader + writer, or reader + idle timeout)
```

The **double-poll** (steps 2 + 4) is critical: user work in step 3 may
queue outbound RPC calls.  Without step 4, those calls are never flushed
before `wasi_poll` blocks, causing deadlock.  See
[rpc-transport.md](rpc-transport.md) for deadlock analysis.

### Waker strategy

The loop uses `Waker::noop()` from the standard library. The loop polls work on
every iteration. WASI pollables and the idle timeout provide wakeups.

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

## WASI stream adapters

`StreamReader` and `StreamWriter` implement `futures::io::AsyncRead` and
`futures::io::AsyncWrite` over WASI input/output streams.  These are
required by capnp-rpc's `VatNetwork`.

The `futures` crate dependency exists solely for these trait impls —
the rest of the runtime uses only `std::future` and `std::task`.

## Entry points

| Function | Purpose |
|----------|---------|
| `system::run(f)` | Bootstrap the host-provided capability, run `f`, and drive RPC. |
| `system::serve(bootstrap, f)` | Same as `run`, but also exports `bootstrap` to host. |
| `system::serve_stdio(bootstrap)` | Export cap over WASI stdin/stdout (no Membrane). |

All three delegate to `poll_loop`.

## Non-goals

- **No guest task pool.** The runtime polls one guest entry future. Guest code
  that needs additional I/O readiness can register pollables through
  `system::run_with`.

- **No tokio / async-std.**  The WASM target doesn't support OS I/O
  primitives these runtimes require.  The hand-rolled loop integrates
  directly with WASI poll, which is the correct abstraction for
  wasm32-wasip2.

- **No timers (yet).**  The idle timeout is internal to the poll loop.
  A future guest API could expose WASI `subscribe-duration` pollables.
