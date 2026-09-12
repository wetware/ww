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

Guest crates are separate WASM link units. Each guest can compile the shared
schema graph locally because Rust values do not cross the process boundary.
The stable Cap'n Proto schema IDs provide wire compatibility between the host
and each guest.

## Current schema graph

`capnp/system.capnp` defines the typed `Membrane` bootstrap and imports three
schemas:

```capnp
using AuthSchema = import "auth.capnp";
using RoutingSchema = import "routing.capnp";
using HttpSchema = import "http.capnp";
```

A crate that generates `system_capnp.rs` must also generate the imported
`auth_capnp.rs`, `routing_capnp.rs`, and `http_capnp.rs` modules. The compiler
must resolve all four files from one stable source prefix.

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

Compile `stem.capnp` in the same command only when the crate also uses its
epoch and provenance types. `system.capnp` does not import `stem.capnp`.

The standalone `membrane.capnp` schema no longer exists. `Membrane`, `Export`,
`Network`, `Routing`, `Executor`, and the listener interfaces all live in
`system.capnp`.

## `crate_provides`

`CompilerCommand::crate_provides(crate_name, file_ids)` redirects references
to schema files whose generated Rust modules already belong to another crate.
First-party build scripts use this declaration for Cap'n Proto's standard
schema file:

```rust
.crate_provides("capnp", [0xa93fc509624c72d9])
```

`crate_provides` does not merge two copies of a project schema into one Rust
type. If a server implementation and its caller exchange generated Rust types
inside one native crate graph, both must import the same provider module.

## Build-script requirements

1. Use the repository `capnp/` directory as `src_prefix`.
2. Compile every imported project schema that must have a Rust module.
3. Emit `cargo:rerun-if-changed` for every compiled schema.
4. Use one provider crate for generated types shared inside a Rust crate graph.
5. Compile guest-local copies only across an RPC process boundary.

If a generated file refers to a missing module such as `crate::auth_capnp`, add
the imported schema to the same compiler command. If Rust reports two similarly
named but incompatible `Server` traits, remove the duplicate project-schema
generation and import the provider crate's generated module.
