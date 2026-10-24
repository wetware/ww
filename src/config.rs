/*! Top-level configuration module for Wetware

This module centralizes configuration primitives that were previously
scoped under `cli::config`. Moving these items to the crate root allows
non-CLI subsystems (like `cell`) to depend on configuration without
creating circular dependencies.

Exports:
- `init_tracing()`: initializes global tracing subscriber

*/

/// Wetware-internal workspace crates whose `tracing::*` calls should follow
/// the binary's default log level. After the #444 workspace split, events
/// from these crates emit at their own target (e.g. `rpc::vat_listener`),
/// not under `ww::*`, so a bare `ww=info` filter would silence them.
const INTERNAL_CRATES: &[&str] = &[
    "ww", "atom", "cache", "cell", "ipfs", "membrane", "rpc", "stem",
];

fn default_filter(level: &str) -> String {
    INTERNAL_CRATES
        .iter()
        .map(|c| format!("{c}={level}"))
        .collect::<Vec<_>>()
        .join(",")
}

/// Initialize tracing using `RUST_LOG` and `WW_LOG_FORMAT`.
///
/// Default log level depends on context:
/// - Destination TTY (interactive command): `warn`
/// - Destination non-TTY (daemon/pipe): `info`
///
/// Applied uniformly across all wetware-internal workspace crates
/// (see `INTERNAL_CRATES`).
///
/// A valid `RUST_LOG` replaces the defaults.
///
/// `WW_LOG_FORMAT` accepts `auto` (default), `compact`, or `json`. Auto uses
/// compact text on a terminal and JSON Lines otherwise. Invalid values fall
/// back to auto with a warning (subject to `RUST_LOG`), without echoing the value.
/// Compact text uses ANSI only on a terminal with absent or empty `NO_COLOR`.
/// JSON contains typed event fields and the current span, without the span list.
///
/// Host diagnostics use stderr, preserving stdout for command results.
/// Attempts to initialize a global `tracing_subscriber` (no-op if already set).
pub fn init_tracing() {
    #[cfg(not(target_arch = "wasm32"))]
    {
        use std::{env::VarError, io::IsTerminal};

        let is_terminal = std::io::stderr().is_terminal();
        let level = if is_terminal { "warn" } else { "info" };
        let filter = tracing_subscriber::EnvFilter::try_from_default_env()
            .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new(default_filter(level)));
        let (json, invalid_format) = match std::env::var("WW_LOG_FORMAT").as_deref() {
            Ok("compact") => (false, false),
            Ok("json") => (true, false),
            Ok("auto") | Err(VarError::NotPresent) => (!is_terminal, false),
            _ => (!is_terminal, true),
        };
        let no_color = std::env::var_os("NO_COLOR").is_some_and(|value| !value.is_empty());
        let subscriber = tracing_subscriber::fmt()
            .with_writer(std::io::stderr)
            .with_env_filter(filter);
        let initialized = if json {
            subscriber
                .json()
                .with_ansi(false)
                .with_current_span(true)
                .with_span_list(false)
                .try_init()
        } else {
            subscriber
                .compact()
                .with_ansi(is_terminal && !no_color)
                .try_init()
        };
        // Do not emit additional diagnostics through a subscriber owned by a caller.
        if initialized.is_ok() && invalid_format {
            tracing::warn!("invalid WW_LOG_FORMAT; expected auto, compact, or json; using auto");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Regression for #448: every wetware-internal workspace crate must have
    /// its own directive in the default filter, otherwise `tracing::*`
    /// events from that crate are silenced (their target is the crate
    /// name, not `ww::<crate>`).
    #[test]
    fn default_filter_covers_all_workspace_crates() {
        let f = default_filter("info");
        for crate_name in INTERNAL_CRATES {
            assert!(
                f.split(',').any(|d| d == format!("{crate_name}=info")),
                "default_filter is missing a directive for {crate_name}: {f:?}"
            );
        }
    }

    /// A typo or stray character in `INTERNAL_CRATES` would otherwise blow up
    /// at runtime when `init_tracing` is called.
    #[test]
    fn default_filter_parses_as_env_filter() {
        for level in ["info", "warn", "debug", "trace"] {
            let f = default_filter(level);
            tracing_subscriber::EnvFilter::try_new(&f)
                .unwrap_or_else(|e| panic!("default_filter({level}) = {f:?} did not parse: {e}"));
        }
    }
}
