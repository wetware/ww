use std::env;
use std::path::{Path, PathBuf};

/// Build script for the chess example crate.
///
/// This does two things:
///
/// 1. Compile Cap'n Proto schemas into Rust types so the chess WASM
///    guest can speak typed RPC with the host.
///
/// 2. Derive schema metadata from the ChessEngine interface definition and
///    write the SchemaBundle bytes later embedded in `ww.schema.v1`.
///
/// The schema pipeline:
///   chess.capnp  →  capnpc (CodeGeneratorRequest)
///                →  schema_id::extract_schemas / extract_schema_bundles
///                →  generated schema constants + chess_schema_bundle.bin
///                →  embedded in WASM custom section "ww.schema.v1"
fn main() {
    let manifest_dir = env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR not set");
    let manifest_path = Path::new(&manifest_dir);
    let out_dir = PathBuf::from(env::var("OUT_DIR").unwrap());

    // Locate the shared schema directory at the repo root. Every crate
    // that speaks Cap'n Proto RPC compiles these same definitions so
    // the wire types are consistent across host and guest.
    let capnp_dir = manifest_path
        .join("../..")
        .join("capnp")
        .canonicalize()
        .expect("capnp dir not found");

    // The chess-specific schema lives next to this crate's Cargo.toml.
    // It defines the ChessEngine interface that the guest exports and
    // peers consume over RPC.
    let local_schema = manifest_path
        .join("chess.capnp")
        .canonicalize()
        .expect("chess.capnp not found next to Cargo.toml");

    // ── Pass 1: shared schemas ──────────────────────────────────────
    // Compile the system-level .capnp files that every guest needs:
    // Host, Executor, IPFS, Routing, etc. These produce Rust modules
    // like `system_capnp::executor::Client`.
    capnpc::CompilerCommand::new()
        .src_prefix(&capnp_dir)
        // schema.capnp types live in the `capnp` crate
        .crate_provides("capnp", [0xa93fc509624c72d9])
        .file(capnp_dir.join("system.capnp"))
        .file(capnp_dir.join("routing.capnp"))
        .file(capnp_dir.join("http.capnp"))
        .file(capnp_dir.join("auth.capnp"))
        .file(capnp_dir.join("membrane.capnp"))
        .file(capnp_dir.join("stem.capnp"))
        .run()
        .expect("failed to compile shared capnp schemas");

    // ── Pass 2: chess-specific schema + schema bundle ───────────────
    // We need two outputs from chess.capnp:
    //   a) Rust types (ChessEngine client/server traits)
    //   b) The raw CodeGeneratorRequest binary, which contains the
    //      canonical encoding of every schema node. We feed this into
    //      schema_id to derive schema constants and the SchemaBundle.
    let raw_request = out_dir.join("chess_request.bin");
    capnpc::CompilerCommand::new()
        .src_prefix(manifest_path)
        .file(&local_schema)
        .raw_code_generator_request_path(&raw_request)
        .run()
        .expect("failed to compile chess.capnp");

    // Extract the canonical bytes for the ChessEngine interface node
    // (type ID 0xd0ac8299df079c61) and its transitive SchemaBundle.
    let schemas = schema_id::extract_schemas(&raw_request, &[("CHESS_ENGINE", 0xd0ac8299df079c61)])
        .expect("extract ChessEngine schema");
    let bundles =
        schema_id::extract_schema_bundles(&raw_request, &[("CHESS_ENGINE", 0xd0ac8299df079c61)])
            .expect("extract ChessEngine schema bundle");

    // Emit `CHESS_ENGINE_SCHEMA_CID` and `CHESS_ENGINE_SCHEMA_BYTES`
    // constants. The guest includes these via `include!(concat!(env!("OUT_DIR"), ...))`.
    schema_id::emit_schema_consts(&out_dir.join("schema_ids.rs"), &schemas)
        .expect("emit schema consts");

    // Write the raw canonical schema bytes and the transitive bundle. The
    // `make chess` target injects the bundle into the compiled WASM binary
    // as a custom section named "ww.schema.v1".
    schema_id::write_schema_bytes(&out_dir.join("chess_engine_schema.bin"), &schemas[0])
        .expect("write schema bytes");
    schema_id::write_schema_bundle_bytes(
        &out_dir.join("chess_engine_schema_bundle.bin"),
        &bundles[0],
    )
    .expect("write schema bundle bytes");

    // ── Cargo rebuild triggers ──────────────────────────────────────
    // Re-run this build script whenever any schema file changes.
    for schema in &["system", "routing", "auth", "membrane", "http", "stem"] {
        println!(
            "cargo:rerun-if-changed={}",
            capnp_dir.join(format!("{schema}.capnp")).display()
        );
    }
    println!("cargo:rerun-if-changed={}", local_schema.display());
}
