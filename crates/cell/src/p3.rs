//! Native WASI P3 transport host and its isolated regression harness.

use bytes::BytesMut;
use core::pin::Pin;
use core::task::{Context, Poll};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::sync::oneshot;
use wasmtime::component::{
    Access, Destination, FutureReader, HasData, Linker, Source, StreamConsumer, StreamProducer,
    StreamReader, StreamResult,
};
use wasmtime::StoreContextMut;

pub(crate) mod bindings {
    wasmtime::component::bindgen!({
        path: "wit/p3",
        world: "transport-host",
        imports: { default: store },
        require_store_data_send: true,
    });
}

#[cfg(test)]
mod fixture_bindings {
    wasmtime::component::bindgen!({
        path: "wit/p3",
        world: "host-substrate-test",
        imports: { default: store },
        exports: { default: async },
        require_store_data_send: true,
    });
}

use bindings::wetware::transport::connection::{self, TransportError};

const STREAM_BUFFER_CAPACITY: usize = 64 * 1024;

type BoxAsyncRead = Pin<Box<dyn AsyncRead + Send + Sync + 'static>>;
type BoxAsyncWrite = Pin<Box<dyn AsyncWrite + Send + Sync + 'static>>;

#[derive(Default)]
struct TransportObserver {
    active_adapters: AtomicUsize,
    consumed_bytes: AtomicUsize,
    completed_flushes: AtomicUsize,
}

/// One authority-granted transport endpoint.
///
/// The grant is consumed by the first `wetware:transport/connection.open`
/// call.  It contains no address or socket authority.
pub(crate) struct GrantedTransport {
    endpoint: Option<(BoxAsyncRead, BoxAsyncWrite)>,
    observer: Arc<TransportObserver>,
}

/// Host side of the two independently closable bounded byte directions.
pub struct HostTransport {
    reader: tokio::io::DuplexStream,
    writer: tokio::io::DuplexStream,
}

impl HostTransport {
    pub(crate) fn pair(capacity: usize) -> (Self, GrantedTransport) {
        let (reader, guest_writer) = tokio::io::duplex(capacity);
        let (guest_reader, writer) = tokio::io::duplex(capacity);
        (
            Self { reader, writer },
            GrantedTransport::from_parts(guest_reader, guest_writer),
        )
    }

    pub(crate) fn bounded_pair() -> (Self, GrantedTransport) {
        Self::pair(STREAM_BUFFER_CAPACITY)
    }
}

impl AsyncRead for HostTransport {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buffer: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.reader).poll_read(cx, buffer)
    }
}

impl AsyncWrite for HostTransport {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        bytes: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        Pin::new(&mut self.writer).poll_write(cx, bytes)
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.writer).poll_flush(cx)
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.writer).poll_shutdown(cx)
    }
}

impl GrantedTransport {
    pub(crate) fn from_parts(
        reader: impl AsyncRead + Send + Sync + 'static,
        writer: impl AsyncWrite + Send + Sync + 'static,
    ) -> Self {
        Self {
            endpoint: Some((Box::pin(reader), Box::pin(writer))),
            observer: Arc::new(TransportObserver::default()),
        }
    }

    #[cfg(test)]
    fn observer(&self) -> Arc<TransportObserver> {
        Arc::clone(&self.observer)
    }
}

pub(crate) trait TransportHostState: Send + 'static {
    fn granted_transport(&mut self) -> &mut GrantedTransport;
}

struct Transport;

struct TransportView<'a> {
    grant: &'a mut GrantedTransport,
}

impl HasData for Transport {
    type Data<'a> = TransportView<'a>;
}

fn transport<T: TransportHostState>(state: &mut T) -> TransportView<'_> {
    TransportView {
        grant: state.granted_transport(),
    }
}

impl connection::Host for TransportView<'_> {}

#[derive(Clone, Copy)]
struct TransportFailure(&'static str);

struct DirectionCompletion {
    sender: Option<oneshot::Sender<Result<(), TransportFailure>>>,
    observer: Arc<TransportObserver>,
}

impl DirectionCompletion {
    fn new(
        sender: oneshot::Sender<Result<(), TransportFailure>>,
        observer: Arc<TransportObserver>,
    ) -> Self {
        observer.active_adapters.fetch_add(1, Ordering::Relaxed);
        Self {
            sender: Some(sender),
            observer,
        }
    }

    fn finish(&mut self, result: Result<(), TransportFailure>) {
        if let Some(sender) = self.sender.take() {
            let _ = sender.send(result);
        }
    }
}

impl Drop for DirectionCompletion {
    fn drop(&mut self) {
        self.finish(Ok(()));
        self.observer
            .active_adapters
            .fetch_sub(1, Ordering::Relaxed);
    }
}

struct InputProducer {
    reader: BoxAsyncRead,
    completion: DirectionCompletion,
}

enum InputReadResult {
    Bytes(usize),
    Closed,
}

impl InputProducer {
    fn poll_read(&mut self, cx: &mut Context<'_>, bytes: &mut [u8]) -> Poll<InputReadResult> {
        let mut buffer = ReadBuf::new(bytes);
        match self.reader.as_mut().poll_read(cx, &mut buffer) {
            Poll::Ready(Ok(())) if buffer.filled().is_empty() => {
                self.completion.finish(Ok(()));
                Poll::Ready(InputReadResult::Closed)
            }
            Poll::Ready(Ok(())) => Poll::Ready(InputReadResult::Bytes(buffer.filled().len())),
            Poll::Ready(Err(_)) => {
                self.completion
                    .finish(Err(TransportFailure("transport read failed")));
                Poll::Ready(InputReadResult::Closed)
            }
            Poll::Pending => Poll::Pending,
        }
    }
}

impl<D> StreamProducer<D> for InputProducer {
    type Item = u8;
    type Buffer = BytesMut;

    fn poll_produce<'a>(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        mut store: StoreContextMut<'a, D>,
        destination: Destination<'a, u8, BytesMut>,
        finish: bool,
    ) -> Poll<wasmtime::Result<StreamResult>> {
        if destination.remaining(&mut store) == Some(0) {
            return Poll::Ready(Ok(StreamResult::Completed));
        }

        let mut destination = destination.as_direct(store, STREAM_BUFFER_CAPACITY);
        match self.poll_read(cx, destination.remaining()) {
            Poll::Ready(InputReadResult::Bytes(count)) => {
                destination.mark_written(count);
                Poll::Ready(Ok(StreamResult::Completed))
            }
            Poll::Ready(InputReadResult::Closed) => Poll::Ready(Ok(StreamResult::Dropped)),
            Poll::Pending if finish => Poll::Ready(Ok(StreamResult::Cancelled)),
            Poll::Pending => Poll::Pending,
        }
    }
}

struct OutputConsumer {
    writer: BoxAsyncWrite,
    completion: DirectionCompletion,
    flush_pending: bool,
}

impl Drop for OutputConsumer {
    fn drop(&mut self) {
        // Policy A: cancellation after the host accepts bytes, but before the
        // required flush completes, fails the whole connection. Reporting an
        // orderly close here could acknowledge a final message that was lost.
        if self.flush_pending && self.completion.sender.is_some() {
            self.completion
                .finish(Err(TransportFailure("transport flush cancelled")));
        }
    }
}

impl OutputConsumer {
    fn failed(&mut self, message: &'static str) -> Poll<wasmtime::Result<StreamResult>> {
        self.completion.finish(Err(TransportFailure(message)));
        Poll::Ready(Ok(StreamResult::Dropped))
    }

    fn poll_flush(
        &mut self,
        cx: &mut Context<'_>,
        finish: bool,
    ) -> Poll<wasmtime::Result<StreamResult>> {
        match self.writer.as_mut().poll_flush(cx) {
            Poll::Ready(Ok(())) => {
                self.flush_pending = false;
                self.completion
                    .observer
                    .completed_flushes
                    .fetch_add(1, Ordering::Relaxed);
                Poll::Ready(Ok(StreamResult::Completed))
            }
            Poll::Ready(Err(_)) => self.failed("transport flush failed"),
            Poll::Pending if finish => self.failed("transport flush cancelled"),
            Poll::Pending => {
                self.flush_pending = true;
                Poll::Pending
            }
        }
    }
}

impl<D: 'static> StreamConsumer<D> for OutputConsumer {
    type Item = u8;

    fn poll_consume(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        store: StoreContextMut<'_, D>,
        source: Source<'_, u8>,
        finish: bool,
    ) -> Poll<wasmtime::Result<StreamResult>> {
        if self.flush_pending {
            return self.poll_flush(cx, finish);
        }

        let mut source = source.as_direct(store);
        let bytes = source.remaining();
        if bytes.is_empty() {
            return Poll::Ready(Ok(StreamResult::Completed));
        }

        match self.writer.as_mut().poll_write(cx, bytes) {
            Poll::Ready(Ok(0)) => self.failed("transport write failed"),
            Poll::Ready(Ok(count)) => {
                source.mark_read(count);
                self.completion
                    .observer
                    .consumed_bytes
                    .fetch_add(count, Ordering::Relaxed);
                self.poll_flush(cx, finish)
            }
            Poll::Ready(Err(_)) => self.failed("transport write failed"),
            Poll::Pending if finish => Poll::Ready(Ok(StreamResult::Cancelled)),
            Poll::Pending => Poll::Pending,
        }
    }
}

fn completion_result(
    result: Result<Result<(), TransportFailure>, oneshot::error::RecvError>,
) -> Result<(), TransportError> {
    match result {
        Ok(Ok(())) => Ok(()),
        Ok(Err(TransportFailure(message))) => Err(TransportError::Failed(message.to_string())),
        Err(_) => Err(TransportError::Failed(
            "transport adapter stopped".to_string(),
        )),
    }
}

async fn await_connection(
    outgoing: oneshot::Receiver<Result<(), TransportFailure>>,
    incoming: oneshot::Receiver<Result<(), TransportFailure>>,
) -> Result<(), TransportError> {
    tokio::pin!(outgoing);
    tokio::pin!(incoming);

    tokio::select! {
        result = &mut outgoing => {
            completion_result(result)?;
            completion_result(incoming.await)
        }
        result = &mut incoming => {
            completion_result(result)?;
            completion_result(outgoing.await)
        }
    }
}

impl<T: TransportHostState> connection::HostWithStore<T> for Transport {
    fn open(
        mut store: Access<'_, T, Self>,
        mut outgoing: StreamReader<u8>,
    ) -> (StreamReader<u8>, FutureReader<Result<(), TransportError>>) {
        let grant = store.get().grant;
        let observer = Arc::clone(&grant.observer);
        let Some((reader, writer)) = grant.endpoint.take() else {
            outgoing
                .close(&mut store)
                .expect("close rejected outgoing stream");
            let incoming = StreamReader::new(&mut store, std::iter::empty())
                .expect("create rejected incoming stream");
            let completion = FutureReader::new(&mut store, async {
                wasmtime::error::Ok(Err(TransportError::Failed(
                    "connection already opened".to_string(),
                )))
            })
            .expect("create rejected connection completion");
            return (incoming, completion);
        };

        let (outgoing_tx, outgoing_rx) = oneshot::channel();
        let (incoming_tx, incoming_rx) = oneshot::channel();
        outgoing
            .pipe(
                &mut store,
                OutputConsumer {
                    writer,
                    completion: DirectionCompletion::new(outgoing_tx, Arc::clone(&observer)),
                    flush_pending: false,
                },
            )
            .expect("attach P3 transport output consumer");

        let incoming = StreamReader::new(
            &mut store,
            InputProducer {
                reader,
                completion: DirectionCompletion::new(incoming_tx, observer),
            },
        )
        .expect("create P3 transport input stream");
        let completion = FutureReader::new(&mut store, async move {
            wasmtime::error::Ok(await_connection(outgoing_rx, incoming_rx).await)
        })
        .expect("create P3 transport completion future");

        (incoming, completion)
    }
}

pub(crate) fn add_transport_to_linker<T: TransportHostState>(
    linker: &mut Linker<T>,
) -> wasmtime::Result<()> {
    connection::add_to_linker::<T, Transport>(linker, transport::<T>)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fs_intercept::{
        override_p3_filesystem_linker, FilesystemHostState, IpfsFilesystemView,
    };
    use crate::vfs::{CidTree, DirEntry, EntryType};
    use anyhow::{Context as _, Result};
    use async_trait::async_trait;
    use cache::{CacheMode, IsolatedPinset, Pinner};
    use cid::Cid;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicBool, AtomicUsize};
    use std::sync::Mutex;
    use std::task::Waker;
    use std::time::Duration;
    use tempfile::TempDir;
    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
    use wasmtime::component::{Component, ResourceTable};
    use wasmtime::{Config, Engine, Store};
    use wasmtime_wasi::filesystem::{WasiFilesystem, WasiFilesystemCtxView};
    use wasmtime_wasi::{FsPerms, WasiCtx, WasiCtxBuilder, WasiCtxView, WasiView};

    const FIXTURE_ENV: &str = "WW_NATIVE_P3_FIXTURE";
    const TEST_CID: &str = "QmYwAPJzv5CZsnN625s3Xf2nemtYgPpHdWEz79ojWnPbdG";
    const TEST_TIMEOUT: Duration = Duration::from_secs(10);

    struct HarnessState {
        wasi: WasiCtx,
        table: ResourceTable,
        transport: GrantedTransport,
        host_call_frames: crate::proc::HostCallFrames,
        host_call_entries: Arc<AtomicUsize>,
        host_call_returns: Arc<AtomicUsize>,
        peak_host_call_depth: Arc<AtomicUsize>,
        cache_mode: Option<Arc<CacheMode>>,
        cid_tree: Option<Arc<CidTree>>,
        writable_descriptors: std::collections::HashSet<u32>,
        _image_root: Option<TempDir>,
        _tree_staging: Option<TempDir>,
        _scratch: TempDir,
    }

    impl WasiView for HarnessState {
        fn ctx(&mut self) -> WasiCtxView<'_> {
            self.host_call_frames.mark();
            WasiCtxView {
                ctx: &mut self.wasi,
                table: &mut self.table,
            }
        }
    }

    impl TransportHostState for HarnessState {
        fn granted_transport(&mut self) -> &mut GrantedTransport {
            self.host_call_frames.mark();
            &mut self.transport
        }
    }

    fn harness_wasi_filesystem(state: &mut HarnessState) -> WasiFilesystemCtxView<'_> {
        state.host_call_frames.mark();
        WasiFilesystemCtxView {
            ctx: state.wasi.filesystem(),
            table: &mut state.table,
        }
    }

    impl FilesystemHostState for HarnessState {
        fn intercepted_filesystem(&mut self) -> IpfsFilesystemView<'_> {
            self.host_call_frames.mark();
            IpfsFilesystemView {
                ctx: self.wasi.filesystem(),
                table: &mut self.table,
                cache_mode: &self.cache_mode,
                cid_tree: &self.cid_tree,
                writable_descriptors: &mut self.writable_descriptors,
            }
        }

        fn wasi_filesystem_getter(
        ) -> for<'a> fn(&'a mut Self) -> <WasiFilesystem as wasmtime::component::HasData>::Data<'a>
        {
            harness_wasi_filesystem
        }

        fn wasi_filesystem_access<'a>(
            store: wasmtime::StoreContextMut<'a, Self>,
        ) -> Access<'a, Self, WasiFilesystem> {
            Access::new(store, |state| harness_wasi_filesystem(state))
        }
    }

    fn transport_state(transport: GrantedTransport) -> Result<HarnessState> {
        let image_root = TempDir::new()?;
        let scratch = TempDir::new()?;
        let mut builder = WasiCtxBuilder::new();
        builder
            .preopened_dir(image_root.path(), "/", FsPerms::ReadOnly)?
            .preopened_dir(scratch.path(), "/tmp", FsPerms::ReadWrite)?;
        Ok(HarnessState {
            wasi: builder.build(),
            table: ResourceTable::new(),
            transport,
            host_call_frames: crate::proc::HostCallFrames::default(),
            host_call_entries: Arc::new(AtomicUsize::new(0)),
            host_call_returns: Arc::new(AtomicUsize::new(0)),
            peak_host_call_depth: Arc::new(AtomicUsize::new(0)),
            cache_mode: None,
            cid_tree: None,
            writable_descriptors: std::collections::HashSet::new(),
            _image_root: Some(image_root),
            _tree_staging: None,
            _scratch: scratch,
        })
    }

    async fn instantiate(
        state: HarnessState,
    ) -> Result<(Store<HarnessState>, fixture_bindings::HostSubstrateTest)> {
        let artifact = std::env::var_os(FIXTURE_ENV)
            .map(PathBuf::from)
            .context("WW_NATIVE_P3_FIXTURE must name the validated native P3 component")?;
        let mut config = Config::new();
        config.wasm_component_model_async(true);
        config.wasm_component_model_threading(true);
        let engine = Engine::new(&config)?;
        let component = Component::from_file(&engine, artifact)?;
        let mut linker = Linker::new(&engine);

        wasmtime_wasi::p3::cli::add_to_linker(&mut linker)?;
        wasmtime_wasi::p3::clocks::add_to_linker(&mut linker)?;
        wasmtime_wasi::p3::filesystem::add_to_linker(&mut linker)?;
        override_p3_filesystem_linker(&mut linker)?;
        add_transport_to_linker(&mut linker)?;

        let mut store = Store::new(&engine, state);
        store.call_hook(|mut context, hook| {
            if matches!(hook, wasmtime::CallHook::CallingHost) {
                context
                    .data()
                    .host_call_entries
                    .fetch_add(1, Ordering::Release);
            }
            if context.data_mut().host_call_frames.on_call_hook(hook) {
                context
                    .data()
                    .host_call_returns
                    .fetch_add(1, Ordering::Release);
            }
            let depth = context.data().host_call_frames.depth();
            context
                .data()
                .peak_host_call_depth
                .fetch_max(depth, Ordering::Release);
            Ok(())
        });
        let instance =
            fixture_bindings::HostSubstrateTest::instantiate_async(&mut store, &component, &linker)
                .await?;
        Ok((store, instance))
    }

    async fn call_exchange(
        store: &mut Store<HarnessState>,
        instance: fixture_bindings::HostSubstrateTest,
        payload: Vec<u8>,
    ) -> Result<Vec<u8>> {
        Ok(store
            .run_concurrent(async move |access| {
                instance
                    .wetware_transport_fixture()
                    .call_exchange(access, payload)
                    .await
            })
            .await??)
    }

    #[tokio::test(flavor = "multi_thread")]
    #[ignore = "requires the validated native P3 fixture artifact"]
    async fn p3_transport_ordering_final_write_and_half_close() -> Result<()> {
        let request = b"guest request in order".to_vec();
        let response = b"host response after guest EOF".to_vec();
        let (mut host, grant) = HostTransport::bounded_pair();
        let (mut store, instance) = instantiate(transport_state(grant)?).await?;

        let guest = call_exchange(&mut store, instance, request.clone());
        let host_io = async {
            let mut received = Vec::new();
            host.read_to_end(&mut received).await?;
            anyhow::ensure!(received == request, "guest-to-host byte order changed");
            host.write_all(&response).await?;
            host.shutdown().await?;
            Result::<()>::Ok(())
        };
        let (returned, ()) =
            tokio::time::timeout(TEST_TIMEOUT, async { tokio::try_join!(guest, host_io) })
                .await??;
        assert_eq!(returned, response);
        store.assert_concurrent_state_empty();

        let ack = b"guest ack after host EOF".to_vec();
        let expected_ack = ack.clone();
        let host_message = b"host closes only its write direction".to_vec();
        let (mut host, grant) = HostTransport::bounded_pair();
        let (mut store, instance) = instantiate(transport_state(grant)?).await?;
        let guest = async {
            Ok::<Vec<u8>, anyhow::Error>(
                store
                    .run_concurrent(async move |access| {
                        instance
                            .wetware_transport_fixture()
                            .call_receive_then_send(access, ack)
                            .await
                    })
                    .await??,
            )
        };
        let host_io = async {
            host.write_all(&host_message).await?;
            host.shutdown().await?;
            let mut received = Vec::new();
            host.read_to_end(&mut received).await?;
            Result::<Vec<u8>>::Ok(received)
        };
        let (returned, received) =
            tokio::time::timeout(TEST_TIMEOUT, async { tokio::try_join!(guest, host_io) })
                .await??;
        assert_eq!(returned, host_message);
        assert_eq!(received, expected_ack);
        store.assert_concurrent_state_empty();
        Ok(())
    }

    #[tokio::test(flavor = "multi_thread")]
    #[ignore = "requires the validated native P3 fixture artifact"]
    async fn p3_transport_backpressure_is_bounded_and_ordered() -> Result<()> {
        let payload: Vec<u8> = (0..(256 * 1024)).map(|index| index as u8).collect();
        let (mut host, grant) = HostTransport::bounded_pair();
        let observer = grant.observer();
        let (mut store, instance) = instantiate(transport_state(grant)?).await?;
        let expected = payload.clone();
        let mut guest = tokio::spawn(async move {
            let result: Result<()> = async {
                store
                    .run_concurrent(async move |access| {
                        instance
                            .wetware_transport_fixture()
                            .call_send(access, payload)
                            .await
                    })
                    .await??;
                Ok(())
            }
            .await;
            (store, result)
        });

        tokio::time::timeout(TEST_TIMEOUT, async {
            while observer.consumed_bytes.load(Ordering::Acquire) < STREAM_BUFFER_CAPACITY {
                tokio::task::yield_now().await;
            }
        })
        .await?;
        assert!(
            !guest.is_finished(),
            "writer completed while 256 KiB exceeded the unread 64 KiB transport"
        );
        let mut received = Vec::new();
        tokio::time::timeout(TEST_TIMEOUT, host.read_to_end(&mut received)).await??;
        let (mut store, result) = tokio::time::timeout(TEST_TIMEOUT, &mut guest).await??;
        result?;
        assert_eq!(received, expected);
        store.assert_concurrent_state_empty();
        Ok(())
    }

    struct FlushGate<W> {
        writer: W,
        open: Arc<AtomicBool>,
        pending_polls: Arc<AtomicUsize>,
        waker: Arc<Mutex<Option<Waker>>>,
    }

    impl<W: AsyncWrite + Unpin> AsyncWrite for FlushGate<W> {
        fn poll_write(
            mut self: Pin<&mut Self>,
            cx: &mut Context<'_>,
            bytes: &[u8],
        ) -> Poll<std::io::Result<usize>> {
            Pin::new(&mut self.writer).poll_write(cx, bytes)
        }

        fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
            if !self.open.load(Ordering::Acquire) {
                *self.waker.lock().expect("flush waker mutex") = Some(cx.waker().clone());
                self.pending_polls.fetch_add(1, Ordering::Release);
                return Poll::Pending;
            }
            Pin::new(&mut self.writer).poll_flush(cx)
        }

        fn poll_shutdown(
            mut self: Pin<&mut Self>,
            cx: &mut Context<'_>,
        ) -> Poll<std::io::Result<()>> {
            Pin::new(&mut self.writer).poll_shutdown(cx)
        }
    }

    struct ReadFailure;

    impl AsyncRead for ReadFailure {
        fn poll_read(
            self: Pin<&mut Self>,
            _cx: &mut Context<'_>,
            _buffer: &mut ReadBuf<'_>,
        ) -> Poll<std::io::Result<()>> {
            Poll::Ready(Err(std::io::Error::other(
                "sensitive reader path: /not/guest/visible",
            )))
        }
    }

    struct FlushFailure;

    impl AsyncWrite for FlushFailure {
        fn poll_write(
            self: Pin<&mut Self>,
            _cx: &mut Context<'_>,
            _bytes: &[u8],
        ) -> Poll<std::io::Result<usize>> {
            Poll::Ready(Ok(0))
        }

        fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
            Poll::Ready(Err(std::io::Error::other(
                "sensitive writer path: /not/guest/visible",
            )))
        }

        fn poll_shutdown(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
            Poll::Ready(Ok(()))
        }
    }

    #[tokio::test]
    async fn input_producer_read_failure_is_sanitized() {
        let observer = Arc::new(TransportObserver::default());
        let (completion_tx, completion_rx) = oneshot::channel();
        let mut producer = InputProducer {
            reader: Box::pin(ReadFailure),
            completion: DirectionCompletion::new(completion_tx, observer),
        };
        let mut cx = Context::from_waker(futures::task::noop_waker_ref());
        let mut bytes = [0; 1];

        assert!(matches!(
            producer.poll_read(&mut cx, &mut bytes),
            Poll::Ready(InputReadResult::Closed)
        ));
        let error = tokio::time::timeout(TEST_TIMEOUT, completion_rx)
            .await
            .expect("read failure completion hung");
        match completion_result(error) {
            Err(TransportError::Failed(message)) => {
                assert_eq!(message, "transport read failed");
            }
            Ok(()) => panic!("read failure reported orderly completion"),
        }
    }

    #[tokio::test]
    async fn output_consumer_flush_failure_is_sanitized() {
        let observer = Arc::new(TransportObserver::default());
        let (completion_tx, completion_rx) = oneshot::channel();
        let mut consumer = OutputConsumer {
            writer: Box::pin(FlushFailure),
            completion: DirectionCompletion::new(completion_tx, observer),
            flush_pending: false,
        };
        let mut cx = Context::from_waker(futures::task::noop_waker_ref());

        assert!(matches!(
            consumer.poll_flush(&mut cx, false),
            Poll::Ready(Ok(StreamResult::Dropped))
        ));
        let error = tokio::time::timeout(TEST_TIMEOUT, completion_rx)
            .await
            .expect("flush failure completion hung");
        match completion_result(error) {
            Err(TransportError::Failed(message)) => {
                assert_eq!(message, "transport flush failed");
            }
            Ok(()) => panic!("flush failure reported orderly completion"),
        }
    }

    #[tokio::test]
    async fn pending_flush_cancellation_fails_the_connection() {
        let (_reader, writer) = tokio::io::duplex(STREAM_BUFFER_CAPACITY);
        let flush_open = Arc::new(AtomicBool::new(false));
        let pending_flush_polls = Arc::new(AtomicUsize::new(0));
        let flush_waker = Arc::new(Mutex::new(None::<Waker>));
        let observer = Arc::new(TransportObserver::default());
        let (completion_tx, completion_rx) = oneshot::channel();
        let mut consumer = OutputConsumer {
            writer: Box::pin(FlushGate {
                writer,
                open: flush_open,
                pending_polls: Arc::clone(&pending_flush_polls),
                waker: flush_waker,
            }),
            completion: DirectionCompletion::new(completion_tx, observer),
            flush_pending: false,
        };
        let mut cx = Context::from_waker(futures::task::noop_waker_ref());

        assert!(matches!(consumer.poll_flush(&mut cx, false), Poll::Pending));
        assert_eq!(pending_flush_polls.load(Ordering::Acquire), 1);
        assert!(matches!(
            consumer.poll_flush(&mut cx, true),
            Poll::Ready(Ok(StreamResult::Dropped))
        ));
        let error = completion_result(completion_rx.await)
            .expect_err("pending flush cancellation must fail the connection");
        match error {
            TransportError::Failed(message) => {
                assert_eq!(message, "transport flush cancelled");
            }
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    #[ignore = "requires the validated native P3 fixture artifact"]
    async fn p3_transport_completion_waits_for_underlying_flush() -> Result<()> {
        let payload = b"final message before immediate guest completion".to_vec();
        let expected = payload.clone();
        let (mut host_reader, guest_writer) = tokio::io::duplex(STREAM_BUFFER_CAPACITY);
        let (guest_reader, host_writer) = tokio::io::duplex(STREAM_BUFFER_CAPACITY);
        let flush_open = Arc::new(AtomicBool::new(false));
        let pending_flush_polls = Arc::new(AtomicUsize::new(0));
        let flush_waker = Arc::new(Mutex::new(None::<Waker>));
        let grant = GrantedTransport::from_parts(
            guest_reader,
            FlushGate {
                writer: guest_writer,
                open: Arc::clone(&flush_open),
                pending_polls: Arc::clone(&pending_flush_polls),
                waker: Arc::clone(&flush_waker),
            },
        );
        let observer = grant.observer();
        let (mut store, instance) = instantiate(transport_state(grant)?).await?;
        drop(host_writer);
        let mut guest = tokio::spawn(async move {
            let result: Result<()> = async {
                store
                    .run_concurrent(async move |access| {
                        instance
                            .wetware_transport_fixture()
                            .call_send(access, payload)
                            .await
                    })
                    .await??;
                Ok(())
            }
            .await;
            (store, result)
        });

        let mut received = vec![0; expected.len()];
        tokio::time::timeout(TEST_TIMEOUT, host_reader.read_exact(&mut received)).await??;
        assert_eq!(received, expected);
        tokio::time::timeout(TEST_TIMEOUT, async {
            while pending_flush_polls.load(Ordering::Acquire) == 0 {
                tokio::task::yield_now().await;
            }
        })
        .await?;
        assert!(
            !guest.is_finished(),
            "guest write completed before host flush"
        );
        assert_eq!(observer.completed_flushes.load(Ordering::Acquire), 0);

        flush_open.store(true, Ordering::Release);
        flush_waker
            .lock()
            .expect("flush waker mutex")
            .take()
            .expect("pending flush registered no waker")
            .wake();
        let (mut store, result) = tokio::time::timeout(TEST_TIMEOUT, &mut guest).await??;
        result?;
        assert_eq!(observer.completed_flushes.load(Ordering::Acquire), 1);
        store.assert_concurrent_state_empty();
        Ok(())
    }

    #[tokio::test(flavor = "multi_thread")]
    #[ignore = "requires the validated native P3 fixture artifact"]
    async fn p3_transport_failure_and_single_open_are_structured() -> Result<()> {
        let (mut host, grant) = HostTransport::bounded_pair();
        let (mut store, instance) = instantiate(transport_state(grant)?).await?;
        let orderly = async {
            Ok::<bool, anyhow::Error>(
                store
                    .run_concurrent(async move |access| {
                        instance
                            .wetware_transport_fixture()
                            .call_orderly_close(access)
                            .await
                    })
                    .await??,
            )
        };
        let host_close = async {
            host.shutdown().await?;
            let mut outgoing = Vec::new();
            host.read_to_end(&mut outgoing).await?;
            anyhow::ensure!(outgoing.is_empty());
            Result::<()>::Ok(())
        };
        let (orderly, ()) = tokio::time::timeout(TEST_TIMEOUT, async {
            tokio::try_join!(orderly, host_close)
        })
        .await??;
        assert!(orderly);
        store.assert_concurrent_state_empty();

        let outgoing = b"outgoing survives incoming drop".to_vec();
        let expected = outgoing.clone();
        let (mut host, grant) = HostTransport::bounded_pair();
        let (mut store, instance) = instantiate(transport_state(grant)?).await?;
        let guest = async {
            Ok::<bool, anyhow::Error>(
                store
                    .run_concurrent(async move |access| {
                        instance
                            .wetware_transport_fixture()
                            .call_drop_incoming(access, outgoing)
                            .await
                    })
                    .await??,
            )
        };
        let host_io = async {
            let mut received = Vec::new();
            host.read_to_end(&mut received).await?;
            anyhow::ensure!(received == expected);
            let error = host
                .write_all(b"guest no longer consumes")
                .await
                .expect_err("host write must fail after guest drops incoming");
            anyhow::ensure!(
                matches!(
                    error.kind(),
                    std::io::ErrorKind::BrokenPipe | std::io::ErrorKind::ConnectionReset
                ),
                "unexpected half-close error: {error}"
            );
            Result::<()>::Ok(())
        };
        let (orderly, ()) =
            tokio::time::timeout(TEST_TIMEOUT, async { tokio::try_join!(guest, host_io) })
                .await??;
        assert!(orderly, "incoming stream drop was reported as a failure");
        store.assert_concurrent_state_empty();

        let (host, grant) = HostTransport::bounded_pair();
        drop(host);
        let (mut store, instance) = instantiate(transport_state(grant)?).await?;
        let failure = store
            .run_concurrent(async move |access| {
                instance
                    .wetware_transport_fixture()
                    .call_abnormal_failure(access)
                    .await
            })
            .await??;
        assert_eq!(failure, "transport write failed");
        store.assert_concurrent_state_empty();

        let (_host, grant) = HostTransport::bounded_pair();
        let (mut store, instance) = instantiate(transport_state(grant)?).await?;
        let failure = store
            .run_concurrent(async move |access| {
                instance
                    .wetware_transport_fixture()
                    .call_second_open(access)
                    .await
            })
            .await??;
        assert_eq!(failure, "connection already opened");
        store.assert_concurrent_state_empty();
        Ok(())
    }

    #[tokio::test(flavor = "multi_thread")]
    #[ignore = "requires the validated native P3 fixture artifact"]
    async fn p3_run_concurrent_owner_abort_drops_transport_without_hang() -> Result<()> {
        let (mut host, grant) = HostTransport::bounded_pair();
        let observer = grant.observer();
        let state = transport_state(grant)?;
        let host_call_entries = Arc::clone(&state.host_call_entries);
        let host_call_returns = Arc::clone(&state.host_call_returns);
        let peak_host_call_depth = Arc::clone(&state.peak_host_call_depth);
        let (mut store, instance) = instantiate(state).await?;
        let initial_host_call_entries = host_call_entries.load(Ordering::Acquire);
        let initial_host_call_returns = host_call_returns.load(Ordering::Acquire);
        // Wasmtime 48 only hard-cancels a concurrent guest task by dropping
        // its Store. Aborting this owner task drops run_concurrent and Store
        // together without adding a guest resource-order workaround.
        let task = tokio::spawn(async move {
            store
                .run_concurrent(async move |access| {
                    instance
                        .wetware_transport_fixture()
                        .call_wait_on_live_resources(access)
                        .await
                })
                .await
        });

        tokio::time::timeout(TEST_TIMEOUT, async {
            while observer.active_adapters.load(Ordering::Acquire) != 2
                || observer.consumed_bytes.load(Ordering::Acquire) < STREAM_BUFFER_CAPACITY
                || host_call_entries.load(Ordering::Acquire) < initial_host_call_entries + 2
                || host_call_returns.load(Ordering::Acquire) < initial_host_call_returns + 1
            {
                tokio::task::yield_now().await;
            }
        })
        .await?;
        // Wasmtime 48 brackets each synchronous VM-to-host transition before
        // it returns control to the concurrent task scheduler. The transport
        // and clock operations overlap, while their CallHook frames do not.
        assert_eq!(peak_host_call_depth.load(Ordering::Acquire), 1);
        assert!(!task.is_finished(), "owner task finished before abort");
        task.abort();
        let cancelled = tokio::time::timeout(TEST_TIMEOUT, task).await?;
        assert!(cancelled
            .expect_err("aborted P3 Store owner task must stop")
            .is_cancelled());
        tokio::time::timeout(TEST_TIMEOUT, async {
            while observer.active_adapters.load(Ordering::Acquire) != 0 {
                tokio::task::yield_now().await;
            }
        })
        .await?;
        let mut remainder = Vec::new();
        tokio::time::timeout(TEST_TIMEOUT, host.read_to_end(&mut remainder)).await??;
        Ok(())
    }

    struct CountingPinner {
        bytes: Vec<u8>,
        fetches: AtomicUsize,
    }

    #[async_trait]
    impl Pinner for CountingPinner {
        async fn pin(&self, _cid: &Cid) -> Result<()> {
            Ok(())
        }

        async fn unpin(&self, _cid: &Cid) -> Result<()> {
            Ok(())
        }

        async fn fetch(&self, _cid: &Cid) -> Result<Vec<u8>> {
            self.fetches.fetch_add(1, Ordering::AcqRel);
            Ok(self.bytes.clone())
        }

        async fn size(&self, _cid: &Cid) -> Result<u64> {
            Ok(self.bytes.len() as u64)
        }
    }

    struct FilesystemPaths {
        scratch: PathBuf,
        tree_staging: PathBuf,
        cache_staging: PathBuf,
        materialized: PathBuf,
    }

    fn filesystem_state(
        transport: GrantedTransport,
        pinner: Arc<CountingPinner>,
    ) -> Result<(HarnessState, FilesystemPaths)> {
        let tree_staging = TempDir::new()?;
        let scratch = TempDir::new()?;
        let cid: Cid = TEST_CID.parse()?;
        let entries = vec![DirEntry {
            name: "known.txt".to_string(),
            cid: cid.to_string(),
            entry_type: EntryType::File,
            size: pinner.bytes.len() as u64,
        }];
        std::fs::write(
            tree_staging.path().join(format!("{cid}.dirlist.json")),
            serde_json::to_vec(&entries)?,
        )?;
        let tree = Arc::new(CidTree::new(
            cid.to_string(),
            ipfs::HttpClient::new("http://127.0.0.1:1".to_string()),
            tree_staging.path().to_path_buf(),
        ));
        let cache = Arc::new(CacheMode::Isolated(IsolatedPinset::new(pinner)?));
        let cache_staging = cache.staging_dir().to_path_buf();
        let paths = FilesystemPaths {
            scratch: scratch.path().to_path_buf(),
            tree_staging: tree_staging.path().to_path_buf(),
            materialized: cache_staging.join(cid.to_string()),
            cache_staging,
        };
        let mut builder = WasiCtxBuilder::new();
        builder
            .preopened_dir(tree.staging_dir(), "/", FsPerms::ReadOnly)?
            .preopened_dir(scratch.path(), "/tmp", FsPerms::ReadWrite)?;
        Ok((
            HarnessState {
                wasi: builder.build(),
                table: ResourceTable::new(),
                transport,
                host_call_frames: crate::proc::HostCallFrames::default(),
                host_call_entries: Arc::new(AtomicUsize::new(0)),
                host_call_returns: Arc::new(AtomicUsize::new(0)),
                peak_host_call_depth: Arc::new(AtomicUsize::new(0)),
                cache_mode: Some(cache),
                cid_tree: Some(tree),
                writable_descriptors: std::collections::HashSet::new(),
                _image_root: None,
                _tree_staging: Some(tree_staging),
                _scratch: scratch,
            },
            paths,
        ))
    }

    #[tokio::test(flavor = "multi_thread")]
    #[ignore = "requires the validated native P3 fixture artifact"]
    async fn p3_filesystem_matches_cid_tree_policy_and_cleans_up() -> Result<()> {
        let (_host, grant) = HostTransport::bounded_pair();
        let pinner = Arc::new(CountingPinner {
            bytes: b"lazy image bytes".to_vec(),
            fetches: AtomicUsize::new(0),
        });
        let (state, paths) = filesystem_state(grant, Arc::clone(&pinner))?;
        assert!(!paths.materialized.exists());
        assert_eq!(pinner.fetches.load(Ordering::Acquire), 0);
        let (mut store, instance) = instantiate(state).await?;
        let observed = store
            .run_concurrent(async move |access| {
                instance
                    .wetware_transport_fixture()
                    .call_filesystem(access)
                    .await
            })
            .await??;
        assert_eq!(observed.image, b"lazy image bytes");
        assert!(observed.missing_rejected);
        assert!(observed.traversal_rejected);
        assert!(observed.image_write_rejected);
        assert_eq!(observed.scratch, b"scratch-data");
        assert_eq!(pinner.fetches.load(Ordering::Acquire), 1);
        assert_eq!(std::fs::read(&paths.materialized)?, b"lazy image bytes");
        assert_eq!(
            std::fs::read(paths.scratch.join("probe.txt"))?,
            b"scratch-data"
        );
        drop(store);
        assert!(!paths.scratch.exists());
        assert!(!paths.cache_staging.exists());
        assert!(!paths.tree_staging.exists());
        Ok(())
    }

    #[test]
    fn p3_fixture_declares_no_socket_authority() {
        let source = include_str!("../wit/p3/fixture.wit");
        assert!(!source.contains("wasi:sockets"));
        assert!(!source.contains("pollable"));
        assert!(!source.contains("subscribe"));
    }

    #[test]
    fn p3_transport_errors_are_fixed_and_sanitized() {
        for message in [
            "transport read failed",
            "transport write failed",
            "transport flush failed",
            "transport flush cancelled",
            "transport adapter stopped",
            "connection already opened",
        ] {
            assert!(!message.contains('/'));
            assert!(!message.contains(':'));
            assert!(!message.contains("::"));
        }
    }
}
