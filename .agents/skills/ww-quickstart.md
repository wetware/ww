---
name: ww-quickstart
description: Build and run Wetware in 5 minutes (from source)
reads:
  - doc/ai-context.md
---

# Quickstart

Build and run Wetware in five minutes.  For first-time setup and
orientation, see `/ww-onboard` instead.

⚗️ Three steps.  ~5 minutes total.

## Step 1 of 3: Build (~2 min)

First, check prerequisites yourself:
- `rustc --version` — Rust toolchain installed?
- `ww doctor` — pinned native WASI P3 toolchain available?

Guest builds require nightly `nightly-2026-08-30` with `rust-src`, WASI SDK
34.0, and `wasm-tools` 1.258.0. Do not install `wasm32-wasip3` through rustup;
the build compiles the Tier 3 standard library from `rust-src`.

Then run `make` yourself. It builds the host binary, canonical kernel/status FHS,
and examples. The first build takes longer. Startup requires running Kubo and
an installed identity; first-time setup is covered by `/ww-onboard`.

## Step 2 of 3: Run (~30 sec)

```sh
cargo run --locked -- run --fhs-root "$PWD/target/fhs" --http-listen 127.0.0.1:2080
```

This command captures the built FHS as its baseline and boots a libp2p swarm.
The Host selects `/boot/kernel.wasm` from the effective root; the packaged Rust
kernel installs the shipped `/status` composition directly.

## Step 3 of 3: Try it (~1 min)

```sh
curl http://127.0.0.1:2080/status
```

The response reports `status: "ok"` and a non-null `peer_id`.

## What happened (optional — ask first)

`ww run` did three things:

1. Started a **libp2p swarm** on the configured port
2. Loaded `/boot/kernel.wasm` from the effective FHS as the kernel Cell (pid0)
3. Spawned it with a **Membrane** whose `graft()` returns typed `peerId`,
   `stat`, `network`, `routing`, `runtime`, `authority`, and `identity` fields.
   `extras` contains only application-defined capabilities.

The kernel grafted onto the Membrane, received epoch-scoped
capabilities, and installed `/status` with a narrow `Membrane` containing only
`peerId` and `Stat`.

Cells read content through WASI filesystem paths. Image paths resolve through
the active `CidTree`; `/ipfs/<cid>/...` also requires applicable root or cache
wiring. The guest VFS does not resolve `/ipns/...` or `/ipld/...` namespaces.

## Next

> Ready to go deeper?  We can explore concepts, study an example,
> or start building something.

Suggest `/ww-concepts`, `/ww-examples`, or `/ww-build-app`.
