use std::path::PathBuf;

fn main() {
    let capnp_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../capnp");
    let out_dir = PathBuf::from(std::env::var("OUT_DIR").unwrap());
    let raw_request = out_dir.join("schema_request.bin");

    capnpc::CompilerCommand::new()
        .src_prefix("../../")
        // Tell capnpc that schema.capnp types live in the `capnp` crate.
        .crate_provides("capnp", [0xa93fc509624c72d9])
        .file(capnp_dir.join("system.capnp"))
        .file(capnp_dir.join("routing.capnp"))
        .file(capnp_dir.join("auth.capnp"))
        .file(capnp_dir.join("membrane.capnp"))
        .file(capnp_dir.join("stem.capnp"))
        .file(capnp_dir.join("http.capnp"))
        .raw_code_generator_request_path(&raw_request)
        .run()
        .expect("capnp compile schemas");

    // Extract canonical Schema.Node bytes for the capability interfaces
    // grafted by MembraneServer. `src/rpc/membrane.rs` uses these at
    // graft time to populate `Export.schema` so guests (and MCP tools)
    // can introspect each capability's interface without hardcoded
    // descriptions. Type IDs are stable across renames; see the capnp
    // `@0x...` annotations on each interface.
    let schemas = schema_id::extract_schemas(
        &raw_request,
        &[
            ("HOST", 0x9ea7_0c8c_9aef_b70c),
            ("RUNTIME", 0x8738_4748_df10_173c),
            ("ROUTING", 0xc033_44a7_b0a3_17be),
            ("IDENTITY", 0xa7c2_00e5_b472_6d89),
            ("HTTP_CLIENT", 0xf00a_15d0_9fb8_f360),
        ],
    )
    .expect("extract capability schemas");

    schema_id::emit_schema_consts(&out_dir.join("schema_ids.rs"), &schemas)
        .expect("emit schema consts");

    for schema in &["system", "routing", "auth", "membrane", "stem", "http"] {
        println!(
            "cargo:rerun-if-changed={}",
            capnp_dir.join(format!("{schema}.capnp")).display()
        );
    }
}
