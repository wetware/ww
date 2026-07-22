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

Then run `make` yourself.  Builds host binary, kernel, shell, and
examples.  First build is slower — tell the user that's normal.

## Step 2 of 3: Run (~30 sec)

```
cargo run -- run std/kernel
```

Boots a libp2p swarm, loads the kernel WASM, drops into the Glia
shell.

## Step 3 of 3: Try it (~1 min)

```clojure
(perform host :id)              ;; your peer identity
(perform host :peers)           ;; connected peers
(perform host :addrs)           ;; listen addresses — round-trip RPC through the Membrane
(perform :exit nil)             ;; end the Glia session
```

⚗️ **That's it.**  You just booted a p2p capability-secured OS:
host, kernel, Membrane, shell.

## What happened (optional — ask first)

`ww run` did three things:

1. Started a **libp2p swarm** on the configured port
2. Loaded `std/kernel/bin/main.wasm` — the kernel Cell (pid0)
3. Spawned it with a **Membrane** — the capability hub that grants
   Host, Executor, IPFS, Routing, and Identity via Cap'n Proto RPC

The kernel grafted onto the Membrane, received epoch-scoped
capabilities, and launched the Glia shell.

## Next

> Ready to go deeper?  We can explore concepts, study an example,
> or start building something.

Suggest `/ww-concepts`, `/ww-examples`, or `/ww-build-app`.
