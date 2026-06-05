# VatConnection Schema Authority

## Status

Implemented design for the vat schema-authority cutover, including
executor-bound vat cells and persistent existing-cap publication.

## Problem

The old vat listener API used caller-provided schema bytes as route authority:

```capnp
VatListener.listen(handler :VatHandler, schema :Data, caps :List(Export))
VatClient.dial(peer :Data, schema :Data) -> (cap :AnyPointer)
```

That split trusted facts across inputs. A host can only make a trusted claim
about a vat service when the route registration, schema metadata, and spawned
implementation are derived from the same final WASM artifact. Passing an
executor plus unrelated schema bytes allows metadata and implementation to
diverge.

The trust model remains: the host is trusted, or it runs in a TEE that attests
the expected host artifact. The issue here is not that the host might lie. The
issue is what facts the trusted host derives from caller-controlled inputs.

## Chosen Model

Vat protocols are caller-chosen service names:

```text
/ww/<version>/vat/<service-name>
```

The protocol path is a locator only. It is not type authority, schema identity,
or implementation identity. Examples should use ordinary names such as
`greeter`, `chess`, `oracle`, and `auction`.

Schema and implementation identities are metadata:

- `schemaBundleCid`: `CIDv1(raw, BLAKE3(canonical SchemaBundle bytes))`
- `wasmArtifactCid`: `CIDv1(raw, BLAKE3(final WASM bytes))`

Remote callers dial a service name, call `describe()` to inspect host-derived
metadata without spawning, then call `bind()` to obtain the application
capability.

## Schema Artifact

Vat WASM artifacts embed a custom section named `ww.schema.v1`. The section body
is canonical Cap'n Proto bytes rooted at:

```capnp
using Schema = import "/capnp/schema.capnp";

struct SchemaBundle {
  formatVersion @0 :UInt16;
  serviceInterfaceId @1 :Schema.Id;
  nodes @2 :List(Schema.Node);
}
```

Validation rules:

- `formatVersion` must be `1`.
- `serviceInterfaceId` must match exactly one node in `nodes`.
- The matching node must be a Cap'n Proto interface.
- The bundle must parse as the typed `SchemaBundle`.
- Tooling is responsible for canonical encoding before insertion.

CIDs are transported as `Data` containing canonical `Cid::to_bytes()`. Text CIDs
are for logs, CLI output, docs, and operator-facing verification only.

## Runtime Semantics

`Runtime.load` is transport-neutral. It computes `wasmArtifactCid` for every
artifact and records the `ww.schema.v1` state if present:

```text
Absent
Valid { schemaBundle, schemaBundleCid }
Invalid { error }
```

`Runtime.load` still succeeds when the section is absent or invalid so raw,
stream, HTTP/WAGI, and direct process cells keep using the same
`Runtime.load -> Executor` path.

`VatListener.listen` is the point that requires valid vat metadata. It fails
clearly for absent or invalid `ww.schema.v1`. `HttpListener`,
`StreamListener`, and direct `Executor.spawn` ignore the section.

The custom section is not a cell-type marker. The serving mode is explicit:
vat, HTTP/WAGI, stream/raw, or direct process spawn.

## Public API

```capnp
struct VatServiceInfo {
  wasmArtifactCid @0 :Data;
  schemaBundleCid @1 :Data;
  schemaBundle @2 :SchemaBundle;
}

interface VatConnection {
  describe @0 () -> (info :VatServiceInfo);
  bind @1 () -> (schemaBundle :SchemaBundle, cap :AnyPointer);
}

interface VatListener {
  listen @0 (
    executor :Executor,
    protocol :Text,
    caps :List(MembraneSchema.Export)
  ) -> (wasmArtifactCid :Data, schemaBundleCid :Data);

  serve @1 (
    cap :AnyPointer,
    protocol :Text
  ) -> (wasmArtifactCid :Data, schemaBundleCid :Data);
}

interface VatClient {
  dial @0 (peer :Data, protocol :Text) -> (connection :VatConnection);
}
```

Semantics:

- `describe()` never spawns a cell.
- `bind()` lazily spawns once for that `VatConnection`.
- `serve()` connections bind to the already-existing published cap; no cell is
  spawned.
- Repeated `bind()` on the same `VatConnection` returns the same schema and cap.
- Dialing again creates a fresh `VatConnection`.
- Disconnect after `bind()` closes the spawned cell's stdin for executor-bound
  cells. Persistent-cap publications have no spawned stdin to close.

## Provenance Rule

`VatListener.listen` accepts only host-minted `Runtime.load` executors.
`Executor` is an object-capability interface, so untrusted guest code can
implement an object with the same interface and lie about artifact metadata.

The host runtime owns a `CapabilityServerSet<ExecutorImpl, executor::Client>`.
The RPC layer receives an injected provenance resolver, which recovers verified
executor metadata from host-minted executors and rejects fake executors before
protocol registration.

This is the general rule for host-policy claims: if an interface claims
host-enforced provenance or policy, consumers at that trust boundary must rely
on host-minted capability resolvers, not interface shape alone. `Executor` vat
metadata is enforced here; `http-client` host-policy mint checks are a P0
follow-up.

`VatListener.serve` takes no schema argument. It derives `VatServiceInfo` from
the WASM artifact that owns the caller's membrane session. The publisher
artifact must have a valid `ww.schema.v1` bundle; otherwise `serve()` fails
before registering the route. This is the safe replacement for the old
`VatHandler.serve` shape.

## Non-Goals

- No recursive attenuation in this effort.
- No caller-supplied schema authority.
- No Routing/DHT API changes.
- No schema-CID-derived vat route registration.
- No content-store schema publication in this cutover.

## Follow-Ups

- P0: reserve core graft names so extras cannot shadow host-minted caps such as
  `http-client`.
- P0: apply the same host-minted policy-cap check to `http-client` consumers at
  the appropriate trust boundary.
- P2: VatListener connection rate limiting.
