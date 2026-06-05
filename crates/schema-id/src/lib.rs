//! Build-time helper for content-addressed Cap'n Proto schema identification.
//!
//! This crate owns byte-level schema authority. It can still extract legacy
//! canonical `Schema.Node` bytes for membrane graft metadata, and now also
//! builds and validates canonical `SchemaBundle` bytes suitable for embedding
//! in the `ww.schema.v1` WASM custom section.

use std::fmt;
use std::path::Path;

#[allow(
    dead_code,
    unused_parens,
    clippy::extra_unused_type_parameters,
    clippy::match_single_binding
)]
mod system_capnp {
    include!(concat!(env!("OUT_DIR"), "/capnp/system_capnp.rs"));
}

#[allow(
    dead_code,
    unused_parens,
    clippy::extra_unused_type_parameters,
    clippy::match_single_binding
)]
mod routing_capnp {
    include!(concat!(env!("OUT_DIR"), "/capnp/routing_capnp.rs"));
}

#[allow(
    dead_code,
    unused_parens,
    clippy::extra_unused_type_parameters,
    clippy::match_single_binding
)]
mod auth_capnp {
    include!(concat!(env!("OUT_DIR"), "/capnp/auth_capnp.rs"));
}

#[allow(
    dead_code,
    unused_parens,
    clippy::extra_unused_type_parameters,
    clippy::match_single_binding
)]
mod membrane_capnp {
    include!(concat!(env!("OUT_DIR"), "/capnp/membrane_capnp.rs"));
}

#[allow(
    unused_parens,
    dead_code,
    clippy::extra_unused_type_parameters,
    clippy::match_single_binding
)]
mod stem_capnp {
    include!(concat!(env!("OUT_DIR"), "/capnp/stem_capnp.rs"));
}

#[allow(
    dead_code,
    unused_parens,
    clippy::extra_unused_type_parameters,
    clippy::match_single_binding
)]
mod http_capnp {
    include!(concat!(env!("OUT_DIR"), "/capnp/http_capnp.rs"));
}

/// Current `SchemaBundle.formatVersion`.
pub const SCHEMA_BUNDLE_FORMAT_VERSION: u16 = 1;

/// WASM custom-section name containing canonical `SchemaBundle` bytes.
pub const SCHEMA_SECTION_NAME: &str = "ww.schema.v1";

/// CIDv1 raw codec.
pub const CID_RAW_CODEC: u64 = 0x55;

/// Multihash code for BLAKE3.
pub const MULTIHASH_BLAKE3_CODE: u64 = 0x1e;

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
    /// CIDv1(raw, BLAKE3(canonical_bytes)) as canonical CID bytes.
    pub cid_bytes: Vec<u8>,
}

/// A named transitive schema bundle with derived identity.
pub struct SchemaBundleEntry {
    /// Const name prefix (e.g., "CHESS_ENGINE").
    pub name: String,
    /// The 64-bit Cap'n Proto type ID of the exported service interface.
    pub service_interface_id: u64,
    /// Canonical Cap'n Proto encoding of `system.SchemaBundle`.
    pub canonical_bytes: Vec<u8>,
    /// CIDv1(raw, BLAKE3(canonical_bytes)) as a string.
    pub cid: String,
    /// CIDv1(raw, BLAKE3(canonical_bytes)) as canonical CID bytes.
    pub cid_bytes: Vec<u8>,
}

/// A validated canonical `SchemaBundle`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ValidatedSchemaBundle {
    pub format_version: u16,
    pub service_interface_id: u64,
    pub canonical_bytes: Vec<u8>,
    pub cid: String,
    pub cid_bytes: Vec<u8>,
}

/// Extract canonical schema bytes for the given interface type IDs from a
/// raw `CodeGeneratorRequest` file produced by capnpc.
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
                let canonical_bytes = canonicalize_node(node)?;
                let cid = compute_cid(&canonical_bytes);
                let cid_bytes = compute_cid_bytes(&canonical_bytes);

                results.push(SchemaEntry {
                    name: name.to_string(),
                    type_id: target_id,
                    canonical_bytes,
                    cid,
                    cid_bytes,
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

/// Extract canonical `SchemaBundle` bytes for the given exported interfaces
/// from a raw `CodeGeneratorRequest`.
///
/// The bundle contains all request nodes sorted by Cap'n Proto type ID. That is
/// intentionally conservative: it preserves the full transitive graph available
/// to the compiler while making bundle bytes independent of capnpc's node order.
pub fn extract_schema_bundles(
    raw_request_path: &Path,
    interfaces: &[(&str, u64)],
) -> capnp::Result<Vec<SchemaBundleEntry>> {
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

    let mut sorted_nodes: Vec<_> = nodes.iter().collect();
    sorted_nodes.sort_by_key(|node| node.get_id());

    let mut results = Vec::new();
    for &(name, service_interface_id) in interfaces {
        let service_node = sorted_nodes
            .iter()
            .copied()
            .find(|node| node.get_id() == service_interface_id)
            .ok_or_else(|| {
                capnp::Error::failed(format!(
                    "schema node with type ID 0x{service_interface_id:016x} not found in CodeGeneratorRequest"
                ))
            })?;
        require_interface_node(service_node, service_interface_id)?;

        let canonical_bytes = canonicalize_bundle_from_nodes(service_interface_id, &sorted_nodes)?;
        let cid = compute_cid(&canonical_bytes);
        let cid_bytes = compute_cid_bytes(&canonical_bytes);

        results.push(SchemaBundleEntry {
            name: name.to_string(),
            service_interface_id,
            canonical_bytes,
            cid,
            cid_bytes,
        });
    }

    Ok(results)
}

/// Canonicalize a single schema.Node into bytes.
pub fn canonicalize_node(node: capnp::schema_capnp::node::Reader<'_>) -> capnp::Result<Vec<u8>> {
    let mut message = capnp::message::Builder::new_default();
    message.set_root_canonical(node)?;
    let segments = message.get_segments_for_output();
    if segments.len() != 1 {
        return Err(capnp::Error::failed(
            "canonical schema.Node message was not single-segment".into(),
        ));
    }
    Ok(segments[0].to_vec())
}

fn canonicalize_bundle_from_nodes(
    service_interface_id: u64,
    sorted_nodes: &[capnp::schema_capnp::node::Reader<'_>],
) -> capnp::Result<Vec<u8>> {
    let mut message = capnp::message::Builder::new_default();
    {
        let mut root = message.init_root::<system_capnp::schema_bundle::Builder>();
        root.set_format_version(SCHEMA_BUNDLE_FORMAT_VERSION);
        root.set_service_interface_id(service_interface_id);
        let mut nodes = root.init_nodes(sorted_nodes.len() as u32);
        for (idx, node) in sorted_nodes.iter().enumerate() {
            nodes
                .reborrow()
                .set_with_caveats(idx as u32, *node)
                .map_err(|e| {
                    capnp::Error::failed(format!(
                        "failed to copy schema node 0x{:016x} into bundle: {e}",
                        node.get_id()
                    ))
                })?;
        }
    }

    let reader: system_capnp::schema_bundle::Reader<'_> = message.get_root_as_reader()?;
    canonicalize_schema_bundle(reader)
}

fn canonicalize_schema_bundle(
    bundle: system_capnp::schema_bundle::Reader<'_>,
) -> capnp::Result<Vec<u8>> {
    let mut message = capnp::message::Builder::new_default();
    message.set_root_canonical(bundle)?;
    let segments = message.get_segments_for_output();
    if segments.len() != 1 {
        return Err(capnp::Error::failed(
            "canonical SchemaBundle message was not single-segment".into(),
        ));
    }
    Ok(segments[0].to_vec())
}

/// Validate canonical bytes for `system.SchemaBundle`.
///
/// This rejects malformed messages, unsupported bundle versions, bundles that
/// omit the declared service interface node, non-interface service nodes, and
/// non-canonical encodings.
pub fn validate_schema_bundle(bytes: &[u8]) -> capnp::Result<ValidatedSchemaBundle> {
    if bytes.is_empty() {
        return Err(capnp::Error::failed(
            "SchemaBundle bytes must not be empty".into(),
        ));
    }
    if !bytes
        .len()
        .is_multiple_of(std::mem::size_of::<capnp::Word>())
    {
        return Err(capnp::Error::failed(format!(
            "SchemaBundle bytes must be word-aligned canonical Cap'n Proto data (got {} bytes)",
            bytes.len()
        )));
    }

    let words = bytes_to_words(bytes);
    let aligned = capnp::Word::words_to_bytes(&words);
    let segments: &[&[u8]] = &[aligned];
    let segment_array = capnp::message::SegmentArray::new(segments);
    let reader = capnp::message::Reader::new(segment_array, capnp::message::ReaderOptions::new());
    let bundle: system_capnp::schema_bundle::Reader<'_> = reader.get_root()?;

    let format_version = bundle.get_format_version();
    if format_version != SCHEMA_BUNDLE_FORMAT_VERSION {
        return Err(capnp::Error::failed(format!(
            "unsupported SchemaBundle formatVersion {format_version}; expected {SCHEMA_BUNDLE_FORMAT_VERSION}"
        )));
    }

    let service_interface_id = bundle.get_service_interface_id();
    let nodes = bundle.get_nodes()?;
    let mut seen_node_ids = std::collections::BTreeSet::new();
    let mut service_node = None;
    for node in nodes.iter() {
        let node_id = node.get_id();
        if !seen_node_ids.insert(node_id) {
            return Err(capnp::Error::failed(format!(
                "SchemaBundle contains duplicate node id 0x{node_id:016x}"
            )));
        }
        if node_id == service_interface_id {
            service_node = Some(node);
        }
    }
    let service_node = service_node.ok_or_else(|| {
        capnp::Error::failed(format!(
            "SchemaBundle serviceInterfaceId 0x{service_interface_id:016x} not found in nodes"
        ))
    })?;
    require_interface_node(service_node, service_interface_id)?;

    let canonical_bytes = canonicalize_schema_bundle(bundle)?;
    if canonical_bytes != bytes {
        return Err(capnp::Error::failed(
            "SchemaBundle bytes are not canonical".into(),
        ));
    }

    let cid = compute_cid(bytes);
    let cid_bytes = compute_cid_bytes(bytes);
    Ok(ValidatedSchemaBundle {
        format_version,
        service_interface_id,
        canonical_bytes,
        cid,
        cid_bytes,
    })
}

fn require_interface_node(
    node: capnp::schema_capnp::node::Reader<'_>,
    target_id: u64,
) -> capnp::Result<()> {
    match node.which()? {
        capnp::schema_capnp::node::Which::Interface(_) => Ok(()),
        _ => Err(capnp::Error::failed(format!(
            "schema node 0x{target_id:016x} is not an interface"
        ))),
    }
}

fn bytes_to_words(bytes: &[u8]) -> Vec<capnp::Word> {
    let word_count = bytes.len().div_ceil(std::mem::size_of::<capnp::Word>());
    let mut words = capnp::Word::allocate_zeroed_vec(word_count);
    capnp::Word::words_to_bytes_mut(&mut words)[..bytes.len()].copy_from_slice(bytes);
    words
}

/// Compute CIDv1(raw, BLAKE3(data)) as a string.
pub fn compute_cid(data: &[u8]) -> String {
    compute_cid_value(data).to_string()
}

/// Compute CIDv1(raw, BLAKE3(data)) as canonical CID bytes.
pub fn compute_cid_bytes(data: &[u8]) -> Vec<u8> {
    compute_cid_value(data).to_bytes()
}

fn compute_cid_value(data: &[u8]) -> cid::Cid {
    let digest = blake3::hash(data);
    let mh = cid::multihash::Multihash::<64>::wrap(MULTIHASH_BLAKE3_CODE, digest.as_bytes())
        .expect("blake3 digest always fits in 64-byte multihash");
    cid::Cid::new_v1(CID_RAW_CODEC, mh)
}

/// Emit Rust source with legacy schema-node constants.
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
        write_bytes_array(&mut code, &entry.canonical_bytes);
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
        write!(code, "pub const {}_CID_BYTES: &[u8] = &[", entry.name).unwrap();
        write_bytes_array(&mut code, &entry.cid_bytes);
        writeln!(code, "\n];").unwrap();
        writeln!(code).unwrap();
    }

    let mut file = std::fs::File::create(output_path)?;
    file.write_all(code.as_bytes())?;
    Ok(())
}

/// Emit Rust source with `SchemaBundle` constants.
pub fn emit_schema_bundle_consts(
    output_path: &Path,
    schemas: &[SchemaBundleEntry],
) -> std::io::Result<()> {
    use std::fmt::Write as _;
    use std::io::Write;

    let mut code = String::new();
    writeln!(code, "// Auto-generated by schema-id. Do not edit.").unwrap();
    writeln!(code).unwrap();

    for entry in schemas {
        writeln!(
            code,
            "/// Canonical SchemaBundle bytes for {} (service interface ID 0x{:016x}).",
            entry.name, entry.service_interface_id
        )
        .unwrap();
        write!(code, "pub const {}_SCHEMA_BUNDLE: &[u8] = &[", entry.name).unwrap();
        write_bytes_array(&mut code, &entry.canonical_bytes);
        writeln!(code, "\n];").unwrap();
        writeln!(
            code,
            "pub const {}_SCHEMA_BUNDLE_CID: &str = \"{}\";",
            entry.name, entry.cid
        )
        .unwrap();
        write!(
            code,
            "pub const {}_SCHEMA_BUNDLE_CID_BYTES: &[u8] = &[",
            entry.name
        )
        .unwrap();
        write_bytes_array(&mut code, &entry.cid_bytes);
        writeln!(code, "\n];").unwrap();
        writeln!(code).unwrap();
    }

    let mut file = std::fs::File::create(output_path)?;
    file.write_all(code.as_bytes())?;
    Ok(())
}

fn write_bytes_array(code: &mut String, bytes: &[u8]) {
    use std::fmt::Write as _;
    for (i, byte) in bytes.iter().enumerate() {
        if i > 0 {
            write!(code, ", ").unwrap();
        }
        if i % 16 == 0 {
            writeln!(code).unwrap();
            write!(code, "    ").unwrap();
        }
        write!(code, "0x{byte:02x}").unwrap();
    }
}

/// Write raw schema-node bytes to a file for post-build injection.
pub fn write_schema_bytes(output_path: &Path, entry: &SchemaEntry) -> std::io::Result<()> {
    std::fs::write(output_path, &entry.canonical_bytes)
}

/// Write raw `SchemaBundle` bytes to a file for WASM custom-section injection.
pub fn write_schema_bundle_bytes(
    output_path: &Path,
    entry: &SchemaBundleEntry,
) -> std::io::Result<()> {
    std::fs::write(output_path, &entry.canonical_bytes)
}

/// Error returned by WASM custom-section parsing and injection helpers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WasmSectionError {
    message: String,
}

impl WasmSectionError {
    fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

impl fmt::Display for WasmSectionError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for WasmSectionError {}

/// Return the first custom section with `name`, if present.
pub fn extract_custom_section(
    wasm: &[u8],
    name: &str,
) -> Result<Option<Vec<u8>>, WasmSectionError> {
    Ok(extract_custom_sections(wasm, name)?.into_iter().next())
}

/// Return all custom sections with `name`, preserving WASM section order.
pub fn extract_custom_sections(wasm: &[u8], name: &str) -> Result<Vec<Vec<u8>>, WasmSectionError> {
    ensure_wasm_header(wasm)?;

    let mut matches = Vec::new();
    let mut pos = 8;
    while pos < wasm.len() {
        let section_start = pos;
        let id = read_u8(wasm, &mut pos)?;
        let payload_len = read_leb_u32(wasm, &mut pos)? as usize;
        let payload_start = pos;
        let payload_end = payload_start
            .checked_add(payload_len)
            .ok_or_else(|| WasmSectionError::new("WASM section payload length overflow"))?;
        if payload_end > wasm.len() {
            return Err(WasmSectionError::new(format!(
                "WASM section at offset {section_start} exceeds file length"
            )));
        }

        if id == 0 {
            let payload = &wasm[payload_start..payload_end];
            if let Some((section_name, body)) = parse_custom_payload(payload)? {
                if section_name == name {
                    matches.push(body.to_vec());
                }
            }
        }
        pos = payload_end;
    }

    Ok(matches)
}

/// Extract and validate the `ww.schema.v1` custom section from a WASM artifact.
pub fn extract_schema_bundle_section(
    wasm: &[u8],
) -> Result<Option<ValidatedSchemaBundle>, capnp::Error> {
    let sections = extract_custom_sections(wasm, SCHEMA_SECTION_NAME).map_err(|e| {
        capnp::Error::failed(format!(
            "failed to parse WASM custom sections while looking for {SCHEMA_SECTION_NAME}: {e}"
        ))
    })?;
    match sections.len() {
        0 => Ok(None),
        1 => {
            let mut sections = sections;
            let bytes = sections.pop().expect("exactly one section");
            validate_schema_bundle(&bytes).map(Some)
        }
        n => Err(capnp::Error::failed(format!(
            "WASM artifact contains {n} {SCHEMA_SECTION_NAME} custom sections; expected exactly one"
        ))),
    }
}

/// Remove all custom sections with `name`.
pub fn strip_custom_section(wasm: &[u8], name: &str) -> Result<Vec<u8>, WasmSectionError> {
    ensure_wasm_header(wasm)?;

    let mut out = wasm[..8].to_vec();
    let mut pos = 8;
    while pos < wasm.len() {
        let section_start = pos;
        let id = read_u8(wasm, &mut pos)?;
        let payload_len_start = pos;
        let payload_len = read_leb_u32(wasm, &mut pos)? as usize;
        let payload_start = pos;
        let payload_end = payload_start
            .checked_add(payload_len)
            .ok_or_else(|| WasmSectionError::new("WASM section payload length overflow"))?;
        if payload_end > wasm.len() {
            return Err(WasmSectionError::new(format!(
                "WASM section at offset {section_start} exceeds file length"
            )));
        }

        let should_strip = if id == 0 {
            let payload = &wasm[payload_start..payload_end];
            parse_custom_payload(payload)?
                .map(|(section_name, _)| section_name == name)
                .unwrap_or(false)
        } else {
            false
        };

        if !should_strip {
            out.push(id);
            out.extend_from_slice(&wasm[payload_len_start..payload_end]);
        }
        pos = payload_end;
    }

    Ok(out)
}

/// Append or replace a custom section.
pub fn inject_custom_section(
    wasm: &[u8],
    name: &str,
    body: &[u8],
) -> Result<Vec<u8>, WasmSectionError> {
    if name.is_empty() {
        return Err(WasmSectionError::new(
            "custom section name must not be empty",
        ));
    }

    let mut out = strip_custom_section(wasm, name)?;
    let mut payload = Vec::new();
    write_leb_u32(name.len() as u32, &mut payload);
    payload.extend_from_slice(name.as_bytes());
    payload.extend_from_slice(body);

    out.push(0);
    write_leb_u32(payload.len() as u32, &mut out);
    out.extend_from_slice(&payload);
    Ok(out)
}

/// Append or replace the `ww.schema.v1` custom section after validating the bundle.
pub fn inject_schema_bundle_section(wasm: &[u8], bundle: &[u8]) -> capnp::Result<Vec<u8>> {
    validate_schema_bundle(bundle)?;
    inject_custom_section(wasm, SCHEMA_SECTION_NAME, bundle).map_err(|e| {
        capnp::Error::failed(format!(
            "failed to inject {SCHEMA_SECTION_NAME} custom section: {e}"
        ))
    })
}

fn ensure_wasm_header(wasm: &[u8]) -> Result<(), WasmSectionError> {
    if wasm.len() < 8 {
        return Err(WasmSectionError::new(
            "WASM artifact is shorter than the 8-byte header",
        ));
    }
    if &wasm[..4] != b"\0asm" {
        return Err(WasmSectionError::new("invalid WASM magic"));
    }
    Ok(())
}

fn read_u8(bytes: &[u8], pos: &mut usize) -> Result<u8, WasmSectionError> {
    let byte = *bytes
        .get(*pos)
        .ok_or_else(|| WasmSectionError::new("unexpected EOF reading WASM byte"))?;
    *pos += 1;
    Ok(byte)
}

fn read_leb_u32(bytes: &[u8], pos: &mut usize) -> Result<u32, WasmSectionError> {
    let mut result: u32 = 0;
    let mut shift = 0;
    for _ in 0..5 {
        let byte = read_u8(bytes, pos)?;
        result |= ((byte & 0x7f) as u32) << shift;
        if byte & 0x80 == 0 {
            return Ok(result);
        }
        shift += 7;
    }
    Err(WasmSectionError::new("invalid u32 LEB128 encoding"))
}

fn write_leb_u32(mut value: u32, out: &mut Vec<u8>) {
    loop {
        let mut byte = (value & 0x7f) as u8;
        value >>= 7;
        if value != 0 {
            byte |= 0x80;
        }
        out.push(byte);
        if value == 0 {
            break;
        }
    }
}

fn parse_custom_payload(payload: &[u8]) -> Result<Option<(&str, &[u8])>, WasmSectionError> {
    let mut pos = 0;
    let name_len = read_leb_u32(payload, &mut pos)? as usize;
    let name_end = pos
        .checked_add(name_len)
        .ok_or_else(|| WasmSectionError::new("custom section name length overflow"))?;
    if name_end > payload.len() {
        return Err(WasmSectionError::new(
            "custom section name exceeds payload length",
        ));
    }
    let name = std::str::from_utf8(&payload[pos..name_end])
        .map_err(|e| WasmSectionError::new(format!("custom section name is not UTF-8: {e}")))?;
    Ok(Some((name, &payload[name_end..])))
}

#[cfg(test)]
mod tests {
    use super::*;

    const TEST_INTERFACE_ID: u64 = 0xe3c2_dfb1_8682_18d1;

    #[test]
    fn test_compute_cid_deterministic() {
        let data = b"test schema node canonical bytes";
        let cid1 = compute_cid(data);
        let cid2 = compute_cid(data);
        assert_eq!(cid1, cid2);
        assert_eq!(compute_cid_bytes(data), compute_cid_bytes(data));
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
        assert_eq!(parsed.codec(), CID_RAW_CODEC);

        let parsed_from_bytes = cid::Cid::try_from(compute_cid_bytes(b"some data").as_slice())
            .expect("CID bytes should parse");
        assert_eq!(parsed_from_bytes, parsed);
    }

    #[test]
    fn schema_bundle_cid_is_over_raw_canonical_bytes() {
        let bundle = test_schema_bundle_bytes();
        let validated = validate_schema_bundle(&bundle).expect("valid bundle");
        assert_eq!(validated.canonical_bytes, bundle);
        assert_eq!(validated.cid, compute_cid(&bundle));
        assert_eq!(validated.cid_bytes, compute_cid_bytes(&bundle));
    }

    #[test]
    fn invalid_schema_bundle_rejected() {
        assert!(validate_schema_bundle(b"not aligned").is_err());

        let mut bundle = test_schema_bundle_bytes();
        let last = bundle.last_mut().expect("non-empty bundle");
        *last ^= 0xff;
        assert!(validate_schema_bundle(&bundle).is_err());
    }

    #[test]
    fn duplicate_schema_bundle_node_ids_rejected() {
        let mut message = capnp::message::Builder::new_default();
        {
            let mut root = message.init_root::<system_capnp::schema_bundle::Builder>();
            root.set_format_version(SCHEMA_BUNDLE_FORMAT_VERSION);
            root.set_service_interface_id(TEST_INTERFACE_ID);
            let mut nodes = root.init_nodes(2);
            for idx in 0..2 {
                let mut node = nodes.reborrow().get(idx);
                node.set_id(TEST_INTERFACE_ID);
                node.set_display_name("test.capnp:TestService");
                node.init_interface();
            }
        }
        let reader: system_capnp::schema_bundle::Reader<'_> = message.get_root_as_reader().unwrap();
        let bundle = canonicalize_schema_bundle(reader).unwrap();

        let error = validate_schema_bundle(&bundle).expect_err("duplicate node id rejected");
        assert!(
            error.to_string().contains("duplicate node id"),
            "got: {error}"
        );
    }

    #[test]
    fn wasm_schema_custom_section_round_trips() {
        let wasm = minimal_wasm();
        let bundle = test_schema_bundle_bytes();
        let injected = inject_schema_bundle_section(&wasm, &bundle).expect("inject");

        let extracted = extract_custom_section(&injected, SCHEMA_SECTION_NAME)
            .expect("parse")
            .expect("section exists");
        assert_eq!(extracted, bundle);

        let validated = extract_schema_bundle_section(&injected)
            .expect("parse schema section")
            .expect("schema section exists");
        assert_eq!(validated.canonical_bytes, bundle);
    }

    #[test]
    fn wasm_artifact_cid_includes_custom_sections() {
        let wasm = minimal_wasm();
        let injected =
            inject_schema_bundle_section(&wasm, &test_schema_bundle_bytes()).expect("inject");

        assert_ne!(compute_cid_bytes(&wasm), compute_cid_bytes(&injected));
        assert_eq!(compute_cid(&injected), compute_cid(&injected));
    }

    #[test]
    fn duplicate_wasm_schema_custom_sections_rejected() {
        let wasm = minimal_wasm();
        let bundle = test_schema_bundle_bytes();
        let mut injected = inject_schema_bundle_section(&wasm, &bundle).expect("inject");

        append_raw_custom_section(&mut injected, SCHEMA_SECTION_NAME, &bundle);

        let error = extract_schema_bundle_section(&injected)
            .expect_err("duplicate schema sections rejected");
        assert!(
            error.to_string().contains("expected exactly one"),
            "got: {error}"
        );
    }

    #[test]
    fn replacing_schema_custom_section_keeps_one_section() {
        let wasm = minimal_wasm();
        let first = inject_schema_bundle_section(&wasm, &test_schema_bundle_bytes()).unwrap();
        let second = inject_schema_bundle_section(&first, &test_schema_bundle_bytes()).unwrap();

        assert_eq!(
            extract_custom_section(&second, SCHEMA_SECTION_NAME)
                .unwrap()
                .unwrap(),
            test_schema_bundle_bytes()
        );
        assert_eq!(
            strip_custom_section(&second, SCHEMA_SECTION_NAME).unwrap(),
            wasm
        );
    }

    fn minimal_wasm() -> Vec<u8> {
        b"\0asm\x01\0\0\0".to_vec()
    }

    fn append_raw_custom_section(wasm: &mut Vec<u8>, name: &str, body: &[u8]) {
        let mut payload = Vec::new();
        write_leb_u32(name.len() as u32, &mut payload);
        payload.extend_from_slice(name.as_bytes());
        payload.extend_from_slice(body);

        wasm.push(0);
        write_leb_u32(payload.len() as u32, wasm);
        wasm.extend_from_slice(&payload);
    }

    fn test_schema_bundle_bytes() -> Vec<u8> {
        let mut message = capnp::message::Builder::new_default();
        {
            let mut root = message.init_root::<system_capnp::schema_bundle::Builder>();
            root.set_format_version(SCHEMA_BUNDLE_FORMAT_VERSION);
            root.set_service_interface_id(TEST_INTERFACE_ID);
            let nodes = root.init_nodes(1);
            let mut node = nodes.get(0);
            node.set_id(TEST_INTERFACE_ID);
            node.set_display_name("test.capnp:TestService");
            node.init_interface();
        }
        let reader: system_capnp::schema_bundle::Reader<'_> = message.get_root_as_reader().unwrap();
        canonicalize_schema_bundle(reader).unwrap()
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
