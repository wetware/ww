//! Guest-side Cap'n Proto transport for asynchronous Wetware cells.
//!
//! The Component Model polls one root future. The root future composes the
//! Cap'n Proto [`RpcSystem`], transport completion, and guest application.
//! P3 streams and application waitables provide all wakeups.

use capnp::capability::FromClientHook;
use capnp_rpc::rpc_twoparty_capnp::Side;
use capnp_rpc::twoparty::VatNetwork;
use capnp_rpc::RpcSystem;
use futures::FutureExt;
use std::cell::Cell;
use std::future::Future;
use std::io::Write;
use std::panic::AssertUnwindSafe;
use std::pin::Pin;
use std::rc::Rc;
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

/// Request-local state for work that must complete before a finite root succeeds.
///
/// Clone the guard into the application future. Call [`Self::complete`] only
/// after the required work succeeds, then call [`Self::require`] if the session
/// returns success. The guard does not change the shared RPC session selector.
#[derive(Clone, Default)]
pub struct CompletionGuard {
    complete: Rc<Cell<bool>>,
}

impl CompletionGuard {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn complete(&self) {
        self.complete.set(true);
    }

    pub fn require(&self, operation: &str) -> Result<(), capnp::Error> {
        if self.complete.get() {
            Ok(())
        } else {
            Err(capnp::Error::failed(format!(
                "{operation} did not complete before the session ended"
            )))
        }
    }
}

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

/// Owns one guest RPC connection, including its synchronous terminalization.
///
/// Extracting `rpc_system` or `client` does not detach the connection: dropping
/// the remaining session owner terminalizes their connection as well.
pub struct RpcSession<C> {
    // Fields drop in declaration order, including after a partial move.
    cleanup: SessionCleanup,
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
        let bootstrap_owner = bootstrap.map(membrane::RpcBootstrap::new);
        let mut rpc_system = membrane::rpc_system(
            Box::new(network),
            bootstrap_owner.as_ref().map(membrane::RpcBootstrap::client),
        );
        let client = rpc_system.bootstrap(Side::Server);
        let cleanup = SessionCleanup::new(rpc_system.get_disconnector(), bootstrap_owner);
        Self {
            cleanup,
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
        cleanup,
        rpc_system,
        client,
        completion,
    } = session;
    let transport = async move { transport_completion_result(completion.await) };
    select_session_with_cleanup(transport, rpc_system, || f(client), cleanup).await
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

// Task 3 makes one supported disconnector poll settle connection-owned work.
// This owner requires no RpcSystem poll, including during component cancellation.
struct SessionCleanup {
    disconnect: Option<capnp_rpc::Disconnector<Side>>,
    bootstrap: Option<membrane::RpcBootstrap>,
}

impl SessionCleanup {
    fn new(
        disconnect: capnp_rpc::Disconnector<Side>,
        bootstrap: Option<membrane::RpcBootstrap>,
    ) -> Self {
        Self {
            disconnect: Some(disconnect),
            bootstrap,
        }
    }

    fn finish(&mut self) -> capnp::Result<()> {
        let mut first = None;
        if let Some(disconnect) = self.disconnect.take() {
            cleanup_step(&mut first, "RPC disconnect", || {
                membrane::initiate_disconnect(disconnect)
                    .now_or_never()
                    .unwrap_or_else(|| {
                        Err(capnp::Error::failed(
                            "RPC terminalization did not complete synchronously".into(),
                        ))
                    })
            });
        }
        // Terminal state must exist before bootstrap destruction can reenter.
        cleanup_step(&mut first, "RPC bootstrap release", || {
            drop(self.bootstrap.take());
            Ok(())
        });
        first.map_or(Ok(()), Err)
    }
}

impl Drop for SessionCleanup {
    fn drop(&mut self) {
        if let Err(error) = self.finish() {
            let _ = writeln!(std::io::stderr(), "RPC session cleanup failed: {error}");
        }
    }
}

// Keep every selected future owned until disconnect and bootstrap release end.
// Explicit finish and Drop use the same consuming path. Separate unwind
// boundaries let one user destructor fail without skipping the other owners.
struct SessionScope<Transport, Rpc, App, MakeApp> {
    cleanup: SessionCleanup,
    application: Option<Pin<Box<App>>>,
    make_application: Option<MakeApp>,
    rpc: Option<Pin<Box<Rpc>>>,
    transport: Option<Pin<Box<Transport>>>,
}

impl<Transport, Rpc, App, MakeApp> SessionScope<Transport, Rpc, App, MakeApp> {
    fn finish(&mut self, keep_transport: bool) -> capnp::Result<()> {
        let mut first = self.cleanup.finish().err();
        cleanup_step(&mut first, "application future release", || {
            drop(self.application.take());
            Ok(())
        });
        cleanup_step(&mut first, "application factory release", || {
            drop(self.make_application.take());
            Ok(())
        });
        cleanup_step(&mut first, "RPC driver release", || {
            drop(self.rpc.take());
            Ok(())
        });
        if !keep_transport || first.is_some() {
            cleanup_step(&mut first, "transport future release", || {
                drop(self.transport.take());
                Ok(())
            });
        }
        first.map_or(Ok(()), Err)
    }
}

impl<Transport, Rpc, App, MakeApp> Drop for SessionScope<Transport, Rpc, App, MakeApp> {
    fn drop(&mut self) {
        if let Err(error) = self.finish(false) {
            let _ = writeln!(std::io::stderr(), "RPC session cleanup failed: {error}");
        }
    }
}

fn cleanup_step(
    first: &mut Option<capnp::Error>,
    stage: &str,
    f: impl FnOnce() -> capnp::Result<()>,
) {
    let result = std::panic::catch_unwind(AssertUnwindSafe(f)).unwrap_or_else(|panic| {
        let detail = panic
            .downcast_ref::<String>()
            .map(String::as_str)
            .or_else(|| panic.downcast_ref::<&str>().copied())
            .unwrap_or("non-string panic payload");
        let error = capnp::Error::failed(format!("{stage} panicked: {detail}"));
        if let Err(secondary) = std::panic::catch_unwind(AssertUnwindSafe(|| drop(panic))) {
            // A recursively panicking payload cannot replace the first cause.
            std::mem::forget(secondary);
        }
        Err(error)
    });
    if let Err(error) = result {
        if let Some(cause) = first.as_ref() {
            let _ = writeln!(
                std::io::stderr(),
                "secondary RPC session cleanup failure after {cause}: {error}"
            );
        } else {
            *first = Some(error);
        }
    }
}

fn select_session_with_cleanup<Transport, Rpc, App, MakeApp>(
    transport: Transport,
    rpc: Rpc,
    make_application: MakeApp,
    cleanup: SessionCleanup,
) -> impl Future<Output = capnp::Result<()>>
where
    Transport: Future<Output = capnp::Result<()>>,
    Rpc: Future<Output = capnp::Result<()>>,
    App: Future<Output = capnp::Result<()>>,
    MakeApp: FnOnce() -> App,
{
    // Construct synchronously: even dropping this future before its first poll
    // must terminalize the connection without starting RPC or application work.
    let mut scope = SessionScope {
        cleanup,
        application: None::<Pin<Box<App>>>,
        make_application: Some(make_application),
        rpc: Some(Box::pin(rpc)),
        transport: Some(Box::pin(transport)),
    };
    async move {
        let selected = AssertUnwindSafe(async {
            // The cleanup owner is installed before invoking application code.
            scope.application = Some(Box::pin(scope.make_application.take().unwrap()()));
            let root = futures::future::select(
                scope.rpc.as_mut().unwrap().as_mut(),
                scope.application.as_mut().unwrap().as_mut(),
            );
            match futures::future::select(
                scope.transport.as_mut().unwrap().as_mut(),
                Box::pin(root),
            )
            .await
            {
                futures::future::Either::Left((result, _)) => (result, true),
                futures::future::Either::Right((root, _)) => match root {
                    futures::future::Either::Left((result, _))
                    | futures::future::Either::Right((result, _)) => (result, false),
                },
            }
        })
        .catch_unwind()
        .await;
        let keep_transport = matches!(&selected, Ok((Ok(()), false)));
        let cleanup = scope.finish(keep_transport);
        let transport = scope.transport.take();
        drop(scope);

        match selected {
            Err(panic) => {
                if let Err(error) = cleanup {
                    let _ = writeln!(
                        std::io::stderr(),
                        "secondary RPC session cleanup failure during unwind: {error}"
                    );
                }
                std::panic::resume_unwind(panic);
            }
            Ok((Err(error), _)) => {
                if let Err(secondary) = cleanup {
                    let _ = writeln!(
                        std::io::stderr(),
                        "secondary RPC session cleanup failure after {error}: {secondary}"
                    );
                }
                Err(error)
            }
            Ok((Ok(()), _)) => {
                cleanup?;
                match transport {
                    Some(transport) => transport.await,
                    None => Ok(()),
                }
            }
        }
    }
}

#[cfg(test)]
fn select_session_with_disconnect<Transport, Rpc, App>(
    transport: Transport,
    rpc: Rpc,
    application: App,
    disconnect: capnp_rpc::Disconnector<Side>,
    bootstrap: Option<membrane::RpcBootstrap>,
) -> impl Future<Output = capnp::Result<()>>
where
    Transport: Future<Output = capnp::Result<()>>,
    Rpc: Future<Output = capnp::Result<()>>,
    App: Future<Output = capnp::Result<()>>,
{
    select_session_with_cleanup(
        transport,
        rpc,
        || application,
        SessionCleanup::new(disconnect, bootstrap),
    )
}

#[cfg(test)]
async fn select_session<Transport, Rpc, App>(
    transport: Transport,
    rpc: Rpc,
    application: App,
) -> capnp::Result<()>
where
    Transport: Future<Output = capnp::Result<()>>,
    Rpc: Future<Output = capnp::Result<()>>,
    App: Future<Output = capnp::Result<()>>,
{
    select_session_with_cleanup(
        transport,
        rpc,
        || application,
        SessionCleanup {
            disconnect: None,
            bootstrap: None,
        },
    )
    .await
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
mod session_tests;

#[cfg(test)]
mod graft_tests {
    use capnp::traits::{Imbue, ImbueMut};
    use std::cell::Cell;
    use std::rc::Rc;
    use std::task::Poll;

    use super::*;

    fn failed(message: &str) -> capnp::Error {
        capnp::Error::failed(message.to_string())
    }

    fn pending_once_then_ok() -> impl Future<Output = Result<(), capnp::Error>> {
        let first_poll = Cell::new(true);
        futures::future::poll_fn(move |cx| {
            if first_poll.replace(false) {
                cx.waker().wake_by_ref();
                Poll::Pending
            } else {
                Poll::Ready(Ok(()))
            }
        })
    }

    struct DropMarker(Rc<Cell<bool>>);

    impl Drop for DropMarker {
        fn drop(&mut self) {
            self.0.set(true);
        }
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
    fn clean_session_completion_does_not_satisfy_an_incomplete_guard() {
        let completion = CompletionGuard::new();
        let output_started = Rc::new(Cell::new(false));
        let output_dropped = Rc::new(Cell::new(false));
        let rpc_completed = Rc::new(Cell::new(false));
        let transport_completed = Rc::new(Cell::new(false));

        let application = {
            let output_started = output_started.clone();
            let output_dropped = output_dropped.clone();
            async move {
                let _drop_marker = DropMarker(output_dropped);
                output_started.set(true);
                std::future::pending::<Result<(), capnp::Error>>().await
            }
        };
        let rpc = {
            let output_started = output_started.clone();
            let rpc_completed = rpc_completed.clone();
            futures::future::poll_fn(move |cx| {
                if output_started.get() {
                    rpc_completed.set(true);
                    Poll::Ready(Ok(()))
                } else {
                    cx.waker().wake_by_ref();
                    Poll::Pending
                }
            })
        };
        let transport = {
            let rpc_completed = rpc_completed.clone();
            let transport_completed = transport_completed.clone();
            futures::future::poll_fn(move |cx| {
                if rpc_completed.get() {
                    transport_completed.set(true);
                    Poll::Ready(Ok(()))
                } else {
                    cx.waker().wake_by_ref();
                    Poll::Pending
                }
            })
        };

        let result = futures::executor::block_on(select_session(transport, rpc, application))
            .and_then(|()| completion.require("finite response output"));

        assert!(output_started.get(), "finite output did not start");
        assert!(
            output_dropped.get(),
            "pending finite output was not dropped"
        );
        assert!(rpc_completed.get(), "RPC did not close cleanly");
        assert!(
            transport_completed.get(),
            "transport did not complete after the clean RPC close"
        );
        let error = result.expect_err("an incomplete response must fail");
        assert!(error.to_string().contains("finite response output"));
    }

    #[test]
    fn completion_guards_are_request_local() {
        let first_request = CompletionGuard::new();
        let first_application = first_request.clone();
        let second_request = CompletionGuard::new();

        first_application.complete();

        first_request
            .require("first finite response output")
            .expect("the first request completed");
        let error = second_request
            .require("second finite response output")
            .expect_err("the second request must remain incomplete");
        assert!(error.to_string().contains("second finite response output"));
    }

    #[test]
    fn completed_guard_accepts_clean_session_completion() {
        let completion = CompletionGuard::new();
        let application = completion.clone();
        let result = futures::executor::block_on(select_session(
            pending_once_then_ok(),
            std::future::pending(),
            async move {
                application.complete();
                Ok(())
            },
        ));

        assert!(result.is_ok());
        completion
            .require("finite response output")
            .expect("completed response");
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

    struct PendingRead;
    impl futures::io::AsyncRead for PendingRead {
        fn poll_read(
            self: Pin<&mut Self>,
            _: &mut Context<'_>,
            _: &mut [u8],
        ) -> Poll<std::io::Result<usize>> {
            Poll::Pending
        }
    }

    struct OwnedExport(Rc<Cell<bool>>);
    impl Drop for OwnedExport {
        fn drop(&mut self) {
            self.0.set(true);
        }
    }
    impl system_capnp::membrane::Server for OwnedExport {}

    struct PanickingBootstrap;
    impl system_capnp::membrane::Server for PanickingBootstrap {}
    impl Drop for PanickingBootstrap {
        fn drop(&mut self) {
            panic!("injected guest bootstrap destructor panic");
        }
    }

    #[test]
    fn bootstrap_panic_preserves_application_error_and_releases_guest_exports() {
        let released = Rc::new(Cell::new(false));
        let owned: system_capnp::membrane::Client =
            capnp_rpc::new_client(OwnedExport(released.clone()));
        let bootstrap: system_capnp::membrane::Client = capnp_rpc::new_client(PanickingBootstrap);
        let bootstrap = membrane::RpcBootstrap::new(bootstrap.client);
        let network = VatNetwork::new(
            PendingRead,
            futures::io::sink(),
            Side::Client,
            Default::default(),
        );
        let mut rpc = membrane::rpc_system(Box::new(network), Some(bootstrap.client()));
        let remote: system_capnp::executor::Client = rpc.bootstrap(Side::Server);
        let mut request = remote.spawn_request();
        request.get().set_membrane(owned);
        let pending = request.send();
        let disconnect = rpc.get_disconnector();
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            futures::executor::block_on(select_session_with_disconnect(
                std::future::pending(),
                rpc,
                std::future::ready(Err(failed("application failed"))),
                disconnect,
                Some(bootstrap),
            ))
        }));
        let error = result
            .expect("secondary bootstrap panic must not replace the selected error")
            .expect_err("preserve the application failure");
        assert!(error.extra.contains("application failed"));
        assert!(
            released.get(),
            "bootstrap panic must not strand other guest exports"
        );
        use futures::FutureExt;
        assert!(pending
            .promise
            .now_or_never()
            .is_some_and(|result| result.is_err()));
        drop(pending.pipeline);
    }

    #[test]
    fn terminal_rpc_rejects_guest_response_with_retained_pipeline() {
        use capnp::capability::Promise;
        use capnp_rpc::Connection;
        use futures::FutureExt;

        struct TerminalNetwork(VatNetwork<PendingRead>);
        impl capnp_rpc::VatNetwork<Side> for TerminalNetwork {
            fn connect(&mut self, vat: Side) -> Option<Box<dyn Connection<Side>>> {
                self.0.connect(vat)
            }
            fn accept(&mut self) -> Promise<Box<dyn Connection<Side>>, capnp::Error> {
                self.0.accept()
            }
            fn drive_until_shutdown(&mut self) -> Promise<(), capnp::Error> {
                Promise::err(failed("injected terminal network failure"))
            }
        }

        let network = TerminalNetwork(VatNetwork::new(
            PendingRead,
            futures::io::sink(),
            Side::Client,
            Default::default(),
        ));
        let mut rpc = membrane::rpc_system(Box::new(network), None);
        let remote: system_capnp::executor::Client = rpc.bootstrap(Side::Server);
        let pending = remote.cid_request().send();
        let disconnect = rpc.get_disconnector();
        let result = futures::executor::block_on(select_session_with_disconnect(
            std::future::pending(),
            rpc,
            std::future::pending(),
            disconnect,
            None,
        ));
        assert!(result.is_err());
        assert!(
            pending
                .promise
                .now_or_never()
                .is_some_and(|result| result.is_err()),
            "finished guest RPC driver must not strand a retained pipeline's response"
        );
        drop(pending.pipeline);
    }

    #[test]
    fn application_error_releases_exports_despite_retained_import() {
        let released = Rc::new(Cell::new(false));
        let owned: system_capnp::membrane::Client =
            capnp_rpc::new_client(OwnedExport(released.clone()));
        let network = VatNetwork::new(
            PendingRead,
            futures::io::sink(),
            Side::Client,
            Default::default(),
        );
        let mut rpc = RpcSystem::new(Box::new(network), None);
        let remote: system_capnp::executor::Client = rpc.bootstrap(Side::Server);
        let mut request = remote.spawn_request();
        request.get().set_membrane(owned);
        let pending = request.send();
        let disconnect = rpc.get_disconnector();
        let result = futures::executor::block_on(select_session_with_disconnect(
            std::future::pending(),
            rpc,
            std::future::ready(Err(failed("application failed"))),
            disconnect,
            None,
        ));
        assert!(result.is_err());
        assert!(
            released.get(),
            "guest session termination must release its queued export"
        );
        use futures::FutureExt;
        assert!(
            pending
                .promise
                .now_or_never()
                .is_some_and(|result| result.is_err()),
            "pending call must be rejected before the guest driver is dropped"
        );
        assert!(remote
            .cid_request()
            .send()
            .promise
            .now_or_never()
            .is_some_and(|result| result.is_err()));
    }

    #[test]
    fn application_error_clears_bootstrap_despite_retained_import() {
        let released = Rc::new(Cell::new(false));
        let owned: system_capnp::membrane::Client =
            capnp_rpc::new_client(OwnedExport(released.clone()));
        let bootstrap = membrane::RpcBootstrap::new(owned.client);
        let network = VatNetwork::new(
            PendingRead,
            futures::io::sink(),
            Side::Client,
            Default::default(),
        );
        let mut rpc = RpcSystem::new(Box::new(network), Some(bootstrap.client()));
        let remote: system_capnp::executor::Client = rpc.bootstrap(Side::Server);
        let disconnect = rpc.get_disconnector();
        let result = futures::executor::block_on(select_session_with_disconnect(
            std::future::pending(),
            rpc,
            std::future::ready(Err(failed("application failed"))),
            disconnect,
            Some(bootstrap),
        ));
        assert!(result.is_err());
        assert!(
            released.get(),
            "connection-state bootstrap ownership must be cleared"
        );
        use futures::FutureExt;
        assert!(remote
            .cid_request()
            .send()
            .promise
            .now_or_never()
            .is_some_and(|result| result.is_err()));
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
