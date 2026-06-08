# Synapse Capability ABI Before Recursive Attenuation

WW uses `Synapse` as the single public capability currency at membrane and vat
boundaries. Native Cap'n Proto clients and Glia `defcap`s are implementation
backends; public transport carries a self-describing Synapse.

## Public ABI

`capnp/synapse.capnp` defines:

```capnp
struct Synapse {
  descriptor @0 :Descriptor;
  invokable @1 :Invokable;
}

interface Invokable {
  invoke @0 (method :MethodKey, payload :Payload) -> (result :Result);
}
```

Future richer behavior should use Cap'n Proto interface inheritance on
`Invokable`, not extra v1 fields on `Synapse`.

`Descriptor` is declaration and introspection metadata. It is not authority and
not behavioral proof. Authority is still carried only by the capability
reference.

## Mandatory Boundaries

These public schemas carry Synapse:

- `Membrane.Export { name, synapse }`
- `Process.bootstrap() -> (synapse)`
- `VatListener.serve(synapse, protocol)`
- `VatClient.dial(peer, protocol) -> (synapse)`
- std/system helpers such as `serve` and `serve_stdio`

`Export.name` remains the local binding key. It is not type authority.

Vat protocol strings remain service-name locators only. They do not carry type,
schema, provenance, or authority.

## Method Identity

Method identity is `(interfaceId, ordinal)`.

Names are diagnostic only. Descriptor validation rejects duplicate
`(interfaceId, ordinal)` entries.

## Payloads

V1 supports Cap'n Proto payloads and Glia values as explicit payload variants.
Application-level `AnyPointer` in lifted public interfaces is fail-closed.
Cap'n Proto internals and generic runtime plumbing are separate from application
schema `AnyPointer`.

## Lifting

Non-Glia public creation is lift-only:

```text
lift(nativeCapnpCapability, schemaMeta) -> Synapse
```

Glia `defcap` produces a Synapse-backed `Val::Cap`; invocation still goes
through `perform` and the effect system. The handler stack is checked before the
backend `Invokable.invoke` fallback.

## Recursive Attenuation Shape

Attenuation is an ordinary Synapse wrapper:

1. Validate the incoming descriptor.
2. Intersect allowed method keys with the wrapped descriptor.
3. On invoke, reject unknown or disallowed method keys.
4. Decode payload according to the descriptor.
5. Forward to the wrapped Synapse.
6. Recursively wrap returned Synapse fields before returning.

Policy composition is intersection by default. Missing schema, unknown methods,
unknown returned-cap fields, unsupported application `AnyPointer`, duplicate
method keys, and descriptor/invokable mismatch all fail closed.

## Removed Sidecars

`Export.schema`, schema-byte extra-cap tuples, `.capnpc` runtime artifacts,
schema-bin copying, and CLI schema-bin discovery are not part of the Synapse ABI.
Build-time schema extraction may still produce constants used to construct
descriptors, but schema bytes are no longer forwarded beside a capability.

## Explicit Non-Goals

- Recursive attenuation policy engine implementation.
- Public direct custom `Invokable` server API.
- Typed facades/codegen.
- Descriptor parse cache.
- Lazy schema registry.
- `VatConnection`, vat `listen`, schema-CID routing, executor provenance checks,
  or schema bytes as vat dial/listen parameters.
