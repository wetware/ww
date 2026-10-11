//! StreamDialer capability: open outgoing libp2p subprotocol streams to remote peers.
//!
//! The `StreamDialer` capability lets a guest open a libp2p stream to a specific peer
//! on a named subprotocol. The host opens the stream and returns a bidirectional
//! `ByteStream` capability — the guest reads/writes whatever wire protocol it
//! wants directly.

use authority::EpochGuard;
use capnp::capability::Promise;
use capnp_rpc::pry;
use futures::io::{AsyncRead, AsyncReadExt, AsyncWrite};
use libp2p::PeerId;
use std::time::Duration;
use tokio::io::{self, AsyncWriteExt};
use tokio_util::compat::{FuturesAsyncReadCompatExt, FuturesAsyncWriteCompatExt};

use authority::system_capnp;

use super::{ByteStreamImpl, StreamMode};

/// Timeout for establishing the libp2p stream to a remote peer.
const DIAL_TIMEOUT: Duration = Duration::from_secs(30);
const DIAL_BUFFER_BYTES: usize = 64 * 1024;

fn dialed_byte_stream<R, W>(stream_read: R, stream_write: W) -> ByteStreamImpl
where
    R: AsyncRead + Unpin + 'static,
    W: AsyncWrite + Unpin + 'static,
{
    let (host_side, guest_side) = io::duplex(DIAL_BUFFER_BYTES);
    let (mut host_read, mut host_write) = io::split(host_side);

    let inbound = tokio::task::spawn_local(async move {
        match io::copy(&mut stream_read.compat(), &mut host_write).await {
            Ok(_) => {
                if let Err(error) = host_write.shutdown().await {
                    tracing::debug!("stream→host shutdown error: {error}");
                }
            }
            Err(error) => tracing::debug!("stream→host pump error: {error}"),
        }
    });

    let outbound = tokio::task::spawn_local(async move {
        let mut compat_write = stream_write.compat_write();
        if let Err(error) = io::copy(&mut host_read, &mut compat_write).await {
            tracing::debug!("host→stream pump error: {error}");
        }
    });

    ByteStreamImpl::new_with_pump_abort_handles(
        guest_side,
        StreamMode::Bidirectional,
        [inbound.abort_handle(), outbound.abort_handle()],
    )
}

pub struct StreamDialerImpl {
    stream_control: libp2p_stream::Control,
    guard: EpochGuard,
}

impl StreamDialerImpl {
    pub fn new(stream_control: libp2p_stream::Control, guard: EpochGuard) -> Self {
        Self {
            stream_control,
            guard,
        }
    }
}

#[allow(refining_impl_trait)]
impl system_capnp::stream_dialer::Server for StreamDialerImpl {
    fn dial(
        self: capnp::capability::Rc<Self>,
        params: system_capnp::stream_dialer::DialParams,
        mut results: system_capnp::stream_dialer::DialResults,
    ) -> Promise<(), capnp::Error> {
        pry!(self.guard.check());

        let params = pry!(params.get());
        let peer_bytes = pry!(params.get_peer()).to_vec();
        let protocol_str = pry!(pry!(params.get_protocol())
            .to_str()
            .map_err(|e| capnp::Error::failed(e.to_string())));

        let peer_id = pry!(PeerId::from_bytes(&peer_bytes)
            .map_err(|e| capnp::Error::failed(format!("invalid peer ID: {e}"))));

        let stream_protocol = pry!(super::stream_protocol(protocol_str));

        let mut control = self.stream_control.clone();

        Promise::from_future(async move {
            tracing::debug!(
                peer = %peer_id,
                protocol = %stream_protocol,
                "Dialing stream subprotocol"
            );

            let stream = tokio::time::timeout(
                DIAL_TIMEOUT,
                control.open_stream(peer_id, stream_protocol.clone()),
            )
            .await
            .map_err(|_| {
                capnp::Error::failed(format!(
                    "timeout dialing {peer_id} on {stream_protocol} after {DIAL_TIMEOUT:?}"
                ))
            })?
            .map_err(|e| {
                capnp::Error::failed(format!(
                    "failed to open stream to {peer_id} on {stream_protocol}: {e}"
                ))
            })?;

            // Split the libp2p stream for bidirectional pumping. The returned
            // ByteStream owns abort handles for both pump tasks.
            let (stream_read, stream_write) = Box::pin(stream).split();
            let stream_cap: system_capnp::byte_stream::Client =
                capnp_rpc::new_client(dialed_byte_stream(stream_read, stream_write));
            results.get().set_stream(stream_cap);

            Ok(())
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::io::{AsyncRead, AsyncWrite};
    use std::pin::Pin;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;
    use std::task::{Context, Poll};
    use tokio::io::AsyncReadExt as _;
    use tokio::sync::oneshot;
    use tokio_util::compat::{TokioAsyncReadCompatExt, TokioAsyncWriteCompatExt};

    struct DropTracked<T> {
        inner: T,
        dropped: Arc<AtomicBool>,
        notification: Option<oneshot::Sender<()>>,
    }

    impl<T> DropTracked<T> {
        fn new(inner: T) -> (Self, DropMonitor) {
            let dropped = Arc::new(AtomicBool::new(false));
            let (notification, receiver) = oneshot::channel();
            (
                Self {
                    inner,
                    dropped: dropped.clone(),
                    notification: Some(notification),
                },
                DropMonitor { dropped, receiver },
            )
        }
    }

    impl<T> Drop for DropTracked<T> {
        fn drop(&mut self) {
            self.dropped.store(true, Ordering::SeqCst);
            if let Some(notification) = self.notification.take() {
                let _ = notification.send(());
            }
        }
    }

    struct DropMonitor {
        dropped: Arc<AtomicBool>,
        receiver: oneshot::Receiver<()>,
    }

    impl DropMonitor {
        fn is_dropped(&self) -> bool {
            self.dropped.load(Ordering::SeqCst)
        }

        async fn wait(&mut self) {
            (&mut self.receiver)
                .await
                .expect("drop notification sender disappeared");
            assert!(self.is_dropped());
        }
    }

    impl<T: AsyncRead + Unpin> AsyncRead for DropTracked<T> {
        fn poll_read(
            mut self: Pin<&mut Self>,
            cx: &mut Context<'_>,
            buffer: &mut [u8],
        ) -> Poll<std::io::Result<usize>> {
            Pin::new(&mut self.inner).poll_read(cx, buffer)
        }
    }

    impl<T: AsyncWrite + Unpin> AsyncWrite for DropTracked<T> {
        fn poll_write(
            mut self: Pin<&mut Self>,
            cx: &mut Context<'_>,
            buffer: &[u8],
        ) -> Poll<std::io::Result<usize>> {
            Pin::new(&mut self.inner).poll_write(cx, buffer)
        }

        fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
            Pin::new(&mut self.inner).poll_flush(cx)
        }

        fn poll_close(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
            Pin::new(&mut self.inner).poll_close(cx)
        }
    }

    fn tracked_dialer_stream(
        capacity: usize,
    ) -> (
        system_capnp::byte_stream::Client,
        io::DuplexStream,
        DropMonitor,
        DropMonitor,
    ) {
        let (network_side, network_peer) = io::duplex(capacity);
        let (network_read, network_write) = io::split(network_side);
        let (network_read, read_drop) = DropTracked::new(network_read.compat());
        let (network_write, write_drop) = DropTracked::new(network_write.compat_write());
        let stream = dialed_byte_stream(network_read, network_write);
        (
            capnp_rpc::new_client(stream),
            network_peer,
            read_drop,
            write_drop,
        )
    }

    #[tokio::test]
    async fn peer_half_close_reaches_guest_eof() {
        tokio::task::LocalSet::new()
            .run_until(async {
                let (client, mut network_peer, mut read_drop, _write_drop) =
                    tracked_dialer_stream(64);
                network_peer.write_all(b"request").await.unwrap();
                network_peer.shutdown().await.unwrap();
                read_drop.wait().await;

                let mut request = client.read_request();
                request.get().set_max_bytes(7);
                let response = request.send().promise.await.unwrap();
                assert_eq!(response.get().unwrap().get_data().unwrap(), b"request");

                let mut request = client.read_request();
                request.get().set_max_bytes(1);
                let read = request.send().promise;
                tokio::pin!(read);
                let response = match futures::poll!(read.as_mut()) {
                    Poll::Ready(result) => result.expect("guest EOF read failed"),
                    Poll::Pending => panic!("guest read remained pending after peer EOF"),
                };
                assert!(response.get().unwrap().get_data().unwrap().is_empty());
            })
            .await;
    }

    #[tokio::test]
    async fn peer_half_close_preserves_guest_output() {
        tokio::task::LocalSet::new()
            .run_until(async {
                let (client, mut network_peer, mut read_drop, mut write_drop) =
                    tracked_dialer_stream(64);
                network_peer.shutdown().await.unwrap();
                read_drop.wait().await;

                let mut request = client.read_request();
                request.get().set_max_bytes(1);
                let response = request.send().promise.await.unwrap();
                assert!(response.get().unwrap().get_data().unwrap().is_empty());
                assert!(!write_drop.is_dropped());

                let mut request = client.write_request();
                request.get().set_data(b"response");
                request.send().promise.await.unwrap();
                let mut response = [0u8; 8];
                {
                    let receive = network_peer.read_exact(&mut response);
                    tokio::pin!(receive);
                    tokio::select! {
                        result = &mut receive => {
                            result.unwrap();
                        }
                        () = write_drop.wait() => {
                            panic!("outbound pump dropped before peer received response")
                        }
                    }
                }
                assert_eq!(&response, b"response");
                assert!(!write_drop.is_dropped());

                client.close_request().send().promise.await.unwrap();
                write_drop.wait().await;
            })
            .await;
    }

    #[tokio::test]
    async fn idle_guest_output_remains_live_after_peer_eof() {
        tokio::task::LocalSet::new()
            .run_until(async {
                let (client, mut network_peer, mut read_drop, mut write_drop) =
                    tracked_dialer_stream(1);
                network_peer.shutdown().await.unwrap();
                read_drop.wait().await;

                let mut request = client.read_request();
                request.get().set_max_bytes(1);
                let response = request.send().promise.await.unwrap();
                assert!(response.get().unwrap().get_data().unwrap().is_empty());
                assert!(!write_drop.is_dropped());

                client.close_request().send().promise.await.unwrap();
                write_drop.wait().await;
            })
            .await;
    }

    #[tokio::test]
    async fn close_cancels_pumps_blocked_on_network_input() {
        tokio::task::LocalSet::new()
            .run_until(async {
                let (client, mut network_peer, mut read_drop, mut write_drop) =
                    tracked_dialer_stream(1);
                network_peer.write_all(b"x").await.unwrap();
                let mut request = client.read_request();
                request.get().set_max_bytes(1);
                let response = request.send().promise.await.unwrap();
                assert_eq!(response.get().unwrap().get_data().unwrap(), b"x");
                assert!(!read_drop.is_dropped());
                assert!(!write_drop.is_dropped());

                client.close_request().send().promise.await.unwrap();
                read_drop.wait().await;
                write_drop.wait().await;
            })
            .await;
    }

    #[tokio::test]
    async fn close_cancels_pump_blocked_on_network_output() {
        tokio::task::LocalSet::new()
            .run_until(async {
                let (client, mut network_peer, mut read_drop, mut write_drop) =
                    tracked_dialer_stream(1);
                let mut request = client.write_request();
                let payload = vec![b'x'; DIAL_BUFFER_BYTES];
                request.get().set_data(&payload);
                request.send().promise.await.unwrap();

                let mut accepted = [0u8; 1];
                network_peer.read_exact(&mut accepted).await.unwrap();
                assert_eq!(accepted, [b'x']);
                assert!(!write_drop.is_dropped());

                client.close_request().send().promise.await.unwrap();
                read_drop.wait().await;
                write_drop.wait().await;
            })
            .await;
    }

    #[tokio::test]
    async fn dropping_byte_stream_server_cancels_both_pumps() {
        tokio::task::LocalSet::new()
            .run_until(async {
                let (client, mut network_peer, mut read_drop, mut write_drop) =
                    tracked_dialer_stream(1);
                network_peer.write_all(b"x").await.unwrap();
                let mut request = client.read_request();
                request.get().set_max_bytes(1);
                let response = request.send().promise.await.unwrap();
                assert_eq!(response.get().unwrap().get_data().unwrap(), b"x");

                drop(client);
                read_drop.wait().await;
                write_drop.wait().await;
            })
            .await;
    }
}
