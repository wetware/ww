use std::env;
use std::path::Path;

fn main() {
    let manifest_dir = env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR not set");
    let capnp_dir = Path::new(&manifest_dir)
        .join("../..")
        .join("capnp")
        .canonicalize()
        .expect("capnp dir not found");

    let schemas = ["system.capnp", "auth.capnp", "stem.capnp"];
    let mut compiler = capnpc::CompilerCommand::new();
    compiler
        .src_prefix(&capnp_dir)
        .crate_provides("capnp", [0xa93f_c509_624c_72d9]);
    for schema in schemas {
        compiler.file(capnp_dir.join(schema));
    }
    compiler.run().expect("failed to compile pid0 schemas");

    for schema in schemas {
        println!(
            "cargo:rerun-if-changed={}",
            capnp_dir.join(schema).display()
        );
    }
    println!(
        "cargo:rerun-if-changed={}",
        capnp_dir.join("membrane.capnp").display()
    );
}
