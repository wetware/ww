//! Guest-side Cap'n Proto transport for asynchronous Wetware cells.
//!
//! The Component Model polls one root future. The root future composes the
//! Cap'n Proto [`RpcSystem`], transport completion, and guest application.
//! P3 streams and application waitables provide all wakeups.

use capnp::capability::FromClientHook;
use capnp_rpc::rpc_twoparty_capnp::Side;
use capnp_rpc::twoparty::VatNetwork;
use capnp_rpc::RpcSystem;
use std::future::Future;
use std::pin::Pin;
use std::task::{Context, Poll};
use wit_bindgen::{
    StreamReader as WasiStreamReader, StreamResult, StreamWriter as WasiStreamWriter,
};

#[allow(
    dead_code,
    clippy::extra_unused_type_parameters,
    clippy::match_single_binding
)]
pub mod system_capnp {
    include!(concat!(env!("OUT_DIR"), "/system_capnp.rs"));
}

#[allow(
    dead_code,
    clippy::extra_unused_type_parameters,
    clippy::match_single_binding
)]
pub mod routing_capnp {
    include!(concat!(env!("OUT_DIR"), "/routing_capnp.rs"));
}

#[allow(
    dead_code,
    clippy::extra_unused_type_parameters,
    clippy::match_single_binding
)]
pub mod auth_capnp {
    include!(concat!(env!("OUT_DIR"), "/auth_capnp.rs"));
}

#[allow(
    dead_code,
    clippy::extra_unused_type_parameters,
    clippy::match_single_binding
)]
pub mod http_capnp {
    include!(concat!(env!("OUT_DIR"), "/http_capnp.rs"));
}

/// Application-defined named capabilities returned in `Membrane.extras`.
pub type Extras<'a> = capnp::struct_list::Reader<'a, system_capnp::export::Owned>;

/// A typed failure to read or resolve one capability from a graft response.
#[derive(Debug)]
pub enum ExtraError {
    InvalidResponse(capnp::Error),
    InvalidName(capnp::Error),
    InvalidCapability(capnp::Error),
    NotFound { name: String },
}

impl std::fmt::Display for ExtraError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidResponse(error)
            | Self::InvalidName(error)
            | Self::InvalidCapability(error) => error.fmt(f),
            Self::NotFound { name } => {
                write!(f, "capability '{name}' not found in graft response")
            }
        }
    }
}

impl std::error::Error for ExtraError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::InvalidResponse(error)
            | Self::InvalidName(error)
            | Self::InvalidCapability(error) => Some(error),
            Self::NotFound { .. } => None,
        }
    }
}

impl From<ExtraError> for capnp::Error {
    fn from(error: ExtraError) -> Self {
        match error {
            ExtraError::InvalidResponse(error)
            | ExtraError::InvalidName(error)
            | ExtraError::InvalidCapability(error) => error,
            ExtraError::NotFound { name } => {
                capnp::Error::failed(format!("capability '{name}' not found in graft response"))
            }
        }
    }
}

/// Look up a typed application capability by name in `Membrane.extras`.
pub fn get_extra<C: FromClientHook>(extras: &Extras<'_>, name: &str) -> Result<C, ExtraError> {
    for index in 0..extras.len() {
        let entry = extras.get(index);
        let entry_name = entry.get_name().map_err(ExtraError::InvalidResponse)?;
        let entry_name = entry_name
            .to_str()
            .map_err(|error| ExtraError::InvalidName(capnp::Error::failed(error.to_string())))?;
        if entry_name == name {
            return entry
                .get_cap()
                .get_as_capability::<C>()
                .map_err(ExtraError::InvalidCapability);
        }
    }
    Err(ExtraError::NotFound {
        name: name.to_string(),
    })
}

#[doc(hidden)]
pub mod bindings {
    wit_bindgen::generate!({
        path: "wit",
        world: "guest",
        generate_all,
        export_macro_name: "__export_system_guest",
        pub_export_macro: true,
    });
}

pub use bindings::__export_system_guest;
pub use bindings::exports::wasi::cli::run::Guest;

/// Export a type that implements the selective P3 [`Guest`] entry point.
#[macro_export]
macro_rules! export {
    ($ty:ident) => {
        $crate::__export_system_guest!($ty with_types_in $crate::bindings);
    };
}

type ReadFuture = Pin<Box<dyn Future<Output = (WasiStreamReader<u8>, StreamResult, Vec<u8>)>>>;

struct StreamReader {
    stream: Option<WasiStreamReader<u8>>,
    pending: Option<ReadFuture>,
    buffer: Vec<u8>,
    offset: usize,
    closed: bool,
}

impl StreamReader {
    fn new(stream: WasiStreamReader<u8>) -> Self {
        Self {
            stream: Some(stream),
            pending: None,
            buffer: Vec::new(),
            offset: 0,
            closed: false,
        }
    }

    fn copy_buffered(&mut self, output: &mut [u8]) -> Option<usize> {
        if self.offset == self.buffer.len() {
            return None;
        }
        let count = output.len().min(self.buffer.len() - self.offset);
        output[..count].copy_from_slice(&self.buffer[self.offset..self.offset + count]);
        self.offset += count;
        if self.offset == self.buffer.len() {
            self.buffer.clear();
            self.offset = 0;
        }
        Some(count)
    }
}

impl futures::io::AsyncRead for StreamReader {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        output: &mut [u8],
    ) -> Poll<std::io::Result<usize>> {
        let this = self.get_mut();
        if output.is_empty() {
            return Poll::Ready(Ok(0));
        }
        if let Some(count) = this.copy_buffered(output) {
            return Poll::Ready(Ok(count));
        }
        if this.closed {
            return Poll::Ready(Ok(0));
        }

        loop {
            if this.pending.is_none() {
                let mut stream = this.stream.take().expect("P3 input stream is available");
                let capacity = output.len().max(16 * 1024);
                this.pending = Some(Box::pin(async move {
                    let (status, bytes) = stream.read(Vec::with_capacity(capacity)).await;
                    (stream, status, bytes)
                }));
            }

            let result = match this
                .pending
                .as_mut()
                .expect("P3 read is pending")
                .as_mut()
                .poll(cx)
            {
                Poll::Pending => return Poll::Pending,
                Poll::Ready(result) => result,
            };
            this.pending = None;
            this.stream = Some(result.0);
            this.buffer = result.2;
            this.offset = 0;

            match result.1 {
                StreamResult::Complete(_) if this.buffer.is_empty() => continue,
                StreamResult::Complete(_) => {
                    return Poll::Ready(Ok(this
                        .copy_buffered(output)
                        .expect("completed P3 read contains bytes")));
                }
                StreamResult::Dropped => {
                    this.closed = true;
                    if this.buffer.is_empty() {
                        return Poll::Ready(Ok(0));
                    }
                    return Poll::Ready(Ok(this
                        .copy_buffered(output)
                        .expect("final P3 read contains bytes")));
                }
                StreamResult::Cancelled => {
                    return Poll::Ready(Err(std::io::Error::new(
                        std::io::ErrorKind::Interrupted,
                        "P3 transport read was cancelled",
                    )));
                }
            }
        }
    }
}

type WriteFuture = Pin<Box<dyn Future<Output = (WasiStreamWriter<u8>, StreamResult, Vec<u8>)>>>;

struct StreamWriter {
    stream: Option<WasiStreamWriter<u8>>,
    pending: Option<WriteFuture>,
}

impl futures::io::AsyncWrite for StreamWriter {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        bytes: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        if bytes.is_empty() {
            return Poll::Ready(Ok(0));
        }

        let this = self.get_mut();
        if this.pending.is_none() {
            let mut stream = match this.stream.take() {
                Some(stream) => stream,
                None => {
                    return Poll::Ready(Err(std::io::Error::new(
                        std::io::ErrorKind::BrokenPipe,
                        "P3 transport output is closed",
                    )));
                }
            };
            let bytes = bytes.to_vec();
            this.pending = Some(Box::pin(async move {
                let (status, remaining) = stream.write(bytes).await;
                (stream, status, remaining.into_vec())
            }));
        }

        let (stream, status, remaining) = match this
            .pending
            .as_mut()
            .expect("P3 write is pending")
            .as_mut()
            .poll(cx)
        {
            Poll::Pending => return Poll::Pending,
            Poll::Ready(result) => result,
        };
        this.pending = None;

        match status {
            StreamResult::Complete(count) => {
                this.stream = Some(stream);
                debug_assert_eq!(count + remaining.len(), bytes.len());
                Poll::Ready(Ok(count))
            }
            StreamResult::Dropped => Poll::Ready(Err(std::io::Error::new(
                std::io::ErrorKind::BrokenPipe,
                "P3 transport output was dropped",
            ))),
            StreamResult::Cancelled => Poll::Ready(Err(std::io::Error::new(
                std::io::ErrorKind::Interrupted,
                "P3 transport write was cancelled",
            ))),
        }
    }

    fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Poll::Ready(Ok(()))
    }

    fn poll_close(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        let this = self.get_mut();
        if let Some(pending) = this.pending.as_mut() {
            if pending.as_mut().poll(cx).is_pending() {
                return Poll::Pending;
            }
            this.pending = None;
        }
        this.stream = None;
        Poll::Ready(Ok(()))
    }
}

pub struct RpcSession<C> {
    pub rpc_system: RpcSystem<Side>,
    pub client: C,
    completion: wit_bindgen::FutureReader<
        Result<(), bindings::wetware::transport::connection::TransportError>,
    >,
}

impl<C: FromClientHook> RpcSession<C> {
    pub fn connect() -> Self {
        Self::connect_with_export(None)
    }

    /// Connect and export `bootstrap` as this vat's bootstrap capability.
    pub fn connect_with_export(bootstrap: Option<capnp::capability::Client>) -> Self {
        let (output, outgoing) = bindings::wit_stream::new();
        let (input, completion) = bindings::wetware::transport::connection::open(outgoing);
        let reader = StreamReader::new(input);
        let writer = StreamWriter {
            stream: Some(output),
            pending: None,
        };
        let network = VatNetwork::new(reader, writer, Side::Client, Default::default());
        let mut rpc_system = RpcSystem::new(Box::new(network), bootstrap);
        let client = rpc_system.bootstrap(Side::Server);
        Self {
            rpc_system,
            client,
            completion,
        }
    }
}

/// Run an application while exporting a bootstrap capability.
pub async fn serve<C, F, Fut>(
    bootstrap: capnp::capability::Client,
    f: F,
) -> Result<(), capnp::Error>
where
    C: FromClientHook,
    F: FnOnce(C) -> Fut,
    Fut: Future<Output = Result<(), capnp::Error>>,
{
    drive_session(RpcSession::<C>::connect_with_export(Some(bootstrap)), f).await
}

/// Run an application with the host-provided bootstrap capability.
pub async fn run<C, F, Fut>(f: F) -> Result<(), capnp::Error>
where
    C: FromClientHook,
    F: FnOnce(C) -> Fut,
    Fut: Future<Output = Result<(), capnp::Error>>,
{
    drive_session(RpcSession::<C>::connect(), f).await
}

async fn drive_session<C, F, Fut>(session: RpcSession<C>, f: F) -> Result<(), capnp::Error>
where
    C: FromClientHook,
    F: FnOnce(C) -> Fut,
    Fut: Future<Output = Result<(), capnp::Error>>,
{
    let RpcSession {
        rpc_system,
        client,
        completion,
    } = session;
    let application = f(client);
    let transport = async move { transport_completion_result(completion.await) };
    select_session(transport, rpc_system, application).await
}

fn transport_completion_result(
    result: Result<(), bindings::wetware::transport::connection::TransportError>,
) -> Result<(), capnp::Error> {
    match result {
        Ok(()) => Ok(()),
        Err(bindings::wetware::transport::connection::TransportError::Failed(message)) => Err(
            capnp::Error::failed(format!("P3 transport failed: {message}")),
        ),
    }
}

async fn select_session<Transport, Rpc, App>(
    transport: Transport,
    rpc: Rpc,
    application: App,
) -> Result<(), capnp::Error>
where
    Transport: Future<Output = Result<(), capnp::Error>>,
    Rpc: Future<Output = Result<(), capnp::Error>>,
    App: Future<Output = Result<(), capnp::Error>>,
{
    let root = futures::future::select(Box::pin(rpc), Box::pin(application));
    match futures::future::select(Box::pin(transport), Box::pin(root)).await {
        futures::future::Either::Left((transport_result, _)) => transport_result,
        futures::future::Either::Right((root_result, transport)) => {
            let root_result = match root_result {
                futures::future::Either::Left((result, application)) => {
                    drop(application);
                    result
                }
                futures::future::Either::Right((result, rpc)) => {
                    drop(rpc);
                    result
                }
            };
            match root_result {
                Err(error) => Err(error),
                Ok(()) => transport.await,
            }
        }
    }
}

#[cfg(test)]
async fn select_root<Rpc, App>(rpc: Rpc, application: App) -> Result<(), capnp::Error>
where
    Rpc: Future<Output = Result<(), capnp::Error>>,
    App: Future<Output = Result<(), capnp::Error>>,
{
    match futures::future::select(Box::pin(rpc), Box::pin(application)).await {
        futures::future::Either::Left((result, _)) => result,
        futures::future::Either::Right((result, _)) => result,
    }
}

#[cfg(test)]
mod graft_tests {
    use capnp::traits::{Imbue, ImbueMut};

    use super::*;

    fn failed(message: &str) -> capnp::Error {
        capnp::Error::failed(message.to_string())
    }

    #[test]
    fn application_first_success_is_root_success() {
        let result = futures::executor::block_on(select_root(
            std::future::pending(),
            std::future::ready(Ok(())),
        ));
        assert!(result.is_ok());
    }

    #[test]
    fn application_first_error_is_root_error() {
        let result = futures::executor::block_on(select_root(
            std::future::pending(),
            std::future::ready(Err(failed("application failed"))),
        ));
        assert!(result
            .expect_err("application error")
            .to_string()
            .contains("application failed"));
    }

    #[test]
    fn rpc_first_clean_close_is_root_success() {
        let result = futures::executor::block_on(select_root(
            std::future::ready(Ok(())),
            std::future::pending(),
        ));
        assert!(result.is_ok());
    }

    #[test]
    fn rpc_first_error_is_root_error() {
        let result = futures::executor::block_on(select_root(
            std::future::ready(Err(failed("RPC failed"))),
            std::future::pending(),
        ));
        assert!(result
            .expect_err("RPC error")
            .to_string()
            .contains("RPC failed"));
    }

    #[test]
    fn transport_failure_is_root_error() {
        let result = transport_completion_result(Err(
            bindings::wetware::transport::connection::TransportError::Failed(
                "transport write failed".to_string(),
            ),
        ));
        assert!(result
            .expect_err("transport failure")
            .to_string()
            .contains("P3 transport failed: transport write failed"));
    }

    #[test]
    fn orderly_transport_completion_is_root_success() {
        let result = futures::executor::block_on(select_session(
            std::future::ready(Ok(())),
            std::future::ready(Ok(())),
            std::future::pending(),
        ));
        assert!(result.is_ok());
    }

    #[test]
    fn transport_error_wins_over_simultaneous_clean_rpc_close() {
        let result = futures::executor::block_on(select_session(
            std::future::ready(Err(failed("transport failed"))),
            std::future::ready(Ok(())),
            std::future::pending(),
        ));
        assert!(result
            .expect_err("transport failure")
            .to_string()
            .contains("transport failed"));
    }

    struct TestMembrane;

    #[allow(refining_impl_trait)]
    impl system_capnp::membrane::Server for TestMembrane {
        fn graft(
            self: capnp::capability::Rc<Self>,
            _params: system_capnp::membrane::GraftParams,
            mut results: system_capnp::membrane::GraftResults,
        ) -> capnp::capability::Promise<(), capnp::Error> {
            results.get().set_peer_id(b"test-peer");
            capnp::capability::Promise::ok(())
        }
    }

    #[test]
    fn resolves_a_named_capability() {
        let client: system_capnp::membrane::Client = capnp_rpc::new_client(TestMembrane);
        let expected_ptr = client.client.hook.get_ptr();
        let mut message = capnp::message::Builder::new_default();
        let mut cap_table = Vec::new();
        {
            let mut graft =
                message.init_root::<system_capnp::membrane::graft_results::Builder<'_>>();
            graft.imbue_mut(&mut cap_table);
            let mut entry = graft.reborrow().init_extras(1).get(0);
            entry.set_name("application");
            entry.init_cap().set_as_capability(client.client.hook);
        }
        let mut graft = message
            .get_root_as_reader::<system_capnp::membrane::graft_results::Reader<'_>>()
            .unwrap();
        graft.imbue(&cap_table);
        let caps = graft.get_extras().unwrap();

        let found: capnp::capability::Client =
            get_extra(&caps, "application").expect("named capability");
        assert_eq!(found.hook.get_ptr(), expected_ptr);
    }

    #[test]
    fn missing_names_return_a_typed_error() {
        let mut message = capnp::message::Builder::new_default();
        let _: capnp::struct_list::Builder<'_, system_capnp::export::Owned> = message.initn_root(0);
        let caps = message.get_root_as_reader::<Extras<'_>>().unwrap();

        assert!(matches!(
            get_extra::<capnp::capability::Client>(&caps, "runtime"),
            Err(ExtraError::NotFound { name }) if name == "runtime"
        ));
    }

    #[test]
    fn invalid_utf8_names_fail_closed() {
        let mut message = capnp::message::Builder::new_default();
        {
            let mut caps: capnp::struct_list::Builder<'_, system_capnp::export::Owned> =
                message.initn_root(1);
            caps.reborrow()
                .get(0)
                .set_name(capnp::text::Reader(&[0xff]));
        }
        let caps = message.get_root_as_reader::<Extras<'_>>().unwrap();

        assert!(matches!(
            get_extra::<capnp::capability::Client>(&caps, "application"),
            Err(ExtraError::InvalidName(_))
        ));
    }
}
