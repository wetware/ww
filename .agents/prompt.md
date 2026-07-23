# Wetware

Peer-to-peer capability-secured OS for autonomous agents.
WASM processes ("cells") run with zero ambient authority — they
can only do what they've been explicitly granted.

Source: https://github.com/wetware/ww

## Quick reference

```
ww run .                          # boot node from current dir
ww run /ipfs/QmHash               # boot from IPFS CID
ww run . --mcp                    # run as MCP server (stdin/stdout)
ww run . --listen /ip4/0.0.0.0/tcp/2025  # libp2p swarm listener
ww run . --http-listen 0.0.0.0:2080   # WAGI HTTP endpoint
ww run . --http-dial api.example.com # allow outbound HTTP to host
ww run . --with-http-admin 127.0.0.1:2026  # admin endpoint (default; use off to disable)
ww run . --identity ~/.ww/identity    # Ed25519 key
ww run . --stem 0xAddr --rpc-url http://... --ws-url ws://...
                                  # on-chain epoch pipeline
```

## Core commands

| Command | What it does |
|---------|-------------|
| `ww init NAME` | Scaffold a new cell guest project |
| `ww build [PATH]` | Compile to wasm32-wasip2, place in boot/main.wasm |
| `ww run [MOUNT...]` | Boot a node; mounts are `source[:target]` |
| `ww push [PATH]` | Snapshot FHS tree to IPFS, optionally update on-chain HEAD |
| `ww keygen` | Generate Ed25519 identity (prints to stdout) |
| `ww shell` | Glia REPL on the local daemon via UDS (~/.ww/run/<peer-id>.sock) |
| `ww doctor` | Check dev environment (Rust, wasm target, Kubo) |
| `ww perform install` | Bootstrap ~/.ww, daemon, MCP wiring |
| `ww perform upgrade` | Self-update binary via IPNS |
| `ww daemon install` | Register background daemon (launchd/systemd) |
| `ww ns add NAME --ipns KEY` | Add a namespace (IPFS mount layer) |
| `ww oci import` | Pull container image from IPFS into Docker/podman |

## Cell modes

Cells are WASM binaries whose stdio is wired to a transport.
`WW_CELL_MODE` env var tells the guest what's connected:

| Mode | stdio carries | Use case |
|------|--------------|----------|
| `vat` | Cap'n Proto RPC | Service mesh, capability exchange |
| `raw` | libp2p stream bytes | Low-level protocols |
| `http` | CGI (WAGI) | HTTP request handlers |
| *(absent)* | Host RPC channel | pid0 kernel — full membrane graft |

## Architecture (three layers)

- **Host** (`ww` binary): libp2p swarm, loads kernel WASM, serves Membrane.
- **Kernel** (pid0): calls `membrane.graft()`, receives `List(Export)` of named capabilities. All policy lives here.
- **Children**: spawned by pid0 with attenuated capabilities.

## Capabilities after graft

Host, Runtime, Routing, Identity, HttpClient, StreamListener,
StreamDialer, VatListener, VatClient.

## Mounts

Every positional arg to `ww run` is a mount: `source[:target]`.
Without `:target`, source mounts at `/` (image layer).
With `:target`, source overlays that guest path.
Layers stack with per-file union; later layers win.

```
ww run images/app ~/.ww/identity:/etc/identity ~/data:/var/data
```

## AI integration

Wetware is the drivetrain, not the engine. An LLM connects *to*
a node over MCP and gets a Glia shell. `ww run . --mcp` makes
the cell an MCP server on stdin/stdout.

## Standard ports

| Port | Service |
|------|---------|
| 2025 | libp2p swarm |
| 2026 | Local HTTP admin (`/healthz`, metrics, peer ID, listen addrs); use `--with-http-admin off` to disable |
| 2080 | HTTP/WAGI |

## Develop → deploy

```sh
ww init myapp                 # scaffold
cd myapp && ww build          # compile to WASM
ww run .                      # test locally
ww push . --ipfs-url http://localhost:5001   # publish to IPFS
ww run /ipfs/<CID>            # run from content-addressed image
```

## More info

- Architecture: `doc/architecture.md` in the repo
- Examples: `examples/` directory (echo, counter, oracle, chess)
- Full guide: `ipfs cat /ipns/releases.wetware.run/.agents/prompt.md`
