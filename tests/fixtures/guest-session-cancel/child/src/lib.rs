//! Actual std/system sessions, retained RPC work, and component task lifetimes.

use capnp::capability::{Promise, Response};
use capnp_rpc::RpcSystem;
use futures::FutureExt;
use std::future::{Future, IntoFuture};
use std::pin::Pin;
use std::task::{Context, Poll, Waker};
use system::system_capnp::{executor, membrane, process};

wit_bindgen::generate!({ path: "../wit", world: "child-world", generate_all });

use wetware::session_cancel::observer::event;

struct Child;
export!(Child);

type SpawnResponse =
    Pin<Box<dyn Future<Output = capnp::Result<Response<executor::spawn_results::Owned>>>>>;
type StdinResponse =
    Pin<Box<dyn Future<Output = capnp::Result<Response<process::stdin_results::Owned>>>>>;

struct Retained {
    ordinary: SpawnResponse,
    response: SpawnResponse,
    pipeline: executor::spawn_results::Pipeline,
    promised: process::Client,
    promised_call: StdinResponse,
    resolution: Pin<Box<dyn Future<Output = capnp::Result<()>>>>,
    import: executor::Client,
    waker: Option<Waker>,
}

// Component guest execution is serialized. These component globals survive
// task destruction; task-local TLS would give the inspecting export a different
// observation. No borrow of the retained box crosses an await or host callback.
static mut RETAINED: *mut Retained = std::ptr::null_mut();
static mut BOOTSTRAP_DROPS: u32 = 0;
static mut EXPORT_DROPS: u32 = 0;

fn retained() -> &'static mut Retained {
    unsafe { RETAINED.as_mut().expect("retained RPC work installed") }
}

struct Tracked(u32);

impl membrane::Server for Tracked {
    fn graft(
        self: capnp::capability::Rc<Self>,
        _: membrane::GraftParams,
        mut results: membrane::GraftResults,
    ) -> impl Future<Output = capnp::Result<()>> + 'static {
        event(18);
        results.get().set_peer_id(b"guest-bootstrap");
        Promise::ok(())
    }
}

impl Drop for Tracked {
    fn drop(&mut self) {
        unsafe {
            if self.0 == 20 {
                BOOTSTRAP_DROPS += 1;
            } else {
                EXPORT_DROPS += 1;
            }
        }
        event(self.0);
    }
}

struct CountPoll<F> {
    future: Option<Pin<Box<F>>>,
    poll: u32,
    destroyed: u32,
}

impl<F> CountPoll<F> {
    fn new(future: F, poll: u32, destroyed: u32) -> Self {
        Self {
            future: Some(Box::pin(future)),
            poll,
            destroyed,
        }
    }
}

impl<F: Future> Future for CountPoll<F> {
    type Output = F::Output;
    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        event(self.poll);
        self.future.as_mut().unwrap().as_mut().poll(cx)
    }
}

impl<F> Drop for CountPoll<F> {
    fn drop(&mut self) {
        drop(self.future.take());
        event(self.destroyed);
    }
}

fn bootstrap() -> capnp::capability::Client {
    let cap: membrane::Client = capnp_rpc::new_client(Tracked(20));
    cap.client
}

fn queue(remote: executor::Client) {
    let exported: membrane::Client = capnp_rpc::new_client(Tracked(21));
    let mut ordinary = remote.spawn_request();
    ordinary.get().set_membrane(exported.clone());
    let ordinary = ordinary.send();
    drop(ordinary.pipeline);

    let mut pipelined = remote.spawn_request();
    pipelined.get().set_membrane(exported);
    let pipelined = pipelined.send();
    let promised = pipelined.pipeline.get_process();
    let promised_call = promised.stdin_request().send();
    drop(promised_call.pipeline);
    let retained = Box::new(Retained {
        ordinary: Box::pin(ordinary.promise),
        response: Box::pin(pipelined.promise),
        pipeline: pipelined.pipeline,
        resolution: Box::pin(promised.client.when_resolved()),
        promised,
        promised_call: Box::pin(promised_call.promise),
        import: remote,
        waker: None,
    });
    unsafe {
        assert!(RETAINED.is_null(), "one victim per component instance");
        RETAINED = Box::into_raw(retained);
    }
}

fn register(cx: &mut Context<'_>) {
    let retained = retained();
    assert!(retained.ordinary.as_mut().poll(cx).is_pending());
    assert!(retained.response.as_mut().poll(cx).is_pending());
    assert!(retained.promised_call.as_mut().poll(cx).is_pending());
    assert!(retained.resolution.as_mut().poll(cx).is_pending());
    retained.waker = Some(cx.waker().clone());
}

async fn roundtrip(remote: &executor::Client) {
    let response = remote.cid_request().send().promise.await.unwrap();
    assert_eq!(response.get().unwrap().get_cid().unwrap(), "session-probe");
}

async fn wait_pending(gate: wit_bindgen::FutureReader<()>, register_work: bool) {
    let mut gate = Box::pin(gate.into_future());
    let mut announced = false;
    std::future::poll_fn(|cx| {
        if register_work {
            register(cx);
        }
        match gate.as_mut().poll(cx) {
            Poll::Pending => {
                if !announced {
                    announced = true;
                    event(12);
                }
                Poll::Pending
            }
            Poll::Ready(()) => Poll::Ready(()),
        }
    })
    .await;
}

async fn application(
    remote: executor::Client,
    gate: wit_bindgen::FutureReader<()>,
) -> capnp::Result<()> {
    roundtrip(&remote).await;
    queue(remote.clone());
    // Non-streaming call ordering makes this reply a dispatch barrier for both
    // preceding spawn calls. Their promises remain unresolved on the peer.
    roundtrip(&remote).await;
    wait_pending(gate, true).await;
    event(120);
    Ok(())
}

fn extracted(
    cx: &mut Context<'_>,
) -> (
    RpcSystem<capnp_rpc::rpc_twoparty_capnp::Side>,
    executor::Client,
) {
    let session = system::RpcSession::<executor::Client>::connect_with_export(Some(bootstrap()));
    queue(session.client.clone());
    register(cx);
    // The inaccessible remainder is destroyed here. Extracted public fields
    // must already be disconnected: partial moves cannot detach the session.
    (session.rpc_system, session.client)
}

impl exports::wetware::session_cancel::child::Guest for Child {
    async fn victim(mode: u32, gate: wit_bindgen::FutureReader<()>) -> u32 {
        unsafe {
            BOOTSTRAP_DROPS = 0;
            EXPORT_DROPS = 0;
        }
        CountPoll::new(
            async move {
                match mode {
                    0 => {
                        let session = system::RpcSession::<executor::Client>::connect_with_export(
                            Some(bootstrap()),
                        );
                        queue(session.client.clone());
                        wait_pending(gate, true).await;
                        drop(session);
                    }
                    1 => {
                        system::run(|remote| CountPoll::new(application(remote, gate), 101, 111))
                            .await
                            .unwrap();
                    }
                    2 => {
                        system::serve(bootstrap(), |remote| {
                            CountPoll::new(application(remote, gate), 101, 111)
                        })
                        .await
                        .unwrap();
                    }
                    3 => {
                        let fields = std::future::poll_fn(|cx| Poll::Ready(extracted(cx))).await;
                        event(13);
                        wait_pending(gate, false).await;
                        drop(fields);
                    }
                    _ => panic!("unknown session scenario"),
                }
                7
            },
            100,
            110,
        )
        .await
    }

    fn certify(mode: u32) -> u32 {
        let (export_drops, bootstrap_drops) = unsafe { (EXPORT_DROPS, BOOTSTRAP_DROPS) };
        assert_eq!(
            export_drops, 1,
            "queued/export-table owner released before inspection"
        );
        assert_eq!(
            bootstrap_drops,
            u32::from(mode != 1),
            "bootstrap owner released before inspection"
        );
        let retained = retained();
        let mut cx = Context::from_waker(Waker::noop());
        assert!(matches!(
            retained.ordinary.as_mut().poll(&mut cx),
            Poll::Ready(Err(_))
        ));
        event(22);
        assert!(matches!(
            retained.response.as_mut().poll(&mut cx),
            Poll::Ready(Err(_))
        ));
        event(23);
        assert!(retained
            .pipeline
            .get_process()
            .stdin_request()
            .send()
            .promise
            .now_or_never()
            .unwrap()
            .is_err());
        event(24);
        assert!(retained.resolution.as_mut().poll(&mut cx).is_ready());
        assert!(retained
            .promised
            .wait_request()
            .send()
            .promise
            .now_or_never()
            .unwrap()
            .is_err());
        assert!(matches!(
            retained.promised_call.as_mut().poll(&mut cx),
            Poll::Ready(Err(_))
        ));
        event(25);
        assert!(retained
            .import
            .cid_request()
            .send()
            .promise
            .now_or_never()
            .unwrap()
            .is_err());
        event(26);
        event(70);
        let waker = retained.waker.take().unwrap();
        waker.wake_by_ref();
        waker.wake_by_ref();
        waker.wake();
        event(71);
        unsafe {
            drop(Box::from_raw(RETAINED));
            RETAINED = std::ptr::null_mut();
        }
        5
    }

    async fn sibling(gate: wit_bindgen::FutureReader<()>) -> u32 {
        system::run(|remote: executor::Client| async move {
            roundtrip(&remote).await;
            event(30);
            gate.await;
            roundtrip(&remote).await;
            event(31);
            Ok(())
        })
        .await
        .unwrap();
        99
    }

    async fn reenter() -> u32 {
        system::run(|remote: executor::Client| async move {
            roundtrip(&remote).await;
            Ok(())
        })
        .await
        .unwrap();
        event(90);
        123
    }
}
