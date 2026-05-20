//! Epoch-guarded Routing capability backed by the in-process Kademlia client.
//!
//! Implements `routing_capnp::routing::Server` by dispatching to the swarm
//! event loop via `SwarmCommand::KadProvide` / `SwarmCommand::KadFindProviders`.
//! All methods check the epoch guard before proceeding.
//!
//! Data-plane reads flow through the WASI virtual filesystem.
//! This capability provides routing plus explicit write/publish control ops.

use blake3;
use capnp::capability::Promise;
use capnp_rpc::pry;
use cid::Cid;
use std::path::{Component, Path};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};
use tokio::sync::{mpsc, oneshot};

use membrane::EpochGuard;

use crate::SwarmCommand;
use membrane::routing_capnp;

/// Convert a CID string to Kademlia record key bytes (multihash).
///
/// Provider records in the Amino DHT are keyed by the multihash of the CID.
fn cid_to_kad_key(cid_str: &str) -> Result<Vec<u8>, capnp::Error> {
    let cid: Cid = cid_str
        .parse()
        .map_err(|e| capnp::Error::failed(format!("invalid CID '{cid_str}': {e}")))?;
    Ok(cid.hash().to_bytes())
}

fn parse_cid_text(cid_str: &str, field: &str) -> Result<Cid, capnp::Error> {
    cid_str
        .parse()
        .map_err(|e| capnp::Error::failed(format!("invalid {field} CID '{cid_str}': {e}")))
}

fn normalize_rel_path(path: &str) -> Result<String, capnp::Error> {
    let trimmed = path.trim();
    if trimmed.is_empty() {
        return Err(capnp::Error::failed("path must not be empty".into()));
    }

    let mut parts: Vec<String> = Vec::new();
    for c in Path::new(trimmed).components() {
        match c {
            Component::Normal(seg) => {
                let s = seg
                    .to_str()
                    .ok_or_else(|| capnp::Error::failed("path must be valid UTF-8".into()))?;
                if s.is_empty() {
                    continue;
                }
                parts.push(s.to_string());
            }
            Component::CurDir => {}
            Component::RootDir | Component::Prefix(_) | Component::ParentDir => {
                return Err(capnp::Error::failed(format!(
                    "path must be relative and must not contain '..': {path}"
                )));
            }
        }
    }

    if parts.is_empty() {
        return Err(capnp::Error::failed("path must not be empty".into()));
    }
    Ok(parts.join("/"))
}

fn maybe_normalize_ipfs_path(path: &str) -> String {
    let trimmed = path.trim();
    if trimmed.is_empty() {
        return String::new();
    }
    if trimmed.starts_with("/ipfs/") {
        return trimmed.to_string();
    }
    format!("/ipfs/{}", trimmed.trim_start_matches('/'))
}

fn enforce_publish_expected(
    name: &str,
    expected_current: &str,
    current_resolved: &str,
) -> Result<(), capnp::Error> {
    let expected = maybe_normalize_ipfs_path(expected_current);
    if expected.is_empty() {
        return Ok(());
    }
    let current = maybe_normalize_ipfs_path(current_resolved);
    if current != expected {
        return Err(capnp::Error::failed(format!(
            "ipns compare-and-set failed for {name}: expected {expected}, current {current}"
        )));
    }
    Ok(())
}

static WORKSPACE_SEQ: AtomicU64 = AtomicU64::new(1);

fn next_workspace_path() -> String {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let seq = WORKSPACE_SEQ.fetch_add(1, Ordering::Relaxed);
    format!("/ww/stage5/{now}-{seq}")
}

/// In-memory routing table for deterministic integration tests.
///
/// No DHT, no swarm, no epoch guard — just a `HashMap<String, Vec<PeerInfo>>`.
/// Multiple nodes can share the same `LocalRouting` (via `Arc<Mutex<…>>`) to
/// simulate provide/findProviders without network non-determinism.
pub struct LocalRouting {
    providers:
        std::sync::Arc<std::sync::Mutex<std::collections::HashMap<String, Vec<crate::PeerInfo>>>>,
}

impl Default for LocalRouting {
    fn default() -> Self {
        Self {
            providers: std::sync::Arc::new(std::sync::Mutex::new(std::collections::HashMap::new())),
        }
    }
}

impl LocalRouting {
    pub fn new() -> Self {
        Self::default()
    }

    /// Create a second handle to the same provider table.
    pub fn clone_table(&self) -> Self {
        Self {
            providers: self.providers.clone(),
        }
    }

    /// Pre-seed a provider entry so `findProviders` returns it.
    pub fn provide_as(&self, cid: &str, peer: crate::PeerInfo) {
        let mut table = self.providers.lock().unwrap();
        table.entry(cid.to_string()).or_default().push(peer);
    }
}

#[allow(refining_impl_trait)]
impl routing_capnp::routing::Server for LocalRouting {
    fn provide(
        self: capnp::capability::Rc<Self>,
        params: routing_capnp::routing::ProvideParams,
        _results: routing_capnp::routing::ProvideResults,
    ) -> Promise<(), capnp::Error> {
        let key_str = pry!(pry!(params.get()).get_key())
            .to_string()
            .unwrap_or_default();
        let _: Cid = pry!(key_str
            .parse()
            .map_err(|e| capnp::Error::failed(format!("invalid CID '{key_str}': {e}"))));

        let mut table = self.providers.lock().unwrap();
        table.entry(key_str).or_default();
        Promise::ok(())
    }

    fn find_providers(
        self: capnp::capability::Rc<Self>,
        params: routing_capnp::routing::FindProvidersParams,
        _results: routing_capnp::routing::FindProvidersResults,
    ) -> Promise<(), capnp::Error> {
        let reader = pry!(params.get());
        let key_str = pry!(reader.get_key()).to_string().unwrap_or_default();
        let sink = pry!(reader.get_sink());

        let providers = {
            let table = self.providers.lock().unwrap();
            table.get(&key_str).cloned().unwrap_or_default()
        };

        Promise::from_future(async move {
            for peer_info in &providers {
                let mut req = sink.provider_request();
                let mut info = req.get().get_info()?;
                info.set_peer_id(&peer_info.peer_id);
                let mut addr_list = info.init_addrs(peer_info.addrs.len() as u32);
                for (j, addr) in peer_info.addrs.iter().enumerate() {
                    addr_list.set(j as u32, addr);
                }
                req.send().await?;
            }
            sink.done_request().send().promise.await?;
            Ok(())
        })
    }

    fn hash(
        self: capnp::capability::Rc<Self>,
        params: routing_capnp::routing::HashParams,
        mut results: routing_capnp::routing::HashResults,
    ) -> Promise<(), capnp::Error> {
        let data = pry!(pry!(params.get()).get_data());
        let digest = blake3::hash(data);
        let mh = pry!(
            cid::multihash::Multihash::<64>::wrap(0x1e, digest.as_bytes())
                .map_err(|e| capnp::Error::failed(format!("multihash wrap: {e}")))
        );
        let c = Cid::new_v1(0x55, mh);
        results.get().set_key(c.to_string());
        Promise::ok(())
    }

    fn resolve(
        self: capnp::capability::Rc<Self>,
        _params: routing_capnp::routing::ResolveParams,
        _results: routing_capnp::routing::ResolveResults,
    ) -> Promise<(), capnp::Error> {
        Promise::err(capnp::Error::unimplemented(
            "LocalRouting: resolve is not implemented".into(),
        ))
    }

    fn mkdir(
        self: capnp::capability::Rc<Self>,
        _params: routing_capnp::routing::MkdirParams,
        _results: routing_capnp::routing::MkdirResults,
    ) -> Promise<(), capnp::Error> {
        Promise::err(capnp::Error::unimplemented(
            "LocalRouting: mkdir is not implemented".into(),
        ))
    }

    fn write_file(
        self: capnp::capability::Rc<Self>,
        _params: routing_capnp::routing::WriteFileParams,
        _results: routing_capnp::routing::WriteFileResults,
    ) -> Promise<(), capnp::Error> {
        Promise::err(capnp::Error::unimplemented(
            "LocalRouting: writeFile is not implemented".into(),
        ))
    }

    fn remove(
        self: capnp::capability::Rc<Self>,
        _params: routing_capnp::routing::RemoveParams,
        _results: routing_capnp::routing::RemoveResults,
    ) -> Promise<(), capnp::Error> {
        Promise::err(capnp::Error::unimplemented(
            "LocalRouting: remove is not implemented".into(),
        ))
    }

    fn publish(
        self: capnp::capability::Rc<Self>,
        _params: routing_capnp::routing::PublishParams,
        _results: routing_capnp::routing::PublishResults,
    ) -> Promise<(), capnp::Error> {
        Promise::err(capnp::Error::unimplemented(
            "LocalRouting: publish is not implemented".into(),
        ))
    }
}

/// Routing capability served to guests via the Membrane graft.
pub struct RoutingImpl {
    swarm_cmd_tx: mpsc::Sender<SwarmCommand>,
    guard: EpochGuard,
    ipfs_client: ipfs::HttpClient,
}

impl RoutingImpl {
    pub fn new(
        swarm_cmd_tx: mpsc::Sender<SwarmCommand>,
        guard: EpochGuard,
        ipfs_client: ipfs::HttpClient,
    ) -> Self {
        Self {
            swarm_cmd_tx,
            guard,
            ipfs_client,
        }
    }
}

#[allow(refining_impl_trait)]
impl routing_capnp::routing::Server for RoutingImpl {
    fn provide(
        self: capnp::capability::Rc<Self>,
        params: routing_capnp::routing::ProvideParams,
        _results: routing_capnp::routing::ProvideResults,
    ) -> Promise<(), capnp::Error> {
        pry!(self.guard.check());
        let key_str = pry!(pry!(params.get()).get_key())
            .to_string()
            .unwrap_or_default();
        let key_bytes = pry!(cid_to_kad_key(&key_str));
        let swarm_cmd_tx = self.swarm_cmd_tx.clone();
        Promise::from_future(async move {
            let (reply_tx, reply_rx) = oneshot::channel();
            swarm_cmd_tx
                .send(SwarmCommand::KadProvide {
                    key: key_bytes,
                    reply: reply_tx,
                })
                .await
                .map_err(|_| capnp::Error::failed("swarm channel closed".into()))?;
            reply_rx
                .await
                .map_err(|_| capnp::Error::failed("swarm reply dropped".into()))?
                .map_err(|e| capnp::Error::failed(format!("kad provide failed: {e}")))?;
            Ok(())
        })
    }

    fn find_providers(
        self: capnp::capability::Rc<Self>,
        params: routing_capnp::routing::FindProvidersParams,
        _results: routing_capnp::routing::FindProvidersResults,
    ) -> Promise<(), capnp::Error> {
        pry!(self.guard.check());
        let reader = pry!(params.get());
        let key_str = pry!(reader.get_key()).to_string().unwrap_or_default();
        let sink = pry!(reader.get_sink());
        let key_bytes = pry!(cid_to_kad_key(&key_str));
        let swarm_cmd_tx = self.swarm_cmd_tx.clone();
        Promise::from_future(async move {
            let (provider_tx, mut provider_rx) = mpsc::unbounded_channel();
            swarm_cmd_tx
                .send(SwarmCommand::KadFindProviders {
                    key: key_bytes,
                    reply: provider_tx,
                })
                .await
                .map_err(|_| capnp::Error::failed("swarm channel closed".into()))?;

            // Stream each discovered provider into the caller's sink.
            while let Some(peer_info) = provider_rx.recv().await {
                let mut req = sink.provider_request();
                let mut info = req.get().get_info()?;
                info.set_peer_id(&peer_info.peer_id);
                let mut addr_list = info.init_addrs(peer_info.addrs.len() as u32);
                for (j, addr) in peer_info.addrs.iter().enumerate() {
                    addr_list.set(j as u32, addr);
                }
                // -> stream: awaits until flow control allows the next send.
                req.send().await?;
            }

            // Signal completion.
            sink.done_request().send().promise.await?;
            Ok(())
        })
    }

    fn hash(
        self: capnp::capability::Rc<Self>,
        params: routing_capnp::routing::HashParams,
        mut results: routing_capnp::routing::HashResults,
    ) -> Promise<(), capnp::Error> {
        let data = pry!(pry!(params.get()).get_data());
        let digest = blake3::hash(data);
        // multihash: varint(0x1e) ++ varint(32) ++ blake3(data)
        let mh = pry!(
            cid::multihash::Multihash::<64>::wrap(0x1e, digest.as_bytes())
                .map_err(|e| capnp::Error::failed(format!("multihash wrap: {e}")))
        );
        let c = Cid::new_v1(0x55, mh); // 0x55 = raw codec
        results.get().set_key(c.to_string());
        Promise::ok(())
    }

    fn resolve(
        self: capnp::capability::Rc<Self>,
        params: routing_capnp::routing::ResolveParams,
        mut results: routing_capnp::routing::ResolveResults,
    ) -> Promise<(), capnp::Error> {
        pry!(self.guard.check());
        let name = pry!(pry!(params.get()).get_name())
            .to_string()
            .unwrap_or_default();
        let ipfs_client = self.ipfs_client.clone();
        Promise::from_future(async move {
            let path = ipfs_client
                .name_resolve(&name)
                .await
                .map_err(|e| capnp::Error::failed(format!("IPNS resolve failed: {e}")))?;
            results.get().set_path(&path);
            Ok(())
        })
    }

    fn mkdir(
        self: capnp::capability::Rc<Self>,
        params: routing_capnp::routing::MkdirParams,
        mut results: routing_capnp::routing::MkdirResults,
    ) -> Promise<(), capnp::Error> {
        pry!(self.guard.check());
        let reader = pry!(params.get());
        let base_cid = pry!(reader.get_base_cid()).to_string().unwrap_or_default();
        let rel_path = pry!(reader.get_path()).to_string().unwrap_or_default();
        let parents = reader.get_parents();
        let ipfs_client = self.ipfs_client.clone();
        Promise::from_future(async move {
            let rel = normalize_rel_path(&rel_path)?;
            let _ = parse_cid_text(&base_cid, "base")?;
            let workspace = next_workspace_path();
            let root = format!("{workspace}/root");
            let mfs = ipfs_client.mfs();
            mfs.files_mkdir(&workspace, true)
                .await
                .map_err(|e| capnp::Error::failed(format!("mfs mkdir workspace failed: {e}")))?;
            mfs.files_cp(&format!("/ipfs/{base_cid}"), &root)
                .await
                .map_err(|e| capnp::Error::failed(format!("mfs seed root failed: {e}")))?;
            let target = format!("{root}/{rel}");
            let mutate_res = mfs
                .files_mkdir(&target, parents)
                .await
                .map_err(|e| capnp::Error::failed(format!("mkdir failed at {target}: {e}")));
            let stat_res = ipfs_client.mfs().files_stat(&root, true).await;
            let _ = ipfs_client.mfs().files_rm(&workspace, true).await;
            mutate_res?;
            let stat =
                stat_res.map_err(|e| capnp::Error::failed(format!("mfs stat root failed: {e}")))?;
            let root_cid = stat.hash;
            let _ = parse_cid_text(&root_cid, "result")?;
            results.get().set_root_cid(&root_cid);
            Ok(())
        })
    }

    fn write_file(
        self: capnp::capability::Rc<Self>,
        params: routing_capnp::routing::WriteFileParams,
        mut results: routing_capnp::routing::WriteFileResults,
    ) -> Promise<(), capnp::Error> {
        pry!(self.guard.check());
        let reader = pry!(params.get());
        let base_cid = pry!(reader.get_base_cid()).to_string().unwrap_or_default();
        let rel_path = pry!(reader.get_path()).to_string().unwrap_or_default();
        let data = pry!(reader.get_data()).to_vec();
        let create_parents = reader.get_create_parents();
        let ipfs_client = self.ipfs_client.clone();
        Promise::from_future(async move {
            let rel = normalize_rel_path(&rel_path)?;
            let _ = parse_cid_text(&base_cid, "base")?;
            let leaf_cid = ipfs_client
                .add_bytes(&data)
                .await
                .map_err(|e| capnp::Error::failed(format!("ipfs add bytes failed: {e}")))?;
            let workspace = next_workspace_path();
            let root = format!("{workspace}/root");
            let mfs = ipfs_client.mfs();
            mfs.files_mkdir(&workspace, true)
                .await
                .map_err(|e| capnp::Error::failed(format!("mfs mkdir workspace failed: {e}")))?;
            mfs.files_cp(&format!("/ipfs/{base_cid}"), &root)
                .await
                .map_err(|e| capnp::Error::failed(format!("mfs seed root failed: {e}")))?;
            let target = format!("{root}/{rel}");
            let mutate_res = async {
                if let Some(parent) = Path::new(&target).parent() {
                    if create_parents {
                        let parent_str = parent.to_string_lossy().to_string();
                        mfs.files_mkdir(&parent_str, true).await.map_err(|e| {
                            capnp::Error::failed(format!(
                                "mkdir parents failed at {parent_str}: {e}"
                            ))
                        })?;
                    }
                }
                let _ = mfs.files_rm(&target, false).await;
                mfs.files_cp(&format!("/ipfs/{leaf_cid}"), &target)
                    .await
                    .map_err(|e| {
                        capnp::Error::failed(format!("write file failed at {target}: {e}"))
                    })
            }
            .await;
            let stat_res = ipfs_client.mfs().files_stat(&root, true).await;
            let _ = ipfs_client.mfs().files_rm(&workspace, true).await;
            mutate_res?;
            let stat =
                stat_res.map_err(|e| capnp::Error::failed(format!("mfs stat root failed: {e}")))?;
            let root_cid = stat.hash;
            let _ = parse_cid_text(&root_cid, "result")?;
            results.get().set_root_cid(&root_cid);
            Ok(())
        })
    }

    fn remove(
        self: capnp::capability::Rc<Self>,
        params: routing_capnp::routing::RemoveParams,
        mut results: routing_capnp::routing::RemoveResults,
    ) -> Promise<(), capnp::Error> {
        pry!(self.guard.check());
        let reader = pry!(params.get());
        let base_cid = pry!(reader.get_base_cid()).to_string().unwrap_or_default();
        let rel_path = pry!(reader.get_path()).to_string().unwrap_or_default();
        let recursive = reader.get_recursive();
        let ipfs_client = self.ipfs_client.clone();
        Promise::from_future(async move {
            let rel = normalize_rel_path(&rel_path)?;
            let _ = parse_cid_text(&base_cid, "base")?;
            let workspace = next_workspace_path();
            let root = format!("{workspace}/root");
            let mfs = ipfs_client.mfs();
            mfs.files_mkdir(&workspace, true)
                .await
                .map_err(|e| capnp::Error::failed(format!("mfs mkdir workspace failed: {e}")))?;
            mfs.files_cp(&format!("/ipfs/{base_cid}"), &root)
                .await
                .map_err(|e| capnp::Error::failed(format!("mfs seed root failed: {e}")))?;
            let target = format!("{root}/{rel}");
            let mutate_res = mfs
                .files_rm(&target, recursive)
                .await
                .map_err(|e| capnp::Error::failed(format!("remove failed at {target}: {e}")));
            let stat_res = ipfs_client.mfs().files_stat(&root, true).await;
            let _ = ipfs_client.mfs().files_rm(&workspace, true).await;
            mutate_res?;
            let stat =
                stat_res.map_err(|e| capnp::Error::failed(format!("mfs stat root failed: {e}")))?;
            let root_cid = stat.hash;
            let _ = parse_cid_text(&root_cid, "result")?;
            results.get().set_root_cid(&root_cid);
            Ok(())
        })
    }

    fn publish(
        self: capnp::capability::Rc<Self>,
        params: routing_capnp::routing::PublishParams,
        mut results: routing_capnp::routing::PublishResults,
    ) -> Promise<(), capnp::Error> {
        pry!(self.guard.check());
        let reader = pry!(params.get());
        let name = pry!(reader.get_name()).to_string().unwrap_or_default();
        let cid = pry!(reader.get_cid()).to_string().unwrap_or_default();
        let expected_current = pry!(reader.get_expected_current())
            .to_string()
            .unwrap_or_default();
        let ipfs_client = self.ipfs_client.clone();
        Promise::from_future(async move {
            let parsed = parse_cid_text(&cid, "publish")?;
            let target = format!("/ipfs/{parsed}");
            if !expected_current.trim().is_empty() {
                let current = ipfs_client
                    .name_resolve(&name)
                    .await
                    .map_err(|e| capnp::Error::failed(format!("ipns resolve failed: {e}")))?;
                enforce_publish_expected(&name, &expected_current, &current)?;
            }
            ipfs_client
                .name_publish(&target, &name)
                .await
                .map_err(|e| capnp::Error::failed(format!("ipns publish failed: {e}")))?;
            results.get().set_published_path(&target);
            Ok(())
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::PeerInfo;
    use crate::SwarmCommand;
    use capnp_rpc::rpc_twoparty_capnp::Side;
    use capnp_rpc::twoparty::VatNetwork;
    use capnp_rpc::RpcSystem;
    use membrane::{Epoch, Provenance};
    use tokio::io;
    use tokio::sync::watch;
    use tokio_util::compat::{TokioAsyncReadCompatExt, TokioAsyncWriteCompatExt};

    fn epoch(seq: u64) -> Epoch {
        Epoch {
            seq,
            head: vec![],
            provenance: Provenance::Block(0),
        }
    }

    /// Bootstrap a Routing client/server pair over in-memory duplex.
    ///
    /// Uses a fake swarm channel (receiver dropped); the epoch guard fires
    /// before the channel is ever used in the stale-epoch tests.
    fn setup_routing(guard: EpochGuard) -> routing_capnp::routing::Client {
        let (_rx, client) = setup_routing_with_swarm(guard);
        client
    }

    /// Bootstrap a Routing client/server pair, returning the swarm command
    /// receiver so the caller can mock swarm responses.
    fn setup_routing_with_swarm(
        guard: EpochGuard,
    ) -> (mpsc::Receiver<SwarmCommand>, routing_capnp::routing::Client) {
        let (client_stream, server_stream) = io::duplex(64 * 1024);
        let (client_read, client_write) = io::split(client_stream);
        let (server_read, server_write) = io::split(server_stream);

        let (swarm_tx, swarm_rx) = mpsc::channel(16);
        let routing_impl = RoutingImpl::new(
            swarm_tx,
            guard,
            ipfs::HttpClient::new("http://localhost:5001".into()),
        );
        let routing_server: routing_capnp::routing::Client = capnp_rpc::new_client(routing_impl);

        let server_network = VatNetwork::new(
            server_read.compat(),
            server_write.compat_write(),
            Side::Server,
            Default::default(),
        );
        let server_rpc = RpcSystem::new(Box::new(server_network), Some(routing_server.client));
        tokio::task::spawn_local(async move {
            let _ = server_rpc.await;
        });

        let client_network = VatNetwork::new(
            client_read.compat(),
            client_write.compat_write(),
            Side::Client,
            Default::default(),
        );
        let mut client_rpc = RpcSystem::new(Box::new(client_network), None);
        let client: routing_capnp::routing::Client = client_rpc.bootstrap(Side::Server);
        tokio::task::spawn_local(async move {
            let _ = client_rpc.await;
        });

        (swarm_rx, client)
    }

    // -------------------------------------------------------------------
    // Key derivation — both nodes must agree on the same Kad key
    // -------------------------------------------------------------------

    /// Build a CID the same way the `hash` RPC method does (CIDv1, raw, blake3).
    fn hash_to_cid(data: &[u8]) -> String {
        let digest = blake3::hash(data);
        let mh = cid::multihash::Multihash::<64>::wrap(0x1e, digest.as_bytes()).unwrap();
        Cid::new_v1(0x55, mh).to_string()
    }

    #[test]
    fn test_hash_is_deterministic() {
        let a = hash_to_cid(b"ww.chess.v1");
        let b = hash_to_cid(b"ww.chess.v1");
        assert_eq!(a, b, "same input must produce same CID");
    }

    #[test]
    fn test_cid_to_kad_key_deterministic() {
        let cid = hash_to_cid(b"ww.chess.v1");
        let key_a = cid_to_kad_key(&cid).unwrap();
        let key_b = cid_to_kad_key(&cid).unwrap();
        assert_eq!(key_a, key_b, "same CID must produce same Kad key");
        assert!(!key_a.is_empty());
    }

    #[test]
    fn test_different_inputs_different_keys() {
        let cid_a = hash_to_cid(b"ww.chess.v1");
        let cid_b = hash_to_cid(b"ww.chess.v2");
        let key_a = cid_to_kad_key(&cid_a).unwrap();
        let key_b = cid_to_kad_key(&cid_b).unwrap();
        assert_ne!(key_a, key_b);
    }

    #[test]
    fn test_cid_to_kad_key_rejects_invalid() {
        assert!(cid_to_kad_key("not-a-cid").is_err());
        assert!(cid_to_kad_key("").is_err());
    }

    #[test]
    fn test_normalize_rel_path_accepts_clean_relative_path() {
        let p = normalize_rel_path("apps/demo/main.glia").unwrap();
        assert_eq!(p, "apps/demo/main.glia");
    }

    #[test]
    fn test_normalize_rel_path_rejects_absolute_and_parent() {
        assert!(normalize_rel_path("/etc/passwd").is_err());
        assert!(normalize_rel_path("../demo").is_err());
        assert!(normalize_rel_path("apps/../demo").is_err());
    }

    #[test]
    fn test_publish_compare_and_set_semantics() {
        // Empty expected => unconditional publish.
        assert!(enforce_publish_expected("ww", "", "/ipfs/bafyok").is_ok());
        // Exact match => ok.
        assert!(enforce_publish_expected("ww", "/ipfs/bafyok", "/ipfs/bafyok").is_ok());
        // CID-only expected is normalized to /ipfs/<cid>.
        assert!(enforce_publish_expected("ww", "bafyok", "/ipfs/bafyok").is_ok());
        // Mismatch => conflict.
        assert!(enforce_publish_expected("ww", "/ipfs/bafyold", "/ipfs/bafynew").is_err());
    }

    // -------------------------------------------------------------------
    // Epoch guard tests
    // -------------------------------------------------------------------

    #[tokio::test]
    async fn test_provide_rejects_stale_epoch() {
        let local = tokio::task::LocalSet::new();
        local
            .run_until(async {
                let (tx, rx) = watch::channel(epoch(1));
                let guard = EpochGuard {
                    issued_seq: 1,
                    receiver: rx,
                };
                let client = setup_routing(guard);

                // Advance epoch → stale.
                tx.send(epoch(2)).unwrap();

                let mut req = client.provide_request();
                req.get().set_key("QmTest");
                match req.send().promise.await {
                    Err(e) => assert!(
                        e.to_string().contains("staleEpoch"),
                        "expected staleEpoch, got: {e}"
                    ),
                    Ok(_) => panic!("expected staleEpoch error"),
                }
            })
            .await;
    }

    #[tokio::test]
    async fn test_find_providers_rejects_stale_epoch() {
        let local = tokio::task::LocalSet::new();
        local
            .run_until(async {
                let (tx, rx) = watch::channel(epoch(1));
                let guard = EpochGuard {
                    issued_seq: 1,
                    receiver: rx,
                };
                let client = setup_routing(guard);

                // Advance epoch → stale.
                tx.send(epoch(2)).unwrap();

                // findProviders needs a sink; we don't care about it since
                // the epoch check fires before the sink is read.
                let req = client.find_providers_request();
                match req.send().promise.await {
                    Err(e) => assert!(
                        e.to_string().contains("staleEpoch"),
                        "expected staleEpoch, got: {e}"
                    ),
                    Ok(_) => panic!("expected staleEpoch error"),
                }
            })
            .await;
    }

    // -------------------------------------------------------------------
    // RPC round-trip tests — happy path through Cap'n Proto serialization
    // -------------------------------------------------------------------

    /// RPC round-trip for `hash`: data → CIDv1 (raw, blake3).
    ///
    /// Exercises Cap'n Proto serialization of the Data param and Text result.
    #[tokio::test]
    async fn test_hash_rpc_round_trip() {
        let local = tokio::task::LocalSet::new();
        local
            .run_until(async {
                let (_tx, rx) = watch::channel(epoch(1));
                let guard = EpochGuard {
                    issued_seq: 1,
                    receiver: rx,
                };
                let client = setup_routing(guard);

                let data = b"ww.chess.v1";
                let mut req = client.hash_request();
                req.get().set_data(data);
                let response = req.send().promise.await.expect("hash RPC");
                let key = response
                    .get()
                    .expect("get results")
                    .get_key()
                    .expect("get key")
                    .to_str()
                    .expect("key utf8");

                // Must match local computation.
                let expected = hash_to_cid(data);
                assert_eq!(key, expected, "RPC hash must match local hash_to_cid");

                // Must be a valid CIDv1.
                let cid: Cid = key.parse().expect("result should be a valid CID");
                assert_eq!(cid.version(), cid::Version::V1);
                assert_eq!(cid.codec(), 0x55, "codec should be raw");
            })
            .await;
    }

    /// RPC round-trip for `provide`: server dispatches SwarmCommand::KadProvide,
    /// mock swarm replies Ok.
    #[tokio::test]
    async fn test_provide_rpc_round_trip() {
        let local = tokio::task::LocalSet::new();
        local
            .run_until(async {
                let (_tx, rx) = watch::channel(epoch(1));
                let guard = EpochGuard {
                    issued_seq: 1,
                    receiver: rx,
                };
                let (mut swarm_rx, client) = setup_routing_with_swarm(guard);

                let cid = hash_to_cid(b"ww.chess.v1");
                let expected_key = cid_to_kad_key(&cid).unwrap();

                // Mock swarm: accept the provide command and reply Ok.
                let cid_clone = cid.clone();
                tokio::task::spawn_local(async move {
                    match swarm_rx.recv().await {
                        Some(SwarmCommand::KadProvide { key, reply }) => {
                            let expected = cid_to_kad_key(&cid_clone).unwrap();
                            assert_eq!(key, expected, "swarm should receive correct key");
                            reply.send(Ok(())).ok();
                        }
                        _ => panic!("expected KadProvide command"),
                    }
                });

                let mut req = client.provide_request();
                req.get().set_key(&cid);
                req.send().promise.await.expect("provide should succeed");

                // Verify the key bytes match what we expect.
                assert!(!expected_key.is_empty());
            })
            .await;
    }

    /// `provide` with an invalid CID should fail at the RPC level.
    #[tokio::test]
    async fn test_provide_rejects_invalid_cid() {
        let local = tokio::task::LocalSet::new();
        local
            .run_until(async {
                let (_tx, rx) = watch::channel(epoch(1));
                let guard = EpochGuard {
                    issued_seq: 1,
                    receiver: rx,
                };
                let (_swarm_rx, client) = setup_routing_with_swarm(guard);

                let mut req = client.provide_request();
                req.get().set_key("not-a-valid-cid");
                match req.send().promise.await {
                    Err(e) => assert!(
                        e.to_string().contains("invalid CID"),
                        "expected 'invalid CID' error, got: {e}"
                    ),
                    Ok(_) => panic!("expected error for invalid CID"),
                }
            })
            .await;
    }

    // -- ProviderSink implementation for testing --------------------------

    /// Collects providers streamed via the ProviderSink protocol.
    struct CollectorSink {
        tx: mpsc::UnboundedSender<PeerInfo>,
    }

    impl routing_capnp::provider_sink::Server for CollectorSink {
        // `-> stream` method: takes only Params, returns Future (no Results).
        async fn provider(
            self: capnp::capability::Rc<Self>,
            params: routing_capnp::provider_sink::ProviderParams,
        ) -> Result<(), capnp::Error> {
            let reader = params.get()?;
            let info = reader.get_info()?;
            let peer_id = info.get_peer_id()?.to_vec();
            let addrs_reader = info.get_addrs()?;
            let addrs: Vec<Vec<u8>> = (0..addrs_reader.len())
                .map(|i| addrs_reader.get(i).expect("get addr").to_vec())
                .collect();
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

    /// RPC round-trip for `find_providers`: mock swarm sends providers,
    /// CollectorSink receives them through Cap'n Proto streaming.
    #[tokio::test]
    async fn test_find_providers_rpc_round_trip() {
        let local = tokio::task::LocalSet::new();
        local
            .run_until(async {
                let (_tx, rx) = watch::channel(epoch(1));
                let guard = EpochGuard {
                    issued_seq: 1,
                    receiver: rx,
                };
                let (mut swarm_rx, client) = setup_routing_with_swarm(guard);

                let cid = hash_to_cid(b"ww.chess.v1");

                // Fake providers to stream.
                let fake_providers = vec![
                    PeerInfo {
                        peer_id: b"peer-a".to_vec(),
                        addrs: vec![b"/ip4/1.2.3.4/tcp/4001".to_vec()],
                    },
                    PeerInfo {
                        peer_id: b"peer-b".to_vec(),
                        addrs: vec![
                            b"/ip4/5.6.7.8/tcp/4001".to_vec(),
                            b"/ip4/9.10.11.12/udp/4001/quic-v1".to_vec(),
                        ],
                    },
                ];
                let providers_clone = fake_providers.clone();

                // Mock swarm: send fake providers then drop the channel.
                tokio::task::spawn_local(async move {
                    match swarm_rx.recv().await {
                        Some(SwarmCommand::KadFindProviders { key: _, reply }) => {
                            for p in providers_clone {
                                reply.send(p).ok();
                            }
                            // Drop reply to signal end of stream.
                        }
                        _ => panic!("expected KadFindProviders command"),
                    }
                });

                // Collector sink to receive streamed providers.
                let (collector_tx, mut collector_rx) = mpsc::unbounded_channel();
                let sink: routing_capnp::provider_sink::Client =
                    capnp_rpc::new_client(CollectorSink { tx: collector_tx });

                let mut req = client.find_providers_request();
                req.get().set_key(&cid);
                req.get().set_count(10);
                req.get().set_sink(sink);
                req.send()
                    .promise
                    .await
                    .expect("findProviders should succeed");

                // Collect all received providers.
                let mut received = Vec::new();
                while let Ok(info) = collector_rx.try_recv() {
                    received.push(info);
                }

                assert_eq!(
                    received.len(),
                    fake_providers.len(),
                    "should receive all providers"
                );
                assert_eq!(received[0].peer_id, b"peer-a");
                assert_eq!(received[1].peer_id, b"peer-b");
                assert_eq!(
                    received[1].addrs.len(),
                    2,
                    "second provider should have 2 addrs"
                );
            })
            .await;
    }

    // -------------------------------------------------------------------
    // LocalRouting tests — deterministic, no swarm
    // -------------------------------------------------------------------

    /// Bootstrap a LocalRouting client over in-memory duplex.
    fn setup_local_routing(local: &LocalRouting) -> routing_capnp::routing::Client {
        let (client_stream, server_stream) = io::duplex(64 * 1024);
        let (client_read, client_write) = io::split(client_stream);
        let (server_read, server_write) = io::split(server_stream);

        let routing_server: routing_capnp::routing::Client =
            capnp_rpc::new_client(local.clone_table());

        let server_network = VatNetwork::new(
            server_read.compat(),
            server_write.compat_write(),
            Side::Server,
            Default::default(),
        );
        let server_rpc = RpcSystem::new(Box::new(server_network), Some(routing_server.client));
        tokio::task::spawn_local(async move {
            let _ = server_rpc.await;
        });

        let client_network = VatNetwork::new(
            client_read.compat(),
            client_write.compat_write(),
            Side::Client,
            Default::default(),
        );
        let mut client_rpc = RpcSystem::new(Box::new(client_network), None);
        let client: routing_capnp::routing::Client = client_rpc.bootstrap(Side::Server);
        tokio::task::spawn_local(async move {
            let _ = client_rpc.await;
        });

        client
    }

    #[tokio::test]
    async fn test_local_routing_hash_matches_real() {
        let local = tokio::task::LocalSet::new();
        local
            .run_until(async {
                let routing = LocalRouting::new();
                let client = setup_local_routing(&routing);

                let data = b"ww.chess.v1";
                let mut req = client.hash_request();
                req.get().set_data(data);
                let response = req.send().promise.await.expect("hash RPC");
                let key = response
                    .get()
                    .expect("get results")
                    .get_key()
                    .expect("get key")
                    .to_str()
                    .expect("key utf8");

                assert_eq!(key, hash_to_cid(data));
            })
            .await;
    }

    #[tokio::test]
    async fn test_local_routing_provide_and_find() {
        let local = tokio::task::LocalSet::new();
        local
            .run_until(async {
                let routing = LocalRouting::new();
                let cid = hash_to_cid(b"ww.chess.v1");

                routing.provide_as(
                    &cid,
                    PeerInfo {
                        peer_id: b"peer-local".to_vec(),
                        addrs: vec![b"/ip4/127.0.0.1/tcp/9000".to_vec()],
                    },
                );

                let client = setup_local_routing(&routing);

                let (collector_tx, mut collector_rx) = mpsc::unbounded_channel();
                let sink: routing_capnp::provider_sink::Client =
                    capnp_rpc::new_client(CollectorSink { tx: collector_tx });

                let mut req = client.find_providers_request();
                req.get().set_key(&cid);
                req.get().set_count(10);
                req.get().set_sink(sink);
                req.send()
                    .promise
                    .await
                    .expect("findProviders should succeed");

                let mut received = Vec::new();
                while let Ok(info) = collector_rx.try_recv() {
                    received.push(info);
                }
                assert_eq!(received.len(), 1);
                assert_eq!(received[0].peer_id, b"peer-local");
            })
            .await;
    }

    #[tokio::test]
    async fn test_local_routing_shared_table() {
        let local = tokio::task::LocalSet::new();
        local
            .run_until(async {
                let routing = LocalRouting::new();
                let cid = hash_to_cid(b"ww.chess.v1");

                routing.provide_as(
                    &cid,
                    PeerInfo {
                        peer_id: b"node-a".to_vec(),
                        addrs: vec![b"/ip4/127.0.0.1/tcp/9001".to_vec()],
                    },
                );

                let client_b = setup_local_routing(&routing);

                let (collector_tx, mut collector_rx) = mpsc::unbounded_channel();
                let sink: routing_capnp::provider_sink::Client =
                    capnp_rpc::new_client(CollectorSink { tx: collector_tx });

                let mut req = client_b.find_providers_request();
                req.get().set_key(&cid);
                req.get().set_count(10);
                req.get().set_sink(sink);
                req.send()
                    .promise
                    .await
                    .expect("findProviders should succeed");

                let mut received = Vec::new();
                while let Ok(info) = collector_rx.try_recv() {
                    received.push(info);
                }
                assert_eq!(received.len(), 1);
                assert_eq!(received[0].peer_id, b"node-a");
            })
            .await;
    }

    #[tokio::test]
    async fn test_local_routing_empty_find() {
        let local = tokio::task::LocalSet::new();
        local
            .run_until(async {
                let routing = LocalRouting::new();
                let client = setup_local_routing(&routing);
                let cid = hash_to_cid(b"nonexistent");

                let (collector_tx, mut collector_rx) = mpsc::unbounded_channel();
                let sink: routing_capnp::provider_sink::Client =
                    capnp_rpc::new_client(CollectorSink { tx: collector_tx });

                let mut req = client.find_providers_request();
                req.get().set_key(&cid);
                req.get().set_count(10);
                req.get().set_sink(sink);
                req.send()
                    .promise
                    .await
                    .expect("findProviders (empty) should succeed");

                assert!(collector_rx.try_recv().is_err());
            })
            .await;
    }

    /// `find_providers` with zero providers: swarm drops channel immediately.
    #[tokio::test]
    async fn test_find_providers_empty_result() {
        let local = tokio::task::LocalSet::new();
        local
            .run_until(async {
                let (_tx, rx) = watch::channel(epoch(1));
                let guard = EpochGuard {
                    issued_seq: 1,
                    receiver: rx,
                };
                let (mut swarm_rx, client) = setup_routing_with_swarm(guard);

                let cid = hash_to_cid(b"nonexistent");

                // Mock swarm: drop channel immediately (no providers).
                tokio::task::spawn_local(async move {
                    match swarm_rx.recv().await {
                        Some(SwarmCommand::KadFindProviders { key: _, reply }) => {
                            drop(reply); // no providers
                        }
                        _ => panic!("expected KadFindProviders command"),
                    }
                });

                let (collector_tx, mut collector_rx) = mpsc::unbounded_channel();
                let sink: routing_capnp::provider_sink::Client =
                    capnp_rpc::new_client(CollectorSink { tx: collector_tx });

                let mut req = client.find_providers_request();
                req.get().set_key(&cid);
                req.get().set_count(10);
                req.get().set_sink(sink);
                req.send()
                    .promise
                    .await
                    .expect("findProviders (empty) should succeed");

                assert!(
                    collector_rx.try_recv().is_err(),
                    "should receive zero providers"
                );
            })
            .await;
    }
}
