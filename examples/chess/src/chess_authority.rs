//! Deployer-side authorization policy for the Chess authority proof.
//!
//! The Chess guest exports one bare `ChessEngine`. This policy is attached by
//! trusted serving configuration and constructs a fresh authority boundary for
//! each successful Terminal login. Fresh authority does not mean fresh game
//! state: every issued client delegates to the same template `ChessEngine`.

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

use authority::{
    AuthPolicy, AuthenticatedIdentity, AuthorizationError, Epoch, EpochGuard, LocalPolicyFuture,
    SessionGrant, SessionTemplate,
};
use membrane::{GuardedPolicy, MethodCaptureError, MethodProfile, Policy, RevocationGuard};
use tokio::sync::watch;

use crate::chess_capnp;

/// Named method-level authority profiles supported by the Chess proof.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ChessProfile {
    Reader,
    Player,
}

#[derive(Clone)]
struct ProfileBinding {
    profile: ChessProfile,
    revocation: Rc<RevocationGuard>,
}

/// Map verified Terminal signing keys to Chess method authority.
///
/// The map is deliberately keyed by the verified per-login credential, not by
/// the libp2p peer ID. Multiple principals may therefore authenticate through
/// one transport node and receive independent authority boundaries.
#[derive(Clone)]
pub struct ChessAuthorization {
    bindings: Rc<RefCell<HashMap<[u8; 32], ProfileBinding>>>,
    epoch_rx: watch::Receiver<Epoch>,
}

impl ChessAuthorization {
    pub fn new(epoch_rx: watch::Receiver<Epoch>) -> Self {
        Self {
            bindings: Rc::new(RefCell::new(HashMap::new())),
            epoch_rx,
        }
    }

    pub fn with_profiles(
        epoch_rx: watch::Receiver<Epoch>,
        profiles: impl IntoIterator<Item = ([u8; 32], ChessProfile)>,
    ) -> Self {
        let policy = Self::new(epoch_rx);
        for (key, profile) in profiles {
            policy.set_profile(key, profile);
        }
        policy
    }

    /// Grant or replace one key's profile.
    ///
    /// Replacing a binding revokes its previous guard first, so already-issued
    /// sessions cannot retain authority that the new profile removed.
    pub fn set_profile(&self, key: [u8; 32], profile: ChessProfile) {
        let binding = ProfileBinding {
            profile,
            revocation: RevocationGuard::new(),
        };
        if let Some(previous) = self.bindings.borrow_mut().insert(key, binding) {
            previous.revocation.revoke();
        }
    }

    /// Revoke one principal without advancing the global Atom epoch.
    ///
    /// Returns whether the key had an active binding. Removing the binding
    /// denies new logins; flipping its shared guard also disables every
    /// capability already issued from that binding.
    pub fn revoke(&self, key: &[u8; 32]) -> bool {
        let previous = self.bindings.borrow_mut().remove(key);
        if let Some(previous) = previous {
            previous.revocation.revoke();
            true
        } else {
            false
        }
    }

    pub fn profile(&self, key: &[u8; 32]) -> Option<ChessProfile> {
        self.bindings
            .borrow()
            .get(key)
            .map(|binding| binding.profile)
    }
}

fn method_policy(profile: ChessProfile) -> Result<Box<dyn Policy>, MethodCaptureError> {
    let reader = MethodProfile::<chess_capnp::chess_engine::Client>::new()
        .allow_method(chess_capnp::chess_engine::Client::get_state_request)?;

    let policy = match profile {
        ChessProfile::Reader => reader.build(),
        ChessProfile::Player => reader
            .allow_method(chess_capnp::chess_engine::Client::apply_move_request)?
            .build(),
    };
    Ok(Box::new(policy))
}

impl AuthPolicy<chess_capnp::chess_engine::Owned> for ChessAuthorization {
    fn authorize<'a>(
        &'a self,
        identity: AuthenticatedIdentity,
        template: SessionTemplate<chess_capnp::chess_engine::Owned>,
    ) -> LocalPolicyFuture<
        'a,
        Result<SessionGrant<chess_capnp::chess_engine::Owned>, AuthorizationError>,
    > {
        let key = identity.verifying_key_bytes();
        let binding = self.bindings.borrow().get(&key).cloned();
        let epoch_rx = self.epoch_rx.clone();

        Box::pin(async move {
            let binding = binding.ok_or_else(|| {
                AuthorizationError::Denied("signing key has no Chess authority profile".into())
            })?;
            let method_policy = method_policy(binding.profile).map_err(|error| {
                AuthorizationError::Internal(capnp::Error::failed(format!(
                    "invalid trusted Chess method profile: {error}"
                )))
            })?;
            let issued_seq = epoch_rx.borrow().seq;
            let guarded = GuardedPolicy::new(method_policy)
                .with_guard(Rc::new(EpochGuard {
                    issued_seq,
                    receiver: epoch_rx,
                }))
                .with_guard(binding.revocation);
            let session = membrane::membrane(template.into_session(), Rc::new(guarded));
            Ok(SessionGrant::new(session))
        })
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use auth::SigningDomain;
    use authority::{auth_capnp, Provenance, TerminalServer};
    use capnp::capability::Promise;
    use capnp_rpc::rpc_twoparty_capnp::Side;
    use capnp_rpc::twoparty::VatNetwork;
    use capnp_rpc::RpcSystem;
    use ed25519_dalek::SigningKey;
    use membrane::{call_failure_code, CallFailureCode};
    use tokio::io;
    use tokio_util::compat::{TokioAsyncReadCompatExt, TokioAsyncWriteCompatExt};

    use super::*;
    use crate::ChessEngineImpl;

    const OPERATION_DEADLINE: Duration = Duration::from_secs(2);

    struct TestSigner {
        keypair: libp2p_identity::Keypair,
    }

    impl TestSigner {
        fn from_ed25519(key: &SigningKey) -> Self {
            let keypair =
                libp2p_identity::ed25519::Keypair::try_from_bytes(&mut key.to_keypair_bytes())
                    .expect("valid signing key");
            Self {
                keypair: keypair.into(),
            }
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

    fn epoch(seq: u64) -> Epoch {
        Epoch {
            seq,
            head: format!("head-{seq}").into_bytes(),
            provenance: Provenance::Block(seq),
        }
    }

    fn connect_terminal(
        terminal: auth_capnp::terminal::Client<chess_capnp::chess_engine::Owned>,
    ) -> auth_capnp::terminal::Client<chess_capnp::chess_engine::Owned> {
        let (client_stream, server_stream) = io::duplex(16 * 1024);
        let (client_read, client_write) = io::split(client_stream);
        let (server_read, server_write) = io::split(server_stream);

        let server_network = VatNetwork::new(
            server_read.compat(),
            server_write.compat_write(),
            Side::Server,
            Default::default(),
        );
        let server_rpc = RpcSystem::new(Box::new(server_network), Some(terminal.client));
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
        let remote = client_rpc.bootstrap(Side::Server);
        tokio::task::spawn_local(async move {
            let _ = client_rpc.await;
        });
        remote
    }

    async fn login(
        terminal: &auth_capnp::terminal::Client<chess_capnp::chess_engine::Owned>,
        key: &SigningKey,
    ) -> (
        auth_capnp::LoginStatus,
        Option<chess_capnp::chess_engine::Client>,
    ) {
        let signer: auth_capnp::signer::Client =
            capnp_rpc::new_client(TestSigner::from_ed25519(key));
        let mut request = terminal.login_request();
        request.get().set_signer(signer);
        let response = tokio::time::timeout(OPERATION_DEADLINE, request.send().promise)
            .await
            .expect("Terminal login timed out")
            .expect("Terminal login transport");
        let result = response.get().expect("Terminal login result");
        let status = result.get_status().expect("known login status");
        let session = result.has_session().then(|| {
            result
                .get_session()
                .expect("granted login has Chess session")
        });
        (status, session)
    }

    async fn get_state(client: &chess_capnp::chess_engine::Client) -> Result<String, capnp::Error> {
        let response = tokio::time::timeout(
            OPERATION_DEADLINE,
            client.get_state_request().send().promise,
        )
        .await
        .map_err(|_| capnp::Error::failed("chess proof getState timed out".into()))??;
        Ok(response
            .get()?
            .get_fen()?
            .to_str()
            .map_err(|error| capnp::Error::failed(error.to_string()))?
            .to_string())
    }

    async fn apply_move(
        client: &chess_capnp::chess_engine::Client,
        chess_move: &str,
    ) -> Result<(), capnp::Error> {
        let mut request = client.apply_move_request();
        request.get().set_uci(chess_move);
        let response = tokio::time::timeout(OPERATION_DEADLINE, request.send().promise)
            .await
            .map_err(|_| capnp::Error::failed("chess proof applyMove timed out".into()))??;
        let result = response.get()?;
        if result.get_ok() {
            Ok(())
        } else {
            Err(capnp::Error::failed(
                result
                    .get_reason()?
                    .to_str()
                    .map_err(|error| capnp::Error::failed(error.to_string()))?
                    .to_string(),
            ))
        }
    }

    #[tokio::test]
    async fn terminal_issues_distinct_authority_over_one_shared_game() {
        let local = tokio::task::LocalSet::new();
        local
            .run_until(async {
                let reader_key = SigningKey::from_bytes(&[1; 32]);
                let player_key = SigningKey::from_bytes(&[2; 32]);
                let replacement_key = SigningKey::from_bytes(&[3; 32]);
                let unknown_key = SigningKey::from_bytes(&[4; 32]);
                let (epoch_tx, epoch_rx) = watch::channel(epoch(1));

                let policy = ChessAuthorization::with_profiles(
                    epoch_rx.clone(),
                    [
                        (reader_key.verifying_key().to_bytes(), ChessProfile::Reader),
                        (player_key.verifying_key().to_bytes(), ChessProfile::Player),
                    ],
                );
                let policy_handle = policy.clone();
                let shared_game: chess_capnp::chess_engine::Client =
                    capnp_rpc::new_client(ChessEngineImpl::new());
                let terminal: auth_capnp::terminal::Client<chess_capnp::chess_engine::Owned> =
                    capnp_rpc::new_client(TerminalServer::with_policy(
                        Box::new(policy),
                        shared_game,
                        SigningDomain::terminal_membrane(),
                        epoch_rx,
                    ));
                let remote = connect_terminal(terminal);

                let (reader_status, reader) = login(&remote, &reader_key).await;
                let (player_status, player) = login(&remote, &player_key).await;
                let (unknown_status, unknown) = login(&remote, &unknown_key).await;
                assert_eq!(reader_status, auth_capnp::LoginStatus::Granted);
                assert_eq!(player_status, auth_capnp::LoginStatus::Granted);
                assert_eq!(unknown_status, auth_capnp::LoginStatus::Denied);
                assert!(unknown.is_none());
                let reader = reader.expect("Reader session");
                let player = player.expect("Player session");

                get_state(&reader).await.expect("Reader may observe");
                let denied = apply_move(&reader, "e2e4")
                    .await
                    .expect_err("Reader must not move");
                assert_eq!(
                    call_failure_code(&denied),
                    Some(CallFailureCode::PermissionDenied)
                );

                apply_move(&player, "e2e4").await.expect("Player may move");
                assert!(
                    get_state(&reader)
                        .await
                        .expect("Reader observes shared game")
                        .contains("4P3"),
                    "Reader and Player must reference the same ChessEngine state"
                );

                assert!(policy_handle.revoke(&reader_key.verifying_key().to_bytes()));
                let revoked = get_state(&reader)
                    .await
                    .expect_err("existing Reader session must be revoked");
                assert_eq!(
                    call_failure_code(&revoked),
                    Some(CallFailureCode::TargetRevoked)
                );
                get_state(&player)
                    .await
                    .expect("revoking Reader must not affect Player");
                let (revoked_login, revoked_session) = login(&remote, &reader_key).await;
                assert_eq!(revoked_login, auth_capnp::LoginStatus::Denied);
                assert!(revoked_session.is_none());

                epoch_tx.send(epoch(2)).expect("advance epoch");
                let stale = get_state(&player)
                    .await
                    .expect_err("established Player session must expire");
                assert_eq!(call_failure_code(&stale), Some(CallFailureCode::StaleEpoch));

                policy_handle.set_profile(
                    replacement_key.verifying_key().to_bytes(),
                    ChessProfile::Reader,
                );
                let (replacement_status, replacement) = login(&remote, &replacement_key).await;
                assert_eq!(replacement_status, auth_capnp::LoginStatus::Granted);
                get_state(&replacement.expect("replacement Reader session"))
                    .await
                    .expect("fresh session under new epoch works");
            })
            .await;
    }
}
