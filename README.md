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
default Rust kernel installs this composition directly. The cell receives a
narrow `Membrane` containing only `peerId` and `Stat`.

## Features

- **Explicit child Membranes.** Each ordinary cell starts with one parent-selected `Membrane` and no ambient node authority. Its `graft()` exposes only the typed fields and application-defined `extras` that the parent chose. Method restrictions remain attached to each capability reference and to capabilities reached through it.
- **Composable membranes.** Tool A calls tool B which calls tool C. Each process boundary carries one explicit `Membrane`. See [examples/oracle/](examples/oracle/) for the runnable version.
- **Content-addressed code.** Cells are identified by CID. The binary that ran is the binary you pinned; no swap-under-the-rug between generation and execution.
- **WASM cell scale.** ~10ms spawn, KB-scale binaries, language-agnostic via native `wasm32-wasip3` components. Per-call sandboxing is only feasible because cells are cheap; microVM cold-start is too slow for that.
- **P2P capability sharing.** A cell can export a typed capability to a peer over libp2p. Service names locate a stream; they do not authorize its caller. A deployer can publish a `Terminal` that authenticates a login identity and issues only the method authority selected for that identity.

## Quickstart

### Install

```bash
curl -sSL https://wetware.run/install | sh
```

Or build from source:

```bash
ww doctor                         # check your dev environment
rustup toolchain install nightly-2026-08-30 --component rust-src
make                              # build everything (host + std + examples)
```

Guest builds use the pinned native P3 toolchain. `ww doctor` reports missing
WASI SDK, `component-ld`, or `wasm-tools` dependencies. Optional: [Kubo](https://docs.ipfs.tech/install/) for IPFS resolution and DHT-based peer discovery.

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

`ww run` starts a libp2p node on port 2025 and merges any [image layers](doc/images.md)
into a virtual FHS filesystem. The Host selects trusted PID0 independently
through `KernelSource`: `--kernel` takes precedence over `WW_KERNEL`, and the
default is embedded `std/kernel`. `ww build` produces `boot/main.wasm` as the
conventional application artifact; the Host does not use it as PID0 input.

PID0 and ordinary children both receive a `Membrane`. PID0 receives a broad
root implementation. Each child receives the exact narrow implementation that
its parent passes to `Executor.spawn()`. Repeated `graft()` calls cannot add
authority because each server returns only its held references. After an epoch
transition, delegated host capabilities stay stale until an authorized ancestor
explicitly re-delegates fresh references or respawns the child.

[doc/architecture.md](doc/architecture.md) is the canonical reference; [doc/capabilities.md](doc/capabilities.md) is the capability surface.

### Cell modes

WASM processes ("cells") run with zero ambient authority. Async RPC Cells use
the granted P3 transport. `WW_CELL_MODE` identifies separate application
plumbing:

| Mode | stdio carries | Host wiring |
|------|--------------|-------------|
| `vat` | application-defined | Serves an existing guest capability |
| `raw` | raw libp2p stream bytes | Long-lived byte/session listener |
| `http` | CGI env vars + stdin/stdout | Stateless WAGI request adapter |
| *(absent)* | process input/output | Process-local P3 RPC; PID0 receives a `Membrane` |

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

- [Architecture map](https://wetware.github.io/ww/): interactive map of the current host, authority, cell, and network paths; see the [maintenance guide](diagrams/README.md) for updates
- [Positioning](doc/positioning.md): the JTBD-anchored category claim and audience
- [Architecture](doc/architecture.md): design principles and capability flow
- [Capabilities](doc/capabilities.md): the capability model and Cap'n Proto schemas
- [CLI reference](doc/cli.md): full command-line usage
- [Image layout](doc/images.md): FHS convention, mounts, on-chain coordination
- [Provider routing](doc/routing.md): independent discovery and host-PeerID announcement capabilities
- [Keys & identity](doc/keys.md): Ed25519 identity management
- [RPC transport](doc/rpc-transport.md): transport plumbing and scheduling model
- [Guest runtime](doc/guest-runtime.md): async runtime for WASM guests
- [Replay protection](doc/replay-protection.md): epoch-bound authentication
- [Examples](examples/): echo, counter, oracle, chess, discovery, and snap-hello-rs
