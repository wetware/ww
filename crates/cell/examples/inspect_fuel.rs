//! Launch one native P3 command and request one owner-coherent fuel snapshot.
//!
//! This trusted local embedder grants no diagnostic interface to the guest.
//! Usage: cargo run -p cell --example inspect_fuel -- command.wasm [total] [epoch]

use anyhow::{anyhow, bail, Context, Result};
use cell::proc::FuelEstimator;
use cell::{Builder, FilesystemOwnership, Program};
use std::time::Duration;

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<()> {
    let mut args = std::env::args_os().skip(1);
    let path = args.next().context(
        "usage: cargo run -p cell --example inspect_fuel -- command.wasm [total_budget] [epoch_budget]",
    )?;
    let total = args
        .next()
        .map(|value| value.to_string_lossy().parse::<u64>())
        .transpose()
        .context("total_budget must be an unsigned integer")?;
    let epoch = args
        .next()
        .map(|value| value.to_string_lossy().parse::<u64>())
        .transpose()
        .context("epoch_budget must be an unsigned integer")?
        .unwrap_or(0);
    if args.next().is_some() {
        bail!("unexpected argument after epoch_budget");
    }
    let bytes = std::fs::read(&path).context("read native P3 command")?;
    let (host_owner, resources) = cache::HostFsOwner::start(
        tokio::runtime::Handle::current(),
        cache::HostLimits::new(4, 2, 1)?,
    )?;

    let execution = async {
        let filesystem = FilesystemOwnership::try_new(cache::CellResources::new(
            resources,
            cache::CellLimits::new(1)?,
        ))?;
        let completion = filesystem.completion();
        let result = async {
            let (runtime, mut publisher) = cell::engine::runtime_engine()?;
            let (builder, _transport) = Builder::ordinary(
                Program::Bytes(bytes),
                tokio::io::empty(),
                tokio::io::sink(),
                tokio::io::sink(),
                filesystem,
            );
            let builder = builder.with_runtime_engine(runtime);
            let builder = match total {
                Some(total) => {
                    builder.with_fuel_estimator(FuelEstimator::new_oneshot(total, epoch, 0))
                }
                None => builder,
            };
            let mut process = builder.build().await?;
            let inspector = process.fuel_inspector()?;

            // Poll the request first so the initial successfully installed grant
            // can answer it before executable instantiation starts.
            let inspect = async {
                let snapshot = inspector.inspect(Duration::from_secs(1)).await?;
                println!("{snapshot:#?}");
                println!("snapshot age at report: {:?}", snapshot.age());
                if let Some(grant) = snapshot.last_applied_grant {
                    println!(
                        "applied authority {} = min(desired {}, total {:?}, epoch {:?}); \
                         installed physical fuel {} includes reserve {}",
                        grant.authority,
                        grant.desired_quantum,
                        grant.total_remaining,
                        grant.epoch_remaining,
                        grant.physical,
                        cell::sched::YIELD_RESERVE,
                    );
                }
                Ok::<_, anyhow::Error>(())
            };
            let run = async {
                // Epoch ticks belong to execution, not diagnostic polling.
                let ticks = async {
                    let mut tick =
                        tokio::time::interval(Duration::from_millis(cell::sched::EPOCH_TICK_MS));
                    tick.tick().await;
                    loop {
                        tick.tick().await;
                        publisher.tick();
                    }
                };
                tokio::time::timeout(Duration::from_secs(5), async {
                    tokio::select! {
                        result = process.run() => result,
                        () = ticks => unreachable!("epoch publisher runs until command stops"),
                    }
                })
                .await
                .map_err(|_| anyhow!("command exceeded the reporter's five-second run limit"))?
            };
            let (inspection, run) = tokio::join!(biased; inspect, run);
            inspection?;
            run
        }
        .await;
        // Completion follows actual Store destruction, including timeout and
        // build failure. Builder::build starts the existing filesystem finalizer.
        let cleanup = completion.wait().await;
        result?;
        cleanup?;
        Ok::<_, anyhow::Error>(())
    }
    .await;
    let shutdown = host_owner.shutdown().await;
    execution?;
    shutdown?;
    Ok(())
}
