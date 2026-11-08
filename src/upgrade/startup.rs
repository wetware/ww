//! Persisted startup identities and stable admission/instance leases.
//!
//! Decoded records and successful structural checks do not authenticate a
//! release, certify a service observation, or authorize a transaction child.
//! The transaction owner must authenticate the complete tuple before admission.

use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::os::fd::AsRawFd;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};

use anyhow::{ensure, Context, Result};
use serde::{de::DeserializeOwned, Deserialize, Serialize};
use sha2::{Digest, Sha256};

use super::provenance::{AuthenticatedGeneration, GenerationRecord};
use super::service::{EnabledState, StartupIdentity};
use super::{storage, ArtifactIdentity};
use crate::installation_state::{MutationLock, Receipt};

const MAX_RECORD: usize = 64 * 1024;
const STARTUP_LOCK: &str = ".ww-startup.lock";
const INSTANCE_LOCK: &str = ".ww-host-instance.lock";
const STARTUP_STORE: &str = ".ww/bin/.ww-host-upgrade";

/// Detect surviving guarded metadata without granting admission. Missing parts
/// must then fail full tuple validation instead of reverting to legacy behavior.
pub fn metadata_present(home: &Path) -> Result<bool> {
    let store = home.join(STARTUP_STORE);
    let mut present = false;
    for path in [
        home.join(STARTUP_LOCK),
        home.join(INSTANCE_LOCK),
        store.join("startup-state.json"),
        store.join("startup-contract.json"),
        store.join("proofs"),
    ] {
        present |= storage::exists(&path)?;
    }
    if let Some(bytes) = storage::read_record(&home.join(".ww/host-upgrade-retained.json"))? {
        let retained: serde_json::Value = serde_json::from_slice(&bytes)?;
        ensure!(
            matches!(retained["version"].as_u64(), Some(1 | 2)),
            "unsupported retained native state"
        );
        present |= retained["version"] == 2;
    }
    Ok(present)
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ContractIdentity {
    pub generation: u64,
    pub digest: [u8; 32],
}

impl ContractIdentity {
    pub fn next_generation(&self) -> Result<u64> {
        self.generation
            .checked_add(1)
            .context("startup contract generation exhausted")
    }

    fn validate(&self) -> Result<()> {
        ensure!(
            self.generation > 0,
            "startup contract generation must be positive"
        );
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ServiceManager {
    Systemd,
    Launchd,
}

/// Identities of observed effective service inputs, never raw environment values.
/// Constructing this record does not establish that a service was observed.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ServiceIdentity {
    pub manager: ServiceManager,
    pub domain: String,
    pub name: String,
    pub definition: [u8; 32],
    pub drop_ins: [u8; 32],
    pub environment_files: [u8; 32],
    pub effective_argv: [u8; 32],
    pub effective_environment: [u8; 32],
    pub registered: bool,
    pub enabled: EnabledState,
}

impl ServiceIdentity {
    fn validate(&self) -> Result<()> {
        for value in [&self.domain, &self.name] {
            ensure!(
                !value.is_empty() && value.len() <= 1024 && !value.chars().any(char::is_control),
                "invalid startup service identity"
            );
        }
        ensure!(
            self.enabled != EnabledState::Unknown,
            "service enablement is unknown"
        );
        Ok(())
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "kebab-case", deny_unknown_fields)]
#[allow(clippy::large_enum_variant)] // Persist one exact owned service identity without a separate allocation.
pub enum InvocationBinding {
    Standalone,
    Managed { service: ServiceIdentity },
}

impl InvocationBinding {
    fn validate(&self) -> Result<()> {
        if let Self::Managed { service } = self {
            service.validate()?;
        }
        Ok(())
    }
}

/// Untrusted persisted admission inputs. The digest detects tuple disagreement;
/// the installation's private custody and authenticated tuple supply authority.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StartupContractV1 {
    pub version: u32,
    pub installation_id: [u8; 32],
    pub identity: ContractIdentity,
    pub artifact: ArtifactIdentity,
    pub startup: StartupIdentity,
    pub invocation: InvocationBinding,
    pub trusted_release_keys: PathBuf,
}

impl StartupContractV1 {
    pub fn new(
        installation_id: [u8; 32],
        generation: u64,
        artifact: ArtifactIdentity,
        startup: StartupIdentity,
        invocation: InvocationBinding,
        trusted_release_keys: PathBuf,
    ) -> Result<Self> {
        let mut contract = Self {
            version: 1,
            installation_id,
            identity: ContractIdentity {
                generation,
                digest: [0; 32],
            },
            artifact,
            startup,
            invocation,
            trusted_release_keys,
        };
        contract.identity.digest = contract.computed_digest()?;
        contract.validate()?;
        Ok(contract)
    }

    fn computed_digest(&self) -> Result<[u8; 32]> {
        // The generation is covered, but the digest field itself is excluded.
        digest(
            b"wetware.startup-contract.v1",
            &(
                self.version,
                self.installation_id,
                self.identity.generation,
                &self.artifact,
                &self.startup,
                &self.invocation,
                &self.trusted_release_keys,
            ),
        )
    }

    pub fn validate(&self) -> Result<()> {
        ensure!(self.version == 1, "unsupported startup contract version");
        nonzero(&self.installation_id, "installation ID")?;
        self.identity.validate()?;
        self.artifact.validate()?;
        self.startup.validate()?;
        self.invocation.validate()?;
        validate_trust_path(&self.trusted_release_keys)?;
        ensure!(
            self.identity.digest == self.computed_digest()?,
            "startup contract digest mismatch"
        );
        Ok(())
    }

    pub fn encode(&self) -> Result<Vec<u8>> {
        self.validate()?;
        encode(self)
    }
    pub fn decode(bytes: &[u8]) -> Result<Self> {
        let value: Self = decode(bytes)?;
        value.validate()?;
        Ok(value)
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "kebab-case", deny_unknown_fields)]
pub enum StartupDisposition {
    RequiredUnbound,
    Uninstalled,
    Blocked,
    Committed {
        contract: ContractIdentity,
    },
    Transaction {
        transaction_id: [u8; 32],
        permit_digest: [u8; 32],
        contract: ContractIdentity,
    },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum InitializationIdentityPolicy {
    CreateIfMissing,
    PreserveExisting,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StartupStateV1 {
    pub version: u32,
    pub installation_id: [u8; 32],
    pub native: GenerationRecord,
    pub trusted_release_keys: PathBuf,
    pub disposition: StartupDisposition,
    /// Exact intended incomplete ownership receipt, only during initialization.
    pub initializing_receipt: Option<Vec<u8>>,
    pub initializing_receipt_sha256: Option<[u8; 32]>,
    pub initializing_identity: Option<InitializationIdentityPolicy>,
    pub initializing_previous_proofs: Vec<super::provenance::ProofReference>,
}

impl StartupStateV1 {
    pub fn validate(&self) -> Result<()> {
        ensure!(self.version == 1, "unsupported startup state version");
        nonzero(&self.installation_id, "installation ID")?;
        self.native.validate()?;
        validate_trust_path(&self.trusted_release_keys)?;
        match &self.disposition {
            StartupDisposition::Committed { contract } => contract.validate()?,
            StartupDisposition::Transaction {
                transaction_id,
                permit_digest,
                contract,
            } => {
                nonzero(transaction_id, "transaction ID")?;
                nonzero(permit_digest, "permit digest")?;
                contract.validate()?;
            }
            _ => (),
        }
        ensure!(
            self.initializing_previous_proofs.len() <= 2,
            "initialization proof cleanup exceeds retention bound"
        );
        for (index, proof) in self.initializing_previous_proofs.iter().enumerate() {
            proof.validate()?;
            ensure!(
                !self.initializing_previous_proofs[..index]
                    .iter()
                    .any(|prior| prior.root == proof.root),
                "duplicate initialization proof ownership"
            );
        }
        ensure!(
            self.initializing_receipt.is_some() || self.initializing_previous_proofs.is_empty(),
            "proof cleanup ownership outside initialization"
        );
        match (
            &self.initializing_receipt,
            &self.initializing_receipt_sha256,
            self.initializing_identity,
        ) {
            (None, None, None) => (),
            (Some(bytes), Some(expected), Some(_)) => {
                ensure!(
                    self.disposition == StartupDisposition::RequiredUnbound,
                    "initialization receipt outside required-unbound startup state"
                );
                ensure!(
                    <[u8; 32]>::from(Sha256::digest(bytes)) == *expected,
                    "initialization receipt digest mismatch"
                );
                let receipt = Receipt::decode(bytes)?;
                ensure!(
                    !receipt.complete,
                    "initialization intent requires an incomplete receipt"
                );
            }
            _ => anyhow::bail!(
                "initialization receipt, digest and identity policy must occur together"
            ),
        }
        Ok(())
    }

    pub fn validate_contract(&self, contract: &StartupContractV1) -> Result<()> {
        self.validate()?;
        contract.validate()?;
        ensure!(
            self.installation_id == contract.installation_id,
            "startup installation ID mismatch"
        );
        ensure!(
            self.native.artifact == contract.artifact,
            "startup native artifact mismatch"
        );
        ensure!(
            self.trusted_release_keys == contract.trusted_release_keys,
            "startup independent trust path mismatch"
        );
        let expected = match &self.disposition {
            StartupDisposition::Committed { contract }
            | StartupDisposition::Transaction { contract, .. } => contract,
            _ => anyhow::bail!("startup disposition does not admit a contract"),
        };
        ensure!(
            *expected == contract.identity,
            "startup state references another contract"
        );
        Ok(())
    }

    pub fn encode(&self) -> Result<Vec<u8>> {
        self.validate()?;
        encode(self)
    }
    pub fn decode(bytes: &[u8]) -> Result<Self> {
        let value: Self = decode(bytes)?;
        value.validate()?;
        Ok(value)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum PermitTarget {
    Candidate,
    RestoredBefore,
}

/// Digests of the exact target tuple; executable identity retains its algorithm.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TargetTupleDigests {
    pub executable: ArtifactIdentity,
    pub native_receipt: [u8; 32],
    pub locator: [u8; 32],
    pub proof: [u8; 32],
    pub contract: [u8; 32],
}

/// Immutable permit payload. This decoded structure is deliberately NOT a
/// StartupPermit: only full journal-v2 validation may grant child admission.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StartupPermitPayload {
    pub version: u32,
    pub transaction_id: [u8; 32],
    pub installation_id: [u8; 32],
    pub role: PermitTarget,
    pub target: TargetTupleDigests,
    pub contract: ContractIdentity,
    pub invocation: InvocationBinding,
}

impl StartupPermitPayload {
    pub fn validate(&self) -> Result<()> {
        ensure!(self.version == 1, "unsupported startup permit version");
        nonzero(&self.transaction_id, "transaction ID")?;
        nonzero(&self.installation_id, "installation ID")?;
        self.contract.validate()?;
        self.target.executable.validate()?;
        self.invocation.validate()?;
        ensure!(
            self.target.contract == self.contract.digest,
            "permit contract digest mismatch"
        );
        ensure!(
            matches!(self.invocation, InvocationBinding::Managed { ref service } if service.registered),
            "transaction permits require a registered managed invocation"
        );
        Ok(())
    }

    pub fn digest(&self) -> Result<[u8; 32]> {
        self.validate()?;
        digest(b"wetware.startup-permit.v1", self)
    }

    /// Compare independently supplied tuple and invocation observations. This
    /// structural check does not authenticate the journal or the observations.
    pub fn validate_binding(
        &self,
        state: &StartupStateV1,
        contract: &StartupContractV1,
        actual_tuple: &TargetTupleDigests,
        actual_invocation: &InvocationBinding,
    ) -> Result<()> {
        self.validate()?;
        state.validate_contract(contract)?;
        ensure!(
            self.installation_id == state.installation_id && self.contract == contract.identity,
            "permit identifies another installation or contract"
        );
        ensure!(
            &self.target == actual_tuple && self.target.executable == contract.artifact,
            "permit target tuple changed"
        );
        ensure!(
            &self.invocation == actual_invocation && self.invocation == contract.invocation,
            "permit invocation changed"
        );
        match &state.disposition {
            StartupDisposition::Transaction {
                transaction_id,
                permit_digest,
                contract: identity,
            } => {
                ensure!(
                    *transaction_id == self.transaction_id
                        && *permit_digest == self.digest()?
                        && *identity == self.contract,
                    "startup state does not bind this transaction permit"
                );
            }
            _ => anyhow::bail!("transaction permit requires transaction startup state"),
        }
        Ok(())
    }

    pub fn encode(&self) -> Result<Vec<u8>> {
        self.validate()?;
        encode(self)
    }
    pub fn decode(bytes: &[u8]) -> Result<Self> {
        let value: Self = decode(bytes)?;
        value.validate()?;
        Ok(value)
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StartupMarkerV1 {
    pub version: u32,
    pub installation_id: [u8; 32],
}

impl StartupMarkerV1 {
    fn validate(&self) -> Result<()> {
        ensure!(self.version == 1, "unsupported startup marker version");
        nonzero(&self.installation_id, "installation ID")
    }
    pub fn encode(&self) -> Result<Vec<u8>> {
        self.validate()?;
        encode(self)
    }
    pub fn decode(bytes: &[u8]) -> Result<Self> {
        let value: Self = decode(bytes)?;
        value.validate()?;
        Ok(value)
    }
}

struct Lease {
    file: File,
    home: PathBuf,
    path: PathBuf,
    exclusive: bool,
}

impl Lease {
    fn acquire(home: &Path, name: &str, exclusive: bool, create: bool) -> Result<Self> {
        ensure!(
            home.is_absolute(),
            "startup installation HOME must be absolute"
        );
        storage::directory(home)?;
        let path = home.join(name);
        let file = OpenOptions::new()
            .read(true)
            .write(exclusive)
            .create(create)
            .truncate(false)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_NONBLOCK)
            .open(&path)
            .with_context(|| format!("open startup lease {}", path.display()))?;
        private_file(&file)?;
        // SAFETY: file owns its descriptor and flock retains no pointer.
        let status = unsafe {
            libc::flock(
                file.as_raw_fd(),
                (if exclusive {
                    libc::LOCK_EX
                } else {
                    libc::LOCK_SH
                }) | libc::LOCK_NB,
            )
        };
        if status != 0 {
            return Err(std::io::Error::last_os_error())
                .with_context(|| format!("acquire startup lease {}", path.display()));
        }
        let lease = Self {
            file,
            home: home.to_owned(),
            path,
            exclusive,
        };
        lease.check_link()?;
        if create {
            storage::sync_parent(&lease.path)?;
        }
        Ok(lease)
    }

    fn check_link(&self) -> Result<()> {
        private_file(&self.file)?;
        let opened = self.file.metadata()?;
        let named = std::fs::symlink_metadata(&self.path)?;
        ensure!(
            named.is_file() && opened.dev() == named.dev() && opened.ino() == named.ino(),
            "startup lease inode changed; preserve installation state for recovery"
        );
        Ok(())
    }

    fn bytes(&self) -> Result<Vec<u8>> {
        self.check_link()?;
        // FileExt avoids sharing a mutable seek position with other reads.
        use std::os::unix::fs::FileExt;
        let size = self.file.metadata()?.len();
        ensure!(size <= MAX_RECORD as u64, "startup marker exceeds 64 KiB");
        let mut bytes = vec![0; size as usize];
        self.file.read_exact_at(&mut bytes, 0)?;
        Ok(bytes)
    }
}

/// Shared after admission, retained until the Host exits. Descriptors never
/// clear CLOEXEC; helper/guest execution must not inherit instance exclusion.
pub struct InstanceLease(Lease);

impl InstanceLease {
    pub fn shared(home: &Path) -> Result<Self> {
        Self::open(home, false, false)
    }
    pub fn exclusive(home: &Path) -> Result<Self> {
        Self::open(home, true, false)
    }
    pub fn exclusive_create(mutation: &MutationLock) -> Result<Self> {
        Self::open(mutation.home(), true, true)
    }
    fn open(home: &Path, exclusive: bool, create: bool) -> Result<Self> {
        let lease = Lease::acquire(home, INSTANCE_LOCK, exclusive, create)?;
        ensure!(
            lease.file.metadata()?.len() == 0,
            "instance lease contains unexpected data"
        );
        Ok(Self(lease))
    }
    pub fn home(&self) -> &Path {
        &self.0.home
    }
}

/// Stable startup marker inode plus shared/exclusive admission exclusion.
/// Writers acquire MutationLock, then instance exclusion when needed, then
/// this lease. Do not wait for instance exclusion while holding this lease.
pub struct StartupLease(Lease);

impl StartupLease {
    pub fn shared(home: &Path) -> Result<Self> {
        Ok(Self(Lease::acquire(home, STARTUP_LOCK, false, false)?))
    }
    pub fn exclusive(home: &Path) -> Result<Self> {
        Ok(Self(Lease::acquire(home, STARTUP_LOCK, true, false)?))
    }
    pub fn exclusive_create(mutation: &MutationLock) -> Result<Self> {
        Ok(Self(Lease::acquire(
            mutation.home(),
            STARTUP_LOCK,
            true,
            true,
        )?))
    }
    pub fn home(&self) -> &Path {
        &self.0.home
    }
    pub fn marker_is_empty(&self) -> Result<bool> {
        Ok(self.0.bytes()?.is_empty())
    }

    pub fn marker(&self) -> Result<StartupMarkerV1> {
        StartupMarkerV1::decode(&self.0.bytes()?)
            .context("required startup marker is missing, incomplete, or corrupt")
    }

    /// Complete only the recorded initialization. The caller must first retain
    /// intent with write_state and persist the matching incomplete receipt.
    /// Current independent authentication is required, including on retry.
    pub fn initialize_marker(
        &mut self,
        intent: &StartupStateV1,
        generation: &AuthenticatedGeneration,
    ) -> Result<StartupMarkerV1> {
        ensure!(
            super::same_generation_identity(&intent.native, generation.record()),
            "initialization release differs from authenticated intent"
        );
        self.initialize_marker_from_intent(intent)
    }

    fn initialize_marker_from_intent(
        &mut self,
        intent: &StartupStateV1,
    ) -> Result<StartupMarkerV1> {
        ensure!(
            self.0.exclusive,
            "marker initialization requires exclusive startup lease"
        );
        intent.validate()?;
        ensure!(
            self.read_state()?.as_ref() == Some(intent),
            "startup initialization intent is not durable"
        );
        let expected_receipt = intent
            .initializing_receipt
            .as_ref()
            .context("missing initialization receipt intent")?;
        let receipt_path = self.0.home.join(".ww/installation.json");
        let actual_receipt =
            private_record(&receipt_path)?.context("incomplete receipt is not durable")?;
        ensure!(
            &actual_receipt == expected_receipt,
            "initialization receipt differs from recorded intent"
        );
        let marker = StartupMarkerV1 {
            version: 1,
            installation_id: intent.installation_id,
        };
        let encoded = marker.encode()?;
        let existing = self.0.bytes()?;
        ensure!(
            encoded.starts_with(&existing),
            "startup marker conflicts with initialization intent"
        );
        // Never replace, unlink, or truncate this stable lock inode. A partial
        // write can only be extended using the exact recorded marker prefix.
        self.0.file.seek(SeekFrom::Start(existing.len() as u64))?;
        storage::checkpoint("startup.marker.write.before")?;
        self.0.file.write_all(&encoded[existing.len()..])?;
        storage::checkpoint("startup.marker.write.after")?;
        self.0.file.sync_all()?;
        storage::checkpoint("startup.marker.sync.after")?;
        storage::sync_parent(&self.0.path)?;
        self.marker()
    }

    pub fn read_state(&self) -> Result<Option<StartupStateV1>> {
        self.0.check_link()?;
        private_record(&self.record_path("startup-state.json")?)?
            .map(|bytes| StartupStateV1::decode(&bytes))
            .transpose()
    }

    pub fn write_state(&self, state: &StartupStateV1) -> Result<()> {
        self.require_writer()?;
        state.validate()?;
        let marker = StartupMarkerV1 {
            version: 1,
            installation_id: state.installation_id,
        }
        .encode()?;
        let existing = self.0.bytes()?;
        let previous = self.read_state()?;
        match &previous {
            None => {
                ensure!(
                    existing.is_empty() && state.initializing_receipt.is_some(),
                    "startup state is missing from an initialized installation"
                );
            }
            Some(previous) => {
                ensure!(
                    previous.installation_id == state.installation_id,
                    "startup installation ID cannot change"
                );
                if existing != marker {
                    ensure!(
                        previous.initializing_receipt.is_some(),
                        "completed startup marker is missing or corrupt"
                    );
                }
                if let Some(incomplete) = &previous.initializing_receipt {
                    ensure!(
                        state.native == previous.native
                            && state.trusted_release_keys == previous.trusted_release_keys
                            && state.disposition == StartupDisposition::RequiredUnbound,
                        "interrupted initialization must retain its exact native generation"
                    );
                    if let Some(next) = &state.initializing_receipt {
                        ensure!(
                            next == incomplete
                                && state.initializing_identity == previous.initializing_identity
                                && state.initializing_previous_proofs
                                    == previous.initializing_previous_proofs,
                            "interrupted initialization receipt or identity policy changed"
                        );
                    } else {
                        // The complete receipt is the installer's terminal fence.
                        // Clearing intent before that fence would lose retry evidence.
                        let mut complete = Receipt::decode(incomplete)?;
                        complete.complete = true;
                        complete.previous = None;
                        let actual = private_record(&self.0.home.join(".ww/installation.json"))?
                            .context("completed installation receipt is missing")?;
                        ensure!(
                            actual == complete.encode()?,
                            "initialization completion requires the exact complete receipt"
                        );
                    }
                } else if state.initializing_receipt.is_some() {
                    ensure!(
                        previous.disposition == StartupDisposition::Uninstalled,
                        "completed installation cannot create a new initialization intent"
                    );
                }
            }
        }
        if existing != marker {
            ensure!(
                state.initializing_receipt.is_some() && marker.starts_with(&existing),
                "startup state conflicts with initialized marker"
            );
        }
        let path = self.record_path("startup-state.json")?;
        storage::write_record(&path, &state.encode()?)
    }

    pub fn read_contract(&self) -> Result<Option<StartupContractV1>> {
        let marker = self.marker()?;
        let result = private_record(&self.record_path("startup-contract.json")?)?
            .map(|bytes| StartupContractV1::decode(&bytes))
            .transpose()?;
        if let Some(contract) = &result {
            ensure!(
                contract.installation_id == marker.installation_id,
                "contract belongs to another installation"
            );
        }
        Ok(result)
    }

    pub fn write_contract(&self, contract: &StartupContractV1) -> Result<()> {
        self.require_writer()?;
        ensure!(
            self.marker()?.installation_id == contract.installation_id,
            "contract belongs to another installation"
        );
        let path = self.record_path("startup-contract.json")?;
        private_record(&path)?;
        storage::write_record(&path, &contract.encode()?)
    }

    fn require_writer(&self) -> Result<()> {
        ensure!(
            self.0.exclusive,
            "startup persistence requires exclusive startup lease"
        );
        self.0.check_link()
    }

    fn record_path(&self, name: &str) -> Result<PathBuf> {
        for relative in [".ww", ".ww/bin", STARTUP_STORE] {
            storage::directory(&self.0.home.join(relative))?;
        }
        Ok(self.0.home.join(STARTUP_STORE).join(name))
    }
}

fn private_file(file: &File) -> Result<()> {
    let metadata = file.metadata()?;
    // SAFETY: geteuid has no pointer arguments.
    let uid = unsafe { libc::geteuid() };
    ensure!(
        metadata.is_file()
            && metadata.nlink() == 1
            && metadata.uid() == uid
            && metadata.mode() & 0o177 == 0,
        "startup files must be private single-link regular files owned by this user"
    );
    Ok(())
}

fn private_record(path: &Path) -> Result<Option<Vec<u8>>> {
    if !storage::exists(path)? {
        return Ok(None);
    }
    let file = storage::open_read(path)?;
    private_file(&file)?;
    let mut bytes = Vec::new();
    file.take(MAX_RECORD as u64 + 1).read_to_end(&mut bytes)?;
    ensure!(bytes.len() <= MAX_RECORD, "startup record exceeds 64 KiB");
    Ok(Some(bytes))
}

fn validate_trust_path(path: &Path) -> Result<()> {
    use std::path::Component;
    let text = path.to_str().context("trust path must be UTF-8")?;
    ensure!(
        path.is_absolute()
            && path.file_name().is_some()
            && text.len() <= 4096
            && !text.chars().any(char::is_control)
            && path
                .components()
                .all(|c| matches!(c, Component::RootDir | Component::Normal(_)))
            && path.as_os_str() == path.components().collect::<PathBuf>().as_os_str(),
        "independent release trust path must be a normalized absolute file path"
    );
    Ok(())
}

fn nonzero(value: &[u8; 32], label: &str) -> Result<()> {
    ensure!(*value != [0; 32], "{label} must not be zero");
    Ok(())
}

fn encode<T: Serialize>(value: &T) -> Result<Vec<u8>> {
    let bytes = serde_json::to_vec(value)?;
    ensure!(bytes.len() <= MAX_RECORD, "startup record exceeds 64 KiB");
    Ok(bytes)
}

fn decode<T: DeserializeOwned + Serialize>(bytes: &[u8]) -> Result<T> {
    ensure!(
        !bytes.is_empty() && bytes.len() <= MAX_RECORD,
        "startup record is empty or exceeds 64 KiB"
    );
    let value: T = serde_json::from_slice(bytes).context("decode startup record")?;
    ensure!(encode(&value)? == bytes, "startup record is not canonical");
    Ok(value)
}

fn digest<T: Serialize>(domain: &[u8], value: &T) -> Result<[u8; 32]> {
    let bytes = encode(value)?;
    let mut hash = Sha256::new();
    hash.update((domain.len() as u64).to_le_bytes());
    hash.update(domain);
    hash.update((bytes.len() as u64).to_le_bytes());
    hash.update(bytes);
    Ok(hash.finalize().into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::upgrade::DigestAlgorithm;
    use std::os::unix::fs::{symlink, MetadataExt, PermissionsExt};

    fn contract() -> StartupContractV1 {
        StartupContractV1::new(
            [1; 32],
            1,
            ArtifactIdentity {
                algorithm: DigestAlgorithm::Sha256,
                digest: "a".repeat(64),
                size: 42,
            },
            StartupIdentity {
                configuration: "configuration".into(),
                fhs_baseline: Some("baseline".into()),
                frozen_sources: "sources".into(),
                publisher_target: None,
            },
            InvocationBinding::Standalone,
            PathBuf::from("/etc/wetware/release-keys"),
        )
        .unwrap()
    }

    #[test]
    fn contract_binds_the_independent_trust_path() {
        let mut changed = contract();
        changed.trusted_release_keys = PathBuf::from("/elsewhere/keys");
        assert!(changed.validate().is_err());
    }

    #[test]
    fn initialization_identity_policy_cannot_change_during_retry() {
        let (_home, _mutation, lease, state) = setup_intent();
        let mut changed = state.clone();
        changed.initializing_identity = Some(InitializationIdentityPolicy::CreateIfMissing);
        assert!(lease.write_state(&changed).is_err());
        changed.initializing_identity = None;
        assert!(changed.validate().is_err());
    }

    #[test]
    fn contract_refuses_changed_inputs_and_noncanonical_records() {
        let original = contract();
        let bytes = original.encode().unwrap();
        assert_eq!(StartupContractV1::decode(&bytes).unwrap(), original);
        let mut changed = original.clone();
        changed.startup.configuration = "other".into();
        assert!(changed.validate().is_err());
        let mut whitespace = bytes.clone();
        whitespace.push(b'\n');
        assert!(StartupContractV1::decode(&whitespace).is_err());
        let unknown = String::from_utf8(bytes)
            .unwrap()
            .replacen("{", "{\"unknown\":0,", 1);
        assert!(StartupContractV1::decode(unknown.as_bytes()).is_err());
        assert!(StartupContractV1::decode(&vec![b' '; MAX_RECORD + 1]).is_err());
    }

    #[test]
    fn contract_rejects_unknown_version_and_generation_overflow() {
        let mut value = contract();
        value.version = 2;
        assert!(value.encode().is_err());
        assert!(ContractIdentity {
            generation: u64::MAX,
            digest: [1; 32]
        }
        .next_generation()
        .is_err());
    }

    #[test]
    fn contended_startup_lease_preserves_the_would_block_error() {
        let home = tempfile::tempdir().unwrap();
        let mutation = MutationLock::acquire(home.path()).unwrap();
        drop(StartupLease::exclusive_create(&mutation).unwrap());
        let _reader = StartupLease::shared(home.path()).unwrap();
        let error = StartupLease::exclusive(home.path()).err().unwrap();
        assert_eq!(
            error
                .downcast_ref::<std::io::Error>()
                .map(std::io::Error::kind),
            Some(std::io::ErrorKind::WouldBlock),
            "contention must remain distinguishable from custody and other IO failures"
        );
    }

    #[test]
    fn instance_leases_share_and_exclude_without_changing_inode() {
        let home = tempfile::tempdir().unwrap();
        let mutation = MutationLock::acquire(home.path()).unwrap();
        let exclusive = InstanceLease::exclusive_create(&mutation).unwrap();
        let before = std::fs::metadata(home.path().join(INSTANCE_LOCK))
            .unwrap()
            .ino();
        assert!(InstanceLease::shared(home.path()).is_err());
        drop(exclusive);
        let first = InstanceLease::shared(home.path()).unwrap();
        let second = InstanceLease::shared(home.path()).unwrap();
        assert!(InstanceLease::exclusive(home.path()).is_err());
        drop(first);
        drop(second);
        // Another parallel test may have forked while the shared descriptors
        // were open. CLOEXEC closes that temporary inheritance at exec.
        let until = std::time::Instant::now() + std::time::Duration::from_secs(1);
        let _exclusive = loop {
            match InstanceLease::exclusive(home.path()) {
                Ok(lease) => break lease,
                Err(error) if std::time::Instant::now() >= until => {
                    panic!("lease remains held: {error:#}")
                }
                Err(_) => std::thread::sleep(std::time::Duration::from_millis(1)),
            }
        };
        assert_eq!(
            before,
            std::fs::metadata(home.path().join(INSTANCE_LOCK))
                .unwrap()
                .ino()
        );
    }

    #[test]
    fn lease_custody_refuses_missing_symlink_hardlink_and_public_permissions() {
        let home = tempfile::tempdir().unwrap();
        assert!(StartupLease::shared(home.path()).is_err());
        assert!(!home.path().join(STARTUP_LOCK).exists());
        let target = home.path().join("target");
        std::fs::write(&target, []).unwrap();
        symlink(&target, home.path().join(STARTUP_LOCK)).unwrap();
        assert!(StartupLease::shared(home.path()).is_err());
        std::fs::remove_file(home.path().join(STARTUP_LOCK)).unwrap();
        std::fs::hard_link(&target, home.path().join(STARTUP_LOCK)).unwrap();
        assert!(StartupLease::shared(home.path()).is_err());
        std::fs::remove_file(home.path().join(STARTUP_LOCK)).unwrap();
        std::fs::write(home.path().join(STARTUP_LOCK), []).unwrap();
        std::fs::set_permissions(
            home.path().join(STARTUP_LOCK),
            std::fs::Permissions::from_mode(0o644),
        )
        .unwrap();
        assert!(StartupLease::shared(home.path()).is_err());
    }
    fn intent() -> StartupStateV1 {
        let contract = contract();
        let hash = cid::multihash::Multihash::<64>::wrap(0x12, &[2; 32]).unwrap();
        let root = cid::Cid::new_v1(0x70, hash).to_string();
        let receipt = Receipt {
            version: 2,
            hashes: std::array::from_fn(|_| "a".repeat(64)),
            previous: None,
            complete: false,
        }
        .encode()
        .unwrap();
        StartupStateV1 {
            version: 1,
            installation_id: contract.installation_id,
            native: GenerationRecord {
                version: 1,
                transport_root: root.clone(),
                descriptor: cid::Cid::new_v1(0x71, hash).to_string(),
                artifact: contract.artifact,
                target: "aarch64-apple-darwin".into(),
                release_version: "0.1.1".into(),
                revision: "a".repeat(40),
                proof: super::super::provenance::ProofReference {
                    root,
                    size: 123,
                    sha256: "b".repeat(64),
                },
                verification_keys:
                    b"MCowBQYDK2VwAyEA11qYAYKxCrfVS/7TyWQHOg7hcvPapiMlrwIaaPcHURo=\n".to_vec(),
            },
            trusted_release_keys: PathBuf::from("/etc/wetware/release-keys"),
            initializing_identity: Some(InitializationIdentityPolicy::PreserveExisting),
            initializing_previous_proofs: Vec::new(),
            disposition: StartupDisposition::RequiredUnbound,
            initializing_receipt_sha256: Some(Sha256::digest(&receipt).into()),
            initializing_receipt: Some(receipt),
        }
    }

    fn setup_intent() -> (
        tempfile::TempDir,
        MutationLock,
        StartupLease,
        StartupStateV1,
    ) {
        let home = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(home.path().join(".ww/bin")).unwrap();
        storage::Paths::open(home.path()).unwrap();
        let mutation = MutationLock::acquire(home.path()).unwrap();
        let lease = StartupLease::exclusive_create(&mutation).unwrap();
        let state = intent();
        lease.write_state(&state).unwrap();
        storage::write_record(
            &home.path().join(".ww/installation.json"),
            state.initializing_receipt.as_ref().unwrap(),
        )
        .unwrap();
        (home, mutation, lease, state)
    }

    #[test]
    fn interrupted_initialization_cannot_change_identity_release_or_receipt() {
        let (_home, _mutation, lease, original) = setup_intent();
        let mut changed = original.clone();
        changed.installation_id = [2; 32];
        assert!(
            lease.write_state(&changed).is_err(),
            "changed initialization ID was accepted"
        );
        changed = original.clone();
        changed.native.release_version = "different".into();
        assert!(
            lease.write_state(&changed).is_err(),
            "changed initialization release was accepted"
        );
        changed = original.clone();
        changed.initializing_receipt.as_mut().unwrap().push(b'\n');
        changed.initializing_receipt_sha256 =
            Some(Sha256::digest(changed.initializing_receipt.as_ref().unwrap()).into());
        assert!(
            lease.write_state(&changed).is_err(),
            "different exact receipt was accepted"
        );
    }

    #[test]
    fn marker_retry_accepts_every_exact_prefix_without_replacing_inode() {
        let marker = StartupMarkerV1 {
            version: 1,
            installation_id: [1; 32],
        }
        .encode()
        .unwrap();
        for prefix in 0..=marker.len() {
            let (home, _mutation, mut lease, state) = setup_intent();
            let inode = lease.0.file.metadata().unwrap().ino();
            lease.0.file.write_all(&marker[..prefix]).unwrap();
            lease.0.file.sync_all().unwrap();
            if prefix < marker.len() {
                assert!(lease.marker().is_err());
            }
            lease.initialize_marker_from_intent(&state).unwrap();
            assert_eq!(lease.0.file.metadata().unwrap().ino(), inode);
            assert_eq!(
                lease.marker().unwrap().installation_id,
                state.installation_id
            );
            assert_eq!(
                std::fs::read(home.path().join(STARTUP_LOCK)).unwrap(),
                marker
            );
        }
    }

    #[test]
    fn marker_retry_requires_exact_durable_intent_receipt_and_prefix() {
        let (home, _mutation, mut lease, state) = setup_intent();
        lease.0.file.write_all(b"{oops").unwrap();
        assert!(lease.initialize_marker_from_intent(&state).is_err());
        assert_eq!(
            std::fs::read(home.path().join(STARTUP_LOCK)).unwrap(),
            b"{oops"
        );
        let (home, _mutation, mut lease, state) = setup_intent();
        storage::write_record(&home.path().join(".ww/installation.json"), b"{}").unwrap();
        assert!(lease.initialize_marker_from_intent(&state).is_err());
        assert_eq!(
            std::fs::metadata(home.path().join(STARTUP_LOCK))
                .unwrap()
                .len(),
            0
        );
    }

    #[test]
    fn initialization_completion_requires_the_exact_complete_receipt() {
        let (home, _mutation, mut lease, mut state) = setup_intent();
        lease.initialize_marker_from_intent(&state).unwrap();
        let mut receipt = Receipt::decode(state.initializing_receipt.as_ref().unwrap()).unwrap();
        state.initializing_receipt = None;
        state.initializing_receipt_sha256 = None;
        state.initializing_identity = None;
        assert!(
            lease.write_state(&state).is_err(),
            "initialization finished before complete receipt"
        );
        receipt.complete = true;
        storage::write_record(
            &home.path().join(".ww/installation.json"),
            &receipt.encode().unwrap(),
        )
        .unwrap();
        lease.write_state(&state).unwrap();
        assert_eq!(lease.read_state().unwrap(), Some(state));
    }

    #[test]
    fn missing_completed_marker_cannot_reinitialize() {
        let (home, mutation, mut lease, mut state) = setup_intent();
        lease.initialize_marker_from_intent(&state).unwrap();
        let mut receipt = Receipt::decode(state.initializing_receipt.as_ref().unwrap()).unwrap();
        receipt.complete = true;
        storage::write_record(
            &home.path().join(".ww/installation.json"),
            &receipt.encode().unwrap(),
        )
        .unwrap();
        state.initializing_receipt = None;
        state.initializing_receipt_sha256 = None;
        state.initializing_identity = None;
        lease.write_state(&state).unwrap();
        drop(lease);
        // Fault injection: the implementation never removes stable lock inodes.
        std::fs::remove_file(home.path().join(STARTUP_LOCK)).unwrap();
        let lease = StartupLease::exclusive_create(&mutation).unwrap();
        assert!(lease.write_state(&intent()).is_err());
    }

    #[test]
    fn lease_descriptors_close_on_exec() {
        let home = tempfile::tempdir().unwrap();
        let mutation = MutationLock::acquire(home.path()).unwrap();
        let lease = InstanceLease::exclusive_create(&mutation).unwrap();
        // SAFETY: fcntl reads flags from the valid owned descriptor.
        let flags = unsafe { libc::fcntl(lease.0.file.as_raw_fd(), libc::F_GETFD) };
        assert!(flags >= 0 && flags & libc::FD_CLOEXEC != 0);
        let mut child = std::process::Command::new("/bin/sh")
            .args(["-c", "read ignored"])
            .stdin(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        drop(lease);
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(1);
        let obtained = loop {
            if InstanceLease::exclusive(home.path()).is_ok() {
                break true;
            }
            if std::time::Instant::now() >= deadline {
                break false;
            }
            std::thread::sleep(std::time::Duration::from_millis(1));
        };
        drop(child.stdin.take());
        child.wait().unwrap();
        assert!(obtained, "exec child retained the instance lease");
    }
    fn managed_contract() -> StartupContractV1 {
        let prior = contract();
        StartupContractV1::new(
            prior.installation_id,
            2,
            prior.artifact,
            prior.startup,
            InvocationBinding::Managed {
                service: ServiceIdentity {
                    manager: ServiceManager::Systemd,
                    domain: "user/501".into(),
                    name: "ww.service".into(),
                    definition: [1; 32],
                    drop_ins: [2; 32],
                    environment_files: [3; 32],
                    effective_argv: [4; 32],
                    effective_environment: [5; 32],
                    registered: true,
                    enabled: EnabledState::Enabled,
                },
            },
            prior.trusted_release_keys,
        )
        .unwrap()
    }

    #[test]
    fn transaction_payload_refuses_replay_role_tuple_and_invocation_substitution() {
        let contract = managed_contract();
        let mut state = intent();
        state.initializing_receipt = None;
        state.initializing_receipt_sha256 = None;
        state.initializing_identity = None;
        let target = TargetTupleDigests {
            executable: contract.artifact.clone(),
            native_receipt: [3; 32],
            locator: [4; 32],
            proof: [0xbb; 32],
            contract: contract.identity.digest,
        };
        let payload = StartupPermitPayload {
            version: 1,
            transaction_id: [9; 32],
            installation_id: state.installation_id,
            role: PermitTarget::Candidate,
            target: target.clone(),
            contract: contract.identity.clone(),
            invocation: contract.invocation.clone(),
        };
        state.disposition = StartupDisposition::Transaction {
            transaction_id: payload.transaction_id,
            permit_digest: payload.digest().unwrap(),
            contract: contract.identity.clone(),
        };
        payload
            .validate_binding(&state, &contract, &target, &contract.invocation)
            .unwrap();
        assert_eq!(
            StartupPermitPayload::decode(&payload.encode().unwrap()).unwrap(),
            payload
        );
        let mut replay = payload.clone();
        replay.transaction_id = [10; 32];
        assert!(replay
            .validate_binding(&state, &contract, &target, &contract.invocation)
            .is_err());
        replay = payload.clone();
        replay.role = PermitTarget::RestoredBefore;
        assert!(replay
            .validate_binding(&state, &contract, &target, &contract.invocation)
            .is_err());
        let mut wrong_tuple = target.clone();
        wrong_tuple.locator = [5; 32];
        assert!(payload
            .validate_binding(&state, &contract, &wrong_tuple, &contract.invocation)
            .is_err());
        assert!(payload
            .validate_binding(&state, &contract, &target, &InvocationBinding::Standalone)
            .is_err());
        replay = payload.clone();
        replay.invocation = InvocationBinding::Standalone;
        assert!(replay.encode().is_err());
        state.disposition = StartupDisposition::Committed {
            contract: contract.identity.clone(),
        };
        assert!(payload
            .validate_binding(&state, &contract, &target, &contract.invocation)
            .is_err());
    }

    // This helper exercises persisted state only. It supplies no authenticated
    // constructor and never starts a Host or a service manager.
    fn run_initialization_records(home: &Path) -> Result<()> {
        let mutation = MutationLock::acquire(home)?;
        let _instance = InstanceLease::exclusive_create(&mutation)?;
        let mut lease = StartupLease::exclusive_create(&mutation)?;
        let intent = intent();
        let current = lease.read_state()?;
        if current
            .as_ref()
            .is_some_and(|state| state.initializing_receipt.is_none())
        {
            ensure!(
                lease.marker()?.installation_id == intent.installation_id,
                "marker changed"
            );
            return Ok(());
        }
        lease.write_state(&intent)?;
        let receipt_path = home.join(".ww/installation.json");
        let bytes = intent.initializing_receipt.as_ref().unwrap();
        let mut complete = Receipt::decode(bytes)?;
        complete.complete = true;
        let complete = complete.encode()?;
        let actual = private_record(&receipt_path)?;
        if actual.as_ref() != Some(&complete) {
            ensure!(
                actual.is_none() || actual.as_ref() == Some(bytes),
                "receipt changed"
            );
            storage::write_record(&receipt_path, bytes)?;
            lease.initialize_marker_from_intent(&intent)?;
            storage::write_record(&receipt_path, &complete)?;
        } else {
            ensure!(
                lease.marker()?.installation_id == intent.installation_id,
                "marker changed"
            );
        }
        let mut finished = intent;
        finished.initializing_receipt = None;
        finished.initializing_receipt_sha256 = None;
        finished.initializing_identity = None;
        lease.write_state(&finished)
    }

    fn initialization_home() -> tempfile::TempDir {
        let home = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(home.path().join(".ww/bin")).unwrap();
        storage::Paths::open(home.path()).unwrap();
        home
    }

    #[test]
    #[ignore = "subprocess worker; executed by startup crash matrix"]
    fn initialization_crash_worker() {
        let home = PathBuf::from(std::env::var_os("WW_STARTUP_CRASH_HOME").expect("worker home"));
        let point = std::env::var("WW_STARTUP_CRASH_POINT")
            .unwrap()
            .parse()
            .unwrap();
        storage::faults::exit_at(point);
        run_initialization_records(&home).unwrap();
        panic!("requested initialization checkpoint did not execute");
    }

    #[test]
    fn process_exit_at_every_initialization_record_boundary_recovers_same_intent() {
        let baseline = initialization_home();
        storage::faults::configure(None);
        run_initialization_records(baseline.path()).unwrap();
        let trace = storage::faults::trace();
        assert!(
            trace.len() >= 30,
            "initialization probes did not execute: {trace:?}"
        );
        assert!(trace
            .iter()
            .any(|point| point == "startup.marker.write.after"));
        eprintln!(
            "startup initialization: {} process-exit checkpoints",
            trace.len()
        );
        for (index, point) in trace.iter().enumerate() {
            let home = initialization_home();
            let output = std::process::Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "upgrade::startup::tests::initialization_crash_worker",
                    "--ignored",
                    "--nocapture",
                ])
                .env("WW_STARTUP_CRASH_HOME", home.path())
                .env("WW_STARTUP_CRASH_POINT", (index + 1).to_string())
                .output()
                .unwrap();
            assert_eq!(
                output.status.code(),
                Some(86),
                "checkpoint {} {point}: {}",
                index + 1,
                String::from_utf8_lossy(&output.stderr)
            );
            storage::faults::configure(None);
            run_initialization_records(home.path())
                .unwrap_or_else(|error| panic!("recover {point}: {error:#}"));
            let lease = StartupLease::shared(home.path()).unwrap();
            let state = lease.read_state().unwrap().unwrap();
            assert_eq!(state.installation_id, intent().installation_id);
            assert_eq!(state.native, intent().native);
            assert_eq!(state.disposition, StartupDisposition::RequiredUnbound);
            assert!(state.initializing_receipt.is_none());
            assert_eq!(
                lease.marker().unwrap().installation_id,
                state.installation_id
            );
            assert!(Receipt::load(&home.path().join(".ww")).is_ok());
        }
    }
}
