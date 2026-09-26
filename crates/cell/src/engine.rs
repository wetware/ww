//! Canonical Wasmtime engine construction and persistent cache policy.
//!
//! Wasmtime owns compiled-code serialization, compatibility keys, and cache
//! cleanup. Wetware supplies a local directory and always falls back to an
//! uncached engine when that optimization cannot be initialized.

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

use tokio::sync::watch;
use wasmtime::component::Component;
use wasmtime::{Cache, CacheConfig, Config, Engine, OperatorCost, VariableOperatorCost};

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

/// A Wasmtime engine paired with its host-published epoch clock.
#[derive(Clone)]
pub struct RuntimeEngine {
    engine: Arc<Engine>,
    epoch_rx: watch::Receiver<u64>,
    epoch_sequence: Arc<Mutex<u64>>,
}

/// The sole publisher for one [`RuntimeEngine`]'s epoch clock.
pub struct EpochPublisher {
    engine: Arc<Engine>,
    epoch_tx: watch::Sender<u64>,
    epoch_sequence: Arc<Mutex<u64>>,
}

/// One Cell's independently observed view of the runtime epoch clock.
pub struct EpochSubscription {
    epoch_rx: watch::Receiver<u64>,
    epoch_sequence: Arc<Mutex<u64>>,
}

impl RuntimeEngine {
    /// Return the raw Wasmtime engine used for compilation and Store creation.
    pub fn engine(&self) -> Arc<Engine> {
        Arc::clone(&self.engine)
    }

    /// Return the latest epoch sequence published after its engine increment.
    pub fn current_epoch(&self) -> u64 {
        *self.epoch_rx.borrow()
    }

    /// Subscribe to future epochs, treating the current publication as seen.
    pub fn subscribe(&self) -> EpochSubscription {
        let mut epoch_rx = self.epoch_rx.clone();
        epoch_rx.borrow_and_update();
        EpochSubscription {
            epoch_rx,
            epoch_sequence: Arc::clone(&self.epoch_sequence),
        }
    }
}

impl EpochPublisher {
    /// Advance Wasmtime's epoch and then publish the paired epoch sequence.
    pub fn tick(&mut self) -> u64 {
        let mut sequence = self
            .epoch_sequence
            .lock()
            .expect("runtime epoch sequence lock poisoned");
        let next = sequence
            .checked_add(1)
            .expect("runtime epoch sequence overflow");
        self.engine.increment_epoch();
        *sequence = next;
        self.epoch_tx.send_replace(next);
        next
    }
}

impl EpochSubscription {
    /// Return the latest epoch sequence published after its engine increment.
    pub fn current_epoch(&self) -> u64 {
        *self.epoch_rx.borrow()
    }

    /// Return the sequence paired with the latest Wasmtime epoch increment.
    pub(crate) fn engine_epoch(&self) -> u64 {
        *self
            .epoch_sequence
            .lock()
            .expect("runtime epoch sequence lock poisoned")
    }

    /// Wait for the next published epoch and return its sequence.
    pub async fn changed(&mut self) -> Result<u64, watch::error::RecvError> {
        self.epoch_rx.changed().await?;
        Ok(*self.epoch_rx.borrow_and_update())
    }
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
        // Wasmtime 46 added size-dependent fuel charges for bulk operations.
        // Wasmtime 45 charged only the flat operator cost. Keep that policy so
        // the Cell fuel estimator sees the same instruction accounting.
        config.operator_cost(wasmtime_45_operator_cost());
        // Epoch: the ExecutorPool's tick task calls Engine::increment_epoch()
        // to reach every Store's epoch_deadline_callback.
        config.epoch_interruption(true);
        // Native WASI P3 components use the Component Model async ABI and
        // concurrent task support. Every Cell still owns exactly one Store.
        config.wasm_component_model_async(true);
        config.wasm_component_model_async_stackful(true);
        config.wasm_component_model_threading(true);
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

fn wasmtime_45_operator_cost() -> OperatorCost {
    OperatorCost {
        variable: VariableOperatorCost {
            memory_copy_per_byte: 0,
            memory_fill_per_byte: 0,
            memory_init_per_byte: 0,
            memory_grow_per_page: 0,
            table_copy_per_element: 0,
            table_fill_per_element: 0,
            table_init_per_element: 0,
            table_grow_per_element: 0,
            array_copy_per_element: 0,
            array_fill_per_element: 0,
            array_new_data_per_element: 0,
            array_init_data_per_element: 0,
            array_new_elem_per_element: 0,
            array_init_elem_per_element: 0,
            array_new_default_per_element: 0,
            array_new_per_element: 0,
        },
        ..OperatorCost::default()
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

/// Build a canonical Wasmtime engine together with its epoch publisher.
pub fn runtime_engine() -> wasmtime::Result<(RuntimeEngine, EpochPublisher)> {
    let engine = Arc::new(wasm_engine()?);
    let (epoch_tx, epoch_rx) = watch::channel(0);
    let epoch_sequence = Arc::new(Mutex::new(0));
    Ok((
        RuntimeEngine {
            engine: Arc::clone(&engine),
            epoch_rx,
            epoch_sequence: Arc::clone(&epoch_sequence),
        },
        EpochPublisher {
            engine,
            epoch_tx,
            epoch_sequence,
        },
    ))
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

    fn memory_fill_fuel(factory: &EngineFactory, length: i32) -> u64 {
        let engine = factory.engine().expect("engine");
        let module = wasmtime::Module::new(
            &engine,
            r#"
                (module
                    (memory 1)
                    (func (export "fill") (param i32)
                        i32.const 0
                        i32.const 0
                        local.get 0
                        memory.fill))
            "#,
        )
        .expect("memory.fill module");
        let mut store = wasmtime::Store::new(&engine, ());
        store.set_epoch_deadline(u64::MAX);
        let initial_fuel = 10_000;
        store.set_fuel(initial_fuel).expect("initial fuel");
        let instance = wasmtime::Instance::new(&mut store, &module, &[]).expect("instance");
        instance
            .get_typed_func::<i32, ()>(&mut store, "fill")
            .expect("fill export")
            .call(&mut store, length)
            .expect("memory.fill call");
        initial_fuel - store.get_fuel().expect("remaining fuel")
    }

    #[test]
    fn bulk_operation_fuel_matches_wasmtime_45_flat_accounting() {
        let factory = EngineFactory::from_settings(Ok(None));
        assert_eq!(
            memory_fill_fuel(&factory, 0),
            memory_fill_fuel(&factory, 4096)
        );
    }

    #[test]
    fn size_dependent_segment_can_exceed_the_runtime_yield_reserve() {
        let mut config = Config::new();
        config.consume_fuel(true);
        let engine = Engine::new(&config).expect("default-cost engine");
        let fill_length = crate::sched::YIELD_RESERVE + 1;
        let memory_pages = fill_length.div_ceil(65_536);
        let module = wasmtime::Module::new(
            &engine,
            format!(
                r#"
                (module
                    (memory {memory_pages})
                    (func (export "fill")
                        i32.const 0
                        i32.const 0
                        i32.const {fill_length}
                        memory.fill))
            "#
            ),
        )
        .expect("memory.fill module");
        let mut store = wasmtime::Store::new(&engine, ());
        store
            .set_fuel(crate::sched::YIELD_RESERVE)
            .expect("runtime reserve fuel");
        let instance = wasmtime::Instance::new(&mut store, &module, &[]).expect("instance");
        let error = instance
            .get_typed_func::<(), ()>(&mut store, "fill")
            .expect("fill export")
            .call(&mut store, ())
            .expect_err("size-dependent segment must exceed the runtime reserve");
        assert!(
            format!("{error:#}").contains("all fuel consumed by WebAssembly"),
            "unexpected segment failure: {error:#}"
        );
    }

    #[test]
    fn production_engine_accepts_component_model_async() {
        let factory = EngineFactory::from_settings(Ok(None));
        let engine = factory.engine().expect("engine");
        Component::new(&engine, "(component (type (func async)))")
            .expect("production engine must accept component-model async");
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

    #[test]
    fn runtime_engine_clones_share_the_same_wasmtime_engine() {
        let (runtime_engine, _publisher) = runtime_engine().expect("runtime engine");
        let cloned = runtime_engine.clone();

        assert!(Arc::ptr_eq(&runtime_engine.engine(), &cloned.engine()));
    }

    #[test]
    fn publisher_tick_advances_the_shared_epoch_sequence() {
        let (runtime_engine, mut publisher) = runtime_engine().expect("runtime engine");

        assert_eq!(runtime_engine.current_epoch(), 0);
        assert_eq!(publisher.tick(), 1);
        assert_eq!(runtime_engine.current_epoch(), 1);
        assert_eq!(publisher.tick(), 2);
        assert_eq!(runtime_engine.current_epoch(), 2);
    }

    #[test]
    fn callback_epoch_observation_is_serialized_with_tick() {
        let (runtime_engine, publisher) = runtime_engine().expect("runtime engine");
        let subscription = runtime_engine.subscribe();
        let sequence = Arc::clone(&publisher.epoch_sequence);
        let sequence_guard = sequence.lock().expect("epoch sequence lock");
        let (started_tx, started_rx) = std::sync::mpsc::channel();
        let (observed_tx, observed_rx) = std::sync::mpsc::channel();
        let reader = std::thread::spawn(move || {
            started_tx.send(()).expect("announce epoch reader");
            observed_tx
                .send(subscription.engine_epoch())
                .expect("publish callback epoch");
        });

        started_rx.recv().expect("epoch reader started");
        assert!(
            observed_rx
                .recv_timeout(std::time::Duration::from_millis(20))
                .is_err(),
            "callback epoch observation escaped the tick critical section"
        );
        drop(sequence_guard);
        assert_eq!(observed_rx.recv().expect("callback epoch"), 0);
        reader.join().expect("epoch reader thread");
    }

    #[tokio::test]
    async fn new_subscription_marks_the_current_epoch_as_its_baseline() {
        let (runtime_engine, mut publisher) = runtime_engine().expect("runtime engine");
        assert_eq!(publisher.tick(), 1);

        let mut subscription = runtime_engine.subscribe();
        assert_eq!(subscription.current_epoch(), 1);
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(20), subscription.changed())
                .await
                .is_err(),
            "a fresh subscription must not observe ticks from before subscription"
        );

        assert_eq!(publisher.tick(), 2);
        assert_eq!(subscription.changed().await.expect("second epoch"), 2);
    }

    #[tokio::test]
    async fn tick_before_changed_is_not_lost() {
        let (runtime_engine, mut publisher) = runtime_engine().expect("runtime engine");
        let mut subscription = runtime_engine.subscribe();

        assert_eq!(publisher.tick(), 1);
        assert_eq!(subscription.changed().await.expect("published epoch"), 1);
    }

    #[tokio::test]
    async fn dropping_the_publisher_closes_subscriptions() {
        let (runtime_engine, publisher) = runtime_engine().expect("runtime engine");
        let mut subscription = runtime_engine.subscribe();

        drop(publisher);

        assert!(subscription.changed().await.is_err());
    }

    #[test]
    fn published_tick_also_advances_the_paired_wasmtime_engine() {
        use std::sync::atomic::AtomicBool;

        let (runtime_engine, mut publisher) = runtime_engine().expect("runtime engine");
        let engine = runtime_engine.engine();
        let module = wasmtime::Module::new(&engine, "(module (func (export \"run\")))")
            .expect("epoch probe module");
        let mut store = wasmtime::Store::new(&engine, ());
        store.set_fuel(u64::MAX).expect("probe fuel");
        let epoch_observed = Arc::new(AtomicBool::new(false));
        let callback_observed = Arc::clone(&epoch_observed);
        store.epoch_deadline_callback(move |_context| {
            callback_observed.store(true, Ordering::SeqCst);
            Ok(wasmtime::UpdateDeadline::Continue(1))
        });
        store.set_epoch_deadline(1);
        let instance = wasmtime::Instance::new(&mut store, &module, &[]).expect("probe instance");
        let run = instance
            .get_typed_func::<(), ()>(&mut store, "run")
            .expect("probe export");

        assert_eq!(publisher.tick(), 1);
        run.call(&mut store, ()).expect("run epoch probe");

        assert!(epoch_observed.load(Ordering::SeqCst));
    }
}
