//! Rust PID0 for the shipped Wetware production composition.
//!
//! One PID0 instance grafts the capabilities it needs, installs the status
//! component at `/status`, and commits readiness. The host owns deployment
//! replacement and PID0 generation lifetime.

use std::cell::Cell;
use std::rc::Rc;

use system::Guest;
use wit_bindgen::StreamResult;

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
mod routing_capnp {
    include!(concat!(env!("OUT_DIR"), "/routing_capnp.rs"));
}

#[allow(dead_code, clippy::extra_unused_type_parameters)]
mod http_capnp {
    include!(concat!(env!("OUT_DIR"), "/http_capnp.rs"));
}

#[allow(dead_code, clippy::extra_unused_type_parameters)]
mod stem_capnp {
    include!(concat!(env!("OUT_DIR"), "/stem_capnp.rs"));
}

mod kernel_runtime {
    wit_bindgen::generate!({
        path: "wit",
        world: "pid0",
        generate_all,
    });
}

use kernel_runtime::wetware::kernel_runtime::readiness::{kernel_ready, ReadyError};

type Membrane = system_capnp::membrane::Client;

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
            eprintln!("[kernel][{}] {}", record.level(), record.args());
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

struct StatusMembrane {
    peer_id: Vec<u8>,
    stat: system_capnp::stat::Client,
}

#[allow(refining_impl_trait)]
impl system_capnp::membrane::Server for StatusMembrane {
    fn graft(
        self: capnp::capability::Rc<Self>,
        _params: system_capnp::membrane::GraftParams,
        mut results: system_capnp::membrane::GraftResults,
    ) -> capnp::capability::Promise<(), capnp::Error> {
        let mut graft = results.get();
        graft.set_peer_id(&self.peer_id);
        graft.set_stat(self.stat.clone());
        capnp::capability::Promise::ok(())
    }
}

async fn install_status_route(
    peer_id: Vec<u8>,
    stat: system_capnp::stat::Client,
    listener: system_capnp::http_listener::Client,
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

    let membrane: Membrane = capnp_rpc::new_client(StatusMembrane { peer_id, stat });
    let mut listen = listener.listen_request();
    listen.get().set_executor(executor);
    listen.get().set_prefix(STATUS_ROUTE);
    listen.get().set_membrane(membrane);
    listen.send().promise.await?;

    log::info!("registered {STATUS_ROUTE} with {path}");
    Ok(())
}

async fn initialize(membrane: &Membrane) -> Result<(), capnp::Error> {
    let graft_response = membrane.graft_request().send().promise.await?;
    let graft = graft_response.get()?;
    if !graft.has_peer_id() {
        return Err(capnp::Error::failed(
            "Membrane.graft result is missing required peerId".into(),
        ));
    }
    let peer_id = graft.get_peer_id()?.to_vec();
    if peer_id.is_empty() {
        return Err(capnp::Error::failed(
            "Membrane.graft result contains an empty peerId".into(),
        ));
    }
    let http = graft.get_network()?.get_http();
    if http.has_listener() {
        let stat = graft.get_stat()?;
        let listener = http.get_listener()?;
        let runtime = graft.get_runtime()?;
        install_status_route(peer_id, stat, listener, &runtime).await?;
    }
    Ok(())
}

async fn wait_for_tty_exit() {
    let (mut stdin, _completion) = kernel_runtime::wasi::cli::stdin::read_via_stream();
    loop {
        let (status, bytes) = stdin.read(Vec::with_capacity(4096)).await;
        match status {
            StreamResult::Complete(_) if !bytes.is_empty() => {
                log::info!("terminal input received");
            }
            StreamResult::Complete(_) => continue,
            StreamResult::Dropped | StreamResult::Cancelled => return,
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
        wait_for_tty_exit().await;
        Ok(())
    } else {
        std::future::pending().await
    }
}

async fn run_impl() -> Result<(), ()> {
    init_logging();

    let initialization_failed = Rc::new(Cell::new(false));
    let callback_failed = Rc::clone(&initialization_failed);

    let run_result = system::run(move |membrane: Membrane| {
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
    })
    .await;

    if let Err(error) = &run_result {
        log::error!("kernel RPC failed: {error}");
    }

    if initialization_failed.get() || run_result.is_err() {
        Err(())
    } else {
        Ok(())
    }
}

struct Kernel;

impl Guest for Kernel {
    async fn run() -> Result<(), ()> {
        run_impl().await
    }
}

system::export!(Kernel);

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
