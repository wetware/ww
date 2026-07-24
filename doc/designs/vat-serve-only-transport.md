# Serve-Only Vat Transport

Schema authority is not part of vat transport. Vat transport serves and dials
capabilities by caller-chosen service-name protocol strings such as `greeter`,
`chess` or `oracle`.

The native vat publication flow is:

```text
spawn isolated cell
  -> get exported capability
  -> select recipient policy
  -> serve authenticated capability sessions
```

This keeps the capability boundary explicit. The cell provides isolation; the
exported capability is the shared application object behind per-stream authority
sessions. Protocol names are locators, not type authority and not provenance
proofs. Services that intentionally need no recipient authentication use the
separately named raw path.

## Public Interfaces

```capnp
interface VatListener {
  serveRaw @0 (cap :Capability, protocol :Text) -> ();
  serveAuthenticated @1 (
    cap :Capability,
    protocol :Text,
    policy :AuthSchema.AuthorityPolicy
  ) -> ();
}

interface VatClient {
  dial @0 (peer :Data, protocol :Text) -> (cap :Capability);
}
```

`VatListener.serveAuthenticated` compiles the deployer policy and creates a
fresh single-use Terminal for each stream. It does not spawn cells or duplicate
the application object. `VatListener.serveRaw` directly publishes an
already-existing capability as an explicit ungated escape hatch.

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

Use `host :serve-vat` language as "publish an authenticated capability
service" rather than "listen with a vat cell" or "register a vat handler."
The caller supplies the application capability, service name, and auth policy;
VatListener creates one single-use Terminal per inbound stream. The service
name is only a routing key, and the libp2p peer ID is not the authenticated
principal.

Use `host :serve-raw-vat` only when unauthenticated publication is intentional.
Its name is deliberately conspicuous because it exposes the supplied
capability directly to every peer that negotiates the protocol.
