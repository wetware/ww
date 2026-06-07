//! Integration test: Epoch → Terminal → Membrane → graft → runtime.shutdown (epoch-guarded).
//! All local: servers and clients are in-process (capnp-rpc local dispatch).
//!
//! Terminal = authentication gate (challenge-response).
//! Membrane = capability provisioning (ocap: having the reference IS authorization).

mod common;

use atom::auth_capnp;
use atom::membrane_capnp;
use atom::system_capnp;
use atom::{AtomIndexer, Epoch, IndexerConfig, MembraneServer, TerminalServer};
use auth::SigningDomain;
use capnp_rpc::new_client;
use common::{deploy_atom, set_head, spawn_anvil, FullStubSessionBuilder, StubSessionBuilder};
use ed25519_dalek::SigningKey;
use membrane::http_capnp;
use membrane::routing_capnp;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::watch;
use tokio::time::timeout;
use tracing_subscriber::EnvFilter;

/// Look up a typed capability by name from the graft caps list.
fn get_graft_cap<T: capnp::capability::FromClientHook>(
    caps: &capnp::struct_list::Reader<'_, membrane_capnp::export::Owned>,
    name: &str,
) -> Result<T, capnp::Error> {
    for i in 0..caps.len() {
        let entry = caps.get(i);
        let n = entry
            .get_name()?
            .to_str()
            .map_err(|e| capnp::Error::failed(e.to_string()))?;
        if n == name {
            return entry.get_cap().get_as_capability();
        }
    }
    Err(capnp::Error::failed(format!(
        "capability '{name}' not found in graft response"
    )))
}

/// Signer that produces libp2p SignedEnvelopes for Terminal challenge-response.
struct TestSigner {
    keypair: libp2p_identity::Keypair,
}

impl TestSigner {
    fn from_ed25519(sk: &SigningKey) -> Self {
        let ed_kp = libp2p_identity::ed25519::Keypair::try_from_bytes(&mut sk.to_keypair_bytes())
            .expect("valid key");
        Self {
            keypair: ed_kp.into(),
        }
    }
}

#[allow(refining_impl_trait)]
impl auth_capnp::signer::Server for TestSigner {
    fn sign(
        self: capnp::capability::Rc<Self>,
        params: auth_capnp::signer::SignParams,
        mut results: auth_capnp::signer::SignResults,
    ) -> capnp::capability::Promise<(), capnp::Error> {
        let p = capnp_rpc::pry!(params.get());
        let nonce = p.get_nonce();
        let epoch_seq = p.get_epoch_seq();
        let domain = SigningDomain::terminal_membrane();

        let mut payload = Vec::with_capacity(16);
        payload.extend_from_slice(&nonce.to_be_bytes());
        payload.extend_from_slice(&epoch_seq.to_be_bytes());

        let envelope = capnp_rpc::pry!(libp2p_core::SignedEnvelope::new(
            &self.keypair,
            domain.as_str().to_string(),
            domain.payload_type().to_vec(),
            payload,
        )
        .map_err(|e| capnp::Error::failed(format!("signing failed: {e}"))));

        results.get().set_sig(&envelope.into_protobuf_encoding());
        capnp::capability::Promise::ok(())
    }
}

fn observed_to_epoch(ev: &atom::HeadUpdatedObserved) -> Epoch {
    Epoch {
        seq: ev.seq,
        head: ev.cid.clone(),
        provenance: membrane::Provenance::Block(ev.block_number),
    }
}

/// Helper: create a Membrane client (no auth — pure ocap).
fn stub_membrane(rx: watch::Receiver<Epoch>) -> membrane_capnp::membrane::Client {
    new_client(MembraneServer::new(rx, StubSessionBuilder))
}

/// Helper: wrap a Membrane client in Terminal (challenge-response auth gate).
fn terminal_membrane(
    rx: watch::Receiver<Epoch>,
    vk: ed25519_dalek::VerifyingKey,
) -> auth_capnp::terminal::Client<membrane_capnp::membrane::Owned> {
    let membrane = stub_membrane(rx.clone());
    new_client(TerminalServer::<membrane_capnp::membrane::Owned>::new(
        vk,
        membrane,
        SigningDomain::terminal_membrane(),
        rx,
    ))
}

#[tokio::test]
async fn test_membrane_graft_runtime_against_anvil() {
    if let Some(reason) = common::foundry_unavailable_reason() {
        eprintln!("skipping test_membrane_graft_runtime_against_anvil: {reason}");
        return;
    }
    let _ = tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::from_default_env().add_directive("atom=debug".parse().unwrap()))
        .with_test_writer()
        .try_init();

    let repo_root = Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(2)
        .unwrap();
    let (mut anvil_process, rpc_url) = spawn_anvil().await.expect("spawn anvil");
    let contract_addr = deploy_atom(repo_root, &rpc_url).expect("deploy Atom");
    let addr_bytes =
        hex::decode(contract_addr.strip_prefix("0x").unwrap_or(&contract_addr)).expect("hex");
    let mut contract_address = [0u8; 20];
    contract_address.copy_from_slice(&addr_bytes);

    set_head(
        repo_root,
        &rpc_url,
        &contract_addr,
        "setHead(bytes)",
        "0x697066732f2f6669727374",
        None,
    )
    .expect("setHead 1");

    let ws_url = rpc_url
        .replace("http://", "ws://")
        .replace("https://", "wss://");
    let config = IndexerConfig {
        ws_url: ws_url.clone(),
        http_url: rpc_url.clone(),
        contract_address,
        start_block: 0,
        getlogs_max_range: 1000,
        reconnection: Default::default(),
    };
    let indexer = Arc::new(AtomIndexer::new(config));
    let mut recv = indexer.subscribe();
    let indexer_clone = Arc::clone(&indexer);
    let indexer_task = tokio::spawn(async move {
        let _ = indexer_clone.run().await;
    });

    let first_ev = timeout(Duration::from_secs(15), async {
        loop {
            if let Ok(ev) = recv.recv().await {
                return ev;
            }
        }
    })
    .await
    .expect("timeout waiting for first event");

    indexer_task.abort();
    let _ = anvil_process.kill();

    let epoch1 = observed_to_epoch(&first_ev);
    let epoch2 = Epoch {
        seq: first_ev.seq + 1,
        head: b"next_head".to_vec(),
        provenance: membrane::Provenance::Block(first_ev.block_number + 1),
    };

    let sk = SigningKey::generate(&mut rand::rngs::OsRng);
    let vk = sk.verifying_key();

    let (tx, rx) = watch::channel(epoch1.clone());
    let terminal = terminal_membrane(rx, vk);
    let signer_client: auth_capnp::signer::Client = new_client(TestSigner::from_ed25519(&sk));

    // Login via Terminal → get Membrane → graft → runtime.shutdown → Ok
    let mut login_req = terminal.login_request();
    login_req.get().set_signer(signer_client);
    let login_resp = login_req.send().promise.await.expect("login RPC");
    let membrane = login_resp
        .get()
        .expect("login results")
        .get_session()
        .expect("session");

    let graft_rpc_response = membrane
        .graft_request()
        .send()
        .promise
        .await
        .expect("graft RPC");
    let graft_response = graft_rpc_response.get().expect("graft results");
    let graft_caps = graft_response.get_caps().expect("caps");
    let runtime: system_capnp::runtime::Client =
        get_graft_cap(&graft_caps, "runtime").expect("runtime");

    // Verify runtime works under current epoch (shutdown succeeds).
    runtime
        .shutdown_request()
        .send()
        .promise
        .await
        .expect("shutdown RPC");

    // Advance epoch → same runtime.shutdown → staleEpoch error
    tx.send(epoch2).unwrap();
    match runtime.shutdown_request().send().promise.await {
        Ok(_) => panic!("shutdown should fail with RPC error after epoch advance"),
        Err(e) => assert!(
            e.to_string().contains("staleEpoch"),
            "error should mention staleEpoch, got: {e}"
        ),
    }
}

/// No-chain regression test: Membrane graft works without auth (pure ocap).
#[tokio::test]
async fn test_membrane_graft_no_auth() {
    let epoch = Epoch {
        seq: 1,
        head: b"head1".to_vec(),
        provenance: membrane::Provenance::Block(100),
    };

    let (_tx, rx) = watch::channel(epoch);
    let membrane = stub_membrane(rx);

    // graft() is parameterless — having the reference IS authorization.
    let graft_resp = membrane
        .graft_request()
        .send()
        .promise
        .await
        .expect("graft RPC");
    let results = graft_resp.get().expect("graft results");
    let graft_caps = results.get_caps().expect("caps");
    let runtime: system_capnp::runtime::Client =
        get_graft_cap(&graft_caps, "runtime").expect("runtime");

    // Verify runtime works (shutdown succeeds under current epoch).
    runtime
        .shutdown_request()
        .send()
        .promise
        .await
        .expect("shutdown RPC");
}

/// No-chain: runtime.shutdown fails with staleEpoch after epoch advance, then re-graft recovers.
#[tokio::test]
async fn test_membrane_stale_epoch_then_recovery_no_chain() {
    let epoch1 = Epoch {
        seq: 1,
        head: b"head1".to_vec(),
        provenance: membrane::Provenance::Block(100),
    };
    let epoch2 = Epoch {
        seq: 2,
        head: b"head2".to_vec(),
        provenance: membrane::Provenance::Block(101),
    };

    let (tx, rx) = watch::channel(epoch1.clone());
    let membrane = stub_membrane(rx);

    // Graft → runtime.shutdown → Ok
    let graft_resp = membrane
        .graft_request()
        .send()
        .promise
        .await
        .expect("graft RPC");
    let graft_results = graft_resp.get().expect("graft results");
    let graft_caps = graft_results.get_caps().expect("caps");
    let runtime: system_capnp::runtime::Client =
        get_graft_cap(&graft_caps, "runtime").expect("runtime");

    runtime
        .shutdown_request()
        .send()
        .promise
        .await
        .expect("shutdown should succeed under current epoch");

    // Advance epoch → same runtime.shutdown → staleEpoch
    tx.send(epoch2).unwrap();
    match runtime.shutdown_request().send().promise.await {
        Ok(_) => panic!("shutdown should fail with RPC error after epoch advance"),
        Err(e) => assert!(
            e.to_string().contains("staleEpoch"),
            "error should mention staleEpoch, got: {e}"
        ),
    }

    // Re-graft → new runtime.shutdown → Ok
    let graft_resp2 = membrane
        .graft_request()
        .send()
        .promise
        .await
        .expect("re-graft RPC");
    let results2 = graft_resp2.get().expect("re-graft results");
    let caps2 = results2.get_caps().expect("caps");
    let runtime2: system_capnp::runtime::Client =
        get_graft_cap(&caps2, "runtime").expect("runtime");

    runtime2
        .shutdown_request()
        .send()
        .promise
        .await
        .expect("shutdown after re-graft should succeed");
}

/// Terminal login with wrong key should fail authentication.
#[tokio::test]
async fn test_terminal_wrong_key_rejected() {
    let epoch = Epoch {
        seq: 1,
        head: b"head".to_vec(),
        provenance: membrane::Provenance::Block(100),
    };

    // Terminal expects key A, signer holds key B.
    let sk_a = SigningKey::generate(&mut rand::rngs::OsRng);
    let sk_b = SigningKey::generate(&mut rand::rngs::OsRng);
    let vk_a = sk_a.verifying_key();

    let (_tx, rx) = watch::channel(epoch);
    let terminal = terminal_membrane(rx, vk_a);
    let signer_client: auth_capnp::signer::Client = new_client(TestSigner::from_ed25519(&sk_b));

    let mut login_req = terminal.login_request();
    login_req.get().set_signer(signer_client);

    match login_req.send().promise.await {
        Ok(resp) => match resp.get() {
            Ok(_) => panic!("login should fail with wrong key"),
            Err(e) => assert!(
                e.to_string().contains("login auth failed"),
                "error should mention login auth failure, got: {e}"
            ),
        },
        Err(e) => assert!(
            e.to_string().contains("login auth failed"),
            "error should mention login auth failure, got: {e}"
        ),
    }
}

/// Terminal login without signer should fail.
#[tokio::test]
async fn test_terminal_missing_signer_rejected() {
    let epoch = Epoch {
        seq: 1,
        head: b"head".to_vec(),
        provenance: membrane::Provenance::Block(100),
    };

    let sk = SigningKey::generate(&mut rand::rngs::OsRng);
    let vk = sk.verifying_key();

    let (_tx, rx) = watch::channel(epoch);
    let terminal = terminal_membrane(rx, vk);

    // Call login without setting signer.
    let login_req = terminal.login_request();
    match login_req.send().promise.await {
        Ok(resp) => match resp.get() {
            Ok(_) => panic!("login should fail without signer"),
            Err(e) => assert!(
                e.to_string().contains("missing signer"),
                "error should mention missing signer, got: {e}"
            ),
        },
        Err(e) => assert!(
            e.to_string().contains("missing signer"),
            "error should mention missing signer, got: {e}"
        ),
    }
}

/// Helper: create a Membrane client with all 5 capabilities populated.
fn full_stub_membrane(rx: watch::Receiver<Epoch>) -> membrane_capnp::membrane::Client {
    new_client(MembraneServer::new(rx, FullStubSessionBuilder))
}

/// Verify that graft() returns all 5 capabilities: identity, host, runtime, routing, http-client.
#[tokio::test]
async fn test_graft_returns_all_five_capabilities() {
    let epoch = Epoch {
        seq: 1,
        head: b"head".to_vec(),
        provenance: membrane::Provenance::Block(100),
    };

    let (_tx, rx) = watch::channel(epoch);
    let membrane = full_stub_membrane(rx);

    let graft_resp = membrane
        .graft_request()
        .send()
        .promise
        .await
        .expect("graft RPC");
    let results = graft_resp.get().expect("graft results");
    let caps = results.get_caps().expect("caps");

    // All 5 capabilities must be present by name.
    assert_eq!(caps.len(), 5, "expected 5 capabilities");
    let _identity: auth_capnp::identity::Client =
        get_graft_cap(&caps, "identity").expect("identity capability should be present");
    let _host: system_capnp::host::Client =
        get_graft_cap(&caps, "host").expect("host capability should be present");
    let runtime: system_capnp::runtime::Client =
        get_graft_cap(&caps, "runtime").expect("runtime capability should be present");
    let _routing: routing_capnp::routing::Client =
        get_graft_cap(&caps, "routing").expect("routing capability should be present");
    let _http_client: http_capnp::http_client::Client =
        get_graft_cap(&caps, "http-client").expect("http-client capability should be present");

    // Verify runtime actually works (shutdown succeeds).
    runtime
        .shutdown_request()
        .send()
        .promise
        .await
        .expect("shutdown RPC");
}

/// Test Terminal-gated Membrane over a VatNetwork stream pair (simulates the
/// libp2p `/ww/0.1.0` path from `serve_one_terminal_stream` in executor.rs).
///
/// Server side: bootstrap = Terminal(Membrane).
/// Client side: bootstrap Terminal → login(signer) → get Membrane → graft → runtime.shutdown.
#[tokio::test]
async fn test_terminal_over_stream_pair() {
    use capnp_rpc::rpc_twoparty_capnp::Side;
    use capnp_rpc::twoparty::VatNetwork;
    use capnp_rpc::RpcSystem;
    use futures::AsyncReadExt;

    let epoch = Epoch {
        seq: 1,
        head: b"head".to_vec(),
        provenance: membrane::Provenance::Block(100),
    };
    let sk = SigningKey::generate(&mut rand::rngs::OsRng);
    let vk = sk.verifying_key();
    let (_tx, rx) = watch::channel(epoch);
    let membrane = full_stub_membrane(rx.clone());

    let terminal = TerminalServer::<membrane_capnp::membrane::Owned>::new(
        vk,
        membrane,
        SigningDomain::terminal_membrane(),
        rx,
    );
    let terminal_client: auth_capnp::terminal::Client<membrane_capnp::membrane::Owned> =
        new_client(terminal);

    let (client_stream, server_stream) = tokio::io::duplex(4096);

    // RpcSystem is !Send, so we need a LocalSet.
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async move {
            let (sr, sw) =
                tokio_util::compat::TokioAsyncReadCompatExt::compat(server_stream).split();
            let server_network = VatNetwork::new(sr, sw, Side::Server, Default::default());
            let server_rpc = RpcSystem::new(Box::new(server_network), Some(terminal_client.client));

            let (cr, cw) =
                tokio_util::compat::TokioAsyncReadCompatExt::compat(client_stream).split();
            let client_network = VatNetwork::new(cr, cw, Side::Client, Default::default());
            let mut client_rpc =
                RpcSystem::new(Box::new(client_network), None::<capnp::capability::Client>);
            let remote_terminal: auth_capnp::terminal::Client<membrane_capnp::membrane::Owned> =
                client_rpc.bootstrap(Side::Server);

            tokio::task::spawn_local(async move {
                let _ = server_rpc.await;
            });
            tokio::task::spawn_local(async move {
                let _ = client_rpc.await;
            });

            // Login with correct signer → get Membrane → graft → runtime.shutdown.
            let signer_client: auth_capnp::signer::Client =
                new_client(TestSigner::from_ed25519(&sk));
            let mut login_req = remote_terminal.login_request();
            login_req.get().set_signer(signer_client);

            let login_resp = timeout(Duration::from_secs(5), login_req.send().promise)
                .await
                .expect("login timed out")
                .expect("login RPC");

            let remote_membrane: membrane_capnp::membrane::Client = login_resp
                .get()
                .expect("login results")
                .get_session()
                .expect("session");

            let graft_resp = timeout(
                Duration::from_secs(5),
                remote_membrane.graft_request().send().promise,
            )
            .await
            .expect("graft timed out")
            .expect("graft RPC");

            let results = graft_resp.get().expect("graft results");
            let stream_caps = results.get_caps().expect("caps");
            let runtime: system_capnp::runtime::Client =
                get_graft_cap(&stream_caps, "runtime").expect("runtime");
            timeout(
                Duration::from_secs(5),
                runtime.shutdown_request().send().promise,
            )
            .await
            .expect("shutdown timed out")
            .expect("shutdown RPC");
        })
        .await;
}

/// Test that Terminal-over-stream rejects login with wrong key.
#[tokio::test]
async fn test_terminal_over_stream_wrong_key_rejected() {
    use capnp_rpc::rpc_twoparty_capnp::Side;
    use capnp_rpc::twoparty::VatNetwork;
    use capnp_rpc::RpcSystem;
    use futures::AsyncReadExt;

    let epoch = Epoch {
        seq: 1,
        head: b"head".to_vec(),
        provenance: membrane::Provenance::Block(100),
    };
    let host_sk = SigningKey::generate(&mut rand::rngs::OsRng);
    let host_vk = host_sk.verifying_key();
    let wrong_sk = SigningKey::generate(&mut rand::rngs::OsRng);

    let (_tx, rx) = watch::channel(epoch);
    let membrane = full_stub_membrane(rx.clone());

    let terminal = TerminalServer::<membrane_capnp::membrane::Owned>::new(
        host_vk,
        membrane,
        SigningDomain::terminal_membrane(),
        rx,
    );
    let terminal_client: auth_capnp::terminal::Client<membrane_capnp::membrane::Owned> =
        new_client(terminal);

    let (client_stream, server_stream) = tokio::io::duplex(4096);

    let local = tokio::task::LocalSet::new();
    local
        .run_until(async move {
            let (sr, sw) =
                tokio_util::compat::TokioAsyncReadCompatExt::compat(server_stream).split();
            let server_network = VatNetwork::new(sr, sw, Side::Server, Default::default());
            let server_rpc = RpcSystem::new(Box::new(server_network), Some(terminal_client.client));

            let (cr, cw) =
                tokio_util::compat::TokioAsyncReadCompatExt::compat(client_stream).split();
            let client_network = VatNetwork::new(cr, cw, Side::Client, Default::default());
            let mut client_rpc =
                RpcSystem::new(Box::new(client_network), None::<capnp::capability::Client>);
            let remote_terminal: auth_capnp::terminal::Client<membrane_capnp::membrane::Owned> =
                client_rpc.bootstrap(Side::Server);

            tokio::task::spawn_local(async move {
                let _ = server_rpc.await;
            });
            tokio::task::spawn_local(async move {
                let _ = client_rpc.await;
            });

            // Login with wrong key — should fail.
            let signer_client: auth_capnp::signer::Client =
                new_client(TestSigner::from_ed25519(&wrong_sk));
            let mut login_req = remote_terminal.login_request();
            login_req.get().set_signer(signer_client);

            let result = timeout(Duration::from_secs(5), login_req.send().promise).await;
            match result {
                Ok(Ok(resp)) => match resp.get() {
                    Ok(_) => panic!("login should fail with wrong key"),
                    Err(e) => assert!(
                        e.to_string().contains("login auth failed"),
                        "expected login auth failure error, got: {e}"
                    ),
                },
                Ok(Err(e)) => assert!(
                    e.to_string().contains("login auth failed"),
                    "expected login auth failure error, got: {e}"
                ),
                Err(_) => panic!("login timed out — expected auth failure"),
            }
        })
        .await;
}

/// Signer that returns malformed signature bytes.
struct MalformedSigner;

#[allow(refining_impl_trait)]
impl auth_capnp::signer::Server for MalformedSigner {
    fn sign(
        self: capnp::capability::Rc<Self>,
        _params: auth_capnp::signer::SignParams,
        mut results: auth_capnp::signer::SignResults,
    ) -> capnp::capability::Promise<(), capnp::Error> {
        // 64 bytes of 0xFF is not a valid signed envelope.
        results.get().set_sig(&[0xFF; 64]);
        capnp::capability::Promise::ok(())
    }
}

/// Terminal should reject login when the signer returns malformed signature bytes.
#[tokio::test]
async fn test_terminal_malformed_signature_rejected() {
    let epoch = Epoch {
        seq: 1,
        head: b"head".to_vec(),
        provenance: membrane::Provenance::Block(100),
    };

    let sk = SigningKey::generate(&mut rand::rngs::OsRng);
    let vk = sk.verifying_key();

    let (_tx, rx) = watch::channel(epoch);
    let terminal = terminal_membrane(rx, vk);
    let signer_client: auth_capnp::signer::Client = new_client(MalformedSigner);

    let mut login_req = terminal.login_request();
    login_req.get().set_signer(signer_client);

    match login_req.send().promise.await {
        Ok(resp) => match resp.get() {
            Ok(_) => panic!("login should fail with malformed signature"),
            Err(e) => assert!(
                e.to_string().contains("invalid signed envelope"),
                "error should mention invalid signed envelope, got: {e}"
            ),
        },
        Err(e) => assert!(
            e.to_string().contains("invalid signed envelope"),
            "error should mention invalid signed envelope, got: {e}"
        ),
    }
}

/// Terminal login should fail after epoch advances (epoch-bound signature is stale).
#[tokio::test]
async fn test_terminal_login_fails_after_epoch_advance() {
    let epoch1 = Epoch {
        seq: 1,
        head: b"head1".to_vec(),
        provenance: membrane::Provenance::Block(100),
    };

    let sk = SigningKey::generate(&mut rand::rngs::OsRng);
    let vk = sk.verifying_key();

    let (tx, rx) = watch::channel(epoch1);
    let terminal = terminal_membrane(rx, vk);

    // Login succeeds under epoch 1.
    let signer1: auth_capnp::signer::Client = new_client(TestSigner::from_ed25519(&sk));
    let mut req1 = terminal.login_request();
    req1.get().set_signer(signer1);
    req1.send()
        .promise
        .await
        .expect("login should succeed under epoch 1");

    // Advance epoch.
    tx.send(Epoch {
        seq: 2,
        head: b"head2".to_vec(),
        provenance: membrane::Provenance::Block(101),
    })
    .unwrap();

    // Login again — the Terminal issues a challenge bound to epoch 2,
    // and the signer signs it correctly, so this should succeed too.
    let signer2: auth_capnp::signer::Client = new_client(TestSigner::from_ed25519(&sk));
    let mut req2 = terminal.login_request();
    req2.get().set_signer(signer2);
    req2.send()
        .promise
        .await
        .expect("login should succeed under epoch 2 (signer signs the new epoch_seq)");
}
