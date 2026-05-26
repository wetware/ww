# CLI Reference

## ww init

Scaffold a new typed cell guest project.

```
ww init <NAME>
```

Creates a Rust project with Cap'n Proto schema, build script, and
FHS boot layout.

## ww build

Compile a guest project to WASM.

```
ww build [PATH]
```

Targets `wasm32-wasip2` and places the artifact at `boot/main.wasm`
inside the project. Defaults to the current directory.

## ww run

Boot a wetware node.

```
ww run [OPTIONS] [MOUNT...]
```

Every positional argument is a mount: `source[:target]`.
Without `:target`, the source is mounted at `/` (image layer).
With `:target`, the source is overlaid at that guest path.
Layers stack with per-file union; later layers win.

### Options

| Flag | Default | Description |
|------|---------|-------------|
| `--port <PORT>` | `2025` | libp2p swarm listen port |
| `--identity <PATH>` | `~/.ww/identity` | Ed25519 identity file path. Also reads `WW_IDENTITY` env. |
| `--insecure-ephemeral` | off | Allow ephemeral identity fallback if identity file is missing (insecure; for quick trial runs). |
| `--mcp` | off | Run as MCP server (JSON-RPC on stdin/stdout) |
| `--http-listen <ADDR>` | none | Enable WAGI HTTP server (e.g. `127.0.0.1:2080`) |
| `--http-dial <HOST>` | none | Allow outbound HTTP to host. Repeatable. Supports exact hosts, `*.example.com`, or `*`. Without this flag, no http-client capability is granted. |
| `--with-http-admin <ADDR>` | none | Enable HTTP admin endpoint (metrics, `/host/id`, `/host/addrs`) |
| `--wasm-debug` | off | Enable WASM debug info for guest processes |
| `--executor-threads <N>` | `0` | Executor worker threads (0 = auto-detect, one per CPU core) |
| `--runtime-cache-policy` | `shared` | `shared`: same WASM bytes share Executor. `isolated`: always fresh. |
| `--ipfs-url <URL>` | `http://localhost:5001` | IPFS HTTP API endpoint. Also reads `IPFS_API` env. |
| `--stem <ADDR>` | none | Atom contract address (hex, 0x-prefixed). Enables epoch pipeline. |
| `--rpc-url <URL>` | `http://127.0.0.1:8545` | HTTP JSON-RPC for eth_call/eth_getLogs |
| `--ws-url <URL>` | `ws://127.0.0.1:8545` | WebSocket JSON-RPC for eth_subscribe |
| `--confirmation-depth <N>` | `6` | Blocks before finalizing HeadUpdated events |
| `--epoch-drain-secs <N>` | `1` | Seconds to drain in-flight ops before epoch advance |

### Examples

```sh
# Dev mode (current directory)
ww run .

# Run as MCP server
ww run . --mcp

# HTTP endpoint with outbound access
ww run . --http-listen 0.0.0.0:2080 --http-dial api.example.com

# Admin metrics + custom port
ww run . --port 3030 --with-http-admin :2026

# Explicit identity path + image layers
ww run --identity ~/.ww/identity images/app /ipfs/QmDataLayer

# Run from IPFS
ww run /ipfs/QmHash...

# On-chain epoch lifecycle
ww run . --stem 0x1234...abcd --rpc-url http://rpc.example.com:8545
```

### Environment variables

| Variable | Set by | Description |
|----------|--------|-------------|
| `WW_IDENTITY` | user | Default identity file path |
| `WW_TTY` | host | Set to `1` when stdin is a terminal (triggers interactive shell mode) |
| `WW_CELL_MODE` | host | Cell transport mode: `vat`, `raw`, `http`, or absent (kernel) |
| `IPFS_API` | user | Default IPFS HTTP API endpoint |
| `RUST_LOG` | user | Host-side tracing verbosity |

## ww shell

Connect to a running daemon and open a Glia REPL.

```
ww shell [ADDR] [--select <index|peer-id>]
```

Shell transport/auth is remote, but evaluation is local:
- Connect over libp2p `/ww/0.1.0`.
- Authenticate via `Terminal(Membrane)` challenge-response.
- Graft capabilities from the daemon membrane.
- Evaluate Glia inside the local `ww shell` process.

- *(no args)* — discover via local host-state (`~/.ww/run/host.json`,
  or `$WW_HOST_STATE_PATH`). Auto-connect if unambiguous; otherwise prompt
  for selection in TTY mode.
- `<multiaddr>` — explicit remote dial.
- `--select <index|peer-id>` — non-interactive target override when discovery
  returns multiple hosts.

`ww shell` does not call daemon-side `runtime.load(shell.wasm)`,
`executor.spawn`, or `process.bootstrap` on connect.
For `load`/`import`, `/ipfs|/ipns|/ipld` paths route through grafted
`system.Ipfs.read`; non-IPFS paths use local process filesystem reads.

### Examples

```sh
ww shell                                    # local host-state discover + connect
ww shell --select 2                         # choose 2nd discovered host
ww shell /dnsaddr/master.wetware.run        # explicit dial
ww shell /ip4/127.0.0.1/tcp/2025/p2p/12D3KooW...
ww shell garbage                            # clap parse error: invalid multiaddr
```

### Auth model

Shell uses Terminal(Membrane) challenge-response auth over libp2p.
The signer key comes from `WW_IDENTITY` or `~/.ww/identity`.

See [shell.md](shell.md) for Glia syntax and the capabilities the
shell cell exposes.

## ww push

Snapshot a project's FHS tree and publish to IPFS.

```
ww push [PATH] [OPTIONS]
```

Adds the tree to IPFS as a directory and returns the root CID.
Optionally updates the on-chain Atom contract HEAD.

| Flag | Default | Description |
|------|---------|-------------|
| `--ipfs-url <URL>` | `http://localhost:5001` | IPFS HTTP API endpoint |
| `--stem <ADDR>` | none | Atom contract address to update |
| `--rpc-url <URL>` | `http://127.0.0.1:8545` | JSON-RPC for eth_sendTransaction |
| `--private-key <KEY>` | none | Hex private key (required with `--stem`) |

## ww keygen

Generate a new Ed25519 identity.

```
ww keygen [--output PATH]
```

Prints the base58btc secret key to stdout, peer ID to stderr.

```sh
ww keygen > ~/.ww/identity
ww keygen --output ~/.ww/identity   # equivalent
```

## ww doctor

Check the development environment for required and optional tools.

```
ww doctor
```

Verifies: Rust toolchain, `wasm32-wasip2` target, Cargo. Optionally
checks for Kubo (IPFS) and Ollama (LLM). Exit 0 if all required
checks pass.

## ww perform

Effectful operations that mutate state beyond the current directory.

### ww perform install

Bootstrap `~/.ww`, daemon, and MCP wiring.

Idempotent: re-running skips completed steps, retries failed ones.

1. Creates `~/.ww` directory structure
2. Generates Ed25519 identity (if missing)
3. Registers background daemon (launchd/systemd)
4. Wires MCP into Claude Code (if installed)

### ww perform upgrade

Self-update the `ww` binary via IPNS.

```
ww perform upgrade [--ipfs-url URL]
```

Resolves `/ipns/releases.wetware.run/Cargo.toml` for the latest
version, fetches the platform binary, and atomically replaces the
running executable.

### ww perform uninstall

Remove daemon, MCP wiring, and optionally `~/.ww`.

## ww daemon

Manage the background daemon.

### ww daemon install

Register wetware as a user-level background service (launchd on
macOS, systemd on Linux).

```
ww daemon install [--identity PATH] [--listen MULTIADDR ...] [--images PATH...]
```

### ww daemon uninstall

Remove the platform service file.

## ww ns

Manage namespaces (IPFS mount layers).

### ww ns list

List configured namespaces.

### ww ns add

Add or update a namespace.

```
ww ns add <NAME> [--ipns KEY]
```

Writes a config file to `~/.ww/etc/ns/<name>`.

## ww oci import

Pull the container image from IPFS into Docker/podman.

```
ww oci import [--cid CID] [--stdout] [--ipfs-url URL]
```

Resolves the OCI tar from `/ipns/releases.wetware.run/oci/image.tar`
and pipes it to `docker load` or `podman load`.

```sh
ww oci import                     # auto-detect and load
ww oci import --cid QmHash...    # specific CID
ww oci import --stdout | podman load
```
