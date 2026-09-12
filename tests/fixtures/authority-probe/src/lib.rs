//! Focused real-WASM probes for the constructive child-authority harness.
//!
//! Each invocation emits exactly one JSON line on stdout. Probe modes are
//! deliberately independent so a failure says which reacquisition path worked.

use std::cell::Cell;
use std::io::IsTerminal;
use std::rc::Rc;

use capnp::capability::{Client as AnyClient, FromClientHook, Promise};
use capnp_rpc::pry;
use serde_json::{json, Value};
use system::Guest;

#[cfg(target_arch = "wasm32")]
mod wasi {
    wit_bindgen::generate!({
        path: "../../../std/system/wit",
        world: "monotonic-random",
        generate_all,
    });
}

#[allow(dead_code, clippy::extra_unused_type_parameters)]
mod system_capnp {
    include!(concat!(env!("OUT_DIR"), "/system_capnp.rs"));
}

#[allow(dead_code, clippy::extra_unused_type_parameters)]
mod routing_capnp {
    include!(concat!(env!("OUT_DIR"), "/routing_capnp.rs"));
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
mod http_capnp {
    include!(concat!(env!("OUT_DIR"), "/http_capnp.rs"));
}

type Membrane = system_capnp::membrane::Client;

fn random_u64() -> u64 {
    #[cfg(target_arch = "wasm32")]
    {
        wasi::wasi::random::random::get_random_u64()
    }
    #[cfg(not(target_arch = "wasm32"))]
    {
        rand::random::<u64>()
    }
}

fn monotonic_now() -> u64 {
    #[cfg(target_arch = "wasm32")]
    {
        wasi::wasi::clocks::monotonic_clock::now()
    }
    #[cfg(not(target_arch = "wasm32"))]
    {
        use std::sync::OnceLock;
        use std::time::Instant;

        static ORIGIN: OnceLock<Instant> = OnceLock::new();
        ORIGIN.get_or_init(Instant::now).elapsed().as_nanos() as u64
    }
}
const PROVIDER_KEY: &str = "bafkreibm6jg3ux5quy7flfgn5gmxk5ubm6yur3apcu3to3d6tmjzptm2ye";

#[derive(Clone)]
struct NamedCap {
    name: String,
    cap: AnyClient,
}

struct ExtrasMembrane {
    extras: Vec<NamedCap>,
}

#[allow(refining_impl_trait)]
impl system_capnp::membrane::Server for ExtrasMembrane {
    fn graft(
        self: capnp::capability::Rc<Self>,
        _params: system_capnp::membrane::GraftParams,
        mut results: system_capnp::membrane::GraftResults,
    ) -> Promise<(), capnp::Error> {
        let mut graft = results.get();
        graft.set_peer_id(b"authority-probe-child");
        let mut extras = graft.init_extras(self.extras.len() as u32);
        for (index, extra) in self.extras.iter().enumerate() {
            let mut entry = extras.reborrow().get(index as u32);
            entry.set_name(&extra.name);
            entry.init_cap().set_as_capability(extra.cap.clone().hook);
        }
        Promise::ok(())
    }
}

fn extras_membrane(extras: Vec<NamedCap>) -> Membrane {
    capnp_rpc::new_client(ExtrasMembrane { extras })
}

fn text_error(error: impl std::fmt::Display) -> String {
    error.to_string()
}

async fn read_extras(membrane: &Membrane) -> Result<Vec<NamedCap>, capnp::Error> {
    let response = membrane.graft_request().send().promise.await?;
    let caps = response.get()?.get_extras()?;
    let mut result = Vec::with_capacity(caps.len() as usize);
    let mut names = std::collections::HashSet::with_capacity(caps.len() as usize);
    for entry in caps.iter() {
        let name = entry
            .get_name()?
            .to_str()
            .map_err(|error| capnp::Error::failed(error.to_string()))?
            .to_owned();
        if name.is_empty() {
            return Err(capnp::Error::failed(
                "capability name must not be empty".into(),
            ));
        }
        if !names.insert(name.clone()) {
            return Err(capnp::Error::failed(format!(
                "duplicate capability name '{name}'"
            )));
        }
        result.push(NamedCap {
            name,
            cap: entry
                .get_cap()
                .get_as_capability::<capnp::capability::Client>()?,
        });
    }
    Ok(result)
}

fn names(caps: &[NamedCap]) -> Vec<String> {
    caps.iter().map(|entry| entry.name.clone()).collect()
}

fn find_cap<T: FromClientHook>(caps: &[NamedCap], name: &str) -> Result<T, capnp::Error> {
    let entry = caps
        .iter()
        .find(|entry| entry.name == name)
        .ok_or_else(|| capnp::Error::failed(format!("capability '{name}' not found")))?;
    Ok(T::new(entry.cap.clone().hook))
}

fn emit(value: Value) {
    println!("{value}");
}

fn value_or_error<T: serde::Serialize, E: std::fmt::Display>(result: Result<T, E>) -> Value {
    match result {
        Ok(value) => serde_json::to_value(value)
            .unwrap_or_else(|error| json!({"serialization_error": error.to_string()})),
        Err(error) => json!({"error": error.to_string()}),
    }
}

fn optional_result<T: serde::Serialize, E: std::fmt::Display>(
    result: Option<Result<T, E>>,
) -> Value {
    result.map(value_or_error).unwrap_or(Value::Null)
}

fn authority_presence(
    graft: system_capnp::membrane::graft_results::Reader<'_>,
) -> Result<Value, capnp::Error> {
    let network = graft.get_network()?;
    let stream = network.get_stream();
    let vat = network.get_vat();
    let http = network.get_http();
    let routing = graft.get_routing()?;
    Ok(json!({
        "stat": graft.has_stat(),
        "network": {
            "stream": {
                "listener": stream.has_listener(),
                "dialer": stream.has_dialer(),
            },
            "vat": {
                "listener": vat.has_listener(),
                "dialer": vat.has_dialer(),
            },
            "http": {
                "listener": http.has_listener(),
                "dialer": http.has_dialer(),
            },
        },
        "routing": {
            "finder": routing.has_finder(),
            "announcer": routing.has_announcer(),
        },
        "runtime": graft.has_runtime(),
        "authority": graft.has_authority(),
        "identity": graft.has_identity(),
        "ipfs": graft.has_ipfs(),
    }))
}

async fn run_inspect_authority() -> Result<(), capnp::Error> {
    system::run(|membrane: Membrane| async move {
        let result: Result<Value, capnp::Error> = async {
            let response = membrane.graft_request().send().promise.await?;
            authority_presence(response.get()?)
        }
        .await;
        emit(match result {
            Ok(detail) => {
                json!({"mode": "inspect-authority", "ok": true, "detail": detail})
            }
            Err(error) => json!({
                "mode": "inspect-authority",
                "ok": false,
                "error": text_error(error),
            }),
        });
        Ok(())
    })
    .await
}

async fn run_enumerate() -> Result<(), capnp::Error> {
    system::run(|membrane: Membrane| async move {
        let first = read_extras(&membrane).await;
        let second = read_extras(&membrane).await;
        emit(json!({
            "mode": "enumerate",
            "first": value_or_error(first.as_ref().map(|caps| names(caps))),
            "second": value_or_error(second.as_ref().map(|caps| names(caps))),
        }));
        Ok(())
    })
    .await
}

struct ReentrantListener;

#[allow(refining_impl_trait)]
impl system_capnp::vat_listener::Server for ReentrantListener {
    fn serve_raw(
        self: capnp::capability::Rc<Self>,
        params: system_capnp::vat_listener::ServeRawParams,
        _results: system_capnp::vat_listener::ServeRawResults,
    ) -> Promise<(), capnp::Error> {
        let callback = pry!(pry!(params.get())
            .get_cap()
            .get_as_capability::<capnp::capability::Client>());
        let callback = system_capnp::runtime::Client::new(callback.hook);

        Promise::from_future(async move {
            let mut load = callback.load_request();
            load.get().set_wasm(&[]);
            load.send().promise.await?;
            Ok(())
        })
    }

    fn serve_authenticated(
        self: capnp::capability::Rc<Self>,
        _params: system_capnp::vat_listener::ServeAuthenticatedParams,
        _results: system_capnp::vat_listener::ServeAuthenticatedResults,
    ) -> Promise<(), capnp::Error> {
        Promise::err(capnp::Error::unimplemented(
            "authority probe only implements serveRaw".into(),
        ))
    }
}

async fn run_reentrant_callback() -> Result<(), capnp::Error> {
    let listener: system_capnp::vat_listener::Client = capnp_rpc::new_client(ReentrantListener);
    system::serve(listener.client, |_membrane: Membrane| async move {
        std::future::pending().await
    })
    .await
}

async fn invoke_named(membrane: Membrane, requested: String) -> Value {
    let result: Result<Value, capnp::Error> = async {
        let response = membrane.graft_request().send().promise.await?;
        let graft = response.get()?;
        let caps = graft.get_extras()?;
        let mut extras = Vec::with_capacity(caps.len() as usize);
        for entry in caps.iter() {
            extras.push(NamedCap {
                name: entry
                    .get_name()?
                    .to_str()
                    .map_err(|error| capnp::Error::failed(error.to_string()))?
                    .to_owned(),
                cap: entry
                    .get_cap()
                    .get_as_capability::<capnp::capability::Client>()?,
            });
        }
        match requested.as_str() {
            "peer-id" => {
                if !graft.has_peer_id() {
                    return Err(capnp::Error::failed("peerId is withheld".into()));
                }
                Ok(json!({"peer_id": graft.get_peer_id()?.to_vec()}))
            }
            "stat" => {
                if !graft.has_stat() {
                    return Err(capnp::Error::failed("stat is withheld".into()));
                }
                let stat = graft.get_stat()?.snapshot_request().send().promise.await?;
                let stat = stat.get()?.get_stat()?;
                Ok(json!({
                    "listen_addrs": stat.get_listen_addrs()?.len(),
                    "connected_peer_count": stat.get_connected_peer_count(),
                }))
            }
            "runtime" => {
                if !graft.has_runtime() {
                    return Err(capnp::Error::failed("runtime is withheld".into()));
                }
                let runtime = graft.get_runtime()?;
                let mut load = runtime.load_request();
                load.get().set_wasm(&[]);
                let executor = load.send().promise.await?.get()?.get_executor()?;
                let cid = executor
                    .cid_request()
                    .send()
                    .promise
                    .await?
                    .get()?
                    .get_cid()?
                    .to_str()
                    .map_err(|error| capnp::Error::failed(error.to_string()))?
                    .to_owned();
                Ok(json!({"executor_obtained": true, "executor_cid": cid}))
            }
            "routing-finder" => {
                let routing = graft.get_routing()?;
                if !routing.has_finder() {
                    return Err(capnp::Error::failed("routing.finder is withheld".into()));
                }
                let finder = routing.get_finder()?;
                let (providers, done) = call_finder(&finder, 0).await?;
                Ok(json!({"providers": providers, "done": done}))
            }
            "routing-announcer" => {
                let routing = graft.get_routing()?;
                if !routing.has_announcer() {
                    return Err(capnp::Error::failed("routing.announcer is withheld".into()));
                }
                let announcer = routing.get_announcer()?;
                call_announcer(&announcer).await?;
                Ok(json!({"provide": true}))
            }
            "identity" => {
                if !graft.has_identity() {
                    return Err(capnp::Error::failed("identity is withheld".into()));
                }
                let identity = graft.get_identity()?;
                let mut signer = identity.signer_request();
                signer.get().set_domain("authority-probe");
                let signer = signer.send().promise.await?.get()?.get_signer()?;
                let mut sign = signer.sign_request();
                sign.get().set_nonce(7);
                sign.get().set_epoch_seq(1);
                let signature_len = sign.send().promise.await?.get()?.get_sig()?.len();
                Ok(json!({"signature_len": signature_len}))
            }
            "authority" => {
                if !graft.has_authority() {
                    return Err(capnp::Error::failed("authority is withheld".into()));
                }
                let authority = graft.get_authority()?;
                let session: auth_capnp::opaque_session::Client = find_cap(&extras, "session")?;
                let mut guard = authority.guard_request();
                guard.get().set_session(session);
                {
                    let mut policy = guard.get().get_policy()?;
                    let mut profiles = policy.reborrow().init_profiles(1);
                    let mut profile = profiles.reborrow().get(0);
                    profile.set_name("authority-probe");
                    let mut methods = profile.init_methods(1);
                    let mut method = methods.reborrow().get(0);
                    method.set_interface_id(0xdb52_c251_06bc_2c5e);
                    method.set_ordinal(0);
                    let mut recipients = policy.init_recipients(1);
                    let mut recipient = recipients.reborrow().get(0);
                    recipient.set_verifying_key(&[0xa5; 32]);
                    recipient.set_profile("authority-probe");
                }
                let terminal = guard.send().promise.await?.get()?.get_terminal()?;
                let _ = terminal;
                Ok(json!({
                    "guard_callable": true,
                    "terminal_obtained": true,
                }))
            }
            "ipfs" => {
                if !graft.has_ipfs() {
                    return Err(capnp::Error::failed("ipfs is withheld".into()));
                }
                let ipfs = graft.get_ipfs()?;
                let mut request = ipfs.read_request();
                request
                    .get()
                    .set_path("/ipfs/bafkreibm6jg3ux5quy7flfgn5gmxk5ubm6yur3apcu3to3d6tmjzptm2ye");
                let response = request.send().promise.await?;
                let stream = response.get()?.get_stream()?;
                let mut read = stream.read_request();
                read.get().set_max_bytes(1);
                let bytes = read.send().promise.await?.get()?.get_data()?.to_vec();
                Ok(json!({
                    "rpc_reached": true,
                    "stream_obtained": true,
                    "read_bytes": bytes.len(),
                }))
            }
            "stream-listener" => {
                let network = graft.get_network()?;
                if !network.get_stream().has_listener() {
                    return Err(capnp::Error::failed(
                        "network.stream.listener is withheld".into(),
                    ));
                }
                let listener = network.get_stream().get_listener()?;
                let executor: system_capnp::executor::Client = find_cap(&extras, "bound-executor")?;
                let mut request = listener.listen_request();
                request.get().set_executor(executor);
                request.get().set_protocol("authority-probe");
                request.get().set_membrane(membrane.clone());
                request.send().promise.await?;
                Ok(json!({"rpc_reached": true}))
            }
            "stream-dialer" => {
                let network = graft.get_network()?;
                if !network.get_stream().has_dialer() {
                    return Err(capnp::Error::failed(
                        "network.stream.dialer is withheld".into(),
                    ));
                }
                let dialer = network.get_stream().get_dialer()?;
                let mut request = dialer.dial_request();
                request.get().set_peer(b"authority-probe-peer");
                request.get().set_protocol("authority-probe");
                let stream = request.send().promise.await?.get()?.get_stream()?;
                let mut read = stream.read_request();
                read.get().set_max_bytes(1);
                let bytes = read.send().promise.await?.get()?.get_data()?.to_vec();
                Ok(json!({"rpc_reached": true, "read_bytes": bytes.len()}))
            }
            "vat-listener" => {
                let network = graft.get_network()?;
                if !network.get_vat().has_listener() {
                    return Err(capnp::Error::failed(
                        "network.vat.listener is withheld".into(),
                    ));
                }
                let listener = network.get_vat().get_listener()?;
                let executor: system_capnp::executor::Client = find_cap(&extras, "bound-executor")?;
                let mut request = listener.serve_raw_request();
                request
                    .get()
                    .init_cap()
                    .set_as_capability(executor.client.hook);
                request.get().set_protocol("authority-probe");
                request.send().promise.await?;
                Ok(json!({"rpc_reached": true}))
            }
            "vat-dialer" => {
                let network = graft.get_network()?;
                if !network.get_vat().has_dialer() {
                    return Err(capnp::Error::failed(
                        "network.vat.dialer is withheld".into(),
                    ));
                }
                let dialer = network.get_vat().get_dialer()?;
                let mut request = dialer.dial_request();
                request.get().set_peer(b"authority-probe-peer");
                request.get().set_protocol("authority-probe");
                let cap = request
                    .send()
                    .promise
                    .await?
                    .get()?
                    .get_cap()
                    .get_as_capability::<capnp::capability::Client>()?;
                let executor = system_capnp::executor::Client::new(cap.hook);
                let cid = executor
                    .cid_request()
                    .send()
                    .promise
                    .await?
                    .get()?
                    .get_cid()?
                    .to_str()
                    .map_err(|error| capnp::Error::failed(error.to_string()))?
                    .to_owned();
                Ok(json!({"rpc_reached": true, "executor_cid": cid}))
            }
            "http-listener" => {
                let network = graft.get_network()?;
                if !network.get_http().has_listener() {
                    return Err(capnp::Error::failed(
                        "network.http.listener is withheld".into(),
                    ));
                }
                let listener = network.get_http().get_listener()?;
                let executor: system_capnp::executor::Client = find_cap(&extras, "bound-executor")?;
                let mut request = listener.listen_request();
                request.get().set_executor(executor);
                request.get().set_prefix("/authority-probe");
                request.get().set_membrane(membrane.clone());
                request.send().promise.await?;
                Ok(json!({"rpc_reached": true}))
            }
            "http-dialer" => {
                let network = graft.get_network()?;
                let http_group = network.get_http();
                if !http_group.has_dialer() {
                    return Err(capnp::Error::failed(
                        "network.http.dialer is withheld".into(),
                    ));
                }
                let http = http_group.get_dialer()?;
                let mut request = http.get_request();
                let url = std::env::var("WW_PROBE_HTTP_URL")
                    .map_err(|error| capnp::Error::failed(error.to_string()))?;
                request
                    .get()
                    .set_url(format!("{url}/authority-probe").as_str());
                request.get().init_headers(0);
                let response = request.send().promise.await?;
                Ok(json!({"rpc_reached": true, "status": response.get()?.get_status()}))
            }
            other => {
                let runtime: system_capnp::runtime::Client = find_cap(&extras, other)?;
                let mut load = runtime.load_request();
                load.get().set_wasm(&[]);
                let executor = load.send().promise.await?.get()?.get_executor()?;
                let cid = executor
                    .cid_request()
                    .send()
                    .promise
                    .await?
                    .get()?
                    .get_cid()?
                    .to_str()
                    .map_err(|error| capnp::Error::failed(error.to_string()))?
                    .to_owned();
                Ok(json!({"cid": cid}))
            }
        }
    }
    .await;

    match result {
        Ok(detail) => json!({"mode": "invoke", "name": requested, "ok": true, "detail": detail}),
        Err(error) => {
            json!({"mode": "invoke", "name": requested, "ok": false, "error": text_error(error)})
        }
    }
}

async fn run_invoke() -> Result<(), capnp::Error> {
    let requested = std::env::var("WW_PROBE_CAP").unwrap_or_else(|_| "peer-id".to_owned());
    system::run(|membrane: Membrane| async move {
        emit(invoke_named(membrane, requested).await);
        Ok(())
    })
    .await
}

async fn run_arbitrary_name() -> Result<(), capnp::Error> {
    let requested =
        std::env::var("WW_PROBE_NAME").unwrap_or_else(|_| "definitely-not-granted".to_owned());
    system::run(|membrane: Membrane| async move {
        let value = match read_extras(&membrane).await {
            Ok(caps) => {
                let matching: Vec<_> = caps
                    .iter()
                    .filter(|entry| entry.name == requested)
                    .map(|entry| entry.name.clone())
                    .collect();
                json!({
                    "mode": "arbitrary-name",
                    "name": requested,
                    "resolved": !matching.is_empty(),
                    "matches": matching,
                })
            }
            Err(error) => json!({
                "mode": "arbitrary-name",
                "name": requested,
                "resolved": false,
                "error": text_error(error),
            }),
        };
        emit(value);
        Ok(())
    })
    .await
}

async fn run_alias_redelivery() -> Result<(), capnp::Error> {
    system::run(|membrane: Membrane| async move {
        let result: Result<Value, capnp::Error> = async {
            let deliveries = [
                read_extras(&membrane).await?,
                read_extras(&membrane).await?,
            ];
            let mut observed = Vec::new();
            for (delivery, caps) in deliveries.iter().enumerate() {
                for name in ["alias-a", "alias-b"] {
                    let runtime: system_capnp::runtime::Client = find_cap(caps, name)?;
                    let mut load = runtime.load_request();
                    load.get().set_wasm(&[]);
                    let executor = load.send().promise.await?.get()?.get_executor()?;
                    let response = executor.cid_request().send().promise.await?;
                    observed.push(json!({
                        "delivery": delivery + 1,
                        "name": name,
                        "cid": response.get()?.get_cid()?.to_str().map_err(|error| capnp::Error::failed(error.to_string()))?,
                    }));
                }
            }
            Ok(json!({"observed": observed}))
        }
        .await;
        emit(match result {
            Ok(detail) => json!({"mode": "alias-redelivery", "ok": true, "detail": detail}),
            Err(error) => {
                json!({"mode": "alias-redelivery", "ok": false, "error": text_error(error)})
            }
        });
        Ok(())
    })
    .await
}

async fn run_attenuated() -> Result<(), capnp::Error> {
    system::run(|membrane: Membrane| async move {
        let result: Result<Value, capnp::Error> = async {
            let response = membrane.graft_request().send().promise.await?;
            let graft = response.get()?;
            if !graft.has_runtime() {
                return Err(capnp::Error::failed("runtime is withheld".into()));
            }
            let runtime = graft.get_runtime()?;
            let mut load = runtime.load_request();
            load.get().set_wasm(&[]);
            let executor = load.send().promise.await?.get()?.get_executor()?;
            let cid_denied = match executor.cid_request().send().promise.await {
                Ok(_) => {
                    return Err(capnp::Error::failed(
                        "recursively attenuated Executor unexpectedly allowed cid".into(),
                    ))
                }
                Err(error) => error,
            };
            let shutdown_denied = match runtime.shutdown_request().send().promise.await {
                Ok(_) => {
                    return Err(capnp::Error::failed(
                        "attenuated runtime unexpectedly allowed shutdown".into(),
                    ))
                }
                Err(error) => error,
            };
            Ok(json!({
                "extras": names(&read_extras(&membrane).await?),
                "executor_returned": true,
                "cid_denied": cid_denied.to_string(),
                "shutdown_denied": shutdown_denied.to_string(),
            }))
        }
        .await;
        emit(match result {
            Ok(detail) => json!({"mode": "attenuated", "ok": true, "detail": detail}),
            Err(error) => json!({"mode": "attenuated", "ok": false, "error": text_error(error)}),
        });
        Ok(())
    })
    .await
}

async fn run_trusted_lattice() -> Result<(), capnp::Error> {
    let image = std::env::var("WW_PROBE_IMAGE")
        .unwrap_or_else(|_| "runtime-selected-image".to_owned())
        .into_bytes();
    system::run(|membrane: Membrane| async move {
        let result: Result<Value, capnp::Error> = async {
            let response = membrane.graft_request().send().promise.await?;
            let graft = response.get()?;
            if !graft.has_runtime() {
                return Err(capnp::Error::failed("runtime is withheld".into()));
            }
            let caps = read_extras(&membrane).await?;
            let runtime = graft.get_runtime()?;
            let bound: system_capnp::executor::Client = find_cap(&caps, "bound-executor")?;

            let mut load = runtime.load_request();
            load.get().set_wasm(&image);
            let selected = load.send().promise.await?.get()?.get_executor()?;
            let selected_cid = selected
                .cid_request()
                .send()
                .promise
                .await?
                .get()?
                .get_cid()?
                .to_str()
                .map_err(|error| capnp::Error::failed(error.to_string()))?
                .to_owned();
            let bound_cid = bound
                .cid_request()
                .send()
                .promise
                .await?
                .get()?
                .get_cid()?
                .to_str()
                .map_err(|error| capnp::Error::failed(error.to_string()))?
                .to_owned();
            Ok(json!({
                "extras": names(&caps),
                "runtime": true,
                "selected_cid": selected_cid,
                "bound_cid": bound_cid,
                "different_images": selected_cid != bound_cid,
            }))
        }
        .await;
        emit(match result {
            Ok(detail) => json!({"mode": "trusted-lattice", "ok": true, "detail": detail}),
            Err(error) => {
                json!({"mode": "trusted-lattice", "ok": false, "error": text_error(error)})
            }
        });
        Ok(())
    })
    .await
}

async fn run_epoch_http_listen() -> Result<(), capnp::Error> {
    system::run(|membrane: Membrane| async move {
        let result: Result<(), capnp::Error> = async {
            let response = membrane.graft_request().send().promise.await?;
            let graft = response.get()?;
            let caps = graft.get_extras()?;
            let mut extras = Vec::with_capacity(caps.len() as usize);
            for entry in caps.iter() {
                extras.push(NamedCap {
                    name: entry
                        .get_name()?
                        .to_str()
                        .map_err(|error| capnp::Error::failed(error.to_string()))?
                        .to_owned(),
                    cap: entry
                        .get_cap()
                        .get_as_capability::<capnp::capability::Client>()?,
                });
            }
            let executor: system_capnp::executor::Client = find_cap(&extras, "bound-executor")?;
            let network = graft.get_network()?;
            let http = network.get_http();
            if !http.has_listener() {
                return Err(capnp::Error::failed(
                    "network.http.listener is withheld".into(),
                ));
            }
            let listener = http.get_listener()?;
            let mut listen = listener.listen_request();
            listen.get().set_executor(executor);
            listen.get().set_prefix("/epoch-probe");
            listen.get().set_membrane(membrane.clone());
            listen.send().promise.await?;
            Ok(())
        }
        .await;

        emit(match result {
            Ok(()) => json!({"mode": "epoch-http-listen", "ok": true}),
            Err(error) => json!({
                "mode": "epoch-http-listen",
                "ok": false,
                "error": text_error(error),
            }),
        });
        Ok(())
    })
    .await
}

async fn run_late_delegation() -> Result<(), capnp::Error> {
    system::run(|membrane: Membrane| async move {
        let result: Result<Value, capnp::Error> = async {
            let initial = read_extras(&membrane).await?;
            let initial_names = names(&initial);
            let vat_client: system_capnp::vat_client::Client = find_cap(&initial, "mailbox")?;
            let mut receive = vat_client.dial_request();
            receive.get().set_peer(&[]);
            receive.get().set_protocol("late-delegation");
            let delegated = receive
                .send()
                .promise
                .await?
                .get()?
                .get_cap()
                .get_as_capability::<AnyClient>()?;
            let delegated = system_capnp::runtime::Client::new(delegated.hook);
            let mut load = delegated.load_request();
            load.get().set_wasm(&[]);
            let executor = load.send().promise.await?.get()?.get_executor()?;
            let cid = executor
                .cid_request()
                .send()
                .promise
                .await?
                .get()?
                .get_cid()?
                .to_str()
                .map_err(|error| capnp::Error::failed(error.to_string()))?
                .to_owned();
            let after_names = names(&read_extras(&membrane).await?);
            Ok(json!({
                "initial_names": initial_names,
                "received_later": ["delegated-x"],
                "current_holdings": ["mailbox", "delegated-x"],
                "after_names": after_names,
                "delegated_cid": cid,
            }))
        }
        .await;
        emit(match result {
            Ok(detail) => json!({"mode": "late-delegation", "ok": true, "detail": detail}),
            Err(error) => {
                json!({"mode": "late-delegation", "ok": false, "error": text_error(error)})
            }
        });
        Ok(())
    })
    .await
}

async fn run_invoke_all() -> Result<(), capnp::Error> {
    system::run(|membrane: Membrane| async move {
        let mut results = Vec::new();
        let mut usable = Vec::new();
        for name in [
            "peer-id",
            "stat",
            "stream-listener",
            "stream-dialer",
            "vat-listener",
            "vat-dialer",
            "http-listener",
            "http-dialer",
            "runtime",
            "routing-finder",
            "routing-announcer",
            "authority",
            "identity",
            "ipfs",
        ] {
            let result = invoke_named(membrane.clone(), name.to_owned()).await;
            if result["ok"] == true {
                usable.push(name);
            }
            results.push(result);
        }
        emit(json!({
            "mode": "invoke-all",
            "usable": usable,
            "results": results,
        }));
        Ok(())
    })
    .await
}

struct ProviderSink {
    providers: Rc<Cell<u32>>,
    done: Rc<Cell<bool>>,
}

#[allow(refining_impl_trait)]
impl routing_capnp::provider_sink::Server for ProviderSink {
    fn provider(
        self: Rc<Self>,
        _params: routing_capnp::provider_sink::ProviderParams,
        _results: routing_capnp::provider_sink::ProviderResults,
    ) -> Promise<(), capnp::Error> {
        self.providers.set(self.providers.get() + 1);
        Promise::ok(())
    }

    fn done(
        self: Rc<Self>,
        _params: routing_capnp::provider_sink::DoneParams,
        _results: routing_capnp::provider_sink::DoneResults,
    ) -> Promise<(), capnp::Error> {
        self.done.set(true);
        Promise::ok(())
    }
}

async fn call_finder(
    finder: &routing_capnp::finder::Client,
    count: u32,
) -> Result<(u32, bool), capnp::Error> {
    let providers = Rc::new(Cell::new(0));
    let done = Rc::new(Cell::new(false));
    let sink: routing_capnp::provider_sink::Client = capnp_rpc::new_client(ProviderSink {
        providers: providers.clone(),
        done: done.clone(),
    });
    let mut find = finder.find_providers_request();
    find.get().set_key(PROVIDER_KEY);
    find.get().set_count(count);
    find.get().set_sink(sink);
    find.send().promise.await?;
    Ok((providers.get(), done.get()))
}

async fn call_announcer(announcer: &routing_capnp::announcer::Client) -> Result<(), capnp::Error> {
    let mut provide = announcer.provide_request();
    provide.get().set_key(PROVIDER_KEY);
    provide.send().promise.await.map(|_| ())
}

async fn run_provider_routing(mode: &'static str) -> Result<(), capnp::Error> {
    system::run(|membrane: Membrane| async move {
        let result: Result<Value, capnp::Error> = async {
            let response = membrane.graft_request().send().promise.await?;
            let graft = response.get()?;
            let routing = graft.get_routing()?;
            match mode {
                "routing-finder" => {
                    if !routing.has_finder() {
                        return Err(capnp::Error::failed("routing.finder is withheld".into()));
                    }
                    let finder = routing.get_finder()?;
                    let (providers, done) = call_finder(&finder, 3).await?;
                    Ok(json!({
                        "find_providers": true,
                        "providers": providers,
                        "done": done,
                        "announcer_withheld": !routing.has_announcer(),
                    }))
                }
                "routing-announcer" => {
                    if !routing.has_announcer() {
                        return Err(capnp::Error::failed("routing.announcer is withheld".into()));
                    }
                    let announcer = routing.get_announcer()?;
                    call_announcer(&announcer).await?;
                    Ok(json!({
                        "provide": true,
                        "finder_withheld": !routing.has_finder(),
                    }))
                }
                "routing-both" => {
                    if !routing.has_finder() || !routing.has_announcer() {
                        return Err(capnp::Error::failed(
                            "routing finder or announcer is withheld".into(),
                        ));
                    }
                    let finder = routing.get_finder()?;
                    let announcer = routing.get_announcer()?;
                    call_announcer(&announcer).await?;
                    let (providers, done) = call_finder(&finder, 3).await?;
                    Ok(json!({
                        "provide": true,
                        "find_providers": true,
                        "providers": providers,
                        "done": done,
                        "typed_fields": ["finder", "announcer"],
                    }))
                }
                _ => Err(capnp::Error::failed(format!(
                    "unknown provider-routing probe mode: {mode}"
                ))),
            }
        }
        .await;
        emit(match result {
            Ok(detail) => json!({"mode": mode, "ok": true, "detail": detail}),
            Err(error) => json!({"mode": mode, "ok": false, "error": text_error(error)}),
        });
        Ok(())
    })
    .await
}

async fn read_all(stream: system_capnp::byte_stream::Client) -> Result<Vec<u8>, capnp::Error> {
    let mut output = Vec::new();
    loop {
        let mut request = stream.read_request();
        request.get().set_max_bytes(64 * 1024);
        let response = request.send().promise.await?;
        let bytes = response.get()?.get_data()?;
        if bytes.is_empty() {
            return Ok(output);
        }
        output.extend_from_slice(bytes);
    }
}

async fn run_descendant() -> Result<(), capnp::Error> {
    system::run(|membrane: Membrane| async move {
        let result: Result<Value, capnp::Error> = async {
            let caps = read_extras(&membrane).await?;
            let executor: system_capnp::executor::Client = find_cap(&caps, "restricted-executor")?;
            let narrow = caps.iter().find(|entry| entry.name == "narrow");

            let mut alias_request = executor.spawn_request();
            {
                let mut args = alias_request.get().init_args(2);
                args.set(0, "authority-probe");
                args.set(1, "alias-redelivery");
            }
            alias_request.get().init_env(0);
            let aliases = narrow
                .map(|narrow| {
                    ["alias-a", "alias-b"]
                        .into_iter()
                        .map(|name| NamedCap {
                            name: name.to_owned(),
                            cap: narrow.cap.clone(),
                        })
                        .collect()
                })
                .unwrap_or_default();
            alias_request.get().set_membrane(extras_membrane(aliases));
            let alias_process = alias_request.send().promise.await?.get()?.get_process()?;
            let alias_stdout = alias_process
                .stdout_request()
                .send()
                .promise
                .await?
                .get()?
                .get_stream()?;
            let alias_output = read_all(alias_stdout).await?;
            let alias_text = String::from_utf8(alias_output)
                .map_err(|error| capnp::Error::failed(error.to_string()))?;
            let aliases: Value = serde_json::from_str(alias_text.trim())
                .map_err(|error| capnp::Error::failed(error.to_string()))?;

            let mut omitted_request = executor.spawn_request();
            {
                let mut args = omitted_request.get().init_args(2);
                args.set(0, "authority-probe");
                args.set(1, "inspect-authority");
            }
            omitted_request.get().init_env(0);
            omitted_request
                .get()
                .set_membrane(extras_membrane(Vec::new()));
            let omitted_process = omitted_request.send().promise.await?.get()?.get_process()?;
            let omitted_stdout = omitted_process
                .stdout_request()
                .send()
                .promise
                .await?
                .get()?
                .get_stream()?;
            let omitted_output = read_all(omitted_stdout).await?;
            let omitted_text = String::from_utf8(omitted_output)
                .map_err(|error| capnp::Error::failed(error.to_string()))?;
            let omitted: Value = serde_json::from_str(omitted_text.trim())
                .map_err(|error| capnp::Error::failed(error.to_string()))?;
            Ok(json!({
                "parent_names": names(&caps),
                "aliases": aliases,
                "omitted": omitted,
            }))
        }
        .await;
        emit(match result {
            Ok(detail) => json!({"mode": "descendant", "ok": true, "detail": detail}),
            Err(error) => json!({"mode": "descendant", "ok": false, "error": text_error(error)}),
        });
        Ok(())
    })
    .await
}

async fn run_raw_runtime() -> Result<(), capnp::Error> {
    system::run(|runtime: system_capnp::runtime::Client| async move {
        let mut load = runtime.load_request();
        load.get().set_wasm(&[]);
        let result = load.send().promise.await;
        emit(match result {
            Ok(_) => json!({"mode": "raw-runtime", "ok": true}),
            Err(error) => json!({"mode": "raw-runtime", "ok": false, "error": text_error(error)}),
        });
        Ok(())
    })
    .await
}

async fn run_substrate() -> Result<(), capnp::Error> {
    system::run(|_membrane: Membrane| async move {
        let args: Vec<String> = std::env::args().collect();
        let env: Vec<(String, String)> = std::env::vars().collect();
        let root = std::fs::read_dir("/").map(|entries| {
            entries
                .filter_map(Result::ok)
                .map(|entry| entry.file_name().to_string_lossy().into_owned())
                .collect::<Vec<_>>()
        });
        let cid_path = std::env::var("WW_PROBE_KNOWN_CID_PATH").ok();
        let known_cid_read = cid_path
            .as_deref()
            .map(std::fs::read)
            .map(|result| result.map(|bytes| bytes.len()).map_err(text_error));
        let cid_enumeration = std::fs::read_dir("/ipfs").map(|entries| entries.count());
        let ipfs_mutation = cid_path
            .as_deref()
            .map(|path| std::fs::write(path, b"authority-probe").map_err(text_error));
        let scratch_path = format!("/tmp/authority-probe-{}", random_u64());
        let scratch = std::fs::write(&scratch_path, b"scratch")
            .and_then(|_| std::fs::read(&scratch_path))
            .map(|bytes| bytes == b"scratch")
            .map_err(text_error);

        emit(json!({
            "mode": "substrate",
            "args": args,
            "env": env,
            "stdio": {
                "stdin_terminal": std::io::stdin().is_terminal(),
                "stdout_terminal": std::io::stdout().is_terminal(),
                "stderr_terminal": std::io::stderr().is_terminal(),
            },
            "filesystem": {
                "root_entries": value_or_error(root),
                "known_cid_path": cid_path,
                "known_cid_read": optional_result(known_cid_read),
                "cid_enumeration": value_or_error(cid_enumeration),
                "ipfs_mutation": optional_result(ipfs_mutation),
                "scratch": value_or_error(scratch),
            },
            "clock": {
                "wall_unix_nanos": std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_nanos())
                    .unwrap_or_default()
                    .to_string(),
                "monotonic_nanos": monotonic_now(),
            },
            "random_u64": random_u64(),
        }));
        Ok(())
    })
    .await
}

async fn run_scratch_observe() -> Result<(), capnp::Error> {
    system::run(|_membrane: Membrane| async move {
        let path = "/tmp/authority-probe-private";
        let observed_before_write = std::path::Path::new(path).exists();
        let write = std::fs::write(path, b"sibling").map_err(text_error);
        emit(json!({
            "mode": "scratch-observe",
            "observed_before_write": observed_before_write,
            "write": value_or_error(write),
        }));
        Ok(())
    })
    .await
}

async fn run_scratch_parent() -> Result<(), capnp::Error> {
    system::run(|membrane: Membrane| async move {
        let result: Result<Value, capnp::Error> = async {
            let caps = read_extras(&membrane).await?;
            let executor: system_capnp::executor::Client = find_cap(&caps, "restricted-executor")?;
            let path = "/tmp/authority-probe-private";
            std::fs::write(path, b"parent")
                .map_err(|error| capnp::Error::failed(error.to_string()))?;

            let mut spawn = executor.spawn_request();
            {
                let mut args = spawn.get().init_args(2);
                args.set(0, "authority-probe");
                args.set(1, "scratch-observe");
            }
            spawn.get().init_env(0);
            spawn.get().set_membrane(extras_membrane(Vec::new()));
            let child = spawn.send().promise.await?.get()?.get_process()?;
            let stdout = child
                .stdout_request()
                .send()
                .promise
                .await?
                .get()?
                .get_stream()?;
            let output = read_all(stdout).await?;
            let child_report: Value = serde_json::from_slice(&output)
                .map_err(|error| capnp::Error::failed(error.to_string()))?;
            let parent_after =
                std::fs::read(path).map_err(|error| capnp::Error::failed(error.to_string()))?;

            Ok(json!({
                "parent_names": names(&caps),
                "child": child_report,
                "parent_after": parent_after,
            }))
        }
        .await;
        emit(match result {
            Ok(detail) => json!({"mode": "scratch-parent", "ok": true, "detail": detail}),
            Err(error) => {
                json!({"mode": "scratch-parent", "ok": false, "error": text_error(error)})
            }
        });
        Ok(())
    })
    .await
}

async fn run_no_graft() -> Result<(), capnp::Error> {
    system::run(|_membrane: Membrane| async move {
        emit(json!({"mode": "no-graft", "ok": true}));
        Ok(())
    })
    .await
}

async fn run_stateful_graft() -> Result<(), capnp::Error> {
    system::run(|membrane: Membrane| async move {
        let mut peer_ids = Vec::new();
        for _ in 0..2 {
            let response = membrane.graft_request().send().promise.await?;
            let graft = response.get()?;
            if !graft.has_peer_id() {
                return Err(capnp::Error::failed("peerId is missing".into()));
            }
            peer_ids.push(graft.get_peer_id()?.to_vec());
        }
        emit(json!({
            "mode": "stateful-graft",
            "ok": true,
            "peer_ids": peer_ids,
        }));
        Ok(())
    })
    .await
}

struct AuthorityProbe;

impl Guest for AuthorityProbe {
    async fn run() -> Result<(), ()> {
        let result = match std::env::args().nth(1).as_deref() {
            Some("invoke") => run_invoke().await,
            Some("arbitrary-name") => run_arbitrary_name().await,
            Some("alias-redelivery") => run_alias_redelivery().await,
            Some("attenuated") => run_attenuated().await,
            Some("trusted-lattice") => run_trusted_lattice().await,
            Some("epoch-http-listen") => run_epoch_http_listen().await,
            Some("late-delegation") => run_late_delegation().await,
            Some("invoke-all") => run_invoke_all().await,
            Some("inspect-authority") => run_inspect_authority().await,
            Some("routing-finder") => run_provider_routing("routing-finder").await,
            Some("routing-announcer") => run_provider_routing("routing-announcer").await,
            Some("routing-both") => run_provider_routing("routing-both").await,
            Some("descendant") => run_descendant().await,
            Some("raw-runtime") => run_raw_runtime().await,
            Some("substrate") => run_substrate().await,
            Some("scratch-observe") => run_scratch_observe().await,
            Some("scratch-parent") => run_scratch_parent().await,
            Some("reentrant-callback") => run_reentrant_callback().await,
            Some("no-graft") => run_no_graft().await,
            Some("stateful-graft") => run_stateful_graft().await,
            _ => run_enumerate().await,
        };
        result.map_err(|error| {
            eprintln!("authority probe RPC failed: {error}");
        })
    }
}

system::export!(AuthorityProbe);

#[cfg(test)]
mod tests {
    use super::*;
    use capnp::traits::{Imbue, ImbueMut};

    struct LeakedStreamListener;

    #[allow(refining_impl_trait)]
    impl system_capnp::stream_listener::Server for LeakedStreamListener {
        fn listen(
            self: capnp::capability::Rc<Self>,
            _params: system_capnp::stream_listener::ListenParams,
            _results: system_capnp::stream_listener::ListenResults,
        ) -> Promise<(), capnp::Error> {
            Promise::ok(())
        }
    }

    #[test]
    fn withheld_authority_is_reported_from_null_typed_pointers() {
        let mut message = capnp::message::Builder::new_default();
        {
            let mut graft =
                message.init_root::<system_capnp::membrane::graft_results::Builder<'_>>();
            graft.set_peer_id(b"test-peer");
            graft.init_extras(0);
        }
        let graft = message
            .get_root_as_reader::<system_capnp::membrane::graft_results::Reader<'_>>()
            .expect("graft reader");

        assert_eq!(
            authority_presence(graft).expect("inspect typed graft pointers"),
            json!({
                "stat": false,
                "network": {
                    "stream": {"listener": false, "dialer": false},
                    "vat": {"listener": false, "dialer": false},
                    "http": {"listener": false, "dialer": false},
                },
                "routing": {"finder": false, "announcer": false},
                "runtime": false,
                "authority": false,
                "identity": false,
                "ipfs": false,
            })
        );
    }

    #[test]
    fn listener_presence_is_detected_without_bound_executor_extra() {
        let listener: system_capnp::stream_listener::Client =
            capnp_rpc::new_client(LeakedStreamListener);
        let mut message = capnp::message::Builder::new_default();
        let mut cap_table = Vec::new();
        {
            let mut graft =
                message.init_root::<system_capnp::membrane::graft_results::Builder<'_>>();
            graft.imbue_mut(&mut cap_table);
            graft.set_peer_id(b"test-peer");
            graft
                .reborrow()
                .init_network()
                .init_stream()
                .set_listener(listener);
            graft.init_extras(0);
        }
        let mut graft = message
            .get_root_as_reader::<system_capnp::membrane::graft_results::Reader<'_>>()
            .expect("graft reader");
        graft.imbue(&cap_table);

        assert_eq!(
            graft.get_extras().expect("extras").len(),
            0,
            "the leaked listener fixture must omit bound-executor"
        );
        let presence = authority_presence(graft).expect("inspect typed graft pointers");
        assert_eq!(presence["network"]["stream"]["listener"], true);
    }
}
