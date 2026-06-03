//! MCP cell — JSON-RPC server for AI agent integration.
//!
//! A raw cell that speaks MCP (Model Context Protocol) over WASI
//! stdin/stdout.  Grafts the membrane to obtain capabilities, sets
//! up a Glia evaluator, and serves JSON-RPC requests.
//!
//! Per-capability tools make Glia's eval surface legible to AI agents.
//! `eval` remains the primary interface; per-cap tools are the discovery
//! layer.  Each tool call translates to a Glia expression internally.
//!
//! ```text
//! Claude Code -> stdin/stdout -> MCP cell (WASM) -> Glia eval -> membrane caps
//! ```

use std::cell::RefCell;
use std::collections::HashMap;
use std::future::Future;
use std::io::Write;
use std::pin::Pin;
use std::rc::Rc;

use glia::eval::{self, Dispatch, Env};
use glia::{make_cap, Val};

use wasip2::exports::cli::run::Guest;

// Shared effect handler factories from the caps crate.
use caps::{
    eval_load_async, get_graft_cap, make_host_handler, make_import_handler, make_routing_handler,
    mcp_adapter, membrane_capnp, routing_capnp, system_capnp, wrap_with_handlers,
};
use mcp_adapter::{ActionPolicy, ExprPart, ToolAction, ToolSpec};

type Membrane = membrane_capnp::membrane::Client;

// ---------------------------------------------------------------------------
// JSON-RPC types (minimal, hand-rolled to avoid pulling in jsonrpc crate)
// ---------------------------------------------------------------------------

/// Incoming JSON-RPC request.
#[derive(serde::Deserialize)]
struct JsonRpcRequest {
    #[allow(dead_code)]
    jsonrpc: Option<String>,
    method: String,
    params: Option<serde_json::Value>,
    id: Option<serde_json::Value>,
}

/// Write a JSON-RPC success response to stdout.
fn write_result(id: &serde_json::Value, result: serde_json::Value) {
    let resp = serde_json::json!({
        "jsonrpc": "2.0",
        "result": result,
        "id": id,
    });
    let mut out = std::io::stdout();
    let _ = serde_json::to_writer(&mut out, &resp);
    let _ = out.write_all(b"\n");
    let _ = out.flush();
}

/// Write a JSON-RPC error response to stdout.
fn write_error(id: &serde_json::Value, code: i64, message: &str) {
    let resp = serde_json::json!({
        "jsonrpc": "2.0",
        "error": {
            "code": code,
            "message": message,
        },
        "id": id,
    });
    let mut out = std::io::stdout();
    let _ = serde_json::to_writer(&mut out, &resp);
    let _ = out.write_all(b"\n");
    let _ = out.flush();
}

// ---------------------------------------------------------------------------
// MCP protocol constants
// ---------------------------------------------------------------------------

const PROTOCOL_VERSION: &str = "2024-11-05";
const SERVER_NAME: &str = "wetware";
const SERVER_VERSION: &str = "0.1.0";

const MCP_HOST_ID_EXPR: &[ExprPart] = &[ExprPart::Literal("(perform host :id)")];
const MCP_HOST_PEERS_EXPR: &[ExprPart] = &[ExprPart::Literal("(perform host :peers)")];
const MCP_HOST_ADDRS_EXPR: &[ExprPart] = &[ExprPart::Literal("(perform host :addrs)")];
const MCP_HOST_ACTIONS: &[ToolAction] = &[
    ToolAction {
        action: Some("id"),
        template: MCP_HOST_ID_EXPR,
    },
    ToolAction {
        action: Some("peers"),
        template: MCP_HOST_PEERS_EXPR,
    },
    ToolAction {
        action: Some("addrs"),
        template: MCP_HOST_ADDRS_EXPR,
    },
];

const MCP_ROUTING_PROVIDE_EXPR: &[ExprPart] = &[
    ExprPart::Literal("(perform routing :provide (bytes "),
    ExprPart::QuotedStringField {
        field: "cid",
        default: "",
    },
    ExprPart::Literal("))"),
];
const MCP_ROUTING_FIND_PROVIDERS_EXPR: &[ExprPart] = &[
    ExprPart::Literal("(perform routing :find-providers (bytes "),
    ExprPart::QuotedStringField {
        field: "cid",
        default: "",
    },
    ExprPart::Literal("))"),
];
const MCP_ROUTING_ACTIONS: &[ToolAction] = &[
    ToolAction {
        action: Some("provide"),
        template: MCP_ROUTING_PROVIDE_EXPR,
    },
    ToolAction {
        action: Some("find_providers"),
        template: MCP_ROUTING_FIND_PROVIDERS_EXPR,
    },
];

const MCP_RUNTIME_RUN_EXPR: &[ExprPart] = &[
    ExprPart::Literal("(perform runtime :run (load "),
    ExprPart::QuotedStringField {
        field: "wasm_path",
        default: "",
    },
    ExprPart::Literal("))"),
];
const MCP_RUNTIME_ACTIONS: &[ToolAction] = &[ToolAction {
    action: Some("run"),
    template: MCP_RUNTIME_RUN_EXPR,
}];

const MCP_IDENTITY_SIGN_EXPR: &[ExprPart] = &[
    ExprPart::Literal("(perform identity :sign "),
    ExprPart::QuotedStringField {
        field: "domain",
        default: "default",
    },
    ExprPart::Literal(" "),
    ExprPart::U64Field {
        field: "nonce",
        default: 0,
    },
    ExprPart::Literal(")"),
];
const MCP_IDENTITY_VERIFY_EXPR: &[ExprPart] = &[
    ExprPart::Literal("(perform identity :verify (bytes "),
    ExprPart::QuotedStringField {
        field: "data",
        default: "",
    },
    ExprPart::Literal(") (bytes "),
    ExprPart::QuotedStringField {
        field: "signature",
        default: "",
    },
    ExprPart::Literal(") (bytes "),
    ExprPart::QuotedStringField {
        field: "pubkey",
        default: "",
    },
    ExprPart::Literal("))"),
];
const MCP_IDENTITY_ACTIONS: &[ToolAction] = &[
    ToolAction {
        action: Some("sign"),
        template: MCP_IDENTITY_SIGN_EXPR,
    },
    ToolAction {
        action: Some("verify"),
        template: MCP_IDENTITY_VERIFY_EXPR,
    },
];

const MCP_HTTP_GET_EXPR: &[ExprPart] = &[
    ExprPart::Literal("(perform http-client :get "),
    ExprPart::QuotedStringField {
        field: "url",
        default: "",
    },
    ExprPart::Literal(")"),
];
const MCP_HTTP_POST_EXPR: &[ExprPart] = &[
    ExprPart::Literal("(perform http-client :post "),
    ExprPart::QuotedStringField {
        field: "url",
        default: "",
    },
    ExprPart::Literal(" "),
    ExprPart::QuotedStringField {
        field: "body",
        default: "",
    },
    ExprPart::Literal(")"),
];
const MCP_HTTP_ACTIONS: &[ToolAction] = &[
    ToolAction {
        action: Some("get"),
        template: MCP_HTTP_GET_EXPR,
    },
    ToolAction {
        action: Some("post"),
        template: MCP_HTTP_POST_EXPR,
    },
];

const MCP_IMPORT_EXPR: &[ExprPart] = &[
    ExprPart::Literal("(def imported (perform import "),
    ExprPart::QuotedStringField {
        field: "path",
        default: "",
    },
    ExprPart::Literal("))"),
];
const MCP_IMPORT_ACTIONS: &[ToolAction] = &[ToolAction {
    action: None,
    template: MCP_IMPORT_EXPR,
}];

const MCP_TOOL_SPECS: &[ToolSpec] = &[
    ToolSpec {
        name: "host",
        action_policy: ActionPolicy::RequiredSafe,
        actions: MCP_HOST_ACTIONS,
    },
    ToolSpec {
        name: "routing",
        action_policy: ActionPolicy::RequiredSafe,
        actions: MCP_ROUTING_ACTIONS,
    },
    ToolSpec {
        name: "runtime",
        action_policy: ActionPolicy::RequiredSafe,
        actions: MCP_RUNTIME_ACTIONS,
    },
    ToolSpec {
        name: "identity",
        action_policy: ActionPolicy::RequiredSafe,
        actions: MCP_IDENTITY_ACTIONS,
    },
    ToolSpec {
        name: "http-client",
        action_policy: ActionPolicy::RequiredSafe,
        actions: MCP_HTTP_ACTIONS,
    },
    ToolSpec {
        name: "import",
        action_policy: ActionPolicy::Ignore,
        actions: MCP_IMPORT_ACTIONS,
    },
];

fn initialize_result() -> serde_json::Value {
    serde_json::json!({
        "protocolVersion": PROTOCOL_VERSION,
        "serverInfo": {
            "name": SERVER_NAME,
            "version": SERVER_VERSION,
        },
        "capabilities": {
            "tools": {},
        },
    })
}

// ---------------------------------------------------------------------------
// Dynamic tool generation from membrane capabilities
// ---------------------------------------------------------------------------

/// Build tool definitions for known capabilities.  Returns per-action schemas
/// that teach MCP clients what each capability can do.
fn tool_def_for_cap(name: &str) -> Option<serde_json::Value> {
    match name {
        "host" => Some(serde_json::json!({
            "name": "host",
            "description": "Node identity and peer management. Actions: id (peer identity), peers (connected peers), addrs (listen addresses).",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "action": {
                        "type": "string",
                        "enum": ["id", "peers", "addrs"],
                        "description": "The operation to perform"
                    }
                },
                "required": ["action"]
            }
        })),
        "routing" => Some(serde_json::json!({
            "name": "routing",
            "description": "DHT content routing (Kademlia). Actions: provide (announce a CID), find_providers (find peers hosting a CID).",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "action": {
                        "type": "string",
                        "enum": ["provide", "find_providers"],
                        "description": "The routing operation"
                    },
                    "cid": {
                        "type": "string",
                        "description": "Content identifier (CID)"
                    }
                },
                "required": ["action", "cid"]
            }
        })),
        "runtime" => Some(serde_json::json!({
            "name": "runtime",
            "description": "Load and execute WASM binaries as cells.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "action": {
                        "type": "string",
                        "enum": ["run"],
                        "description": "The runtime operation"
                    },
                    "wasm_path": {
                        "type": "string",
                        "description": "Path to WASM binary in the FHS image (e.g. bin/myapp.wasm)"
                    }
                },
                "required": ["action", "wasm_path"]
            }
        })),
        "identity" => Some(serde_json::json!({
            "name": "identity",
            "description": "Ed25519 cryptographic operations. Actions: sign (sign a nonce with a domain-scoped key), verify (verify a signature against a public key).",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "action": {
                        "type": "string",
                        "enum": ["sign", "verify"],
                        "description": "The cryptographic operation"
                    },
                    "domain": {
                        "type": "string",
                        "description": "Signing domain (for sign action)"
                    },
                    "nonce": {
                        "type": "integer",
                        "description": "Nonce to sign (for sign action)"
                    },
                    "data": {
                        "type": "string",
                        "description": "Hex-encoded data to verify (for verify action)"
                    },
                    "signature": {
                        "type": "string",
                        "description": "Hex-encoded signature (for verify action)"
                    },
                    "pubkey": {
                        "type": "string",
                        "description": "Hex-encoded Ed25519 public key (for verify action)"
                    }
                },
                "required": ["action"]
            }
        })),
        "http-client" => Some(serde_json::json!({
            "name": "http-client",
            "description": "Outbound HTTP requests. Actions: get (HTTP GET), post (HTTP POST with body).",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "action": {
                        "type": "string",
                        "enum": ["get", "post"],
                        "description": "HTTP method"
                    },
                    "url": {
                        "type": "string",
                        "description": "Request URL"
                    },
                    "body": {
                        "type": "string",
                        "description": "Request body (for post action)"
                    }
                },
                "required": ["action", "url"]
            }
        })),
        "import" => Some(serde_json::json!({
            "name": "import",
            "description": "Load a Glia module by path. Returns the module's exported bindings.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "path": {
                        "type": "string",
                        "description": "Module path (e.g. 'core' resolves to /lib/core.glia)"
                    }
                },
                "required": ["path"]
            }
        })),
        _ => None,
    }
}

/// The eval tool — always present as the primary power interface.
fn eval_tool_def() -> serde_json::Value {
    serde_json::json!({
        "name": "eval",
        "description": "Evaluate a Glia s-expression on the Wetware node. This is the primary interface — use for complex operations, scripting, multi-step workflows, and anything not covered by other tools. Examples: (perform host :id), (def x (perform host :peers)), (help)",
        "inputSchema": {
            "type": "object",
            "properties": {
                "expression": {
                    "type": "string",
                    "description": "Glia s-expression to evaluate"
                }
            },
            "required": ["expression"]
        }
    })
}

/// Build the tools list from the grafted capabilities.
///
/// Only known capabilities get per-cap tools.  Unknown caps are
/// accessible through `eval` — we don't generate tools from
/// untrusted capability names.
fn build_tools_list(cap_names: &[String]) -> serde_json::Value {
    let mut tools: Vec<serde_json::Value> = Vec::new();

    for name in cap_names {
        if let Some(def) = tool_def_for_cap(name) {
            tools.push(def);
        }
        // Unknown caps: no tool.  Use eval to interact.
    }

    // eval is always last — the primary power interface.
    tools.push(eval_tool_def());

    serde_json::json!({ "tools": tools })
}

/// Translate a per-cap tool call into a Glia expression for eval.
///
/// Only known capabilities are supported.  Unknown caps must use the `eval` tool
/// directly — we do not generate expressions from untrusted capability names.
fn tool_call_to_glia(tool_name: &str, args: &serde_json::Value) -> Option<String> {
    mcp_adapter::tool_call_to_glia(MCP_TOOL_SPECS, tool_name, args)
}

// ---------------------------------------------------------------------------
// Dispatch — delegates to shared caps crate handlers
// ---------------------------------------------------------------------------

type HandlerFn = for<'a> fn(
    &'a [Val],
    &'a RefCell<McpSession>,
) -> Pin<Box<dyn Future<Output = Result<Val, Val>> + 'a>>;

struct McpSession {
    #[allow(dead_code)]
    host: Option<system_capnp::host::Client>,
    #[allow(dead_code)]
    routing: Option<routing_capnp::routing::Client>,
}

struct McpDispatch<'s> {
    ctx: &'s RefCell<McpSession>,
    table: &'s HashMap<&'static str, HandlerFn>,
}

impl<'s> Dispatch for McpDispatch<'s> {
    fn call<'a>(
        &'a self,
        name: &'a str,
        args: &'a [Val],
    ) -> Pin<Box<dyn Future<Output = Result<Val, Val>> + 'a>> {
        Box::pin(async move {
            match self.table.get(name) {
                Some(handler) => handler(args, self.ctx).await,
                None => Err(Val::from(format!("{name}: command not found"))),
            }
        })
    }
}

fn build_dispatch() -> HashMap<&'static str, HandlerFn> {
    let mut t: HashMap<&'static str, HandlerFn> = HashMap::new();
    t.insert("load", |a, _| Box::pin(eval_load_async(a)));
    t.insert("help", |_, _| {
        Box::pin(std::future::ready(Ok(Val::Str(HELP_TEXT.to_string()))))
    });
    t
}

const HELP_TEXT: &str = "\
MCP Glia evaluator. Available commands:
  (perform host :id)       - peer identity
  (perform host :addrs)    - listen addresses
  (perform host :peers)    - connected peers
  (perform routing :find \"name\") - DHT lookup
  (help)                   - this message";

// ---------------------------------------------------------------------------
// MCP JSON-RPC server loop
// ---------------------------------------------------------------------------

/// Evaluate a Glia expression. Returns the formatted result on success;
/// preserves the error `Val` on failure so the caller can route it through
/// the structured-error MCP envelope adapter.
async fn eval_expression(
    expr_text: &str,
    env: &mut Env,
    ctx: &RefCell<McpSession>,
    dispatch_table: &HashMap<&'static str, HandlerFn>,
) -> Result<String, Val> {
    let expr = glia::read(expr_text).map_err(|e| glia::error::parse(None, e))?;

    let wrapped = wrap_with_handlers(&expr, &[]);
    let dispatch = McpDispatch {
        ctx,
        table: dispatch_table,
    };
    match eval::eval_toplevel(&wrapped, env, &dispatch).await {
        Ok(Val::Nil) => Ok("nil".to_string()),
        Ok(result) => Ok(format!("{result}")),
        Err(e) => Err(e),
    }
}

/// Handle a single JSON-RPC request and write the response to stdout.
///
/// Returns `true` to continue the loop, `false` on exit conditions.
async fn handle_request(
    line: &str,
    env: &mut Env,
    ctx: &RefCell<McpSession>,
    dispatch_table: &HashMap<&'static str, HandlerFn>,
    tools_list: &serde_json::Value,
    cap_names: &[String],
) -> bool {
    let req: JsonRpcRequest = match serde_json::from_str(line) {
        Ok(r) => r,
        Err(e) => {
            let null_id = serde_json::Value::Null;
            write_error(&null_id, -32700, &format!("Parse error: {e}"));
            return true;
        }
    };

    let id = match req.id {
        Some(ref id) => id.clone(),
        None => return true,
    };

    match req.method.as_str() {
        "initialize" => {
            write_result(&id, initialize_result());
        }
        "ping" => {
            write_result(&id, serde_json::json!({}));
        }
        "tools/list" => {
            write_result(&id, tools_list.clone());
        }
        "tools/call" => {
            let params = req.params.unwrap_or(serde_json::Value::Null);
            let tool_name = params.get("name").and_then(|v| v.as_str()).unwrap_or("");
            let arguments = params
                .get("arguments")
                .cloned()
                .unwrap_or(serde_json::Value::Null);

            if tool_name == "eval" {
                // Direct eval path.
                let expression = arguments
                    .get("expression")
                    .and_then(|v| v.as_str())
                    .unwrap_or("");

                if expression.is_empty() {
                    write_tool_error(&id, "empty expression", &serde_json::Value::Null);
                } else {
                    match eval_expression(expression, env, ctx, dispatch_table).await {
                        Ok(result) => write_tool_result(&id, &result),
                        Err(err) => {
                            let text = mcp_adapter::val_to_mcp_error_text(&err);
                            let data = mcp_adapter::val_to_mcp_error_data(&err);
                            write_tool_error(&id, &text, &data);
                        }
                    }
                }
            } else if cap_names.iter().any(|n| n == tool_name)
                || tool_def_for_cap(tool_name).is_some()
            {
                // Per-cap tool: translate to Glia expression and eval.
                match tool_call_to_glia(tool_name, &arguments) {
                    Some(expr) => match eval_expression(&expr, env, ctx, dispatch_table).await {
                        Ok(result) => write_tool_result(&id, &result),
                        Err(err) => {
                            let action = arguments
                                .get("action")
                                .and_then(|v| v.as_str())
                                .unwrap_or("unknown");
                            let text = format!(
                                    "{}\n\nhint: capability '{tool_name}' action '{action}' failed. Try: (perform {tool_name} :{action})",
                                    mcp_adapter::val_to_mcp_error_text(&err),
                                );
                            let data = mcp_adapter::val_to_mcp_error_data(&err);
                            write_tool_error(&id, &text, &data);
                        }
                    },
                    None => {
                        let action = arguments
                            .get("action")
                            .and_then(|v| v.as_str())
                            .unwrap_or("(none)");
                        write_tool_error(
                            &id,
                            &format!("Unknown action '{action}' for capability '{tool_name}'"),
                            &serde_json::Value::Null,
                        );
                    }
                }
            } else {
                write_error(&id, -32602, &format!("Unknown tool: {tool_name}"));
            }
        }
        _ => {
            write_error(&id, -32601, &format!("Method not found: {}", req.method));
        }
    }

    true
}

/// Write a successful tool call result.
///
/// If the `WW_CELL_CID` environment variable is set, prepends
/// `[CID: <value>]` to the response text so AI agents can observe
/// content-addressed provenance of the executing WASM.
fn write_tool_result(id: &serde_json::Value, text: &str) {
    let annotated = match std::env::var("WW_CELL_CID") {
        Ok(cid) if !cid.is_empty() => format!("[CID: {cid}]\n\n{text}"),
        _ => text.to_string(),
    };
    write_result(
        id,
        serde_json::json!({
            "content": [{"type": "text", "text": annotated}],
        }),
    );
}

/// Write a tool call error result.
///
/// `data` is attached as a `structuredContent` field for clients that
/// can introspect machine-readable error fields. Pass
/// `&serde_json::Value::Null` for legacy / unstructured errors.
fn write_tool_error(id: &serde_json::Value, message: &str, data: &serde_json::Value) {
    let mut payload = serde_json::Map::new();
    payload.insert(
        "content".into(),
        serde_json::json!([{"type": "text", "text": message}]),
    );
    payload.insert("isError".into(), serde_json::Value::Bool(true));
    if !data.is_null() {
        payload.insert("structuredContent".into(), data.clone());
    }
    write_result(id, serde_json::Value::Object(payload));
}

// ---------------------------------------------------------------------------
// Entry point
// ---------------------------------------------------------------------------

struct McpCell;

impl Guest for McpCell {
    fn run() -> Result<(), ()> {
        run_impl();
        Ok(())
    }
}

wasip2::cli::command::export!(McpCell);

fn run_impl() {
    use futures::io::{AsyncBufReadExt, BufReader};

    let ctx = Rc::new(RefCell::new(McpSession {
        host: None,
        routing: None,
    }));
    let dispatch_table = Rc::new(build_dispatch());

    // Register stdin in the poll set so poll_loop can service it
    // concurrently with the RPC transport (WIT channel).
    let stdin_stream = wasip2::cli::stdin::get_stdin();
    let stdin_reader = system::StreamReader::new(stdin_stream);
    let mut poll_set = system::PollSet::new();
    poll_set.push(stdin_reader.pollable());

    // Connect to the membrane via the WIT streams connection.
    // stdin/stdout remain free for JSON-RPC I/O. The poll set
    // ensures poll_loop wakes on stdin data as well as RPC messages.
    system::run_with(poll_set, |membrane: Membrane| {
        let ctx = Rc::clone(&ctx);
        let dispatch_table = Rc::clone(&dispatch_table);

        async move {
            let mut env = Env::new();

            // 1. Graft the membrane to obtain capabilities.
            let graft_resp = membrane.graft_request().send().promise.await?;
            let results = graft_resp.get()?;
            let caps = results.get_caps()?;

            // Extract capability names from the graft for dynamic tool generation.
            let cap_names: Vec<String> = (0..caps.len())
                .filter_map(|i| {
                    caps.get(i)
                        .get_name()
                        .ok()
                        .and_then(|s| s.to_string().ok())
                })
                .collect();

            let host: system_capnp::host::Client = get_graft_cap(&caps, "host")?;
            let routing: routing_capnp::routing::Client = get_graft_cap(&caps, "routing")?;

            // Build the dynamic tools list from grafted capabilities.
            let tools_list = build_tools_list(&cap_names);

            // Populate session.
            {
                let mut s = ctx.borrow_mut();
                s.host = Some(host.clone());
                s.routing = Some(routing.clone());
            }

            // 2. Bind cap values + effect handlers into the environment.
            //    Uses shared handler factories from the caps crate.
            //    Each cap must be Val::Cap (not Val::Nil) so that
            //    with-effect-handler can match on it.
            {
                let cap_handlers: [(&str, Val); 3] = [
                    ("host", make_host_handler(host)),
                    ("routing", make_routing_handler(routing)),
                    ("import", make_import_handler()),
                ];
                for (name, handler) in cap_handlers {
                    env.set(
                        name.to_string(),
                        make_cap(name, format!("mcp:{name}"), std::rc::Rc::new(())),
                    );
                    env.set(format!("{name}-handler"), handler);
                }
            }

            // 3. Load the prelude (macro definitions).
            {
                let prelude_forms = glia::read_many(glia::PRELUDE).expect("prelude: parse error");
                struct NoopDispatch;
                impl Dispatch for NoopDispatch {
                    fn call<'a>(
                        &'a self,
                        name: &'a str,
                        _args: &'a [Val],
                    ) -> Pin<Box<dyn Future<Output = Result<Val, Val>> + 'a>> {
                        Box::pin(std::future::ready(Err(Val::from(format!(
                            "{name}: not available"
                        )))))
                    }
                }
                let noop = NoopDispatch;
                for form in &prelude_forms {
                    let mut fut = Box::pin(eval::eval_toplevel(form, &mut env, &noop));
                    let waker = std::task::Waker::noop();
                    let mut cx = std::task::Context::from_waker(waker);
                    match fut.as_mut().poll(&mut cx) {
                        std::task::Poll::Ready(Ok(_)) => {}
                        std::task::Poll::Ready(Err(e)) => log::error!("prelude: {e}"),
                        std::task::Poll::Pending => log::error!("prelude: unexpected pending"),
                    }
                }
            }

            // 4. JSON-RPC loop on async stdin.
            //    BufReader + read_line yields Pending when stdin is empty,
            //    allowing poll_loop to service both stdin and RPC concurrently.
            let mut buf_reader = BufReader::new(stdin_reader);
            let mut line = String::new();
            loop {
                line.clear();
                match buf_reader.read_line(&mut line).await {
                    Ok(0) => break, // EOF
                    Ok(_) => {
                        let trimmed = line.trim();
                        if trimmed.is_empty() {
                            continue;
                        }
                        let cont = handle_request(
                            trimmed,
                            &mut env,
                            &ctx,
                            &dispatch_table,
                            &tools_list,
                            &cap_names,
                        )
                        .await;
                        if !cont {
                            break;
                        }
                    }
                    Err(_) => break, // stdin error
                }
            }

            // Clean exit on stdin EOF.
            Ok(())
        }
    });
}

// ---------------------------------------------------------------------------
// Tests — pure logic only, no WASI/WASM dependencies
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    // -- tool_call_to_glia --

    fn args(json: serde_json::Value) -> serde_json::Value {
        json
    }

    #[test]
    fn tool_call_host_id() {
        let result = tool_call_to_glia("host", &args(serde_json::json!({"action": "id"})));
        assert_eq!(result, Some("(perform host :id)".into()));
    }

    #[test]
    fn tool_call_host_peers() {
        let result = tool_call_to_glia("host", &args(serde_json::json!({"action": "peers"})));
        assert_eq!(result, Some("(perform host :peers)".into()));
    }

    #[test]
    fn tool_call_host_addrs() {
        let result = tool_call_to_glia("host", &args(serde_json::json!({"action": "addrs"})));
        assert_eq!(result, Some("(perform host :addrs)".into()));
    }

    #[test]
    fn tool_call_host_unknown_action() {
        let result = tool_call_to_glia("host", &args(serde_json::json!({"action": "delete"})));
        assert_eq!(result, None);
    }

    #[test]
    fn tool_call_routing_provide() {
        let result = tool_call_to_glia(
            "routing",
            &args(serde_json::json!({"action": "provide", "cid": "QmFoo"})),
        );
        assert_eq!(
            result,
            Some(r#"(perform routing :provide (bytes "QmFoo"))"#.into())
        );
    }

    #[test]
    fn tool_call_routing_find_providers() {
        let result = tool_call_to_glia(
            "routing",
            &args(serde_json::json!({"action": "find_providers", "cid": "QmBar"})),
        );
        assert_eq!(
            result,
            Some(r#"(perform routing :find-providers (bytes "QmBar"))"#.into())
        );
    }

    #[test]
    fn tool_call_runtime_run() {
        let result = tool_call_to_glia(
            "runtime",
            &args(serde_json::json!({"action": "run", "wasm_path": "bin/app.wasm"})),
        );
        assert_eq!(
            result,
            Some(r#"(perform runtime :run (load "bin/app.wasm"))"#.into())
        );
    }

    #[test]
    fn tool_call_identity_sign() {
        let result = tool_call_to_glia(
            "identity",
            &args(serde_json::json!({"action": "sign", "domain": "test", "nonce": 42})),
        );
        assert_eq!(result, Some(r#"(perform identity :sign "test" 42)"#.into()));
    }

    #[test]
    fn tool_call_identity_verify() {
        let result = tool_call_to_glia(
            "identity",
            &args(serde_json::json!({
                "action": "verify",
                "data": "deadbeef",
                "signature": "sig",
                "pubkey": "pk"
            })),
        );
        assert_eq!(
            result,
            Some(r#"(perform identity :verify (bytes "deadbeef") (bytes "sig") (bytes "pk"))"#.into())
        );
    }

    #[test]
    fn tool_call_http_post() {
        let result = tool_call_to_glia(
            "http-client",
            &args(serde_json::json!({
                "action": "post",
                "url": "https://example.test",
                "body": r#"{"ok":true}"#
            })),
        );
        assert_eq!(
            result,
            Some(r#"(perform http-client :post "https://example.test" "{\"ok\":true}")"#.into())
        );
    }

    #[test]
    fn tool_call_import_uses_standalone_mcp_binding() {
        let result = tool_call_to_glia("import", &args(serde_json::json!({"path": "core"})));
        assert_eq!(result, Some(r#"(def imported (perform import "core"))"#.into()));
    }

    #[test]
    fn tool_call_unknown_tool() {
        let result = tool_call_to_glia("foobar", &args(serde_json::json!({"action": "x"})));
        assert_eq!(result, None);
    }

    #[test]
    fn tool_call_rejects_injection_in_action() {
        // Action with injection chars should be rejected by is_safe_identifier
        let result = tool_call_to_glia("host", &args(serde_json::json!({"action": "id) (evil"})));
        assert_eq!(result, None);
    }

    #[test]
    fn tool_call_escapes_user_input() {
        // CID with quotes should be escaped, not injected
        let result = tool_call_to_glia(
            "routing",
            &args(serde_json::json!({"action": "provide", "cid": r#"Qm")(evil"#})),
        );
        assert_eq!(
            result,
            Some(r#"(perform routing :provide (bytes "Qm\")(evil"))"#.into())
        );
    }

    // -- tool_def_for_cap --

    #[test]
    fn tool_def_known_caps() {
        for name in &[
            "host",
            "routing",
            "runtime",
            "identity",
            "http-client",
            "import",
        ] {
            assert!(
                tool_def_for_cap(name).is_some(),
                "expected tool def for {name}"
            );
        }
    }

    #[test]
    fn tool_def_unknown_cap() {
        assert!(tool_def_for_cap("foobar").is_none());
    }

    #[test]
    fn tool_def_has_required_fields() {
        let def = tool_def_for_cap("host").unwrap();
        assert!(def.get("name").is_some());
        assert!(def.get("description").is_some());
        assert!(def.get("inputSchema").is_some());
        let schema = def.get("inputSchema").unwrap();
        assert_eq!(schema.get("type").unwrap(), "object");
    }

    // -- build_tools_list --

    #[test]
    fn build_tools_list_empty_caps() {
        let list = build_tools_list(&[]);
        let tools = list.get("tools").unwrap().as_array().unwrap();
        assert_eq!(tools.len(), 1); // eval only
        assert_eq!(tools[0].get("name").unwrap(), "eval");
    }

    #[test]
    fn build_tools_list_known_caps() {
        let caps = vec!["host".into(), "routing".into()];
        let list = build_tools_list(&caps);
        let tools = list.get("tools").unwrap().as_array().unwrap();
        assert_eq!(tools.len(), 3); // host + routing + eval
        assert_eq!(tools[0].get("name").unwrap(), "host");
        assert_eq!(tools[1].get("name").unwrap(), "routing");
        assert_eq!(tools[2].get("name").unwrap(), "eval");
    }

    #[test]
    fn build_tools_list_unknown_caps_filtered() {
        let caps = vec!["host".into(), "unknown_thing".into()];
        let list = build_tools_list(&caps);
        let tools = list.get("tools").unwrap().as_array().unwrap();
        assert_eq!(tools.len(), 2); // host + eval (unknown filtered)
    }

    // -- initialize_result --

    #[test]
    fn initialize_result_structure() {
        let result = initialize_result();
        assert_eq!(result.get("protocolVersion").unwrap(), PROTOCOL_VERSION);
        let info = result.get("serverInfo").unwrap();
        assert_eq!(info.get("name").unwrap(), SERVER_NAME);
        assert_eq!(info.get("version").unwrap(), SERVER_VERSION);
        assert!(result.get("capabilities").unwrap().get("tools").is_some());
    }
}
