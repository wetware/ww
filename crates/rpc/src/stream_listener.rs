//! StreamListener capability: guest-exported subprotocols via process-per-connection.
//!
//! The `StreamListener` capability lets a guest register a libp2p subprotocol cell.
//! For each incoming stream on that subprotocol, the host spawns a fresh WASI
//! process (via the guest-provided `Executor`) with stdin/stdout wired to the
//! stream — the cell speaks whatever wire protocol it wants over stdio.

use capnp::capability::Promise;
use capnp_rpc::pry;
use futures::io::{AsyncReadExt, AsyncWriteExt};
use futures::StreamExt;
use membrane::EpochGuard;

use membrane::system_capnp;

pub struct StreamListenerImpl {
    stream_control: libp2p_stream::Control,
    guard: EpochGuard,
}

impl StreamListenerImpl {
    pub fn new(stream_control: libp2p_stream::Control, guard: EpochGuard) -> Self {
        Self {
            stream_control,
            guard,
        }
    }
}

/// A captured init.d `with`-block grant. Cloned per incoming stream and
/// re-emitted on the spawned cell's graft.
type ExtraCap = (String, capnp::capability::Client);

#[allow(refining_impl_trait)]
impl system_capnp::stream_listener::Server for StreamListenerImpl {
    fn listen(
        self: capnp::capability::Rc<Self>,
        params: system_capnp::stream_listener::ListenParams,
        _results: system_capnp::stream_listener::ListenResults,
    ) -> Promise<(), capnp::Error> {
        pry!(self.guard.check());

        let params = pry!(params.get());
        let executor: system_capnp::executor::Client = pry!(params.get_executor());
        let protocol_str = pry!(pry!(params.get_protocol())
            .to_str()
            .map_err(|e| capnp::Error::failed(e.to_string())));

        let protocol_suffix = protocol_str.to_string();
        let stream_protocol = pry!(super::stream_protocol(&protocol_suffix));

        let extra_caps: Vec<ExtraCap> = {
            let mut caps_vec = Vec::new();
            if let Ok(caps_reader) = params.get_caps() {
                for entry in caps_reader.iter() {
                    if let Ok(name) = entry.get_name().map(|n| n.to_string().unwrap_or_default()) {
                        if let Ok(cap) = entry
                            .get_cap()
                            .get_as_capability::<capnp::capability::Client>()
                        {
                            caps_vec.push((name, cap));
                        }
                    }
                }
            }
            caps_vec
        };

        let mut control = self.stream_control.clone();
        let mut incoming = pry!(control
            .accept(stream_protocol.clone())
            .map_err(|e| capnp::Error::failed(format!("failed to register protocol cell: {e}"))));

        tracing::info!(protocol = %stream_protocol, "Registered stream subprotocol cell");

        // Accept loop: for each incoming connection, spawn a cell process.
        // Watches the epoch guard so we stop accepting when capabilities are revoked.
        let mut epoch_rx = self.guard.receiver.clone();
        let issued_seq = self.guard.issued_seq;
        tokio::task::spawn_local(async move {
            loop {
                tokio::select! {
                    conn = incoming.next() => {
                        let Some((peer_id, stream)) = conn else {
                            tracing::warn!(protocol = %stream_protocol, "Stream subprotocol accept loop ended unexpectedly");
                            break;
                        };
                        let _accept_span = tracing::info_span!(
                            "stream.accept",
                            peer = %peer_id,
                            protocol = %stream_protocol,
                        ).entered();
                        tracing::debug!("Incoming stream connection");
                        let executor = executor.clone();
                        let protocol = protocol_suffix.clone();
                        let caps = extra_caps.clone();
                        tokio::task::spawn_local(async move {
                            let _handle_span = tracing::info_span!(
                                "stream.handle",
                                protocol = protocol.as_str(),
                            ).entered();
                            if let Err(e) = handle_connection(executor, caps, stream, &protocol).await {
                                tracing::error!("Stream cell connection error: {e}");
                            }
                        });
                    }
                    _ = epoch_rx.changed() => {
                        if epoch_rx.borrow().seq != issued_seq {
                            tracing::warn!(
                                protocol = %stream_protocol,
                                "Epoch became stale, closing stream accept loop"
                            );
                            break;
                        }
                    }
                }
            }
        });

        Promise::ok(())
    }
}

/// Spawn a cell process for a single connection and pump
/// stdin/stdout between the libp2p stream and the process.
async fn handle_connection(
    executor: system_capnp::executor::Client,
    caps: Vec<ExtraCap>,
    stream: libp2p::Stream,
    protocol: &str,
) -> Result<(), capnp::Error> {
    // Spawn cell process via Executor.spawn().
    let mut spawn_req = executor.spawn_request();
    if !caps.is_empty() {
        let mut caps_builder = spawn_req.get().init_caps(caps.len() as u32);
        for (i, (name, cap)) in caps.iter().enumerate() {
            let mut entry = caps_builder.reborrow().get(i as u32);
            entry.set_name(name);
            entry.init_cap().set_as_capability(cap.clone().hook);
        }
    }
    let response = spawn_req.send().promise.await?;
    let process = response.get()?.get_process()?;

    // Get stdin (write-only) and stdout (read-only) ByteStream clients.
    let stdin_resp = process.stdin_request().send().promise.await?;
    let stdin = stdin_resp.get()?.get_stream()?;

    let stdout_resp = process.stdout_request().send().promise.await?;
    let stdout = stdout_resp.get()?.get_stream()?;

    // Split the libp2p stream into read and write halves.
    let (reader, writer) = Box::pin(stream).split();

    // Keep a handle to close stdin after the pumps finish.
    let stdin_close = stdin.clone();

    // Pump data concurrently: stream->stdin and stdout->stream.
    // When either pump finishes, drop the other and clean up.
    futures::future::select(
        Box::pin(pump_stream_to_stdin(reader, stdin)),
        Box::pin(pump_stdout_to_stream(stdout, writer)),
    )
    .await;

    // Ensure stdin is closed so the cell sees EOF.
    let _ = stdin_close.close_request().send().promise.await;

    // Wait for the cell process to exit.
    let wait_resp = process.wait_request().send().promise.await?;
    let exit_code = wait_resp.get()?.get_exit_code();
    tracing::debug!(exit_code, protocol, "Cell process exited");

    Ok(())
}

/// Read from the libp2p stream and write to the cell's stdin.
pub(crate) async fn pump_stream_to_stdin(
    mut reader: impl futures::io::AsyncRead + Unpin,
    stdin: system_capnp::byte_stream::Client,
) {
    let _span = tracing::info_span!("stream.pump_in").entered();
    let mut buf = vec![0u8; 64 * 1024];
    loop {
        match reader.read(&mut buf).await {
            Ok(0) => {
                let _ = stdin.close_request().send().promise.await;
                break;
            }
            Ok(n) => {
                tracing::trace!(bytes = n, "pump_in: read chunk");
                let mut req = stdin.write_request();
                req.get().set_data(&buf[..n]);
                if let Err(e) = req.send().promise.await {
                    tracing::debug!("stdin write failed: {e}");
                    break;
                }
            }
            Err(e) => {
                tracing::debug!("stream read error: {e}");
                let _ = stdin.close_request().send().promise.await;
                break;
            }
        }
    }
}

/// Read from the cell's stdout and write to the libp2p stream.
pub(crate) async fn pump_stdout_to_stream(
    stdout: system_capnp::byte_stream::Client,
    mut writer: impl futures::io::AsyncWrite + Unpin,
) {
    let _span = tracing::info_span!("stream.pump_out").entered();
    loop {
        let mut req = stdout.read_request();
        req.get().set_max_bytes(64 * 1024);
        let result: Result<Vec<u8>, capnp::Error> = req.send().promise.await.and_then(|response| {
            let data = response.get()?.get_data()?.to_vec();
            Ok(data)
        });
        match result {
            Ok(data) if data.is_empty() => break,
            Ok(data) => {
                tracing::trace!(bytes = data.len(), "pump_out: write chunk");
                if let Err(e) = writer.write_all(&data).await {
                    tracing::debug!("stream write error: {e}");
                    break;
                }
                if let Err(e) = writer.flush().await {
                    tracing::debug!("stream flush error: {e}");
                    break;
                }
            }
            Err(e) => {
                tracing::debug!("stdout read error: {e}");
                break;
            }
        }
    }
}
