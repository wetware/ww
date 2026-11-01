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
///
/// `WW_OTLP_ENDPOINT` opts into local Cell trace export. Retain the returned
/// guard until after runtime shutdown; dropping it stops admission and drains
/// the exporter for at most two seconds. Invalid telemetry settings disable
/// export and emit event 111 without echoing configuration values.
#[cfg(not(target_arch = "wasm32"))]
pub fn init_tracing() -> Option<crate::telemetry::TelemetryGuard> {
    #[cfg(not(target_arch = "wasm32"))]
    {
        use std::{env::VarError, io::IsTerminal};
        use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt};

        // Repeated Kernel::run calls must not start temporary exporter threads.
        // Unlike tracing::dispatcher::has_been_set(), this does not mistake a
        // previously scoped test subscriber for a permanent global subscriber.
        static INITIALIZED: std::sync::OnceLock<()> = std::sync::OnceLock::new();
        if INITIALIZED.set(()).is_err() {
            return None;
        }

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
        let telemetry = crate::telemetry::Config::from_env()
            .and_then(|config| config.map(crate::telemetry::start).transpose());
        let invalid_telemetry = telemetry.is_err();
        let (layer, guard) = match telemetry {
            Ok(Some((layer, guard))) => (Some(layer), Some(guard)),
            _ => (None, None),
        };
        let initialized = tracing_subscriber::registry()
            .with(filter)
            .with(layer)
            .with(
                tracing_subscriber::fmt::layer()
                    .with_ansi(false)
                    .log_internal_errors(false)
                    .with_writer(std::io::stderr)
                    .fmt_fields(crate::logging::SafeFields)
                    .event_format(crate::logging::SafeEvent {
                        json,
                        ansi: !json && is_terminal && !no_color,
                    }),
            )
            .try_init();
        // Do not emit additional diagnostics through a subscriber owned by a caller.
        if initialized.is_ok() && invalid_format {
            tracing::warn!(
                event_code = 103u64,
                "invalid WW_LOG_FORMAT; expected auto, compact, or json; using auto"
            );
        }
        if initialized.is_ok() && invalid_telemetry {
            // Never echo configuration: it might contain credentials.
            tracing::warn!(
                event_code = rpc::observability::EventCode::TelemetryDisabled as u16,
                "invalid local telemetry configuration; trace export disabled"
            );
        }
        if initialized.is_ok() {
            guard
        } else {
            None
        }
    }
}

#[cfg(target_arch = "wasm32")]
pub fn init_tracing() {}

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
    #[test]
    fn telemetry_configuration_failure_is_classified_and_redacted() {
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "config::tests::telemetry_configuration_probe",
                "--ignored",
                "--nocapture",
            ])
            .env("WW_LOG_FORMAT", "json")
            .env("RUST_LOG", "ww=warn")
            .env(
                "WW_OTLP_ENDPOINT",
                "http://private-secret@127.0.0.1/v1/traces",
            )
            .output()
            .unwrap();
        assert!(output.status.success());
        let stderr = String::from_utf8(output.stderr).unwrap();
        assert!(!stderr.contains("private-secret"));
        let events: Vec<serde_json::Value> = stderr
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0]["fields"]["event_code"], 111);
        assert_eq!(events[0]["fields"]["outcome"], "disabled");
        assert_eq!(
            events[0]["fields"]["reason"],
            "configuration_or_start_failed"
        );
    }

    #[test]
    #[ignore = "isolated global subscriber entry point"]
    fn telemetry_configuration_probe() {
        assert!(init_tracing().is_none());
        // A second initialization does not re-read the invalid configuration,
        // emit another warning, or start an exporter worker.
        for _ in 0..100 {
            assert!(init_tracing().is_none());
        }
    }
}
