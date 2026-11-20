//! Offline construction and verification of authenticated release packages.

use std::{
    io::{Read, Write},
    path::{Path, PathBuf},
};

use anyhow::{ensure, Result};
use clap::{Parser, Subcommand};
use ed25519_dalek::Signer;
use ww_release::{
    auth::TrustedKeys,
    package::{self, Package},
};

#[derive(Parser)]
#[command(
    about = "Prepare and verify authenticated Wetware release packages",
    version
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Sign exact manifest bytes with a canonical base58btc Ed25519 seed.
    Sign {
        #[arg(long)]
        manifest: PathBuf,
        #[arg(long)]
        signature: PathBuf,
        #[arg(long)]
        private_key: PathBuf,
        #[arg(long)]
        trusted_keys: PathBuf,
    },
    /// Export the seed's public key as Ed25519 SPKI DER, without signing.
    PublicKey {
        #[arg(long)]
        private_key: PathBuf,
        #[arg(long)]
        output: PathBuf,
    },
    /// Pack an unsigned content tree into a fresh immutable spool.
    PackTree {
        #[arg(long)]
        tree: PathBuf,
        #[arg(long)]
        spool: PathBuf,
    },
    /// Materialize an unsigned package tree into a fresh private directory.
    UnpackTree {
        #[arg(long)]
        spool: PathBuf,
        #[arg(long)]
        tree: PathBuf,
    },
    /// Import a complete bounded CAR into a fresh spool, without authentication.
    ImportCar {
        #[arg(long)]
        car: PathBuf,
        #[arg(long)]
        spool: PathBuf,
    },
    /// Export exactly one package's reachable graph as deterministic CARv1.
    ExportCar {
        #[arg(long)]
        spool: PathBuf,
        #[arg(long)]
        car: PathBuf,
    },
    /// Build the descriptor and manifest binding before offline signing.
    Prepare {
        #[arg(long)]
        tree: PathBuf,
        #[arg(long)]
        spool: PathBuf,
        #[arg(long)]
        metadata: PathBuf,
    },
    /// Validate the signed tree and build the immutable transport graph.
    Finalize {
        #[arg(long)]
        tree: PathBuf,
        #[arg(long)]
        spool: PathBuf,
        #[arg(long)]
        trusted_keys: PathBuf,
    },
    /// Authenticate a complete local package.
    Verify {
        #[arg(long)]
        spool: PathBuf,
        #[arg(long)]
        trusted_keys: PathBuf,
    },
}

fn read_bounded(path: &Path, limit: usize) -> Result<Vec<u8>> {
    let mut bytes = Vec::new();
    std::fs::File::open(path)?
        .take(limit as u64 + 1)
        .read_to_end(&mut bytes)?;
    ensure!(
        bytes.len() <= limit,
        "file exceeds byte limit: {}",
        path.display()
    );
    Ok(bytes)
}

fn keys(path: &Path) -> Result<TrustedKeys> {
    TrustedKeys::from_bytes(&read_bounded(path, 122)?)
}

fn emit(value: &impl serde::Serialize) -> Result<()> {
    let mut stdout = std::io::stdout().lock();
    serde_json::to_writer(&mut stdout, value)?;
    stdout.write_all(b"\n")?;
    Ok(())
}

fn package_result(package: &Package, trust: &TrustedKeys) -> Result<serde_json::Value> {
    let release = package.authenticate(trust)?;
    Ok(serde_json::json!({
        "root": package.root().to_string(),
        "descriptor": release.descriptor_cid().to_string(),
        "sequence": release.descriptor().sequence,
    }))
}

fn private_key(path: &Path) -> Result<ed25519_dalek::SigningKey> {
    let metadata = std::fs::symlink_metadata(path)?;
    ensure!(
        metadata.is_file(),
        "private key must be a regular file, not a symlink"
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        ensure!(
            metadata.permissions().mode() & 0o077 == 0,
            "private key must not grant group or other access"
        );
    }
    let bytes = read_bounded(path, 45)?;
    let text = std::str::from_utf8(&bytes)
        .map_err(|_| anyhow::anyhow!("private key must be UTF-8 base58btc"))?;
    rpc::keys::decode_canonical(text)
}

fn write_new(path: &Path, bytes: &[u8]) -> Result<()> {
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut output = options.open(path)?;
    if let Err(error) = output.write_all(bytes).and_then(|()| output.sync_all()) {
        let _ = std::fs::remove_file(path);
        return Err(error.into());
    }
    Ok(())
}

fn main() -> Result<()> {
    match Cli::parse().command {
        Command::Sign {
            manifest,
            signature,
            private_key: key_path,
            trusted_keys,
        } => {
            ensure!(
                std::fs::symlink_metadata(&manifest)?.is_file(),
                "manifest must be a regular file"
            );
            let release_dir = manifest.canonicalize()?.parent().unwrap().to_path_buf();
            ensure!(
                !key_path.canonicalize()?.starts_with(release_dir),
                "private signing key must be outside the release tree"
            );
            let key = private_key(&key_path)?;
            let manifest = read_bounded(&manifest, ww_release::auth::MAX_MANIFEST_BYTES)?;
            let signature_bytes = key.sign(&manifest).to_bytes();
            keys(&trusted_keys)?.authenticate(&manifest, &signature_bytes)?;
            write_new(&signature, &signature_bytes)
        }
        Command::PublicKey {
            private_key: key_path,
            output,
        } => {
            let key = private_key(&key_path)?;
            let mut der = vec![
                0x30, 0x2a, 0x30, 0x05, 0x06, 0x03, 0x2b, 0x65, 0x70, 0x03, 0x21, 0x00,
            ];
            der.extend_from_slice(key.verifying_key().as_bytes());
            write_new(&output, &der)
        }
        Command::PackTree { tree, spool } => {
            let package = package::pack_tree(&tree, &spool)?;
            emit(&serde_json::json!({ "root": package.root().to_string() }))
        }
        Command::UnpackTree { spool, tree } => {
            let package = Package::open(&spool)?;
            package::unpack_tree(&package, &tree)?;
            emit(&serde_json::json!({ "root": package.root().to_string() }))
        }
        Command::ImportCar { car, spool } => {
            let package = package::read_car(&car, &spool)?;
            emit(&serde_json::json!({ "root": package.root().to_string() }))
        }
        Command::ExportCar { spool, car } => {
            let package = Package::open(&spool)?;
            package::write_car(&package, &car)?;
            emit(&serde_json::json!({ "root": package.root().to_string() }))
        }
        Command::Prepare {
            tree,
            spool,
            metadata,
        } => {
            let metadata = serde_json::from_slice(&read_bounded(&metadata, 64 * 1024)?)?;
            let descriptor = package::prepare(&tree, &spool, metadata)?;
            emit(&serde_json::json!({
                "descriptor": descriptor.cid()?.to_string(),
                "sequence": descriptor.sequence,
            }))
        }
        Command::Finalize {
            tree,
            spool,
            trusted_keys,
        } => {
            let trust = keys(&trusted_keys)?;
            let package = package::finalize(&tree, &spool, &trust)?;
            emit(&package_result(&package, &trust)?)
        }
        Command::Verify {
            spool,
            trusted_keys,
        } => emit(&package_result(
            &Package::open(&spool)?,
            &keys(&trusted_keys)?,
        )?),
    }
}
