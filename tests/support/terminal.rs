use std::time::Duration;

use auth::SigningDomain;
use authority::get_graft_cap;
use capnp::capability::Promise;
use capnp_rpc::{new_client, pry};
use ed25519_dalek::SigningKey;
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
    pub graft: GraftCaps,
}

pub struct GraftCaps {
    pub host: ww::system_capnp::host::Client,
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
        Self { graft }
    }
}

pub async fn expect_stale_or_disconnected(host: &ww::system_capnp::host::Client) {
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
                if let Some(code) = membrane::call_failure_code(&error) {
                    assert_eq!(
                        code,
                        membrane::CallFailureCode::StaleEpoch,
                        "old-generation capability returned the wrong structured failure: {error}"
                    );
                }
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
    assert_exact_names(&names);
    GraftCaps {
        host: get_graft_cap(&caps, "host").expect("graft host capability"),
    }
}

fn assert_exact_names(names: &[String]) {
    assert_eq!(
        names,
        EXPECTED_GRAFT_NAMES.map(str::to_string),
        "Terminal-visible graft name set changed"
    );
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
