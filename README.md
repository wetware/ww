# Wetware

[![CI](https://github.com/wetware/ww/actions/workflows/rust.yml/badge.svg)](https://github.com/wetware/ww/actions/workflows/rust.yml)

Wetware lets you safely run code you didn't write, don't trust, and cannot see: third-party MCP servers, code your LLM produced at runtime, tools other agents handed you across the swarm. It's a decentralized operating system for multi-tool agent swarms.

Cells are WASM processes with no ambient node-control authority. They receive
a fixed WASI substrate: stdio, arguments/environment, clocks, randomness, an
image-root filesystem, and private writable `/tmp`. Known-CID reads additionally
depend on the execution context's root or cache wiring. See the
[WASI host functions](doc/api/wasm-guest.md#wasi-host-functions).
Node-control operations require explicitly granted, typed Cap'n Proto
capabilities. Those references can be attenuated to a method allowlist; the
restriction travels with the reference across local and libp2p RPC boundaries
and recursively confines capabilities returned through it. Argument- and
resource-level filtering remain separate, application-level designs. Least
privilege is enforced by the runtime, not delegated to a prompt or to the
model running inside the cell.

## Try it in 60 seconds

This signed-install example applies to the Linux GNU ARM64 qualification
candidate. Native acceptance is still pending; other native targets refuse
guarded admission. Provision the independent release keyring and select
`RELEASE_CID` first; see [installation](doc/installation.md).

```sh
curl -fsSL https://wetware.run/install | sh -s -- --release "$RELEASE_CID"
export WW_COMPILE_WORKERS=2
# Admit these exact inputs while the installed Host is stopped.
ww startup admit --timeout-secs 300 run --executor-threads 2 --http-listen 127.0.0.1:2080
# Start in the foreground; query /status from another terminal.
ww run --executor-threads 2 --http-listen 127.0.0.1:2080
# In another terminal: curl http://localhost:2080/status
```

Keep the admitted working directory, environment, identity, and sources for
the run command. Changed inputs require stopped re-admission. The 300-second
budget is an explicit example, not a qualification result.

```json
{
  "status":       "ok",
  "version":      "0.1.0",
  "peer_id":      "12D3KooWRLf8DAFsNfbv3s2DjRMbUuPc8AYdcBfokZbz6kJ2aUss",
  "listen_addrs": ["/ip4/127.0.0.1/tcp/2025", "/ip6/::1/tcp/2025", ...],
  "peer_count":   216
}
```

The status request reaches a WebAssembly cell running inside the Host. The
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
curl -fsSL https://wetware.run/install | sh -s -- --release "$RELEASE_CID"
```

Set `RELEASE_CID` to the selected signed release and provision the independent
release keyring first. The installer authenticates and installs the Host,
baseline FHS, and identity. Automatic channel selection is not yet integrated.
Installation does not register a daemon or configure name publication. Register
separately with `ww daemon install`. Fresh guarded installations require explicit
stopped `ww startup admit` before activation. Native adapter qualification gates
apply to admission and replacement: Linux GNU ARM64 is a qualification candidate,
and other targets remain refused. See [native platform limits](doc/native-upgrades.md).
`ww image update` changes only the baseline and never restarts the Host.
`ww uninstall` preserves user state. `ww upgrade` uses authenticated native
selection, but replacement remains gated on native operational admission.
See [installation and recovery](doc/installation.md) for ownership, migration,
service activation, and outstanding upgrade integration gates.

Or build from source:

```bash
ww doctor                         # check your dev environment
rustup toolchain install nightly-2026-08-30 --component rust-src
make                              # build everything (host + std + examples)
```

Guest builds use the pinned native P3 toolchain. `ww doctor` reports missing
WASI SDK, `component-ld`, or `wasm-tools` dependencies. Current `ww run` boot
requires [Kubo](https://docs.ipfs.tech/install/), including installed and local-image
boot. Kubo provides the image/content backend; Composer merges image
directories in-process, and provider routing uses Wetware's own swarm. Guest
compilation and pure/unit tests do not require a running Kubo daemon. See
[deployment startup requirements](doc/deployment.md).

Repository builds use committed Cargo lockfiles. See the
[lockfile policy and dependency-update workflow](doc/dependencies/lockfiles.md).

### Run a node

For a guarded installation on the candidate platform, admit the exact invocation
before starting it. Stop the Host before changing and re-admitting its inputs:

```bash
export WW_COMPILE_WORKERS=2
ww startup admit --timeout-secs 300 run --executor-threads 2
ww run --executor-threads 2              # boot the admitted installed FHS
# After stopping the Host:
ww image check                          # validate static PID0 compatibility
ww startup admit --timeout-secs 300 run --executor-threads 2 .
ww run --executor-threads 2 .            # explicitly overlay the current directory
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

Mounts use `source[:/guest/target]`, for example `ww run ./config:/etc/myapp`.
Local sources freeze once at startup; Stem updates reuse those exact snapshots.
Use explicit local paths (`./app`, `/path`, or `~/app`). Bare `ww run` does not
mount CWD. It uses the installed FHS at `~/.ww/fhs`; `--fhs-root` selects another
baseline directory.

`ww run` starts a libp2p node on port 2025 and merges any [image layers](doc/images.md)
into a virtual FHS filesystem. Precedence, from lowest to highest, is installed
baseline, authoritative Stem head, frozen namespace images, then frozen CLI
operations. The Host selects PID0 from `/boot/kernel.wasm` in that exact effective
root for every epoch. Invalid selected kernels fail without an alternate-kernel
fallback.
`ww build` places the application component at
`bin/<project-directory>.wasm`. `ww push` imports that image tree and reports its
IPFS directory-root CID. The Host does not use the application component as PID0
input. See the [image lifecycle](doc/images.md#build-and-publication).

PID0 and ordinary children both receive a `Membrane`. PID0 receives a broad
root implementation. Each child receives the exact narrow implementation that
its parent passes to `Executor.spawn()`. Repeated `graft()` calls cannot add
authority because each server returns only its held references. After an epoch
transition, delegated host capabilities stay stale until an authorized ancestor
explicitly re-delegates fresh references or respawns the child.

[doc/architecture.md](doc/architecture.md) is the canonical reference; [doc/capabilities.md](doc/capabilities.md) is the capability surface.

### Cell modes

WASM processes ("cells") have no ambient node-control authority and receive
the WASI substrate described above. Async RPC Cells use the granted P3
transport. `WW_CELL_MODE` identifies separate application
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
cd examples                                 # from the repository root
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
- [Release rehearsal](doc/release-rehearsal.md): disposable genuine-artifact and consumer qualification
- [Image layout](doc/images.md): FHS convention, mounts, on-chain coordination
- [Provider routing](doc/routing.md): independent discovery and host-PeerID announcement capabilities
- [Keys & identity](doc/keys.md): Ed25519 identity management
- [RPC transport](doc/rpc-transport.md): transport plumbing and scheduling model
- [Guest runtime](doc/guest-runtime.md): async runtime for WASM guests
- [Runtime topology](doc/runtimes.md): host service threads and filesystem lifecycle
- [Local observability](doc/observability.md): reproduce a Cell incident and follow metrics, logs, and traces in Grafana
- [Replay protection](doc/replay-protection.md): epoch-bound authentication
- [Examples](examples/): echo, counter, oracle, chess, discovery, and snap-hello-rs
