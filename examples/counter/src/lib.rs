//! Counter — HTTP/WAGI cell demo.
//!
//! WAGI (WebAssembly Gateway Interface) is CGI for WASM. The host injects
//! HTTP metadata as env vars, pipes the body to stdin, and reads a CGI
//! response from stdout. Fresh cell per request. Stateless.
//!
//! Supports:
//!   GET  /counter → "0" (counter always starts at 0 per request)
//!   POST /counter → "1" (increments from 0)
//!   *             → 405 Method Not Allowed

use wagi_guest as wagi;

struct CounterCell;

impl wagi::Guest for CounterCell {
    fn run() -> Result<(), ()> {
        let count: u64 = 0;
        let ct = ("Content-Type", "text/plain");

        match wagi::method().as_str() {
            "GET" => wagi::respond(200, &[ct], &count.to_string()),
            "POST" => wagi::respond(200, &[ct], &(count + 1).to_string()),
            _ => wagi::respond(405, &[ct], "Method Not Allowed"),
        }

        Ok(())
    }
}

wagi::export!(CounterCell);
