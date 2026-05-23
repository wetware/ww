# Wetware

[![CI](https://github.com/wetware/ww/actions/workflows/rust.yml/badge.svg)](https://github.com/wetware/ww/actions/workflows/rust.yml)

Wetware lets you safely run code you didn't write, don't trust, and cannot see: third-party MCP servers, code your LLM produced at runtime, tools other agents handed you across the swarm. It's a decentralized operating system for multi-tool agent swarms.

Cells are WASM processes that run with zero ambient authority. Their only access to the world is the membrane they were grafted, a typed bundle of capabilities served over Cap'n Proto RPC. When a cell calls another cell, the caller chooses which capabilities to hand over and how to attenuate each one, and the runtime enforces the boundary. Each call carries only the capabilities you handed it; the trust boundary is the membrane, not the audit. There is no scheduler, no central trust authority, no shared state. Cells coordinate through content-addressed data in IPFS, and over libp2p streams.

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

The second command hit a WebAssembly cell running inside the daemon. The cell can't read your filesystem, reach the network, or see your environment variables. The only thing it can do is what the membrane handed it; in this case, the `host` capability, so it can report your peer ID and connected peers. The wiring that hands the `host` capability (and nothing else) to the HTTP handler cell lives at `~/.ww/etc/init.d/05-status.glia`:

```clojure
(perform host :listen (cell (load "bin/status.wasm")) "/status")
```

That's the whole registration.

Here is the capability surface in action, directly in the Wetware shell (Glia):
- `defcap` defines a capability server in Glia.
- `attenuate` derives a restricted capability.
- `isolate` runs with only explicitly granted capabilities.

```clojure
;; Define a local capability server with two methods.
(defcap directory
  :lookup   (fn [name]
              (perform routing :find name :count 5))
  :announce (fn [name]
              (perform routing :provide name)
              :ok))

;; Attenuate to a read-only view (lookup only).
(def directory-ro
  (attenuate directory [:lookup]))

;; Allowed call inside isolated context.
(isolate {:caps {:directory directory-ro}}
  (perform directory :lookup "service:invoices"))

;; Denied call: announce was not granted to this isolate.
(isolate {:caps {:directory directory-ro}}
  (perform directory :announce "service:payments"))
```

## Features

- **Per-call capability attenuation.** Each cell starts with a typed bundle of capabilities and nothing else. When it spawns a sub-cell, it chooses which capabilities to hand down, narrowed however it likes (with per-method granularity). The runtime enforces the boundary at every call.
- **Composable membranes.** Tool A calls tool B which calls tool C, each link carrying only what the previous layer authorized. Trust narrows at every hop. See [examples/oracle/](examples/oracle/) for the runnable version.
- **Content-addressed code.** Cells are identified by CID. The binary that ran is the binary you pinned; no swap-under-the-rug between generation and execution.
- **WASM cell scale.** ~10ms spawn, KB-scale binaries, language-agnostic via `wasm32-wasip2`. Per-call sandboxing is only feasible because cells are cheap; microVM cold-start is too slow for that.
- **P2P capability sharing.** A cell can export a typed capability to a cell on a peer's machine over libp2p. Graft and attenuation work identically across the wire; the membrane is the boundary, not the host.
- **MCP integration.** `ww perform install` wires the node into Claude Code as an MCP server. The same capability surface curl hits, an LLM reaches via attenuated capabilities. See [.agents/prompt.md](.agents/prompt.md).
- **Glia shell.** A Clojure-inspired language where capabilities are first-class values and every side effect (capability calls, exceptions, I/O) is gated by an effect system. The same shell serves humans (REPL) and LLMs (over MCP).

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
ww shell                                # discover a local node via mDNS, then open REPL
```

`ww shell` uses libp2p transport and Terminal(Membrane) auth. By default it
discovers local hosts via mDNS and prefers an unambiguous identity match.
If multiple hosts remain, TTY sessions prompt for selection, and
non-interactive sessions can pass `--select <index|peer-id>`.

### Boot a cell

`examples/oracle/` is a working cell that serves the same data over Cap'n Proto RPC and HTTP/WAGI:

```bash
ww run --http-listen 127.0.0.1:2080 --port=2025 std/kernel examples/oracle
curl http://localhost:2080/oracle
```

Read [examples/oracle/README.md](examples/oracle/README.md) for the full walkthrough, including the DHT-based consumer flow.

### Use it from an LLM

```bash
ww perform install
```

Wires the node into Claude Code as an MCP server. The LLM gets a Glia shell with attenuated capabilities, same membrane and same guarantees as the `curl` flow above. See [.agents/prompt.md](.agents/prompt.md).

## How it works

`ww run` starts a libp2p node on port 2025, merges any [image layers](doc/images.md) into a virtual FHS filesystem, and spawns `boot/main.wasm` with a Membrane: the typed capability hub the cell uses to reach the host.

A guest calls `membrane.graft()` to obtain its capabilities as a `List(Export)`. When the on-chain epoch advances (new code deployed, configuration changed), the membrane revokes everything; the guest re-grafts and picks up the new state automatically. This is the same machinery a parent cell uses to grant a child cell an attenuated subset, so capability flow across cells, hosts, and revocation cycles all use one mechanism.

[doc/architecture.md](doc/architecture.md) is the canonical reference; [doc/capabilities.md](doc/capabilities.md) is the capability surface.

### Cell modes

WASM processes ("cells") run with zero ambient authority. Their stdio is wired to a transport based on `WW_CELL_MODE`:

| Mode | stdio carries | Use case |
|------|--------------|----------|
| `vat` | Cap'n Proto RPC | Service mesh, capability exchange |
| `raw` | libp2p stream bytes | Low-level protocols |
| `http` | CGI (WAGI) | HTTP request handlers |
| *(absent)* | Host RPC channel | pid0 kernel, full membrane graft |

## The shell

Glia is a Clojure-inspired language where capabilities are first-class values. The design blends three traditions:

- **E-lang**: capabilities as values you can pass, compose, and attenuate
- **Clojure**: s-expression syntax, immutable data, functional composition
- **Unix**: processes, PATH lookup, stdin/stdout, init.d scripts

```
/ > (perform host :id)
"12D3KooWExample..."
/ > (perform host :addrs)
("/ip4/127.0.0.1/tcp/2025" "/ip4/192.168.1.5/tcp/2025")
```

See [doc/shell.md](doc/shell.md) for the full syntax and capability reference.

## Standard ports

| Port | Service |
|------|---------|
| 2025 | libp2p swarm |
| 2026 | HTTP admin (metrics, peer ID, listen addrs) |
| 2080 | HTTP/WAGI |

## Publishing a cell

```sh
ww init myapp                                # scaffold a new cell project
cd myapp && ww build                         # compile to WASM
ww run .                                     # test locally
ww push . --ipfs-url http://localhost:5001   # publish to IPFS
ww run /ipfs/<CID>                           # run from content-addressed image
```

## Roadmap

- **dosync**: transactional state management for Glia. Atomic multi-field updates over content-addressed stems. "Every agent gets its own Datomic, as a language primitive."
- **`ww shell` capability discovery**: attach a shell to a running node, enumerate cells, call them via Cap'n Proto from Glia.

## Learn more

- [Positioning](doc/positioning.md): the JTBD-anchored category claim and audience
- [Architecture](doc/architecture.md): design principles and capability flow
- [Capabilities](doc/capabilities.md): the capability model and Cap'n Proto schemas
- [CLI reference](doc/cli.md): full command-line usage
- [Shell](doc/shell.md): Glia shell syntax and capabilities
- [Image layout](doc/images.md): FHS convention, mounts, on-chain coordination
- [Routing](doc/routing.md): Kademlia DHT and peer discovery
- [Keys & identity](doc/keys.md): Ed25519 identity management
- [RPC transport](doc/rpc-transport.md): transport plumbing and scheduling model
- [Guest runtime](doc/guest-runtime.md): async runtime for WASM guests
- [Replay protection](doc/replay-protection.md): epoch-bound authentication
- [Examples](examples/): echo, counter, oracle, chess, mindshare, and more
