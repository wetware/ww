//! Explicit deployer-side construction of policy-bound Terminal capabilities.

use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::fmt;
use std::rc::Rc;

use auth::SigningDomain;
use call_guard::{membrane, Allowlist, GuardedPolicy, MethodKey, Policy, RevocationGuard};
use capnp::capability::Promise;
use capnp_rpc::pry;

use crate::auth_capnp;
use crate::{
    AuthPolicy, AuthenticatedIdentity, AuthorizationError, Epoch, EpochGuard, LocalPolicyFuture,
    SessionGrant, SessionTemplate, TerminalServer,
};
use tokio::sync::watch;

type RecipientMethods = (String, HashSet<MethodKey>);
type CompiledPolicy = HashMap<[u8; 32], RecipientMethods>;

#[derive(Clone)]
struct Binding {
    profile_name: Rc<str>,
    methods: Rc<HashSet<MethodKey>>,
    revocation: Rc<RevocationGuard>,
}

/// A compiled map from verified signing keys to method-level authority.
///
/// This is the production-baseline map policy. It is intentionally narrower
/// than a general policy language: it does not inspect method arguments or
/// infer identity from the transport peer.
#[derive(Clone)]
pub struct KeyMethodAuthorization {
    bindings: Rc<RefCell<HashMap<[u8; 32], Binding>>>,
    epoch_rx: watch::Receiver<Epoch>,
}

impl KeyMethodAuthorization {
    /// Compile the wire policy used by trusted publication configuration.
    pub fn from_policy(
        epoch_rx: watch::Receiver<Epoch>,
        policy: auth_capnp::authority_policy::Reader<'_>,
    ) -> Result<Self, PolicyCompileError> {
        let bindings = compile_policy(policy)?;
        Ok(Self::new(
            epoch_rx,
            bindings
                .into_iter()
                .map(|(key, (profile_name, methods))| (key, profile_name, methods)),
        ))
    }

    pub fn new(
        epoch_rx: watch::Receiver<Epoch>,
        bindings: impl IntoIterator<Item = ([u8; 32], String, HashSet<MethodKey>)>,
    ) -> Self {
        let bindings = bindings
            .into_iter()
            .map(|(key, profile_name, methods)| {
                (
                    key,
                    Binding {
                        profile_name: profile_name.into(),
                        methods: Rc::new(methods),
                        revocation: RevocationGuard::new(),
                    },
                )
            })
            .collect();
        Self {
            bindings: Rc::new(RefCell::new(bindings)),
            epoch_rx,
        }
    }

    pub fn revoke(&self, key: &[u8; 32]) -> bool {
        let binding = self.bindings.borrow_mut().remove(key);
        if let Some(binding) = binding {
            binding.revocation.revoke();
            true
        } else {
            false
        }
    }
}

impl AuthPolicy<auth_capnp::opaque_session::Owned> for KeyMethodAuthorization {
    fn authorize<'a>(
        &'a self,
        identity: AuthenticatedIdentity,
        template: SessionTemplate<auth_capnp::opaque_session::Owned>,
    ) -> LocalPolicyFuture<
        'a,
        Result<SessionGrant<auth_capnp::opaque_session::Owned>, AuthorizationError>,
    > {
        let key = identity.verifying_key_bytes();
        let binding = self.bindings.borrow().get(&key).cloned();
        let epoch_rx = self.epoch_rx.clone();
        Box::pin(async move {
            let binding = binding.ok_or_else(|| {
                tracing::warn!(
                    recipient_key = %hex::encode(key),
                    "Terminal login denied: no authority profile"
                );
                AuthorizationError::Denied("signing key has no authority profile".into())
            })?;
            tracing::info!(
                recipient_key = %hex::encode(key),
                profile = %binding.profile_name,
                method_count = binding.methods.len(),
                "Terminal authority profile issued"
            );
            let allowlist = binding
                .methods
                .iter()
                .fold(Allowlist::new(), |policy, method| {
                    policy.allow(method.interface_id, method.method_id)
                });
            let issued_seq = epoch_rx.borrow().seq;
            let guarded = GuardedPolicy::new(Box::new(allowlist))
                .with_guard(Rc::new(EpochGuard {
                    issued_seq,
                    receiver: epoch_rx,
                }))
                .with_guard(binding.revocation);
            let session = membrane(template.into_session(), Rc::new(guarded) as Rc<dyn Policy>);
            Ok(SessionGrant::new(session))
        })
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum PolicyCompileError {
    EmptyProfileName,
    DuplicateProfile(String),
    EmptyProfile(String),
    NoRecipients,
    InvalidKeyLength { length: usize },
    DuplicateRecipient([u8; 32]),
    UnknownProfile(String),
    Malformed(String),
}

impl fmt::Display for PolicyCompileError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::EmptyProfileName => formatter.write_str("profile name must not be empty"),
            Self::DuplicateProfile(name) => write!(formatter, "duplicate profile '{name}'"),
            Self::EmptyProfile(name) => write!(formatter, "profile '{name}' has no methods"),
            Self::NoRecipients => formatter.write_str("policy has no recipients"),
            Self::InvalidKeyLength { length } => {
                write!(
                    formatter,
                    "recipient verifying key must be 32 bytes, got {length}"
                )
            }
            Self::DuplicateRecipient(key) => {
                write!(formatter, "duplicate recipient {}", hex::encode(key))
            }
            Self::UnknownProfile(name) => write!(formatter, "unknown profile '{name}'"),
            Self::Malformed(detail) => write!(formatter, "malformed policy: {detail}"),
        }
    }
}

impl std::error::Error for PolicyCompileError {}

fn compile_policy(
    policy: auth_capnp::authority_policy::Reader<'_>,
) -> Result<CompiledPolicy, PolicyCompileError> {
    let mut profiles = HashMap::<String, HashSet<MethodKey>>::new();
    let profile_list = policy
        .get_profiles()
        .map_err(|error| PolicyCompileError::Malformed(error.to_string()))?;
    for profile in profile_list {
        let name = profile
            .get_name()
            .ok()
            .and_then(|name| name.to_str().ok())
            .unwrap_or_default()
            .to_string();
        if name.is_empty() {
            return Err(PolicyCompileError::EmptyProfileName);
        }
        let mut methods = HashSet::new();
        if let Ok(method_list) = profile.get_methods() {
            for method in method_list {
                methods.insert(MethodKey::new(
                    method.get_interface_id(),
                    method.get_ordinal(),
                ));
            }
        }
        if methods.is_empty() {
            return Err(PolicyCompileError::EmptyProfile(name));
        }
        if profiles.insert(name.clone(), methods).is_some() {
            return Err(PolicyCompileError::DuplicateProfile(name));
        }
    }

    let mut bindings = HashMap::new();
    let recipients = policy
        .get_recipients()
        .map_err(|error| PolicyCompileError::Malformed(error.to_string()))?;
    if recipients.is_empty() {
        return Err(PolicyCompileError::NoRecipients);
    }
    for recipient in recipients {
        let raw_key = recipient.get_verifying_key().unwrap_or_default();
        let key: [u8; 32] =
            raw_key
                .try_into()
                .map_err(|_| PolicyCompileError::InvalidKeyLength {
                    length: raw_key.len(),
                })?;
        let profile_name = recipient
            .get_profile()
            .ok()
            .and_then(|name| name.to_str().ok())
            .unwrap_or_default()
            .to_string();
        let methods = profiles
            .get(&profile_name)
            .cloned()
            .ok_or_else(|| PolicyCompileError::UnknownProfile(profile_name.clone()))?;
        if bindings
            .insert(key, (profile_name.clone(), methods))
            .is_some()
        {
            return Err(PolicyCompileError::DuplicateRecipient(key));
        }
    }
    Ok(bindings)
}

/// Trusted constructor that explicitly attaches one policy before publication.
pub struct AuthorityServer {
    guard: EpochGuard,
}

impl AuthorityServer {
    pub fn new(guard: EpochGuard) -> Self {
        Self { guard }
    }
}

#[allow(refining_impl_trait)]
impl auth_capnp::authority::Server for AuthorityServer {
    fn guard(
        self: capnp::capability::Rc<Self>,
        params: auth_capnp::authority::GuardParams,
        mut results: auth_capnp::authority::GuardResults,
    ) -> Promise<(), capnp::Error> {
        pry!(self.guard.check());
        let params = pry!(params.get());
        let session = pry!(params.get_session());
        let policy = pry!(params.get_policy());
        let authorization = pry!(KeyMethodAuthorization::from_policy(
            self.guard.receiver.clone(),
            policy
        )
        .map_err(|error| { capnp::Error::failed(format!("invalid authority policy: {error}")) }));
        let terminal: auth_capnp::terminal::Client<auth_capnp::opaque_session::Owned> =
            capnp_rpc::new_client(TerminalServer::with_policy(
                Box::new(authorization),
                session,
                SigningDomain::terminal_membrane(),
                self.guard.receiver.clone(),
            ));
        results.get().set_terminal(terminal);
        Promise::ok(())
    }
}

#[cfg(test)]
mod tests {
    use capnp::traits::HasTypeId;

    use super::*;
    use crate::{membrane_capnp, membrane_client, Provenance};

    fn epoch(seq: u64) -> Epoch {
        Epoch {
            seq,
            head: format!("head-{seq}").into_bytes(),
            provenance: Provenance::Block(seq),
        }
    }

    fn write_policy(
        mut policy: auth_capnp::authority_policy::Builder<'_>,
        key: &[u8],
        profile_name: &str,
        recipient_profile: &str,
    ) {
        let mut profiles = policy.reborrow().init_profiles(1);
        let mut profile = profiles.reborrow().get(0);
        profile.set_name(profile_name);
        let mut methods = profile.init_methods(1);
        let mut method = methods.reborrow().get(0);
        method.set_interface_id(membrane_capnp::membrane::Client::TYPE_ID);
        method.set_ordinal(0);

        let mut recipients = policy.init_recipients(1);
        let mut recipient = recipients.reborrow().get(0);
        recipient.set_verifying_key(key);
        recipient.set_profile(recipient_profile);
    }

    #[test]
    fn policy_compilation_rejects_unknown_profiles_and_bad_keys() {
        let mut message = capnp::message::Builder::new_default();
        let policy = message.init_root::<auth_capnp::authority_policy::Builder>();
        write_policy(policy, &[7; 32], "reader", "missing");
        let policy = message
            .get_root_as_reader::<auth_capnp::authority_policy::Reader>()
            .unwrap();
        assert_eq!(
            compile_policy(policy),
            Err(PolicyCompileError::UnknownProfile("missing".into()))
        );

        let mut message = capnp::message::Builder::new_default();
        let policy = message.init_root::<auth_capnp::authority_policy::Builder>();
        write_policy(policy, &[7; 31], "reader", "reader");
        let policy = message
            .get_root_as_reader::<auth_capnp::authority_policy::Reader>()
            .unwrap();
        assert_eq!(
            compile_policy(policy),
            Err(PolicyCompileError::InvalidKeyLength { length: 31 })
        );
    }

    #[tokio::test]
    async fn authority_constructs_terminal_from_explicit_policy() {
        let local = tokio::task::LocalSet::new();
        local
            .run_until(async {
                let (_epoch_tx, epoch_rx) = watch::channel(epoch(1));
                let guard = EpochGuard {
                    issued_seq: 1,
                    receiver: epoch_rx.clone(),
                };
                let authority: auth_capnp::authority::Client =
                    capnp_rpc::new_client(AuthorityServer::new(guard));
                let session = membrane_client(epoch_rx);
                let mut request = authority.guard_request();
                request
                    .get()
                    .set_session(auth_capnp::opaque_session::Client {
                        client: session.client,
                    });
                write_policy(request.get().init_policy(), &[9; 32], "reader", "reader");
                let response = request
                    .send()
                    .promise
                    .await
                    .expect("explicit policy compiles");
                assert!(
                    response.get().expect("guard results").has_terminal(),
                    "authority constructor returns a Terminal capability"
                );
            })
            .await;
    }
}
