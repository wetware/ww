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
    "ww", "atom", "cache", "cell", "glia", "ipfs", "membrane", "rpc", "stem",
];

fn default_filter(level: &str) -> String {
    INTERNAL_CRATES
        .iter()
        .map(|c| format!("{c}={level}"))
        .collect::<Vec<_>>()
        .join(",")
}

/// Initialize tracing using `RUST_LOG`.
///
/// Default log level depends on context:
/// - TTY (interactive shell): `warn` — keep the Glia REPL clean
/// - Non-TTY (daemon/pipe): `info` — standard Rust log behavior
///
/// Applied uniformly across all wetware-internal workspace crates
/// (see `INTERNAL_CRATES`).
///
/// `RUST_LOG` always takes precedence when set.
///
/// When `stderr` is true, logs are written to stderr instead of stdout.
/// This is required for MCP mode where stdout carries JSON-RPC.
///
/// Attempts to initialize a global `tracing_subscriber` (no-op if already set).
pub fn init_tracing_to_stderr(stderr: bool) {
    #[cfg(not(target_arch = "wasm32"))]
    {
        use std::io::IsTerminal;
        let level = if std::io::stdout().is_terminal() {
            "warn"
        } else {
            "info"
        };
        let default_filter = default_filter(level);
        let filter = tracing_subscriber::EnvFilter::try_from_default_env()
            .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new(default_filter));
        if stderr {
            let _ = tracing_subscriber::fmt()
                .with_writer(std::io::stderr)
                .with_env_filter(filter)
                .try_init();
        } else {
            let _ = tracing_subscriber::fmt().with_env_filter(filter).try_init();
        }
    }
}

/// Initialize tracing using `RUST_LOG` (default: `ww=info`).
///
/// Attempts to initialize a global `tracing_subscriber` (no-op if already set).
pub fn init_tracing() {
    init_tracing_to_stderr(false);
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
    /// at runtime when `init_tracing_to_stderr` is called.
    #[test]
    fn default_filter_parses_as_env_filter() {
        for level in ["info", "warn", "debug", "trace"] {
            let f = default_filter(level);
            tracing_subscriber::EnvFilter::try_new(&f)
                .unwrap_or_else(|e| panic!("default_filter({level}) = {f:?} did not parse: {e}"));
        }
    }
}
