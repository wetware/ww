use std::cell::RefCell;
use std::fmt;
use std::future::Future;
use std::io;
use std::panic::AssertUnwindSafe;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

#[cfg(feature = "test-support")]
use std::path::PathBuf;

use futures::FutureExt;
use tokio::sync::{oneshot, watch};
use tokio_util::task::TaskTracker;

/// The filesystem phase represented by one owned operation.
///
/// This is deliberately descriptive rather than an admission policy. Issue
/// #713 can account by kind without changing the completion lifetime created
/// here.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FilesystemOperation {
    ListingRead,
    ListingWrite,
    DescriptorOpen,
    DirectoryBuild,
    FileStage,
    ProcessSetup,
    ProcessCleanup,
}

#[cfg(feature = "test-support")]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FsTestStage {
    Started,
    DirectoryEntry,
    BeforePublish,
    Published,
    CleanupStarted,
    Completed,
}

#[cfg(feature = "test-support")]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FsTestEvent {
    pub operation: FilesystemOperation,
    pub stage: FsTestStage,
    pub path: Option<PathBuf>,
}

#[cfg(feature = "test-support")]
type FsTestHook = Arc<dyn Fn(FsTestEvent) + Send + Sync>;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FsWorkError(Arc<str>);

impl FsWorkError {
    fn new(message: impl Into<String>) -> Self {
        Self(Arc::from(message.into()))
    }
}

impl fmt::Display for FsWorkError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl std::error::Error for FsWorkError {}

type Cleanup = Box<dyn FnOnce() -> io::Result<()> + Send + 'static>;

#[derive(Clone)]
enum CompletionState {
    Running,
    Finished(Result<(), FsWorkError>),
}

struct State {
    id: u64,
    lifecycle: Mutex<Lifecycle>,
    tasks: TaskTracker,
    first_failure: Mutex<Option<FsWorkError>>,
    completion_tx: watch::Sender<CompletionState>,
    finished: AtomicBool,
    #[cfg(feature = "test-support")]
    test_hook: Mutex<Option<FsTestHook>>,
}

static NEXT_SCOPE_ID: AtomicU64 = AtomicU64::new(1);
tokio::task_local! {
    static ACTIVE_SCOPE_ID: u64;
}
std::thread_local! {
    static BLOCKING_SCOPE_IDS: RefCell<Vec<u64>> = const { RefCell::new(Vec::new()) };
}

struct BlockingScopeGuard;

impl BlockingScopeGuard {
    fn enter(id: u64) -> Self {
        BLOCKING_SCOPE_IDS.with(|ids| ids.borrow_mut().push(id));
        Self
    }
}

impl Drop for BlockingScopeGuard {
    fn drop(&mut self) {
        BLOCKING_SCOPE_IDS.with(|ids| {
            ids.borrow_mut().pop().expect("blocking scope stack empty");
        });
    }
}

struct Lifecycle {
    accepting: bool,
    cleanup_started: bool,
    cleanups: Vec<(FilesystemOperation, Cleanup)>,
}

impl State {
    #[cfg(feature = "test-support")]
    fn test_event(&self, event: FsTestEvent) {
        let hook = self
            .test_hook
            .lock()
            .expect("filesystem test hook poisoned")
            .clone();
        if let Some(hook) = hook {
            if std::panic::catch_unwind(AssertUnwindSafe(|| hook(event))).is_err() {
                self.fail(FsWorkError::new("filesystem test hook panicked"));
            }
        }
    }

    fn can_submit(&self, lifecycle: &Lifecycle) -> bool {
        lifecycle.accepting
            || ACTIVE_SCOPE_ID
                .try_with(|active| *active == self.id)
                .unwrap_or(false)
            || BLOCKING_SCOPE_IDS.with(|ids| ids.borrow().last() == Some(&self.id))
    }

    fn close(&self) {
        let mut lifecycle = self
            .lifecycle
            .lock()
            .expect("filesystem lifecycle poisoned");
        if lifecycle.accepting {
            lifecycle.accepting = false;
            self.tasks.close();
        }
    }

    fn fail(&self, error: FsWorkError) {
        let mut first = self
            .first_failure
            .lock()
            .expect("filesystem failure state poisoned");
        if first.is_none() {
            *first = Some(error);
        }
    }

    fn finish(&self, result: Result<(), FsWorkError>) {
        if !self.finished.swap(true, Ordering::AcqRel) {
            let _ = self.completion_tx.send(CompletionState::Finished(result));
        }
    }
}

/// Cloneable submission handle for one process or host filesystem lifetime.
#[derive(Clone)]
pub struct FsScope {
    state: Arc<State>,
}

/// Unique foreground lifetime. Dropping it closes new operation admission.
pub struct FsOwner {
    state: Arc<State>,
}

/// The single finalizer which drains operations and runs private cleanup.
pub struct FsFinalizer {
    state: Option<Arc<State>>,
}

/// Repeatable proof that all owned work and private cleanup completed.
#[derive(Clone)]
pub struct FsCompletion {
    receiver: watch::Receiver<CompletionState>,
}

impl FsScope {
    pub fn new() -> (Self, FsOwner, FsFinalizer, FsCompletion) {
        let (completion_tx, receiver) = watch::channel(CompletionState::Running);
        let state = Arc::new(State {
            id: NEXT_SCOPE_ID.fetch_add(1, Ordering::Relaxed),
            lifecycle: Mutex::new(Lifecycle {
                accepting: true,
                cleanup_started: false,
                cleanups: Vec::new(),
            }),
            tasks: TaskTracker::new(),
            first_failure: Mutex::new(None),
            completion_tx,
            finished: AtomicBool::new(false),
            #[cfg(feature = "test-support")]
            test_hook: Mutex::new(None),
        });
        (
            Self {
                state: Arc::clone(&state),
            },
            FsOwner {
                state: Arc::clone(&state),
            },
            FsFinalizer {
                state: Some(Arc::clone(&state)),
            },
            FsCompletion { receiver },
        )
    }

    /// Run an entire async filesystem operation independently of its waiter.
    /// The returned outer error describes ownership machinery; the inner result
    /// is the operation's ordinary result and does not poison finalization.
    pub async fn run_operation<T, E, F>(
        &self,
        kind: FilesystemOperation,
        operation: F,
    ) -> Result<Result<T, E>, FsWorkError>
    where
        T: Send + 'static,
        E: Send + 'static,
        F: Future<Output = Result<T, E>> + Send + 'static,
    {
        let state = Arc::clone(&self.state);
        let (result_tx, result_rx) = oneshot::channel();
        let (scope_id, task_token) = {
            let lifecycle = self
                .state
                .lifecycle
                .lock()
                .expect("filesystem lifecycle poisoned");
            if !self.state.can_submit(&lifecycle) {
                return Err(FsWorkError::new("filesystem scope is closed"));
            }
            (self.state.id, self.state.tasks.token())
        };
        tokio::spawn(async move {
            let _task_token = task_token;
            let operation = ACTIVE_SCOPE_ID.scope(scope_id, operation);
            match AssertUnwindSafe(operation).catch_unwind().await {
                Ok(result) => {
                    let _ = result_tx.send(result);
                }
                Err(_) => {
                    state.fail(FsWorkError::new(format!(
                        "filesystem operation {kind:?} panicked"
                    )));
                }
            }
        });
        result_rx.await.map_err(|_| {
            FsWorkError::new(format!(
                "filesystem operation {kind:?} ended without a result"
            ))
        })
    }

    /// Run one synchronous local filesystem phase on Tokio's blocking pool.
    pub async fn run_blocking<T, F>(
        &self,
        kind: FilesystemOperation,
        operation: F,
    ) -> Result<T, FsWorkError>
    where
        T: Send + 'static,
        F: FnOnce() -> T + Send + 'static,
    {
        let task_token = {
            let lifecycle = self
                .state
                .lifecycle
                .lock()
                .expect("filesystem lifecycle poisoned");
            if !self.state.can_submit(&lifecycle) {
                return Err(FsWorkError::new("filesystem scope is closed"));
            }
            self.state.tasks.token()
        };
        let state = Arc::clone(&self.state);
        let handle = tokio::task::spawn_blocking(move || {
            let _task_token = task_token;
            let _scope = BlockingScopeGuard::enter(state.id);
            #[cfg(feature = "test-support")]
            state.test_event(FsTestEvent {
                operation: kind,
                stage: FsTestStage::Started,
                path: None,
            });
            #[cfg(feature = "test-support")]
            let _completed = TestCompletionEvent {
                state: Arc::clone(&state),
                operation: kind,
            };
            std::panic::catch_unwind(AssertUnwindSafe(operation)).map_err(|_| {
                let error =
                    FsWorkError::new(format!("filesystem blocking operation {kind:?} panicked"));
                state.fail(error.clone());
                error
            })
        });
        handle.await.map_err(|error| {
            let error = FsWorkError::new(format!(
                "filesystem blocking operation {kind:?} failed: {error}"
            ));
            self.state.fail(error.clone());
            error
        })?
    }

    #[cfg(feature = "test-support")]
    pub fn set_test_hook(&self, hook: FsTestHook) {
        *self
            .state
            .test_hook
            .lock()
            .expect("filesystem test hook poisoned") = Some(hook);
    }

    #[cfg(feature = "test-support")]
    pub fn test_event(
        &self,
        operation: FilesystemOperation,
        stage: FsTestStage,
        path: Option<PathBuf>,
    ) {
        self.state.test_event(FsTestEvent {
            operation,
            stage,
            path,
        });
    }

    /// Transfer one private filesystem cleanup into this lifetime.
    ///
    /// An already-admitted operation may call this after foreground admission
    /// closes. The finalizer drains operation tokens before taking this list.
    pub fn own_cleanup<F>(&self, kind: FilesystemOperation, cleanup: F) -> Result<(), FsWorkError>
    where
        F: FnOnce() -> io::Result<()> + Send + 'static,
    {
        let mut lifecycle = self
            .state
            .lifecycle
            .lock()
            .expect("filesystem lifecycle poisoned");
        if !self.state.can_submit(&lifecycle) {
            return Err(FsWorkError::new("filesystem scope is closed"));
        }
        if lifecycle.cleanup_started {
            return Err(FsWorkError::new("filesystem cleanup has already started"));
        }
        lifecycle.cleanups.push((kind, Box::new(cleanup)));
        Ok(())
    }

    pub fn own_temp_dir(
        &self,
        kind: FilesystemOperation,
        directory: tempfile::TempDir,
    ) -> Result<(), FsWorkError> {
        #[cfg(feature = "test-support")]
        let path = directory.path().to_owned();
        #[cfg(feature = "test-support")]
        let state = Arc::clone(&self.state);
        self.own_cleanup(kind, move || {
            #[cfg(feature = "test-support")]
            state.test_event(FsTestEvent {
                operation: kind,
                stage: FsTestStage::CleanupStarted,
                path: Some(path.clone()),
            });
            let result = directory.close();
            #[cfg(feature = "test-support")]
            state.test_event(FsTestEvent {
                operation: kind,
                stage: FsTestStage::Completed,
                path: Some(path),
            });
            result
        })
    }

    /// Record a failure to remove private state from inside an owned operation.
    pub fn record_cleanup_failure(&self, error: impl fmt::Display) {
        self.state.fail(FsWorkError::new(format!(
            "filesystem cleanup failed: {error}"
        )));
    }
}

#[cfg(feature = "test-support")]
struct TestCompletionEvent {
    state: Arc<State>,
    operation: FilesystemOperation,
}

#[cfg(feature = "test-support")]
impl Drop for TestCompletionEvent {
    fn drop(&mut self) {
        self.state.test_event(FsTestEvent {
            operation: self.operation,
            stage: FsTestStage::Completed,
            path: None,
        });
    }
}

impl Drop for FsOwner {
    fn drop(&mut self) {
        self.state.close();
    }
}

struct FinalizerGuard {
    state: Option<Arc<State>>,
}

impl Drop for FinalizerGuard {
    fn drop(&mut self) {
        if let Some(state) = self.state.take() {
            continue_finalization_off_thread(state);
        }
    }
}

async fn finalize_state(state: Arc<State>) -> Result<(), FsWorkError> {
    state.tasks.wait().await;
    let cleanups = {
        let mut lifecycle = state
            .lifecycle
            .lock()
            .expect("filesystem lifecycle poisoned");
        lifecycle.cleanup_started = true;
        std::mem::take(&mut lifecycle.cleanups)
    };
    if !cleanups.is_empty() {
        let cleanup_state = Arc::clone(&state);
        let cleanup = state.tasks.spawn_blocking(move || {
            for (kind, cleanup) in cleanups {
                match std::panic::catch_unwind(AssertUnwindSafe(cleanup)) {
                    Ok(Ok(())) => {}
                    Ok(Err(error)) => {
                        cleanup_state.fail(FsWorkError::new(format!(
                            "filesystem cleanup {kind:?} failed: {error}"
                        )));
                    }
                    Err(_) => {
                        cleanup_state.fail(FsWorkError::new(format!(
                            "filesystem cleanup {kind:?} panicked"
                        )));
                    }
                }
            }
        });
        if let Err(error) = cleanup.await {
            state.fail(FsWorkError::new(format!(
                "filesystem finalizer panicked: {error}"
            )));
        }
    }

    let result = state
        .first_failure
        .lock()
        .expect("filesystem failure state poisoned")
        .clone()
        .map_or(Ok(()), Err);
    state.finish(result.clone());
    result
}

fn continue_finalization_off_thread(state: Arc<State>) {
    state.close();
    let empty = {
        let mut lifecycle = state
            .lifecycle
            .lock()
            .expect("filesystem lifecycle poisoned");
        if state.tasks.is_empty() && lifecycle.cleanups.is_empty() {
            lifecycle.cleanup_started = true;
            true
        } else {
            false
        }
    };
    if empty {
        let result = state
            .first_failure
            .lock()
            .expect("filesystem failure state poisoned")
            .clone()
            .map_or(Ok(()), Err);
        state.finish(result);
        return;
    }
    let thread_state = Arc::clone(&state);
    let spawn = std::thread::Builder::new()
        .name("ww-fs-finalizer".to_owned())
        .spawn(move || {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build();
            match runtime {
                Ok(runtime) => {
                    let _ = runtime.block_on(finalize_state(thread_state));
                }
                Err(error) => thread_state.finish(Err(FsWorkError::new(format!(
                    "failed to start filesystem finalizer runtime: {error}"
                )))),
            }
        });
    if let Err(error) = spawn {
        let cleanups = {
            let mut lifecycle = state
                .lifecycle
                .lock()
                .expect("filesystem lifecycle poisoned");
            lifecycle.cleanup_started = true;
            std::mem::take(&mut lifecycle.cleanups)
        };
        // Never recursively delete process-private trees on the owner thread.
        // Leaking is the fail-closed fallback when the OS refuses a finalizer
        // thread; completion reports the cleanup failure.
        std::mem::forget(cleanups);
        state.finish(Err(FsWorkError::new(format!(
            "failed to spawn filesystem finalizer thread: {error}"
        ))));
    }
}

impl FsFinalizer {
    pub async fn run(mut self) -> Result<(), FsWorkError> {
        let state = self
            .state
            .take()
            .expect("filesystem finalizer already taken");
        let mut guard = FinalizerGuard {
            state: Some(Arc::clone(&state)),
        };
        let result = finalize_state(Arc::clone(&state)).await;
        guard.state = None;
        result
    }
}

impl Drop for FsFinalizer {
    fn drop(&mut self) {
        if let Some(state) = self.state.take() {
            continue_finalization_off_thread(state);
        }
    }
}

impl FsCompletion {
    pub async fn wait(&self) -> Result<(), FsWorkError> {
        let mut receiver = self.receiver.clone();
        loop {
            match receiver.borrow().clone() {
                CompletionState::Running => {}
                CompletionState::Finished(result) => return result,
            }
            receiver.changed().await.map_err(|_| {
                FsWorkError::new("filesystem completion owner disappeared without a result")
            })?;
        }
    }
}
