# Architecture

This document covers the design principles and capability flow of Wetware.
For transport plumbing (duplex streams, WASI bindings, scheduling, deadlocks),
see [rpc-transport.md](rpc-transport.md).  For the guest-side async runtime
(poll loop, effect system, oneshot channels), see
[guest-runtime.md](guest-runtime.md).

## Overview

Wetware is a decentralized operating system for autonomous agents. It runs
WASM guests in sandboxed cells with zero ambient authority: all capabilities
are explicitly granted over Cap'n Proto RPC, and object lifetimes are managed
through on-chain epoch boundaries.

`ww run` resolves one or more image layers into a unified FHS root, loads
`boot/main.wasm` from the result, and spawns the agent with epoch-scoped
capabilities served over in-memory Cap'n Proto RPC.

The host is deliberately simple. It merges layers, loads a binary, and
hands it capabilities. Everything else — peer discovery, service management,
access control — is the agent's job. The host is the sandbox; the agent
is the policy engine.

## No ambient authority

Wetware follows capability-based security. All authority flows through
explicitly-passed Cap'n Proto capability objects. There is no ambient authority.

Traditional programs inherit authority from their environment: they can read
files, open sockets, inspect environment variables, and call any syscall the OS
allows. A Wetware agent has none of that. Its WASI sandbox provides stdio
(bound to the host terminal) and a data stream (bound to the RPC connection).
That's it. The agent's only connection to the outside world is the `Membrane`
the host hands it at boot — and it calls `graft()` to obtain actual
capabilities. (Having a Membrane reference IS authorization — ocap model.
Authentication, if needed, is handled by wrapping the Membrane in a
`Terminal(Membrane)` challenge-response layer.)

```
Traditional process:        Wetware guest:
  env vars     -> yes         env vars     -> only if explicitly passed
  filesystem   -> yes         filesystem   -> virtual WASI FS (read-only, from image layers)
  network      -> yes         network      -> no
  syscalls     -> yes         syscalls     -> WASI subset only
  ambient auth -> yes         ambient auth -> none
                              graft caps   -> the only authority (named exports via List(Export))
```

**The guest filesystem is virtual and read-only.** The host merges image
layers into a virtual WASI filesystem that the guest can read via standard
POSIX file operations. Content is reactive to stem updates: when the
on-chain head advances, the filesystem reflects the new image.

This is the foundation that makes untrusted code execution safe. An agent can
only do what the capabilities it holds allow. If you don't hand it the
`Executor` capability, it can't spawn children. If you don't hand it a
`connect` method, it can't dial peers.

## Comparison: Cloudflare Workers

Cloudflare Workers is the closest prior art for sandboxed, instruction-metered
guest execution at the edge.  The table below maps the two models side by side.

| Dimension | Cloudflare Workers | Wetware |
|---|---|---|
| Isolation unit | V8 Isolate | WASM Component (Cell) |
| Runtime | JavaScript / V8 | WASM / Wasmtime |
| OS threads | Shared pool across isolates | Dedicated OS thread per executor worker |
| Task scheduling | V8 event loop per isolate | `tokio::task::spawn_local` on a `LocalSet` per worker |
| Multiplexing | Many isolates per thread (V8-managed) | M:N — many cells per worker (EWMA fuel scheduler) |
| Preemption mechanism | V8 interrupt API (time-based) | Wasmtime fuel counter (instruction-based, deterministic) |
| CPU-bound guest behavior | Isolate interrupted after CPU time budget | Cell yields every `YIELD_INTERVAL` instructions via `fuel_async_yield_interval` |
| Cold start | ~0ms (isolate reuse within process) | Per-cell WASM compilation; `Engine` is shared across cells |
| Authority model | Ambient — `fetch`, KV, R2 via binding config | Zero ambient — all capabilities granted via `membrane.graft()` |
| Inter-cell communication | Service bindings (HTTP), Durable Object RPC | Cap'n Proto RPC over in-process or libp2p transport |
| Shared state | Durable Objects (single-writer actor) | Capabilities are the unit of shared state; epoch lifecycle for revocation |
| Memory isolation | Separate V8 heap per isolate | Separate Wasmtime `Store` per cell |
| Send-safety | N/A (JavaScript) | `Store` is `!Send` — cells are pinned to their worker's `LocalSet` |

The sharpest differences:

**Preemption.** Workers uses time-based interrupts external to V8.  Wetware's
preemption is instruction-count-based and baked into Wasmtime — the same binary
consumes the same fuel regardless of host CPU speed, making scheduling behavior
deterministic and independently verifiable.

**Authority.** Workers still grants ambient authority: you configure which
services a Worker can reach, but inside those bounds it calls `fetch()` freely.
Wetware has no ambient authority at all.  Every capability is an unforgeable
object reference.  If the `Executor` capability is not in scope, a cell cannot
spawn children — there is no configuration flag to bypass this.

## Layers

```
┌─────────────────────────────────────────────────────┐
│  Host (ww binary)                                   │
│  - loads kernel from boot/main.wasm                 │
│  - starts libp2p swarm                              │
│  - serves Membrane to kernel                        │
│                                                     │
│  ┌───────────────────────────────────────────────┐  │
│  │  kernel (pid0)                                │  │
│  │  - grafts onto Membrane, obtains capabilities  │  │
│  │  - interprets bin/, svc/, etc/                │  │
│  │  - connects to bootstrap peers                │  │
│  │  - spawns services                            │  │
│  │  - defines what to export to the network      │  │
│  │                                               │  │
│  │  ┌─────────────┐  ┌─────────────┐             │  │
│  │  │ child-echo  │  │ metrics     │  ...        │  │
│  │  │ (service)   │  │ (service)   │             │  │
│  │  └─────────────┘  └─────────────┘             │  │
│  └───────────────────────────────────────────────┘  │
└─────────────────────────────────────────────────────┘
```

**Host** (`ww` binary) is the supervisor. It loads `boot/main.wasm` from an
image, starts a libp2p swarm, and serves a `Membrane` to pid0 over Cap'n
Proto RPC. It knows nothing about the rest of the image layout — `bin/`,
`svc/`, `etc/` are opaque directories as far as the host is concerned.

**pid0** (the kernel agent loaded from `boot/main.wasm`) is init. It
receives a `Membrane`, calls `graft()`, and uses the resulting capabilities
to interpret the image layout: look up executables from `bin/`, spawn
services from `svc/`, apply configuration from `etc/`. pid0 is where
policy lives.

**Children** are agents spawned by pid0 (or by other children) via
`runtime.load(wasm)` followed by `executor.spawn()`. Each child gets
its own set of capabilities over its own RPC connection. pid0 can scope
these capabilities, giving a child a restricted view of the host.

## Capability flow

### Inbound: host to guest

The host creates a Membrane and bootstraps it to pid0 over in-memory
Cap'n Proto RPC. pid0 calls `membrane.graft()` to obtain epoch-guarded
capabilities as a `List(Export)` of named capabilities:

- **Host** — peer identity, listen addresses, network access
- **Runtime** — load WASM binaries, obtain scoped Executors (with compilation caching)
- **Routing** — Kademlia DHT (provide, findProviders)
- **Identity** — host-side Ed25519 signing (private key never enters WASM)
- **HttpClient** — outbound HTTP requests (domain-scoped via `--http-dial`)
- **StreamListener / StreamDialer** — open and accept libp2p byte streams
- **VatListener / VatClient** — serve and consume Cap'n Proto RPC over the network

All capabilities are epoch-guarded: they become stale when the on-chain
head advances. The guest must re-graft to obtain fresh capabilities.

Having a Membrane reference IS authorization (ocap model). `graft()` is
parameterless -- no signer needed. To gate access for remote peers, wrap
the Membrane in `Terminal(Membrane)`, which requires challenge-response
authentication before handing out the Membrane reference.

```
Host                             pid0
----                             ----
create Membrane
  with GraftBuilder
    Host, Runtime, Routing,
    Identity, HttpClient,
    StreamListener, StreamDialer,
    VatListener, VatClient
serve via RpcSystem ----------> membrane.graft() -> List(Export { name, cap })
                                  lookup("host").id()
                                  lookup("host").addrs()
                                  lookup("runtime").load(wasm) -> executor
                                  executor.spawn(args, env) -> process
```

### Outbound: guest to host

Cap'n Proto RPC is bidirectional. The guest can export capabilities *back*
to the host — the host just bootstraps from the guest's side of the
connection. The host doesn't need to know in advance what the guest will
export.

This is the key insight: the RPC connection is symmetric. Both sides can
serve capabilities. Both sides can hold references to the other's objects.

### Network: host to remote peers

The host can take whatever capability it bootstrapped from the guest and
serve it over a libp2p stream protocol. Remote peers get a Cap'n Proto
client stub pointing at the guest's exported capability, proxied through
the host.

```
Node A                                     Node B
──────                                     ──────
pid0 exports Membrane ──> host             host ──> pid0 imports Membrane
                          serves on                  as a client stub
                          libp2p stream
                            <═══════════════>
                          Cap'n Proto RPC over libp2p
```

### The Membrane pattern

pid0 receives a `Membrane` from the host, calls `graft()`, and obtains
capabilities. It can then wrap, filter, or extend those capabilities into
a new **Membrane**: an object that controls what the outside world can do.

```
1. Host hands pid0 a Membrane reference
2. pid0 calls graft(), receives capabilities (identity, host, runtime, routing, httpClient)
3. pid0 can wrap or filter capabilities into a new Membrane
4. pid0 exports that Membrane back to the host (optionally wrapped in Terminal)
5. Host serves the exported capability on a libp2p stream protocol
6. Remote peers authenticate via Terminal (if present), then interact with the Membrane
```

This is how pid0 controls access. The host doesn't decide what remote
peers can do — pid0 does, by choosing what to export. The host is just
the transport.

## Configuration

There is one configuration model: FHS. An image is an FHS directory tree.
pid0 interprets it. The host only reads `boot/main.wasm`; everything
else is between the image author and pid0.

See the [README](../README.md) for the image layout. In brief:

```
<image>/
  boot/main.wasm    # agent entrypoint — consumed by host
  bin/              # executables on the kernel's PATH — consumed by pid0
  svc/<name>/       # nested service images — consumed by pid0
  etc/              # configuration — consumed by pid0
```

The FHS root that pid0 sees can be assembled from multiple **layers**
via per-file union:

```
ww run [--stem <contract>] [<path> ...]
```

The Stem contract's head CID (if provided) forms the base layer.
Positional arguments are stacked on top in order. Later layers override
earlier layers at the file level. There are no deletes — you can add
and override, but not remove.

```
ww run --stem 0xABC... /ipfs/QmOverlay ./local-tweaks
        │                │                │
        ▼                ▼                ▼
   base layer       middle layer      top layer
   (from chain)     (from IPFS)       (local fs)
```

No single layer needs to be complete. A Stem CID might provide `etc/`
and `bin/` but no `boot/main.wasm`, expecting an overlay to supply the
entrypoint. The only requirement is that the **union** contains
`boot/main.wasm`.

```sh
# Standalone: fully self-contained local image
ww run ./my-image

# Cluster provides everything, run as-is
ww run --stem 0xABC...

# Cluster provides authority + bootstrap, you provide the code
ww run --stem 0xABC... ./my-app

# Cluster base, IPFS plugin, local dev config
ww run --stem 0xABC... /ipfs/QmPlugin ./local-config
```

### Layer resolution

- **Per-file union.** Each layer contributes files. If two layers provide
  the same path, the later layer wins.
- **No deletes.** To remove something from a lower layer, publish a new
  version of that layer without it.
- **Directories merge, files replace.** If layer A has `boot/QmPeerA` and
  layer B has `boot/QmPeerB`, the result has both. If layer B also has
  `boot/QmPeerA`, layer B's version wins.

### Stem integration

When `--stem` is provided, the host reads the head CID from the
contract, fetches it from IPFS as the base layer, and boots pid0 with
an epoch-scoped Membrane. When the on-chain head advances, capabilities
are revoked and the host reloads with the new base.

Without `--stem`, pid0 gets a Membrane with no epoch lifecycle.
The process exits when pid0 exits.

### Node config

Orthogonal to the image, **node config** controls how the host
behaves on this particular machine: `--listen`, `--wasm-debug`, IPFS
daemon address, log levels, resource limits. Node config is set via CLI
flags or env vars — it never lives inside image layers.

## Network architecture

Two nodes running Wetware communicate via capability passing over
libp2p:

```
┌─────────────────────┐              ┌─────────────────────┐
│  Node A (server)    │              │  Node B (client)     │
│                     │              │                      │
│  pid0 exports       │   libp2p    │  pid0 receives       │
│  Membrane ─────────>│<═══════════>│──────> Membrane stub  │
│                     │  Cap'n Proto │                      │
│  ww run <server>    │     RPC     │  ww run <client>     │
└─────────────────────┘              └─────────────────────┘
```

`ww run <server-image>` boots pid0, which exports a Membrane on the
network. `ww run <client-image>` connects to the server's peer ID and
receives a capability stub for the Membrane. The client can then call
methods on the Membrane as if it were local — Cap'n Proto handles the
serialization and transport.

All network communication is capability-mediated. A guest can only talk
to peers it has a capability reference for. There is no "broadcast to
the network" or "listen for connections" — only explicit capability
passing.

## Epoch lifecycle

When `--stem` points to an Atom smart contract, the host starts an
epoch pipeline that watches for `HeadUpdated` events on-chain:

```
AtomIndexer (WebSocket + HTTP backfill)
    |  HeadUpdatedObserved events
    v
Finalizer (K-confirmation strategy)
    |  FinalizedEvent
    v
pin new CID / unpin old CID on IPFS
    |
    v
epoch_tx.send(Epoch { seq, head, provenance })
    |
    v
EpochGuard invalidation → stale capabilities fail → guest re-grafts
```

The epoch channel is created before the guest spawns, so the pipeline
runs concurrently with the guest via `CellBuilder::with_epoch_rx()`.

## VFS & capability model

Wetware's filesystem is the IPFS UnixFS DAG, exposed to guests through
the WASI virtual filesystem (`CidTree` in `src/vfs.rs`, WASI integration
in `src/fs_intercept.rs`). This is the load-bearing claim that makes
the runtime capability-secure: **CIDs are unforgeable references, and
the filesystem a cell sees IS the subgraph reachable from CIDs it
knows.** No path-based permission model, no separate "IPFS" cap layered
on top of the normal filesystem.

```
        stem::Atom (root binding, swappable on epoch advance)
              │
              ▼
        CidTree.root  (Arc<String>, the cell's root CID)
              │
              ▼
   walk DAG via ipfs.ls() / ipfs.cat()  ──► IPFS UnixFS blocks
              │
              ▼
   guest path resolution  (e.g. /etc/init.d/05-status.glia)
              │
              ▼
   WASI fs syscalls  ──►  fs_intercept.rs  ──►  CidTree::resolve_path
                                                       │
                                                       ▼
                                          ResolvedNode { CidFile | CidDir | LocalFile | LocalDir }
```

The cell's WASI root preopen points at `CidTree::staging_dir()`
(`src/cell/proc.rs:537-559`), where the host materializes content on
demand. **WASI preopens are a protocol detail, not a security
boundary.** They give the guest a descriptor to anchor lookups
against. Reachability is governed by `CidTree`'s root, not by the
preopen.

**Three attenuation points, no fourth:**

1. **Membrane graft** — RPC capabilities (`identity`, `host`,
   `runtime`, `routing`, `http-client`, plus init.d `with` extras).
   Surface is `src/rpc/membrane.rs:HostGraftBuilder`.
2. **Root Atom binding** — the `stem::Atom` whose value is the cell's
   root CID. Swap the Atom and `CidTree::swap_root` updates the view
   atomically.
3. **Glia env bindings** — what's callable inside the cell. Filesystem
   data-plane access is via WASI path I/O (`load`, `import`, guest file
   reads), not a separate `perform fs` surface.

Backend virtual mode now rejects targeted mounts, so host-local overrides are
not active in the backend mount path. Data-plane content for backend cells must
flow through `/ipfs` / `/ipns` root layers.

Revocation = epoch advance + respawn. Classical ocap: you cannot
un-hand a CID. Advance the epoch (RPC caps fail `staleEpoch`), kill
and respawn the cell under a different root Atom — the new cell sees
a different slice of the universe.

For the agent-facing view of all this (WASI path I/O, `(schema cap)`,
structured errors, attenuation strategies), see
[capabilities.md](capabilities.md).

### Two host caches operate together

The host runs two caches behind the WASI VFS, each doing a different job.
Both are kept across cells; both are reset on host restart.

```
┌─────────────────────────────────────────────────────────────────┐
│  fs_intercept (overrides every guest WASI fs op)                │
│   resolve_path  ──►  CidTree::resolve_path                      │
│         │                       │                                │
│         │                       │  ResolvedNode                  │
│         ▼                       ▼                                │
│   ┌─────────────────┐    ┌──────────────────────────┐           │
│   │ PinsetCache     │    │ CidTree.staging_dir       │           │
│   │ (raw bytes)     │    │ (dir-listing stubs)       │           │
│   │                 │    │                           │           │
│   │ TempDir         │    │ /tmp/ww-staging-<pid>/    │           │
│   │ ARC-managed,    │    │ • <cid>.dirlist.json      │           │
│   │ 128 MiB budget, │    │   (3-tier mem/disk/ipfs   │           │
│   │ CID-keyed.      │    │    listing cache)         │           │
│   │ Open file FDs   │    │ • dir-<cid>/ subtrees     │           │
│   │ point HERE.     │    │   (sparse stubs at        │           │
│   │                 │    │    correct size for       │           │
│   │ Eviction        │    │    cap_std::fs::Dir::     │           │
│   │ unpins from     │    │    readdir)               │           │
│   │ IPFS too.       │    │                           │           │
│   └─────────────────┘    └──────────────────────────┘           │
│         ▲                       ▲                                │
│         │ ensure(cid)           │ ls_dir(cid)                    │
│         │ fetch(cid)            │                                │
│         ▼                       ▼                                │
│  ┌────────────────────────────────────────────────┐             │
│  │  Local Kubo daemon (kubo-rpc-api at :5001)     │             │
│  └────────────────────────────────────────────────┘             │
└─────────────────────────────────────────────────────────────────┘
```

**`PinsetCache`** (`crates/cache/src/pinset.rs`) is the host-wide raw-byte
cache. ARC-managed with a 128 MiB budget by default. Holds materialized
file content in a tempdir keyed by CID. Pins to the local Kubo node so
that content stays available; eviction unpins. Open WASI file descriptors
opened by the guest point at real files in this directory.

**`CidTree.staging_dir`** (`src/vfs.rs`) is per-process scratch. Holds
two kinds of artifacts: persisted JSON dir-listings (`<cid>.dirlist.json`)
that back the 3-tier memory/disk/IPFS lookup cache, and per-directory
stub trees (`dir-<cid>/`) used to satisfy `cap_std::fs::Dir::readdir`
without materializing every file's content. Stubs are sparse files with
the declared size; opening one redirects (via `fs_intercept`) to the
real bytes in `PinsetCache`.

The single WASI preopen at `/` points at `staging_dir`. It is a protocol
anchor, not a guest-visible filesystem: `fs_intercept` overrides every
`open_at`, `readdir`, and `stat` before it reads from the staging
directory, so the guest never sees the stub contents directly. See
`src/cell/proc.rs` (preopen call) and `src/fs_intercept.rs` (overrides).

## State management

Wetware provides two coordination primitives via **stems**:

- **Atomic stems** (on-chain): a linearizable register backed by a smart
  contract. The contract stores a single CID (the "head"). When updated,
  all capabilities are revoked and the epoch advances. This is the
  mechanism behind `--stem`.

- **Eventual stems** (IPNS): a mutable pointer backed by IPNS. Updates
  propagate through the DHT with eventual consistency. Used for
  namespace resolution (`ww ns add`) and configuration distribution.

**Planned: dosync transactions.** Atomic multi-field updates over
content-addressed state using Clojure-inspired STM semantics. Each
agent gets transactional state as a language primitive:

```clojure
(dosync game
  (alter! [:board :e2] nil)
  (alter! [:board :e4] :white-pawn))
```

The dosync model collapses three boundaries into one: consistency
boundary = stem root, authority boundary = write capability, identity
boundary = entity-over-time. See CEO plan `2026-04-11-dosync-transactions`
for the full design.

## Distribution

Wetware images are content-addressed FHS trees stored in IPFS. The
distribution model:

1. `ww build` compiles to `boot/main.wasm`
2. `ww push` adds the tree to IPFS and returns a CID
3. `ww run /ipfs/<CID>` boots from content-addressed storage
4. On-chain stems point to CIDs for automatic updates

IPNS provides mutable pointers for release channels:
`/ipns/releases.wetware.run` resolves to the latest release tree.
`ww perform upgrade` uses this to self-update the binary.

## AI integration

Wetware is the drivetrain, not the engine. An LLM connects to a
node and gets a capability-secured shell:

- **MCP mode** (`ww shell --mcp`): the shell process serves MCP on
  stdin/stdout over the standard shell dial/login/graft path. `eval` is
  the universal primitive; per-cap sugar tools (`host`, `routing`, ...)
  translate to internal Glia expressions. Glia host interaction begins with
  `perform` (`:load`, `:stdout`, `:exit`, or a capability method), while the
  membrane provides the authority boundary — AI agents can only do what their
  capabilities allow. MCP rejects `:stdout` and `:exit` with typed errors to
  keep JSON-RPC stdout clean.

- **Structured errors and introspection.** Errors are values
  (`Result<Val, Val>`) with namespaced `:glia.error/*` keys
  (`crates/glia/src/error.rs`). The MCP envelope surfaces the schema
  as `structuredContent` so agents can route on `:glia.error/type`
  without parsing prose. `(schema cap)`, `(doc cap)`, and `(help cap)`
  Glia builtins return `Schema.Node` bytes, summaries, and references
  for any grafted cap — the agent introspects the surface rather
  than relying on hardcoded knowledge. See
  [capabilities.md](capabilities.md) for the full schema.

- **Glia shell** (`ww shell`): interactive REPL for capability
  exploration. `ww perform install` wires MCP into Claude Code.

## See also

- [routing.md](routing.md) — DHT design, capability model, bootstrap lifecycle
- [shell.md](shell.md) — kernel shell reference (interactive + daemon modes)
- [cli.md](cli.md) — CLI flags and usage
- [rpc-transport.md](rpc-transport.md) — transport plumbing, scheduling model, deadlock analysis
- [../capnp/system.capnp](../capnp/system.capnp) — Host, Runtime, Executor, Process, ByteStream interfaces
- [../README.md](../README.md) — image layout, build instructions, usage
