use std::cell::RefCell;
use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;

use capnp::capability::FromClientHook;
use capnp::traits::HasTypeId;
use caps::{make_import_cap, make_import_handler};
use glia::eval::{self, Dispatch, Env};
use glia::{extract_method, make_cap, read, read_many, AttenuatedCapInner, GliaCapInner, Val};

use std::rc::Rc;

use wasip2::cli::stderr::get_stderr;
use wasip2::cli::stdin::get_stdin;
use wasip2::cli::stdout::get_stdout;
use wasip2::exports::cli::run::Guest;

#[allow(dead_code)]
mod system_capnp {
    include!(concat!(env!("OUT_DIR"), "/system_capnp.rs"));
}

#[allow(dead_code)]
mod synapse_capnp {
    include!(concat!(env!("OUT_DIR"), "/synapse_capnp.rs"));
}

#[allow(dead_code, clippy::extra_unused_type_parameters)]
mod stem_capnp {
    include!(concat!(env!("OUT_DIR"), "/stem_capnp.rs"));
}

#[allow(dead_code, clippy::extra_unused_type_parameters)]
mod auth_capnp {
    include!(concat!(env!("OUT_DIR"), "/auth_capnp.rs"));
}

#[allow(dead_code, clippy::extra_unused_type_parameters)]
mod membrane_capnp {
    include!(concat!(env!("OUT_DIR"), "/membrane_capnp.rs"));
}

#[allow(dead_code)]
mod routing_capnp {
    include!(concat!(env!("OUT_DIR"), "/routing_capnp.rs"));
}

#[allow(dead_code)]
mod http_capnp {
    include!(concat!(env!("OUT_DIR"), "/http_capnp.rs"));
}

// Content-addressed schema CIDs for built-in capability interfaces.
#[allow(dead_code)]
mod schema_ids {
    include!(concat!(env!("OUT_DIR"), "/schema_ids.rs"));
}

/// Bootstrap capability: the concrete Membrane defined in membrane.capnp.
type Membrane = membrane_capnp::membrane::Client;

fn write_synapse_from_client(
    mut builder: synapse_capnp::synapse::Builder<'_>,
    name: &str,
    client: capnp::capability::Client,
) {
    let mut descriptor = builder.reborrow().init_descriptor();
    descriptor.set_display_name(name);
    descriptor.set_interface_id(0);
    descriptor.set_schema_cid("");
    descriptor.set_payload_codec(synapse_capnp::PayloadCodec::Capnp);
    descriptor.reborrow().init_methods(0);
    let mut invoker_ids = descriptor.reborrow().init_invoker_interface_ids(1);
    invoker_ids.set(0, synapse_capnp::invokable::Client::TYPE_ID);
    descriptor.init_schema_nodes(0);
    builder.set_invokable(synapse_capnp::invokable::Client::new(client.hook));
}

/// Exported kernel bootstrap capability.
///
/// The kernel boots with host-provided `Membrane` access, and the shell client
/// expects the kernel process to export a `Membrane` bootstrap cap in return.
/// This proxy forwards `graft()` to the active host membrane once initialization
/// has stored it.
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
            Some(m) => m,
            None => {
                return capnp::capability::Promise::err(capnp::Error::failed(
                    "kernel bootstrap membrane not ready".into(),
                ))
            }
        };

        capnp::capability::Promise::from_future(async move {
            let resp = membrane.graft_request().send().promise.await?;
            let src_caps = resp.get()?.get_caps()?;
            let mut dst_caps = results.get().init_caps(src_caps.len());

            for i in 0..src_caps.len() {
                let src = src_caps.get(i);
                let mut dst = dst_caps.reborrow().get(i);
                dst.set_name(src.get_name()?);
                dst.set_synapse(src.get_synapse()?)?;
            }

            Ok(())
        })
    }
}

struct StderrLogger;

impl log::Log for StderrLogger {
    fn enabled(&self, metadata: &log::Metadata<'_>) -> bool {
        metadata.level() <= log::Level::Warn
    }

    fn log(&self, record: &log::Record<'_>) {
        if !self.enabled(record.metadata()) {
            return;
        }
        let stderr = get_stderr();
        let _ = stderr.blocking_write_and_flush(
            format!("[{}] {}\n", record.level(), record.args()).as_bytes(),
        );
    }

    fn flush(&self) {}
}

static LOGGER: StderrLogger = StderrLogger;

fn init_logging() {
    if log::set_logger(&LOGGER).is_ok() {
        // pid0 runs on the operator's console — keep it quiet by default.
        // Warnings and errors only; info/debug/trace are suppressed.
        log::set_max_level(log::LevelFilter::Warn);
    }
}

// ---------------------------------------------------------------------------
// Evaluator — dispatches (capability method args...) to RPC calls
// ---------------------------------------------------------------------------

struct Session {
    host: system_capnp::host::Client,
    runtime: system_capnp::runtime::Client,
    routing: routing_capnp::routing::Client,
    /// Host-side node identity hub for this session.
    ///
    /// Call `identity.signer("ww-membrane-graft")` (or another known domain) to
    /// obtain a domain-scoped [`auth_capnp::signer::Client`].  The identity secret
    /// never crosses the host–guest boundary; only this capability reference is passed.
    #[allow(dead_code)]
    identity: auth_capnp::identity::Client,
    /// Outbound HTTP capability for WASM guests.
    ///
    /// Domain-scoped proxy — the host checks URL host against an allowlist.
    /// Exposed to glia scripts via `(perform host :http-client)`.
    /// `None` when the operator did not pass `--http-dial`.
    http_client: Option<http_capnp::http_client::Client>,
    cwd: String,
}


// ---------------------------------------------------------------------------
// Cap extraction — get type-erased capnp Client from Val::Cap.inner
// ---------------------------------------------------------------------------

/// Extract the type-erased `capnp::capability::Client` from a `Val::Cap`.
///
/// Tries each known capnp client type and returns the inner `.client` field.
/// Returns `None` if the inner type doesn't match any known cap.
fn extract_capnp_client(
    inner: &std::rc::Rc<dyn std::any::Any>,
) -> Option<capnp::capability::Client> {
    if let Some(c) = inner.downcast_ref::<capnp::capability::Client>() {
        return Some(c.clone());
    }

    macro_rules! try_downcast {
        ($ty:ty) => {
            if let Some(c) = inner.downcast_ref::<$ty>() {
                return Some(c.client.clone());
            }
        };
    }
    try_downcast!(system_capnp::host::Client);
    try_downcast!(system_capnp::runtime::Client);
    try_downcast!(routing_capnp::routing::Client);
    try_downcast!(auth_capnp::identity::Client);
    try_downcast!(http_capnp::http_client::Client);
    try_downcast!(system_capnp::executor::Client);
    None
}

fn collect_forwardable_caps(
    caps: &[(String, Val)],
    context: &str,
) -> Vec<(String, capnp::capability::Client)> {
    caps.iter()
        .filter_map(|(name, val)| {
            if let Val::Cap { inner, .. } = val {
                if let Some(client) = extract_capnp_client(inner) {
                    return Some((name.clone(), client));
                }
                log::debug!("{context} — cap '{name}' is not a capnp client, skipping");
            }
            None
        })
        .collect()
}

fn make_generic_cap(cap: capnp::capability::Client) -> Val {
    make_cap("cap", "capnp:capability", Rc::new(cap))
}

fn make_process_cap(process: system_capnp::process::Client) -> Val {
    let mut methods = HashMap::new();

    let bootstrap_process = process.clone();
    methods.insert(
        "bootstrap".to_string(),
        Val::AsyncNativeFn {
            name: "process-bootstrap".into(),
            func: Rc::new(move |args: Vec<Val>| {
                let process = bootstrap_process.clone();
                Box::pin(async move {
                    if !args.is_empty() {
                        return Err(glia::error::arity("process :bootstrap", "0", args.len()));
                    }
                    let resp = process
                        .bootstrap_request()
                        .send()
                        .promise
                        .await
                        .map_err(|e| Val::from(e.to_string()))?;
                    let cap = resp
                        .get()
                        .map_err(|e| Val::from(e.to_string()))?
                        .get_synapse()
                        .map_err(|e| Val::from(e.to_string()))?
                        .get_invokable()
                        .map_err(|e| Val::from(e.to_string()))?;
                    Ok(make_generic_cap(cap.client))
                })
            }),
        },
    );

    let wait_process = process.clone();
    methods.insert(
        "wait".to_string(),
        Val::AsyncNativeFn {
            name: "process-wait".into(),
            func: Rc::new(move |args: Vec<Val>| {
                let process = wait_process.clone();
                Box::pin(async move {
                    if !args.is_empty() {
                        return Err(glia::error::arity("process :wait", "0", args.len()));
                    }
                    let resp = process
                        .wait_request()
                        .send()
                        .promise
                        .await
                        .map_err(|e| Val::from(e.to_string()))?;
                    let exit_code = resp
                        .get()
                        .map_err(|e| Val::from(e.to_string()))?
                        .get_exit_code();
                    Ok(Val::Int(exit_code as i64))
                })
            }),
        },
    );

    methods.insert(
        "kill".to_string(),
        Val::AsyncNativeFn {
            name: "process-kill".into(),
            func: Rc::new(move |args: Vec<Val>| {
                let process = process.clone();
                Box::pin(async move {
                    if !args.is_empty() {
                        return Err(glia::error::arity("process :kill", "0", args.len()));
                    }
                    process
                        .kill_request()
                        .send()
                        .promise
                        .await
                        .map_err(|e| Val::from(e.to_string()))?;
                    Ok(Val::Nil)
                })
            }),
        },
    );

    make_cap(
        "process",
        "ww.process.v1",
        Rc::new(GliaCapInner {
            methods,
            descriptor: b"ww.process.v1\nmethods=bootstrap,wait,kill\n".to_vec(),
        }),
    )
}

fn make_executor_cap(executor: system_capnp::executor::Client) -> Val {
    let mut methods = HashMap::new();
    methods.insert(
        "spawn".to_string(),
        Val::AsyncNativeFn {
            name: "executor-spawn".into(),
            func: Rc::new(move |args: Vec<Val>| {
                let executor = executor.clone();
                Box::pin(async move {
                    let mut spawn_args: Vec<String> = Vec::new();
                    let mut env_pairs: Vec<String> = Vec::new();
                    let mut cap_pairs: Vec<(String, capnp::capability::Client)> = Vec::new();

                    let mut i = 0;
                    while i < args.len() {
                        let key = match &args[i] {
                            Val::Keyword(k) => k.as_str(),
                            other => {
                                return Err(glia::error::type_mismatch(
                                    "executor :spawn option",
                                    "keyword",
                                    other,
                                ))
                            }
                        };
                        i += 1;
                        let value = args.get(i).ok_or_else(|| {
                            Val::from(format!("executor :spawn — missing value for :{key}"))
                        })?;
                        match key {
                            "args" => match value {
                                Val::List(items) | Val::Vector(items) => {
                                    spawn_args = items
                                        .iter()
                                        .map(|v| match v {
                                            Val::Str(s) | Val::Sym(s) => Ok(s.clone()),
                                            other => Err(glia::error::type_mismatch(
                                                "executor :spawn :args item",
                                                "string",
                                                other,
                                            )),
                                        })
                                        .collect::<Result<Vec<_>, _>>()?;
                                }
                                other => {
                                    return Err(glia::error::type_mismatch(
                                        "executor :spawn :args",
                                        "list or vector",
                                        other,
                                    ))
                                }
                            },
                            "env" => match value {
                                Val::Map(pairs) => {
                                    env_pairs = pairs
                                        .iter()
                                        .map(|(k, v)| {
                                            let key = match k {
                                                Val::Str(s) | Val::Sym(s) => s.clone(),
                                                other => format!("{other}"),
                                            };
                                            let val = match v {
                                                Val::Str(s) | Val::Sym(s) => s.clone(),
                                                other => format!("{other}"),
                                            };
                                            format!("{key}={val}")
                                        })
                                        .collect();
                                }
                                other => {
                                    return Err(glia::error::type_mismatch(
                                        "executor :spawn :env",
                                        "map",
                                        other,
                                    ))
                                }
                            },
                            "caps" => match value {
                                Val::Map(pairs) => {
                                    for (name_val, cap_val) in pairs.iter() {
                                        let name = match name_val {
                                            Val::Str(s) | Val::Sym(s) => s.clone(),
                                            other => {
                                                return Err(glia::error::type_mismatch(
                                                    "executor :spawn :caps key",
                                                    "string",
                                                    other,
                                                ))
                                            }
                                        };
                                        let Val::Cap { inner, .. } = cap_val else {
                                            return Err(glia::error::type_mismatch(
                                                "executor :spawn :caps value",
                                                "cap",
                                                cap_val,
                                            ));
                                        };
                                        if let Some(client) = extract_capnp_client(inner) {
                                            cap_pairs.push((name, client));
                                        }
                                    }
                                }
                                other => {
                                    return Err(glia::error::type_mismatch(
                                        "executor :spawn :caps",
                                        "map",
                                        other,
                                    ))
                                }
                            },
                            other => {
                                return Err(Val::from(format!(
                                    "executor :spawn — unknown option :{other}"
                                )))
                            }
                        }
                        i += 1;
                    }

                    let mut req = executor.spawn_request();
                    {
                        let mut b = req.get();
                        if !spawn_args.is_empty() {
                            let mut arg_list = b.reborrow().init_args(spawn_args.len() as u32);
                            for (j, a) in spawn_args.iter().enumerate() {
                                arg_list.set(j as u32, a);
                            }
                        }
                        if !env_pairs.is_empty() {
                            let mut env_list = b.reborrow().init_env(env_pairs.len() as u32);
                            for (j, e) in env_pairs.iter().enumerate() {
                                env_list.set(j as u32, e);
                            }
                        }
                        if !cap_pairs.is_empty() {
                            let mut caps_builder = b.init_caps(cap_pairs.len() as u32);
                            for (j, (name, client)) in cap_pairs.into_iter().enumerate() {
                                let mut entry = caps_builder.reborrow().get(j as u32);
                                entry.set_name(&name);
                                write_synapse_from_client(entry.init_synapse(), &name, client);
                            }
                        }
                    }
                    let resp = req
                        .send()
                        .promise
                        .await
                        .map_err(|e| Val::from(e.to_string()))?;
                    let process = resp
                        .get()
                        .map_err(|e| Val::from(e.to_string()))?
                        .get_process()
                        .map_err(|e| Val::from(e.to_string()))?;
                    Ok(make_process_cap(process))
                })
            }),
        },
    );

    make_cap(
        "executor",
        schema_ids::EXECUTOR_CID.to_string(),
        Rc::new(GliaCapInner {
            methods,
            descriptor: b"ww.executor.v1\nmethods=spawn\n".to_vec(),
        }),
    )
}

// ---------------------------------------------------------------------------
// Dispatch table — builtins that don't go through the effect system
// ---------------------------------------------------------------------------

/// Async handler: takes evaluated args and the shell context.
type HandlerFn = for<'a> fn(
    &'a [Val],
    &'a RefCell<Session>,
) -> Pin<Box<dyn Future<Output = Result<Val, Val>> + 'a>>;

/// Build the dispatch table for builtins only. Capability verbs (host, runtime,
/// ipfs, routing) are handled via cap-targeted perform + with-effect-handler.
fn build_dispatch() -> HashMap<&'static str, HandlerFn> {
    let mut t: HashMap<&'static str, HandlerFn> = HashMap::new();
    t.insert("load", |a, _| Box::pin(std::future::ready(eval_load(a))));
    t.insert("cd", |a, c| Box::pin(std::future::ready(eval_cd(a, c))));
    t.insert("help", |_, _| {
        Box::pin(std::future::ready(Ok(Val::Str(HELP_TEXT.to_string()))))
    });
    t.insert("exit", |_, _| {
        Box::pin(std::future::ready({
            std::process::exit(0);
            #[allow(unreachable_code)]
            Ok(Val::Nil)
        }))
    });
    t
}

/// (load "path") — read bytes from the WASI virtual filesystem.
///
/// Relative paths like `"bin/chess-demo.wasm"` are resolved against the WASI
/// root (`/`), which the host preopens to the merged FHS image directory.
/// Absolute paths are used as-is.
///
/// Loaded files are cached in a thread-local map so repeated loads of the
/// same path (e.g. chess.glia loading the WASM for both :listen and :run)
/// return a cheap clone.  This also works around an ESPIPE (os error 29)
/// that occurs on the second `std::fs::read` in WASI P2 when the RPC
/// connection streams have been created between reads.
fn eval_load(args: &[Val]) -> Result<Val, Val> {
    thread_local! {
        static CACHE: RefCell<HashMap<String, Vec<u8>>> = RefCell::new(HashMap::new());
    }

    let path = match args.first() {
        Some(Val::Str(s)) => s.clone(),
        _ => return Err("(load \"<path>\")".into()),
    };
    // Resolve relative to WASI root — the host mounts the merged image at `/`.
    let resolved = if path.starts_with('/') {
        path.clone()
    } else {
        format!("/{path}")
    };

    // Return cached bytes if already loaded.
    let cached = CACHE.with(|c| c.borrow().get(&resolved).cloned());
    if let Some(bytes) = cached {
        return Ok(Val::Bytes(bytes));
    }

    let bytes =
        std::fs::read(&resolved).map_err(|e| Val::from(format!("load: {resolved}: {e}")))?;
    CACHE.with(|c| c.borrow_mut().insert(resolved, bytes.clone()));
    Ok(Val::Bytes(bytes))
}

fn eval_cd(args: &[Val], ctx: &RefCell<Session>) -> Result<Val, Val> {
    let path = match args.first() {
        Some(Val::Str(s)) => s.clone(),
        Some(Val::Sym(s)) => s.clone(),
        None => "/".to_string(),
        _ => return Err("(cd \"<path>\")".into()),
    };
    ctx.borrow_mut().cwd = path;
    Ok(Val::Nil)
}

// ---------------------------------------------------------------------------
// Kernel dispatch — bridges glia's evaluator to kernel capabilities
// ---------------------------------------------------------------------------

/// Bundles the capability context and dispatch table so the kernel can
/// implement [`glia::eval::Dispatch`].
struct KernelDispatch<'k> {
    ctx: &'k RefCell<Session>,
    table: &'k HashMap<&'static str, HandlerFn>,
}

impl<'k> Dispatch for KernelDispatch<'k> {
    fn call<'a>(
        &'a self,
        name: &'a str,
        args: &'a [Val],
    ) -> Pin<Box<dyn Future<Output = Result<Val, Val>> + 'a>> {
        Box::pin(async move {
            match self.table.get(name) {
                Some(handler) => handler(args, self.ctx).await,
                None => eval_path_lookup(name, args, self.ctx).await,
            }
        })
    }
}

// ---------------------------------------------------------------------------
// Evaluator — delegates to glia with kernel dispatch
// ---------------------------------------------------------------------------

fn eval<'a>(
    expr: &'a Val,
    env: &'a mut Env,
    ctx: &'a RefCell<Session>,
    dispatch: &'a HashMap<&'static str, HandlerFn>,
) -> Pin<Box<dyn Future<Output = Result<Val, Val>> + 'a>> {
    Box::pin(async move {
        let kd = KernelDispatch {
            ctx,
            table: dispatch,
        };
        eval::eval_toplevel(expr, env, &kd).await
    })
}

// ---------------------------------------------------------------------------
// Cap handlers — AsyncNativeFn closures that dispatch cap-targeted performs
// ---------------------------------------------------------------------------
//
// Each handler receives (data, resume) where data = (:method args...).
// The handler makes the RPC call and calls resume(result) to continue the body.
//
// Pattern: handler calls resume → returns Err(Val::Resume(val)) → poll_fn
// catches it → body future resumes with the value from the oneshot channel.

/// Call the resume function with a result value.
/// Returns the Resume sentinel that the poll_fn state machine expects.
fn call_resume(resume: &Val, val: Val) -> Result<Val, Val> {
    match resume {
        Val::NativeFn { func, .. } => func(&[val]),
        _ => Err(Val::from("cap handler: invalid resume function")),
    }
}

fn make_host_handler(
    host: system_capnp::host::Client,
    runtime: system_capnp::runtime::Client,
    http_client: Option<http_capnp::http_client::Client>,
) -> Val {
    Val::AsyncNativeFn {
        name: "host-handler".into(),
        func: Rc::new(move |args: Vec<Val>| {
            let host = host.clone();
            let runtime = runtime.clone();
            let http_client = http_client.clone();
            Box::pin(async move {
                let (method, rest) = extract_method(&args[0])?;
                let resume = &args[1];
                let result = match method {
                    "id" => {
                        let resp = host
                            .id_request()
                            .send()
                            .promise
                            .await
                            .map_err(|e| Val::from(e.to_string()))?;
                        let id = resp
                            .get()
                            .map_err(|e| Val::from(e.to_string()))?
                            .get_peer_id()
                            .map_err(|e| Val::from(e.to_string()))?;
                        Val::Str(bs58::encode(id).into_string())
                    }
                    "addrs" => {
                        let resp = host
                            .addrs_request()
                            .send()
                            .promise
                            .await
                            .map_err(|e| Val::from(e.to_string()))?;
                        let addrs = resp
                            .get()
                            .map_err(|e| Val::from(e.to_string()))?
                            .get_addrs()
                            .map_err(|e| Val::from(e.to_string()))?;
                        let items: Vec<Val> = (0..addrs.len())
                            .filter_map(|i| {
                                addrs
                                    .get(i)
                                    .ok()
                                    .and_then(|d| multiaddr::Multiaddr::try_from(d.to_vec()).ok())
                                    .map(|m| Val::Str(m.to_string()))
                            })
                            .collect();
                        Val::List(items)
                    }
                    "peers" => {
                        let resp = host
                            .peers_request()
                            .send()
                            .promise
                            .await
                            .map_err(|e| Val::from(e.to_string()))?;
                        let peers = resp
                            .get()
                            .map_err(|e| Val::from(e.to_string()))?
                            .get_peers()
                            .map_err(|e| Val::from(e.to_string()))?;
                        let items: Vec<Val> = (0..peers.len())
                            .filter_map(|i| {
                                let peer = peers.get(i);
                                let id = peer
                                    .get_peer_id()
                                    .ok()
                                    .map(|b| bs58::encode(b).into_string())?;
                                let addrs = peer.get_addrs().ok()?;
                                let addr_vals: Vec<Val> = (0..addrs.len())
                                    .filter_map(|j| {
                                        addrs
                                            .get(j)
                                            .ok()
                                            .and_then(|a| {
                                                multiaddr::Multiaddr::try_from(a.to_vec()).ok()
                                            })
                                            .map(|m| Val::Str(m.to_string()))
                                    })
                                    .collect();
                                Some(Val::Map(glia::ValMap::from_pairs(vec![
                                    (Val::Keyword("peer-id".into()), Val::Str(id)),
                                    (Val::Keyword("addrs".into()), Val::List(addr_vals)),
                                ])))
                            })
                            .collect();
                        Val::List(items)
                    }
                    "listen" => {
                        let (cell, prefix) = match rest {
                            [Val::Cell { wasm, caps }, Val::Str(prefix)] => ((wasm, caps), prefix),
                            [Val::Cell { .. }] => {
                                return Err(Val::from(
                                    "host :listen — vat cell listen was removed; use (perform runtime :load ...), (perform executor :spawn), (perform process :bootstrap), then (perform host :serve-vat cap \"service\")",
                                ))
                            }
                            [] => {
                                return Err(Val::from(
                                    "host :listen — usage: (perform host :listen <cell> \"/path\") for HTTP cells",
                                ))
                            }
                            [other, ..] => {
                                return Err(Val::from(format!(
                                    "host :listen — expected cell and HTTP path string, got {other}"
                                )))
                            }
                        };

                        let (wasm, caps) = cell;
                        let mut load_req = runtime.load_request();
                        load_req.get().set_wasm(wasm);
                        let executor = load_req.send().pipeline.get_executor();

                        let network_resp = host
                            .network_request()
                            .send()
                            .promise
                            .await
                            .map_err(|e| Val::from(e.to_string()))?;
                        let network = network_resp.get().map_err(|e| Val::from(e.to_string()))?;
                        let listener = network
                            .get_http_listener()
                            .map_err(|e| Val::from(e.to_string()))?;
                        let mut req = listener.listen_request();
                        req.get().set_executor(executor);
                        req.get().set_prefix(prefix);

                        let valid_caps = collect_forwardable_caps(caps, "host :listen");
                        if !valid_caps.is_empty() {
                            let mut caps_builder = req.get().init_caps(valid_caps.len() as u32);
                            for (i, (name, client)) in valid_caps.into_iter().enumerate() {
                                let mut entry = caps_builder.reborrow().get(i as u32);
                                entry.set_name(&name);
                                write_synapse_from_client(entry.init_synapse(), &name, client);
                            }
                        }

                        req.send()
                            .promise
                            .await
                            .map_err(|e| Val::from(e.to_string()))?;
                        log::info!("host :listen — registered HTTP handler at {prefix} (cell)");
                        Val::Nil
                    }
                    "listen-stream" => {
                        let (wasm, caps, protocol) = match rest {
                            [Val::Cell { wasm, caps }, Val::Str(protocol)] => {
                                (wasm, caps, protocol)
                            }
                            [] | [_] => {
                                return Err(Val::from(
                                    "host :listen-stream — usage: (perform host :listen-stream <cell> \"protocol\")",
                                ))
                            }
                            [other, ..] => {
                                return Err(Val::from(format!(
                                    "host :listen-stream — expected cell and protocol string, got {other}"
                                )))
                            }
                        };

                        let mut load_req = runtime.load_request();
                        load_req.get().set_wasm(wasm);
                        let executor = load_req.send().pipeline.get_executor();

                        let network_resp = host
                            .network_request()
                            .send()
                            .promise
                            .await
                            .map_err(|e| Val::from(e.to_string()))?;
                        let network = network_resp.get().map_err(|e| Val::from(e.to_string()))?;
                        let listener = network
                            .get_stream_listener()
                            .map_err(|e| Val::from(e.to_string()))?;
                        let mut req = listener.listen_request();
                        req.get().set_executor(executor);
                        req.get().set_protocol(protocol);

                        let valid_caps = collect_forwardable_caps(caps, "host :listen-stream");
                        if !valid_caps.is_empty() {
                            let mut caps_builder = req.get().init_caps(valid_caps.len() as u32);
                            for (i, (name, client)) in valid_caps.into_iter().enumerate() {
                                let mut entry = caps_builder.reborrow().get(i as u32);
                                entry.set_name(&name);
                                write_synapse_from_client(entry.init_synapse(), &name, client);
                            }
                        }

                        req.send()
                            .promise
                            .await
                            .map_err(|e| Val::from(e.to_string()))?;
                        log::info!(
                            "host :listen-stream — registered stream handler /ww/0.1.0/stream/{protocol}"
                        );
                        Val::Nil
                    }
                    "serve-vat" => {
                        let (cap, protocol) = match rest {
                            [Val::Cap { inner, .. }, Val::Str(protocol)] => {
                                let cap = extract_capnp_client(inner).ok_or_else(|| {
                                    Val::from(
                                        "host :serve-vat — cap is not backed by a Cap'n Proto client",
                                    )
                                })?;
                                (cap, protocol)
                            }
                            [] | [_] => {
                                return Err(Val::from(
                                    "host :serve-vat — usage: (perform host :serve-vat cap \"service\")",
                                ))
                            }
                            [other, ..] => {
                                return Err(Val::from(format!(
                                    "host :serve-vat — expected cap and protocol string, got {other}"
                                )))
                            }
                        };

                        let network_resp = host
                            .network_request()
                            .send()
                            .promise
                            .await
                            .map_err(|e| Val::from(e.to_string()))?;
                        let network = network_resp.get().map_err(|e| Val::from(e.to_string()))?;
                        let listener = network
                            .get_vat_listener()
                            .map_err(|e| Val::from(e.to_string()))?;
                        let mut req = listener.serve_request();
                        write_synapse_from_client(req.get().init_synapse(), "vat-service", cap);
                        req.get().set_protocol(protocol);
                        req.send()
                            .promise
                            .await
                            .map_err(|e| Val::from(e.to_string()))?;
                        log::info!("host :serve-vat — registered vat service {protocol}");
                        Val::Nil
                    }
                    "http-client" => {
                        // (perform host :http-client) → Val::Cap wrapping HttpClient
                        // Future: parse :allow and :rate kwargs from rest
                        match &http_client {
                            Some(c) => make_cap(
                                "http",
                                schema_ids::HTTP_CLIENT_CID.to_string(),
                                Rc::new(c.clone()),
                            ),
                            None => return Err(Val::from(
                                "http-client not available (node started without --http-dial)",
                            )),
                        }
                    }
                    _ => return Err(Val::from(format!("host: unknown method :{method}"))),
                };
                call_resume(resume, result)
            })
        }),
    }
}

fn make_runtime_handler(runtime: system_capnp::runtime::Client) -> Val {
    Val::AsyncNativeFn {
        name: "runtime-handler".into(),
        func: Rc::new(move |args: Vec<Val>| {
            let runtime = runtime.clone();
            Box::pin(async move {
                let (method, rest) = extract_method(&args[0])?;
                let resume = &args[1];
                let result = match method {
                    "load" => {
                        let wasm = match rest.first() {
                            Some(Val::Bytes(b)) => b.clone(),
                            _ => {
                                return Err(Val::from(
                                    "runtime :load — first arg must be wasm bytes",
                                ))
                            }
                        };
                        if rest.len() != 1 {
                            return Err(glia::error::arity("runtime :load", "1", rest.len()));
                        }

                        let mut load_req = runtime.load_request();
                        load_req.get().set_wasm(&wasm);
                        let executor = load_req.send().pipeline.get_executor();
                        make_executor_cap(executor)
                    }
                    "run" => {
                        // (perform runtime :run <wasm-bytes> :env {"KEY" "VAL" ...})
                        let wasm = match rest.first() {
                            Some(Val::Bytes(b)) => b.clone(),
                            _ => {
                                return Err(Val::from(
                                    "runtime :run — first arg must be wasm bytes",
                                ))
                            }
                        };
                        // Parse optional keyword args: :env {map}, :args [list]
                        let mut env_pairs: Vec<String> = Vec::new();
                        let mut spawn_args: Vec<String> = Vec::new();
                        let mut i = 1;
                        while i < rest.len() {
                            if let Val::Keyword(k) = &rest[i] {
                                if k == "env" {
                                    i += 1;
                                    if let Some(Val::Map(pairs)) = rest.get(i) {
                                        for (k, v) in pairs {
                                            let key = match k {
                                                Val::Str(s) | Val::Sym(s) => s.clone(),
                                                other => format!("{other}"),
                                            };
                                            let val = match v {
                                                Val::Str(s) | Val::Sym(s) => s.clone(),
                                                other => format!("{other}"),
                                            };
                                            env_pairs.push(format!("{key}={val}"));
                                        }
                                    }
                                } else if k == "args" {
                                    i += 1;
                                    if let Some(Val::List(items) | Val::Vector(items)) = rest.get(i)
                                    {
                                        spawn_args = items
                                            .iter()
                                            .map(|v| match v {
                                                Val::Str(s) | Val::Sym(s) => Ok(s.clone()),
                                                other => Err(glia::error::type_mismatch(
                                                    "runtime :run :args item",
                                                    "string",
                                                    other,
                                                )),
                                            })
                                            .collect::<Result<Vec<_>, _>>()?;
                                    }
                                }
                            }
                            i += 1;
                        }

                        log::info!(
                            "runtime :run — spawning process ({} bytes, {} env vars)",
                            wasm.len(),
                            env_pairs.len()
                        );

                        // runtime.load(wasm) → Executor (pipelining)
                        let mut load_req = runtime.load_request();
                        load_req.get().set_wasm(&wasm);
                        let executor = load_req.send().pipeline.get_executor();

                        // executor.spawn(args, env) → Process
                        let mut req = executor.spawn_request();
                        {
                            let mut b = req.get();
                            if !spawn_args.is_empty() {
                                let mut arg_list = b.reborrow().init_args(spawn_args.len() as u32);
                                for (j, a) in spawn_args.iter().enumerate() {
                                    arg_list.set(j as u32, a);
                                }
                            }
                            if !env_pairs.is_empty() {
                                let mut env_list = b.init_env(env_pairs.len() as u32);
                                for (j, e) in env_pairs.iter().enumerate() {
                                    env_list.set(j as u32, e);
                                }
                            }
                        }
                        let resp = req
                            .send()
                            .promise
                            .await
                            .map_err(|e| Val::from(e.to_string()))?;
                        let process = resp
                            .get()
                            .map_err(|e| Val::from(e.to_string()))?
                            .get_process()
                            .map_err(|e| Val::from(e.to_string()))?;

                        log::info!("runtime :run — process spawned, waiting for exit");
                        let wait_resp = process
                            .wait_request()
                            .send()
                            .promise
                            .await
                            .map_err(|e| Val::from(e.to_string()))?;
                        let exit_code = wait_resp
                            .get()
                            .map_err(|e| Val::from(e.to_string()))?
                            .get_exit_code();
                        log::info!("runtime :run — process exited ({})", exit_code);
                        Val::Int(exit_code as i64)
                    }
                    _ => return Err(Val::from(format!("runtime: unknown method :{method}"))),
                };
                call_resume(resume, result)
            })
        }),
    }
}

/// ProviderSink that collects streamed results into a channel.
struct CollectorSink {
    tx: std::sync::mpsc::Sender<(Vec<u8>, Vec<Vec<u8>>)>,
}

impl routing_capnp::provider_sink::Server for CollectorSink {
    async fn provider(
        self: capnp::capability::Rc<Self>,
        params: routing_capnp::provider_sink::ProviderParams,
    ) -> Result<(), capnp::Error> {
        let reader = params.get()?;
        let info = reader.get_info()?;
        let peer_id = info.get_peer_id()?.to_vec();
        let addrs_reader = info.get_addrs()?;
        let addrs: Vec<Vec<u8>> = (0..addrs_reader.len())
            .filter_map(|i| addrs_reader.get(i).ok().map(|a| a.to_vec()))
            .collect();
        let _ = self.tx.send((peer_id, addrs));
        Ok(())
    }

    async fn done(
        self: capnp::capability::Rc<Self>,
        _params: routing_capnp::provider_sink::DoneParams,
        _results: routing_capnp::provider_sink::DoneResults,
    ) -> Result<(), capnp::Error> {
        Ok(())
    }
}

/// Hash a name to a CID via routing.hash() RPC.
async fn routing_hash(routing: &routing_capnp::routing::Client, name: &str) -> Result<String, Val> {
    let mut req = routing.hash_request();
    req.get().set_data(name.as_bytes());
    let resp = req
        .send()
        .promise
        .await
        .map_err(|e| Val::from(e.to_string()))?;
    resp.get()
        .map_err(|e| Val::from(e.to_string()))?
        .get_key()
        .map_err(|e| Val::from(e.to_string()))?
        .to_str()
        .map(|s| s.to_string())
        .map_err(|e| Val::from(e.to_string()))
}

fn make_routing_handler(routing: routing_capnp::routing::Client) -> Val {
    Val::AsyncNativeFn {
        name: "routing-handler".into(),
        func: Rc::new(move |args: Vec<Val>| {
            let routing = routing.clone();
            Box::pin(async move {
                let (method, rest) = extract_method(&args[0])?;
                let resume = &args[1];
                let result = match method {
                    "provide" => {
                        let name = match rest.first() {
                            Some(Val::Str(s)) => s.clone(),
                            _ => return Err(Val::from("routing :provide — expected string")),
                        };
                        let cid = routing_hash(&routing, &name).await?;
                        let mut req = routing.provide_request();
                        req.get().set_key(&cid);
                        req.send()
                            .promise
                            .await
                            .map_err(|e| Val::from(e.to_string()))?;
                        Val::Nil
                    }
                    "find" => {
                        let name = match rest.first() {
                            Some(Val::Str(s)) => s.clone(),
                            _ => return Err(Val::from("routing :find — expected string")),
                        };
                        // Parse optional :count keyword.
                        let mut count: u32 = 20;
                        let mut i = 1;
                        while i < rest.len() {
                            if let Val::Keyword(k) = &rest[i] {
                                if k == "count" {
                                    i += 1;
                                    if let Some(Val::Int(n)) = rest.get(i) {
                                        count = if *n <= 0 { u32::MAX } else { *n as u32 };
                                    }
                                }
                            }
                            i += 1;
                        }

                        let cid = routing_hash(&routing, &name).await?;

                        let (tx, rx) = std::sync::mpsc::channel();
                        let sink: routing_capnp::provider_sink::Client =
                            capnp_rpc::new_client(CollectorSink { tx });

                        let mut req = routing.find_providers_request();
                        req.get().set_key(&cid);
                        req.get().set_count(count);
                        req.get().set_sink(sink);
                        req.send()
                            .promise
                            .await
                            .map_err(|e| Val::from(e.to_string()))?;

                        let mut providers = Vec::new();
                        while let Ok((peer_id, addrs)) = rx.try_recv() {
                            let id_str = bs58::encode(&peer_id).into_string();
                            let addr_vals: Vec<Val> = addrs
                                .into_iter()
                                .filter_map(|a| multiaddr::Multiaddr::try_from(a).ok())
                                .map(|m| Val::Str(m.to_string()))
                                .collect();
                            providers.push(Val::Map(glia::ValMap::from_pairs(vec![
                                (Val::Keyword("peer-id".into()), Val::Str(id_str)),
                                (Val::Keyword("addrs".into()), Val::List(addr_vals)),
                            ])));
                        }
                        Val::List(providers)
                    }
                    "hash" => {
                        let data = match rest.first() {
                            Some(Val::Str(s)) => s.as_bytes().to_vec(),
                            Some(Val::Bytes(b)) => b.clone(),
                            _ => return Err(Val::from("routing :hash — expected string or bytes")),
                        };
                        let mut req = routing.hash_request();
                        req.get().set_data(&data);
                        let resp = req
                            .send()
                            .promise
                            .await
                            .map_err(|e| Val::from(e.to_string()))?;
                        let key = resp
                            .get()
                            .map_err(|e| Val::from(e.to_string()))?
                            .get_key()
                            .map_err(|e| Val::from(e.to_string()))?
                            .to_str()
                            .map_err(|e| Val::from(e.to_string()))?;
                        Val::Str(key.to_string())
                    }
                    "resolve" => {
                        let name = match rest.first() {
                            Some(Val::Str(s)) => s.clone(),
                            _ => {
                                return Err(Val::from(
                                    "routing :resolve — expected IPNS name string",
                                ))
                            }
                        };
                        let mut req = routing.resolve_request();
                        req.get().set_name(&name);
                        let resp = req
                            .send()
                            .promise
                            .await
                            .map_err(|e| Val::from(e.to_string()))?;
                        let path = resp
                            .get()
                            .map_err(|e| Val::from(e.to_string()))?
                            .get_path()
                            .map_err(|e| Val::from(e.to_string()))?
                            .to_str()
                            .map_err(|e| Val::from(e.to_string()))?;
                        Val::Str(path.to_string())
                    }
                    _ => return Err(Val::from(format!("routing: unknown method :{method}"))),
                };
                call_resume(resume, result)
            })
        }),
    }
}

async fn eval_path_lookup(cmd: &str, args: &[Val], ctx: &RefCell<Session>) -> Result<Val, Val> {
    // Convert args to strings once — used for whichever candidate we find.
    let str_args: Vec<String> = args
        .iter()
        .map(|v| match v {
            Val::Str(s) | Val::Sym(s) => s.clone(),
            other => format!("{other}"),
        })
        .collect();

    let path_var = std::env::var("PATH").unwrap_or_else(|_| "/bin".to_string());
    for dir in path_var.split(':') {
        // Candidate 1: <dir>/<cmd>.wasm (flat binary)
        // Candidate 2: <dir>/<cmd>/main.wasm (image-style nested)
        let candidates = [
            format!("{dir}/{cmd}.wasm"),
            format!("{dir}/{cmd}/main.wasm"),
        ];
        let bytes = candidates.iter().find_map(|p| std::fs::read(p).ok());
        if let Some(bytes) = bytes {
            // runtime.load(wasm) → Executor (pipelining)
            let mut load_req = ctx.borrow().runtime.load_request();
            load_req.get().set_wasm(&bytes);
            let executor = load_req.send().pipeline.get_executor();

            // executor.spawn(args, env) → Process
            let mut req = executor.spawn_request();
            {
                let b = req.get();
                let mut arg_list = b.init_args(str_args.len() as u32);
                for (i, a) in str_args.iter().enumerate() {
                    arg_list.set(i as u32, a);
                }
            }
            let resp = req
                .send()
                .promise
                .await
                .map_err(|e| Val::from(e.to_string()))?;
            let process = resp
                .get()
                .map_err(|e| Val::from(e.to_string()))?
                .get_process()
                .map_err(|e| Val::from(e.to_string()))?;

            // Read stdout to completion.
            let stdout_resp = process
                .stdout_request()
                .send()
                .promise
                .await
                .map_err(|e| Val::from(e.to_string()))?;
            let stdout_stream = stdout_resp
                .get()
                .map_err(|e| Val::from(e.to_string()))?
                .get_stream()
                .map_err(|e| Val::from(e.to_string()))?;

            let mut output = Vec::new();
            loop {
                let mut req = stdout_stream.read_request();
                req.get().set_max_bytes(65536);
                let resp = req
                    .send()
                    .promise
                    .await
                    .map_err(|e| Val::from(e.to_string()))?;
                let chunk = resp
                    .get()
                    .map_err(|e| Val::from(e.to_string()))?
                    .get_data()
                    .map_err(|e| Val::from(e.to_string()))?;
                if chunk.is_empty() {
                    break;
                }
                output.extend_from_slice(chunk);
            }

            // Wait for exit.
            let wait_resp = process
                .wait_request()
                .send()
                .promise
                .await
                .map_err(|e| Val::from(e.to_string()))?;
            let exit_code = wait_resp
                .get()
                .map_err(|e| Val::from(e.to_string()))?
                .get_exit_code();

            let out_str = String::from_utf8_lossy(&output).trim_end().to_string();
            if exit_code != 0 {
                return Err(Val::from(format!(
                    "{cmd}: exit code {exit_code}\n{out_str}"
                )));
            }
            return Ok(Val::Str(out_str));
        }
    }
    Err(Val::from(format!("{cmd}: command not found")))
}

const HELP_TEXT: &str = "\
Capabilities (via perform):
  (perform host :id)                         Peer ID
  (perform host :addrs)                      Listen addresses
  (perform host :peers)                      Connected peers
  (perform host :listen <cell> \"/path\")     Register HTTP/WAGI cell
  (perform host :listen-stream <cell> \"p\")  Register byte-stream cell
  (perform host :serve-vat cap \"service\")   Register vat capability

  (perform runtime :load <wasm>)             Load wasm, return executor
  (perform runtime :run <wasm> :env {})      Spawn foreground process
  (perform executor :spawn)                  Spawn process
  (perform process :bootstrap)               Get exported cap
  (perform process :wait)                    Wait for process exit
  (perform process :kill)                    Kill process

  (perform routing :provide \"<name>\")        Announce to DHT (hashes internally)
  (perform routing :find \"<name>\" :count N)  Discover providers (default 20)
  (perform routing :hash \"<data>\")           Hash data to CID
  (perform routing :resolve \"<ipns-name>\")   Resolve IPNS name to /ipfs/ path

Effects:
  (perform :load \"<path>\")                   Load bytes from virtual filesystem

Built-ins:
  (load \"<path>\")                Load bytes (dispatch form)
  (cd \"<path>\")                  Change working directory
  (help)                         This message
  (exit)                         Quit

Unrecognized commands are looked up in PATH (default /bin).";

// ---------------------------------------------------------------------------
// Init.d — evaluate scripts from $WW_ROOT/etc/init.d/*.glia
// ---------------------------------------------------------------------------

/// Parse an init.d script from raw bytes. Returns `None` on error (logs details).
/// Extracted from `run_initd` for testability — the caller uses `None` to skip
/// the failed script and continue (SysV best-effort model).
fn parse_initd_script(name: &str, data: &[u8]) -> Option<Vec<Val>> {
    let content = match std::str::from_utf8(data) {
        Ok(s) => s,
        Err(e) => {
            log::error!("init.d: {name}: not valid UTF-8: {e}");
            return None;
        }
    };
    match read_many(content) {
        Ok(forms) => {
            log::info!("init.d: parsed {name} ({} form(s))", forms.len());
            Some(forms)
        }
        Err(e) => {
            log::error!("init.d: {name}: parse error: {e}");
            None
        }
    }
}

/// Wrap a form in cap handlers + keyword effect handlers.
///
/// Produces:
/// ```glia
/// (with-effect-handler host host-handler
///   (with-effect-handler runtime runtime-handler
///     (with-effect-handler routing routing-handler
///       (with-effect-handler :load (fn [path resume] (resume (load path)))
///         <form>))))
/// ```
///
/// Cap handlers are looked up from the environment by name. Keyword effect
/// handlers wrap builtins that use the effect protocol.
fn wrap_with_handlers(form: &Val) -> Val {
    // Innermost: keyword effect handler for :load.
    let with_load = Val::List(vec![
        Val::Sym("with-effect-handler".into()),
        Val::Keyword("load".into()),
        Val::List(vec![
            Val::Sym("fn".into()),
            Val::Vector(vec![Val::Sym("path".into()), Val::Sym("resume".into())]),
            Val::List(vec![
                Val::Sym("resume".into()),
                Val::List(vec![Val::Sym("load".into()), Val::Sym("path".into())]),
            ]),
        ]),
        form.clone(),
    ]);

    // Wrap in cap handlers (innermost to outermost).
    let caps = ["import", "routing", "runtime", "host"];
    let mut wrapped = with_load;
    for cap_name in &caps {
        let handler_name = format!("{cap_name}-handler");
        wrapped = Val::List(vec![
            Val::Sym("with-effect-handler".into()),
            Val::Sym(cap_name.to_string()),
            Val::Sym(handler_name),
            wrapped,
        ]);
    }
    wrapped
}

/// Scan `$WW_ROOT/etc/init.d/*.glia` via the WASI virtual filesystem,
/// parse and evaluate each file as a glia script. Returns true if any
/// expression blocked
/// (i.e. a foreground process ran to completion via `(runtime run ...)`).
async fn run_initd(
    env: &mut Env,
    ctx: &RefCell<Session>,
    dispatch: &HashMap<&'static str, HandlerFn>,
) -> Result<bool, Box<dyn std::error::Error>> {
    let ww_root = std::env::var("WW_ROOT").unwrap_or_default();
    if ww_root.is_empty() {
        log::debug!("init.d: WW_ROOT not set, skipping");
        return Ok(false);
    }
    let root = ww_root.trim_end_matches('/');

    // Read init.d scripts via WASI virtual filesystem.
    // Try $WW_ROOT/etc/init.d first (IPFS CidTree path), then fall back to
    // /etc/init.d (direct WASI preopen for local images).
    let initd_paths = [format!("{root}/etc/init.d"), "/etc/init.d".to_string()];
    let (initd_path, entries) = {
        let mut found = None;
        for path in &initd_paths {
            if let Ok(dir) = std::fs::read_dir(path) {
                let mut names: Vec<String> = dir
                    .filter_map(|entry| {
                        let entry = entry.ok()?;
                        let name = entry.file_name().to_str()?.to_string();
                        if name.ends_with(".glia") {
                            Some(name)
                        } else {
                            None
                        }
                    })
                    .collect();
                names.sort();
                found = Some((path.clone(), names));
                break;
            }
        }
        match found {
            Some(f) => f,
            None => {
                log::warn!(
                    "init.d: not found (tried {} paths), skipping",
                    initd_paths.len()
                );
                return Ok(false);
            }
        }
    };

    if entries.is_empty() {
        log::info!("init.d: no scripts found");
        return Ok(false);
    }

    log::info!("init.d: found {} script(s)", entries.len());
    let mut blocked = false;

    // SysV init: execute each script in lexicographic order, best-effort.
    // On failure: log with full context, continue to next script.
    for name in &entries {
        let script_path = format!("{initd_path}/{name}");

        // Read the glia script via WASI FS — failure skips this script.
        let data = match std::fs::read(&script_path) {
            Ok(d) => d,
            Err(e) => {
                log::error!("init.d: {name}: read failed: {e}");
                continue;
            }
        };

        let forms = match parse_initd_script(name, &data) {
            Some(f) => f,
            None => continue, // SysV: skip failed script
        };

        for (i, form) in forms.iter().enumerate() {
            log::info!("init.d: {name}: evaluating form {}/{}", i + 1, forms.len());
            // Wrap each form in default effect handlers so init.d
            // scripts can use (perform :load ...) etc.
            let wrapped = wrap_with_handlers(form);
            match eval(&wrapped, env, ctx, dispatch).await {
                Ok(Val::Nil) => {}
                Ok(Val::Int(code)) => {
                    // A (runtime run ...) that returned an exit code means
                    // a foreground process ran to completion.
                    log::info!("init.d: {name}: foreground process exited ({code})");
                    blocked = true;
                }
                Ok(result) => {
                    log::debug!("init.d: {name}: {result}");
                }
                Err(e) => {
                    log::error!("init.d: {name}: form {}: {e}", i + 1);
                }
            }
        }
    }

    Ok(blocked)
}

// ---------------------------------------------------------------------------
// Shell mode (TTY)
// ---------------------------------------------------------------------------

fn write_prompt(stdout: &wasip2::io::streams::OutputStream, cwd: &str) {
    let prompt = format!("{} ❯ ", cwd);
    let _ = stdout.blocking_write_and_flush(prompt.as_bytes());
}

async fn run_shell(
    env: &mut Env,
    ctx: RefCell<Session>,
    dispatch: &HashMap<&'static str, HandlerFn>,
) -> Result<(), Box<dyn std::error::Error>> {
    let stdin = get_stdin();
    let stdout = get_stdout();
    let stderr = get_stderr();

    write_prompt(&stdout, &ctx.borrow().cwd);
    let mut buf: Vec<u8> = Vec::new();

    'outer: loop {
        match stdin.blocking_read(4096) {
            Ok(b) if b.is_empty() => break 'outer,
            Ok(b) => buf.extend_from_slice(&b),
            Err(_) => break 'outer,
        }

        while let Some(pos) = buf.iter().position(|&b| b == b'\n') {
            let line_bytes = buf.drain(..=pos).collect::<Vec<_>>();
            let line = match std::str::from_utf8(&line_bytes) {
                Ok(s) => s.trim(),
                Err(_) => {
                    write_prompt(&stdout, &ctx.borrow().cwd);
                    continue;
                }
            };

            if line.is_empty() {
                write_prompt(&stdout, &ctx.borrow().cwd);
                continue;
            }

            match read(line) {
                Ok(expr) => {
                    let wrapped = wrap_with_handlers(&expr);
                    match eval(&wrapped, env, &ctx, dispatch).await {
                        Ok(Val::Nil) => {}
                        Ok(result) => {
                            let _ =
                                stdout.blocking_write_and_flush(format!("{result}\n").as_bytes());
                        }
                        Err(e) => {
                            // Unhandled (throw ...) arrives as
                            // Val::Effect{effect_type:"glia.exception",..};
                            // peel so the structured error fields are visible.
                            let inner = glia::error::unwrap_thrown(&e).unwrap_or(&e);
                            let msg = glia::error::message(inner)
                                .map(str::to_string)
                                .unwrap_or_else(|| format!("{inner}"));
                            let line = match glia::error::type_tag(inner) {
                                Some(tag) => format!("error: [{tag}] {msg}\n"),
                                None => format!("error: {msg}\n"),
                            };
                            let _ = stderr.blocking_write_and_flush(line.as_bytes());
                        }
                    }
                }
                Err(e) => {
                    let _ =
                        stderr.blocking_write_and_flush(format!("parse error: {e}\n").as_bytes());
                }
            }

            write_prompt(&stdout, &ctx.borrow().cwd);
        }
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// Daemon mode (non-TTY) — keep alive until host terminates
// ---------------------------------------------------------------------------

async fn run_daemon() -> Result<(), Box<dyn std::error::Error>> {
    // Yield forever. The host keeps us alive by holding our process handle
    // and terminates us on shutdown (SIGTERM → close pipe → store drop).
    // Unlike blocking_read(), this doesn't pin a worker thread doing nothing.
    std::future::pending::<()>().await;
    Ok(())
}

// ---------------------------------------------------------------------------
// Graft helpers: name-based lookup in parallel lists
// ---------------------------------------------------------------------------

/// Look up a typed capability by name from the graft caps list.
fn get_graft_cap<T: capnp::capability::FromClientHook>(
    caps: &capnp::struct_list::Reader<'_, membrane_capnp::export::Owned>,
    name: &str,
) -> Result<T, capnp::Error> {
    for i in 0..caps.len() {
        let entry = caps.get(i);
        let n = entry
            .get_name()?
            .to_str()
            .map_err(|e| capnp::Error::failed(e.to_string()))?;
        if n == name {
            let invokable = entry.get_synapse()?.get_invokable()?;
            return Ok(T::new(invokable.client.hook));
        }
    }
    Err(capnp::Error::failed(format!(
        "capability '{name}' not found in graft response"
    )))
}

// ---------------------------------------------------------------------------
// Introspection builtins: (schema cap), (doc cap), (help cap)
// ---------------------------------------------------------------------------
//
// `(schema cap)` returns the canonical Schema.Node bytes for a grafted
// capability, sourced from the build-time registry baked into the kernel
// (see std/kernel/build.rs). `(doc cap)` returns a human-readable summary
// of the cap. `(help cap)` is a friendly alias that includes the
// canonical name, schema CID, method count, and inline notes.
//
// All three are pure data lookups — no RPC, no effects. They take a
// `Val::Cap` and return immediately. Errors are produced via the
// `glia::error::*` constructors so MCP / try forms can route on
// `:glia.error/type`.

/// Map a canonical cap name to its build-time schema bytes. Returns
/// `None` for unknown names so callers can produce a structured error.
fn schema_bytes_for_cap(name: &str) -> Option<&'static [u8]> {
    match name {
        "host" => Some(schema_ids::HOST_SCHEMA),
        "runtime" => Some(schema_ids::RUNTIME_SCHEMA),
        "routing" => Some(schema_ids::ROUTING_SCHEMA),
        "identity" => Some(schema_ids::IDENTITY_SCHEMA),
        // The cap is grafted under the name "http"; its schema is for
        // the HttpClient interface.
        "http" | "http-client" => Some(schema_ids::HTTP_CLIENT_SCHEMA),
        _ => None,
    }
}

/// Single-arg helper used by all three introspection builtins. Validates
/// arity and the argument type, returning the inner cap fields on
/// success. Builds structured errors on failure.
fn unwrap_cap_arg<'a>(
    builtin: &'static str,
    args: &'a [Val],
) -> Result<(&'a str, &'a str, &'a Rc<dyn std::any::Any>), Val> {
    if args.len() != 1 {
        return Err(glia::error::arity(builtin, "1", args.len()));
    }
    match &args[0] {
        Val::Cap {
            name,
            schema_cid,
            inner,
            ..
        } => Ok((name.as_str(), schema_cid.as_str(), inner)),
        other => Err(glia::error::type_mismatch(builtin, "cap", other)),
    }
}

fn make_schema_builtin() -> Val {
    Val::NativeFn {
        name: "schema".into(),
        func: Rc::new(|args: &[Val]| -> Result<Val, Val> {
            let (cap_name, _schema_cid, inner) = unwrap_cap_arg("schema", args)?;
            match schema_bytes_for_cap(cap_name) {
                Some(bytes) => Ok(Val::Bytes(bytes.to_vec())),
                None => {
                    if let Some(glia_cap) = inner.downcast_ref::<GliaCapInner>() {
                        return Ok(Val::Bytes(glia_cap.descriptor.clone()));
                    }
                    if let Some(att) = inner.downcast_ref::<AttenuatedCapInner>() {
                        return Ok(Val::Bytes(att.descriptor.clone()));
                    }
                    Err(glia::error::permission_denied(
                        &format!("schema for cap '{cap_name}' not registered"),
                        Some("schemas registered for: host, runtime, routing, identity, http"),
                    ))
                }
            }
        }),
    }
}

fn make_doc_builtin() -> Val {
    Val::NativeFn {
        name: "doc".into(),
        func: Rc::new(|args: &[Val]| -> Result<Val, Val> {
            let (cap_name, schema_cid, inner) = unwrap_cap_arg("doc", args)?;
            // Human-readable summary. `(schema cap)` is the source of
            // truth for machine-readable interface introspection; `doc`
            // is the operator-friendly view.
            let summary = match cap_name {
                "host" => "host — node identity, listeners, peer management",
                "runtime" => "runtime — cell spawn + execution",
                "routing" => "routing — DHT content routing (provide / find)",
                "identity" => "identity — node Ed25519 signing keys",
                "http" | "http-client" => "http-client — outbound HTTP requests (gated by --http-dial)",
                _ => {
                    if let Some(glia_cap) = inner.downcast_ref::<GliaCapInner>() {
                        return Ok(Val::Str(format!(
                            "glia capability — local method table\n  cap-name:   {cap_name}\n  schema-cid: {schema_cid}\n  methods:    {}",
                            glia_cap.methods.len()
                        )));
                    }
                    if let Some(att) = inner.downcast_ref::<AttenuatedCapInner>() {
                        return Ok(Val::Str(format!(
                            "attenuated capability — method whitelist\n  cap-name:   {cap_name}\n  schema-cid: {schema_cid}\n  methods:    {}",
                            att.allow_methods.len()
                        )));
                    }
                    return Err(glia::error::permission_denied(
                        &format!("docs for cap '{cap_name}' not available"),
                        None,
                    ));
                }
            };
            Ok(Val::Str(format!(
                "{summary}\n  cap-name:   {cap_name}\n  schema-cid: {schema_cid}"
            )))
        }),
    }
}

fn make_help_builtin() -> Val {
    Val::NativeFn {
        name: "help".into(),
        func: Rc::new(|args: &[Val]| -> Result<Val, Val> {
            let (cap_name, schema_cid, inner) = unwrap_cap_arg("help", args)?;
            let mut text = String::new();
            text.push_str(&format!("== {cap_name} ==\n"));
            text.push_str(&format!("schema-cid: {schema_cid}\n"));
            if let Some(bytes) = schema_bytes_for_cap(cap_name) {
                text.push_str(&format!(
                    "schema-bytes: {} (call (schema {cap_name}) to retrieve)\n",
                    bytes.len()
                ));
            } else if let Some(glia_cap) = inner.downcast_ref::<GliaCapInner>() {
                text.push_str(&format!(
                    "schema-bytes: {} (derived glia descriptor)\n",
                    glia_cap.descriptor.len()
                ));
            } else if let Some(att) = inner.downcast_ref::<AttenuatedCapInner>() {
                text.push_str(&format!(
                    "schema-bytes: {} (attenuation descriptor)\n",
                    att.descriptor.len()
                ));
            } else {
                text.push_str("schema-bytes: not registered\n");
            }
            text.push_str("usage:        (perform ");
            text.push_str(cap_name);
            text.push_str(" :<method> <args>...)\n");
            text.push_str("introspect:   (schema ");
            text.push_str(cap_name);
            text.push_str(") | (doc ");
            text.push_str(cap_name);
            text.push_str(")\n");
            Ok(Val::Str(text))
        }),
    }
}

// ---------------------------------------------------------------------------
// Entry point
// ---------------------------------------------------------------------------

struct Kernel;

impl Guest for Kernel {
    fn run() -> Result<(), ()> {
        run_impl();
        Ok(())
    }
}

fn run_impl() {
    init_logging();

    let exported_membrane: Rc<RefCell<Option<Membrane>>> = Rc::new(RefCell::new(None));
    let bootstrap: membrane_capnp::membrane::Client = capnp_rpc::new_client(KernelBootstrap {
        membrane: Rc::clone(&exported_membrane),
    });

    system::serve(bootstrap.client, move |membrane: Membrane| {
        let exported_membrane = Rc::clone(&exported_membrane);
        async move {
        *exported_membrane.borrow_mut() = Some(membrane.clone());
        let graft_resp = membrane.graft_request().send().promise.await?;
        let results = graft_resp.get()?;

        // Iterate the caps list to find capabilities by name.
        let caps = results.get_caps()?;

        let host: system_capnp::host::Client = get_graft_cap(&caps, "host")?;
        let runtime: system_capnp::runtime::Client = get_graft_cap(&caps, "runtime")?;
        let routing: routing_capnp::routing::Client = get_graft_cap(&caps, "routing")?;
        let identity: auth_capnp::identity::Client = get_graft_cap(&caps, "identity")?;
        let http_client: Option<http_capnp::http_client::Client> =
            get_graft_cap(&caps, "http-client").ok();

        let ctx = RefCell::new(Session {
            host: host.clone(),
            runtime: runtime.clone(),
            routing: routing.clone(),
            identity,
            http_client: http_client.clone(),
            cwd: "/".to_string(),
        });

        let dispatch = build_dispatch();
        let mut env = Env::new();

        // Bind graft caps + effect handlers from the membrane response.
        // The membrane exports a flat list of named capabilities; we iterate
        // it, downcast each to its typed client, and bind both a Val::Cap
        // (for collect_caps / :listen forwarding) and an effect handler
        // (for `(perform cap :method ...)` in Glia).
        {
            let s = ctx.borrow();
            for i in 0..caps.len() {
                let entry = caps.get(i);
                let cap_name = entry
                    .get_name()?
                    .to_str()
                    .map_err(|e| capnp::Error::failed(e.to_string()))?;

                let (schema_cid, inner, handler): (&str, Rc<dyn std::any::Any>, Val) =
                    match cap_name {
                        "host" => (
                            schema_ids::HOST_CID,
                            Rc::new(s.host.clone()),
                            make_host_handler(
                                s.host.clone(),
                                s.runtime.clone(),
                                s.http_client.clone(),
                            ),
                        ),
                        "runtime" => (
                            schema_ids::RUNTIME_CID,
                            Rc::new(s.runtime.clone()),
                            make_runtime_handler(s.runtime.clone()),
                        ),
                        "routing" => (
                            schema_ids::ROUTING_CID,
                            Rc::new(s.routing.clone()),
                            make_routing_handler(s.routing.clone()),
                        ),
                        "identity" => {
                            // Identity is stored in the Session but has no
                            // Glia effect handler — skip env binding.
                            continue;
                        }
                        "http-client" => {
                            match s.http_client.clone() {
                                Some(c) => (
                                    schema_ids::HTTP_CLIENT_CID,
                                    Rc::new(c),
                                    // No standalone handler — http-client is accessed
                                    // via (perform host :http-client).
                                    Val::Nil,
                                ),
                                None => {
                                    log::warn!("graft: host sent 'http-client' but Session has None, skipping");
                                    continue;
                                }
                            }
                        }
                        other => {
                            log::warn!("graft: unknown cap '{other}', skipping");
                            continue;
                        }
                    };

                env.set(
                    cap_name.to_string(),
                    make_cap(cap_name, schema_cid.to_string(), inner),
                );
                if !matches!(handler, Val::Nil) {
                    env.set(format!("{cap_name}-handler"), handler);
                }
            }

            // Introspection builtins. `(schema cap)` returns the cap's
            // canonical Schema.Node bytes; `(doc cap)` returns a human-
            // readable summary. Bytes come from the build-time schema
            // registry baked into the kernel (see std/kernel/build.rs).
            env.set("schema".to_string(), make_schema_builtin());
            env.set("doc".to_string(), make_doc_builtin());
            env.set("help".to_string(), make_help_builtin());
            env.set("import".to_string(), make_import_cap());
            env.set("import-handler".to_string(), make_import_handler());
        }

        // Load the prelude (standard macros: when, and, or, defn, cond, not).
        {
            let mut kd = KernelDispatch {
                ctx: &ctx,
                table: &dispatch,
            };
            glia::load_prelude(&mut env, &mut kd).await;
        }

        // Run init.d scripts first. If a foreground process blocked
        // (e.g. `(runtime run ...)` in the script), we're done.
        let blocked = run_initd(&mut env, &ctx, &dispatch)
            .await
            .unwrap_or_else(|e| {
                log::error!("init.d: {e}");
                false
            });

        if !blocked {
            let is_tty = std::env::var("WW_TTY").is_ok();
            let result = if is_tty {
                run_shell(&mut env, ctx, &dispatch).await
            } else {
                run_daemon().await
            };

            if let Err(e) = result {
                log::error!("kernel error: {e}");
            }
        }

            Ok(())
        }
    });
}

wasip2::cli::command::export!(Kernel);

#[cfg(test)]
mod tests {
    use super::*;

    // --- init.d parse + SysV error recovery ---

    #[test]
    fn parse_initd_script_valid() {
        let data = b"(cd \"/foo\") (cd \"/bar\")";
        let forms = parse_initd_script("test.glia", data).unwrap();
        assert_eq!(forms.len(), 2);
    }

    #[test]
    fn parse_initd_script_malformed() {
        let data = b"(cd \"/foo\") (broken";
        assert!(parse_initd_script("bad.glia", data).is_none());
    }

    #[test]
    fn parse_initd_script_invalid_utf8() {
        assert!(parse_initd_script("binary.glia", &[0xFF, 0xFE]).is_none());
    }

    #[test]
    fn parse_initd_script_empty() {
        let forms = parse_initd_script("empty.glia", b"").unwrap();
        assert!(forms.is_empty());
    }

    #[test]
    fn parse_initd_script_comments_only() {
        let data = b"; just a comment\n; another one\n";
        let forms = parse_initd_script("comments.glia", data).unwrap();
        assert!(forms.is_empty());
    }

    #[test]
    fn sysv_continues_past_failed_scripts() {
        // SysV contract: each script is processed independently.
        // parse_initd_script returns None on failure, enabling the caller
        // to `continue` to the next script.
        let scripts: Vec<(&str, &[u8])> = vec![
            ("01-bad.glia", &[0xFF, 0xFE]),           // invalid UTF-8
            ("02-broken.glia", b"(unclosed"),         // parse error
            ("03-good.glia", b"(cd \"/ok\")"),        // valid
            ("04-also-bad.glia", b"(a) )unexpected"), // parse error
            ("05-also-good.glia", b"(help)"),         // valid
        ];

        let results: Vec<Option<Vec<Val>>> = scripts
            .iter()
            .map(|(name, data)| parse_initd_script(name, data))
            .collect();

        assert!(results[0].is_none(), "invalid UTF-8 should fail");
        assert!(results[1].is_none(), "unclosed paren should fail");
        assert_eq!(
            results[2].as_ref().unwrap().len(),
            1,
            "valid script should parse"
        );
        assert!(results[3].is_none(), "unexpected close should fail");
        assert_eq!(
            results[4].as_ref().unwrap().len(),
            1,
            "valid script should parse"
        );
    }

    // --- load ---

    #[test]
    fn eval_load_missing_file_returns_error() {
        let result = eval_load(&[Val::Str("/nonexistent/path.wasm".into())]);
        assert!(result.is_err());
        let msg = format!("{}", result.unwrap_err());
        assert!(msg.contains("No such file"), "got: {msg}");
    }

    #[test]
    fn eval_load_missing_arg_returns_error() {
        assert!(eval_load(&[]).is_err());
        assert!(eval_load(&[Val::Int(42)]).is_err());
    }

    #[test]
    fn eval_load_relative_path_prepends_slash() {
        // A relative path like "bin/foo.wasm" should resolve to "/bin/foo.wasm".
        let result = eval_load(&[Val::Str("nonexistent/relative.wasm".into())]);
        assert!(result.is_err());
        let msg = format!("{}", result.unwrap_err());
        assert!(
            msg.contains("/nonexistent/relative.wasm"),
            "expected resolved path with leading /, got: {msg}"
        );
    }

    #[test]
    fn eval_load_caches_repeated_reads() {
        let dir = tempfile::tempdir().unwrap();
        let file_path = dir.path().join("cached.bin");
        std::fs::write(&file_path, b"cached-bytes").unwrap();

        let abs = file_path.to_str().unwrap().to_string();
        let first = eval_load(&[Val::Str(abs.clone())]);
        assert_eq!(first.unwrap(), Val::Bytes(b"cached-bytes".to_vec()));

        // Mutate the file on disk — cached result should still return old bytes.
        std::fs::write(&file_path, b"new-bytes").unwrap();
        let second = eval_load(&[Val::Str(abs)]);
        assert_eq!(second.unwrap(), Val::Bytes(b"cached-bytes".to_vec()));
    }

    // --- wrap_with_handlers ---

    #[test]
    fn wrap_with_handlers_nests_effect_handlers() {
        let form = Val::Sym("body".into());
        let wrapped = wrap_with_handlers(&form);
        // Outermost should be (with-effect-handler host host-handler ...)
        if let Val::List(items) = &wrapped {
            assert_eq!(items[0], Val::Sym("with-effect-handler".into()));
            assert_eq!(items[1], Val::Sym("host".into()));
            assert_eq!(items[2], Val::Sym("host-handler".into()));
            // items[3] is (with-effect-handler runtime ...)
            if let Val::List(inner) = &items[3] {
                assert_eq!(inner[0], Val::Sym("with-effect-handler".into()));
                assert_eq!(inner[1], Val::Sym("runtime".into()));
            } else {
                panic!("expected nested effect handler");
            }
        } else {
            panic!("expected List");
        }
    }

    // --- dispatch table ---

    #[test]
    fn dispatch_table_has_builtins() {
        let table = build_dispatch();
        let expected = ["load", "cd", "help", "exit"];
        for verb in &expected {
            assert!(table.contains_key(verb), "missing dispatch entry: {verb}");
        }
        assert_eq!(
            table.len(),
            expected.len(),
            "unexpected extra entries in dispatch table"
        );
    }

    // ===================================================================
    // Integration tests — dispatch handlers against capnp-rpc stub servers
    // ===================================================================

    use capnp::capability::Promise;

    // Fixed test data: a 38-byte multihash peer ID (identity hash of "test-peer").
    // bs58 of these bytes is "12D3KooW..." in real life; here we use a short
    // deterministic value so assertions are stable.
    const STUB_PEER_ID: &[u8] = b"test-peer-id-multihash-bytes-1234";
    // /ip4/127.0.0.1/tcp/4001 as multiaddr bytes
    const STUB_MULTIADDR: &[u8] = &[0x04, 127, 0, 0, 1, 0x06, 0x0f, 0xa1];

    // --- Stub Host: returns fixed peer ID, addrs, peers ---

    struct TestHost;

    #[allow(refining_impl_trait)]
    impl system_capnp::host::Server for TestHost {
        fn id(
            self: capnp::capability::Rc<Self>,
            _params: system_capnp::host::IdParams,
            mut results: system_capnp::host::IdResults,
        ) -> Promise<(), capnp::Error> {
            results.get().set_peer_id(STUB_PEER_ID);
            Promise::ok(())
        }

        fn addrs(
            self: capnp::capability::Rc<Self>,
            _params: system_capnp::host::AddrsParams,
            mut results: system_capnp::host::AddrsResults,
        ) -> Promise<(), capnp::Error> {
            let mut list = results.get().init_addrs(1);
            list.set(0, STUB_MULTIADDR);
            Promise::ok(())
        }

        fn peers(
            self: capnp::capability::Rc<Self>,
            _params: system_capnp::host::PeersParams,
            mut results: system_capnp::host::PeersResults,
        ) -> Promise<(), capnp::Error> {
            let mut list = results.get().init_peers(1);
            {
                let mut peer = list.reborrow().get(0);
                peer.set_peer_id(STUB_PEER_ID);
                let mut addrs = peer.init_addrs(1);
                addrs.set(0, STUB_MULTIADDR);
            }
            Promise::ok(())
        }

        fn network(
            self: capnp::capability::Rc<Self>,
            _params: system_capnp::host::NetworkParams,
            mut results: system_capnp::host::NetworkResults,
        ) -> Promise<(), capnp::Error> {
            let mut r = results.get();
            r.set_stream_listener(capnp_rpc::new_client(TestStreamListener));
            r.set_stream_dialer(capnp_rpc::new_client(TestStreamDialer));
            r.set_vat_listener(capnp_rpc::new_client(TestVatListener));
            r.set_vat_client(capnp_rpc::new_client(TestVatClient));
            Promise::ok(())
        }
    }

    // --- Stub Runtime: load returns a stub Executor ---

    struct TestRuntime;

    #[allow(refining_impl_trait)]
    impl system_capnp::runtime::Server for TestRuntime {
        fn load(
            self: capnp::capability::Rc<Self>,
            _params: system_capnp::runtime::LoadParams,
            mut results: system_capnp::runtime::LoadResults,
        ) -> Promise<(), capnp::Error> {
            results
                .get()
                .set_executor(capnp_rpc::new_client(TestExecutor));
            Promise::ok(())
        }

        fn shutdown(
            self: capnp::capability::Rc<Self>,
            _params: system_capnp::runtime::ShutdownParams,
            _results: system_capnp::runtime::ShutdownResults,
        ) -> Promise<(), capnp::Error> {
            Promise::ok(())
        }
    }

    // --- Stub Executor: spawn returns unimplemented ---

    struct TestExecutor;

    #[allow(refining_impl_trait)]
    impl system_capnp::executor::Server for TestExecutor {
        fn spawn(
            self: capnp::capability::Rc<Self>,
            _params: system_capnp::executor::SpawnParams,
            _results: system_capnp::executor::SpawnResults,
        ) -> Promise<(), capnp::Error> {
            Promise::err(capnp::Error::unimplemented("stub".into()))
        }
    }

    // --- Stub Routing: hash returns fixed CID, provide succeeds, findProviders streams 2 results ---

    struct TestRouting;

    #[allow(refining_impl_trait)]
    impl routing_capnp::routing::Server for TestRouting {
        fn hash(
            self: capnp::capability::Rc<Self>,
            _params: routing_capnp::routing::HashParams,
            mut results: routing_capnp::routing::HashResults,
        ) -> Promise<(), capnp::Error> {
            results.get().set_key("QmTestCid123");
            Promise::ok(())
        }

        fn provide(
            self: capnp::capability::Rc<Self>,
            _params: routing_capnp::routing::ProvideParams,
            _results: routing_capnp::routing::ProvideResults,
        ) -> Promise<(), capnp::Error> {
            Promise::ok(())
        }

        fn resolve(
            self: capnp::capability::Rc<Self>,
            _params: routing_capnp::routing::ResolveParams,
            mut results: routing_capnp::routing::ResolveResults,
        ) -> Promise<(), capnp::Error> {
            results.get().set_path("/ipfs/bafyrei-test-resolved");
            Promise::ok(())
        }

        fn find_providers(
            self: capnp::capability::Rc<Self>,
            params: routing_capnp::routing::FindProvidersParams,
            _results: routing_capnp::routing::FindProvidersResults,
        ) -> Promise<(), capnp::Error> {
            let params = capnp_rpc::pry!(params.get());
            let count = params.get_count();
            let sink = capnp_rpc::pry!(params.get_sink());

            // Stream `min(count, 2)` providers.
            let n = std::cmp::min(count, 2) as usize;
            Promise::from_future(async move {
                for i in 0..n {
                    let mut req = sink.provider_request();
                    {
                        let mut info = req.get().init_info();
                        info.set_peer_id(format!("peer-{i}").as_bytes());
                        let mut addrs = info.init_addrs(1);
                        addrs.set(0, STUB_MULTIADDR);
                    }
                    req.send().await?;
                }
                let done_req = sink.done_request();
                done_req.send().promise.await?;
                Ok(())
            })
        }
    }

    // --- Stub VatListener: asserts cap + protocol are present ---

    struct TestVatListener;

    #[allow(refining_impl_trait)]
    impl system_capnp::vat_listener::Server for TestVatListener {
        fn serve(
            self: capnp::capability::Rc<Self>,
            params: system_capnp::vat_listener::ServeParams,
            _results: system_capnp::vat_listener::ServeResults,
        ) -> Promise<(), capnp::Error> {
            let params = capnp_rpc::pry!(params.get());
            if !params.has_cap() {
                return Promise::err(capnp::Error::failed("cap not set".into()));
            }
            if !params.has_protocol() {
                return Promise::err(capnp::Error::failed("protocol not set".into()));
            }
            Promise::ok(())
        }
    }

    // --- Stub StreamListener: asserts executor is present ---

    struct TestStreamListener;

    #[allow(refining_impl_trait)]
    impl system_capnp::stream_listener::Server for TestStreamListener {
        fn listen(
            self: capnp::capability::Rc<Self>,
            params: system_capnp::stream_listener::ListenParams,
            _results: system_capnp::stream_listener::ListenResults,
        ) -> Promise<(), capnp::Error> {
            let params = capnp_rpc::pry!(params.get());
            if !params.has_executor() {
                return Promise::err(capnp::Error::failed("executor not set".into()));
            }
            if !params.has_protocol() {
                return Promise::err(capnp::Error::failed("protocol not set".into()));
            }
            Promise::ok(())
        }
    }

    // --- Stub StreamDialer + VatClient (unused, just satisfy network result) ---

    struct TestStreamDialer;
    impl system_capnp::stream_dialer::Server for TestStreamDialer {}

    struct TestVatClient;
    impl system_capnp::vat_client::Server for TestVatClient {}

    // --- Stub Identity (unimplemented — not under test) ---

    struct TestIdentity;

    #[allow(refining_impl_trait)]
    impl auth_capnp::identity::Server for TestIdentity {
        fn signer(
            self: capnp::capability::Rc<Self>,
            _p: auth_capnp::identity::SignerParams,
            _r: auth_capnp::identity::SignerResults,
        ) -> Promise<(), capnp::Error> {
            Promise::err(capnp::Error::unimplemented("stub".into()))
        }

        fn verify(
            self: capnp::capability::Rc<Self>,
            _p: auth_capnp::identity::VerifyParams,
            _r: auth_capnp::identity::VerifyResults,
        ) -> Promise<(), capnp::Error> {
            Promise::err(capnp::Error::unimplemented("stub".into()))
        }
    }

    // --- Stub Membrane: returns fixed graft caps ---

    struct TestMembrane {
        runtime: system_capnp::runtime::Client,
    }

    #[allow(refining_impl_trait)]
    impl membrane_capnp::membrane::Server for TestMembrane {
        fn graft(
            self: capnp::capability::Rc<Self>,
            _params: membrane_capnp::membrane::GraftParams,
            mut results: membrane_capnp::membrane::GraftResults,
        ) -> Promise<(), capnp::Error> {
            let mut caps = results.get().init_caps(1);
            let mut entry = caps.reborrow().get(0);
            entry.set_name("runtime");
            write_synapse_from_client(
                entry.init_synapse(),
                "runtime",
                self.runtime.client.clone(),
            );
            Promise::ok(())
        }
    }

    // --- Helper: construct a Session with test stubs ---

    struct TestHttpClient;

    #[allow(refining_impl_trait)]
    impl http_capnp::http_client::Server for TestHttpClient {
        fn get(
            self: capnp::capability::Rc<Self>,
            _p: http_capnp::http_client::GetParams,
            _r: http_capnp::http_client::GetResults,
        ) -> Promise<(), capnp::Error> {
            Promise::err(capnp::Error::unimplemented("stub".into()))
        }
    }

    fn test_session() -> Session {
        Session {
            host: capnp_rpc::new_client(TestHost),
            runtime: capnp_rpc::new_client(TestRuntime),
            routing: capnp_rpc::new_client(TestRouting),
            identity: capnp_rpc::new_client(TestIdentity),
            http_client: Some(capnp_rpc::new_client(TestHttpClient)),
            cwd: "/".into(),
        }
    }

    /// Run an async block on a single-threaded tokio + capnp-rpc LocalSet.
    async fn run_local<F, T>(f: F) -> T
    where
        F: Future<Output = T>,
    {
        tokio::task::LocalSet::new().run_until(f).await
    }

    /// Call an AsyncNativeFn handler with a method keyword + rest args.
    /// Provides a resume function and extracts the resumed value.
    /// Returns Ok(resumed_value) or the handler's Err.
    async fn call_handler(handler: &Val, method: &str, rest: &[Val]) -> Result<Val, Val> {
        let func = match handler {
            Val::AsyncNativeFn { func, .. } => func.clone(),
            _ => panic!("expected AsyncNativeFn"),
        };
        let mut data_items = vec![Val::Keyword(method.into())];
        data_items.extend_from_slice(rest);
        let data = Val::List(data_items);

        // Create a resume function that captures the value.
        let captured: Rc<RefCell<Option<Val>>> = Rc::new(RefCell::new(None));
        let cap = captured.clone();
        let resume = Val::NativeFn {
            name: "test-resume".into(),
            func: Rc::new(move |args: &[Val]| {
                *cap.borrow_mut() = Some(args[0].clone());
                Err(Val::Resume(Box::new(args[0].clone())))
            }),
        };

        match func(vec![data, resume]).await {
            Err(Val::Resume(_)) => {
                // Handler called resume — extract the value.
                Ok(captured.borrow().clone().unwrap())
            }
            Err(e) => Err(e),
            Ok(v) => Ok(v), // Handler returned directly without resume.
        }
    }

    // --- kernel bootstrap tests ---

    #[tokio::test]
    async fn test_kernel_bootstrap_forwards_membrane_graft() {
        run_local(async {
            let runtime: system_capnp::runtime::Client = capnp_rpc::new_client(TestRuntime);
            let upstream: Membrane = capnp_rpc::new_client(TestMembrane {
                runtime: runtime.clone(),
            });
            let state: Rc<RefCell<Option<Membrane>>> = Rc::new(RefCell::new(Some(upstream)));

            let bootstrap: Membrane = capnp_rpc::new_client(KernelBootstrap { membrane: state });
            let resp = bootstrap
                .graft_request()
                .send()
                .promise
                .await
                .expect("bootstrap graft should succeed");
            let caps = resp.get().unwrap().get_caps().unwrap();

            let forwarded_runtime: system_capnp::runtime::Client =
                get_graft_cap(&caps, "runtime").expect("runtime cap should be forwarded");
            let load_resp = forwarded_runtime
                .load_request()
                .send()
                .promise
                .await
                .expect("forwarded runtime should be callable");
            assert!(load_resp.get().unwrap().has_executor());
        })
        .await;
    }

    #[tokio::test]
    async fn test_kernel_bootstrap_errors_when_membrane_not_ready() {
        run_local(async {
            let state: Rc<RefCell<Option<Membrane>>> = Rc::new(RefCell::new(None));
            let bootstrap: Membrane = capnp_rpc::new_client(KernelBootstrap { membrane: state });

            match bootstrap.graft_request().send().promise.await {
                Ok(_) => panic!("bootstrap graft should fail before membrane is ready"),
                Err(err) => {
                    assert!(
                        format!("{err}").contains("not ready"),
                        "unexpected error: {err}"
                    );
                }
            }
        })
        .await;
    }

    // --- host tests ---

    #[tokio::test]
    async fn test_host_id_returns_bs58() {
        run_local(async {
            let s = test_session();
            let handler =
                make_host_handler(s.host.clone(), s.runtime.clone(), s.http_client.clone());
            let result = call_handler(&handler, "id", &[]).await.unwrap();
            let expected = bs58::encode(STUB_PEER_ID).into_string();
            assert_eq!(result, Val::Str(expected));
        })
        .await;
    }

    #[tokio::test]
    async fn test_host_addrs_returns_multiaddr_strings() {
        run_local(async {
            let s = test_session();
            let handler =
                make_host_handler(s.host.clone(), s.runtime.clone(), s.http_client.clone());
            let result = call_handler(&handler, "addrs", &[]).await.unwrap();
            match result {
                Val::List(addrs) => {
                    assert_eq!(addrs.len(), 1);
                    assert_eq!(addrs[0], Val::Str("/ip4/127.0.0.1/tcp/4001".into()));
                }
                other => panic!("expected list, got {other:?}"),
            }
        })
        .await;
    }

    #[tokio::test]
    async fn test_host_peers_returns_map_format() {
        run_local(async {
            let s = test_session();
            let handler =
                make_host_handler(s.host.clone(), s.runtime.clone(), s.http_client.clone());
            let result = call_handler(&handler, "peers", &[]).await.unwrap();
            match result {
                Val::List(peers) => {
                    assert_eq!(peers.len(), 1);
                    match &peers[0] {
                        Val::Map(entries) => {
                            assert_eq!(entries.len(), 2);
                            let expected_id = bs58::encode(STUB_PEER_ID).into_string();
                            assert_eq!(
                                entries.get(&Val::Keyword("peer-id".into())),
                                Some(&Val::Str(expected_id))
                            );
                            assert!(entries.get(&Val::Keyword("addrs".into())).is_some());
                        }
                        other => panic!("expected map, got {other:?}"),
                    }
                }
                other => panic!("expected list, got {other:?}"),
            }
        })
        .await;
    }

    #[tokio::test]
    async fn test_host_unknown_method_returns_error() {
        run_local(async {
            let s = test_session();
            let handler =
                make_host_handler(s.host.clone(), s.runtime.clone(), s.http_client.clone());
            let err = call_handler(&handler, "bogus", &[]).await.unwrap_err();
            let msg = format!("{err}");
            assert!(msg.contains("unknown method"), "got: {msg}");
        })
        .await;
    }

    // --- host listen / serve tests ---

    fn test_cell() -> Val {
        Val::Cell {
            wasm: b"fake-wasm".to_vec(),
            caps: vec![],
        }
    }

    #[tokio::test]
    async fn test_host_listen_http_cell_succeeds() {
        run_local(async {
            let s = test_session();
            let handler =
                make_host_handler(s.host.clone(), s.runtime.clone(), s.http_client.clone());
            let result =
                call_handler(&handler, "listen", &[test_cell(), Val::Str("/demo".into())]).await;
            assert!(result.is_ok(), "HTTP listen failed: {:?}", result.err());
        })
        .await;
    }

    #[tokio::test]
    async fn test_host_listen_vat_cell_removed_errors() {
        run_local(async {
            let s = test_session();
            let handler =
                make_host_handler(s.host.clone(), s.runtime.clone(), s.http_client.clone());
            let err = call_handler(&handler, "listen", &[test_cell()])
                .await
                .unwrap_err();
            let msg = format!("{err}");
            assert!(msg.contains("vat cell listen was removed"), "got: {msg}");
        })
        .await;
    }

    #[tokio::test]
    async fn test_host_listen_stream_cell_succeeds() {
        run_local(async {
            let s = test_session();
            let handler =
                make_host_handler(s.host.clone(), s.runtime.clone(), s.http_client.clone());
            let result = call_handler(
                &handler,
                "listen-stream",
                &[test_cell(), Val::Str("my-protocol".into())],
            )
            .await;
            assert!(result.is_ok(), "StreamListener listen failed: {:?}", result.err());
        })
        .await;
    }

    #[tokio::test]
    async fn test_host_serve_vat_cap_succeeds() {
        run_local(async {
            let s = test_session();
            let handler =
                make_host_handler(s.host.clone(), s.runtime.clone(), s.http_client.clone());
            let cap = make_cap("host", "test-host-cid", Rc::new(s.host.clone()));
            let result =
                call_handler(&handler, "serve-vat", &[cap, Val::Str("greeter".into())]).await;
            assert!(result.is_ok(), "VatListener serve failed: {:?}", result.err());
        })
        .await;
    }

    #[tokio::test]
    async fn test_host_serve_vat_non_capnp_cap_errors() {
        run_local(async {
            let s = test_session();
            let handler =
                make_host_handler(s.host.clone(), s.runtime.clone(), s.http_client.clone());
            let cap = make_cap("not-capnp", "test-cid", Rc::new(42i32));
            let err = call_handler(&handler, "serve-vat", &[cap, Val::Str("greeter".into())])
                .await
                .unwrap_err();
            let msg = format!("{err}");
            assert!(msg.contains("not backed by a Cap'n Proto client"), "got: {msg}");
        })
        .await;
    }

    #[tokio::test]
    async fn test_host_listen_and_serve_wrong_arity_returns_error() {
        run_local(async {
            let s = test_session();
            let handler =
                make_host_handler(s.host.clone(), s.runtime.clone(), s.http_client.clone());
            assert!(call_handler(&handler, "listen", &[]).await.is_err());
            assert!(call_handler(&handler, "listen-stream", &[test_cell()]).await.is_err());
            assert!(call_handler(&handler, "serve-vat", &[]).await.is_err());
        })
        .await;
    }

    // --- runtime tests ---

    #[tokio::test]
    async fn test_runtime_unknown_method_returns_error() {
        run_local(async {
            let s = test_session();
            let handler = make_runtime_handler(s.runtime.clone());
            let err = call_handler(&handler, "bogus", &[]).await.unwrap_err();
            let msg = format!("{err}");
            assert!(msg.contains("unknown method"), "got: {msg}");
        })
        .await;
    }

    // --- routing tests ---

    #[tokio::test]
    async fn test_routing_provide_succeeds() {
        run_local(async {
            let s = test_session();
            let handler = make_routing_handler(s.routing.clone());
            let result = call_handler(&handler, "provide", &[Val::Str("oracle".into())])
                .await
                .unwrap();
            assert_eq!(result, Val::Nil);
        })
        .await;
    }

    #[tokio::test]
    async fn test_routing_provide_missing_name() {
        run_local(async {
            let s = test_session();
            let handler = make_routing_handler(s.routing.clone());
            let err = call_handler(&handler, "provide", &[]).await.unwrap_err();
            let msg = format!("{err}");
            assert!(msg.contains("routing :provide"), "got: {msg}");
        })
        .await;
    }

    #[tokio::test]
    async fn test_routing_find_default_count() {
        run_local(async {
            let s = test_session();
            let handler = make_routing_handler(s.routing.clone());
            let result = call_handler(&handler, "find", &[Val::Str("oracle".into())])
                .await
                .unwrap();
            match result {
                Val::List(providers) => {
                    assert_eq!(providers.len(), 2);
                    match &providers[0] {
                        Val::Map(entries) => {
                            let peer_id = entries
                                .get(&Val::Keyword("peer-id".into()))
                                .expect("missing :peer-id key");
                            assert_eq!(
                                *peer_id,
                                Val::Str(bs58::encode(b"peer-0").into_string())
                            );
                        }
                        other => panic!("expected map, got {other:?}"),
                    }
                }
                other => panic!("expected list, got {other:?}"),
            }
        })
        .await;
    }

    #[tokio::test]
    async fn test_routing_find_custom_count() {
        run_local(async {
            let s = test_session();
            let handler = make_routing_handler(s.routing.clone());
            let result = call_handler(
                &handler,
                "find",
                &[
                    Val::Str("oracle".into()),
                    Val::Keyword("count".into()),
                    Val::Int(1),
                ],
            )
            .await
            .unwrap();
            match result {
                Val::List(providers) => assert_eq!(providers.len(), 1),
                other => panic!("expected list, got {other:?}"),
            }
        })
        .await;
    }

    #[tokio::test]
    async fn test_routing_find_zero_count_means_no_limit() {
        run_local(async {
            let s = test_session();
            let handler = make_routing_handler(s.routing.clone());
            let result = call_handler(
                &handler,
                "find",
                &[
                    Val::Str("oracle".into()),
                    Val::Keyword("count".into()),
                    Val::Int(0),
                ],
            )
            .await
            .unwrap();
            match result {
                Val::List(providers) => assert_eq!(providers.len(), 2),
                other => panic!("expected list, got {other:?}"),
            }
        })
        .await;
    }

    #[tokio::test]
    async fn test_routing_find_missing_name() {
        run_local(async {
            let s = test_session();
            let handler = make_routing_handler(s.routing.clone());
            let err = call_handler(&handler, "find", &[]).await.unwrap_err();
            let msg = format!("{err}");
            assert!(msg.contains("routing :find"), "got: {msg}");
        })
        .await;
    }

    #[tokio::test]
    async fn test_routing_hash() {
        run_local(async {
            let s = test_session();
            let handler = make_routing_handler(s.routing.clone());
            let result = call_handler(&handler, "hash", &[Val::Str("test-data".into())])
                .await
                .unwrap();
            assert_eq!(result, Val::Str("QmTestCid123".into()));
        })
        .await;
    }

    #[tokio::test]
    async fn test_routing_unknown_method_returns_error() {
        run_local(async {
            let s = test_session();
            let handler = make_routing_handler(s.routing.clone());
            let err = call_handler(&handler, "bogus", &[]).await.unwrap_err();
            let msg = format!("{err}");
            assert!(msg.contains("unknown method"), "got: {msg}");
        })
        .await;
    }

    #[tokio::test]
    async fn test_routing_resolve_succeeds() {
        run_local(async {
            let s = test_session();
            let handler = make_routing_handler(s.routing.clone());
            let result =
                call_handler(&handler, "resolve", &[Val::Str("/ipns/k51qzi-test".into())])
                    .await
                    .unwrap();
            assert_eq!(result, Val::Str("/ipfs/bafyrei-test-resolved".into()));
        })
        .await;
    }

    #[tokio::test]
    async fn test_routing_resolve_missing_name() {
        run_local(async {
            let s = test_session();
            let handler = make_routing_handler(s.routing.clone());
            let err = call_handler(&handler, "resolve", &[]).await.unwrap_err();
            let msg = format!("{err}");
            assert!(msg.contains("routing :resolve"), "got: {msg}");
        })
        .await;
    }

    // --- perform :load effect round-trip ---

    /// Helper: bind all caps + handlers in env (same as kernel boot).
    fn bind_caps_in_env(env: &mut Env, session: &Session) {
        let caps: [(&str, &str, Rc<dyn std::any::Any>, Val); 3] = [
            (
                "host",
                "test-host-cid",
                Rc::new(session.host.clone()),
                make_host_handler(
                    session.host.clone(),
                    session.runtime.clone(),
                    session.http_client.clone(),
                ),
            ),
            (
                "runtime",
                "test-runtime-cid",
                Rc::new(session.runtime.clone()),
                make_runtime_handler(session.runtime.clone()),
            ),
            (
                "routing",
                "test-routing-cid",
                Rc::new(session.routing.clone()),
                make_routing_handler(session.routing.clone()),
            ),
        ];
        for (name, cid, inner, handler) in caps {
            env.set(
                name.to_string(),
                make_cap(name, cid, inner),
            );
            env.set(format!("{name}-handler"), handler);
        }
        env.set("import".to_string(), make_import_cap());
        env.set("import-handler".to_string(), make_import_handler());

        // http-client with real capnp client (tests always provide one).
        if let Some(ref c) = session.http_client {
            env.set(
                "http-client".to_string(),
                make_cap("http-client", "test-http-cid", Rc::new(c.clone())),
            );
        }
    }

    /// Verify that (perform :load "path") inside wrap_with_handlers
    /// actually resolves through the effect handler → eval_load → filesystem.
    #[tokio::test]
    async fn test_perform_load_resolves_through_effect_handler() {
        run_local(async {
            let ctx = RefCell::new(test_session());
            let dispatch = build_dispatch();
            let mut env = Env::new();
            bind_caps_in_env(&mut env, &ctx.borrow());

            let dir = tempfile::tempdir().unwrap();
            let file_path = dir.path().join("test.bin");
            std::fs::write(&file_path, b"hello-bytes").unwrap();
            std::env::remove_var("WW_ROOT");

            let form = Val::List(vec![
                Val::Sym("perform".into()),
                Val::Keyword("load".into()),
                Val::Str(file_path.to_str().unwrap().to_string()),
            ]);
            let wrapped = wrap_with_handlers(&form);
            let result = eval(&wrapped, &mut env, &ctx, &dispatch).await;
            assert_eq!(result.unwrap(), Val::Bytes(b"hello-bytes".to_vec()));
        })
        .await;
    }

    /// Verify that (perform :load "missing") fails with a clear error.
    #[tokio::test]
    async fn test_perform_load_missing_file_returns_error() {
        run_local(async {
            let ctx = RefCell::new(test_session());
            let dispatch = build_dispatch();
            let mut env = Env::new();
            bind_caps_in_env(&mut env, &ctx.borrow());

            std::env::remove_var("WW_ROOT");

            let form = Val::List(vec![
                Val::Sym("perform".into()),
                Val::Keyword("load".into()),
                Val::Str("/nonexistent/path/missing.wasm".to_string()),
            ]);
            let wrapped = wrap_with_handlers(&form);
            let result = eval(&wrapped, &mut env, &ctx, &dispatch).await;
            assert!(result.is_err(), "expected error for missing file");
        })
        .await;
    }

    // --- init script eval integration ---

    /// Eval an HTTP cell listen form end-to-end.
    #[tokio::test]
    async fn test_glia_http_listen_form_evals_end_to_end() {
        run_local(async {
            let ctx = RefCell::new(test_session());
            let dispatch = build_dispatch();
            let mut env = Env::new();
            bind_caps_in_env(&mut env, &ctx.borrow());

            let dir = tempfile::tempdir().unwrap();
            let wasm_path = dir.path().join("chess-demo.wasm");
            std::fs::write(&wasm_path, b"fake-wasm-bytes").unwrap();
            std::env::remove_var("WW_ROOT");

            let script = format!(
                r#"(perform host :listen (cell (perform :load "{}")) "/demo")"#,
                wasm_path.to_str().unwrap()
            );
            let form = read(&script).unwrap();
            let wrapped = wrap_with_handlers(&form);
            let result = eval(&wrapped, &mut env, &ctx, &dispatch).await;
            assert!(
                result.is_ok(),
                "HTTP listen form failed: {:?}",
                result.unwrap_err()
            );
        })
        .await;
    }

    /// Eval stream listen + runtime run forms through the kernel eval pipeline.
    #[tokio::test]
    async fn test_glia_stream_listen_and_runtime_run_forms_eval() {
        run_local(async {
            let ctx = RefCell::new(test_session());
            let dispatch = build_dispatch();
            let mut env = Env::new();
            bind_caps_in_env(&mut env, &ctx.borrow());

            let dir = tempfile::tempdir().unwrap();
            let wasm_path = dir.path().join("chess-demo.wasm");
            std::fs::write(&wasm_path, b"fake-wasm-bytes").unwrap();
            std::env::remove_var("WW_ROOT");

            let script = format!(
                r#"(perform host :listen-stream (cell (perform :load "{}")) "chess")
                   (perform runtime :run (perform :load "{}"))"#,
                wasm_path.to_str().unwrap(),
                wasm_path.to_str().unwrap()
            );

            let forms = read_many(&script).unwrap();
            assert_eq!(forms.len(), 2, "chess.glia should have 2 forms");

            // First form: stream listen should succeed.
            let wrapped = wrap_with_handlers(&forms[0]);
            let result = eval(&wrapped, &mut env, &ctx, &dispatch).await;
            assert!(
                result.is_ok(),
                "first form failed: {:?}",
                result.unwrap_err()
            );

            // Second form: (perform runtime :run ...) — fails because TestExecutor
            // returns "unimplemented" for spawn. Expected.
            let wrapped = wrap_with_handlers(&forms[1]);
            let result = eval(&wrapped, &mut env, &ctx, &dispatch).await;
            assert!(result.is_err(), "runtime run should fail against stub");
        })
        .await;
    }

    // --- run_initd integration ---

    /// run_initd with no WW_ROOT set returns false (no scripts to run).
    #[tokio::test]
    async fn test_run_initd_no_ww_root_skips() {
        run_local(async {
            let ctx = RefCell::new(test_session());
            let dispatch = build_dispatch();
            let mut env = Env::new();
            std::env::remove_var("WW_ROOT");
            let blocked = run_initd(&mut env, &ctx, &dispatch).await.unwrap();
            assert!(!blocked, "should not block when WW_ROOT is unset");
        })
        .await;
    }

    /// run_initd with empty WW_ROOT skips gracefully.
    #[tokio::test]
    async fn test_run_initd_empty_ww_root_skips() {
        run_local(async {
            let ctx = RefCell::new(test_session());
            let dispatch = build_dispatch();
            let mut env = Env::new();
            std::env::set_var("WW_ROOT", "");
            let blocked = run_initd(&mut env, &ctx, &dispatch).await.unwrap();
            assert!(!blocked, "should not block when WW_ROOT is empty");
        })
        .await;
    }

    // --- extract_capnp_client tests ---

    #[test]
    fn test_extract_capnp_client_known_types() {
        let s = test_session();
        // Host client
        let inner: Rc<dyn std::any::Any> = Rc::new(s.host.clone());
        assert!(
            extract_capnp_client(&inner).is_some(),
            "should extract host client"
        );
        // Runtime client
        let inner: Rc<dyn std::any::Any> = Rc::new(s.runtime.clone());
        assert!(
            extract_capnp_client(&inner).is_some(),
            "should extract runtime client"
        );
        // Routing client
        let inner: Rc<dyn std::any::Any> = Rc::new(s.routing.clone());
        assert!(
            extract_capnp_client(&inner).is_some(),
            "should extract routing client"
        );
        // HttpClient
        let inner: Rc<dyn std::any::Any> = Rc::new(s.http_client.clone().unwrap());
        assert!(
            extract_capnp_client(&inner).is_some(),
            "should extract http_client"
        );
    }

    #[test]
    fn test_extract_capnp_client_unknown_type_returns_none() {
        let inner: Rc<dyn std::any::Any> = Rc::new(42i32);
        assert!(
            extract_capnp_client(&inner).is_none(),
            "unknown type should return None"
        );
    }

    // --- cell-based listen with caps ---

    #[tokio::test]
    async fn test_host_listen_cell_with_caps() {
        run_local(async {
            let s = test_session();
            let handler =
                make_host_handler(s.host.clone(), s.runtime.clone(), s.http_client.clone());
            let http_cap = make_cap(
                "http",
                "test-http-cid",
                Rc::new(s.http_client.clone().unwrap()),
            );
            let cell = Val::Cell {
                wasm: b"fake-wasm".to_vec(),
                caps: vec![("http".to_string(), http_cap)],
            };
            let result =
                call_handler(&handler, "listen", &[cell, Val::Str("/demo".into())]).await;
            assert!(
                result.is_ok(),
                "cell-based listen with caps failed: {:?}",
                result.unwrap_err()
            );
        })
        .await;
    }

    #[tokio::test]
    async fn test_host_listen_cell_without_caps() {
        run_local(async {
            let s = test_session();
            let handler =
                make_host_handler(s.host.clone(), s.runtime.clone(), s.http_client.clone());
            let cell = Val::Cell {
                wasm: b"fake-wasm".to_vec(),
                caps: vec![],
            };
            let result =
                call_handler(&handler, "listen", &[cell, Val::Str("/demo".into())]).await;
            assert!(
                result.is_ok(),
                "cell-based listen without caps failed: {:?}",
                result.unwrap_err()
            );
        })
        .await;
    }

    // --- Introspection builtins: (schema cap), (doc cap), (help cap) ---

    /// Construct a cap-shaped Val for tests. The `inner` is a placeholder
    /// since the introspection builtins only read `name` and `schema_cid`.
    fn test_cap(name: &str, schema_cid: &str) -> Val {
        make_cap(name, schema_cid, Rc::new(()))
    }

    /// Invoke a builtin (NativeFn) with the given args. Panics if the
    /// builtin is the wrong variant.
    fn call_builtin(builtin: &Val, args: &[Val]) -> Result<Val, Val> {
        match builtin {
            Val::NativeFn { func, .. } => func(args),
            other => panic!("expected NativeFn, got {other:?}"),
        }
    }

    #[test]
    fn schema_returns_bytes_for_each_known_cap() {
        let builtin = make_schema_builtin();
        for name in ["host", "runtime", "routing", "identity", "http", "http-client"] {
            let cap = test_cap(name, "test-cid");
            let result = call_builtin(&builtin, &[cap]).unwrap_or_else(|e| {
                panic!("schema for '{name}' returned error: {e}");
            });
            match result {
                Val::Bytes(bytes) => assert!(
                    !bytes.is_empty(),
                    "schema for '{name}' returned empty bytes"
                ),
                other => panic!("schema for '{name}' returned {other:?}, expected Val::Bytes"),
            }
        }
    }

    #[test]
    fn schema_unknown_cap_returns_permission_denied() {
        let builtin = make_schema_builtin();
        let cap = test_cap("nonexistent", "fake-cid");
        let err = call_builtin(&builtin, &[cap]).unwrap_err();
        assert_eq!(
            glia::error::type_tag(&err),
            Some(glia::error::tag::PERMISSION_DENIED)
        );
        assert!(glia::error::message(&err).unwrap().contains("nonexistent"));
    }

    #[test]
    fn schema_arity_mismatch_returns_structured_error() {
        let builtin = make_schema_builtin();
        let err = call_builtin(&builtin, &[]).unwrap_err();
        assert_eq!(
            glia::error::type_tag(&err),
            Some(glia::error::tag::ARITY)
        );
    }

    #[test]
    fn schema_non_cap_arg_returns_type_mismatch() {
        let builtin = make_schema_builtin();
        let err = call_builtin(&builtin, &[Val::Int(42)]).unwrap_err();
        assert_eq!(
            glia::error::type_tag(&err),
            Some(glia::error::tag::TYPE_MISMATCH)
        );
    }

    #[test]
    fn doc_returns_summary_for_each_cap() {
        let builtin = make_doc_builtin();
        for name in ["host", "runtime", "routing", "identity", "http"] {
            let cap = test_cap(name, "test-cid-123");
            let result = call_builtin(&builtin, &[cap]).unwrap();
            match result {
                Val::Str(s) => {
                    assert!(s.contains(name), "doc for '{name}' missing cap name: {s}");
                    assert!(
                        s.contains("test-cid-123"),
                        "doc for '{name}' missing schema cid: {s}"
                    );
                }
                other => panic!("doc for '{name}' returned {other:?}, expected Val::Str"),
            }
        }
    }

    #[test]
    fn doc_unknown_cap_returns_permission_denied() {
        let builtin = make_doc_builtin();
        let cap = test_cap("ghost", "ghost-cid");
        let err = call_builtin(&builtin, &[cap]).unwrap_err();
        assert_eq!(
            glia::error::type_tag(&err),
            Some(glia::error::tag::PERMISSION_DENIED)
        );
    }

    #[test]
    fn help_includes_schema_byte_count_for_known_cap() {
        let builtin = make_help_builtin();
        let cap = test_cap("host", "host-cid");
        let result = call_builtin(&builtin, &[cap]).unwrap();
        let text = match result {
            Val::Str(s) => s,
            other => panic!("expected Val::Str, got {other:?}"),
        };
        assert!(text.contains("host"), "help missing cap name: {text}");
        assert!(text.contains("host-cid"), "help missing cid: {text}");
        assert!(
            text.contains("schema-bytes"),
            "help should mention schema bytes: {text}"
        );
        assert!(
            text.contains("(schema host)"),
            "help should suggest the schema builtin: {text}"
        );
    }

    #[test]
    fn help_unknown_cap_says_not_registered() {
        let builtin = make_help_builtin();
        let cap = test_cap("future-cap", "future-cid");
        let result = call_builtin(&builtin, &[cap]).unwrap();
        let text = match result {
            Val::Str(s) => s,
            other => panic!("expected Val::Str, got {other:?}"),
        };
        assert!(
            text.contains("not registered"),
            "help should note unregistered schema: {text}"
        );
    }

    #[test]
    fn schema_dynamic_glia_cap_returns_descriptor_bytes() {
        let builtin = make_schema_builtin();
        let mut methods = std::collections::HashMap::new();
        methods.insert(
            "lookup".to_string(),
            Val::NativeFn {
                name: "lookup".into(),
                func: Rc::new(|_args: &[Val]| Ok(Val::Nil)),
            },
        );
        let descriptor = b"glia-cap-descriptor".to_vec();
        let cap = make_cap(
            "directory",
            "glia:defcap:v1",
            Rc::new(GliaCapInner {
                methods,
                descriptor: descriptor.clone(),
            }),
        );
        let out = call_builtin(&builtin, &[cap]).unwrap();
        assert_eq!(out, Val::Bytes(descriptor));
    }

    #[test]
    fn schema_dynamic_attenuated_cap_returns_descriptor_bytes() {
        let builtin = make_schema_builtin();
        let base = test_cap("runtime", "test-runtime");
        let mut allow = std::collections::BTreeSet::new();
        allow.insert("run".to_string());
        let descriptor = b"attenuated-cap-descriptor".to_vec();
        let cap = make_cap(
            "runtime-ro",
            "glia:defcap:v1",
            Rc::new(AttenuatedCapInner {
                base,
                allow_methods: allow,
                descriptor: descriptor.clone(),
            }),
        );
        let out = call_builtin(&builtin, &[cap]).unwrap();
        assert_eq!(out, Val::Bytes(descriptor));
    }

    #[test]
    fn unwrap_cap_arg_rejects_zero_args() {
        let err = unwrap_cap_arg("schema", &[]).unwrap_err();
        assert_eq!(
            glia::error::type_tag(&err),
            Some(glia::error::tag::ARITY)
        );
    }

    #[test]
    fn unwrap_cap_arg_rejects_two_args() {
        let err = unwrap_cap_arg("schema", &[Val::Nil, Val::Nil]).unwrap_err();
        assert_eq!(
            glia::error::type_tag(&err),
            Some(glia::error::tag::ARITY)
        );
    }

}
