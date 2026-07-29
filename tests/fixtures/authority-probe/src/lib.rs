//! Focused real-WASM probes for the constructive child-authority harness.
//!
//! Each invocation emits exactly one JSON line on stdout. Probe modes are
//! deliberately independent so a failure says which reacquisition path worked.

use std::cell::Cell;
use std::io::IsTerminal;
use std::rc::Rc;

use capnp::capability::{Client as AnyClient, FromClientHook, Promise};
use serde_json::{json, Value};
use wasip2::exports::cli::run::Guest;

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
mod membrane_capnp {
    include!(concat!(env!("OUT_DIR"), "/membrane_capnp.rs"));
}

#[allow(dead_code, clippy::extra_unused_type_parameters)]
mod http_capnp {
    include!(concat!(env!("OUT_DIR"), "/http_capnp.rs"));
}

type InitialGrants = membrane_capnp::initial_grants::Client;

#[derive(Clone)]
struct NamedCap {
    name: String,
    cap: AnyClient,
}

fn text_error(error: impl std::fmt::Display) -> String {
    error.to_string()
}

async fn read_initial_grants(grants: &InitialGrants) -> Result<Vec<NamedCap>, capnp::Error> {
    let response = grants.get_request().send().promise.await?;
    let caps = response.get()?.get_caps()?;
    let mut result = Vec::with_capacity(caps.len() as usize);
    for entry in caps.iter() {
        result.push(NamedCap {
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

fn run_enumerate() {
    system::run(|initial_grants: InitialGrants| async move {
        let first = read_initial_grants(&initial_grants).await;
        let second = read_initial_grants(&initial_grants).await;
        emit(json!({
            "mode": "enumerate",
            "first": value_or_error(first.as_ref().map(|caps| names(caps))),
            "second": value_or_error(second.as_ref().map(|caps| names(caps))),
        }));
        Ok(())
    });
}

async fn invoke_named(initial_grants: InitialGrants, requested: String) -> Value {
    let caps = match read_initial_grants(&initial_grants).await {
        Ok(caps) => caps,
        Err(error) => {
            return json!({"mode": "invoke", "name": requested, "ok": false, "error": text_error(error)});
        }
    };

    let result: Result<Value, capnp::Error> = async {
        match requested.as_str() {
            "host" => {
                let host: system_capnp::host::Client = find_cap(&caps, &requested)?;
                let id = host.id_request().send().promise.await?;
                let peer_id = id.get()?.get_peer_id()?.to_vec();
                let network = host.network_request().send().promise.await?;
                let network = network.get()?;
                let _ = network.get_stream_listener()?;
                let _ = network.get_stream_dialer()?;
                let _ = network.get_vat_listener()?;
                let _ = network.get_vat_client()?;
                let _ = network.get_http_listener()?;
                Ok(json!({"peer_id": peer_id, "network_caps": true}))
            }
            "ambient-parent" | "alias-a" | "alias-b" | "status-source" | "narrow"
            | "delegated-x" | "tracked" => {
                let host: system_capnp::host::Client = find_cap(&caps, &requested)?;
                let id = host.id_request().send().promise.await?;
                Ok(json!({"peer_id": id.get()?.get_peer_id()?.to_vec()}))
            }
            "runtime" => {
                let runtime: system_capnp::runtime::Client = find_cap(&caps, &requested)?;
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
            "routing" => {
                let routing: routing_capnp::routing::Client = find_cap(&caps, &requested)?;
                let mut hash = routing.hash_request();
                hash.get().set_data(b"authority-probe");
                let response = hash.send().promise.await?;
                let key = response
                    .get()?
                    .get_key()?
                    .to_str()
                    .map_err(|error| capnp::Error::failed(error.to_string()))?
                    .to_owned();
                Ok(json!({"hash": key}))
            }
            "identity" => {
                let identity: auth_capnp::identity::Client = find_cap(&caps, &requested)?;
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
                let authority: auth_capnp::authority::Client = find_cap(&caps, &requested)?;
                let session: auth_capnp::opaque_session::Client = find_cap(&caps, "host")?;
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
                let ipfs: system_capnp::ipfs::Client = find_cap(&caps, &requested)?;
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
            "http-client" => {
                let http: http_capnp::http_client::Client = find_cap(&caps, &requested)?;
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
            other => Err(capnp::Error::failed(format!(
                "no typed invocation registered for '{other}'"
            ))),
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

fn run_invoke() {
    let requested = std::env::var("WW_PROBE_CAP").unwrap_or_else(|_| "host".to_owned());
    system::run(|initial_grants: InitialGrants| async move {
        emit(invoke_named(initial_grants, requested).await);
        Ok(())
    });
}

fn run_arbitrary_name() {
    let requested =
        std::env::var("WW_PROBE_NAME").unwrap_or_else(|_| "definitely-not-granted".to_owned());
    system::run(|initial_grants: InitialGrants| async move {
        let value = match read_initial_grants(&initial_grants).await {
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
    });
}

fn run_alias_redelivery() {
    system::run(|initial_grants: InitialGrants| async move {
        let result: Result<Value, capnp::Error> = async {
            let deliveries = [
                read_initial_grants(&initial_grants).await?,
                read_initial_grants(&initial_grants).await?,
            ];
            let mut observed = Vec::new();
            for (delivery, caps) in deliveries.iter().enumerate() {
                for name in ["alias-a", "alias-b"] {
                    let host: system_capnp::host::Client = find_cap(caps, name)?;
                    let response = host.id_request().send().promise.await?;
                    observed.push(json!({
                        "delivery": delivery + 1,
                        "name": name,
                        "peer_id": response.get()?.get_peer_id()?.to_vec(),
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
    });
}

fn run_attenuated() {
    system::run(|initial_grants: InitialGrants| async move {
        let result: Result<Value, capnp::Error> = async {
            let caps = read_initial_grants(&initial_grants).await?;
            let names = names(&caps);
            let host: system_capnp::host::Client = find_cap(&caps, "attenuated-host")?;
            let id = host.id_request().send().promise.await?;
            let peer_id = id.get()?.get_peer_id()?.to_vec();
            let denied = match host.network_request().send().promise.await {
                Ok(_) => {
                    return Err(capnp::Error::failed(
                        "attenuated host unexpectedly allowed network".into(),
                    ))
                }
                Err(error) => error,
            };
            Ok(json!({
                "names": names,
                "peer_id": peer_id,
                "denied": denied.to_string(),
            }))
        }
        .await;
        emit(match result {
            Ok(detail) => json!({"mode": "attenuated", "ok": true, "detail": detail}),
            Err(error) => json!({"mode": "attenuated", "ok": false, "error": text_error(error)}),
        });
        Ok(())
    });
}

fn run_trusted_lattice() {
    let image = std::env::var("WW_PROBE_IMAGE")
        .unwrap_or_else(|_| "runtime-selected-image".to_owned())
        .into_bytes();
    system::run(|initial_grants: InitialGrants| async move {
        let result: Result<Value, capnp::Error> = async {
            let caps = read_initial_grants(&initial_grants).await?;
            let runtime: system_capnp::runtime::Client = find_cap(&caps, "runtime")?;
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
                "names": names(&caps),
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
    });
}

fn run_late_delegation() {
    system::run(|initial_grants: InitialGrants| async move {
        let result: Result<Value, capnp::Error> = async {
            let initial = read_initial_grants(&initial_grants).await?;
            let initial_names = names(&initial);
            let mailbox: system_capnp::host::Client = find_cap(&initial, "mailbox")?;
            let network = mailbox.network_request().send().promise.await?;
            let vat_client = network.get()?.get_vat_client()?;
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
            let delegated: system_capnp::host::Client =
                system_capnp::host::Client::new(delegated.hook);
            let response = delegated.id_request().send().promise.await?;
            let peer_id = response.get()?.get_peer_id()?.to_vec();
            let after_names = names(&read_initial_grants(&initial_grants).await?);
            Ok(json!({
                "initial_names": initial_names,
                "received_later": ["delegated-x"],
                "current_holdings": ["mailbox", "delegated-x"],
                "after_names": after_names,
                "delegated_peer_id": peer_id,
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
    });
}

fn run_invoke_all() {
    system::run(|initial_grants: InitialGrants| async move {
        let mut results = Vec::new();
        let mut usable = Vec::new();
        for name in [
            "host",
            "runtime",
            "routing",
            "authority",
            "identity",
            "ipfs",
            "http-client",
        ] {
            let result = invoke_named(initial_grants.clone(), name.to_owned()).await;
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
    });
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

fn run_routing() {
    system::run(|initial_grants: InitialGrants| async move {
        let result: Result<Value, capnp::Error> = async {
            let caps = read_initial_grants(&initial_grants).await?;
            let routing: routing_capnp::routing::Client = find_cap(&caps, "routing")?;

            let mut hash = routing.hash_request();
            hash.get().set_data(b"authority-probe-routing");
            let hash = hash.send().promise.await?;
            let key = hash
                .get()?
                .get_key()?
                .to_str()
                .map_err(|error| capnp::Error::failed(error.to_string()))?
                .to_owned();

            let mut provide = routing.provide_request();
            provide.get().set_key(&key);
            provide.send().promise.await?;

            let providers = Rc::new(Cell::new(0));
            let done = Rc::new(Cell::new(false));
            let sink: routing_capnp::provider_sink::Client = capnp_rpc::new_client(ProviderSink {
                providers: providers.clone(),
                done: done.clone(),
            });
            let mut find = routing.find_providers_request();
            find.get().set_key(&key);
            find.get().set_count(3);
            find.get().set_sink(sink);
            find.send().promise.await?;

            let mut publish = routing.publish_request();
            publish.get().set_name("");
            publish.get().set_cid("");
            publish.get().set_expected_current("");
            let publish_reached = publish.send().promise.await.is_err();

            let mut write = routing.write_file_request();
            write.get().set_base_cid("");
            write.get().set_path("authority-probe");
            write.get().set_data(b"mutable");
            write.get().set_create_parents(true);
            let mutable_rpc_reached = write.send().promise.await.is_err();

            Ok(json!({
                "hash": key,
                "provide": true,
                "find_providers": true,
                "providers": providers.get(),
                "done": done.get(),
                "publish_rpc_reached": publish_reached,
                "mutable_rpc_reached": mutable_rpc_reached,
            }))
        }
        .await;
        emit(match result {
            Ok(detail) => json!({"mode": "routing", "ok": true, "detail": detail}),
            Err(error) => json!({"mode": "routing", "ok": false, "error": text_error(error)}),
        });
        Ok(())
    });
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

fn run_descendant() {
    let http_url = std::env::var("WW_PROBE_HTTP_URL").ok();
    system::run(|initial_grants: InitialGrants| async move {
        let result: Result<Value, capnp::Error> = async {
            let caps = read_initial_grants(&initial_grants).await?;
            let executor: system_capnp::executor::Client = find_cap(&caps, "restricted-executor")?;
            let narrow = caps.iter().find(|entry| entry.name == "narrow");

            let mut alias_request = executor.spawn_request();
            {
                let mut args = alias_request.get().init_args(2);
                args.set(0, "authority-probe");
                args.set(1, "alias-redelivery");
            }
            alias_request.get().init_env(0);
            if let Some(narrow) = narrow {
                let mut grants = alias_request.get().init_caps(2);
                for (index, name) in ["alias-a", "alias-b"].iter().enumerate() {
                    let mut entry = grants.reborrow().get(index as u32);
                    entry.set_name(name);
                    entry.init_cap().set_as_capability(narrow.cap.clone().hook);
                }
            } else {
                alias_request.get().init_caps(0);
            }
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
                args.set(1, "invoke-all");
            }
            if let Some(http_url) = http_url {
                let mut env = omitted_request.get().init_env(1);
                env.set(0, format!("WW_PROBE_HTTP_URL={http_url}"));
            } else {
                omitted_request.get().init_env(0);
            }
            omitted_request.get().init_caps(0);
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
    });
}

fn run_raw_host() {
    system::run(|host: system_capnp::host::Client| async move {
        let result = host.id_request().send().promise.await;
        emit(match result {
            Ok(response) => json!({
                "mode": "raw-host",
                "ok": true,
                "peer_id": response.get().and_then(|r| r.get_peer_id()).map(|v| v.to_vec()).unwrap_or_default(),
            }),
            Err(error) => json!({"mode": "raw-host", "ok": false, "error": text_error(error)}),
        });
        Ok(())
    });
}

fn run_substrate() {
    system::run(|_initial_grants: InitialGrants| async move {
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
        let scratch_path = format!("/tmp/authority-probe-{}", rand::random::<u64>());
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
                "monotonic_nanos": wasip2::clocks::monotonic_clock::now(),
            },
            "random_u64": rand::random::<u64>(),
        }));
        Ok(())
    });
}

fn run_scratch_observe() {
    system::run(|_initial_grants: InitialGrants| async move {
        let path = "/tmp/authority-probe-private";
        let observed_before_write = std::path::Path::new(path).exists();
        let write = std::fs::write(path, b"sibling").map_err(text_error);
        emit(json!({
            "mode": "scratch-observe",
            "observed_before_write": observed_before_write,
            "write": value_or_error(write),
        }));
        Ok(())
    });
}

fn run_scratch_parent() {
    system::run(|initial_grants: InitialGrants| async move {
        let result: Result<Value, capnp::Error> = async {
            let caps = read_initial_grants(&initial_grants).await?;
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
            spawn.get().init_caps(0);
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
    });
}

struct AuthorityProbe;

impl Guest for AuthorityProbe {
    fn run() -> Result<(), ()> {
        match std::env::args().nth(1).as_deref() {
            Some("invoke") => run_invoke(),
            Some("arbitrary-name") => run_arbitrary_name(),
            Some("alias-redelivery") => run_alias_redelivery(),
            Some("attenuated") => run_attenuated(),
            Some("trusted-lattice") => run_trusted_lattice(),
            Some("late-delegation") => run_late_delegation(),
            Some("invoke-all") => run_invoke_all(),
            Some("routing") => run_routing(),
            Some("descendant") => run_descendant(),
            Some("raw-host") => run_raw_host(),
            Some("substrate") => run_substrate(),
            Some("scratch-observe") => run_scratch_observe(),
            Some("scratch-parent") => run_scratch_parent(),
            _ => run_enumerate(),
        }
        Ok(())
    }
}

wasip2::cli::command::export!(AuthorityProbe);
