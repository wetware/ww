//! Verify exact release manifest bytes against independently provisioned public keys.
//!
//! The caller must obtain the keyring through a trusted channel, never from the
//! release tree. Authentication authorizes bytes; it does not establish freshness.
//! Native upgrades remain disabled until the separate Host upgrade implementation.

use std::collections::BTreeMap;

use anyhow::{bail, ensure, Context, Result};
use base64::{engine::general_purpose::STANDARD, Engine as _};
use ed25519_dalek::{Signature, VerifyingKey};
use sha2::{Digest, Sha256};

/// Upper bound shared with the shell installer.
pub const MAX_MANIFEST_BYTES: usize = 1024 * 1024;
const SPKI_PREFIX: &[u8] = &[
    0x30, 0x2a, 0x30, 0x05, 0x06, 0x03, 0x2b, 0x65, 0x70, 0x03, 0x21, 0x00,
];

/// One release key, or two keys during an explicitly managed rotation.
///
/// This type contains only public keys. Provisioning and removal are operator
/// actions outside release discovery. A release cannot extend this keyring.
pub struct TrustedKeys(Vec<VerifyingKey>);

impl TrustedKeys {
    /// Parse one or two LF-terminated canonical base64 Ed25519 SPKI DER records.
    pub fn from_bytes(bytes: &[u8]) -> Result<Self> {
        ensure!(
            (bytes.len() == 61 || bytes.len() == 122) && bytes.ends_with(b"\n"),
            "release keyring must contain one or two public keys"
        );
        let text = std::str::from_utf8(bytes).context("release keyring is not UTF-8")?;
        let mut keys = Vec::new();
        for line in text[..text.len() - 1].split('\n') {
            ensure!(line.len() == 60, "invalid release public key length");
            let der = STANDARD
                .decode(line)
                .context("invalid release public key base64")?;
            ensure!(
                STANDARD.encode(&der) == line,
                "noncanonical release public key base64"
            );
            ensure!(
                der.len() == 44 && der.starts_with(SPKI_PREFIX),
                "release public key must be Ed25519 SPKI"
            );
            let raw: [u8; 32] = der[SPKI_PREFIX.len()..].try_into()?;
            let key =
                VerifyingKey::from_bytes(&raw).context("invalid Ed25519 release public key")?;
            ensure!(!key.is_weak(), "weak Ed25519 release public key");
            ensure!(!keys.contains(&key), "duplicate release public key");
            keys.push(key);
        }
        ensure!((1..=2).contains(&keys.len()), "invalid release key count");
        Ok(Self(keys))
    }

    /// Authenticate the exact manifest bytes before parsing any checksum.
    ///
    /// The signature is a raw 64-byte pure Ed25519 detached signature. No
    /// Ed25519ph prehash, newline conversion, or checksum fallback is accepted.
    pub fn authenticate(&self, manifest: &[u8], signature: &[u8]) -> Result<AuthenticatedManifest> {
        ensure!(
            !manifest.is_empty() && manifest.len() <= MAX_MANIFEST_BYTES,
            "invalid release manifest size"
        );
        let signature = Signature::from_slice(signature)
            .context("release signature must contain exactly 64 bytes")?;
        ensure!(
            self.0
                .iter()
                .any(|key| key.verify_strict(manifest, &signature).is_ok()),
            "release manifest signature is not authorized"
        );
        AuthenticatedManifest::parse(manifest)
    }
}

/// Checksum entries from an authenticated, strictly parsed SHA-256 manifest.
///
/// Private fields prevent callers from constructing authenticated state from an
/// unverified manifest. Artifact paths remain strings; this module performs no I/O.
pub struct AuthenticatedManifest {
    sha256: BTreeMap<String, [u8; 32]>,
}

impl AuthenticatedManifest {
    fn parse(bytes: &[u8]) -> Result<Self> {
        ensure!(
            bytes.is_ascii() && bytes.ends_with(b"\n"),
            "release manifest must be ASCII with a final LF"
        );
        let text = std::str::from_utf8(bytes)?;
        let body = text
            .strip_prefix("# sha256\n")
            .context("release manifest must explicitly select sha256")?;
        let mut sha256 = BTreeMap::new();
        for line in body
            .strip_suffix('\n')
            .context("release manifest has no checksum entries")?
            .split('\n')
        {
            let (hash, path) = line
                .split_once("  ")
                .context("invalid release checksum record")?;
            ensure!(
                hash.len() == 64
                    && hash
                        .bytes()
                        .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)),
                "invalid SHA-256 digest"
            );
            ensure!(valid_path(path), "invalid release artifact path");
            let mut digest = [0; 32];
            hex::decode_to_slice(hash, &mut digest)?;
            ensure!(
                sha256.insert(path.to_owned(), digest).is_none(),
                "duplicate release artifact path"
            );
        }
        ensure!(
            !sha256.is_empty(),
            "release manifest has no checksum entries"
        );
        Ok(Self { sha256 })
    }

    /// Return the expected SHA-256 digest for a required artifact.
    pub fn sha256_for(&self, path: &str) -> Result<&[u8; 32]> {
        self.sha256
            .get(path)
            .with_context(|| format!("missing release checksum entry: {path}"))
    }

    /// Verify fetched bytes before execution, installation, or other consumption.
    pub fn verify_artifact(&self, path: &str, bytes: &[u8]) -> Result<()> {
        let expected = self.sha256_for(path)?;
        let actual: [u8; 32] = Sha256::digest(bytes).into();
        if &actual != expected {
            bail!("release SHA-256 mismatch: {path}");
        }
        Ok(())
    }
}

fn valid_path(path: &str) -> bool {
    !path.is_empty()
        && path
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"._/-".contains(&b))
        && path
            .split('/')
            .all(|component| !matches!(component, "" | "." | ".."))
}

#[cfg(test)]
mod tests {
    use super::*;
    use ed25519_dalek::{Signer, SigningKey};
    use sha2::{Digest, Sha256};

    fn keyring(keys: &[SigningKey]) -> Vec<u8> {
        let mut output = String::new();
        for key in keys {
            let mut der = vec![
                0x30, 0x2a, 0x30, 0x05, 0x06, 0x03, 0x2b, 0x65, 0x70, 0x03, 0x21, 0x00,
            ];
            der.extend(key.verifying_key().as_bytes());
            output.push_str(&STANDARD.encode(der));
            output.push('\n');
        }
        output.into_bytes()
    }

    fn manifest(path: &str, bytes: &[u8]) -> Vec<u8> {
        format!("# sha256\n{}  {path}\n", hex::encode(Sha256::digest(bytes))).into_bytes()
    }

    fn authenticate(key: &SigningKey, bytes: &[u8]) -> Result<AuthenticatedManifest> {
        TrustedKeys::from_bytes(&keyring(std::slice::from_ref(key)))?
            .authenticate(bytes, &key.sign(bytes).to_bytes())
    }

    #[test]
    fn valid_signature_and_matching_artifact() {
        let key = SigningKey::from_bytes(&[1; 32]);
        let verified = authenticate(&key, &manifest("bin/ww/linux/x86_64/ww", b"host")).unwrap();
        verified
            .verify_artifact("bin/ww/linux/x86_64/ww", b"host")
            .unwrap();
        assert!(verified
            .verify_artifact("bin/ww/linux/x86_64/ww", b"malicious host")
            .is_err());
        assert!(verified
            .verify_artifact("bin/ww/macos/aarch64/ww", b"host")
            .is_err());
    }

    #[test]
    fn exact_bytes_and_attacker_checksums_cannot_reuse_signature() {
        let key = SigningKey::from_bytes(&[1; 32]);
        let attacker = SigningKey::from_bytes(&[2; 32]);
        let trusted = TrustedKeys::from_bytes(&keyring(std::slice::from_ref(&key))).unwrap();
        let original = manifest("bin/ww/linux/x86_64/ww", b"host");
        let signature = key.sign(&original).to_bytes();
        for changed in [
            manifest("bin/ww/linux/x86_64/ww", b"attacker host"),
            [original.as_slice(), b"\n"].concat(),
            String::from_utf8(original.clone())
                .unwrap()
                .replace('\n', "\r\n")
                .into_bytes(),
        ] {
            assert!(trusted.authenticate(&changed, &signature).is_err());
            assert!(trusted
                .authenticate(&changed, &attacker.sign(&changed).to_bytes())
                .is_err());
        }
        assert!(trusted
            .authenticate(&original, &attacker.sign(&original).to_bytes())
            .is_err());
    }

    #[test]
    fn every_truncated_signature_and_trailing_bytes_fail() {
        let key = SigningKey::from_bytes(&[3; 32]);
        let trusted = TrustedKeys::from_bytes(&keyring(std::slice::from_ref(&key))).unwrap();
        let bytes = manifest("host", b"host");
        let signature = key.sign(&bytes).to_bytes();
        for size in 0..64 {
            assert!(
                trusted.authenticate(&bytes, &signature[..size]).is_err(),
                "length {size}"
            );
        }
        assert!(trusted
            .authenticate(&bytes, &[signature.as_slice(), b"\n"].concat())
            .is_err());
        assert!(trusted.authenticate(&bytes, &[0; 64]).is_err());
        for offset in 0..64 {
            let mut changed = signature;
            changed[offset] ^= 1;
            assert!(
                trusted.authenticate(&bytes, &changed).is_err(),
                "offset {offset}"
            );
        }
        let mut noncanonical_s = signature;
        noncanonical_s[63] |= 0x80;
        assert!(trusted.authenticate(&bytes, &noncanonical_s).is_err());
    }

    #[test]
    fn one_key_and_two_key_rotation_are_explicit() {
        let old = SigningKey::from_bytes(&[4; 32]);
        let new = SigningKey::from_bytes(&[5; 32]);
        let bytes = manifest("host", b"host");
        let overlap = TrustedKeys::from_bytes(&keyring(&[old.clone(), new.clone()])).unwrap();
        for key in [&old, &new] {
            overlap
                .authenticate(&bytes, &key.sign(&bytes).to_bytes())
                .unwrap();
        }
        let after = TrustedKeys::from_bytes(&keyring(std::slice::from_ref(&new))).unwrap();
        assert!(after
            .authenticate(&bytes, &old.sign(&bytes).to_bytes())
            .is_err());
        after
            .authenticate(&bytes, &new.sign(&bytes).to_bytes())
            .unwrap();
        let before = TrustedKeys::from_bytes(&keyring(&[old])).unwrap();
        assert!(before
            .authenticate(&bytes, &new.sign(&bytes).to_bytes())
            .is_err());
    }

    #[test]
    fn malformed_unbounded_duplicate_and_weak_keyrings_fail() {
        let key = SigningKey::from_bytes(&[6; 32]);
        let encoded = keyring(std::slice::from_ref(&key));
        let mut wrong_oid = STANDARD.decode(&encoded[..60]).unwrap();
        wrong_oid[8] = 0x71;
        let wrong_oid = format!("{}\n", STANDARD.encode(wrong_oid)).into_bytes();
        let mut identity_point = SPKI_PREFIX.to_vec();
        identity_point.extend([0; 32]);
        identity_point[12] = 1;
        let weak = format!("{}\n", STANDARD.encode(identity_point)).into_bytes();
        let invalid = vec![
            vec![],
            vec![b'x'; 122],
            encoded[..60].to_vec(),
            [encoded.as_slice(), b"\n"].concat(),
            String::from_utf8(encoded.clone())
                .unwrap()
                .replace('\n', "\r\n")
                .into_bytes(),
            keyring(&[key.clone(), key.clone()]),
            keyring(&[
                key,
                SigningKey::from_bytes(&[7; 32]),
                SigningKey::from_bytes(&[8; 32]),
            ]),
            wrong_oid,
            weak,
        ];
        for bytes in invalid {
            assert!(
                TrustedKeys::from_bytes(&bytes).is_err(),
                "accepted {bytes:?}"
            );
        }
    }

    #[test]
    fn small_order_public_keys_and_noncanonical_aliases_fail() {
        // Five Y coordinates cover the eight canonical torsion points. The last
        // two coordinates are noncanonical encodings of Y=0 and Y=1.
        for y in [
            "0000000000000000000000000000000000000000000000000000000000000000",
            "0100000000000000000000000000000000000000000000000000000000000000",
            "26e8958fc2b227b045c3f489f2ef98f0d5dfac05d3c63339b13802886d53fc05",
            "c7176a703d4dd84fba3c0b760d10670f2a2053fa2c39ccc64ec7fd7792ac037a",
            "ecffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff7f",
            "edffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff7f",
            "eeffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff7f",
        ] {
            for sign_bit in [0, 0x80] {
                let mut point = hex::decode(y).unwrap();
                point[31] |= sign_bit;
                let der = [SPKI_PREFIX, &point].concat();
                let keyring = format!("{}\n", STANDARD.encode(der));
                assert!(
                    TrustedKeys::from_bytes(keyring.as_bytes()).is_err(),
                    "accepted {y}, {sign_bit}"
                );
            }
        }
    }

    #[test]
    fn signed_malformed_manifests_do_not_become_authenticated_checksums() {
        let key = SigningKey::from_bytes(&[9; 32]);
        let valid = String::from_utf8(manifest("host", b"host")).unwrap();
        let record = valid.strip_prefix("# sha256\n").unwrap();
        let digest = hex::encode(Sha256::digest(b"host"));
        let mut invalid = vec![
            String::new(),
            "# sha256\n".into(),
            format!("# sha256\n{record}{record}"),
            valid.replace("# sha256", "# blake3"),
            valid.replace("# sha256", "# SHA256"),
            record.to_owned(),
            valid.trim_end_matches('\n').to_owned(),
            valid.replace('\n', "\r\n"),
            valid.replace("  ", " "),
            valid.replace("  ", "\t"),
            valid.replace(&digest, &digest.to_uppercase()),
            format!("# sha256\n{digest}  host extra\n"),
            format!("{valid}# blake3\n{record}"),
            format!("{valid}\n"),
        ];
        for path in [
            "", "/host", "host/", "a//b", ".", "..", "a/../b", "./host", "a/./b", "host\t",
            "host\0", "høst", "a\\b",
        ] {
            invalid.push(format!("# sha256\n{digest}  {path}\n"));
        }
        for bytes in invalid {
            assert!(
                authenticate(&key, bytes.as_bytes()).is_err(),
                "accepted {bytes:?}"
            );
        }
        let oversized = vec![b'x'; MAX_MANIFEST_BYTES + 1];
        assert!(authenticate(&key, &oversized).is_err());
    }

    #[test]
    fn platform_paths_and_descriptor_binding_use_sha256_identity() {
        let key = SigningKey::from_bytes(&[10; 32]);
        let mut bytes = b"# sha256\n".to_vec();
        for path in [
            "bin/ww/linux/x86_64/ww",
            "bin/ww/linux/aarch64/ww",
            "bin/ww/macos/x86_64/ww",
            "bin/ww/macos/aarch64/ww",
            "bin/ww/linux/musl-x86_64/ww",
            "RELEASE-DESCRIPTOR",
        ] {
            bytes.extend_from_slice(&manifest(path, path.as_bytes())[9..]);
        }
        let verified = authenticate(&key, &bytes).unwrap();
        verified
            .verify_artifact("RELEASE-DESCRIPTOR", b"RELEASE-DESCRIPTOR")
            .unwrap();
        assert!(verified
            .verify_artifact("RELEASE-DESCRIPTOR", b"different CID or bytes")
            .is_err());
        assert_eq!(
            verified.sha256_for("RELEASE-DESCRIPTOR").unwrap(),
            &<[u8; 32]>::from(Sha256::digest(b"RELEASE-DESCRIPTOR"))
        );
    }

    #[test]
    fn openssl_publisher_and_both_verifiers_agree_on_exact_bytes() {
        use std::{fs, path::Path, process::Command};

        fn checked(command: &mut Command) -> std::process::Output {
            let output = command.output().expect("run OpenSSL or release script");
            assert!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
            output
        }

        // The standalone qualification harness also compiles this exact file.
        let repository = Path::new(env!("CARGO_MANIFEST_DIR"))
            .ancestors()
            .find(|path| path.join("scripts/sign_release.sh").is_file())
            .expect("repository release scripts");
        let temporary = tempfile::tempdir().unwrap();
        let release = temporary.path().join("release");
        fs::create_dir(&release).unwrap();
        // NamedTempFile creates a private 0600 file outside the release tree.
        let private = tempfile::NamedTempFile::new_in(temporary.path()).unwrap();
        checked(
            Command::new("openssl")
                .args(["genpkey", "-algorithm", "ED25519", "-out"])
                .arg(private.path()),
        );
        let public = checked(
            Command::new("openssl")
                .arg("pkey")
                .arg("-in")
                .arg(private.path())
                .args(["-pubout", "-outform", "DER"]),
        )
        .stdout;
        let keys = format!("{}\n", STANDARD.encode(public));
        let keyring = temporary.path().join("trusted-keys");
        fs::write(&keyring, &keys).unwrap();
        let trusted = TrustedKeys::from_bytes(keys.as_bytes()).unwrap();
        let manifest_path = release.join("CHECKSUMS.txt");
        let signature_path = release.join("CHECKSUMS.txt.sig");
        let bytes = manifest("bin/ww/linux/x86_64/ww", b"host artifact");
        fs::write(&manifest_path, &bytes).unwrap();
        checked(
            Command::new("bash")
                .arg(repository.join("scripts/sign_release.sh"))
                .arg(&manifest_path)
                .arg(&signature_path)
                .arg(private.path())
                .arg(&keyring)
                // Exercise local signing with ephemeral test keys even in PR CI.
                // The production signing admission tests exercise CI refusal separately.
                .env_remove("GITHUB_ACTIONS"),
        );
        let signature = fs::read(&signature_path).unwrap();
        let verified = trusted.authenticate(&bytes, &signature).unwrap();
        verified
            .verify_artifact("bin/ww/linux/x86_64/ww", b"host artifact")
            .unwrap();
        assert!(verified
            .verify_artifact("bin/ww/linux/x86_64/ww", b"attacker artifact")
            .is_err());
        let shell_accepts = || {
            Command::new("sh")
                .arg(repository.join("scripts/install.sh"))
                .arg("--verify-manifest")
                .arg(&manifest_path)
                .arg("--signature")
                .arg(&signature_path)
                .arg("--trusted-release-keys")
                .arg(&keyring)
                .output()
                .unwrap()
                .status
                .success()
        };
        assert!(shell_accepts());

        // Matching attacker checksums cannot reuse the authorized signature.
        let changed = manifest("bin/ww/linux/x86_64/ww", b"attacker artifact");
        fs::write(&manifest_path, &changed).unwrap();
        assert!(trusted.authenticate(&changed, &signature).is_err());
        assert!(!shell_accepts());
        fs::write(&manifest_path, &bytes).unwrap();
        for malformed_signature in [&signature[..63], &[signature.as_slice(), b"\n"].concat()] {
            fs::write(&signature_path, malformed_signature).unwrap();
            assert!(trusted.authenticate(&bytes, malformed_signature).is_err());
            assert!(!shell_accepts());
        }

        // A valid signature does not authorize a different digest algorithm.
        let malformed_manifest = String::from_utf8(bytes)
            .unwrap()
            .replace("# sha256", "# blake3");
        fs::write(&manifest_path, &malformed_manifest).unwrap();
        checked(
            Command::new("openssl")
                .args(["pkeyutl", "-sign", "-rawin", "-inkey"])
                .arg(private.path())
                .arg("-in")
                .arg(&manifest_path)
                .arg("-out")
                .arg(&signature_path),
        );
        let signature = fs::read(&signature_path).unwrap();
        assert!(trusted
            .authenticate(malformed_manifest.as_bytes(), &signature)
            .is_err());
        assert!(!shell_accepts());
    }
}
