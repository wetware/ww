//! Ed25519 key management for wetware hosts.
//!
//! A single Ed25519 keypair serves as the node's identity:
//! - libp2p PeerId (via libp2p's ed25519 support)
//! - Membrane Signer for epoch-scoped session authentication
//!
//! Operator identity (secp256k1 for on-chain Stem contract ownership) is a
//! separate concern managed outside the node runtime.
//!
//! Keys are stored as base58btc (Bitcoin alphabet, ~44 chars for 32 bytes)
//! on the local filesystem, aligning with the libp2p ecosystem.
#![cfg(not(target_arch = "wasm32"))]

use anyhow::{bail, Context, Result};
use base58::{FromBase58, ToBase58};
use ed25519_dalek::SigningKey;
use libp2p::identity::Keypair;
use std::io::Write;
use std::path::Path;

/// Generate a new random Ed25519 signing key using the OS CSPRNG.
pub fn generate() -> Result<SigningKey> {
    use rand::TryRngCore;
    let mut secret_bytes = [0u8; 32];
    rand::rngs::OsRng
        .try_fill_bytes(&mut secret_bytes)
        .context("OS CSPRNG failed")?;
    Ok(SigningKey::from_bytes(&secret_bytes))
}

/// Encode a signing key as a base58btc string.
pub fn encode(sk: &SigningKey) -> String {
    sk.to_bytes().to_base58()
}

/// Convert an Ed25519 [`SigningKey`] into a libp2p [`Keypair`].
///
/// The resulting keypair can be used directly with [`SwarmBuilder::with_existing_identity`].
pub fn to_libp2p(sk: &SigningKey) -> Result<Keypair> {
    let kp = libp2p::identity::ed25519::Keypair::try_from_bytes(&mut sk.to_keypair_bytes())
        .context("failed to convert Ed25519 key to libp2p identity")?;
    Ok(Keypair::from(kp))
}

/// Decode a base58btc string into a 32-byte signing key.
fn decode(s: &str) -> Result<SigningKey> {
    let bytes = s
        .from_base58()
        .map_err(|_| anyhow::anyhow!("key must be base58btc-encoded (~44 chars)"))?;

    if bytes.len() != 32 {
        bail!("expected 32-byte key, got {} bytes", bytes.len());
    }

    let mut arr = [0u8; 32];
    arr.copy_from_slice(&bytes);
    Ok(SigningKey::from_bytes(&arr))
}

/// Load an Ed25519 private key from a local filesystem path (base58btc).
pub fn load(path: &str) -> Result<SigningKey> {
    let contents =
        std::fs::read_to_string(path).with_context(|| format!("read key file: {path}"))?;
    decode(contents.trim()).with_context(|| format!("invalid key in {path}"))
}

/// Write a base58btc-encoded Ed25519 private key to disk.
///
/// The replacement is written with mode 0600, synced, renamed atomically, and
/// followed by a parent-directory sync. Parent directories created by this
/// function use mode 0700 on Unix.
pub fn save(sk: &SigningKey, path: &Path) -> Result<()> {
    atomic_write_private(path, encode(sk).as_bytes())
        .with_context(|| format!("write key: {}", path.display()))
}

/// Atomically replace one trusted private-state file.
///
/// Wetware supports one running process per private state directory. This
/// helper provides crash durability, not multiprocess coordination or
/// protection against malicious filesystem rollback.
pub fn atomic_write_private(path: &Path, bytes: &[u8]) -> Result<()> {
    atomic_write_private_with(path, |file| file.write_all(bytes))
}

fn atomic_write_private_with(
    path: &Path,
    write: impl FnOnce(&mut std::fs::File) -> std::io::Result<()>,
) -> Result<()> {
    use std::fs::OpenOptions;

    #[cfg(unix)]
    use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};

    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let parent_existed = parent.exists();
    std::fs::create_dir_all(parent)
        .with_context(|| format!("create private-state directory: {}", parent.display()))?;
    #[cfg(unix)]
    if !parent_existed {
        std::fs::set_permissions(parent, std::fs::Permissions::from_mode(0o700))
            .with_context(|| format!("restrict private-state directory: {}", parent.display()))?;
    }

    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .context("private-state path has no UTF-8 file name")?;
    let mut temporary = None;
    for _ in 0..128 {
        let candidate = parent.join(format!(
            ".{name}.ww-tmp-{}-{:016x}",
            std::process::id(),
            rand::random::<u64>()
        ));
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        options.mode(0o600);
        match options.open(&candidate) {
            Ok(file) => {
                temporary = Some((candidate, file));
                break;
            }
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => {
                return Err(error).with_context(|| {
                    format!("create temporary private-state file for {}", path.display())
                });
            }
        }
    }
    let (temporary_path, mut file) =
        temporary.context("could not allocate private-state temp file")?;

    let result = (|| -> Result<()> {
        write(&mut file).with_context(|| format!("write {}", temporary_path.display()))?;
        file.sync_all()
            .with_context(|| format!("sync {}", temporary_path.display()))?;
        drop(file);
        std::fs::rename(&temporary_path, path).with_context(|| {
            format!(
                "replace private-state file {} with {}",
                path.display(),
                temporary_path.display()
            )
        })?;
        std::fs::File::open(parent)
            .and_then(|directory| directory.sync_all())
            .with_context(|| format!("sync private-state directory: {}", parent.display()))?;
        Ok(())
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&temporary_path);
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_base58() {
        let sk = generate().unwrap();
        let encoded = encode(&sk);
        let decoded = decode(&encoded).unwrap();
        assert_eq!(sk.to_bytes(), decoded.to_bytes());
    }

    #[test]
    fn hex_encoding_rejected() {
        let sk = generate().unwrap();
        let hex_str = hex::encode(sk.to_bytes());
        // Hex is not accepted — base58btc only.
        assert!(decode(&hex_str).is_err());
    }

    #[test]
    fn base58_is_shorter_than_hex() {
        let sk = generate().unwrap();
        let b58 = encode(&sk);
        let hex_str = hex::encode(sk.to_bytes());
        assert!(
            b58.len() < hex_str.len(),
            "base58 ({}) should be shorter than hex ({})",
            b58.len(),
            hex_str.len()
        );
    }

    #[test]
    fn invalid_encoding_rejected() {
        assert!(decode("not-valid-anything!!!").is_err());
    }

    #[test]
    fn wrong_length_rejected() {
        let short = [1u8; 16].to_base58();
        assert!(decode(&short).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn save_is_restrictive_and_load_compatible() {
        use std::os::unix::fs::PermissionsExt;

        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("private/identity");
        let key = generate().unwrap();
        save(&key, &path).unwrap();

        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert_eq!(
            std::fs::metadata(path.parent().unwrap())
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o700
        );
        assert_eq!(
            load(path.to_str().unwrap()).unwrap().to_bytes(),
            key.to_bytes()
        );
    }

    #[test]
    fn interrupted_write_preserves_canonical_file() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("identity");
        std::fs::write(&path, b"canonical").unwrap();

        let error = atomic_write_private_with(&path, |file| {
            file.write_all(b"partial")?;
            Err(std::io::Error::other("interrupted"))
        })
        .unwrap_err();

        assert!(format!("{error:#}").contains("interrupted"));
        assert_eq!(std::fs::read(&path).unwrap(), b"canonical");
        assert_eq!(std::fs::read_dir(directory.path()).unwrap().count(), 1);
    }
}
