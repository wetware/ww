//! Transitional Rust PID0 for the shipped Wetware production composition.
//!
//! This component is deliberately explicit. Each generation grafts the three
//! capabilities it needs, transparently republishes the temporary compatibility
//! membrane, installs the existing status component at `/status`, and commits
//! readiness. Non-TTY execution probes the guarded host capability for epoch
//! replacement.

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use system::{get_graft_cap, membrane_capnp};
use wasip2::cli::stderr::get_stderr;
use wasip2::exports::cli::run::Guest;

#[allow(dead_code, clippy::extra_unused_type_parameters)]
mod system_capnp {
    include!(concat!(env!("OUT_DIR"), "/system_capnp.rs"));
}

#[allow(
    dead_code,
    clippy::extra_unused_type_parameters,
    clippy::match_single_binding
)]
mod auth_capnp {
    include!(concat!(env!("OUT_DIR"), "/auth_capnp.rs"));
}

#[allow(dead_code, clippy::extra_unused_type_parameters)]
mod stem_capnp {
    include!(concat!(env!("OUT_DIR"), "/stem_capnp.rs"));
}

mod kernel_runtime {
    wit_bindgen::generate!({
        path: "../kernel/wit",
        world: "pid0",
    });
}

mod pid0_runtime_abi {
    include!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../kernel/abi/pid0_export_membrane_cap.rs"
    ));
}

use kernel_runtime::wetware::kernel_runtime::readiness::{kernel_ready, ReadyError};
use pid0_runtime_abi::PID0_EXPORT_MEMBRANE_CAP;

type Membrane = membrane_capnp::membrane::Client;

const STATUS_COMPONENT_PATH: &str = "bin/status.wasm";
const STATUS_ROUTE: &str = "/status";
const EPOCH_STALE_PROBE_INTERVAL_NS: u64 = 5_000_000_000;
const INITIAL_INIT_FAILED: &str = "INITIAL_INIT_FAILED";
const EPOCH_RESTART_INIT_FAILED: &str = "EPOCH_RESTART_INIT_FAILED";

struct StderrLogger;

impl log::Log for StderrLogger {
    fn enabled(&self, metadata: &log::Metadata<'_>) -> bool {
        metadata.level() <= log::Level::Info
    }

    fn log(&self, record: &log::Record<'_>) {
        if self.enabled(record.metadata()) {
            let stderr = get_stderr();
            let _ = stderr.blocking_write_and_flush(
                format!("[kernel-next][{}] {}\n", record.level(), record.args()).as_bytes(),
            );
        }
    }

    fn flush(&self) {}
}

static LOGGER: StderrLogger = StderrLogger;

fn init_logging() {
    if log::set_logger(&LOGGER).is_ok() {
        log::set_max_level(log::LevelFilter::Info);
    }
}

/// Temporary compatibility relay for the remote Glia shell/MCP capability API.
///
/// Kernel-next does not use this membrane for composition, readiness, child
/// grants, or epoch handling. It republishes the host-provided membrane without
/// modification because the current host/PID0 bootstrap contract expects a
/// guest membrane. This relay is expected to disappear with the Glia shell/MCP
/// and `/ww/0.1.0` remote membrane API.
struct KernelBootstrap {
    membrane: Rc<RefCell<Option<Membrane>>>,
}

#[allow(refining_impl_trait)]
impl membrane_capnp::membrane::Server for KernelBootstrap {
    fn graft(
        self: capnp::capability::Rc<Self>,
        _params: membrane_capnp::membrane::GraftParams,
        mut results: membrane_capnp::membrane::GraftResults,
    ) -> capnp::capability::Promise<(), capnp::Error> {
        let membrane = match self.membrane.borrow().clone() {
            Some(membrane) => membrane,
            None => {
                return capnp::capability::Promise::err(capnp::Error::failed(
                    "INIT_MEMBRANE_NOT_READY: kernel bootstrap membrane not ready".into(),
                ));
            }
        };

        capnp::capability::Promise::from_future(async move {
            let response = membrane.graft_request().send().promise.await?;
            let source = response.get()?.get_caps()?;
            let mut destination = results.get().init_caps(source.len());
            for index in 0..source.len() {
                let source_entry = source.get(index);
                let mut destination_entry = destination.reborrow().get(index);
                destination_entry.set_name(source_entry.get_name()?);
                destination_entry.init_cap().set_as_capability(
                    source_entry
                        .get_cap()
                        .get_as_capability::<capnp::capability::Client>()?
                        .hook,
                );
            }
            Ok(())
        })
    }
}

fn status_component_path(root: &str) -> Result<String, capnp::Error> {
    if root.is_empty() {
        return Err(capnp::Error::failed("WW_ROOT is empty".into()));
    }
    if root == "/" {
        Ok(format!("/{STATUS_COMPONENT_PATH}"))
    } else {
        Ok(format!(
            "{}/{}",
            root.trim_end_matches('/'),
            STATUS_COMPONENT_PATH
        ))
    }
}

fn write_cap(mut builder: capnp::any_pointer::Builder<'_>, client: capnp::capability::Client) {
    builder.set_as_capability(client.hook);
}

async fn install_status_route(
    host: &system_capnp::host::Client,
    runtime: &system_capnp::runtime::Client,
) -> Result<(), capnp::Error> {
    let root = std::env::var("WW_ROOT")
        .map_err(|error| capnp::Error::failed(format!("WW_ROOT is not set: {error}")))?;
    let path = status_component_path(&root)?;
    let wasm = std::fs::read(&path).map_err(|error| {
        capnp::Error::failed(format!("failed to read status component '{path}': {error}"))
    })?;

    let mut load = runtime.load_request();
    load.get().set_wasm(&wasm);
    let executor = load.send().pipeline.get_executor();

    let network_response = host.network_request().send().promise.await?;
    let listener = network_response.get()?.get_http_listener()?;
    let mut listen = listener.listen_request();
    listen.get().set_executor(executor);
    listen.get().set_prefix(STATUS_ROUTE);
    let mut grants = listen.get().init_caps(1);
    let mut host_grant = grants.reborrow().get(0);
    host_grant.set_name("host");
    write_cap(host_grant.init_cap(), host.clone().client);
    listen.send().promise.await?;

    log::info!("registered {STATUS_ROUTE} with {path}");
    Ok(())
}

async fn initialize_generation(
    membrane: &Membrane,
    exported_membrane: &Rc<RefCell<Option<Membrane>>>,
) -> Result<system_capnp::host::Client, capnp::Error> {
    let graft_response = membrane.graft_request().send().promise.await?;
    let caps = graft_response.get()?.get_caps()?;

    let export_membrane: Membrane = get_graft_cap(&caps, PID0_EXPORT_MEMBRANE_CAP)?;
    let host: system_capnp::host::Client = get_graft_cap(&caps, "host")?;
    let runtime: system_capnp::runtime::Client = get_graft_cap(&caps, "runtime")?;

    // The current host/PID0 compatibility contract requires publication before
    // initialization. Kernel-next does not use this membrane or treat publication
    // as a readiness commit.
    *exported_membrane.borrow_mut() = Some(export_membrane);
    install_status_route(&host, &runtime).await?;
    Ok(host)
}

async fn wait_monotonic(duration_ns: u64) {
    #[cfg(target_arch = "wasm32")]
    {
        let deadline = wasip2::clocks::monotonic_clock::now().saturating_add(duration_ns);
        std::future::poll_fn(|_cx| {
            if wasip2::clocks::monotonic_clock::now() >= deadline {
                std::task::Poll::Ready(())
            } else {
                std::task::Poll::Pending
            }
        })
        .await;
    }

    #[cfg(not(target_arch = "wasm32"))]
    std::thread::sleep(std::time::Duration::from_nanos(duration_ns));
}

async fn wait_for_stale_epoch(host: &system_capnp::host::Client) -> Result<(), capnp::Error> {
    loop {
        wait_monotonic(EPOCH_STALE_PROBE_INTERVAL_NS).await;
        match host.id_request().send().promise.await {
            Ok(_) => {}
            Err(error)
                if membrane::call_failure_code(&error)
                    == Some(membrane::CallFailureCode::StaleEpoch) =>
            {
                return Ok(());
            }
            Err(error) => {
                log::warn!("epoch probe failed without stale-epoch marker; retrying: {error}");
            }
        }
    }
}

fn wait_for_tty_exit() {
    let stdin = wasip2::cli::stdin::get_stdin();
    loop {
        match stdin.blocking_read(4096) {
            Ok(bytes) if bytes.is_empty() => return,
            Ok(_) => {}
            Err(_) => return,
        }
    }
}

async fn run_generations(
    membrane: Membrane,
    exported_membrane: Rc<RefCell<Option<Membrane>>>,
) -> Result<(), capnp::Error> {
    let mut generation = 0_u64;
    loop {
        let host = initialize_generation(&membrane, &exported_membrane)
            .await
            .map_err(|error| {
                let code = if generation == 0 {
                    INITIAL_INIT_FAILED
                } else {
                    EPOCH_RESTART_INIT_FAILED
                };
                capnp::Error::failed(format!("{code}: {error}"))
            })?;

        match kernel_ready() {
            Ok(()) => {
                log::info!("generation {generation} committed readiness");
                if std::env::var("WW_TTY").is_ok() {
                    wait_for_tty_exit();
                    return Ok(());
                }
                wait_for_stale_epoch(&host).await?;
                log::warn!(
                    "pid0 host authority became stale; re-grafting and rebuilding composition"
                );
            }
            Err(ReadyError::StaleGeneration) => {
                log::warn!("pid0 generation became stale before activation; re-grafting");
            }
        }

        generation += 1;
    }
}

fn run_impl() -> Result<(), ()> {
    init_logging();

    let initialization_failed = Rc::new(Cell::new(false));
    let exported_membrane = Rc::new(RefCell::new(None));
    let bootstrap: Membrane = capnp_rpc::new_client(KernelBootstrap {
        membrane: Rc::clone(&exported_membrane),
    });
    let callback_failed = Rc::clone(&initialization_failed);

    system::serve(bootstrap.client, move |membrane: Membrane| {
        let exported_membrane = Rc::clone(&exported_membrane);
        let callback_failed = Rc::clone(&callback_failed);
        async move {
            match run_generations(membrane, exported_membrane).await {
                Ok(()) => Ok(()),
                Err(error) => {
                    callback_failed.set(true);
                    Err(error)
                }
            }
        }
    });

    if initialization_failed.get() {
        Err(())
    } else {
        Ok(())
    }
}

struct KernelNext;

impl Guest for KernelNext {
    fn run() -> Result<(), ()> {
        run_impl()
    }
}

wasip2::cli::command::export!(KernelNext);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_path_is_root_relative() {
        assert_eq!(
            status_component_path("/ipfs/example").expect("resolve status path"),
            "/ipfs/example/bin/status.wasm"
        );
    }
}
