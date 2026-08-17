# Wetware

[![CI](https://github.com/wetware/ww/actions/workflows/rust.yml/badge.svg)](https://github.com/wetware/ww/actions/workflows/rust.yml)

Wetware lets you safely run code you didn't write, don't trust, and cannot see: third-party MCP servers, code your LLM produced at runtime, tools other agents handed you across the swarm. It's a decentralized operating system for multi-tool agent swarms.

Cells are WASM processes that run with zero ambient authority. Their only
access to the world is through explicitly granted, typed Cap'n Proto
capabilities. Those references can be attenuated to a method allowlist; the
restriction travels with the reference across local and libp2p RPC boundaries
and recursively confines capabilities returned through it. Argument- and
resource-level filtering remain separate, application-level designs. Least
privilege is enforced by the runtime, not delegated to a prompt or to the
model running inside the cell.

## Try it in 60 seconds

```sh
curl -sSL https://wetware.run/install | sh
curl http://localhost:2080/status
```

```json
{
  "status":       "ok",
  "version":      "0.1.0",
  "peer_id":      "12D3KooWRLf8DAFsNfbv3s2DjRMbUuPc8AYdcBfokZbz6kJ2aUss",
  "listen_addrs": ["/ip4/127.0.0.1/tcp/2025", "/ip6/::1/tcp/2025", ...],
  "peer_count":   216
}
```

The second command hit a WebAssembly cell running inside the daemon. The
default Rust kernel installs this composition directly. The cell receives only
the explicit `host` grant, which lets the cell report peer identity and peers.

## Features

- **Explicit child grants.** Each ordinary cell starts with a typed bundle of capabilities and nothing else. Parent cells choose which capabilities to hand down; method-level restrictions are enforced on the capability reference and on capabilities reached through it.
- **Composable membranes.** Tool A calls tool B which calls tool C, each link carrying an explicit capability set. The membrane is the boundary at every hop. See [examples/oracle/](examples/oracle/) for the runnable version.
- **Content-addressed code.** Cells are identified by CID. The binary that ran is the binary you pinned; no swap-under-the-rug between generation and execution.
- **WASM cell scale.** ~10ms spawn, KB-scale binaries, language-agnostic via `wasm32-wasip2`. Per-call sandboxing is only feasible because cells are cheap; microVM cold-start is too slow for that.
- **P2P capability sharing.** A cell can export a typed capability to a peer over libp2p. Service names locate a stream; they do not authorize its caller. A deployer can publish a `Terminal` that authenticates a login identity and issues only the method authority selected for that identity.

## Quickstart

### Install

```bash
curl -sSL https://wetware.run/install | sh
```

Or build from source:

```bash
ww doctor                         # check your dev environment
rustup target add wasm32-wasip2   # one-time
make                              # build everything (host + std + examples)
```

Requires a Rust toolchain with the `wasm32-wasip2` target. Optional: [Kubo](https://docs.ipfs.tech/install/) for IPFS resolution and DHT-based peer discovery.

### Run a node

```bash
ww run .                                # boot a node from current dir
```

### Build the example cells

```bash
make examples
```

The repository keeps the Rust example crates as buildable guest-component
references. The Rust PID0 installs only the default `/status` composition, so
the repository does not currently ship a generic runtime composition for these
examples.

## How it works

`ww run` starts a libp2p node on port 2025, merges any [image layers](doc/images.md) into a virtual FHS filesystem, and spawns trusted `boot/main.wasm` (pid0) with the full graft-capable Membrane.

Pid0 calls `membrane.graft()` to obtain host capabilities. Ordinary children
instead call `initial_grants.get()` and receive exactly the immutable
`List(Export)` selected by their parent—no host graft or fallback. After an
epoch transition, delegated host capabilities stay stale until an authorized
ancestor explicitly re-delegates fresh references or respawns the child.

[doc/architecture.md](doc/architecture.md) is the canonical reference; [doc/capabilities.md](doc/capabilities.md) is the capability surface.

### Cell modes

WASM processes ("cells") run with zero ambient authority. Their stdio is wired to a transport based on `WW_CELL_MODE`:

| Mode | stdio carries | Use case |
|------|--------------|----------|
| `vat` | Cap'n Proto RPC | Long-lived capability services |
| `raw` | libp2p stream bytes | Long-lived byte/session protocols |
| `http` | CGI (WAGI) | Stateless HTTP request adapters |
| *(absent)* | Host RPC channel | pid0 kernel, full membrane graft |

## Standard ports

| Port | Service |
|------|---------|
| 2025 | libp2p swarm |
| 2026 | Local HTTP admin (`/healthz`, metrics, peer ID, listen addrs); disable with `--with-http-admin off` |
| 2080 | HTTP/WAGI |

## Publishing a cell

```sh
ww init myapp                                # scaffold a new cell project
cd myapp && ww build                         # compile to WASM
ww push . --ipfs-url http://localhost:5001   # publish to IPFS
```

The Rust PID0 does not automatically compose arbitrary guest components from
an image. A published guest requires an application-specific composition path.

## Learn more

- [Positioning](doc/positioning.md): the JTBD-anchored category claim and audience
- [Architecture](doc/architecture.md): design principles and capability flow
- [Capabilities](doc/capabilities.md): the capability model and Cap'n Proto schemas
- [CLI reference](doc/cli.md): full command-line usage
- [Image layout](doc/images.md): FHS convention, mounts, on-chain coordination
- [Routing](doc/routing.md): Kademlia DHT and peer discovery
- [Keys & identity](doc/keys.md): Ed25519 identity management
- [RPC transport](doc/rpc-transport.md): transport plumbing and scheduling model
- [Guest runtime](doc/guest-runtime.md): async runtime for WASM guests
- [Replay protection](doc/replay-protection.md): epoch-bound authentication
- [Examples](examples/): echo, counter, oracle, chess, discovery, and snap-hello-rs
