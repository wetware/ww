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

**Cells** are the unit of computation.  Each Cell is a WASM binary
whose stdio is wired to a transport by the host.  The `WW_CELL_MODE`
envvar tells the guest what plumbing it's running under:

| `WW_CELL_MODE` | stdio carries | Host wires up |
|----------------|--------------|---------------|
| `vat` | Cap'n Proto RPC | `/ww/0.1.0/vat/{protocol}` named service |
| `raw` | raw libp2p stream bytes | `/ww/0.1.0/stream/{protocol}` listener |
| `http` | CGI env vars + stdin/stdout | WAGI (CGI for WASM) |
| absent | Cap'n Proto RPC (host channel) | pid0 -- full Membrane graft |

The kernel (`boot/main.wasm`) is trusted pid0. Its stdio is the host's Cap'n
Proto RPC channel, not a libp2p stream. It alone receives the graft-capable
`Membrane` and spawns other cells.

Architecture (three layers):
- **Host** (`ww` binary): boots a libp2p swarm, loads the kernel
  WASM, serves a Membrane over Cap'n Proto RPC.
- **Kernel** (pid0): calls `membrane.graft()` to obtain capabilities
  (Host, Runtime, Routing, Identity, HttpClient). The Rust kernel installs the
  shipped `/status` composition.
- **Ordinary children**: spawned with an immutable `InitialAuthorityRecord`
  delivered by `InitialGrants`; they do not receive `Membrane.graft()`.

Key abstractions:
- **Membrane**: graft-capable authority issuance for pid0 and separately for
  authenticated remote sessions; it is not child bootstrap.
- **InitialGrants**: the grants-only ordinary-child bootstrap. It returns the
  exact parent-selected record and has no refresh, graft, or lookup API.
- **Epoch lifecycle**: an advance stales host-issued guarded references. The
  host terminates the old pid0 and starts a replacement for the new generation.
  Children cannot refresh themselves.
- **FHS images**: layers are stacked with per-file union.  Later
  layers override earlier ones.
- **Cap'n Proto RPC**: bidirectional -- both host and guest can serve
  and consume capabilities.
Wetware does not embed an LLM. "Agent" means any autonomous process: AI,
human, or script. Wetware controls the authority available to that process.

Capabilities after pid0 grafting (ordinary children receive only explicitly
granted entries):

| Capability | Purpose |
|------------|---------|
| Host | Peer identity, addresses, peer management |
| Runtime | Load WASM binaries, obtain scoped Executors |
| Routing | Kademlia DHT (provide, findProviders) |
| Identity | Host-side signing (private key never enters WASM) |
| HttpClient | Outbound HTTP requests |
| StreamListener / StreamDialer | P2P byte streams for raw cells |
| VatListener / VatClient | Cap'n Proto RPC for capnp cells |

Grant authoring must prefer an image-bound Executor over Runtime, scoped Signer over Identity,
  attenuated methods over broad Host/Routing, and a capability protocol over
  bearer tokens in args/env.

Quick start:
```
rustup target add wasm32-wasip2
make
cargo run -- run --http-listen 127.0.0.1:2080 std/status
curl http://127.0.0.1:2080/status
```

Concurrency model (E-ordering):
Method calls on a single Cap'n Proto object are serialized -- no
races within an object.  Calls across objects are independent and
concurrent.  Pipelining lets you chain calls on promises.  No locks,
no semaphores -- the object IS the synchronization boundary.
