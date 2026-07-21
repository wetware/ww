//! Shared Glia effect handler factories for Cap'n Proto capabilities.
//!
//! Extracted from the shell cell so that multiple cells (shell, MCP, etc.)
//! can reuse the same handler implementations without duplication.

use std::cell::RefCell;
use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::rc::Rc;

use glia::{make_cap, Val};

pub mod mcp_adapter;

// Re-export extract_method from glia for downstream consumers (shell, MCP).
pub use glia::extract_method;

// Re-export schema_capnp so generated stem code can resolve `crate::schema_capnp`.
pub use capnp::schema_capnp;

// Generated Cap'n Proto modules.
#[allow(unused_parens, clippy::match_single_binding)]
pub mod system_capnp {
    include!(concat!(env!("OUT_DIR"), "/system_capnp.rs"));
}

#[allow(
    unused_parens,
    clippy::extra_unused_type_parameters,
    clippy::match_single_binding
)]
pub mod stem_capnp {
    include!(concat!(env!("OUT_DIR"), "/stem_capnp.rs"));
}
#[allow(
    unused_parens,
    clippy::extra_unused_type_parameters,
    clippy::match_single_binding
)]
pub mod auth_capnp {
    include!(concat!(env!("OUT_DIR"), "/auth_capnp.rs"));
}
#[allow(
    unused_parens,
    clippy::extra_unused_type_parameters,
    clippy::match_single_binding
)]
pub mod membrane_capnp {
    include!(concat!(env!("OUT_DIR"), "/membrane_capnp.rs"));
}
#[allow(unused_parens, clippy::match_single_binding)]
pub mod routing_capnp {
    include!(concat!(env!("OUT_DIR"), "/routing_capnp.rs"));
}
#[allow(unused_parens, clippy::match_single_binding)]
pub mod http_capnp {
    include!(concat!(env!("OUT_DIR"), "/http_capnp.rs"));
}

// ---------------------------------------------------------------------------
// File loading (same pattern as kernel)
// ---------------------------------------------------------------------------

thread_local! {
    static LOAD_CACHE: RefCell<HashMap<String, Vec<u8>>> = RefCell::new(HashMap::new());
    static LOAD_BACKEND: RefCell<Option<Rc<dyn LoadBackend>>> = RefCell::new(None);
}

pub trait LoadBackend {
    fn load<'a>(
        &'a self,
        path: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<Vec<u8>, Val>> + 'a>>;
}

/// Explicit load state owned by one embedding runtime.  New effect handlers
/// receive this object directly instead of consulting process/thread globals.
#[derive(Clone)]
pub struct LoadRuntime {
    root: String,
    backend: Rc<dyn LoadBackend>,
    cache: Rc<RefCell<HashMap<String, Vec<u8>>>>,
}

impl LoadRuntime {
    pub fn new(root: impl Into<String>, backend: Rc<dyn LoadBackend>) -> Self {
        Self {
            root: root.into(),
            backend,
            cache: Rc::new(RefCell::new(HashMap::new())),
        }
    }

    pub fn resolve(&self, path: &str) -> String {
        if path.starts_with('/') {
            path.to_string()
        } else if self.root.is_empty() || self.root == "/" {
            format!("/{path}")
        } else {
            format!("{}/{path}", self.root.trim_end_matches('/'))
        }
    }

    pub fn load<'a>(
        &'a self,
        path: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<Val, Val>> + 'a>> {
        let resolved = self.resolve(path);
        if let Some(bytes) = self.cache.borrow().get(&resolved).cloned() {
            return Box::pin(async move { Ok(Val::Bytes(bytes)) });
        }
        let backend = self.backend.clone();
        let cache = self.cache.clone();
        Box::pin(async move {
            let bytes = backend.load(&resolved).await?;
            cache.borrow_mut().insert(resolved, bytes.clone());
            Ok(Val::Bytes(bytes))
        })
    }

    /// Handle the data payload of `(perform :load path)` without exposing a
    /// guest-callable `load` primitive.
    pub fn load_value<'a>(
        &'a self,
        data: Val,
    ) -> Pin<Box<dyn Future<Output = Result<Val, Val>> + 'a>> {
        let path = match data {
            Val::Str(path) | Val::Sym(path) => path,
            other => {
                return Box::pin(async move {
                    Err(Val::from(format!(
                        "load: expected a path string, got {other}"
                    )))
                })
            }
        };
        let runtime = self.clone();
        Box::pin(async move { runtime.load(&path).await })
    }
}

pub fn default_load_runtime() -> LoadRuntime {
    LoadRuntime::new(
        std::env::var("WW_ROOT").unwrap_or_else(|_| "/".into()),
        Rc::new(FsLoadBackend),
    )
}

/// WASI/local-filesystem backend for embeddings without grafted IPFS reads.
pub struct FsLoadBackend;

impl LoadBackend for FsLoadBackend {
    fn load<'a>(
        &'a self,
        path: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<Vec<u8>, Val>> + 'a>> {
        Box::pin(async move { read_default_path(path) })
    }
}

pub fn set_load_backend(backend: Rc<dyn LoadBackend>) {
    LOAD_BACKEND.with(|slot| {
        *slot.borrow_mut() = Some(backend);
    });
}

pub fn clear_load_backend() {
    LOAD_BACKEND.with(|slot| {
        *slot.borrow_mut() = None;
    });
}

pub fn clear_load_cache() {
    LOAD_CACHE.with(|cache| cache.borrow_mut().clear());
}

fn resolve_load_path(args: &[Val]) -> Result<String, Val> {
    let path = match args.first() {
        Some(Val::Str(s)) => s.clone(),
        Some(Val::Sym(s)) => s.clone(),
        _ => return Err(Val::from("load: expected a path string")),
    };

    // Normalize: ensure leading /
    let resolved = if path.starts_with('/') {
        path.clone()
    } else {
        let root = std::env::var("WW_ROOT").unwrap_or_default();
        format!("{root}/{path}")
    };

    Ok(resolved)
}

fn read_default_path(path: &str) -> Result<Vec<u8>, Val> {
    std::fs::read(path).map_err(|e| Val::from(format!("load: {path}: {e}")))
}

pub async fn eval_load_async(args: &[Val]) -> Result<Val, Val> {
    let resolved = resolve_load_path(args)?;

    if let Some(bytes) = LOAD_CACHE.with(|cache| cache.borrow().get(&resolved).cloned()) {
        return Ok(Val::Bytes(bytes));
    }

    let maybe_backend = LOAD_BACKEND.with(|slot| slot.borrow().clone());
    let bytes = if let Some(backend) = maybe_backend {
        backend.load(&resolved).await?
    } else {
        read_default_path(&resolved)?
    };

    LOAD_CACHE.with(|cache| {
        cache.borrow_mut().insert(resolved.clone(), bytes.clone());
    });

    Ok(Val::Bytes(bytes))
}

pub fn eval_load(args: &[Val]) -> Result<Val, Val> {
    let resolved = resolve_load_path(args)?;

    LOAD_CACHE.with(|cache| {
        let mut cache = cache.borrow_mut();
        if let Some(bytes) = cache.get(&resolved) {
            return Ok(Val::Bytes(bytes.clone()));
        }
        match read_default_path(&resolved) {
            Ok(bytes) => {
                cache.insert(resolved, bytes.clone());
                Ok(Val::Bytes(bytes))
            }
            Err(e) => Err(e),
        }
    })
}

// ---------------------------------------------------------------------------
// Cap handler helpers
// ---------------------------------------------------------------------------

pub fn call_resume(resume: &Val, val: Val) -> Result<Val, Val> {
    match resume {
        Val::NativeFn { func, .. } => func(&[val]),
        _ => Err(Val::from("cap handler: invalid resume function")),
    }
}

// ---------------------------------------------------------------------------
// Import handler — module loading as a membrane capability
// ---------------------------------------------------------------------------

/// Schema CID for the import capability (local, not a real capnp schema).
pub const IMPORT_SCHEMA_CID: &str = "local:import";

thread_local! {
    /// Cache for imported module maps, keyed by resolved path.
    /// Second `(def m (perform import "core"))` returns the cached map.
    static IMPORT_CACHE: RefCell<HashMap<String, Val>> = RefCell::new(HashMap::new());
}

/// Create a `Val::Cap` representing the import capability.
///
/// Callers bind this in the env so `(perform import "core")` resolves
/// `import` to a cap value that the effect system can match.
pub fn make_import_cap() -> Val {
    make_cap("import", IMPORT_SCHEMA_CID, Rc::new(()))
}

/// Clear the import cache. Useful for testing or when the virtual filesystem changes.
pub fn clear_import_cache() {
    IMPORT_CACHE.with(|cache| cache.borrow_mut().clear());
}

/// Resolve an import path to an absolute filesystem path.
///
/// - Relative path (no leading `/`): resolved to `/lib/{path}.glia`
/// - Absolute path (leading `/`): appended with `.glia`
fn resolve_import_path(path: &str) -> String {
    if path.starts_with('/') {
        format!("{}.glia", path)
    } else {
        format!("/lib/{}.glia", path)
    }
}

/// Create the import effect handler.
///
/// Usage: `(def core (perform import "core"))`
///
/// The handler:
/// 1. Resolves the path (relative → `/lib/`, absolute → as-is)
/// 2. Checks the import cache
/// 3. If not cached: loads file, parses as Glia forms, evals in fresh scope
/// 4. Collects bindings as a `Val::Map`
/// 5. Caches the map for idempotent re-import
/// 6. Resumes with the map
///
/// The caller binds the returned map: `(def core (perform import "core"))`.
/// Access members via the map: `(core :help)`.
pub fn make_import_handler(load_runtime: LoadRuntime) -> Val {
    Val::AsyncNativeFn {
        name: "import-handler".into(),
        func: Rc::new(move |args: Vec<Val>| {
            let load_runtime = load_runtime.clone();
            Box::pin(async move {
                // Effect data for `(perform import "core")`:
                // args[0] = data list, args[1] = resume continuation.
                //
                // Cap-targeted perform packs all args after the target into a
                // list: `(perform import "core")` → data = Val::List(["core"]).
                let data = &args[0];
                let resume = &args[1];

                // Extract the path from the data list.
                // `(perform import "core")` → data = ["core"]
                let path = match data {
                    Val::List(items) => match items.first() {
                        Some(Val::Str(s)) => s.clone(),
                        other => {
                            let desc = match other {
                                Some(v) => format!("{v}"),
                                None => "nothing".into(),
                            };
                            return Err(Val::from(format!(
                                "import: expected string path, got {desc}"
                            )));
                        }
                    },
                    Val::Str(s) => s.clone(),
                    other => {
                        return Err(Val::from(format!(
                            "import: expected path string, got {other}"
                        )));
                    }
                };

                let resolved = resolve_import_path(&path);

                // Check cache
                let cached = IMPORT_CACHE.with(|cache| cache.borrow().get(&resolved).cloned());
                if let Some(map) = cached {
                    return call_resume(resume, map);
                }

                // Load the file via eval_load
                let bytes_val = load_runtime.load(&resolved).await?;
                let content = match &bytes_val {
                    Val::Bytes(b) => std::str::from_utf8(b)
                        .map_err(|e| {
                            Val::from(format!("import: invalid UTF-8 in {resolved}: {e}"))
                        })?
                        .to_string(),
                    Val::Str(s) => s.clone(),
                    other => {
                        return Err(Val::from(format!(
                            "import: load returned {other}, expected bytes or string"
                        )));
                    }
                };

                // Parse the file as Glia forms
                let forms = glia::read_many(&content)
                    .map_err(|e| Val::from(format!("import: parse error in {resolved}: {e}")))?;

                // Evaluate in a fresh Env (isolated scope)
                let mut import_env = glia::eval::Env::new();
                // Load prelude so imported modules can use `defn`, `when`, etc.
                {
                    let prelude_forms = glia::read_many(glia::PRELUDE)
                        .map_err(|e| Val::from(format!("import: prelude parse: {e}")))?;
                    struct NoopDispatch;
                    impl glia::eval::Dispatch for NoopDispatch {
                        fn call<'a>(
                            &'a self,
                            name: &'a str,
                            _args: &'a [glia::Val],
                        ) -> std::pin::Pin<
                            Box<
                                dyn std::future::Future<Output = Result<glia::Val, glia::Val>> + 'a,
                            >,
                        > {
                            Box::pin(std::future::ready(Err(glia::Val::from(format!(
                                "{name}: not available during import"
                            )))))
                        }
                    }
                    let noop = NoopDispatch;
                    for form in &prelude_forms {
                        // Prelude forms are synchronous (macros only), so poll once.
                        let mut fut =
                            Box::pin(glia::eval::eval_toplevel(form, &mut import_env, &noop));
                        let waker = std::task::Waker::noop();
                        let mut cx = std::task::Context::from_waker(waker);
                        match fut.as_mut().poll(&mut cx) {
                            std::task::Poll::Ready(Ok(_)) => {}
                            std::task::Poll::Ready(Err(e)) => {
                                return Err(Val::from(format!("import: prelude error: {e}")));
                            }
                            std::task::Poll::Pending => {
                                return Err(Val::from("import: prelude unexpectedly pending"));
                            }
                        }
                    }
                }

                // Evaluate module forms
                {
                    struct NoopDispatch;
                    impl glia::eval::Dispatch for NoopDispatch {
                        fn call<'a>(
                            &'a self,
                            name: &'a str,
                            _args: &'a [glia::Val],
                        ) -> std::pin::Pin<
                            Box<
                                dyn std::future::Future<Output = Result<glia::Val, glia::Val>> + 'a,
                            >,
                        > {
                            Box::pin(std::future::ready(Err(glia::Val::from(format!(
                                "{name}: not available during import"
                            )))))
                        }
                    }
                    let noop = NoopDispatch;
                    for form in &forms {
                        let analyzed = glia::expr::analyze(form).map_err(|e| {
                            Val::from(format!("import: analyze error in {resolved}: {e}"))
                        })?;
                        let mut fut = Box::pin(glia::eval::eval_toplevel_expr(
                            &analyzed,
                            &mut import_env,
                            &noop,
                        ));
                        let waker = std::task::Waker::noop();
                        let mut cx = std::task::Context::from_waker(waker);
                        match fut.as_mut().poll(&mut cx) {
                            std::task::Poll::Ready(Ok(_)) => {}
                            std::task::Poll::Ready(Err(e)) => {
                                return Err(Val::from(format!(
                                    "import: eval error in {resolved}: {e}"
                                )));
                            }
                            std::task::Poll::Pending => {
                                return Err(Val::from(format!(
                                    "import: eval unexpectedly pending in {resolved}"
                                )));
                            }
                        }
                    }
                }

                // Collect bindings as a Val::Map
                let bindings = import_env.bindings();
                let map_entries: Vec<(Val, Val)> = bindings
                    .into_iter()
                    .map(|(name, val)| (Val::Keyword(name), val))
                    .collect();
                let module_map = Val::Map(glia::ValMap::from_pairs(map_entries));

                // Cache the map
                IMPORT_CACHE.with(|cache| {
                    cache.borrow_mut().insert(resolved, module_map.clone());
                });

                call_resume(resume, module_map)
            })
        }),
    }
}

// ---------------------------------------------------------------------------
// Effect handler factories
// ---------------------------------------------------------------------------

pub fn make_host_handler(host: system_capnp::host::Client) -> Val {
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
                        let encoded = bs58::encode(peer_id_bytes).into_string();
                        call_resume(resume, Val::Str(encoded))
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
                        call_resume(resume, Val::List(items))
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
                                let encoded = bs58::encode(peer_id).into_string();
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
                        call_resume(resume, Val::List(items))
                    }
                    other => Err(Val::from(format!("host: unknown method :{other}"))),
                }
            })
        }),
    }
}

pub fn make_routing_handler(routing: routing_capnp::routing::Client) -> Val {
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
                        call_resume(resume, Val::Nil)
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
                        call_resume(resume, Val::Str(path))
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
                        call_resume(resume, Val::Str(root))
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
                        call_resume(resume, Val::Str(root))
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
                        call_resume(resume, Val::Str(root))
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
                        call_resume(resume, Val::Str(published))
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
                        call_resume(resume, Val::Str(key))
                    }
                    // findProviders uses streaming sink — deferred to follow-up.
                    other => Err(Val::from(format!("routing: unknown method :{other}"))),
                }
            })
        }),
    }
}

// ---------------------------------------------------------------------------
// Effect handler wrapping — nests with-effect-handler forms around an expr
// ---------------------------------------------------------------------------

/// Wraps a Glia form in standard capability effect handlers.
///
/// Environmental host effects (`:load`, `:stdout`, `:exit`) deliberately do
/// not appear here. Embeddings install them as Rust-owned frames around the
/// top-level evaluator, so guest code cannot obtain a default handler value.
///
/// The `extra_caps` parameter allows callers to inject additional
/// capability names that sit outside the core set.
pub fn wrap_with_handlers(form: &Val, extra_caps: &[&str]) -> Val {
    // Wrap in cap handlers (innermost to outermost)
    // Core caps first, then any extras
    let mut caps: Vec<&str> = vec!["import", "routing", "host"];
    for extra in extra_caps {
        caps.insert(0, extra);
    }
    let mut wrapped = form.clone();
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

// ---------------------------------------------------------------------------
// Graft helpers: name-based lookup in parallel lists
// ---------------------------------------------------------------------------

/// Look up a typed capability by name from the graft caps list.
pub fn get_graft_cap<T: capnp::capability::FromClientHook>(
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
            return entry.get_cap().get_as_capability::<T>();
        }
    }
    Err(capnp::Error::failed(format!(
        "capability '{name}' not found in graft response"
    )))
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn import_path_resolution_relative() {
        assert_eq!(resolve_import_path("core"), "/lib/core.glia");
    }

    #[test]
    fn import_path_resolution_nested() {
        assert_eq!(resolve_import_path("std/net"), "/lib/std/net.glia");
    }

    #[test]
    fn import_path_resolution_absolute() {
        assert_eq!(resolve_import_path("/absolute/path"), "/absolute/path.glia");
    }

    #[test]
    fn import_cache_clear() {
        IMPORT_CACHE.with(|cache| {
            cache
                .borrow_mut()
                .insert("/lib/test.glia".into(), Val::Map(glia::ValMap::new()));
        });
        clear_import_cache();
        IMPORT_CACHE.with(|cache| {
            assert!(cache.borrow().is_empty());
        });
    }

    #[test]
    fn import_handler_returns_map() {
        clear_import_cache();

        // Pre-populate the cache to test that the handler returns a map
        let test_map = Val::Map(glia::ValMap::from_pairs(vec![
            (Val::Keyword("x".into()), Val::Int(42)),
            (Val::Keyword("y".into()), Val::Int(99)),
        ]));
        IMPORT_CACHE.with(|cache| {
            cache
                .borrow_mut()
                .insert("/lib/core.glia".into(), test_map.clone());
        });

        // Build the handler and call it with cached data
        let handler = make_import_handler(default_load_runtime());
        match &handler {
            Val::AsyncNativeFn { func, .. } => {
                // Simulate: (perform import "core")
                // Effect data is a list of the args: ["core"]
                let data = Val::List(vec![Val::Str("core".into())]);

                // Build a resume function that captures the result
                let result = Rc::new(RefCell::new(None));
                let result_clone = result.clone();
                let resume = Val::NativeFn {
                    name: "test-resume".into(),
                    func: Rc::new(move |args: &[Val]| {
                        *result_clone.borrow_mut() = Some(args[0].clone());
                        Ok(Val::Nil)
                    }),
                };

                let args = vec![data, resume];
                let fut = func(args);

                // Poll the future (should resolve immediately for cached imports)
                let waker = std::task::Waker::noop();
                let mut cx = std::task::Context::from_waker(waker);
                let mut pinned = fut;
                match std::pin::Pin::new(&mut pinned).poll(&mut cx) {
                    std::task::Poll::Ready(Ok(_)) => {}
                    std::task::Poll::Ready(Err(e)) => panic!("import handler failed: {e}"),
                    std::task::Poll::Pending => panic!("import handler unexpectedly pending"),
                }

                // Verify the result is the cached map
                let r = result.borrow();
                let result_val = r.as_ref().expect("resume should have been called");
                match result_val {
                    Val::Map(entries) => {
                        assert_eq!(entries.len(), 2);
                        // Check that :x is 42
                        let x = entries
                            .iter()
                            .find(|(k, _)| matches!(k, Val::Keyword(s) if s == "x"))
                            .map(|(_, v)| v);
                        assert_eq!(x, Some(&Val::Int(42)));
                    }
                    other => panic!("expected Map, got {other}"),
                }
            }
            _ => panic!("expected AsyncNativeFn"),
        }

        clear_import_cache();
    }

    #[test]
    fn import_cached_returns_same_map() {
        clear_import_cache();

        let test_map = Val::Map(glia::ValMap::from_pairs(vec![(
            Val::Keyword("a".into()),
            Val::Int(1),
        )]));
        IMPORT_CACHE.with(|cache| {
            cache
                .borrow_mut()
                .insert("/lib/cached.glia".into(), test_map.clone());
        });

        // Call handler twice — both should return the same cached map
        let handler = make_import_handler(default_load_runtime());
        let func = match &handler {
            Val::AsyncNativeFn { func, .. } => func.clone(),
            _ => panic!("expected AsyncNativeFn"),
        };

        for _ in 0..2 {
            let data = Val::List(vec![Val::Str("cached".into())]);
            let result = Rc::new(RefCell::new(None));
            let result_clone = result.clone();
            let resume = Val::NativeFn {
                name: "test-resume".into(),
                func: Rc::new(move |args: &[Val]| {
                    *result_clone.borrow_mut() = Some(args[0].clone());
                    Ok(Val::Nil)
                }),
            };

            let fut = func(vec![data, resume]);
            let waker = std::task::Waker::noop();
            let mut cx = std::task::Context::from_waker(waker);
            let mut pinned = fut;
            match std::pin::Pin::new(&mut pinned).poll(&mut cx) {
                std::task::Poll::Ready(Ok(_)) => {}
                other => panic!("unexpected poll result: {other:?}"),
            }

            let r = result.borrow();
            match r.as_ref().unwrap() {
                Val::Map(entries) => assert_eq!(entries.len(), 1),
                other => panic!("expected Map, got {other}"),
            }
        }

        clear_import_cache();
    }

    #[test]
    fn import_missing_file_returns_error() {
        clear_import_cache();

        let handler = make_import_handler(default_load_runtime());
        let func = match &handler {
            Val::AsyncNativeFn { func, .. } => func.clone(),
            _ => panic!("expected AsyncNativeFn"),
        };

        let data = Val::List(vec![Val::Str("nonexistent".into())]);
        let resume = Val::NativeFn {
            name: "test-resume".into(),
            func: Rc::new(move |_args: &[Val]| Ok(Val::Nil)),
        };

        let fut = func(vec![data, resume]);
        let waker = std::task::Waker::noop();
        let mut cx = std::task::Context::from_waker(waker);
        let mut pinned = fut;
        match std::pin::Pin::new(&mut pinned).poll(&mut cx) {
            std::task::Poll::Ready(Err(_)) => {} // expected — file doesn't exist
            std::task::Poll::Ready(Ok(_)) => panic!("expected error for missing file"),
            std::task::Poll::Pending => panic!("unexpected pending"),
        }

        clear_import_cache();
    }

    // -- call_resume --

    #[test]
    fn call_resume_with_native_fn() {
        let resume = Val::NativeFn {
            name: "test-resume".into(),
            func: std::rc::Rc::new(|args: &[Val]| Ok(args[0].clone())),
        };
        let result = call_resume(&resume, Val::Int(42));
        assert_eq!(result, Ok(Val::Int(42)));
    }

    #[test]
    fn call_resume_with_non_fn_errors() {
        let result = call_resume(&Val::Nil, Val::Int(1));
        assert!(result.is_err());
        let result = call_resume(&Val::Int(5), Val::Str("x".into()));
        assert!(result.is_err());
    }

    // -- extract_method --

    #[test]
    fn extract_method_valid() {
        let data = Val::List(vec![
            Val::Keyword("peers".into()),
            Val::Int(1),
            Val::Str("extra".into()),
        ]);
        let (method, rest) = extract_method(&data).unwrap();
        assert_eq!(method, "peers");
        assert_eq!(rest, vec![Val::Int(1), Val::Str("extra".into())]);
    }

    #[test]
    fn extract_method_keyword_only() {
        let data = Val::List(vec![Val::Keyword("id".into())]);
        let (method, rest) = extract_method(&data).unwrap();
        assert_eq!(method, "id");
        assert!(rest.is_empty());
    }

    #[test]
    fn extract_method_non_list_errors() {
        assert!(extract_method(&Val::Int(5)).is_err());
        assert!(extract_method(&Val::Nil).is_err());
    }

    #[test]
    fn extract_method_non_keyword_head_errors() {
        let data = Val::List(vec![Val::Str("not-keyword".into())]);
        assert!(extract_method(&data).is_err());
    }

    #[test]
    fn extract_method_empty_list_errors() {
        assert!(extract_method(&Val::List(vec![])).is_err());
    }

    // -- wrap_with_handlers --

    #[test]
    fn wrap_with_handlers_no_extras() {
        let form = Val::Int(42);
        let wrapped = wrap_with_handlers(&form, &[]);
        // Should be nested: host(routing(import(:load(42))))
        // Outermost is host
        match &wrapped {
            Val::List(items) => {
                assert_eq!(items[0], Val::Sym("with-effect-handler".into()));
                assert_eq!(items[1], Val::Sym("host".to_string()));
            }
            other => panic!("expected list, got {other:?}"),
        }
    }

    #[test]
    fn wrap_with_handlers_with_extras() {
        let form = Val::Int(1);
        let wrapped = wrap_with_handlers(&form, &["custom"]);
        // "custom" is an extra cap — should appear in the wrapping
        match &wrapped {
            Val::List(items) => {
                assert_eq!(items[0], Val::Sym("with-effect-handler".into()));
                // The outermost is still host (extras are inserted at index 0
                // of the caps vec, so they're INNER, not outer)
                assert_eq!(items[1], Val::Sym("host".to_string()));
            }
            other => panic!("expected list, got {other:?}"),
        }
    }
}
