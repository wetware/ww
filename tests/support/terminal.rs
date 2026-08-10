use std::time::Duration;

use auth::SigningDomain;
use authority::get_graft_cap;
use capnp::capability::Promise;
use capnp::traits::HasTypeId;
use capnp_rpc::{new_client, pry};
use ed25519_dalek::{Signer as _, SigningKey};
use libp2p::{Multiaddr, PeerId, StreamProtocol};
use libp2p_core::SignedEnvelope;
use tokio::sync::oneshot;

const CAPNP_PROTOCOL: StreamProtocol = StreamProtocol::new("/ww/0.1.0");
const RPC_TIMEOUT: Duration = Duration::from_secs(30);

pub const EXPECTED_GRAFT_NAMES: [&str; 6] = [
    "authority",
    "host",
    "identity",
    "ipfs",
    "routing",
    "runtime",
];

struct LocalSigner {
    keypair: libp2p::identity::Keypair,
}

impl LocalSigner {
    fn from_signing_key(signing_key: &SigningKey) -> Self {
        Self {
            keypair: ww::keys::to_libp2p(signing_key).expect("convert Terminal signing key"),
        }
    }
}

#[allow(refining_impl_trait)]
impl ww::auth_capnp::signer::Server for LocalSigner {
    fn sign(
        self: capnp::capability::Rc<Self>,
        params: ww::auth_capnp::signer::SignParams,
        mut results: ww::auth_capnp::signer::SignResults,
    ) -> Promise<(), capnp::Error> {
        let params = pry!(params.get());
        let mut payload = Vec::with_capacity(16);
        payload.extend_from_slice(&params.get_nonce().to_be_bytes());
        payload.extend_from_slice(&params.get_epoch_seq().to_be_bytes());
        let domain = SigningDomain::terminal_membrane();
        let envelope = pry!(SignedEnvelope::new(
            &self.keypair,
            domain.as_str().to_string(),
            domain.payload_type().to_vec(),
            payload,
        )
        .map_err(|error| capnp::Error::failed(format!("Terminal signing failed: {error}"))));
        results.get().set_sig(&envelope.into_protobuf_encoding());
        Promise::ok(())
    }
}

pub struct TerminalSession {
    membrane: ww::membrane_capnp::membrane::Client,
    pub graft: GraftCaps,
}

pub struct GraftCaps {
    pub names: Vec<String>,
    pub authority: ww::auth_capnp::authority::Client,
    pub host: ww::system_capnp::host::Client,
    pub identity: ww::auth_capnp::identity::Client,
    pub ipfs: ww::system_capnp::ipfs::Client,
    pub routing: ww::routing_capnp::routing::Client,
    pub runtime: ww::system_capnp::runtime::Client,
}

impl TerminalSession {
    pub async fn connect(peer_id: PeerId, address: Multiaddr, signing_key: &SigningKey) -> Self {
        let transport_key = libp2p::identity::Keypair::generate_ed25519();
        let mut swarm = ww::host::ClientSwarm::new(transport_key).expect("build Terminal swarm");
        let mut stream_control = swarm.stream_control();
        swarm.add_peer_addr(peer_id, address);
        let (connected_tx, connected_rx) = oneshot::channel();
        tokio::task::spawn_local(swarm.run(Some(connected_tx)));
        let connected = tokio::time::timeout(RPC_TIMEOUT, connected_rx)
            .await
            .expect("Terminal libp2p connection timed out")
            .expect("Terminal connection notifier dropped")
            .expect("Terminal libp2p connection failed");
        assert_eq!(connected, peer_id, "Terminal connected to wrong peer");

        let stream = tokio::time::timeout(
            RPC_TIMEOUT,
            stream_control.open_stream(peer_id, CAPNP_PROTOCOL),
        )
        .await
        .expect("opening /ww/0.1.0 timed out")
        .expect("open /ww/0.1.0 stream");
        let dial = ww::rpc::vat_dial::connect::<
            _,
            ww::auth_capnp::terminal::Client<ww::membrane_capnp::membrane::Owned>,
        >(stream);
        let terminal = dial.bootstrap;
        drop(dial.driver);

        let membrane = login_membrane(&terminal, signing_key).await;
        let graft = graft(&membrane).await;
        assert_exact_names(&graft.names);
        Self { membrane, graft }
    }

    pub async fn regraft(&self) -> GraftCaps {
        let graft = graft(&self.membrane).await;
        assert_exact_names(&graft.names);
        graft
    }
}

impl GraftCaps {
    pub async fn semantic_probes(
        &self,
        signing_key: &SigningKey,
        status_wasm: &[u8],
        marker_path: &str,
        expected_marker: &[u8],
    ) {
        let host_id = timeout_rpc("host.id", self.host.id_request().send().promise)
            .await
            .get()
            .expect("host.id results")
            .get_peer_id()
            .expect("host peer ID")
            .to_vec();
        assert!(!host_id.is_empty(), "host.id returned an empty peer ID");

        let mut hash = self.routing.hash_request();
        hash.get().set_data(b"pid0-epoch-e2e");
        let hash = timeout_rpc("routing.hash", hash.send().promise).await;
        let hash = hash
            .get()
            .expect("routing.hash results")
            .get_key()
            .expect("routing hash key")
            .to_str()
            .expect("routing hash UTF-8")
            .to_string();
        hash.parse::<cid::Cid>()
            .expect("routing.hash must return a CID");

        let mut read = self.ipfs.read_request();
        read.get().set_path(marker_path);
        let stream = timeout_rpc("ipfs.read", read.send().promise)
            .await
            .get()
            .expect("ipfs.read results")
            .get_stream()
            .expect("ipfs byte stream");
        let marker = read_stream(&stream).await;
        assert_eq!(marker, expected_marker, "IPFS generation marker mismatch");

        let mut load = self.runtime.load_request();
        load.get().set_wasm(status_wasm);
        let executor = timeout_rpc("runtime.load", load.send().promise)
            .await
            .get()
            .expect("runtime.load results")
            .get_executor()
            .expect("runtime executor");
        let executor_cid = timeout_rpc("executor.cid", executor.cid_request().send().promise)
            .await
            .get()
            .expect("executor.cid results")
            .get_cid()
            .expect("executor CID")
            .to_str()
            .expect("executor CID UTF-8")
            .to_string();
        assert_eq!(
            executor_cid,
            ww::kernel::runtime_cid(status_wasm).to_string(),
            "runtime.load/executor.cid must identify the loaded status component"
        );

        let data = b"pid0-e2e-identity-verify";
        let signature = signing_key.sign(data);
        let mut verify = self.identity.verify_request();
        verify.get().set_data(data);
        verify.get().set_signature(&signature.to_bytes());
        verify
            .get()
            .set_pubkey(&signing_key.verifying_key().to_bytes());
        assert!(
            timeout_rpc("identity.verify", verify.send().promise)
                .await
                .get()
                .expect("identity.verify results")
                .get_valid(),
            "identity.verify rejected a valid signature"
        );

        self.probe_authority_login(signing_key, &host_id).await;
    }

    async fn probe_authority_login(&self, signing_key: &SigningKey, host_id: &[u8]) {
        let mut guard = self.authority.guard_request();
        guard
            .get()
            .set_session(ww::auth_capnp::opaque_session::Client {
                client: self.host.clone().client,
            });
        let mut policy = guard.get().init_policy();
        let mut profiles = policy.reborrow().init_profiles(1);
        let mut profile = profiles.reborrow().get(0);
        profile.set_name("host-id");
        let mut methods = profile.init_methods(1);
        let mut method = methods.reborrow().get(0);
        method.set_interface_id(ww::system_capnp::host::Client::TYPE_ID);
        method.set_ordinal(0);
        let mut recipients = policy.init_recipients(1);
        let mut recipient = recipients.reborrow().get(0);
        recipient.set_verifying_key(&signing_key.verifying_key().to_bytes());
        recipient.set_profile("host-id");

        let terminal = timeout_rpc("authority.guard", guard.send().promise)
            .await
            .get()
            .expect("authority.guard results")
            .get_terminal()
            .expect("authority Terminal");
        let opaque = login_opaque(&terminal, signing_key).await;
        let guarded_host = ww::system_capnp::host::Client {
            client: opaque.client,
        };
        let guarded_id = timeout_rpc(
            "authority Terminal host.id",
            guarded_host.id_request().send().promise,
        )
        .await
        .get()
        .expect("guarded host.id results")
        .get_peer_id()
        .expect("guarded peer ID")
        .to_vec();
        assert_eq!(guarded_id, host_id, "authority-issued host.id changed");
    }
}

pub async fn expect_stale_host(host: &ww::system_capnp::host::Client) {
    let deadline = tokio::time::Instant::now() + RPC_TIMEOUT;
    loop {
        match tokio::time::timeout(Duration::from_secs(2), host.id_request().send().promise).await {
            Ok(Ok(_)) => {
                assert!(
                    tokio::time::Instant::now() < deadline,
                    "retained old-generation host capability unexpectedly remained live"
                );
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
            Ok(Err(error)) => {
                assert_eq!(
                    membrane::call_failure_code(&error),
                    Some(membrane::CallFailureCode::StaleEpoch),
                    "old-generation capability returned the wrong structured failure: {error}"
                );
                return;
            }
            Err(_) => {
                assert!(
                    tokio::time::Instant::now() < deadline,
                    "old-generation host probe timed out instead of returning StaleEpoch"
                );
            }
        }
    }
}

async fn login_membrane(
    terminal: &ww::auth_capnp::terminal::Client<ww::membrane_capnp::membrane::Owned>,
    signing_key: &SigningKey,
) -> ww::membrane_capnp::membrane::Client {
    let signer: ww::auth_capnp::signer::Client =
        new_client(LocalSigner::from_signing_key(signing_key));
    let mut request = terminal.login_request();
    request.get().set_signer(signer);
    let response = timeout_rpc("Terminal.login", request.send().promise).await;
    let results = response.get().expect("Terminal.login results");
    assert_eq!(
        results.get_status().expect("known Terminal login status"),
        ww::auth_capnp::LoginStatus::Granted,
        "Terminal login denied: {}",
        results
            .get_detail()
            .ok()
            .and_then(|detail| detail.to_str().ok())
            .unwrap_or("no detail")
    );
    results.get_session().expect("granted Terminal session")
}

async fn login_opaque(
    terminal: &ww::auth_capnp::terminal::Client<ww::auth_capnp::opaque_session::Owned>,
    signing_key: &SigningKey,
) -> ww::auth_capnp::opaque_session::Client {
    let signer: ww::auth_capnp::signer::Client =
        new_client(LocalSigner::from_signing_key(signing_key));
    let mut request = terminal.login_request();
    request.get().set_signer(signer);
    let response = timeout_rpc("Authority Terminal.login", request.send().promise).await;
    let results = response.get().expect("Authority Terminal.login results");
    assert_eq!(
        results
            .get_status()
            .expect("known Authority Terminal login status"),
        ww::auth_capnp::LoginStatus::Granted,
        "Authority Terminal login denied"
    );
    results
        .get_session()
        .expect("granted Authority Terminal session")
}

async fn graft(membrane: &ww::membrane_capnp::membrane::Client) -> GraftCaps {
    let response = timeout_rpc("Membrane.graft", membrane.graft_request().send().promise).await;
    let caps = response
        .get()
        .expect("Membrane.graft results")
        .get_caps()
        .expect("graft caps");
    let mut names = Vec::with_capacity(caps.len() as usize);
    for index in 0..caps.len() {
        names.push(
            caps.get(index)
                .get_name()
                .expect("graft cap name")
                .to_str()
                .expect("graft cap name UTF-8")
                .to_string(),
        );
    }
    names.sort();
    GraftCaps {
        authority: get_graft_cap(&caps, "authority").expect("graft authority capability"),
        host: get_graft_cap(&caps, "host").expect("graft host capability"),
        identity: get_graft_cap(&caps, "identity").expect("graft identity capability"),
        ipfs: get_graft_cap(&caps, "ipfs").expect("graft ipfs capability"),
        routing: get_graft_cap(&caps, "routing").expect("graft routing capability"),
        runtime: get_graft_cap(&caps, "runtime").expect("graft runtime capability"),
        names,
    }
}

fn assert_exact_names(names: &[String]) {
    assert_eq!(
        names,
        EXPECTED_GRAFT_NAMES.map(str::to_string),
        "Terminal-visible graft name set changed"
    );
}

async fn read_stream(stream: &ww::system_capnp::byte_stream::Client) -> Vec<u8> {
    let mut output = Vec::new();
    loop {
        let mut request = stream.read_request();
        request.get().set_max_bytes(64 * 1024);
        let chunk = timeout_rpc("ByteStream.read", request.send().promise).await;
        let chunk = chunk
            .get()
            .expect("ByteStream.read results")
            .get_data()
            .expect("ByteStream data");
        if chunk.is_empty() {
            return output;
        }
        output.extend_from_slice(chunk);
    }
}

async fn timeout_rpc<T>(
    operation: &str,
    promise: capnp::capability::Promise<capnp::capability::Response<T>, capnp::Error>,
) -> capnp::capability::Response<T>
where
    T: capnp::traits::Owned,
{
    tokio::time::timeout(RPC_TIMEOUT, promise)
        .await
        .unwrap_or_else(|_| panic!("{operation} timed out"))
        .unwrap_or_else(|error| panic!("{operation} failed: {error}"))
}
