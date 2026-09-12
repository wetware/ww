//! StreamListener capability: guest-exported subprotocols via process-per-connection.
//!
//! The `StreamListener` capability lets a guest register a libp2p subprotocol cell.
//! For each incoming stream on that subprotocol, the host spawns a fresh WASI
//! process (via the guest-provided `Executor`) with stdin/stdout wired to the
//! stream — the cell speaks whatever wire protocol it wants over stdio.

use authority::EpochGuard;
use capnp::capability::Promise;
use capnp_rpc::pry;
use futures::io::{AsyncReadExt, AsyncWriteExt};
use futures::StreamExt;

use crate::{inbound_connection_budget, ConnectionBudget};
use authority::system_capnp;

pub struct StreamListenerImpl {
    stream_control: libp2p_stream::Control,
    guard: EpochGuard,
    budget: ConnectionBudget,
}

impl StreamListenerImpl {
    pub fn new(stream_control: libp2p_stream::Control, guard: EpochGuard) -> Self {
        Self {
            stream_control,
            guard,
            budget: inbound_connection_budget(),
        }
    }

    pub fn with_budget(mut self, budget: ConnectionBudget) -> Self {
        self.budget = budget;
        self
    }
}

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

        if !params.has_membrane() {
            return Promise::err(capnp::Error::failed(
                "stream listener: membrane is required".into(),
            ));
        }
        let membrane = pry!(params.get_membrane());

        let mut control = self.stream_control.clone();
        let mut incoming = pry!(control
            .accept(stream_protocol.clone())
            .map_err(|e| capnp::Error::failed(format!("failed to register protocol cell: {e}"))));

        tracing::info!(protocol = %stream_protocol, "Registered stream subprotocol cell");

        // Accept loop: for each incoming connection, spawn a cell process.
        // Watches the epoch guard so we stop accepting when capabilities are revoked.
        let mut epoch_rx = self.guard.receiver.clone();
        let issued_seq = self.guard.issued_seq;
        let budget = self.budget.clone();
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
                        let permit = match budget.try_acquire() {
                            Ok(permit) => permit,
                            Err(error) => {
                                tracing::warn!(
                                    capacity = error.capacity,
                                    active = budget.active(),
                                    "rejecting stream connection: service connection budget exhausted"
                                );
                                drop(stream);
                                continue;
                            }
                        };
                        let executor = executor.clone();
                        let protocol = protocol_suffix.clone();
                        let membrane = membrane.clone();
                        tokio::task::spawn_local(async move {
                            let _permit = permit;
                            let _handle_span = tracing::info_span!(
                                "stream.handle",
                                protocol = protocol.as_str(),
                            ).entered();
                            if let Err(e) = handle_connection(executor, membrane, stream, &protocol).await {
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
    membrane: system_capnp::membrane::Client,
    stream: libp2p::Stream,
    protocol: &str,
) -> Result<(), capnp::Error> {
    let process = spawn_connection_child(&executor, membrane).await?;

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

async fn spawn_connection_child(
    executor: &system_capnp::executor::Client,
    membrane: system_capnp::membrane::Client,
) -> Result<system_capnp::process::Client, capnp::Error> {
    // Spawn cell process via Executor.spawn().
    let mut spawn_req = executor.spawn_request();
    spawn_req.get().set_membrane(membrane);
    let response = spawn_req.send().promise.await?;
    response.get()?.get_process()
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

#[cfg(test)]
mod tests {
    use super::*;
    use authority::{GraftBuilder, MembraneServer};
    use std::cell::{Cell, RefCell};
    use std::rc::Rc;

    struct RecordingExecutor {
        observed_membranes: Rc<RefCell<Vec<system_capnp::membrane::Client>>>,
    }

    #[allow(refining_impl_trait)]
    impl system_capnp::executor::Server for RecordingExecutor {
        fn spawn(
            self: capnp::capability::Rc<Self>,
            params: system_capnp::executor::SpawnParams,
            _results: system_capnp::executor::SpawnResults,
        ) -> Promise<(), capnp::Error> {
            let params = pry!(params.get());
            let membrane = pry!(params.get_membrane());
            self.observed_membranes.borrow_mut().push(membrane);
            Promise::err(capnp::Error::failed(
                "recording executor has no process".into(),
            ))
        }
    }

    struct ExtrasBuilder {
        capability: capnp::capability::Client,
        grafts: Rc<Cell<u32>>,
    }

    impl GraftBuilder for ExtrasBuilder {
        fn build(
            &self,
            _guard: &EpochGuard,
            mut builder: system_capnp::membrane::graft_results::Builder<'_>,
        ) -> Result<(), capnp::Error> {
            let graft = self.grafts.get() + 1;
            self.grafts.set(graft);
            builder.set_peer_id(&graft.to_be_bytes());
            let mut extra = builder.reborrow().init_extras(1).get(0);
            extra.set_name("application-extra");
            extra
                .init_cap()
                .set_as_capability(self.capability.clone().hook);
            Ok(())
        }
    }

    fn test_membrane(grafts: Rc<Cell<u32>>) -> system_capnp::membrane::Client {
        let epoch = authority::Epoch {
            seq: 1,
            head: Vec::new(),
            root: None,
        };
        let (_tx, rx) = tokio::sync::watch::channel(epoch);
        let capability: system_capnp::executor::Client = capnp_rpc::new_client(RecordingExecutor {
            observed_membranes: Rc::new(RefCell::new(Vec::new())),
        });
        capnp_rpc::new_client(MembraneServer::new(
            rx,
            ExtrasBuilder {
                capability: capability.client,
                grafts,
            },
        ))
    }

    fn test_guard() -> EpochGuard {
        let epoch = authority::Epoch {
            seq: 1,
            head: Vec::new(),
            root: None,
        };
        let (_tx, rx) = tokio::sync::watch::channel(epoch);
        EpochGuard {
            issued_seq: 1,
            receiver: rx,
        }
    }

    #[tokio::test]
    async fn listener_registration_does_not_graft_the_supplied_membrane() {
        let local = tokio::task::LocalSet::new();
        local
            .run_until(async {
                let grafts = Rc::new(Cell::new(0));
                let membrane = test_membrane(grafts.clone());
                let observed_membranes = Rc::new(RefCell::new(Vec::new()));
                let executor: system_capnp::executor::Client =
                    capnp_rpc::new_client(RecordingExecutor { observed_membranes });
                let listener: system_capnp::stream_listener::Client =
                    capnp_rpc::new_client(StreamListenerImpl::new(
                        libp2p_stream::Behaviour::new().new_control(),
                        test_guard(),
                    ));
                let mut request = listener.listen_request();
                request.get().set_executor(executor);
                request.get().set_protocol("stateful-forwarding-test");
                request.get().set_membrane(membrane);

                request
                    .send()
                    .promise
                    .await
                    .expect("register stream listener");

                assert_eq!(
                    grafts.get(),
                    0,
                    "stream registration must not inspect the supplied Membrane"
                );
            })
            .await;
    }

    #[tokio::test]
    async fn spawn_connection_child_forwards_stateful_membrane_for_each_connection() {
        let local = tokio::task::LocalSet::new();
        local
            .run_until(async {
                let observed_membranes = Rc::new(RefCell::new(Vec::new()));
                let executor: system_capnp::executor::Client =
                    capnp_rpc::new_client(RecordingExecutor {
                        observed_membranes: observed_membranes.clone(),
                    });
                let grafts = Rc::new(Cell::new(0));
                let membrane = test_membrane(grafts.clone());

                for _ in 0..2 {
                    let result = spawn_connection_child(&executor, membrane.clone()).await;
                    assert!(result.is_err(), "recording executor intentionally rejects");
                }

                let observed = observed_membranes.take();
                assert_eq!(
                    observed.len(),
                    2,
                    "each connection must receive the registration-time Membrane"
                );
                assert_eq!(
                    grafts.get(),
                    0,
                    "stream child spawning must forward without grafting"
                );
                for (index, membrane) in observed.into_iter().enumerate() {
                    let response = membrane
                        .graft_request()
                        .send()
                        .promise
                        .await
                        .expect("delegated Membrane graft");
                    let extras = response
                        .get()
                        .expect("graft results")
                        .get_extras()
                        .expect("extras");
                    assert_eq!(extras.len(), 1);
                    assert_eq!(
                        response
                            .get()
                            .expect("graft results")
                            .get_peer_id()
                            .expect("stateful peerId"),
                        &((index + 1) as u32).to_be_bytes()
                    );
                    assert_eq!(
                        extras
                            .get(0)
                            .get_name()
                            .expect("extra name")
                            .to_str()
                            .expect("UTF-8 extra"),
                        "application-extra"
                    );
                }
                assert_eq!(
                    grafts.get(),
                    2,
                    "each stream child must reach the supplied Membrane server"
                );
            })
            .await;
    }
}
