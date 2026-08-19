use std::env;
use std::path::Path;

/// Build script for the chess example crate.
///
/// Compiles Cap'n Proto schemas into Rust types so the chess WASM guest can
/// speak typed RPC with the host.
fn main() {
    let manifest_dir = env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR not set");
    let manifest_path = Path::new(&manifest_dir);

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

    // ── Pass 2: chess-specific schema ───────────────────────────────
    capnpc::CompilerCommand::new()
        .src_prefix(manifest_path)
        .file(&local_schema)
        .run()
        .expect("failed to compile chess.capnp");

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
