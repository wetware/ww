//! Deterministic capability-catalog generation from Cap'n Proto reflection.
//!
//! Schema facts (interface IDs, names, CIDs, methods, and ordinals) come from
//! a compiler-produced `CodeGeneratorRequest`. Repository policy that Cap'n
//! Proto cannot express lives in the small checked overlay.

use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

const CATALOG_NOTICE: &str = "Documentation only. A catalog entry does not imply runtime availability or possession, and a name or interface ID cannot resolve, mint, or grant a capability. Only an explicitly possessed capability reference can be placed in a child :grants map.";

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PolicyOverlay {
    pub schema_version: u32,
    pub capabilities: Vec<PolicyEntry>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PolicyEntry {
    pub catalog_id: String,
    pub conventional_name: String,
    pub interface_id: String,
    pub schema_path: String,
    pub provider: String,
    pub normally_grantable: bool,
    pub effect_class: String,
    pub sensitivity: String,
    pub description: String,
    pub attenuation: AttenuationPolicy,
    #[serde(default)]
    pub required_configuration: Vec<String>,
    pub grant_example: String,
    #[serde(default)]
    pub security_notes: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct AttenuationPolicy {
    pub supported: bool,
    pub mechanism: String,
    pub guidance: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Catalog {
    pub schema_version: u32,
    pub notice: String,
    pub generated_from: Vec<String>,
    pub capabilities: Vec<CatalogEntry>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CatalogEntry {
    pub catalog_id: String,
    pub conventional_name: String,
    pub interface_name: String,
    pub interface_id: String,
    pub schema_path: String,
    pub schema_cid: String,
    pub methods: Vec<CatalogMethod>,
    pub provider: String,
    pub normally_grantable: bool,
    pub effect_class: String,
    pub sensitivity: String,
    pub description: String,
    pub attenuation: AttenuationPolicy,
    pub required_configuration: Vec<String>,
    pub grant_example: String,
    pub security_notes: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CatalogMethod {
    pub name: String,
    pub ordinal: u16,
}

#[derive(Debug)]
struct ReflectedInterface {
    interface_name: String,
    display_name: String,
    schema_cid: String,
    methods: Vec<CatalogMethod>,
}

pub fn load_overlay(path: &Path) -> Result<PolicyOverlay, String> {
    let bytes = std::fs::read(path).map_err(|error| format!("read {}: {error}", path.display()))?;
    serde_json::from_slice(&bytes).map_err(|error| format!("parse {}: {error}", path.display()))
}

pub fn generate_from_request(
    raw_request_path: &Path,
    overlay: PolicyOverlay,
) -> Result<Catalog, String> {
    let request_data = std::fs::read(raw_request_path)
        .map_err(|error| format!("read {}: {error}", raw_request_path.display()))?;
    let message_reader = capnp::serialize::read_message(
        &mut request_data.as_slice(),
        capnp::message::ReaderOptions::new(),
    )
    .map_err(|error| format!("decode schema request: {error}"))?;
    let request: capnp::schema_capnp::code_generator_request::Reader = message_reader
        .get_root()
        .map_err(|error| format!("read schema request root: {error}"))?;
    let nodes = request
        .get_nodes()
        .map_err(|error| format!("read schema nodes: {error}"))?;

    let mut reflected = BTreeMap::new();
    for node in nodes {
        let interface = match node
            .which()
            .map_err(|error| format!("read schema node kind: {error}"))?
        {
            capnp::schema_capnp::node::Interface(interface) => interface,
            _ => continue,
        };
        let display_name_reader = node
            .get_display_name()
            .map_err(|error| format!("read interface display name: {error}"))?;
        let display_name = display_name_reader
            .to_str()
            .map_err(|error| format!("decode interface display name: {error}"))?
            .to_owned();
        let interface_name = display_name
            .rsplit(':')
            .next()
            .unwrap_or(display_name.as_str())
            .to_owned();
        let methods_reader = interface
            .get_methods()
            .map_err(|error| format!("read methods for {display_name}: {error}"))?;
        let mut methods = Vec::with_capacity(methods_reader.len() as usize);
        for (ordinal, method) in methods_reader.iter().enumerate() {
            let ordinal = u16::try_from(ordinal)
                .map_err(|_| format!("too many methods on interface {display_name}"))?;
            let name_reader = method
                .get_name()
                .map_err(|error| format!("read method on {display_name}: {error}"))?;
            let name = name_reader
                .to_str()
                .map_err(|error| format!("decode method on {display_name}: {error}"))?
                .to_owned();
            methods.push(CatalogMethod { name, ordinal });
        }
        let canonical = super::canonicalize_node(node)
            .map_err(|error| format!("canonicalize {display_name}: {error}"))?;
        reflected.insert(
            node.get_id(),
            ReflectedInterface {
                interface_name,
                display_name,
                schema_cid: super::compute_cid(&canonical),
                methods,
            },
        );
    }

    let mut catalog_ids = BTreeSet::new();
    let mut conventional_names = BTreeSet::new();
    let mut capabilities = Vec::with_capacity(overlay.capabilities.len());
    for policy in overlay.capabilities {
        if !catalog_ids.insert(policy.catalog_id.clone()) {
            return Err(format!("duplicate catalogId {:?}", policy.catalog_id));
        }
        if !conventional_names.insert(policy.conventional_name.clone()) {
            return Err(format!(
                "duplicate conventionalName {:?}",
                policy.conventional_name
            ));
        }
        if policy.catalog_id.trim().is_empty() || policy.conventional_name.trim().is_empty() {
            return Err("catalogId and conventionalName must be non-empty".to_owned());
        }
        let interface_id = parse_interface_id(&policy.interface_id)?;
        let schema = reflected.get(&interface_id).ok_or_else(|| {
            format!(
                "{} references unknown interface ID {}",
                policy.catalog_id, policy.interface_id
            )
        })?;
        let expected_suffix = format!(":{}", schema.interface_name);
        if !schema.display_name.ends_with(&expected_suffix) {
            return Err(format!(
                "{} interface display name {:?} does not end in {:?}",
                policy.catalog_id, schema.display_name, expected_suffix
            ));
        }
        let expected_path = policy.schema_path.trim_start_matches("./");
        let reflected_path = schema
            .display_name
            .split_once(':')
            .map(|(path, _)| path)
            .unwrap_or(schema.display_name.as_str())
            .trim_start_matches("./");
        if !reflected_path.ends_with(expected_path) {
            return Err(format!(
                "{} schemaPath {:?} does not match reflected path {:?}",
                policy.catalog_id, policy.schema_path, reflected_path
            ));
        }
        if policy.sensitivity.trim().is_empty()
            || policy.effect_class.trim().is_empty()
            || policy.description.trim().is_empty()
        {
            return Err(format!(
                "{} must define sensitivity, effectClass, and description",
                policy.catalog_id
            ));
        }
        capabilities.push(CatalogEntry {
            catalog_id: policy.catalog_id,
            conventional_name: policy.conventional_name,
            interface_name: schema.interface_name.clone(),
            interface_id: format!("0x{interface_id:016x}"),
            schema_path: policy.schema_path,
            schema_cid: schema.schema_cid.clone(),
            methods: schema.methods.clone(),
            provider: policy.provider,
            normally_grantable: policy.normally_grantable,
            effect_class: policy.effect_class,
            sensitivity: policy.sensitivity,
            description: policy.description,
            attenuation: policy.attenuation,
            required_configuration: policy.required_configuration,
            grant_example: policy.grant_example,
            security_notes: policy.security_notes,
        });
    }
    capabilities.sort_by(|left, right| left.catalog_id.cmp(&right.catalog_id));

    Ok(Catalog {
        schema_version: overlay.schema_version,
        notice: CATALOG_NOTICE.to_owned(),
        generated_from: vec![
            "Cap'n Proto CodeGeneratorRequest schema reflection".to_owned(),
            "capnp/capability-policy.json repository policy overlay".to_owned(),
        ],
        capabilities,
    })
}

pub fn generate_repository_catalog(
    repository_root: &Path,
    overlay_path: &Path,
) -> Result<Catalog, String> {
    let temp = tempfile::tempdir().map_err(|error| format!("create temp directory: {error}"))?;
    let raw_request = temp.path().join("capability-catalog-request.bin");
    let capnp_dir = repository_root.join("capnp");
    let mut compiler = capnpc::CompilerCommand::new();
    compiler
        .src_prefix(repository_root)
        .output_path(temp.path())
        .crate_provides("capnp", [0xa93f_c509_624c_72d9])
        .raw_code_generator_request_path(&raw_request);
    for schema in [
        "system.capnp",
        "routing.capnp",
        "auth.capnp",
        "membrane.capnp",
        "stem.capnp",
        "http.capnp",
        "shell.capnp",
    ] {
        compiler.file(capnp_dir.join(schema));
    }
    compiler
        .run()
        .map_err(|error| format!("compile capability schemas: {error}"))?;
    generate_from_request(&raw_request, load_overlay(overlay_path)?)
}

pub fn to_pretty_json(catalog: &Catalog) -> Result<String, String> {
    let mut json = serde_json::to_string_pretty(catalog)
        .map_err(|error| format!("encode catalog: {error}"))?;
    json.push('\n');
    Ok(json)
}

pub fn write_catalog(path: &Path, catalog: &Catalog) -> Result<(), String> {
    let json = to_pretty_json(catalog)?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|error| format!("create {}: {error}", parent.display()))?;
    }
    std::fs::write(path, json).map_err(|error| format!("write {}: {error}", path.display()))
}

pub fn check_catalog(path: &Path, catalog: &Catalog) -> Result<(), String> {
    let expected = to_pretty_json(catalog)?;
    let actual = std::fs::read_to_string(path)
        .map_err(|error| format!("read {}: {error}", path.display()))?;
    if actual == expected {
        Ok(())
    } else {
        Err(format!(
            "{} is stale; run `cargo run -p schema-id --bin capability-catalog -- generate`",
            path.display()
        ))
    }
}

pub fn repository_root_from_manifest() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .expect("schema-id repository root")
}

fn parse_interface_id(value: &str) -> Result<u64, String> {
    let digits = value
        .strip_prefix("0x")
        .or_else(|| value.strip_prefix("0X"))
        .ok_or_else(|| format!("interfaceId {value:?} must use 0x-prefixed hexadecimal"))?
        .replace('_', "");
    u64::from_str_radix(&digits, 16)
        .map_err(|error| format!("invalid interfaceId {value:?}: {error}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn interface_id_requires_hex_prefix() {
        assert!(parse_interface_id("42").is_err());
        assert_eq!(parse_interface_id("0x2a").unwrap(), 42);
    }
}
