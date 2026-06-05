//! Epoch-scoped capability primitives over Cap'n Proto RPC.
//!
//! - **Epoch** -- a monotonic sequence number anchored to on-chain state
//! - **EpochGuard** -- checks whether a capability's epoch is still current
//! - **MembraneServer** -- server that issues epoch-scoped sessions via `graft()`
//! - **SessionBuilder** -- trait for injecting domain-specific capabilities into sessions

#[allow(unused_parens, clippy::match_single_binding)]
pub mod system_capnp {
    include!(concat!(env!("OUT_DIR"), "/capnp/system_capnp.rs"));
}

#[allow(unused_parens, clippy::match_single_binding)]
pub mod routing_capnp {
    include!(concat!(env!("OUT_DIR"), "/capnp/routing_capnp.rs"));
}

#[allow(
    unused_parens,
    clippy::extra_unused_type_parameters,
    clippy::match_single_binding
)]
pub mod stem_capnp {
    include!(concat!(env!("OUT_DIR"), "/capnp/stem_capnp.rs"));
}

#[allow(
    unused_parens,
    clippy::extra_unused_type_parameters,
    clippy::match_single_binding
)]
pub mod auth_capnp {
    include!(concat!(env!("OUT_DIR"), "/capnp/auth_capnp.rs"));
}

#[allow(
    unused_parens,
    clippy::extra_unused_type_parameters,
    clippy::match_single_binding
)]
pub mod membrane_capnp {
    include!(concat!(env!("OUT_DIR"), "/capnp/membrane_capnp.rs"));
}

#[allow(unused_parens, clippy::match_single_binding)]
pub mod http_capnp {
    include!(concat!(env!("OUT_DIR"), "/capnp/http_capnp.rs"));
}

/// Canonical Schema.Node bytes for each grafted capability interface.
/// Populated into `Export.schema` at graft time so guests can introspect
/// the interface without hardcoded descriptions. See `build.rs`.
pub mod schema_registry {
    include!(concat!(env!("OUT_DIR"), "/schema_ids.rs"));

    /// Resolve canonical Schema.Node bytes by the canonical cap name used
    /// in the membrane graft loop (e.g. "host", "runtime", "routing",
    /// "identity", "http-client"). Returns `None` for unknown names so
    /// callers can fall back to an empty schema rather than panicking.
    pub fn schema_by_name(name: &str) -> Option<&'static [u8]> {
        match name {
            "host" => Some(HOST_SCHEMA),
            "runtime" => Some(RUNTIME_SCHEMA),
            "routing" => Some(ROUTING_SCHEMA),
            "identity" => Some(IDENTITY_SCHEMA),
            "http-client" => Some(HTTP_CLIENT_SCHEMA),
            _ => None,
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use capnp::traits::HasTypeId;

        #[test]
        fn each_core_cap_has_non_empty_bytes() {
            for name in ["host", "runtime", "routing", "identity", "http-client"] {
                let bytes = schema_by_name(name)
                    .unwrap_or_else(|| panic!("missing schema for cap '{name}'"));
                assert!(!bytes.is_empty(), "schema for '{name}' is empty");
            }
        }

        #[test]
        fn unknown_cap_returns_none() {
            assert!(schema_by_name("nonexistent").is_none());
            assert!(schema_by_name("").is_none());
        }

        #[test]
        fn bytes_are_word_aligned() {
            for name in ["host", "runtime", "routing", "identity", "http-client"] {
                let bytes = schema_by_name(name).expect("schema present");
                assert_eq!(
                    bytes.len() % 8,
                    0,
                    "canonical schema for '{name}' must be word-aligned (got {} bytes)",
                    bytes.len()
                );
            }
        }

        #[test]
        fn bytes_parse_as_schema_node() {
            for name in ["host", "runtime", "routing", "identity", "http-client"] {
                let bytes = schema_by_name(name).expect("schema present");
                // Capnp segments require 8-byte alignment; the static byte
                // slice is only byte-aligned, so copy into a Word buffer.
                let word_count = bytes.len().div_ceil(8);
                let mut words: Vec<capnp::Word> =
                    vec![capnp::word(0, 0, 0, 0, 0, 0, 0, 0); word_count];
                capnp::Word::words_to_bytes_mut(&mut words)[..bytes.len()].copy_from_slice(bytes);
                let aligned = capnp::Word::words_to_bytes(&words);
                let segments: &[&[u8]] = &[aligned];
                let segment_array = capnp::message::SegmentArray::new(segments);
                let reader = capnp::message::Reader::new(
                    segment_array,
                    capnp::message::ReaderOptions::new(),
                );
                let node: capnp::schema_capnp::node::Reader =
                    reader.get_root().expect("root is a node");
                let which = node.which().expect("node has Which");
                assert!(
                    matches!(which, capnp::schema_capnp::node::Which::Interface(_)),
                    "schema for '{name}' is not an interface node"
                );
            }
        }

        #[test]
        fn split_schema_type_ids_are_pinned_for_wire_compat() {
            // These IDs were historically defined in stem.capnp and are now
            // split across auth.capnp and membrane.capnp. Keep them pinned.
            assert_eq!(
                <crate::auth_capnp::signer::Client as HasTypeId>::TYPE_ID,
                0xafaf_af94_68b6_a274
            );
            assert_eq!(
                <crate::auth_capnp::identity::Client as HasTypeId>::TYPE_ID,
                0xa7c2_00e5_b472_6d89
            );
            assert_eq!(
                <crate::auth_capnp::terminal::Client<capnp::any_pointer::Owned> as HasTypeId>::TYPE_ID,
                0xeae8_840b_2a89_8ba9
            );
            assert_eq!(
                <crate::membrane_capnp::export::Reader<'static> as HasTypeId>::TYPE_ID,
                0xbb8d_5590_cb2f_3d2e
            );
            assert_eq!(
                <crate::membrane_capnp::membrane::Client as HasTypeId>::TYPE_ID,
                0xdb52_c251_06bc_2c5e
            );
        }

        #[test]
        fn core_cap_schema_cids_are_stable() {
            // CID snapshots guard against accidental protocol drift.
            assert_eq!(
                HOST_CID,
                "bafkr4igsegudkpusfsovfzun74xed4d433r7gh2gh7acujjiy6cy5um42a"
            );
            assert_eq!(
                RUNTIME_CID,
                "bafkr4ibig2jnysgetthrw3xv4h373dvzua4hao7f2mowbwydjmzw75fwpy"
            );
            assert_eq!(
                ROUTING_CID,
                "bafkr4ids5ycfp6wd4ta5nf6e7deg625pyiur6ee53u63t47dsfoiwv5zsy"
            );
            assert_eq!(
                IDENTITY_CID,
                "bafkr4iakqlclxvdrgqk63shamitujssfmdnkzroyr5fw7wxjwcyhrqtpjy"
            );
            assert_eq!(
                HTTP_CLIENT_CID,
                "bafkr4ibch3gln5hzay6uivfxkwb5gsqphlkxn75rh3gb6fj2viznd33ari"
            );
        }
    }
}

pub mod epoch;
pub mod membrane;
pub mod terminal;

pub use epoch::{Epoch, EpochGuard, Provenance};
pub use membrane::{membrane_client, GraftBuilder, MembraneServer, NoExtension};
pub use terminal::{AllowAllPolicy, AuthPolicy, TerminalServer, VerifyingKeyPolicy};
