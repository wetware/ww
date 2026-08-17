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
- `rustup target list --installed | grep wasm32-wasip2` — present?
  If missing, run `rustup target add wasm32-wasip2`.

Then run `make` yourself. It builds the host binary, both kernels, the shell,
and examples. The first build takes longer.

## Step 2 of 3: Run (~30 sec)

```sh
cargo run -- run --http-listen 127.0.0.1:2080 std/status
```

This command boots a libp2p swarm with the embedded Rust kernel. The kernel
installs the shipped `/status` composition directly.

## Step 3 of 3: Try it (~1 min)

```sh
curl http://127.0.0.1:2080/status
```

The response reports `status: "ok"` and a non-null `peer_id`.

## What happened (optional — ask first)

`ww run` did three things:

1. Started a **libp2p swarm** on the configured port
2. Loaded embedded `std/kernel/bin/main.wasm` — the Rust kernel Cell (pid0)
3. Spawned it with a **Membrane** whose `graft()` returns a `List(Export)`.
   Canonical exports are `identity`, `host`, `runtime`, `routing`,
   `authority`, and `ipfs`; `http-client` appears when configured.

The kernel grafted onto the Membrane, received epoch-scoped
capabilities, and installed the `/status` cell with an explicit `host` grant.

## Next

> Ready to go deeper?  We can explore concepts, study an example,
> or start building something.

Suggest `/ww-concepts`, `/ww-examples`, or `/ww-build-app`.
