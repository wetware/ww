fn main() {
    capnpc::CompilerCommand::new()
        .file("chess.capnp")
        .run()
        .expect("failed to compile chess.capnp");
    println!("cargo:rerun-if-changed=chess.capnp");
}
