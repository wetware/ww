//! Explicit stopped admission. This command never registers or starts a service.

use std::{
    path::PathBuf,
    time::{Duration, Instant},
};

use anyhow::{ensure, Context, Result};
use clap::{Args, Subcommand};
use ww::upgrade::{
    provenance::IndependentTrustSource,
    service::NativeService,
    startup::{InvocationBinding, StartupContractV1},
    Engine,
};

use super::{startup, RunArgs};

#[derive(Subcommand)]
pub(super) enum StartupAction {
    /// Authorize exact startup inputs while the installation is stopped.
    Admit(AdmitArgs),
    /// Private, descriptor-bound static preparation protocol.
    #[command(hide = true)]
    Preflight {
        #[arg(long)]
        fd: i32,
    },
}

#[derive(Args)]
pub(super) struct AdmitArgs {
    /// Prepare and report the proposed identity without changing admission.
    #[arg(long)]
    check: bool,
    #[arg(long)]
    json: bool,
    /// Positive total operation budget; preparation can retain cache and pins.
    #[arg(long, value_name = "N", value_parser = clap::value_parser!(u32).range(1..))]
    timeout_secs: u32,
    /// Explicitly rebind current independent release trust during stopped admission.
    #[arg(long, value_name = "FILE")]
    trusted_release_keys: Option<PathBuf>,
    #[command(subcommand)]
    invocation: AdmitInvocation,
}

#[derive(Subcommand)]
#[allow(clippy::large_enum_variant)]
enum AdmitInvocation {
    /// Admit the actual loaded managed service configuration.
    Service,
    /// Admit these exact standalone run options.
    Run(RunArgs),
}

impl StartupAction {
    pub(super) async fn run(self) -> Result<()> {
        match self {
            Self::Preflight { fd } => startup::native_preflight(fd).await,
            Self::Admit(args) => {
                let result = admit(&args).await;
                if args.json {
                    let value = match &result {
                        Ok(value) => {
                            serde_json::json!({"version":1,"operation":"startup admit","status":"completed","activation":false,"result":value})
                        }
                        Err(error) => {
                            serde_json::json!({"version":1,"operation":"startup admit","status":"failed","activation":false,"error":format!("{error:#}")})
                        }
                    };
                    println!("{}", serde_json::to_string(&value)?);
                } else if result.is_ok() {
                    eprintln!(
                        "Startup inputs {}. No service was started.",
                        if args.check { "checked" } else { "admitted" }
                    );
                }
                result.map(|_| ())
            }
        }
    }
}

async fn admit(args: &AdmitArgs) -> Result<serde_json::Value> {
    let timeout = Duration::from_secs(u64::from(args.timeout_secs));
    let deadline = Instant::now()
        .checked_add(timeout)
        .context("startup admission deadline overflow")?;
    let home = dirs::home_dir().context("cannot determine installation HOME")?;
    let mut engine = Engine::open(&home)?;
    let mut service = NativeService::new(&home)?;
    if let Some(outcome) = engine.recover_admission(&mut service, deadline, args.check)? {
        return Ok(serde_json::to_value(outcome)?);
    }
    // Exclude admitted instances before capture, including preparation effects.
    let admission = engine.begin_admission(&mut service, deadline)?;
    let state = admission.startup_state()?;
    let trust = match &args.trusted_release_keys {
        Some(path) => IndependentTrustSource::from_path(path)?,
        None => IndependentTrustSource::read_bound(&state.trusted_release_keys)?,
    };
    let (run, invocation) = match &args.invocation {
        AdmitInvocation::Run(run) => (
            startup::normalize_run(run.clone())?,
            InvocationBinding::Standalone,
        ),
        AdmitInvocation::Service => service_run(&home, deadline)?,
    };
    ensure!(
        run.config.home == home,
        "startup HOME differs from the installation"
    );
    ensure!(
        !run.config.insecure_ephemeral,
        "startup admission requires persistent identity"
    );
    let inputs = startup::capture_for_admission(run, deadline).await?;
    let generation = admission
        .startup_contract()?
        .map(|contract| contract.identity.next_generation())
        .transpose()?
        .unwrap_or(1);
    let contract = StartupContractV1::new(
        state.installation_id,
        generation,
        state.native.artifact.clone(),
        inputs.startup_identity()?,
        invocation,
        trust.path().to_owned(),
    )?;
    let outcome = admission.finish(&mut service, contract, &inputs, args.check)?;
    Ok(serde_json::to_value(outcome)?)
}

#[cfg(target_os = "linux")]
pub(super) fn service_run(
    home: &std::path::Path,
    deadline: Instant,
) -> Result<(startup::NormalizedRun, InvocationBinding)> {
    let observed = ww::upgrade::native_linux::observe_invocation(home, deadline)?;
    let context = startup::InvocationContext::service(
        observed.cwd,
        observed.home,
        observed.uid,
        &observed.environment,
    )?;
    let run = startup::normalize_service_run(&observed.argv, &observed.environment, context)?;
    Ok((
        run,
        InvocationBinding::Managed {
            service: observed.identity,
        },
    ))
}

#[cfg(not(target_os = "linux"))]
pub(super) fn service_run(
    _: &std::path::Path,
    _: Instant,
) -> Result<(startup::NormalizedRun, InvocationBinding)> {
    anyhow::bail!("managed startup admission is unavailable: native process attestation is not qualified on this platform")
}
