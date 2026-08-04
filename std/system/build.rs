use std::env;
use std::path::Path;

fn main() {
    let manifest_dir = env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR not set");
    let capnp_dir = Path::new(&manifest_dir)
        .join("../..")
        .join("capnp")
        .canonicalize()
        .expect("capnp dir not found");
    let membrane_schema = capnp_dir.join("membrane.capnp");

    capnpc::CompilerCommand::new()
        .src_prefix(&capnp_dir)
        .file(&membrane_schema)
        .run()
        .expect("failed to compile membrane.capnp");

    println!("cargo:rerun-if-changed={}", membrane_schema.display());
}
