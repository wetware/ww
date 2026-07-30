//! Build-time helper for content-addressed Cap'n Proto schema identification.
//!
//! Extracts canonical schema bytes from a `CodeGeneratorRequest` and derives
//! deterministic CIDs for schema metadata, diagnostics, and tooling.
//!
//! # Usage in build.rs
//!
//! ```rust,no_run
//! use std::path::PathBuf;
//!
//! fn main() {
//!     let out_dir = PathBuf::from(std::env::var("OUT_DIR").unwrap());
//!     let raw_request = out_dir.join("schema_request.bin");
//!
//!     // Step 1: compile schemas, saving the raw CodeGeneratorRequest.
//!     capnpc::CompilerCommand::new()
//!         .src_prefix("../../")
//!         .file("chess.capnp")
//!         .raw_code_generator_request_path(&raw_request)
//!         .run()
//!         .expect("capnp compile");
//!
//!     // Step 2: extract schema bytes for specific interfaces.
//!     let schemas = schema_id::extract_schemas(
//!         &raw_request,
//!         &[("CHESS_ENGINE", 0xe3c2dfb1868218d1)],
//!     ).expect("extract schemas");
//!
//!     // Step 3: emit constants.
//!     schema_id::emit_schema_consts(&out_dir.join("schema_ids.rs"), &schemas)
//!         .expect("emit schema consts");
//! }
//! ```
//!
//! Then in your lib.rs:
//! ```rust,ignore
//! include!(concat!(env!("OUT_DIR"), "/schema_ids.rs"));
//! // Now available: CHESS_ENGINE_SCHEMA, CHESS_ENGINE_CID
//! ```

use std::path::Path;

/// A named schema with its canonical bytes and derived CID.
pub struct SchemaEntry {
    /// Const name prefix (e.g., "CHESS_ENGINE").
    pub name: String,
    /// The 64-bit Cap'n Proto type ID.
    pub type_id: u64,
    /// Canonical Cap'n Proto encoding of the schema.Node.
    pub canonical_bytes: Vec<u8>,
    /// CIDv1(raw, BLAKE3(canonical_bytes)) as a string.
    pub cid: String,
}

/// Extract canonical schema bytes for the given interface type IDs from a
/// raw `CodeGeneratorRequest` file (produced by `capnpc`'s
/// `raw_code_generator_request_path`).
///
/// `interfaces` is a list of `(const_name, type_id)` pairs, e.g.:
/// ```text
/// &[("CHESS_ENGINE", 0xe3c2dfb1868218d1)]
/// ```
pub fn extract_schemas(
    raw_request_path: &Path,
    interfaces: &[(&str, u64)],
) -> capnp::Result<Vec<SchemaEntry>> {
    let request_data = std::fs::read(raw_request_path).map_err(|e| {
        capnp::Error::failed(format!(
            "failed to read raw CodeGeneratorRequest at {}: {e}",
            raw_request_path.display()
        ))
    })?;

    let message_reader = capnp::serialize::read_message(
        &mut request_data.as_slice(),
        capnp::message::ReaderOptions::new(),
    )?;

    let request: capnp::schema_capnp::code_generator_request::Reader = message_reader.get_root()?;
    let nodes = request.get_nodes()?;

    let mut results = Vec::new();

    for &(name, target_id) in interfaces {
        let mut found = false;
        for node in nodes.iter() {
            if node.get_id() == target_id {
                // Serialize this single node into a fresh message, then canonicalize.
                let canonical_bytes = canonicalize_node(node)?;
                let cid = compute_cid(&canonical_bytes);

                results.push(SchemaEntry {
                    name: name.to_string(),
                    type_id: target_id,
                    canonical_bytes,
                    cid,
                });
                found = true;
                break;
            }
        }
        if !found {
            return Err(capnp::Error::failed(format!(
                "schema node with type ID 0x{target_id:016x} not found in CodeGeneratorRequest"
            )));
        }
    }

    Ok(results)
}

/// Canonicalize a single schema.Node into bytes.
///
/// Copies the node into a fresh single-segment message as root, then
/// calls Cap'n Proto's canonical serialization.
fn canonicalize_node(node: capnp::schema_capnp::node::Reader) -> capnp::Result<Vec<u8>> {
    let mut message = capnp::message::Builder::new_default();
    message.set_root_canonical(node)?;
    let segments = message.get_segments_for_output();
    assert_eq!(
        segments.len(),
        1,
        "canonical message should be single-segment"
    );
    Ok(segments[0].to_vec())
}

/// Compute CIDv1(raw, BLAKE3(data)).
pub fn compute_cid(data: &[u8]) -> String {
    let digest = blake3::hash(data);
    let mh = cid::multihash::Multihash::<64>::wrap(0x1e, digest.as_bytes())
        .expect("blake3 digest always fits in 64-byte multihash");
    cid::Cid::new_v1(0x55, mh).to_string()
}

/// Emit Rust source with schema constants.
///
/// For each entry, generates:
/// ```rust,ignore
/// /// Canonical schema bytes for {name} (type ID 0x{type_id}).
/// pub const {NAME}_SCHEMA: &[u8] = &[...];
/// /// Content-addressed ID: CIDv1(raw, BLAKE3(canonical schema)).
/// pub const {NAME}_CID: &str = "...";
/// ```
pub fn emit_schema_consts(output_path: &Path, schemas: &[SchemaEntry]) -> std::io::Result<()> {
    use std::fmt::Write as _;
    use std::io::Write;

    let mut code = String::new();
    writeln!(code, "// Auto-generated by schema-id. Do not edit.").unwrap();
    writeln!(code).unwrap();

    for entry in schemas {
        writeln!(
            code,
            "/// Canonical schema bytes for {} (type ID 0x{:016x}).",
            entry.name, entry.type_id
        )
        .unwrap();
        write!(code, "pub const {}_SCHEMA: &[u8] = &[", entry.name).unwrap();
        for (i, byte) in entry.canonical_bytes.iter().enumerate() {
            if i > 0 {
                write!(code, ", ").unwrap();
            }
            if i % 16 == 0 {
                writeln!(code).unwrap();
                write!(code, "    ").unwrap();
            }
            write!(code, "0x{byte:02x}").unwrap();
        }
        writeln!(code, "\n];").unwrap();
        writeln!(code).unwrap();
        writeln!(
            code,
            "/// Content-addressed ID: CIDv1(raw, BLAKE3(canonical schema))."
        )
        .unwrap();
        writeln!(
            code,
            "pub const {}_CID: &str = \"{}\";",
            entry.name, entry.cid
        )
        .unwrap();
        writeln!(code).unwrap();
    }

    let mut file = std::fs::File::create(output_path)?;
    file.write_all(code.as_bytes())?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_compute_cid_deterministic() {
        let data = b"test schema node canonical bytes";
        let cid1 = compute_cid(data);
        let cid2 = compute_cid(data);
        assert_eq!(cid1, cid2);
    }

    #[test]
    fn test_compute_cid_different_inputs() {
        let cid1 = compute_cid(b"\x00\x00\x00\x01 schema A");
        let cid2 = compute_cid(b"\x00\x00\x00\x02 schema A");
        assert_ne!(cid1, cid2);
    }

    #[test]
    fn test_compute_cid_is_valid() {
        let cid_str = compute_cid(b"some data");
        let parsed: cid::Cid = cid_str.parse().expect("should parse as CID");
        assert_eq!(parsed.version(), cid::Version::V1);
        assert_eq!(parsed.codec(), 0x55); // raw
    }

    /// Roundtrip: push schema bytes to IPFS and verify CID + content match.
    /// Requires a running Kubo daemon with BLAKE3 support.
    #[test]
    #[ignore]
    fn test_ipfs_roundtrip() {
        use std::io::Write;
        use std::process::Command;

        let data = b"test schema bytes for IPFS roundtrip";
        let expected_cid = compute_cid(data);

        // Push
        let mut child = Command::new("ipfs")
            .args(["block", "put", "--mhtype=blake3", "--cid-codec=raw"])
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .spawn()
            .expect("ipfs must be on PATH");

        child.stdin.as_mut().unwrap().write_all(data).unwrap();
        let output = child.wait_with_output().unwrap();
        assert!(output.status.success(), "ipfs block put failed");

        let ipfs_cid = String::from_utf8_lossy(&output.stdout).trim().to_string();
        assert_eq!(ipfs_cid, expected_cid, "CID mismatch");

        // Fetch back
        let get = Command::new("ipfs")
            .args(["block", "get", &expected_cid])
            .output()
            .expect("ipfs block get failed");
        assert!(get.status.success());
        assert_eq!(
            &get.stdout,
            data.as_slice(),
            "bytes mismatch after roundtrip"
        );
    }
}
