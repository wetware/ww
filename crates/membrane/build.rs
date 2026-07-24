fn main() {
    // Compiles the toy `Thing` interface used only by the crate's integration
    // tests (cast-bypass, recursive rewrap, pipelining, twoparty RPC).
    capnpc::CompilerCommand::new()
        .file("test_thing.capnp")
        .run()
        .expect("failed to compile test_thing.capnp");
}
