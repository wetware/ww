# Guest Runtime

Wetware runs production Cells as native `wasm32-wasip3` components. Wasmtime
owns Component Model suspension and resumption. Wetware does not implement a
guest scheduler.

Primary code references:

- `crates/cell/src/engine.rs` configures the production Component Model engine.
- `crates/cell/src/proc.rs` owns one `Store` and one component instance per Cell.
- `crates/cell/src/p3.rs` implements the granted P3 transport.
- `std/system/src/lib.rs` constructs and composes a guest Cap'n Proto session.
- `std/kernel/src/lib.rs` defines the PID0 application Future.

See [rpc-transport.md](rpc-transport.md) for transport behavior and
[api/wasm-guest.md](api/wasm-guest.md) for host interfaces.

## Production execution model

Each Cell has one Wasmtime `Store`. `Proc` invokes the generated async
`wasi:cli/run` export through `Store::run_concurrent`. Component Model async
tasks can suspend on P3 imports while the host continues to drive the Store.

The host retains the existing executor topology. Each executor OS thread owns
a current-thread Tokio runtime and a `LocalSet`. Multiple Cell owners can run
on one executor thread, but each Cell keeps an independent Store, fuel state,
filesystem state, and transport grant.

Wasmtime fuel yields and host epoch ticks control Cell-level execution. This
policy can stop a compute-bound Cell. The policy does not provide fairness
between child Futures inside one guest.

## Async Rust guest model

An async Rust guest exports one generated P3 entry task. The task creates one
ordinary Rust session Future with three inputs:

```text
generated wasi:cli/run task
  `-- session Future
        |-- P3 transport completion
        `-- first of
              |-- capnp_rpc::RpcSystem
              `-- application Future tree
```

`std/system` supplies P3 `AsyncRead` and `AsyncWrite` adapters. The crate also
opens the one-shot transport, constructs `VatNetwork` and `RpcSystem`, obtains
the host bootstrap, and composes all three session inputs.

`RpcSystem` owns request-level Cap'n Proto concurrency. A server method can
return a request-owned Future through `Promise::from_future`. Wetware does not
provide detached local task spawning.

The guest runtime contains no Tokio, custom executor, event loop, parker,
`PollSet`, readiness registry, fixed liveness timer, task queue, generic spawn,
or async channel runtime. The `wit-bindgen` `async-spawn` and
`inter-task-wakeup` features remain disabled.

## Root Future completion

The session first selects between `RpcSystem` and the application Future. The
P3 transport completion participates in the outer session result.

- If the application finishes first, `std/system` drops `RpcSystem`. An
  application error returns immediately. Application success waits for the
  resulting transport completion.
- If `RpcSystem` closes first, an orderly peer close maps to the documented
  session outcome. An RPC error returns immediately. `std/system` drops the
  application Future, and an orderly RPC result waits for transport completion.
- If transport completion finishes first, orderly closure succeeds. A
  `TransportError::Failed` value fails the session.
- If the host cancels the export or drops the Store, the complete Future tree
  drops. Store teardown reclaims P3 resources and closes the host transport.

The guest does not drain RPC for an arbitrary interval. A completed P3 stream
write already waits for the underlying host flush.

## Supported suspension sources

First-party async guests suspend on host-visible P3 operations:

- the granted transport input, output, and completion resources;
- monotonic-clock waits;
- PID0 standard input;
- filesystem operations supplied by WASI P3.

These operations can resume through Component Model P3 without a Wetware
scheduler. Current first-party guests have no pure-Rust external wake source
after the generated task enters Component Model Wait.

The following cases remain outside the runtime contract:

- non-yielding application CPU work blocks same-Cell async progress until a
  host fuel or epoch boundary returns control;
- a genuinely synchronous import blocks its Cell;
- pure-Rust wake sources that are invisible to P3 are unsupported;
- detached spawn, task handles, and generic async channels are unsupported.

Future language runtimes can define scheduling above the common P3 substrate.
They must not change the host transport authority model.

## PID0

PID0 uses the same root-Future model. Its application branch grafts host
capabilities, loads the standard composition, and commits readiness. PID0 then
waits for P3 standard input when `WW_TTY` is set. A daemon PID0 awaits forever.

The standard-input wait is asynchronous. Input resumes the application branch,
and EOF completes an interactive PID0 successfully. Production PID0 uses
`system::run`, so it exports no guest bootstrap capability after initialization.
The host `HttpListener` serves `/status` through a separate status Cell. Route
availability does not prove PID0 guest-server progress.

PID0 alone imports
`wetware:kernel-runtime/readiness@1.0.0`. The host installs this interface only
on the trusted PID0 linker. The function commits the generation already bound
to PID0's process-local graft.

## Synchronous Cells

Synchronous Cells use a P3 CLI export but do not link `std/system` or construct
an `RpcSystem`. Echo, counter, snap-hello-rs, and the routing-key probe remain
runtime-free unless their own behavior needs RPC.

The `wasm32-wasip3` target does not imply that every guest uses async RPC. It
defines the common Component Model ABI and lets synchronous guests use only
their declared interfaces.

## Architecture guardrails

Do not add guest polling to repair a missing wake. First identify whether the
operation exposes a P3 waitable and whether the generated task owns that
waitable.

Do not enable `async-spawn` or `inter-task-wakeup` without a current guest that
requires the corresponding semantics. Do not add a P2 compatibility path for
new guests.
