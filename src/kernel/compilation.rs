//! PID0 compilation admission. Dropping a waiter never frees a native worker.

use std::sync::Arc;

use anyhow::{Context, Result};
use cell::engine::RuntimeEngine;
use cell::proc::ValidatedKernel;
use tokio::sync::Semaphore;

/// One physical compilation slot for one Deployment's successive generations.
///
/// Deployment serializes generations and cancels the old generation's waiter
/// before starting its replacement. Thus only its latest generation can wait
/// here. There is no job queue or completed-component cache in this owner.
#[derive(Clone)]
pub struct Compilation {
    admission: Arc<Semaphore>,
}

impl Default for Compilation {
    fn default() -> Self {
        Self {
            admission: Arc::new(Semaphore::new(1)),
        }
    }
}

impl Compilation {
    /// Compile and statically validate with the exact production kernel linker.
    ///
    /// Canceling this future cancels admission or result waiting. Wasmtime's
    /// synchronous compiler continues after admission until it returns. The
    /// native closure retains admission throughout that interval, and Tokio
    /// drops an unobserved result rather than caching it in CompilationService.
    pub async fn prepare(
        &self,
        runtime: RuntimeEngine,
        bytes: Vec<u8>,
    ) -> Result<Arc<ValidatedKernel>> {
        Ok(self
            .run(move || ValidatedKernel::compile(&runtime, &bytes).map(Arc::new))
            .await??)
    }

    pub(super) async fn run<T: Send + 'static>(
        &self,
        compile: impl FnOnce() -> T + Send + 'static,
    ) -> Result<T> {
        let permit = self
            .admission
            .clone()
            .acquire_owned()
            .await
            .context("PID0 compilation admission closed")?;
        tokio::task::spawn_blocking(move || {
            let _permit = permit;
            compile()
        })
        .await
        .context("PID0 native compilation worker failed")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, Ordering};
    use tokio::sync::oneshot;

    #[derive(Debug)]
    struct Output(Option<oneshot::Sender<()>>);

    impl Drop for Output {
        fn drop(&mut self) {
            let _ = self.0.take().unwrap().send(());
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn dropped_waiter_retains_physical_admission_and_discards_late_result() {
        let compiler = Compilation::default();
        let first = compiler.clone();
        let (entered_tx, entered_rx) = oneshot::channel();
        let (finish_tx, finish_rx) = std::sync::mpsc::channel();
        let (dropped_tx, dropped_rx) = oneshot::channel();
        let waiter = tokio::spawn(async move {
            first
                .run(move || {
                    entered_tx.send(()).unwrap();
                    finish_rx.recv().unwrap();
                    Output(Some(dropped_tx))
                })
                .await
        });
        entered_rx.await.unwrap();
        waiter.abort();
        assert!(waiter.await.unwrap_err().is_cancelled());
        assert_eq!(compiler.admission.available_permits(), 0);

        // Poll a replacement into admission, then cancel before native work.
        let stale_ran = Arc::new(AtomicBool::new(false));
        let stale_mark = stale_ran.clone();
        let mut stale = Box::pin(compiler.run(move || stale_mark.store(true, Ordering::SeqCst)));
        assert!(matches!(
            futures::poll!(&mut stale),
            std::task::Poll::Pending
        ));
        drop(stale);
        assert_eq!(compiler.admission.available_permits(), 0);

        let latest_ran = Arc::new(AtomicBool::new(false));
        let latest_mark = latest_ran.clone();
        let mut latest = Box::pin(compiler.run(move || latest_mark.store(true, Ordering::SeqCst)));
        assert!(matches!(
            futures::poll!(&mut latest),
            std::task::Poll::Pending
        ));
        assert!(!latest_ran.load(Ordering::SeqCst));
        finish_tx.send(()).unwrap();
        latest.await.unwrap();
        dropped_rx.await.unwrap();
        assert!(!stale_ran.load(Ordering::SeqCst));
        assert!(latest_ran.load(Ordering::SeqCst));
        assert_eq!(compiler.admission.available_permits(), 1);
    }

    #[tokio::test]
    async fn native_panic_releases_admission_after_worker_unwinds() {
        let compiler = Compilation::default();
        let error = compiler
            .run(|| panic!("compiler probe panic"))
            .await
            .unwrap_err();
        assert!(error
            .to_string()
            .contains("native compilation worker failed"));
        assert_eq!(compiler.admission.available_permits(), 1);
        assert_eq!(compiler.run(|| 7).await.unwrap(), 7);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    #[ignore = "measurement requires WW_KERNEL_VALIDATION_FIXTURE packaged PID0"]
    async fn measure_native_worker_lifetime_after_waiter_cancellation() {
        let path = std::env::var("WW_KERNEL_VALIDATION_FIXTURE").unwrap();
        let bytes = std::fs::read(path).unwrap();
        let (runtime, _clock) = cell::engine::runtime_engine().unwrap();
        let compiler = Compilation::default();
        let worker = compiler.clone();
        let (entered_tx, entered_rx) = oneshot::channel();
        let (compiled_tx, compiled_rx) = oneshot::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let finished = Arc::new(AtomicBool::new(false));
        let worker_finished = finished.clone();
        let waiter = tokio::spawn(async move {
            worker
                .run(move || {
                    let started = std::time::Instant::now();
                    entered_tx.send(()).unwrap();
                    let result = ValidatedKernel::compile(&runtime, &bytes);
                    worker_finished.store(true, Ordering::SeqCst);
                    compiled_tx
                        .send((started.elapsed(), result.is_ok()))
                        .unwrap();
                    // This barrier makes admission assertions independent of CPU
                    // scheduling, including a compile that finishes before abort.
                    release_rx.recv().unwrap();
                    result
                })
                .await
        });
        entered_rx.await.unwrap();
        let finished_at_cancel = finished.load(Ordering::SeqCst);
        let cancelled_at = std::time::Instant::now();
        waiter.abort();
        assert!(matches!(waiter.await, Err(error) if error.is_cancelled()));
        let cancellation_latency = cancelled_at.elapsed();
        assert_eq!(compiler.admission.available_permits(), 0);
        let mut replacement = Box::pin(compiler.run(|| ()));
        assert!(matches!(
            futures::poll!(&mut replacement),
            std::task::Poll::Pending
        ));
        let (native_time, valid) = compiled_rx.await.unwrap();
        let post_cancellation = cancelled_at.elapsed();
        assert!(valid);
        release_tx.send(()).unwrap();
        replacement.await.unwrap();
        eprintln!(
            "native_compile_ms={} logical_cancel_us={} native_remaining_ms={} finished_at_cancel={finished_at_cancel}",
            native_time.as_millis(), cancellation_latency.as_micros(), post_cancellation.as_millis()
        );
        assert_eq!(compiler.admission.available_permits(), 1);
    }
}
