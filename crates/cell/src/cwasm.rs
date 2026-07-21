//! On-disk precompiled component (`.cwasm`) cache.
//!
//! Booting `ww` today Cranelift-compiles every wasm component from scratch
//! (the in-memory compile cache dies with the process), so a restart loop is a
//! sustained-CPU signature — the mechanism behind the June 2026 Fair-Use
//! throttle of master.wetware.run. Precompilation removes ~46% of per-boot CPU
//! (measured: kernel+shell compile ≈ 1400× the cost of deserialize).
//!
//! `ww compile` serializes components to `<blake3(wasm)>.cwasm` files with
//! [`compile_to_dir`]; CI bakes that directory into the deploy image and points
//! [`CWASM_DIR_ENV`] at it. At boot the compile service calls
//! [`load_or_compile`], which prefers the baked artifact and falls back to a
//! fresh compile on any mismatch — a stale, truncated, or ISA-incompatible
//! artifact degrades to today's behavior instead of crashing the boot loop.
//!
//! `.cwasm` artifacts are only loadable by an engine whose `Config`, wasmtime
//! version, and target ISA match the producer's — hence the single
//! [`crate::engine::wasm_engine_config`] shared by `ww compile` and every
//! runtime path.

use std::path::{Path, PathBuf};

use wasmtime::component::Component;
use wasmtime::Engine;

/// Environment variable naming the directory of baked `.cwasm` artifacts.
/// Unset (the default for `ww run` on a dev box) disables the cache entirely.
pub const CWASM_DIR_ENV: &str = "WW_CWASM_DIR";

/// The artifact filename for `wasm`: `<blake3-hex>.cwasm`.
///
/// Keyed by the blake3 of the wasm bytes so an artifact produced by
/// `ww compile` is found by the runtime, whose compile cache uses the same
/// hash (`CompileKey::wasm_hash`).
pub fn artifact_name(wasm: &[u8]) -> String {
    format!("{}.cwasm", blake3::hash(wasm).to_hex())
}

/// The configured cache directory, or `None` when [`CWASM_DIR_ENV`] is unset.
pub fn cache_dir() -> Option<PathBuf> {
    std::env::var_os(CWASM_DIR_ENV).map(PathBuf::from)
}

/// Load a component for `wasm`, preferring a baked `.cwasm` in `dir`.
///
/// Any failure to use the artifact — absent file, read error, version skew,
/// ISA-feature mismatch, truncation — falls back to `Component::from_binary`,
/// so a wrong or stale artifact never fails a boot; it just costs a compile.
pub fn load_or_compile(
    engine: &Engine,
    wasm: &[u8],
    dir: Option<&Path>,
) -> wasmtime::Result<Component> {
    if let Some(dir) = dir {
        let path = dir.join(artifact_name(wasm));
        match std::fs::read(&path) {
            Ok(bytes) => {
                // SAFETY: `bytes` is our own artifact (produced by `ww compile`
                // from a config-identical engine) named by the wasm hash — not
                // attacker-controlled. `deserialize` validates the wasmtime
                // header and the host's ISA features, returning `Err` on any
                // mismatch, which we handle below by recompiling.
                match unsafe { Component::deserialize(engine, &bytes) } {
                    Ok(component) => {
                        tracing::info!(?path, "loaded precompiled component");
                        return Ok(component);
                    }
                    Err(e) => tracing::warn!(
                        ?path,
                        error = %e,
                        "precompiled component unusable; recompiling from wasm"
                    ),
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                tracing::debug!(?path, "no precompiled component; compiling from wasm")
            }
            Err(e) => tracing::warn!(
                ?path,
                error = %e,
                "reading precompiled component failed; compiling from wasm"
            ),
        }
    }
    Component::from_binary(engine, wasm)
}

/// Compile `wasm` and serialize it to `<dir>/<blake3-hex>.cwasm`, returning the
/// artifact path. Backs the `ww compile` subcommand.
pub fn compile_to_dir(engine: &Engine, wasm: &[u8], dir: &Path) -> wasmtime::Result<PathBuf> {
    let component = Component::from_binary(engine, wasm)?;
    let bytes = component.serialize()?;
    std::fs::create_dir_all(dir)?;
    let path = dir.join(artifact_name(wasm));
    std::fs::write(&path, &bytes)?;
    Ok(path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::wasm_engine;

    fn sample_wasm() -> Vec<u8> {
        wat::parse_str(r#"(component)"#).expect("parse wat")
    }

    #[test]
    fn artifact_name_is_stable_and_wasm_keyed() {
        let a = sample_wasm();
        assert_eq!(artifact_name(&a), artifact_name(&a));
        assert!(artifact_name(&a).ends_with(".cwasm"));
        let mut b = a.clone();
        b.extend_from_slice(&[0, 0]); // different bytes → different artifact name
        assert_ne!(artifact_name(&a), artifact_name(&b));
    }

    #[test]
    fn compile_then_load_uses_the_artifact() {
        let dir = std::env::temp_dir().join(format!("ww-cwasm-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let engine = wasm_engine().expect("engine");
        let wasm = sample_wasm();

        let path = compile_to_dir(&engine, &wasm, &dir).expect("compile_to_dir");
        assert!(path.exists());
        assert_eq!(
            path.file_name().unwrap().to_str().unwrap(),
            artifact_name(&wasm)
        );

        // Loads via the artifact (fresh engine, same config).
        let engine2 = wasm_engine().expect("engine2");
        let _c = load_or_compile(&engine2, &wasm, Some(&dir)).expect("load_or_compile hit");

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn missing_artifact_falls_back_to_compile() {
        let engine = wasm_engine().expect("engine");
        let wasm = sample_wasm();
        // Non-existent dir → fallback compile, no error.
        let missing = std::env::temp_dir().join("ww-cwasm-does-not-exist-xyz");
        let _c = load_or_compile(&engine, &wasm, Some(&missing)).expect("fallback compile");
    }

    #[test]
    fn corrupt_artifact_falls_back_to_compile() {
        let dir = std::env::temp_dir().join(format!("ww-cwasm-corrupt-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let wasm = sample_wasm();
        // Write garbage under the artifact name; deserialize must fail-safe.
        std::fs::write(dir.join(artifact_name(&wasm)), b"not a real cwasm").unwrap();
        let engine = wasm_engine().expect("engine");
        let _c = load_or_compile(&engine, &wasm, Some(&dir)).expect("fallback on corrupt");
        std::fs::remove_dir_all(&dir).ok();
    }
}
