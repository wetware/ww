//! End-to-end test for the std/status cell.
//!
//! Spawns the real status WASM via Runtime/Executor with WAGI CGI env
//! vars, reads the CGI response from stdout, asserts the JSON body's
//! shape AND that `peer_id` is a non-null base58 string.
//!
//! The `peer_id` non-null assertion is the load-bearing one: it proves
//! the `host` cap from the kernel's default graft actually reaches the
//! WAGI cell's membrane (the contract HttpListener caps propagation
//! landed in #429 makes possible). Without it, a regression would let
//! `peer_id: null` slip through and the engagement starter kit's WHOA 1
//! pitch would be hollow even though the test "passes."
//!
//! Requires pre-built status WASM: `make -C std/status`.

use tokio::sync::{mpsc, watch};

use ww::dispatcher::wagi;
use ww::launcher::create_runtime_client;
use ww::rpc::{CachePolicy, NetworkState};

const STATUS_WASM_PATH: &str = "std/status/bin/status.wasm";

fn status_wasm_exists() -> bool {
    std::path::Path::new(STATUS_WASM_PATH).exists()
}

/// Synthetic peer ID used to seed NetworkState. Real libp2p peer IDs
/// are base58-encoded multihashes — use a real Ed25519 key so the cell's
/// `Multiaddr::try_from` / `bs58::encode` round-trip succeeds in the
/// graceful-degradation paths.
fn synth_peer_id_bytes() -> Vec<u8> {
    let kp = libp2p::identity::Keypair::generate_ed25519();
    libp2p::PeerId::from_public_key(&kp.public()).to_bytes()
}

#[tokio::test(flavor = "current_thread")]
async fn status_cell_serves_json_with_non_null_peer_id() {
    if !status_wasm_exists() {
        eprintln!("skipping: {STATUS_WASM_PATH} not built (run `make -C std/status` first)");
        return;
    }

    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            // ── Set up an in-process runtime with a known peer ID ───────
            //
            // The full membrane (with `host` cap) is only wired when the
            // runtime has BOTH an epoch receiver AND a stream control.
            // Without them, RuntimeImpl falls back to the test-only
            // `build_peer_rpc` path which does NOT expose host. This was
            // the cause of CGI-empty-output failures during dev.
            let network_state = NetworkState::new();
            let peer_id_bytes = synth_peer_id_bytes();
            network_state.set_local_peer_id(peer_id_bytes.clone()).await;

            let epoch = membrane::Epoch {
                seq: 1,
                head: vec![],
                provenance: membrane::Provenance::Block(0),
            };
            let (_epoch_tx, epoch_rx) = watch::channel(epoch);
            let guard = membrane::EpochGuard {
                issued_seq: 1,
                receiver: epoch_rx.clone(),
            };
            let stream_control = libp2p_stream::Behaviour::new().new_control();

            let (swarm_tx, _swarm_rx) = mpsc::channel(16);
            let runtime = create_runtime_client(
                network_state,
                swarm_tx,
                false,
                Some(guard),
                Some(epoch_rx),
                None,
                Some(stream_control),
                None,
                None,
                CachePolicy::Shared,
                ww::ipfs::HttpClient::new("http://localhost:5001".into()),
                Vec::new(),
            );

            // Load the status WASM.
            let wasm = std::fs::read(STATUS_WASM_PATH).expect("failed to read status.wasm");
            let mut load_req = runtime.load_request();
            load_req.get().set_wasm(&wasm);
            let load_resp = load_req.send().promise.await.expect("runtime.load failed");
            let executor = load_resp
                .get()
                .expect("get load response")
                .get_executor()
                .expect("get executor");

            // ── Spawn the cell with WAGI env vars (GET /status) ─────────
            let env = wagi::build_cgi_env(
                "GET",
                "/status",
                "",  // no query string
                &[], // no extra HTTP headers
                "localhost",
                2080,
            );

            let mut spawn_req = executor.spawn_request();
            {
                let mut builder = spawn_req.get();
                let mut env_list = builder.reborrow().init_env(env.len() as u32);
                for (i, e) in env.iter().enumerate() {
                    env_list.set(i as u32, e);
                }
            }
            let spawn_resp = spawn_req
                .send()
                .promise
                .await
                .expect("executor.spawn failed");
            let process = spawn_resp
                .get()
                .expect("get spawn response")
                .get_process()
                .expect("get process");

            // Close stdin (no body for GET) so the WAGI guest's read loop ends.
            let stdin_resp = process
                .stdin_request()
                .send()
                .promise
                .await
                .expect("get stdin");
            let stdin = stdin_resp
                .get()
                .expect("stdin response")
                .get_stream()
                .expect("get stream");
            stdin
                .close_request()
                .send()
                .promise
                .await
                .expect("close stdin");

            // Read stdout until EOF.
            let stdout_resp = process
                .stdout_request()
                .send()
                .promise
                .await
                .expect("get stdout");
            let stdout = stdout_resp
                .get()
                .expect("stdout response")
                .get_stream()
                .expect("get stream");
            let mut response = Vec::new();
            loop {
                let mut read_req = stdout.read_request();
                read_req.get().set_max_bytes(64 * 1024);
                let read_resp = read_req.send().promise.await.expect("stdout.read failed");
                let chunk = read_resp
                    .get()
                    .expect("get read response")
                    .get_data()
                    .expect("get data");
                if chunk.is_empty() {
                    break;
                }
                response.extend_from_slice(chunk);
                if response.len() > 16 * 1024 * 1024 {
                    panic!("status response exceeded 16MiB — guest is misbehaving");
                }
            }

            // Wait for the cell to exit.
            let wait_resp = process
                .wait_request()
                .send()
                .promise
                .await
                .expect("wait failed");
            let exit_code = wait_resp.get().expect("wait response").get_exit_code();
            assert_eq!(exit_code, 0, "status cell should exit cleanly");

            // ── Parse the CGI response ─────────────────────────────────
            let cgi = wagi::parse_cgi_response(&response)
                .expect("CGI parse should succeed — guest produced malformed output");
            assert_eq!(cgi.status_code, 200, "expected HTTP 200");
            let body = std::str::from_utf8(&cgi.body).expect("response body should be UTF-8 JSON");
            let json: serde_json::Value = serde_json::from_str(body)
                .unwrap_or_else(|e| panic!("response should parse as JSON: {e}\nbody: {body}"));

            // Static fields.
            assert_eq!(
                json["status"], "ok",
                "status field should be \"ok\", got: {}",
                json["status"]
            );
            assert!(
                json["version"].as_str().is_some_and(|s| !s.is_empty()),
                "version should be a non-empty string, got: {}",
                json["version"]
            );

            // CRITICAL: peer_id must be non-null. This is what proves the
            // `host` cap actually reached the WAGI cell's membrane. If it
            // came back null, capability propagation regressed and the
            // engagement starter kit's pitch is hollow.
            let peer_id = json["peer_id"].as_str().unwrap_or_else(|| {
                panic!(
                    "peer_id MUST be a non-null base58 string — \
                     null indicates the host cap did not reach the cell. \
                     Body: {body}"
                )
            });
            assert!(
                !peer_id.is_empty(),
                "peer_id should not be empty, got: {peer_id:?}"
            );
            // Sanity check: real libp2p Ed25519 peer IDs are base58 strings
            // starting with "12D" or "Qm". A typo / wrong encoding would
            // produce something else.
            assert!(
                peer_id.starts_with("12D") || peer_id.starts_with("Qm"),
                "peer_id should look like a libp2p base58 PeerID, got: {peer_id:?}"
            );

            // listen_addrs and peer_count should be non-null arrays/integers
            // (NetworkState was seeded with a peer ID; addrs/peers default
            // to empty but not null).
            assert!(
                json["listen_addrs"].is_array(),
                "listen_addrs should be an array, got: {}",
                json["listen_addrs"]
            );
            assert!(
                json["peer_count"].is_number(),
                "peer_count should be a number, got: {}",
                json["peer_count"]
            );
        })
        .await;
}
