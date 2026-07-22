//! On-disk precompiled component (`.cwasm`) cache.
//!
//! Booting `ww` today Cranelift-compiles every wasm component from scratch
//! (the in-memory compile cache dies with the process), so a restart loop is a
//! sustained-CPU signature — the mechanism behind the June 2026 Fair-Use
//! throttle of master.wetware.run. Precompilation removes ~46% of per-boot CPU
//! (measured: kernel+shell compile ≈ 1400× the cost of deserialize).
//!
//! `ww compile` serializes components into a host-local cache with
//! [`compile_to_dir`]. At boot the compile service calls [`load_or_compile`],
//! which prefers a locally compatible artifact and, after a cache miss, writes
//! the newly compiled component for the next boot. A stale, truncated, or
//! ISA-incompatible artifact degrades to a fresh compile instead of crashing
//! the boot loop.
//!
//! `.cwasm` artifacts are only loadable by an engine whose `Config`, wasmtime
//! version, and target ISA match the producer's — hence the single
//! [`crate::engine::wasm_engine_config`] shared by `ww compile` and every
//! runtime path.

use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};

use wasmtime::component::Component;
use wasmtime::Engine;

/// Environment variable naming a writable, host-local `.cwasm` cache.
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

/// A stable directory name for artifacts compatible with `engine`.
///
/// Wasmtime's compatibility hash includes its target and compilation settings.
/// Keeping it in the cache path makes a shared or restored cache harmless:
/// another architecture simply gets a separate entry rather than repeatedly
/// trying to deserialize incompatible native code.
pub fn compatibility_dir(engine: &Engine) -> String {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    engine.precompile_compatibility_hash().hash(&mut hasher);
    format!("{:016x}", hasher.finish())
}

fn artifact_path(engine: &Engine, wasm: &[u8], dir: &Path) -> PathBuf {
    dir.join(compatibility_dir(engine))
        .join(artifact_name(wasm))
}

/// The configured cache directory, or `None` when [`CWASM_DIR_ENV`] is unset.
pub fn cache_dir() -> Option<PathBuf> {
    std::env::var_os(CWASM_DIR_ENV).map(PathBuf::from)
}

/// Load a component for `wasm`, preferring a compatible local `.cwasm` in
/// `dir` and warming the cache after a miss.
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
        let path = artifact_path(engine, wasm, dir);
        match std::fs::read(&path) {
            Ok(bytes) => {
                // SAFETY: this cache is written only by this process after
                // compiling the corresponding wasm, then published by atomic
                // rename. Operators must not share the writable cache with
                // untrusted principals: arbitrary serialized components are
                // native code.
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
                tracing::debug!(?path, "precompiled component absent; compiling from wasm")
            }
            Err(e) => tracing::warn!(
                ?path,
                error = %e,
                "reading precompiled component failed; compiling from wasm"
            ),
        }
    }
    let component = Component::from_binary(engine, wasm)?;
    if let Some(dir) = dir {
        if let Err(e) = write_component(engine, wasm, dir, &component) {
            tracing::warn!(error = %e, cache_dir = ?dir, "failed to warm precompiled component cache");
        }
    }
    Ok(component)
}

/// Compile `wasm` and serialize it below
/// `<dir>/<compatibility-hash>/<blake3(wasm)>.cwasm`, returning the artifact
/// path. Backs the `ww compile` subcommand.
pub fn compile_to_dir(engine: &Engine, wasm: &[u8], dir: &Path) -> wasmtime::Result<PathBuf> {
    let component = Component::from_binary(engine, wasm)?;
    write_component(engine, wasm, dir, &component)
}

fn write_component(
    engine: &Engine,
    wasm: &[u8],
    dir: &Path,
    component: &Component,
) -> wasmtime::Result<PathBuf> {
    let path = artifact_path(engine, wasm, dir);
    let parent = path.parent().expect("artifact path always has a parent");
    std::fs::create_dir_all(parent)?;
    let bytes = component.serialize()?;
    let tmp = parent.join(format!(
        ".{}.{}.tmp",
        artifact_name(wasm),
        std::process::id()
    ));
    std::fs::write(&tmp, bytes)?;
    std::fs::rename(&tmp, &path)?;
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
    fn cache_dir_reflects_env() {
        // No other test reads WW_CWASM_DIR via cache_dir() (they pass dirs
        // explicitly), so mutating the process env here doesn't race them.
        std::env::remove_var(CWASM_DIR_ENV);
        assert_eq!(cache_dir(), None);
        std::env::set_var(CWASM_DIR_ENV, "/cwasm");
        assert_eq!(cache_dir().as_deref(), Some(std::path::Path::new("/cwasm")));
        std::env::remove_var(CWASM_DIR_ENV);
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
        assert_eq!(
            path.parent().unwrap().file_name().unwrap(),
            std::ffi::OsStr::new(&compatibility_dir(&engine))
        );

        // Loads via the artifact (fresh engine, same config).
        let engine2 = wasm_engine().expect("engine2");
        let _c = load_or_compile(&engine2, &wasm, Some(&dir)).expect("load_or_compile hit");

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn missing_artifact_compiles_and_warms_cache() {
        let engine = wasm_engine().expect("engine");
        let wasm = sample_wasm();
        let missing = std::env::temp_dir().join(format!("ww-cwasm-miss-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&missing);
        let _c = load_or_compile(&engine, &wasm, Some(&missing)).expect("fallback compile");
        assert!(artifact_path(&engine, &wasm, &missing).exists());
        std::fs::remove_dir_all(&missing).ok();
    }

    #[test]
    fn cache_write_failure_falls_back_to_compile() {
        let root = std::env::temp_dir().join(format!("ww-cwasm-file-{}", std::process::id()));
        let _ = std::fs::remove_file(&root);
        // A regular file cannot contain the compatibility subdirectory, so
        // cache warming fails after compilation. The component must still run.
        std::fs::write(&root, b"not a cache directory").unwrap();
        let engine = wasm_engine().expect("engine");
        let component = load_or_compile(&engine, &sample_wasm(), Some(&root));
        assert!(
            component.is_ok(),
            "cache write failure must not fail compilation"
        );
        std::fs::remove_file(&root).ok();
    }

    #[cfg(unix)]
    #[test]
    fn unreadable_artifact_falls_back_to_compile() {
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir().join(format!("ww-cwasm-noperm-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let wasm = sample_wasm();
        let engine = wasm_engine().expect("engine");
        let path = artifact_path(&engine, &wasm, &dir);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, b"whatever").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o000)).unwrap();
        // Read fails with EACCES (not NotFound) — must degrade to compile.
        let _c = load_or_compile(&engine, &wasm, Some(&dir)).expect("fallback on unreadable");
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).ok();
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn corrupt_artifact_falls_back_to_compile() {
        let dir = std::env::temp_dir().join(format!("ww-cwasm-corrupt-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let wasm = sample_wasm();
        // Write garbage under the artifact name; deserialize must fail-safe.
        let engine = wasm_engine().expect("engine");
        let path = artifact_path(&engine, &wasm, &dir);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, b"not a real cwasm").unwrap();
        let _c = load_or_compile(&engine, &wasm, Some(&dir)).expect("fallback on corrupt");
        std::fs::remove_dir_all(&dir).ok();
    }
}
