//! Discovery guest: two-agent Greeter demo via schema-keyed RPC.
//!
//! Demonstrates the full Wetware discovery flow:
//!   build → schema-inject → IPFS push → provide on DHT →
//!   findProviders → dial via VatClient → typed RPC call
//!
//! Two modes, selected by subcommand:
//!
//! **Cell mode** (no args, default): spawned by VatListener per RPC
//! connection. Creates a Greeter and exports it via `system::serve()`.
//!
//! **`serve`**: provides the schema CID on the DHT, discovers peers via
//! `routing.find_providers()`, dials them via `VatClient`, calls `greet()`.

use std::cell::RefCell;
use std::collections::HashSet;
use std::rc::Rc;

use capnp::capability::Promise;
use capnp_rpc::pry;
use wasip2::cli::stderr::get_stderr;
use wasip2::exports::cli::run::Guest;

// ---------------------------------------------------------------------------
// Cap'n Proto generated modules
// ---------------------------------------------------------------------------

#[allow(dead_code)]
mod system_capnp {
    include!(concat!(env!("OUT_DIR"), "/system_capnp.rs"));
}

#[allow(dead_code, clippy::extra_unused_type_parameters)]
mod stem_capnp {
    include!(concat!(env!("OUT_DIR"), "/stem_capnp.rs"));
}

#[allow(dead_code, clippy::extra_unused_type_parameters)]
mod auth_capnp {
    include!(concat!(env!("OUT_DIR"), "/auth_capnp.rs"));
}

#[allow(dead_code, clippy::extra_unused_type_parameters)]
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

#[allow(dead_code)]
mod greeter_capnp {
    include!(concat!(env!("OUT_DIR"), "/greeter_capnp.rs"));
}

// Build-time schema constants: GREETER_SCHEMA (&[u8]) and GREETER_CID (&str).
include!(concat!(env!("OUT_DIR"), "/schema_ids.rs"));

/// Bootstrap capability: the concrete Membrane defined in membrane.capnp.
type Membrane = membrane_capnp::membrane::Client;

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

/// Short peer ID for human-readable logs (last 4 bytes = 8 hex chars).
fn short_id(peer_id: &[u8]) -> String {
    let h = hex::encode(peer_id);
    if h.len() > 8 {
        format!("..{}", &h[h.len() - 8..])
    } else {
        h
    }
}

// ---------------------------------------------------------------------------
// Logging (WASI stderr)
// ---------------------------------------------------------------------------

struct StderrLogger;

impl log::Log for StderrLogger {
    fn enabled(&self, metadata: &log::Metadata<'_>) -> bool {
        metadata.level() <= log::Level::Trace
    }

    fn log(&self, record: &log::Record<'_>) {
        if !self.enabled(record.metadata()) {
            return;
        }
        let stderr = get_stderr();
        let _ = stderr.blocking_write_and_flush(
            format!("[{}] {}\n", record.level(), record.args()).as_bytes(),
        );
    }

    fn flush(&self) {}
}

static LOGGER: StderrLogger = StderrLogger;

fn init_logging() {
    if log::set_logger(&LOGGER).is_ok() {
        log::set_max_level(log::LevelFilter::Trace);
    }
}

// ---------------------------------------------------------------------------
// GreeterImpl — Cap'n Proto server
// ---------------------------------------------------------------------------

struct GreeterImpl {
    /// Peer ID of the host node, included in greetings.
    peer_id: Vec<u8>,
}

#[allow(refining_impl_trait)]
impl greeter_capnp::greeter::Server for GreeterImpl {
    fn greet(
        self: Rc<Self>,
        params: greeter_capnp::greeter::GreetParams,
        mut results: greeter_capnp::greeter::GreetResults,
    ) -> Promise<(), capnp::Error> {
        let name = pry!(pry!(params.get()).get_name()).to_str().unwrap_or("?");
        let greeting = format!("Hello, {}! I'm {}", name, short_id(&self.peer_id));
        results.get().set_greeting(&greeting);
        Promise::ok(())
    }
}

// ---------------------------------------------------------------------------
// Cell mode — RPC capability export via system::serve()
// ---------------------------------------------------------------------------

fn run_cell() {
    // In cell mode we need the host's peer ID for the greeting.
    // The peer ID is passed via environment variable by the host.
    let peer_id = std::env::var("WW_PEER_ID")
        .ok()
        .and_then(|s| hex::decode(s).ok())
        .unwrap_or_default();

    let greeter = GreeterImpl { peer_id };
    let client: greeter_capnp::greeter::Client = capnp_rpc::new_client(greeter);
    log::info!("cell: exporting Greeter via RPC");
    system::serve(client.client, |_membrane: Membrane| async move {
        std::future::pending().await
    });
}

// ---------------------------------------------------------------------------
// GreetingSink — discovers peers and dials them via VatClient
// ---------------------------------------------------------------------------

struct GreetingSink {
    vat_client: system_capnp::vat_client::Client,
    self_id: Vec<u8>,
    seen: Rc<RefCell<HashSet<Vec<u8>>>>,
}

#[allow(refining_impl_trait)]
impl routing_capnp::provider_sink::Server for GreetingSink {
    fn provider(
        self: Rc<Self>,
        params: routing_capnp::provider_sink::ProviderParams,
    ) -> Promise<(), capnp::Error> {
        let peer_id = pry!(pry!(pry!(params.get()).get_info()).get_peer_id()).to_vec();

        if peer_id == self.self_id || !self.seen.borrow_mut().insert(peer_id.clone()) {
            return Promise::ok(());
        }

        let vat_client = self.vat_client.clone();
        let self_id = self.self_id.clone();
        let peer = peer_id.clone();

        Promise::from_future(async move {
            if let Err(e) = greet_peer(&vat_client, &self_id, &peer).await {
                log::error!("greet {} failed: {e}", short_id(&peer));
            }
            Ok(())
        })
    }

    fn done(
        self: Rc<Self>,
        _params: routing_capnp::provider_sink::DoneParams,
        _results: routing_capnp::provider_sink::DoneResults,
    ) -> Promise<(), capnp::Error> {
        Promise::ok(())
    }
}

// ---------------------------------------------------------------------------
// greet_peer — dial via VatClient and call Greeter.greet()
// ---------------------------------------------------------------------------

async fn greet_peer(
    vat_client: &system_capnp::vat_client::Client,
    self_id: &[u8],
    peer_id: &[u8],
) -> Result<(), capnp::Error> {
    let us = short_id(self_id);
    let them = short_id(peer_id);

    let mut req = vat_client.dial_request();
    req.get().set_peer(peer_id);
    req.get().set_schema(GREETER_SCHEMA);
    let resp = req.send().promise.await?;
    let greeter: greeter_capnp::greeter::Client = resp.get()?.get_cap().get_as_capability()?;

    let mut greet_req = greeter.greet_request();
    greet_req.get().set_name(format!("peer {us}"));
    let greet_resp = greet_req.send().promise.await?;
    let greeting = greet_resp
        .get()?
        .get_greeting()?
        .to_str()
        .unwrap_or("(invalid UTF-8)");

    log::info!("{us} -> {them}: {greeting}");
    Ok(())
}

// ---------------------------------------------------------------------------
// Service mode — discovery loop with VatClient
// ---------------------------------------------------------------------------

async fn run_service(membrane: Membrane) -> Result<(), capnp::Error> {
    let graft_resp = membrane.graft_request().send().promise.await?;
    let results = graft_resp.get()?;
    let caps = results.get_caps()?;
    let host: system_capnp::host::Client = get_graft_cap(&caps, "host")?;
    let routing: routing_capnp::routing::Client = get_graft_cap(&caps, "routing")?;

    let network_resp = host.network_request().send().promise.await?;
    let network = network_resp.get()?;
    let vat_client = network.get_vat_client()?;

    let id_resp = host.id_request().send().promise.await?;
    let self_id = id_resp.get()?.get_peer_id()?.to_vec();
    log::info!("service: peer {}", short_id(&self_id));
    log::info!("service: schema CID {GREETER_CID}");
    log::info!("service: looking for peers...");

    let seen = Rc::new(RefCell::new(HashSet::<Vec<u8>>::new()));
    let mut cooldown_ms: u64 = 2_000;
    const BASE_MS: u64 = 2_000;
    const MAX_MS: u64 = 900_000;

    loop {
        let prev_seen = seen.borrow().len();

        // Re-provide (DHT records expire).
        let mut provide_req = routing.provide_request();
        provide_req.get().set_key(GREETER_CID);
        provide_req.send().promise.await?;

        // Search for peers; GreetingSink dials new ones via RPC.
        let sink: routing_capnp::provider_sink::Client = capnp_rpc::new_client(GreetingSink {
            vat_client: vat_client.clone(),
            self_id: self_id.clone(),
            seen: seen.clone(),
        });
        let mut fp_req = routing.find_providers_request();
        {
            let mut b = fp_req.get();
            b.set_key(GREETER_CID);
            b.set_count(5);
            b.set_sink(sink);
        }
        fp_req.send().promise.await?;

        let now_seen = seen.borrow().len();
        if now_seen > prev_seen {
            log::info!("service: found {} peer(s)", now_seen);
            cooldown_ms = BASE_MS;
        } else {
            cooldown_ms = (cooldown_ms * 2).min(MAX_MS);
        }

        let delay_ms = cooldown_ms / 2 + rand::random_range(0..=cooldown_ms / 2);
        let pause = wasip2::clocks::monotonic_clock::subscribe_duration(delay_ms * 1_000_000);
        pause.block();
    }
}

// ---------------------------------------------------------------------------
// Entry point
// ---------------------------------------------------------------------------

struct DiscoveryGuest;

impl Guest for DiscoveryGuest {
    fn run() -> Result<(), ()> {
        init_logging();
        match std::env::args().nth(1).as_deref() {
            Some("serve") => {
                log::info!("discovery: serve — DHT provide + peer discovery");
                system::run(|membrane: Membrane| async move { run_service(membrane).await });
            }
            _ => {
                // Default (no args): cell mode — spawned by VatListener.
                run_cell();
            }
        }
        Ok(())
    }
}

wasip2::cli::command::export!(DiscoveryGuest);

// ---------------------------------------------------------------------------
// Unit tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_short_id_truncates() {
        let id = hex::decode("0123456789abcdef0123456789abcdef").unwrap();
        let s = short_id(&id);
        assert_eq!(s, "..89abcdef");
    }

    #[test]
    fn test_short_id_short_input() {
        let id = hex::decode("abcd").unwrap();
        let s = short_id(&id);
        assert_eq!(s, "abcd");
    }

    // -----------------------------------------------------------------------
    // RPC round-trip test (Cap'n Proto over in-memory duplex)
    // -----------------------------------------------------------------------

    use capnp_rpc::rpc_twoparty_capnp::Side;
    use capnp_rpc::twoparty::VatNetwork;
    use capnp_rpc::RpcSystem;
    use tokio::io;
    use tokio_util::compat::{TokioAsyncReadCompatExt, TokioAsyncWriteCompatExt};

    fn setup_greeter() -> greeter_capnp::greeter::Client {
        let (client_stream, server_stream) = io::duplex(8 * 1024);
        let (client_read, client_write) = io::split(client_stream);
        let (server_read, server_write) = io::split(server_stream);

        let greeter = GreeterImpl {
            peer_id: b"test-peer-id".to_vec(),
        };
        let server: greeter_capnp::greeter::Client = capnp_rpc::new_client(greeter);

        let server_network = VatNetwork::new(
            server_read.compat(),
            server_write.compat_write(),
            Side::Server,
            Default::default(),
        );
        let server_rpc = RpcSystem::new(Box::new(server_network), Some(server.client));
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
        let client: greeter_capnp::greeter::Client = client_rpc.bootstrap(Side::Server);
        tokio::task::spawn_local(async move {
            let _ = client_rpc.await;
        });

        client
    }

    #[tokio::test]
    async fn test_rpc_greet() {
        let local = tokio::task::LocalSet::new();
        local
            .run_until(async {
                let client = setup_greeter();
                let mut req = client.greet_request();
                req.get().set_name("world");
                let resp = req.send().promise.await.unwrap();
                let greeting = resp
                    .get()
                    .unwrap()
                    .get_greeting()
                    .unwrap()
                    .to_str()
                    .unwrap();
                assert!(
                    greeting.contains("Hello, world!"),
                    "unexpected greeting: {greeting}"
                );
                assert!(
                    greeting.contains("I'm"),
                    "should include peer identity: {greeting}"
                );
            })
            .await;
    }

    #[tokio::test]
    async fn test_rpc_greet_empty_name() {
        let local = tokio::task::LocalSet::new();
        local
            .run_until(async {
                let client = setup_greeter();
                let mut req = client.greet_request();
                req.get().set_name("");
                let resp = req.send().promise.await.unwrap();
                let greeting = resp
                    .get()
                    .unwrap()
                    .get_greeting()
                    .unwrap()
                    .to_str()
                    .unwrap();
                assert!(greeting.contains("Hello, !"), "unexpected: {greeting}");
            })
            .await;
    }

    // -----------------------------------------------------------------------
    // Discovery backoff & jitter (same as chess, validates constants)
    // -----------------------------------------------------------------------

    const BASE_MS: u64 = 2_000;
    const MAX_MS: u64 = 900_000;

    #[test]
    fn test_backoff_doubles_to_max() {
        let mut cooldown = BASE_MS;
        for _ in 0..30 {
            cooldown = (cooldown * 2).min(MAX_MS);
        }
        assert_eq!(cooldown, MAX_MS);
    }

    #[test]
    fn test_jitter_within_bounds() {
        for cooldown in [BASE_MS, 4_000, 64_000, MAX_MS] {
            for _ in 0..500 {
                let delay = cooldown / 2 + rand::random_range(0..=cooldown / 2);
                assert!(delay >= cooldown / 2);
                assert!(delay <= cooldown);
            }
        }
    }
}
