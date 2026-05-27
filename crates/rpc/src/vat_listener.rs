//! VatListener capability: guest-exported subprotocols via Cap'n Proto RPC.
//!
//! The `VatListener` capability lets a guest register a libp2p subprotocol cell
//! that exports a typed capability. Two modes are supported:
//!
//! **Spawn mode** (VatHandler::spawn): for each incoming connection, spawn a fresh
//! cell process via the Executor, capture its `system::serve()` exported
//! bootstrap capability, and serve it to the connecting peer via Cap'n Proto RPC.
//!
//! **Serve mode** (VatHandler::serve): bootstrap each connection with a persistent
//! capability — no cell spawning. One capability serves all connections.
//!
//! **Stdin semantics for vat cells (spawn mode):** stdin is a shutdown signal
//! channel, not a data transport. The host never writes bytes — it only closes
//! stdin to signal the cell to drain gracefully (equivalent to Go's `<-chan struct{}`).
//!
//! This is the capability-mode counterpart of `StreamListener` (byte-stream mode).

use capnp::capability::Promise;
use capnp_rpc::pry;
use capnp_rpc::rpc_twoparty_capnp::Side;
use capnp_rpc::twoparty::VatNetwork;
use capnp_rpc::RpcSystem;
use futures::io::{AsyncRead, AsyncReadExt, AsyncWrite};
use futures::StreamExt;
use membrane::EpochGuard;

use membrane::system_capnp;

pub struct VatListenerImpl {
    stream_control: libp2p_stream::Control,
    guard: EpochGuard,
}

impl VatListenerImpl {
    pub fn new(stream_control: libp2p_stream::Control, guard: EpochGuard) -> Self {
        Self {
            stream_control,
            guard,
        }
    }
}

#[allow(refining_impl_trait)]
impl system_capnp::vat_listener::Server for VatListenerImpl {
    fn listen(
        self: capnp::capability::Rc<Self>,
        params: system_capnp::vat_listener::ListenParams,
        _results: system_capnp::vat_listener::ListenResults,
    ) -> Promise<(), capnp::Error> {
        pry!(self.guard.check());

        let params = pry!(params.get());

        // Read schema bytes from the explicit param.
        let schema_bytes: Vec<u8> = pry!(params.get_schema()).to_vec();
        if schema_bytes.is_empty() {
            return Promise::err(capnp::Error::failed(
                "schema bytes must not be empty".into(),
            ));
        }
        let schema_bytes = pry!(super::canonicalize_schema_bytes(&schema_bytes));

        let protocol_cid = super::schema_cid(&schema_bytes);
        let stream_protocol = pry!(super::schema_protocol(&protocol_cid));

        let mut control = self.stream_control.clone();
        let mut incoming =
            pry!(control
                .accept(stream_protocol.clone())
                .map_err(|e| capnp::Error::failed(format!(
                    "failed to register vat protocol cell: {e}"
                ))));

        tracing::info!(protocol = %stream_protocol, "Registered vat subprotocol cell");

        // Read the VatHandler union to determine mode.
        let handler = pry!(params.get_handler());
        let handler_which = pry!(handler.which());

        // Read optional caps from the listen request (init.d `with` block grants).
        // Collect as (name, client, schema_bytes) tuples; the schema bytes are
        // re-emitted on each spawned-cell graft so guests can introspect the
        // cap's interface end-to-end.
        let extra_caps: Vec<(String, capnp::capability::Client, Vec<u8>)> = {
            let mut caps_vec = Vec::new();
            if let Ok(caps_reader) = params.get_caps() {
                for entry in caps_reader.iter() {
                    let name = match entry.get_name() {
                        Ok(n) => match n.to_str() {
                            Ok(s) => s.to_string(),
                            Err(e) => {
                                return Promise::err(capnp::Error::failed(format!(
                                    "invalid utf8 cap name: {e}"
                                )))
                            }
                        },
                        Err(e) => return Promise::err(capnp::Error::from(e)),
                    };
                    let cap = match entry.get_cap().get_as_capability() {
                        Ok(v) => v,
                        Err(e) => return Promise::err(e),
                    };
                    let schema_bytes = match entry.get_schema() {
                        Ok(node) => match super::canonicalize_schema_node(node) {
                            Some(bytes) => bytes,
                            None => {
                                return Promise::err(capnp::Error::failed(
                                    "invalid cap schema: canonicalization failed".into(),
                                ))
                            }
                        },
                        Err(_) => Vec::new(),
                    };
                    caps_vec.push((name, cap, schema_bytes));
                }
            }
            caps_vec
        };

        match handler_which {
            system_capnp::vat_handler::Which::Spawn(executor) => {
                let executor: system_capnp::executor::Client = pry!(executor);

                // Accept loop: for each incoming connection, spawn a cell and bridge RPC.
                let mut epoch_rx = self.guard.receiver.clone();
                let issued_seq = self.guard.issued_seq;
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
                                    mode = "spawn",
                                ).entered();
                                tracing::debug!("Incoming vat connection");
                                let executor = executor.clone();
                                let protocol_cid = protocol_cid.clone();
                                let schema_bytes = schema_bytes.clone();
                                let caps = extra_caps.clone();
                                tokio::task::spawn_local(async move {
                                    let _handle_span = tracing::info_span!(
                                        "vat.handle",
                                        protocol = protocol_cid,
                                    ).entered();
                                    if let Err(e) =
                                        handle_vat_connection_spawn(
                                            executor,
                                            caps,
                                            stream,
                                            &protocol_cid,
                                            &schema_bytes,
                                        )
                                        .await
                                    {
                                        tracing::error!("Vat cell connection error: {e}");
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
            }
            system_capnp::vat_handler::Which::Serve(cap_ptr) => {
                let typed = pry!(cap_ptr);
                let bootstrap_cap: capnp::capability::Client =
                    match typed.get_cap().get_as_capability() {
                        Ok(v) => v,
                        Err(e) => return Promise::err(e),
                    };
                let served_schema = match typed.get_schema() {
                    Ok(schema) => schema,
                    Err(e) => return Promise::err(capnp::Error::from(e)),
                };
                let served_root = match served_schema.get_root() {
                    Ok(root) => root,
                    Err(e) => return Promise::err(capnp::Error::from(e)),
                };
                let served_schema_bytes = match super::canonicalize_schema_node(served_root) {
                    Some(bytes) => bytes,
                    None => {
                        return Promise::err(capnp::Error::failed(
                            "invalid serve schema: canonicalization failed".into(),
                        ))
                    }
                };
                if served_schema_bytes != schema_bytes {
                    return Promise::err(capnp::Error::failed(
                        "vat-listener.listen schema must match handler.serve typed schema".into(),
                    ));
                }

                // Accept loop: for each incoming connection, bootstrap with the persistent cap.
                let mut epoch_rx = self.guard.receiver.clone();
                let issued_seq = self.guard.issued_seq;
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
                                    mode = "serve",
                                ).entered();
                                tracing::debug!("Incoming vat connection");
                                let cap = bootstrap_cap.clone();
                                let protocol_cid = protocol_cid.clone();
                                let schema_bytes = schema_bytes.clone();
                                tokio::task::spawn_local(async move {
                                    let _handle_span = tracing::info_span!(
                                        "vat.handle",
                                        protocol = protocol_cid,
                                    ).entered();
                                    if let Err(e) =
                                        handle_vat_connection_serve(cap, stream, &protocol_cid, &schema_bytes).await
                                    {
                                        tracing::error!("Vat serve connection error: {e}");
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
            }
        }

        Promise::ok(())
    }
}

/// Spawn mode: spawn a cell process for a single connection and bridge its
/// exported bootstrap capability to the connecting peer via Cap'n Proto RPC.
///
/// Architecture (two-RPC-system bridge):
///
/// ```text
/// Remote peer  <──[libp2p stream]──>  Host bridge  <──[WASI bidi]──>  Cell
///                  RPC system B                        RPC system A
///                  (Side::Server)                      (Side::Server)
///                  bootstrap =                         bootstrap = Membrane
///                  cell_cap <── captures <───────── cell exports via serve()
/// ```
///
/// Generic over stream type so integration tests can substitute an in-memory
/// duplex for the libp2p stream. Production callers pass `libp2p::Stream`.
pub async fn handle_vat_connection_spawn(
    executor: system_capnp::executor::Client,
    caps: Vec<(String, capnp::capability::Client, Vec<u8>)>,
    stream: impl AsyncRead + AsyncWrite + Unpin + 'static,
    protocol_cid: &str,
    schema_bytes: &[u8],
) -> Result<(), capnp::Error> {
    // 1. Spawn cell process via Executor.spawn(), forwarding caps with
    //    their canonical Schema.Node bytes so the spawned cell's graft
    //    can populate Export.schema for the guest.
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
                let schema_node: capnp::schema_capnp::node::Reader = reader.get_root()?;
                entry.reborrow().set_schema(schema_node)?;
            }
            entry.init_cap().set_as_capability(client.hook.clone());
        }
    }
    let response = spawn_req.send().promise.await?;
    let process = response.get()?.get_process()?;

    // 2. Get stdin handle. For vat cells, stdin is a shutdown signal:
    //    closing it tells the cell to drain and exit gracefully.
    //    No bytes are ever written — it's a <-chan struct{}.
    let stdin_resp = process.stdin_request().send().promise.await?;
    let stdin = stdin_resp.get()?.get_stream()?;

    // 3. Get the cell's exported bootstrap capability.
    //    Timeout guards against cells that never call system::serve().
    //    On failure, close stdin to clean up the orphaned cell process.
    let bootstrap_resp = match tokio::time::timeout(std::time::Duration::from_secs(10), {
        let mut req = process.bootstrap_request();
        req.get().set_schema(&schema_bytes);
        req.send().promise
    })
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
    let bootstrap_cap: capnp::capability::Client = match bootstrap_resp.get().and_then(|r| {
        let typed = r.get_typed()?;
        typed.get_cap().get_as_capability()
    }) {
        Ok(cap) => cap,
        Err(e) => {
            let _ = stdin.close_request().send().promise.await;
            return Err(e);
        }
    };

    // 4. Write schema attestation first, then start Cap'n Proto RPC.
    let mut stream = stream;
    super::vat_dial::write_schema_attestation(&mut stream, schema_bytes).await?;

    // 5. Bridge: serve the cell's cap to the remote peer over the libp2p stream.
    let (reader, writer) = Box::pin(stream).split();
    let network = VatNetwork::new(reader, writer, Side::Server, Default::default());
    let peer_rpc = RpcSystem::new(Box::new(network), Some(bootstrap_cap));

    // 6. Drive the peer RPC system and cell process concurrently.
    //    When EITHER side finishes (peer disconnects OR cell exits),
    //    we tear down both to avoid serving a dead capability or
    //    keeping a cell alive with no peer.
    let wait_fut = async {
        let resp = process.wait_request().send().promise.await;
        match resp {
            Ok(r) => r.get().map(|r| r.get_exit_code()).unwrap_or(1),
            Err(_) => 1,
        }
    };

    tokio::select! {
        _ = peer_rpc => {
            tracing::debug!(protocol = protocol_cid, "Peer disconnected, signaling cell shutdown");
            // Peer disconnected. Close stdin to signal graceful shutdown.
            let _ = stdin.close_request().send().promise.await;
        }
        exit_code = wait_fut => {
            tracing::debug!(exit_code, protocol = protocol_cid, "Vat cell process exited");
            // Cell exited on its own. The peer RPC will get disconnected
            // errors on subsequent calls since the bootstrap cap is dead.
        }
    }

    Ok(())
}

/// Serve mode: bootstrap each connection with a persistent capability.
/// No cell spawning — one capability serves all connections.
///
/// Generic over stream type for testability.
pub async fn handle_vat_connection_serve(
    bootstrap_cap: capnp::capability::Client,
    stream: impl AsyncRead + AsyncWrite + Unpin + 'static,
    protocol_cid: &str,
    schema_bytes: &[u8],
) -> Result<(), capnp::Error> {
    let mut stream = stream;
    super::vat_dial::write_schema_attestation(&mut stream, schema_bytes).await?;

    let (reader, writer) = Box::pin(stream).split();
    let network = VatNetwork::new(reader, writer, Side::Server, Default::default());
    let peer_rpc = RpcSystem::new(Box::new(network), Some(bootstrap_cap));

    // Drive the RPC system until the peer disconnects.
    let _ = peer_rpc.await;
    tracing::debug!(protocol = protocol_cid, "Serve-mode peer disconnected");

    Ok(())
}
