mod bindings {
    wit_bindgen::generate!({
        path: "wit",
        world: "probe",
    });
}

struct RoutingKeyProbe;

impl bindings::Guest for RoutingKeyProbe {
    fn probe() -> String {
        routing_key::derive(b"ww.chess.v1")
    }
}

bindings::export!(RoutingKeyProbe with_types_in bindings);
