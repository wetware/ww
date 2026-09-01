wit_bindgen::generate!({
    path: "wit",
    world: "fixture",
    async: true,
});

struct Fixture;

impl exports::wetware::p3_fixture::probe::Guest for Fixture {
    async fn ping(input: u32) -> u32 {
        input.wrapping_add(1)
    }
}

export!(Fixture);
