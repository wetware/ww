//! Typed invocation data shared by startup capture and bounded native preflight.
//!
//! These records describe inputs; decoding does not grant startup authority.
//! Preflight uses `sources` only and never reopens the local paths in `config`.

use std::{net::SocketAddr, path::PathBuf};

use anyhow::{ensure, Context, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use super::service::StartupIdentity;

pub const NATIVE_PROTOCOL_VERSION: u32 = 1;
pub const MAX_PREFLIGHT_REQUEST_BYTES: usize = 1024 * 1024;
pub const MAX_NATIVE_RESPONSE_BYTES: usize = 64 * 1024;

/// Transport request only. Independent parent verification supplies authority.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PreflightRequest {
    pub version: u32,
    pub nonce: [u8; 32],
    pub artifact: super::ArtifactIdentity,
    /// None requests only this build's guard protocol capability. That branch
    /// performs no source capture, cache initialization or network preparation.
    pub inputs: Option<PreflightInputs>,
    pub timeout_millis: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PreparedHead {
    pub effective_root: String,
    pub kernel_source: String,
    pub kernel_runtime: String,
    pub kernel_source_cid: String,
    pub kernel_wasm_blake3: String,
    pub kernel_size: u64,
    pub authoritative_head: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PreflightResponse {
    pub version: u32,
    pub nonce: [u8; 32],
    pub artifact: super::ArtifactIdentity,
    pub guard_capable: bool,
    pub capture_digest: Option<[u8; 32]>,
    pub prepared: Option<PreparedHead>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReadinessChallenge {
    pub version: u32,
    pub nonce: [u8; 32],
    pub launch_nonce: Option<[u8; 32]>,
    pub timeout_millis: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReadinessResponse {
    pub version: u32,
    pub nonce: [u8; 32],
    pub launch_nonce: [u8; 32],
    pub transaction_id: Option<[u8; 32]>,
    pub permit_digest: Option<[u8; 32]>,
    pub contract: super::startup::ContractIdentity,
    pub artifact: super::ArtifactIdentity,
    pub startup: StartupIdentity,
    pub serving_ready: bool,
    pub epoch_seq: u64,
    pub effective_root: String,
    pub kernel_source: String,
    pub kernel_runtime: String,
    pub kernel_source_cid: Option<String>,
}

/// Address a single process birth. The path is discovery, not process proof;
/// callers independently verify peer credentials and the pinned process.
pub fn evidence_socket_path(
    home: &std::path::Path,
    pid: u32,
    token: &super::attestation::ProcessStartToken,
) -> Result<PathBuf> {
    ensure!(
        home.is_absolute() && pid != 0,
        "invalid readiness socket identity"
    );
    let token = match token {
        super::attestation::ProcessStartToken::Linux {
            boot_id,
            start_ticks,
        } => digest(b"wetware.readiness-address.v1", &(boot_id, start_ticks))?,
    };
    let path = home
        .join(".ww/bin/.ww-host-upgrade")
        .join(format!("r-{pid}-{}.sock", hex::encode(&token[..16])));
    use std::os::unix::ffi::OsStrExt;
    ensure!(
        path.as_os_str().as_bytes().len() < 104,
        "installation path exceeds native readiness socket limit"
    );
    Ok(path)
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum RuntimeCachePolicy {
    Shared,
    Isolated,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MountInput {
    pub source: String,
    pub target: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RuntimeSettings {
    pub kubo_wait_max_secs: u64,
    pub kubo_boot_retry_max_secs: u64,
    pub kubo_operation_timeout_secs: u64,
    pub compile_workers: usize,
    pub tracing: crate::config::TracingSettings,
    pub compilation_cache: crate::cell::engine::CapturedCacheSettings,
    pub interactive: bool,
    pub oci_image_id: Option<String>,
    /// Names only. Guarded operations require this list to be empty; values can
    /// contain credentials and never enter debug output or admission evidence.
    pub unsupported_environment: Vec<String>,
}

/// Every runtime-affecting `ww run` option after defaults and path expansion.
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EffectiveRunConfig {
    pub cwd: PathBuf,
    pub home: PathBuf,
    pub uid: u32,
    pub mounts: Vec<MountInput>,
    pub fhs_root: Option<PathBuf>,
    pub namespace_roots: Vec<PathBuf>,
    pub listen: Vec<String>,
    pub wasm_debug: bool,
    pub identity_path: Option<PathBuf>,
    pub insecure_ephemeral: bool,
    pub stem: Option<String>,
    pub ipns_stem: Option<String>,
    pub ipns_routing_url: String,
    pub rpc_url: String,
    pub ws_url: String,
    pub confirmation_depth: u64,
    pub executor_threads: usize,
    pub http_listen: Option<SocketAddr>,
    pub http_dial: Vec<String>,
    pub runtime_cache_policy: RuntimeCachePolicy,
    pub http_admin: Option<SocketAddr>,
    pub ipfs_url: String,
    pub runtime: RuntimeSettings,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NamespaceInput {
    pub name: String,
    pub ipns: String,
    pub bootstrap: String,
    pub source_path: PathBuf,
    pub content_sha256: [u8; 32],
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PublisherInput {
    pub name: String,
    pub bootstrap: String,
    pub routing_url: String,
    pub state_directory: PathBuf,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum SourceKind {
    File,
    Directory,
}

/// A selected immutable node, retained by the parent's capture continuation.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ImmutableSourceInput {
    pub cid: String,
    pub target: String,
    pub kind: SourceKind,
    pub dag_size: u64,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PreflightInputs {
    pub version: u32,
    pub config: EffectiveRunConfig,
    pub namespaces: Vec<NamespaceInput>,
    pub identity_fingerprint: [u8; 32],
    /// Digests from the same capture which produces `sources`, in mount order.
    pub operation_digests: Vec<[u8; 32]>,
    pub has_baseline: bool,
    pub sources: Vec<ImmutableSourceInput>,
    pub publisher: Option<PublisherInput>,
}

impl PreflightInputs {
    pub fn validate(&self) -> Result<()> {
        ensure!(self.version == 1, "unsupported preflight inputs version");
        ensure!(
            self.config.cwd.is_absolute() && self.config.home.is_absolute(),
            "preflight directories must be absolute"
        );
        ensure!(
            self.config.executor_threads > 0 && self.config.runtime.compile_workers > 0,
            "preflight worker counts must be positive"
        );
        ensure!(
            self.config.runtime.kubo_operation_timeout_secs > 0,
            "preflight operation timeout must be positive"
        );
        ensure!(
            self.config.runtime.unsupported_environment.is_empty(),
            "preflight rejects unsupported transport, loader, or debug environment settings"
        );
        self.config.runtime.compilation_cache.validate()?;
        ensure!(
            self.operation_digests.len() <= crate::cell::mount::MAX_MOUNT_OPERATIONS,
            "preflight mount operation limit exceeded"
        );
        ensure!(
            !self.has_baseline || !self.operation_digests.is_empty(),
            "preflight baseline is missing"
        );
        ensure!(
            self.sources.len() == self.operation_digests.len(),
            "preflight sources do not match the captured operations"
        );
        for source in &self.sources {
            let cid: cid::Cid = source.cid.parse().context("invalid preflight source CID")?;
            ensure!(
                source.cid == cid.to_string(),
                "preflight source CID is not canonical"
            );
            crate::cell::image::GuestTarget::new(&source.target)?;
        }
        Ok(())
    }

    /// Startup identity is available before publication; it excludes transport
    /// references which only become available after the admitted continuation.
    pub fn startup_identity(&self) -> Result<StartupIdentity> {
        let offset = usize::from(self.has_baseline);
        ensure!(
            offset <= self.operation_digests.len(),
            "captured baseline is missing"
        );
        Ok(StartupIdentity {
            configuration: hex::encode(digest(
                b"wetware.effective-run.v1",
                &(&self.config, &self.namespaces, self.identity_fingerprint),
            )?),
            fhs_baseline: self
                .has_baseline
                .then(|| hex::encode(self.operation_digests[0])),
            frozen_sources: hex::encode(digest(
                b"wetware.frozen-startup-sources.v1",
                &self.operation_digests[offset..],
            )?),
            publisher_target: self
                .publisher
                .as_ref()
                .map(|publisher| {
                    digest(b"wetware.startup-publisher.v1", publisher).map(hex::encode)
                })
                .transpose()?,
        })
    }

    pub fn capture_digest(&self) -> Result<[u8; 32]> {
        digest(b"wetware.preflight-inputs.v1", self)
    }
}

// These inputs can contain credentials in service URLs. Generic debug output
// must not publish raw configuration or publisher values.
impl std::fmt::Debug for EffectiveRunConfig {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("EffectiveRunConfig")
            .finish_non_exhaustive()
    }
}
impl std::fmt::Debug for PublisherInput {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("PublisherInput")
            .finish_non_exhaustive()
    }
}
impl std::fmt::Debug for PreflightInputs {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("PreflightInputs")
            .field("version", &self.version)
            .field("source_count", &self.sources.len())
            .finish_non_exhaustive()
    }
}

fn digest(domain: &[u8], value: &(impl Serialize + ?Sized)) -> Result<[u8; 32]> {
    let encoded = serde_json::to_vec(value)?;
    let mut hash = Sha256::new();
    hash.update((domain.len() as u64).to_be_bytes());
    hash.update(domain);
    hash.update((encoded.len() as u64).to_be_bytes());
    hash.update(encoded);
    Ok(hash.finalize().into())
}
