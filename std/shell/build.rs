use std::env;
use std::path::{Path, PathBuf};

/// Build script for the shell cell.
///
/// Compiles shell.capnp.
/// Shared schemas (system, routing, stem, http) are provided by the caps crate.
fn main() {
    let manifest_dir = env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR not set");
    let manifest_path = Path::new(&manifest_dir);
    let out_dir = PathBuf::from(env::var("OUT_DIR").unwrap());

    let capnp_dir = manifest_path
        .join("../..")
        .join("capnp")
        .canonicalize()
        .expect("capnp dir not found");

    let shell_schema = capnp_dir
        .join("shell.capnp")
        .canonicalize()
        .expect("shell.capnp not found in capnp/");

    // Shell schema + schema CID
    let raw_request = out_dir.join("shell_request.bin");
    capnpc::CompilerCommand::new()
        .src_prefix(&capnp_dir)
        .file(&shell_schema)
        .raw_code_generator_request_path(&raw_request)
        .run()
        .expect("failed to compile shell.capnp");

    let shell_id = find_interface_id(&raw_request, "Shell")
        .expect("Shell interface not found in CodeGeneratorRequest");

    let schemas = schema_id::extract_schemas(&raw_request, &[("SHELL", shell_id)])
        .expect("extract Shell schema");

    schema_id::emit_schema_consts(&out_dir.join("schema_ids.rs"), &schemas)
        .expect("emit schema consts");

    // Cargo rebuild triggers
    println!(
        "cargo:rerun-if-changed={}",
        capnp_dir.join("shell.capnp").display()
    );
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
