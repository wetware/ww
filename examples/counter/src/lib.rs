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

impl wagi::AsyncGuest for CounterCell {
    async fn run() -> Result<(), ()> {
        let count: u64 = 0;
        let ct = ("Content-Type", "text/plain");

        let (status, body) = match wagi::method().as_str() {
            "GET" => (200, count.to_string()),
            "POST" => (200, (count + 1).to_string()),
            _ => (405, "Method Not Allowed".to_string()),
        };
        wagi::respond_bytes_async(status, &[ct], body.as_bytes())
            .await
            .map_err(|error| eprintln!("counter response failed: {error}"))
    }
}

wagi::export_async!(CounterCell);
