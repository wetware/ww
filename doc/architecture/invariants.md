# Producer-Authoritative Recursion Invariants

## Core invariants

1. Recursive attenuation authority is producer-sourced only.
2. `process.bootstrap()` takes no schema input and returns `TypedCap`.
3. `vat-client.dial(peer, descriptor)` and `vat-listener.listen(handler, descriptor, caps)` route by descriptor CID.
4. `TypedCap.schema` (`root` + `deps`) is the authority source for dynamic method-policy enforcement.
5. Unknown or malformed dynamic policy/schema cases fail closed.

## Descriptor identity

`VatDescriptor` is canonicalized as a Cap'n Proto message and hashed as:

`CIDv1(raw, BLAKE3(canonical VatDescriptor bytes))`

Current descriptor shape:

- `wasiCid: Data`
- `schemaCid: Data`

## Explicit non-goals in this cycle

- No caller-side schema assertion API surface (no `expect-cid`/`expect-schema`).
- No fallback from producer schema to caller hints.
