//! Mindshare guest: symmetric peer-to-peer context sharing for LLMs.
//!
//! Demonstrates:
//!   - Bidirectional Mindshare capability exchange
//!   - ContextWriter for pushing CID-addressed content
//!   - Rate-limited Prompt capability
//!
//! Cell logic will be implemented in a follow-up PR.

use wasip2::cli::stderr::get_stderr;
use wasip2::exports::cli::run::Guest;

// Cap'n Proto generated modules — uncomment as cell logic lands.
// mod system_capnp { include!(concat!(env!("OUT_DIR"), "/system_capnp.rs")); }
// mod stem_capnp { include!(concat!(env!("OUT_DIR"), "/stem_capnp.rs")); }
// mod routing_capnp { include!(concat!(env!("OUT_DIR"), "/routing_capnp.rs")); }
// mod http_capnp { include!(concat!(env!("OUT_DIR"), "/http_capnp.rs")); }
// mod mindshare_capnp { include!(concat!(env!("OUT_DIR"), "/mindshare_capnp.rs")); }

// Build-time schema constants: MINDSHARE_SCHEMA (&[u8]) and MINDSHARE_CID (&str).
// Vat publication will use the service name below; the schema CID is metadata.
include!(concat!(env!("OUT_DIR"), "/schema_ids.rs"));

const MINDSHARE_SERVICE: &str = "mindshare";

// ---------------------------------------------------------------------------
// Logging (WASI stderr)
// ---------------------------------------------------------------------------

struct StderrLogger;

impl log::Log for StderrLogger {
    fn enabled(&self, metadata: &log::Metadata<'_>) -> bool {
        metadata.level() <= log::Level::Trace
    }

    fn log(&self, record: &log::Record<'_>) {
        if !self.enabled(record.metadata()) {
            return;
        }
        let stderr = get_stderr();
        let _ = stderr.blocking_write_and_flush(
            format!("[{}] {}\n", record.level(), record.args()).as_bytes(),
        );
    }

    fn flush(&self) {}
}

static LOGGER: StderrLogger = StderrLogger;

fn init_logging() {
    if log::set_logger(&LOGGER).is_ok() {
        log::set_max_level(log::LevelFilter::Trace);
    }
}

// ---------------------------------------------------------------------------
// Entry point
// ---------------------------------------------------------------------------

struct MindshareGuest;

impl Guest for MindshareGuest {
    fn run() -> Result<(), ()> {
        init_logging();
        // Stub: cell logic will be implemented in a follow-up PR.
        // For now, just verify the schema compiles.
        log::info!("mindshare service name: {MINDSHARE_SERVICE}");
        Ok(())
    }
}

wasip2::cli::command::export!(MindshareGuest);
