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
use capnp_rpc::pry;
use ed25519_dalek::VerifyingKey;
use libp2p_core::SignedEnvelope;
use tokio::sync::watch;

/// Policy for Terminal authentication decisions.
///
/// Called after signature verification and nonce check succeed.
/// The policy decides authorization (is this key allowed?),
/// not authentication (is the signature valid?).
pub trait AuthPolicy: Send + 'static {
    /// Check whether the authenticated key is authorized.
    fn check(&self, verifying_key: &VerifyingKey) -> Result<(), capnp::Error>;
}

/// Accept only a specific verifying key. This is the pre-refactor behavior.
pub struct VerifyingKeyPolicy {
    pub expected: VerifyingKey,
}

impl AuthPolicy for VerifyingKeyPolicy {
    fn check(&self, vk: &VerifyingKey) -> Result<(), capnp::Error> {
        if vk.to_bytes() != self.expected.to_bytes() {
            return Err(capnp::Error::failed(
                "login auth failed: signing key does not match expected identity".into(),
            ));
        }
        Ok(())
    }
}

/// Accept any valid signature. Logs the verifying key for audit.
pub struct AllowAllPolicy;

impl AuthPolicy for AllowAllPolicy {
    fn check(&self, vk: &VerifyingKey) -> Result<(), capnp::Error> {
        tracing::info!(
            peer_key = hex::encode(vk.to_bytes()),
            "access granted (allow-all policy)"
        );
        Ok(())
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
    policy: Box<dyn AuthPolicy>,
    session: <Session as capnp::traits::Owned>::Reader<'static>,
    domain: SigningDomain,
    epoch_rx: watch::Receiver<Epoch>,
}

impl<Session> TerminalServer<Session>
where
    Session: capnp::traits::Owned,
    <Session as capnp::traits::Owned>::Reader<'static>: Clone,
{
    /// Create a new Terminal guarding the given session capability.
    ///
    /// Uses `VerifyingKeyPolicy` — only the given key is accepted.
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
            Box::new(VerifyingKeyPolicy { expected: vk }),
            session,
            domain,
            epoch_rx,
        )
    }

    /// Create a new Terminal with a custom auth policy.
    pub fn with_policy(
        policy: Box<dyn AuthPolicy>,
        session: <Session as capnp::traits::Owned>::Reader<'static>,
        domain: SigningDomain,
        epoch_rx: watch::Receiver<Epoch>,
    ) -> Self {
        Self {
            policy,
            session,
            domain,
            epoch_rx,
        }
    }
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
        let signer: auth_capnp::signer::Client = match pry!(params.get()).get_signer() {
            Ok(s) => s,
            Err(_) => return Promise::err(Error::failed("missing signer".into())),
        };

        let session = self.session.clone();
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
            let envelope = SignedEnvelope::from_protobuf_encoding(sig_bytes)
                .map_err(|e| Error::failed(format!("invalid signed envelope: {e}")))?;

            // Verify signature and extract payload + signing key.
            // This checks domain separation and payload type in one step.
            let (payload, pubkey) = envelope
                .payload_and_signing_key(domain.as_str().to_string(), domain.payload_type())
                .map_err(|e| Error::failed(format!("login auth failed: {e}")))?;

            // Check the nonce || epoch_seq matches our challenge.
            let mut expected_payload = Vec::with_capacity(16);
            expected_payload.extend_from_slice(&nonce.to_be_bytes());
            expected_payload.extend_from_slice(&epoch_seq.to_be_bytes());
            if payload != expected_payload {
                return Err(Error::failed(
                    "login auth failed: challenge mismatch".into(),
                ));
            }

            // Verify the epoch hasn't advanced since we issued the challenge.
            // This closes the race where the epoch changes between challenge
            // issuance and response verification.
            let current_seq = self.epoch_rx.borrow().seq;
            if current_seq != epoch_seq {
                return Err(Error::failed(
                    "login auth failed: epoch advanced during authentication".into(),
                ));
            }

            // Extract the ed25519 key and delegate authorization to the policy.
            let envelope_ed = pubkey
                .clone()
                .try_into_ed25519()
                .map_err(|_| Error::failed("login auth failed: not an ed25519 key".into()))?;
            let vk = to_verifying_key(envelope_ed)?;
            self.policy.check(&vk)?;

            results.get().set_session(session)?;
            Ok(())
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::membrane_capnp;
    use ed25519_dalek::SigningKey;

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

                req.send()
                    .promise
                    .await
                    .expect("login should succeed with matching epoch");
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

                match req.send().promise.await {
                    Ok(resp) => match resp.get() {
                        Ok(_) => panic!("login should fail with wrong epoch_seq"),
                        Err(e) => assert!(
                            e.to_string().contains("challenge mismatch"),
                            "expected challenge mismatch, got: {e}"
                        ),
                    },
                    Err(e) => assert!(
                        e.to_string().contains("challenge mismatch"),
                        "expected challenge mismatch, got: {e}"
                    ),
                }
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

                match req.send().promise.await {
                    Ok(resp) => match resp.get() {
                        Ok(_) => panic!("login should fail when epoch advances during auth"),
                        Err(e) => assert!(
                            e.to_string()
                                .contains("epoch advanced during authentication"),
                            "expected epoch-advanced error, got: {e}"
                        ),
                    },
                    Err(e) => assert!(
                        e.to_string()
                            .contains("epoch advanced during authentication"),
                        "expected epoch-advanced error, got: {e}"
                    ),
                }
            })
            .await;
    }
}
