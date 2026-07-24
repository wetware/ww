//! Terminal server: challenge-response auth gate for capability access.
//!
//! `Terminal(Session)` is the authentication boundary. The caller must prove
//! identity by signing a nonce with the expected key. On success, the guarded
//! session capability is returned.
//!
//! This separates authentication (Terminal) from capability provisioning
//! (Membrane). Having a Membrane reference IS authorization (ocap); Terminal
//! is the gate that decides who gets that reference.

use crate::auth_capnp;
use crate::epoch::Epoch;
use auth::SigningDomain;
use capnp::capability::Promise;
use capnp::Error;
#[cfg(test)]
use capnp_rpc::pry;
use ed25519_dalek::VerifyingKey;
use libp2p_core::SignedEnvelope;
use std::cell::RefCell;
use std::future::Future;
use std::pin::Pin;
use std::time::Duration;
use tokio::sync::{oneshot, watch};

/// Default upper bound for authorization backend work during a login.
pub const DEFAULT_POLICY_TIMEOUT: Duration = Duration::from_secs(5);

/// An identity whose challenge proof has already been verified by Terminal.
#[derive(Clone, Debug)]
pub struct AuthenticatedIdentity {
    verifying_key: VerifyingKey,
}

impl AuthenticatedIdentity {
    pub fn verifying_key(&self) -> &VerifyingKey {
        &self.verifying_key
    }

    pub fn verifying_key_bytes(&self) -> [u8; 32] {
        self.verifying_key.to_bytes()
    }
}

/// The deployer-provided capability from which a policy constructs a login session.
///
/// Cloning the template clones only the capability reference. A policy that needs an
/// isolated application instance must construct one explicitly.
pub struct SessionTemplate<Session>
where
    Session: capnp::traits::Owned,
{
    session: <Session as capnp::traits::Owned>::Reader<'static>,
}

impl<Session> Clone for SessionTemplate<Session>
where
    Session: capnp::traits::Owned + 'static,
    <Session as capnp::traits::Owned>::Reader<'static>: Clone,
{
    fn clone(&self) -> Self {
        Self {
            session: self.session.clone(),
        }
    }
}

impl<Session> SessionTemplate<Session>
where
    Session: capnp::traits::Owned,
{
    pub fn new(session: <Session as capnp::traits::Owned>::Reader<'static>) -> Self {
        Self { session }
    }

    pub fn into_session(self) -> <Session as capnp::traits::Owned>::Reader<'static> {
        self.session
    }
}

/// A complete, owned authorization decision ready for one synchronous commit.
///
/// Policies must finish all I/O and capability construction before returning a grant.
/// Consuming `self` in [`SessionGrant::commit`] makes a grant one-shot and keeps policy
/// code from holding a mutable Cap'n Proto response across an await point.
pub struct SessionGrant<Session>
where
    Session: capnp::traits::Owned,
{
    session: <Session as capnp::traits::Owned>::Reader<'static>,
}

impl<Session> SessionGrant<Session>
where
    Session: capnp::traits::Owned,
{
    pub fn new(session: <Session as capnp::traits::Owned>::Reader<'static>) -> Self {
        Self { session }
    }

    pub fn from_template(template: SessionTemplate<Session>) -> Self {
        Self::new(template.into_session())
    }

    fn commit(self, results: &mut auth_capnp::terminal::LoginResults<Session>) -> Result<(), Error>
    where
        <Session as capnp::traits::Owned>::Reader<'static>: capnp::traits::SetterInput<Session>,
    {
        let mut builder = results.get();
        builder.set_status(auth_capnp::LoginStatus::Granted);
        builder.set_detail("");
        // Install the capability last. If this fallible operation fails, login
        // returns an RPC exception rather than exposing a successful result.
        builder.set_session(self.session)
    }
}

/// Expected authorization outcomes. Internal bugs remain RPC exceptions.
#[derive(Debug)]
pub enum AuthorizationError {
    Denied(String),
    BackendUnavailable(String),
    Overloaded(String),
    Internal(Error),
}

impl AuthorizationError {
    fn into_login_outcome(self) -> Result<(auth_capnp::LoginStatus, String), Error> {
        match self {
            Self::Denied(detail) => Ok((auth_capnp::LoginStatus::Denied, detail)),
            Self::BackendUnavailable(detail) => {
                Ok((auth_capnp::LoginStatus::BackendUnavailable, detail))
            }
            Self::Overloaded(detail) => Ok((auth_capnp::LoginStatus::Overloaded, detail)),
            Self::Internal(error) => Err(error),
        }
    }
}

/// A local, possibly `!Send` authorization future.
pub type LocalPolicyFuture<'a, T> = Pin<Box<dyn Future<Output = T> + 'a>>;

/// Policy for constructing the authority session returned by Terminal.
///
/// Called only after signature, domain, nonce, and challenge-epoch verification.
/// The policy may perform asynchronous backend I/O and must return a completed,
/// owned grant. It never receives the mutable login response.
pub trait AuthPolicy<Session>: 'static
where
    Session: capnp::traits::Owned,
{
    fn authorize<'a>(
        &'a self,
        identity: AuthenticatedIdentity,
        template: SessionTemplate<Session>,
    ) -> LocalPolicyFuture<'a, Result<SessionGrant<Session>, AuthorizationError>>;
}

/// Compatibility policy that returns the fixed template for one verifying key.
pub struct FixedSessionPolicy {
    expected: VerifyingKey,
}

impl FixedSessionPolicy {
    pub fn new(expected: VerifyingKey) -> Self {
        Self { expected }
    }
}

impl<Session> AuthPolicy<Session> for FixedSessionPolicy
where
    Session: capnp::traits::Owned + 'static,
{
    fn authorize<'a>(
        &'a self,
        identity: AuthenticatedIdentity,
        template: SessionTemplate<Session>,
    ) -> LocalPolicyFuture<'a, Result<SessionGrant<Session>, AuthorizationError>> {
        Box::pin(async move {
            if identity.verifying_key_bytes() != self.expected.to_bytes() {
                return Err(AuthorizationError::Denied(
                    "signing key is not authorized".into(),
                ));
            }
            Ok(SessionGrant::from_template(template))
        })
    }
}

/// Explicit compatibility policy that returns the fixed template to any valid signer.
pub struct AllowAllPolicy;

impl<Session> AuthPolicy<Session> for AllowAllPolicy
where
    Session: capnp::traits::Owned + 'static,
{
    fn authorize<'a>(
        &'a self,
        identity: AuthenticatedIdentity,
        template: SessionTemplate<Session>,
    ) -> LocalPolicyFuture<'a, Result<SessionGrant<Session>, AuthorizationError>> {
        Box::pin(async move {
            tracing::info!(
                peer_key = hex::encode(identity.verifying_key_bytes()),
                "access granted (explicit allow-all policy)"
            );
            Ok(SessionGrant::from_template(template))
        })
    }
}

/// Convert a libp2p ed25519 public key to an ed25519-dalek VerifyingKey.
fn to_verifying_key(pk: libp2p_identity::ed25519::PublicKey) -> Result<VerifyingKey, capnp::Error> {
    VerifyingKey::from_bytes(&pk.to_bytes())
        .map_err(|_| capnp::Error::failed("login auth failed: invalid ed25519 key bytes".into()))
}

/// Authentication gate that guards access to a capability via challenge-response.
///
/// Generic over the session type — typically `Terminal<membrane::Owned>`, but
/// can wrap any Cap'n Proto capability interface.
///
/// # Auth flow
///
/// 1. Caller sends `login(signer)` request
/// 2. Terminal reads the current epoch and generates a random nonce
/// 3. Terminal sends `signer.sign(nonce, epoch_seq)` — both values are
///    bound into the signed payload (`nonce || epoch_seq`, 16 bytes)
/// 4. Signer returns a libp2p `SignedEnvelope` (RFC 0002)
/// 5. Terminal decodes the envelope, verifies the signature + domain +
///    challenge payload, and checks the signing key against the auth policy
/// 6. Terminal verifies the epoch hasn't advanced since step 2
/// 7. On success, returns the guarded `session` capability
///
/// # Security properties
///
/// The signed challenge binds two independent values:
///
/// - **Random nonce** — prevents replay within the same epoch.  A fresh
///   `u64` is drawn from the OS CSPRNG for every login attempt.
/// - **Epoch sequence** — prevents cross-epoch reuse.  A signature captured
///   during epoch N is cryptographic garbage during epoch N+1 because the
///   epoch_seq in the payload won't match the Terminal's current epoch.
///
/// Together they provide defence-in-depth: the nonce stops same-epoch replay,
/// the epoch binding stops stale-epoch replay, and the EpochGuard on issued
/// capabilities provides a final runtime check at capability-use time.
pub struct TerminalServer<Session: capnp::traits::Owned> {
    policy: Box<dyn AuthPolicy<Session>>,
    template: SessionTemplate<Session>,
    domain: SigningDomain,
    epoch_rx: watch::Receiver<Epoch>,
    policy_timeout: Duration,
    grant_notifier: RefCell<Option<oneshot::Sender<()>>>,
}

impl<Session> TerminalServer<Session>
where
    Session: capnp::traits::Owned + 'static,
    <Session as capnp::traits::Owned>::Reader<'static>: Clone,
{
    /// Create a new Terminal guarding the given session capability.
    ///
    /// Uses [`FixedSessionPolicy`] — only the given key is accepted and each
    /// successful login receives a fresh grant containing the fixed template.
    ///
    /// The `domain` determines the signing context for challenge-response auth.
    /// Different guarded capabilities should use different domains to prevent
    /// cross-protocol signature replay.
    ///
    /// The `epoch_rx` provides the current epoch for binding into the
    /// challenge-response — signatures are tied to the epoch they were issued in.
    pub fn new(
        vk: VerifyingKey,
        session: <Session as capnp::traits::Owned>::Reader<'static>,
        domain: SigningDomain,
        epoch_rx: watch::Receiver<Epoch>,
    ) -> Self {
        Self::with_policy(
            Box::new(FixedSessionPolicy::new(vk)),
            session,
            domain,
            epoch_rx,
        )
    }

    /// Create a new Terminal with a custom auth policy.
    pub fn with_policy(
        policy: Box<dyn AuthPolicy<Session>>,
        session: <Session as capnp::traits::Owned>::Reader<'static>,
        domain: SigningDomain,
        epoch_rx: watch::Receiver<Epoch>,
    ) -> Self {
        Self::with_policy_timeout(policy, session, domain, epoch_rx, DEFAULT_POLICY_TIMEOUT)
    }

    /// Create a Terminal with a custom policy and authorization deadline.
    pub fn with_policy_timeout(
        policy: Box<dyn AuthPolicy<Session>>,
        session: <Session as capnp::traits::Owned>::Reader<'static>,
        domain: SigningDomain,
        epoch_rx: watch::Receiver<Epoch>,
        policy_timeout: Duration,
    ) -> Self {
        Self {
            policy,
            template: SessionTemplate::new(session),
            domain,
            epoch_rx,
            policy_timeout,
            grant_notifier: RefCell::new(None),
        }
    }

    /// Notify a connection supervisor after the first successfully committed login.
    pub fn with_grant_notifier(self, notifier: oneshot::Sender<()>) -> Self {
        *self.grant_notifier.borrow_mut() = Some(notifier);
        self
    }
}

fn set_login_outcome<Session>(
    results: &mut auth_capnp::terminal::LoginResults<Session>,
    status: auth_capnp::LoginStatus,
    detail: &str,
) where
    Session: capnp::traits::Owned,
{
    let mut builder = results.get();
    builder.set_status(status);
    builder.set_detail(detail);
}

#[allow(refining_impl_trait)]
impl<Session> auth_capnp::terminal::Server<Session> for TerminalServer<Session>
where
    Session: capnp::traits::Owned + 'static,
    <Session as capnp::traits::Owned>::Reader<'static>: capnp::traits::SetterInput<Session> + Clone,
{
    fn login(
        self: capnp::capability::Rc<Self>,
        params: auth_capnp::terminal::LoginParams<Session>,
        mut results: auth_capnp::terminal::LoginResults<Session>,
    ) -> Promise<(), Error> {
        let signer: auth_capnp::signer::Client = match params.get().and_then(|p| p.get_signer()) {
            Ok(signer) => signer,
            Err(_) => {
                set_login_outcome(
                    &mut results,
                    auth_capnp::LoginStatus::InvalidRequest,
                    "missing signer",
                );
                return Promise::ok(());
            }
        };

        let template = self.template.clone();
        let domain = self.domain.clone();

        // Read current epoch — the seq is bound into the signed challenge
        // so that a captured signature from epoch N is garbage during epoch N+1.
        let epoch = self.epoch_rx.borrow().clone();
        let epoch_seq = epoch.seq;

        let nonce: u64 = rand::random();
        let mut sign_req = signer.sign_request();
        sign_req.get().set_nonce(nonce);
        sign_req.get().set_epoch_seq(epoch_seq);

        Promise::from_future(async move {
            let sign_resp = sign_req.send().promise.await?;
            let sig_bytes = sign_resp.get()?.get_sig()?;

            // Decode the libp2p SignedEnvelope (RFC 0002).
            let envelope = match SignedEnvelope::from_protobuf_encoding(sig_bytes) {
                Ok(envelope) => envelope,
                Err(error) => {
                    set_login_outcome(
                        &mut results,
                        auth_capnp::LoginStatus::InvalidProof,
                        &format!("invalid signed envelope: {error}"),
                    );
                    return Ok(());
                }
            };

            // Verify signature and extract payload + signing key.
            // This checks domain separation and payload type in one step.
            let (payload, pubkey) = match envelope
                .payload_and_signing_key(domain.as_str().to_string(), domain.payload_type())
            {
                Ok(verified) => verified,
                Err(error) => {
                    set_login_outcome(
                        &mut results,
                        auth_capnp::LoginStatus::InvalidProof,
                        &format!("signature verification failed: {error}"),
                    );
                    return Ok(());
                }
            };

            // Check the nonce || epoch_seq matches our challenge.
            let mut expected_payload = Vec::with_capacity(16);
            expected_payload.extend_from_slice(&nonce.to_be_bytes());
            expected_payload.extend_from_slice(&epoch_seq.to_be_bytes());
            if payload != expected_payload {
                set_login_outcome(
                    &mut results,
                    auth_capnp::LoginStatus::InvalidProof,
                    "challenge mismatch",
                );
                return Ok(());
            }

            // Extract the ed25519 key and delegate authorization to the policy.
            let envelope_ed = match pubkey.clone().try_into_ed25519() {
                Ok(key) => key,
                Err(_) => {
                    set_login_outcome(
                        &mut results,
                        auth_capnp::LoginStatus::InvalidProof,
                        "signing key is not Ed25519",
                    );
                    return Ok(());
                }
            };
            let verifying_key = match to_verifying_key(envelope_ed) {
                Ok(key) => key,
                Err(error) => {
                    set_login_outcome(
                        &mut results,
                        auth_capnp::LoginStatus::InvalidProof,
                        &error.to_string(),
                    );
                    return Ok(());
                }
            };
            let identity = AuthenticatedIdentity { verifying_key };

            let grant = match tokio::time::timeout(
                self.policy_timeout,
                self.policy.authorize(identity, template),
            )
            .await
            {
                Ok(Ok(grant)) => grant,
                Ok(Err(error)) => {
                    let (status, detail) = error.into_login_outcome()?;
                    set_login_outcome(&mut results, status, &detail);
                    return Ok(());
                }
                Err(_) => {
                    set_login_outcome(
                        &mut results,
                        auth_capnp::LoginStatus::TimedOut,
                        "authorization policy timed out",
                    );
                    return Ok(());
                }
            };

            // Recheck after all asynchronous policy work and immediately before
            // the synchronous one-shot commit.
            if self.epoch_rx.borrow().seq != epoch_seq {
                set_login_outcome(
                    &mut results,
                    auth_capnp::LoginStatus::StaleEpoch,
                    "epoch advanced during authentication",
                );
                return Ok(());
            }

            grant.commit(&mut results)?;
            if let Some(notifier) = self.grant_notifier.borrow_mut().take() {
                let _ = notifier.send(());
            }
            Ok(())
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::membrane_capnp;
    use crate::test_session_capnp::{leaf, structured_session};
    use capnp::capability::Rc as CapRc;
    use ed25519_dalek::SigningKey;
    use std::cell::Cell;
    use std::rc::Rc;

    fn test_epoch(seq: u64) -> Epoch {
        Epoch {
            seq,
            head: vec![0xAB],
            provenance: crate::epoch::Provenance::Block(100),
        }
    }

    /// In-process test signer that produces valid libp2p SignedEnvelopes.
    struct TestSigner {
        keypair: libp2p_identity::Keypair,
    }

    impl TestSigner {
        fn from_ed25519(sk: &SigningKey) -> Self {
            let ed_kp =
                libp2p_identity::ed25519::Keypair::try_from_bytes(&mut sk.to_keypair_bytes())
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
        ) -> Promise<(), Error> {
            let p = pry!(params.get());
            let nonce = p.get_nonce();
            let epoch_seq = p.get_epoch_seq();
            let domain = SigningDomain::terminal_membrane();

            let mut payload = Vec::with_capacity(16);
            payload.extend_from_slice(&nonce.to_be_bytes());
            payload.extend_from_slice(&epoch_seq.to_be_bytes());

            let envelope = pry!(SignedEnvelope::new(
                &self.keypair,
                domain.as_str().to_string(),
                domain.payload_type().to_vec(),
                payload,
            )
            .map_err(|e| Error::failed(format!("signing failed: {e}"))));

            results.get().set_sig(&envelope.into_protobuf_encoding());
            Promise::ok(())
        }
    }

    /// Signer that ignores the epoch_seq from params and signs a hardcoded value.
    struct WrongEpochSigner {
        keypair: libp2p_identity::Keypair,
        forced_epoch_seq: u64,
    }

    #[allow(refining_impl_trait)]
    impl auth_capnp::signer::Server for WrongEpochSigner {
        fn sign(
            self: capnp::capability::Rc<Self>,
            params: auth_capnp::signer::SignParams,
            mut results: auth_capnp::signer::SignResults,
        ) -> Promise<(), Error> {
            let p = pry!(params.get());
            let nonce = p.get_nonce();
            let domain = SigningDomain::terminal_membrane();

            let mut payload = Vec::with_capacity(16);
            payload.extend_from_slice(&nonce.to_be_bytes());
            payload.extend_from_slice(&self.forced_epoch_seq.to_be_bytes());

            let envelope = pry!(SignedEnvelope::new(
                &self.keypair,
                domain.as_str().to_string(),
                domain.payload_type().to_vec(),
                payload,
            )
            .map_err(|e| Error::failed(format!("signing failed: {e}"))));

            results.get().set_sig(&envelope.into_protobuf_encoding());
            Promise::ok(())
        }
    }

    fn terminal_with_epoch(
        vk: VerifyingKey,
        epoch: Epoch,
    ) -> (
        auth_capnp::terminal::Client<membrane_capnp::membrane::Owned>,
        watch::Sender<Epoch>,
    ) {
        let (tx, rx) = watch::channel(epoch);
        let membrane: membrane_capnp::membrane::Client =
            crate::membrane::membrane_client(rx.clone());
        let terminal = TerminalServer::<membrane_capnp::membrane::Owned>::new(
            vk,
            membrane,
            SigningDomain::terminal_membrane(),
            rx,
        );
        (capnp_rpc::new_client(terminal), tx)
    }

    #[test]
    fn terminal_server_constructs_with_membrane_owned() {
        let sk = SigningKey::generate(&mut rand::rngs::OsRng);
        let vk = sk.verifying_key();

        let (_tx, rx) = watch::channel(test_epoch(1));
        let membrane: membrane_capnp::membrane::Client =
            crate::membrane::membrane_client(rx.clone());

        let _terminal = TerminalServer::<membrane_capnp::membrane::Owned>::new(
            vk,
            membrane,
            SigningDomain::terminal_membrane(),
            rx,
        );
    }

    #[test]
    fn terminal_server_constructs_with_custom_policy() {
        let (_tx, rx) = watch::channel(test_epoch(1));
        let membrane: membrane_capnp::membrane::Client =
            crate::membrane::membrane_client(rx.clone());

        let _terminal = TerminalServer::<membrane_capnp::membrane::Owned>::with_policy(
            Box::new(AllowAllPolicy),
            membrane,
            SigningDomain::terminal_membrane(),
            rx,
        );
    }

    /// Login succeeds when the signer returns the correct epoch_seq.
    #[tokio::test]
    async fn login_succeeds_with_matching_epoch() {
        let local = tokio::task::LocalSet::new();
        local
            .run_until(async {
                let sk = SigningKey::generate(&mut rand::rngs::OsRng);
                let vk = sk.verifying_key();
                let (terminal, _tx) = terminal_with_epoch(vk, test_epoch(1));

                let signer: auth_capnp::signer::Client =
                    capnp_rpc::new_client(TestSigner::from_ed25519(&sk));
                let mut req = terminal.login_request();
                req.get().set_signer(signer);

                let response = req
                    .send()
                    .promise
                    .await
                    .expect("login should succeed with matching epoch");
                let result = response.get().expect("login results");
                assert_eq!(
                    result.get_status().expect("known status"),
                    auth_capnp::LoginStatus::Granted
                );
                result.get_session().expect("granted session");
            })
            .await;
    }

    #[tokio::test]
    async fn grant_notifier_fires_only_after_successful_commit() {
        let local = tokio::task::LocalSet::new();
        local
            .run_until(async {
                let expected = SigningKey::generate(&mut rand::rngs::OsRng);
                let wrong = SigningKey::generate(&mut rand::rngs::OsRng);
                let (_epoch_tx, epoch_rx) = watch::channel(test_epoch(1));
                let membrane = crate::membrane::membrane_client(epoch_rx.clone());
                let (granted_tx, mut granted_rx) = oneshot::channel();
                let terminal = TerminalServer::new(
                    expected.verifying_key(),
                    membrane,
                    SigningDomain::terminal_membrane(),
                    epoch_rx,
                )
                .with_grant_notifier(granted_tx);
                let client: auth_capnp::terminal::Client<membrane_capnp::membrane::Owned> =
                    capnp_rpc::new_client(terminal);

                let wrong_signer: auth_capnp::signer::Client =
                    capnp_rpc::new_client(TestSigner::from_ed25519(&wrong));
                let mut denied = client.login_request();
                denied.get().set_signer(wrong_signer);
                let denied = denied.send().promise.await.expect("typed denial");
                assert_eq!(
                    denied
                        .get()
                        .expect("login results")
                        .get_status()
                        .expect("known status"),
                    auth_capnp::LoginStatus::Denied
                );
                assert!(matches!(
                    granted_rx.try_recv(),
                    Err(oneshot::error::TryRecvError::Empty)
                ));

                let expected_signer: auth_capnp::signer::Client =
                    capnp_rpc::new_client(TestSigner::from_ed25519(&expected));
                let mut granted = client.login_request();
                granted.get().set_signer(expected_signer);
                let granted = granted.send().promise.await.expect("granted login");
                assert_eq!(
                    granted
                        .get()
                        .expect("login results")
                        .get_status()
                        .expect("known status"),
                    auth_capnp::LoginStatus::Granted
                );
                granted_rx.await.expect("grant notification");
            })
            .await;
    }

    /// Login fails when the signer signs a different epoch_seq than the Terminal's current epoch.
    #[tokio::test]
    async fn login_fails_with_wrong_epoch_seq() {
        let local = tokio::task::LocalSet::new();
        local
            .run_until(async {
                let sk = SigningKey::generate(&mut rand::rngs::OsRng);
                let vk = sk.verifying_key();
                let (terminal, _tx) = terminal_with_epoch(vk, test_epoch(1));

                let ed_kp =
                    libp2p_identity::ed25519::Keypair::try_from_bytes(&mut sk.to_keypair_bytes())
                        .expect("valid key");
                let signer: auth_capnp::signer::Client = capnp_rpc::new_client(WrongEpochSigner {
                    keypair: ed_kp.into(),
                    forced_epoch_seq: 999, // wrong epoch
                });

                let mut req = terminal.login_request();
                req.get().set_signer(signer);

                let response = req.send().promise.await.expect("typed login outcome");
                let result = response.get().expect("login results");
                assert_eq!(
                    result.get_status().expect("known status"),
                    auth_capnp::LoginStatus::InvalidProof
                );
                assert!(
                    result.get_session().is_err(),
                    "invalid proof must not return a session"
                );
            })
            .await;
    }

    /// Signer that advances the epoch as a side-effect of signing.
    /// This simulates the race where the epoch changes between challenge
    /// issuance and response verification.
    struct EpochAdvancingSigner {
        keypair: libp2p_identity::Keypair,
        epoch_tx: watch::Sender<Epoch>,
    }

    #[allow(refining_impl_trait)]
    impl auth_capnp::signer::Server for EpochAdvancingSigner {
        fn sign(
            self: capnp::capability::Rc<Self>,
            params: auth_capnp::signer::SignParams,
            mut results: auth_capnp::signer::SignResults,
        ) -> Promise<(), Error> {
            let p = pry!(params.get());
            let nonce = p.get_nonce();
            let epoch_seq = p.get_epoch_seq();
            let domain = SigningDomain::terminal_membrane();

            // Sign correctly with the epoch_seq the Terminal sent.
            let mut payload = Vec::with_capacity(16);
            payload.extend_from_slice(&nonce.to_be_bytes());
            payload.extend_from_slice(&epoch_seq.to_be_bytes());

            let envelope = pry!(SignedEnvelope::new(
                &self.keypair,
                domain.as_str().to_string(),
                domain.payload_type().to_vec(),
                payload,
            )
            .map_err(|e| Error::failed(format!("signing failed: {e}"))));

            results.get().set_sig(&envelope.into_protobuf_encoding());

            // Advance the epoch AFTER signing but BEFORE the Terminal verifies.
            // The Terminal's post-sign check (`current_seq != epoch_seq`) catches this.
            self.epoch_tx.send(test_epoch(epoch_seq + 1)).ok();

            Promise::ok(())
        }
    }

    /// Login fails when the epoch advances between challenge issuance and response verification.
    #[tokio::test]
    async fn login_fails_when_epoch_advances_during_auth() {
        let local = tokio::task::LocalSet::new();
        local
            .run_until(async {
                let sk = SigningKey::generate(&mut rand::rngs::OsRng);
                let vk = sk.verifying_key();
                let (terminal, tx) = terminal_with_epoch(vk, test_epoch(1));

                let ed_kp =
                    libp2p_identity::ed25519::Keypair::try_from_bytes(&mut sk.to_keypair_bytes())
                        .expect("valid key");
                let signer: auth_capnp::signer::Client =
                    capnp_rpc::new_client(EpochAdvancingSigner {
                        keypair: ed_kp.into(),
                        epoch_tx: tx,
                    });

                let mut req = terminal.login_request();
                req.get().set_signer(signer);

                let response = req.send().promise.await.expect("typed login outcome");
                let result = response.get().expect("login results");
                assert_eq!(
                    result.get_status().expect("known status"),
                    auth_capnp::LoginStatus::StaleEpoch
                );
                assert!(
                    result.get_session().is_err(),
                    "stale login must not return a session"
                );
            })
            .await;
    }

    struct LeafServer(&'static str);

    impl leaf::Server for LeafServer {
        fn read(
            self: CapRc<Self>,
            _params: leaf::ReadParams,
            mut results: leaf::ReadResults,
        ) -> impl Future<Output = Result<(), Error>> + 'static {
            results.get().set_value(self.0);
            std::future::ready(Ok(()))
        }
    }

    struct StructuredSessionServer {
        first: Option<leaf::Client>,
        second: Option<leaf::Client>,
    }

    impl structured_session::Server for StructuredSessionServer {
        fn capabilities(
            self: CapRc<Self>,
            _params: structured_session::CapabilitiesParams,
            mut results: structured_session::CapabilitiesResults,
        ) -> impl Future<Output = Result<(), Error>> + 'static {
            let mut builder = results.get();
            if let Some(first) = &self.first {
                builder.set_first(first.clone());
            }
            if let Some(second) = &self.second {
                builder.set_second(second.clone());
            }
            std::future::ready(Ok(()))
        }
    }

    fn structured_session(
        first: &'static str,
        second: Option<&'static str>,
    ) -> structured_session::Client {
        capnp_rpc::new_client(StructuredSessionServer {
            first: Some(capnp_rpc::new_client(LeafServer(first))),
            second: second.map(|value| capnp_rpc::new_client(LeafServer(value))),
        })
    }

    fn structured_terminal(
        policy: Box<dyn AuthPolicy<structured_session::Owned>>,
        template: structured_session::Client,
        timeout: Duration,
    ) -> auth_capnp::terminal::Client<structured_session::Owned> {
        let (_tx, rx) = watch::channel(test_epoch(1));
        capnp_rpc::new_client(TerminalServer::with_policy_timeout(
            policy,
            template,
            SigningDomain::terminal_membrane(),
            rx,
            timeout,
        ))
    }

    async fn structured_login(
        terminal: &auth_capnp::terminal::Client<structured_session::Owned>,
        signing_key: &SigningKey,
    ) -> capnp::capability::Response<
        auth_capnp::terminal::login_results::Owned<structured_session::Owned>,
    > {
        let signer: auth_capnp::signer::Client =
            capnp_rpc::new_client(TestSigner::from_ed25519(signing_key));
        let mut request = terminal.login_request();
        request.get().set_signer(signer);
        request.send().promise.await.expect("login RPC")
    }

    struct ReplacementPolicy {
        replacement: structured_session::Client,
        called: Rc<Cell<bool>>,
    }

    impl AuthPolicy<structured_session::Owned> for ReplacementPolicy {
        fn authorize<'a>(
            &'a self,
            _identity: AuthenticatedIdentity,
            _template: SessionTemplate<structured_session::Owned>,
        ) -> LocalPolicyFuture<
            'a,
            Result<SessionGrant<structured_session::Owned>, AuthorizationError>,
        > {
            Box::pin(async move {
                tokio::task::yield_now().await;
                self.called.set(true);
                Ok(SessionGrant::new(self.replacement.clone()))
            })
        }
    }

    #[tokio::test]
    async fn async_local_policy_can_replace_and_withhold_structured_capabilities() {
        let local = tokio::task::LocalSet::new();
        local
            .run_until(async {
                let called = Rc::new(Cell::new(false));
                let terminal = structured_terminal(
                    Box::new(ReplacementPolicy {
                        replacement: structured_session("replacement", None),
                        called: Rc::clone(&called),
                    }),
                    structured_session("template-first", Some("template-second")),
                    Duration::from_secs(1),
                );
                let key = SigningKey::generate(&mut rand::rngs::OsRng);

                let login = structured_login(&terminal, &key).await;
                let result = login.get().expect("login results");
                assert_eq!(
                    result.get_status().expect("known status"),
                    auth_capnp::LoginStatus::Granted
                );
                assert!(called.get());

                let session = result.get_session().expect("granted session");
                let caps = session
                    .capabilities_request()
                    .send()
                    .promise
                    .await
                    .expect("capabilities RPC");
                let caps = caps.get().expect("capabilities results");
                assert!(caps.has_first());
                assert!(!caps.has_second(), "policy must be able to withhold a cap");

                let value = caps
                    .get_first()
                    .expect("first cap")
                    .read_request()
                    .send()
                    .promise
                    .await
                    .expect("read RPC");
                assert_eq!(
                    value
                        .get()
                        .expect("read results")
                        .get_value()
                        .expect("value")
                        .to_str()
                        .expect("utf-8"),
                    "replacement"
                );
            })
            .await;
    }

    enum ExpectedPolicyFailure {
        Denied,
        BackendUnavailable,
        Overloaded,
    }

    struct FailingPolicy(ExpectedPolicyFailure);

    impl AuthPolicy<structured_session::Owned> for FailingPolicy {
        fn authorize<'a>(
            &'a self,
            _identity: AuthenticatedIdentity,
            _template: SessionTemplate<structured_session::Owned>,
        ) -> LocalPolicyFuture<
            'a,
            Result<SessionGrant<structured_session::Owned>, AuthorizationError>,
        > {
            Box::pin(async move {
                Err(match self.0 {
                    ExpectedPolicyFailure::Denied => {
                        AuthorizationError::Denied("not authorized".into())
                    }
                    ExpectedPolicyFailure::BackendUnavailable => {
                        AuthorizationError::BackendUnavailable("database unavailable".into())
                    }
                    ExpectedPolicyFailure::Overloaded => {
                        AuthorizationError::Overloaded("policy queue full".into())
                    }
                })
            })
        }
    }

    #[tokio::test]
    async fn expected_policy_failures_are_typed_and_sessionless() {
        let local = tokio::task::LocalSet::new();
        local
            .run_until(async {
                let key = SigningKey::generate(&mut rand::rngs::OsRng);
                for (failure, expected) in [
                    (
                        ExpectedPolicyFailure::Denied,
                        auth_capnp::LoginStatus::Denied,
                    ),
                    (
                        ExpectedPolicyFailure::BackendUnavailable,
                        auth_capnp::LoginStatus::BackendUnavailable,
                    ),
                    (
                        ExpectedPolicyFailure::Overloaded,
                        auth_capnp::LoginStatus::Overloaded,
                    ),
                ] {
                    let terminal = structured_terminal(
                        Box::new(FailingPolicy(failure)),
                        structured_session("template", Some("second")),
                        Duration::from_secs(1),
                    );
                    let response = structured_login(&terminal, &key).await;
                    let result = response.get().expect("login results");
                    assert_eq!(result.get_status().expect("known status"), expected);
                    assert!(result.get_session().is_err());
                }
            })
            .await;
    }

    struct DropSignal(Rc<Cell<bool>>);

    impl Drop for DropSignal {
        fn drop(&mut self) {
            self.0.set(true);
        }
    }

    struct HangingPolicy {
        dropped: Rc<Cell<bool>>,
    }

    impl AuthPolicy<structured_session::Owned> for HangingPolicy {
        fn authorize<'a>(
            &'a self,
            _identity: AuthenticatedIdentity,
            _template: SessionTemplate<structured_session::Owned>,
        ) -> LocalPolicyFuture<
            'a,
            Result<SessionGrant<structured_session::Owned>, AuthorizationError>,
        > {
            Box::pin(async move {
                let _drop_signal = DropSignal(Rc::clone(&self.dropped));
                std::future::pending().await
            })
        }
    }

    #[tokio::test]
    async fn policy_timeout_is_typed_and_drops_the_local_future() {
        let local = tokio::task::LocalSet::new();
        local
            .run_until(async {
                let dropped = Rc::new(Cell::new(false));
                let terminal = structured_terminal(
                    Box::new(HangingPolicy {
                        dropped: Rc::clone(&dropped),
                    }),
                    structured_session("template", Some("second")),
                    Duration::from_millis(1),
                );
                let key = SigningKey::generate(&mut rand::rngs::OsRng);

                let response = structured_login(&terminal, &key).await;
                let result = response.get().expect("login results");
                assert_eq!(
                    result.get_status().expect("known status"),
                    auth_capnp::LoginStatus::TimedOut
                );
                assert!(result.get_session().is_err());
                assert!(dropped.get(), "timeout must drop the local policy future");
            })
            .await;
    }

    #[tokio::test]
    async fn flat_login_result_preserves_session_pipelining() {
        let local = tokio::task::LocalSet::new();
        local
            .run_until(async {
                let key = SigningKey::generate(&mut rand::rngs::OsRng);
                let terminal = structured_terminal(
                    Box::new(FixedSessionPolicy::new(key.verifying_key())),
                    structured_session("pipelined", Some("second")),
                    Duration::from_secs(1),
                );
                let signer: auth_capnp::signer::Client =
                    capnp_rpc::new_client(TestSigner::from_ed25519(&key));
                let mut request = terminal.login_request();
                request.get().set_signer(signer);

                let remote = request.send();
                let pipelined_session = remote.pipeline.get_session();
                let pipelined_call = pipelined_session.capabilities_request().send();
                let (login, capabilities) = tokio::join!(remote.promise, pipelined_call.promise);

                let login = login.expect("login RPC");
                assert_eq!(
                    login
                        .get()
                        .expect("login results")
                        .get_status()
                        .expect("known status"),
                    auth_capnp::LoginStatus::Granted
                );
                let capabilities = capabilities.expect("pipelined capabilities RPC");
                assert!(capabilities
                    .get()
                    .expect("capabilities results")
                    .has_first());
            })
            .await;
    }
}
