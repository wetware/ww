//! Epoch-guarded provider discovery and announcement capabilities.
//!
//! [`FinderImpl`] and [`AnnouncerImpl`] are independent Cap'n Proto servers.
//! Both dispatch to the swarm event loop, but neither exposes routing-key
//! derivation, IPNS operations, or persistent content mutation.

use std::cell::Cell;
use std::collections::HashMap;
use std::rc::Rc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use authority::{routing_capnp, EpochGuard};
use capnp::capability::Promise;
use capnp_rpc::pry;
use cid::Cid;
use tokio::sync::{mpsc, oneshot, watch};

use crate::{PeerInfo, ProviderOwnerId, ProviderQueryId, SwarmCommand};

/// Finder calls use the same finite budget as network dial operations.
pub const FIND_PROVIDERS_TIMEOUT: Duration = Duration::from_secs(30);

static NEXT_PROVIDER_OWNER: AtomicU64 = AtomicU64::new(1);
static NEXT_PROVIDER_QUERY: AtomicU64 = AtomicU64::new(1);

/// Convert canonical CID text to the multihash bytes used as a Kademlia key.
fn cid_to_kad_key(cid_text: &str) -> Result<Vec<u8>, capnp::Error> {
    let cid: Cid = cid_text
        .parse()
        .map_err(|error| capnp::Error::failed(format!("invalid CID '{cid_text}': {error}")))?;
    Ok(cid.hash().to_bytes())
}

/// Deterministic provider table for integration tests and local embedders.
///
/// The factory returns distinct Finder and Announcer servers over one table;
/// it is not itself a broad capability.
#[derive(Clone, Default)]
pub struct LocalProviderRouting {
    providers: Arc<Mutex<HashMap<String, Vec<PeerInfo>>>>,
}

impl LocalProviderRouting {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn finder(&self) -> LocalFinder {
        LocalFinder {
            providers: Arc::clone(&self.providers),
        }
    }

    pub fn announcer(&self) -> LocalAnnouncer {
        LocalAnnouncer {
            providers: Arc::clone(&self.providers),
        }
    }

    /// Pre-seed one provider entry for deterministic discovery tests.
    pub fn provide_as(&self, cid: &str, peer: PeerInfo) {
        self.providers
            .lock()
            .expect("local provider table poisoned")
            .entry(cid.to_string())
            .or_default()
            .push(peer);
    }
}

pub struct LocalFinder {
    providers: Arc<Mutex<HashMap<String, Vec<PeerInfo>>>>,
}

#[allow(refining_impl_trait)]
impl routing_capnp::finder::Server for LocalFinder {
    fn find_providers(
        self: capnp::capability::Rc<Self>,
        params: routing_capnp::finder::FindProvidersParams,
        _results: routing_capnp::finder::FindProvidersResults,
    ) -> Promise<(), capnp::Error> {
        let reader = pry!(params.get());
        let cid = pry!(reader.get_key()).to_string().unwrap_or_default();
        let count = reader.get_count() as usize;
        let sink = pry!(reader.get_sink());

        if count == 0 {
            return Promise::from_future(async move {
                sink.done_request().send().promise.await?;
                Ok(())
            });
        }

        let providers = self
            .providers
            .lock()
            .expect("local provider table poisoned")
            .get(&cid)
            .cloned()
            .unwrap_or_default();

        Promise::from_future(async move {
            let mut seen = std::collections::HashSet::new();
            for peer in providers
                .iter()
                .filter(|peer| seen.insert(peer.peer_id.clone()))
                .take(count)
            {
                send_provider(&sink, peer).await?;
            }
            sink.done_request().send().promise.await?;
            Ok(())
        })
    }
}

pub struct LocalAnnouncer {
    providers: Arc<Mutex<HashMap<String, Vec<PeerInfo>>>>,
}

#[allow(refining_impl_trait)]
impl routing_capnp::announcer::Server for LocalAnnouncer {
    fn provide(
        self: capnp::capability::Rc<Self>,
        params: routing_capnp::announcer::ProvideParams,
        _results: routing_capnp::announcer::ProvideResults,
    ) -> Promise<(), capnp::Error> {
        let cid = pry!(pry!(params.get()).get_key())
            .to_string()
            .unwrap_or_default();
        let _: Cid = pry!(cid
            .parse()
            .map_err(|error| capnp::Error::failed(format!("invalid CID '{cid}': {error}"))));
        self.providers
            .lock()
            .expect("local provider table poisoned")
            .entry(cid)
            .or_default();
        Promise::ok(())
    }
}

/// Observational provider-discovery authority for one epoch.
pub struct FinderImpl {
    swarm_cmd_tx: mpsc::Sender<SwarmCommand>,
    guard: EpochGuard,
    query_timeout: Duration,
}

impl FinderImpl {
    pub fn new(swarm_cmd_tx: mpsc::Sender<SwarmCommand>, guard: EpochGuard) -> Self {
        Self {
            swarm_cmd_tx,
            guard,
            query_timeout: FIND_PROVIDERS_TIMEOUT,
        }
    }

    #[cfg(test)]
    fn with_query_timeout(mut self, query_timeout: Duration) -> Self {
        self.query_timeout = query_timeout;
        self
    }
}

/// Host-PeerID provider-announcement authority for one epoch.
pub struct AnnouncerImpl {
    swarm_cmd_tx: mpsc::Sender<SwarmCommand>,
    guard: EpochGuard,
    owner: ProviderOwnerId,
    cleanup_started: Cell<bool>,
    registration_scope: Option<watch::Receiver<()>>,
    active: Rc<Cell<bool>>,
}

impl AnnouncerImpl {
    pub fn new(swarm_cmd_tx: mpsc::Sender<SwarmCommand>, guard: EpochGuard) -> Self {
        Self {
            swarm_cmd_tx,
            guard,
            owner: ProviderOwnerId(NEXT_PROVIDER_OWNER.fetch_add(1, Ordering::Relaxed)),
            cleanup_started: Cell::new(false),
            registration_scope: None,
            active: Rc::new(Cell::new(true)),
        }
    }

    pub fn with_registration_scope(mut self, registration_scope: watch::Receiver<()>) -> Self {
        self.registration_scope = Some(registration_scope);
        self
    }

    fn start_epoch_cleanup(&self) {
        if self.cleanup_started.replace(true) {
            return;
        }

        let mut epoch_rx = self.guard.receiver.clone();
        let issued_seq = self.guard.issued_seq;
        let owner = self.owner;
        let swarm_cmd_tx = self.swarm_cmd_tx.clone();
        let mut registration_scope = self.registration_scope.clone();
        let active = Rc::clone(&self.active);
        tokio::task::spawn_local(async move {
            loop {
                if epoch_rx.borrow().seq != issued_seq {
                    break;
                }
                if let Some(scope) = registration_scope.as_mut() {
                    tokio::select! {
                        changed = epoch_rx.changed() => {
                            if changed.is_err() {
                                break;
                            }
                        }
                        _ = scope.changed() => {
                            break;
                        }
                    }
                } else if epoch_rx.changed().await.is_err() {
                    break;
                }
            }
            active.set(false);
            let _ = swarm_cmd_tx
                .send(SwarmCommand::KadReleaseProviderOwner { owner })
                .await;
        });
    }
}

impl Drop for AnnouncerImpl {
    fn drop(&mut self) {
        self.active.set(false);
        if self.cleanup_started.get() {
            let _ = self
                .swarm_cmd_tx
                .try_send(SwarmCommand::KadReleaseProviderOwner { owner: self.owner });
        }
    }
}

struct FindCancellation {
    sender: Option<watch::Sender<bool>>,
}

impl FindCancellation {
    fn new(sender: watch::Sender<bool>) -> Self {
        Self {
            sender: Some(sender),
        }
    }

    fn cancel(&mut self) {
        if let Some(sender) = self.sender.take() {
            sender.send_replace(true);
        }
    }

    fn complete(&mut self) {
        let _ = self.sender.take();
    }
}

impl Drop for FindCancellation {
    fn drop(&mut self) {
        self.cancel();
    }
}

#[allow(refining_impl_trait)]
impl routing_capnp::finder::Server for FinderImpl {
    fn find_providers(
        self: capnp::capability::Rc<Self>,
        params: routing_capnp::finder::FindProvidersParams,
        _results: routing_capnp::finder::FindProvidersResults,
    ) -> Promise<(), capnp::Error> {
        pry!(self.guard.check());
        let reader = pry!(params.get());
        let cid = pry!(reader.get_key()).to_string().unwrap_or_default();
        let count = reader.get_count();
        let sink = pry!(reader.get_sink());

        if count == 0 {
            let mut epoch_rx = self.guard.receiver.clone();
            let issued_seq = self.guard.issued_seq;
            let timeout = self.query_timeout;
            return Promise::from_future(async move {
                let done = sink.done_request().send().promise;
                tokio::pin!(done);
                let deadline = tokio::time::sleep(timeout);
                tokio::pin!(deadline);
                loop {
                    tokio::select! {
                        result = &mut done => return result.map(|_| ()),
                        changed = epoch_rx.changed() => {
                            if changed.is_err() || epoch_rx.borrow().seq != issued_seq {
                                return Err(authority::stale_epoch_error(
                                    "Finder authority epoch no longer current",
                                ));
                            }
                        }
                        () = &mut deadline => return Ok(()),
                    }
                }
            });
        }

        let key = pry!(cid_to_kad_key(&cid));
        let request = ProviderQueryId(NEXT_PROVIDER_QUERY.fetch_add(1, Ordering::Relaxed));
        let swarm_cmd_tx = self.swarm_cmd_tx.clone();
        let mut epoch_rx = self.guard.receiver.clone();
        let issued_seq = self.guard.issued_seq;
        let timeout = self.query_timeout;

        Promise::from_future(async move {
            // Only one result crosses from the swarm into the Cap'n Proto
            // sink at a time. The swarm retains at most `count` selected
            // results while this handoff is occupied.
            let (provider_tx, mut provider_rx) = mpsc::channel(1);
            let (cancel_tx, cancel_rx) = watch::channel(false);
            let deadline = tokio::time::sleep(timeout);
            tokio::pin!(deadline);
            let admission_tx = swarm_cmd_tx.clone();
            let command = admission_tx.send(SwarmCommand::KadFindProviders {
                request,
                key,
                count,
                reply: provider_tx,
                cancel: cancel_rx,
            });
            tokio::pin!(command);
            loop {
                tokio::select! {
                    result = &mut command => {
                        result.map_err(|_| capnp::Error::failed("swarm channel closed".into()))?;
                        break;
                    }
                    changed = epoch_rx.changed() => {
                        if changed.is_err() || epoch_rx.borrow().seq != issued_seq {
                            return Err(authority::stale_epoch_error(
                                "Finder authority epoch no longer current",
                            ));
                        }
                    }
                    () = &mut deadline => return Ok(()),
                }
            }
            let mut cancellation = FindCancellation::new(cancel_tx);
            let mut notify_done = true;

            'providers: loop {
                tokio::select! {
                    provider = provider_rx.recv() => {
                        let Some(provider) = provider else {
                            cancellation.complete();
                            break;
                        };
                        let send = send_provider(&sink, &provider);
                        tokio::pin!(send);
                        loop {
                            tokio::select! {
                                result = &mut send => {
                                    if let Err(error) = result {
                                        cancellation.cancel();
                                        return Err(error);
                                    }
                                    break;
                                }
                                changed = epoch_rx.changed() => {
                                    if changed.is_err() || epoch_rx.borrow().seq != issued_seq {
                                        cancellation.cancel();
                                        return Err(authority::stale_epoch_error(
                                            "Finder authority epoch no longer current",
                                        ));
                                    }
                                }
                                () = &mut deadline => {
                                    cancellation.cancel();
                                    notify_done = false;
                                    break 'providers;
                                }
                            }
                        }
                    }
                    changed = epoch_rx.changed() => {
                        if changed.is_err() || epoch_rx.borrow().seq != issued_seq {
                            cancellation.cancel();
                            return Err(authority::stale_epoch_error(
                                "Finder authority epoch no longer current",
                            ));
                        }
                    }
                    () = &mut deadline => {
                        cancellation.cancel();
                        notify_done = false;
                        break;
                    }
                }
            }

            if !notify_done {
                return Ok(());
            }

            let done = sink.done_request().send().promise;
            tokio::pin!(done);
            loop {
                tokio::select! {
                    result = &mut done => return result.map(|_| ()),
                    changed = epoch_rx.changed() => {
                        if changed.is_err() || epoch_rx.borrow().seq != issued_seq {
                            return Err(authority::stale_epoch_error(
                                "Finder authority epoch no longer current",
                            ));
                        }
                    }
                    () = &mut deadline => return Ok(()),
                }
            }
        })
    }
}

#[allow(refining_impl_trait)]
impl routing_capnp::announcer::Server for AnnouncerImpl {
    fn provide(
        self: capnp::capability::Rc<Self>,
        params: routing_capnp::announcer::ProvideParams,
        _results: routing_capnp::announcer::ProvideResults,
    ) -> Promise<(), capnp::Error> {
        pry!(self.guard.check());
        let cid = pry!(pry!(params.get()).get_key())
            .to_string()
            .unwrap_or_default();
        let key = pry!(cid_to_kad_key(&cid));
        self.start_epoch_cleanup();

        let owner = self.owner;
        let swarm_cmd_tx = self.swarm_cmd_tx.clone();
        let guard = self.guard.clone();
        let active = Rc::clone(&self.active);
        Promise::from_future(async move {
            guard.check()?;
            if !active.get() {
                return Err(capnp::Error::failed(
                    "Announcer owner scope already ended".into(),
                ));
            }
            let (reply_tx, reply_rx) = oneshot::channel();
            swarm_cmd_tx
                .send(SwarmCommand::KadProvide {
                    owner,
                    key,
                    reply: reply_tx,
                })
                .await
                .map_err(|_| capnp::Error::failed("swarm channel closed".into()))?;
            reply_rx
                .await
                .map_err(|_| capnp::Error::failed("swarm reply dropped".into()))?
                .map_err(|error| capnp::Error::failed(format!("kad provide failed: {error}")))?;
            guard.check()?;
            if !active.get() {
                return Err(capnp::Error::failed(
                    "Announcer owner scope ended during provide".into(),
                ));
            }
            Ok(())
        })
    }
}

async fn send_provider(
    sink: &routing_capnp::provider_sink::Client,
    peer: &PeerInfo,
) -> Result<(), capnp::Error> {
    let mut request = sink.provider_request();
    let mut info = request.get().get_info()?;
    info.set_peer_id(&peer.peer_id);
    let mut addrs = info.init_addrs(peer.addrs.len() as u32);
    for (index, addr) in peer.addrs.iter().enumerate() {
        addrs.set(index as u32, addr);
    }
    request.send().promise.await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use authority::Epoch;
    use capnp_rpc::rpc_twoparty_capnp::Side;
    use capnp_rpc::twoparty::VatNetwork;
    use capnp_rpc::RpcSystem;
    use tokio::io;
    use tokio::sync::watch;
    use tokio_util::compat::{TokioAsyncReadCompatExt, TokioAsyncWriteCompatExt};

    fn epoch(seq: u64) -> Epoch {
        Epoch {
            seq,
            head: Vec::new(),
            root: None,
        }
    }

    fn test_cid() -> &'static str {
        "bafkr4ifcoue3f52zpzpz2xei7dqhs3gajm326llyljbwisxkwea7hbowyy"
    }

    fn bootstrap_client<C>(server: capnp::capability::Client) -> C
    where
        C: capnp::capability::FromClientHook,
    {
        let (client_stream, server_stream) = io::duplex(64 * 1024);
        let (client_read, client_write) = io::split(client_stream);
        let (server_read, server_write) = io::split(server_stream);
        let server_network = VatNetwork::new(
            server_read.compat(),
            server_write.compat_write(),
            Side::Server,
            Default::default(),
        );
        tokio::task::spawn_local(async move {
            let _ = RpcSystem::new(Box::new(server_network), Some(server)).await;
        });
        let client_network = VatNetwork::new(
            client_read.compat(),
            client_write.compat_write(),
            Side::Client,
            Default::default(),
        );
        let mut client_rpc = RpcSystem::new(Box::new(client_network), None);
        let client = client_rpc.bootstrap(Side::Server);
        tokio::task::spawn_local(async move {
            let _ = client_rpc.await;
        });
        client
    }

    fn bootstrap_finder<T>(server: T) -> routing_capnp::finder::Client
    where
        T: routing_capnp::finder::Server + 'static,
    {
        let server: routing_capnp::finder::Client = capnp_rpc::new_client(server);
        bootstrap_client(server.client)
    }

    fn bootstrap_announcer<T>(server: T) -> routing_capnp::announcer::Client
    where
        T: routing_capnp::announcer::Server + 'static,
    {
        let server: routing_capnp::announcer::Client = capnp_rpc::new_client(server);
        bootstrap_client(server.client)
    }

    struct Collector {
        tx: mpsc::UnboundedSender<PeerInfo>,
        reject: bool,
    }

    struct BlockingCollector;

    impl routing_capnp::provider_sink::Server for Collector {
        async fn provider(
            self: capnp::capability::Rc<Self>,
            params: routing_capnp::provider_sink::ProviderParams,
            _results: routing_capnp::provider_sink::ProviderResults,
        ) -> Result<(), capnp::Error> {
            if self.reject {
                return Err(capnp::Error::failed("sink closed".into()));
            }
            let info = params.get()?.get_info()?;
            let peer_id = info.get_peer_id()?.to_vec();
            let source = info.get_addrs()?;
            let addrs = (0..source.len())
                .map(|index| source.get(index).map(|addr| addr.to_vec()))
                .collect::<Result<Vec<_>, _>>()?;
            let _ = self.tx.send(PeerInfo { peer_id, addrs });
            Ok(())
        }

        async fn done(
            self: capnp::capability::Rc<Self>,
            _params: routing_capnp::provider_sink::DoneParams,
            _results: routing_capnp::provider_sink::DoneResults,
        ) -> Result<(), capnp::Error> {
            Ok(())
        }
    }

    impl routing_capnp::provider_sink::Server for BlockingCollector {
        async fn provider(
            self: capnp::capability::Rc<Self>,
            _params: routing_capnp::provider_sink::ProviderParams,
            _results: routing_capnp::provider_sink::ProviderResults,
        ) -> Result<(), capnp::Error> {
            std::future::pending().await
        }

        async fn done(
            self: capnp::capability::Rc<Self>,
            _params: routing_capnp::provider_sink::DoneParams,
            _results: routing_capnp::provider_sink::DoneResults,
        ) -> Result<(), capnp::Error> {
            Ok(())
        }
    }

    fn collector(
        reject: bool,
    ) -> (
        routing_capnp::provider_sink::Client,
        mpsc::UnboundedReceiver<PeerInfo>,
    ) {
        let (tx, rx) = mpsc::unbounded_channel();
        (capnp_rpc::new_client(Collector { tx, reject }), rx)
    }

    #[tokio::test]
    async fn legacy_routing_interface_id_is_rejected_by_new_capabilities() {
        const LEGACY_ROUTING_INTERFACE_ID: u64 = 0xa7c3_e8f1_d4b2_9065;

        tokio::task::LocalSet::new()
            .run_until(async {
                let routing = LocalProviderRouting::new();
                let finder = bootstrap_finder(routing.finder());
                let announcer = bootstrap_announcer(routing.announcer());

                for client in [finder.client, announcer.client] {
                    let request = client
                        .new_call::<capnp::any_pointer::Owned, capnp::any_pointer::Owned>(
                            LEGACY_ROUTING_INTERFACE_ID,
                            0,
                            None,
                        );
                    let error = match request.send().promise.await {
                        Ok(_) => panic!("legacy Routing interface ID resolved"),
                        Err(error) => error,
                    };
                    assert_eq!(error.kind, capnp::ErrorKind::Unimplemented);
                }
            })
            .await;
    }

    #[tokio::test]
    async fn finder_count_zero_launches_no_query() {
        tokio::task::LocalSet::new()
            .run_until(async {
                let (_epoch_tx, epoch_rx) = watch::channel(epoch(1));
                let (swarm_tx, mut swarm_rx) = mpsc::channel(4);
                let client = bootstrap_finder(FinderImpl::new(
                    swarm_tx,
                    EpochGuard {
                        issued_seq: 1,
                        receiver: epoch_rx,
                    },
                ));
                let (sink, mut providers) = collector(false);
                let mut request = client.find_providers_request();
                request.get().set_key(test_cid());
                request.get().set_count(0);
                request.get().set_sink(sink);
                request.send().promise.await.expect("zero-count find");
                assert!(swarm_rx.try_recv().is_err());
                assert!(providers.try_recv().is_err());
            })
            .await;
    }

    #[tokio::test]
    async fn finder_uses_single_slot_buffer_and_cancels_closed_sink() {
        tokio::task::LocalSet::new()
            .run_until(async {
                let (_epoch_tx, epoch_rx) = watch::channel(epoch(1));
                let (swarm_tx, mut swarm_rx) = mpsc::channel(8);
                let client = bootstrap_finder(FinderImpl::new(
                    swarm_tx,
                    EpochGuard {
                        issued_seq: 1,
                        receiver: epoch_rx,
                    },
                ));
                let (sink, _providers) = collector(true);
                let mut request = client.find_providers_request();
                request.get().set_key(test_cid());
                request.get().set_count(5);
                request.get().set_sink(sink);
                let call = request.send().promise;

                let (reply, cancel) = match swarm_rx.recv().await.expect("find command") {
                    SwarmCommand::KadFindProviders {
                        count,
                        reply,
                        cancel,
                        ..
                    } => {
                        assert_eq!(count, 5);
                        assert_eq!(reply.capacity(), 1);
                        (reply, cancel)
                    }
                    _ => panic!("expected find command"),
                };
                reply
                    .send(PeerInfo {
                        peer_id: b"peer".to_vec(),
                        addrs: Vec::new(),
                    })
                    .await
                    .expect("send provider");
                assert!(call.await.is_err());
                assert!(*cancel.borrow());
                assert!(swarm_rx.try_recv().is_err());
            })
            .await;
    }

    #[tokio::test]
    async fn finder_deadline_cancels_finite_query() {
        tokio::task::LocalSet::new()
            .run_until(async {
                let (_epoch_tx, epoch_rx) = watch::channel(epoch(1));
                let (swarm_tx, mut swarm_rx) = mpsc::channel(8);
                let server = FinderImpl::new(
                    swarm_tx,
                    EpochGuard {
                        issued_seq: 1,
                        receiver: epoch_rx,
                    },
                )
                .with_query_timeout(Duration::from_millis(10));
                let client = bootstrap_finder(server);
                let (sink, _providers) = collector(false);
                let mut request = client.find_providers_request();
                request.get().set_key(test_cid());
                request.get().set_count(1);
                request.get().set_sink(sink);
                let call = request.send().promise;
                let (held_reply, cancel) = match swarm_rx.recv().await.expect("find command") {
                    SwarmCommand::KadFindProviders { reply, cancel, .. } => (reply, cancel),
                    _ => panic!("expected find command"),
                };
                call.await.expect("deadline is normal completion");
                assert!(*cancel.borrow());
                assert!(swarm_rx.try_recv().is_err());
                drop(held_reply);
            })
            .await;
    }

    #[tokio::test]
    async fn finder_deadline_includes_swarm_command_admission() {
        tokio::task::LocalSet::new()
            .run_until(async {
                let (_epoch_tx, epoch_rx) = watch::channel(epoch(1));
                let (swarm_tx, mut swarm_rx) = mpsc::channel(1);
                swarm_tx
                    .send(SwarmCommand::KadReleaseProviderOwner {
                        owner: ProviderOwnerId(0),
                    })
                    .await
                    .expect("occupy swarm command channel");
                let finder = bootstrap_finder(
                    FinderImpl::new(
                        swarm_tx,
                        EpochGuard {
                            issued_seq: 1,
                            receiver: epoch_rx,
                        },
                    )
                    .with_query_timeout(Duration::from_millis(10)),
                );
                let (sink, _providers) = collector(false);
                let mut request = finder.find_providers_request();
                request.get().set_key(test_cid());
                request.get().set_count(1);
                request.get().set_sink(sink);

                tokio::time::timeout(Duration::from_secs(1), request.send().promise)
                    .await
                    .expect("Finder call exceeded its admission deadline")
                    .expect("admission deadline is normal completion");
                assert!(matches!(
                    swarm_rx.recv().await,
                    Some(SwarmCommand::KadReleaseProviderOwner {
                        owner: ProviderOwnerId(0)
                    })
                ));
                assert!(swarm_rx.try_recv().is_err());
            })
            .await;
    }

    #[tokio::test]
    async fn finder_cancellation_bypasses_a_full_command_channel() {
        tokio::task::LocalSet::new()
            .run_until(async {
                let (_epoch_tx, epoch_rx) = watch::channel(epoch(1));
                let (swarm_tx, mut swarm_rx) = mpsc::channel(1);
                let flood_tx = swarm_tx.clone();
                let finder = bootstrap_finder(
                    FinderImpl::new(
                        swarm_tx,
                        EpochGuard {
                            issued_seq: 1,
                            receiver: epoch_rx,
                        },
                    )
                    .with_query_timeout(Duration::from_millis(10)),
                );
                let (sink, _providers) = collector(false);
                let mut request = finder.find_providers_request();
                request.get().set_key(test_cid());
                request.get().set_count(1);
                request.get().set_sink(sink);
                let call = request.send().promise;

                let (held_reply, cancel) = match swarm_rx.recv().await.expect("find command") {
                    SwarmCommand::KadFindProviders { reply, cancel, .. } => (reply, cancel),
                    _ => panic!("expected find command"),
                };
                flood_tx
                    .send(SwarmCommand::KadReleaseProviderOwner {
                        owner: ProviderOwnerId(0),
                    })
                    .await
                    .expect("fill command channel");

                tokio::time::timeout(Duration::from_secs(1), call)
                    .await
                    .expect("Finder call exceeded its deadline")
                    .expect("deadline is normal completion");
                assert!(*cancel.borrow());
                assert!(matches!(
                    swarm_rx.recv().await,
                    Some(SwarmCommand::KadReleaseProviderOwner {
                        owner: ProviderOwnerId(0)
                    })
                ));
                assert!(swarm_rx.try_recv().is_err());
                drop(held_reply);
            })
            .await;
    }

    #[tokio::test]
    async fn finder_deadline_cancels_while_sink_is_unresponsive() {
        tokio::task::LocalSet::new()
            .run_until(async {
                let (_epoch_tx, epoch_rx) = watch::channel(epoch(1));
                let (swarm_tx, mut swarm_rx) = mpsc::channel(8);
                let server = FinderImpl::new(
                    swarm_tx,
                    EpochGuard {
                        issued_seq: 1,
                        receiver: epoch_rx,
                    },
                )
                .with_query_timeout(Duration::from_millis(10));
                let client = bootstrap_finder(server);
                let sink: routing_capnp::provider_sink::Client =
                    capnp_rpc::new_client(BlockingCollector);
                let mut request = client.find_providers_request();
                request.get().set_key(test_cid());
                request.get().set_count(1);
                request.get().set_sink(sink);
                let call = request.send().promise;
                let (reply, cancel) = match swarm_rx.recv().await.expect("find command") {
                    SwarmCommand::KadFindProviders { reply, cancel, .. } => (reply, cancel),
                    _ => panic!("expected find command"),
                };
                reply
                    .send(PeerInfo {
                        peer_id: b"peer".to_vec(),
                        addrs: Vec::new(),
                    })
                    .await
                    .expect("send provider");

                tokio::time::timeout(Duration::from_secs(1), call)
                    .await
                    .expect("Finder call exceeded its deadline")
                    .expect("deadline is normal completion");
                assert!(*cancel.borrow());
                assert!(swarm_rx.try_recv().is_err());
            })
            .await;
    }

    #[tokio::test]
    async fn stale_epoch_stops_finder_and_announcer() {
        tokio::task::LocalSet::new()
            .run_until(async {
                let (epoch_tx, epoch_rx) = watch::channel(epoch(1));
                let (swarm_tx, mut swarm_rx) = mpsc::channel(8);
                let finder = bootstrap_finder(FinderImpl::new(
                    swarm_tx.clone(),
                    EpochGuard {
                        issued_seq: 1,
                        receiver: epoch_rx.clone(),
                    },
                ));
                let announcer = bootstrap_announcer(AnnouncerImpl::new(
                    swarm_tx,
                    EpochGuard {
                        issued_seq: 1,
                        receiver: epoch_rx,
                    },
                ));
                epoch_tx.send(epoch(2)).expect("advance epoch");

                let finder_error = match finder.find_providers_request().send().promise.await {
                    Ok(_) => panic!("stale Finder call succeeded"),
                    Err(error) => error,
                };
                assert!(finder_error.to_string().contains("staleEpoch"));
                let mut provide = announcer.provide_request();
                provide.get().set_key(test_cid());
                let announcer_error = match provide.send().promise.await {
                    Ok(_) => panic!("stale Announcer call succeeded"),
                    Err(error) => error,
                };
                assert!(announcer_error.to_string().contains("staleEpoch"));
                assert!(swarm_rx.try_recv().is_err());
            })
            .await;
    }

    #[tokio::test]
    async fn epoch_change_cancels_active_finder_query() {
        tokio::task::LocalSet::new()
            .run_until(async {
                let (epoch_tx, epoch_rx) = watch::channel(epoch(1));
                let (swarm_tx, mut swarm_rx) = mpsc::channel(8);
                let finder = bootstrap_finder(FinderImpl::new(
                    swarm_tx,
                    EpochGuard {
                        issued_seq: 1,
                        receiver: epoch_rx,
                    },
                ));
                let (sink, _providers) = collector(false);
                let mut request = finder.find_providers_request();
                request.get().set_key(test_cid());
                request.get().set_count(4);
                request.get().set_sink(sink);
                let call = request.send().promise;
                let (held_reply, cancel) = match swarm_rx.recv().await.expect("find command") {
                    SwarmCommand::KadFindProviders { reply, cancel, .. } => (reply, cancel),
                    _ => panic!("expected find command"),
                };

                epoch_tx.send(epoch(2)).expect("advance epoch");
                let error = match call.await {
                    Ok(_) => panic!("stale active Finder call succeeded"),
                    Err(error) => error,
                };
                assert!(error.to_string().contains("staleEpoch"));
                assert!(*cancel.borrow());
                assert!(swarm_rx.try_recv().is_err());
                drop(held_reply);
            })
            .await;
    }

    #[tokio::test]
    async fn announcer_releases_owner_when_epoch_ends() {
        tokio::task::LocalSet::new()
            .run_until(async {
                let (epoch_tx, epoch_rx) = watch::channel(epoch(1));
                let (swarm_tx, mut swarm_rx) = mpsc::channel(8);
                let client = bootstrap_announcer(AnnouncerImpl::new(
                    swarm_tx,
                    EpochGuard { issued_seq: 1, receiver: epoch_rx },
                ));
                let mut request = client.provide_request();
                request.get().set_key(test_cid());
                let call = request.send().promise;
                let (owner, reply) = match swarm_rx.recv().await.expect("provide command") {
                    SwarmCommand::KadProvide { owner, reply, .. } => (owner, reply),
                    _ => panic!("expected provide command"),
                };
                reply.send(Ok(())).expect("provide reply");
                call.await.expect("provide succeeds");
                epoch_tx.send(epoch(2)).expect("advance epoch");
                assert!(matches!(
                    swarm_rx.recv().await,
                    Some(SwarmCommand::KadReleaseProviderOwner { owner: released }) if released == owner
                ));
            })
            .await;
    }

    #[tokio::test]
    async fn announcer_releases_owner_when_epoch_source_closes() {
        tokio::task::LocalSet::new()
            .run_until(async {
                let (epoch_tx, epoch_rx) = watch::channel(epoch(1));
                let (swarm_tx, mut swarm_rx) = mpsc::channel(8);
                let client = bootstrap_announcer(AnnouncerImpl::new(
                    swarm_tx,
                    EpochGuard {
                        issued_seq: 1,
                        receiver: epoch_rx,
                    },
                ));
                let mut request = client.provide_request();
                request.get().set_key(test_cid());
                let call = request.send().promise;
                let (owner, reply) = match swarm_rx.recv().await.expect("provide command") {
                    SwarmCommand::KadProvide { owner, reply, .. } => (owner, reply),
                    _ => panic!("expected provide command"),
                };
                reply.send(Ok(())).expect("provide reply");
                call.await.expect("provide succeeds");

                drop(epoch_tx);
                assert!(matches!(
                    swarm_rx.recv().await,
                    Some(SwarmCommand::KadReleaseProviderOwner { owner: released }) if released == owner
                ));
            })
            .await;
    }

    #[tokio::test]
    async fn epoch_change_during_provide_releases_owner_and_fails_call() {
        tokio::task::LocalSet::new()
            .run_until(async {
                let (epoch_tx, epoch_rx) = watch::channel(epoch(1));
                let (swarm_tx, mut swarm_rx) = mpsc::channel(8);
                let announcer = bootstrap_announcer(AnnouncerImpl::new(
                    swarm_tx,
                    EpochGuard {
                        issued_seq: 1,
                        receiver: epoch_rx,
                    },
                ));
                let mut request = announcer.provide_request();
                request.get().set_key(test_cid());
                let call = request.send().promise;
                let (owner, reply) = match swarm_rx.recv().await.expect("provide command") {
                    SwarmCommand::KadProvide { owner, reply, .. } => (owner, reply),
                    _ => panic!("expected provide command"),
                };

                epoch_tx.send(epoch(2)).expect("advance epoch");
                assert!(matches!(
                    swarm_rx.recv().await,
                    Some(SwarmCommand::KadReleaseProviderOwner { owner: released }) if released == owner
                ));
                reply.send(Ok(())).expect("late provide result");
                let error = match call.await {
                    Ok(_) => panic!("provide succeeded after its epoch ended"),
                    Err(error) => error,
                };
                assert!(error.to_string().contains("staleEpoch"));
            })
            .await;
    }

    #[tokio::test]
    async fn local_finder_deduplicates_and_obeys_count() {
        tokio::task::LocalSet::new()
            .run_until(async {
                let routing = LocalProviderRouting::new();
                for peer in [
                    b"a".as_slice(),
                    b"a".as_slice(),
                    b"b".as_slice(),
                    b"c".as_slice(),
                ] {
                    routing.provide_as(
                        test_cid(),
                        PeerInfo {
                            peer_id: peer.to_vec(),
                            addrs: Vec::new(),
                        },
                    );
                }
                let finder = bootstrap_finder(routing.finder());
                let (sink, mut providers) = collector(false);
                let mut request = finder.find_providers_request();
                request.get().set_key(test_cid());
                request.get().set_count(2);
                request.get().set_sink(sink);
                request.send().promise.await.expect("local find");
                assert_eq!(providers.try_recv().expect("one provider").peer_id, b"a");
                assert_eq!(providers.try_recv().expect("second provider").peer_id, b"b");
                assert!(providers.try_recv().is_err());

                let (sink, mut providers) = collector(false);
                let mut request = finder.find_providers_request();
                request.get().set_key(test_cid());
                request.get().set_count(1);
                request.get().set_sink(sink);
                request.send().promise.await.expect("count-one find");
                assert_eq!(providers.try_recv().expect("one provider").peer_id, b"a");
                assert!(providers.try_recv().is_err());
            })
            .await;
    }
}
