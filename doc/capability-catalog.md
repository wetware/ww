# Capability catalog

Wetware’s capability catalog is static documentation for authors and tools. It
does not enumerate a node’s live services, prove that a capability is available,
or confer a reference. Names, catalog IDs, interface IDs, and schema CIDs are
labels and type metadata—not authorization or lookup keys.

The checked artifact is
[`doc/generated/capability-catalog.json`](generated/capability-catalog.json).
Generate or verify it with:

```sh
cargo run -p schema-id --bin capability-catalog -- generate
cargo run -p schema-id --bin capability-catalog -- check
```

## Source of truth

The generator joins two inputs:

1. Cap’n Proto `CodeGeneratorRequest` reflection supplies static facts:
   interface name and ID, canonical schema CID, and method names and ordinals.
2. [`capnp/capability-policy.json`](../capnp/capability-policy.json) supplies the
   small repository-policy overlay that schemas cannot express: conventional
   grant label, provider/effect classification, normal grantability,
   sensitivity, attenuation guidance, configuration requirements, examples,
   and security notes.

The overlay identifies an interface by ID and expected schema path. Generation
fails when the ID does not resolve to an interface, the path is wrong, or a
catalog ID or conventional name is duplicated. Method metadata is never copied
into the overlay. The JSON is sorted and contains no timestamp, secret, RPC
client, or live reference, so identical inputs produce identical bytes.

`provider` distinguishes repository conventions:

- `host-provided`: pid0 may receive the reference from the host graft;
- `host-derived`: a possessed host capability returns a narrower reference;
- `application-or-host-derived`: application policy or a host constructor may
  create the reference;
- `trusted-root` / `substrate-related`: bootstrap interfaces documented for
  architecture and tooling, not ordinary child grants.

Application-defined interfaces use the same Cap’n Proto mechanics but are not
automatically globally registered. Their parent-chosen grant-map keys remain
local labels. An application can use the generated catalog’s format as tooling
input without implying that a node has or can resolve the described reference.

## Glia discovery

The kernel embeds generator output and installs a data-only builtin:

```clojure
(capabilities)          ; complete static catalog as JSON text
(capabilities :runtime) ; one static entry as JSON text
(capabilities "ww.host")
```

Each response states that documentation does not imply runtime availability or
possession. The builtin has no `Session`, RPC client, effect handler, registry,
or environment-mutation access. An unknown label returns a structured error; it
never attempts service discovery or capability resolution.

Keep this distinct from possession-oriented introspection:

- `(schema cap)`, `(doc cap)`, and `(help cap)` require a capability value
  already present in the current Glia environment.
- A catalog entry is a known interface description.
- Runtime availability is node/configuration state.
- A local capability binding is an actually possessed reference.
- Only that possessed reference can be inserted into `:grants`.

The catalog deliberately does not enumerate current lexical bindings. Such an
enumerator is unnecessary for static discovery and would need a separate,
careful UI that reports only already-present values without reintroducing
lexical capture.
