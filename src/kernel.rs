//! Host-side kernel source selection, resolution, and runtime identity.

use std::fmt;
use std::path::PathBuf;
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use cid::Cid;

use crate::cell::loaders::{HostPathLoader, IpfsLoader};
use crate::cell::Loader;

#[cfg(test)]
#[path = "../std/kernel/abi/kernel_abi_fingerprint.rs"]
mod kernel_abi_fingerprint;

/// Version of the native host ↔ pid0 component contract.
pub const KERNEL_ABI_VERSION: &str = env!("WW_KERNEL_ABI");

/// Build-locked fingerprint of the native/PID0 WIT and Cap'n Proto ABI inputs.
pub const KERNEL_ABI_FINGERPRINT: &str = env!("WW_KERNEL_ABI_FPR");

/// Source selected for the pid0 component.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum KernelSource {
    Path(PathBuf),
    Cid(Cid),
    Embedded(&'static str),
}

/// Stable, owned description of a selected source for logs and `/version`.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum KernelSourceRecord {
    Path { original: String },
    Cid { original: String },
    Embedded { original: String },
}

impl fmt::Display for KernelSourceRecord {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Path { original } => write!(f, "file:{original}"),
            Self::Cid { original } => write!(f, "cid:{original}"),
            Self::Embedded { original } => write!(f, "embedded:{original}"),
        }
    }
}

impl KernelSourceRecord {
    /// Bound structured log fields without changing the exact source retained
    /// for `/version` and diagnostics.
    pub fn log_value(&self) -> String {
        let value = self.to_string();
        if value.len() <= 512 {
            value
        } else {
            format!("<kernel source omitted: {} bytes>", value.len())
        }
    }
}

/// Resolution metadata retained for diagnostics.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct KernelMeta {
    pub size: usize,
    pub source_cid: Option<Cid>,
    pub load_duration: Duration,
}

/// Exact pid0 bytes and their loaded-byte runtime identity.
#[derive(Debug)]
pub struct ResolvedKernel {
    pub bytes: Vec<u8>,
    pub cid: Cid,
    pub source: KernelSourceRecord,
    pub metadata: KernelMeta,
}

impl ResolvedKernel {
    pub fn identity(&self) -> KernelIdentity {
        KernelIdentity {
            cid: self.cid.to_string(),
            source: self.source.to_string(),
            wasm_blake3: blake3::hash(&self.bytes).to_hex().to_string(),
            source_cid: self.metadata.source_cid.as_ref().map(ToString::to_string),
            size: self.metadata.size,
            abi: KERNEL_ABI_VERSION.to_string(),
            abi_fingerprint: KERNEL_ABI_FINGERPRINT.to_string(),
        }
    }
}

/// Late-bound identity published by the admin plane after source resolution.
#[derive(Clone, Debug, Eq, PartialEq, serde::Serialize)]
pub struct KernelIdentity {
    pub cid: String,
    pub source: String,
    pub wasm_blake3: String,
    pub source_cid: Option<String>,
    pub size: usize,
    pub abi: String,
    pub abi_fingerprint: String,
}

/// Shared identity state keeps `/version` available before Kubo and pid0.
#[derive(Clone, Debug)]
pub struct KernelIdentityState {
    requested_source: String,
    resolved: Arc<OnceLock<KernelIdentity>>,
}

impl KernelIdentityState {
    pub fn pending(source: &KernelSource) -> Self {
        Self {
            requested_source: source.record().to_string(),
            resolved: Arc::new(OnceLock::new()),
        }
    }

    pub fn pending_source(&self) -> String {
        format!("<pending: {}>", self.requested_source)
    }

    pub fn get(&self) -> Option<&KernelIdentity> {
        self.resolved.get()
    }

    pub fn publish(&self, identity: KernelIdentity) -> Result<()> {
        self.resolved
            .set(identity)
            .map_err(|_| anyhow::anyhow!("kernel runtime identity was already published"))
    }
}

impl serde::Serialize for KernelIdentityState {
    fn serialize<S>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        serde::Serialize::serialize(&self.get(), serializer)
    }
}

impl KernelSource {
    /// Parse the documented bare or explicit source syntax.
    pub fn parse(input: &str) -> Result<Self> {
        if input.is_empty() {
            bail!("kernel source must not be empty");
        }

        if let Some(path) = input.strip_prefix("file:") {
            if path.is_empty() {
                bail!("explicit file: kernel source requires a path");
            }
            return Ok(Self::Path(PathBuf::from(path)));
        }

        if let Some(value) = input.strip_prefix("cid:") {
            if value.is_empty() {
                bail!("explicit cid: kernel source requires a CID");
            }
            return value
                .parse()
                .map(Self::Cid)
                .with_context(|| format!("invalid explicit kernel CID '{value}'"));
        }

        if let Some(name) = input.strip_prefix("embedded:") {
            return match name {
                "main" => Ok(Self::Embedded("main")),
                "" => bail!("explicit embedded: kernel source requires a name"),
                other => bail!("unknown embedded kernel '{other}' (available: main)"),
            };
        }

        match input.parse::<Cid>() {
            Ok(cid) => Ok(Self::Cid(cid)),
            Err(_) => Ok(Self::Path(PathBuf::from(input))),
        }
    }

    pub fn record(&self) -> KernelSourceRecord {
        match self {
            Self::Path(path) => KernelSourceRecord::Path {
                original: path.display().to_string(),
            },
            Self::Cid(cid) => KernelSourceRecord::Cid {
                original: cid.to_string(),
            },
            Self::Embedded(name) => KernelSourceRecord::Embedded {
                original: (*name).to_string(),
            },
        }
    }

    /// Resolve this source exactly once. Explicit sources never fall back.
    pub async fn resolve(
        &self,
        ipfs_client: crate::ipfs::HttpClient,
        embedded_main: &'static [u8],
    ) -> Result<ResolvedKernel> {
        let started = Instant::now();
        let (bytes, source_cid) = match self {
            Self::Path(path) => {
                let metadata = tokio::fs::metadata(path)
                    .await
                    .with_context(|| format!("kernel file '{}' does not exist", path.display()))?;
                if !metadata.is_file() {
                    bail!("kernel path '{}' is not a regular file", path.display());
                }
                let loader = HostPathLoader;
                let path = path
                    .to_str()
                    .ok_or_else(|| anyhow::anyhow!("kernel path is not valid UTF-8"))?;
                (
                    loader
                        .load(path)
                        .await
                        .with_context(|| format!("failed to load kernel file '{path}'"))?,
                    None,
                )
            }
            Self::Cid(source_cid) => {
                let path = format!("/ipfs/{source_cid}");
                let loader = IpfsLoader::new(ipfs_client);
                (
                    loader.load(&path).await.with_context(|| {
                        format!("failed to load requested kernel CID {source_cid} from Kubo")
                    })?,
                    Some(source_cid.to_owned()),
                )
            }
            Self::Embedded("main") => {
                if embedded_main.is_empty() {
                    bail!("embedded kernel 'main' is missing or empty; run `make std`");
                }
                (embedded_main.to_vec(), None)
            }
            Self::Embedded(name) => bail!("unknown embedded kernel '{name}'"),
        };

        if bytes.is_empty() {
            bail!("resolved kernel '{}' is empty", self.record());
        }

        let cid = runtime_cid(&bytes);
        if let Some(source_cid) = source_cid.as_ref() {
            validate_source_cid(source_cid, &cid)?;
        }

        Ok(ResolvedKernel {
            metadata: KernelMeta {
                size: bytes.len(),
                source_cid,
                load_duration: started.elapsed(),
            },
            bytes,
            cid,
            source: self.record(),
        })
    }
}

fn validate_source_cid(source_cid: &Cid, runtime_cid: &Cid) -> Result<()> {
    if source_cid.codec() == 0x55 && source_cid.hash().code() == 0x1e && source_cid != runtime_cid {
        bail!(
            "raw BLAKE3 kernel CID mismatch: requested {source_cid}, loaded bytes identify as {runtime_cid}"
        );
    }
    Ok(())
}

/// CLI wins over environment; absent selectors retain the embedded default.
pub fn select_kernel_source(cli: Option<&str>, env: Option<&str>) -> Result<KernelSource> {
    match cli.or(env) {
        Some(value) => KernelSource::parse(value),
        None => Ok(KernelSource::Embedded("main")),
    }
}

pub fn runtime_cid(bytes: &[u8]) -> Cid {
    let digest = blake3::hash(bytes);
    let mh = cid::multihash::Multihash::<64>::wrap(0x1e, digest.as_bytes())
        .expect("blake3 digest always fits in 64-byte multihash");
    Cid::new_v1(0x55, mh)
}

#[cfg(test)]
mod tests {
    use super::*;

    const TEST_CID: &str = "bafkr4if3s6yv23hd3hgfvftj2g2uwdrqazv53p36p5lqyy7n77d5t5p54a";

    #[test]
    fn selector_precedence_is_cli_then_env_then_embedded() {
        assert_eq!(
            select_kernel_source(Some("file:/cli.wasm"), Some("file:/env.wasm")).unwrap(),
            KernelSource::Path(PathBuf::from("/cli.wasm"))
        );
        assert_eq!(
            select_kernel_source(None, Some("file:/env.wasm")).unwrap(),
            KernelSource::Path(PathBuf::from("/env.wasm"))
        );
        assert_eq!(
            select_kernel_source(None, None).unwrap(),
            KernelSource::Embedded("main")
        );
    }

    #[test]
    fn explicit_prefixes_override_cid_path_ambiguity() {
        assert_eq!(
            KernelSource::parse(&format!("file:{TEST_CID}")).unwrap(),
            KernelSource::Path(PathBuf::from(TEST_CID))
        );
        assert!(matches!(
            KernelSource::parse(&format!("cid:{TEST_CID}")).unwrap(),
            KernelSource::Cid(_)
        ));
        assert!(matches!(
            KernelSource::parse(TEST_CID).unwrap(),
            KernelSource::Cid(_)
        ));
        assert_eq!(
            KernelSource::parse("not-a-cid.wasm").unwrap(),
            KernelSource::Path(PathBuf::from("not-a-cid.wasm"))
        );
        assert_eq!(
            KernelSource::parse(" file with spaces.wasm ").unwrap(),
            KernelSource::Path(PathBuf::from(" file with spaces.wasm "))
        );
    }

    #[tokio::test]
    async fn local_file_resolution_reports_loaded_byte_identity() {
        let file = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(file.path(), b"kernel bytes").unwrap();
        let source = KernelSource::Path(file.path().to_owned());
        let resolved = source
            .resolve(
                crate::ipfs::HttpClient::new("http://127.0.0.1:1".into()),
                b"unused",
            )
            .await
            .unwrap();
        assert_eq!(resolved.bytes, b"kernel bytes");
        assert_eq!(resolved.cid, runtime_cid(b"kernel bytes"));
        assert_eq!(resolved.metadata.size, 12);
        assert_eq!(resolved.metadata.source_cid, None);
    }

    #[tokio::test]
    async fn missing_and_directory_paths_fail_with_named_errors() {
        let missing = KernelSource::Path(PathBuf::from("/definitely/missing/kernel.wasm"));
        let error = missing
            .resolve(
                crate::ipfs::HttpClient::new("http://127.0.0.1:1".into()),
                b"unused",
            )
            .await
            .unwrap_err();
        assert!(format!("{error:#}").contains("does not exist"));

        let directory = tempfile::tempdir().unwrap();
        let error = KernelSource::Path(directory.path().to_owned())
            .resolve(
                crate::ipfs::HttpClient::new("http://127.0.0.1:1".into()),
                b"unused",
            )
            .await
            .unwrap_err();
        assert!(format!("{error:#}").contains("not a regular file"));
    }

    #[tokio::test]
    async fn empty_kernel_file_fails_with_named_error() {
        let file = tempfile::NamedTempFile::new().unwrap();
        let error = KernelSource::Path(file.path().to_owned())
            .resolve(
                crate::ipfs::HttpClient::new("http://127.0.0.1:1".into()),
                b"unused",
            )
            .await
            .unwrap_err();
        let message = format!("{error:#}");
        assert!(message.contains("resolved kernel"), "{message}");
        assert!(message.contains("is empty"), "{message}");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn unreadable_kernel_file_fails_with_named_error() {
        use std::os::unix::fs::PermissionsExt;

        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("unreadable.wasm");
        std::fs::write(&path, b"kernel").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o000)).unwrap();
        let result = KernelSource::Path(path.clone())
            .resolve(
                crate::ipfs::HttpClient::new("http://127.0.0.1:1".into()),
                b"unused",
            )
            .await;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        let error = result.expect_err("unreadable kernel file must fail");
        let message = format!("{error:#}");
        assert!(message.contains("failed to load kernel file"), "{message}");
    }

    #[tokio::test]
    async fn explicit_cid_failure_does_not_fall_back_to_embedded() {
        let source = KernelSource::parse(&format!("cid:{TEST_CID}")).unwrap();
        let error = source
            .resolve(
                crate::ipfs::HttpClient::new("http://127.0.0.1:1".into()),
                b"valid embedded bytes that must not be selected",
            )
            .await
            .unwrap_err();
        let message = format!("{error:#}");
        assert!(message.contains("requested kernel CID"), "{message}");
        assert!(message.contains(TEST_CID), "{message}");
    }

    #[test]
    fn raw_blake3_source_cid_mismatch_fails_closed() {
        let requested = runtime_cid(b"requested content");
        let loaded = runtime_cid(b"different loaded content");
        let error = validate_source_cid(&requested, &loaded).unwrap_err();
        let message = error.to_string();
        assert!(message.contains("CID mismatch"), "{message}");
        assert!(message.contains(&requested.to_string()), "{message}");
        assert!(message.contains(&loaded.to_string()), "{message}");
    }

    #[test]
    fn parser_errors_name_explicit_interpretation() {
        let cid_error = KernelSource::parse("cid:not-a-cid")
            .unwrap_err()
            .to_string();
        assert!(cid_error.contains("explicit kernel CID"), "{cid_error}");

        let file_error = KernelSource::parse("file:").unwrap_err().to_string();
        assert!(file_error.contains("file:"), "{file_error}");
    }

    #[tokio::test]
    async fn embedded_resolution_fails_closed_when_artifact_is_missing() {
        let error = KernelSource::Embedded("main")
            .resolve(
                crate::ipfs::HttpClient::new("http://127.0.0.1:1".into()),
                b"",
            )
            .await
            .unwrap_err();
        assert!(format!("{error:#}").contains("missing or empty"));
    }

    #[test]
    fn identity_is_late_bound_once() {
        let source = KernelSource::Embedded("main");
        let state = KernelIdentityState::pending(&source);
        assert_eq!(state.pending_source(), "<pending: embedded:main>");
        assert!(state.get().is_none());

        let resolved = ResolvedKernel {
            bytes: b"kernel".to_vec(),
            cid: runtime_cid(b"kernel"),
            source: source.record(),
            metadata: KernelMeta {
                size: 6,
                source_cid: None,
                load_duration: Duration::ZERO,
            },
        };
        state.publish(resolved.identity()).unwrap();
        assert_eq!(state.get().unwrap().cid, runtime_cid(b"kernel").to_string());
        assert!(state.publish(resolved.identity()).is_err());
    }
}
