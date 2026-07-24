//! VatListener capability: publish capability services over Cap'n Proto RPC.
//!
//! Authenticated publication shares the caller's application capability and
//! compiled auth policy across the service, but creates a fresh single-use
//! Terminal for every inbound libp2p stream. Each stream gets an independent
//! challenge, login deadline, authenticated principal, and issued session.
//!
//! `serveRaw` is the explicit unauthenticated escape hatch. In both modes the
//! protocol name is a locator only; it never authorizes a recipient.

use std::time::Duration;

use auth::SigningDomain;
use authority::{auth_capnp, EpochGuard, KeyMethodAuthorization, TerminalServer};
use capnp::capability::Promise;
use capnp_rpc::pry;
use capnp_rpc::rpc_twoparty_capnp::Side;
use capnp_rpc::twoparty::VatNetwork;
use capnp_rpc::RpcSystem;
use futures::io::{AsyncRead, AsyncReadExt, AsyncWrite};
use futures::StreamExt;

use crate::{inbound_connection_budget, terminal_login_timeout, ConnectionBudget};
use authority::system_capnp;

pub struct VatListenerImpl {
    stream_control: libp2p_stream::Control,
    guard: EpochGuard,
    budget: ConnectionBudget,
    login_timeout: Duration,
}

impl VatListenerImpl {
    pub fn new(stream_control: libp2p_stream::Control, guard: EpochGuard) -> Self {
        Self {
            stream_control,
            guard,
            budget: inbound_connection_budget(),
            login_timeout: terminal_login_timeout(),
        }
    }

    pub fn with_budget(mut self, budget: ConnectionBudget) -> Self {
        self.budget = budget;
        self
    }

    pub fn with_login_timeout(mut self, timeout: Duration) -> Self {
        self.login_timeout = timeout;
        self
    }
}

#[allow(refining_impl_trait)]
impl system_capnp::vat_listener::Server for VatListenerImpl {
    fn serve_raw(
        self: capnp::capability::Rc<Self>,
        params: system_capnp::vat_listener::ServeRawParams,
        _results: system_capnp::vat_listener::ServeRawResults,
    ) -> Promise<(), capnp::Error> {
        pry!(self.guard.check());

        let params = pry!(params.get());
        let bootstrap_cap = pry!(params
            .get_cap()
            .get_as_capability::<capnp::capability::Client>());
        let protocol = pry!(pry!(params.get_protocol())
            .to_str()
            .map_err(|e| capnp::Error::failed(e.to_string())));
        let protocol_name = protocol.to_string();
        let stream_protocol = pry!(super::vat_protocol(&protocol_name));

        let mut control = self.stream_control.clone();
        let mut incoming =
            pry!(control
                .accept(stream_protocol.clone())
                .map_err(|e| capnp::Error::failed(format!(
                    "failed to register vat service '{protocol_name}': {e}"
                ))));

        tracing::info!(
            protocol = %stream_protocol,
            service = %protocol_name,
            "published vat service"
        );

        let mut epoch_rx = self.guard.receiver.clone();
        let issued_seq = self.guard.issued_seq;
        let budget = self.budget.clone();
        tokio::task::spawn_local(async move {
            loop {
                tokio::select! {
                    conn = incoming.next() => {
                        let Some((peer_id, stream)) = conn else {
                            tracing::warn!(protocol = %stream_protocol, "vat accept loop ended unexpectedly");
                            break;
                        };
                        let _accept_span = tracing::info_span!(
                            "vat.accept",
                            peer = %peer_id,
                            protocol = %stream_protocol,
                            service = %protocol_name,
                        ).entered();
                        tracing::debug!("incoming vat connection");
                        let permit = match budget.try_acquire() {
                            Ok(permit) => permit,
                            Err(error) => {
                                tracing::warn!(
                                    capacity = error.capacity,
                                    active = budget.active(),
                                    "rejecting vat connection: service connection budget exhausted"
                                );
                                drop(stream);
                                continue;
                            }
                        };
                        let cap = bootstrap_cap.clone();
                        let protocol = protocol_name.clone();
                        tokio::task::spawn_local(async move {
                            let _permit = permit;
                            let _handle_span = tracing::info_span!(
                                "vat.handle",
                                service = protocol.as_str(),
                            ).entered();
                            if let Err(e) = handle_vat_connection_serve(cap, stream, &protocol).await {
                                tracing::error!("vat service connection error: {e}");
                            }
                        });
                    }
                    _ = epoch_rx.changed() => {
                        if epoch_rx.borrow().seq != issued_seq {
                            tracing::warn!(
                                protocol = %stream_protocol,
                                "epoch became stale, closing vat accept loop"
                            );
                            break;
                        }
                    }
                }
            }
        });

        Promise::ok(())
    }

    fn serve_authenticated(
        self: capnp::capability::Rc<Self>,
        params: system_capnp::vat_listener::ServeAuthenticatedParams,
        _results: system_capnp::vat_listener::ServeAuthenticatedResults,
    ) -> Promise<(), capnp::Error> {
        pry!(self.guard.check());

        let params = pry!(params.get());
        let session_cap = pry!(params
            .get_cap()
            .get_as_capability::<capnp::capability::Client>());
        let session = auth_capnp::opaque_session::Client {
            client: session_cap,
        };
        let protocol = pry!(pry!(params.get_protocol())
            .to_str()
            .map_err(|e| capnp::Error::failed(e.to_string())));
        let protocol_name = protocol.to_string();
        let stream_protocol = pry!(super::vat_protocol(&protocol_name));
        let policy = pry!(params.get_policy());
        let authorization = pry!(KeyMethodAuthorization::from_policy(
            self.guard.receiver.clone(),
            policy
        )
        .map_err(|error| capnp::Error::failed(format!(
            "invalid authenticated VAT policy: {error}"
        ))));

        let mut control = self.stream_control.clone();
        let mut incoming =
            pry!(control
                .accept(stream_protocol.clone())
                .map_err(|e| capnp::Error::failed(format!(
                    "failed to register authenticated vat service '{protocol_name}': {e}"
                ))));

        tracing::info!(
            protocol = %stream_protocol,
            service = %protocol_name,
            "published authenticated vat service"
        );

        let mut epoch_rx = self.guard.receiver.clone();
        let issued_seq = self.guard.issued_seq;
        let budget = self.budget.clone();
        let login_timeout = self.login_timeout;
        tokio::task::spawn_local(async move {
            loop {
                tokio::select! {
                    conn = incoming.next() => {
                        let Some((peer_id, stream)) = conn else {
                            tracing::warn!(protocol = %stream_protocol, "authenticated vat accept loop ended unexpectedly");
                            break;
                        };
                        let _accept_span = tracing::info_span!(
                            "vat.authenticated.accept",
                            peer = %peer_id,
                            protocol = %stream_protocol,
                            service = %protocol_name,
                        ).entered();
                        let permit = match budget.try_acquire() {
                            Ok(permit) => permit,
                            Err(error) => {
                                tracing::warn!(
                                    capacity = error.capacity,
                                    active = budget.active(),
                                    "rejecting authenticated vat connection: service connection budget exhausted"
                                );
                                drop(stream);
                                continue;
                            }
                        };

                        let (granted_tx, granted_rx) = tokio::sync::oneshot::channel();
                        let terminal = TerminalServer::with_policy(
                            Box::new(authorization.clone()),
                            session.clone(),
                            SigningDomain::terminal_membrane(),
                            epoch_rx.clone(),
                        )
                        .single_use()
                        .with_grant_notifier(granted_tx);
                        let terminal: auth_capnp::terminal::Client<
                            auth_capnp::opaque_session::Owned,
                        > = capnp_rpc::new_client(terminal);
                        let protocol = protocol_name.clone();
                        tokio::task::spawn_local(async move {
                            let _permit = permit;
                            let _handle_span = tracing::info_span!(
                                "vat.authenticated.handle",
                                service = protocol.as_str(),
                            ).entered();
                            match handle_authenticated_vat_connection(
                                terminal.client,
                                stream,
                                &protocol,
                                granted_rx,
                                login_timeout,
                            )
                            .await
                            {
                                AuthenticatedConnectionOutcome::Authenticated => {}
                                AuthenticatedConnectionOutcome::ConnectionClosed => {
                                    tracing::debug!("authenticated vat peer disconnected");
                                }
                                AuthenticatedConnectionOutcome::LoginTimedOut => {
                                    tracing::warn!(
                                        "closing authenticated vat connection: login deadline expired"
                                    );
                                }
                            }
                        });
                    }
                    _ = epoch_rx.changed() => {
                        if epoch_rx.borrow().seq != issued_seq {
                            tracing::warn!(
                                protocol = %stream_protocol,
                                "epoch became stale, closing authenticated vat accept loop"
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

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AuthenticatedConnectionOutcome {
    Authenticated,
    ConnectionClosed,
    LoginTimedOut,
}

async fn handle_authenticated_vat_connection(
    bootstrap_cap: capnp::capability::Client,
    stream: impl AsyncRead + AsyncWrite + 'static,
    protocol: &str,
    granted_rx: tokio::sync::oneshot::Receiver<()>,
    login_timeout: Duration,
) -> AuthenticatedConnectionOutcome {
    let (reader, writer) = Box::pin(stream).split();
    let network = VatNetwork::new(reader, writer, Side::Server, Default::default());
    let rpc_system = RpcSystem::new(Box::new(network), Some(bootstrap_cap));
    tokio::pin!(rpc_system);
    let deadline = tokio::time::sleep(login_timeout);
    tokio::pin!(deadline);

    tokio::select! {
        biased;
        result = granted_rx => {
            if result.is_ok() {
                let _ = rpc_system.await;
                AuthenticatedConnectionOutcome::Authenticated
            } else {
                AuthenticatedConnectionOutcome::ConnectionClosed
            }
        }
        _ = &mut deadline => AuthenticatedConnectionOutcome::LoginTimedOut,
        _ = rpc_system.as_mut() => {
            tracing::debug!(protocol, "authenticated vat peer disconnected before login");
            AuthenticatedConnectionOutcome::ConnectionClosed
        }
    }
}

/// Bootstrap one remote peer with a persistent capability.
///
/// Generic over stream type for testability.
pub async fn handle_vat_connection_serve(
    bootstrap_cap: capnp::capability::Client,
    stream: impl AsyncRead + AsyncWrite + 'static,
    protocol: &str,
) -> Result<(), capnp::Error> {
    let (reader, writer) = Box::pin(stream).split();
    let network = VatNetwork::new(reader, writer, Side::Server, Default::default());
    let peer_rpc = RpcSystem::new(Box::new(network), Some(bootstrap_cap));

    let _ = peer_rpc.await;
    tracing::debug!(protocol, "vat peer disconnected");

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use authority::{Epoch, Provenance};
    use tokio::sync::{oneshot, watch};
    use tokio_util::compat::TokioAsyncReadCompatExt;

    #[tokio::test]
    async fn committed_login_wins_a_deadline_tie() {
        let local = tokio::task::LocalSet::new();
        local
            .run_until(async {
                let (_epoch_tx, epoch_rx) = watch::channel(Epoch {
                    seq: 1,
                    head: vec![1],
                    provenance: Provenance::Block(1),
                });
                let bootstrap = authority::membrane_client(epoch_rx);
                let (server_stream, peer_stream) = tokio::io::duplex(64);
                let (granted_tx, granted_rx) = oneshot::channel();
                granted_tx.send(()).expect("pre-commit grant signal");
                drop(peer_stream);

                let outcome = handle_authenticated_vat_connection(
                    bootstrap.client,
                    server_stream.compat(),
                    "deadline-tie",
                    granted_rx,
                    Duration::ZERO,
                )
                .await;

                assert_eq!(
                    outcome,
                    AuthenticatedConnectionOutcome::Authenticated,
                    "a synchronously observable grant must win over an expiring deadline"
                );
            })
            .await;
    }
}
