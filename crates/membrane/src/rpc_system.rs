//! Disconnect before network completion can terminate the RPC task set.

use capnp::capability::{Client, Promise};
use capnp_rpc::{Connection, Disconnector, RpcSystem, VatNetwork};
use futures::FutureExt;
use std::cell::RefCell;
use std::io::Write;
use std::panic::AssertUnwindSafe;
use std::rc::Rc;

/// Construct an RPC system that terminalizes connection ownership before its
/// network task completes. Owners must still disconnect on cancellation and
/// clear any local bootstrap ownership.
pub fn rpc_system<VatId: 'static>(
    network: Box<dyn VatNetwork<VatId>>,
    bootstrap: Option<Client>,
) -> RpcSystem<VatId> {
    let disconnect = Rc::new(RefCell::new(None));
    let rpc = RpcSystem::new(
        Box::new(DisconnectNetwork {
            inner: network,
            disconnect: disconnect.clone(),
        }),
        bootstrap,
    );
    *disconnect.borrow_mut() = Some(rpc.get_disconnector());
    rpc
}

/// Initiate connection terminalization through one supported future poll.
///
/// The vendored disconnector settles pending work and releases exports during
/// this poll. Waiting for its transport bookkeeping here could deadlock the
/// containing RPC task set, or hang after that task set has already ended.
/// Owners may subsequently drive a still-live RPC system to flush shutdown.
/// Errors and destructor panics propagate to the owner's failure boundary.
pub async fn initiate_disconnect<VatId: 'static>(
    disconnect: Disconnector<VatId>,
) -> capnp::Result<()> {
    match futures::future::select(Box::pin(disconnect), std::future::ready(())).await {
        futures::future::Either::Left((result, _)) => result,
        futures::future::Either::Right(_) => Ok(()),
    }
}

fn panic_error(stage: &str, panic: Box<dyn std::any::Any + Send>) -> capnp::Error {
    let detail = panic
        .downcast_ref::<String>()
        .map(String::as_str)
        .or_else(|| panic.downcast_ref::<&str>().copied())
        .unwrap_or("non-string panic payload");
    let error = capnp::Error::failed(format!("{stage} panicked: {detail}"));
    if let Err(secondary) = std::panic::catch_unwind(AssertUnwindSafe(|| drop(panic))) {
        // An arbitrary payload may panic again when destroyed. Its destructor
        // cannot replace the cause or skip connection terminalization.
        std::mem::forget(secondary);
        let _ = writeln!(
            std::io::stderr(),
            "{stage} panic payload destructor also panicked"
        );
    }
    error
}

struct DisconnectNetwork<VatId: 'static> {
    inner: Box<dyn VatNetwork<VatId>>,
    disconnect: Rc<RefCell<Option<Disconnector<VatId>>>>,
}

impl<VatId: 'static> VatNetwork<VatId> for DisconnectNetwork<VatId> {
    fn connect(&mut self, vat: VatId) -> Option<Box<dyn Connection<VatId>>> {
        self.inner.connect(vat)
    }

    fn accept(&mut self) -> Promise<Box<dyn Connection<VatId>>, capnp::Error> {
        self.inner.accept()
    }

    fn drive_until_shutdown(&mut self) -> Promise<(), capnp::Error> {
        let network = self.inner.drive_until_shutdown();
        let disconnect = self.disconnect.clone();
        Promise::from_future(async move {
            let result = AssertUnwindSafe(network)
                .catch_unwind()
                .await
                .unwrap_or_else(|panic| Err(panic_error("RPC network", panic)));
            let disconnect = disconnect.borrow_mut().take();
            let teardown = if let Some(disconnect) = disconnect {
                AssertUnwindSafe(initiate_disconnect(disconnect))
                    .catch_unwind()
                    .await
                    .unwrap_or_else(|panic| Err(panic_error("RPC disconnect", panic)))
            } else {
                Ok(())
            };
            match (result, teardown) {
                (Err(mut first), Err(secondary)) => {
                    let _ = writeln!(
                        std::io::stderr(),
                        "secondary RPC disconnect failure after {first}: {secondary}"
                    );
                    // RpcSystem normalizes Disconnected to success. Preserve
                    // the first cause's detail, but do not let that normal
                    // close convention hide a disconnect failure.
                    if first.kind == capnp::ErrorKind::Disconnected {
                        first.kind = capnp::ErrorKind::Failed;
                    }
                    Err(first)
                }
                (Err(first), Ok(())) => Err(first),
                (Ok(()), teardown) => teardown,
            }
        })
    }
}
