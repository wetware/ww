---
name: ww-reference
description: Capability schemas, CLI flags, and API reference
reads:
  - doc/ai-context.md
  - doc/cli.md
  - doc/capabilities.md
  - doc/rpc-transport.md
  - doc/keys.md
  - doc/guest-runtime.md
---
# Browse Reference

Deep-dive into schemas, CLI, and APIs.

## Start with what they need

Don't present a wall of options cold.  Ask:

> What are you looking up?  If you tell me what you're trying to
> do, I can point you to the right thing.

If they already know what they want, jump straight there.

If they want to browse, show the menu:

> Pick a topic, or tell me what you're trying to do:
>
> 1. **Cell types** — raw, http, capnp, pid0 (`capnp/cell.capnp`)
> 2. **System capabilities** — Host, Executor, Process, streams
>    (`capnp/system.capnp`)
> 3. **Membrane & auth** — Terminal, Membrane, Epoch, Identity
>    (`capnp/stem.capnp`)
> 4. **Routing / DHT** — provide, findProviders
>    (`capnp/routing.capnp`, `doc/routing.md`)
> 5. **CLI** — flags, subcommands, env vars (`doc/cli.md`)
> 6. **RPC transport** — duplex streams, scheduling (`doc/rpc-transport.md`)
> 7. **schema-inject** — post-build cell type injection
>    (`crates/schema-id/src/bin/schema-inject.rs`)
> 8. **Signing & keys** — Signer interface, key derivation
>     (`doc/keys.md`)
> 9. **Cross-crate schemas** — sharing Cap'n Proto definitions
>     across crates (`doc/capnp-cross-crate.md`)
> 10. **Guest API** — WASI bindings for guest WASM modules
>     (`doc/api/wasm-guest.md`)
> 11. **Guest runtime** — poll loop, Cap'n Proto RPC, and WASI
>     integration (`doc/guest-runtime.md`)
> 12. **Design docs** — historical and current design records
>     (`doc/designs/`; check each document's status header)

## How to present reference material

When walking through a `.capnp` file:
- Explain each interface and method in **plain language first**
- Then show the schema definition
- One interface at a time — don't dump the whole file

For `schema-inject`, run `cargo run -p schema-id --bin schema-inject -- --help`
yourself and show the user the actual CLI output.  Then walk through
the three modes with examples:
- `--raw bitswap` — raw libp2p streams
- `--http /api/v1` — HTTP/FastCGI routing
- `--capnp schema.bytes [--no-ipfs]` — typed Cap'n Proto RPC

Note: `--no-ipfs` (capnp only) skips pushing canonical schema bytes
to IPFS via Kubo.  Useful offline or when Kubo isn't running.
Protocol IDs for raw cells must not contain `/` (host prefixes
`/ww/0.1.0/stream/` automatically).

## After each topic

> Found what you needed?  Want to look at something else, or
> try a different skill?

Suggest other `/ww-*` skills as appropriate.
