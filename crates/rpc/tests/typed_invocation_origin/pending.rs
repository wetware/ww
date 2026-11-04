//! Supported pending-capability composition, not a natural export-task race.
//! A public future client queues B's original Params, then drains into the
//! retained RPC hook after that hook resolves to the original local grant.
use super::*;
use capnp::capability::FromClientHook;
use std::cell::Cell;

const B_MARKER: &str = "gate2a-pending-b-copy-and-redirect";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Disposition {
    Sent,
    Taken,
    DroppedUnsent,
}

type WireEvents = Rc<RefCell<Vec<(String, Disposition)>>>;

fn dial_protocol(body: any_pointer::Reader<'_>) -> Option<String> {
    let root: capnp_rpc::rpc_capnp::message::Reader<'_> = body.get_as().ok()?;
    let capnp_rpc::rpc_capnp::message::Call(call) = root.which().ok()? else {
        return None;
    };
    let call = call.ok()?;
    if call.get_interface_id() != <vat_client::Client as HasTypeId>::TYPE_ID
        || call.get_method_id() != 0
    {
        return None;
    }
    let params: vat_client::dial_params::Reader<'_> =
        call.get_params().ok()?.get_content().get_as().ok()?;
    Some(params.get_protocol().ok()?.to_str().ok()?.to_owned())
}

struct ObservedOutgoing {
    inner: Option<Box<dyn OutgoingMessage>>,
    events: WireEvents,
}
impl ObservedOutgoing {
    fn record(&self, disposition: Disposition) {
        if let Some(protocol) = self
            .inner
            .as_ref()
            .and_then(|inner| inner.get_body_as_reader().ok())
            .and_then(dial_protocol)
        {
            self.events.borrow_mut().push((protocol, disposition));
        }
    }
}
impl Drop for ObservedOutgoing {
    fn drop(&mut self) {
        self.record(Disposition::DroppedUnsent);
    }
}
impl OutgoingMessage for ObservedOutgoing {
    fn get_body(&mut self) -> capnp::Result<any_pointer::Builder<'_>> {
        self.inner.as_mut().unwrap().get_body()
    }
    fn get_body_as_reader(&self) -> capnp::Result<any_pointer::Reader<'_>> {
        self.inner.as_ref().unwrap().get_body_as_reader()
    }
    fn send(
        mut self: Box<Self>,
    ) -> (
        Promise<(), capnp::Error>,
        Rc<capnp::message::Builder<capnp::message::HeapAllocator>>,
    ) {
        self.record(Disposition::Sent);
        self.inner.take().unwrap().send()
    }
    fn take(mut self: Box<Self>) -> capnp::message::Builder<capnp::message::HeapAllocator> {
        self.record(Disposition::Taken);
        self.inner.take().unwrap().take()
    }
    fn size_in_words(&self) -> usize {
        self.inner.as_ref().unwrap().size_in_words()
    }
}

struct ObservedIncoming {
    inner: Box<dyn IncomingMessage>,
    arrived: Option<tokio::sync::mpsc::UnboundedSender<()>>,
    reported: Cell<bool>,
}
impl IncomingMessage for ObservedIncoming {
    fn get_body(&self) -> capnp::Result<any_pointer::Reader<'_>> {
        let body = self.inner.get_body()?;
        if !self.reported.get() && dial_protocol(body).as_deref() == Some(B_MARKER) {
            self.reported.set(true);
            if let Some(arrived) = &self.arrived {
                let _ = arrived.send(());
            }
        }
        Ok(body)
    }
}

struct ObservedConnection {
    inner: Box<dyn Connection<Side>>,
    events: WireEvents,
    arrived: Option<tokio::sync::mpsc::UnboundedSender<()>>,
}
impl Connection<Side> for ObservedConnection {
    fn incoming_invocation_origin(&self) -> Option<InvocationOrigin> {
        self.inner.incoming_invocation_origin()
    }
    fn get_peer_vat_id(&self) -> Side {
        self.inner.get_peer_vat_id()
    }
    fn new_outgoing_message(&mut self, words: u32) -> Box<dyn OutgoingMessage> {
        Box::new(ObservedOutgoing {
            inner: Some(self.inner.new_outgoing_message(words)),
            events: self.events.clone(),
        })
    }
    fn receive_incoming_message(
        &mut self,
    ) -> Promise<Option<Box<dyn IncomingMessage>>, capnp::Error> {
        let incoming = self.inner.receive_incoming_message();
        let arrived = self.arrived.clone();
        Promise::from_future(async move {
            Ok(incoming.await?.map(|inner| {
                Box::new(ObservedIncoming {
                    inner,
                    arrived,
                    reported: Cell::new(false),
                }) as Box<dyn IncomingMessage>
            }))
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

struct ObservedNetwork {
    inner: Box<dyn Network<Side>>,
    events: WireEvents,
    arrived: Option<tokio::sync::mpsc::UnboundedSender<()>>,
}
impl Network<Side> for ObservedNetwork {
    fn connect(&mut self, vat: Side) -> Option<Box<dyn Connection<Side>>> {
        self.inner.connect(vat).map(|inner| {
            Box::new(ObservedConnection {
                inner,
                events: self.events.clone(),
                arrived: self.arrived.clone(),
            }) as Box<dyn Connection<Side>>
        })
    }
    fn accept(&mut self) -> Promise<Box<dyn Connection<Side>>, capnp::Error> {
        let accept = self.inner.accept();
        let events = self.events.clone();
        let arrived = self.arrived.clone();
        Promise::from_future(async move {
            Ok(Box::new(ObservedConnection {
                inner: accept.await?,
                events,
                arrived,
            }) as Box<dyn Connection<Side>>)
        })
    }
    fn drive_until_shutdown(&mut self) -> Promise<(), capnp::Error> {
        self.inner.drive_until_shutdown()
    }
}

struct RecordedLeaf(Rc<RefCell<Vec<InvocationOrigin>>>);
impl vat_client::Server for RecordedLeaf {
    async fn dial(
        self: Rc<Self>,
        params: vat_client::DialParams,
        _: vat_client::DialResults,
    ) -> capnp::Result<()> {
        self.0.borrow_mut().push(owner_of(&*params.hook)?.0);
        Ok(())
    }
}
struct DialOnly(Rc<RefCell<Vec<u16>>>);
impl membrane::Policy for DialOnly {
    fn check(&self, interface: u64, method: u16) -> capnp::Result<()> {
        self.0.borrow_mut().push(method);
        if interface == <vat_client::Client as HasTypeId>::TYPE_ID && method == 0 {
            Ok(())
        } else {
            Err(capnp::Error::failed("outside original dial grant".into()))
        }
    }
}
struct GatedReturn {
    original_import: vat_client::Client,
    entered: RefCell<Option<tokio::sync::oneshot::Sender<()>>>,
    release: RefCell<Option<tokio::sync::oneshot::Receiver<()>>>,
}
impl vat_client::Server for GatedReturn {
    async fn dial(
        self: Rc<Self>,
        _: vat_client::DialParams,
        mut results: vat_client::DialResults,
    ) -> capnp::Result<()> {
        let release = self.release.borrow_mut().take().unwrap();
        let _ = self.entered.borrow_mut().take().unwrap().send(());
        release
            .await
            .map_err(|_| capnp::Error::failed("return gate lost".into()))?;
        results
            .get()
            .init_cap()
            .set_as_capability(self.original_import.client.hook.add_ref());
        Ok(())
    }
}

#[tokio::test(flavor = "current_thread")]
async fn pending_original_grant_preserves_a_b_b_through_private_copy_and_local_redirect() {
    tokio::time::timeout(std::time::Duration::from_secs(10), tokio::task::LocalSet::new().run_until(async {
        let owner_a = Owner::new(2201);
        let owner_b = Owner::new(2202);
        let observed = Rc::new(RefCell::new(Vec::new()));
        let policy_calls = Rc::new(RefCell::new(Vec::new()));
        let original: vat_client::Client = membrane::membrane(
            capnp_rpc::new_client(RecordedLeaf(observed.clone())),
            Rc::new(DialOnly(policy_calls.clone())),
        );
        let original_identity = (original.client.hook.get_ptr(), original.client.hook.get_brand());
        let a_events = Rc::new(RefCell::new(Vec::new()));
        let (host_io, guest_io) = tokio::io::duplex(8192);
        let (hr, hw) = tokio::io::split(host_io);
        let (gr, gw) = tokio::io::split(guest_io);
        let mut host = ManagedRpcSystem::new(Box::new(TaggedNetwork {
            inner: Box::new(ObservedNetwork {
                inner: Box::new(VatNetwork::new(hr.compat(), hw.compat_write(), Side::Server, Default::default())),
                events: a_events.clone(), arrived: None,
            }), owner: owner_a.clone(),
        }), Some(capnp_rpc::new_client::<vat_client::Client, _>(ReturnBorrowedCap(original.clone())).client));
        let guest_service: vat_client::Client = host.bootstrap(Side::Client);
        let (install, installed) = tokio::sync::oneshot::channel();
        let deferred: vat_client::Client = capnp_rpc::new_future_client(async move {
            installed.await.map_err(|_| capnp::Error::failed("guest bootstrap lost".into()))
        });
        let mut guest = ManagedRpcSystem::new(Box::new(VatNetwork::new(gr.compat(), gw.compat_write(), Side::Client, Default::default())), Some(deferred.client));
        let fetch: vat_client::Client = guest.bootstrap(Side::Server);
        let host = ManagedRpc::spawn(host);
        let guest = ManagedRpc::spawn(guest);
        let response = fetch.dial_request().send().promise.await.unwrap();
        let original_import: vat_client::Client = response.get().unwrap().get_cap().get_as_capability().unwrap();
        invoke(&original_import).await;
        assert!(observed.borrow()[0] == owner_a.0);

        let (returned, return_gate) = tokio::sync::oneshot::channel();
        let (entered, entry) = tokio::sync::oneshot::channel();
        install.send(capnp_rpc::new_client(GatedReturn {
            original_import, entered: RefCell::new(Some(entered)), release: RefCell::new(Some(return_gate)),
        })).ok().unwrap();
        let result = guest_service.dial_request().send();
        let retained_rpc = vat_client::Client::new(result.pipeline.get_cap().as_cap());
        entry.await.unwrap();
        assert!(retained_rpc.client.hook.get_resolved().is_none());
        let rpc_brand = retained_rpc.client.hook.get_brand();
        assert_ne!(rpc_brand, 0);
        assert_ne!(rpc_brand, original_identity.1);

        let (drain, drain_gate) = tokio::sync::oneshot::channel();
        let retained_for_queue = retained_rpc.clone();
        let queued: vat_client::Client = capnp_rpc::new_future_client(async move {
            drain_gate.await.map_err(|_| capnp::Error::failed("queue gate lost".into()))?;
            // Do not substitute get_resolved_cap's local result. Queue drain
            // calls this retained RPC hook and therefore exercises its copy.
            Ok(retained_for_queue)
        });
        let (arrived, mut arrival) = tokio::sync::mpsc::unbounded_channel();
        let (bh_io, bg_io) = tokio::io::duplex(8192);
        let (br, bw) = tokio::io::split(bh_io);
        let bh = ManagedRpc::spawn(ManagedRpcSystem::new(Box::new(TaggedNetwork {
            inner: Box::new(ObservedNetwork {
                inner: Box::new(VatNetwork::new(br.compat(), bw.compat_write(), Side::Server, Default::default())),
                events: Rc::new(RefCell::new(Vec::new())), arrived: Some(arrived),
            }), owner: owner_b.clone(),
        }), Some(queued.client)));
        let rpc::vat_dial::VatDial { bootstrap: b, driver: bg } = rpc::vat_dial::connect::<_, vat_client::Client>(bg_io.compat());
        let mut request = b.dial_request();
        request.get().set_protocol(B_MARKER);
        let b_response = request.send();
        arrival.recv().await.unwrap();
        // The receive callback and handle_message's queue insertion execute
        // synchronously in one driver poll before this task can resume.
        assert!(retained_rpc.client.hook.get_resolved().is_none());
        assert_eq!(observed.borrow().len(), 1);
        assert!(!a_events.borrow().iter().any(|(protocol, _)| protocol == B_MARKER));

        returned.send(()).unwrap();
        result.promise.await.unwrap();
        let resolved = capnp::capability::get_resolved_cap(retained_rpc.clone()).await;
        assert_eq!((resolved.client.hook.get_ptr(), resolved.client.hook.get_brand()), original_identity);
        assert_eq!(retained_rpc.client.hook.get_brand(), rpc_brand);
        assert_eq!(observed.borrow().len(), 1);
        drain.send(()).unwrap();
        b_response.promise.await.unwrap();
        assert!(observed.borrow()[1] == owner_b.0);
        let marked: Vec<_> = a_events.borrow().iter().filter(|(protocol, _)| protocol == B_MARKER).map(|(_, disposition)| *disposition).collect();
        assert_eq!(marked, vec![Disposition::DroppedUnsent], "private copy allocates the marked outgoing Call; local redirect discards it without send/take");

        let denied = b.client.new_call::<any_pointer::Owned, any_pointer::Owned>(<vat_client::Client as HasTypeId>::TYPE_ID, u16::MAX, None).send().promise.await;
        assert!(denied.is_err(), "original attenuation must survive delegation");
        assert_eq!(observed.borrow().len(), 2);
        assert_eq!(*policy_calls.borrow(), vec![0, 0, u16::MAX]);
        let independent: vat_client::Client = capnp_rpc::new_client(RecordedLeaf(observed.clone()));
        let (independent_b, ih, ig) = pair_with_owner(owner_b.clone(), independent);
        invoke(&independent_b).await;
        {
            let owners = observed.borrow();
            assert_eq!(owners.len(), 3);
            assert!(owners[0] == owner_a.0 && owners[1] == owner_b.0 && owners[2] == owner_b.0, "same original delegated grant must yield [A, B, B]");
        }
        close_pair(ih, ig).await;
        close_pair(bh, bg).await;
        close_pair(host, guest).await;
    })).await.expect("pending grant watchdog");
}
