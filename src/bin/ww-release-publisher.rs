//! Infrastructure executable. This binary does not start a Wetware Host.

use std::{
    io::{Read, Write},
    path::{Path, PathBuf},
    time::Duration,
};

use anyhow::{ensure, Context, Result};
use clap::{Parser, Subcommand};
use ww::{
    release_auth::TrustedKeys,
    release_package::{self, Package},
    release_publisher::{self, Config, KuboApi, Request},
};

#[derive(Parser)]
#[command(about = "Prepare and publish authenticated Wetware releases", version)]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    /// Build the descriptor and signed-manifest binding before offline signing.
    Prepare {
        #[arg(long)]
        tree: PathBuf,
        #[arg(long)]
        spool: PathBuf,
        #[arg(long)]
        metadata: PathBuf,
    },
    /// Validate the signed tree and build the immutable legacy-compatible graph.
    Finalize {
        #[arg(long)]
        tree: PathBuf,
        #[arg(long)]
        spool: PathBuf,
        #[arg(long)]
        trusted_keys: PathBuf,
    },
    /// Authenticate a complete local package without Kubo operations.
    Verify {
        #[arg(long)]
        spool: PathBuf,
        #[arg(long)]
        trusted_keys: PathBuf,
    },
    /// Retain immutable candidate content in a CI/disposable Kubo; no IPNS write.
    Seed {
        #[arg(long)]
        spool: PathBuf,
        #[arg(long)]
        trusted_keys: PathBuf,
        #[arg(long)]
        kubo_api: String,
    },
    /// Emit the content-bound request ID and installed-producer request JSON.
    Request {
        #[arg(long)]
        root: String,
    },
    /// Explicit local qualification entry point; reads the request on stdin.
    Publish {
        #[arg(long)]
        config: PathBuf,
        #[arg(long)]
        package: Option<PathBuf>,
        #[arg(long)]
        kubo_api: String,
    },
}

fn keys(path: &Path) -> Result<TrustedKeys> {
    TrustedKeys::from_bytes(&release_publisher::read_bounded(path, 122)?)
}

fn emit(value: &impl serde::Serialize) -> Result<()> {
    let raw = serde_json::to_vec(value)?;
    ensure!(
        raw.len() < 16 * 1024,
        "publisher result exceeds worker output limit"
    );
    let mut stdout = std::io::stdout().lock();
    stdout.write_all(&raw)?;
    stdout.write_all(b"\n")?;
    Ok(())
}

fn package_result(package: &Package, trust: &TrustedKeys) -> Result<serde_json::Value> {
    let release = package.authenticate(trust)?;
    Ok(serde_json::json!({
        "root": package.root().to_string(),
        "descriptor": release.descriptor_cid().to_string(),
        "sequence": release.descriptor().sequence,
        "request_id": Request::new(package.root()).request_id,
        "stages": package.stage_count(),
    }))
}

async fn publish(config_path: &Path, endpoint: &str, package: Option<&Path>) -> Result<()> {
    let mut bytes = Vec::new();
    std::io::stdin()
        .take(release_publisher::MAX_REQUEST_BYTES as u64 + 1)
        .read_to_end(&mut bytes)?;
    let request = Request::parse(&bytes)?;
    let operation = async {
        let config = Config::load(config_path)?;
        release_publisher::publish_request(request.clone(), &config, endpoint, package).await
    }
    .await;
    match operation {
        Ok(outcome) => {
            if !outcome.delivery_acknowledged {
                eprintln!("Release committed locally; IPNS delivery acknowledgement is unknown. Retry the same request.");
            }
            emit(&outcome)
        }
        Err(error) => {
            // A failed journal sync can leave the replacement snapshot visible.
            // Do not infer rollback from an error or emit untrusted diagnostics.
            eprintln!(
                "Release publication did not complete: {}",
                error.to_string().chars().take(1024).collect::<String>()
            );
            emit(&serde_json::json!({
                "version": 1,
                "request_id": request.request_id,
                "descriptor": request.descriptor,
                "local_committed": null,
                "delivery_acknowledged": false,
                "error": "publication-result-unknown",
            }))
        }
    }
}

#[tokio::main(worker_threads = 2)]
async fn main() -> Result<()> {
    match Cli::parse().command {
        Some(Command::Prepare {
            tree,
            spool,
            metadata,
        }) => {
            let metadata =
                serde_json::from_slice(&release_publisher::read_bounded(&metadata, 64 * 1024)?)?;
            let descriptor = release_package::prepare(&tree, &spool, metadata)?;
            emit(
                &serde_json::json!({"descriptor": descriptor.cid()?.to_string(), "sequence": descriptor.sequence}),
            )
        }
        Some(Command::Finalize {
            tree,
            spool,
            trusted_keys,
        }) => {
            let trust = keys(&trusted_keys)?;
            let package = release_package::finalize(&tree, &spool, &trust)?;
            emit(&package_result(&package, &trust)?)
        }
        Some(Command::Verify {
            spool,
            trusted_keys,
        }) => emit(&package_result(
            &Package::open(&spool)?,
            &keys(&trusted_keys)?,
        )?),
        Some(Command::Seed {
            spool,
            trusted_keys,
            kubo_api,
        }) => {
            let package = Package::open(&spool)?;
            let result = package_result(&package, &keys(&trusted_keys)?)?;
            let api = KuboApi::new(&kubo_api, Duration::from_secs(120))?;
            package.seed(api.content()).await?;
            emit(&result)
        }
        Some(Command::Request { root }) => {
            let request = Request::new(ww::ipfs::cid_identity::parse_cid(&root)?);
            ensure!(
                request.descriptor == root,
                "candidate CID must be canonical"
            );
            request.root()?;
            emit(&request)
        }
        Some(Command::Publish {
            config,
            package,
            kubo_api,
        }) => publish(&config, &kubo_api, package.as_deref()).await,
        None => {
            ensure!(
                std::env::var("WETWARE_PUBLISH_ABI").as_deref() == Ok("1"),
                "installed publisher requires WETWARE_PUBLISH_ABI=1"
            );
            let endpoint = std::env::var("KUBO_API")
                .context("installed publisher requires the configured Kubo Unix socket")?;
            ensure!(
                endpoint.starts_with("/unix/"),
                "installed publisher requires a Kubo Unix socket"
            );
            publish(Path::new(release_publisher::CONFIG_PATH), &endpoint, None).await
        }
    }
}
