//! Status cell — minimal WAGI endpoint reporting node status.
//!
//! Returns JSON describing the running node:
//!
//! ```json
//! {
//!   "status": "ok",
//!   "version": "0.1.2",
//!   "peer_id": "12D3Koo...",
//!   "listen_addrs": ["/ip4/127.0.0.1/tcp/2025", ...],
//!   "peer_count": 3
//! }
//! ```
//!
//! `status` and `version` are always populated. Required `peer_id` metadata
//! comes from the narrow Membrane. `listen_addrs` and `peer_count` come from
//! one `Stat` snapshot. A withheld or unavailable `Stat` degrades those two
//! mutable fields to `null`.
//!
//! WAGI mode only. Runs once per HTTP request — fresh cell, no state.

use std::future::Future;
use system::Guest;

#[cfg(target_arch = "wasm32")]
mod wasi {
    wit_bindgen::generate!({
        path: "../system/wit",
        world: "monotonic",
        generate_all,
    });
}

#[allow(dead_code)]
mod system_capnp {
    include!(concat!(env!("OUT_DIR"), "/system_capnp.rs"));
}

#[allow(dead_code)]
mod stem_capnp {
    include!(concat!(env!("OUT_DIR"), "/stem_capnp.rs"));
}

#[allow(
    dead_code,
    clippy::extra_unused_type_parameters,
    clippy::match_single_binding
)]
mod auth_capnp {
    include!(concat!(env!("OUT_DIR"), "/auth_capnp.rs"));
}

#[allow(dead_code)]
mod routing_capnp {
    include!(concat!(env!("OUT_DIR"), "/routing_capnp.rs"));
}

#[allow(dead_code)]
mod http_capnp {
    include!(concat!(env!("OUT_DIR"), "/http_capnp.rs"));
}

type Membrane = system_capnp::membrane::Client;

const STAT_CALL_TIMEOUT_NS: u64 = 500_000_000; // 500ms

/// Best-effort logger to WASI stderr.
struct StderrLogger;

impl log::Log for StderrLogger {
    fn enabled(&self, _: &log::Metadata<'_>) -> bool {
        true
    }
    fn log(&self, record: &log::Record<'_>) {
        eprintln!("[status][{}] {}", record.level(), record.args());
    }
    fn flush(&self) {}
}

static LOGGER: StderrLogger = StderrLogger;

fn init_logging() {
    let _ = log::set_logger(&LOGGER);
    log::set_max_level(log::LevelFilter::Info);
}

/// Call `Stat.snapshot()` once so both mutable fields come from one
/// observation. Errors and timeouts degrade both fields to `null`.
async fn stat_snapshot(stat: Option<system_capnp::stat::Client>) -> Option<(Vec<String>, usize)> {
    let stat = stat?;
    timeout_future(
        async move {
            let response = stat.snapshot_request().send().promise.await.ok()?;
            let snapshot = response.get().ok()?.get_stat().ok()?;
            let addrs = snapshot.get_listen_addrs().ok()?;
            let listen_addrs = addrs
                .iter()
                .filter_map(|address| {
                    let bytes = address.ok()?;
                    let multiaddr = multiaddr::Multiaddr::try_from(bytes.to_vec()).ok()?;
                    Some(multiaddr.to_string())
                })
                .collect();
            Some((listen_addrs, snapshot.get_connected_peer_count() as usize))
        },
        STAT_CALL_TIMEOUT_NS,
    )
    .await
    .flatten()
}

#[cfg(not(target_arch = "wasm32"))]
async fn timeout_future<F>(future: F, timeout_ns: u64) -> Option<F::Output>
where
    F: Future,
{
    tokio::time::timeout(std::time::Duration::from_nanos(timeout_ns), future)
        .await
        .ok()
}

#[cfg(target_arch = "wasm32")]
async fn timeout_future<F>(future: F, timeout_ns: u64) -> Option<F::Output>
where
    F: Future,
{
    let deadline = wasi::wasi::clocks::monotonic_clock::wait_for(timeout_ns);
    match futures::future::select(Box::pin(future), Box::pin(deadline)).await {
        futures::future::Either::Left((value, _)) => Some(value),
        futures::future::Either::Right(((), _)) => None,
    }
}

async fn build_status_json(peer_id: Vec<u8>, stat: Option<system_capnp::stat::Client>) -> String {
    let peer_id = bs58::encode(peer_id).into_string();
    let snapshot = stat_snapshot(stat).await;
    let (listen_addrs, peer_count) = match snapshot {
        Some((listen_addrs, peer_count)) => (Some(listen_addrs), Some(peer_count)),
        None => (None, None),
    };

    let body = serde_json::json!({
        "status":       "ok",
        "version":      env!("CARGO_PKG_VERSION"),
        "peer_id":      peer_id,
        "listen_addrs": listen_addrs,
        "peer_count":   peer_count,
    });
    serde_json::to_string(&body).unwrap_or_else(|_| r#"{"status":"err","reason":"json"}"#.into())
}

async fn status_json_from_membrane(membrane: &Membrane) -> Result<String, capnp::Error> {
    let graft_response = membrane.graft_request().send().promise.await?;
    let graft = graft_response.get()?;
    if !graft.has_peer_id() {
        return Err(capnp::Error::failed(
            "Membrane.graft result is missing required peerId".into(),
        ));
    }
    let peer_id = graft.get_peer_id()?.to_vec();
    if peer_id.is_empty() {
        return Err(capnp::Error::failed(
            "Membrane.graft result contains an empty peerId".into(),
        ));
    }
    let stat = graft.has_stat().then(|| graft.get_stat()).transpose()?;

    Ok(build_status_json(peer_id, stat).await)
}

async fn run_http() -> Result<(), ()> {
    use wagi_guest as wagi;

    system::run(|membrane: Membrane| async move {
        let json = status_json_from_membrane(&membrane).await?;
        wagi::respond_bytes_async(
            200,
            &[("Content-Type", "application/json")],
            json.as_bytes(),
        )
        .await
        .map_err(capnp::Error::failed)?;
        Ok(())
    })
    .await
    .map_err(|error| {
        log::error!("status RPC failed: {error}");
    })
}

struct StatusGuest;

impl Guest for StatusGuest {
    async fn run() -> Result<(), ()> {
        init_logging();

        // HTTP/WAGI mode: detected by CGI env var presence.
        if std::env::var("REQUEST_METHOD").is_ok() {
            return run_http().await;
        }

        // Non-WAGI invocation: not a supported mode for status. Exit cleanly.
        log::info!("status cell invoked outside WAGI mode — exiting");
        Ok(())
    }
}

system::export!(StatusGuest);

#[cfg(test)]
mod tests {
    use super::*;
    use capnp::capability::Promise;

    const TEST_PEER_ID: &[u8] = b"status-test-peer";
    const SECRET_CONNECTED_PEER_ID: &str = "secret-connected-peer-id";
    const SECRET_CONNECTED_PEER_ADDR: &str = "/ip4/198.51.100.7/tcp/6553";

    struct SnapshotStat {
        connected_peers: Vec<(&'static str, &'static str)>,
    }

    #[allow(refining_impl_trait)]
    impl system_capnp::stat::Server for SnapshotStat {
        fn snapshot(
            self: capnp::capability::Rc<Self>,
            _params: system_capnp::stat::SnapshotParams,
            mut results: system_capnp::stat::SnapshotResults,
        ) -> Promise<(), capnp::Error> {
            let address: multiaddr::Multiaddr = "/ip4/127.0.0.1/tcp/2025"
                .parse()
                .expect("valid test multiaddr");
            let mut stat = results.get().init_stat();
            stat.reborrow()
                .init_listen_addrs(1)
                .set(0, &address.to_vec());
            stat.set_connected_peer_count(self.connected_peers.len() as u32);
            Promise::ok(())
        }
    }

    struct PendingStat;

    #[allow(refining_impl_trait)]
    impl system_capnp::stat::Server for PendingStat {
        fn snapshot(
            self: capnp::capability::Rc<Self>,
            _params: system_capnp::stat::SnapshotParams,
            _results: system_capnp::stat::SnapshotResults,
        ) -> Promise<(), capnp::Error> {
            Promise::from_future(async {
                std::future::pending::<()>().await;
                #[allow(unreachable_code)]
                Ok(())
            })
        }
    }

    struct MissingPeerIdMembrane;

    #[allow(refining_impl_trait)]
    impl system_capnp::membrane::Server for MissingPeerIdMembrane {
        fn graft(
            self: capnp::capability::Rc<Self>,
            _params: system_capnp::membrane::GraftParams,
            _results: system_capnp::membrane::GraftResults,
        ) -> Promise<(), capnp::Error> {
            Promise::ok(())
        }
    }

    struct EmptyPeerIdMembrane;

    #[allow(refining_impl_trait)]
    impl system_capnp::membrane::Server for EmptyPeerIdMembrane {
        fn graft(
            self: capnp::capability::Rc<Self>,
            _params: system_capnp::membrane::GraftParams,
            mut results: system_capnp::membrane::GraftResults,
        ) -> Promise<(), capnp::Error> {
            results.get().set_peer_id(&[]);
            Promise::ok(())
        }
    }

    struct MinimalStatusMembrane {
        stat: system_capnp::stat::Client,
    }

    #[allow(refining_impl_trait)]
    impl system_capnp::membrane::Server for MinimalStatusMembrane {
        fn graft(
            self: capnp::capability::Rc<Self>,
            _params: system_capnp::membrane::GraftParams,
            mut results: system_capnp::membrane::GraftResults,
        ) -> Promise<(), capnp::Error> {
            let mut graft = results.get();
            graft.set_peer_id(TEST_PEER_ID);
            graft.set_stat(self.stat.clone());
            Promise::ok(())
        }
    }

    #[tokio::test(flavor = "current_thread")]
    async fn production_status_decode_rejects_missing_peer_id() {
        let membrane: Membrane = capnp_rpc::new_client(MissingPeerIdMembrane);

        let error = match status_json_from_membrane(&membrane).await {
            Ok(_) => panic!("missing peerId must fail status decoding"),
            Err(error) => error,
        };

        assert!(error.to_string().contains("peerId"));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn production_status_decode_rejects_empty_peer_id() {
        let membrane: Membrane = capnp_rpc::new_client(EmptyPeerIdMembrane);

        let error = match status_json_from_membrane(&membrane).await {
            Ok(_) => panic!("empty peerId must fail status decoding"),
            Err(error) => error,
        };

        assert!(error.to_string().contains("peerId"));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn minimal_status_accepts_peer_id_and_stat_without_optional_authority() {
        let stat: system_capnp::stat::Client = capnp_rpc::new_client(SnapshotStat {
            connected_peers: vec![],
        });
        let membrane: Membrane = capnp_rpc::new_client(MinimalStatusMembrane { stat });

        let response = membrane
            .graft_request()
            .send()
            .promise
            .await
            .expect("minimal graft");
        let graft = response.get().expect("minimal graft results");
        assert!(graft.has_peer_id());
        assert!(graft.has_stat());
        assert!(!graft.has_network());
        assert!(!graft.has_routing());
        assert!(!graft.has_runtime());
        assert!(!graft.has_authority());
        assert!(!graft.has_identity());
        assert!(!graft.has_ipfs());
        assert!(!graft.has_extras());

        let json = status_json_from_membrane(&membrane)
            .await
            .expect("minimal status");
        let value: serde_json::Value =
            serde_json::from_str(&json).expect("body should parse as JSON");
        assert_eq!(value["peer_id"], bs58::encode(TEST_PEER_ID).into_string());
        assert_eq!(value["listen_addrs"][0], "/ip4/127.0.0.1/tcp/2025");
        assert_eq!(value["peer_count"], 0);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn withheld_stat_returns_null_dynamic_values() {
        let json = build_status_json(TEST_PEER_ID.to_vec(), None).await;
        let value: serde_json::Value =
            serde_json::from_str(&json).expect("body should parse as JSON");
        assert_eq!(value["status"], "ok");
        assert_eq!(value["version"], env!("CARGO_PKG_VERSION"));
        assert_eq!(value["peer_id"], bs58::encode(TEST_PEER_ID).into_string());
        assert!(value["listen_addrs"].is_null());
        assert!(value["peer_count"].is_null());
    }

    #[tokio::test(flavor = "current_thread")]
    async fn one_snapshot_populates_both_mutable_fields() {
        let stat: system_capnp::stat::Client = capnp_rpc::new_client(SnapshotStat {
            connected_peers: vec![
                (SECRET_CONNECTED_PEER_ID, SECRET_CONNECTED_PEER_ADDR),
                ("second-secret-peer-id", "/ip4/203.0.113.9/tcp/6554"),
                ("third-secret-peer-id", "/ip4/192.0.2.11/tcp/6555"),
            ],
        });
        let json = build_status_json(TEST_PEER_ID.to_vec(), Some(stat)).await;
        let value: serde_json::Value =
            serde_json::from_str(&json).expect("body should parse as JSON");

        assert_eq!(value["peer_id"], bs58::encode(TEST_PEER_ID).into_string());
        assert_eq!(value["listen_addrs"][0], "/ip4/127.0.0.1/tcp/2025");
        assert_eq!(value["peer_count"], 3);
        assert!(!json.contains(SECRET_CONNECTED_PEER_ID));
        assert!(!json.contains(SECRET_CONNECTED_PEER_ADDR));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn snapshot_timeout_degrades_both_mutable_fields() {
        let stat: system_capnp::stat::Client = capnp_rpc::new_client(PendingStat);
        let started = tokio::time::Instant::now();
        let json = build_status_json(TEST_PEER_ID.to_vec(), Some(stat)).await;

        assert!(started.elapsed() < std::time::Duration::from_secs(1));
        let value: serde_json::Value =
            serde_json::from_str(&json).expect("body should parse as JSON");
        assert_eq!(value["peer_id"], bs58::encode(TEST_PEER_ID).into_string());
        assert!(value["listen_addrs"].is_null());
        assert!(value["peer_count"].is_null());
    }
}
