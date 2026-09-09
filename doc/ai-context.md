# AI Context

Concise reference for AI agents working with Wetware.  Skills
read this on demand -- it is NOT embedded in the system prompt.

For full details, read `doc/architecture.md` and the files
referenced below.

---

**Wetware** is a peer-to-peer operating system for autonomous agents.
It replaces ambient authority with capability-based security.  Agents
run as WASM processes called **Cells** with zero ambient authority --
they can only do what they've been explicitly granted capabilities
to do.

**Cells** are the unit of computation. Each Cell is a native
`wasm32-wasip3` component. Async RPC Cells receive one capability-granted
`wetware:transport@0.2.0` connection. Byte and HTTP handlers also use stdio.
The `WW_CELL_MODE` envvar identifies application plumbing:

| `WW_CELL_MODE` | stdio carries | Host wires up |
|----------------|--------------|---------------|
| `vat` | application-defined | `/ww/0.1.0/vat/{protocol}` serves an existing guest capability |
| `raw` | raw libp2p stream bytes | `/ww/0.1.0/stream/{protocol}` listener |
| `http` | CGI env vars + stdin/stdout | WAGI (CGI for WASM) |
| absent | process input/output | process-local P3 RPC transport; PID0 receives a `Membrane` |

The trusted PID0 implementation is `std/kernel`, which is embedded in `ww` by
default. PID0 uses the granted P3 transport for Cap'n Proto RPC. It alone
receives the process-local, graft-capable `Membrane`.

Architecture (three layers):
- **Host** (`ww` binary): boots a libp2p swarm, prepares each effective
  `CidTree` root, and owns PID0 replacement across epochs.
- **Kernel** (`std/kernel`): calls `membrane.graft()` once, loads
  `$WW_ROOT/bin/status.wasm`, grants `host`, installs `/status`, calls
  `kernel_ready()`, and normally remains alive until Host termination.
  Interactive `WW_TTY` execution can also end on stdin EOF.
- **Ordinary children**: spawned with an immutable `InitialAuthorityRecord`
  delivered by `InitialGrants`; they do not receive `Membrane.graft()`.

Key abstractions:
- **Membrane**: process-local, graft-capable authority issuance for PID0. It is
  not the ordinary-child bootstrap or a bare `/ww/0.1.0` network payload.
- **InitialGrants**: the grants-only ordinary-child bootstrap. It returns the
  exact parent-selected record and has no refresh, graft, or lookup API.
- **Epoch lifecycle**: an advance stales host-issued guarded references. The
  Host terminates the old PID0, prepares the effective root, and starts a fresh
  PID0 for the new generation.
  Children cannot refresh themselves.
- **FHS images**: layers are stacked with per-file union.  Later
  layers override earlier ones.
- **Cap'n Proto RPC**: bidirectional -- both host and guest can serve
  and consume capabilities.
- **Network transport**: authenticated vat services use
  `/ww/0.1.0/vat/*`; byte streams use `/ww/0.1.0/stream/*`.

The Host↔PID0 ABI is version 3. No ABI-v2 compatibility shim exists.
Wetware does not embed an LLM. "Agent" means any autonomous process: AI,
human, or script. Wetware controls the authority available to that process.

Capabilities after pid0 grafting (ordinary children receive only explicitly
granted entries):

| Capability | Purpose |
|------------|---------|
| Host | Peer identity, addresses, peer management |
| Runtime | Load WASM binaries, obtain scoped Executors |
| Finder (`routing-finder`) | Bounded, deduplicated Kademlia provider discovery |
| Announcer (`routing-announcer`) | Announce the Wetware host PeerID for an owner epoch |
| Identity | Host-side signing (private key never enters WASM) |
| HttpClient | Outbound HTTP requests |
| StreamListener / StreamDialer | P2P byte streams for raw cells |
| VatListener / VatClient | Cap'n Proto RPC for capnp cells |

Canonical CIDv1/raw/BLAKE3 routing-key derivation is the optional pure
`wetware:routing/key@0.1.0` WIT import. It is not a capability reference.

Grant authoring must prefer an image-bound Executor over Runtime, scoped Signer
over Identity, Finder without Announcer when observation is sufficient, and a
capability protocol over bearer tokens in args/env.

Quick start:
```
rustup toolchain install nightly-2026-08-30 --component rust-src
make
cargo run -- run --http-listen 127.0.0.1:2080 std/status
curl http://127.0.0.1:2080/status
```

Guest async model: Wasmtime P3 drives one generated task. `std/system` selects
one real `RpcSystem` with the application Future. Wetware has no guest
scheduler, polling timer, spawn API, or channel runtime.

Cap'n Proto concurrency model (E-ordering):
Method calls on a single Cap'n Proto object are serialized -- no
races within an object.  Calls across objects are independent and
concurrent.  Pipelining lets you chain calls on promises.  No locks,
no semaphores -- the object IS the synchronization boundary.
