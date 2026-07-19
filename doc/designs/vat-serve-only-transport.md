# Serve-Only Vat Transport

Schema authority is not part of vat transport. Vat transport serves and dials
capabilities by caller-chosen service-name protocol strings such as `greeter`,
`chess` or `oracle`.

The native vat publication flow is:

```text
spawn isolated cell
  -> get exported capability
  -> optionally wrap or attenuate it
  -> serve that capability
```

This keeps the capability boundary explicit. The cell provides isolation; the
exported capability is the object that gets forwarded over the network. Protocol
names are locators, not type authority and not provenance proofs.

## Public Interfaces

```capnp
interface VatListener {
  serve @0 (cap :Capability, protocol :Text) -> ();
}

interface VatClient {
  dial @0 (peer :Data, protocol :Text) -> (cap :Capability);
}
```

`VatListener.serve` publishes an already-existing capability. It does not spawn
cells and it does not install a per-request handler. Publisher lifecycle is
owned by the publisher that created the capability.

`VatClient.dial` opens `/ww/<version>/vat/<protocol>` and returns the remote
bootstrap capability.

## Non-Goals

The serve-only model intentionally does not include:

- schema-CID vat paths
- schema bytes as listen/dial parameters
- `VatConnection` metadata envelopes
- executor provenance checks
- per-connection vat cell spawning
- recursive attenuation

Recursive attenuation is still a required future feature, but it should be
designed after this transport simplification lands.

## Related Adapters

HTTP and stream listeners remain byte adapters. They still spawn cells per
request or stream because their job is to bridge external byte protocols into
WASI processes. Stream cells can receive explicit capability grants just like
HTTP cells.

HTTP/WAGI is intentionally the stateless request/response adapter. Long-lived
browser-facing sessions belong on the stream/WebSocket path, and
Wetware-native stateful services belong on vat RPC.

## Naming Guidance

Use `host :serve-vat` language as "publish an existing capability" rather than
"listen with a vat cell" or "register a vat handler." The service name is the
routing key; the served capability is the service object.
