//! Shared helpers for the public Synapse capability ABI.

use std::collections::BTreeSet;

#[allow(unused_parens, clippy::match_single_binding)]
pub mod synapse_capnp {
    include!(concat!(env!("OUT_DIR"), "/synapse_capnp.rs"));
}

/// Validate descriptor invariants that are independent of any backend.
///
/// Method authority is keyed by `(interfaceId, ordinal)`. Names are diagnostic
/// and are intentionally not part of the collision domain.
pub fn validate_descriptor(descriptor: synapse_capnp::descriptor::Reader<'_>) -> capnp::Result<()> {
    let methods = descriptor.get_methods()?;
    let mut seen = BTreeSet::new();
    for method in methods.iter() {
        let key = (method.get_interface_id(), method.get_ordinal());
        if !seen.insert(key) {
            return Err(capnp::Error::failed(format!(
                "duplicate Synapse method key: interface=0x{:016x} ordinal={}",
                key.0, key.1
            )));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn duplicate_method_keys_fail_closed() {
        let mut message = capnp::message::Builder::new_default();
        {
            let mut descriptor = message.init_root::<synapse_capnp::descriptor::Builder<'_>>();
            let mut methods = descriptor.reborrow().init_methods(2);
            for i in 0..2 {
                let mut method = methods.reborrow().get(i);
                method.set_interface_id(0x1234);
                method.set_ordinal(7);
                method.set_name("same");
            }
        }
        let descriptor = message
            .get_root_as_reader::<synapse_capnp::descriptor::Reader<'_>>()
            .expect("descriptor reader");
        let err = validate_descriptor(descriptor).expect_err("duplicate key rejected");
        assert!(err.to_string().contains("duplicate Synapse method key"));
    }
}
