fn main() {
    capnpc::CompilerCommand::new()
        .file("greeter.capnp")
        .run()
        .expect("failed to compile greeter.capnp");
    println!("cargo:rerun-if-changed=greeter.capnp");
}
