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
//! `status` and `version` are always populated. `peer_id`, `listen_addrs`,
//! and `peer_count` come from the `host` capability if it's in the cell's
//! graft; if the cap is withheld they degrade to `null`.
//!
//! WAGI mode only. Runs once per HTTP request — fresh cell, no state.

use capnp::capability::FromClientHook;
use std::future::Future;
use wasip2::cli::stderr::get_stderr;
use wasip2::exports::cli::run::Guest;

#[allow(dead_code)]
mod system_capnp {
    include!(concat!(env!("OUT_DIR"), "/system_capnp.rs"));
}

#[allow(dead_code)]
mod synapse_capnp {
    include!(concat!(env!("OUT_DIR"), "/synapse_capnp.rs"));
}

#[allow(dead_code)]
mod stem_capnp {
    include!(concat!(env!("OUT_DIR"), "/stem_capnp.rs"));
}

#[allow(dead_code)]
mod auth_capnp {
    include!(concat!(env!("OUT_DIR"), "/auth_capnp.rs"));
}

#[allow(dead_code)]
mod membrane_capnp {
    include!(concat!(env!("OUT_DIR"), "/membrane_capnp.rs"));
}

#[allow(dead_code)]
mod routing_capnp {
    include!(concat!(env!("OUT_DIR"), "/routing_capnp.rs"));
}

#[allow(dead_code)]
mod http_capnp {
    include!(concat!(env!("OUT_DIR"), "/http_capnp.rs"));
}

type Membrane = membrane_capnp::membrane::Client;

const HOST_CALL_TIMEOUT_NS: u64 = 500_000_000; // 500ms

/// Look up a typed capability by name in the graft caps list.
/// Returns `None` if the cap is missing — used for graceful degradation.
fn graft_cap_opt<T: FromClientHook>(
    caps: &capnp::struct_list::Reader<'_, membrane_capnp::export::Owned>,
    name: &str,
) -> Option<T> {
    for i in 0..caps.len() {
        let entry = caps.get(i);
        let n = entry.get_name().ok()?.to_str().ok()?;
        if n == name {
            let invokable = entry.get_synapse().ok()?.get_invokable().ok()?;
            return Some(T::new(invokable.client.hook));
        }
    }
    None
}

/// Best-effort logger to WASI stderr.
struct StderrLogger;

impl log::Log for StderrLogger {
    fn enabled(&self, _: &log::Metadata<'_>) -> bool {
        true
    }
    fn log(&self, record: &log::Record<'_>) {
        let stderr = get_stderr();
        let _ = stderr.blocking_write_and_flush(
            format!("[status][{}] {}\n", record.level(), record.args()).as_bytes(),
        );
    }
    fn flush(&self) {}
}

static LOGGER: StderrLogger = StderrLogger;

fn init_logging() {
    let _ = log::set_logger(&LOGGER);
    log::set_max_level(log::LevelFilter::Info);
}

/// Best-effort host introspection. Each call swallows errors and returns
/// `None` so the JSON response can degrade per field instead of failing
/// the whole request.
async fn with_host_timeout<T>(future: impl Future<Output = Option<T>>) -> Option<T> {
    timeout_future(future, HOST_CALL_TIMEOUT_NS).await.flatten()
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
    use std::pin::Pin;
    use std::task::{Context, Poll};

    struct WasiDeadline<F> {
        future: Pin<Box<F>>,
        deadline_ns: u64,
    }

    impl<F> Future for WasiDeadline<F>
    where
        F: Future,
    {
        type Output = Option<F::Output>;

        fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
            if wasip2::clocks::monotonic_clock::now() >= self.deadline_ns {
                return Poll::Ready(None);
            }

            let this = self.as_mut().get_mut();
            match this.future.as_mut().poll(cx) {
                Poll::Ready(value) => Poll::Ready(Some(value)),
                Poll::Pending => {
                    if wasip2::clocks::monotonic_clock::now() >= this.deadline_ns {
                        Poll::Ready(None)
                    } else {
                        Poll::Pending
                    }
                }
            }
        }
    }

    let deadline_ns = wasip2::clocks::monotonic_clock::now().saturating_add(timeout_ns);
    WasiDeadline {
        future: Box::pin(future),
        deadline_ns,
    }
    .await
}

async fn host_id(host: &system_capnp::host::Client) -> Option<String> {
    with_host_timeout(async {
        let resp = host.id_request().send().promise.await.ok()?;
        let bytes = resp.get().ok()?.get_peer_id().ok()?;
        Some(bs58::encode(bytes).into_string())
    })
    .await
}

async fn host_addrs(host: &system_capnp::host::Client) -> Option<Vec<String>> {
    with_host_timeout(async {
        let resp = host.addrs_request().send().promise.await.ok()?;
        let addrs = resp.get().ok()?.get_addrs().ok()?;
        Some(
            addrs
                .iter()
                .filter_map(|a| {
                    let bytes = a.ok()?;
                    let ma = multiaddr::Multiaddr::try_from(bytes.to_vec()).ok()?;
                    Some(ma.to_string())
                })
                .collect(),
        )
    })
    .await
}

async fn host_peer_count(host: &system_capnp::host::Client) -> Option<usize> {
    with_host_timeout(async {
        let resp = host.peers_request().send().promise.await.ok()?;
        let peers = resp.get().ok()?.get_peers().ok()?;
        Some(peers.len() as usize)
    })
    .await
}

/// Build the JSON body for `/status`. `host_cap` is `None` when the
/// graft did not include it.
async fn build_status_json(host_cap: Option<system_capnp::host::Client>) -> String {
    let (peer_id, listen_addrs, peer_count) = match host_cap {
        Some(h) => (
            host_id(&h).await,
            host_addrs(&h).await,
            host_peer_count(&h).await,
        ),
        None => (None, None, None),
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

fn run_http() -> Result<(), ()> {
    use wagi_guest as wagi;

    system::run(|membrane: Membrane| async move {
        let graft_resp = membrane.graft_request().send().promise.await?;
        let caps = graft_resp.get()?.get_caps()?;
        let host_cap: Option<system_capnp::host::Client> = graft_cap_opt(&caps, "host");
        if host_cap.is_none() {
            log::info!("host cap withheld — peer_id/listen_addrs/peer_count will be null");
        }

        let json = build_status_json(host_cap).await;
        // `respond_bytes` flushes explicitly; plain `respond` uses `print!`
        // and can lose buffered bytes on cell teardown (the body sat in
        // the stdout buffer while only the headers shipped).
        wagi::respond_bytes(
            200,
            &[("Content-Type", "application/json")],
            json.as_bytes(),
        );
        Ok(())
    });

    Ok(())
}

struct StatusGuest;

impl Guest for StatusGuest {
    fn run() -> Result<(), ()> {
        init_logging();

        // HTTP/WAGI mode: detected by CGI env var presence.
        if std::env::var("REQUEST_METHOD").is_ok() {
            return run_http();
        }

        // Non-WAGI invocation: not a supported mode for status. Exit cleanly.
        log::info!("status cell invoked outside WAGI mode — exiting");
        Ok(())
    }
}

wasip2::cli::command::export!(StatusGuest);

#[cfg(test)]
mod tests {
    use super::*;
    use capnp::capability::Promise;

    const TEST_PEER_ID: &[u8] = b"status-test-peer";

    struct SlowPeersHost;

    #[allow(refining_impl_trait)]
    impl system_capnp::host::Server for SlowPeersHost {
        fn id(
            self: capnp::capability::Rc<Self>,
            _params: system_capnp::host::IdParams,
            mut results: system_capnp::host::IdResults,
        ) -> Promise<(), capnp::Error> {
            results.get().set_peer_id(TEST_PEER_ID);
            Promise::ok(())
        }

        fn addrs(
            self: capnp::capability::Rc<Self>,
            _params: system_capnp::host::AddrsParams,
            mut results: system_capnp::host::AddrsResults,
        ) -> Promise<(), capnp::Error> {
            let addr: multiaddr::Multiaddr = "/ip4/127.0.0.1/tcp/2025"
                .parse()
                .expect("valid test multiaddr");
            let mut addrs = results.get().init_addrs(1);
            addrs.set(0, &addr.to_vec());
            Promise::ok(())
        }

        fn peers(
            self: capnp::capability::Rc<Self>,
            _params: system_capnp::host::PeersParams,
            _results: system_capnp::host::PeersResults,
        ) -> Promise<(), capnp::Error> {
            Promise::from_future(async {
                std::future::pending::<()>().await;
                #[allow(unreachable_code)]
                Ok(())
            })
        }

        fn network(
            self: capnp::capability::Rc<Self>,
            _params: system_capnp::host::NetworkParams,
            _results: system_capnp::host::NetworkResults,
        ) -> Promise<(), capnp::Error> {
            Promise::err(capnp::Error::unimplemented("test host network".into()))
        }
    }

    /// `build_status_json` with `None` host cap must return null for
    /// host-derived fields and populate `status` + `version`. This is
    /// the graceful-degradation contract the engagement starter kit
    /// pitch depends on.
    #[tokio::test(flavor = "current_thread")]
    async fn build_status_json_null_host_returns_null_fields_and_populates_static() {
        let json = build_status_json(None).await;
        let v: serde_json::Value = serde_json::from_str(&json).expect("body should parse as JSON");
        assert_eq!(v["status"], "ok");
        assert_eq!(v["version"], env!("CARGO_PKG_VERSION"));
        assert!(
            v["peer_id"].is_null(),
            "peer_id should be null when host cap is absent"
        );
        assert!(
            v["listen_addrs"].is_null(),
            "listen_addrs should be null when host cap is absent"
        );
        assert!(
            v["peer_count"].is_null(),
            "peer_count should be null when host cap is absent"
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn build_status_json_times_out_slow_peer_count_only() {
        let host: system_capnp::host::Client = capnp_rpc::new_client(SlowPeersHost);
        let started = tokio::time::Instant::now();

        let json = build_status_json(Some(host)).await;
        let elapsed = started.elapsed();
        assert!(
            elapsed < std::time::Duration::from_secs(1),
            "status host timeout should return promptly, took {elapsed:?}"
        );

        let v: serde_json::Value = serde_json::from_str(&json).expect("body should parse as JSON");
        assert_eq!(v["status"], "ok");
        assert_eq!(
            v["peer_id"],
            bs58::encode(TEST_PEER_ID).into_string(),
            "fast host.id should still populate"
        );
        assert_eq!(
            v["listen_addrs"][0], "/ip4/127.0.0.1/tcp/2025",
            "fast host.addrs should still populate"
        );
        assert!(
            v["peer_count"].is_null(),
            "slow host.peers should degrade to null"
        );
    }
}
