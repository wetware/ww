//! Cancel a real child export through the generated import's subtask-cancel.

use std::future::Future;
use std::task::Poll;

wit_bindgen::generate!({ path: "../wit", world: "parent-world", generate_all });

use wetware::session_cancel::{
    child,
    observer::{event, wait_event},
};

struct Parent;
export!(Parent);

impl Guest for Parent {
    async fn run(mode: u32, cancel: bool) -> u32 {
        let (release, gate) = wit_future::new(|| unreachable!());
        let mut victim = Box::pin(child::victim(mode, gate));
        std::future::poll_fn(|cx| {
            assert!(victim.as_mut().poll(cx).is_pending());
            Poll::Ready(())
        })
        .await;
        wait_event(12).await;
        wait_event(30).await;
        event(40);
        if cancel {
            drop(victim);
            // No driver/task is polled by this inspection: retained work must
            // already be terminal when generated import cancellation returns.
            assert_eq!(child::certify(mode), 5);
            event(41);
            assert!(release.write(()).await.is_err());
            event(60);
        } else {
            release.write(()).await.unwrap();
            assert_eq!(victim.await, 7);
            assert_eq!(child::certify(mode), 5);
            event(41);
        }
        event(50);
        42
    }

    async fn sibling(gate: wit_bindgen::FutureReader<()>) -> u32 {
        assert_eq!(child::sibling(gate).await, 99);
        event(32);
        99
    }

    async fn reenter() -> u32 {
        child::reenter().await
    }
}
