use std::env;
use std::path::PathBuf;

fn main() {
    let capnp_dir = PathBuf::from(env::var_os("CARGO_MANIFEST_DIR").unwrap())
        .join("../../..")
        .join("capnp")
        .canonicalize()
        .expect("repository capnp directory");

    capnpc::CompilerCommand::new()
        .src_prefix(&capnp_dir)
        .crate_provides("capnp", [0xa93f_c509_624c_72d9])
        .file(capnp_dir.join("system.capnp"))
        .file(capnp_dir.join("routing.capnp"))
        .file(capnp_dir.join("auth.capnp"))
        .file(capnp_dir.join("membrane.capnp"))
        .file(capnp_dir.join("http.capnp"))
        .run()
        .expect("compile authority-probe schemas");

    for schema in ["system", "routing", "auth", "membrane", "http"] {
        println!(
            "cargo:rerun-if-changed={}",
            capnp_dir.join(format!("{schema}.capnp")).display()
        );
    }
}
