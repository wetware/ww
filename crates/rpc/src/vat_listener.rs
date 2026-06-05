//! VatListener capability: guest-exported subprotocols via Cap'n Proto RPC.
//!
//! `VatListener.listen(executor, protocol, caps)` registers a caller-chosen
//! service locator at `/ww/0.1.0/vat/{protocol}`. The locator is not type
//! authority. The trusted host derives schema/artifact metadata from the
//! host-minted `Runtime.load()` executor and serves a `VatConnection` wrapper
//! to dialers.
//!
//! `VatConnection.describe()` returns metadata without spawning a cell.
//! `VatConnection.bind()` lazily spawns the executor-bound cell once and returns
//! the application capability exported by the cell.
//!
//! Stdin semantics for vat cells: stdin is a shutdown signal channel, not a
//! data transport. The host never writes bytes. It closes stdin to signal the
//! cell to drain gracefully when the remote peer disconnects.

use std::cell::RefCell;
use std::rc::Rc;

use async_trait::async_trait;
use capnp::capability::Promise;
use capnp_rpc::pry;
use capnp_rpc::rpc_twoparty_capnp::Side;
use capnp_rpc::twoparty::VatNetwork;
use capnp_rpc::RpcSystem;
use futures::io::{AsyncRead, AsyncReadExt, AsyncWrite};
use futures::StreamExt;
use membrane::EpochGuard;
use tokio::sync::oneshot;

use membrane::system_capnp;

/// Host-derived metadata for a Runtime-minted executor that is valid for vat
/// publication.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ExecutorVatMetadata {
    pub wasm_artifact_cid: Vec<u8>,
    pub schema_bundle_cid: Vec<u8>,
    pub schema_bundle: Vec<u8>,
}

/// Host-side provenance resolver for Executor capabilities.
///
/// Implementations must only return metadata for Executors minted by the
/// trusted host RuntimeImpl. Guest-implemented Executor objects must be
/// rejected, even if they satisfy the Cap'n Proto interface shape.
#[async_trait(?Send)]
pub trait ExecutorResolver {
    async fn resolve(
        &self,
        executor: system_capnp::executor::Client,
    ) -> Result<ExecutorVatMetadata, capnp::Error>;
}

pub struct VatListenerImpl {
    stream_control: libp2p_stream::Control,
    guard: EpochGuard,
    executor_resolver: Option<Rc<dyn ExecutorResolver>>,
}

impl VatListenerImpl {
    pub fn new(stream_control: libp2p_stream::Control, guard: EpochGuard) -> Self {
        Self {
            stream_control,
            guard,
            executor_resolver: None,
        }
    }

    pub fn with_executor_resolver(
        stream_control: libp2p_stream::Control,
        guard: EpochGuard,
        executor_resolver: Option<Rc<dyn ExecutorResolver>>,
    ) -> Self {
        Self {
            stream_control,
            guard,
            executor_resolver,
        }
    }
}

#[allow(refining_impl_trait)]
impl system_capnp::vat_listener::Server for VatListenerImpl {
    fn listen(
        self: capnp::capability::Rc<Self>,
        params: system_capnp::vat_listener::ListenParams,
        mut results: system_capnp::vat_listener::ListenResults,
    ) -> Promise<(), capnp::Error> {
        pry!(self.guard.check());

        let params = pry!(params.get());
        let executor = pry!(params.get_executor());
        let protocol_reader = pry!(params.get_protocol());
        let protocol = pry!(protocol_reader
            .to_str()
            .map_err(|e| capnp::Error::failed(format!("vat protocol is not UTF-8: {e}"))))
        .to_string();
        let stream_protocol = pry!(super::vat_protocol(&protocol));

        let resolver = match &self.executor_resolver {
            Some(resolver) => resolver.clone(),
            None => {
                return Promise::err(capnp::Error::failed(
                    "VatListener.listen requires a host Executor provenance resolver".into(),
                ))
            }
        };

        let extra_caps = read_extra_caps(params.get_caps());
        let mut control = self.stream_control.clone();
        let mut epoch_rx = self.guard.receiver.clone();
        let issued_seq = self.guard.issued_seq;

        Promise::from_future(async move {
            let metadata = resolver.resolve(executor.clone()).await?;

            let mut incoming = control.accept(stream_protocol.clone()).map_err(|e| {
                capnp::Error::failed(format!("failed to register vat protocol: {e}"))
            })?;

            tracing::info!(
                protocol = %stream_protocol,
                schema_bundle_cid = %cid_display(&metadata.schema_bundle_cid),
                wasm_artifact_cid = %cid_display(&metadata.wasm_artifact_cid),
                "Registered vat service"
            );

            results
                .get()
                .set_wasm_artifact_cid(&metadata.wasm_artifact_cid);
            results
                .get()
                .set_schema_bundle_cid(&metadata.schema_bundle_cid);

            tokio::task::spawn_local(async move {
                loop {
                    tokio::select! {
                        conn = incoming.next() => {
                            let Some((peer_id, stream)) = conn else {
                                tracing::warn!(protocol = %stream_protocol, "Vat accept loop ended unexpectedly");
                                break;
                            };
                            let _accept_span = tracing::info_span!(
                                "vat.accept",
                                peer = %peer_id,
                                protocol = %stream_protocol,
                            ).entered();
                            tracing::debug!("Incoming vat connection");

                            let source = VatConnectionSource::Spawn {
                                executor: executor.clone(),
                                caps: extra_caps.clone(),
                                state: Rc::new(RefCell::new(BindState::Empty)),
                            };
                            let metadata = metadata.clone();

                            tokio::task::spawn_local(async move {
                                if let Err(e) = handle_vat_connection(metadata, source, stream).await
                                {
                                    tracing::error!("Vat connection error: {e}");
                                }
                            });
                        }
                        _ = epoch_rx.changed() => {
                            if epoch_rx.borrow().seq != issued_seq {
                                tracing::warn!(
                                    protocol = %stream_protocol,
                                    "Epoch became stale, closing vat accept loop"
                                );
                                break;
                            }
                        }
                    }
                }
            });

            Ok(())
        })
    }
}

fn read_extra_caps(
    caps: capnp::Result<capnp::struct_list::Reader<'_, membrane::membrane_capnp::export::Owned>>,
) -> Vec<(String, capnp::capability::Client, Vec<u8>)> {
    let mut caps_vec = Vec::new();
    let Ok(caps_reader) = caps else {
        return caps_vec;
    };

    for entry in caps_reader.iter() {
        let name = entry
            .get_name()
            .ok()
            .and_then(|n| n.to_str().ok())
            .unwrap_or_default()
            .to_string();
        let cap = match entry.get_cap().get_as_capability() {
            Ok(cap) => cap,
            Err(_) => continue,
        };
        let schema_bytes = match entry.get_schema() {
            Ok(node) => super::canonicalize_schema_node(node).unwrap_or_default(),
            Err(_) => Vec::new(),
        };
        caps_vec.push((name, cap, schema_bytes));
    }

    caps_vec
}

/// Serve one incoming peer stream with a host-side VatConnection wrapper.
async fn handle_vat_connection(
    metadata: ExecutorVatMetadata,
    source: VatConnectionSource,
    stream: impl AsyncRead + AsyncWrite + 'static,
) -> Result<(), capnp::Error> {
    let close_state = match &source {
        VatConnectionSource::Spawn { state, .. } => state.clone(),
    };
    let vat_connection: system_capnp::vat_connection::Client =
        capnp_rpc::new_client(VatConnectionImpl { metadata, source });

    let (reader, writer) = Box::pin(stream).split();
    let network = VatNetwork::new(reader, writer, Side::Server, Default::default());
    let peer_rpc = RpcSystem::new(Box::new(network), Some(vat_connection.client));

    let _ = peer_rpc.await;
    close_bound_stdin(&close_state).await;

    Ok(())
}

enum VatConnectionSource {
    Spawn {
        executor: system_capnp::executor::Client,
        caps: Vec<(String, capnp::capability::Client, Vec<u8>)>,
        state: Rc<RefCell<BindState>>,
    },
}

struct VatConnectionImpl {
    metadata: ExecutorVatMetadata,
    source: VatConnectionSource,
}

#[allow(refining_impl_trait)]
impl system_capnp::vat_connection::Server for VatConnectionImpl {
    fn describe(
        self: capnp::capability::Rc<Self>,
        _params: system_capnp::vat_connection::DescribeParams,
        mut results: system_capnp::vat_connection::DescribeResults,
    ) -> Promise<(), capnp::Error> {
        let metadata = self.metadata.clone();
        match set_info(results.get().init_info(), &metadata) {
            Ok(()) => Promise::ok(()),
            Err(e) => Promise::err(e),
        }
    }

    fn bind(
        self: capnp::capability::Rc<Self>,
        _params: system_capnp::vat_connection::BindParams,
        results: system_capnp::vat_connection::BindResults,
    ) -> Promise<(), capnp::Error> {
        let (executor, caps, state) = match &self.source {
            VatConnectionSource::Spawn {
                executor,
                caps,
                state,
            } => (executor, caps, state),
        };

        let action = {
            let mut state = state.borrow_mut();
            match &mut *state {
                BindState::Ready(bound) => BindAction::Ready(bound.clone()),
                BindState::Failed(message) => BindAction::Failed(message.clone()),
                BindState::Spawning(waiters) => {
                    let (tx, rx) = oneshot::channel();
                    waiters.push(tx);
                    BindAction::Wait(rx)
                }
                BindState::Empty => {
                    *state = BindState::Spawning(Vec::new());
                    BindAction::Spawn
                }
            }
        };

        match action {
            BindAction::Ready(bound) => finish_bind(results, bound),
            BindAction::Failed(message) => Promise::err(capnp::Error::failed(message)),
            BindAction::Wait(rx) => Promise::from_future(async move {
                let bound = rx
                    .await
                    .map_err(|_| capnp::Error::failed("vat bind waiter dropped".into()))?
                    .map_err(capnp::Error::failed)?;
                set_bind_results(results, &bound.schema_bundle, bound.app_cap)
            }),
            BindAction::Spawn => {
                let executor = executor.clone();
                let caps = caps.clone();
                let metadata = self.metadata.clone();
                let state = state.clone();

                Promise::from_future(async move {
                    let result =
                        spawn_bound_vat_cell(executor, caps, metadata.schema_bundle.clone())
                            .await
                            .map_err(|e| e.to_string());

                    let waiters = {
                        let mut state = state.borrow_mut();
                        match std::mem::replace(
                            &mut *state,
                            match &result {
                                Ok(bound) => BindState::Ready(bound.clone()),
                                Err(message) => BindState::Failed(message.clone()),
                            },
                        ) {
                            BindState::Spawning(waiters) => waiters,
                            _ => Vec::new(),
                        }
                    };

                    for waiter in waiters {
                        let _ = waiter.send(result.clone());
                    }

                    let bound = result.map_err(capnp::Error::failed)?;
                    set_bind_results(results, &bound.schema_bundle, bound.app_cap)
                })
            }
        }
    }
}

enum BindAction {
    Ready(BoundVatCell),
    Failed(String),
    Wait(oneshot::Receiver<Result<BoundVatCell, String>>),
    Spawn,
}

enum BindState {
    Empty,
    Spawning(Vec<oneshot::Sender<Result<BoundVatCell, String>>>),
    Ready(BoundVatCell),
    Failed(String),
}

#[derive(Clone)]
struct BoundVatCell {
    schema_bundle: Vec<u8>,
    app_cap: capnp::capability::Client,
    stdin: system_capnp::byte_stream::Client,
    _process: system_capnp::process::Client,
}

fn finish_bind(
    results: system_capnp::vat_connection::BindResults,
    bound: BoundVatCell,
) -> Promise<(), capnp::Error> {
    match set_bind_results(results, &bound.schema_bundle, bound.app_cap) {
        Ok(()) => Promise::ok(()),
        Err(e) => Promise::err(e),
    }
}

fn set_info(
    mut info: system_capnp::vat_service_info::Builder<'_>,
    metadata: &ExecutorVatMetadata,
) -> Result<(), capnp::Error> {
    info.set_wasm_artifact_cid(&metadata.wasm_artifact_cid);
    info.set_schema_bundle_cid(&metadata.schema_bundle_cid);
    with_schema_bundle_reader(&metadata.schema_bundle, |schema_bundle| {
        info.set_schema_bundle(schema_bundle)
    })
}

fn set_bind_results(
    mut results: system_capnp::vat_connection::BindResults,
    schema_bundle: &[u8],
    app_cap: capnp::capability::Client,
) -> Result<(), capnp::Error> {
    let mut builder = results.get();
    with_schema_bundle_reader(schema_bundle, |schema_bundle| {
        builder.reborrow().set_schema_bundle(schema_bundle)
    })?;
    builder.init_cap().set_as_capability(app_cap.hook);
    Ok(())
}

fn with_schema_bundle_reader<T>(
    bytes: &[u8],
    f: impl FnOnce(system_capnp::schema_bundle::Reader<'_>) -> Result<T, capnp::Error>,
) -> Result<T, capnp::Error> {
    let aligned = crate::graft::bytes_to_aligned_words(bytes);
    let segments: &[&[u8]] = &[capnp::Word::words_to_bytes(&aligned)];
    let segment_array = capnp::message::SegmentArray::new(segments);
    let reader = capnp::message::Reader::new(segment_array, capnp::message::ReaderOptions::new());
    let schema_bundle: system_capnp::schema_bundle::Reader<'_> = reader.get_root()?;
    f(schema_bundle)
}

async fn spawn_bound_vat_cell(
    executor: system_capnp::executor::Client,
    caps: Vec<(String, capnp::capability::Client, Vec<u8>)>,
    schema_bundle: Vec<u8>,
) -> Result<BoundVatCell, capnp::Error> {
    let (app_cap, stdin, process) = spawn_vat_bootstrap(executor, caps).await?;
    Ok(BoundVatCell {
        schema_bundle,
        app_cap,
        stdin,
        _process: process,
    })
}

async fn spawn_vat_bootstrap(
    executor: system_capnp::executor::Client,
    caps: Vec<(String, capnp::capability::Client, Vec<u8>)>,
) -> Result<
    (
        capnp::capability::Client,
        system_capnp::byte_stream::Client,
        system_capnp::process::Client,
    ),
    capnp::Error,
> {
    let mut spawn_req = executor.spawn_request();
    if !caps.is_empty() {
        let mut caps_builder = spawn_req.get().init_caps(caps.len() as u32);
        for (i, (name, client, schema_bytes)) in caps.iter().enumerate() {
            let mut entry = caps_builder.reborrow().get(i as u32);
            entry.set_name(name);
            if !schema_bytes.is_empty() {
                let aligned = crate::graft::bytes_to_aligned_words(schema_bytes);
                let segments: &[&[u8]] = &[capnp::Word::words_to_bytes(&aligned)];
                let segment_array = capnp::message::SegmentArray::new(segments);
                let reader = capnp::message::Reader::new(
                    segment_array,
                    capnp::message::ReaderOptions::new(),
                );
                let schema_node: capnp::schema_capnp::node::Reader<'_> = reader.get_root()?;
                entry.reborrow().set_schema(schema_node)?;
            }
            entry.init_cap().set_as_capability(client.hook.clone());
        }
    }
    let response = spawn_req.send().promise.await?;
    let process = response.get()?.get_process()?;

    let stdin_resp = process.stdin_request().send().promise.await?;
    let stdin = stdin_resp.get()?.get_stream()?;

    let bootstrap_resp = match tokio::time::timeout(
        std::time::Duration::from_secs(10),
        process.bootstrap_request().send().promise,
    )
    .await
    {
        Ok(Ok(resp)) => resp,
        Ok(Err(e)) => {
            let _ = stdin.close_request().send().promise.await;
            return Err(e);
        }
        Err(_timeout) => {
            let _ = stdin.close_request().send().promise.await;
            return Err(capnp::Error::failed(
                "cell did not export bootstrap capability within 10s \
                 (did the guest call system::serve()?)"
                    .into(),
            ));
        }
    };
    let bootstrap_cap: capnp::capability::Client = match bootstrap_resp
        .get()
        .and_then(|r| r.get_cap().get_as_capability())
    {
        Ok(cap) => cap,
        Err(e) => {
            let _ = stdin.close_request().send().promise.await;
            return Err(e);
        }
    };

    Ok((bootstrap_cap, stdin, process))
}

async fn close_bound_stdin(state: &Rc<RefCell<BindState>>) {
    let stdin = {
        let state = state.borrow();
        match &*state {
            BindState::Ready(bound) => Some(bound.stdin.clone()),
            _ => None,
        }
    };

    if let Some(stdin) = stdin {
        tracing::debug!("Peer disconnected, signaling vat cell shutdown");
        let _ = stdin.close_request().send().promise.await;
    }
}

fn cid_display(bytes: &[u8]) -> String {
    cid::Cid::try_from(bytes)
        .map(|cid| cid.to_string())
        .unwrap_or_else(|_| hex::encode(bytes))
}

/// Compatibility helper for older tests that validate the direct bridge.
/// Production VatListener now serves `VatConnection` and calls this through
/// `VatConnection.bind()`.
pub async fn handle_vat_connection_spawn(
    executor: system_capnp::executor::Client,
    caps: Vec<(String, capnp::capability::Client, Vec<u8>)>,
    stream: impl AsyncRead + AsyncWrite + 'static,
    protocol: &str,
) -> Result<(), capnp::Error> {
    let (bootstrap_cap, stdin, process) = spawn_vat_bootstrap(executor, caps).await?;

    let (reader, writer) = Box::pin(stream).split();
    let network = VatNetwork::new(reader, writer, Side::Server, Default::default());
    let peer_rpc = RpcSystem::new(Box::new(network), Some(bootstrap_cap));

    let wait_fut = async {
        let resp = process.wait_request().send().promise.await;
        match resp {
            Ok(r) => r.get().map(|r| r.get_exit_code()).unwrap_or(1),
            Err(_) => 1,
        }
    };

    tokio::select! {
        _ = peer_rpc => {
            tracing::debug!(protocol = %protocol, "Peer disconnected, signaling cell shutdown");
            let _ = stdin.close_request().send().promise.await;
        }
        exit_code = wait_fut => {
            tracing::debug!(exit_code, protocol = %protocol, "Vat cell process exited");
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;

    struct CountingExecutor {
        spawn_count: Rc<Cell<u32>>,
        process: system_capnp::process::Client,
    }

    #[allow(refining_impl_trait)]
    impl system_capnp::executor::Server for CountingExecutor {
        fn spawn(
            self: capnp::capability::Rc<Self>,
            _params: system_capnp::executor::SpawnParams,
            mut results: system_capnp::executor::SpawnResults,
        ) -> Promise<(), capnp::Error> {
            self.spawn_count.set(self.spawn_count.get() + 1);
            results.get().set_process(self.process.clone());
            Promise::ok(())
        }
    }

    struct TrackingByteStream {
        closed: Rc<Cell<u32>>,
    }

    #[allow(refining_impl_trait)]
    impl system_capnp::byte_stream::Server for TrackingByteStream {
        fn read(
            self: capnp::capability::Rc<Self>,
            _params: system_capnp::byte_stream::ReadParams,
            _results: system_capnp::byte_stream::ReadResults,
        ) -> Promise<(), capnp::Error> {
            Promise::err(capnp::Error::failed("unused".into()))
        }

        fn write(
            self: capnp::capability::Rc<Self>,
            _params: system_capnp::byte_stream::WriteParams,
            _results: system_capnp::byte_stream::WriteResults,
        ) -> Promise<(), capnp::Error> {
            Promise::err(capnp::Error::failed("unused".into()))
        }

        fn close(
            self: capnp::capability::Rc<Self>,
            _params: system_capnp::byte_stream::CloseParams,
            _results: system_capnp::byte_stream::CloseResults,
        ) -> Promise<(), capnp::Error> {
            self.closed.set(self.closed.get() + 1);
            Promise::ok(())
        }
    }

    #[tokio::test]
    async fn describe_does_not_spawn() {
        let local = tokio::task::LocalSet::new();
        local
            .run_until(async {
                let spawn_count = Rc::new(Cell::new(0));
                let connection = test_connection(spawn_count.clone());

                let response = connection
                    .describe_request()
                    .send()
                    .promise
                    .await
                    .expect("describe");
                let info = response.get().unwrap().get_info().unwrap();
                assert_eq!(
                    info.get_schema_bundle_cid().unwrap().to_vec(),
                    schema_id::compute_cid_bytes(&test_schema_bundle_bytes())
                );
                assert_eq!(spawn_count.get(), 0);
            })
            .await;
    }

    #[tokio::test]
    async fn bind_spawns_once_and_repeated_bind_is_stable() {
        let local = tokio::task::LocalSet::new();
        local
            .run_until(async {
                let spawn_count = Rc::new(Cell::new(0));
                let connection = test_connection(spawn_count.clone());

                let first = connection
                    .bind_request()
                    .send()
                    .promise
                    .await
                    .expect("first bind");
                let first_schema = first.get().unwrap().get_schema_bundle().unwrap();
                assert_eq!(first_schema.get_format_version(), 1);

                let second = connection
                    .bind_request()
                    .send()
                    .promise
                    .await
                    .expect("second bind");
                let second_schema = second.get().unwrap().get_schema_bundle().unwrap();
                assert_eq!(
                    second_schema.get_service_interface_id(),
                    0xd0ac_8299_df07_9c61
                );

                assert_eq!(spawn_count.get(), 1);
            })
            .await;
    }

    #[tokio::test]
    async fn close_bound_stdin_closes_ready_cell_stdin() {
        let local = tokio::task::LocalSet::new();
        local
            .run_until(async {
                let closed = Rc::new(Cell::new(0));
                let stdin: system_capnp::byte_stream::Client =
                    capnp_rpc::new_client(TrackingByteStream {
                        closed: closed.clone(),
                    });
                let state = Rc::new(RefCell::new(BindState::Ready(BoundVatCell {
                    schema_bundle: test_schema_bundle_bytes(),
                    app_cap: test_bootstrap_cap(),
                    stdin,
                    _process: capnp_rpc::new_client(test_process_impl(test_bootstrap_cap())),
                })));

                close_bound_stdin(&state).await;
                assert_eq!(closed.get(), 1);
            })
            .await;
    }

    fn test_connection(spawn_count: Rc<Cell<u32>>) -> system_capnp::vat_connection::Client {
        let process = capnp_rpc::new_client(test_process_impl(test_bootstrap_cap()));
        let executor: system_capnp::executor::Client = capnp_rpc::new_client(CountingExecutor {
            spawn_count,
            process,
        });
        capnp_rpc::new_client(VatConnectionImpl {
            metadata: test_metadata(),
            source: VatConnectionSource::Spawn {
                executor,
                caps: Vec::new(),
                state: Rc::new(RefCell::new(BindState::Empty)),
            },
        })
    }

    fn test_process_impl(bootstrap: capnp::capability::Client) -> crate::ProcessImpl {
        let (stdin_stream, _) = tokio::io::duplex(1);
        let (stdout_stream, _) = tokio::io::duplex(1);
        let (stderr_stream, _) = tokio::io::duplex(1);
        let stdin = capnp_rpc::new_client(crate::ByteStreamImpl::new(
            stdin_stream,
            crate::StreamMode::WriteOnly,
        ));
        let stdout = capnp_rpc::new_client(crate::ByteStreamImpl::new(
            stdout_stream,
            crate::StreamMode::ReadOnly,
        ));
        let stderr = capnp_rpc::new_client(crate::ByteStreamImpl::new(
            stderr_stream,
            crate::StreamMode::ReadOnly,
        ));
        let (_exit_tx, exit_rx) = tokio::sync::oneshot::channel();
        let (kill_tx, _kill_rx) = tokio::sync::watch::channel(false);
        crate::ProcessImpl::with_bootstrap(stdin, stdout, stderr, exit_rx, bootstrap, kill_tx)
    }

    fn test_bootstrap_cap() -> capnp::capability::Client {
        let (tx, _rx) = tokio::sync::mpsc::channel(1);
        let host = crate::HostImpl::new(
            crate::NetworkState::from_peer_id(vec![1, 2, 3, 4]),
            tx,
            false,
            None,
            None,
        );
        let host: system_capnp::host::Client = capnp_rpc::new_client(host);
        host.client
    }

    fn test_metadata() -> ExecutorVatMetadata {
        let schema_bundle = test_schema_bundle_bytes();
        ExecutorVatMetadata {
            wasm_artifact_cid: schema_id::compute_cid_bytes(b"test wasm"),
            schema_bundle_cid: schema_id::compute_cid_bytes(&schema_bundle),
            schema_bundle,
        }
    }

    fn test_schema_bundle_bytes() -> Vec<u8> {
        let mut message = capnp::message::Builder::new_default();
        {
            let mut root = message.init_root::<system_capnp::schema_bundle::Builder>();
            root.set_format_version(schema_id::SCHEMA_BUNDLE_FORMAT_VERSION);
            root.set_service_interface_id(0xd0ac_8299_df07_9c61);
            let nodes = root.init_nodes(1);
            let mut node = nodes.get(0);
            node.set_id(0xd0ac_8299_df07_9c61);
            node.set_display_name("test.capnp:ChessEngine");
            node.init_interface();
        }
        let reader: system_capnp::schema_bundle::Reader<'_> =
            message.get_root_as_reader().expect("schema bundle root");
        let mut canonical = capnp::message::Builder::new_default();
        canonical
            .set_root_canonical(reader)
            .expect("canonical schema bundle");
        let segments = canonical.get_segments_for_output();
        assert_eq!(segments.len(), 1);
        segments[0].to_vec()
    }
}
