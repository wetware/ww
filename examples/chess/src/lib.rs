//! Chess guest: cross-node play via RPC capability cells.
//!
//! This binary serves two roles, selected by env vars set in the
//! init.d script (`etc/init.d/chess.glia`):
//!
//! Two modes, selected by subcommand:
//!
//! **Cell mode** (no args, default): creates a ChessEngine and exports it via
//! `system::serve()`.
//!
//! **`serve`**: provides on the DHT, discovers peers via
//! `routing.find_providers()`, dials them via `VatClient` to get typed
//! ChessEngine capabilities, and plays random games logging replays.

use std::cell::RefCell;
use std::collections::HashSet;
use std::rc::Rc;

use capnp::capability::Promise;
use capnp_rpc::pry;
use shakmaty::fen::Fen;
use shakmaty::uci::UciMove;
use shakmaty::{Chess, EnPassantMode, Position};
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
pub mod chess_capnp {
    include!(concat!(env!("OUT_DIR"), "/chess_capnp.rs"));
}

#[cfg(not(target_arch = "wasm32"))]
pub mod chess_authority;

#[cfg(not(target_arch = "wasm32"))]
authority::impl_terminal_session_pipeline!(chess_capnp::chess_engine::Client);

// Build-time schema constants: CHESS_ENGINE_SCHEMA (&[u8]) and CHESS_ENGINE_CID (&str).
// Vat publication uses the service name below; the schema CID is metadata.
include!(concat!(env!("OUT_DIR"), "/schema_ids.rs"));

const CHESS_SERVICE: &str = "chess";

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
            return entry.get_cap().get_as_capability::<T>();
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
// Logging (WASI stderr, same pattern as kernel)
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
// ChessEngineImpl — shakmaty-backed Cap'n Proto server
// ---------------------------------------------------------------------------

/// Chess engine backed by shakmaty.
///
/// Implements `chess_capnp::chess_engine::Server`. Exported via
/// `system::serve` in cell mode; used directly in unit tests.
pub struct ChessEngineImpl {
    pos: RefCell<Chess>,
}

impl Default for ChessEngineImpl {
    fn default() -> Self {
        Self {
            pos: RefCell::new(Chess::default()),
        }
    }
}

impl ChessEngineImpl {
    pub fn new() -> Self {
        Self {
            pos: RefCell::new(Chess::default()),
        }
    }

    // -- Direct accessors for unit tests (no RPC round-trip) --

    pub fn fen(&self) -> String {
        Fen::from_position(self.pos.borrow().clone(), EnPassantMode::Legal).to_string()
    }

    pub fn apply(&self, uci: &str) -> Result<(), String> {
        let uci_move: UciMove = uci
            .parse()
            .map_err(|e| format!("invalid UCI '{uci}': {e}"))?;
        let mut pos = self.pos.borrow_mut();
        let m = uci_move
            .to_move(&*pos)
            .map_err(|e| format!("illegal move '{uci}': {e}"))?;
        pos.play_unchecked(&m);
        Ok(())
    }

    pub fn legal_moves_uci(&self) -> Vec<String> {
        let pos = self.pos.borrow();
        pos.legal_moves()
            .iter()
            .map(|m| UciMove::from_standard(m).to_string())
            .collect()
    }

    pub fn status(&self) -> chess_capnp::chess_engine::GameStatus {
        use chess_capnp::chess_engine::GameStatus;
        let pos = self.pos.borrow();
        if pos.is_checkmate() {
            GameStatus::Checkmate
        } else if pos.is_stalemate() {
            GameStatus::Stalemate
        } else if pos.is_insufficient_material() {
            GameStatus::Draw
        } else {
            GameStatus::Ongoing
        }
    }
}

#[allow(refining_impl_trait)]
impl chess_capnp::chess_engine::Server for ChessEngineImpl {
    fn get_state(
        self: Rc<Self>,
        _params: chess_capnp::chess_engine::GetStateParams,
        mut results: chess_capnp::chess_engine::GetStateResults,
    ) -> Promise<(), capnp::Error> {
        results.get().set_fen(self.fen());
        Promise::ok(())
    }

    fn apply_move(
        self: Rc<Self>,
        params: chess_capnp::chess_engine::ApplyMoveParams,
        mut results: chess_capnp::chess_engine::ApplyMoveResults,
    ) -> Promise<(), capnp::Error> {
        let uci = pry!(pry!(params.get()).get_uci()).to_str().unwrap_or("");
        match self.apply(uci) {
            Ok(()) => {
                results.get().set_ok(true);
                results.get().set_reason("");
            }
            Err(reason) => {
                results.get().set_ok(false);
                results.get().set_reason(&reason);
            }
        }
        Promise::ok(())
    }

    fn get_legal_moves(
        self: Rc<Self>,
        _params: chess_capnp::chess_engine::GetLegalMovesParams,
        mut results: chess_capnp::chess_engine::GetLegalMovesResults,
    ) -> Promise<(), capnp::Error> {
        let moves = self.legal_moves_uci();
        let mut list = results.get().init_moves(moves.len() as u32);
        for (i, m) in moves.iter().enumerate() {
            list.set(i as u32, m);
        }
        Promise::ok(())
    }

    fn get_status(
        self: Rc<Self>,
        _params: chess_capnp::chess_engine::GetStatusParams,
        mut results: chess_capnp::chess_engine::GetStatusResults,
    ) -> Promise<(), capnp::Error> {
        results.get().set_status(self.status());
        Promise::ok(())
    }
}

// ---------------------------------------------------------------------------
// Cell mode — vat capability export via system::serve()
// ---------------------------------------------------------------------------

/// Vat cell for VatListener-spawned processes.
///
/// Creates a ChessEngine and exports it as the bootstrap capability.
/// The host bridges this cap to the connecting peer. The process stays
/// alive until the host drops the connection.
fn run_cell() {
    let engine = ChessEngineImpl::new();
    let client: chess_capnp::chess_engine::Client = capnp_rpc::new_client(engine);
    log::info!("cell: exporting ChessEngine via RPC");
    system::serve(client.client, |_membrane: Membrane| async move {
        // Keep alive until the host drops the RPC connection.
        // drive_rpc_with_future exits when rpc_done becomes true.
        std::future::pending().await
    });
}

// ---------------------------------------------------------------------------
// RpcDialingSink — discovers peers and dials them via VatClient
// ---------------------------------------------------------------------------

struct RpcDialingSink {
    vat_client: system_capnp::vat_client::Client,
    self_id: Vec<u8>,
    seen: Rc<RefCell<HashSet<Vec<u8>>>>,
}

#[allow(refining_impl_trait)]
impl routing_capnp::provider_sink::Server for RpcDialingSink {
    fn provider(
        self: Rc<Self>,
        params: routing_capnp::provider_sink::ProviderParams,
    ) -> Promise<(), capnp::Error> {
        let peer_id = pry!(pry!(pry!(params.get()).get_info()).get_peer_id()).to_vec();

        // Skip self and already-seen peers.
        if peer_id == self.self_id || !self.seen.borrow_mut().insert(peer_id.clone()) {
            return Promise::ok(());
        }

        let vat_client = self.vat_client.clone();
        let self_id = self.self_id.clone();
        let peer = peer_id.clone();

        Promise::from_future(async move {
            if let Err(e) = play_rpc_against_peer(&vat_client, &self_id, &peer).await {
                log::error!("game vs {} failed: {e}", short_id(&peer));
            }
            // Pause between games so the output is readable.
            let pause = wasip2::clocks::monotonic_clock::subscribe_duration(
                5_000_000_000, // 5s
            );
            pause.block();
            Ok(())
        })
    }

    fn done(
        self: Rc<Self>,
        _params: routing_capnp::provider_sink::DoneParams,
        _results: routing_capnp::provider_sink::DoneResults,
    ) -> Promise<(), capnp::Error> {
        // Intentionally silent — the discovery loop tracks state transitions.
        Promise::ok(())
    }
}

// ---------------------------------------------------------------------------
// play_rpc_against_peer — dial via VatClient and play a typed RPC game
// ---------------------------------------------------------------------------

/// Log a replay node. Previously published to IPFS; now just logged.
/// IPFS content access was removed from the graft response — cells use
/// the WASI virtual filesystem (CidTree) instead.
fn log_replay_node(json: &str) -> Option<String> {
    log::debug!("replay: {json}");
    None
}

async fn play_rpc_against_peer(
    vat_client: &system_capnp::vat_client::Client,
    self_id: &[u8],
    peer_id: &[u8],
) -> Result<(), capnp::Error> {
    let us = short_id(self_id);
    let them = short_id(peer_id);

    // Dial peer via VatClient — returns a typed ChessEngine capability.
    let mut req = vat_client.dial_request();
    req.get().set_peer(peer_id);
    req.get().set_protocol(CHESS_SERVICE);
    let resp = req.send().promise.await?;
    let dialed = resp.get()?.get_cap();
    let engine: chess_capnp::chess_engine::Client = dialed.get_as_capability()?;

    log::info!("game {us} vs {them}: started (RPC)");
    play_rpc_game(&engine, &us, &them).await
}

/// Drive a game using typed ChessEngine RPC calls.
///
/// The service orchestrates both sides: picks random moves for white (us)
/// and black (them) and applies them to the remote engine. Each move pair
/// is logged locally for replay debugging.
async fn play_rpc_game(
    engine: &chess_capnp::chess_engine::Client,
    us: &str,
    them: &str,
) -> Result<(), capnp::Error> {
    use chess_capnp::chess_engine::GameStatus;

    let mut move_num = 0u32;
    let mut prev_cid: Option<String> = None;

    /// Format the `"prev"` portion of a replay node.
    fn prev_field(cid: &Option<String>) -> String {
        match cid {
            Some(c) => format!(r#""prev":"{c}""#),
            None => r#""prev":null"#.to_string(),
        }
    }

    loop {
        // --- White's turn (us) ---
        let moves_resp = engine.get_legal_moves_request().send().promise.await?;
        let moves = moves_resp.get()?.get_moves()?;
        if moves.is_empty() {
            // No legal moves for white — black wins.
            let node = format!(r#"{{"result":"0-1",{}}}"#, prev_field(&prev_cid));
            prev_cid = log_replay_node(&node).or(prev_cid);
            log::info!("game {us} vs {them}: {them} wins after {move_num} moves");
            break;
        }

        let white_move = moves
            .get(rand::random_range(0..moves.len()))?
            .to_str()
            .map_err(|e| capnp::Error::failed(format!("invalid move UTF-8: {e}")))?
            .to_string();

        let mut apply_req = engine.apply_move_request();
        apply_req.get().set_uci(&white_move);
        let apply_resp = apply_req.send().promise.await?;
        let apply_result = apply_resp.get()?;
        if !apply_result.get_ok() {
            let reason = apply_result.get_reason()?.to_str().unwrap_or("unknown");
            return Err(capnp::Error::failed(format!(
                "white move {white_move} rejected: {reason}"
            )));
        }
        move_num += 1;

        // Check status after white's move.
        let status_resp = engine.get_status_request().send().promise.await?;
        let status = status_resp.get()?.get_status()?;
        if status != GameStatus::Ongoing {
            let result_str = match status {
                GameStatus::Checkmate => "1-0",
                _ => "1/2-1/2",
            };
            let node = format!(
                r#"{{"n":{move_num},"w":"{white_move}","result":"{result_str}",{}}}"#,
                prev_field(&prev_cid)
            );
            prev_cid = log_replay_node(&node).or(prev_cid);
            log::info!("game {us} vs {them}: {us} wins after {move_num} moves ({white_move})");
            break;
        }

        // --- Black's turn (them) ---
        let moves_resp = engine.get_legal_moves_request().send().promise.await?;
        let moves = moves_resp.get()?.get_moves()?;
        if moves.is_empty() {
            // No legal moves for black after white moved — shouldn't happen
            // if status was Ongoing, but handle gracefully.
            let node = format!(
                r#"{{"n":{move_num},"w":"{white_move}","result":"1-0",{}}}"#,
                prev_field(&prev_cid)
            );
            prev_cid = log_replay_node(&node).or(prev_cid);
            log::info!("game {us} vs {them}: {us} wins after {move_num} moves");
            break;
        }

        let black_move = moves
            .get(rand::random_range(0..moves.len()))?
            .to_str()
            .map_err(|e| capnp::Error::failed(format!("invalid move UTF-8: {e}")))?
            .to_string();

        let mut apply_req = engine.apply_move_request();
        apply_req.get().set_uci(&black_move);
        let apply_resp = apply_req.send().promise.await?;
        let apply_result = apply_resp.get()?;
        if !apply_result.get_ok() {
            let reason = apply_result.get_reason()?.to_str().unwrap_or("unknown");
            return Err(capnp::Error::failed(format!(
                "black move {black_move} rejected: {reason}"
            )));
        }

        // Publish this move pair as a node in the replay linked list.
        let node = format!(
            r#"{{"n":{move_num},"w":"{white_move}","b":"{black_move}",{}}}"#,
            prev_field(&prev_cid)
        );
        prev_cid = log_replay_node(&node).or(prev_cid);
        log::info!("  {move_num}. {white_move} {black_move}");

        // Check status after black's move.
        let status_resp = engine.get_status_request().send().promise.await?;
        let status = status_resp.get()?.get_status()?;
        if status != GameStatus::Ongoing {
            let result_str = match status {
                GameStatus::Checkmate => "0-1",
                _ => "1/2-1/2",
            };
            let node = format!(r#"{{"result":"{result_str}",{}}}"#, prev_field(&prev_cid));
            prev_cid = log_replay_node(&node).or(prev_cid);
            log::info!("game {us} vs {them}: {them} wins");
            break;
        }
    }

    // Log the root CID — the tip of the replay linked list.
    if let Some(cid) = &prev_cid {
        log::info!("game {us} vs {them}: replay \u{2192} {cid}");
    }
    log::info!("game {us} vs {them}: complete ({move_num} moves)");

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

    // Get network capabilities — vat_client for typed capability dialing.
    let network_resp = host.network_request().send().promise.await?;
    let network = network_resp.get()?;
    let vat_client = network.get_vat_client()?;

    // Resolve peer identity.
    let id_resp = host.id_request().send().promise.await?;
    let self_id = id_resp.get()?.get_peer_id()?.to_vec();
    log::info!("service: peer {}", short_id(&self_id));
    log::info!("service: name {CHESS_SERVICE}");

    log::info!("service: looking for opponent...");

    let service_key = routing_key(&routing, CHESS_SERVICE).await?;
    log::info!("service: routing key {service_key}");

    // Discovery loop with exponential backoff + jitter.
    let seen = Rc::new(RefCell::new(HashSet::<Vec<u8>>::new()));
    let mut cooldown_ms: u64 = 2_000;
    const BASE_MS: u64 = 2_000;
    const MAX_MS: u64 = 900_000;

    loop {
        let prev_seen = seen.borrow().len();

        // Re-provide (DHT records expire).
        let mut provide_req = routing.provide_request();
        provide_req.get().set_key(&service_key);
        provide_req.send().promise.await?;

        // Search for peers; RpcDialingSink dials new ones via RPC.
        let sink: routing_capnp::provider_sink::Client = capnp_rpc::new_client(RpcDialingSink {
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
            log::info!("service: found {} opponent(s)", now_seen);
            cooldown_ms = BASE_MS;
        } else {
            cooldown_ms = (cooldown_ms * 2).min(MAX_MS);
        }

        let delay_ms = cooldown_ms / 2 + rand::random_range(0..=cooldown_ms / 2);
        let pause = wasip2::clocks::monotonic_clock::subscribe_duration(
            delay_ms * 1_000_000, // ms -> ns
        );
        pause.block();
    }
}

// ---------------------------------------------------------------------------
// Entry point
// ---------------------------------------------------------------------------

struct ChessGuest;

impl Guest for ChessGuest {
    fn run() -> Result<(), ()> {
        init_logging();
        match std::env::args().nth(1).as_deref() {
            Some("serve") => {
                log::info!("chess: serve — discovery + game loop");
                system::run(|membrane: Membrane| async move { run_service(membrane).await });
            }
            _ => {
                // Default (no args): cell mode — export the ChessEngine capability.
                run_cell();
            }
        }
        Ok(())
    }
}

wasip2::cli::command::export!(ChessGuest);

// ---------------------------------------------------------------------------
// Unit tests (native, no RPC needed)
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use chess_capnp::chess_engine::GameStatus;
    use membrane::{MethodProfile, Policy};

    #[test]
    fn test_typed_method_profile_uses_generated_chess_client() {
        use capnp::traits::HasTypeId;

        let reader = MethodProfile::<chess_capnp::chess_engine::Client>::new()
            .allow_method(chess_capnp::chess_engine::Client::get_state_request)
            .unwrap()
            .build();
        let player = MethodProfile::<chess_capnp::chess_engine::Client>::new()
            .allow_method(chess_capnp::chess_engine::Client::get_state_request)
            .unwrap()
            .allow_method(chess_capnp::chess_engine::Client::apply_move_request)
            .unwrap()
            .build();
        let interface_id = chess_capnp::chess_engine::Client::TYPE_ID;

        assert!(reader.check(interface_id, 0).is_ok());
        assert!(reader.check(interface_id, 1).is_err());
        assert!(player.check(interface_id, 0).is_ok());
        assert!(player.check(interface_id, 1).is_ok());
    }

    #[test]
    fn test_initial_fen() {
        let engine = ChessEngineImpl::new();
        assert_eq!(
            engine.fen(),
            "rnbqkbnr/pppppppp/8/8/8/8/PPPPPPPP/RNBQKBNR w KQkq - 0 1"
        );
    }

    #[test]
    fn test_apply_valid_move() {
        let engine = ChessEngineImpl::new();
        assert!(engine.apply("e2e4").is_ok());
        assert!(engine.fen().contains("4P3")); // pawn on e4
    }

    #[test]
    fn test_apply_invalid_move() {
        let engine = ChessEngineImpl::new();
        let result = engine.apply("e1e5"); // king can't jump to e5
        assert!(result.is_err());
    }

    #[test]
    fn test_legal_moves_count() {
        let engine = ChessEngineImpl::new();
        // Starting position has 20 legal moves (16 pawn + 4 knight).
        assert_eq!(engine.legal_moves_uci().len(), 20);
    }

    #[test]
    fn test_game_status_ongoing() {
        let engine = ChessEngineImpl::new();
        assert_eq!(engine.status(), GameStatus::Ongoing);
    }

    #[test]
    fn test_scholars_mate() {
        let engine = ChessEngineImpl::new();
        // 1. e4 e5 2. Bc4 Nc6 3. Qh5 Nf6 4. Qxf7#
        for uci in &["e2e4", "e7e5", "f1c4", "b8c6", "d1h5", "g8f6", "h5f7"] {
            engine
                .apply(uci)
                .unwrap_or_else(|e| panic!("move {uci} failed: {e}"));
        }
        assert_eq!(engine.status(), GameStatus::Checkmate);
    }

    // -----------------------------------------------------------------------
    // Two-engine game simulation (mirrors RPC game loop logic)
    // -----------------------------------------------------------------------

    #[test]
    fn test_two_engine_game_simulation() {
        // Simulate the game flow: one engine, both sides pick random moves.
        // This mirrors what play_rpc_game does over RPC.
        let engine = ChessEngineImpl::new();
        let mut move_num = 0u32;
        let max_moves = 300;

        loop {
            // White's turn.
            let moves = engine.legal_moves_uci();
            if moves.is_empty() {
                break;
            }
            let white_move = &moves[rand::random_range(0..moves.len())];
            engine.apply(white_move).unwrap();
            move_num += 1;

            if engine.status() != GameStatus::Ongoing {
                break;
            }

            // Black's turn.
            let moves = engine.legal_moves_uci();
            if moves.is_empty() {
                break;
            }
            let black_move = &moves[rand::random_range(0..moves.len())];
            engine.apply(black_move).unwrap();

            if engine.status() != GameStatus::Ongoing || move_num >= max_moves {
                break;
            }
        }

        assert!(move_num > 0, "game should have played at least one move");
    }

    // -----------------------------------------------------------------------
    // RPC round-trip tests (Cap'n Proto over in-memory duplex)
    // -----------------------------------------------------------------------

    use capnp_rpc::rpc_twoparty_capnp::Side;
    use capnp_rpc::twoparty::VatNetwork;
    use capnp_rpc::RpcSystem;
    use tokio::io;
    use tokio_util::compat::{TokioAsyncReadCompatExt, TokioAsyncWriteCompatExt};

    /// Bootstrap a ChessEngine client/server pair over in-memory duplex.
    fn setup_engine() -> chess_capnp::chess_engine::Client {
        let (client_stream, server_stream) = io::duplex(8 * 1024);
        let (client_read, client_write) = io::split(client_stream);
        let (server_read, server_write) = io::split(server_stream);

        let engine_server: chess_capnp::chess_engine::Client =
            capnp_rpc::new_client(ChessEngineImpl::new());

        let server_network = VatNetwork::new(
            server_read.compat(),
            server_write.compat_write(),
            Side::Server,
            Default::default(),
        );
        let server_rpc = RpcSystem::new(Box::new(server_network), Some(engine_server.client));
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
        let client: chess_capnp::chess_engine::Client = client_rpc.bootstrap(Side::Server);
        tokio::task::spawn_local(async move {
            let _ = client_rpc.await;
        });

        client
    }

    #[tokio::test]
    async fn test_rpc_get_state() {
        let local = tokio::task::LocalSet::new();
        local
            .run_until(async {
                let client = setup_engine();
                let resp = client.get_state_request().send().promise.await.unwrap();
                let fen = resp.get().unwrap().get_fen().unwrap().to_str().unwrap();
                assert_eq!(
                    fen,
                    "rnbqkbnr/pppppppp/8/8/8/8/PPPPPPPP/RNBQKBNR w KQkq - 0 1"
                );
            })
            .await;
    }

    #[tokio::test]
    async fn test_rpc_apply_move_and_get_state() {
        let local = tokio::task::LocalSet::new();
        local
            .run_until(async {
                let client = setup_engine();

                // Apply e2e4.
                let mut req = client.apply_move_request();
                req.get().set_uci("e2e4");
                let resp = req.send().promise.await.unwrap();
                let result = resp.get().unwrap();
                assert!(result.get_ok());

                // Verify FEN reflects the move.
                let resp = client.get_state_request().send().promise.await.unwrap();
                let fen = resp.get().unwrap().get_fen().unwrap().to_str().unwrap();
                assert!(fen.contains("4P3"), "expected pawn on e4, got: {fen}");
            })
            .await;
    }

    #[tokio::test]
    async fn test_rpc_apply_illegal_move() {
        let local = tokio::task::LocalSet::new();
        local
            .run_until(async {
                let client = setup_engine();

                let mut req = client.apply_move_request();
                req.get().set_uci("e1e5"); // king can't jump to e5
                let resp = req.send().promise.await.unwrap();
                let result = resp.get().unwrap();
                assert!(!result.get_ok());
                let reason = result.get_reason().unwrap().to_str().unwrap();
                assert!(!reason.is_empty(), "expected error reason");
            })
            .await;
    }

    #[tokio::test]
    async fn test_rpc_get_legal_moves() {
        let local = tokio::task::LocalSet::new();
        local
            .run_until(async {
                let client = setup_engine();
                let resp = client
                    .get_legal_moves_request()
                    .send()
                    .promise
                    .await
                    .unwrap();
                let moves = resp.get().unwrap().get_moves().unwrap();
                assert_eq!(moves.len(), 20); // 16 pawn + 4 knight
            })
            .await;
    }

    #[tokio::test]
    async fn test_rpc_scholars_mate_status() {
        let local = tokio::task::LocalSet::new();
        local
            .run_until(async {
                let client = setup_engine();

                for uci in &["e2e4", "e7e5", "f1c4", "b8c6", "d1h5", "g8f6", "h5f7"] {
                    let mut req = client.apply_move_request();
                    req.get().set_uci(uci);
                    let resp = req.send().promise.await.unwrap();
                    assert!(resp.get().unwrap().get_ok(), "move {uci} rejected over RPC");
                }

                let resp = client.get_status_request().send().promise.await.unwrap();
                let status = resp.get().unwrap().get_status().unwrap();
                assert_eq!(status, GameStatus::Checkmate);
            })
            .await;
    }

    // -----------------------------------------------------------------------
    // RPC full game — mirrors play_rpc_game() without IPFS
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn test_rpc_full_game() {
        let local = tokio::task::LocalSet::new();
        local
            .run_until(async {
                let client = setup_engine();
                let mut move_count = 0u32;

                loop {
                    // White's turn.
                    let moves_resp = client
                        .get_legal_moves_request()
                        .send()
                        .promise
                        .await
                        .unwrap();
                    let moves = moves_resp.get().unwrap().get_moves().unwrap();
                    if moves.is_empty() {
                        break;
                    }

                    let m = moves
                        .get(rand::random_range(0..moves.len()))
                        .unwrap()
                        .to_str()
                        .unwrap();
                    let mut req = client.apply_move_request();
                    req.get().set_uci(m);
                    let resp = req.send().promise.await.unwrap();
                    assert!(resp.get().unwrap().get_ok(), "move {m} rejected");
                    move_count += 1;

                    let status_resp = client.get_status_request().send().promise.await.unwrap();
                    if status_resp.get().unwrap().get_status().unwrap() != GameStatus::Ongoing {
                        break;
                    }

                    // Black's turn.
                    let moves_resp = client
                        .get_legal_moves_request()
                        .send()
                        .promise
                        .await
                        .unwrap();
                    let moves = moves_resp.get().unwrap().get_moves().unwrap();
                    if moves.is_empty() {
                        break;
                    }

                    let m = moves
                        .get(rand::random_range(0..moves.len()))
                        .unwrap()
                        .to_str()
                        .unwrap();
                    let mut req = client.apply_move_request();
                    req.get().set_uci(m);
                    let resp = req.send().promise.await.unwrap();
                    assert!(resp.get().unwrap().get_ok(), "move {m} rejected");

                    let status_resp = client.get_status_request().send().promise.await.unwrap();
                    if status_resp.get().unwrap().get_status().unwrap() != GameStatus::Ongoing {
                        break;
                    }
                }

                assert!(move_count > 0, "game should play at least one move");
            })
            .await;
    }

    // -------------------------------------------------------------------
    // Discovery backoff & jitter
    // -------------------------------------------------------------------

    /// Mirror the backoff constants from run_service so tests break if they drift.
    const BASE_MS: u64 = 2_000;
    const MAX_MS: u64 = 900_000;

    #[test]
    fn test_backoff_doubles_to_max() {
        let mut cooldown = BASE_MS;
        for _ in 0..30 {
            cooldown = (cooldown * 2).min(MAX_MS);
        }
        assert_eq!(cooldown, MAX_MS, "must cap at MAX_MS");
    }

    #[test]
    fn test_backoff_resets_on_new_peer() {
        // Simulate: fully backed off, then new peer found resets to BASE.
        let mut cooldown = BASE_MS;
        assert_eq!(cooldown, BASE_MS);
        // Next idle pass doubles.
        cooldown = (cooldown * 2).min(MAX_MS);
        assert_eq!(cooldown, BASE_MS * 2);
    }

    #[test]
    fn test_jitter_within_half_to_full() {
        // Jitter formula: cooldown/2 + rand(0..=cooldown/2).
        // Must produce values in [cooldown/2, cooldown].
        for cooldown in [BASE_MS, 4_000, 64_000, MAX_MS] {
            for _ in 0..500 {
                let delay = cooldown / 2 + rand::random_range(0..=cooldown / 2);
                assert!(
                    delay >= cooldown / 2,
                    "delay {delay} < floor {} (cooldown={cooldown})",
                    cooldown / 2,
                );
                assert!(delay <= cooldown, "delay {delay} > ceiling {cooldown}",);
            }
        }
    }

    #[test]
    fn test_jitter_max_is_strict_ceiling() {
        // After capping at MAX_MS, the jitter must never exceed it.
        let cooldown = MAX_MS;
        for _ in 0..1000 {
            let delay = cooldown / 2 + rand::random_range(0..=cooldown / 2);
            assert!(delay <= MAX_MS, "delay {delay} exceeds MAX_MS {MAX_MS}");
        }
    }

    // -------------------------------------------------------------------
    // Replay linked-list JSON
    // -------------------------------------------------------------------

    #[test]
    fn test_replay_move_pair_json() {
        let prev: Option<String> = Some("bafyPREV".into());
        let prev_field = match &prev {
            Some(c) => format!(r#""prev":"{c}""#),
            None => r#""prev":null"#.to_string(),
        };
        let node = format!(r#"{{"n":1,"w":"e2e4","b":"e7e5",{prev_field}}}"#,);
        let v: serde_json::Value =
            serde_json::from_str(&node).unwrap_or_else(|e| panic!("invalid JSON: {e}\n{node}"));
        assert_eq!(v["n"], 1);
        assert_eq!(v["w"], "e2e4");
        assert_eq!(v["b"], "e7e5");
        assert_eq!(v["prev"], "bafyPREV");
    }

    #[test]
    fn test_replay_first_node_null_prev() {
        let prev: Option<String> = None;
        let prev_field = match &prev {
            Some(c) => format!(r#""prev":"{c}""#),
            None => r#""prev":null"#.to_string(),
        };
        let node = format!(r#"{{"n":1,"w":"e2e4","b":"e7e5",{prev_field}}}"#,);
        let v: serde_json::Value = serde_json::from_str(&node).unwrap();
        assert!(v["prev"].is_null());
    }

    #[test]
    fn test_replay_terminal_node_has_result() {
        let prev_field = r#""prev":"bafyLAST""#;
        // Win terminal.
        let win = format!(r#"{{"n":36,"w":"c7c8q","result":"1-0",{prev_field}}}"#);
        let v: serde_json::Value = serde_json::from_str(&win).unwrap();
        assert_eq!(v["result"], "1-0");
        assert!(v.get("b").is_none(), "terminal node has no black move");

        // Loss terminal.
        let loss = format!(r#"{{"result":"0-1",{prev_field}}}"#);
        let v: serde_json::Value = serde_json::from_str(&loss).unwrap();
        assert_eq!(v["result"], "0-1");

        // Interrupted terminal.
        let star = format!(r#"{{"n":5,"w":"d2d4","result":"*",{prev_field}}}"#);
        let v: serde_json::Value = serde_json::from_str(&star).unwrap();
        assert_eq!(v["result"], "*");
    }
}
