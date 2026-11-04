//! #785 regression: the actual outbound session follows its explicit Cell owner.
//! The main RPC connection models a Cell boundary; no Wasm Store runs here.

use crate::{
    local_tasks::LocalTaskScope,
    managed_rpc::{ManagedRpc, ManagedRpcSystem},
};
use authority::{system_capnp, Epoch, EpochGuard};
use capnp::capability::Promise;
use capnp_rpc::{rpc_twoparty_capnp::Side, twoparty::VatNetwork};
use futures::{io::AsyncReadExt, StreamExt};
use std::{
    cell::{Cell, RefCell},
    future::Future,
    rc::Rc,
    time::Duration,
};
use tokio::sync::{oneshot, watch};
use tokio_util::compat::{FuturesAsyncReadCompatExt, TokioAsyncReadCompatExt};

fn swarm() -> libp2p::Swarm<libp2p_stream::Behaviour> {
    use libp2p::Transport;
    let key = libp2p::identity::Keypair::generate_ed25519();
    let transport = libp2p::core::transport::MemoryTransport::default()
        .upgrade(libp2p::core::upgrade::Version::V1)
        .authenticate(libp2p::noise::Config::new(&key).unwrap())
        .multiplex(libp2p::yamux::Config::default())
        .boxed();
    libp2p::Swarm::new(
        transport,
        libp2p_stream::Behaviour::new(),
        key.public().to_peer_id(),
        libp2p::swarm::Config::with_tokio_executor()
            .with_idle_connection_timeout(Duration::from_secs(60)),
    )
}

struct Probe {
    calls: Rc<Cell<usize>>,
    dropped: Option<oneshot::Sender<()>>,
}
impl system_capnp::process::Server for Probe {
    fn stdin(
        self: capnp::capability::Rc<Self>,
        _: system_capnp::process::StdinParams,
        _: system_capnp::process::StdinResults,
    ) -> impl Future<Output = capnp::Result<()>> + 'static {
        self.calls.set(self.calls.get() + 1);
        Promise::ok(())
    }
}
impl Drop for Probe {
    fn drop(&mut self) {
        if let Some(tx) = self.dropped.take() {
            let _ = tx.send(());
        }
    }
}

// Retain the exact returned capability on the Host side before exporting it to
// the simulated Cell. Retention is deliberately not an ownership transfer.
struct ForwardDial {
    dialer: system_capnp::vat_client::Client,
    retained: Rc<RefCell<Option<system_capnp::process::Client>>>,
}
impl system_capnp::vat_client::Server for ForwardDial {
    async fn dial(
        self: capnp::capability::Rc<Self>,
        params: system_capnp::vat_client::DialParams,
        mut results: system_capnp::vat_client::DialResults,
    ) -> capnp::Result<()> {
        let origin = params.hook.invocation_origin();
        let params = params.get()?;
        let mut request = self.dialer.dial_request();
        if let Some(origin) = origin {
            request.hook.set_invocation_origin(origin)?;
        }
        request.get().set_peer(params.get_peer()?);
        request.get().set_protocol(params.get_protocol()?);
        let response = request.send().promise.await?;
        let cap: system_capnp::process::Client = response.get()?.get_cap().get_as()?;
        *self.retained.borrow_mut() = Some(cap.clone());
        results.get().init_cap().set_as_capability(cap.client.hook);
        Ok(())
    }
}

#[tokio::test(flavor = "current_thread")]
async fn outbound_vat_joins_cell_owner_after_main_rpc_terminalization() {
    outbound_vat_owner_case(false).await;
}

#[tokio::test(flavor = "current_thread")]
async fn delegated_vat_uses_initiating_registry_and_survives_grant_owner_close() {
    outbound_vat_owner_case(true).await;
}

async fn outbound_vat_owner_case(delegated: bool) {
    tokio::time::timeout(
        Duration::from_secs(15),
        tokio::task::LocalSet::new().run_until(async {
            let mut server = swarm();
            let peer = *server.local_peer_id();
            let mut server_control = server.behaviour().new_control();
            let mut incoming = server_control
                .accept(crate::vat_protocol("ownership-repro").unwrap())
                .unwrap();
            server.listen_on("/memory/0".parse().unwrap()).unwrap();
            let address = loop {
                if let libp2p::swarm::SwarmEvent::NewListenAddr { address, .. } =
                    server.select_next_some().await
                {
                    break address;
                }
            };
            let mut client = swarm();
            let control = client.behaviour().new_control();
            client.dial(address).unwrap();
            let server_swarm =
                tokio::task::spawn_local(async move { while server.next().await.is_some() {} });
            while !matches!(
                client.select_next_some().await,
                libp2p::swarm::SwarmEvent::ConnectionEstablished { .. }
            ) {}
            let client_swarm =
                tokio::task::spawn_local(async move { while client.next().await.is_some() {} });

            // Construct remote ManagedRpc before entering the local executor scope.
            // Its terminalization must come from the wire, not scope cancellation.
            let calls = Rc::new(Cell::new(0));
            let remote_calls = calls.clone();
            let (dropped_tx, mut dropped_rx) = oneshot::channel();
            let (remote_io, mut bridge_io) = tokio::io::duplex(8192);
            let (read, write) = remote_io.compat().split();
            let bootstrap: system_capnp::process::Client = capnp_rpc::new_client(Probe {
                calls: remote_calls,
                dropped: Some(dropped_tx),
            });
            let remote_system = ManagedRpcSystem::new(
                Box::new(VatNetwork::new(
                    read,
                    write,
                    Side::Server,
                    Default::default(),
                )),
                Some(bootstrap.client),
            );
            let mut remote = ManagedRpc::spawn(remote_system);
            let (transport_dropped_tx, transport_dropped_rx) = oneshot::channel();
            let remote_transport = tokio::task::spawn_local(async move {
                let (_, stream) = incoming.next().await.unwrap();
                let mut stream = stream.compat();
                let _ = tokio::io::copy_bidirectional(&mut stream, &mut bridge_io).await;
                drop(stream);
                drop(bridge_io);
                transport_dropped_tx.send(()).unwrap();
            });

            // Create the dialer but do not poll any outgoing call yet.
            let (_epoch_tx, receiver) = watch::channel(Epoch {
                seq: 1,
                head: vec![],
                root: None,
            });
            let registry = crate::outbound::OutboundRegistry::default();
            let owner = registry.register().unwrap();
            let granting_registry = if delegated {
                crate::outbound::OutboundRegistry::new_with_budget(
                    crate::outbound::EffectLimits {
                        max_owners: 1,
                        max_effects_per_cell: 0,
                        max_total_effects: 0,
                    },
                    crate::outbound::EffectBudget::new(0),
                )
            } else {
                registry.clone()
            };
            let grant_owner = delegated.then(|| granting_registry.register().unwrap());
            let vat: system_capnp::vat_client::Client = capnp_rpc::new_client(
                crate::vat_client::VatClientImpl::new(
                    control,
                    EpochGuard {
                        issued_seq: 1,
                        receiver,
                    },
                )
                .with_outbound_registry(granting_registry.clone()),
            );
            let retained = Rc::new(RefCell::new(None));
            let host: system_capnp::vat_client::Client = capnp_rpc::new_client(ForwardDial {
                dialer: vat,
                retained: retained.clone(),
            });
            let (host_io, guest_io) = tokio::io::duplex(8192);
            let (read, write) = host_io.compat().split();
            let system = ManagedRpcSystem::new(
                crate::outbound::with_origin(
                    Box::new(VatNetwork::new(
                        read,
                        write,
                        Side::Server,
                        Default::default(),
                    )),
                    owner.origin(),
                ),
                Some(host.client),
            );
            let mut main = ManagedRpc::spawn(system);
            let crate::vat_dial::VatDial {
                bootstrap: guest,
                driver: mut guest_driver,
            } = crate::vat_dial::connect::<_, system_capnp::vat_client::Client>(guest_io.compat());

            let scope = LocalTaskScope::enter();
            let mut request = guest.dial_request();
            request.get().set_peer(&peer.to_bytes());
            request.get().set_protocol("ownership-repro");
            let response = request.send().promise.await.unwrap();
            let retained = retained.borrow_mut().take().unwrap();
            retained.stdin_request().send().promise.await.unwrap();
            assert_eq!(calls.get(), 1);
            assert_eq!(registry.total_active(), 1);
            if let Some(grant_owner) = grant_owner {
                assert_eq!(granting_registry.total_active(), 0);
                grant_owner.close_and_join().await.unwrap();
                assert_eq!(
                    registry.total_active(),
                    1,
                    "grant owner closure cannot release B's session"
                );
            }
            drop(response);

            // Join the real main RPC owner; this is stronger than merely dropping
            // an abort handle. Keep executor, remote transport and import alive.
            assert!(main.shutdown_and_join().await.is_ok());
            assert!(guest_driver.shutdown_and_join().await.is_ok());
            assert!(guest.dial_request().send().promise.await.is_err());
            assert!(matches!(
                dropped_rx.try_recv(),
                Err(oneshot::error::TryRecvError::Empty)
            ));
            retained.stdin_request().send().promise.await.unwrap();
            assert_eq!(
                calls.get(),
                2,
                "remote invocation executed after main RPC terminalization"
            );

            let completed = owner.close_and_join().await.unwrap();
            assert_eq!(
                (completed.committed, completed.joined, completed.active),
                (1, 1, 0)
            );
            assert_eq!(registry.total_active(), 0);
            scope.shutdown();
            scope.wait().await;
            assert!(retained.stdin_request().send().promise.await.is_err());
            assert!(
                (&mut remote).await.is_ok(),
                "remote driver must terminate from peer disconnect"
            );
            dropped_rx.await.expect("remote bootstrap released");
            transport_dropped_rx
                .await
                .expect("remote libp2p stream dropped");
            remote_transport.await.unwrap();
            server_swarm.abort();
            client_swarm.abort();
            let _ = server_swarm.await;
            let _ = client_swarm.await;
        }),
    )
    .await
    .expect("reproduction watchdog");
}

struct ReverseInvoker {
    observed: Rc<Cell<bool>>,
    peer: libp2p::PeerId,
}

impl system_capnp::vat_listener::Server for ReverseInvoker {
    async fn serve_raw(
        self: capnp::capability::Rc<Self>,
        params: system_capnp::vat_listener::ServeRawParams,
        _: system_capnp::vat_listener::ServeRawResults,
    ) -> capnp::Result<()> {
        let callback: system_capnp::vat_client::Client = params.get()?.get_cap().get_as()?;
        let mut request = callback.dial_request();
        request.get().set_peer(&self.peer.to_bytes());
        request.get().set_protocol("reverse-forbidden");
        match request.send().promise.await {
            Err(error) if error.extra.contains("missing invocation origin") => {
                self.observed.set(true);
                Ok(())
            }
            Err(error) => Err(capnp::Error::failed(format!(
                "reverse call returned an unexpected error: {error}"
            ))),
            Ok(_) => Err(capnp::Error::failed(
                "remote reverse call inherited Cell ownership".into(),
            )),
        }
    }
}

#[tokio::test(flavor = "current_thread")]
async fn outbound_remote_reverse_call_cannot_borrow_cell_ownership() {
    tokio::time::timeout(
        Duration::from_secs(15),
        tokio::task::LocalSet::new().run_until(async {
            let mut server = swarm();
            let peer = *server.local_peer_id();
            let protocol = crate::vat_protocol("reverse-origin").unwrap();
            let mut server_control = server.behaviour().new_control();
            let mut incoming = server_control.accept(protocol.clone()).unwrap();
            server.listen_on("/memory/0".parse().unwrap()).unwrap();
            let address = loop {
                if let libp2p::swarm::SwarmEvent::NewListenAddr { address, .. } =
                    server.select_next_some().await
                {
                    break address;
                }
            };
            let mut client = swarm();
            let control = client.behaviour().new_control();
            client.dial(address).unwrap();
            let server_swarm =
                tokio::task::spawn_local(async move { while server.next().await.is_some() {} });
            while !matches!(
                client.select_next_some().await,
                libp2p::swarm::SwarmEvent::ConnectionEstablished { .. }
            ) {}
            let client_swarm =
                tokio::task::spawn_local(async move { while client.next().await.is_some() {} });

            let observed = Rc::new(Cell::new(false));
            let bootstrap: system_capnp::vat_listener::Client =
                capnp_rpc::new_client(ReverseInvoker {
                    observed: observed.clone(),
                    peer,
                });
            let remote = tokio::task::spawn_local(async move {
                let (_, stream) = incoming.next().await.unwrap();
                let (read, write) = stream.split();
                ManagedRpc::spawn(ManagedRpcSystem::new(
                    Box::new(VatNetwork::new(
                        read,
                        write,
                        Side::Server,
                        Default::default(),
                    )),
                    Some(bootstrap.client),
                ))
                .await
            });

            let registry = crate::outbound::OutboundRegistry::default();
            let owner = registry.register().unwrap();
            let callback: system_capnp::vat_client::Client = capnp_rpc::new_client(
                crate::vat_client::VatClientImpl::new(
                    control.clone(),
                    EpochGuard::fixed(Epoch::zero()),
                )
                .with_outbound_registry(registry.clone()),
            );
            let service =
                crate::vat_dial::connect_libp2p_owned::<system_capnp::vat_listener::Client>(
                    &registry,
                    Some(owner.origin()),
                    control,
                    peer,
                    protocol,
                )
                .unwrap()
                .await
                .unwrap();
            assert_eq!(registry.total_active(), 1);
            let mut request = service.serve_raw_request();
            request
                .get()
                .init_cap()
                .set_as_capability(callback.client.hook);
            request.get().set_protocol("reverse-callback");
            request.send().promise.await.unwrap();
            assert!(
                observed.get(),
                "actual reverse request must reach the protected sink"
            );
            assert_eq!(
                registry.total_active(),
                1,
                "remote caller cannot add a Cell-owned effect"
            );
            let completed = owner.close_and_join().await.unwrap();
            assert_eq!(
                (completed.committed, completed.joined, completed.active),
                (1, 1, 0)
            );
            assert!(remote.await.unwrap().is_ok());
            server_swarm.abort();
            client_swarm.abort();
            let _ = server_swarm.await;
            let _ = client_swarm.await;
        }),
    )
    .await
    .expect("remote reverse call watchdog");
}
