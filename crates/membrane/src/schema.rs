//! Cap'n Proto schema resolution for static membrane allowlists.
//!
//! Wetware stores compiled `schema.Node` values as a single raw segment. This
//! module parses that representation once, then resolves kebab-case policy
//! names (for example, `http-client`) to the camelCase method names and wire
//! ordinals in the interface schema.

use std::fmt;

use capnp::schema_capnp;

use crate::{Allowlist, MethodKey};

/// Parsed method metadata from one compiled Cap'n Proto interface node.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CompiledSchema {
    interface_id: u64,
    methods: Vec<String>,
}

impl CompiledSchema {
    /// Parse a canonical `schema.Node` stored as one raw segment.
    ///
    /// The input may have arbitrary byte alignment. It is copied into Cap'n
    /// Proto word storage before parsing so embedded byte slices are safe.
    pub fn from_node_bytes(schema: &[u8]) -> Result<Self, ResolveError> {
        let mut words = capnp::Word::allocate_zeroed_vec(schema.len().div_ceil(8));
        capnp::Word::words_to_bytes_mut(&mut words)[..schema.len()].copy_from_slice(schema);
        let segments = [&capnp::Word::words_to_bytes(&words)[..schema.len()]];
        let segment_array = capnp::message::SegmentArray::new(&segments);
        let reader =
            capnp::message::Reader::new(segment_array, capnp::message::ReaderOptions::new());
        let node: schema_capnp::node::Reader =
            reader.get_root().map_err(ResolveError::InvalidSchema)?;

        let interface_id = node.get_id();
        let interface = match node.which() {
            Ok(schema_capnp::node::Which::Interface(interface)) => interface,
            _ => return Err(ResolveError::NotAnInterface),
        };
        let schema_methods = interface
            .get_methods()
            .map_err(ResolveError::InvalidSchema)?;
        let mut methods = Vec::with_capacity(schema_methods.len() as usize);
        for method in schema_methods.iter() {
            let name = method
                .get_name()
                .and_then(|name| {
                    name.to_str()
                        .map_err(|error| capnp::Error::failed(error.to_string()))
                })
                .map_err(ResolveError::InvalidSchema)?;
            methods.push(name.to_string());
        }

        Ok(Self {
            interface_id,
            methods,
        })
    }

    /// The interface type ID that scopes every method ordinal in this schema.
    pub fn interface_id(&self) -> u64 {
        self.interface_id
    }

    /// Resolve one kebab-case policy name to its Cap'n Proto wire coordinate.
    pub fn method_key(&self, method: &str) -> Option<MethodKey> {
        let capnp_name = to_capnp_method_name(method);
        self.methods
            .iter()
            .position(|name| *name == capnp_name)
            .and_then(|ordinal| u16::try_from(ordinal).ok())
            .map(|ordinal| MethodKey::new(self.interface_id, ordinal))
    }

    /// Camel-case method names exactly as recorded in the compiled schema.
    pub fn available_methods(&self) -> &[String] {
        &self.methods
    }
}

/// A typed failure to parse an interface schema or resolve a requested method.
#[derive(Debug)]
pub enum ResolveError {
    InvalidSchema(capnp::Error),
    NotAnInterface,
    UnknownMethod {
        method: String,
        available: Vec<String>,
    },
}

impl fmt::Display for ResolveError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidSchema(error) => error.fmt(f),
            Self::NotAnInterface => write!(f, "schema node is not an interface"),
            Self::UnknownMethod { method, .. } => {
                write!(f, "method :{method} not found on interface")
            }
        }
    }
}

impl std::error::Error for ResolveError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::InvalidSchema(error) => Some(error),
            Self::NotAnInterface | Self::UnknownMethod { .. } => None,
        }
    }
}

/// Resolve kebab-case method names into a fail-closed static allowlist.
pub fn resolve_allowlist(
    schema: &CompiledSchema,
    methods: &[&str],
) -> Result<Allowlist, ResolveError> {
    let mut allowlist = Allowlist::new();
    for method in methods {
        let key = schema
            .method_key(method)
            .ok_or_else(|| ResolveError::UnknownMethod {
                method: (*method).to_string(),
                available: schema.available_methods().to_vec(),
            })?;
        allowlist = allowlist.allow(key.interface_id, key.method_id);
    }
    Ok(allowlist)
}

fn to_capnp_method_name(policy_name: &str) -> String {
    let mut output = String::with_capacity(policy_name.len());
    let mut uppercase_next = false;
    for character in policy_name.chars() {
        if character == '-' {
            uppercase_next = true;
        } else if uppercase_next {
            output.extend(character.to_uppercase());
            uppercase_next = false;
        } else {
            output.push(character);
        }
    }
    output
}

#[cfg(test)]
mod tests {
    use capnp::message::{Builder, HeapAllocator};

    use super::*;
    use crate::Policy;

    const INTERFACE_ID: u64 = 0xfeed_face_cafe_beef;

    fn interface_schema(methods: &[&str]) -> Vec<u8> {
        let mut message = Builder::new(HeapAllocator::new());
        {
            let mut node = message.init_root::<schema_capnp::node::Builder<'_>>();
            node.set_id(INTERFACE_ID);
            let mut interface = node.init_interface();
            let mut schema_methods = interface.reborrow().init_methods(methods.len() as u32);
            for (ordinal, name) in methods.iter().enumerate() {
                schema_methods.reborrow().get(ordinal as u32).set_name(name);
            }
        }
        message.get_segments_for_output()[0].to_vec()
    }

    #[test]
    fn resolves_kebab_case_names_to_wire_keys() {
        let bytes = interface_schema(&["id", "httpClient"]);
        let schema = CompiledSchema::from_node_bytes(&bytes).expect("compiled interface");
        let allowlist = resolve_allowlist(&schema, &["http-client"]).expect("known schema method");

        assert!(allowlist.check(INTERFACE_ID, 1).is_ok());
        assert!(allowlist.check(INTERFACE_ID, 0).is_err());
        assert_eq!(
            schema.method_key("http-client"),
            Some(MethodKey::new(INTERFACE_ID, 1))
        );
    }

    #[test]
    fn accepts_unaligned_schema_bytes() {
        let bytes = interface_schema(&["id"]);
        let mut unaligned = vec![0_u8; bytes.len() + 1];
        unaligned[1..].copy_from_slice(&bytes);

        let schema = CompiledSchema::from_node_bytes(&unaligned[1..])
            .expect("schema parsing must not depend on source alignment");
        let allowlist = resolve_allowlist(&schema, &["id"]).expect("known schema method");
        assert!(allowlist.check(INTERFACE_ID, 0).is_ok());
    }

    #[test]
    fn unknown_methods_fail_closed_with_available_names() {
        let bytes = interface_schema(&["id", "httpClient"]);
        let schema = CompiledSchema::from_node_bytes(&bytes).expect("compiled interface");

        let error = match resolve_allowlist(&schema, &["missing"]) {
            Ok(_) => panic!("unknown method must fail"),
            Err(error) => error,
        };
        match error {
            ResolveError::UnknownMethod { method, available } => {
                assert_eq!(method, "missing");
                assert_eq!(available, ["id", "httpClient"]);
            }
            error => panic!("unexpected error: {error}"),
        }
    }

    #[test]
    fn rejects_non_interface_nodes() {
        let mut message = Builder::new_default();
        message
            .init_root::<schema_capnp::node::Builder<'_>>()
            .init_struct();
        let bytes = message.get_segments_for_output()[0].to_vec();

        assert!(matches!(
            CompiledSchema::from_node_bytes(&bytes),
            Err(ResolveError::NotAnInterface)
        ));
    }
}
