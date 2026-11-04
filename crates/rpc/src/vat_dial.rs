//! Paved-path helper for opening a Cap'n Proto vat connection as a client.
//!
//! Wraps an `AsyncRead + AsyncWrite` stream as a Cap'n Proto RPC client vat,
//! drives the `RpcSystem` in a managed background task, and returns the
//! remote's bootstrap capability typed as `C`.
//!
//! # Why this exists
//!
//! capnp-rpc-rust's `RpcSystem` is a `Future` whose `poll` drives I/O on the
//! underlying `VatNetwork` transport.  Every derived `Client`/`Promise` —
//! including the bootstrap cap returned by [`capnp_rpc::RpcSystem::bootstrap`]
//! and the response promise of every method call — depends on the system
//! being polled.  Awaiting a derived promise WITHOUT concurrently polling the
//! system deadlocks: the awaited promise registers a waker on internal state
//! that never advances.
//!
//! The shape of the bug is subtle — the obvious-looking ordering hangs:
//!
//! ```ignore
//! // BUG: rpc_system never polled, first method call hangs forever.
//! let mut rpc_system = RpcSystem::new(network, None);
//! let client: SomeClient = rpc_system.bootstrap(Side::Server);
//! let resp = client.some_method().send().promise.await?;  // <-- hangs
//! tokio::task::spawn_local(rpc_system);                   // never reached
//! ```
//!
//! This helper encapsulates the correct ordering (spawn driver, *then* return
//! the client) so callers cannot get it wrong: the only way to obtain the
//! typed bootstrap `C` is to go through [`connect`], which spawns the driver
//! before returning.
//!
//! # Counterpart
//!
//! Mirrors [`vat_listener`](super::vat_listener) on the listen side, and the
//! cell-side [`std::system::RpcSession`] paved path for guests.
//!
//! # What the driver does on its own
//!
//! Once spawned, the driver flushes the Bootstrap message that
//! `RpcSystem::bootstrap()` queued eagerly, receives the remote's Return, and
//! resolves the underlying `PromiseClient`.  This happens **whether or not
//! the caller ever makes a method call** — the connection is fully live as
//! soon as the driver runs.  What changes when a method call IS made is
//! observability: the call's response promise is an awaitable signal that
//! the roundtrip succeeded (or surfaces an error if it failed).
//!
//! # Why we don't await a handshake check
//!
//! Earlier revisions of this helper awaited `bootstrap_cap.when_resolved()`
//! to verify the remote speaks Cap'n Proto before returning.  Empirically
//! (regression test in this module confirmed it), `when_resolved()` on the
//! cap returned by `RpcSystem::bootstrap()` does not reliably fire in
//! capnp-rpc-rust 0.25 — even after the Bootstrap roundtrip completes and
//! the underlying `PromiseClient` is internally marked `is_resolved`,
//! `when_more_resolved` keeps appending waiters to an already-drained queue.
//!
//! The canonical [capnproto-rust hello-world client] sidesteps this by
//! spawning the system and going straight to method calls — the first call
//! pipelines on the bootstrap, so its response promise IS the handshake
//! observable.  We follow that pattern: callers that want an explicit
//! liveness check should make a lightweight typed call (e.g. shell.eval("")).
//! The `ManagedRpc` owner controls connection lifetime. Dropping the owner
//! requests connection shutdown. Retained imported capabilities may still
//! exist as values, but they do not override that shutdown.
//!
//! # Trade-offs
//!
//! Without an awaited handshake check, `connect` cannot synchronously
//! distinguish "bootstrap roundtrip succeeded" from "bootstrap roundtrip
//! still pending" at return time.  Per-scenario:
//!
//! - **Healthy remote.** The bootstrap RTT is paid pipelined under the
//!   first method call.  Same total latency as the (buggy) `when_resolved`
//!   path would have given, just shifted from connect-time to
//!   first-call-time.
//! - **Healthy remote, cold-cache WASM compile on the other side.** Cost
//!   shifted from `connect` to the first method call's response timeout;
//!   the caller sees their `connect` return immediately, then their first
//!   call wait for compile.
//! - **Remote up but not speaking Cap'n Proto on the negotiated
//!   subprotocol.** Surfaces through the caller's first-method timeout rather
//!   than a precise
//!   `RPC handshake timeout` at connect.  Time-to-failure is unchanged
//!   (~30s either way); **diagnostic precision is slightly reduced**.
//! - **Connection drops mid-session.** No change vs. the prior code:
//!   in-flight promises fail with `Disconnected`.
//! - **Caller never invokes a method.** No 30s connect penalty for
//!   dials that don't end up using the cap. Dropping the `ManagedRpc` owner
//!   requests connection shutdown even if the cap is retained.
//! - **libp2p-level dial failure** (host unreachable, no subprotocol
//!   negotiated).  No change: `vat_dial::connect` is never reached.
//!
//! The diagnostic-precision regression in the "wrong protocol" case is
//! the only qualitative cost.  We accept it because (a) the alternative
//! mechanism (`when_resolved`) was empirically broken in capnp-rpc-rust
//! 0.25; (b) libp2p subprotocol negotiation already established that
//! the peer claims to speak our exact capnp interface, so this case is
//! rare in practice; (c) the canonical capnproto-rust pattern operates
//! the same way.
//!
//! [capnproto-rust hello-world client]: https://github.com/capnproto/capnproto-rust/blob/master/capnp-rpc/examples/hello-world/client.rs

use crate::managed_rpc::ManagedRpcSystem as RpcSystem;
use capnp::capability::FromClientHook;
use capnp_rpc::rpc_twoparty_capnp::Side;
use capnp_rpc::twoparty::VatNetwork;
use futures::io::{AsyncRead, AsyncReadExt, AsyncWrite};

/// A bootstrapped Cap'n Proto vat connection.
///
/// Owns a typed bootstrap capability and its managed RPC driver.
/// Dropping the driver or calling [`Self::abort`] requests disconnection;
/// awaiting it joins bounded teardown. Retained clients then become unusable.
pub struct VatDial<C> {
    /// The remote's bootstrap capability, ready for use.  The first method
    /// call on this client triggers the Bootstrap roundtrip (pipelined).
    pub bootstrap: C,
    /// Managed owner of the `RpcSystem` driver task. The inner result
    /// is the `RpcSystem` outcome (`Ok` = clean close, `Err` = RPC error).
    pub driver: crate::managed_rpc::ManagedRpc,
}

impl<C> VatDial<C> {
    /// Request managed disconnection.  After calling this, the bootstrap
    /// capability and any pipelined references become unusable.
    pub fn abort(&self) {
        self.driver.shutdown();
    }
}

/// Open a Cap'n Proto vat as a client over the given stream and return its
/// bootstrap capability typed as `C`.
///
/// The `RpcSystem` is spawned as a managed `tokio::task::spawn_local` task
/// **before** returning; once that task is polled (which happens as soon as
/// control yields), it flushes the Bootstrap message and receives the remote
/// Return.  Must be called from within a [`tokio::task::LocalSet`]
/// (capnp-rpc-rust is single-threaded).
///
/// The returned client is immediately usable.  Bootstrap traffic flows on its
/// own; the caller does not need to make a method call to "kick" anything.
/// What a method call provides is an awaitable Promise that observes whether
/// the roundtrip succeeded — useful as an explicit liveness check, but not
/// required for the connection to function.
///
/// # Type parameters
///
/// * `S`: the underlying stream type (e.g., `libp2p::Stream`).  Must be
///   `'static` because the stream is moved into the spawned driver task.
/// * `C`: the typed bootstrap capability (any `capnp_rpc`-generated client).
///   Use [`capnp::capability::Client`] for an untyped/generic bootstrap.
pub fn connect<S, C>(stream: S) -> VatDial<C>
where
    S: AsyncRead + AsyncWrite + 'static,
    C: FromClientHook,
{
    let (reader, writer) = Box::pin(stream).split();
    let network = VatNetwork::new(reader, writer, Side::Client, Default::default());
    let mut rpc_system = RpcSystem::new(Box::new(network), None);
    let bootstrap_cap: capnp::capability::Client = rpc_system.bootstrap(Side::Server);

    // CRITICAL ORDERING: spawn the driver BEFORE returning. Derived promises
    // (method calls, when_resolved, etc.) only make progress while the
    // RpcSystem is being polled.
    let driver = crate::managed_rpc::ManagedRpc::spawn(rpc_system);

    let typed: C = FromClientHook::new(bootstrap_cap.hook);
    VatDial {
        bootstrap: typed,
        driver,
    }
}

/// The caller's wait for a Cell-owned dial. Dropping it cancels admission to a
/// session until commit; an already committed session belongs to the Cell.
/// Buffered success is rejected if scope closure wins the final delivery check.
pub struct OwnedDial<C> {
    response: tokio::sync::oneshot::Receiver<capnp::Result<C>>,
    cancelled: tokio_util::sync::CancellationToken,
    closure: crate::outbound::EffectsCloseHandle,
}

impl<C> std::future::Future for OwnedDial<C> {
    type Output = capnp::Result<C>;
    fn poll(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Self::Output> {
        let result = std::task::ready!(std::pin::Pin::new(&mut self.response).poll(cx))
            .unwrap_or_else(|_| Err(capnp::Error::failed("outbound dial worker lost".into())));
        // Session ownership commits in the worker. Caller delivery is a second
        // decision: a buffered success must not survive closure-winning delivery.
        // The gate check ends before dropping C or invoking caller code.
        let result = match result {
            Ok(client) if self.closure.is_closed() => {
                drop(client);
                Err(capnp::Error::failed(
                    "outbound Cell closed before result delivery".into(),
                ))
            }
            result => result,
        };
        std::task::Poll::Ready(result)
    }
}

impl<C> Drop for OwnedDial<C> {
    fn drop(&mut self) {
        self.cancelled.cancel();
    }
}

/// Admit before suspension, then supervise the native operation independently
/// of its caller. Once polled, `open` is retained until its actual result: a
/// dropped libp2p wait has no backend cancellation acknowledgement. The returned
/// wait can time out without releasing the attempt's accounting prematurely.
/// Submission authorization and Cell closure share a gate. Authorization commits
/// to supervised drain; it does not acknowledge native enqueue or emission.
/// Authorized work may first poll or emit after closure, but cannot escape its
/// receipt or certify cleanup before its terminal result and worker join.
///
/// The supplied future must submit no native work before its first poll.
/// Dropping the unpolled future must leave no native obligation. Once polled,
/// the future must return only after the native operation is terminal.
/// Use `connect_libp2p_owned` for libp2p; its channel errors need classification.
pub fn connect_owned<S, C, F>(
    registry: &crate::outbound::OutboundRegistry,
    origin: Option<capnp::private::capability::InvocationOrigin>,
    open: F,
) -> capnp::Result<OwnedDial<C>>
where
    S: AsyncRead + AsyncWrite + 'static,
    C: FromClientHook + 'static,
    F: std::future::Future<Output = capnp::Result<S>> + 'static,
{
    connect_owned_classified(
        registry,
        origin,
        async move {
            open.await.map_err(|error| OpenFailure {
                error,
                terminal: true,
            })
        },
        DialMetadata::default(),
    )
}

#[derive(Default)]
struct DialMetadata {
    peer: Option<libp2p::PeerId>,
    protocol: Option<libp2p::StreamProtocol>,
    service: Option<String>,
}

/// A pinned-source completion classification. A channel disappearing does not
/// acknowledge destruction of a handler-side upgrade. Keep those permits
/// quarantined even when the outer task has joined.
struct OpenFailure {
    error: capnp::Error,
    terminal: bool,
}

fn classify_open_error(error: libp2p_stream::OpenStreamError) -> OpenFailure {
    use futures::channel::{mpsc, oneshot};
    use libp2p_stream::OpenStreamError;
    let terminal = match &error {
        OpenStreamError::UnsupportedProtocol(_) => true,
        OpenStreamError::Io(error) => !error.get_ref().is_some_and(|source| {
            source.is::<oneshot::Canceled>() || source.is::<mpsc::SendError>()
        }),
        _ => false,
    };
    OpenFailure {
        error: capnp::Error::failed(format!("failed to open stream: {error}")),
        terminal,
    }
}

/// Retain a pinned libp2p-stream 0.5.0-alpha request through its actual result.
/// Requires default V1 substream negotiation, never V1Lazy. Public libp2p does
/// not expose whether a submitted request is queued or actively negotiating.
/// Cancellation suppresses RPC commit; it cannot stop an admitted upgrade.
/// A result-channel loss quarantines capacity instead of certifying cleanup.
/// Known completion releases the local negotiation obligation after late-stream
/// disposal and worker join. It does not acknowledge muxer reclamation or remote
/// rollback. Re-audit error provenance when upgrading libp2p-stream.
pub fn connect_libp2p_owned<C>(
    registry: &crate::outbound::OutboundRegistry,
    origin: Option<capnp::private::capability::InvocationOrigin>,
    control: libp2p_stream::Control,
    peer: libp2p::PeerId,
    protocol: libp2p::StreamProtocol,
) -> capnp::Result<OwnedDial<C>>
where
    C: FromClientHook + 'static,
{
    connect_libp2p_owned_logged(registry, origin, control, peer, protocol, None)
}

pub(crate) fn connect_libp2p_owned_logged<C>(
    registry: &crate::outbound::OutboundRegistry,
    origin: Option<capnp::private::capability::InvocationOrigin>,
    mut control: libp2p_stream::Control,
    peer: libp2p::PeerId,
    protocol: libp2p::StreamProtocol,
    service: Option<String>,
) -> capnp::Result<OwnedDial<C>>
where
    C: FromClientHook + 'static,
{
    let metadata = DialMetadata {
        peer: Some(peer),
        protocol: Some(protocol.clone()),
        service,
    };
    connect_owned_classified(
        registry,
        origin,
        async move {
            control
                .open_stream(peer, protocol)
                .await
                .map_err(classify_open_error)
        },
        metadata,
    )
}

fn connect_owned_classified<S, C, F>(
    registry: &crate::outbound::OutboundRegistry,
    origin: Option<capnp::private::capability::InvocationOrigin>,
    open: F,
    metadata: DialMetadata,
) -> capnp::Result<OwnedDial<C>>
where
    S: AsyncRead + AsyncWrite + 'static,
    C: FromClientHook + 'static,
    F: std::future::Future<Output = Result<S, OpenFailure>> + 'static,
{
    use crate::outbound::EffectOutcome;
    let admission = registry.admit(origin)?;
    // The retained worker runs independently of the opening call. Preserve
    // causal correlation without making that call the session's parent.
    let opening = tracing::Span::current();
    let scope = admission.scope.clone();
    let submission = admission.submission();
    let closure = scope.close_handle();
    let cancelled = tokio_util::sync::CancellationToken::new();
    let worker_cancelled = cancelled.clone();
    let (response, receiver) = tokio::sync::oneshot::channel();
    let worker = tokio::task::spawn_local(async move {
        if worker_cancelled.is_cancelled() || response.is_closed() || !submission.authorize() {
            let _ = response.send(Err(capnp::Error::failed(
                "outbound dial cancelled before start".into(),
            )));
            return EffectOutcome::default();
        }
        tracing::debug!(
            parent: &opening,
            peer = metadata.peer.as_ref().map(tracing::field::display),
            protocol = metadata.protocol.as_ref().map(tracing::field::display),
            service = metadata.service.as_deref(),
            "dialing vat service"
        );
        let stream = match open.await {
            Ok(stream) => stream,
            Err(failure) => {
                let _ = response.send(Err(failure.error));
                return EffectOutcome {
                    lost: !failure.terminal,
                    ..Default::default()
                };
            }
        };
        if scope.is_closed() || worker_cancelled.is_cancelled() || response.is_closed() {
            drop(stream);
            let _ = response.send(Err(capnp::Error::failed(
                "outbound dial cancelled before commit".into(),
            )));
            return EffectOutcome::default();
        }
        // The actual connection continues to classify incoming calls as remote
        // ingress. A Cell owns its lifetime, not that remote principal's calls.
        // Construction can invoke arbitrary FromClientHook implementations.
        // Prepare outside the close gate, then atomically adopt or join it.
        let VatDial {
            bootstrap,
            mut driver,
        } = opening.in_scope(|| connect::<_, C>(stream));
        // ManagedRpc retained the follows_from link. Session completion must
        // stay outside the opening call's span and does not need its handle.
        drop(opening);
        let committed = !worker_cancelled.is_cancelled() && !response.is_closed() && scope.commit();
        let rpc_id = driver.rpc_id();
        // The worker owns the driver before making its capability visible.
        if committed {
            let _ = response.send(Ok(bootstrap));
        } else {
            drop(bootstrap);
            let _ = response.send(Err(capnp::Error::failed(
                "outbound dial closed before commit".into(),
            )));
            driver.shutdown();
        }
        let joined = tokio::select! {
            biased;
            _ = scope.closed.cancelled() => driver.shutdown_and_join().await,
            result = &mut driver => result,
        };
        match joined {
            Ok(result) => {
                let rpc_error = result.is_err();
                if let Err(error) = result {
                    tracing::warn!(
                        event_code = crate::observability::EventCode::RpcFailed as u16,
                        rpc_id,
                        peer = metadata.peer.as_ref().map(tracing::field::display),
                        protocol = metadata.protocol.as_ref().map(tracing::field::display),
                        service = metadata.service.as_deref(),
                        error = &error as &dyn std::error::Error,
                        "vat dial session ended with error: {error}"
                    );
                } else {
                    tracing::trace!(
                        rpc_id,
                        peer = metadata.peer.as_ref().map(tracing::field::display),
                        protocol = metadata.protocol.as_ref().map(tracing::field::display),
                        service = metadata.service.as_deref(),
                        "vat dial session ended cleanly"
                    );
                }
                EffectOutcome {
                    session_joined: committed,
                    rpc_error,
                    lost: false,
                }
            }
            Err(error) => {
                tracing::warn!(
                    event_code = crate::observability::EventCode::RpcFailed as u16,
                    rpc_id,
                    peer = metadata.peer.as_ref().map(tracing::field::display),
                    protocol = metadata.protocol.as_ref().map(tracing::field::display),
                    service = metadata.service.as_deref(),
                    error = &error as &dyn std::error::Error,
                    "vat dial driver task failed: {error}"
                );
                EffectOutcome {
                    lost: true,
                    ..Default::default()
                }
            }
        }
    });
    // Keep accounting in a separate join monitor. A worker returning is not yet
    // its join acknowledgement, and losing either worker cannot certify cleanup.
    tokio::task::spawn_local(crate::local_tasks::track(async move {
        let outcome = worker.await.unwrap_or_else(|error| {
            tracing::warn!(error = %error, "outbound attempt worker lost");
            EffectOutcome {
                lost: true,
                ..Default::default()
            }
        });
        admission.finish(outcome);
    }));
    Ok(OwnedDial {
        response: receiver,
        cancelled,
        closure,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::time::Duration;

    use authority::system_capnp;
    use capnp::capability::Promise;
    use capnp_rpc::rpc_twoparty_capnp::Side;
    use capnp_rpc::twoparty::VatNetwork;
    use capnp_rpc::RpcSystem;
    use tokio::io;
    use tokio_util::compat::{TokioAsyncReadCompatExt, TokioAsyncWriteCompatExt};

    #[tokio::test(flavor = "current_thread")]
    async fn closed_scope_rejects_buffered_dial_result() {
        tokio::task::LocalSet::new()
            .run_until(async {
                let registry = crate::outbound::OutboundRegistry::default();
                let owner = registry.register().unwrap();
                let stream = make_membrane_server(vec![1, 2, 3]);
                let dial = connect_owned::<_, system_capnp::membrane::Client, _>(
                    &registry,
                    Some(owner.origin()),
                    async { Ok(stream.compat_write()) },
                )
                .unwrap();
                tokio::time::timeout(Duration::from_secs(2), async {
                    while owner.completion().snapshot().committed != 1 {
                        tokio::task::yield_now().await;
                    }
                })
                .await
                .unwrap();
                let done = tokio::time::timeout(Duration::from_secs(2), owner.close_and_join())
                    .await
                    .unwrap()
                    .unwrap();
                assert_eq!((done.active, done.committed, done.joined), (0, 1, 1));
                assert!(
                    dial.await.is_err(),
                    "closure before caller delivery must reject buffered success"
                );
            })
            .await;
    }

    struct TestMembrane {
        peer_id: Vec<u8>,
    }

    #[allow(refining_impl_trait)]
    impl system_capnp::membrane::Server for TestMembrane {
        fn graft(
            self: capnp::capability::Rc<Self>,
            _params: system_capnp::membrane::GraftParams,
            mut results: system_capnp::membrane::GraftResults,
        ) -> Promise<(), capnp::Error> {
            results.get().set_peer_id(&self.peer_id);
            Promise::ok(())
        }
    }

    /// Serve one typed Membrane over a duplex pair and return the dial side.
    fn make_membrane_server(peer_id: Vec<u8>) -> io::DuplexStream {
        let (client_stream, server_stream) = io::duplex(8 * 1024);
        let (server_read, server_write) = io::split(server_stream);
        let membrane: system_capnp::membrane::Client =
            capnp_rpc::new_client(TestMembrane { peer_id });
        let network = VatNetwork::new(
            server_read.compat(),
            server_write.compat_write(),
            Side::Server,
            Default::default(),
        );
        let server_rpc = RpcSystem::new(Box::new(network), Some(membrane.client));
        tokio::task::spawn_local(async move {
            let _ = server_rpc.await;
        });

        client_stream
    }

    #[test]
    fn channel_loss_is_not_a_native_terminal_acknowledgement() {
        use libp2p_stream::OpenStreamError;
        use std::io;
        let (sender, receiver) = futures::channel::oneshot::channel::<()>();
        drop(sender);
        let cancelled = futures::executor::block_on(receiver).unwrap_err();
        assert!(
            !classify_open_error(OpenStreamError::Io(io::Error::new(
                io::ErrorKind::ConnectionReset,
                cancelled,
            )))
            .terminal
        );
        let (mut sender, receiver) = futures::channel::mpsc::channel::<()>(0);
        drop(receiver);
        let failed = sender.try_send(()).unwrap_err().into_send_error();
        assert!(
            !classify_open_error(OpenStreamError::Io(io::Error::new(
                io::ErrorKind::ConnectionReset,
                failed,
            )))
            .terminal
        );
        assert!(
            classify_open_error(OpenStreamError::Io(io::Error::from(
                io::ErrorKind::TimedOut,
            )))
            .terminal
        );
        assert!(
            classify_open_error(OpenStreamError::UnsupportedProtocol(
                libp2p::StreamProtocol::new("/not-supported"),
            ))
            .terminal
        );
    }

    /// Regression test for the original `ww shell` 30s handshake bug
    /// (https://github.com/wetware/ww/issues/450).
    ///
    /// `connect` must spawn the `RpcSystem` driver before returning so that
    /// derived promises can resolve.  If anyone reorders `connect` so the
    /// driver isn't spawned, the method call below hangs until the 2s
    /// timeout — failing this test.
    #[tokio::test]
    async fn connect_returns_working_client() {
        let local = tokio::task::LocalSet::new();
        local
            .run_until(async {
                let client_stream = make_membrane_server(vec![1, 2, 3, 4]);

                let conn: VatDial<system_capnp::membrane::Client> =
                    connect(client_stream.compat_write());

                // First method call drives the Bootstrap roundtrip.
                // If `connect` didn't spawn the driver, this hangs forever.
                let resp = tokio::time::timeout(
                    Duration::from_secs(2),
                    conn.bootstrap.graft_request().send().promise,
                )
                .await
                .expect("method call should not time out — driver must be running")
                .expect("graft RPC should succeed");
                let peer_id = resp.get().unwrap().get_peer_id().unwrap();
                assert_eq!(peer_id, &[1, 2, 3, 4]);
            })
            .await;
    }

    /// After multiple method calls, the rpc_system driver must still be
    /// polling — the second call must also succeed.  Catches a regression
    /// where the driver might be replaced by a "one-shot" mechanism.
    #[tokio::test]
    async fn connect_supports_multiple_calls() {
        let local = tokio::task::LocalSet::new();
        local
            .run_until(async {
                let client_stream = make_membrane_server(vec![9, 8, 7, 6]);

                let conn: VatDial<system_capnp::membrane::Client> =
                    connect(client_stream.compat_write());

                // First call.
                let resp = tokio::time::timeout(
                    Duration::from_secs(2),
                    conn.bootstrap.graft_request().send().promise,
                )
                .await
                .expect("first call")
                .expect("first id");
                assert_eq!(resp.get().unwrap().get_peer_id().unwrap(), &[9, 8, 7, 6]);

                // Second call must also succeed.
                let resp2 = tokio::time::timeout(
                    Duration::from_secs(2),
                    conn.bootstrap.graft_request().send().promise,
                )
                .await
                .expect("second call")
                .expect("second id");
                assert_eq!(resp2.get().unwrap().get_peer_id().unwrap(), &[9, 8, 7, 6]);
            })
            .await;
    }

    /// When the transport closes before any traffic flows, the first method
    /// call must fail with an error (not hang forever).  Drives the second
    /// half of the bug-class: forgetting to spawn the driver would also
    /// hide transport failures.
    #[tokio::test]
    async fn connect_first_call_errors_on_closed_transport() {
        let local = tokio::task::LocalSet::new();
        local
            .run_until(async {
                let (client_stream, server_stream) = io::duplex(8 * 1024);
                drop(server_stream);

                let conn: VatDial<capnp::capability::Client> =
                    connect(client_stream.compat_write());

                // Issue a bootstrap call on the generic client. Without
                // knowing the interface we use an arbitrary interface_id +
                // method_id; the call must surface a transport error.
                let req: capnp::capability::Request<
                    capnp::any_pointer::Owned,
                    capnp::any_pointer::Owned,
                > = conn.bootstrap.new_call(0xdead_beef_dead_beefu64, 0, None);
                let res = tokio::time::timeout(Duration::from_secs(2), req.send().promise).await;
                match res {
                    Ok(Err(_)) => {} // expected: RPC error from closed transport
                    Ok(Ok(_)) => panic!("expected error from closed transport, got Ok"),
                    Err(_) => panic!("call hung on closed transport — driver missing?"),
                }
            })
            .await;
    }
}
