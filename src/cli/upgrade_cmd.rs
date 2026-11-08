//! Authenticated release selection and the explicit native replacement command.
//!
//! Release metadata supplies authority; the installed receipt supplies ownership.
//! NativeService separately refuses replacement until operational compatibility
//! and, for running services, child-side restart admission are qualified.

use anyhow::{bail, ensure, Context, Result};
use clap::Args;
use std::fs::OpenOptions;
use std::future::Future;
use std::io::Read;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};
use ww::release::auth::{TrustedKeys, MAX_MANIFEST_BYTES};
use ww::release::descriptor::{
    binding_cid, AuthenticatedRelease, Selection, SignedReleaseProof, COMPATIBILITY_PROFILE,
};
pub(super) use ww::upgrade::provenance::InstalledRelease;
use ww::upgrade::provenance::{AuthenticatedGeneration, IndependentTrustSource};
use ww::upgrade::service::ServiceControl;
use ww::upgrade::{ArtifactIdentity, DigestAlgorithm, Engine, Outcome, UpgradeRequest};

const MAX_HOST_BYTES: usize = 1024 * 1024 * 1024;
const MAX_FHS_BYTES: usize = 256 * 1024 * 1024;
const MAX_ROOT_BYTES: usize = 2 * 1024 * 1024;
const DOWNLOAD_TIMEOUT: Duration = Duration::from_secs(300);

#[derive(Args, Clone, Debug)]
pub(super) struct ReleaseArgs {
    /// Select an immutable signed release CID, including an historic release.
    #[arg(long, value_name = "CID")]
    pub release: Option<String>,

    /// Kubo HTTP API endpoint used only to fetch release content.
    #[arg(long, default_value = "http://localhost:5001", env = "IPFS_API")]
    pub ipfs_url: String,

    /// Routing V1 endpoint. Automatic release discovery awaits channel provisioning.
    #[arg(
        long,
        default_value = "http://localhost:8080",
        env = "IPFS_ROUTING_API"
    )]
    pub ipns_routing_url: String,

    /// Independently provisioned release keyring; downloaded keys are never trusted.
    #[arg(long, default_value = "/etc/wetware/release-keys", value_name = "FILE")]
    pub trusted_release_keys: PathBuf,
}

#[derive(Args, Debug)]
pub(super) struct UpgradeArgs {
    #[command(flatten)]
    pub release: ReleaseArgs,
    /// Restore the previous artifact retained by the native upgrade engine.
    #[arg(long, conflicts_with = "release", requires = "timeout_secs")]
    pub rollback: bool,
    /// Authenticated bootstrap recovery; never selects a new operation.
    #[arg(long, hide = true, requires_all = ["release", "timeout_secs"], conflicts_with_all = ["rollback", "check"])]
    pub recover_only: bool,
    /// Positive operation budget for changed upgrades, rollback and recovery.
    /// Exact-artifact no-ops and read-only checks do not require this option.
    #[arg(long, value_name = "N", value_parser = clap::value_parser!(u32).range(1..))]
    pub timeout_secs: Option<u32>,
    /// Authenticate and inspect without replacing installed files or restarting.
    #[arg(long)]
    pub check: bool,
    /// Emit one versioned result object on stdout.
    #[arg(long)]
    pub json: bool,
}

#[derive(Clone, Copy)]
pub(super) enum Scope {
    Metadata,
    Host,
    Fhs,
    Both,
}

pub(super) struct SelectedRelease {
    pub root: cid::Cid,
    pub generation: AuthenticatedGeneration,
    pub trust: IndependentTrustSource,
    pub selection: Selection,
    pub host: Option<Vec<u8>>,
    pub kernel: Option<Vec<u8>>,
    pub status: Option<Vec<u8>>,
}

pub(super) fn platform() -> Result<&'static str> {
    match (std::env::consts::OS, std::env::consts::ARCH) {
        ("linux", "x86_64") if cfg!(target_env = "musl") => Ok("x86_64-unknown-linux-musl"),
        ("linux", "x86_64") if cfg!(target_env = "gnu") => Ok("x86_64-unknown-linux-gnu"),
        ("linux", "aarch64") if cfg!(target_env = "gnu") => Ok("aarch64-unknown-linux-gnu"),
        ("macos", "x86_64") => Ok("x86_64-apple-darwin"),
        ("macos", "aarch64") => Ok("aarch64-apple-darwin"),
        _ => bail!("authenticated releases do not support this platform"),
    }
}

pub(super) fn read_bounded(path: &Path, limit: usize) -> Result<Vec<u8>> {
    let mut file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC)
        .open(path)
        .with_context(|| format!("read {}", path.display()))?;
    ensure!(
        file.metadata()?.is_file(),
        "{} is not a regular file",
        path.display()
    );
    let mut bytes = Vec::new();
    (&mut file).take(limit as u64 + 1).read_to_end(&mut bytes)?;
    ensure!(
        bytes.len() <= limit,
        "{} exceeds its size limit",
        path.display()
    );
    Ok(bytes)
}

pub(super) fn release_cid(value: &str) -> Result<cid::Cid> {
    let cid = ww::ipfs::cid_identity::parse_cid(value)
        .context("--release must be one immutable release CID")?;
    ensure!(
        cid.codec() == 0x70 && cid.hash().code() == 0x12 && cid.hash().size() == 32,
        "release root must be a SHA-256 DAG-PB CID"
    );
    Ok(cid)
}

fn http_client() -> Result<reqwest::Client> {
    reqwest::Client::builder()
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(DOWNLOAD_TIMEOUT)
        .build()
        .context("create bounded release download client")
}

async fn cat(
    client: &reqwest::Client,
    endpoint: &str,
    path: &str,
    limit: usize,
) -> Result<Vec<u8>> {
    let mut response = client
        .post(format!("{}/api/v0/cat", endpoint.trim_end_matches('/')))
        .query(&[("arg", path)])
        .send()
        .await
        .with_context(|| format!("fetch release content {path}"))?
        .error_for_status()?;
    ensure!(
        response
            .content_length()
            .is_none_or(|size| size <= limit as u64),
        "release response exceeds size limit: {path}"
    );
    let mut bytes = Vec::new();
    while let Some(chunk) = response.chunk().await? {
        ensure!(
            bytes
                .len()
                .checked_add(chunk.len())
                .is_some_and(|size| size <= limit),
            "release response exceeds size limit: {path}"
        );
        bytes.extend_from_slice(&chunk);
    }
    Ok(bytes)
}

async fn artifact(
    client: &reqwest::Client,
    endpoint: &str,
    selected: &ww::release::descriptor::Artifact,
    limit: usize,
) -> Result<Vec<u8>> {
    let size =
        usize::try_from(selected.size).context("artifact size does not fit this platform")?;
    ensure!(
        size <= limit,
        "selected artifact exceeds size limit: {}",
        selected.path
    );
    // Fetch by the authenticated artifact CID, never an unsigned tree path.
    let bytes = cat(client, endpoint, &format!("/ipfs/{}", selected.root), size).await?;
    selected.verify(&bytes)?;
    Ok(bytes)
}

/// Authenticate the immutable release before downloading selected artifact bytes.
/// Explicit historic selection never opens or rewrites a Consumer freshness floor.
pub(super) async fn select_release(args: &ReleaseArgs, scope: Scope) -> Result<SelectedRelease> {
    let selected = args.release.as_deref().with_context(|| {
        format!(
            "automatic release selection is unavailable: no trusted raw IPNS channel is provisioned for {}; use --release <CID> with independently provisioned release keys",
            args.ipns_routing_url
        )
    })?;
    let root = release_cid(selected)?;
    let trust = IndependentTrustSource::from_path(&args.trusted_release_keys)?;
    let keys = TrustedKeys::from_bytes(trust.keyring())?;
    let client = http_client()?;
    let ipfs = ww::ipfs::HttpClient::with_http_client(args.ipfs_url.clone(), client.clone());
    tokio::time::timeout(DOWNLOAD_TIMEOUT, async {
        let manifest = cat(
            &client,
            &args.ipfs_url,
            &format!("/ipfs/{root}/CHECKSUMS.txt"),
            MAX_MANIFEST_BYTES,
        )
        .await?;
        let signature = cat(
            &client,
            &args.ipfs_url,
            &format!("/ipfs/{root}/CHECKSUMS.txt.sig"),
            64,
        )
        .await?;
        // Authenticate before consuming even the descriptor locator.
        let authenticated_manifest = keys.authenticate(&manifest, &signature)?;
        let binding = cat(
            &client,
            &args.ipfs_url,
            &format!("/ipfs/{root}/RELEASE-DESCRIPTOR"),
            128,
        )
        .await?;
        authenticated_manifest.verify_artifact("RELEASE-DESCRIPTOR", &binding)?;
        let descriptor_cid = binding_cid(&binding)?;
        let descriptor = ipfs
            .block_get(&descriptor_cid, ww::ipfs::map::MAX_BLOCK_BYTES)
            .await?;
        let authenticated = AuthenticatedRelease::authenticate(
            &keys,
            &manifest,
            &signature,
            &binding,
            descriptor_cid,
            &descriptor,
        )?;
        let root_bytes = ipfs.block_get(&root, MAX_ROOT_BYTES).await?;
        let proof = SignedReleaseProof::capture(root, &root_bytes, &authenticated)?;
        let generation = AuthenticatedGeneration::from_proof(proof, trust.keyring(), platform()?)?;
        let selection = authenticated.select(platform()?, COMPATIBILITY_PROFILE)?;
        let host = if matches!(scope, Scope::Host | Scope::Both) {
            Some(
                artifact(
                    &client,
                    &args.ipfs_url,
                    &selection.host().artifact,
                    MAX_HOST_BYTES,
                )
                .await?,
            )
        } else {
            None
        };
        let (kernel, status) = if matches!(scope, Scope::Fhs | Scope::Both) {
            (
                Some(
                    artifact(
                        &client,
                        &args.ipfs_url,
                        &selection.deployment().kernel,
                        MAX_FHS_BYTES,
                    )
                    .await?,
                ),
                Some(
                    artifact(
                        &client,
                        &args.ipfs_url,
                        &selection.deployment().status,
                        MAX_FHS_BYTES,
                    )
                    .await?,
                ),
            )
        } else {
            (None, None)
        };
        Ok(SelectedRelease {
            root,
            generation,
            trust,
            selection,
            host,
            kernel,
            status,
        })
    })
    .await
    .context("release download exceeded its deadline")?
}

pub(super) async fn run(args: UpgradeArgs) -> Result<()> {
    let result = execute(&args).await;
    if args.json {
        println!("{}", serde_json::to_string(&result_json(&result))?);
    } else if let Ok(report) = &result {
        eprintln!("Native Host upgrade: {:?}", report.outcome);
        if let Some(artifact) = &report.selected_artifact {
            eprintln!(
                "Selected Host: {:?}:{} ({} bytes)",
                artifact.algorithm, artifact.digest, artifact.size
            );
        }
        if let Some(artifact) = &report.observed_artifact {
            eprintln!(
                "Installed Host at {}: {:?}:{} ({} bytes)",
                report.target.display(),
                artifact.algorithm,
                artifact.digest,
                artifact.size
            );
        }
    }
    result.map(|_| ())
}

struct ExecutionReport {
    outcome: Outcome,
    selected_artifact: Option<ArtifactIdentity>,
    observed_artifact: Option<ArtifactIdentity>,
    target: PathBuf,
    changed_paths: Vec<PathBuf>,
}

fn result_json(result: &Result<ExecutionReport>) -> serde_json::Value {
    match result {
        Ok(report) => serde_json::json!({
            "version": 1, "operation": "upgrade", "status": "completed",
            "outcome": report.outcome,
            "selected_artifact": report.selected_artifact,
            "observed_artifact": report.observed_artifact,
            "target": report.target,
            // Native artifact paths only; journals and retained copies are recovery metadata.
            "changed_paths": report.changed_paths,
            "activation": matches!(report.outcome,
                Outcome::Installed {live_ready:true} | Outcome::Recovered {live_ready:true, ..}),
            "recovery": match &report.outcome {
                Outcome::Recovered {action, ..} => Some(action),
                _ => None,
            },
        }),
        Err(error) => serde_json::json!({
            "version": 1, "operation": "upgrade", "status": "failed",
            "error": format!("{error:#}"), "activation": null,
            "selected_artifact": null, "observed_artifact": null,
            "changed_paths": null,
            "recovery": "inspect pending native transaction before another operation",
        }),
    }
}

fn observe_artifact(path: &Path) -> Result<Option<ArtifactIdentity>> {
    if !ww::upgrade::storage::exists(path)? {
        return Ok(None);
    }
    let bytes = read_bounded(path, MAX_HOST_BYTES)?;
    Ok(Some(ArtifactIdentity {
        algorithm: DigestAlgorithm::Blake3,
        digest: blake3::hash(&bytes).to_hex().to_string(),
        size: bytes.len() as u64,
    }))
}

fn completed(
    outcome: Outcome,
    selected_artifact: Option<ArtifactIdentity>,
    target: PathBuf,
    before: Option<ArtifactIdentity>,
) -> Result<ExecutionReport> {
    let observed_artifact = observe_artifact(&target)?;
    let changed_paths = if observed_artifact != before {
        vec![target.clone()]
    } else {
        Vec::new()
    };
    Ok(ExecutionReport {
        outcome,
        selected_artifact,
        observed_artifact,
        target,
        changed_paths,
    })
}

async fn execute(args: &UpgradeArgs) -> Result<ExecutionReport> {
    let home = dirs::home_dir().context("cannot determine HOME")?;
    execute_at(args, &home).await
}

fn operation_deadline(seconds: Option<u32>) -> Result<Option<Instant>> {
    seconds
        .map(|seconds| {
            ensure!(seconds > 0, "--timeout-secs must be positive");
            Instant::now()
                .checked_add(Duration::from_secs(u64::from(seconds)))
                .context("--timeout-secs exceeds the monotonic clock range")
        })
        .transpose()
}

fn require_deadline(deadline: Option<Instant>) -> Result<Instant> {
    deadline.context("changed native upgrade, rollback and recovery require an explicit positive --timeout-secs N")
}

fn remaining_budget(deadline: Instant) -> Result<Duration> {
    deadline
        .checked_duration_since(Instant::now())
        .filter(|remaining| !remaining.is_zero())
        .context("native operation exceeded its --timeout-secs deadline")
}

async fn within_budget<T>(
    deadline: Option<Instant>,
    operation: impl Future<Output = Result<T>>,
) -> Result<T> {
    if let Some(deadline) = deadline {
        tokio::time::timeout(remaining_budget(deadline)?, operation)
            .await
            .context("native operation exceeded its --timeout-secs deadline")?
    } else {
        operation.await
    }
}

async fn execute_at(args: &UpgradeArgs, home: &Path) -> Result<ExecutionReport> {
    let deadline = operation_deadline(args.timeout_secs)?;
    if args.recover_only || args.rollback {
        require_deadline(deadline)?;
    }
    let root = home.join(".ww");
    let target = root.join("bin/ww");
    let lock = ww::installation_state::MutationLock::acquire(home)?;
    lock.reject_pending_image()?;
    let pending = ww::upgrade::storage::exists(&root.join("host-upgrade.json"))?;
    let mut service = ww::upgrade::service::NativeService::new(home)?;
    if args.recover_only {
        let deadline = require_deadline(deadline)?;
        let mut engine = Engine::from_lock(lock)?;
        engine.require_pending_recovery()?;
        let before = observe_artifact(&target)?;
        let executable = read_bounded(&std::env::current_exe()?, MAX_HOST_BYTES)?;
        let authenticated = within_budget(
            Some(deadline),
            authenticate_recovery_executable(&args.release, &executable),
        )
        .await?;
        engine.validate_recovery_executable(&authenticated)?;
        // Only guarded recovery relies on this process enforcing the native
        // protocol. Legacy unchanged-byte abort never grants startup authority.
        if ww::upgrade::startup::metadata_present(home)? {
            ww::upgrade::attestation::PinnedProcess::capture(
                std::process::id(),
                authenticated.artifact(),
                deadline,
            )?;
        }
        let selected_artifact = Some(authenticated.artifact().clone());
        let outcome = match engine.recover_authenticated_without_preparation_until(
            &mut service,
            deadline,
            &authenticated,
        )? {
            Some(outcome) => outcome,
            None => recover_prepared(&mut engine, &mut service, home, deadline).await?,
        };
        return completed(outcome, selected_artifact, target, before);
    }
    if args.check {
        ensure!(
            !args.rollback,
            "rollback preflight is unavailable; --check cannot restore retained software"
        );
        lock.reject_pending()?;
    } else if pending {
        let deadline = require_deadline(deadline)?;
        let mut engine = Engine::from_lock(lock)?;
        let before = observe_artifact(&target)?;
        let outcome = recover_prepared(&mut engine, &mut service, home, deadline).await?;
        return completed(outcome, None, target, before);
    }
    let before = observe_artifact(&target)?;
    if !ww::upgrade::startup::metadata_present(home)? {
        ensure!(!args.rollback, "legacy native rollback requires separately reviewed migration; no startup authority was granted");
        // Preserve exact-artifact no-ops for existing installations. No store,
        // candidate payload, service observation, or admission is needed.
        let receipt = ww::installation_state::Receipt::load(&root)?;
        let installed = within_budget(deadline, installed_release(&root, &args.release)).await?;
        let installed_bytes = read_bounded(&target, MAX_HOST_BYTES)?;
        installed.selection.verify_host(&installed_bytes)?;
        ensure!(
            receipt.executable_digest() == blake3::hash(&installed_bytes).to_hex().as_str(),
            "installed Host does not match ownership receipt"
        );
        let candidate =
            within_budget(deadline, select_release(&args.release, Scope::Metadata)).await?;
        if installed.generation.artifact() == candidate.generation.artifact() {
            return completed(
                Outcome::AlreadyInstalled,
                Some(candidate.generation.artifact().clone()),
                target,
                before,
            );
        }
        if !args.check {
            require_deadline(deadline)?;
        }
        bail!("legacy installation has no guarded startup contract; native operational integration requires a separately reviewed migration; no Host or service was changed");
    }
    let mut engine = Engine::from_lock(lock)?;
    let installed = engine.current_generation()?;
    // Validate retained contract metadata even for an exact-artifact no-op.
    engine.startup_contract()?;
    if args.rollback {
        let deadline = require_deadline(deadline)?;
        let contract = engine.preparation_contract()?;
        let inputs = super::startup::capture_for_upgrade(home, &contract, deadline, None).await?;
        let outcome = engine.operator_rollback_prepared_until(&mut service, deadline, &inputs)?;
        return completed(outcome, None, target, before);
    }
    let candidate = within_budget(deadline, select_release(&args.release, Scope::Metadata)).await?;
    if installed.artifact() == candidate.generation.artifact() {
        return completed(
            Outcome::AlreadyInstalled,
            Some(candidate.generation.artifact().clone()),
            target,
            before,
        );
    }
    // --check has a preparation-only budget. Every mutating operation requires
    // the caller's explicit positive budget, including all capture and I/O.
    let deadline = if args.check {
        deadline.unwrap_or_else(|| Instant::now() + DOWNLOAD_TIMEOUT)
    } else {
        require_deadline(deadline)?
    };
    let contract = engine.preparation_contract()?;
    let inputs = super::startup::capture_for_upgrade(home, &contract, deadline, None).await?;
    let candidate =
        within_budget(Some(deadline), select_release(&args.release, Scope::Host)).await?;
    let selected_artifact = Some(candidate.generation.artifact().clone());
    let request = UpgradeRequest {
        installed,
        candidate: candidate.generation,
        bytes: std::io::Cursor::new(candidate.host.context("selected native artifact missing")?),
        inputs,
    };
    let outcome = if args.check {
        engine.check_upgrade_until(&mut service, deadline, request)?
    } else {
        engine.operator_upgrade_until(&mut service, deadline, || Ok(request))?
    };
    completed(outcome, selected_artifact, target, before)
}

async fn recover_prepared(
    engine: &mut Engine,
    service: &mut impl ServiceControl,
    home: &Path,
    deadline: Instant,
) -> Result<Outcome> {
    if let Some(outcome) = engine.recover_without_preparation_until(service, deadline)? {
        return Ok(outcome);
    }
    let contract = engine.preparation_contract()?;
    let inputs = super::startup::capture_for_upgrade(home, &contract, deadline, None).await?;
    engine.recover_prepared_until(service, deadline, &inputs)
}

async fn authenticate_recovery_executable(
    args: &ReleaseArgs,
    executable: &[u8],
) -> Result<AuthenticatedGeneration> {
    let selected = select_release(args, Scope::Host).await?;
    ensure!(
        selected.host.as_deref() == Some(executable),
        "running recovery executable does not match the authenticated selected Host"
    );
    Ok(selected.generation)
}

async fn installed_release(root: &Path, args: &ReleaseArgs) -> Result<SelectedRelease> {
    let pointer_path = root.join("installed-release.json");
    let pointer: InstalledRelease = serde_json::from_slice(
        &read_bounded(&pointer_path, 1024).context(
            "authenticated installed release provenance is unavailable; restore the exact installed release locator from an authenticated installation before upgrading; ownership hashes do not authenticate a release",
        )?,
    ).context("invalid installed release locator")?;
    ensure!(
        pointer.version == 1,
        "unsupported installed release locator version"
    );
    let mut installed_args = args.clone();
    installed_args.release = Some(pointer.release);
    select_release(&installed_args, Scope::Metadata).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;
    use ed25519_dalek::{Signer, SigningKey};
    use sha2::{Digest, Sha256};
    use std::collections::BTreeMap;
    use std::sync::{Arc, Mutex};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use ww::release::descriptor::{
        platform_path, Artifact, DeploymentArtifact, Descriptor, HostArtifact, Provenance,
        SOURCE_REPOSITORY,
    };

    fn cid(codec: u64, bytes: &[u8]) -> cid::Cid {
        cid::Cid::new_v1(
            codec,
            cid::multihash::Multihash::wrap(0x12, &Sha256::digest(bytes)).unwrap(),
        )
    }

    fn file(path: &str, bytes: &[u8]) -> Artifact {
        Artifact {
            path: path.into(),
            root: cid(0x55, bytes),
            sha256: Sha256::digest(bytes).into(),
            size: bytes.len() as u64,
        }
    }

    fn keyring(key: &SigningKey) -> Vec<u8> {
        // Release trust uses one base64-encoded Ed25519 SPKI DER key per line.
        let mut der = vec![
            0x30, 0x2a, 0x30, 0x05, 0x06, 0x03, 0x2b, 0x65, 0x70, 0x03, 0x21, 0x00,
        ];
        der.extend_from_slice(key.verifying_key().as_bytes());
        const ALPHABET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
        let mut encoded = Vec::new();
        for block in der.chunks(3) {
            let value = (u32::from(block[0]) << 16)
                | (u32::from(*block.get(1).unwrap_or(&0)) << 8)
                | u32::from(*block.get(2).unwrap_or(&0));
            encoded.extend([
                ALPHABET[(value >> 18) as usize],
                ALPHABET[((value >> 12) & 63) as usize],
                if block.len() > 1 {
                    ALPHABET[((value >> 6) & 63) as usize]
                } else {
                    b'='
                },
                if block.len() > 2 {
                    ALPHABET[(value & 63) as usize]
                } else {
                    b'='
                },
            ]);
        }
        encoded.push(b'\n');
        encoded
    }

    struct Fixture {
        key: SigningKey,
        root: cid::Cid,
        routes: BTreeMap<String, Vec<u8>>,
        descriptor: Descriptor,
    }

    impl Fixture {
        fn new(target: &str) -> Self {
            Self::with_host(target, b"authenticated Host")
        }

        fn with_host(target: &str, host_bytes: &[u8]) -> Self {
            let key = SigningKey::from_bytes(&[7; 32]);
            let name = libp2p::identity::Keypair::generate_ed25519()
                .public()
                .to_peer_id();
            let provenance = Provenance {
                repository: SOURCE_REPOSITORY.into(),
                revision: "a".repeat(40),
                builder: "test".into(),
            };
            let host = file(platform_path(target).unwrap(), host_bytes);
            let kernel = file("fhs/boot/kernel.wasm", b"authenticated kernel");
            let status = file("fhs/bin/status.wasm", b"authenticated status");
            let descriptor = Descriptor {
                channel: ww::ipns::canonical_name(name),
                sequence: 3,
                version: "0.1.1".into(),
                source: provenance.clone(),
                hosts: vec![HostArtifact {
                    target: target.into(),
                    artifact: host.clone(),
                    provenance,
                    compatibility: COMPATIBILITY_PROFILE.into(),
                }],
                deployment: DeploymentArtifact {
                    root: cid(0x70, b"FHS graph"),
                    kernel: kernel.clone(),
                    status: status.clone(),
                    compatibility: COMPATIBILITY_PROFILE.into(),
                },
            };
            let binding = descriptor.binding().unwrap();
            let mut manifest = format!(
                "# sha256\n{}  RELEASE-DESCRIPTOR\n",
                hex::encode(Sha256::digest(&binding))
            );
            for artifact in [&host, &kernel, &status] {
                manifest.push_str(&format!(
                    "{}  {}\n",
                    hex::encode(artifact.sha256),
                    artifact.path
                ));
            }
            let manifest = manifest.into_bytes();
            let descriptor_cid = descriptor.cid().unwrap();
            // One plain UnixFS directory link. This is protocol fixture data;
            // production root parsing uses verify_transport_root.
            let mut link = vec![0x0a, descriptor_cid.to_bytes().len() as u8];
            link.extend(descriptor_cid.to_bytes());
            link.extend([0x12, 18]);
            link.extend(b"release-descriptor");
            let mut root_bytes = vec![0x0a, 2, 8, 1, 0x12, link.len() as u8];
            root_bytes.extend(link);
            let root = cid(0x70, &root_bytes);
            let routes = BTreeMap::from([
                (format!("cat:/ipfs/{root}/CHECKSUMS.txt"), manifest.clone()),
                (
                    format!("cat:/ipfs/{root}/CHECKSUMS.txt.sig"),
                    key.sign(&manifest).to_bytes().to_vec(),
                ),
                (format!("cat:/ipfs/{root}/RELEASE-DESCRIPTOR"), binding),
                (
                    format!("block/get:{descriptor_cid}"),
                    descriptor.encode().unwrap(),
                ),
                (format!("block/get:{root}"), root_bytes),
                (format!("cat:/ipfs/{}", host.root), host_bytes.to_vec()),
                (
                    format!("cat:/ipfs/{}", kernel.root),
                    b"authenticated kernel".to_vec(),
                ),
                (
                    format!("cat:/ipfs/{}", status.root),
                    b"authenticated status".to_vec(),
                ),
            ]);
            Self {
                key,
                root,
                routes,
                descriptor,
            }
        }
    }

    struct Server {
        endpoint: String,
        requests: Arc<Mutex<Vec<String>>>,
        task: tokio::task::JoinHandle<()>,
    }
    impl Drop for Server {
        fn drop(&mut self) {
            self.task.abort();
        }
    }
    impl Server {
        async fn start(routes: BTreeMap<String, Vec<u8>>) -> Self {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let endpoint = format!("http://{}", listener.local_addr().unwrap());
            let requests = Arc::new(Mutex::new(Vec::new()));
            let recorded = requests.clone();
            let task = tokio::spawn(async move {
                loop {
                    let (mut socket, _) = listener.accept().await.unwrap();
                    let mut request = Vec::new();
                    loop {
                        let mut buffer = [0; 4096];
                        let count = socket.read(&mut buffer).await.unwrap();
                        assert_ne!(count, 0);
                        request.extend_from_slice(&buffer[..count]);
                        if request.windows(4).any(|window| window == b"\r\n\r\n") {
                            break;
                        }
                    }
                    let request = String::from_utf8(request).unwrap();
                    let target = request
                        .lines()
                        .next()
                        .unwrap()
                        .split_whitespace()
                        .nth(1)
                        .unwrap();
                    let url = reqwest::Url::parse(&format!("http://localhost{target}")).unwrap();
                    let arg = url
                        .query_pairs()
                        .find(|(key, _)| key == "arg")
                        .unwrap()
                        .1
                        .into_owned();
                    let key = format!("{}:{arg}", url.path().strip_prefix("/api/v0/").unwrap());
                    recorded.lock().unwrap().push(key.clone());
                    let (status, body) = match routes.get(&key) {
                        Some(body) => ("200 OK", body.as_slice()),
                        None => ("404 Not Found", &b"missing"[..]),
                    };
                    socket.write_all(format!("HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", body.len()).as_bytes()).await.unwrap();
                    socket.write_all(body).await.unwrap();
                }
            });
            Self {
                endpoint,
                requests,
                task,
            }
        }
    }

    fn args(fixture: &Fixture, server: &Server, home: &Path) -> ReleaseArgs {
        let trusted_release_keys = home.join("release-keys");
        std::fs::write(&trusted_release_keys, keyring(&fixture.key)).unwrap();
        ReleaseArgs {
            release: Some(fixture.root.to_string()),
            ipfs_url: server.endpoint.clone(),
            ipns_routing_url: "http://127.0.0.1:1".into(),
            trusted_release_keys,
        }
    }

    #[test]
    fn recovery_only_grammar_requires_explicit_release_and_rejects_other_operations() {
        for arguments in [
            vec!["ww", "upgrade", "--recover-only"],
            vec![
                "ww",
                "upgrade",
                "--recover-only",
                "--release",
                "CID",
                "--check",
            ],
            vec![
                "ww",
                "upgrade",
                "--recover-only",
                "--release",
                "CID",
                "--rollback",
            ],
        ] {
            assert!(super::super::Cli::try_parse_from(arguments).is_err());
        }
        let parsed = super::super::Cli::try_parse_from([
            "ww",
            "upgrade",
            "--recover-only",
            "--release",
            "CID",
            "--timeout-secs",
            "15",
            "--json",
        ])
        .unwrap();
        assert!(matches!(
            parsed.command,
            super::super::Commands::Upgrade(UpgradeArgs {
                recover_only: true,
                ..
            })
        ));
    }

    #[test]
    fn timeout_grammar_accepts_positive_budget_and_exempts_inspection() {
        for arguments in [
            vec!["ww", "upgrade", "--release", "CID", "--timeout-secs", "17"],
            vec!["ww", "upgrade", "--rollback", "--timeout-secs", "17"],
            vec![
                "ww",
                "upgrade",
                "--recover-only",
                "--release",
                "CID",
                "--timeout-secs",
                "17",
            ],
            vec!["ww", "upgrade", "--check", "--release", "CID"],
            vec!["ww", "upgrade", "--release", "CID"],
        ] {
            assert!(
                super::super::Cli::try_parse_from(&arguments).is_ok(),
                "{arguments:?}"
            );
        }
        for value in ["0", "-1", "NaN", "1.5", "18446744073709551616"] {
            assert!(
                super::super::Cli::try_parse_from([
                    "ww",
                    "upgrade",
                    "--release",
                    "CID",
                    "--timeout-secs",
                    value
                ])
                .is_err(),
                "{value}"
            );
        }
        assert!(super::super::Cli::try_parse_from(["ww", "upgrade", "--rollback"]).is_err());
        assert!(super::super::Cli::try_parse_from([
            "ww",
            "upgrade",
            "--recover-only",
            "--release",
            "CID"
        ])
        .is_err());
    }

    #[tokio::test]
    async fn recovery_only_without_valid_pending_journal_refuses_before_network() {
        for malformed_journal in [false, true] {
            let fixture = Fixture::new(platform().unwrap());
            let server = Server::start(fixture.routes.clone()).await;
            let home = tempfile::tempdir().unwrap();
            let root = home.path().join(".ww");
            std::fs::create_dir_all(root.join("bin")).unwrap();
            std::fs::write(root.join("identity"), b"preserved").unwrap();
            if malformed_journal {
                std::fs::write(root.join("host-upgrade.json"), b"invalid journal").unwrap();
            }
            let args = UpgradeArgs {
                release: args(&fixture, &server, home.path()),
                rollback: false,
                recover_only: true,
                timeout_secs: Some(15),
                check: false,
                json: false,
            };
            let error = execute_at(&args, home.path()).await.err().unwrap();
            assert!(error.to_string().contains(if malformed_journal {
                "invalid Host transaction journal"
            } else {
                "no pending Host transaction"
            }));
            assert!(server.requests.lock().unwrap().is_empty());
            assert_eq!(std::fs::read(root.join("identity")).unwrap(), b"preserved");
            assert!(!root.join("installation.json").exists());
            assert!(!root.join("fhs").exists());
        }
    }

    #[tokio::test]
    async fn recovery_executable_requires_exact_authenticated_host_bytes() {
        let fixture = Fixture::new(platform().unwrap());
        let server = Server::start(fixture.routes.clone()).await;
        let home = tempfile::tempdir().unwrap();
        let args = args(&fixture, &server, home.path());
        let authenticated = authenticate_recovery_executable(&args, b"authenticated Host")
            .await
            .unwrap();
        assert_eq!(
            authenticated.artifact().digest,
            hex::encode(fixture.descriptor.hosts[0].artifact.sha256)
        );
        let error = authenticate_recovery_executable(&args, b"unrelated recovery executable")
            .await
            .unwrap_err();
        assert!(error
            .to_string()
            .contains("running recovery executable does not match"));
        assert!(!server.requests.lock().unwrap().iter().any(|path| {
            path == &format!("cat:/ipfs/{}", fixture.descriptor.deployment.kernel.root)
                || path == &format!("cat:/ipfs/{}", fixture.descriptor.deployment.status.root)
        }));
    }

    #[test]
    fn structured_results_distinguish_noop_recovery_and_uncertain_failure() {
        let home = tempfile::tempdir().unwrap();
        let target = home.path().join("ww");
        std::fs::write(&target, b"native before").unwrap();
        let before = observe_artifact(&target).unwrap();
        let selected = ArtifactIdentity {
            algorithm: DigestAlgorithm::Sha256,
            digest: hex::encode(Sha256::digest(b"native before")),
            size: 13,
        };
        let report = completed(
            Outcome::AlreadyInstalled,
            Some(selected.clone()),
            target.clone(),
            before.clone(),
        );
        let value = result_json(&report);
        assert_eq!(value["outcome"]["kind"], "already-installed");
        assert_eq!(value["selected_artifact"]["digest"], selected.digest);
        assert_eq!(value["changed_paths"], serde_json::json!([]));
        assert_eq!(value["activation"], false);
        assert_eq!(value["recovery"], serde_json::Value::Null);

        std::fs::write(&target, b"restored native").unwrap();
        let restored = ArtifactIdentity {
            algorithm: DigestAlgorithm::Sha256,
            digest: hex::encode(Sha256::digest(b"restored native")),
            size: 15,
        };
        let report = completed(
            Outcome::Recovered {
                artifact: restored.clone(),
                action: ww::upgrade::RecoveryAction::RestoredPrevious,
                live_ready: false,
            },
            Some(selected),
            target.clone(),
            before,
        );
        let value = result_json(&report);
        assert_eq!(value["outcome"]["kind"], "recovered");
        assert_eq!(value["outcome"]["artifact"]["digest"], restored.digest);
        assert_eq!(value["recovery"], "restored-previous");
        assert_eq!(value["changed_paths"], serde_json::json!([target]));
        assert_eq!(value["activation"], false);
        assert_eq!(value["observed_artifact"]["algorithm"], "blake3");

        let value = result_json(&Err(anyhow::anyhow!("service state is unconfirmed")));
        assert_eq!(value["status"], "failed");
        assert_eq!(value["activation"], serde_json::Value::Null);
        assert_eq!(value["changed_paths"], serde_json::Value::Null);
    }

    #[tokio::test]
    async fn authenticated_scoped_download_binds_root_platform_and_exact_bytes() {
        let fixture = Fixture::new(platform().unwrap());
        let server = Server::start(fixture.routes.clone()).await;
        let home = tempfile::tempdir().unwrap();
        let args = args(&fixture, &server, home.path());
        let selected = select_release(&args, Scope::Fhs).await.unwrap();
        assert!(selected.host.is_none());
        assert_eq!(
            selected.kernel.as_deref(),
            Some(&b"authenticated kernel"[..])
        );
        assert_eq!(
            selected.status.as_deref(),
            Some(&b"authenticated status"[..])
        );
        assert!(!server.requests.lock().unwrap().contains(&format!(
            "cat:/ipfs/{}",
            fixture.descriptor.hosts[0].artifact.root
        )));
        let candidate = selected.generation.selection();
        assert!(format!("{candidate:?}")
            .contains(&hex::encode(fixture.descriptor.hosts[0].artifact.sha256)));
        let retained = selected.generation.record();
        assert_eq!(retained.transport_root, fixture.root.to_string());
        assert_eq!(
            retained.descriptor,
            fixture.descriptor.cid().unwrap().to_string()
        );
        let proof = retained
            .proof
            .verify(selected.generation.proof_bytes())
            .unwrap();
        let keys = TrustedKeys::from_bytes(&read_bounded(&args.trusted_release_keys, 122).unwrap())
            .unwrap();
        assert_eq!(
            proof.authenticate(&keys).unwrap().descriptor(),
            &fixture.descriptor
        );
        let selected = select_release(&args, Scope::Both).await.unwrap();
        assert_eq!(selected.host.as_deref(), Some(&b"authenticated Host"[..]));
        // Immutable selection creates no persistent consumer floor.
        assert_eq!(std::fs::read_dir(home.path()).unwrap().count(), 1);
    }

    #[tokio::test]
    async fn wrong_key_refuses_before_consuming_locator_or_artifacts() {
        let fixture = Fixture::new(platform().unwrap());
        let server = Server::start(fixture.routes.clone()).await;
        let home = tempfile::tempdir().unwrap();
        let args = args(&fixture, &server, home.path());
        std::fs::write(
            &args.trusted_release_keys,
            keyring(&SigningKey::from_bytes(&[8; 32])),
        )
        .unwrap();
        assert!(select_release(&args, Scope::Both).await.is_err());
        assert_eq!(server.requests.lock().unwrap().len(), 2);
    }

    #[tokio::test]
    async fn changed_signed_binding_and_changed_host_bytes_are_rejected() {
        for corrupt_binding in [true, false] {
            let mut fixture = Fixture::new(platform().unwrap());
            let key = if corrupt_binding {
                format!("cat:/ipfs/{}/RELEASE-DESCRIPTOR", fixture.root)
            } else {
                format!("cat:/ipfs/{}", fixture.descriptor.hosts[0].artifact.root)
            };
            fixture.routes.get_mut(&key).unwrap()[0] ^= 1;
            let server = Server::start(fixture.routes.clone()).await;
            let home = tempfile::tempdir().unwrap();
            assert!(
                select_release(&args(&fixture, &server, home.path()), Scope::Host)
                    .await
                    .is_err()
            );
            if corrupt_binding {
                assert_eq!(server.requests.lock().unwrap().len(), 3);
            }
        }
    }

    #[tokio::test]
    async fn wrong_platform_refuses_before_artifact_download() {
        let target = if platform().unwrap() == "x86_64-unknown-linux-gnu" {
            "aarch64-apple-darwin"
        } else {
            "x86_64-unknown-linux-gnu"
        };
        let fixture = Fixture::new(target);
        let server = Server::start(fixture.routes.clone()).await;
        let home = tempfile::tempdir().unwrap();
        assert!(
            select_release(&args(&fixture, &server, home.path()), Scope::Host)
                .await
                .is_err()
        );
        assert_eq!(server.requests.lock().unwrap().len(), 5);
    }

    #[tokio::test]
    async fn missing_channel_and_invalid_cid_refuse_without_network() {
        let fixture = Fixture::new(platform().unwrap());
        let server = Server::start(fixture.routes.clone()).await;
        let home = tempfile::tempdir().unwrap();
        let mut args = args(&fixture, &server, home.path());
        args.release = None;
        assert!(select_release(&args, Scope::Host)
            .await
            .err()
            .unwrap()
            .to_string()
            .contains("no trusted raw IPNS channel"));
        args.release = Some("/ipns/attacker".into());
        assert!(select_release(&args, Scope::Host).await.is_err());
        assert!(server.requests.lock().unwrap().is_empty());
    }

    #[test]
    fn local_provenance_and_trust_reads_refuse_symlinks_and_oversize() {
        let home = tempfile::tempdir().unwrap();
        let source = home.path().join("source");
        let link = home.path().join("link");
        std::fs::write(&source, b"12345").unwrap();
        std::os::unix::fs::symlink(&source, &link).unwrap();
        assert!(read_bounded(&link, 10).is_err());
        assert!(read_bounded(&source, 4).is_err());
        assert_eq!(read_bounded(&source, 5).unwrap(), b"12345");
    }
    fn installed_fixture(home: &Path, fixture: &Fixture, bytes: &[u8]) {
        let root = home.join(".ww");
        std::fs::create_dir_all(root.join("bin")).unwrap();
        std::fs::write(root.join("bin/ww"), bytes).unwrap();
        let receipt = ww::installation_state::Receipt {
            version: 2,
            hashes: [
                blake3::hash(bytes).to_hex().to_string(),
                "kernel".into(),
                "status".into(),
                "namespace".into(),
                "service".into(),
            ],
            previous: None,
            complete: true,
        };
        std::fs::write(root.join("installation.json"), receipt.encode().unwrap()).unwrap();
        std::fs::write(
            root.join("installed-release.json"),
            serde_json::to_vec(&InstalledRelease::from_root(fixture.root).unwrap()).unwrap(),
        )
        .unwrap();
    }

    #[tokio::test]
    async fn exact_artifact_without_timeout_preserves_provenance_and_skips_payload() {
        let installed = Fixture::new(platform().unwrap());
        let candidate = Fixture::new(platform().unwrap());
        assert_ne!(installed.root, candidate.root);
        let mut routes = installed.routes.clone();
        routes.extend(candidate.routes.clone());
        let server = Server::start(routes).await;
        let home = tempfile::tempdir().unwrap();
        installed_fixture(home.path(), &installed, b"authenticated Host");
        let locator = home.path().join(".ww/installed-release.json");
        let before = std::fs::read(&locator).unwrap();
        let args = UpgradeArgs {
            release: args(&candidate, &server, home.path()),
            rollback: false,
            recover_only: false,
            timeout_secs: None,
            check: false,
            json: false,
        };
        let report = execute_at(&args, home.path()).await.unwrap();
        assert_eq!(report.outcome, Outcome::AlreadyInstalled);
        assert_eq!(std::fs::read(locator).unwrap(), before);
        assert!(report.changed_paths.is_empty());
        assert!(!home.path().join(".ww/bin/.ww-host-upgrade").exists());
        assert!(!server.requests.lock().unwrap().contains(&format!(
            "cat:/ipfs/{}",
            candidate.descriptor.hosts[0].artifact.root
        )));
    }

    #[tokio::test]
    async fn changed_artifact_requires_budget_before_payload_or_engine_store() {
        let installed = Fixture::new(platform().unwrap());
        let candidate = Fixture::with_host(platform().unwrap(), b"different authenticated Host");
        assert_eq!(installed.descriptor.version, candidate.descriptor.version);
        let mut routes = installed.routes.clone();
        routes.extend(candidate.routes.clone());
        let server = Server::start(routes).await;
        let home = tempfile::tempdir().unwrap();
        installed_fixture(home.path(), &installed, b"authenticated Host");
        let args = UpgradeArgs {
            release: args(&candidate, &server, home.path()),
            rollback: false,
            recover_only: false,
            timeout_secs: None,
            check: false,
            json: false,
        };
        let error = execute_at(&args, home.path()).await.err().unwrap();
        assert!(error.to_string().contains("--timeout-secs"), "{error:#}");
        assert_eq!(
            std::fs::read(home.path().join(".ww/bin/ww")).unwrap(),
            b"authenticated Host"
        );
        assert!(!home.path().join(".ww/bin/.ww-host-upgrade").exists());
        assert!(!server.requests.lock().unwrap().contains(&format!(
            "cat:/ipfs/{}",
            candidate.descriptor.hosts[0].artifact.root
        )));
    }

    #[tokio::test]
    async fn recovery_budget_is_checked_before_lock_creation_or_download() {
        for recovery_only in [false, true] {
            let fixture = Fixture::new(platform().unwrap());
            let server = Server::start(fixture.routes.clone()).await;
            let home = tempfile::tempdir().unwrap();
            let args = UpgradeArgs {
                release: args(&fixture, &server, home.path()),
                rollback: !recovery_only,
                recover_only: recovery_only,
                timeout_secs: None,
                check: false,
                json: false,
            };
            let error = execute_at(&args, home.path()).await.err().unwrap();
            assert!(error.to_string().contains("--timeout-secs"));
            assert!(!home.path().join(".ww-installation.lock").exists());
            assert!(!home.path().join(".ww").exists());
            assert!(server.requests.lock().unwrap().is_empty());
        }
    }

    #[tokio::test]
    async fn expired_budget_does_not_poll_download_future() {
        let called = std::sync::atomic::AtomicBool::new(false);
        let deadline = Instant::now().checked_sub(Duration::from_secs(1)).unwrap();
        let result = within_budget(Some(deadline), async {
            called.store(true, std::sync::atomic::Ordering::Relaxed);
            Ok(())
        })
        .await;
        assert!(result.is_err());
        assert!(!called.load(std::sync::atomic::Ordering::Relaxed));
        assert!(operation_deadline(Some(0)).is_err());
    }
}
