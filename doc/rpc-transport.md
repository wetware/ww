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

The host retains the conceptual session used before the P3 migration. PID0 and
ordinary children both bootstrap a `Membrane`. PID0 receives the broad root
object. Each child receives the object supplied by its parent.

The PID0 bootstrap direction carries host authority into PID0. Production PID0
uses `system::run` and does not export a guest bootstrap capability.

## Guest session

`RpcSession::connect` performs these operations:

1. Open the granted transport once.
2. Adapt its P3 byte streams to `futures::io::AsyncRead` and `AsyncWrite`.
3. Construct a Cap'n Proto `VatNetwork` and real `RpcSystem`.
4. Bootstrap the host-provided `Membrane` capability.
5. Optionally expose one guest bootstrap capability to the host.

The generated P3 task composes `RpcSystem` and transport completion with the
application Future. `TransportError::Failed` fails the root. Orderly transport
closure remains successful. Wasmtime P3 drives all waits. The guest does not
poll `wasi:io/poll`, run a timer-based pump, or own an event loop.

Within one selector observation, transport completion precedes RPC completion,
which precedes application completion. All three futures remain inline. A
private synchronous cleanup owner exists before the application factory runs
and also belongs to direct `RpcSession` values.

Normal completion stops result selection, consumes cleanup ownership once,
terminalizes the RPC connection, releases the local bootstrap, then drops the
application, uncalled application factory, and RPC driver. An error or unwind
also drops the remaining transport future before propagation. Successful
application or RPC completion waits for transport completion after local cleanup.
Cleanup never polls `RpcSystem`, including after a completed or unwound poll.
Dropping an unpolled session performs zero RPC and application polls.

Cleanup rejects retained calls, response pipelines, and promised capabilities
through the Task-3 connection terminalization contract. Each cleanup stage runs
even if another stage reports a native destructor panic. A selected primary
failure or original unwind remains primary; otherwise cleanup failure fails the
session. Repeated cleanup is harmless because each owner is consumed once.

The public `RpcSession.rpc_system` and `RpcSession.client` fields remain
extractable. The remaining session owner still controls their connection and
terminalizes it when that owner leaves scope. Extraction is not a detach API.

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
Process abort therefore discards guest state and drops host-side P3 resources
and transport adapters. Store destruction does not certify execution of guest
Rust destructors. Neither does a Wasm `panic=abort` trap.

Generated P3 subtask cancellation is a separate path: it destroys the guest
session while the Store and sibling tasks can continue. The synchronous session
owner terminalizes RPC state during that destruction. Wetware pins upstream
`wit-bindgen` v0.62.0 plus the cancellation-wake fix at fork revision
`025f95c294373c73e12eaee335577c81477d5a20`. The fix suppresses notifications from
destructor-driven wakes before retiring the cancelled task's wake stream. The
pin contains no later generator changes from upstream main.

Wetware-owned RPC drivers use managed disconnect. Normal completion, transport
failure, explicit shutdown, and owner cancellation explicitly disconnect Cap'n
Proto before abandoning its driver. This releases exported capabilities even
when local code retains imported clients or pending calls. Merely aborting a
bare `RpcSystem` is insufficient for that ownership guarantee. Teardown also
covers cancellation before the driver's first poll and already-disconnected
connections; disconnect requests are idempotent.

After Store teardown, the child lifecycle requests managed shutdown and joins
its host RPC driver. Disconnect releases exported ownership before the
one-second `DISCONNECT_GRACE` bounds flushing the close. The LocalSet remains
driven until tracked lifecycle and RPC workers finish during host shutdown.
PID0 retains its separate deployment-owned lifetime root; its driver also uses
managed teardown.

## Process ownership and cleanup

`Process` is an owning execution capability. Cloning or transmitting it shares
execution ownership. Releasing one reference does not request termination while
another Process owner remains. When the backend loses its final owning
reference, it requests termination. Requests, responses, pipelines, and membrane
wrappers can retain ownership. Holding only stdin, stdout, stderr, or a guest
bootstrap capability does not; retain the Process for continued execution.

Explicit `kill()` requests termination before final ownership loss. Neither its
response nor final reference release acknowledges asynchronous cleanup. Both
use the same idempotent termination control. The backend `OwnedChildLifecycle`
remains the sole teardown owner: it drops the Store and joins auxiliary work
before publishing `Cleaned(exit)`. Lifecycle-owner loss before proven teardown
is `Lost`, an error rather than a fabricated exit code.

`wait()` is repeatable observation while the Process remains usable. Concurrent
waits coexist, cancelling one does not consume the result, and later waits see
the retained terminal result. A dispatched pending wait does not independently
own execution. The small retained cleanup state contains no Store, guest memory,
stdio buffers, Membrane, RPC task, or Process façade.

Detected RPC disconnect releases ownership exported through that connection;
failure detection is not instantaneous. Application-created capability cycles
can extend execution lifetime. Runtime-created transport cycles have a guaranteed
managed-disconnect operation that releases their exports. For example, a caller
may own a Process whose child imports that caller's Membrane; disconnect cuts the
transport cycle without weakening ordinary capability composition.

This contract supplies no stable process identity, persistence, reconnection,
recovery, detach, or daemon API. A future daemon design would require independent
backend ownership and application responsibility for its lifetime. Fuel bounds
computation, not elapsed retention: blocked async work can remain alive without
consuming guest fuel. Forced wall-clock retirement is separate future work.

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
typed `peerId`, `stat`, `network`, `routing`, `runtime`, `authority`,
and `identity` fields. `extras` contains only application-defined named
capabilities.

Graft-issued host capabilities retain PID0's `EpochGuard`.

## Ordinary-child bootstrap

The host forwards the `Membrane` supplied to `Executor.spawn()` unchanged as
the ordinary child's bootstrap. Stream and HTTP listener registrations also
forward one registration-time `Membrane` to each spawned child. Repeated graft
calls cannot recover fields omitted by that Membrane implementation. A child
receives fresh host authority only through explicit ancestor delegation or a
new process.

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

`StreamListener.listen` spawns one process per inbound stream, forwards the
registration-time `Membrane`, and pumps bytes through process stdin and stdout.
`StreamDialer.dial` returns a bidirectional `ByteStream` capability.

Each accepted stream has one supervisor. The supervisor retains an owning
Process reference, a replayable wait observation, both transport directions,
one cancellation token, and one connection permit. Clean peer input EOF closes
child stdin only. Child stdout and the network output direction remain usable,
so a response produced after input EOF can still reach the peer.

The supervisor starts one non-renewable 30-second completion deadline when it
first observes peer input EOF or child completion with unfinished output. The
deadline covers stdin EOF propagation, remaining handler work, stdout drain,
and network write, flush, and close. It never restarts between phases. Normal
completion observes child exit and final output EOF. Expiry, cancellation, or
transport/executor failure stops normal gateway work, makes a bounded
best-effort kill request when useful, and drops Process and ownership-bearing
requests, responses, and pipelines.

`ConnectionPermit` accounts only for gateway-owned accepted connection state.
It returns once that local state and transport end, without requiring a remote
cleanup acknowledgement. Backend lifecycle cleanup proceeds independently after
termination or final ownership loss. The same algorithm accepts local, wrapped,
forwarded, or remote Executors; a single-child timeout never disconnects a shared
executor connection or kills a sibling.

A final registration epoch check occurs after scheduling and immediately before
`Executor.spawn()` dispatch, with no intervening yield. Stale admission drops
the accepted transport and returns its permit without spawning. A later epoch
transition does not retroactively invalidate an already-dispatched spawn; the
listener's host cancellation path still ends its accepted supervisors.

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
