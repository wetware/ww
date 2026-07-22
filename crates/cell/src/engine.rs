//! Single source of truth for the wasmtime engine configuration.
//!
//! Precompiled `.cwasm` artifacts (optionally produced by `ww compile` or
//! warmed locally at boot, then loaded via `Component::deserialize`) are only
//! loadable by an `Engine` built with the *identical* `Config` — engine
//! settings, wasmtime version, and target ISA must all match, or
//! deserialization fails. Historically this config was duplicated across
//! `src/services.rs`, `crates/cell/src/proc.rs`, and `src/host.rs` with
//! drift (some paths omitted `epoch_interruption`), which would silently
//! invalidate cached artifacts. Everything now goes through
//! [`wasm_engine_config`] so that drift is impossible by construction.

use wasmtime::{Config, Engine};

/// The canonical wasmtime `Config` for wetware cells.
///
/// Every `Engine` that runs a guest — the ExecutorPool, per-cell `Proc`, the
/// integration-test host — and every path that *produces* a `.cwasm`
/// (`ww compile`) MUST build its engine from this. Keep it deterministic:
/// precompiled artifacts are selected using this engine's compatibility
/// fingerprint, so artifacts made by another architecture or configuration
/// cannot be mistaken for a local cache hit.
pub fn wasm_engine_config() -> Config {
    let mut config = Config::new();
    // Fuel: cooperative preemption for guests (Trap::OutOfFuel).
    config.consume_fuel(true);
    // Epoch: the ExecutorPool's tick task calls Engine::increment_epoch() to
    // reach every Store's epoch_deadline_callback. Enabled everywhere so a
    // .cwasm compiled here is loadable by the pool's engine.
    config.epoch_interruption(true);

    config
}

/// Build an `Engine` from [`wasm_engine_config`]. Convenience for the common
/// case; use the `Config` directly only when you need to tweak it further
/// (which, for `.cwasm` compatibility, you generally should not).
pub fn wasm_engine() -> wasmtime::Result<Engine> {
    Engine::new(&wasm_engine_config())
}

#[cfg(test)]
mod tests {
    use super::*;
    use wasmtime::component::Component;

    /// The load-bearing invariant for local precompilation: a component
    /// serialized by an engine from `wasm_engine_config` deserializes cleanly
    /// in a *fresh* engine from the same config. If this breaks, cached .cwasm
    /// won't load from the local cache at boot.
    #[test]
    fn cwasm_round_trips_across_fresh_engines() {
        // Minimal valid component.
        let wat = r#"(component)"#;
        let bytes = wat::parse_str(wat).expect("parse wat");

        let e1 = wasm_engine().expect("engine 1");
        let component = Component::new(&e1, &bytes).expect("compile");
        let cwasm = component.serialize().expect("serialize");

        // Fresh engine, same config — must deserialize.
        let e2 = wasm_engine().expect("engine 2");
        // SAFETY: cwasm was produced by us from a config-identical engine.
        let _re = unsafe { Component::deserialize(&e2, &cwasm).expect("deserialize") };
    }
}
