# RPC Transport

Wetware uses Cap'n Proto RPC over two distinct byte transports:

- Each Cell receives one authority-granted in-memory host connection.
- Published vat services use named libp2p streams.

Raw byte-stream services also use named libp2p streams. They do not create a
Cap'n Proto vat unless the application implements that protocol.

Primary code references:

- `crates/cell/src/p3.rs` implements the Cell transport.
- `crates/cell/src/proc.rs` places the transport grant in one Cell Store.
- `src/kernel.rs` drives the PID0 host-side `RpcSystem`.
- `src/launcher.rs` drives ordinary-child host-side `RpcSystem` instances.
- `std/system/src/lib.rs` constructs the guest-side session.
- `crates/rpc/src/graft.rs` defines PID0 and child bootstraps.
- `crates/rpc/src/vat_listener.rs` and `vat_client.rs` implement network vats.
- `crates/rpc/src/stream_listener.rs` and `stream_dialer.rs` implement byte streams.

## Process-local P3 connection

`Builder` creates two bounded 64 KiB directions. The host receives a
`HostTransport`. The Store receives a one-shot `GrantedTransport` through
`wetware:transport/connection@0.2.0`.

```text
host RpcSystem                              guest RpcSystem
      |                                          |
HostTransport <--- two bounded directions ---> P3 stream resources
                    64 KiB each                  |
                                       wetware:transport connection
```

The grant authorizes exactly one host-to-Cell byte connection. The interface
contains no address, dial operation, socket operation, scheduler state,
pollable, or explicit flush function. A second `open` call returns the fixed
`connection already opened` failure.

The host retains the conceptual session used before the P3 migration. PID0's
host side serves a `Membrane`. An ordinary child's host side serves the
immutable `InitialGrants` record selected by its parent.

The PID0 bootstrap direction carries host authority into PID0. Production PID0
uses `system::run` and does not export a guest bootstrap capability.

## Guest session

`RpcSession::connect` performs these operations:

1. Open the granted transport once.
2. Adapt its P3 byte streams to `futures::io::AsyncRead` and `AsyncWrite`.
3. Construct a Cap'n Proto `VatNetwork` and real `RpcSystem`.
4. Bootstrap the host `Membrane` or `InitialGrants` capability.
5. Optionally expose one guest bootstrap capability to the host.

The generated P3 task composes `RpcSystem` and transport completion with the
application Future. `TransportError::Failed` fails the root. Orderly transport
closure remains successful. Wasmtime P3 drives all waits. The guest does not
poll `wasi:io/poll`, run a timer-based pump, or own an event loop.

`Process.bootstrap()` returns the guest capability supplied to `system::serve`.
That capability differs from the host bootstrap received by the guest.

## Backpressure and completed writes

Each direction has an independent 64 KiB Tokio duplex buffer. The adapter adds
no payload queue. A write that exceeds available capacity remains pending until
the host consumes bytes.

The host output consumer reports a guest write as complete only after two
events occur:

1. The underlying host `AsyncWrite` accepts all bytes for that P3 operation.
2. `poll_flush` returns success.

This rule protects a final completed message when the application Future
finishes immediately after the write. The root Future does not add an RPC drain
delay.

## Cancellation during a pending flush

Wetware uses fail-on-cancel semantics for an accepted write whose host flush is
pending. Cancelling that P3 operation fails the connection with the fixed
`transport flush cancelled` diagnostic.

The adapter does not claim that the accepted bytes reached a downstream peer.
The connection cannot continue after an operation loses ownership of a pending
flush. Store teardown then drops both adapters and closes the host transport.
This rule avoids both silent success and a detached flush task.

## Directional close and failure

| Event | Other direction | Connection completion |
|---|---|---|
| Guest drops outgoing | Host reads EOF | Waits for incoming direction |
| Host closes its writer | Guest reads EOF | Waits for outgoing direction |
| Guest drops incoming | Host writes fail locally | Waits for outgoing, then `Ok` |
| Both directions close normally | Closed | `Ok` |
| Read, write, or flush fails | Terminates | `failed(string)` |
| Pending flush is cancelled | Terminates | `failed("transport flush cancelled")` |

Diagnostics are fixed strings. They do not contain host paths, addresses,
implementation type names, secrets, or debug output.

## Store and RPC cancellation

Wasmtime 48 hard-cancels a concurrent component task when its owning Store is
dropped. `Proc` owns the Store and the `Store::run_concurrent` Future together.
Process abort therefore drops the generated root Future, guest `RpcSystem`,
pending request Futures, P3 resources, and both transport adapters.

Wetware does not use Cap'n Proto `Disconnector` as a graceful-shutdown driver.
Structured drop is the process shutdown policy. The host-side driver remains
owned by the process lifecycle and closes when that lifecycle ends.

After Store teardown, Wetware gives the child host `RpcSystem` a one-second
`RPC_EOF_GRACE` to observe transport EOF. If the `RpcSystem` does not finish in
that interval, the process lifecycle aborts it as a malformed-peer fallback.

## WASI authority

The production linker registers only the P3 packages used by first-party
components: CLI, clocks, filesystem, random, and the Wetware imports.
Production does not register `wasi:sockets`. The granted transport remains the
only guest RPC or network path.

Linker registration and resource grant are separate controls. All Cells have
explicit stdin, stdout, and stderr resources. PID0 can receive terminal-backed
stdin. Filesystem preopens expose an immutable image root and a private
writable `/tmp`. PID0 alone receives the private readiness import.

Build validation prints every component's WIT, rejects WASI 0.2 imports, and
rejects `wasi:sockets` imports.

## PID0 bootstrap

The host serves a process-local `Membrane` to PID0. `Membrane.graft()` returns
the canonical exports available for the current generation:

- `identity`, when a signing key is configured;
- `host`;
- `runtime`;
- `routing-finder`;
- `routing-announcer`;
- `authority`;
- `ipfs`;
- `http-client`, when an outbound HTTP allowlist is configured.

Graft-issued host capabilities retain PID0's `EpochGuard`.

## Ordinary-child bootstrap

The host serves `InitialGrants` to an ordinary child. `InitialGrants.get()`
returns exactly the immutable `List(Export)` selected by the parent. The host
does not add PID0 exports to this record.

An ordinary child cannot call `Membrane.graft()`. A child receives fresh host
authority only through explicit ancestor delegation or a new process.

## Network service boundary

Wetware publishes services only below these protocol prefixes:

| Prefix | Payload | Host capabilities |
|---|---|---|
| `/ww/0.1.0/vat/{protocol}` | Cap'n Proto RPC | `VatListener`, `VatClient` |
| `/ww/0.1.0/stream/{protocol}` | Application bytes | `StreamListener`, `StreamDialer` |

Protocol names locate streams. They do not grant authority or identify a
schema. Constructors reject empty names and names that contain `/`.

`VatListener` publishes an existing capability. `serveRaw` is the explicit
unauthenticated path. `serveAuthenticated` creates a fresh single-use
`Terminal` for each connection and releases policy-selected authority after
login. Epoch expiry stops both accept loops.

`VatClient.dial` starts its client-side `RpcSystem` before a typed method waits
for a result. The first typed response reports bootstrap or transport failure.

`StreamListener.listen` spawns one process per inbound stream, supplies the
registration-time initial grants, and pumps bytes through process stdin and
stdout. `StreamDialer.dial` returns a bidirectional `ByteStream` capability.

`HttpListener.listen` creates an HTTP route, spawns one process per request,
supplies CGI environment variables and request bytes, and reads the CGI
response from stdout. The host never selects these modes from a WASM custom
section.

## Forward-progress constraints

A live async Cell can serve Cap'n Proto requests while its application Future
waits on supported P3 operations. Component Model P3 and the generated bindings
drive suspension and resumption.

The host must continue to drive the process-local `RpcSystem`. The guest root
Future must retain its `RpcSystem`. Network vat clients must start their driver
before awaiting derived promises. Application protocols must avoid call cycles
where both peers await callbacks that neither side can poll.

Non-yielding CPU work and synchronous blocking imports stop same-Cell async
progress. Host fuel and epoch interruption still control Cell teardown.
