//! Offline construction and verification of authenticated release packages.

use std::{
    io::{Read, Write},
    path::{Path, PathBuf},
};

use anyhow::{ensure, Result};
use clap::{Parser, Subcommand};
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

fn main() -> Result<()> {
    match Cli::parse().command {
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
