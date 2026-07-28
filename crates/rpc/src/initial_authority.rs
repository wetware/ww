//! Immutable record of the exact named capabilities delegated at child birth.

use authority::membrane_capnp;

use crate::named_capability::{decode_exports, encode_exports, NamedCapabilities};

/// The complete parent-delegated authority assigned to a child at birth.
///
/// The record contains only validated named capability references. It has no
/// mutation API and no ambient host, runtime, routing, identity, storage, HTTP,
/// policy, provenance, supervision, or observability state.
///
/// ```compile_fail
/// # use rpc::{InitialAuthorityRecord, NamedCapabilities};
/// let mut record = InitialAuthorityRecord::new(NamedCapabilities::default());
/// record.grants_mut().clear();
/// ```
#[derive(Clone, Default)]
pub struct InitialAuthorityRecord {
    grants: NamedCapabilities,
}

impl InitialAuthorityRecord {
    pub fn new(grants: NamedCapabilities) -> Self {
        Self { grants }
    }

    /// Decode, validate, and retain a wire grant list.
    pub fn decode(
        reader: capnp::struct_list::Reader<'_, membrane_capnp::export::Owned>,
    ) -> Result<Self, capnp::Error> {
        decode_exports(reader).map(Self::new)
    }

    pub fn grants(&self) -> &NamedCapabilities {
        &self.grants
    }

    pub fn encode(
        &self,
        builder: capnp::struct_list::Builder<'_, membrane_capnp::export::Owned>,
    ) -> Result<(), capnp::Error> {
        encode_exports(&self.grants, builder)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use authority::system_capnp;
    use capnp::traits::{Imbue, ImbueMut};
    use std::cell::Cell;
    use std::rc::Rc;

    struct DropTrackedServer {
        dropped: Rc<Cell<bool>>,
    }

    impl Drop for DropTrackedServer {
        fn drop(&mut self) {
            self.dropped.set(true);
        }
    }

    impl system_capnp::host::Server for DropTrackedServer {}

    fn tracked_cap(dropped: Rc<Cell<bool>>) -> capnp::capability::Client {
        let host: system_capnp::host::Client = capnp_rpc::new_client(DropTrackedServer { dropped });
        host.client
    }

    #[test]
    fn empty_record_succeeds() {
        assert!(InitialAuthorityRecord::default().grants().is_empty());
    }

    #[test]
    fn record_contains_exact_validated_grants_and_no_other_storage() {
        let grants = NamedCapabilities::try_from_pairs([
            ("alpha", tracked_cap(Rc::new(Cell::new(false)))),
            ("beta", tracked_cap(Rc::new(Cell::new(false)))),
        ])
        .unwrap();
        let record = InitialAuthorityRecord::new(grants);
        assert_eq!(
            record
                .grants()
                .iter()
                .map(|entry| entry.name())
                .collect::<Vec<_>>(),
            ["alpha", "beta"]
        );
        assert_eq!(
            std::mem::size_of::<InitialAuthorityRecord>(),
            std::mem::size_of::<NamedCapabilities>(),
            "the record stores only its validated grant collection"
        );
    }

    #[test]
    fn record_ownership_pins_and_releases_its_test_local_client_reference() {
        let dropped = Rc::new(Cell::new(false));
        let parent_reference = tracked_cap(dropped.clone());
        let record = InitialAuthorityRecord::new(
            NamedCapabilities::try_from_pairs([("held", parent_reference.clone())]).unwrap(),
        );

        drop(parent_reference);
        assert!(
            !dropped.get(),
            "dropping the parent's reference must not revoke a committed grant"
        );

        drop(record);
        assert!(
            dropped.get(),
            "with no other RPC owners, dropping the record releases the fixture's final client reference"
        );
    }

    #[test]
    fn repeated_record_encoding_is_idempotent_and_record_is_unchanged() {
        let record = InitialAuthorityRecord::new(
            NamedCapabilities::try_from_pairs([("held", tracked_cap(Rc::new(Cell::new(false))))])
                .unwrap(),
        );

        for _ in 0..2 {
            let mut message = capnp::message::Builder::new_default();
            let mut cap_table = Vec::new();
            {
                let mut results =
                    message.init_root::<membrane_capnp::membrane::graft_results::Builder<'_>>();
                results.imbue_mut(&mut cap_table);
                let exports = results.reborrow().init_caps(record.grants().len() as u32);
                record.encode(exports).unwrap();
            }
            let mut results = message
                .get_root_as_reader::<membrane_capnp::membrane::graft_results::Reader<'_>>()
                .unwrap();
            results.imbue(&cap_table);
            let decoded = InitialAuthorityRecord::decode(results.get_caps().unwrap()).unwrap();
            assert_eq!(
                decoded
                    .grants()
                    .iter()
                    .map(|entry| entry.name())
                    .collect::<Vec<_>>(),
                ["held"]
            );
        }

        assert_eq!(
            record
                .grants()
                .iter()
                .map(|entry| entry.name())
                .collect::<Vec<_>>(),
            ["held"]
        );
    }
}
