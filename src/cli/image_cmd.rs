//! One-shot image preparation without deployment or guest execution.

use std::{path::PathBuf, time::Duration};

use anyhow::{bail, Context, Result};
use clap::{Args, Subcommand};
use serde::Serialize;
use tokio::sync::watch;
use ww::{cell::image, ipfs, stem::Source as _};

#[derive(Subcommand)]
pub enum ImageAction {
    /// Check static PID0 compatibility. Imports content and retains shared Kubo pins.
    Check(Check),
}

#[derive(Args)]
pub struct Check {
    /// Ordered SOURCE[:/TARGET] overrides, using the same grammar as ww run.
    #[arg(value_name = "MOUNT")]
    mounts: Vec<String>,
    /// Installed FHS directory (default: $HOME/.ww/fhs).
    #[arg(long, env = "WW_FHS_ROOT", value_name = "PATH")]
    fhs_root: Option<PathBuf>,
    /// Host configuration roots containing etc/ns.
    #[arg(long, value_name = "PATH")]
    namespace_root: Vec<PathBuf>,
    /// Identity used by ww run to identify its own ww namespace publisher.
    /// Read only when that namespace has both a PeerID and a bootstrap CID.
    #[arg(long, env = "WW_IDENTITY", value_name = "PATH")]
    identity: Option<PathBuf>,
    /// Read one finalized Atom head; do not follow changes.
    #[arg(long, conflicts_with = "ipns_stem")]
    stem: Option<String>,
    /// Read one signed IPNS head using the existing follower watermark.
    #[arg(long, conflicts_with = "stem")]
    ipns_stem: Option<String>,
    #[arg(
        long,
        default_value = "http://localhost:8080",
        env = "IPFS_ROUTING_API"
    )]
    ipns_routing_url: String,
    #[arg(long, default_value = "http://127.0.0.1:8545")]
    rpc_url: String,
    #[arg(long, default_value = "6")]
    confirmation_depth: u64,
    #[arg(long, default_value = "http://localhost:5001", env = "IPFS_API")]
    ipfs_url: String,
    /// Emit a machine-readable success result. Errors go to stderr.
    #[arg(long)]
    json: bool,
}

const TIMEOUT: Duration = Duration::from_secs(30);
const STAGES: &[&str] = &[
    "source-resolution",
    "filesystem-composition",
    "kernel-selection",
    "verified-component-bytes",
    "production-p3-static-validation",
];

#[derive(Debug, Serialize)]
struct Report {
    effective_root: String,
    kernel_source: String,
    kernel_runtime: String,
    composer_profile: &'static str,
    hash_algorithm: &'static str,
    static_validation: &'static str,
    stages: &'static [&'static str],
    retained_roots: Vec<String>,
}

impl Report {
    fn new(kernel: &ww::kernel::PreparedKernel, retained: &[cid::Cid]) -> Self {
        Self {
            effective_root: kernel.effective_root().to_string(),
            kernel_source: kernel.source_file().to_string(),
            kernel_runtime: kernel.runtime_cid().to_string(),
            composer_profile: image::COMPOSER_PROFILE,
            hash_algorithm: "sha2-256",
            static_validation: "PASS",
            stages: STAGES,
            retained_roots: retained.iter().map(ToString::to_string).collect(),
        }
    }

    fn render(&self, json: bool) -> Result<String> {
        if json {
            return Ok(serde_json::to_string_pretty(self)?);
        }
        Ok(format!(
            "Effective root:    {}\nKernel source:     {}\nKernel runtime:    {}\nComposer profile:  {} ({})\nStatic validation: {}\nCompleted stages:  {}\nRetained roots:    {}\nStatic validation does not establish runtime readiness.",
            self.effective_root, self.kernel_source, self.kernel_runtime,
            self.composer_profile, self.hash_algorithm, self.static_validation,
            self.stages.join(", "), self.retained_roots.join(", "),
        ))
    }
}

impl ImageAction {
    pub async fn run(self) -> Result<()> {
        match self {
            Self::Check(check) => check.run().await,
        }
    }
}

async fn cancellable<T>(
    cancel: &mut watch::Receiver<bool>,
    future: impl std::future::Future<Output = Result<T>>,
) -> Result<T> {
    anyhow::ensure!(!*cancel.borrow(), "image check cancelled");
    tokio::select! {
        biased;
        _ = cancel.changed() => bail!("image check cancelled"),
        result = tokio::time::timeout(TIMEOUT, future) => result.context("image check operation exceeded 30 seconds")?,
    }
}

// Namespace resolution owns a deadline per name, including bootstrap fallback.
// An outer timeout would race that fallback and change the selected image.
async fn resolve_namespaces(
    configs: &[ww::ns::NamespaceConfig],
    client: &ipfs::BootClient,
    cancel: &mut watch::Receiver<bool>,
) -> Result<Vec<(String, String)>> {
    anyhow::ensure!(!*cancel.borrow(), "image check cancelled");
    let status = ww::metrics::RuntimeStatus::starting();
    tokio::select! {
        biased;
        _ = cancel.changed() => bail!("image check cancelled"),
        result = ww::ns::resolve_namespaces(configs, client, &status) => result,
    }
}

fn publisher_baseline(
    configs: &[ww::ns::NamespaceConfig],
    identity: Option<&std::path::Path>,
) -> Result<Option<String>> {
    let Some(config) = configs.iter().find(|config| config.name == "ww") else {
        return Ok(None);
    };
    let Some(bootstrap) = config.bootstrap_ipfs_path()? else {
        return Ok(None);
    };
    let Ok(name) = ww::ipns::parse_name(&config.ipns) else {
        return Ok(None);
    };
    // Boot reads this key before deciding whether to publish the ww namespace.
    // Reading the key here creates no Membrane or signing service.
    let (key, _, _) = super::Commands::resolve_identity(identity, false)
        .context("cannot determine ww namespace startup publication; specify --identity or check explicit immutable mounts")?;
    let peer = ww::keys::to_libp2p(&key)?.public().to_peer_id();
    Ok((peer == name).then_some(bootstrap))
}

fn validate_resolution(
    configs: &[ww::ns::NamespaceConfig],
    resolved: &[(String, String)],
    published_baseline: Option<&str>,
) -> Result<()> {
    for config in configs {
        anyhow::ensure!(
            config.ipfs_path()?.is_none() || resolved.iter().any(|(name, _)| name == &config.name),
            "namespace {} is unavailable; image check cannot validate a partial image",
            config.name
        );
    }
    if let Some(expected) = published_baseline {
        anyhow::ensure!(
                    resolved.iter().any(|(name, path)| name == "ww" && path == expected),
                    "host-owned ww namespace does not resolve to its configured bootstrap {expected}; ww run publishes this bootstrap before resolving names. Image check cannot reproduce that publication; check explicit immutable mounts or resolve the publisher state first"
                );
    }
    Ok(())
}

impl Check {
    async fn head(&self) -> Result<Option<cid::Cid>> {
        let update = if let Some(address) = &self.stem {
            let mut source = ww::stem::atom::Source::new(ww::stem::atom::Config::new(
                self.rpc_url.clone(),
                super::parse_contract_address(address)?,
                self.confirmation_depth,
            ))?;
            source.current().await?
        } else if let Some(name) = &self.ipns_stem {
            ww::stem::ipns::Source::snapshot(ww::stem::ipns::Config::new(
                ww::ipns::parse_name(name)?,
                self.ipns_routing_url.clone(),
                dirs::home_dir()
                    .context("--ipns-stem requires HOME")?
                    .join(".ww"),
            ))
            .await?
        } else {
            return Ok(None);
        };
        match update {
            ww::stem::Update::Head(head) => Ok(Some(head.cid)),
            ww::stem::Update::InvalidHead(invalid) => {
                bail!("invalid authoritative head: {}", invalid.reason)
            }
        }
    }

    async fn run(mut self) -> Result<()> {
        ww::config::init_tracing_to_stderr(true);
        // Resolve local configuration before any backend effects.
        let authoritative = self.stem.is_some() || self.ipns_stem.is_some();
        let (baseline, mounts) = super::Commands::startup_mounts(
            &self.mounts,
            self.fhs_root.take(),
            &mut self.namespace_root,
            authoritative,
        )
        .context("source configuration")?;
        let namespaces = super::Commands::startup_namespaces(
            &mounts,
            &self.namespace_root,
            authoritative,
            baseline.is_some(),
        )
        .context("source configuration")?;
        let published_baseline = publisher_baseline(&namespaces, self.identity.as_deref())?;
        let (owner, resources) = cache::HostFsOwner::start(
            tokio::runtime::Handle::current(),
            cache::HostLimits::default(),
        )
        .context("image check filesystem resources")?;
        let (cancel_tx, mut cancel) = watch::channel(false);
        let check = self.prepare(
            baseline,
            mounts,
            namespaces,
            published_baseline,
            &resources,
            &mut cancel,
        );
        tokio::pin!(check);
        let result = tokio::select! {
            result = &mut check => result,
            signal = tokio::signal::ctrl_c() => {
                let _ = cancel_tx.send(true);
                let result = check.await;
                signal.context("install image check cancellation handler")
                    .and(result)
                    .and_then(|_| Err(anyhow::anyhow!("image check cancelled")))
            }
        };
        let cleanup = owner
            .shutdown()
            .await
            .context("image check local filesystem cleanup");
        let report = with_cleanup(result, cleanup)?;
        println!("{}", report.render(self.json)?);
        Ok(())
    }

    async fn prepare(
        &self,
        baseline: Option<ww::cell::mount::Mount>,
        mounts: Vec<ww::cell::mount::Mount>,
        namespaces: Vec<ww::ns::NamespaceConfig>,
        published_baseline: Option<String>,
        resources: &std::sync::Arc<cache::HostResources>,
        cancel: &mut watch::Receiver<bool>,
    ) -> Result<Report> {
        let mut retained = Vec::new();
        let (scope, scope_owner, finalizer, completion) = resources.try_filesystem_scope()?;
        let outcome: Result<Report> = async {
            let client = ipfs::HttpClient::new(self.ipfs_url.clone());
            let boot = ipfs::BootClient::one_attempt(client, TIMEOUT);
            let head = cancellable(cancel, self.head()).await.context("authoritative head resolution")?;
            let resolved = resolve_namespaces(&namespaces, &boot, cancel)
                .await.context("namespace resolution (image check does not publish names)")?;
            validate_resolution(&namespaces, &resolved, published_baseline.as_deref())?;
            let has_baseline = baseline.is_some();
            let mut all_mounts: Vec<_> = baseline.into_iter().collect();
            all_mounts.extend(resolved.into_iter().map(|(_, source)| ww::cell::mount::Mount {
                source, target: image::GuestTarget::root(),
            }));
            all_mounts.extend(mounts);
            let transaction = resources.try_foreground_transaction(resources.try_logical_io()?, &[&scope])?;
            let resolved = image::resolve_frozen_mounts_with_cancel(
                &all_mounts, &boot, &transaction, head.is_some(), cancel,
            ).await.context("source capture and retention")?;
            drop(transaction);
            retained.extend(resolved.retained_roots);
            let mut frozen = resolved.mounts;
            let baseline = has_baseline.then(|| frozen.remove(0));
            if let Some(head) = head {
                cancellable(cancel, boot.pin_add_once(&head.to_string())).await
                    .with_context(|| format!("authoritative head retention {head}; failed acknowledgement may leave a shared pin"))?;
                retained.push(head);
            }
            let effective = ww::image::compose_root(head, baseline.as_ref(), &frozen, &boot, cancel)
                .await.context("filesystem composition")?;
            retained.push(effective);
            retained.sort();
            retained.dedup();
            let (runtime, _clock) = ww::cell::engine::runtime_engine()
                .map_err(|error| anyhow::anyhow!("canonical engine: {error:#}"))?;
            let kernel = ww::image::prepare_kernel(
                effective, &boot, runtime, &ww::kernel::Compilation::default(), cancel,
            ).await.context("kernel selection and static validation")?;
            Ok(Report::new(&kernel, &retained))
        }.await;
        drop(scope_owner);
        drop(finalizer);
        let cleanup = completion
            .wait()
            .await
            .context("image check snapshot cleanup");
        if !retained.is_empty() {
            eprintln!(
                "Shared Kubo pins retained (no exclusive ownership): {}",
                retained
                    .iter()
                    .map(ToString::to_string)
                    .collect::<Vec<_>>()
                    .join(", ")
            );
        }
        with_cleanup(outcome, cleanup)
    }
}

// Preserve both preparation and cleanup failures in the command's error output.
fn with_cleanup<T>(outcome: Result<T>, cleanup: Result<()>) -> Result<T> {
    match (outcome, cleanup) {
        (result, Ok(())) => result,
        (Ok(_), Err(error)) => Err(error),
        (Err(error), Err(cleanup)) => {
            Err(error.context(format!("cleanup also failed: {cleanup:#}")))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ROOT: &str = "bafybeidpy7qr6ny6gz7tjqnj4kfnr6inigfle3boepq6afg6uzuc5h4tf4";

    fn config() -> ww::ns::NamespaceConfig {
        ww::ns::NamespaceConfig {
            name: "ww".into(),
            ipns: "example.test".into(),
            bootstrap: format!("/ipfs/{ROOT}"),
        }
    }

    #[tokio::test]
    async fn namespace_timeout_keeps_bootstrap_fallback_and_cancellation() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let boot = ipfs::BootClient::one_attempt(
            ipfs::HttpClient::new(format!("http://{}", listener.local_addr().unwrap())),
            Duration::from_millis(20),
        );
        // Leave the listener open without serving a response: name resolution stalls.
        let (tx, mut cancel) = watch::channel(false);
        let configs = [config()];
        let resolved = resolve_namespaces(&configs, &boot, &mut cancel)
            .await
            .unwrap();
        assert_eq!(resolved, vec![("ww".into(), format!("/ipfs/{ROOT}"))]);
        validate_resolution(&configs, &resolved, None).unwrap();
        tx.send(true).unwrap();
        assert!(resolve_namespaces(&configs, &boot, &mut cancel)
            .await
            .unwrap_err()
            .to_string()
            .contains("cancelled"));
    }

    #[test]
    fn unresolved_images_and_unpublished_bootstrap_cannot_report_success() {
        let configs = [config()];
        let expected = format!("/ipfs/{ROOT}");
        assert!(validate_resolution(&configs, &[], None)
            .unwrap_err()
            .to_string()
            .contains("cannot validate a partial image"));
        assert!(validate_resolution(
            &configs,
            &[("ww".into(), "/ipfs/different".into())],
            Some(&expected)
        )
        .unwrap_err()
        .to_string()
        .contains("does not resolve to its configured bootstrap"));
        validate_resolution(
            &configs,
            &[("ww".into(), expected.clone())],
            Some(&expected),
        )
        .unwrap();
        let empty = [ww::ns::NamespaceConfig {
            name: "empty".into(),
            ipns: String::new(),
            bootstrap: String::new(),
        }];
        validate_resolution(&empty, &[], None).unwrap();
    }

    #[test]
    fn publication_policy_uses_the_run_identity_without_writing_state() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("identity");
        let key = ww::keys::generate().unwrap();
        ww::keys::save(&key, &path).unwrap();
        let before = std::fs::read(&path).unwrap();
        let mut config = config();
        config.ipns = ww::keys::to_libp2p(&key)
            .unwrap()
            .public()
            .to_peer_id()
            .to_string();
        assert_eq!(
            publisher_baseline(&[config.clone()], Some(&path)).unwrap(),
            Some(format!("/ipfs/{ROOT}"))
        );
        assert_eq!(std::fs::read(&path).unwrap(), before);
        assert!(
            publisher_baseline(&[config.clone()], Some(&directory.path().join("missing"))).is_err()
        );
        config.ipns = ww::keys::to_libp2p(&ww::keys::generate().unwrap())
            .unwrap()
            .public()
            .to_peer_id()
            .to_string();
        assert!(publisher_baseline(&[config.clone()], Some(&path))
            .unwrap()
            .is_none());
        config.ipns = "example.test".into();
        assert!(
            publisher_baseline(&[config], Some(&directory.path().join("missing")))
                .unwrap()
                .is_none()
        );
    }

    #[tokio::test]
    async fn failed_owned_cleanup_blocks_success_and_keeps_both_diagnostics() {
        let (owner, resources) = cache::HostFsOwner::start(
            tokio::runtime::Handle::current(),
            cache::HostLimits::default(),
        )
        .unwrap();
        let (scope, scope_owner, finalizer, completion) = resources.try_filesystem_scope().unwrap();
        scope
            .own_cleanup(cache::FilesystemOperation::ProcessCleanup, || {
                Err(std::io::Error::new(
                    std::io::ErrorKind::PermissionDenied,
                    "cleanup fixture denied",
                ))
            })
            .unwrap();
        drop(scope_owner);
        drop(finalizer);
        let error = with_cleanup(
            Ok("must not publish success"),
            completion
                .wait()
                .await
                .context("image check snapshot cleanup"),
        )
        .unwrap_err();
        assert!(format!("{error:#}").contains("cleanup fixture denied"));
        let error = with_cleanup::<()>(
            Err(anyhow::anyhow!("kernel validation failed")),
            completion
                .wait()
                .await
                .context("image check snapshot cleanup"),
        )
        .unwrap_err();
        let diagnostic = format!("{error:#}");
        assert!(diagnostic.contains("kernel validation failed"));
        assert!(diagnostic.contains("cleanup fixture denied"));
        owner.shutdown().await.unwrap();
    }
}
