//! Authenticated release selection and the explicit native replacement command.
//!
//! Release metadata supplies authority; the installed receipt supplies ownership.
//! NativeService separately refuses replacement until operational compatibility
//! and, for running services, child-side restart admission are qualified.

use anyhow::{bail, ensure, Context, Result};
use clap::Args;
use serde::{Deserialize, Serialize};
use std::fs::OpenOptions;
use std::io::Read;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::time::Duration;
use ww::release::auth::{TrustedKeys, MAX_MANIFEST_BYTES};
use ww::release::descriptor::{
    binding_cid, verify_transport_root, AuthenticatedRelease, Selection, COMPATIBILITY_PROFILE,
};
use ww::upgrade::service::ServiceControl;
use ww::upgrade::{
    ArtifactIdentity, DigestAlgorithm, Engine, Outcome, PreverifiedCandidate, UpgradeRequest,
};

const MAX_HOST_BYTES: usize = 1024 * 1024 * 1024;
const MAX_FHS_BYTES: usize = 256 * 1024 * 1024;
const MAX_ROOT_BYTES: usize = 2 * 1024 * 1024;
const DOWNLOAD_TIMEOUT: Duration = Duration::from_secs(300);
// The native adapter currently refuses operational admission. This bounds that
// preflight; it is not a qualified live restart/readiness timeout.
const ADMISSION_TIMEOUT: Duration = Duration::from_secs(30);

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
    #[arg(long, conflicts_with = "release")]
    pub rollback: bool,
    /// Authenticated bootstrap recovery; never selects a new operation.
    #[arg(long, hide = true, requires = "release", conflicts_with_all = ["rollback", "check"])]
    pub recover_only: bool,
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
    pub authenticated: AuthenticatedRelease,
    pub selection: Selection,
    pub host: Option<Vec<u8>>,
    pub kernel: Option<Vec<u8>>,
    pub status: Option<Vec<u8>>,
}

/// This record is an untrusted locator, never authentication evidence.
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct InstalledRelease {
    pub version: u8,
    pub release: String,
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
    let keys = TrustedKeys::from_bytes(&read_bounded(&args.trusted_release_keys, 122)?)?;
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
        verify_transport_root(root, &root_bytes, descriptor_cid)?;
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
            authenticated,
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

async fn execute_at(args: &UpgradeArgs, home: &Path) -> Result<ExecutionReport> {
    let root = home.join(".ww");
    let target = root.join("bin/ww");
    let mut service = ww::upgrade::service::NativeService::new(home)?;
    if args.recover_only {
        let mut engine = Engine::open(home)?;
        engine.require_pending_recovery()?;
        let before = observe_artifact(&target)?;
        let executable = read_bounded(&std::env::current_exe()?, MAX_HOST_BYTES)?;
        let authenticated = authenticate_recovery_executable(&args.release, &executable).await?;
        let selected_artifact = Some(authenticated.artifact().clone());
        let outcome =
            engine.recover_authenticated(&mut service, ADMISSION_TIMEOUT, authenticated)?;
        return completed(outcome, selected_artifact, target, before);
    }
    if args.check {
        ensure!(!args.rollback, "rollback preflight remains unavailable until native compatibility admission is qualified");
        let lock = ww::installation_state::MutationLock::acquire(home)?;
        lock.reject_pending()?;
        let before = observe_artifact(&target)?;
        let receipt = ww::installation_state::Receipt::load(&root)?;
        let installed = installed_release(&root, &args.release).await?;
        let installed_bytes = read_bounded(&root.join("bin/ww"), MAX_HOST_BYTES)?;
        installed.selection.verify_host(&installed_bytes)?;
        ensure!(
            receipt.executable_digest() == blake3::hash(&installed_bytes).to_hex().as_str(),
            "installed Host does not match ownership receipt"
        );
        let candidate = select_release(&args.release, Scope::Host).await?;
        let current = &installed.selection.host().artifact;
        let next = &candidate.selection.host().artifact;
        if current.sha256 == next.sha256 && current.size == next.size {
            let selected =
                PreverifiedCandidate::from_release(&candidate.authenticated, platform()?)?;
            return completed(
                Outcome::AlreadyInstalled,
                Some(selected.artifact().clone()),
                target,
                before,
            );
        }
        let _snapshot = service.snapshot()?;
        bail!("native Host upgrade preflight cannot certify candidate compatibility with the effective deployment; native operational integration remains unavailable; no Host or service was changed");
    }
    let mut engine = Engine::open(home)?;
    let before = observe_artifact(&target)?;
    // Resolve recovery under the shared lock before any release discovery.
    let recovery = engine.recover(&mut service, ADMISSION_TIMEOUT)?;
    if recovery != Outcome::NoPendingTransaction {
        return completed(recovery, None, target, before);
    }
    if args.rollback {
        let outcome = engine.operator_rollback(&mut service, ADMISSION_TIMEOUT)?;
        return completed(outcome, None, target, before);
    }
    let installed = installed_release(&root, &args.release).await?;
    let candidate = select_release(&args.release, Scope::Host).await?;
    let request = UpgradeRequest {
        installed: PreverifiedCandidate::from_release(&installed.authenticated, platform()?)?,
        candidate: PreverifiedCandidate::from_release(&candidate.authenticated, platform()?)?,
        bytes: std::io::Cursor::new(candidate.host.context("selected native artifact missing")?),
    };
    let selected_artifact = Some(request.candidate.artifact().clone());
    let outcome = engine.operator_upgrade(&mut service, ADMISSION_TIMEOUT, || Ok(request))?;
    completed(outcome, selected_artifact, target, before)
}

async fn authenticate_recovery_executable(
    args: &ReleaseArgs,
    executable: &[u8],
) -> Result<PreverifiedCandidate> {
    let selected = select_release(args, Scope::Host).await?;
    ensure!(
        selected.host.as_deref() == Some(executable),
        "running recovery executable does not match the authenticated selected Host"
    );
    PreverifiedCandidate::from_release(&selected.authenticated, platform()?)
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
            let key = SigningKey::from_bytes(&[7; 32]);
            let name = libp2p::identity::Keypair::generate_ed25519()
                .public()
                .to_peer_id();
            let provenance = Provenance {
                repository: SOURCE_REPOSITORY.into(),
                revision: "a".repeat(40),
                builder: "test".into(),
            };
            let host = file(platform_path(target).unwrap(), b"authenticated Host");
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
                (
                    format!("cat:/ipfs/{}", host.root),
                    b"authenticated Host".to_vec(),
                ),
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
        let candidate =
            PreverifiedCandidate::from_release(&selected.authenticated, platform().unwrap())
                .unwrap();
        assert!(format!("{candidate:?}")
            .contains(&hex::encode(fixture.descriptor.hosts[0].artifact.sha256)));
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
}
