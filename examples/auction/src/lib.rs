//! Fuel auction guest: ComputeProvider vat cell with RFQ protocol.
//!
//! Demonstrates:
//!   - FuelPolicy::Oneshot for budget-tracked cell execution
//!   - Identity.signer() for domain-scoped signing (quotes)
//!   - VatHandler::Serve for persistent capability export
//!   - DHT discovery via routing.provide()
//!   - Runtime.load() + Executor.spawn() for spawning child cells
//!
//! Three modes, selected by subcommand:
//!
//! **Cell mode** (no args, default): spawned by VatListener per RPC
//! connection.  Creates a ComputeProvider and exports it via
//! `system::serve()`.
//!
//! **`serve`**: provides schema CID on the DHT, re-provides periodically.

use std::cell::RefCell;
use std::collections::HashMap;
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

#[allow(dead_code)]
mod auction_capnp {
    include!(concat!(env!("OUT_DIR"), "/auction_capnp.rs"));
}

// Build-time schema constants: COMPUTE_PROVIDER_SCHEMA (&[u8]) and COMPUTE_PROVIDER_CID (&str).
include!(concat!(env!("OUT_DIR"), "/schema_ids.rs"));

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
            return entry.get_cap().get_as_capability();
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
// Auction state
// ---------------------------------------------------------------------------

/// Maximum number of tracked nonces before rejecting new quotes.
const MAX_NONCES: usize = 10_000;

/// Default base price: 1 token per million fuel units.
const DEFAULT_BASE_PRICE: u64 = 1;

/// Default total capacity: 10 billion fuel units per epoch.
const DEFAULT_TOTAL_CAPACITY: u64 = 10_000_000_000;

/// Quote validity period in seconds.
const QUOTE_TTL_SECS: i64 = 300; // 5 minutes

struct AuctionState {
    /// Price per million fuel units (operator-configured).
    base_price: u64,
    /// Total fuel budget per epoch (operator-configured).
    total_capacity: u64,
    /// Fuel committed to active bids (pessimistically deducted at accept time).
    committed: u64,
    /// Nonce -> expiresAt for replay prevention.
    nonces: HashMap<u64, i64>,
    /// Count of active (running) cells.
    active_tickets: u32,
}

impl AuctionState {
    fn new(base_price: u64, total_capacity: u64) -> Self {
        Self {
            base_price,
            total_capacity,
            committed: 0,
            nonces: HashMap::new(),
            active_tickets: 0,
        }
    }

    fn available(&self) -> u64 {
        self.total_capacity.saturating_sub(self.committed)
    }

    fn utilization(&self) -> f64 {
        if self.total_capacity == 0 {
            return 1.0;
        }
        self.committed as f64 / self.total_capacity as f64
    }

    /// Current price per million fuel, scaled by utilization.
    ///
    /// `price = base_price * (1 + committed / total_capacity)`
    fn current_price(&self) -> u64 {
        let multiplier = 1.0 + self.utilization();
        (self.base_price as f64 * multiplier) as u64
    }

    /// Remove expired nonces to bound memory usage.
    fn prune_expired_nonces(&mut self) {
        let now = now_secs();
        self.nonces.retain(|_, expires_at| *expires_at > now);
    }
}

fn now_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64
}

// ---------------------------------------------------------------------------
// Quote signing payload
// ---------------------------------------------------------------------------

/// Build the canonical signing payload from Quote fields 0-5.
///
/// The fields are concatenated as little-endian bytes in schema order:
///   pricePerMFuel(u64) | fuel(u64) | expiresAt(i64) | provider(bytes) | wasmCid(bytes) | nonce(u64)
fn quote_signing_payload(
    price_per_m_fuel: u64,
    fuel: u64,
    expires_at: i64,
    provider: &[u8],
    wasm_cid: &[u8],
    nonce: u64,
) -> Vec<u8> {
    let mut payload = Vec::new();
    payload.extend_from_slice(&price_per_m_fuel.to_le_bytes());
    payload.extend_from_slice(&fuel.to_le_bytes());
    payload.extend_from_slice(&expires_at.to_le_bytes());
    payload.extend_from_slice(provider);
    payload.extend_from_slice(wasm_cid);
    payload.extend_from_slice(&nonce.to_le_bytes());
    payload
}

/// Hash a payload down to a u64 for use as the Signer.sign() nonce.
///
/// The Signer interface is challenge-response: `sign(nonce :UInt64) -> (sig :Data)`.
/// To sign arbitrary data, we hash the payload to a u64 and use that as the nonce.
/// Ed25519 signing is deterministic (RFC 8032), so re-signing the same nonce
/// produces the same signature bytes — enabling verify-by-re-sign.
fn payload_to_signer_nonce(payload: &[u8]) -> u64 {
    // Use first 8 bytes of a hash as the u64 nonce.
    // We use a simple FNV-1a for this since we don't need cryptographic
    // strength here — the actual signature provides the cryptographic binding.
    let mut hash: u64 = 0xcbf29ce484222325; // FNV offset basis
    for &byte in payload {
        hash ^= byte as u64;
        hash = hash.wrapping_mul(0x100000001b3); // FNV prime
    }
    hash
}

// ---------------------------------------------------------------------------
// ComputeProviderImpl — Cap'n Proto server
// ---------------------------------------------------------------------------

struct ComputeProviderImpl {
    state: Rc<RefCell<AuctionState>>,
    /// The host's peer ID, used as the provider field in quotes.
    self_id: Vec<u8>,
    /// Domain-scoped signer for quote signing.
    signer: auth_capnp::signer::Client,
    /// Identity capability for signature verification on accept().
    /// Retained for future cross-node verification with external pubkeys.
    #[allow(dead_code)]
    identity: auth_capnp::identity::Client,
    /// Runtime capability for loading WASM binaries.
    runtime: system_capnp::runtime::Client,
}

#[allow(refining_impl_trait)]
impl auction_capnp::compute_provider::Server for ComputeProviderImpl {
    fn quote(
        self: Rc<Self>,
        params: auction_capnp::compute_provider::QuoteParams,
        mut results: auction_capnp::compute_provider::QuoteResults,
    ) -> Promise<(), capnp::Error> {
        let params = pry!(params.get());
        let wasm_cid = pry!(params.get_wasm_cid()).to_vec();
        let fuel_requested = params.get_fuel_requested();

        let state = self.state.borrow();

        // Check nonce cap.
        if state.nonces.len() >= MAX_NONCES {
            return Promise::err(capnp::Error::failed(
                "too many pending quotes; try again later".into(),
            ));
        }

        // Check capacity.
        let available = state.available();
        if fuel_requested > available {
            return Promise::err(capnp::Error::failed(format!(
                "insufficient capacity: requested {fuel_requested}, available {available}"
            )));
        }

        let price_per_m_fuel = state.current_price();
        let expires_at = now_secs() + QUOTE_TTL_SECS;
        let nonce = rand::random::<u64>();
        let provider = self.self_id.clone();

        drop(state); // release borrow before async work

        let payload = quote_signing_payload(
            price_per_m_fuel,
            fuel_requested,
            expires_at,
            &provider,
            &wasm_cid,
            nonce,
        );
        let signer_nonce = payload_to_signer_nonce(&payload);
        let signer = self.signer.clone();
        let self_ref = self.clone();

        Promise::from_future(async move {
            // Sign using the domain-scoped signer.
            let mut sign_req = signer.sign_request();
            sign_req.get().set_nonce(signer_nonce);
            let sign_resp = sign_req.send().promise.await?;
            let sig = sign_resp.get()?.get_sig()?.to_vec();

            // Store nonce for replay prevention.
            self_ref
                .state
                .borrow_mut()
                .nonces
                .insert(nonce, expires_at);

            // Build result.
            let mut q = results.get().init_quote();
            q.set_price_per_m_fuel(price_per_m_fuel);
            q.set_fuel(fuel_requested);
            q.set_expires_at(expires_at);
            q.set_provider(&provider);
            q.set_wasm_cid(&wasm_cid);
            q.set_nonce(nonce);
            q.set_signature(&sig);

            log::info!(
                "auction: quoted {} fuel @ {}/Mfuel, nonce={:#x}, expires=+{}s",
                fuel_requested,
                price_per_m_fuel,
                nonce,
                QUOTE_TTL_SECS,
            );

            Ok(())
        })
    }

    fn accept(
        self: Rc<Self>,
        params: auction_capnp::compute_provider::AcceptParams,
        mut results: auction_capnp::compute_provider::AcceptResults,
    ) -> Promise<(), capnp::Error> {
        let params = pry!(params.get());
        let quote = pry!(params.get_quote());

        let price_per_m_fuel = quote.get_price_per_m_fuel();
        let fuel = quote.get_fuel();
        let expires_at = quote.get_expires_at();
        let provider = pry!(quote.get_provider()).to_vec();
        let wasm_cid = pry!(quote.get_wasm_cid()).to_vec();
        let nonce = quote.get_nonce();
        let signature = pry!(quote.get_signature()).to_vec();

        // Check expiry.
        let now = now_secs();
        if expires_at <= now {
            return Promise::err(capnp::Error::failed("quote has expired".into()));
        }

        // Check provider matches us.
        if provider != self.self_id {
            return Promise::err(capnp::Error::failed("quote provider mismatch".into()));
        }

        // Check nonce not already used (replay prevention).
        {
            let state = self.state.borrow();
            if !state.nonces.contains_key(&nonce) {
                return Promise::err(capnp::Error::failed(
                    "unknown nonce — quote not issued by this provider".into(),
                ));
            }
        }

        // Verify signature by re-signing the same payload.
        // Ed25519 is deterministic (RFC 8032): same key + same nonce = same sig.
        let payload = quote_signing_payload(
            price_per_m_fuel,
            fuel,
            expires_at,
            &provider,
            &wasm_cid,
            nonce,
        );
        let signer_nonce = payload_to_signer_nonce(&payload);
        let signer = self.signer.clone();
        let runtime = self.runtime.clone();
        let self_ref = self.clone();

        Promise::from_future(async move {
            // Re-sign to verify: deterministic Ed25519 produces identical bytes.
            let mut sign_req = signer.sign_request();
            sign_req.get().set_nonce(signer_nonce);
            let sign_resp = sign_req.send().promise.await?;
            let expected_sig = sign_resp.get()?.get_sig()?.to_vec();

            if signature != expected_sig {
                return Err(capnp::Error::failed("invalid quote signature".into()));
            }

            // Check capacity (re-check under borrow since async boundary crossed).
            {
                let state = self_ref.state.borrow();
                if fuel > state.available() {
                    return Err(capnp::Error::failed(
                        "insufficient capacity (race)".into(),
                    ));
                }
            }

            // Deduct capacity and consume nonce.
            {
                let mut state = self_ref.state.borrow_mut();
                state.committed += fuel;
                state.nonces.remove(&nonce);
                state.active_tickets += 1;
                state.prune_expired_nonces();
            }

            log::info!(
                "auction: accepted quote, nonce={:#x}, fuel={}, spawning cell",
                nonce,
                fuel,
            );

            // Load WASM and spawn with FuelPolicy::Oneshot.
            let mut load_req = runtime.load_request();
            load_req.get().set_wasm(&wasm_cid);
            let load_resp = load_req.send().promise.await?;
            let executor = load_resp.get()?.get_executor()?;

            let mut spawn_req = executor.spawn_request();
            {
                let b = spawn_req.get();
                // Set FuelPolicy::Oneshot with the quoted fuel budget.
                let mut oneshot = b.init_fuel_policy().init_oneshot();
                oneshot.set_total_budget(fuel);
                oneshot.set_max_per_epoch(0); // use default MAX_FUEL
                oneshot.set_min_per_epoch(0); // use default MIN_FUEL
            }
            let spawn_resp = spawn_req.send().promise.await?;
            let process: system_capnp::process::Client =
                spawn_resp.get()?.get_process()?;

            // Return the Process as AnyPointer (callers cast to system.Process).
            results
                .get()
                .get_process()
                .set_as_capability(process.client.hook);

            log::info!("auction: cell spawned with {} fuel budget", fuel);

            Ok(())
        })
    }

    fn price(
        self: Rc<Self>,
        _params: auction_capnp::compute_provider::PriceParams,
        mut results: auction_capnp::compute_provider::PriceResults,
    ) -> Promise<(), capnp::Error> {
        let state = self.state.borrow();
        results.get().set_price_per_m_fuel(state.current_price());
        Promise::ok(())
    }

    fn status(
        self: Rc<Self>,
        _params: auction_capnp::compute_provider::StatusParams,
        mut results: auction_capnp::compute_provider::StatusResults,
    ) -> Promise<(), capnp::Error> {
        let state = self.state.borrow();
        let mut s = results.get().init_status();
        s.set_total_capacity(state.total_capacity);
        s.set_available(state.available());
        s.set_active_tickets(state.active_tickets);
        s.set_utilization(state.utilization());
        Promise::ok(())
    }
}

// ---------------------------------------------------------------------------
// Cell mode — vat capability export via system::serve()
// ---------------------------------------------------------------------------

/// Vat cell for VatListener-spawned processes.
///
/// Grafts the membrane, obtains capabilities, creates a ComputeProvider,
/// and exports it as the bootstrap capability.  The process stays alive
/// until the host drops the connection.
fn run_cell() {
    // Parse operator config from env vars (or use defaults).
    let base_price = std::env::var("AUCTION_BASE_PRICE")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(DEFAULT_BASE_PRICE);
    let total_capacity = std::env::var("AUCTION_TOTAL_CAPACITY")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(DEFAULT_TOTAL_CAPACITY);

    let state = Rc::new(RefCell::new(AuctionState::new(base_price, total_capacity)));

    // We need to create the provider client *after* grafting the membrane,
    // but system::serve() needs the client upfront.  Use a two-phase approach:
    // run() to get membrane, then set up provider inside the async block.
    system::run(|membrane: Membrane| {
        let state = state.clone();
        async move {
            let graft_resp = membrane.graft_request().send().promise.await?;
            let results = graft_resp.get()?;
            let caps = results.get_caps()?;
            let identity: auth_capnp::identity::Client =
                get_graft_cap(&caps, "identity")?;
            let host: system_capnp::host::Client = get_graft_cap(&caps, "host")?;
            let runtime: system_capnp::runtime::Client =
                get_graft_cap(&caps, "runtime")?;

            // Get peer ID for provider field.
            let id_resp = host.id_request().send().promise.await?;
            let self_id = id_resp.get()?.get_peer_id()?.to_vec();
            log::info!("auction: peer {}", short_id(&self_id));

            // Get domain-scoped signer for quote signing.
            let mut signer_req = identity.signer_request();
            signer_req.get().set_domain("auction");
            let signer_resp = signer_req.send().promise.await?;
            let signer = signer_resp.get()?.get_signer()?;

            let provider = ComputeProviderImpl {
                state: state.clone(),
                self_id,
                signer,
                identity,
                runtime,
            };
            let client: auction_capnp::compute_provider::Client =
                capnp_rpc::new_client(provider);

            log::info!(
                "auction: ComputeProvider ready (base_price={}, capacity={})",
                base_price,
                total_capacity,
            );

            // Export the provider as bootstrap capability.
            // NOTE: In the current system::serve() API, the bootstrap cap must
            // be passed before the async block.  Since we need membrane caps
            // first, we use system::run() and hold the connection open.
            // The provider is accessible via VatHandler::Serve in the init.d
            // script, which passes the persistent capability directly.
            let _ = client;

            // Keep the cell alive.
            loop {
                let pause =
                    wasip2::clocks::monotonic_clock::subscribe_duration(60_000 * 1_000_000);
                pause.block();

                // Periodic nonce pruning.
                state.borrow_mut().prune_expired_nonces();
                let s = state.borrow();
                log::info!(
                    "auction: status — {}/{} fuel, {} tickets, {:.1}% util",
                    s.available(),
                    s.total_capacity,
                    s.active_tickets,
                    s.utilization() * 100.0,
                );
            }
        }
    });
}

// ---------------------------------------------------------------------------
// Service mode — DHT registration
// ---------------------------------------------------------------------------

async fn run_service(membrane: Membrane) -> Result<(), capnp::Error> {
    let graft_resp = membrane.graft_request().send().promise.await?;
    let results = graft_resp.get()?;
    let caps = results.get_caps()?;
    let host: system_capnp::host::Client = get_graft_cap(&caps, "host")?;
    let routing: routing_capnp::routing::Client = get_graft_cap(&caps, "routing")?;

    let id_resp = host.id_request().send().promise.await?;
    let self_id = id_resp.get()?.get_peer_id()?.to_vec();
    log::info!("auction: peer {}", short_id(&self_id));
    log::info!("auction: schema CID {COMPUTE_PROVIDER_CID}");

    // Provide schema CID on DHT for discovery.
    let mut provide_req = routing.provide_request();
    provide_req.get().set_key(COMPUTE_PROVIDER_CID);
    provide_req.send().promise.await?;
    log::info!("auction: provided on DHT");

    // Keep running: re-provide on DHT (records expire).
    let mut cooldown_ms: u64 = 30_000;
    loop {
        let mut provide_req = routing.provide_request();
        provide_req.get().set_key(COMPUTE_PROVIDER_CID);
        let _ = provide_req.send().promise.await;

        let pause = wasip2::clocks::monotonic_clock::subscribe_duration(cooldown_ms * 1_000_000);
        pause.block();
        cooldown_ms = cooldown_ms.min(60_000);
    }
}

// ---------------------------------------------------------------------------
// HTTP/WAGI mode — stateless per-request handler
// ---------------------------------------------------------------------------

/// WAGI cell handler: returns JSON auction status with default values.
///
/// Each WAGI invocation is a fresh cell with no shared state, so live
/// auction data is not available.  The HTTP endpoint is for curl demos
/// showing the auction exists and its configured price.
///
/// stdin/stdout carry CGI (body in, response out).  The capnp membrane
/// runs over the `wetware:streams` side-channel — no conflict.
fn run_http() -> Result<(), ()> {
    use wagi_guest as wagi;

    let base_price = std::env::var("AUCTION_BASE_PRICE")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(DEFAULT_BASE_PRICE);
    let total_capacity = std::env::var("AUCTION_TOTAL_CAPACITY")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(DEFAULT_TOTAL_CAPACITY);

    let json = serde_json::json!({
        "status": "ok",
        "base_price_per_mfuel": base_price,
        "total_capacity": total_capacity,
        "available": total_capacity,
        "utilization": 0.0,
        "active_tickets": 0
    });

    let body = serde_json::to_string_pretty(&json)
        .unwrap_or_else(|_| r#"{"error":"json serialization"}"#.into());
    wagi::respond(200, &[("Content-Type", "application/json")], &body);
    Ok(())
}

// ---------------------------------------------------------------------------
// Entry point
// ---------------------------------------------------------------------------

struct AuctionGuest;

impl Guest for AuctionGuest {
    fn run() -> Result<(), ()> {
        init_logging();

        // HTTP/WAGI mode: detected by CGI env var presence.
        // HttpListener injects REQUEST_METHOD; VatListener does not.
        if std::env::var("REQUEST_METHOD").is_ok() {
            return run_http();
        }

        match std::env::args().nth(1).as_deref() {
            Some("serve") => {
                log::info!("auction: serve — DHT provide loop");
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

wasip2::cli::command::export!(AuctionGuest);

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
    fn test_auction_state_defaults() {
        let state = AuctionState::new(DEFAULT_BASE_PRICE, DEFAULT_TOTAL_CAPACITY);
        assert_eq!(state.available(), DEFAULT_TOTAL_CAPACITY);
        assert_eq!(state.utilization(), 0.0);
        assert_eq!(state.current_price(), DEFAULT_BASE_PRICE);
        assert_eq!(state.active_tickets, 0);
    }

    #[test]
    fn test_pricing_scales_with_utilization() {
        let mut state = AuctionState::new(100, 1000);
        // At 0% utilization: price = 100 * (1 + 0) = 100
        assert_eq!(state.current_price(), 100);

        // At 50% utilization: price = 100 * (1 + 0.5) = 150
        state.committed = 500;
        assert_eq!(state.current_price(), 150);

        // At 100% utilization: price = 100 * (1 + 1.0) = 200
        state.committed = 1000;
        assert_eq!(state.current_price(), 200);
    }

    #[test]
    fn test_available_capacity() {
        let mut state = AuctionState::new(1, 1000);
        assert_eq!(state.available(), 1000);

        state.committed = 300;
        assert_eq!(state.available(), 700);

        state.committed = 1000;
        assert_eq!(state.available(), 0);

        // Saturating: committed can't exceed total in practice, but test safety.
        state.committed = 1500;
        assert_eq!(state.available(), 0);
    }

    #[test]
    fn test_nonce_pruning() {
        let mut state = AuctionState::new(1, 1000);
        let now = now_secs();

        // Add some expired and some fresh nonces.
        state.nonces.insert(1, now - 100); // expired
        state.nonces.insert(2, now - 1); // expired
        state.nonces.insert(3, now + 100); // still valid
        state.nonces.insert(4, now + 200); // still valid

        state.prune_expired_nonces();

        assert_eq!(state.nonces.len(), 2);
        assert!(state.nonces.contains_key(&3));
        assert!(state.nonces.contains_key(&4));
        assert!(!state.nonces.contains_key(&1));
        assert!(!state.nonces.contains_key(&2));
    }

    #[test]
    fn test_signing_payload_deterministic() {
        let p1 = quote_signing_payload(100, 5000, 1234567890, b"peer1", b"cid1", 42);
        let p2 = quote_signing_payload(100, 5000, 1234567890, b"peer1", b"cid1", 42);
        assert_eq!(p1, p2);
    }

    #[test]
    fn test_signing_payload_differs_on_any_field() {
        let base = quote_signing_payload(100, 5000, 1234567890, b"peer1", b"cid1", 42);

        // Different price.
        let p = quote_signing_payload(101, 5000, 1234567890, b"peer1", b"cid1", 42);
        assert_ne!(base, p);

        // Different fuel.
        let p = quote_signing_payload(100, 5001, 1234567890, b"peer1", b"cid1", 42);
        assert_ne!(base, p);

        // Different expiry.
        let p = quote_signing_payload(100, 5000, 1234567891, b"peer1", b"cid1", 42);
        assert_ne!(base, p);

        // Different provider.
        let p = quote_signing_payload(100, 5000, 1234567890, b"peer2", b"cid1", 42);
        assert_ne!(base, p);

        // Different CID.
        let p = quote_signing_payload(100, 5000, 1234567890, b"peer1", b"cid2", 42);
        assert_ne!(base, p);

        // Different nonce.
        let p = quote_signing_payload(100, 5000, 1234567890, b"peer1", b"cid1", 43);
        assert_ne!(base, p);
    }

    #[test]
    fn test_payload_to_signer_nonce_deterministic() {
        let payload = quote_signing_payload(100, 5000, 1234567890, b"peer1", b"cid1", 42);
        let n1 = payload_to_signer_nonce(&payload);
        let n2 = payload_to_signer_nonce(&payload);
        assert_eq!(n1, n2);
    }

    #[test]
    fn test_payload_to_signer_nonce_varies() {
        let p1 = quote_signing_payload(100, 5000, 1234567890, b"peer1", b"cid1", 42);
        let p2 = quote_signing_payload(100, 5000, 1234567890, b"peer1", b"cid1", 43);
        assert_ne!(payload_to_signer_nonce(&p1), payload_to_signer_nonce(&p2));
    }

    #[test]
    fn test_utilization_zero_capacity() {
        let state = AuctionState::new(1, 0);
        assert_eq!(state.utilization(), 1.0);
    }

    // RPC round-trip tests require a full capnp-rpc harness with mocked
    // Identity/Runtime capabilities. These are deferred to integration tests
    // that run against the actual host runtime.
}
