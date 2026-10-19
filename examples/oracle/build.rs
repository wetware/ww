fn main() {
    capnpc::CompilerCommand::new()
        .file("oracle.capnp")
        .run()
        .expect("failed to compile oracle.capnp");
    println!("cargo:rerun-if-changed=oracle.capnp");
}
