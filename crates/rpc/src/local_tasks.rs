//! Structured shutdown for existing tasks on one dedicated executor thread.

use std::{cell::RefCell, future::Future, rc::Rc};
use tokio_util::{sync::CancellationToken, task::TaskTracker};

struct State {
    shutdown: CancellationToken,
    tasks: TaskTracker,
}

thread_local! {
    // The executor thread owns this scope. It stores only cancellation and a
    // task count: no Process capabilities, Stores, resource IDs, or task list.
    static CURRENT: RefCell<Option<Rc<State>>> = const { RefCell::new(None) };
}

/// Shutdown boundary for a dedicated thread's LocalSet. Enter before polling
/// it; stop admission, request shutdown and await `wait` before destroying it.
/// The Rc makes the guard thread-bound, including across its asynchronous wait.
pub struct LocalTaskScope {
    state: Rc<State>,
}

impl LocalTaskScope {
    pub fn enter() -> Self {
        let state = Rc::new(State {
            shutdown: CancellationToken::new(),
            tasks: TaskTracker::new(),
        });
        CURRENT.with(|current| {
            let mut current = current.borrow_mut();
            assert!(current.is_none(), "one task scope per executor thread");
            *current = Some(state.clone());
        });
        Self { state }
    }

    pub fn shutdown(&self) {
        self.state.shutdown.cancel();
        self.state.tasks.close();
    }

    pub async fn wait(&self) {
        self.state.tasks.wait().await;
    }
}

impl Drop for LocalTaskScope {
    fn drop(&mut self) {
        self.shutdown();
        CURRENT.with(|current| current.borrow_mut().take());
    }
}

/// A task's cancellation authority is local to that task. Only executor
/// shutdown propagates from the scope to every sibling.
pub fn shutdown_token() -> CancellationToken {
    CURRENT.with(|current| {
        current
            .borrow()
            .as_ref()
            .map(|state| state.shutdown.child_token())
            .unwrap_or_default()
    })
}

/// Register before spawning, so shutdown also joins tasks not yet polled.
/// Outside an executor scope, the task retains its normal standalone owner.
pub fn track<F: Future>(future: F) -> impl Future<Output = F::Output> {
    let token = CURRENT.with(|current| current.borrow().as_ref().map(|state| state.tasks.token()));
    async move {
        let _token = token;
        future.await
    }
}
