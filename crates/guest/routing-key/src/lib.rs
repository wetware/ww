//! Optional guest binding for canonical routing-key derivation.

mod bindings {
    wit_bindgen::generate!({
        path: "wit",
        world: "key-client",
        generate_all,
    });
}

/// Derive the canonical CID text used as a provider-routing key.
pub fn derive(data: &[u8]) -> String {
    bindings::wetware::routing::key::derive(data)
}
