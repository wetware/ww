# Cross-Crate Cap'n Proto Schema Sharing

## Rust type identity

`capnpc` generates Rust traits and client types in the crate that runs the
compiler. Two crates that compile the same schema get distinct Rust types.
Identical schema IDs make the wire protocols compatible, but they do not make
the generated Rust traits interchangeable.

Host-side crates therefore use one owner for shared generated types.
`crates/authority` compiles the repository schemas and exposes modules such as
`authority::system_capnp`, `authority::auth_capnp`, and
`authority::routing_capnp`. Host crates such as `crates/rpc` import those
modules instead of compiling another host-side copy.

RPC guests import the shared generated modules from `std/system`:
`system::system_capnp`, `system::auth_capnp`, `system::routing_capnp`, and
`system::http_capnp`. They do not generate another copy of the platform schemas.
Guests without an application schema need no `build.rs` or `capnpc` build
dependency. Non-RPC guests do not need to depend on `system`.

Host and guest components are separate link units. The stable Cap'n Proto
schema IDs provide wire compatibility across their RPC boundary. Native tests
can include both providers, but each server and its in-process callers must
use the same provider's Rust types. For example, native chess authority tests
use `authority::auth_capnp`, while the chess guest uses `system` bindings.

## Ownership

| Consumer | Generated locally | Shared platform provider |
| --- | --- | --- |
| Host crates | Application/test schemas where needed | `crates/authority` |
| `std/system` | Four platform schemas | `std/system` |
| Kernel, status, authority probe | None | `std/system` |
| Chess | `chess.capnp` | `std/system` |
| Oracle | `oracle.capnp` | `std/system` |
| Discovery | `greeter.capnp` | `std/system` |
| `ww init` RPC project | New project's schema | `std/system` |

The three example schemas and the generated scaffold schema have no platform
imports. Their build scripts compile only the application schema. The host
still generates `greeter.capnp` for its RPC test client. `crates/authority` and
`crates/membrane` retain their test schemas.

## Current schema graph

`capnp/system.capnp` defines the typed `Membrane` bootstrap and imports three
schemas:

```capnp
using AuthSchema = import "auth.capnp";
using RoutingSchema = import "routing.capnp";
using HttpSchema = import "http.capnp";
```

Generated `system_capnp.rs` refers to companion types through paths such as
`crate::auth_capnp::authority::Client`, `crate::routing_capnp::finder::Client`,
and `crate::http_capnp::http_client::Client`. Those modules must exist in the
provider crate unless generation redirects the imported paths to another
provider. Both platform providers generate all four files with a stable source
prefix. Importing `system::system_capnp` preserves its references to companion
modules inside `system`.

```rust
let schemas = [
    "system.capnp",
    "routing.capnp",
    "auth.capnp",
    "http.capnp",
];

let mut compiler = capnpc::CompilerCommand::new();
compiler.src_prefix(&capnp_dir);
for schema in schemas {
    compiler.file(capnp_dir.join(schema));
}
compiler.run().expect("failed to compile shared schemas");
```

Epoch lifecycle state is host-owned in `authority::Epoch`
(`crates/authority/src/epoch.rs`); it has no Cap'n Proto schema.

The standalone `membrane.capnp` schema no longer exists. `Membrane`, `Export`,
`Network`, `Routing`, `Executor`, and the listener interfaces all live in
`system.capnp`.

## `crate_provides`

`CompilerCommand::crate_provides(crate_name, file_ids)` redirects references
to schema files whose generated Rust modules already belong to another crate.
For example, the native provider declares Cap'n Proto's standard schema file:

```rust
.crate_provides("capnp", [0xa93fc509624c72d9])
```

`crate_provides` does not merge two copies of a project schema into one Rust
type. If a server implementation and its caller exchange generated Rust types
inside one native crate graph, both must import the same provider module.

If an application schema later imports platform schemas, map their file IDs to
`system` with `crate_provides`, and provide the schema import path. Pass only
the application schema to `.file()`. `crate_provides` redirects imported Rust
paths; it does not suppress generation of files explicitly passed to `.file()`.
The current application schemas need no provider mapping.

## Build-script requirements

1. Generate platform schemas only in `crates/authority` and `std/system`.
2. Keep application schemas in their application crate.
3. Use a stable `src_prefix` when compiling schemas outside the crate directory.
4. Emit `cargo:rerun-if-changed` for each compiled schema and relevant import.
5. Use one provider for types exchanged inside a Rust crate graph.

If generated application code refers to a missing platform module such as
`crate::auth_capnp`, configure the imported schema's provider. If Rust reports
two similarly named but incompatible `Server` traits, remove duplicate platform
generation and import the intended provider's generated module.

## Generation, compilation, linkage, and authority

These are separate properties:

1. **Generation:** `std/system` runs `capnpc` for the shared guest bindings.
   Each application runs `capnpc` only for its own schema, if any.
2. **Rust compilation:** rustc processes all included generated modules in
   `system`. Cargo can reuse that compilation within a compatible build graph;
   separate targets, features, profiles, or target directories can rebuild it.
3. **Final linkage:** optimization and linking determine which generated code
   and data survive in each component. Public modules do not imply that every
   generated implementation remains in the executable.
4. **Wire authority:** a guest can call capabilities it receives through RPC.
   Importing a generated type does not supply a capability reference or add
   a host grant.

Consolidating bindings changes generation and Rust type ownership. It does not
change schema IDs, method ordinals, graft policy, or capability transfer.
