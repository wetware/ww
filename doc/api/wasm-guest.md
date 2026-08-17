# WASM Guest API Reference

This document specifies the host-guest interface for Wetware WASM components.
A guest is a WASI P2 component (`wasm32-wasip2`) that communicates with
the host via two channels: standard WASI interfaces and a custom bidirectional
stream used for Cap'n Proto RPC.

## Component Model

Guests are WASI CLI commands. The host instantiates the guest as a
`wasi:cli/command` component and calls `wasi:cli/run#run` to start it.

**Target triple**: `wasm32-wasip2`

**Required export**:

| Export | Signature | Description |
|--------|-----------|-------------|
| `wasi:cli/run#run` | `() -> result` | Entry point. Called by host to start the guest. |

## WASI Host Functions

Standard WASI P2 interfaces provided by the host. Implemented via
`wasmtime_wasi::p2::add_to_linker_async`.

### wasi:io/streams@0.2.9

| Resource | Method | Signature | Description |
|----------|--------|-----------|-------------|
| `input-stream` | `read` | `(len: u64) -> result<list<u8>, stream-error>` | Non-blocking read up to `len` bytes. Empty list = no data yet. |
| `input-stream` | `blocking-read` | `(len: u64) -> result<list<u8>, stream-error>` | Blocking read up to `len` bytes. |
| `input-stream` | `skip` | `(len: u64) -> result<u64, stream-error>` | Skip up to `len` bytes, return count skipped. |
| `input-stream` | `blocking-skip` | `(len: u64) -> result<u64, stream-error>` | Blocking skip. |
| `input-stream` | `subscribe` | `() -> pollable` | Get pollable for read readiness. |
| `output-stream` | `check-write` | `() -> result<u64, stream-error>` | Return max bytes the next `write` may accept. Never blocks. |
| `output-stream` | `write` | `(contents: list<u8>) -> result<_, stream-error>` | Non-blocking write. Precondition: `len(contents) <= check-write()`. Traps otherwise. |
| `output-stream` | `blocking-write-and-flush` | `(contents: list<u8>) -> result<_, stream-error>` | Write up to 4096 bytes and flush. Blocks until complete. |
| `output-stream` | `flush` | `() -> result<_, stream-error>` | Request flush of buffered output. Non-blocking. |
| `output-stream` | `blocking-flush` | `() -> result<_, stream-error>` | Flush and block until complete. |
| `output-stream` | `subscribe` | `() -> pollable` | Get pollable for write readiness. |
| `output-stream` | `write-zeroes` | `(len: u64) -> result<_, stream-error>` | Write `len` zero bytes. Same preconditions as `write`. |
| `output-stream` | `splice` | `(src: borrow<input-stream>, len: u64) -> result<u64, stream-error>` | Pipe from input to this output. |

### wasi:io/poll@0.2.9

| Function | Signature | Description |
|----------|-----------|-------------|
| `poll` | `(in: list<borrow<pollable>>) -> list<u32>` | Wait until one or more pollables are ready. Returns indices of ready items. Traps if list is empty. |

| Resource | Method | Signature | Description |
|----------|--------|-----------|-------------|
| `pollable` | `ready` | `() -> bool` | Check readiness without blocking. |
| `pollable` | `block` | `()` | Block until ready. |

### wasi:io/error@0.2.9

| Resource | Method | Signature | Description |
|----------|--------|-----------|-------------|
| `error` | `to-debug-string` | `() -> string` | Human-readable error description. Not for machine parsing. |

### wasi:cli/stdin, wasi:cli/stdout, wasi:cli/stderr

| Function | Signature | Description |
|----------|-----------|-------------|
| `get-stdin` | `() -> input-stream` | Guest's standard input. Connected to host-provided pipe. |
| `get-stdout` | `() -> output-stream` | Guest's standard output. Connected to host-provided pipe. |
| `get-stderr` | `() -> output-stream` | Guest's standard error. Connected to host-provided pipe. |

**Stdio behavior**: The host provides explicit async pipes for each stream.
In byte-stream mode (`StreamListener`/`StreamDialer`), stdin/stdout are wired to the
libp2p stream. In RPC mode (`VatListener`/`VatClient`), stdin/stdout are
used for direct RPC bootstrapping via `serve_stdio()`. Stderr is always
available for logging.

### wasi:clocks/monotonic-clock

| Function | Signature | Description |
|----------|-----------|-------------|
| `subscribe-duration` | `(ns: u64) -> pollable` | Create a pollable that resolves after `ns` nanoseconds. Used for idle poll timeouts. |

### wasi:filesystem/types (conditional)

Filesystem access is **read-only** and only available when an image root
is mounted. The host preopens the merged FHS image directory at `/` with
`DirPerms::READ` and `FilePerms::READ`.

When IPFS caching is active, filesystem operations are intercepted by
`fs_intercept` to resolve content from IPFS transparently.

**Constraint**: Guests cannot write to the filesystem. All writes must go
through capabilities (IPFS, ByteStream, etc.).

## Custom Interfaces

### wetware:streams/streams@0.1.0

Bidirectional data channel between host and guest, used as the transport
layer for Cap'n Proto RPC (pid0 Membrane or ordinary-child InitialGrants).

| Function | Signature | Description |
|----------|-----------|-------------|
| `create-connection` | `() -> connection` | Create a bidirectional stream pair. Can only be called **once** per process. |

| Resource | Method | Signature | Description |
|----------|--------|-----------|-------------|
| `connection` | `get-input-stream` | `() -> input-stream` | Get the read half. Can only be called **once**. |
| `connection` | `get-output-stream` | `() -> output-stream` | Get the write half. Can only be called **once**. |

**Transport**: Backed by a `tokio::io::DuplexStream` (64 KiB buffer).
The host holds the other end and runs Cap'n Proto RPC over it.

**Constraint**: Both `create-connection` and the `get-*-stream` methods
are one-shot. Second calls return an error. This enforces single-owner
semantics on the RPC channel.

**Availability**: Only present when the host enables data streams
(`Builder::with_data_streams()`). Guests spawned without data streams
(e.g., byte-pump handlers) will get an error on `create-connection`.

### wetware:kernel-runtime/readiness@1.0.0 (private PID0 ABI)

This interface is installed only when the host instantiates the trusted PID0
kernel. It is deliberately absent from ordinary-cell linkers.

| Function | Signature | Description |
|----------|-----------|-------------|
| `kernel-ready` (`kernel_ready()` in generated Rust) | `() -> result<_, stale-generation>` | Commit the generation bound by PID0's process-local graft. The guest supplies no generation or token. |

This host function is not a Cap'n Proto capability. It cannot appear in a
`Membrane` graft or `InitialGrants`, be delegated to a child, or cross a
network connection. A stale-generation result makes the PID0 initialization
fail. The Host owns termination and replacement.

## Cap'n Proto RPC (over wetware:streams)

Once the guest obtains input/output streams from `wetware:streams`,
it bootstraps a Cap'n Proto RPC session over them. The host serves the full
**Membrane** only to trusted pid0. Ordinary children receive the distinct
**InitialGrants** closed-delivery capability.

### Connection Setup

1. Guest calls `create-connection()` → gets `connection` resource
2. Guest calls `connection.get-input-stream()` and `connection.get-output-stream()`
3. Guest creates `VatNetwork::new(reader, writer, Side::Client, ...)`
4. Guest creates `RpcSystem::new(network, bootstrap_export)`
5. Guest bootstraps the host-provided capability:
   `rpc_system.bootstrap(Side::Server)` → `Membrane` for pid0 or
   `InitialGrants` for an ordinary child
6. Guest optionally exports its own bootstrap cap (for `system::serve()`)

### Guest Entry Points

The `system` crate (`std/system`) provides two entry points that handle
all connection setup automatically:

| Function | Signature | Description |
|----------|-----------|-------------|
| `system::run` | `(f: FnOnce(C) -> Future) -> ()` | Bootstrap host cap, run closure, drive RPC. |
| `system::serve` | `(bootstrap: Client, f: FnOnce(C) -> Future) -> ()` | Same as `run`, but also exports `bootstrap` to host. |
| `system::serve_stdio` | `(bootstrap: Client) -> ()` | Export cap over stdin/stdout (no host bootstrap). For byte-stream handlers. |

### Child Initial Grants

An ordinary child calls `initial_grants.get()` to obtain exactly the immutable
named capabilities delegated at spawn:

| Capability | Interface | Description |
|------------|-----------|-------------|
| Host | `system_capnp::host` | Node identity, network interfaces. |
| Runtime | `system_capnp::runtime` | Load WASM binaries and obtain Executors. |
| Routing | `routing_capnp::routing` | DHT operations (provide/find_providers). |
| Identity | `auth_capnp::identity` | Host-side signing, only when explicitly delegated. |

IPFS content is not an RPC capability. If the host explicitly installs the
known-CID cache substrate, guests may read `/ipfs/<cid>/...` for CIDs they
already know. Without that execution-context wiring the path does not
materialize. The substrate provides no enumeration, mutation, pin management,
publishing, routing, dialing, or ambient network API, though reads can consume
node network, disk, cache, and eviction resources.

Host-derived grants retain their **epoch guards** and become invalid when the
host advances its epoch. Repeated `InitialGrants.get()` calls return the same
recorded references; fresh authority requires explicit ancestor re-delegation
or child respawn. Non-host grants keep their own normal lifetime semantics.

## Cap'n Proto RPC (system.capnp)

Full interface reference for the capabilities available to guests.

### Host

| Method | Signature | Description |
|--------|-----------|-------------|
| `id` | `() -> (peerId: Data)` | This node's libp2p peer ID. |
| `addrs` | `() -> (addrs: List(Data))` | Multiaddrs this node listens on. |
| `peers` | `() -> (peers: List(PeerInfo))` | Currently connected peers. |
| `network` | `() -> (streamListener, streamDialer, vatListener, vatClient, httpListener)` | Get network interfaces (byte-stream + RPC + HTTP modes). |

### Runtime

| Method | Signature | Description |
|--------|-----------|-------------|
| `load` | `(wasm: Data) -> (executor: Executor)` | Compile (or cache-hit) WASM bytes and return an Executor bound to that binary. |
| `shutdown` | `() -> ()` | Terminate all tasks spawned through this Runtime. |

### Executor

| Method | Signature | Description |
|--------|-----------|-------------|
| `spawn` | `(args: List(Text), env: List(Text), caps: List(Export), fuelPolicy: FuelPolicy) -> (process: Process)` | Spawn a new instance of the bound WASM binary with args, env, explicit initial grants, and fuel policy. |

### Process

| Method | Signature | Description |
|--------|-----------|-------------|
| `stdin` | `() -> (stream: ByteStream)` | Writable stream to guest's stdin. |
| `stdout` | `() -> (stream: ByteStream)` | Readable stream from guest's stdout. |
| `stderr` | `() -> (stream: ByteStream)` | Readable stream from guest's stderr. |
| `wait` | `() -> (exitCode: Int32)` | Block until process exits. |
| `bootstrap` | `() -> (cap: Capability)` | Get the capability exported by the guest via `system::serve()`. |

### ByteStream

| Method | Signature | Description |
|--------|-----------|-------------|
| `read` | `(maxBytes: UInt32) -> (data: Data)` | Read up to `maxBytes`. Empty data = EOF. |
| `write` | `(data: Data) -> ()` | Write data to stream. |
| `close` | `() -> ()` | Close stream. Further reads return EOF, writes fail. |

### StreamListener (byte-stream mode)

| Method | Signature | Description |
|--------|-----------|-------------|
| `listen` | `(executor: Executor, protocol: Text, caps: List(Export)) -> ()` | Accept streams on `/ww/0.1.0/stream/{protocol}`. Per-stream: spawn handler via Executor, wire stdin/stdout, and forward optional caps. |

### StreamDialer (byte-stream mode)

| Method | Signature | Description |
|--------|-----------|-------------|
| `dial` | `(peer: Data, protocol: Text) -> (stream: ByteStream)` | Open stream to peer on `/ww/0.1.0/stream/{protocol}`. Returns bidirectional ByteStream. |

### VatListener (capability mode)

| Method | Signature | Description |
|--------|-----------|-------------|
| `serveRaw` | `(cap: Capability, protocol: Text) -> ()` | Accept unauthenticated connections on `/ww/0.1.0/vat/{protocol}` and bootstrap each connection with the provided capability. |
| `serveAuthenticated` | `(cap: Capability, protocol: Text, policy: AuthorityPolicy) -> ()` | Create a fresh `Terminal` for each connection and expose the capability only after login satisfies `policy`. |

### VatClient (capability mode)

| Method | Signature | Description |
|--------|-----------|-------------|
| `dial` | `(peer: Data, protocol: Text) -> (cap: Capability)` | Open connection to peer on `/ww/0.1.0/vat/{protocol}`. Bootstrap RPC and return the remote capability. |

## Service Cell Registration

The host does not inspect WASM custom sections to decide whether a binary is a
raw, HTTP, or vat service cell. Byte adapters receive their routing inputs
explicitly at registration time. Vat publication serves an already-existing
capability; spawn, bootstrap, wrapping, and attenuation happen before
`VatListener.serveRaw()` or `VatListener.serveAuthenticated()`.

## Implementation Constraints

### Single-threaded guest execution

Guests run on a single WASM thread. The `system` crate uses cooperative
polling (`noop_waker` + manual `wasi:io/poll`) instead of a real async
runtime. There is no `tokio` or `async-std` inside the guest.

### Write tracking

The guest tracks whether writes occurred during a poll cycle via a
thread-local `WRITE_OCCURRED` flag. This prevents a deadlock where:
1. RPC system queues a write
2. Guest blocks on reader-only poll
3. Host never receives the write → both sides wait forever

When writes occurred, the guest polls both reader and writer. When idle
(no writes, no progress), it polls reader + a 100ms timeout to handle
missed wakeups from the host's `AsyncReadStream` background task.

### Resource cleanup

Cap'n Proto destructors attempt to close WASI handles that may already
be torn down by the host. The `system` crate calls `std::mem::forget()`
on RPC resources at exit to avoid panics. This is a WASI P2 wart —
revisit when wasmtime stabilizes resource cleanup ordering.

### Epoch guards

Host capabilities grafted by pid0 are wrapped in epoch guards. When the host
advances its epoch (e.g., on-chain state change), delegated copies also become
invalid and calls return `staleEpoch` errors. Ordinary children cannot
re-graft. The Host terminates the old PID0 and starts a fresh PID0 for the new
generation.

### Pipe buffer sizes

| Buffer | Size | Location |
|--------|------|----------|
| stdio (stdout, stderr) | 1024 bytes | `crates/cell/src/proc.rs` (`BUFFER_SIZE`) |
| data stream (RPC transport) | 64 KiB | `crates/cell/src/proc.rs` (`PIPE_BUFFER_SIZE`) |

> **Note:** See the source constants for authoritative values; sizes listed here may lag behind changes.

### Idle poll timeout

100ms (`IDLE_POLL_TIMEOUT_NS`). Created via `wasi:clocks/monotonic-clock.subscribe-duration`.
Fires when no writes occurred and no progress was made, preventing indefinite
blocking on missed wakeups.
