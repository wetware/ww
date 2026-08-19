# RPC Transport: Host Channels and Network Services

Wetware uses Cap'n Proto RPC in two distinct places:

- Each WASM process has an in-memory host channel for its bootstrap and
  delegated capabilities.
- Published vat services run Cap'n Proto RPC over named libp2p streams.

Raw byte-stream services also use named libp2p streams, but they do not add a
Cap'n Proto vat unless the application implements one over stdin/stdout.

Primary implementation references:

- `crates/cell/src/proc.rs` — in-memory host/guest stream creation
- `src/kernel.rs` — trusted kernel startup and host-side RPC driver
- `src/launcher.rs` — ordinary-child startup and host-side RPC driver
- `crates/rpc/src/graft.rs` — PID0 and ordinary-child bootstraps
- `std/system/src/lib.rs` — guest-side RPC session and poll loop
- `crates/rpc/src/vat_listener.rs` and `crates/rpc/src/vat_client.rs` — network vats
- `crates/rpc/src/stream_listener.rs` and `crates/rpc/src/stream_dialer.rs` — byte streams
- `capnp/membrane.capnp` and `capnp/system.capnp` — public capability interfaces

## Process-local host channel

Every PID0 or ordinary-child process starts with a bidirectional in-memory
stream created by `cell::proc::Builder::with_data_streams()`. The host retains
one end. The guest receives the other end through
`wetware:streams/streams@0.1.0`.

```text
Host                                      WASM guest
----                                      ----------
tokio::io::DuplexStream                   wetware:streams connection
        |                                 WASI input/output streams
        v                                         |
VatNetwork + RpcSystem <--- Cap'n Proto RPC ----> VatNetwork + RpcSystem
```

The stream has no libp2p or OS-socket hop. `crates/cell/src/proc.rs` exposes
the guest end as WASI stream resources and returns the host end through
`DataStreamHandles`.

### PID0 bootstrap

`kernel::Generation` passes the host stream halves to
`build_kernel_membrane_rpc()`. The host serves a process-local `Membrane` as
the bootstrap capability.

PID0 calls `Membrane.graft()`, which returns `List(Export)`. The canonical
exports are:

- `identity`, when a signing key is configured;
- `host`;
- `runtime`;
- `routing`;
- `authority`;
- `ipfs`;
- `http-client`, when an outbound HTTP allowlist is configured.

The host can also append explicitly configured extra exports. Graft-issued
host capabilities use the PID0 generation's epoch guard.

### Ordinary-child bootstrap

`src/launcher.rs` passes the host stream halves and the child's
`InitialAuthorityRecord` to `build_initial_authority_rpc()`. The host serves
`InitialGrants`, whose `get()` method returns exactly the immutable
parent-selected `List(Export)`.

An ordinary child does not receive `Membrane.graft()`. The host does not add
PID0 exports to the child's record.

### Guest setup

`RpcSession::connect()` in `std/system/src/lib.rs` performs the guest setup:

1. Call `create_connection()` once.
2. Take the connection's input and output streams once.
3. Wrap the streams in `StreamReader` and `StreamWriter`.
4. Construct the guest `VatNetwork` and `RpcSystem`.
5. Bootstrap the host-provided `Membrane` or `InitialGrants` capability.

`system::run()` drives the RPC system with an async guest closure.
`system::run_with()` also includes caller-provided `PollSet` entries.
`system::serve()` additionally exports one guest bootstrap capability.
The parent can retrieve that guest export through `Process.bootstrap()`.

`system::serve_stdio()` is separate. It serves a guest capability over WASI
stdin/stdout and does not connect to the process-local host bootstrap.

## Network protocol boundary

Wetware publishes network services only below these protocol prefixes:

| Prefix | Payload | Host capabilities |
|--------|---------|-------------------|
| `/ww/0.1.0/vat/{protocol}` | Cap'n Proto RPC | `VatListener`, `VatClient` |
| `/ww/0.1.0/stream/{protocol}` | Application-defined bytes | `StreamListener`, `StreamDialer` |

The bare `/ww/0.1.0` compatibility publication no longer exists. The
process-local PID0 `Membrane` is not a network bootstrap.

Protocol names are locators. They do not carry authority or schema identity.
The protocol constructors reject empty names and names that contain `/`.

### Vat services

`VatListener` publishes an existing capability. It does not load or spawn a
WASM process.

- `serveRaw(cap, protocol)` accepts a vat stream and exposes `cap` directly.
  This method is the explicit unauthenticated escape hatch.
- `serveAuthenticated(cap, protocol, policy)` creates a fresh, single-use
  `Terminal` for each inbound stream. The `Terminal` releases policy-selected
  authority only after login succeeds within the configured deadline.

Both methods stop their accept loops when their epoch guard becomes stale.
Connection budgets limit concurrent inbound streams.

`VatClient.dial(peer, protocol)` opens the named vat stream and returns the
remote bootstrap capability. `connect()` in `crates/rpc/src/vat_dial.rs`
starts the client-side `RpcSystem` driver before returning the capability. The
caller's first typed method response reports bootstrap or transport failure.

```text
publisher                                         remote peer
---------                                         -----------
existing capability
        |
VatListener.serveAuthenticated()
        |
fresh Terminal <--- /ww/0.1.0/vat/{protocol} ---> VatClient.dial()
```

### Byte-stream services

`StreamListener.listen(executor, protocol, caps)` registers a named byte-stream
handler. For each inbound `/ww/0.1.0/stream/{protocol}` connection, the
listener:

1. spawns one process through the supplied `Executor`;
2. copies the registration-time `List(Export)` into the child's initial grants;
3. pumps the network stream to the child's stdin;
4. pumps the child's stdout to the network stream.

`StreamDialer.dial(peer, protocol)` opens the named stream and returns a
bidirectional `ByteStream` capability. The bytes have application-defined
semantics.

This raw stream path differs from vat publication. `StreamListener` spawns a
process and wires bytes. `VatListener` serves an existing capability through a
Cap'n Proto `RpcSystem`.

### HTTP registration

`HttpListener.listen(executor, prefix, caps)` is another explicit registration
path. It creates an HTTP route, spawns one process per request, supplies CGI
environment variables and request bytes, and reads the CGI response from
stdout. The host does not select HTTP, byte-stream, or vat behavior from a
WASM custom section.

## Guest scheduling

WASM guests use the cooperative `poll_loop` in `std/system/src/lib.rs`. Each
cycle performs these actions:

1. Poll the guest `RpcSystem` for inbound work.
2. Poll the application future.
3. If the cycle wrote RPC bytes, poll the `RpcSystem` again to flush them.
4. Block in `wasi:io/poll` on the RPC reader, optional writer, additional
   `PollSet` entries, or the idle timeout.

The second RPC poll is required after application work queues an outbound
call. Without that poll, the guest can block before it sends the request.

The writer participates in the poll set only after a write attempt. Otherwise,
the loop includes a 100 ms monotonic-clock pollable as protection against a
missed host-stream wakeup.

Host-side `RpcSystem` instances run as local Tokio tasks. `kernel::Generation`
starts the kernel driver. `ExecutorImpl` starts ordinary-child drivers in
`src/launcher.rs`.

## Executor scheduling

`ExecutorPool` in `src/services.rs` runs worker OS threads. Each worker has a
current-thread Tokio runtime and a `LocalSet` because WASM stores and Cap'n
Proto clients are local tasks. The pool assigns new cells to the least-loaded
worker and uses round-robin assignment for equal loads.

Scheduled cells use Wasmtime fuel for cooperative yielding. See
[fuel-scheduling.md](designs/fuel-scheduling.md) for the scheduling policy.

## Backpressure and lifetime

The process-local duplex stream uses `PIPE_BUFFER_SIZE` from
`crates/cell/src/proc.rs`. Guest writes obey the WASI output stream's
`check_write()` budget. Network byte-stream pumps use bounded chunks and wait
for their readers and writers.

The host keeps each process-local `RpcSystem` driver alive with the process.
Dropping the driver closes the RPC path. Guest code must keep `system::run()`,
`system::run_with()`, or `system::serve()` active while it awaits RPC promises.

`Process.bootstrap()` is parent-held authority exported by a guest through
`system::serve()`. It is distinct from the host-provided `InitialGrants`
bootstrap that the ordinary child receives.

## Deadlock constraints

The following constraints keep RPC progress possible:

- The host must poll the process's `RpcSystem` while the process is active.
- The guest must poll its `RpcSystem` while it awaits derived promises.
- The guest poll loop must flush calls queued by the application future before
  it blocks on WASI pollables.
- A vat dialer must start its `RpcSystem` before awaiting a derived promise.
- Application protocols must avoid call cycles in which both peers wait for
  callbacks that neither peer can poll.

Host-side vat clients use `connect()` in `crates/rpc/src/vat_dial.rs` for the
required driver-before-await ordering. Guest code uses the `system` crate
entry points instead of constructing an undriven `RpcSystem`.
