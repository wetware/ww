use std::sync::Arc;

use tokio::sync::watch;

/// Owns the production shared Engine and its 10 ms epoch ticker.
pub struct TickedExecutor {
    pool: ww::services::ExecutorPool,
    _shutdown_tx: watch::Sender<()>,
}

impl TickedExecutor {
    pub fn new() -> Self {
        let (shutdown_tx, shutdown_rx) = watch::channel(());
        Self {
            pool: ww::services::ExecutorPool::new(1, shutdown_rx),
            _shutdown_tx: shutdown_tx,
        }
    }

    pub fn engine(&self) -> Arc<wasmtime::Engine> {
        self.pool.engine()
    }
}
