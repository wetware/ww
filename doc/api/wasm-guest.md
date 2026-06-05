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
layer for Cap'n Proto RPC (Membrane bootstrap).

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

## Cap'n Proto RPC (over wetware:streams)

Once the guest obtains input/output streams from `wetware:streams`,
it bootstraps a Cap'n Proto RPC session over them. The host serves
the **Membrane** as the bootstrap capability.

### Connection Setup

1. Guest calls `create-connection()` → gets `connection` resource
2. Guest calls `connection.get-input-stream()` and `connection.get-output-stream()`
3. Guest creates `VatNetwork::new(reader, writer, Side::Client, ...)`
4. Guest creates `RpcSystem::new(network, bootstrap_export)`
5. Guest bootstraps host capability: `rpc_system.bootstrap(Side::Server)` → `Membrane`
6. Guest optionally exports its own bootstrap cap (for `system::serve()`)

### Guest Entry Points

The `system` crate (`std/system`) provides two entry points that handle
all connection setup automatically:

| Function | Signature | Description |
|----------|-----------|-------------|
| `system::run` | `(f: FnOnce(C) -> Future) -> ()` | Bootstrap host cap, run closure, drive RPC. |
| `system::serve` | `(bootstrap: Client, f: FnOnce(C) -> Future) -> ()` | Same as `run`, but also exports `bootstrap` to host. |
| `system::serve_stdio` | `(bootstrap: Client) -> ()` | Export cap over stdin/stdout (no Membrane). For byte-stream handlers. |

### Membrane Capabilities

After bootstrapping, the guest calls `membrane.graft()` to obtain
session-scoped capabilities:

| Capability | Interface | Description |
|------------|-----------|-------------|
| Host | `system_capnp::host` | Node identity, network interfaces. |
| Runtime | `system_capnp::runtime` | Load WASM binaries and obtain Executors. |
| Routing | `routing_capnp::routing` | DHT operations (provide/find_providers). |
| Identity | `stem_capnp::identity` | Host-side signing (private key never leaves host). |

IPFS content is not a capability; guests read `/ipfs/<cid>/...` through
the WASI virtual filesystem.

All capabilities are **epoch-guarded**: they become invalid when the
host advances its epoch. Calls on stale capabilities return a
`staleEpoch` error.

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
| `spawn` | `(args: List(Text), env: List(Text)) -> (process: Process)` | Spawn a new instance of the bound WASM binary with the given args and env. |

### Process

| Method | Signature | Description |
|--------|-----------|-------------|
| `stdin` | `() -> (stream: ByteStream)` | Writable stream to guest's stdin. |
| `stdout` | `() -> (stream: ByteStream)` | Readable stream from guest's stdout. |
| `stderr` | `() -> (stream: ByteStream)` | Readable stream from guest's stderr. |
| `wait` | `() -> (exitCode: Int32)` | Block until process exits. |
| `bootstrap` | `() -> (cap: AnyPointer)` | Get the capability exported by the guest via `system::serve()`. Type-erased. |

### ByteStream

| Method | Signature | Description |
|--------|-----------|-------------|
| `read` | `(maxBytes: UInt32) -> (data: Data)` | Read up to `maxBytes`. Empty data = EOF. |
| `write` | `(data: Data) -> ()` | Write data to stream. |
| `close` | `() -> ()` | Close stream. Further reads return EOF, writes fail. |

### StreamListener (byte-stream mode)

| Method | Signature | Description |
|--------|-----------|-------------|
| `listen` | `(executor: Executor, protocol: Text) -> ()` | Accept streams on `/ww/0.1.0/stream/{protocol}`. Per-stream: spawn handler via Executor, wire stdin/stdout. |

### StreamDialer (byte-stream mode)

| Method | Signature | Description |
|--------|-----------|-------------|
| `dial` | `(peer: Data, protocol: Text) -> (stream: ByteStream)` | Open stream to peer on `/ww/0.1.0/stream/{protocol}`. Returns bidirectional ByteStream. |

### VatListener (capability mode)

| Method | Signature | Description |
|--------|-----------|-------------|
| `listen` | `(executor: Executor, protocol: Text, caps: List(Export)) -> ()` | Accept connections on `/ww/0.1.0/vat/{protocol}`. `protocol` is a caller-chosen service name/locator, not type authority. The host derives the schema from the same host-minted `Runtime.load` executor that will spawn the vat cell. |
| `serve` | `(cap: AnyPointer, protocol: Text) -> ()` | Publish an already-existing capability on `/ww/0.1.0/vat/{protocol}`. No schema bytes are accepted; the schema comes from the publishing WASM artifact's `ww.schema.v1` section. |

### VatClient (capability mode)

| Method | Signature | Description |
|--------|-----------|-------------|
| `dial` | `(peer: Data, protocol: Text) -> (connection: VatConnection)` | Open connection to peer on `/ww/0.1.0/vat/{protocol}`. Use `connection.describe()` to inspect the declared schema without spawning, then `connection.bind()` to lazily obtain the exported app capability. |

### VatConnection (capability mode)

| Method | Signature | Description |
|--------|-----------|-------------|
| `describe` | `() -> (schemaBundle: SchemaBundle)` | Return the typed `SchemaBundle` without spawning a cell. |
| `bind` | `() -> (schemaBundle: SchemaBundle, cap: AnyPointer)` | Spawn/attach once for executor-bound services, or return the persistent published cap for `serve()` services. Repeated calls on the same connection return the same schema and cap. |

## Service Cell Registration

The host does not inspect WASM custom sections to decide whether a binary is a
raw, HTTP, or vat service cell. Listener capabilities receive their routing
inputs explicitly at registration time.

Vat cells must embed canonical `SchemaBundle` bytes in the `ww.schema.v1` WASM
custom section. `Runtime.load` records schema metadata but remains transport
neutral; `VatListener.listen` is where missing or invalid vat schema metadata
fails clearly.

`VatListener.serve` uses the same metadata requirement for the caller artifact:
the publishing process must have been spawned from a WASM artifact with a valid
`ww.schema.v1` section. This keeps persistent-cap publication from reintroducing
caller-supplied schema authority.

Glia registration forms:

```clojure
(perform host :listen :vat "greeter" (cell (load "bin/greeter.wasm")))
(perform host :listen :http "/status" (cell (load "bin/status.wasm")))
(perform host :listen :raw "echo" runtime (load "bin/echo.wasm"))
```

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

All Membrane-provided capabilities are wrapped in epoch guards. When
the host advances its epoch (e.g., on-chain state change), all outstanding
capabilities become invalid. Calls return `staleEpoch` errors. Guests
must re-graft to obtain fresh capabilities.

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
