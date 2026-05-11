//! Paved-path helper for opening a Cap'n Proto vat connection as a client.
//!
//! Wraps an `AsyncRead + AsyncWrite` stream as a Cap'n Proto RPC client vat,
//! drives the `RpcSystem` in a detached background task, and returns the
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
//! Callers that just hold the cap pay no penalty — the connection idles
//! cleanly until the cap is dropped.
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
//!   subprotocol.** Surfaces as a first-method-call timeout (e.g.
//!   `eval timeout (30s)` in `ww shell`) rather than a precise
//!   `RPC handshake timeout` at connect.  Time-to-failure is unchanged
//!   (~30s either way); **diagnostic precision is slightly reduced**.
//! - **Connection drops mid-session.** No change vs. the prior code:
//!   in-flight promises fail with `Disconnected`.
//! - **Caller never invokes a method.** No 30s connect penalty for
//!   dials that don't end up using the cap; connection idles cleanly
//!   until the cap is dropped.  Pure improvement.
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

use capnp::capability::FromClientHook;
use capnp_rpc::rpc_twoparty_capnp::Side;
use capnp_rpc::twoparty::VatNetwork;
use capnp_rpc::RpcSystem;
use futures::io::{AsyncRead, AsyncReadExt, AsyncWrite};

/// A bootstrapped Cap'n Proto vat connection.
///
/// Holds the typed bootstrap capability plus a `JoinHandle` to the
/// detached `RpcSystem` driver task.  Dropping a `VatDial` detaches the
/// driver — the underlying connection closes naturally when all derived
/// clients are dropped or the transport fails.  Call [`Self::abort`] to
/// actively cancel the driver.
///
/// The driver `JoinHandle` carries the eventual `RpcSystem` result: `Ok(())`
/// means the remote disconnected cleanly, `Err(e)` means the connection ended
/// with a transport- or protocol-level error.  Callers wanting that signal can
/// `.await` `driver` and match on the inner result.
pub struct VatDial<C> {
    /// The remote's bootstrap capability, ready for use.  The first method
    /// call on this client triggers the Bootstrap roundtrip (pipelined).
    pub bootstrap: C,
    /// JoinHandle for the spawned `RpcSystem` driver task.  The inner result
    /// is the `RpcSystem` outcome (`Ok` = clean close, `Err` = RPC error).
    pub driver: tokio::task::JoinHandle<Result<(), capnp::Error>>,
}

impl<C> VatDial<C> {
    /// Actively cancel the driver task.  After calling this, the bootstrap
    /// capability and any pipelined references become unusable.
    pub fn abort(&self) {
        self.driver.abort();
    }
}

/// Open a Cap'n Proto vat as a client over the given stream and return its
/// bootstrap capability typed as `C`.
///
/// The `RpcSystem` is spawned as a detached `tokio::task::spawn_local` task
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
    let driver = tokio::task::spawn_local(rpc_system);

    let typed: C = FromClientHook::new(bootstrap_cap.hook);
    VatDial {
        bootstrap: typed,
        driver,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::time::Duration;

    use crate::{build_peer_rpc, NetworkState, SwarmCommand};
    use membrane::system_capnp;
    use tokio::io;
    use tokio::sync::mpsc;
    use tokio_util::compat::TokioAsyncWriteCompatExt;

    /// Helper: spin up a server-side `host::Client` over a duplex pair and
    /// return the *client-side* half of the duplex for the test to dial.
    fn make_host_server(
        local_peer_id: Vec<u8>,
    ) -> (io::DuplexStream, mpsc::Receiver<SwarmCommand>) {
        let (client_stream, server_stream) = io::duplex(8 * 1024);
        let (server_read, server_write) = io::split(server_stream);

        let network_state = NetworkState::from_peer_id(local_peer_id);
        let (swarm_tx, swarm_rx) = mpsc::channel(16);

        let server_rpc = build_peer_rpc(server_read, server_write, network_state, swarm_tx, false);
        tokio::task::spawn_local(async move {
            let _ = server_rpc.await;
        });

        (client_stream, swarm_rx)
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
                let (client_stream, _swarm_rx) = make_host_server(vec![1, 2, 3, 4]);

                let conn: VatDial<system_capnp::host::Client> =
                    connect(client_stream.compat_write());

                // First method call drives the Bootstrap roundtrip.
                // If `connect` didn't spawn the driver, this hangs forever.
                let resp = tokio::time::timeout(
                    Duration::from_secs(2),
                    conn.bootstrap.id_request().send().promise,
                )
                .await
                .expect("method call should not time out — driver must be running")
                .expect("id RPC should succeed");
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
                let (client_stream, _swarm_rx) = make_host_server(vec![9, 8, 7, 6]);

                let conn: VatDial<system_capnp::host::Client> =
                    connect(client_stream.compat_write());

                // First call.
                let resp = tokio::time::timeout(
                    Duration::from_secs(2),
                    conn.bootstrap.id_request().send().promise,
                )
                .await
                .expect("first call")
                .expect("first id");
                assert_eq!(resp.get().unwrap().get_peer_id().unwrap(), &[9, 8, 7, 6]);

                // Second call must also succeed.
                let resp2 = tokio::time::timeout(
                    Duration::from_secs(2),
                    conn.bootstrap.id_request().send().promise,
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
