//! Minimal WASI echo cell for integration testing.
//!
//! The cell copies stdin to stdout and exits. It has no RPC runtime.

use std::io::Write;

mod bindings {
    wit_bindgen::generate!({
        path: "../../std/system/wit",
        world: "sync-command",
        generate_all,
    });
}

struct EchoCell;

impl bindings::exports::wasi::cli::run::Guest for EchoCell {
    async fn run() -> Result<(), ()> {
        let mut input = std::io::stdin().lock();
        let mut output = std::io::stdout().lock();
        std::io::copy(&mut input, &mut output).map_err(|_| ())?;
        output.flush().map_err(|_| ())
    }
}

bindings::export!(EchoCell with_types_in bindings);
