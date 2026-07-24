# Single-Authority Capability Model

Status: ADOPTED (2026-07). Implemented across #563 (isolate removal),
#564 (`wetware-membrane` crate), #566 (`Export { name, cap }` ABI cutover),
#568 (production membrane recursion), and the `(attenuate ...)`
reification PR. Supersedes the `Invokable`-as-currency portions of
`uniform-ww-capability-abi.md`; introspection, method-identity, and
fail-closed rules from that document are unchanged.

## Decision

Cap'n Proto capability references, attenuated by hook-level membranes,
are the sole authority model in WW. Glia is a client of that model, not
a second enforcement layer. The public ABI carries no Glia types.

## Layering Rules

1. **Enforcement lives at the capnp hook layer.** Any policy that must
   hold after a capability crosses a boundary (membrane graft, vat
   dial/serve, process bootstrap, stdio serve) is a membrane wrapper on
   the capability reference itself: a `ClientHook` proxy
   (`crates/membrane`) that filters on `(interfaceId, ordinal)`,
   fails closed on unknown or denied methods, and recursively wraps
   capabilities in results, promise pipelines, and promise resolution.
   Casting is safe because filtering happens below the type layer.

2. **Glia's handler stack is local semantics only.** `perform`
   interception, env bindings, and `with-effect-handler` remain the
   programmable interposition surface *inside* a cell. They are never
   load-bearing across a boundary.

3. **Glia reifies policy to share it.** `(attenuate cap [:method ...])`
   on a capnp-backed cap returns a new `Val::Cap` backed by a
   membrane-wrapped hook. The policy travels with the reference,
   enforced by the hook, with no evaluator in the loop. (Mechanism
   below.)

4. **The public schema is Glia-free.** Glia data crossing a boundary is
   encoded as ordinary Cap'n Proto structs or bytes at the edge, like
   any other guest language.

## Public ABI Shape

The Synapse struct and the `Invokable.invoke` calling convention are
removed (#566). Boundaries carry the capability directly:

```capnp
struct Export {
  name @0 :Text;        # local binding key, not authority
  cap  @1 :Capability;
}
```

- **Attenuation is opaque** (E/CapTP alignment). An attenuated
  capability advertises its schema, not its policy; denied methods fail
  closed at call time. There is no `allowedMethods` sidecar — a method
  list beside a cap would be an unverifiable claim, exactly the
  descriptor-is-not-authority failure mode. Where a granter wants the
  holder to know the effective surface (e.g. agent/MCP UX), it passes
  documentation as ordinary data alongside the grant; that is
  application convention, not ABI.
- Method identity is `(interfaceId, ordinal)`. Names are diagnostic
  only.
- Callers use typed Cap'n Proto clients directly.

### Schema association is deferred (D24 amendment)

Earlier drafts carried `Export.schemaCid` and a schema-by-CID
distribution design. That is deferred wholesale: every consumer of
runtime schema fetch is downstream of dynamic invocation, which
capnp-rust (0.25.x) cannot do — `dynamic_value` works only with
compile-time schemas; there is no runtime `Schema.Node` loading.
Enumeration from fetched bytes works today, but nothing can *call* with
it, so the field would be dead weight. Cap'n Proto field evolution makes
adding it later compatible and free.

Three candidate architectures are recorded for when upstream dynamic
invocation lands, each deserving its own design pass:
1. a per-`Export` `schemaCid` field (schema travels beside the cap);
2. an `interfaceId -> schemaCid` registry reachable via `routing`;
3. a reflection capability (ask the cap for its own schema).

## Attenuation Mechanism (implemented)

`(attenuate cap [:method ...])` dispatches on what backs the cap:

- **capnp-backed caps** are reified by the kernel through
  `glia::eval::Dispatch::reify_attenuation`:
  1. Method keywords are resolved against the cap's compiled canonical
     `schema.Node` (kebab-case keyword → camelCase capnp name; ordinal =
     position in the interface's method list). Unknown names fail closed
     at attenuation time.
  2. The client hook is wrapped in a `wetware-membrane` `Allowlist` keyed by
     `(interfaceId, ordinal)`.
  3. The result is a `glia::HandledCapInner` cap: its carried handler is
     the kernel's typed adapter rebuilt over the MEMBRANED client and
     gated by the same allowlist, so local `perform` sees the same
     policy — and the same structured `:glia.error/permission-denied` —
     as remote callers. Its export payload is the membraned
     `capnp::capability::Client`, which `extract_capnp_client` returns,
     so every boundary path (`:listen` cap forwarding, `host
     :serve-vat`) publishes the restricted cap.
  Re-attenuation intersects allowlists and collapses to a single
  membrane layer at the hook. Membrane overhead is sub-microsecond and
  constant per layer (`crates/membrane/benches/membrane.rs`).

- **capnp-backed caps without a compiled schema** (e.g. a generic cap
  obtained from a vat dial) fail closed: ordinals cannot be resolved,
  and falling back to a string check the wire would not enforce is
  exactly the failure mode this design removes. Unblocking this is the
  deferred schema-association design above.

- **Pure-Glia caps (`defcap`)** keep the evaluator-local
  `AttenuatedCapInner` path. This is a deliberate narrowing of "delete
  the second mechanism": a defcap cap is a table of Glia closures with
  no capnp client, so it *cannot cross a boundary* — there is nothing to
  export and nothing for a membrane to wrap. Inside one evaluator, the
  local allowlist is dynamic-scope interposition within a single trust
  domain, not competing enforcement. The reification invariant below
  binds the future case.

## Reification Invariant

Any Glia-constructed capability that crosses a boundary is
membrane-governed. Today that is every capnp-backed cap (enforced by
construction: boundary paths obtain the client via
`extract_capnp_client`, which returns the membraned client for
attenuated caps). When the deferred defcap-export bridge
(`GliaCapInner` → capnp server) is built, it MUST:
1. produce caps governed by the SAME membrane mechanism as every other
   cap — no second enforcement path, no per-interface filtered proxies;
2. reify `attenuate` on such caps into the membrane exactly as for
   grafted caps;
3. reuse `crates/membrane`; anything the crate cannot express is a
   signal to fix the crate, not to fork a path.

## One Mechanism, Three Configuration Surfaces

`doc/capabilities.md` describes three "attenuation points" (membrane
graft, root Atom binding, Glia env). These are three *configuration
surfaces* over the same authority model, not three enforcement
mechanisms: the graft decides which capability references enter the
cell; the Atom root decides the reachable content subgraph; Glia env
bindings decide which references code inside the cell can name. Where
policy must survive a boundary crossing, it is always the hook-level
membrane.

## What This Removed

- `Synapse { descriptor, invokable }` as a struct and as a concept.
- `Invokable.invoke(MethodKey, Payload)` as the public call path.
- `Payload` / `Result` / `Value` / `PayloadCodec` from the public schema.
- The dual call convention: hook-cast consumers and invoke() consumers
  can no longer diverge, because there is only one convention.
- `isolate` (weak isolation-vs-attenuation separation; attenuation is
  the membrane, isolation is a spawned cell — see TODOS for the
  revisit-gate).

## Non-Goals

- Argument-level attenuation (payload inspection). Method-level only;
  argument policy requires schema-driven payload decoding via capnp
  dynamic reflection and is deferred with schema association.
- Schema-driven Glia dispatch. Hand-written kernel adapters remain;
  replacing them requires upstream capnp runtime schema loading. Either
  way it is a kernel implementation detail with no ABI change.
- Removing Glia. The duality is resolved by layering, not deletion: the
  core is Glia-free; the shell, MCP eval surface, and init.d keep Glia
  as userland.
