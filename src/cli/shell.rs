//! `ww shell` CLI surface.

use anyhow::{bail, Context, Result};
use auth::SigningDomain;
use capnp::capability::FromClientHook;
use capnp_rpc::{new_client, pry};
use caps::mcp_adapter::{self, ActionPolicy, ExprPart, ToolAction, ToolSpec};
use caps::{
    clear_import_cache, extract_method, make_import_cap, make_import_handler, LoadBackend,
    LoadRuntime,
};
use glia::effect::{EffectTarget, HostEffect, HostEffectResult};
use glia::eval::{self, Dispatch, Env, EvalOutcome};
use glia::{make_cap, Val};
use libp2p::multiaddr::Protocol;
use libp2p::{Multiaddr, PeerId, StreamProtocol};
use libp2p_core::SignedEnvelope;
use rustyline::error::ReadlineError;
use std::collections::HashMap;
use std::future::Future;
use std::io::{self, IsTerminal, Write};
use std::path::PathBuf;
use std::pin::Pin;
use std::rc::Rc;
use std::time::Duration;
use tokio::sync::oneshot;

const CAPNP_PROTOCOL: StreamProtocol = StreamProtocol::new("/ww/0.1.0");
const DISCOVERY_TIMEOUT: Duration = Duration::from_secs(10);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const STREAM_READY_TIMEOUT: Duration = Duration::from_secs(15);
const RPC_TIMEOUT: Duration = Duration::from_secs(30);
const TEST_DISCOVERY_ENV: &str = "WW_TEST_SHELL_CANDIDATES";
const IPFS_STREAM_READ_CHUNK_BYTES: u32 = 64 * 1024;
const MCP_PROTOCOL_VERSION: &str = "2024-11-05";
const MCP_SERVER_NAME: &str = "wetware";
const MCP_SERVER_VERSION: &str = "0.1.0";

const HELP_TEXT: &str = "\
Remote Glia shell. Available commands:
  (perform host :id)       — peer identity
  (perform host :addrs)    — listen addresses
  (perform host :peers)    — connected peers
  (perform routing :resolve \"/ipns/<name>\") — IPNS resolve
  (help)                   — this message
  (perform :exit nil)      — disconnect";

const SHELL_MCP_HOST_ID_EXPR: &[ExprPart] = &[ExprPart::Literal("(perform host :id)")];
const SHELL_MCP_HOST_PEERS_EXPR: &[ExprPart] = &[ExprPart::Literal("(perform host :peers)")];
const SHELL_MCP_HOST_ADDRS_EXPR: &[ExprPart] = &[ExprPart::Literal("(perform host :addrs)")];
const SHELL_MCP_HOST_ACTIONS: &[ToolAction] = &[
    ToolAction {
        action: Some("id"),
        template: SHELL_MCP_HOST_ID_EXPR,
    },
    ToolAction {
        action: Some("peers"),
        template: SHELL_MCP_HOST_PEERS_EXPR,
    },
    ToolAction {
        action: Some("addrs"),
        template: SHELL_MCP_HOST_ADDRS_EXPR,
    },
];

const SHELL_MCP_ROUTING_PROVIDE_EXPR: &[ExprPart] = &[
    ExprPart::Literal("(perform routing :provide "),
    ExprPart::QuotedStringField {
        field: "key",
        default: "",
    },
    ExprPart::Literal(")"),
];
const SHELL_MCP_ROUTING_RESOLVE_EXPR: &[ExprPart] = &[
    ExprPart::Literal("(perform routing :resolve "),
    ExprPart::QuotedStringField {
        field: "name",
        default: "",
    },
    ExprPart::Literal(")"),
];
const SHELL_MCP_ROUTING_HASH_EXPR: &[ExprPart] = &[
    ExprPart::Literal("(perform routing :hash "),
    ExprPart::QuotedStringField {
        field: "data",
        default: "",
    },
    ExprPart::Literal(")"),
];
const SHELL_MCP_ROUTING_ACTIONS: &[ToolAction] = &[
    ToolAction {
        action: Some("provide"),
        template: SHELL_MCP_ROUTING_PROVIDE_EXPR,
    },
    ToolAction {
        action: Some("resolve"),
        template: SHELL_MCP_ROUTING_RESOLVE_EXPR,
    },
    ToolAction {
        action: Some("hash"),
        template: SHELL_MCP_ROUTING_HASH_EXPR,
    },
];

const SHELL_MCP_IMPORT_EXPR: &[ExprPart] = &[
    ExprPart::Literal("(perform import "),
    ExprPart::QuotedStringField {
        field: "path",
        default: "",
    },
    ExprPart::Literal(")"),
];
const SHELL_MCP_IMPORT_ACTIONS: &[ToolAction] = &[ToolAction {
    action: None,
    template: SHELL_MCP_IMPORT_EXPR,
}];

const SHELL_MCP_TOOL_SPECS: &[ToolSpec] = &[
    ToolSpec {
        name: "host",
        action_policy: ActionPolicy::RequiredSafe,
        actions: SHELL_MCP_HOST_ACTIONS,
    },
    ToolSpec {
        name: "routing",
        action_policy: ActionPolicy::RequiredSafe,
        actions: SHELL_MCP_ROUTING_ACTIONS,
    },
    ToolSpec {
        name: "import",
        action_policy: ActionPolicy::Ignore,
        actions: SHELL_MCP_IMPORT_ACTIONS,
    },
];

#[derive(Clone, Debug)]
struct Candidate {
    peer_id: Option<PeerId>,
    addrs: Vec<Multiaddr>,
}

struct LocalSigner {
    keypair: libp2p::identity::Keypair,
}

struct GraftedShellCaps {
    host: ww::system_capnp::host::Client,
    routing: ww::routing_capnp::routing::Client,
    ipfs: Option<ww::system_capnp::ipfs::Client>,
}

type HandlerFn =
    for<'a> fn(&'a [Val]) -> Pin<Box<dyn Future<Output = std::result::Result<Val, Val>> + 'a>>;

struct LocalShellDispatch<'a> {
    table: &'a HashMap<&'static str, HandlerFn>,
}

impl<'a> Dispatch for LocalShellDispatch<'a> {
    fn call<'b>(
        &'b self,
        name: &'b str,
        args: &'b [Val],
    ) -> Pin<Box<dyn Future<Output = std::result::Result<Val, Val>> + 'b>> {
        Box::pin(async move {
            match self.table.get(name) {
                Some(handler) => handler(args).await,
                None => Err(Val::from(format!("{name}: command not found"))),
            }
        })
    }
}

struct LocalShellRuntime {
    env: Env,
    dispatch: HashMap<&'static str, HandlerFn>,
    load_runtime: LoadRuntime,
}

#[derive(Clone, Copy)]
enum ShellEffectMode {
    Interactive,
    Mcp,
}

enum ShellEvalResult {
    Value(String),
    Error(String),
    Exit,
}

impl LocalSigner {
    fn from_signing_key(sk: &ed25519_dalek::SigningKey) -> Result<Self> {
        let keypair = ww::keys::to_libp2p(sk)?;
        Ok(Self { keypair })
    }
}

#[allow(refining_impl_trait)]
impl ww::auth_capnp::signer::Server for LocalSigner {
    fn sign(
        self: capnp::capability::Rc<Self>,
        params: ww::auth_capnp::signer::SignParams,
        mut results: ww::auth_capnp::signer::SignResults,
    ) -> capnp::capability::Promise<(), capnp::Error> {
        let p = pry!(params.get());
        let nonce = p.get_nonce();
        let epoch_seq = p.get_epoch_seq();
        let domain = SigningDomain::terminal_membrane();

        let mut payload = Vec::with_capacity(16);
        payload.extend_from_slice(&nonce.to_be_bytes());
        payload.extend_from_slice(&epoch_seq.to_be_bytes());

        let envelope = pry!(SignedEnvelope::new(
            &self.keypair,
            domain.as_str().to_string(),
            domain.payload_type().to_vec(),
            payload,
        )
        .map_err(|e| capnp::Error::failed(format!("signing failed: {e}"))));

        results.get().set_sig(&envelope.into_protobuf_encoding());
        capnp::capability::Promise::ok(())
    }
}

/// Run the interactive shell client.
///
/// - `ww shell <addr>` dials explicit multiaddr.
/// - `ww shell` discovers local host candidates and connects when unambiguous.
pub async fn run_shell(addr: Option<Multiaddr>, select: Option<String>) -> Result<()> {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async move { run_shell_local(addr, select).await })
        .await
}

pub async fn run_mcp(addr: Option<Multiaddr>, select: Option<String>) -> Result<()> {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async move { run_mcp_local(addr, select).await })
        .await
}

async fn run_shell_local(addr: Option<Multiaddr>, select: Option<String>) -> Result<()> {
    let (signing_key, preferred_peer_id) = load_shell_identity()?;
    let target = resolve_target(
        addr,
        select.as_deref(),
        Some(preferred_peer_id),
        stdin_is_interactive_tty(),
    )
    .await?;

    let grafted_caps = dial_shell(&target, &signing_key).await?;
    let mut runtime = build_local_shell_runtime(grafted_caps).await;
    run_repl(&mut runtime).await
}

async fn run_mcp_local(addr: Option<Multiaddr>, select: Option<String>) -> Result<()> {
    let (signing_key, preferred_peer_id) = load_shell_identity()?;
    let target = resolve_target(addr, select.as_deref(), Some(preferred_peer_id), false).await?;
    // MCP mode is stdio protocol mode; never prompt interactively on stdin.
    let grafted_caps = dial_shell(&target, &signing_key).await?;
    let mut runtime = build_local_shell_runtime(grafted_caps).await;
    run_mcp_stdio(&mut runtime).await
}

async fn resolve_target(
    addr: Option<Multiaddr>,
    select: Option<&str>,
    preferred_peer_id: Option<libp2p::PeerId>,
    interactive: bool,
) -> Result<Candidate> {
    if let Some(addr) = addr {
        let peer = peer_id_from_addr(&addr);
        let addrs = vec![addr];
        return candidate_from_parts(peer, addrs);
    }

    let candidates = if let Some(candidates) = discovery_candidates_override()? {
        candidates
    } else {
        discover_local_candidates().await?
    };
    choose_candidate(candidates, preferred_peer_id, select, interactive)
}

#[derive(serde::Deserialize)]
struct JsonRpcRequest {
    #[allow(dead_code)]
    jsonrpc: Option<String>,
    method: String,
    params: Option<serde_json::Value>,
    id: Option<serde_json::Value>,
}

fn write_jsonrpc_result(id: &serde_json::Value, result: serde_json::Value) {
    let response = serde_json::json!({
        "jsonrpc": "2.0",
        "result": result,
        "id": id,
    });
    let mut out = std::io::stdout();
    let _ = serde_json::to_writer(&mut out, &response);
    let _ = out.write_all(b"\n");
    let _ = out.flush();
}

fn write_jsonrpc_error(id: &serde_json::Value, code: i64, message: &str) {
    let response = serde_json::json!({
        "jsonrpc": "2.0",
        "error": {
            "code": code,
            "message": message,
        },
        "id": id,
    });
    let mut out = std::io::stdout();
    let _ = serde_json::to_writer(&mut out, &response);
    let _ = out.write_all(b"\n");
    let _ = out.flush();
}

fn write_mcp_tool_result(id: &serde_json::Value, text: &str) {
    write_jsonrpc_result(
        id,
        serde_json::json!({
            "content": [{ "type": "text", "text": text }],
        }),
    );
}

fn write_mcp_tool_error(id: &serde_json::Value, message: &str, data: &serde_json::Value) {
    let mut payload = serde_json::Map::new();
    payload.insert(
        "content".into(),
        serde_json::json!([{"type": "text", "text": message}]),
    );
    payload.insert("isError".into(), serde_json::Value::Bool(true));
    if !data.is_null() {
        payload.insert("structuredContent".into(), data.clone());
    }
    write_jsonrpc_result(id, serde_json::Value::Object(payload));
}

fn mcp_initialize_result() -> serde_json::Value {
    serde_json::json!({
        "protocolVersion": MCP_PROTOCOL_VERSION,
        "serverInfo": {
            "name": MCP_SERVER_NAME,
            "version": MCP_SERVER_VERSION,
        },
        "capabilities": {
            "tools": {},
        },
    })
}

fn mcp_tools_list() -> serde_json::Value {
    serde_json::json!({
        "tools": [
            {
                "name": "host",
                "description": "Node identity and peer management. Actions: id, peers, addrs.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "action": {
                            "type": "string",
                            "enum": ["id", "peers", "addrs"]
                        }
                    },
                    "required": ["action"]
                }
            },
            {
                "name": "routing",
                "description": "DHT routing operations. Actions: provide, resolve, hash.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "action": {
                            "type": "string",
                            "enum": ["provide", "resolve", "hash"]
                        },
                        "key": { "type": "string" },
                        "name": { "type": "string" },
                        "data": { "type": "string" }
                    },
                    "required": ["action"]
                }
            },
            {
                "name": "import",
                "description": "Load a Glia module by path.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "path": { "type": "string" }
                    },
                    "required": ["path"]
                }
            },
            {
                "name": "eval",
                "description": "Evaluate a Glia s-expression.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "expression": { "type": "string" }
                    },
                    "required": ["expression"]
                }
            }
        ]
    })
}

fn mcp_tool_to_glia(tool_name: &str, args: &serde_json::Value) -> Option<String> {
    mcp_adapter::tool_call_to_glia(SHELL_MCP_TOOL_SPECS, tool_name, args)
}

async fn run_mcp_stdio(runtime: &mut LocalShellRuntime) -> Result<()> {
    use tokio::io::{AsyncBufReadExt, BufReader};

    let tools = mcp_tools_list();
    let stdin = tokio::io::stdin();
    let mut reader = BufReader::new(stdin);
    let mut line = String::new();

    loop {
        line.clear();
        let read = reader.read_line(&mut line).await?;
        if read == 0 {
            break;
        }
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }

        let request: JsonRpcRequest = match serde_json::from_str(trimmed) {
            Ok(req) => req,
            Err(err) => {
                let null_id = serde_json::Value::Null;
                write_jsonrpc_error(&null_id, -32700, &format!("Parse error: {err}"));
                continue;
            }
        };

        let Some(id) = request.id else {
            continue;
        };

        match request.method.as_str() {
            "initialize" => write_jsonrpc_result(&id, mcp_initialize_result()),
            "ping" => write_jsonrpc_result(&id, serde_json::json!({})),
            "tools/list" => write_jsonrpc_result(&id, tools.clone()),
            "tools/call" => {
                let params = request.params.unwrap_or(serde_json::Value::Null);
                let tool_name = params.get("name").and_then(|v| v.as_str()).unwrap_or("");
                let arguments = params
                    .get("arguments")
                    .cloned()
                    .unwrap_or(serde_json::Value::Null);

                let expression = if tool_name == "eval" {
                    let expression = arguments
                        .get("expression")
                        .and_then(|v| v.as_str())
                        .unwrap_or("");
                    if expression.is_empty() {
                        write_mcp_tool_error(&id, "empty expression", &serde_json::Value::Null);
                        continue;
                    }
                    expression.to_string()
                } else if let Some(expr) = mcp_tool_to_glia(tool_name, &arguments) {
                    expr
                } else {
                    write_jsonrpc_error(&id, -32602, &format!("Unknown tool: {tool_name}"));
                    continue;
                };

                match shell_eval_raw(runtime, &expression, ShellEffectMode::Mcp).await? {
                    Ok(EvalOutcome::Value(Val::Nil)) => write_mcp_tool_result(&id, "nil"),
                    Ok(EvalOutcome::Value(value)) => {
                        write_mcp_tool_result(&id, &format!("{value}"))
                    }
                    Ok(EvalOutcome::Exit) => {
                        let err = mcp_adapter::protocol_mode_unavailable("exit");
                        write_mcp_tool_error(
                            &id,
                            &mcp_adapter::val_to_mcp_error_text(&err),
                            &mcp_adapter::val_to_mcp_error_data(&err),
                        );
                    }
                    Err(err) => {
                        let text = mcp_adapter::val_to_mcp_error_text(&err);
                        let data = mcp_adapter::val_to_mcp_error_data(&err);
                        write_mcp_tool_error(&id, &text, &data);
                    }
                }
            }
            _ => write_jsonrpc_error(
                &id,
                -32601,
                &format!("Method not found: {}", request.method),
            ),
        }
    }

    Ok(())
}

async fn dial_shell(
    target: &Candidate,
    signing_key: &ed25519_dalek::SigningKey,
) -> Result<GraftedShellCaps> {
    // Use a fresh transport identity for the shell client swarm.
    // The user's persistent key from ~/.ww/identity is still used for
    // Terminal login signing below (LocalSigner). Reusing the same libp2p
    // peer ID as the daemon causes self-dial rejection.
    let keypair = libp2p::identity::Keypair::generate_ed25519();
    let mut client = ww::host::ClientSwarm::new(keypair)?;
    let mut stream_control = client.stream_control();

    let (connected_tx, connected_rx) = oneshot::channel::<Result<PeerId, String>>();

    if let Some(peer_id) = target.peer_id {
        // Seed known addresses and initiate dial.
        for addr in &target.addrs {
            client.add_peer_addr(peer_id, addr.clone());
        }
    } else if let Some(addr) = target.addrs.first() {
        client
            .dial(addr.clone())
            .map_err(|e| anyhow::anyhow!("failed to dial {addr}: {e}"))?;
    } else {
        bail!("no dial addresses provided");
    }

    tokio::task::spawn_local(client.run(Some(connected_tx)));

    let connected_peer = await_connected_peer(connected_rx).await?;

    let remote_peer = target.peer_id.unwrap_or(connected_peer);

    let stream_open_deadline = tokio::time::Instant::now() + STREAM_READY_TIMEOUT;
    let stream = loop {
        match tokio::time::timeout(
            CONNECT_TIMEOUT,
            stream_control.open_stream(remote_peer, CAPNP_PROTOCOL),
        )
        .await
        {
            Ok(Ok(stream)) => break stream,
            Ok(Err(e)) => {
                if tokio::time::Instant::now() >= stream_open_deadline {
                    return Err(anyhow::anyhow!("failed to open shell stream: {e}"));
                }
                tokio::time::sleep(Duration::from_millis(500)).await;
            }
            Err(_) => {
                if tokio::time::Instant::now() >= stream_open_deadline {
                    return Err(anyhow::anyhow!("timed out opening shell stream"));
                }
                tokio::time::sleep(Duration::from_millis(500)).await;
            }
        }
    };

    let ww::rpc::vat_dial::VatDial {
        bootstrap: terminal,
        driver: _driver,
    } = ww::rpc::vat_dial::connect::<
        _,
        ww::auth_capnp::terminal::Client<ww::membrane_capnp::membrane::Owned>,
    >(stream);

    let signer_client: ww::auth_capnp::signer::Client =
        new_client(LocalSigner::from_signing_key(signing_key)?);

    let mut login_req = terminal.login_request();
    login_req.get().set_signer(signer_client);
    let login_resp = tokio::time::timeout(RPC_TIMEOUT, login_req.send().promise)
        .await
        .context("terminal login timed out")??;

    let membrane = login_resp
        .get()?
        .get_session()
        .context("terminal login returned no session")?;

    let graft_resp = tokio::time::timeout(RPC_TIMEOUT, membrane.graft_request().send().promise)
        .await
        .context("graft request timed out")??;
    let caps = graft_resp.get()?.get_caps()?;
    let host = get_graft_cap(&caps, "host")?;
    let routing = get_graft_cap(&caps, "routing")?;
    let ipfs = get_graft_cap(&caps, "ipfs").ok();
    Ok(GraftedShellCaps {
        host,
        routing,
        ipfs,
    })
}

async fn await_connected_peer(
    connected_rx: oneshot::Receiver<Result<PeerId, String>>,
) -> Result<PeerId> {
    tokio::time::timeout(CONNECT_TIMEOUT, connected_rx)
        .await
        .context("timed out waiting for libp2p connection")?
        .context("connection notification channel dropped")?
        .map_err(|e| anyhow::anyhow!("libp2p connection failed before establish: {e}"))
}

async fn run_repl(runtime: &mut LocalShellRuntime) -> Result<()> {
    let mut rl = rustyline::DefaultEditor::new().context("failed to initialize line editor")?;

    loop {
        match rl.readline("ww> ") {
            Ok(line) => {
                let input = line.trim();
                if input.is_empty() {
                    continue;
                }
                if input == ":q" || input == ":quit" || input == ":exit" {
                    break;
                }
                let _ = rl.add_history_entry(input);

                match shell_eval(runtime, input).await? {
                    ShellEvalResult::Value(result) => println!("{result}"),
                    ShellEvalResult::Error(result) => eprintln!("{result}"),
                    ShellEvalResult::Exit => break,
                }
            }
            Err(ReadlineError::Interrupted) => {
                eprintln!("^C");
                continue;
            }
            Err(ReadlineError::Eof) => break,
            Err(e) => return Err(anyhow::anyhow!("readline error: {e}")),
        }
    }

    Ok(())
}

async fn build_local_shell_runtime(caps: GraftedShellCaps) -> LocalShellRuntime {
    let GraftedShellCaps {
        host,
        routing,
        ipfs,
    } = caps;

    clear_import_cache();
    let load_backend = Rc::new(ShellLoadBackend { ipfs });
    let load_runtime = caps::LoadRuntime::new(
        std::env::var("WW_ROOT").unwrap_or_else(|_| "/".into()),
        load_backend,
    );

    let mut env = Env::new();

    env.set(
        "host".to_string(),
        make_cap("host", "shell:host", Rc::new(host.clone())),
    );
    env.set(
        "routing".to_string(),
        make_cap("routing", "shell:routing", Rc::new(routing.clone())),
    );
    env.set("import".to_string(), make_import_cap());

    env.set("host-handler".to_string(), make_host_handler_local(host));
    env.set(
        "routing-handler".to_string(),
        make_routing_handler_local(routing),
    );
    env.set(
        "import-handler".to_string(),
        make_import_handler(load_runtime.clone()),
    );

    let dispatch = build_dispatch();
    {
        let mut prelude_dispatch = LocalShellDispatch { table: &dispatch };
        glia::load_prelude(&mut env, &mut prelude_dispatch).await;
    }

    LocalShellRuntime {
        env,
        dispatch,
        load_runtime,
    }
}

fn build_dispatch() -> HashMap<&'static str, HandlerFn> {
    fn help_handler(
        _args: &[Val],
    ) -> Pin<Box<dyn Future<Output = std::result::Result<Val, Val>> + '_>> {
        Box::pin(std::future::ready(Ok(Val::Str(HELP_TEXT.to_string()))))
    }

    let mut table: HashMap<&'static str, HandlerFn> = HashMap::new();
    table.insert("help", help_handler);
    table
}

struct ShellLoadBackend {
    ipfs: Option<ww::system_capnp::ipfs::Client>,
}

async fn load_ipfs_read(
    ipfs: &ww::system_capnp::ipfs::Client,
    path: &str,
) -> std::result::Result<Vec<u8>, capnp::Error> {
    let mut req = ipfs.read_request();
    req.get().set_path(path);
    let resp = req.send().promise.await?;
    let stream = resp.get()?.get_stream()?;
    let mut out = Vec::new();

    loop {
        let mut read_req = stream.read_request();
        read_req.get().set_max_bytes(IPFS_STREAM_READ_CHUNK_BYTES);
        let read_resp = read_req.send().promise.await?;
        let chunk = read_resp.get()?.get_data()?;
        if chunk.is_empty() {
            break;
        }
        out.extend_from_slice(chunk);
    }

    Ok(out)
}

impl LoadBackend for ShellLoadBackend {
    fn load<'a>(
        &'a self,
        path: &'a str,
    ) -> Pin<Box<dyn Future<Output = std::result::Result<Vec<u8>, Val>> + 'a>> {
        Box::pin(async move {
            if ww::ipfs::is_ipfs_path(path) {
                let ipfs = self.ipfs.clone().ok_or_else(|| {
                    Val::from("load: /ipfs and /ipns paths require grafted 'ipfs' capability")
                })?;
                match load_ipfs_read(&ipfs, path).await {
                    Ok(bytes) => Ok(bytes),
                    Err(err) => Err(Val::from(format!("load: {path}: {err}"))),
                }
            } else {
                std::fs::read(path).map_err(|e| Val::from(format!("load: {path}: {e}")))
            }
        })
    }
}

fn call_resume_local(resume: &Val, val: Val) -> Result<Val, Val> {
    match resume {
        Val::NativeFn { func, .. } => func(&[val]),
        _ => Err(Val::from("cap handler: invalid resume function")),
    }
}

fn make_host_handler_local(host: ww::system_capnp::host::Client) -> Val {
    use base58::ToBase58;

    Val::AsyncNativeFn {
        name: "host-handler".into(),
        func: Rc::new(move |args: Vec<Val>| {
            let host = host.clone();
            Box::pin(async move {
                let (method, _rest) = extract_method(&args[0])?;
                let resume = &args[1];

                match method {
                    "id" => {
                        let resp = host
                            .id_request()
                            .send()
                            .promise
                            .await
                            .map_err(|e| Val::from(e.to_string()))?;
                        let peer_id_bytes = resp
                            .get()
                            .map_err(|e| Val::from(e.to_string()))?
                            .get_peer_id()
                            .map_err(|e| Val::from(e.to_string()))?;
                        let encoded = peer_id_bytes.to_base58();
                        call_resume_local(resume, Val::Str(encoded))
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
                        let items: Vec<Val> = addrs
                            .iter()
                            .filter_map(|a| {
                                a.ok().and_then(|bytes| {
                                    multiaddr::Multiaddr::try_from(bytes.to_vec())
                                        .ok()
                                        .map(|ma| Val::Str(ma.to_string()))
                                })
                            })
                            .collect();
                        call_resume_local(resume, Val::List(items))
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
                        let items: Vec<Val> = peers
                            .iter()
                            .filter_map(|p| {
                                let peer_id = p.get_peer_id().ok()?;
                                let encoded = peer_id.to_base58();
                                let addrs: Vec<Val> = p
                                    .get_addrs()
                                    .ok()?
                                    .iter()
                                    .filter_map(|a| {
                                        a.ok().and_then(|bytes| {
                                            multiaddr::Multiaddr::try_from(bytes.to_vec())
                                                .ok()
                                                .map(|ma| Val::Str(ma.to_string()))
                                        })
                                    })
                                    .collect();
                                Some(Val::Map(glia::ValMap::from_pairs(vec![
                                    (Val::Keyword("peer-id".into()), Val::Str(encoded)),
                                    (Val::Keyword("addrs".into()), Val::List(addrs)),
                                ])))
                            })
                            .collect();
                        call_resume_local(resume, Val::List(items))
                    }
                    other => Err(Val::from(format!("host: unknown method :{other}"))),
                }
            })
        }),
    }
}

fn make_routing_handler_local(routing: ww::routing_capnp::routing::Client) -> Val {
    Val::AsyncNativeFn {
        name: "routing-handler".into(),
        func: Rc::new(move |args: Vec<Val>| {
            let routing = routing.clone();
            Box::pin(async move {
                let (method, rest) = extract_method(&args[0])?;
                let resume = &args[1];

                match method {
                    "provide" => {
                        let key = match rest.first() {
                            Some(Val::Str(s)) => s.clone(),
                            _ => return Err(Val::from("routing :provide — expected key string")),
                        };
                        let mut req = routing.provide_request();
                        req.get().set_key(&key);
                        req.send()
                            .promise
                            .await
                            .map_err(|e| Val::from(e.to_string()))?;
                        call_resume_local(resume, Val::Nil)
                    }
                    "resolve" => {
                        let name = match rest.first() {
                            Some(Val::Str(s)) => s.clone(),
                            _ => return Err(Val::from("routing :resolve — expected name string")),
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
                            .to_string()
                            .map_err(|e| Val::from(format!("{e}")))?;
                        call_resume_local(resume, Val::Str(path))
                    }
                    "mkdir" => {
                        let base_cid = match rest.first() {
                            Some(Val::Str(s)) => s.clone(),
                            _ => {
                                return Err(Val::from("routing :mkdir — expected base CID string"));
                            }
                        };
                        let path = match rest.get(1) {
                            Some(Val::Str(s)) => s.clone(),
                            _ => return Err(Val::from("routing :mkdir — expected path string")),
                        };
                        let parents = match rest.get(2) {
                            Some(Val::Bool(b)) => *b,
                            _ => true,
                        };
                        let mut req = routing.mkdir_request();
                        let mut r = req.get();
                        r.set_base_cid(&base_cid);
                        r.set_path(&path);
                        r.set_parents(parents);
                        let resp = req
                            .send()
                            .promise
                            .await
                            .map_err(|e| Val::from(e.to_string()))?;
                        let root = resp
                            .get()
                            .map_err(|e| Val::from(e.to_string()))?
                            .get_root_cid()
                            .map_err(|e| Val::from(e.to_string()))?
                            .to_string()
                            .map_err(|e| Val::from(format!("{e}")))?;
                        call_resume_local(resume, Val::Str(root))
                    }
                    "write-file" => {
                        let base_cid = match rest.first() {
                            Some(Val::Str(s)) => s.clone(),
                            _ => {
                                return Err(Val::from(
                                    "routing :write-file — expected base CID string",
                                ));
                            }
                        };
                        let path = match rest.get(1) {
                            Some(Val::Str(s)) => s.clone(),
                            _ => {
                                return Err(Val::from(
                                    "routing :write-file — expected path string",
                                ));
                            }
                        };
                        let data = match rest.get(2) {
                            Some(Val::Bytes(b)) => b.clone(),
                            Some(Val::Str(s)) => s.as_bytes().to_vec(),
                            _ => {
                                return Err(Val::from(
                                    "routing :write-file — expected bytes or string data",
                                ));
                            }
                        };
                        let create_parents = match rest.get(3) {
                            Some(Val::Bool(b)) => *b,
                            _ => true,
                        };
                        let mut req = routing.write_file_request();
                        let mut r = req.get();
                        r.set_base_cid(&base_cid);
                        r.set_path(&path);
                        r.set_data(&data);
                        r.set_create_parents(create_parents);
                        let resp = req
                            .send()
                            .promise
                            .await
                            .map_err(|e| Val::from(e.to_string()))?;
                        let root = resp
                            .get()
                            .map_err(|e| Val::from(e.to_string()))?
                            .get_root_cid()
                            .map_err(|e| Val::from(e.to_string()))?
                            .to_string()
                            .map_err(|e| Val::from(format!("{e}")))?;
                        call_resume_local(resume, Val::Str(root))
                    }
                    "remove" => {
                        let base_cid = match rest.first() {
                            Some(Val::Str(s)) => s.clone(),
                            _ => {
                                return Err(Val::from(
                                    "routing :remove — expected base CID string",
                                ));
                            }
                        };
                        let path = match rest.get(1) {
                            Some(Val::Str(s)) => s.clone(),
                            _ => return Err(Val::from("routing :remove — expected path string")),
                        };
                        let recursive = match rest.get(2) {
                            Some(Val::Bool(b)) => *b,
                            _ => false,
                        };
                        let mut req = routing.remove_request();
                        let mut r = req.get();
                        r.set_base_cid(&base_cid);
                        r.set_path(&path);
                        r.set_recursive(recursive);
                        let resp = req
                            .send()
                            .promise
                            .await
                            .map_err(|e| Val::from(e.to_string()))?;
                        let root = resp
                            .get()
                            .map_err(|e| Val::from(e.to_string()))?
                            .get_root_cid()
                            .map_err(|e| Val::from(e.to_string()))?
                            .to_string()
                            .map_err(|e| Val::from(format!("{e}")))?;
                        call_resume_local(resume, Val::Str(root))
                    }
                    "publish" => {
                        let name = match rest.first() {
                            Some(Val::Str(s)) => s.clone(),
                            _ => return Err(Val::from("routing :publish — expected name string")),
                        };
                        let cid = match rest.get(1) {
                            Some(Val::Str(s)) => s.clone(),
                            _ => return Err(Val::from("routing :publish — expected CID string")),
                        };
                        let expected = match rest.get(2) {
                            Some(Val::Str(s)) => s.clone(),
                            Some(Val::Nil) | None => String::new(),
                            _ => {
                                return Err(Val::from(
                                    "routing :publish — expected current path string or nil",
                                ));
                            }
                        };
                        let mut req = routing.publish_request();
                        let mut r = req.get();
                        r.set_name(&name);
                        r.set_cid(&cid);
                        r.set_expected_current(&expected);
                        let resp = req
                            .send()
                            .promise
                            .await
                            .map_err(|e| Val::from(e.to_string()))?;
                        let published = resp
                            .get()
                            .map_err(|e| Val::from(e.to_string()))?
                            .get_published_path()
                            .map_err(|e| Val::from(e.to_string()))?
                            .to_string()
                            .map_err(|e| Val::from(format!("{e}")))?;
                        call_resume_local(resume, Val::Str(published))
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
                            .to_string()
                            .map_err(|e| Val::from(format!("{e}")))?;
                        call_resume_local(resume, Val::Str(key))
                    }
                    other => Err(Val::from(format!("routing: unknown method :{other}"))),
                }
            })
        }),
    }
}

async fn shell_eval(runtime: &mut LocalShellRuntime, text: &str) -> Result<ShellEvalResult> {
    match shell_eval_raw(runtime, text, ShellEffectMode::Interactive).await? {
        Ok(EvalOutcome::Value(Val::Nil)) => Ok(ShellEvalResult::Value(String::new())),
        Ok(EvalOutcome::Value(result)) => Ok(ShellEvalResult::Value(format!("{result}"))),
        Ok(EvalOutcome::Exit) => Ok(ShellEvalResult::Exit),
        Err(err) => {
            let inner = glia::error::unwrap_thrown(&err).unwrap_or(&err);
            let msg = glia::error::message(inner)
                .map(str::to_string)
                .unwrap_or_else(|| format!("{inner}"));
            let formatted = match glia::error::type_tag(inner) {
                Some(tag) => format!("[{tag}] {msg}"),
                None => msg,
            };
            Ok(ShellEvalResult::Error(formatted))
        }
    }
}

async fn shell_eval_raw(
    runtime: &mut LocalShellRuntime,
    text: &str,
    mode: ShellEffectMode,
) -> Result<std::result::Result<EvalOutcome, Val>> {
    if text.trim().is_empty() {
        return Ok(Ok(EvalOutcome::Value(Val::Nil)));
    }

    let expr = match glia::read(text) {
        Ok(expr) => expr,
        Err(e) => return Ok(Err(glia::error::parse(None, e))),
    };

    let dispatch = LocalShellDispatch {
        table: &runtime.dispatch,
    };
    let load_runtime = runtime.load_runtime.clone();
    let load = Rc::new(move |data: Val| {
        let runtime = load_runtime.clone();
        Box::pin(async move { Ok(HostEffectResult::Resume(runtime.load_value(data).await?)) })
            as Pin<Box<dyn Future<Output = std::result::Result<HostEffectResult, Val>>>>
    });
    let stdout = Rc::new(move |data: Val| match mode {
        ShellEffectMode::Interactive => Box::pin(async move {
            let text = match data {
                Val::Str(s) => s,
                other => format!("{other}"),
            };
            println!("{text}");
            Ok(HostEffectResult::Resume(Val::Nil))
        })
            as Pin<Box<dyn Future<Output = std::result::Result<HostEffectResult, Val>>>>,
        ShellEffectMode::Mcp => {
            Box::pin(async { Err(mcp_adapter::protocol_mode_unavailable("stdout")) })
                as Pin<Box<dyn Future<Output = std::result::Result<HostEffectResult, Val>>>>
        }
    });
    let exit = Rc::new(move |_data: Val| match mode {
        ShellEffectMode::Interactive => Box::pin(async { Ok(HostEffectResult::Exit) })
            as Pin<Box<dyn Future<Output = std::result::Result<HostEffectResult, Val>>>>,
        ShellEffectMode::Mcp => {
            Box::pin(async { Err(mcp_adapter::protocol_mode_unavailable("exit")) })
                as Pin<Box<dyn Future<Output = std::result::Result<HostEffectResult, Val>>>>
        }
    });
    let effects = [
        HostEffect {
            target: EffectTarget::Keyword("load".into()),
            handler: load,
        },
        HostEffect {
            target: EffectTarget::Keyword("stdout".into()),
            handler: stdout,
        },
        HostEffect {
            target: EffectTarget::Keyword("exit".into()),
            handler: exit,
        },
    ];
    // Capability effects remain mediated by the standard `std/caps` wrappers;
    // the keyword host effects above are evaluator-owned frames and therefore
    // never become guest-visible handler values.
    let wrapped = caps::wrap_with_handlers(&expr, &[]);
    let eval_result = tokio::time::timeout(
        RPC_TIMEOUT,
        eval::eval_toplevel_with_host_effects(&wrapped, &mut runtime.env, &dispatch, &effects),
    )
    .await
    .context("shell eval timed out")?;

    Ok(eval_result)
}

fn get_graft_cap<T: FromClientHook>(
    caps: &capnp::struct_list::Reader<'_, ww::membrane_capnp::export::Owned>,
    name: &str,
) -> Result<T, capnp::Error> {
    for i in 0..caps.len() {
        let entry = caps.get(i);
        let n = entry
            .get_name()?
            .to_str()
            .map_err(|e| capnp::Error::failed(e.to_string()))?;
        if n == name {
            return entry.get_cap().get_as_capability::<T>();
        }
    }

    Err(capnp::Error::failed(format!(
        "capability '{name}' not found in graft response"
    )))
}

fn shell_identity_path() -> Result<PathBuf> {
    if let Some(path) = std::env::var_os("WW_IDENTITY") {
        return Ok(PathBuf::from(path));
    }

    let home = dirs::home_dir().context("cannot determine home directory")?;
    Ok(home.join(".ww/identity"))
}

fn load_shell_identity() -> Result<(ed25519_dalek::SigningKey, PeerId)> {
    let path = shell_identity_path()?;
    if !path.exists() {
        bail!(
            "Identity file not found: {}\n\
             `ww shell` requires a persistent identity to authenticate.\n\
             Create one with: ww keygen > ~/.ww/identity",
            path.display()
        );
    }

    let sk = ww::keys::load(path.to_str().context("identity path is non-UTF-8")?)?;
    let peer_id = ww::keys::to_libp2p(&sk)?.public().to_peer_id();
    Ok((sk, peer_id))
}

async fn discover_local_candidates() -> Result<Vec<Candidate>> {
    let path = ww::local_host::state_path()?;

    let deadline = tokio::time::Instant::now() + DISCOVERY_TIMEOUT;
    loop {
        if let Some(host) = ww::local_host::read_live_host_state()? {
            return Ok(vec![Candidate {
                peer_id: Some(host.peer_id),
                addrs: host.addrs,
            }]);
        }

        if tokio::time::Instant::now() >= deadline {
            bail!(
                "No local wetware host discovered.\n\
                 Expected live host state at: {}\n\
                 Start a host first: `ww run .`",
                path.display()
            );
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

fn discovery_candidates_override() -> Result<Option<Vec<Candidate>>> {
    #[cfg(debug_assertions)]
    {
        match std::env::var(TEST_DISCOVERY_ENV) {
            Ok(raw) => Ok(Some(parse_discovery_candidates_json(&raw)?)),
            Err(std::env::VarError::NotPresent) => Ok(None),
            Err(std::env::VarError::NotUnicode(_)) => {
                bail!("{TEST_DISCOVERY_ENV} must be valid UTF-8 JSON")
            }
        }
    }
    #[cfg(not(debug_assertions))]
    {
        Ok(None)
    }
}

fn parse_discovery_candidates_json(raw: &str) -> Result<Vec<Candidate>> {
    #[derive(serde::Deserialize)]
    struct RawCandidate {
        peer_id: String,
        addrs: Vec<String>,
    }

    let raw_candidates: Vec<RawCandidate> = serde_json::from_str(raw)
        .map_err(|e| anyhow::anyhow!("invalid {TEST_DISCOVERY_ENV} JSON: {e}"))?;

    let mut candidates = Vec::with_capacity(raw_candidates.len());
    for (index, entry) in raw_candidates.into_iter().enumerate() {
        let peer_id: PeerId = entry.peer_id.parse().map_err(|e| {
            anyhow::anyhow!("invalid peer_id at {TEST_DISCOVERY_ENV}[{index}]: {e}")
        })?;

        let mut addrs = Vec::with_capacity(entry.addrs.len());
        for (addr_index, addr) in entry.addrs.into_iter().enumerate() {
            let parsed: Multiaddr = addr.parse().map_err(|e| {
                anyhow::anyhow!(
                    "invalid multiaddr at {TEST_DISCOVERY_ENV}[{index}].addrs[{addr_index}]: {e}"
                )
            })?;
            addrs.push(parsed);
        }

        candidates.push(Candidate {
            peer_id: Some(peer_id),
            addrs,
        });
    }

    Ok(candidates)
}

fn choose_candidate(
    candidates: Vec<Candidate>,
    preferred: Option<PeerId>,
    select: Option<&str>,
    interactive_tty: bool,
) -> Result<Candidate> {
    if candidates.is_empty() {
        bail!(
            "No wetware hosts discovered.\n\
             Try `ww shell <multiaddr>` to connect explicitly."
        );
    }

    if let Some(selector) = select {
        let selected = choose_candidate_by_selector(&candidates, selector)?;
        return ensure_candidate_addr(selected);
    }

    if candidates.len() == 1 {
        let mut candidates = candidates;
        return ensure_candidate_addr(candidates.remove(0));
    }

    if let Some(preferred_peer) = preferred {
        let mut matches: Vec<Candidate> = candidates
            .iter()
            .filter(|c| c.peer_id == Some(preferred_peer))
            .cloned()
            .collect();

        if matches.len() == 1 {
            return ensure_candidate_addr(matches.remove(0));
        }
    }

    if let Some(candidate) = choose_by_locality(&candidates) {
        return ensure_candidate_addr(candidate);
    }

    if interactive_tty {
        return select_candidate_interactive(&candidates);
    }

    let listing = format_candidates(&candidates);
    bail!(
        "Multiple wetware hosts discovered; refusing to guess.\n\
         If this is a script/non-interactive session, pass one of:\n\
         - `ww shell --select <index|peer-id>`\n\
         Use an explicit multiaddr: `ww shell <multiaddr>`\n\
         Discovered hosts:{listing}"
    )
}

fn stdin_is_interactive_tty() -> bool {
    io::stdin().is_terminal() && io::stdout().is_terminal()
}

fn format_candidates(candidates: &[Candidate]) -> String {
    let mut listing = String::new();
    for (index, c) in candidates.iter().enumerate() {
        let addrs = c
            .addrs
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join(", ");
        if let Some(peer_id) = c.peer_id {
            listing.push_str(&format!("\n  [{}] {} [{}]", index + 1, peer_id, addrs));
        } else {
            listing.push_str(&format!("\n  [{}] <unknown-peer> [{}]", index + 1, addrs));
        }
    }
    listing
}

fn choose_candidate_by_selector(candidates: &[Candidate], selector: &str) -> Result<Candidate> {
    let selector = selector.trim();
    if selector.is_empty() {
        bail!("empty selector: expected index (1..N) or peer id");
    }

    if let Ok(index) = selector.parse::<usize>() {
        if index == 0 || index > candidates.len() {
            bail!(
                "selector index {} out of range; expected 1..{}",
                index,
                candidates.len()
            );
        }
        return Ok(candidates[index - 1].clone());
    }

    let peer_id: PeerId = selector
        .parse()
        .map_err(|_| anyhow::anyhow!("invalid selector '{selector}': expected index or peer id"))?;

    let mut matches = candidates
        .iter()
        .filter(|c| c.peer_id == Some(peer_id))
        .cloned()
        .collect::<Vec<_>>();

    if matches.is_empty() {
        bail!("selector peer id {peer_id} not found in discovered candidates");
    }
    if matches.len() > 1 {
        bail!("selector peer id {peer_id} matched multiple candidates");
    }
    Ok(matches.remove(0))
}

fn select_candidate_interactive(candidates: &[Candidate]) -> Result<Candidate> {
    println!("Multiple wetware hosts discovered.");
    println!("Select a host by index or peer id:");
    print!("{}", format_candidates(candidates));
    println!();

    for _ in 0..5 {
        eprint!("selection> ");
        io::stderr().flush().context("failed to flush stderr")?;

        let mut line = String::new();
        io::stdin()
            .read_line(&mut line)
            .context("failed to read selection from stdin")?;
        let selector = line.trim();

        if selector.eq_ignore_ascii_case("q")
            || selector.eq_ignore_ascii_case("quit")
            || selector.eq_ignore_ascii_case("exit")
        {
            bail!("selection canceled");
        }

        match choose_candidate_by_selector(candidates, selector) {
            Ok(candidate) => return ensure_candidate_addr(candidate),
            Err(err) => eprintln!("Invalid selection: {err}"),
        }
    }

    bail!("too many invalid selections; aborted")
}

fn ensure_candidate_addr(candidate: Candidate) -> Result<Candidate> {
    if candidate.addrs.is_empty() {
        if let Some(peer_id) = candidate.peer_id {
            bail!("discovered peer {} has no dialable addresses", peer_id);
        }
        bail!("candidate has no dialable addresses");
    }
    Ok(candidate)
}

// Candidate locality preference for no-selector/no-identity shell discovery:
// loopback > private LAN > direct public IP > unknown/direct name > relay.
// Returns Some only when there is a unique best candidate.
fn choose_by_locality(candidates: &[Candidate]) -> Option<Candidate> {
    let mut best: Option<(usize, i8)> = None;
    let mut tie = false;
    for (idx, c) in candidates.iter().enumerate() {
        let score = candidate_locality_score(c);
        match best {
            None => {
                best = Some((idx, score));
                tie = false;
            }
            Some((_, best_score)) if score > best_score => {
                best = Some((idx, score));
                tie = false;
            }
            Some((_, best_score)) if score == best_score => {
                tie = true;
            }
            _ => {}
        }
    }
    if tie {
        None
    } else {
        best.map(|(idx, _)| candidates[idx].clone())
    }
}

fn candidate_locality_score(candidate: &Candidate) -> i8 {
    candidate
        .addrs
        .iter()
        .map(addr_locality_score)
        .max()
        .unwrap_or(-100)
}

fn addr_locality_score(addr: &Multiaddr) -> i8 {
    use std::net::IpAddr;

    let mut ip: Option<IpAddr> = None;
    let mut relay = false;

    for p in addr.iter() {
        match p {
            Protocol::Ip4(v4) => ip = Some(IpAddr::V4(v4)),
            Protocol::Ip6(v6) => ip = Some(IpAddr::V6(v6)),
            Protocol::P2pCircuit => relay = true,
            _ => {}
        }
    }

    if relay {
        return 0;
    }

    match ip {
        Some(IpAddr::V4(v4)) if v4.is_loopback() => 5,
        Some(IpAddr::V6(v6)) if v6.is_loopback() => 5,
        Some(IpAddr::V4(v4))
            if v4.is_private() || (v4.octets()[0] == 169 && v4.octets()[1] == 254) =>
        {
            4
        }
        Some(IpAddr::V6(v6))
            if (v6.segments()[0] & 0xffc0) == 0xfe80 || (v6.segments()[0] & 0xfe00) == 0xfc00 =>
        {
            4
        }
        Some(_) => 3,
        None => 2,
    }
}

fn peer_id_from_addr(addr: &Multiaddr) -> Option<PeerId> {
    for protocol in addr.iter() {
        if let Protocol::P2p(peer_id) = protocol {
            return Some(peer_id);
        }
    }
    None
}

fn candidate_from_parts(peer: Option<PeerId>, addrs: Vec<Multiaddr>) -> Result<Candidate> {
    if addrs.is_empty() {
        bail!("no dial addresses provided")
    }
    Ok(Candidate {
        peer_id: peer,
        addrs,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use authority::TerminalServer;
    use capnp::capability::Promise;
    use capnp_rpc::rpc_twoparty_capnp::Side;
    use capnp_rpc::twoparty::VatNetwork;
    use capnp_rpc::RpcSystem;
    use futures::AsyncReadExt;
    use futures::StreamExt;
    use std::cell::RefCell;
    use std::rc::Rc;
    use tokio::io::AsyncWriteExt;
    use tokio::sync::{mpsc, watch};

    const TEST_PEER_ID_BYTES: &[u8] = b"shell-local-eval-peer-id";

    fn maddr(s: &str) -> Multiaddr {
        s.parse().unwrap()
    }

    struct TestHost;

    #[allow(refining_impl_trait)]
    impl ww::system_capnp::host::Server for TestHost {
        fn id(
            self: capnp::capability::Rc<Self>,
            _params: ww::system_capnp::host::IdParams,
            mut results: ww::system_capnp::host::IdResults,
        ) -> Promise<(), capnp::Error> {
            results.get().set_peer_id(TEST_PEER_ID_BYTES);
            Promise::ok(())
        }

        fn addrs(
            self: capnp::capability::Rc<Self>,
            _params: ww::system_capnp::host::AddrsParams,
            mut results: ww::system_capnp::host::AddrsResults,
        ) -> Promise<(), capnp::Error> {
            let mut list = results.get().init_addrs(1);
            list.set(0, maddr("/ip4/127.0.0.1/tcp/2025").to_vec().as_slice());
            Promise::ok(())
        }

        fn peers(
            self: capnp::capability::Rc<Self>,
            _params: ww::system_capnp::host::PeersParams,
            mut results: ww::system_capnp::host::PeersResults,
        ) -> Promise<(), capnp::Error> {
            let mut list = results.get().init_peers(0);
            list.reborrow();
            Promise::ok(())
        }

        fn network(
            self: capnp::capability::Rc<Self>,
            _params: ww::system_capnp::host::NetworkParams,
            _results: ww::system_capnp::host::NetworkResults,
        ) -> Promise<(), capnp::Error> {
            Promise::err(capnp::Error::unimplemented(
                "network not implemented".into(),
            ))
        }
    }

    struct TestRouting;

    #[allow(refining_impl_trait)]
    impl ww::routing_capnp::routing::Server for TestRouting {
        fn provide(
            self: capnp::capability::Rc<Self>,
            _params: ww::routing_capnp::routing::ProvideParams,
            _results: ww::routing_capnp::routing::ProvideResults,
        ) -> Promise<(), capnp::Error> {
            Promise::err(capnp::Error::unimplemented("test routing".into()))
        }

        fn find_providers(
            self: capnp::capability::Rc<Self>,
            _params: ww::routing_capnp::routing::FindProvidersParams,
            _results: ww::routing_capnp::routing::FindProvidersResults,
        ) -> Promise<(), capnp::Error> {
            Promise::err(capnp::Error::unimplemented("test routing".into()))
        }

        fn resolve(
            self: capnp::capability::Rc<Self>,
            _params: ww::routing_capnp::routing::ResolveParams,
            _results: ww::routing_capnp::routing::ResolveResults,
        ) -> Promise<(), capnp::Error> {
            Promise::err(capnp::Error::unimplemented("test routing".into()))
        }

        fn mkdir(
            self: capnp::capability::Rc<Self>,
            _params: ww::routing_capnp::routing::MkdirParams,
            _results: ww::routing_capnp::routing::MkdirResults,
        ) -> Promise<(), capnp::Error> {
            Promise::err(capnp::Error::unimplemented("test routing".into()))
        }

        fn write_file(
            self: capnp::capability::Rc<Self>,
            _params: ww::routing_capnp::routing::WriteFileParams,
            _results: ww::routing_capnp::routing::WriteFileResults,
        ) -> Promise<(), capnp::Error> {
            Promise::err(capnp::Error::unimplemented("test routing".into()))
        }

        fn remove(
            self: capnp::capability::Rc<Self>,
            _params: ww::routing_capnp::routing::RemoveParams,
            _results: ww::routing_capnp::routing::RemoveResults,
        ) -> Promise<(), capnp::Error> {
            Promise::err(capnp::Error::unimplemented("test routing".into()))
        }

        fn publish(
            self: capnp::capability::Rc<Self>,
            _params: ww::routing_capnp::routing::PublishParams,
            _results: ww::routing_capnp::routing::PublishResults,
        ) -> Promise<(), capnp::Error> {
            Promise::err(capnp::Error::unimplemented("test routing".into()))
        }

        fn hash(
            self: capnp::capability::Rc<Self>,
            _params: ww::routing_capnp::routing::HashParams,
            _results: ww::routing_capnp::routing::HashResults,
        ) -> Promise<(), capnp::Error> {
            Promise::err(capnp::Error::unimplemented("test routing".into()))
        }
    }

    struct TestIpfs {
        seen_paths: Rc<RefCell<Vec<String>>>,
    }

    #[allow(refining_impl_trait)]
    impl ww::system_capnp::ipfs::Server for TestIpfs {
        fn read(
            self: capnp::capability::Rc<Self>,
            params: ww::system_capnp::ipfs::ReadParams,
            mut results: ww::system_capnp::ipfs::ReadResults,
        ) -> Promise<(), capnp::Error> {
            if let Ok(p) = params.get() {
                if let Ok(path) = p.get_path() {
                    if let Ok(path_str) = path.to_str() {
                        self.seen_paths.borrow_mut().push(path_str.to_string());
                    }
                }
            }

            let (mut writer, reader) = tokio::io::duplex(1024);
            let stream_client: ww::system_capnp::byte_stream::Client = capnp_rpc::new_client(
                ww::rpc::ByteStreamImpl::new(reader, ww::rpc::StreamMode::ReadOnly),
            );
            results.get().set_stream(stream_client);
            tokio::spawn(async move {
                let _ = writer.write_all(b"abc").await;
                let _ = writer.shutdown().await;
            });
            Promise::ok(())
        }
    }

    fn test_grafted_caps() -> (GraftedShellCaps, Rc<RefCell<Vec<String>>>) {
        let seen_paths = Rc::new(RefCell::new(Vec::new()));
        let caps = GraftedShellCaps {
            host: capnp_rpc::new_client(TestHost),
            routing: capnp_rpc::new_client(TestRouting),
            ipfs: Some(capnp_rpc::new_client(TestIpfs {
                seen_paths: seen_paths.clone(),
            })),
        };
        (caps, seen_paths)
    }

    struct TestMembrane {
        host: ww::system_capnp::host::Client,
        routing: ww::routing_capnp::routing::Client,
        ipfs: Option<ww::system_capnp::ipfs::Client>,
    }

    #[allow(refining_impl_trait)]
    impl ww::membrane_capnp::membrane::Server for TestMembrane {
        fn graft(
            self: capnp::capability::Rc<Self>,
            _params: ww::membrane_capnp::membrane::GraftParams,
            mut results: ww::membrane_capnp::membrane::GraftResults,
        ) -> Promise<(), capnp::Error> {
            let count = if self.ipfs.is_some() { 3 } else { 2 };
            let mut caps = results.get().init_caps(count);

            {
                let mut entry = caps.reborrow().get(0);
                entry.set_name("host");
                entry
                    .init_cap()
                    .set_as_capability(self.host.client.hook.clone());
            }
            {
                let mut entry = caps.reborrow().get(1);
                entry.set_name("routing");
                entry
                    .init_cap()
                    .set_as_capability(self.routing.client.hook.clone());
            }
            if let Some(ipfs) = &self.ipfs {
                let mut entry = caps.reborrow().get(2);
                entry.set_name("ipfs");
                entry.init_cap().set_as_capability(ipfs.client.hook.clone());
            }

            Promise::ok(())
        }
    }

    async fn serve_terminal_streams(
        mut control: libp2p_stream::Control,
        terminal: ww::auth_capnp::terminal::Client<ww::membrane_capnp::membrane::Owned>,
    ) {
        let mut incoming = match control.accept(CAPNP_PROTOCOL) {
            Ok(s) => s,
            Err(_) => return,
        };

        while let Some((_peer_id, stream)) = incoming.next().await {
            let terminal = terminal.clone();
            tokio::task::spawn_local(async move {
                let (reader, writer) = Box::pin(stream).split();
                let network = VatNetwork::new(reader, writer, Side::Server, Default::default());
                let rpc = RpcSystem::new(Box::new(network), Some(terminal.client));
                let _ = rpc.await;
            });
        }
    }

    #[test]
    fn choose_prefers_matching_identity_when_multiple() {
        let p1: PeerId = "12D3KooWJ3qM19qUUj8JdT9kPEg6VZLoes6eexfUYd6Xn7SPrf8n"
            .parse()
            .unwrap();
        let p2: PeerId = "12D3KooWQdQnZYK7hX8Q2Yb8qXWQYvdr4jRWk6TUhSxvVmF5vU3P"
            .parse()
            .unwrap();

        let chosen = choose_candidate(
            vec![
                Candidate {
                    peer_id: Some(p1),
                    addrs: vec![maddr("/ip4/10.0.0.1/tcp/2025")],
                },
                Candidate {
                    peer_id: Some(p2),
                    addrs: vec![maddr("/ip4/10.0.0.2/tcp/2025")],
                },
            ],
            Some(p2),
            None,
            false,
        )
        .unwrap();

        assert_eq!(chosen.peer_id, Some(p2));
    }

    #[test]
    fn choose_errors_on_multiple_without_preference_match() {
        let p1: PeerId = "12D3KooWJ3qM19qUUj8JdT9kPEg6VZLoes6eexfUYd6Xn7SPrf8n"
            .parse()
            .unwrap();
        let p2: PeerId = "12D3KooWQdQnZYK7hX8Q2Yb8qXWQYvdr4jRWk6TUhSxvVmF5vU3P"
            .parse()
            .unwrap();
        let p3: PeerId = "12D3KooWJfUGS8thH9bC4x6hFQ3mFAH3RT6N8gW2H8RyV8Xxwy9A"
            .parse()
            .unwrap();

        let err = choose_candidate(
            vec![
                Candidate {
                    peer_id: Some(p1),
                    addrs: vec![maddr("/ip4/10.0.0.1/tcp/2025")],
                },
                Candidate {
                    peer_id: Some(p2),
                    addrs: vec![maddr("/ip4/10.0.0.2/tcp/2025")],
                },
            ],
            Some(p3),
            None,
            false,
        )
        .unwrap_err()
        .to_string();

        assert!(err.contains("Multiple wetware hosts discovered"), "{err}");
        assert!(err.contains("--select <index|peer-id>"), "{err}");
    }

    #[test]
    fn candidate_from_parts_allows_addr_without_peer_id() {
        let c = candidate_from_parts(None, vec![maddr("/ip4/127.0.0.1/tcp/2025")]).unwrap();
        assert_eq!(c.peer_id, None);
        assert_eq!(c.addrs.len(), 1);
    }

    #[test]
    fn choose_candidate_errors_when_empty() {
        let err = choose_candidate(vec![], None, None, false)
            .unwrap_err()
            .to_string();
        assert!(err.contains("No wetware hosts discovered"), "{err}");
    }

    #[test]
    fn choose_candidate_respects_numeric_selector() {
        let p1: PeerId = "12D3KooWJ3qM19qUUj8JdT9kPEg6VZLoes6eexfUYd6Xn7SPrf8n"
            .parse()
            .unwrap();
        let p2: PeerId = "12D3KooWQdQnZYK7hX8Q2Yb8qXWQYvdr4jRWk6TUhSxvVmF5vU3P"
            .parse()
            .unwrap();

        let chosen = choose_candidate(
            vec![
                Candidate {
                    peer_id: Some(p1),
                    addrs: vec![maddr("/ip4/10.0.0.1/tcp/2025")],
                },
                Candidate {
                    peer_id: Some(p2),
                    addrs: vec![maddr("/ip4/10.0.0.2/tcp/2025")],
                },
            ],
            None,
            Some("2"),
            false,
        )
        .unwrap();

        assert_eq!(chosen.peer_id, Some(p2));
    }

    #[test]
    fn choose_candidate_respects_peer_id_selector() {
        let p1: PeerId = "12D3KooWJ3qM19qUUj8JdT9kPEg6VZLoes6eexfUYd6Xn7SPrf8n"
            .parse()
            .unwrap();
        let p2: PeerId = "12D3KooWQdQnZYK7hX8Q2Yb8qXWQYvdr4jRWk6TUhSxvVmF5vU3P"
            .parse()
            .unwrap();

        let chosen = choose_candidate(
            vec![
                Candidate {
                    peer_id: Some(p1),
                    addrs: vec![maddr("/ip4/10.0.0.1/tcp/2025")],
                },
                Candidate {
                    peer_id: Some(p2),
                    addrs: vec![maddr("/ip4/10.0.0.2/tcp/2025")],
                },
            ],
            None,
            Some("12D3KooWQdQnZYK7hX8Q2Yb8qXWQYvdr4jRWk6TUhSxvVmF5vU3P"),
            false,
        )
        .unwrap();

        assert_eq!(chosen.peer_id, Some(p2));
    }

    #[test]
    fn choose_candidate_rejects_invalid_selector() {
        let p1: PeerId = "12D3KooWJ3qM19qUUj8JdT9kPEg6VZLoes6eexfUYd6Xn7SPrf8n"
            .parse()
            .unwrap();
        let p2: PeerId = "12D3KooWQdQnZYK7hX8Q2Yb8qXWQYvdr4jRWk6TUhSxvVmF5vU3P"
            .parse()
            .unwrap();

        let err = choose_candidate(
            vec![
                Candidate {
                    peer_id: Some(p1),
                    addrs: vec![maddr("/ip4/10.0.0.1/tcp/2025")],
                },
                Candidate {
                    peer_id: Some(p2),
                    addrs: vec![maddr("/ip4/10.0.0.2/tcp/2025")],
                },
            ],
            None,
            Some("99"),
            false,
        )
        .unwrap_err()
        .to_string();

        assert!(err.contains("out of range"), "{err}");
    }

    #[test]
    fn peer_id_from_addr_extracts_when_present() {
        let peer_id: PeerId = "12D3KooWJ3qM19qUUj8JdT9kPEg6VZLoes6eexfUYd6Xn7SPrf8n"
            .parse()
            .unwrap();
        let addr = maddr(&format!("/ip4/127.0.0.1/tcp/2025/p2p/{peer_id}"));
        assert_eq!(peer_id_from_addr(&addr), Some(peer_id));
    }

    #[test]
    fn peer_id_from_addr_returns_none_without_p2p() {
        let addr = maddr("/ip4/127.0.0.1/tcp/2025");
        assert_eq!(peer_id_from_addr(&addr), None);
    }

    #[test]
    fn parse_discovery_candidates_json_parses_valid_input() {
        let input = r#"[
            {
                "peer_id": "12D3KooWJ3qM19qUUj8JdT9kPEg6VZLoes6eexfUYd6Xn7SPrf8n",
                "addrs": ["/ip4/127.0.0.1/tcp/2025"]
            }
        ]"#;

        let parsed = parse_discovery_candidates_json(input).unwrap();
        assert_eq!(parsed.len(), 1);
        assert_eq!(parsed[0].addrs.len(), 1);
        assert_eq!(
            parsed[0].addrs[0].to_string(),
            "/ip4/127.0.0.1/tcp/2025".to_string()
        );
    }

    #[test]
    fn parse_discovery_candidates_json_rejects_invalid_multiaddr() {
        let input = r#"[
            {
                "peer_id": "12D3KooWJ3qM19qUUj8JdT9kPEg6VZLoes6eexfUYd6Xn7SPrf8n",
                "addrs": ["not-a-multiaddr"]
            }
        ]"#;

        let err = parse_discovery_candidates_json(input)
            .unwrap_err()
            .to_string();
        assert!(err.contains("invalid multiaddr"), "{err}");
    }

    #[test]
    fn choose_prefers_local_candidate_over_relay() {
        let local_peer: PeerId = "12D3KooWJ3qM19qUUj8JdT9kPEg6VZLoes6eexfUYd6Xn7SPrf8n"
            .parse()
            .unwrap();
        let relay_peer: PeerId = "12D3KooWQdQnZYK7hX8Q2Yb8qXWQYvdr4jRWk6TUhSxvVmF5vU3P"
            .parse()
            .unwrap();

        let chosen = choose_candidate(
            vec![
                Candidate {
                    peer_id: Some(relay_peer),
                    addrs: vec![maddr(
                        "/ip4/23.92.30.240/tcp/4001/p2p/12D3KooWQ9jBCBSvw13vETKe8sUVUx1y878qNrneNGTrHdKxenp1/p2p-circuit",
                    )],
                },
                Candidate {
                    peer_id: Some(local_peer),
                    addrs: vec![maddr("/ip4/192.168.1.44/tcp/2025")],
                },
            ],
            None,
            None,
            false,
        )
        .unwrap();

        assert_eq!(chosen.peer_id, Some(local_peer));
    }

    #[test]
    fn choose_prefers_loopback_over_lan() {
        let lan_peer: PeerId = "12D3KooWJ3qM19qUUj8JdT9kPEg6VZLoes6eexfUYd6Xn7SPrf8n"
            .parse()
            .unwrap();
        let loopback_peer: PeerId = "12D3KooWQdQnZYK7hX8Q2Yb8qXWQYvdr4jRWk6TUhSxvVmF5vU3P"
            .parse()
            .unwrap();

        let chosen = choose_candidate(
            vec![
                Candidate {
                    peer_id: Some(lan_peer),
                    addrs: vec![maddr("/ip4/10.0.0.1/tcp/2025")],
                },
                Candidate {
                    peer_id: Some(loopback_peer),
                    addrs: vec![maddr("/ip4/127.0.0.1/tcp/2025")],
                },
            ],
            None,
            None,
            false,
        )
        .unwrap();

        assert_eq!(chosen.peer_id, Some(loopback_peer));
    }

    #[tokio::test]
    async fn await_connected_peer_returns_peer_on_success() {
        let expected: PeerId = "12D3KooWJ3qM19qUUj8JdT9kPEg6VZLoes6eexfUYd6Xn7SPrf8n"
            .parse()
            .unwrap();
        let (tx, rx) = oneshot::channel::<Result<PeerId, String>>();
        tx.send(Ok(expected)).unwrap();

        let got = await_connected_peer(rx).await.unwrap();
        assert_eq!(got, expected);
    }

    #[tokio::test]
    async fn await_connected_peer_surfaces_dial_failure() {
        let (tx, rx) = oneshot::channel::<Result<PeerId, String>>();
        tx.send(Err("dial failed".to_string())).unwrap();

        let err = await_connected_peer(rx).await.unwrap_err().to_string();
        assert!(err.contains("dial failed"), "{err}");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn local_runtime_initializes_without_runtime_cap_and_evals_arithmetic() {
        let (caps, _seen_paths) = test_grafted_caps();
        let mut runtime = build_local_shell_runtime(caps).await;
        match shell_eval(&mut runtime, "(+ 1 2)").await.unwrap() {
            ShellEvalResult::Value(result) => assert_eq!(result, "3"),
            ShellEvalResult::Error(err) => panic!("unexpected eval error: {err}"),
            ShellEvalResult::Exit => panic!("unexpected exit"),
        }
    }

    #[tokio::test(flavor = "current_thread")]
    async fn local_runtime_dispatches_host_calls_through_grafted_cap() {
        use base58::ToBase58;
        let (caps, _seen_paths) = test_grafted_caps();
        let mut runtime = build_local_shell_runtime(caps).await;
        match shell_eval(&mut runtime, "(perform host :id)")
            .await
            .unwrap()
        {
            ShellEvalResult::Value(result) => {
                assert_eq!(result, format!("\"{}\"", TEST_PEER_ID_BYTES.to_base58()));
            }
            ShellEvalResult::Error(err) => panic!("unexpected eval error: {err}"),
            ShellEvalResult::Exit => panic!("unexpected exit"),
        }
    }

    #[tokio::test(flavor = "current_thread")]
    async fn local_runtime_routes_ipfs_loads_through_grafted_ipfs_cap() {
        let (caps, seen_paths) = test_grafted_caps();
        let mut runtime = build_local_shell_runtime(caps).await;
        match shell_eval(&mut runtime, "(perform :load \"/ipfs/bafy-test/path\")")
            .await
            .unwrap()
        {
            ShellEvalResult::Value(result) => assert_eq!(result, "<3 bytes>"),
            ShellEvalResult::Error(err) => panic!("unexpected eval error: {err}"),
            ShellEvalResult::Exit => panic!("unexpected exit"),
        }
        assert_eq!(
            seen_paths.borrow().as_slice(),
            &["/ipfs/bafy-test/path".to_string()]
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn local_repl_exit_is_a_control_outcome() {
        let (caps, _seen_paths) = test_grafted_caps();
        let mut runtime = build_local_shell_runtime(caps).await;
        assert!(matches!(
            shell_eval(&mut runtime, "(perform :exit nil)")
                .await
                .unwrap(),
            ShellEvalResult::Exit
        ));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn mcp_mode_rejects_stdout_and_exit_with_typed_errors() {
        let (caps, _seen_paths) = test_grafted_caps();
        let mut runtime = build_local_shell_runtime(caps).await;

        for (expr, effect) in [
            ("(perform :stdout \"protocol corruption\")", "stdout"),
            ("(perform :exit nil)", "exit"),
        ] {
            let err = shell_eval_raw(&mut runtime, expr, ShellEffectMode::Mcp)
                .await
                .unwrap()
                .unwrap_err();
            assert_eq!(
                glia::error::type_tag(&err),
                Some("glia.error/protocol-mode-unavailable")
            );
            assert_eq!(
                mcp_adapter::val_to_mcp_error_data(&err)
                    .get("effect")
                    .and_then(|value| value.as_str()),
                Some(effect)
            );
        }
    }

    #[tokio::test(flavor = "current_thread")]
    async fn shell_dial_login_graft_and_local_eval_smoke() {
        let local = tokio::task::LocalSet::new();
        local
            .run_until(async {
                let server_signing_key = ww::keys::generate().expect("generate signing key");
                let server_libp2p_key =
                    ww::keys::to_libp2p(&server_signing_key).expect("convert key");

                let listen_addr: Multiaddr = "/ip4/127.0.0.1/tcp/0".parse().unwrap();
                let host = ww::host::Libp2pHost::new(
                    vec![listen_addr],
                    server_libp2p_key,
                    None,
                    Vec::new(),
                )
                .expect("start host");
                let peer_id = host.local_peer_id();
                let stream_control = host.stream_control();

                let network_state = ww::rpc::NetworkState::from_peer_id(peer_id.to_bytes());
                let host_network_state = network_state.clone();
                let (_swarm_tx, swarm_rx) = mpsc::channel(4);
                let host_task = tokio::task::spawn_local(async move {
                    host.run(host_network_state, swarm_rx).await
                });

                let (grafted, _seen_paths) = test_grafted_caps();
                let membrane: ww::membrane_capnp::membrane::Client =
                    capnp_rpc::new_client(TestMembrane {
                        host: grafted.host,
                        routing: grafted.routing,
                        ipfs: grafted.ipfs,
                    });

                let epoch = authority::Epoch {
                    seq: 1,
                    head: b"head".to_vec(),
                    provenance: authority::Provenance::Block(0),
                };
                let (_epoch_tx, epoch_rx) = watch::channel(epoch);
                let terminal_server = TerminalServer::<ww::membrane_capnp::membrane::Owned>::new(
                    server_signing_key.verifying_key(),
                    membrane,
                    auth::SigningDomain::terminal_membrane(),
                    epoch_rx,
                );
                let terminal_client = capnp_rpc::new_client(terminal_server);
                let terminal_task = tokio::task::spawn_local(serve_terminal_streams(
                    stream_control,
                    terminal_client,
                ));

                let dial_addr = {
                    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
                    loop {
                        let snapshot = network_state.snapshot().await;
                        if let Some(raw_addr) = snapshot.listen_addrs.first() {
                            let addr =
                                Multiaddr::try_from(raw_addr.clone()).expect("decode listen addr");
                            break addr;
                        }
                        if tokio::time::Instant::now() >= deadline {
                            panic!("host did not publish listen address");
                        }
                        tokio::time::sleep(Duration::from_millis(50)).await;
                    }
                };

                let target = Candidate {
                    peer_id: Some(peer_id),
                    addrs: vec![dial_addr],
                };
                let caps = dial_shell(&target, &server_signing_key)
                    .await
                    .expect("dial/login/graft");
                let mut runtime = build_local_shell_runtime(caps).await;
                match shell_eval(&mut runtime, "(+ 1 2)").await.unwrap() {
                    ShellEvalResult::Value(result) => assert_eq!(result, "3"),
                    ShellEvalResult::Error(err) => panic!("unexpected eval error: {err}"),
                    ShellEvalResult::Exit => panic!("unexpected exit"),
                }

                host_task.abort();
                terminal_task.abort();
            })
            .await;
    }

    #[test]
    fn shell_connect_path_does_not_include_remote_shell_bootstrap_calls() {
        let src = include_str!("shell.rs");
        let start = src
            .find("async fn dial_shell")
            .expect("dial_shell function should exist");
        let end = src
            .find("async fn await_connected_peer")
            .expect("await_connected_peer should exist");
        let dial_body = &src[start..end];
        assert!(
            !dial_body.contains("load_request("),
            "runtime.load path detected"
        );
        assert!(
            !dial_body.contains("spawn_request("),
            "executor.spawn path detected"
        );
        assert!(
            !dial_body.contains("bootstrap_request("),
            "process.bootstrap path detected"
        );
    }

    #[test]
    fn mcp_tool_host_id_maps_to_glia_expression() {
        let args = serde_json::json!({ "action": "id" });
        let expr = mcp_tool_to_glia("host", &args);
        assert_eq!(expr, Some("(perform host :id)".to_string()));
    }

    #[test]
    fn mcp_tool_routing_uses_shell_actions() {
        let provide = mcp_tool_to_glia(
            "routing",
            &serde_json::json!({ "action": "provide", "key": "QmFoo" }),
        );
        assert_eq!(
            provide,
            Some(r#"(perform routing :provide "QmFoo")"#.into())
        );

        let resolve = mcp_tool_to_glia(
            "routing",
            &serde_json::json!({ "action": "resolve", "name": "/ipns/example" }),
        );
        assert_eq!(
            resolve,
            Some(r#"(perform routing :resolve "/ipns/example")"#.into())
        );

        let hash = mcp_tool_to_glia(
            "routing",
            &serde_json::json!({ "action": "hash", "data": "payload" }),
        );
        assert_eq!(hash, Some(r#"(perform routing :hash "payload")"#.into()));

        let standalone_only = mcp_tool_to_glia(
            "routing",
            &serde_json::json!({ "action": "find_providers", "cid": "QmFoo" }),
        );
        assert_eq!(standalone_only, None);
    }

    #[test]
    fn mcp_tool_import_uses_shell_expression() {
        let expr = mcp_tool_to_glia("import", &serde_json::json!({ "path": "core" }));
        assert_eq!(expr, Some(r#"(perform import "core")"#.into()));
    }

    #[test]
    fn mcp_tool_escapes_shell_user_input() {
        let expr = mcp_tool_to_glia(
            "routing",
            &serde_json::json!({ "action": "hash", "data": r#"payload") (evil"# }),
        );
        assert_eq!(
            expr,
            Some(r#"(perform routing :hash "payload\") (evil")"#.into())
        );
    }

    #[test]
    fn mcp_tool_rejects_unsafe_action_identifier() {
        let args = serde_json::json!({ "action": "id) (evil" });
        let expr = mcp_tool_to_glia("host", &args);
        assert_eq!(expr, None);
    }
}
