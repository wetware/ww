# WASM Guest API Reference

This document specifies the host-guest interface for Wetware WASM components.
A first-party guest is a native WASI P3 component (`wasm32-wasip3`). Standard
WASI interfaces provide process I/O. A capability-granted transport provides
the process-local Cap'n Proto connection.

## Component Model

Guests export `wasi:cli/run@0.3.0`. The host instantiates the component and
calls the generated async `run` task through `Store::run_concurrent`.

**Target triple**: `wasm32-wasip3`

**Required export**:

| Export | Signature | Description |
|--------|-----------|-------------|
| `wasi:cli/run@0.3.0#run` | `() -> result` | Entry task. Called by `Proc` to start the guest. |

## WASI Host Functions

The production linker composes these P3 packages explicitly:

| Package | Purpose |
|---|---|
| `wasi:cli@0.3.x` | arguments, environment, exit, stdio, and terminal queries |
| `wasi:clocks@0.3.x` | monotonic and wall clocks plus async waits |
| `wasi:filesystem@0.3.x` | preopens, descriptors, and filesystem types |
| `wasi:random@0.3.x` | secure and insecure random data required by Rust guests |

The production linker does not register `wasi:sockets`. The artifact check
also rejects socket imports. A component receives no ambient dial or listen
authority.

The host supplies explicit stdin, stdout, and stderr streams. PID0 can receive
terminal-backed stdin. Byte and HTTP handlers use stdin and stdout for their
application protocol. The process-local RPC session does not use stdio.

The image and `/ipfs` filesystem is read-only. Each process receives a private
ephemeral writable `/tmp`. The P3 filesystem interceptor preserves CidTree lazy
materialization and path confinement.

## Custom Interfaces

### wetware:transport/connection@0.2.0

This interface grants one ordered bidirectional byte connection for Cap'n
Proto RPC.

| Function | Signature | Description |
|---|---|---|
| `open` | `(outgoing: stream<u8>) -> (incoming: stream<u8>, completion: future<result>)` | Exchange P3 stream resources once. |

The host backs each direction with an independent bounded 64 KiB buffer. Guest
write completion waits for the underlying host flush. Dropping one direction
preserves an orderly half-close. A second `open` reports
`connection already opened`.

The interface contains no address selection or socket operation. Possession of
the grant is the authority to use this one connection.

### wetware:routing/key@0.1.0 (optional)

This pure host import derives the canonical provider-routing CID for caller
supplied bytes.

| Function | Signature | Description |
|----------|-----------|-------------|
| `derive` | `(data: list<u8>) -> string` | Return canonical CIDv1/raw/BLAKE3-256 text. |

The algorithm uses BLAKE3-256, multihash code `0x1e`, and raw codec `0x55`.
The import grants no object-capability authority. A component must declare the
`wetware:routing/key@0.1.0` import to receive its bindings. Components that do
not declare it instantiate normally. The Rust wrapper crate is `routing-key`.

### wetware:kernel-runtime/readiness@1.0.0 (private PID0 ABI)

This interface is installed only when the host instantiates the trusted PID0
kernel. It is deliberately absent from ordinary-cell linkers.

| Function | Signature | Description |
|----------|-----------|-------------|
| `kernel-ready` (`kernel_ready()` in generated Rust) | `() -> result<_, ready-error>` | Commit the generation bound by PID0's process-local graft. `ready-error` currently contains `stale-generation`. The guest supplies no generation or token. |

This host function is not a Cap'n Proto capability. It cannot appear in a
`Membrane` graft or `InitialGrants`, be delegated to a child, or cross a
network connection. A stale-generation result makes the PID0 initialization
fail. The Host owns termination and replacement.

## Cap'n Proto RPC (over wetware:transport)

Once the guest opens `wetware:transport`, it bootstraps a Cap'n Proto RPC
session over the P3 streams. The host serves the full
**Membrane** only to trusted pid0. Ordinary children receive the distinct
**InitialGrants** closed-delivery capability.

### Connection Setup

1. Guest creates the outgoing P3 stream pair.
2. Guest calls `connection.open()` once and receives the incoming stream.
3. Guest adapts both streams for `VatNetwork`.
4. Guest creates `RpcSystem::new(network, bootstrap_export)`.
5. Guest bootstraps the host-provided capability:
   `rpc_system.bootstrap(Side::Server)` → `Membrane` for pid0 or
   `InitialGrants` for an ordinary child
6. Guest optionally exports its own bootstrap capability with `system::serve`.

### Guest Entry Points

The `system` crate (`std/system`) provides these entry points, which handle
connection setup automatically:

| Function | Signature | Description |
|----------|-----------|-------------|
| `system::run` | `(f: FnOnce(C) -> Future) -> Future<Result<(), Error>>` | Bootstrap the host and select `RpcSystem` with the application Future. |
| `system::serve` | `(bootstrap: Client, f: FnOnce(C) -> Future) -> Future<Result<(), Error>>` | Run the session and export one guest bootstrap capability. |

### Graft Exports and Child Initial Grants

`Membrane.graft()` returns a `List(Export)`. The current host graft uses the
following canonical names. `identity` requires a configured signing key, and
`http-client` appears only when the operator configures an `--http-dial`
allowlist. Trusted PID0 can delegate any selected references to an ordinary
child through `Executor.spawn` or a listener's registration-time grant list.

| Export name | Interface | Description |
|-------------|-----------|-------------|
| `identity` | `auth_capnp::identity` | Host-side signing. |
| `host` | `system_capnp::host` | Node identity and network interfaces. |
| `runtime` | `system_capnp::runtime` | Load WASM binaries and obtain Executors. |
| `routing-finder` | `routing_capnp::finder` | Find a bounded number of unique DHT providers. |
| `routing-announcer` | `routing_capnp::announcer` | Announce the Wetware host PeerID as a DHT provider. |
| `authority` | `auth_capnp::authority` | Construct a policy-bound `Terminal` over an explicit capability. |
| `ipfs` | `system_capnp::ipfs` | Read `/ipfs`, `/ipns`, or `/ipld` content through a `ByteStream`. |
| `http-client` | `http_capnp::http_client` | Make outbound HTTP requests to the configured host allowlist. |

An ordinary child calls `InitialGrants.get()` to obtain exactly the immutable
named references selected by its parent. `InitialGrants` does not add the
canonical graft exports automatically.

WASI filesystem access and the `ipfs` RPC capability are separate surfaces.
When the host installs the content substrate, WASI guests can read IPFS-family
paths through the virtual filesystem. The `ipfs` export provides
`Ipfs.read(path)` for non-WASI clients and for explicit delegation. Neither
surface provides enumeration, mutation, pin management, or publishing.

Host-derived grants retain their **epoch guards** and become invalid when the
host advances its epoch. Repeated `InitialGrants.get()` calls return the same
recorded references; fresh authority requires explicit ancestor re-delegation
or child respawn. Non-host grants keep their own normal lifetime semantics.

`routing-finder` and `routing-announcer` are independently delegable. The
optional routing-key WIT import is not part of `Membrane.graft()` or
`InitialGrants`.

## Cap'n Proto RPC (system.capnp)

Full interface reference for the capabilities available to guests.

### Host

| Method | Signature | Description |
|--------|-----------|-------------|
| `id` | `() -> (peerId: Data)` | This node's libp2p peer ID. |
| `addrs` | `() -> (addrs: List(Data))` | Multiaddrs this node listens on. |
| `peers` | `() -> (peers: List(PeerInfo))` | Currently connected peers. |
| `network` | `() -> (streamListener, streamDialer, vatListener, vatClient, httpListener)` | Get network interfaces (byte-stream + RPC + HTTP modes). |

### Provider routing (`routing.capnp`)

| Interface | Method | Signature | Description |
|-----------|--------|-----------|-------------|
| `Finder` | `findProviders` | `(key: Text, count: UInt32, sink: ProviderSink) -> ()` | Deliver at most `count` unique WAN/LAN provider PeerIDs through a single-slot handoff. The swarm selects and retains at most `min(count, 16)` results. `count == 0` starts no query. A per-request token stops remaining work after sink failure, epoch expiry, or the 30-second deadline. The deadline also includes command admission. |
| `Announcer` | `provide` | `(key: Text) -> ()` | Announce the Wetware host PeerID on WAN and LAN. Local registration and republication stop after the final owner epoch ends. |

The removed broad `Routing` interface is not available. Guests have no
provider-routing methods for IPNS resolution or publication, persistent
UnixFS mutation, or CID derivation.

### Runtime

| Method | Signature | Description |
|--------|-----------|-------------|
| `load` | `(wasm: Data) -> (executor: Executor)` | Compile (or cache-hit) WASM bytes and return an Executor bound to that binary. |
| `shutdown` | `() -> ()` | Terminate all tasks spawned through this Runtime. |

### Executor

| Method | Signature | Description |
|--------|-----------|-------------|
| `spawn` | `(args: List(Text), env: List(Text), caps: List(Export), fuelPolicy: FuelPolicy) -> (process: Process)` | Spawn a new instance of the bound WASM binary with args, env, explicit initial grants, and fuel policy. |
| `cid` | `() -> (cid: Text)` | Return the CID of the WASM binary bound to this Executor. |

### Process

| Method | Signature | Description |
|--------|-----------|-------------|
| `stdin` | `() -> (stream: ByteStream)` | Writable stream to guest's stdin. |
| `stdout` | `() -> (stream: ByteStream)` | Readable stream from guest's stdout. |
| `stderr` | `() -> (stream: ByteStream)` | Readable stream from guest's stderr. |
| `wait` | `() -> (exitCode: Int32)` | Block until process exits. |
| `bootstrap` | `() -> (cap: Capability)` | Get the capability exported by the guest via `system::serve()`. |
| `kill` | `() -> ()` | Terminate the process by revoking its fuel. |

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

### Component Model async execution

Wasmtime drives the generated P3 task. `std/system` composes one real
`capnp_rpc::RpcSystem` with one application Future. The guest contains no
Tokio, custom executor, poll loop, fixed liveness timer, task queue, generic
spawn, or channel runtime.

The `wit-bindgen` `async-spawn` and `inter-task-wakeup` features remain
disabled. Current first-party wake sources are P3 operations. A pure-Rust wake
source that becomes ready without a P3 operation is unsupported.

### Resource cleanup

Root-Future or Store cancellation drops Cap'n Proto and P3 resources in normal
Rust ownership order. The guest does not leak resources with
`std::mem::forget` and does not use a P2 resource-order workaround.

### Epoch guards

Host capabilities grafted by pid0 are wrapped in epoch guards. When the host
advances its epoch (e.g., on-chain state change), delegated copies also become
invalid and calls return `staleEpoch` errors. Ordinary children cannot
re-graft. The Host terminates the old PID0 and starts a fresh PID0 for the new
generation.

### Host I/O buffering

| Path | Buffering | Location |
|---|---|---|
| stdout and stderr | No adapter payload buffer; P3 handles share and flush the configured `AsyncWrite` | `crates/cell/src/proc.rs` |
| RPC transport | Independent 64 KiB buffer in each direction | `crates/cell/src/p3.rs` (`STREAM_BUFFER_CAPACITY`) |

P3 stdout and stderr completion waits for the underlying host flush. A finite
command can therefore complete without losing its final output bytes.

### Forward-progress limits

RPC can progress while the application awaits a supported P3 clock, transport,
stdin, or filesystem operation. Non-yielding CPU work and synchronous imports
can block other Futures inside the same Cell. Host fuel and epoch interruption
still bound Cell-level execution and teardown.
