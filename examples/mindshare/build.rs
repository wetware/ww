use std::env;
use std::path::{Path, PathBuf};

/// Build script for the mindshare example.
///
/// Compiles mindshare.capnp and shared system schemas, extracts the
/// Mindshare interface's canonical bytes, and derives its schema CID metadata.
fn main() {
    let manifest_dir = env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR not set");
    let manifest_path = Path::new(&manifest_dir);
    let out_dir = PathBuf::from(env::var("OUT_DIR").unwrap());

    let capnp_dir = manifest_path
        .join("../..")
        .join("capnp")
        .canonicalize()
        .expect("capnp dir not found");

    let local_schema = manifest_path
        .join("mindshare.capnp")
        .canonicalize()
        .expect("mindshare.capnp not found next to Cargo.toml");

    // Pass 1: shared schemas
    capnpc::CompilerCommand::new()
        .src_prefix(&capnp_dir)
        // schema.capnp types live in the `capnp` crate
        .crate_provides("capnp", [0xa93fc509624c72d9])
        .file(capnp_dir.join("system.capnp"))
        .file(capnp_dir.join("routing.capnp"))
        .file(capnp_dir.join("auth.capnp"))
        .file(capnp_dir.join("membrane.capnp"))
        .file(capnp_dir.join("stem.capnp"))
        .file(capnp_dir.join("http.capnp"))
        .run()
        .expect("failed to compile shared capnp schemas");

    // Pass 2: mindshare schema + schema CID
    let raw_request = out_dir.join("mindshare_request.bin");
    capnpc::CompilerCommand::new()
        .src_prefix(manifest_path)
        .file(&local_schema)
        .raw_code_generator_request_path(&raw_request)
        .run()
        .expect("failed to compile mindshare.capnp");

    let mindshare_id = find_interface_id(&raw_request, "Mindshare")
        .expect("Mindshare interface not found in CodeGeneratorRequest");

    let schemas = schema_id::extract_schemas(&raw_request, &[("MINDSHARE", mindshare_id)])
        .expect("extract Mindshare schema");

    schema_id::emit_schema_consts(&out_dir.join("schema_ids.rs"), &schemas)
        .expect("emit schema consts");

    schema_id::write_schema_bytes(&out_dir.join("mindshare_schema.bin"), &schemas[0])
        .expect("write schema bytes");

    // Cargo rebuild triggers
    for schema in &["system", "routing", "auth", "membrane", "stem", "http"] {
        println!(
            "cargo:rerun-if-changed={}",
            capnp_dir.join(format!("{schema}.capnp")).display()
        );
    }
    println!("cargo:rerun-if-changed={}", local_schema.display());
}

/// Scan a raw CodeGeneratorRequest for an interface node with the given
/// display name and return its type ID.
fn find_interface_id(raw_request_path: &Path, name: &str) -> Option<u64> {
    let data = std::fs::read(raw_request_path).ok()?;
    let reader =
        capnp::serialize::read_message(&mut data.as_slice(), capnp::message::ReaderOptions::new())
            .ok()?;
    let request: capnp::schema_capnp::code_generator_request::Reader = reader.get_root().ok()?;
    for node in request.get_nodes().ok()?.iter() {
        if let Ok(n) = node.get_display_name() {
            if n.to_str().ok()?.ends_with(&format!(":{name}")) || n.to_str().ok()? == name {
                if matches!(
                    node.which(),
                    Ok(capnp::schema_capnp::node::Which::Interface(_))
                ) {
                    return Some(node.get_id());
                }
            }
        }
    }
    None
}
