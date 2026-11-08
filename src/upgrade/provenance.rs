//! Signed native generations and the separate, untrusted installation locator.
//!
//! Decoding a record never authenticates a release. A new generation must use
//! independently provisioned current keys, even when a record carries old keys.

use anyhow::{ensure, Context, Result};
use cid::Cid;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::fs::OpenOptions;
use std::io::Read;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::{Component, Path, PathBuf};

use crate::release::auth::TrustedKeys;
use crate::release::descriptor::SignedReleaseProof;

use super::{ArtifactIdentity, PreverifiedCandidate};

/// An independently provisioned current trust source. The persisted path binds
/// the source, not its contents: the owner can rotate or remove public keys.
/// The file contains public keys only. Same-UID and root edits are inside the
/// installation's existing private-file custody boundary.
pub struct IndependentTrustSource {
    path: PathBuf,
    keyring: Vec<u8>,
}

impl IndependentTrustSource {
    /// Resolve an explicitly supplied provisioning path once. Later reads must
    /// use `read_bound` so replacing a component with a symlink cannot redirect
    /// a persisted trust binding.
    pub fn from_path(path: &Path) -> Result<Self> {
        let path = resolve_trust_path(path)?;
        Self::read_bound(&path)
    }

    pub fn read_bound(path: &Path) -> Result<Self> {
        ensure!(path.is_absolute(), "release trust path must be absolute");
        ensure!(
            path.components()
                .all(|c| matches!(c, Component::RootDir | Component::Normal(_))),
            "release trust path must be normalized"
        );
        // SAFETY: geteuid takes no pointers.
        let uid = unsafe { libc::geteuid() };
        let parent = path.parent().context("release trust path has no parent")?;
        let mut prefix = PathBuf::new();
        for component in parent.components() {
            prefix.push(component.as_os_str());
            trust_directory(&prefix, uid)?;
        }
        let mut file = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_NONBLOCK)
            .open(path)
            .context("open independently provisioned release keyring")?;
        let metadata = file.metadata()?;
        ensure!(metadata.is_file() && metadata.nlink() == 1
            && (metadata.uid() == 0 || metadata.uid() == uid) && metadata.mode() & 0o022 == 0,
            "release keyring must be a single-link regular file owned by root or this user without other-user writes");
        ensure!(metadata.len() <= 122, "release keyring exceeds size bound");
        let mut keyring = Vec::new();
        (&mut file).take(123).read_to_end(&mut keyring)?;
        TrustedKeys::from_bytes(&keyring)?;
        let after = file.metadata()?;
        let bound =
            std::fs::symlink_metadata(path).context("reobserve independent keyring path")?;
        ensure!(
            metadata.dev() == after.dev()
                && metadata.ino() == after.ino()
                && metadata.len() == after.len()
                && metadata.mtime() == after.mtime()
                && metadata.mtime_nsec() == after.mtime_nsec()
                && metadata.ctime() == after.ctime()
                && metadata.ctime_nsec() == after.ctime_nsec()
                && bound.is_file()
                && bound.dev() == after.dev()
                && bound.ino() == after.ino(),
            "release keyring changed during read"
        );
        Ok(Self {
            path: path.to_owned(),
            keyring,
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }
    pub fn keyring(&self) -> &[u8] {
        &self.keyring
    }
}

fn trust_directory(path: &Path, uid: u32) -> Result<()> {
    let metadata = std::fs::symlink_metadata(path).context("inspect release trust directory")?;
    // Root-owned sticky temporary ancestors protect the following owned entry.
    // Every descendant is checked independently.
    let protected = metadata.mode() & 0o022 == 0
        || (metadata.uid() == 0 && metadata.mode() & libc::S_ISVTX as u32 != 0);
    ensure!(metadata.is_dir() && (metadata.uid() == 0 || metadata.uid() == uid) && protected,
        "release trust directory must be owned by root or this user without symlinks or unprotected other-user writes");
    Ok(())
}

/// Check the complete provisioning route, including intermediate alias targets.
/// Checking only the final canonical path would overlook an alias that another
/// user can redirect to an unrelated root-owned or owner-owned public key.
fn resolve_trust_path(path: &Path) -> Result<PathBuf> {
    use std::collections::VecDeque;
    let absolute = if path.is_absolute() {
        path.to_owned()
    } else {
        std::env::current_dir()?.join(path)
    };
    let mut pending: VecDeque<_> = absolute
        .components()
        .map(|c| c.as_os_str().to_owned())
        .collect();
    let mut resolved = PathBuf::from("/");
    let mut links = 0;
    // SAFETY: geteuid takes no pointers.
    let uid = unsafe { libc::geteuid() };
    while let Some(component) = pending.pop_front() {
        match component.as_encoded_bytes() {
            b"/" => {
                resolved = PathBuf::from("/");
                continue;
            }
            b"." => continue,
            b".." => {
                trust_directory(&resolved, uid)?;
                resolved.pop();
                continue;
            }
            _ => {}
        }
        trust_directory(&resolved, uid)?;
        let next = resolved.join(&component);
        let metadata = std::fs::symlink_metadata(&next).context("resolve release trust path")?;
        if metadata.file_type().is_symlink() {
            links += 1;
            ensure!(links <= 40, "release trust path contains too many symlinks");
            ensure!(
                metadata.uid() == 0 || metadata.uid() == uid,
                "release trust alias is not owned by root or this user"
            );
            let target = std::fs::read_link(&next)?;
            for part in target.components().rev() {
                pending.push_front(part.as_os_str().to_owned());
            }
        } else {
            resolved = next;
        }
    }
    Ok(resolved)
}

/// Compact v1 transport-root locator consumed by shell recovery. This is not
/// signed evidence and must not be substituted for a generation's proof.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct InstalledRelease {
    pub version: u8,
    pub release: String,
}

impl InstalledRelease {
    pub fn from_root(root: Cid) -> Result<Self> {
        validate_root(root)?;
        Ok(Self {
            version: 1,
            release: root.to_string(),
        })
    }

    pub fn root(&self) -> Result<Cid> {
        ensure!(
            self.version == 1,
            "unsupported installed release locator version"
        );
        let root = self
            .release
            .parse()
            .context("invalid installed release root")?;
        validate_root(root)?;
        Ok(root)
    }
}

/// An immutable proof file reference. The root identifies release transport;
/// SHA-256 and size identify the exact retained evidence encoding.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ProofReference {
    pub root: String,
    pub size: u64,
    pub sha256: String,
}

impl ProofReference {
    pub fn validate(&self) -> Result<()> {
        canonical_root(&self.root)?;
        ensure!(
            self.size > 0 && self.size <= 8 * 1024 * 1024,
            "retained release proof exceeds its size bound"
        );
        ensure!(
            canonical_digest(&self.sha256),
            "invalid retained proof digest"
        );
        Ok(())
    }

    pub fn verify(&self, bytes: &[u8]) -> Result<SignedReleaseProof> {
        self.validate()?;
        ensure!(
            bytes.len() as u64 == self.size,
            "retained proof size changed"
        );
        ensure!(
            hex::encode(Sha256::digest(bytes)) == self.sha256,
            "retained proof bytes changed"
        );
        let proof = SignedReleaseProof::decode(bytes)?;
        ensure!(
            proof.transport_root().to_string() == self.root,
            "retained proof transport root changed"
        );
        Ok(proof)
    }
}

/// Untrusted journal/retention data. `verification_keys` are public historical
/// evidence, not authority to admit a new candidate. Only committed retention
/// may authorize their use for recovery of that exact generation.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct GenerationRecord {
    pub version: u32,
    pub transport_root: String,
    pub descriptor: String,
    pub artifact: ArtifactIdentity,
    pub target: String,
    pub release_version: String,
    pub revision: String,
    pub proof: ProofReference,
    pub verification_keys: Vec<u8>,
}

impl GenerationRecord {
    pub fn validate(&self) -> Result<()> {
        ensure!(self.version == 1, "unsupported native generation version");
        canonical_root(&self.transport_root)?;
        let descriptor: Cid = self.descriptor.parse().context("invalid descriptor CID")?;
        ensure!(
            descriptor.to_string() == self.descriptor
                && descriptor.codec() == 0x71
                && descriptor.hash().code() == 0x12
                && descriptor.hash().size() == 32,
            "native descriptor must be a canonical SHA-256 DAG-CBOR CID"
        );
        self.artifact.validate()?;
        crate::release::descriptor::platform_path(&self.target)?;
        ensure!(
            !self.release_version.is_empty()
                && self.release_version.len() <= 128
                && self
                    .release_version
                    .bytes()
                    .all(|byte| byte.is_ascii_graphic()),
            "invalid native release version"
        );
        ensure!(
            self.revision.len() == 40
                && self
                    .revision
                    .bytes()
                    .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)),
            "invalid native release revision"
        );
        self.proof.validate()?;
        ensure!(
            self.proof.root == self.transport_root,
            "native proof root mismatch"
        );
        TrustedKeys::from_bytes(&self.verification_keys)?;
        Ok(())
    }
}

/// Authenticated release selection with retained, root-bound signed evidence.
/// Private fields and the absence of `Deserialize` prevent record decoding from
/// constructing authenticated state. This type does not attest running bytes.
pub struct AuthenticatedGeneration {
    record: GenerationRecord,
    proof_bytes: Vec<u8>,
    selected: PreverifiedCandidate,
}

impl std::fmt::Debug for AuthenticatedGeneration {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("AuthenticatedGeneration")
            .field("transport_root", &self.record.transport_root)
            .field("artifact", &self.record.artifact)
            .finish_non_exhaustive()
    }
}

impl AuthenticatedGeneration {
    pub fn from_proof(
        proof: SignedReleaseProof,
        independently_provisioned_keyring: &[u8],
        target: &str,
    ) -> Result<Self> {
        let keys = TrustedKeys::from_bytes(independently_provisioned_keyring)?;
        let authenticated = proof.authenticate(&keys)?;
        let selected = PreverifiedCandidate::from_release(&authenticated, target)?;
        let proof_bytes = proof.encode()?;
        let root = proof.transport_root().to_string();
        let record = GenerationRecord {
            version: 1,
            transport_root: root.clone(),
            descriptor: authenticated.descriptor_cid().to_string(),
            artifact: selected.artifact().clone(),
            target: target.to_owned(),
            release_version: selected.version.clone(),
            revision: selected.revision.clone(),
            proof: ProofReference {
                root,
                size: proof_bytes.len() as u64,
                sha256: hex::encode(Sha256::digest(&proof_bytes)),
            },
            verification_keys: independently_provisioned_keyring.to_vec(),
        };
        record.validate()?;
        Ok(Self {
            record,
            proof_bytes,
            selected,
        })
    }

    pub fn artifact(&self) -> &ArtifactIdentity {
        &self.record.artifact
    }

    pub fn record(&self) -> &GenerationRecord {
        &self.record
    }

    pub fn proof_bytes(&self) -> &[u8] {
        &self.proof_bytes
    }

    pub fn selection(&self) -> &PreverifiedCandidate {
        &self.selected
    }
}

fn canonical_digest(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

fn validate_root(root: Cid) -> Result<()> {
    ensure!(
        root.codec() == 0x70 && root.hash().code() == 0x12 && root.hash().size() == 32,
        "release root must be a SHA-256 DAG-PB CID"
    );
    Ok(())
}

fn canonical_root(value: &str) -> Result<Cid> {
    let root: Cid = value.parse().context("invalid release transport root")?;
    validate_root(root)?;
    ensure!(
        root.to_string() == value,
        "noncanonical release transport root"
    );
    Ok(root)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::{symlink, PermissionsExt};

    const PUBLIC_KEY: &[u8] = b"MCowBQYDK2VwAyEA11qYAYKxCrfVS/7TyWQHOg7hcvPapiMlrwIaaPcHURo=\n";

    #[test]
    fn trust_binding_rereads_current_file_and_refuses_removed_or_redirected_keys() {
        let home = tempfile::tempdir().unwrap();
        let path = home.path().join("release-keys");
        std::fs::write(&path, PUBLIC_KEY).unwrap();
        let source = IndependentTrustSource::from_path(&path).unwrap();
        assert!(source.path().is_absolute());
        assert_eq!(source.keyring(), PUBLIC_KEY);
        let replacement = home.path().join("replacement");
        std::fs::write(&replacement, PUBLIC_KEY).unwrap();
        std::fs::rename(&replacement, &path).unwrap();
        assert_eq!(
            IndependentTrustSource::read_bound(source.path())
                .unwrap()
                .keyring(),
            PUBLIC_KEY
        );
        std::fs::write(&path, b"").unwrap();
        assert!(IndependentTrustSource::read_bound(source.path()).is_err());
        std::fs::remove_file(&path).unwrap();
        assert!(IndependentTrustSource::read_bound(source.path()).is_err());
        std::fs::write(&replacement, PUBLIC_KEY).unwrap();
        symlink(&replacement, &path).unwrap();
        assert!(IndependentTrustSource::read_bound(source.path()).is_err());
    }

    #[test]
    fn trust_binding_rejects_unsafe_custody_and_oversized_keys() {
        let home = tempfile::tempdir().unwrap();
        let path = home.path().join("release-keys");
        std::fs::write(&path, PUBLIC_KEY).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o666)).unwrap();
        assert!(IndependentTrustSource::from_path(&path).is_err());
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        let other = home.path().join("hardlink");
        std::fs::hard_link(&path, &other).unwrap();
        assert!(IndependentTrustSource::from_path(&path).is_err());
        std::fs::remove_file(&other).unwrap();
        std::fs::write(&path, vec![b'x'; 123]).unwrap();
        assert!(IndependentTrustSource::from_path(&path).is_err());
        std::fs::write(&path, PUBLIC_KEY).unwrap();
        let directory = home.path().join("writable");
        std::fs::create_dir(&directory).unwrap();
        std::fs::set_permissions(&directory, std::fs::Permissions::from_mode(0o777)).unwrap();
        let bad = directory.join("keys");
        std::fs::write(&bad, PUBLIC_KEY).unwrap();
        assert!(IndependentTrustSource::from_path(&bad).is_err());
        let alias = directory.join("alias");
        symlink(&path, &alias).unwrap();
        assert!(IndependentTrustSource::from_path(&alias).is_err());
        assert!(IndependentTrustSource::read_bound(Path::new("release-keys")).is_err());
    }

    #[test]
    fn locator_preserves_compact_shell_schema_without_claiming_authentication() {
        let root = Cid::new_v1(
            0x70,
            cid::multihash::Multihash::wrap(0x12, &[7; 32]).unwrap(),
        );
        let locator = InstalledRelease::from_root(root).unwrap();
        assert_eq!(
            serde_json::to_string(&locator).unwrap(),
            format!("{{\"version\":1,\"release\":\"{root}\"}}")
        );
        assert_eq!(locator.root().unwrap(), root);
    }
}
