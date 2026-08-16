//! Rust PID0 for the shipped Wetware production composition.
//!
//! One PID0 instance grafts the capabilities it needs, installs the status
//! component at `/status`, and commits readiness. The host owns deployment
//! replacement and PID0 generation lifetime.

use std::cell::Cell;
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
        path: "wit",
        world: "pid0",
    });
}

use kernel_runtime::wetware::kernel_runtime::readiness::{kernel_ready, ReadyError};

type Membrane = membrane_capnp::membrane::Client;

const STATUS_COMPONENT_PATH: &str = "bin/status.wasm";
const STATUS_ROUTE: &str = "/status";
const INITIAL_INIT_FAILED: &str = "INITIAL_INIT_FAILED";

struct StderrLogger;

impl log::Log for StderrLogger {
    fn enabled(&self, metadata: &log::Metadata<'_>) -> bool {
        metadata.level() <= log::Level::Info
    }

    fn log(&self, record: &log::Record<'_>) {
        if self.enabled(record.metadata()) {
            let stderr = get_stderr();
            let _ = stderr.blocking_write_and_flush(
                format!("[kernel][{}] {}\n", record.level(), record.args()).as_bytes(),
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

async fn initialize(membrane: &Membrane) -> Result<(), capnp::Error> {
    let graft_response = membrane.graft_request().send().promise.await?;
    let caps = graft_response.get()?.get_caps()?;

    let host: system_capnp::host::Client = get_graft_cap(&caps, "host")?;
    let runtime: system_capnp::runtime::Client = get_graft_cap(&caps, "runtime")?;

    install_status_route(&host, &runtime).await?;
    Ok(())
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

async fn run_kernel(membrane: Membrane) -> Result<(), capnp::Error> {
    initialize(&membrane)
        .await
        .map_err(|error| capnp::Error::failed(format!("{INITIAL_INIT_FAILED}: {error}")))?;

    match kernel_ready() {
        Ok(()) => log::info!("committed readiness"),
        Err(ReadyError::StaleGeneration) => {
            return Err(capnp::Error::failed(
                "KERNEL_READY_FAILED: stale generation".into(),
            ));
        }
    }

    if std::env::var("WW_TTY").is_ok() {
        wait_for_tty_exit();
        Ok(())
    } else {
        std::future::pending().await
    }
}

fn run_impl() -> Result<(), ()> {
    init_logging();

    let initialization_failed = Rc::new(Cell::new(false));
    let callback_failed = Rc::clone(&initialization_failed);

    system::run(move |membrane: Membrane| {
        let callback_failed = Rc::clone(&callback_failed);
        async move {
            match run_kernel(membrane).await {
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

struct Kernel;

impl Guest for Kernel {
    fn run() -> Result<(), ()> {
        run_impl()
    }
}

wasip2::cli::command::export!(Kernel);

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
