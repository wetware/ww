# Wetware

Peer-to-peer capability-secured OS for autonomous agents.
WASM processes ("cells") run with zero ambient authority — they
can only do what they've been explicitly granted.

Source: https://github.com/wetware/ww

## Quick reference

```
ww run                            # boot the installed FHS, independently of CWD
ww run .                          # explicitly overlay the current directory
ww run /ipfs/QmHash               # boot from IPFS CID
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
| `ww build [PATH]` | Compile a native `wasm32-wasip3` component into `bin/` |
| `ww run [MOUNT...]` | Boot a node; mounts are `source[:target]` |
| `ww push [PATH]` | Snapshot FHS tree to IPFS, optionally update on-chain HEAD |
| `ww keygen` | Generate Ed25519 identity (prints to stdout) |
| `ww doctor` | Check dev environment (Rust, wasm target, Kubo) |
| `ww perform install` | Bootstrap ~/.ww and the daemon |
| `ww perform upgrade` | Self-update binary via IPNS |
| `ww daemon install` | Register background daemon (launchd/systemd) |
| `ww ns add NAME --ipns KEY` | Add a namespace (IPFS mount layer) |
| `ww oci import` | Pull container image from IPFS into Docker/podman |

## Cell modes

Async RPC Cells use the capability-granted P3 transport. `WW_CELL_MODE` tells
the guest which separate application plumbing the Host configured:

| Mode | stdio carries | Host wiring |
|------|--------------|-------------|
| `vat` | application-defined | Serves an existing guest capability |
| `raw` | raw libp2p stream bytes | Low-level protocols |
| `http` | CGI env vars + stdin/stdout | HTTP request handlers |
| *(absent)* | process input/output | Process-local P3 RPC; PID0 receives a `Membrane` |

## Architecture (three layers)

- **Host** (`ww` binary): libp2p swarm, selects `/boot/kernel.wasm` from each epoch's effective FHS, serves Membrane.
- **Kernel** (pid0): the packaged Rust kernel calls `membrane.graft()`, receives typed authority, and installs `/status`.
- **Children**: spawned with parent-selected narrow `Membrane` implementations.

## Capabilities after graft

peerId, Stat, grouped Network leaves, Routing fields, Runtime, Authority,
Identity, IPFS, and application-defined extras.

## Mounts

Every positional arg to `ww run` is a mount: `source[:target]`.
Without `:target`, source mounts at `/` (image layer).
With `:target`, source overlays that guest path.
Precedence is installed baseline, Stem head, frozen namespace images, then
frozen CLI operations. Later layers win; directories merge recursively.
`--fhs-root` or `WW_FHS_ROOT` selects the baseline. `--kernel` and `WW_KERNEL`
are rejected; use a targeted `/boot/kernel.wasm` mount.

```
ww run ./images/app ./config:/etc/myapp ~/data:/var/data
```

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
ww run .                      # mount the image; selected PID0 controls execution
ww push . --ipfs-url http://localhost:5001   # publish to IPFS
ww run /ipfs/<CID>            # mount the immutable image
```

## More info

- Architecture: `doc/architecture.md` in the repo
- Examples: `examples/` directory (echo, counter, oracle, chess)
- Full guide: `ipfs cat /ipns/releases.wetware.run/.agents/prompt.md`
