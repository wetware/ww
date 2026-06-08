use std::env;
use std::path::{Path, PathBuf};

fn main() {
    let manifest_dir = env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR not set");
    let capnp_dir = Path::new(&manifest_dir)
        .join("../..")
        .join("capnp")
        .canonicalize()
        .expect("capnp dir not found");

    let out_dir = PathBuf::from(env::var("OUT_DIR").unwrap());
    let raw_request = out_dir.join("schema_request.bin");

    capnpc::CompilerCommand::new()
        .src_prefix(&capnp_dir)
        // Tell capnpc that schema.capnp types live in the `capnp` crate,
        // not in this crate. Without this, generated code for stem.capnp
        // emits `crate::schema_capnp::node` instead of `::capnp::schema_capnp::node`.
        .crate_provides("capnp", [0xa93fc509624c72d9])
        .file(capnp_dir.join("system.capnp"))
        .file(capnp_dir.join("synapse.capnp"))
        .file(capnp_dir.join("routing.capnp"))
        .file(capnp_dir.join("auth.capnp"))
        .file(capnp_dir.join("membrane.capnp"))
        .file(capnp_dir.join("stem.capnp"))
        .file(capnp_dir.join("http.capnp"))
        .raw_code_generator_request_path(&raw_request)
        .run()
        .expect("failed to compile capnp schemas");

    // Extract schema CIDs for built-in capability interfaces.
    let schemas = schema_id::extract_schemas(
        &raw_request,
        &[
            ("HOST", 0x9ea7_0c8c_9aef_b70c),
            ("RUNTIME", 0x8738_4748_df10_173c),
            ("EXECUTOR", 0xbfa3_7c1e_99b4_a492),
            ("ROUTING", 0xc033_44a7_b0a3_17be),
            ("STREAM_LISTENER", 0xb216_08b1_a223_181b),
            ("STREAM_DIALER", 0xa7c3_62e6_7f22_5afa),
            ("VAT_LISTENER", 0xd64b_e194_6f81_a365),
            ("VAT_CLIENT", 0xa08a_8e8f_90a8_2679),
            ("IDENTITY", 0xa7c2_00e5_b472_6d89),
            ("HTTP_CLIENT", 0xf00a_15d0_9fb8_f360),
        ],
    )
    .expect("extract schemas");

    schema_id::emit_schema_consts(&out_dir.join("schema_ids.rs"), &schemas)
        .expect("emit schema consts");

    println!(
        "cargo:rerun-if-changed={}",
        capnp_dir.join("system.capnp").display()
    );
    println!(
        "cargo:rerun-if-changed={}",
        capnp_dir.join("synapse.capnp").display()
    );
    println!(
        "cargo:rerun-if-changed={}",
        capnp_dir.join("routing.capnp").display()
    );
    println!(
        "cargo:rerun-if-changed={}",
        capnp_dir.join("auth.capnp").display()
    );
    println!(
        "cargo:rerun-if-changed={}",
        capnp_dir.join("membrane.capnp").display()
    );
    println!(
        "cargo:rerun-if-changed={}",
        capnp_dir.join("stem.capnp").display()
    );
}
