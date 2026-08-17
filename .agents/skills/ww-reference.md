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
> 1. **Guest and service transports** — host bootstrap, byte streams, HTTP,
>    and Cap'n Proto vats (`doc/api/wasm-guest.md`, `capnp/system.capnp`)
> 2. **System capabilities** — Host, Executor, Process, streams
>    (`capnp/system.capnp`)
> 3. **Membrane & auth** — Terminal, Membrane, Epoch, Identity
>    (`capnp/stem.capnp`)
> 4. **Routing / DHT** — provide, findProviders
>    (`capnp/routing.capnp`, `doc/routing.md`)
> 5. **CLI** — flags, subcommands, env vars (`doc/cli.md`)
> 6. **RPC transport** — duplex streams, scheduling (`doc/rpc-transport.md`)
> 7. **Signing & keys** — Signer interface, key derivation
>     (`doc/keys.md`)
> 8. **Cross-crate schemas** — sharing Cap'n Proto definitions
>     across crates (`doc/capnp-cross-crate.md`)
> 9. **Guest API** — WASI bindings for guest WASM modules
>     (`doc/api/wasm-guest.md`)
> 10. **Guest runtime** — poll loop, Cap'n Proto RPC, and WASI
>     integration (`doc/guest-runtime.md`)
> 11. **Design docs** — historical and current design records
>     (`doc/designs/`; check each document's status header)

## How to present reference material

When walking through a `.capnp` file:
- Explain each interface and method in **plain language first**
- Then show the schema definition
- One interface at a time — don't dump the whole file

For service transports, show the relevant `capnp/system.capnp` method. Explain
that the registration or publication call selects the transport. The host does
not read a custom section to select one. Protocol names must not contain `/`;
the host adds `/ww/0.1.0/stream/` or `/ww/0.1.0/vat/`.

## After each topic

> Found what you needed?  Want to look at something else, or
> try a different skill?

Suggest other `/ww-*` skills as appropriate.
