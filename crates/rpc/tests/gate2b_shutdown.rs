//! Gate 2B characterizations of the separately reported shutdown defect.
//! A joined managed worker is not necessarily an error-free RPC shutdown.
use capnp::capability::Promise;
use capnp_rpc::{rpc_twoparty_capnp::Side, Connection, IncomingMessage, OutgoingMessage};
use futures::FutureExt;
use rpc::managed_rpc::{ManagedRpc, ManagedRpcSystem};
use std::{panic::AssertUnwindSafe, time::Duration};
use tokio::sync::oneshot;
use tokio_util::compat::{TokioAsyncReadCompatExt, TokioAsyncWriteCompatExt};

#[tokio::test(flavor = "current_thread")]
async fn raw_disconnect_then_eof_retains_known_dependency_panic() {
    tokio::time::timeout(Duration::from_secs(3), async {
        let (peer, stream) = tokio::io::duplex(8192);
        let (read, write) = tokio::io::split(stream);
        let rpc = capnp_rpc::RpcSystem::new(
            Box::new(capnp_rpc::twoparty::VatNetwork::new(
                read.compat(),
                write.compat_write(),
                Side::Server,
                Default::default(),
            )),
            None,
        );
        let mut disconnect = Box::pin(rpc.get_disconnector());
        let mut rpc = Box::pin(rpc);
        assert!(rpc.as_mut().now_or_never().is_none());
        assert!(disconnect.as_mut().now_or_never().is_none());
        drop(peer);
        let panic = AssertUnwindSafe(rpc)
            .catch_unwind()
            .await
            .expect_err("known dependency failure changed; reclassify this characterization");
        let text = panic
            .downcast_ref::<&str>()
            .copied()
            .or_else(|| panic.downcast_ref::<String>().map(String::as_str))
            .unwrap_or("");
        assert!(
            text.contains("message loop outlived connection state?"),
            "{text}"
        );
    })
    .await
    .expect("raw shutdown watchdog");
}

struct ObservedNetwork {
    inner: Box<dyn capnp_rpc::VatNetwork<Side>>,
    accepted: Option<oneshot::Sender<()>>,
    shutdown: Option<oneshot::Sender<()>>,
}
impl capnp_rpc::VatNetwork<Side> for ObservedNetwork {
    fn connect(&mut self, side: Side) -> Option<Box<dyn Connection<Side>>> {
        self.inner.connect(side)
    }
    fn accept(&mut self) -> Promise<Box<dyn Connection<Side>>, capnp::Error> {
        let accepted = self.accepted.take();
        let shutdown = self.shutdown.take();
        let future = self.inner.accept();
        Promise::from_future(async move {
            let inner = future.await?;
            if let Some(sender) = accepted {
                let _ = sender.send(());
            }
            Ok(Box::new(ObservedConnection { inner, shutdown }) as Box<dyn Connection<Side>>)
        })
    }
    fn drive_until_shutdown(&mut self) -> Promise<(), capnp::Error> {
        self.inner.drive_until_shutdown()
    }
}
struct ObservedConnection {
    inner: Box<dyn Connection<Side>>,
    shutdown: Option<oneshot::Sender<()>>,
}
impl Connection<Side> for ObservedConnection {
    fn incoming_invocation_origin(&self) -> Option<capnp::private::capability::InvocationOrigin> {
        self.inner.incoming_invocation_origin()
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
        self.inner.receive_incoming_message()
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
        if let Some(sender) = self.shutdown.take() {
            let _ = sender.send(());
        }
        self.inner.shutdown(result)
    }
}

#[tokio::test(flavor = "current_thread")]
async fn managed_disconnect_then_eof_joins_actual_worker() {
    tokio::time::timeout(
        Duration::from_secs(3),
        tokio::task::LocalSet::new().run_until(async {
            let (peer, stream) = tokio::io::duplex(8192);
            let (read, write) = tokio::io::split(stream);
            let (accepted, accepted_rx) = oneshot::channel();
            let (shutdown, shutdown_rx) = oneshot::channel();
            let network = ObservedNetwork {
                inner: Box::new(capnp_rpc::twoparty::VatNetwork::new(
                    read.compat(),
                    write.compat_write(),
                    Side::Server,
                    Default::default(),
                )),
                accepted: Some(accepted),
                shutdown: Some(shutdown),
            };
            let driver = ManagedRpc::spawn(ManagedRpcSystem::new(Box::new(network), None));
            accepted_rx.await.expect("accepted live transport");
            driver.shutdown();
            shutdown_rx.await.expect("disconnect reached transport");
            drop(peer);
            // This transport can finish managed shutdown before the late EOF is
            // read. The contract here is actual worker join, not a forced panic.
            // The raw test above independently establishes the remaining defect.
            if let Err(error) = driver.await.expect("actual managed worker joined") {
                assert!(
                    error
                        .extra
                        .contains("message loop outlived connection state?"),
                    "{error}"
                );
                assert!(error.extra.contains("panicked"), "{error}");
            }
        }),
    )
    .await
    .expect("managed shutdown watchdog");
}
