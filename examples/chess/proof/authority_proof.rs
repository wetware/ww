use std::fmt;
use std::time::Duration;

use auth::SigningDomain;
use authority::{auth_capnp, system_capnp, Epoch, EpochGuard, Provenance};
use capnp::capability::{FromClientHook, Promise};
use chess::chess_authority::{chess_method_profile, ChessProfile};
use chess::{chess_capnp, ChessEngineImpl};
use ed25519_dalek::SigningKey;
use libp2p_identity::PeerId;
use membrane::{call_failure_code, CallFailureCode};
use tokio::sync::{mpsc, oneshot, watch};

const OPERATION_DEADLINE: Duration = Duration::from_secs(2);
const CLEANUP_DEADLINE: Duration = Duration::from_secs(2);
const LOGIN_DEADLINE: Duration = Duration::from_millis(500);
const PROTOCOL_NAME: &str = "chess-authority-proof";
const READER_KEY_SEED: u8 = 11;
const PLAYER_KEY_SEED: u8 = 12;
const UNKNOWN_KEY_SEED: u8 = 13;
const INITIAL_FEN: &str = "rnbqkbnr/pppppppp/8/8/8/8/PPPPPPPP/RNBQKBNR w KQkq - 0 1";
const E2E4_FEN: &str = "rnbqkbnr/pppppppp/8/8/4P3/8/PPPP1PPP/RNBQKBNR b KQkq - 0 1";

type ChessClient = chess_capnp::chess_engine::Client;
type TerminalClient = auth_capnp::terminal::Client<auth_capnp::opaque_session::Owned>;
type Remote = ww::rpc::vat_dial::VatDial<TerminalClient>;
type HostTask = tokio::task::JoinHandle<anyhow::Result<()>>;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ProofDenialCode {
    PermissionDenied,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ProofEvent {
    Setup { runtime_version: String },
    Published { service: String },
    UnknownAuthorizationDenied,
    ReaderRead { fen: String },
    ReaderWriteDenied { code: ProofDenialCode },
    PlayerWrite { chess_move: String },
    SharedState { fen: String },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ProofStage {
    Setup,
    Publication,
    UnknownAuthorization,
    ReaderRead,
    ReaderWriteDenied,
    PlayerWrite,
    SharedState,
    InjectedFailure,
    Cleanup,
    Transcript,
}

impl fmt::Display for ProofStage {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Setup => "setup",
            Self::Publication => "publication",
            Self::UnknownAuthorization => "unknownAuthorization",
            Self::ReaderRead => "readerRead",
            Self::ReaderWriteDenied => "readerWriteDenied",
            Self::PlayerWrite => "playerWrite",
            Self::SharedState => "sharedState",
            Self::InjectedFailure => "injectedFailure",
            Self::Cleanup => "cleanup",
            Self::Transcript => "transcript",
        })
    }
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct CleanupEvidence {
    pub epoch_advanced: bool,
    pub issued_capability_rejected: bool,
    pub active_connections: usize,
    pub remote_rpc_systems_awaited: usize,
    pub server_host_awaited: bool,
    pub client_host_awaited: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProofOutcome {
    pub events: Vec<ProofEvent>,
    pub cleanup: CleanupEvidence,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProcessOutput {
    pub stdout: String,
    pub stderr: String,
    pub exit_code: i32,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProofFailure {
    pub stage: ProofStage,
    pub diagnostic: String,
    pub cleanup: CleanupEvidence,
    pub cleanup_diagnostics: Vec<String>,
}

impl ProofFailure {
    fn new(stage: ProofStage, diagnostic: impl Into<String>) -> Self {
        Self {
            stage,
            diagnostic: diagnostic.into(),
            cleanup: CleanupEvidence::default(),
            cleanup_diagnostics: Vec::new(),
        }
    }
}

impl fmt::Display for ProofFailure {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "FAIL {} {}", self.stage, self.diagnostic)?;
        for diagnostic in &self.cleanup_diagnostics {
            write!(formatter, "; cleanup: {diagnostic}")?;
        }
        Ok(())
    }
}

impl std::error::Error for ProofFailure {}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct ProofOptions {
    pub fail_after_reader_authorization: bool,
}

#[derive(Clone)]
struct TestSigner {
    keypair: libp2p_identity::Keypair,
}

impl TestSigner {
    fn from_ed25519(key: &SigningKey) -> Result<Self, capnp::Error> {
        let keypair =
            libp2p_identity::ed25519::Keypair::try_from_bytes(&mut key.to_keypair_bytes())
                .map_err(|error| capnp::Error::failed(format!("invalid signing key: {error}")))?;
        Ok(Self {
            keypair: keypair.into(),
        })
    }
}

#[allow(refining_impl_trait)]
impl auth_capnp::signer::Server for TestSigner {
    fn sign(
        self: capnp::capability::Rc<Self>,
        params: auth_capnp::signer::SignParams,
        mut results: auth_capnp::signer::SignResults,
    ) -> Promise<(), capnp::Error> {
        let params = capnp_rpc::pry!(params.get());
        let mut payload = Vec::with_capacity(16);
        payload.extend_from_slice(&params.get_nonce().to_be_bytes());
        payload.extend_from_slice(&params.get_epoch_seq().to_be_bytes());
        let domain = SigningDomain::terminal_membrane();
        let envelope = capnp_rpc::pry!(libp2p_core::SignedEnvelope::new(
            &self.keypair,
            domain.as_str().to_string(),
            domain.payload_type().to_vec(),
            payload,
        )
        .map_err(|error| capnp::Error::failed(format!("signing failed: {error}"))));
        results.get().set_sig(&envelope.into_protobuf_encoding());
        Promise::ok(())
    }
}

struct Runner {
    options: ProofOptions,
    events: Vec<ProofEvent>,
    epoch_tx: watch::Sender<Epoch>,
    epoch_rx: watch::Receiver<Epoch>,
    service_budget: ww::rpc::ConnectionBudget,
    shared_game: Option<ChessClient>,
    reader: Option<ChessClient>,
    player: Option<ChessClient>,
    remotes: Vec<Remote>,
    server_streams: Option<libp2p_stream::Control>,
    client_streams: Option<libp2p_stream::Control>,
    server_commands: Option<mpsc::Sender<ww::rpc::SwarmCommand>>,
    client_commands: Option<mpsc::Sender<ww::rpc::SwarmCommand>>,
    server_host: Option<HostTask>,
    client_host: Option<HostTask>,
    server_peer: Option<PeerId>,
}

impl Runner {
    fn new(options: ProofOptions) -> Result<Self, ProofFailure> {
        let (epoch_tx, epoch_rx) = watch::channel(epoch(1));
        let service_budget = ww::rpc::ConnectionBudget::new(8)
            .map_err(|error| ProofFailure::new(ProofStage::Setup, error.to_string()))?;
        Ok(Self {
            options,
            events: Vec::with_capacity(7),
            epoch_tx,
            epoch_rx,
            service_budget,
            shared_game: None,
            reader: None,
            player: None,
            remotes: Vec::with_capacity(3),
            server_streams: None,
            client_streams: None,
            server_commands: None,
            client_commands: None,
            server_host: None,
            client_host: None,
            server_peer: None,
        })
    }

    async fn execute(&mut self) -> Result<(), ProofFailure> {
        chess_method_profile(ChessProfile::Reader).map_err(|error| {
            ProofFailure::new(
                ProofStage::Setup,
                format!("failed to capture Reader method profile: {error}"),
            )
        })?;
        chess_method_profile(ChessProfile::Player).map_err(|error| {
            ProofFailure::new(
                ProofStage::Setup,
                format!("failed to capture Player method profile: {error}"),
            )
        })?;

        let (server_peer, server_state, server_streams, server_commands, server_host) =
            start_libp2p_host(21)?;
        self.server_peer = Some(server_peer);
        self.server_streams = Some(server_streams);
        self.server_commands = Some(server_commands);
        self.server_host = Some(server_host);

        let (_client_peer, client_state, client_streams, client_commands, client_host) =
            start_libp2p_host(22)?;
        self.client_streams = Some(client_streams);
        self.client_commands = Some(client_commands);
        self.client_host = Some(client_host);

        timeout(
            ProofStage::Setup,
            "server listen-address publication",
            server_state.wait_for_listen_addr(),
        )
        .await?;
        timeout(
            ProofStage::Setup,
            "client listen-address publication",
            client_state.wait_for_listen_addr(),
        )
        .await?;
        self.events.push(ProofEvent::Setup {
            runtime_version: ww::VERSION.to_string(),
        });

        let shared_game: ChessClient = capnp_rpc::new_client(ChessEngineImpl::new());
        self.shared_game = Some(shared_game.clone());
        let guard = EpochGuard {
            issued_seq: self.epoch_rx.borrow().seq,
            receiver: self.epoch_rx.clone(),
        };
        let listener = ww::rpc::vat_listener::VatListenerImpl::new(
            self.server_streams
                .as_ref()
                .ok_or_else(|| ProofFailure::new(ProofStage::Setup, "server streams missing"))?
                .clone(),
            guard,
        )
        .with_budget(self.service_budget.clone())
        .with_login_timeout(LOGIN_DEADLINE);
        let listener: system_capnp::vat_listener::Client = capnp_rpc::new_client(listener);
        let reader_key = SigningKey::from_bytes(&[READER_KEY_SEED; 32]);
        let player_key = SigningKey::from_bytes(&[PLAYER_KEY_SEED; 32]);
        let mut publication = listener.serve_authenticated_request();
        publication
            .get()
            .init_cap()
            .set_as_capability(shared_game.client.clone().hook);
        publication.get().set_protocol(PROTOCOL_NAME);
        write_chess_policy(publication.get().init_policy(), &reader_key, &player_key)?;
        timeout(
            ProofStage::Publication,
            "authenticated service publication",
            publication.send().promise,
        )
        .await?
        .map_err(|error| {
            ProofFailure::new(
                ProofStage::Publication,
                format!("authenticated service publication failed: {error}"),
            )
        })?;
        self.events.push(ProofEvent::Published {
            service: PROTOCOL_NAME.to_string(),
        });

        connect_hosts(
            self.client_commands.as_ref().ok_or_else(|| {
                ProofFailure::new(ProofStage::Setup, "client command channel missing")
            })?,
            server_peer,
            &server_state,
        )
        .await?;

        let unknown_key = SigningKey::from_bytes(&[UNKNOWN_KEY_SEED; 32]);
        let unknown_remote = self.open_remote(ProofStage::UnknownAuthorization).await?;
        self.remotes.push(unknown_remote);
        let (unknown_status, unknown) = login_opaque(
            &self
                .remotes
                .last()
                .expect("just pushed Unknown remote")
                .bootstrap,
            &unknown_key,
            ProofStage::UnknownAuthorization,
        )
        .await?;
        if unknown_status != auth_capnp::LoginStatus::Denied || unknown.is_some() {
            return Err(ProofFailure::new(
                ProofStage::UnknownAuthorization,
                format!("unknown valid identity received status {unknown_status:?} or a session"),
            ));
        }
        self.events.push(ProofEvent::UnknownAuthorizationDenied);

        let reader_remote = self.open_remote(ProofStage::ReaderRead).await?;
        self.remotes.push(reader_remote);
        let (reader_status, reader) = login_opaque(
            &self
                .remotes
                .last()
                .expect("just pushed Reader remote")
                .bootstrap,
            &reader_key,
            ProofStage::ReaderRead,
        )
        .await?;
        if reader_status != auth_capnp::LoginStatus::Granted {
            return Err(ProofFailure::new(
                ProofStage::ReaderRead,
                format!("Reader login returned {reader_status:?}"),
            ));
        }
        self.reader = Some(reader.ok_or_else(|| {
            ProofFailure::new(ProofStage::ReaderRead, "Reader login omitted its session")
        })?);

        if self.options.fail_after_reader_authorization {
            return Err(ProofFailure::new(
                ProofStage::InjectedFailure,
                "forced failure after Reader authorization",
            ));
        }

        let initial_fen = get_state(
            self.reader.as_ref().expect("Reader stored"),
            ProofStage::ReaderRead,
        )
        .await?;
        if initial_fen != INITIAL_FEN {
            return Err(ProofFailure::new(
                ProofStage::ReaderRead,
                format!("unexpected initial state: {initial_fen}"),
            ));
        }
        self.events
            .push(ProofEvent::ReaderRead { fen: initial_fen });

        let reader_move = apply_move(
            self.reader.as_ref().expect("Reader stored"),
            "e2e4",
            ProofStage::ReaderWriteDenied,
        )
        .await;
        let denial = match reader_move {
            Ok(()) => {
                return Err(ProofFailure::new(
                    ProofStage::ReaderWriteDenied,
                    "Reader applyMove unexpectedly succeeded",
                ));
            }
            Err(denial) => denial,
        };
        if call_failure_code_from_proof(&denial) != Some(CallFailureCode::PermissionDenied) {
            return Err(ProofFailure::new(
                ProofStage::ReaderWriteDenied,
                format!("Reader applyMove returned the wrong error: {denial}"),
            ));
        }
        self.events.push(ProofEvent::ReaderWriteDenied {
            code: ProofDenialCode::PermissionDenied,
        });

        let player_remote = self.open_remote(ProofStage::PlayerWrite).await?;
        self.remotes.push(player_remote);
        let (player_status, player) = login_opaque(
            &self
                .remotes
                .last()
                .expect("just pushed Player remote")
                .bootstrap,
            &player_key,
            ProofStage::PlayerWrite,
        )
        .await?;
        if player_status != auth_capnp::LoginStatus::Granted {
            return Err(ProofFailure::new(
                ProofStage::PlayerWrite,
                format!("Player login returned {player_status:?}"),
            ));
        }
        self.player = Some(player.ok_or_else(|| {
            ProofFailure::new(ProofStage::PlayerWrite, "Player login omitted its session")
        })?);
        apply_move(
            self.player.as_ref().expect("Player stored"),
            "e2e4",
            ProofStage::PlayerWrite,
        )
        .await?;
        self.events.push(ProofEvent::PlayerWrite {
            chess_move: "e2e4".to_string(),
        });

        let changed_fen = get_state(
            self.reader.as_ref().expect("Reader stored"),
            ProofStage::SharedState,
        )
        .await?;
        if changed_fen != E2E4_FEN {
            return Err(ProofFailure::new(
                ProofStage::SharedState,
                format!("Reader did not observe e2e4 in the shared game: {changed_fen}"),
            ));
        }
        self.events
            .push(ProofEvent::SharedState { fen: changed_fen });
        Ok(())
    }

    async fn open_remote(&mut self, stage: ProofStage) -> Result<Remote, ProofFailure> {
        let peer = self
            .server_peer
            .ok_or_else(|| ProofFailure::new(stage, "server peer missing"))?;
        let protocol = ww::rpc::vat_protocol(PROTOCOL_NAME)
            .map_err(|error| ProofFailure::new(stage, error.to_string()))?;
        let stream = timeout(
            stage,
            "opening authenticated Chess stream",
            self.client_streams
                .as_mut()
                .ok_or_else(|| ProofFailure::new(stage, "client streams missing"))?
                .open_stream(peer, protocol),
        )
        .await?
        .map_err(|error| {
            ProofFailure::new(stage, format!("failed to open Chess stream: {error}"))
        })?;
        Ok(ww::rpc::vat_dial::connect::<_, TerminalClient>(stream))
    }

    async fn cleanup(&mut self) -> CleanupReport {
        let mut report = CleanupReport::default();

        match self.epoch_tx.send(epoch(2)) {
            Ok(()) => report.evidence.epoch_advanced = true,
            Err(error) => report
                .diagnostics
                .push(format!("failed to advance Atom epoch: {error}")),
        }

        if let Some(reader) = self.reader.as_ref() {
            match get_state(reader, ProofStage::Cleanup).await {
                Err(error)
                    if matches!(
                        call_failure_code_from_proof(&error),
                        Some(CallFailureCode::StaleEpoch | CallFailureCode::TargetRevoked)
                    ) =>
                {
                    report.evidence.issued_capability_rejected = true;
                }
                Err(error) => report.diagnostics.push(format!(
                    "issued Reader capability failed with the wrong cleanup result: {error}"
                )),
                Ok(fen) => report.diagnostics.push(format!(
                    "issued Reader capability remained live after epoch advance: {fen}"
                )),
            }
        }

        self.reader.take();
        self.player.take();
        self.shared_game.take();

        for remote in self.remotes.drain(..) {
            let ww::rpc::vat_dial::VatDial { bootstrap, driver } = remote;
            drop(bootstrap);
            driver.abort();
            match tokio::time::timeout(CLEANUP_DEADLINE, driver).await {
                Ok(Err(error)) if error.is_cancelled() => {
                    report.evidence.remote_rpc_systems_awaited += 1;
                }
                Ok(Ok(Ok(()))) => {
                    report.evidence.remote_rpc_systems_awaited += 1;
                }
                Ok(Ok(Err(error))) => report
                    .diagnostics
                    .push(format!("remote RPC system failed during cleanup: {error}")),
                Ok(Err(error)) => report
                    .diagnostics
                    .push(format!("remote RPC task join failed: {error}")),
                Err(_) => report
                    .diagnostics
                    .push("timed out awaiting a remote RPC system".to_string()),
            }
        }

        let drained = tokio::time::timeout(CLEANUP_DEADLINE, async {
            while self.service_budget.active() != 0 {
                tokio::task::yield_now().await;
            }
        })
        .await;
        report.evidence.active_connections = self.service_budget.active();
        if drained.is_err() {
            report.diagnostics.push(format!(
                "connection budget did not drain (active={})",
                report.evidence.active_connections
            ));
        }

        self.server_streams.take();
        self.client_streams.take();
        self.server_commands.take();
        self.client_commands.take();
        report.evidence.server_host_awaited =
            await_aborted_host(self.server_host.take(), "server", &mut report.diagnostics).await;
        report.evidence.client_host_awaited =
            await_aborted_host(self.client_host.take(), "client", &mut report.diagnostics).await;

        report
    }
}

#[derive(Default)]
pub struct CleanupReport {
    pub evidence: CleanupEvidence,
    pub diagnostics: Vec<String>,
}

pub fn merge_cleanup(
    result: Result<(), ProofFailure>,
    events: Vec<ProofEvent>,
    cleanup: CleanupReport,
) -> Result<ProofOutcome, ProofFailure> {
    match result {
        Ok(()) if cleanup.diagnostics.is_empty() => Ok(ProofOutcome {
            events,
            cleanup: cleanup.evidence,
        }),
        Ok(()) => Err(ProofFailure {
            stage: ProofStage::Cleanup,
            diagnostic: "observable cleanup failed".to_string(),
            cleanup: cleanup.evidence,
            cleanup_diagnostics: cleanup.diagnostics,
        }),
        Err(mut failure) => {
            failure.cleanup = cleanup.evidence;
            failure.cleanup_diagnostics = cleanup.diagnostics;
            Err(failure)
        }
    }
}

pub async fn run_authority_proof(options: ProofOptions) -> Result<ProofOutcome, ProofFailure> {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async move {
            let mut runner = Runner::new(options)?;
            let result = runner.execute().await;
            let cleanup = runner.cleanup().await;
            merge_cleanup(result, runner.events, cleanup)
        })
        .await
}

pub fn expected_events() -> Vec<ProofEvent> {
    vec![
        ProofEvent::Setup {
            runtime_version: ww::VERSION.to_string(),
        },
        ProofEvent::Published {
            service: PROTOCOL_NAME.to_string(),
        },
        ProofEvent::UnknownAuthorizationDenied,
        ProofEvent::ReaderRead {
            fen: INITIAL_FEN.to_string(),
        },
        ProofEvent::ReaderWriteDenied {
            code: ProofDenialCode::PermissionDenied,
        },
        ProofEvent::PlayerWrite {
            chess_move: "e2e4".to_string(),
        },
        ProofEvent::SharedState {
            fen: E2E4_FEN.to_string(),
        },
    ]
}

pub fn render_success(events: &[ProofEvent]) -> Result<String, ProofFailure> {
    if events != expected_events() {
        return Err(ProofFailure::new(
            ProofStage::Transcript,
            "incomplete or reordered evidence",
        ));
    }

    let reader = public_fingerprint(READER_KEY_SEED);
    let player = public_fingerprint(PLAYER_KEY_SEED);
    let unknown = public_fingerprint(UNKNOWN_KEY_SEED);
    Ok(format!(
        concat!(
            "WETWARE CHESS AUTHORITY PROOF\n",
            "Wetware {}\n",
            "\n",
            "IDENTITIES\n",
            "  Reader  {}\n",
            "  Player  {}\n",
            "  Unknown {}\n",
            "\n",
            "POLICY\n",
            "  Reader  -> getState\n",
            "  Player  -> getState, applyMove\n",
            "  Unknown -> no profile\n",
            "\n",
            "OUTCOMES\n",
            "  Unknown login       DENIED\n",
            "  Reader getState     ALLOWED\n",
            "  Reader applyMove    DENIED: permissionDenied\n",
            "  Player applyMove    ALLOWED: e2e4\n",
            "  Reader getState     ALLOWED: shared board contains e2e4\n",
            "\n",
            "RESULT\n",
            "  Same remote service. Different issued authority.\n",
            "\n",
            "SCOPE\n",
            "This proof controls method calls made through the issued ChessEngine capability.\n",
            "It does not prove the executor lacks ambient credentials, shell access, network egress,\n",
            "alternate APIs, or other bypass paths. It does not enforce per-customer, per-side,\n",
            "per-move, per-argument, or per-resource policy.\n",
            "\n",
            "PASS\n",
        ),
        ww::VERSION,
        reader,
        player,
        unknown,
    ))
}

pub fn render_failure(failure: &ProofFailure) -> String {
    failure.to_string()
}

pub fn render_process_result(result: Result<ProofOutcome, ProofFailure>) -> ProcessOutput {
    match result {
        Ok(outcome) => match render_success(&outcome.events) {
            Ok(stdout) => ProcessOutput {
                stdout,
                stderr: String::new(),
                exit_code: 0,
            },
            Err(failure) => ProcessOutput {
                stdout: String::new(),
                stderr: format!("{}\n", render_failure(&failure)),
                exit_code: 1,
            },
        },
        Err(failure) => ProcessOutput {
            stdout: String::new(),
            stderr: format!("{}\n", render_failure(&failure)),
            exit_code: 1,
        },
    }
}

fn write_chess_policy(
    mut policy: auth_capnp::authority_policy::Builder<'_>,
    reader_key: &SigningKey,
    player_key: &SigningKey,
) -> Result<(), ProofFailure> {
    // This typed capture protects trusted configuration from accidental
    // ordinal mistakes. It does not constrain malicious configuration code.
    let reader = chess_method_profile(ChessProfile::Reader).map_err(|error| {
        ProofFailure::new(
            ProofStage::Publication,
            format!("failed to capture Reader method profile: {error}"),
        )
    })?;
    let player = chess_method_profile(ChessProfile::Player).map_err(|error| {
        ProofFailure::new(
            ProofStage::Publication,
            format!("failed to capture Player method profile: {error}"),
        )
    })?;

    let profiles = [("reader", reader), ("player", player)];
    let mut profile_builders = policy.reborrow().init_profiles(profiles.len() as u32);
    for (index, (name, profile)) in profiles.iter().enumerate() {
        let methods = profile.method_keys();
        let mut builder = profile_builders.reborrow().get(index as u32);
        builder.set_name(name);
        let mut method_builders = builder.reborrow().init_methods(methods.len() as u32);
        for (method_index, method) in methods.into_iter().enumerate() {
            let mut method_builder = method_builders.reborrow().get(method_index as u32);
            method_builder.set_interface_id(method.interface_id);
            method_builder.set_ordinal(method.method_id);
        }
    }

    let recipients = [
        (reader_key.verifying_key().to_bytes(), "reader"),
        (player_key.verifying_key().to_bytes(), "player"),
    ];
    let mut recipient_builders = policy.init_recipients(recipients.len() as u32);
    for (index, (key, profile)) in recipients.iter().enumerate() {
        let mut builder = recipient_builders.reborrow().get(index as u32);
        builder.set_verifying_key(key);
        builder.set_profile(profile);
    }
    Ok(())
}

async fn login_opaque(
    terminal: &TerminalClient,
    key: &SigningKey,
    stage: ProofStage,
) -> Result<(auth_capnp::LoginStatus, Option<ChessClient>), ProofFailure> {
    let signer: auth_capnp::signer::Client = capnp_rpc::new_client(
        TestSigner::from_ed25519(key)
            .map_err(|error| ProofFailure::new(stage, error.to_string()))?,
    );
    let mut request = terminal.login_request();
    request.get().set_signer(signer);
    let response = timeout(stage, "Terminal login", request.send().promise)
        .await?
        .map_err(|error| ProofFailure::new(stage, format!("Terminal login failed: {error}")))?;
    let result = response
        .get()
        .map_err(|error| ProofFailure::new(stage, format!("invalid login response: {error}")))?;
    let status = result
        .get_status()
        .map_err(|error| ProofFailure::new(stage, format!("unknown login status: {error}")))?;
    let session = if result.has_session() {
        let opaque = result
            .get_session()
            .map_err(|error| ProofFailure::new(stage, format!("invalid login session: {error}")))?;
        Some(FromClientHook::new(opaque.client.hook))
    } else {
        None
    };
    Ok((status, session))
}

async fn get_state(client: &ChessClient, stage: ProofStage) -> Result<String, ProofFailure> {
    let response = timeout(
        stage,
        "getState RPC",
        client.get_state_request().send().promise,
    )
    .await?
    .map_err(|error| ProofFailure::new(stage, format!("getState failed: {error}")))?;
    let result = response
        .get()
        .map_err(|error| ProofFailure::new(stage, format!("invalid getState response: {error}")))?;
    result
        .get_fen()
        .map_err(|error| ProofFailure::new(stage, format!("missing FEN: {error}")))?
        .to_str()
        .map(str::to_string)
        .map_err(|error| ProofFailure::new(stage, format!("invalid FEN text: {error}")))
}

async fn apply_move(
    client: &ChessClient,
    chess_move: &str,
    stage: ProofStage,
) -> Result<(), ProofFailure> {
    let mut request = client.apply_move_request();
    request.get().set_uci(chess_move);
    let response = timeout(stage, "applyMove RPC", request.send().promise)
        .await?
        .map_err(|error| ProofFailure::new(stage, format!("applyMove failed: {error}")))?;
    let result = response.get().map_err(|error| {
        ProofFailure::new(stage, format!("invalid applyMove response: {error}"))
    })?;
    if result.get_ok() {
        Ok(())
    } else {
        let reason = result
            .get_reason()
            .map_err(|error| ProofFailure::new(stage, format!("missing move reason: {error}")))?
            .to_str()
            .map_err(|error| ProofFailure::new(stage, format!("invalid move reason: {error}")))?;
        Err(ProofFailure::new(stage, format!("move rejected: {reason}")))
    }
}

fn call_failure_code_from_proof(failure: &ProofFailure) -> Option<CallFailureCode> {
    call_failure_code(&capnp::Error::failed(failure.diagnostic.clone()))
}

fn start_libp2p_host(
    seed: u8,
) -> Result<
    (
        PeerId,
        ww::rpc::NetworkState,
        libp2p_stream::Control,
        mpsc::Sender<ww::rpc::SwarmCommand>,
        HostTask,
    ),
    ProofFailure,
> {
    let signing_key = SigningKey::from_bytes(&[seed; 32]);
    let keypair = ww::keys::to_libp2p(&signing_key).map_err(|error| {
        ProofFailure::new(
            ProofStage::Setup,
            format!("host key conversion failed: {error}"),
        )
    })?;
    let listen_addr = "/ip4/127.0.0.1/tcp/0"
        .parse()
        .map_err(|error| ProofFailure::new(ProofStage::Setup, format!("{error}")))?;
    let host = ww::host::Libp2pHost::new(vec![listen_addr], keypair, None, Vec::new()).map_err(
        |error| ProofFailure::new(ProofStage::Setup, format!("host setup failed: {error}")),
    )?;
    let peer_id = host.local_peer_id();
    let stream_control = host.stream_control();
    let network_state = ww::rpc::NetworkState::from_peer_id(peer_id.to_bytes());
    let host_state = network_state.clone();
    let (swarm_tx, swarm_rx) = mpsc::channel(4);
    let task = tokio::task::spawn_local(async move { host.run(host_state, swarm_rx).await });
    Ok((peer_id, network_state, stream_control, swarm_tx, task))
}

async fn connect_hosts(
    client_commands: &mpsc::Sender<ww::rpc::SwarmCommand>,
    server_peer: PeerId,
    server_state: &ww::rpc::NetworkState,
) -> Result<(), ProofFailure> {
    let raw_addr = timeout(
        ProofStage::Setup,
        "server listen address lookup",
        server_state.wait_for_listen_addr(),
    )
    .await?;
    let server_addr = libp2p_core::Multiaddr::try_from(raw_addr).map_err(|error| {
        ProofFailure::new(
            ProofStage::Setup,
            format!("server listen address did not decode: {error}"),
        )
    })?;
    let (reply_tx, reply_rx) = oneshot::channel();
    client_commands
        .send(ww::rpc::SwarmCommand::Connect {
            peer_id: server_peer,
            addrs: vec![server_addr],
            reply: reply_tx,
        })
        .await
        .map_err(|error| {
            ProofFailure::new(
                ProofStage::Setup,
                format!("client command channel closed: {error}"),
            )
        })?;
    timeout(ProofStage::Setup, "direct host connection", reply_rx)
        .await?
        .map_err(|error| {
            ProofFailure::new(
                ProofStage::Setup,
                format!("host connection reply dropped: {error}"),
            )
        })?
        .map_err(|error| {
            ProofFailure::new(
                ProofStage::Setup,
                format!("direct host connection failed: {error}"),
            )
        })
}

async fn timeout<T>(
    stage: ProofStage,
    operation: &str,
    future: impl std::future::Future<Output = T>,
) -> Result<T, ProofFailure> {
    tokio::time::timeout(OPERATION_DEADLINE, future)
        .await
        .map_err(|_| ProofFailure::new(stage, format!("{operation} timed out")))
}

async fn await_aborted_host(
    task: Option<HostTask>,
    name: &str,
    diagnostics: &mut Vec<String>,
) -> bool {
    let Some(task) = task else {
        return false;
    };
    task.abort();
    match tokio::time::timeout(CLEANUP_DEADLINE, task).await {
        Ok(Err(error)) if error.is_cancelled() => true,
        Ok(Ok(Ok(()))) => true,
        Ok(Ok(Err(error))) => {
            diagnostics.push(format!("{name} host failed during cleanup: {error}"));
            true
        }
        Ok(Err(error)) => {
            diagnostics.push(format!("{name} host task join failed: {error}"));
            true
        }
        Err(_) => {
            diagnostics.push(format!("timed out awaiting {name} host task"));
            false
        }
    }
}

fn epoch(seq: u64) -> Epoch {
    Epoch {
        seq,
        head: format!("head-{seq}").into_bytes(),
        provenance: Provenance::Block(seq),
    }
}

fn public_fingerprint(seed: u8) -> String {
    let key = SigningKey::from_bytes(&[seed; 32]);
    let encoded = hex::encode(key.verifying_key().to_bytes());
    format!("{}…{}", &encoded[..12], &encoded[encoded.len() - 12..])
}
