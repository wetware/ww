# RPC Transport: Async Bidirectional Cap'n Proto over WASI Streams

This document explains how the host and WASM agents communicate over
Cap'n Proto RPC, with a focus on the transport plumbing, scheduling
model, and deadlock analysis.

Primary code references:
- `src/rpc/mod.rs`
- `src/cell/proc.rs`
- `std/system/src/lib.rs`

## High-level layout

At runtime there are two RPC links:
- Host <-> pid0 (kernel agent)
- Host <-> child (agent spawned by pid0 via `runtime.load()` + `executor.spawn()`)

Both links use the same transport mechanism: Cap'n Proto RPC over a
bidirectional in-memory duplex stream exposed to agents as WASI
io/streams resources.

```
          (Cap'n Proto RPC over WASI io/streams)
┌───────────────────────────┐           ┌───────────────────────────┐
│        Host (ww)          │           │        pid0 (kernel)      │
│  - serves Membrane        │<=========>│  - grafts, obtains Session │
│  - VatNetwork + RpcSystem │           │  - poll_loop event loop    │
└───────────────────────────┘           └───────────────────────────┘
           ^                                            |
           | runtime.load + spawn (child)               | Cap'n Proto RPC
           |                                            v
┌───────────────────────────┐           ┌───────────────────────────┐
│        Host (ww)          │           │        child agent         │
│  - serves Session         │<=========>│  - poll_loop event loop    │
│  - VatNetwork + RpcSystem │           │                            │
└───────────────────────────┘           └───────────────────────────┘
```

## Design rationale

The transport was migrated from a custom `wetware:streams` interface
with stub pollables and a busy-spin loop to standard WASI `io/streams`
+ `io/poll`. This eliminates CPU busy-spins in idle agents and aligns
with the WASI component model.

The transport uses **in-memory pipes** (`tokio::io::duplex`) with
Wasmtime's `AsyncReadStream` / `AsyncWriteStream` adapters. No OS
socket I/O is involved — all RPC messages traverse memory-only duplex
streams.

## Transport plumbing: host side

### 1) Creating the transport channel

`ProcBuilder::with_data_streams()` allocates an in-memory duplex stream:

- `tokio::io::duplex(PIPE_BUFFER_SIZE)` yields `(host_stream, guest_stream)`.
- `host_stream` stays on the host; `guest_stream` is injected into the
  guest runtime state (`ComponentRunStates.data_stream`).

Reference: `src/cell/proc.rs`

```
tokio::io::duplex()  ->  host_stream  <----->  guest_stream
```

### 2) Exposing to the agent as WASI io/streams

When the agent calls `wetware:streams/streams#create-connection`, the host
replaces the `guest_stream` with WASI stream resources:

- Split guest stream into read/write halves.
- Wrap them as `AsyncReadStream` and `AsyncWriteStream`.
- Store them in a `ConnectionState` resource.

Reference: `src/cell/proc.rs`

```
guest_stream
  -> split (guest_read, guest_write)
  -> DynInputStream (AsyncReadStream)
  -> DynOutputStream (AsyncWriteStream)
```

### 3) Wiring Cap'n Proto RPC over the host side

`ExecutorImpl::spawn()` in the host sets up the child process and builds a Cap'n Proto
`VatNetwork` over the host's stream halves:

- `handles.take_host_split()` yields `(reader, writer)`.
- `build_peer_rpc(reader, writer, wasm_debug)` wraps them in a `VatNetwork`.
- A `RpcSystem` is spawned in a local task set alongside the guest process.

Reference: `src/rpc/mod.rs`

```
host_stream -> split -> (AsyncRead, AsyncWrite) -> VatNetwork -> RpcSystem
```

## Transport plumbing: guest side

### 1) Guest stream connection

`RpcSession::connect()` in the guest SDK calls the WIT binding
`create_connection()` and obtains a WASI input and output stream:

- `create_connection()` -> `connection.get_input_stream()` and
  `connection.get_output_stream()`.
- These are wrapped as `StreamReader` and `StreamWriter`.

Reference: `std/system/src/lib.rs`

```
create_connection()
  -> WASI InputStream  -> StreamReader (AsyncRead)
  -> WASI OutputStream -> StreamWriter (AsyncWrite)
```

### 2) Cap'n Proto RPC over WASI streams

The guest constructs a Cap'n Proto `VatNetwork` over those stream adapters
and bootstraps a client:

Reference: `std/system/src/lib.rs`

```
StreamReader/StreamWriter -> VatNetwork -> RpcSystem -> client
```

## Scheduling model and CPU behavior

Agents are not busy-spinning when idle. They run a cooperative event
loop in `poll_loop`:

1) Poll the RPC system and any application futures/promises.
2) If no progress is made, block in `wasi_poll::poll` on stream readiness
   (pollable handles from the WASI streams).

Reference: `std/system/src/lib.rs`

Key points:
- **No constant CPU polling**: when nothing makes progress, the agent
  blocks on `wasi_poll::poll`, yielding to the host runtime.
- **Asynchronous on the host**: the host side uses Tokio async I/O and a
  spawned `RpcSystem` to process messages without blocking the host thread.
- **Double-poll pattern**: `poll_loop` calls `rpc_system.poll`
  twice per iteration — once before and once after `poll_work` — to flush
  RPC writes the user future queued before blocking in `wasi_poll`. Missing
  the second poll causes deadlock: calls queued during `poll_work` are never
  sent, and the host never responds.

```
loop:
  poll_rpc        -- deliver inbound messages
  poll_future     -- run application logic (may queue outbound RPC)
  poll_rpc        -- flush outbound messages
  if done -> exit
  if no progress -> wasi_poll::poll([reader, writer])
```

## End-to-end flows

### Flow A: pid0 spawns a child agent

```
pid0 (kernel)               host                        child agent
─────────────              ──────                      ─────────────
graft() -> Session
load+spawn    ───────────> spawn child Proc + RpcSystem
                           return Process cap
wait RPC     <──────────── ProcessImpl::wait
```

### Flow B: child loads + spawns a grandchild

```
child agent                 host
───────────                ──────
graft() -> Session
runtime.load(wasm) ──────> RuntimeImpl::load (cache lookup or compile)
  -> Executor client <──── return pipelined Executor
executor.spawn()   ──────> ExecutorImpl::spawn
  -> Process client <───── return Process cap
```

## Transport diagram (host/guest boundary)

```
Guest code (WASM)                           Host (Tokio + Wasmtime)
─────────────────                          ────────────────────────
RpcSession::connect()                      ProcBuilder::with_data_streams()
  create_connection()  <WIT>              create duplex (host/guest)
  get_input_stream()   <WIT>              map to AsyncReadStream
  get_output_stream()  <WIT>              map to AsyncWriteStream
  StreamReader/Writer                      host_stream split
        |                                        |
        v                                        v
  VatNetwork + RpcSystem                VatNetwork + RpcSystem
        |                                        |
        +------------- Cap'n Proto RPC ----------+
```

## Cell I/O semantics

All cell types get `with_data_streams()` + membrane RPC. The WIT membrane
channel is universal. What differs is the stdin/stdout semantics:

| Cell type | stdin carries | stdout carries |
|-----------|--------------|----------------|
| **Raw** (`Cell::raw`) | Wire protocol bytes (libp2p stream) | Wire protocol bytes |
| **HTTP/WAGI** (`Cell::http`) | CGI request body (RFC 3875) | CGI response |
| **Cap'n Proto** (`Cell::capnp`) | Shutdown signal only (close = graceful exit) | Unused |
| **pid0** (no cell section) | Host terminal / daemon stdin | Host terminal |

For Cap'n Proto (RPC) cells, stdin is a shutdown signal channel, not a data
transport. The host never writes bytes. It only closes stdin to tell the cell
to drain gracefully (equivalent to Go's `<-chan struct{}`). All RPC I/O goes
through the WIT data_streams side-channel.

`handle_vat_connection` in `src/rpc/vat_listener.rs` demonstrates the pattern:
when a peer disconnects, the host closes stdin to signal the cell to shut down.
Error paths (bootstrap timeout, capability extraction failure) also close stdin
to prevent orphaned cell processes.

## Executor pool and M:N scheduling

Cell processes run on an `ExecutorPool` of N worker threads (one per CPU core
by default, configurable via `--executor-threads`). Each worker is an OS thread
with a `current_thread` tokio runtime and a `LocalSet`, because `wasmtime::Store`
is `!Send`.

Cells are assigned to the least-loaded worker at spawn time (with round-robin
fallback for ties). Multiple cells on the same worker cooperatively share the
thread via the EWMA fuel scheduler: cells yield every ~10K instructions at
wasmtime host call boundaries, giving sibling cells a chance to run.
See [fuel-scheduling.md](designs/fuel-scheduling.md) for the full design.

Reference: `src/runtime.rs`

## Notes and implications

- The transport is **async** on the host and **poll-driven** on the guest
  with explicit blocking on WASI pollables.
- RPC messages traverse memory-only duplex streams; there is no OS socket I/O.
- Backpressure is mediated by the WASI output stream budget (`check_write`)
  and the duplex buffer size (`PIPE_BUFFER_SIZE`, currently 64 KiB).
- `blocking_read()` inside a `system::run` closure is safe: wasmtime's async
  machinery suspends the entire `call_async` future while waiting, yielding
  the tokio thread. The poll loop resumes exactly where it left off.

## Deadlock analysis and mitigations

The guest can deadlock if it blocks without a peer making progress, or if
both ends wait for each other without driving their RPC systems. The
transport itself is cooperative: it requires one side to be actively polled
to move data.

### Deadlock causes

1) **Host stops driving the child RPC system.**
   In `ExecutorImpl::spawn()`, the host spawns a local task to run the child's `RpcSystem`.
   If that task is never scheduled (or exits early), the guest will block in
   `wasi_poll::poll` waiting for read/write readiness that never comes.

2) **Guest waits on RPC promises without driving the poll loop.**
   Cap'n Proto RPC futures only make progress when `rpc_system.poll`
   is driven. If user code blocks or returns without continuing the
   `poll_loop`, responses will never be delivered.

3) **Missing flush poll (the double-poll requirement).**
   If the poll loop only calls `poll_rpc` once (before `poll_future`), RPC
   calls queued during `future.poll` are never flushed to the wire before
   `wasi_poll` blocks. The host never sees the request, the guest never
   gets a response.

4) **Backpressure deadlock: both sides waiting on writable/readable state.**
   If the guest's writer is not ready and the host's reader isn't draining
   (or vice versa), both sides can become stuck waiting for readiness.
   The guest-side `StreamWriter` reports readiness via `check_write`; if
   it repeatedly returns 0, the driver waits for readiness with
   `wasi_poll::poll`.

5) **Application-level wait cycles.**
   A guest can block awaiting a host response that itself depends on a
   guest callback or further guest progress (capability-based cycles).
   This shows up when the guest stops polling while the host expects a
   follow-up message from the same guest.

6) **Client-side dial: awaiting a derived promise before spawning the
   RpcSystem.** This is a client-side analog of cause (1), specific to
   code that *dials* a remote vat (e.g., `ww shell` against a daemon,
   or `VatClient::dial()` for outgoing guest dials). The buggy shape:

   ```rust
   // BUG: when_resolved() / .send().promise / etc. registers a waker
   //      on RpcSystem-internal state that never advances because no
   //      one is polling the system. Deadlock until the timeout fires.
   let mut rpc_system = RpcSystem::new(network, None);
   let client = rpc_system.bootstrap(Side::Server);
   client.when_resolved().await?;                  // <-- hangs forever
   tokio::task::spawn_local(rpc_system);           // never reached
   ```

   A second, capnp-rpc-rust-internal quirk compounds the first:
   `when_resolved()` on a fresh `PromiseClient` does not reliably
   fire even with correct ordering in capnp-rpc-rust 0.25
   (`when_more_resolved` keeps appending waiters to an already-drained
   queue after `PromiseClient::resolve`). The canonical
   [capnproto-rust hello-world client] sidesteps this entirely by going
   straight to method calls — the response promise IS the handshake
   observable.

   Surfaced as #450, which manifested as a 30s `ww shell` "RPC handshake
   timeout" on every connect.

   [capnproto-rust hello-world client]: https://github.com/capnproto/capnproto-rust/blob/master/capnp-rpc/examples/hello-world/client.rs

### Mitigations

1) **Ensure both RPC systems are continuously driven.**
   Host: keep the child's `RpcSystem` alive for the lifetime of the guest
   process. Guest: keep the poll loop running whenever promises are
   outstanding.

2) **Use the double-poll pattern.**
   Always call `poll_rpc` both before and after `poll_work` in the
   poll loop. This is the single most common deadlock fix.

3) **Use timeouts on long-lived RPC calls.**
   On the guest, wrap poll-driven workflows with timeouts and error
   if no progress is made for a window. On the host,
   bound guest process execution (`tokio::time::timeout`).

4) **Add explicit yield points in agents.**
   If an agent performs long CPU work, ensure it periodically drives the
   RPC system and yields to pollables. Without this, inbound RPC traffic
   will stall.

5) **Tune buffer size.**
   Increase `PIPE_BUFFER_SIZE` or chunk writes to reduce long stalls when
   one side is briefly slow to drain.

6) **Observability.**
   Keep trace logging enabled during development to detect stalled states
   and verify that the RPC system is being polled.

7) **Use the `vat_dial` paved-path helper for client-side dials.**
   For any code that dials a remote vat (host-side CLI, the
   `VatClient::dial()` capability impl, future internal callers), use
   [`crates/rpc/src/vat_dial.rs::connect`] instead of building the
   `RpcSystem` + `VatNetwork` + `bootstrap()` chain by hand.  The helper
   spawns the driver *before* returning the typed bootstrap client, so
   callers structurally cannot reproduce cause (6).  Returns
   `VatDial<C>` carrying the typed cap plus a `JoinHandle` for the
   detached driver; the driver flushes the Bootstrap message and
   receives the remote Return on its own, with no handshake-check
   `await` needed.  Regression tests live alongside the helper.

   **Trade-off** (accepted, documented in the helper's module docs):
   `vat_dial::connect` does not synchronously verify the remote
   responded to Bootstrap before returning the cap.  In the rare case
   that the peer accepts the libp2p subprotocol stream but doesn't
   actually speak Cap'n Proto, the failure surfaces on the caller's
   first method call via that call's own timeout (e.g. `eval timeout
   (30s)` in `ww shell`) rather than as a distinct "handshake timeout"
   at connect.  Time-to-failure is unchanged (~30s either way);
   diagnostic precision is slightly reduced.  Acceptable because libp2p
   subprotocol negotiation already established the peer claims to speak
   our exact capnp interface, the canonical capnproto-rust pattern
   operates the same way, and the alternative (`when_resolved` await)
   was empirically broken in capnp-rpc-rust 0.25.  In every other
   scenario the new behaviour is a strict improvement: no 30s connect
   penalty for dials that never make a method call, no synchronous wait
   for cold-cache WASM compile on the other side, etc.

   [`crates/rpc/src/vat_dial.rs::connect`]: ../crates/rpc/src/vat_dial.rs

## Open questions

- **Duplex buffer size.** The current `PIPE_BUFFER_SIZE` (64 KiB) was
  chosen pragmatically. Profiling under realistic RPC payloads may suggest
  a different value.
- **WASI io/streams linkage.** The stream setup currently lives in `Proc`.
  It may be worth lifting to a shared helper if additional agent types
  (e.g. `crates/proc/` stream handlers) need the same plumbing.
