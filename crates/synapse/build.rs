use std::path::PathBuf;

fn main() {
    let capnp_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../capnp");
    capnpc::CompilerCommand::new()
        .src_prefix(&capnp_dir)
        .file(capnp_dir.join("synapse.capnp"))
        .run()
        .expect("compile synapse.capnp");

    println!(
        "cargo:rerun-if-changed={}",
        capnp_dir.join("synapse.capnp").display()
    );
}
