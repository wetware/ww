//! Single-threaded oneshot channel for effect resume.
//!
//! Used by the effect system: `perform` awaits the receiver while the handler
//! decides whether to resume or abort. OneshotSender is NOT Clone — one-shot
//! is enforced by move semantics. Sender's Drop impl signals abandonment so
//! the receiver can detect abort (handler didn't call resume).
//!
//! Zero external dependencies — uses only `core::task` and `std::rc`/`std::cell`.

use crate::Val;
use std::cell::{Cell, RefCell};
use std::future::Future;
use std::pin::Pin;
use std::rc::Rc;
use std::task::{Context, Poll, Waker};

/// Shared state backing a oneshot channel.
struct Slot {
    value: RefCell<Option<Val>>,
    waker: RefCell<Option<Waker>>,
    /// `true` while the sender exists. Set to `false` on send or drop.
    alive: Cell<bool>,
}

/// Create a new oneshot channel.
pub fn channel() -> (Sender, Receiver) {
    let slot = Rc::new(Slot {
        value: RefCell::new(None),
        waker: RefCell::new(None),
        alive: Cell::new(true),
    });
    (
        Sender {
            slot: Some(slot.clone()),
        },
        Receiver { slot },
    )
}

/// Sending half. NOT Clone — ownership is the one-shot enforcement.
pub struct Sender {
    /// `Option` so we can take it in `send()` to prevent Drop from firing.
    slot: Option<Rc<Slot>>,
}

impl Sender {
    /// Send a value and wake the receiver. Consumes the sender.
    pub fn send(mut self, val: Val) {
        if let Some(slot) = self.slot.take() {
            *slot.value.borrow_mut() = Some(val);
            slot.alive.set(false);
            if let Some(w) = slot.waker.borrow_mut().take() {
                w.wake();
            }
        }
    }
}

impl Drop for Sender {
    fn drop(&mut self) {
        // If slot is still Some, sender was dropped without calling send() (abort).
        // Signal the receiver to wake and detect abandonment.
        if let Some(slot) = self.slot.take() {
            if slot.alive.get() {
                slot.alive.set(false);
                if let Some(w) = slot.waker.borrow_mut().take() {
                    w.wake();
                }
            }
        }
    }
}

/// Receiving half. Awaits the resume value or detects abandonment.
pub struct Receiver {
    slot: Rc<Slot>,
}

impl Future for Receiver {
    type Output = Result<Val, Val>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context) -> Poll<Self::Output> {
        // Check if value was sent.
        if let Some(v) = self.slot.value.borrow_mut().take() {
            return Poll::Ready(Ok(v));
        }
        // Check if sender was dropped without sending (abort). Surface a
        // structured `:glia.error/continuation-abandoned` carrier so callers
        // can route on error type rather than parsing prose.
        if !self.slot.alive.get() {
            return Poll::Ready(Err(crate::error::continuation_abandoned()));
        }
        // Still alive, no value yet — register waker and wait.
        *self.slot.waker.borrow_mut() = Some(cx.waker().clone());
        Poll::Pending
    }
}

// =========================================================================
// Tests
// =========================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    };
    use std::task::Wake;

    fn poll_rx(rx: &mut Receiver) -> Poll<Result<Val, Val>> {
        let mut cx = Context::from_waker(Waker::noop());
        Pin::new(rx).poll(&mut cx)
    }

    #[test]
    fn send_then_poll() {
        let (tx, mut rx) = channel();
        tx.send(Val::Int(42));
        assert!(matches!(poll_rx(&mut rx), Poll::Ready(Ok(Val::Int(42)))));
    }

    #[test]
    fn poll_before_send() {
        let (tx, mut rx) = channel();
        assert!(matches!(poll_rx(&mut rx), Poll::Pending));
        tx.send(Val::Int(99));
        assert!(matches!(poll_rx(&mut rx), Poll::Ready(Ok(Val::Int(99)))));
    }

    #[test]
    fn sender_drop_abandonment() {
        let (tx, mut rx) = channel();
        drop(tx);
        assert!(matches!(poll_rx(&mut rx), Poll::Ready(Err(_))));
    }

    #[test]
    fn sender_drop_yields_structured_abandonment_error() {
        // Abandonment surfaces a structured :glia.error/continuation-abandoned
        // carrier, not a plain string — callers route on type, not prose.
        let (tx, mut rx) = channel();
        drop(tx);
        match poll_rx(&mut rx) {
            Poll::Ready(Err(err)) => {
                assert_eq!(
                    crate::error::type_tag(&err),
                    Some(crate::error::tag::CONTINUATION_ABANDONED)
                );
            }
            other => panic!("expected structured abandonment error, got {other:?}"),
        }
    }

    #[test]
    fn drop_after_send_no_double_signal() {
        let (tx, mut rx) = channel();
        tx.send(Val::Int(7));
        assert!(matches!(poll_rx(&mut rx), Poll::Ready(Ok(Val::Int(7)))));
    }

    #[test]
    fn waker_is_stored_and_called_on_send() {
        let (tx, mut rx) = channel();
        let woken = Arc::new(AtomicBool::new(false));
        struct TrackWake(Arc<AtomicBool>);
        impl Wake for TrackWake {
            fn wake(self: Arc<Self>) {
                self.0.store(true, Ordering::SeqCst);
            }
        }
        let waker = Waker::from(Arc::new(TrackWake(woken.clone())));
        let mut cx = Context::from_waker(&waker);

        assert!(matches!(Pin::new(&mut rx).poll(&mut cx), Poll::Pending));
        assert!(!woken.load(Ordering::SeqCst));

        tx.send(Val::Int(1));
        assert!(woken.load(Ordering::SeqCst));
    }

    #[test]
    fn waker_called_on_abandonment() {
        let (tx, mut rx) = channel();
        let woken = Arc::new(AtomicBool::new(false));
        struct TrackWake(Arc<AtomicBool>);
        impl Wake for TrackWake {
            fn wake(self: Arc<Self>) {
                self.0.store(true, Ordering::SeqCst);
            }
        }
        let waker = Waker::from(Arc::new(TrackWake(woken.clone())));
        let mut cx = Context::from_waker(&waker);

        assert!(matches!(Pin::new(&mut rx).poll(&mut cx), Poll::Pending));
        drop(tx);
        assert!(woken.load(Ordering::SeqCst));
    }

    #[test]
    fn send_various_val_types() {
        let (tx, mut rx) = channel();
        tx.send(Val::Nil);
        assert!(matches!(poll_rx(&mut rx), Poll::Ready(Ok(Val::Nil))));

        let (tx, mut rx) = channel();
        tx.send(Val::Str("hello".into()));
        assert!(matches!(poll_rx(&mut rx), Poll::Ready(Ok(Val::Str(s))) if s == "hello"));
    }
}
