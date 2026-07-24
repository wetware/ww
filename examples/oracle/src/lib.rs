//! Price oracle guest: gas price feed via named vat RPC.
//!
//! Demonstrates:
//!   - HttpClient capability for outbound HTTP (domain-scoped)
//!   - Subcommand dispatch (cell / serve / consume)
//!   - DHT discovery via routing.provide()/findProviders()
//!
//! Three modes, selected by subcommand:
//!
//! **Cell mode** (no args, default): creates a PriceOracle, fetches prices via
//! HttpClient, and exports it via `system::serve()`.
//!
//! **`serve`**: provides on the DHT, re-provides periodically.
//!
//! **`consume`**: discovers oracle providers via DHT, dials via VatClient,
//! queries prices.

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

use capnp::capability::Promise;
use capnp_rpc::pry;
use wasip2::cli::stderr::get_stderr;
use wasip2::exports::cli::run::Guest;

// Cap'n Proto generated modules
#[allow(dead_code)]
mod system_capnp {
    include!(concat!(env!("OUT_DIR"), "/system_capnp.rs"));
}

#[allow(dead_code)]
mod stem_capnp {
    include!(concat!(env!("OUT_DIR"), "/stem_capnp.rs"));
}

#[allow(dead_code, clippy::match_single_binding)]
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

#[allow(dead_code)]
mod oracle_capnp {
    include!(concat!(env!("OUT_DIR"), "/oracle_capnp.rs"));
}

// Build-time schema constants: PRICE_ORACLE_SCHEMA (&[u8]) and PRICE_ORACLE_CID (&str).
// Vat publication uses the service name below; the schema CID is metadata.
include!(concat!(env!("OUT_DIR"), "/schema_ids.rs"));

const ORACLE_SERVICE: &str = "oracle";

type Membrane = membrane_capnp::membrane::Client;

/// Look up a typed capability by name from the graft caps list.
fn get_graft_cap<T: capnp::capability::FromClientHook>(
    caps: &capnp::struct_list::Reader<'_, membrane_capnp::export::Owned>,
    name: &str,
) -> Result<T, capnp::Error> {
    for i in 0..caps.len() {
        let entry = caps.get(i);
        let n = entry.get_name()?.to_str().map_err(|e| capnp::Error::failed(e.to_string()))?;
        if n == name {
            return entry.get_cap().get_as_capability::<T>();
        }
    }
    Err(capnp::Error::failed(format!(
        "capability '{name}' not found in graft response"
    )))
}

fn short_id(peer_id: &[u8]) -> String {
    let h = hex::encode(peer_id);
    if h.len() > 8 {
        format!("..{}", &h[h.len() - 8..])
    } else {
        h
    }
}

async fn routing_key(
    routing: &routing_capnp::routing::Client,
    service: &str,
) -> Result<String, capnp::Error> {
    let mut req = routing.hash_request();
    req.get().set_data(service.as_bytes());
    let resp = req.send().promise.await?;
    let key = resp
        .get()?
        .get_key()?
        .to_str()
        .map_err(|e| capnp::Error::failed(e.to_string()))?
        .to_string();
    Ok(key)
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
// Cached price data
// ---------------------------------------------------------------------------

#[derive(Clone, Debug)]
struct PriceEntry {
    price: i64,
    decimals: u8,
    timestamp: i64,
    confidence: f64,
}

type PriceCache = Rc<RefCell<HashMap<String, PriceEntry>>>;

/// Supported trading pairs.
const PAIRS: &[&str] = &["ETH/gas", "POLYGON/gas", "BASE/gas"];

/// Initialize cache with zero-confidence entries.
fn init_cache() -> PriceCache {
    let mut map = HashMap::new();
    for pair in PAIRS {
        map.insert(
            pair.to_string(),
            PriceEntry {
                price: 0,
                decimals: 9,
                timestamp: 0,
                confidence: 0.0,
            },
        );
    }
    Rc::new(RefCell::new(map))
}

// ---------------------------------------------------------------------------
// PriceOracleImpl — Cap'n Proto server
// ---------------------------------------------------------------------------

struct PriceOracleImpl {
    cache: PriceCache,
}

#[allow(refining_impl_trait)]
impl oracle_capnp::price_oracle::Server for PriceOracleImpl {
    fn get_price(
        self: Rc<Self>,
        params: oracle_capnp::price_oracle::GetPriceParams,
        mut results: oracle_capnp::price_oracle::GetPriceResults,
    ) -> Promise<(), capnp::Error> {
        let pair = pry!(pry!(params.get()).get_pair()).to_str().unwrap_or("?");
        let cache = self.cache.borrow();
        match cache.get(pair) {
            Some(entry) => {
                let mut r = results.get();
                r.set_price(entry.price);
                r.set_decimals(entry.decimals);
                r.set_timestamp(entry.timestamp);
                r.set_confidence(entry.confidence);
                Promise::ok(())
            }
            None => Promise::err(capnp::Error::failed(format!("unknown pair: {pair}"))),
        }
    }

    fn get_pairs(
        self: Rc<Self>,
        _params: oracle_capnp::price_oracle::GetPairsParams,
        mut results: oracle_capnp::price_oracle::GetPairsResults,
    ) -> Promise<(), capnp::Error> {
        let mut list = results.get().init_pairs(PAIRS.len() as u32);
        for (i, pair) in PAIRS.iter().enumerate() {
            list.set(i as u32, pair);
        }
        Promise::ok(())
    }
}

// ---------------------------------------------------------------------------
// Price fetcher (uses HttpClient capability)
// ---------------------------------------------------------------------------

/// Blocknative gas price API response (simplified).
#[derive(serde::Deserialize, Debug)]
struct BlocknativeResponse {
    #[serde(rename = "blockPrices")]
    block_prices: Vec<BlockPrice>,
}

#[derive(serde::Deserialize, Debug)]
struct BlockPrice {
    #[serde(rename = "estimatedPrices")]
    estimated_prices: Vec<EstimatedPrice>,
}

#[derive(serde::Deserialize, Debug)]
struct EstimatedPrice {
    confidence: f64,
    #[serde(rename = "maxFeePerGas")]
    max_fee_per_gas: f64,
}

/// Fetch gas prices from Blocknative and update the cache.
async fn fetch_prices(
    http: &http_capnp::http_client::Client,
    cache: &PriceCache,
) -> Result<(), capnp::Error> {
    let mut req = http.get_request();
    req.get()
        .set_url("https://api.blocknative.com/gasprices/blockprices");
    req.get().init_headers(0);
    let resp = req.send().promise.await?;
    let reader = resp.get()?;
    let status = reader.get_status();
    let body = reader.get_body()?;

    if status != 200 {
        return Err(capnp::Error::failed(format!(
            "Blocknative API returned status {status}"
        )));
    }

    let parsed: BlocknativeResponse =
        serde_json::from_slice(body).map_err(|e| capnp::Error::failed(format!("JSON: {e}")))?;

    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64;

    if let Some(block) = parsed.block_prices.first() {
        if let Some(est) = block.estimated_prices.first() {
            let price_gwei = est.max_fee_per_gas;
            let price_wei = (price_gwei * 1_000_000_000.0) as i64;
            let mut cache = cache.borrow_mut();
            // ETH/gas is the primary pair from Blocknative.
            cache.insert(
                "ETH/gas".to_string(),
                PriceEntry {
                    price: price_wei,
                    decimals: 9,
                    timestamp: now,
                    confidence: est.confidence / 100.0,
                },
            );
            log::info!(
                "ETH/gas: {:.2} gwei (confidence {:.0}%)",
                price_gwei,
                est.confidence
            );
        }
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// Cell mode — vat capability export via system::serve()
// ---------------------------------------------------------------------------

/// Vat cell for VatListener-spawned processes.
///
/// Creates a PriceOracle and exports it as the bootstrap capability.
/// Fetches prices via HttpClient in the background. The process stays
/// alive until the host drops the connection.
fn run_cell() {
    let cache = init_cache();
    let oracle = PriceOracleImpl {
        cache: cache.clone(),
    };
    let client: oracle_capnp::price_oracle::Client = capnp_rpc::new_client(oracle);
    log::info!("cell: exporting PriceOracle via RPC");
    system::serve(client.client, |membrane: Membrane| async move {
        // Fetch prices using HttpClient from the membrane.
        let graft_resp = membrane.graft_request().send().promise.await?;
        let graft = graft_resp.get()?;
        let caps = graft.get_caps()?;
        let http: http_capnp::http_client::Client = get_graft_cap(&caps, "http-client")?;

        if let Err(e) = fetch_prices(&http, &cache).await {
            log::warn!("cell: initial price fetch failed: {e}");
        }

        // Keep fetching prices while the RPC connection is alive.
        let mut cooldown_ms: u64 = 30_000;
        loop {
            let pause =
                wasip2::clocks::monotonic_clock::subscribe_duration(cooldown_ms * 1_000_000);
            pause.block();

            if let Err(e) = fetch_prices(&http, &cache).await {
                log::warn!("cell: price refresh failed: {e}");
            }
            cooldown_ms = cooldown_ms.min(60_000);
        }
    });
}

// ---------------------------------------------------------------------------
// Service mode — DHT registration and discovery
// ---------------------------------------------------------------------------

async fn run_service(membrane: Membrane) -> Result<(), capnp::Error> {
    let graft_resp = membrane.graft_request().send().promise.await?;
    let results = graft_resp.get()?;
    let caps = results.get_caps()?;
    let host: system_capnp::host::Client = get_graft_cap(&caps, "host")?;
    let routing: routing_capnp::routing::Client = get_graft_cap(&caps, "routing")?;

    let id_resp = host.id_request().send().promise.await?;
    let self_id = id_resp.get()?.get_peer_id()?.to_vec();
    log::info!("oracle: peer {}", short_id(&self_id));
    log::info!("oracle: service name {ORACLE_SERVICE}");
    let service_key = routing_key(&routing, ORACLE_SERVICE).await?;
    log::info!("oracle: routing key {service_key}");

    // Provide service-name routing key on DHT for discovery.
    let mut provide_req = routing.provide_request();
    provide_req.get().set_key(&service_key);
    provide_req.send().promise.await?;
    log::info!("oracle: provided on DHT");

    // Keep running: re-provide on DHT (records expire).
    let mut cooldown_ms: u64 = 30_000;
    loop {
        let mut provide_req = routing.provide_request();
        provide_req.get().set_key(&service_key);
        let _ = provide_req.send().promise.await;

        let pause = wasip2::clocks::monotonic_clock::subscribe_duration(cooldown_ms * 1_000_000);
        pause.block();
        cooldown_ms = cooldown_ms.min(60_000);
    }
}

// ---------------------------------------------------------------------------
// Consumer mode — discover and query oracle
// ---------------------------------------------------------------------------

struct OracleSink {
    vat_client: system_capnp::vat_client::Client,
    self_id: Vec<u8>,
    seen: Rc<RefCell<std::collections::HashSet<Vec<u8>>>>,
}

#[allow(refining_impl_trait)]
impl routing_capnp::provider_sink::Server for OracleSink {
    fn provider(
        self: Rc<Self>,
        params: routing_capnp::provider_sink::ProviderParams,
    ) -> Promise<(), capnp::Error> {
        let peer_id = pry!(pry!(pry!(params.get()).get_info()).get_peer_id()).to_vec();

        if peer_id == self.self_id || !self.seen.borrow_mut().insert(peer_id.clone()) {
            return Promise::ok(());
        }

        let vat_client = self.vat_client.clone();
        let peer = peer_id.clone();

        Promise::from_future(async move {
            if let Err(e) = query_oracle(&vat_client, &peer).await {
                log::error!("query {} failed: {e}", short_id(&peer));
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

async fn query_oracle(
    vat_client: &system_capnp::vat_client::Client,
    peer_id: &[u8],
) -> Result<(), capnp::Error> {
    let them = short_id(peer_id);

    // Dial the oracle peer.
    let mut req = vat_client.dial_request();
    req.get().set_peer(peer_id);
    req.get().set_protocol(ORACLE_SERVICE);
    let resp = req.send().promise.await?;
    let dialed = resp.get()?.get_cap();
    let oracle: oracle_capnp::price_oracle::Client =
        dialed.get_as_capability()?;

    // Query available pairs.
    let pairs_resp = oracle.get_pairs_request().send().promise.await?;
    let pairs = pairs_resp.get()?.get_pairs()?;

    for i in 0..pairs.len() {
        let pair = pairs.get(i)?.to_str().unwrap_or("?");

        let mut price_req = oracle.get_price_request();
        price_req.get().set_pair(pair);
        let price_resp = price_req.send().promise.await?;
        let r = price_resp.get()?;

        let price = r.get_price();
        let decimals = r.get_decimals();
        let confidence = r.get_confidence();
        let divisor = 10_f64.powi(decimals as i32);
        let display_price = price as f64 / divisor;

        log::info!(
            "{them}: {pair} = {display_price:.2} gwei (confidence {:.0}%)",
            confidence * 100.0
        );
    }

    Ok(())
}

async fn run_consumer(membrane: Membrane) -> Result<(), capnp::Error> {
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
    log::info!("consumer: peer {}", short_id(&self_id));
    log::info!("consumer: looking for oracle providers...");
    let service_key = routing_key(&routing, ORACLE_SERVICE).await?;
    log::info!("consumer: routing key {service_key}");

    let seen = Rc::new(RefCell::new(std::collections::HashSet::<Vec<u8>>::new()));
    let mut cooldown_ms: u64 = 2_000;
    const BASE_MS: u64 = 2_000;
    const MAX_MS: u64 = 60_000;

    loop {
        let prev_seen = seen.borrow().len();

        let sink: routing_capnp::provider_sink::Client = capnp_rpc::new_client(OracleSink {
            vat_client: vat_client.clone(),
            self_id: self_id.clone(),
            seen: seen.clone(),
        });
        let mut fp_req = routing.find_providers_request();
        {
            let mut b = fp_req.get();
            b.set_key(&service_key);
            b.set_count(5);
            b.set_sink(sink);
        }
        fp_req.send().promise.await?;

        let now_seen = seen.borrow().len();
        if now_seen > prev_seen {
            log::info!("consumer: found {} oracle provider(s)", now_seen);
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
// HTTP/WAGI mode — stateless per-request handler
// ---------------------------------------------------------------------------

/// WAGI cell handler: graft membrane, fetch prices, respond with JSON.
///
/// stdin/stdout carry CGI (body in, response out). The capnp membrane runs
/// over the `wetware:streams` side-channel — no conflict.
fn run_http() -> Result<(), ()> {
    use wagi_guest as wagi;

    system::run(|membrane: Membrane| async move {
        let graft_resp = membrane.graft_request().send().promise.await?;
        let graft = graft_resp.get()?;
        let graft_caps = graft.get_caps()?;
        let http: http_capnp::http_client::Client =
            get_graft_cap(&graft_caps, "http-client")?;

        let cache = init_cache();
        if let Err(e) = fetch_prices(&http, &cache).await {
            log::warn!("http: price fetch failed: {e}");
            wagi::respond(
                502,
                &[("Content-Type", "text/plain")],
                &format!("price fetch failed: {e}"),
            );
            return Ok(());
        }

        let query = wagi::query();
        let json = build_json_response(&cache, &query);
        wagi::respond(200, &[("Content-Type", "application/json")], &json);
        Ok(())
    });

    Ok(())
}

/// Build a JSON response from the price cache.
///
/// If `query` contains `pair=X`, return only that pair.
/// Otherwise return all pairs.
fn build_json_response(cache: &PriceCache, query: &str) -> String {
    let map = cache.borrow();

    // Parse ?pair=ETH/gas from query string.
    let filter_pair = query
        .split('&')
        .find_map(|kv| kv.strip_prefix("pair="))
        .map(|s| s.replace("%2F", "/").replace("%2f", "/"));

    let mut pairs = serde_json::Map::new();
    for (name, entry) in map.iter() {
        if let Some(ref filter) = filter_pair {
            if name != filter {
                continue;
            }
        }
        let divisor = 10_f64.powi(entry.decimals as i32);
        let display_price = entry.price as f64 / divisor;
        let mut obj = serde_json::Map::new();
        obj.insert("price".into(), serde_json::Value::from(display_price));
        obj.insert("unit".into(), serde_json::Value::from("gwei"));
        obj.insert(
            "confidence".into(),
            serde_json::Value::from(entry.confidence),
        );
        obj.insert(
            "timestamp".into(),
            serde_json::Value::from(entry.timestamp),
        );
        pairs.insert(name.clone(), serde_json::Value::Object(obj));
    }

    let root = serde_json::json!({ "pairs": pairs });
    serde_json::to_string_pretty(&root).unwrap_or_else(|_| r#"{"error":"json serialization"}"#.into())
}

// ---------------------------------------------------------------------------
// Entry point
// ---------------------------------------------------------------------------

struct OracleGuest;

impl Guest for OracleGuest {
    fn run() -> Result<(), ()> {
        init_logging();

        // HTTP/WAGI mode: detected by CGI env var presence.
        // HttpListener injects REQUEST_METHOD; vat publication does not.
        if std::env::var("REQUEST_METHOD").is_ok() {
            return run_http();
        }

        match std::env::args().nth(1).as_deref() {
            Some("serve") => {
                log::info!("oracle: serve — DHT provide loop");
                system::run(|membrane: Membrane| async move { run_service(membrane).await });
            }
            Some("consume") => {
                log::info!("oracle: consume — discover + query prices");
                system::run(|membrane: Membrane| async move { run_consumer(membrane).await });
            }
            _ => {
                // Default (no args): cell mode — export the PriceOracle capability.
                run_cell();
            }
        }
        Ok(())
    }
}

wasip2::cli::command::export!(OracleGuest);

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
    fn test_init_cache_has_all_pairs() {
        let cache = init_cache();
        let map = cache.borrow();
        for pair in PAIRS {
            assert!(map.contains_key(*pair), "missing pair: {pair}");
        }
    }

    #[test]
    fn test_cache_initial_confidence_is_zero() {
        let cache = init_cache();
        let map = cache.borrow();
        for entry in map.values() {
            assert_eq!(entry.confidence, 0.0);
        }
    }

    #[test]
    fn test_blocknative_json_parsing() {
        let json = r#"{
            "blockPrices": [{
                "estimatedPrices": [{
                    "confidence": 99,
                    "price": 25.5,
                    "maxFeePerGas": 30.123456789
                }]
            }]
        }"#;
        let parsed: BlocknativeResponse = serde_json::from_str(json).unwrap();
        let est = &parsed.block_prices[0].estimated_prices[0];
        assert_eq!(est.confidence, 99.0);
        assert!((est.max_fee_per_gas - 30.123456789).abs() < 0.0001);
    }

    // RPC round-trip test
    use capnp_rpc::rpc_twoparty_capnp::Side;
    use capnp_rpc::twoparty::VatNetwork;
    use capnp_rpc::RpcSystem;
    use tokio::io;
    use tokio_util::compat::{TokioAsyncReadCompatExt, TokioAsyncWriteCompatExt};

    fn setup_oracle() -> oracle_capnp::price_oracle::Client {
        let (client_stream, server_stream) = io::duplex(8 * 1024);
        let (client_read, client_write) = io::split(client_stream);
        let (server_read, server_write) = io::split(server_stream);

        let cache = init_cache();
        // Seed with a test price.
        cache.borrow_mut().insert(
            "ETH/gas".to_string(),
            PriceEntry {
                price: 30_000_000_000,
                decimals: 9,
                timestamp: 1700000000,
                confidence: 0.99,
            },
        );

        let oracle = PriceOracleImpl { cache };
        let server: oracle_capnp::price_oracle::Client = capnp_rpc::new_client(oracle);

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
        let client: oracle_capnp::price_oracle::Client = client_rpc.bootstrap(Side::Server);
        tokio::task::spawn_local(async move {
            let _ = client_rpc.await;
        });

        client
    }

    #[tokio::test]
    async fn test_rpc_get_price() {
        let local = tokio::task::LocalSet::new();
        local
            .run_until(async {
                let client = setup_oracle();
                let mut req = client.get_price_request();
                req.get().set_pair("ETH/gas");
                let resp = req.send().promise.await.unwrap();
                let r = resp.get().unwrap();
                assert_eq!(r.get_price(), 30_000_000_000);
                assert_eq!(r.get_decimals(), 9);
                assert!((r.get_confidence() - 0.99).abs() < 0.001);
            })
            .await;
    }

    #[tokio::test]
    async fn test_rpc_get_pairs() {
        let local = tokio::task::LocalSet::new();
        local
            .run_until(async {
                let client = setup_oracle();
                let resp = client.get_pairs_request().send().promise.await.unwrap();
                let pairs = resp.get().unwrap().get_pairs().unwrap();
                assert_eq!(pairs.len(), PAIRS.len() as u32);
            })
            .await;
    }

    #[tokio::test]
    async fn test_rpc_unknown_pair() {
        let local = tokio::task::LocalSet::new();
        local
            .run_until(async {
                let client = setup_oracle();
                let mut req = client.get_price_request();
                req.get().set_pair("DOGE/usd");
                let resp = req.send().promise.await;
                assert!(resp.is_err(), "unknown pair should error");
            })
            .await;
    }

    // --- JSON response tests ---

    #[test]
    fn test_json_response_all_pairs() {
        let cache = init_cache();
        cache.borrow_mut().insert(
            "ETH/gas".to_string(),
            PriceEntry {
                price: 30_000_000_000,
                decimals: 9,
                timestamp: 1700000000,
                confidence: 0.99,
            },
        );
        let json = build_json_response(&cache, "");
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert!(parsed["pairs"]["ETH/gas"]["price"].is_number());
        assert!(parsed["pairs"]["POLYGON/gas"].is_object());
        assert!(parsed["pairs"]["BASE/gas"].is_object());
    }

    #[test]
    fn test_json_response_filtered_pair() {
        let cache = init_cache();
        cache.borrow_mut().insert(
            "ETH/gas".to_string(),
            PriceEntry {
                price: 30_000_000_000,
                decimals: 9,
                timestamp: 1700000000,
                confidence: 0.99,
            },
        );
        let json = build_json_response(&cache, "pair=ETH%2Fgas");
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert!(parsed["pairs"]["ETH/gas"]["price"].is_number());
        // Other pairs should not be present
        assert!(parsed["pairs"]["POLYGON/gas"].is_null());
    }

    #[test]
    fn test_json_response_unknown_pair_filter() {
        let cache = init_cache();
        let json = build_json_response(&cache, "pair=DOGE%2Fusd");
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
        let pairs = parsed["pairs"].as_object().unwrap();
        assert!(pairs.is_empty());
    }
}
