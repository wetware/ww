//! Canonical Wasmtime engine construction and persistent cache policy.
//!
//! Wasmtime owns compiled-code serialization, compatibility keys, and cache
//! cleanup. Wetware supplies a local directory and always falls back to an
//! uncached engine when that optimization cannot be initialized.

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, OnceLock};

use wasmtime::component::Component;
use wasmtime::{Cache, CacheConfig, Config, Engine};

/// Directory for Wasmtime's persistent compilation cache.
pub const CWASM_DIR_ENV: &str = "WW_CWASM_DIR";
/// Wasmtime cache cleanup threshold, in bytes.
pub const CWASM_CACHE_MAX_BYTES_ENV: &str = "WW_CWASM_CACHE_MAX_BYTES";
/// Conservative default below the production PVC capacity.
pub const DEFAULT_CWASM_CACHE_MAX_BYTES: u64 = 320 * 1024 * 1024;

/// Operational state of the optional persistent compilation cache.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum WasmtimeCacheState {
    Disabled,
    Enabled,
    Fallback,
}

impl WasmtimeCacheState {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Disabled => "disabled",
            Self::Enabled => "enabled",
            Self::Fallback => "fallback",
        }
    }
}

/// Point-in-time data used by the local Prometheus endpoint.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct WasmtimeCacheSnapshot {
    pub state: WasmtimeCacheState,
    pub hits: u64,
    /// Successful cache writes after compilation, not lookup misses.
    pub stores: u64,
    /// Calls to the canonical `Component::from_binary` path. Cache hits are
    /// included because Wasmtime resolves them inside that API.
    pub component_compilations: u64,
}

/// Read-only handle for Wasmtime cache state and counters.
#[derive(Clone)]
pub struct WasmtimeCacheMetrics {
    factory: Arc<EngineFactory>,
}

impl WasmtimeCacheMetrics {
    pub fn snapshot(&self) -> WasmtimeCacheSnapshot {
        self.factory.snapshot()
    }
}

#[derive(Clone, Debug)]
struct CacheSettings {
    directory: PathBuf,
    max_bytes: u64,
}

impl CacheSettings {
    fn from_env() -> Result<Option<Self>, String> {
        let Some(directory) = std::env::var_os(CWASM_DIR_ENV).map(PathBuf::from) else {
            return Ok(None);
        };

        let max_bytes = match std::env::var(CWASM_CACHE_MAX_BYTES_ENV) {
            Ok(value) => value.parse::<u64>().map_err(|_| {
                format!(
                    "{CWASM_CACHE_MAX_BYTES_ENV} must be a positive integer byte count, got {value:?}"
                )
            })?,
            Err(std::env::VarError::NotPresent) => DEFAULT_CWASM_CACHE_MAX_BYTES,
            Err(error) => return Err(format!("failed to read {CWASM_CACHE_MAX_BYTES_ENV}: {error}")),
        };

        if max_bytes == 0 {
            return Err(format!(
                "{CWASM_CACHE_MAX_BYTES_ENV} must be greater than zero"
            ));
        }

        Ok(Some(Self {
            directory,
            max_bytes,
        }))
    }
}

/// The single process-wide owner of Wasmtime cache policy and counters.
struct EngineFactory {
    cache: Option<Cache>,
    state: WasmtimeCacheState,
    component_compilations: AtomicU64,
}

impl EngineFactory {
    fn from_env() -> Self {
        Self::from_settings(CacheSettings::from_env())
    }

    fn from_settings(settings: Result<Option<CacheSettings>, String>) -> Self {
        match settings {
            Ok(None) => {
                tracing::info!(state = "disabled", "Wasmtime persistent cache disabled");
                Self::without_cache(WasmtimeCacheState::Disabled)
            }
            Ok(Some(settings)) => {
                let mut config = CacheConfig::new();
                config.with_directory(&settings.directory);
                config.with_files_total_size_soft_limit(settings.max_bytes);

                match Cache::new(config) {
                    Ok(cache) => {
                        tracing::info!(
                            state = "enabled",
                            directory = %settings.directory.display(),
                            max_bytes = settings.max_bytes,
                            "Wasmtime persistent cache enabled"
                        );
                        Self {
                            cache: Some(cache),
                            state: WasmtimeCacheState::Enabled,
                            component_compilations: AtomicU64::new(0),
                        }
                    }
                    Err(error) => {
                        tracing::warn!(
                            state = "fallback",
                            directory = %settings.directory.display(),
                            error = %error,
                            "Wasmtime persistent cache unavailable; compiling without it"
                        );
                        Self::without_cache(WasmtimeCacheState::Fallback)
                    }
                }
            }
            Err(error) => {
                tracing::warn!(
                    state = "fallback",
                    error = %error,
                    "Wasmtime persistent cache configuration invalid; compiling without it"
                );
                Self::without_cache(WasmtimeCacheState::Fallback)
            }
        }
    }

    fn without_cache(state: WasmtimeCacheState) -> Self {
        Self {
            cache: None,
            state,
            component_compilations: AtomicU64::new(0),
        }
    }

    fn config(&self) -> Config {
        let mut config = Config::new();
        // Fuel: cooperative preemption for guests (Trap::OutOfFuel).
        config.consume_fuel(true);
        // Epoch: the ExecutorPool's tick task calls Engine::increment_epoch()
        // to reach every Store's epoch_deadline_callback.
        config.epoch_interruption(true);
        if let Some(cache) = &self.cache {
            config.cache(Some(cache.clone()));
        }
        config
    }

    fn engine(&self) -> wasmtime::Result<Engine> {
        Engine::new(&self.config())
    }

    fn compile_component(&self, engine: &Engine, wasm: &[u8]) -> wasmtime::Result<Component> {
        self.component_compilations.fetch_add(1, Ordering::Relaxed);
        Component::from_binary(engine, wasm)
    }

    fn snapshot(&self) -> WasmtimeCacheSnapshot {
        let (hits, stores) = self
            .cache
            .as_ref()
            .map(|cache| (cache.cache_hits() as u64, cache.cache_misses() as u64))
            .unwrap_or((0, 0));

        WasmtimeCacheSnapshot {
            state: self.state,
            hits,
            stores,
            component_compilations: self.component_compilations.load(Ordering::Relaxed),
        }
    }
}

fn engine_factory() -> &'static Arc<EngineFactory> {
    static FACTORY: OnceLock<Arc<EngineFactory>> = OnceLock::new();
    FACTORY.get_or_init(|| Arc::new(EngineFactory::from_env()))
}

/// Build the canonical Wasmtime `Config` for wetware cells.
pub fn wasm_engine_config() -> Config {
    engine_factory().config()
}

/// Build an engine from the process-shared canonical factory.
pub fn wasm_engine() -> wasmtime::Result<Engine> {
    engine_factory().engine()
}

/// Return the live Wasmtime cache state and counters.
pub fn wasmtime_cache_metrics() -> WasmtimeCacheMetrics {
    WasmtimeCacheMetrics {
        factory: Arc::clone(engine_factory()),
    }
}

/// Compile a component through the canonical accounting boundary.
pub fn compile_component(engine: &Engine, wasm: &[u8]) -> wasmtime::Result<Component> {
    engine_factory().compile_component(engine, wasm)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn component_bytes() -> Vec<u8> {
        wat::parse_str("(component)").expect("minimal component")
    }

    #[test]
    fn disabled_cache_still_compiles_components() {
        let factory = EngineFactory::from_settings(Ok(None));
        let engine = factory.engine().expect("engine");
        factory
            .compile_component(&engine, &component_bytes())
            .expect("component compiles without cache");

        assert_eq!(factory.snapshot().state, WasmtimeCacheState::Disabled);
        assert_eq!(factory.snapshot().component_compilations, 1);
    }

    #[test]
    fn configured_cache_hits_across_fresh_engines() {
        let dir = tempfile::tempdir().expect("cache directory");
        let factory = EngineFactory::from_settings(Ok(Some(CacheSettings {
            directory: dir.path().to_path_buf(),
            max_bytes: DEFAULT_CWASM_CACHE_MAX_BYTES,
        })));
        let wasm = component_bytes();

        let first = factory.engine().expect("first engine");
        factory
            .compile_component(&first, &wasm)
            .expect("first component compile");
        assert_eq!(factory.snapshot().stores, 1);

        let second = factory.engine().expect("second engine");
        factory
            .compile_component(&second, &wasm)
            .expect("second component compile");
        let snapshot = factory.snapshot();
        assert_eq!(snapshot.state, WasmtimeCacheState::Enabled);
        assert_eq!(snapshot.hits, 1);
        assert_eq!(snapshot.stores, 1);
        assert_eq!(snapshot.component_compilations, 2);
    }

    #[test]
    fn invalid_cache_directory_falls_back_without_blocking_compile() {
        let dir = tempfile::tempdir().expect("temporary directory");
        let file = dir.path().join("not-a-directory");
        std::fs::write(&file, "cache cannot be created here").expect("fixture file");
        let factory = EngineFactory::from_settings(Ok(Some(CacheSettings {
            directory: file,
            max_bytes: DEFAULT_CWASM_CACHE_MAX_BYTES,
        })));

        let engine = factory.engine().expect("fallback engine");
        factory
            .compile_component(&engine, &component_bytes())
            .expect("fallback still compiles");
        assert_eq!(factory.snapshot().state, WasmtimeCacheState::Fallback);
    }

    #[test]
    fn invalid_cache_budget_falls_back_without_blocking_compile() {
        let factory = EngineFactory::from_settings(Err("zero cache budget".to_string()));
        let engine = factory.engine().expect("fallback engine");
        factory
            .compile_component(&engine, &component_bytes())
            .expect("fallback still compiles");
        assert_eq!(factory.snapshot().state, WasmtimeCacheState::Fallback);
    }

    #[test]
    fn configured_cache_uses_requested_soft_limit() {
        let dir = tempfile::tempdir().expect("cache directory");
        let max_bytes = 123_456;
        let factory = EngineFactory::from_settings(Ok(Some(CacheSettings {
            directory: dir.path().to_path_buf(),
            max_bytes,
        })));
        assert_eq!(
            factory
                .cache
                .as_ref()
                .expect("enabled cache")
                .files_total_size_soft_limit(),
            max_bytes
        );
    }
}
