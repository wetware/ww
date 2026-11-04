//! Typed invocation provenance across queues, pipelines, membranes, and private RPC forwarding.

use ::authority::system_capnp::vat_client;
use capnp::private::capability::{InvocationOrigin, ParamsHook};
use capnp::traits::HasTypeId;
use capnp::{any_pointer, capability::Promise};
use capnp_rpc::{
    rpc_twoparty_capnp::Side, twoparty::VatNetwork, Connection, IncomingMessage, OutgoingMessage,
    VatNetwork as Network,
};
use rpc::managed_rpc::{ManagedRpc, ManagedRpcSystem};
use std::{cell::RefCell, rc::Rc};
use tokio_util::compat::{TokioAsyncReadCompatExt, TokioAsyncWriteCompatExt};

// Test-only active identity labels. This is not a parameter-address registry.
thread_local! {
    static OWNER_LABELS: RefCell<std::collections::HashMap<InvocationOrigin, u64>> = RefCell::new(std::collections::HashMap::new());
}
#[derive(Clone)]
struct Owner(InvocationOrigin);
impl Owner {
    fn new(label: u64) -> Self {
        let origin = InvocationOrigin::new();
        OWNER_LABELS.with(|labels| {
            let mut labels = labels.borrow_mut();
            assert!(labels.len() < 256, "bounded fixture owner capacity");
            labels.insert(origin.clone(), label);
        });
        Self(origin)
    }
    fn label(&self) -> u64 {
        OWNER_LABELS.with(|labels| {
            *labels
                .borrow()
                .get(&self.0)
                .expect("registered fixture owner")
        })
    }
}
fn owner_of(params: &dyn ParamsHook) -> capnp::Result<Owner> {
    let origin = params
        .invocation_origin()
        .ok_or_else(|| capnp::Error::failed("missing ownership".into()))?;
    if !OWNER_LABELS.with(|labels| labels.borrow().contains_key(&origin)) {
        return Err(capnp::Error::failed("unknown ownership".into()));
    }
    Ok(Owner(origin))
}
thread_local! { static RECEIVER_HOSTED: std::cell::Cell<usize> = const { std::cell::Cell::new(0) }; }
thread_local! { static PIPELINE_SEEN: RefCell<Option<tokio::sync::mpsc::UnboundedSender<()>>> = const { RefCell::new(None) }; }
fn observe_pipeline() -> tokio::sync::mpsc::UnboundedReceiver<()> {
    let (send, receive) = tokio::sync::mpsc::unbounded_channel();
    PIPELINE_SEEN.with(|slot| {
        slot.replace(Some(send));
    });
    receive
}
struct TaggedMessage {
    inner: Box<dyn IncomingMessage>,
}

impl IncomingMessage for TaggedMessage {
    fn get_body(&self) -> capnp::Result<any_pointer::Reader<'_>> {
        let body = self.inner.get_body()?;
        let message: capnp_rpc::rpc_capnp::message::Reader<'_> = body.get_as()?;
        if let capnp_rpc::rpc_capnp::message::Call(call) = message.which()? {
            let call = call?;
            if matches!(
                call.get_target()?.which()?,
                capnp_rpc::rpc_capnp::message_target::PromisedAnswer(_)
            ) {
                PIPELINE_SEEN.with(|slot| {
                    if let Some(send) = slot.borrow().as_ref() {
                        let _ = send.send(());
                    }
                });
            }
            for descriptor in call.get_params()?.get_cap_table()? {
                if matches!(
                    descriptor.which()?,
                    capnp_rpc::rpc_capnp::cap_descriptor::ReceiverHosted(_)
                ) {
                    RECEIVER_HOSTED.with(|count| count.set(count.get() + 1));
                }
            }
        }
        Ok(body)
    }
}

struct TaggedConnection {
    inner: Box<dyn Connection<Side>>,
    owner: Owner,
}

impl Connection<Side> for TaggedConnection {
    fn incoming_invocation_origin(&self) -> Option<InvocationOrigin> {
        Some(self.owner.0.clone())
    }
    fn get_peer_vat_id(&self) -> Side {
        self.inner.get_peer_vat_id()
    }

    fn new_outgoing_message(&mut self, words: u32) -> Box<dyn OutgoingMessage> {
        self.inner.new_outgoing_message(words)
    }

    fn receive_incoming_message(
        &mut self,
    ) -> Promise<Option<Box<dyn IncomingMessage>>, capnp::Error> {
        let incoming = self.inner.receive_incoming_message();
        Promise::from_future(async move {
            Ok(incoming
                .await?
                .map(|inner| Box::new(TaggedMessage { inner }) as Box<dyn IncomingMessage>))
        })
    }

    fn new_stream(
        &mut self,
    ) -> (
        Box<dyn capnp_rpc::FlowController>,
        Promise<(), capnp::Error>,
    ) {
        self.inner.new_stream()
    }

    fn shutdown(&mut self, result: capnp::Result<()>) -> Promise<(), capnp::Error> {
        self.inner.shutdown(result)
    }
}

struct TaggedNetwork {
    inner: Box<dyn Network<Side>>,
    owner: Owner,
}

impl Network<Side> for TaggedNetwork {
    fn connect(&mut self, vat: Side) -> Option<Box<dyn Connection<Side>>> {
        let owner = self.owner.clone();
        self.inner
            .connect(vat)
            .map(|inner| Box::new(TaggedConnection { inner, owner }) as Box<dyn Connection<Side>>)
    }

    fn accept(&mut self) -> Promise<Box<dyn Connection<Side>>, capnp::Error> {
        let accept = self.inner.accept();
        let owner = self.owner.clone();
        Promise::from_future(async move {
            Ok(Box::new(TaggedConnection {
                inner: accept.await?,
                owner,
            }) as Box<dyn Connection<Side>>)
        })
    }

    fn drive_until_shutdown(&mut self) -> Promise<(), capnp::Error> {
        self.inner.drive_until_shutdown()
    }
}

struct Leaf(Rc<RefCell<Vec<Vec<u64>>>>);

impl vat_client::Server for Leaf {
    async fn dial(
        self: capnp::capability::Rc<Self>,
        params: vat_client::DialParams,
        _: vat_client::DialResults,
    ) -> capnp::Result<()> {
        let (params, owner) = (params.get()?, owner_of(&*params.hook)?);
        assert_eq!(params.get_protocol()?.to_str()?, "provenance-probe");
        self.0.borrow_mut().push(vec![owner.label()]);
        Ok(())
    }
}

struct TypedForward(vat_client::Client);

impl vat_client::Server for TypedForward {
    async fn dial(
        self: capnp::capability::Rc<Self>,
        params: vat_client::DialParams,
        results: vat_client::DialResults,
    ) -> capnp::Result<()> {
        let (reader, owner) = (params.get()?, owner_of(&*params.hook)?);
        let mut request = self.0.dial_request();
        request.hook.set_invocation_origin(owner.0)?;
        request.get().set_peer(reader.get_peer()?);
        request.get().set_protocol(reader.get_protocol()?);
        let response = request.send().promise.await?;
        let mut results = results;
        results.set(response.get()?)?;
        Ok(())
    }
}

struct Allow;

impl membrane::Policy for Allow {
    fn check(&self, _: u64, _: u16) -> capnp::Result<()> {
        Ok(())
    }
}

fn pair(label: u64, bootstrap: vat_client::Client) -> (vat_client::Client, ManagedRpc, ManagedRpc) {
    pair_with_owner(Owner::new(label), bootstrap)
}
fn pair_with_owner(
    owner: Owner,
    bootstrap: vat_client::Client,
) -> (vat_client::Client, ManagedRpc, ManagedRpc) {
    let (host, guest) = tokio::io::duplex(8192);
    let (reader, writer) = tokio::io::split(host);
    let network = VatNetwork::new(
        reader.compat(),
        writer.compat_write(),
        Side::Server,
        Default::default(),
    );
    let host = ManagedRpc::spawn(ManagedRpcSystem::new(
        Box::new(TaggedNetwork {
            inner: Box::new(network),
            owner,
        }),
        Some(bootstrap.client),
    ));
    let rpc::vat_dial::VatDial { bootstrap, driver } = rpc::vat_dial::connect(guest.compat());
    (bootstrap, host, driver)
}

async fn invoke(client: &vat_client::Client) {
    let mut request = client.dial_request();
    request.get().set_protocol("provenance-probe");
    request.get().set_peer(b"test-only");
    request.send().promise.await.unwrap();
}

#[tokio::test(flavor = "current_thread")]
async fn typed_forwarding_preserves_owner() {
    tokio::time::timeout(
        std::time::Duration::from_secs(5),
        tokio::task::LocalSet::new().run_until(async {
            let observed = Rc::new(RefCell::new(Vec::new()));
            let leaf: vat_client::Client = capnp_rpc::new_client(Leaf(observed.clone()));
            let (a, mut host_a, mut guest_a) = pair(1, leaf.clone());
            let (b, mut host_b, mut guest_b) =
                pair(2, membrane::membrane(leaf.clone(), Rc::new(Allow)));
            let (proxy, mut host_proxy, mut guest_proxy) =
                pair(3, capnp_rpc::new_client(TypedForward(leaf)));

            invoke(&a).await;
            invoke(&b).await;
            invoke(&proxy).await;
            assert_eq!(
                *observed.borrow(),
                vec![vec![1], vec![2], vec![3]],
                "direct, membrane, and explicit typed forwarding retain ingress identity"
            );

            for driver in [&mut host_a, &mut host_b, &mut host_proxy] {
                assert!(driver.shutdown_and_join().await.is_ok());
            }
            for driver in [&mut guest_a, &mut guest_b, &mut guest_proxy] {
                assert!(driver.shutdown_and_join().await.is_ok());
            }
        }),
    )
    .await
    .expect("provenance probe watchdog");
}

struct UntaggedForward(vat_client::Client);
impl vat_client::Server for UntaggedForward {
    async fn dial(
        self: Rc<Self>,
        params: vat_client::DialParams,
        _: vat_client::DialResults,
    ) -> capnp::Result<()> {
        let params = params.get()?;
        let mut request = self.0.dial_request();
        request.get().set_peer(params.get_peer()?);
        request.get().set_protocol(params.get_protocol()?);
        request.send().promise.await?;
        Ok(())
    }
}

async fn checked_invoke(client: &vat_client::Client) -> capnp::Result<()> {
    let mut request = client.dial_request();
    request.get().set_protocol("provenance-probe");
    request.send().promise.await?;
    Ok(())
}

async fn close_pair(mut host: ManagedRpc, mut guest: ManagedRpc) {
    assert!(host.shutdown_and_join().await.is_ok());
    assert!(guest.shutdown_and_join().await.is_ok());
}

#[tokio::test(flavor = "current_thread")]
async fn nested_forwarding_and_untagged_forwarding_fail_closed() {
    tokio::time::timeout(
        std::time::Duration::from_secs(5),
        tokio::task::LocalSet::new().run_until(async {
            let observed = Rc::new(RefCell::new(Vec::new()));
            let leaf: vat_client::Client = capnp_rpc::new_client(Leaf(observed.clone()));
            assert!(checked_invoke(&leaf)
                .await
                .err()
                .unwrap()
                .extra
                .contains("missing"));
            let inner = capnp_rpc::new_client(TypedForward(leaf.clone()));
            let nested =
                capnp_rpc::new_client(TypedForward(membrane::membrane(inner, Rc::new(Allow))));
            let (client, host, guest) = pair(4, nested);
            invoke(&client).await;
            assert_eq!(*observed.borrow(), vec![vec![4]]);
            close_pair(host, guest).await;
            let (client, host, guest) = pair(5, capnp_rpc::new_client(UntaggedForward(leaf)));
            assert!(checked_invoke(&client)
                .await
                .err()
                .unwrap()
                .extra
                .contains("missing"));
            assert_eq!(
                *observed.borrow(),
                vec![vec![4]],
                "ownerless call must not enter leaf work"
            );
            close_pair(host, guest).await;
        }),
    )
    .await
    .unwrap();
}

struct ReturnCap {
    cap: vat_client::Client,
    entered: tokio::sync::mpsc::UnboundedSender<u64>,
    release: RefCell<Option<tokio::sync::oneshot::Receiver<()>>>,
}
impl vat_client::Server for ReturnCap {
    async fn dial(
        self: Rc<Self>,
        params: vat_client::DialParams,
        mut results: vat_client::DialResults,
    ) -> capnp::Result<()> {
        let (_, owner) = (params.get()?, owner_of(&*params.hook)?);
        self.entered.send(owner.label()).unwrap();
        let release = self.release.borrow_mut().take();
        if let Some(release) = release {
            release.await.unwrap();
        }
        results
            .get()
            .init_cap()
            .set_as_capability(self.cap.client.hook.add_ref());
        Ok(())
    }
}

#[tokio::test(flavor = "current_thread")]
async fn promised_target_and_response_pipeline_preserve_each_invocation() {
    tokio::time::timeout(
        std::time::Duration::from_secs(5),
        tokio::task::LocalSet::new().run_until(async {
            use capnp::capability::FromClientHook;
            let observed = Rc::new(RefCell::new(Vec::new()));
            let leaf: vat_client::Client = capnp_rpc::new_client(Leaf(observed.clone()));
            let (release_target, target) = tokio::sync::oneshot::channel();
            let future: vat_client::Client = capnp_rpc::new_future_client(async move {
                target
                    .await
                    .map_err(|_| capnp::Error::failed("target lost".into()))
            });
            let (a, ah, ag) = pair(10, capnp_rpc::new_client(TypedForward(future.clone())));
            let (b, bh, bg) = pair(11, capnp_rpc::new_client(TypedForward(future)));
            let a_call = checked_invoke(&a);
            let b_call = checked_invoke(&b);
            let release = async {
                release_target.send(leaf.clone()).ok().unwrap();
            };
            let (a_result, b_result, ()) = futures::join!(a_call, b_call, release);
            a_result.unwrap();
            b_result.unwrap();
            observed.borrow_mut().sort();
            assert_eq!(*observed.borrow(), vec![vec![10], vec![11]]);
            close_pair(ah, ag).await;
            close_pair(bh, bg).await;

            let (entered, mut events) = tokio::sync::mpsc::unbounded_channel();
            let (release, held) = tokio::sync::oneshot::channel();
            let return_cap = capnp_rpc::new_client(ReturnCap {
                cap: leaf,
                entered,
                release: RefCell::new(Some(held)),
            });
            let (a, ah, ag) = pair(12, capnp_rpc::new_client(TypedForward(return_cap)));
            let first = a.dial_request().send();
            assert_eq!(events.recv().await.unwrap(), 12);
            let mut pipeline_seen = observe_pipeline();
            let pipelined = vat_client::Client::new(first.pipeline.get_cap().as_cap());
            let mut request = pipelined.dial_request();
            request.get().set_protocol("provenance-probe");
            let second = request.send();
            pipeline_seen.recv().await.unwrap();
            PIPELINE_SEEN.with(|slot| {
                slot.take();
            });
            release.send(()).unwrap();
            second.promise.await.unwrap();
            first.promise.await.unwrap();
            assert_eq!(observed.borrow().last(), Some(&vec![12]));
            close_pair(ah, ag).await;
        }),
    )
    .await
    .unwrap();
}

mod scheduling {
    use super::{owner_of, Owner, TaggedNetwork};
    use ::authority::system_capnp;
    use capnp::{
        any_pointer,
        capability::{
            DispatchCallResult, FromClientHook, FromServer, Params, Promise, Results, Server,
        },
    };
    use capnp_rpc::{rpc_twoparty_capnp::Side, twoparty::VatNetwork, RpcSystem};
    use futures::{io::AsyncReadExt, task::AtomicWaker};
    use std::{
        cell::{Cell, RefCell},
        future::Future,
        ops::Deref,
        pin::Pin,
        rc::Rc,
        task::{Context, Poll},
        time::Duration,
    };
    use tokio::sync::{mpsc, oneshot};
    use tokio_util::compat::TokioAsyncReadCompatExt;

    thread_local! { static POLL_OWNER: Cell<&'static str> = const { Cell::new("none") }; }

    #[derive(Default)]
    struct PollGate {
        enabled: Cell<bool>,
        wake: AtomicWaker,
    }
    impl PollGate {
        fn enable(&self) {
            self.enabled.set(true);
            self.wake.wake();
        }
        fn disable(&self) {
            self.enabled.set(false);
        }
    }
    struct TaggedDriver<F> {
        owner: &'static str,
        gate: Rc<PollGate>,
        inner: Pin<Box<F>>,
    }
    impl<F: Future> Future for TaggedDriver<F> {
        type Output = F::Output;
        fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
            self.gate.wake.register(cx.waker());
            if !self.gate.enabled.get() {
                return Poll::Pending;
            }
            struct Restore(&'static str);
            impl Drop for Restore {
                fn drop(&mut self) {
                    POLL_OWNER.with(|owner| owner.set(self.0));
                }
            }
            let _restore = Restore(POLL_OWNER.with(|owner| owner.replace(self.owner)));
            self.inner.as_mut().poll(cx)
        }
    }

    struct Probe {
        events: mpsc::UnboundedSender<(u16, &'static str, u64)>,
        forward: Option<capnp::capability::Client>,
        first: RefCell<Option<oneshot::Receiver<()>>>,
        streaming: bool,
        streaming_error: bool,
    }
    #[derive(Clone)]
    struct ProbeDispatch(Rc<Probe>);
    impl Deref for ProbeDispatch {
        type Target = Probe;
        fn deref(&self) -> &Probe {
            &self.0
        }
    }
    impl Server for ProbeDispatch {
        fn dispatch_call(
            self,
            interface: u64,
            method: u16,
            params: Params<any_pointer::Owned>,
            results: Results<any_pointer::Owned>,
        ) -> DispatchCallResult {
            let owner = owner_of(&*params.hook).unwrap();
            if let Some(target) = &self.forward {
                let mut request = target.hook.new_call(interface, method, None);
                request.hook.set_invocation_origin(owner.0).unwrap();
                request.get().set_as(params.get().unwrap()).unwrap();
                let response = request.send();
                return DispatchCallResult::new(
                    Promise::from_future(async move {
                        let response = response.promise.await?;
                        let mut results = results;
                        results.get().set_as(response.get()?)?;
                        Ok(())
                    }),
                    false,
                );
            }
            self.events
                .send((method, POLL_OWNER.with(Cell::get), owner.label()))
                .unwrap();
            if method == 0 && self.streaming {
                let gate = self.first.borrow_mut().take().unwrap();
                let fail = self.streaming_error;
                DispatchCallResult::new(
                    Promise::from_future(async move {
                        gate.await.unwrap();
                        if fail {
                            return Err(capnp::Error::failed("streaming failure".into()));
                        }
                        Ok(())
                    }),
                    true,
                )
            } else {
                DispatchCallResult::new(Promise::ok(()), false)
            }
        }
        fn as_ptr(&self) -> usize {
            Rc::as_ptr(&self.0) as usize
        }
    }
    struct ProbeClient(capnp::capability::Client);
    impl FromClientHook for ProbeClient {
        fn new(hook: Box<dyn capnp::private::capability::ClientHook>) -> Self {
            Self(capnp::capability::Client::new(hook))
        }
        fn into_client_hook(self) -> Box<dyn capnp::private::capability::ClientHook> {
            self.0.hook
        }
        fn as_client_hook(&self) -> &dyn capnp::private::capability::ClientHook {
            self.0.hook.as_ref()
        }
    }
    impl FromServer<Probe> for ProbeClient {
        type Dispatch = ProbeDispatch;
        fn from_server(server: Rc<Probe>) -> ProbeDispatch {
            ProbeDispatch(server)
        }
    }

    struct Barrier;
    impl system_capnp::process::Server for Barrier {
        fn wait(
            self: Rc<Self>,
            _: system_capnp::process::WaitParams,
            _: system_capnp::process::WaitResults,
        ) -> impl Future<Output = capnp::Result<()>> + 'static {
            Promise::ok(())
        }
    }
    struct Root {
        shared: system_capnp::process::Client,
        barrier: system_capnp::process::Client,
    }
    impl system_capnp::membrane::Server for Root {
        fn graft(
            self: Rc<Self>,
            _: system_capnp::membrane::GraftParams,
            mut results: system_capnp::membrane::GraftResults,
        ) -> impl Future<Output = capnp::Result<()>> + 'static {
            let mut extras = results.get().init_extras(2);
            extras
                .reborrow()
                .get(0)
                .init_cap()
                .set_as_capability(self.shared.client.hook.add_ref());
            extras
                .reborrow()
                .get(1)
                .init_cap()
                .set_as_capability(self.barrier.client.hook.add_ref());
            Promise::ok(())
        }
    }
    struct Pair {
        shared: system_capnp::process::Client,
        barrier: system_capnp::process::Client,
        gate: Rc<PollGate>,
        disconnect: Pin<Box<dyn Future<Output = capnp::Result<()>>>>,
        host: tokio::task::JoinHandle<capnp::Result<()>>,
        guest: tokio::task::JoinHandle<capnp::Result<()>>,
    }
    async fn pair(owner: &'static str, shared: system_capnp::process::Client) -> Pair {
        let root: system_capnp::membrane::Client = capnp_rpc::new_client(Root {
            shared,
            barrier: capnp_rpc::new_client(Barrier),
        });
        let (a, b) = tokio::io::duplex(8192);
        let (ar, aw) = a.compat().split();
        let (br, bw) = b.compat().split();
        let host = membrane::rpc_system(
            Box::new(TaggedNetwork {
                inner: Box::new(VatNetwork::new(ar, aw, Side::Server, Default::default())),
                owner: Owner::new(if owner == "A" { 1 } else { 2 }),
            }),
            Some(root.client),
        );
        let disconnect = Box::pin(host.get_disconnector());
        let gate = Rc::new(PollGate::default());
        gate.enable();
        let host = tokio::task::spawn_local(TaggedDriver {
            owner,
            gate: gate.clone(),
            inner: Box::pin(host),
        });
        let mut guest = RpcSystem::new(
            Box::new(VatNetwork::new(br, bw, Side::Client, Default::default())),
            None,
        );
        let root: system_capnp::membrane::Client = guest.bootstrap(Side::Server);
        let guest = tokio::task::spawn_local(guest);
        let response = root.graft_request().send().promise.await.unwrap();
        let extras = response.get().unwrap().get_extras().unwrap();
        let shared = extras.get(0).get_cap().get_as().unwrap();
        let barrier = extras.get(1).get_cap().get_as().unwrap();
        Pair {
            shared,
            barrier,
            gate,
            disconnect,
            host,
            guest,
        }
    }
    async fn close(mut pair: Pair) {
        pair.gate.enable();
        let _ = pair.disconnect.as_mut().await;
        let _ = pair.host.await.unwrap();
        let _ = pair.guest.await.unwrap();
    }

    #[tokio::test(flavor = "current_thread")]
    async fn tagged_streaming_queue_preserves_b_under_a_poll() {
        tokio::time::timeout(
            Duration::from_secs(5),
            tokio::task::LocalSet::new().run_until(async {
                let (events_tx, mut events) = mpsc::unbounded_channel();
                let (release, held) = oneshot::channel();
                let probe: ProbeClient = capnp_rpc::new_client(Probe {
                    events: events_tx.clone(),
                    forward: None,
                    first: RefCell::new(Some(held)),
                    streaming: true,
                    streaming_error: false,
                });
                let forward: ProbeClient = capnp_rpc::new_client(Probe {
                    events: events_tx,
                    forward: Some(probe.0),
                    first: RefCell::new(None),
                    streaming: false,
                    streaming_error: false,
                });
                let shared = system_capnp::process::Client { client: forward.0 };
                let a = pair("A", shared.clone()).await;
                let b = pair("B", shared).await;
                let first = a.shared.stdin_request().send();
                assert_eq!(events.recv().await.unwrap(), (0, "A", 1));
                let second = b.shared.stdout_request().send();
                // Same-connection ordered barrier: B's preceding call was processed.
                b.barrier.wait_request().send().promise.await.unwrap();
                assert!(events.try_recv().is_err(), "B must remain queued behind A");
                b.gate.disable();
                release.send(()).unwrap();
                let observed = events.recv().await.unwrap();
                println!(
                    "origin=B, dispatch_poll_owner={}, method={}, retained_owner={}",
                    observed.1, observed.0, observed.2
                );
                assert_eq!(observed, (1, "A", 2));
                b.gate.enable();
                first.promise.await.unwrap();
                second.promise.await.unwrap();
                close(a).await;
                close(b).await;
            }),
        )
        .await
        .expect("probe watchdog");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn queued_future_resolution_alone_preserves_per_call_poll_owner() {
        tokio::time::timeout(
            Duration::from_secs(5),
            tokio::task::LocalSet::new().run_until(async {
                let (events_tx, mut events) = mpsc::unbounded_channel();
                let probe: ProbeClient = capnp_rpc::new_client(Probe {
                    events: events_tx.clone(),
                    forward: None,
                    first: RefCell::new(None),
                    streaming: false,
                    streaming_error: false,
                });
                let (release, held) = oneshot::channel();
                let shared: system_capnp::process::Client =
                    capnp_rpc::new_future_client(async move {
                        held.await.unwrap();
                        Ok(system_capnp::process::Client { client: probe.0 })
                    });
                let forward: ProbeClient = capnp_rpc::new_client(Probe {
                    events: events_tx,
                    forward: Some(shared.client),
                    first: RefCell::new(None),
                    streaming: false,
                    streaming_error: false,
                });
                let shared = system_capnp::process::Client { client: forward.0 };
                let a = pair("A", shared.clone()).await;
                let b = pair("B", shared).await;
                let first = a.shared.stdin_request().send();
                a.barrier.wait_request().send().promise.await.unwrap();
                let second = b.shared.stdout_request().send();
                b.barrier.wait_request().send().promise.await.unwrap();
                assert!(events.try_recv().is_err());
                a.gate.disable();
                release.send(()).unwrap();
                let observed_b = events.recv().await.unwrap();
                assert_eq!(observed_b, (1, "B", 2));
                assert!(
                    events.try_recv().is_err(),
                    "A's local call remains lazy until A polls"
                );
                a.gate.enable();
                let observed_a = events.recv().await.unwrap();
                assert_eq!(observed_a, (0, "A", 1));
                println!(
                    "queued resolution control: origin A -> {}, origin B -> {}",
                    observed_a.1, observed_b.1
                );
                first.promise.await.unwrap();
                second.promise.await.unwrap();
                close(a).await;
                close(b).await;
            }),
        )
        .await
        .expect("probe watchdog");
    }
    #[tokio::test(flavor = "current_thread")]
    async fn dropping_streaming_a_skips_cancelled_b_and_dispatches_c_with_c_context() {
        use futures::FutureExt;
        let (events_tx, mut events) = mpsc::unbounded_channel();
        let (_release, held) = oneshot::channel();
        let probe: ProbeClient = capnp_rpc::new_client(Probe {
            events: events_tx,
            forward: None,
            first: RefCell::new(Some(held)),
            streaming: true,
            streaming_error: false,
        });
        let target = system_capnp::process::Client { client: probe.0 };
        let mut a = target.stdin_request();
        a.hook.set_invocation_origin(Owner::new(201).0).unwrap();
        let a = a.send();
        let mut ap = Box::pin(a.promise);
        assert!(ap.as_mut().now_or_never().is_none());
        assert_eq!(events.try_recv().unwrap().2, 201);
        let mut b = target.stdout_request();
        b.hook.set_invocation_origin(Owner::new(202).0).unwrap();
        let b = b.send();
        let mut bp = Box::pin(b.promise);
        assert!(bp.as_mut().now_or_never().is_none());
        let mut c = target.stderr_request();
        c.hook.set_invocation_origin(Owner::new(203).0).unwrap();
        let c = c.send();
        let mut cp = Box::pin(c.promise);
        assert!(cp.as_mut().now_or_never().is_none());
        assert!(events.try_recv().is_err());
        drop(bp);
        drop(b.pipeline);
        drop(ap);
        drop(a.pipeline);
        cp.await.unwrap();
        let event = events.try_recv().unwrap();
        assert_eq!((event.0, event.2), (2, 203));
        assert!(events.try_recv().is_err(), "cancelled B never dispatched");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn streaming_error_rejects_queued_and_later_calls_without_dispatching_their_context() {
        use futures::FutureExt;
        let (events_tx, mut events) = mpsc::unbounded_channel();
        let (release, held) = oneshot::channel();
        let probe: ProbeClient = capnp_rpc::new_client(Probe {
            events: events_tx,
            forward: None,
            first: RefCell::new(Some(held)),
            streaming: true,
            streaming_error: true,
        });
        let target = system_capnp::process::Client { client: probe.0 };
        let mut a = target.stdin_request();
        a.hook.set_invocation_origin(Owner::new(204).0).unwrap();
        let a = a.send();
        let mut ap = Box::pin(a.promise);
        assert!(ap.as_mut().now_or_never().is_none());
        assert_eq!(events.try_recv().unwrap().2, 204);
        let mut b = target.stdout_request();
        b.hook.set_invocation_origin(Owner::new(205).0).unwrap();
        let b = b.send();
        let mut bp = Box::pin(b.promise);
        assert!(bp.as_mut().now_or_never().is_none());
        release.send(()).unwrap();
        assert!(ap.await.err().unwrap().extra.contains("streaming failure"));
        assert!(bp.await.err().unwrap().extra.contains("streaming failure"));
        let mut c = target.stderr_request();
        c.hook.set_invocation_origin(Owner::new(206).0).unwrap();
        assert!(c
            .send()
            .promise
            .await
            .err()
            .unwrap()
            .extra
            .contains("streaming failure"));
        assert!(events.try_recv().is_err());
    }

    struct SharedProducer {
        cap: system_capnp::process::Client,
        release: RefCell<Option<oneshot::Receiver<()>>>,
        events: mpsc::UnboundedSender<(&'static str, &'static str, u64)>,
    }
    impl system_capnp::process::Server for SharedProducer {
        async fn bootstrap(
            self: Rc<Self>,
            params: system_capnp::process::BootstrapParams,
            mut results: system_capnp::process::BootstrapResults,
        ) -> capnp::Result<()> {
            let before = owner_of(&*params.hook)?;
            self.events
                .send(("entered", POLL_OWNER.with(Cell::get), before.label()))
                .unwrap();
            let held = self.release.borrow_mut().take().unwrap();
            held.await.unwrap();
            let after = owner_of(&*params.hook)?;
            assert!(before.0.eq(&after.0));
            self.events
                .send(("resumed", POLL_OWNER.with(Cell::get), after.label()))
                .unwrap();
            results
                .get()
                .init_cap()
                .set_as_capability(self.cap.client.hook.add_ref());
            Ok(())
        }
    }
    struct SavePipeline {
        target: system_capnp::process::Client,
        saved: Rc<RefCell<Option<system_capnp::process::Client>>>,
    }
    impl system_capnp::process::Server for SavePipeline {
        async fn bootstrap(
            self: Rc<Self>,
            params: system_capnp::process::BootstrapParams,
            mut results: system_capnp::process::BootstrapResults,
        ) -> capnp::Result<()> {
            let owner = owner_of(&*params.hook)?;
            let mut request = self.target.bootstrap_request();
            request.hook.set_invocation_origin(owner.0)?;
            let sent = request.send();
            *self.saved.borrow_mut() = Some(system_capnp::process::Client::new(
                sent.pipeline.get_cap().as_cap(),
            ));
            let response = sent.promise.await?;
            results.set(response.get()?)?;
            Ok(())
        }
    }
    struct GetSavedPipeline(Rc<RefCell<Option<system_capnp::process::Client>>>);
    impl system_capnp::process::Server for GetSavedPipeline {
        async fn bootstrap(
            self: Rc<Self>,
            _: system_capnp::process::BootstrapParams,
            mut results: system_capnp::process::BootstrapResults,
        ) -> capnp::Result<()> {
            let cap = self.0.borrow().as_ref().unwrap().clone();
            results.get().init_cap().set_as_capability(cap.client.hook);
            Ok(())
        }
    }
    #[tokio::test(flavor = "current_thread")]
    async fn shared_answer_producer_a_resumes_under_b_but_consumer_keeps_b() {
        tokio::time::timeout(
            Duration::from_secs(5),
            tokio::task::LocalSet::new().run_until(async {
                let (dispatch_tx, mut dispatches) = mpsc::unbounded_channel();
                let child: ProbeClient = capnp_rpc::new_client(Probe {
                    events: dispatch_tx,
                    forward: None,
                    first: RefCell::new(None),
                    streaming: false,
                    streaming_error: false,
                });
                let (release, held) = oneshot::channel();
                let (event_tx, mut events) = mpsc::unbounded_channel();
                let producer = capnp_rpc::new_client(SharedProducer {
                    cap: system_capnp::process::Client { client: child.0 },
                    release: RefCell::new(Some(held)),
                    events: event_tx,
                });
                let saved = Rc::new(RefCell::new(None));
                let forward = capnp_rpc::new_client(SavePipeline {
                    target: producer,
                    saved: saved.clone(),
                });
                let a = pair("A", forward).await;
                let b = pair("B", capnp_rpc::new_client(GetSavedPipeline(saved))).await;
                let first = a.shared.bootstrap_request().send();
                assert_eq!(events.recv().await.unwrap(), ("entered", "A", 1));
                a.gate.disable();
                let response = b.shared.bootstrap_request().send().promise.await.unwrap();
                let pipeline: system_capnp::process::Client = response
                    .get()
                    .unwrap()
                    .get_cap()
                    .get_as_capability()
                    .unwrap();
                let second = pipeline.wait_request().send();
                b.barrier.wait_request().send().promise.await.unwrap();
                assert!(dispatches.try_recv().is_err());
                release.send(()).unwrap();
                second.promise.await.unwrap();
                assert_eq!(events.recv().await.unwrap(), ("resumed", "B", 1));
                assert_eq!(dispatches.recv().await.unwrap(), (3, "B", 2));
                a.gate.enable();
                first.promise.await.unwrap();
                close(a).await;
                close(b).await;
            }),
        )
        .await
        .expect("shared pipeline watchdog");
    }
}

struct Exchange {
    cap: vat_client::Client,
    saved: Rc<RefCell<Option<vat_client::Client>>>,
    seen: Rc<RefCell<Vec<(u64, usize, usize)>>>,
}
#[derive(Clone)]
struct ExchangeDispatch(Rc<Exchange>);
impl std::ops::Deref for ExchangeDispatch {
    type Target = Exchange;
    fn deref(&self) -> &Exchange {
        &self.0
    }
}
struct ExchangeClient(capnp::capability::Client);
impl capnp::capability::FromClientHook for ExchangeClient {
    fn new(hook: Box<dyn capnp::private::capability::ClientHook>) -> Self {
        Self(capnp::capability::Client::new(hook))
    }
    fn into_client_hook(self) -> Box<dyn capnp::private::capability::ClientHook> {
        self.0.hook
    }
    fn as_client_hook(&self) -> &dyn capnp::private::capability::ClientHook {
        self.0.hook.as_ref()
    }
}
impl capnp::capability::FromServer<Exchange> for ExchangeClient {
    type Dispatch = ExchangeDispatch;
    fn from_server(server: Rc<Exchange>) -> ExchangeDispatch {
        ExchangeDispatch(server)
    }
}
impl capnp::capability::Server for ExchangeDispatch {
    fn dispatch_call(
        self,
        interface: u64,
        _: u16,
        params: capnp::capability::Params<any_pointer::Owned>,
        mut results: capnp::capability::Results<any_pointer::Owned>,
    ) -> capnp::capability::DispatchCallResult {
        let result = (|| -> capnp::Result<()> {
            let (reader, owner) = (params.get()?, owner_of(&*params.hook)?);
            if interface == <vat_client::Client as HasTypeId>::TYPE_ID {
                let result: vat_client::dial_results::Builder = results.get().get_as()?;
                result
                    .init_cap()
                    .set_as_capability(self.0.cap.client.hook.add_ref());
            } else {
                let reader: ::authority::system_capnp::vat_listener::serve_raw_params::Reader =
                    reader.get_as()?;
                let cap: vat_client::Client = reader.get_cap().get_as_capability()?;
                self.0.seen.borrow_mut().push((
                    owner.label(),
                    cap.client.hook.get_ptr(),
                    cap.client.hook.get_brand(),
                ));
                *self.0.saved.borrow_mut() = Some(cap);
            }
            Ok(())
        })();
        capnp::capability::DispatchCallResult::new(
            Promise::from_future(async move { result }),
            false,
        )
    }
    fn as_ptr(&self) -> usize {
        Rc::as_ptr(&self.0) as usize
    }
}

#[tokio::test(flavor = "current_thread")]
async fn receiver_hosted_round_trip_preserves_identity_and_new_caller() {
    tokio::time::timeout(
        std::time::Duration::from_secs(5),
        tokio::task::LocalSet::new().run_until(async {
            let observed = Rc::new(RefCell::new(Vec::new()));
            let leaf: vat_client::Client = capnp_rpc::new_client(Leaf(observed.clone()));
            let expected = (leaf.client.hook.get_ptr(), leaf.client.hook.get_brand());
            let saved = Rc::new(RefCell::new(None));
            let seen = Rc::new(RefCell::new(Vec::new()));
            let exchange: ExchangeClient = capnp_rpc::new_client(Exchange {
                cap: leaf,
                saved: saved.clone(),
                seen: seen.clone(),
            });
            let (a, ah, ag) = pair(40, vat_client::Client { client: exchange.0 });
            RECEIVER_HOSTED.with(|count| count.set(0));
            let response = a.dial_request().send().promise.await.unwrap();
            let imported: vat_client::Client = response
                .get()
                .unwrap()
                .get_cap()
                .get_as_capability()
                .unwrap();
            let sink = ::authority::system_capnp::vat_listener::Client {
                client: a.client.clone(),
            };
            let mut request = sink.serve_raw_request();
            request
                .get()
                .init_cap()
                .set_as_capability(imported.client.hook.add_ref());
            request.send().promise.await.unwrap();
            assert_eq!(
                *seen.borrow(),
                vec![(40, expected.0, expected.1)],
                "receiverHosted resolves to the exact original Host hook"
            );
            assert!(
                RECEIVER_HOSTED.with(|count| count.get()) > 0,
                "wire must carry receiverHosted"
            );
            let resolved = saved.borrow_mut().take().unwrap();
            let (b, bh, bg) = pair(41, resolved);
            invoke(&b).await;
            assert_eq!(
                *observed.borrow(),
                vec![vec![41]],
                "delegation does not retain A's invocation owner"
            );
            close_pair(ah, ag).await;
            close_pair(bh, bg).await;
        }),
    )
    .await
    .unwrap();
}

#[tokio::test(flavor = "current_thread")]
async fn intentional_guest_proxy_owns_its_fresh_host_request() {
    tokio::time::timeout(
        std::time::Duration::from_secs(5),
        tokio::task::LocalSet::new().run_until(async {
            let observed = Rc::new(RefCell::new(Vec::new()));
            let leaf: vat_client::Client = capnp_rpc::new_client(Leaf(observed.clone()));
            let (host_io, guest_io) = tokio::io::duplex(8192);
            let (hr, hw) = tokio::io::split(host_io);
            let (gr, gw) = tokio::io::split(guest_io);
            let mut host_system = ManagedRpcSystem::new(
                Box::new(TaggedNetwork {
                    inner: Box::new(VatNetwork::new(
                        hr.compat(),
                        hw.compat_write(),
                        Side::Server,
                        Default::default(),
                    )),
                    owner: Owner::new(51),
                }),
                Some(leaf.client),
            );
            let guest_proxy: vat_client::Client = host_system.bootstrap(Side::Client);
            let (send_proxy, receive_proxy) = tokio::sync::oneshot::channel();
            let future_proxy: vat_client::Client = capnp_rpc::new_future_client(async move {
                receive_proxy
                    .await
                    .map_err(|_| capnp::Error::failed("proxy lost".into()))
            });
            let mut guest_system = ManagedRpcSystem::new(
                Box::new(VatNetwork::new(
                    gr.compat(),
                    gw.compat_write(),
                    Side::Client,
                    Default::default(),
                )),
                Some(future_proxy.client),
            );
            let host_leaf: vat_client::Client = guest_system.bootstrap(Side::Server);
            // Guest code deliberately constructs a fresh Host-facing invocation.
            send_proxy
                .send(capnp_rpc::new_client(UntaggedForward(host_leaf)))
                .ok()
                .unwrap();
            let bh = ManagedRpc::spawn(host_system);
            let bg = ManagedRpc::spawn(guest_system);
            let (a, ah, ag) = pair(50, guest_proxy);
            invoke(&a).await;
            assert_eq!(*observed.borrow(), vec![vec![51]]);
            close_pair(ah, ag).await;
            close_pair(bh, bg).await;
        }),
    )
    .await
    .unwrap();
}

fn request_with_owner<C: capnp::capability::FromClientHook, P, R>(
    target: &C,
    owner: Owner,
    build: impl FnOnce(&C) -> capnp::capability::Request<P, R>,
) -> capnp::capability::Request<P, R> {
    let mut request = build(target);
    request.hook.set_invocation_origin(owner.0).unwrap();
    request
}
struct TypedCallback(vat_client::Client);
impl vat_client::Server for TypedCallback {
    async fn dial(
        self: Rc<Self>,
        params: vat_client::DialParams,
        mut results: vat_client::DialResults,
    ) -> capnp::Result<()> {
        let (reader, owner) = (params.get()?, owner_of(&*params.hook)?);
        let peer = reader.get_peer()?.to_vec();
        let protocol = reader.get_protocol()?.to_str()?.to_owned();
        tokio::task::yield_now().await;
        let mut request = request_with_owner(&self.0, owner, vat_client::Client::dial_request);
        request.get().set_peer(&peer);
        request.get().set_protocol(protocol);
        let response = request.send().promise.await?;
        results.set(response.get()?)?;
        Ok(())
    }
}
#[tokio::test(flavor = "current_thread")]
async fn internal_typed_request_preserves_owner_results_and_pipeline() {
    tokio::time::timeout(
        std::time::Duration::from_secs(5),
        tokio::task::LocalSet::new().run_until(async {
            use capnp::capability::FromClientHook;
            let observed = Rc::new(RefCell::new(Vec::new()));
            let leaf: vat_client::Client = capnp_rpc::new_client(Leaf(observed.clone()));
            let (entered, mut events) = tokio::sync::mpsc::unbounded_channel();
            let (release, held) = tokio::sync::oneshot::channel();
            let target: vat_client::Client = capnp_rpc::new_client(ReturnCap {
                cap: leaf,
                entered,
                release: RefCell::new(Some(held)),
            });
            let ptr = target.client.hook.get_ptr();
            let (client, host, guest) =
                pair(70, capnp_rpc::new_client(TypedCallback(target.clone())));
            let first = client.dial_request().send();
            assert_eq!(events.recv().await.unwrap(), 70);
            let mut pipeline_seen = observe_pipeline();
            let pipelined = vat_client::Client::new(first.pipeline.get_cap().as_cap());
            let mut request = pipelined.dial_request();
            request.get().set_protocol("provenance-probe");
            let second = request.send();
            pipeline_seen.recv().await.unwrap();
            PIPELINE_SEEN.with(|slot| {
                slot.take();
            });
            release.send(()).unwrap();
            second.promise.await.unwrap();
            first.promise.await.unwrap();
            assert_eq!(target.client.hook.get_ptr(), ptr);
            assert_eq!(*observed.borrow(), vec![vec![70]]);
            close_pair(host, guest).await;
        }),
    )
    .await
    .unwrap();
}

#[tokio::test(flavor = "current_thread")]
async fn conflicting_context_is_sticky_and_ownerless_or_unknown_origin_fails_closed() {
    let observed = Rc::new(RefCell::new(Vec::new()));
    let bare: vat_client::Client = capnp_rpc::new_client(Leaf(observed.clone()));
    for target in [bare.clone(), membrane::membrane(bare, Rc::new(Allow))] {
        let a = Owner::new(100);
        let mut request = target.dial_request();
        request.get().set_protocol("provenance-probe");
        request.hook.set_invocation_origin(a.0.clone()).unwrap();
        request.hook.set_invocation_origin(a.0.clone()).unwrap();
        // Equal diagnostic labels still have distinct origin identities.
        assert!(request
            .hook
            .set_invocation_origin(Owner::new(100).0)
            .is_err());
        assert!(request.hook.set_invocation_origin(a.0).is_err());
        assert!(request.send().promise.await.is_err());
        assert!(checked_invoke(&target)
            .await
            .err()
            .unwrap()
            .extra
            .contains("missing"));
        let mut wrong = target.dial_request();
        wrong
            .hook
            .set_invocation_origin(InvocationOrigin::new())
            .unwrap();
        assert!(wrong
            .send()
            .promise
            .await
            .err()
            .unwrap()
            .extra
            .contains("unknown ownership"));
    }
    assert!(observed.borrow().is_empty());
}

struct Reentrant {
    child: vat_client::Client,
    before_after: Rc<RefCell<Vec<(u64, u64)>>>,
}
impl vat_client::Server for Reentrant {
    async fn dial(
        self: Rc<Self>,
        params: vat_client::DialParams,
        _: vat_client::DialResults,
    ) -> capnp::Result<()> {
        let owner = owner_of(&*params.hook)?;
        let mut nested = self.child.dial_request();
        nested.hook.set_invocation_origin(Owner::new(102).0)?;
        nested.get().set_protocol("provenance-probe");
        nested.send().promise.await?;
        let after = owner_of(&*params.hook)?;
        assert!(owner.0.eq(&after.0));
        self.before_after
            .borrow_mut()
            .push((owner.label(), after.label()));
        Ok(())
    }
}
#[tokio::test(flavor = "current_thread")]
async fn reentrant_request_cannot_replace_suspended_outer_context() {
    let observed = Rc::new(RefCell::new(Vec::new()));
    let child = capnp_rpc::new_client(Leaf(observed.clone()));
    let before_after = Rc::new(RefCell::new(Vec::new()));
    let outer: vat_client::Client = capnp_rpc::new_client(Reentrant {
        child,
        before_after: before_after.clone(),
    });
    let mut request = outer.dial_request();
    request
        .hook
        .set_invocation_origin(Owner::new(101).0)
        .unwrap();
    request.send().promise.await.unwrap();
    assert_eq!(*observed.borrow(), vec![vec![102]]);
    assert_eq!(*before_after.borrow(), vec![(101, 101)]);
    assert!(checked_invoke(&outer)
        .await
        .err()
        .unwrap()
        .extra
        .contains("missing"));
}

struct PanicProbe(Rc<RefCell<Vec<u64>>>);
impl vat_client::Server for PanicProbe {
    async fn dial(
        self: Rc<Self>,
        params: vat_client::DialParams,
        _: vat_client::DialResults,
    ) -> capnp::Result<()> {
        let owner = owner_of(&*params.hook)?;
        if params.get()?.get_protocol()?.to_str()? == "panic" {
            panic!("intentional request unwind");
        }
        self.0.borrow_mut().push(owner.label());
        Ok(())
    }
}
#[tokio::test(flavor = "current_thread")]
async fn panic_does_not_seed_the_next_request() {
    use futures::FutureExt;
    let observed = Rc::new(RefCell::new(Vec::new()));
    let target: vat_client::Client = capnp_rpc::new_client(PanicProbe(observed.clone()));
    let mut request = target.dial_request();
    request
        .hook
        .set_invocation_origin(Owner::new(103).0)
        .unwrap();
    request.get().set_protocol("panic");
    let panicked = std::panic::AssertUnwindSafe(async { request.send().promise.await })
        .catch_unwind()
        .await;
    assert!(panicked.is_err());
    assert!(checked_invoke(&target)
        .await
        .err()
        .unwrap()
        .extra
        .contains("missing"));
    let mut request = target.dial_request();
    request
        .hook
        .set_invocation_origin(Owner::new(104).0)
        .unwrap();
    request.send().promise.await.unwrap();
    assert_eq!(*observed.borrow(), vec![104]);
}

#[tokio::test(flavor = "current_thread")]
async fn cancelled_deferred_request_cannot_seed_next_owner() {
    use futures::FutureExt;
    let observed = Rc::new(RefCell::new(Vec::new()));
    let leaf: vat_client::Client = capnp_rpc::new_client(Leaf(observed.clone()));
    let (release, held) = tokio::sync::oneshot::channel();
    let future: vat_client::Client = capnp_rpc::new_future_client(async move {
        held.await
            .map_err(|_| capnp::Error::failed("lost target".into()))
    });
    let mut canceled = future.dial_request();
    canceled
        .hook
        .set_invocation_origin(Owner::new(105).0)
        .unwrap();
    canceled.get().set_protocol("provenance-probe");
    let canceled = canceled.send();
    let mut promise = Box::pin(canceled.promise);
    assert!(promise.as_mut().now_or_never().is_none());
    drop(promise);
    drop(canceled.pipeline);
    release.send(leaf).ok().unwrap();
    let mut kept = future.dial_request();
    kept.hook.set_invocation_origin(Owner::new(106).0).unwrap();
    kept.get().set_protocol("provenance-probe");
    kept.send().promise.await.unwrap();
    assert_eq!(*observed.borrow(), vec![vec![106]]);
    assert!(checked_invoke(&future)
        .await
        .err()
        .unwrap()
        .extra
        .contains("missing"));
}

struct IdentityLeaf {
    expected: InvocationOrigin,
    calls: Rc<std::cell::Cell<usize>>,
}
impl vat_client::Server for IdentityLeaf {
    async fn dial(
        self: Rc<Self>,
        params: vat_client::DialParams,
        _: vat_client::DialResults,
    ) -> capnp::Result<()> {
        let actual = owner_of(&*params.hook)?;
        assert!(
            actual.0.eq(&self.expected),
            "the exact trusted ingress token must survive"
        );
        self.calls.set(self.calls.get() + 1);
        Ok(())
    }
}
#[tokio::test(flavor = "current_thread")]
async fn trusted_ingress_token_identity_survives_nested_typed_and_membrane_forwarding() {
    tokio::time::timeout(
        std::time::Duration::from_secs(5),
        tokio::task::LocalSet::new().run_until(async {
            let owner = Owner::new(107);
            let calls = Rc::new(std::cell::Cell::new(0));
            let leaf: vat_client::Client = capnp_rpc::new_client(IdentityLeaf {
                expected: owner.0.clone(),
                calls: calls.clone(),
            });
            let forward: vat_client::Client = capnp_rpc::new_client(TypedForward(leaf));
            let nested =
                capnp_rpc::new_client(TypedForward(membrane::membrane(forward, Rc::new(Allow))));
            let (client, host, guest) = pair_with_owner(owner, nested);
            invoke(&client).await;
            // A remote request may retain metadata for local redirect, but cannot encode it on the wire.
            let mut remote = client.dial_request();
            remote
                .hook
                .set_invocation_origin(Owner::new(999).0)
                .unwrap();
            remote.get().set_protocol("provenance-probe");
            remote.send().promise.await.unwrap();
            assert_eq!(calls.get(), 2);
            close_pair(host, guest).await;
        }),
    )
    .await
    .unwrap();
}

struct ReturnBorrowedCap(vat_client::Client);
impl vat_client::Server for ReturnBorrowedCap {
    async fn dial(
        self: Rc<Self>,
        _: vat_client::DialParams,
        mut results: vat_client::DialResults,
    ) -> capnp::Result<()> {
        results
            .get()
            .init_cap()
            .set_as_capability(self.0.client.hook.add_ref());
        Ok(())
    }
}
#[tokio::test(flavor = "current_thread")]
async fn request_created_remote_then_redirected_local_retains_context() {
    redirect_roundtrip(false).await;
}
#[tokio::test(flavor = "current_thread")]
async fn streaming_request_created_remote_then_redirected_local_retains_context() {
    redirect_roundtrip(true).await;
}
async fn redirect_roundtrip(streaming: bool) {
    use capnp::capability::FromClientHook;
    tokio::time::timeout(
        std::time::Duration::from_secs(5),
        tokio::task::LocalSet::new().run_until(async {
            let owner = Owner::new(301);
            let calls = Rc::new(std::cell::Cell::new(0));
            let leaf: vat_client::Client = capnp_rpc::new_client(IdentityLeaf {
                expected: owner.0.clone(),
                calls: calls.clone(),
            });
            let expected_ptr = leaf.client.hook.get_ptr();
            let (host_io, guest_io) = tokio::io::duplex(8192);
            let (hr, hw) = tokio::io::split(host_io);
            let (gr, gw) = tokio::io::split(guest_io);
            let mut host = ManagedRpcSystem::new(
                Box::new(VatNetwork::new(
                    hr.compat(),
                    hw.compat_write(),
                    Side::Server,
                    Default::default(),
                )),
                Some(
                    capnp_rpc::new_client::<vat_client::Client, _>(ReturnBorrowedCap(leaf)).client,
                ),
            );
            let remote: vat_client::Client = host.bootstrap(Side::Client);
            let (send, receive) = tokio::sync::oneshot::channel();
            let deferred: vat_client::Client = capnp_rpc::new_future_client(async move {
                receive
                    .await
                    .map_err(|_| capnp::Error::failed("lost bootstrap".into()))
            });
            let mut guest = ManagedRpcSystem::new(
                Box::new(VatNetwork::new(
                    gr.compat(),
                    gw.compat_write(),
                    Side::Client,
                    Default::default(),
                )),
                Some(deferred.client),
            );
            let borrowed: vat_client::Client = guest.bootstrap(Side::Server);
            let host = ManagedRpc::spawn(host);
            let guest = ManagedRpc::spawn(guest);
            let exported = borrowed.dial_request().send().promise.await.unwrap();
            let leaf_import: vat_client::Client = exported
                .get()
                .unwrap()
                .get_cap()
                .get_as_capability()
                .unwrap();
            send.send(capnp_rpc::new_client(ReturnBorrowedCap(leaf_import)))
                .ok()
                .unwrap();
            let sent = remote.dial_request().send();
            let pipeline = vat_client::Client::new(sent.pipeline.get_cap().as_cap());
            let mut request = pipeline.dial_request();
            assert_ne!(
                request.hook.get_brand(),
                0,
                "request must be allocated while still remote"
            );
            request.hook.set_invocation_origin(owner.0).unwrap();
            sent.promise.await.unwrap();
            let resolved = capnp::capability::get_resolved_cap(pipeline).await;
            assert_eq!(
                resolved.client.hook.get_ptr(),
                expected_ptr,
                "receiverHosted resolves to the actual original local capability"
            );
            if streaming {
                request.hook.send_streaming().await.unwrap();
            } else {
                request.send().promise.await.unwrap();
            }
            assert_eq!(calls.get(), 1);
            close_pair(host, guest).await;
        }),
    )
    .await
    .expect("redirect watchdog");
}

#[path = "typed_invocation_origin/authority.rs"]
mod authority;
#[path = "typed_invocation_origin/pending.rs"]
mod pending;
#[path = "typed_invocation_origin/reconnect.rs"]
mod reconnect;
