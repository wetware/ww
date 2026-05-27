use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::future::Future;
use std::pin::Pin;

use caps::{make_import_cap, make_import_handler};
use glia::eval::{self, Dispatch, Env};
use glia::{
    extract_method, make_cap, read, read_many, AttenuatedCapInner, AttenuationPolicy, GliaCapInner,
    Val,
};

use std::rc::Rc;

use wasip2::cli::stderr::get_stderr;
use wasip2::cli::stdin::get_stdin;
use wasip2::cli::stdout::get_stdout;
use wasip2::exports::cli::run::Guest;

#[allow(dead_code)]
mod system_capnp {
    include!(concat!(env!("OUT_DIR"), "/system_capnp.rs"));
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

/// Exported kernel bootstrap capability.
///
/// The kernel boots with host-provided `Membrane` access, and the shell client
/// expects the kernel process to export a `Membrane` bootstrap cap in return.
/// This proxy forwards `graft()` to the active host membrane once initialization
/// has stored it.
struct KernelBootstrap {
    membrane: Rc<RefCell<Option<Membrane>>>,
    policy: Rc<RefCell<Option<ExportPolicy>>>,
}

const INIT_MEMBRANE_NOT_READY: &str = "INIT_MEMBRANE_NOT_READY";
const INIT_POLICY_NOT_READY: &str = "INIT_POLICY_NOT_READY";

#[derive(Debug, Clone, Default)]
struct ExportPolicy {
    caps: BTreeMap<String, ExportCapPolicy>,
}

#[derive(Debug, Clone, Default)]
struct ExportCapPolicy {
    allow_methods: Option<BTreeSet<String>>,
    returns: BTreeMap<String, BTreeMap<String, ExportCapPolicy>>,
}

#[derive(Copy, Clone)]
enum MethodFilterCap {
    Host,
    Runtime,
    Routing,
    Identity,
    Ipfs,
    HttpClient,
    StreamListener,
    StreamDialer,
    VatListener,
    VatClient,
    HttpListener,
    Executor,
    Process,
    Signer,
    ByteStream,
    DynamicAny,
}

fn method_filter_cap(cap_name: &str) -> Option<MethodFilterCap> {
    match cap_name {
        "host" => Some(MethodFilterCap::Host),
        "runtime" => Some(MethodFilterCap::Runtime),
        "routing" => Some(MethodFilterCap::Routing),
        "identity" => Some(MethodFilterCap::Identity),
        "ipfs" => Some(MethodFilterCap::Ipfs),
        "http-client" => Some(MethodFilterCap::HttpClient),
        _ => None,
    }
}

fn deny_method(interface: &str, method: &str) -> capnp::Error {
    capnp::Error::failed(format!(
        "permission denied: {interface}.{method} blocked by export policy"
    ))
}

fn allow_method(
    policy: &ExportCapPolicy,
    interface: &str,
    method: &str,
) -> Result<(), capnp::Error> {
    let Some(allow) = &policy.allow_methods else {
        return Ok(());
    };
    if allow.contains(method) {
        return Ok(());
    }
    Err(deny_method(interface, method))
}

fn return_policy<'a>(
    policy: &'a ExportCapPolicy,
    method: &str,
    field: &str,
) -> Option<&'a ExportCapPolicy> {
    policy
        .returns
        .get(method)
        .and_then(|fields| fields.get(field))
}

#[derive(Clone, Default)]
struct DynamicMethodPolicy {
    interface_id: u64,
    methods_by_id: BTreeMap<u16, String>,
    allowed_ids: Option<BTreeSet<u16>>,
}

fn parse_interface_methods_from_schema(
    schema_bytes: &[u8],
) -> Result<(u64, BTreeMap<String, u16>, BTreeMap<u16, String>), capnp::Error> {
    if schema_bytes.is_empty() {
        return Err(capnp::Error::failed(
            "schema must not be empty for AnyPointer attenuation".into(),
        ));
    }

    let words = bytes_to_aligned_words(schema_bytes);
    let segments: &[&[u8]] = &[capnp::Word::words_to_bytes(&words)];
    let segment_array = capnp::message::SegmentArray::new(segments);
    let reader = capnp::message::Reader::new(segment_array, capnp::message::ReaderOptions::new());
    let node: capnp::schema_capnp::node::Reader<'_> = reader.get_root()?;
    let iface = match node.which()? {
        capnp::schema_capnp::node::Which::Interface(i) => i,
        _ => {
            return Err(capnp::Error::failed(
                "schema must decode to a capnp interface node".into(),
            ))
        }
    };

    let mut by_name = BTreeMap::new();
    let mut by_id = BTreeMap::new();
    for method in iface.get_methods()?.iter() {
        let name = method
            .get_name()?
            .to_str()
            .map_err(|e| capnp::Error::failed(e.to_string()))?
            .to_string();
        let id = method.get_code_order();
        by_name.insert(name.clone(), id);
        by_id.insert(id, name);
    }
    Ok((node.get_id(), by_name, by_id))
}

fn bytes_to_aligned_words(bytes: &[u8]) -> Vec<capnp::Word> {
    let word_count = bytes.len().div_ceil(8);
    let mut words = vec![capnp::word(0, 0, 0, 0, 0, 0, 0, 0); word_count];
    capnp::Word::words_to_bytes_mut(&mut words)[..bytes.len()].copy_from_slice(bytes);
    words
}

fn canonicalize_schema_node_bytes(
    node: capnp::schema_capnp::node::Reader<'_>,
) -> Result<Vec<u8>, capnp::Error> {
    let mut msg = capnp::message::Builder::new_default();
    msg.set_root_canonical(node)?;
    let segments = msg.get_segments_for_output();
    if segments.len() != 1 {
        return Err(capnp::Error::failed(
            "schema node canonicalization produced unexpected segment layout".into(),
        ));
    }
    Ok(segments[0].to_vec())
}

fn build_dynamic_method_policy(
    interface: &str,
    method: &str,
    field: &str,
    policy: &ExportCapPolicy,
    schema_bytes: &[u8],
) -> Result<DynamicMethodPolicy, capnp::Error> {
    if !policy.returns.is_empty() {
        return Err(capnp::Error::failed(format!(
            "export policy {interface}.{method}.{field}: recursive :returns for unknown dynamic schema is not supported; use a known typed interface schema or omit nested :returns"
        )));
    }

    let (interface_id, by_name, by_id) = parse_interface_methods_from_schema(schema_bytes)?;
    let allowed_ids = match &policy.allow_methods {
        None => None,
        Some(allow_names) => {
            let mut ids = BTreeSet::new();
            for name in allow_names {
                let Some(id) = by_name.get(name) else {
                    return Err(capnp::Error::failed(format!(
                        "export policy {interface}.{method}.{field}: unknown method '{name}' for schema interface id 0x{interface_id:x}"
                    )));
                };
                ids.insert(*id);
            }
            Some(ids)
        }
    };

    Ok(DynamicMethodPolicy {
        interface_id,
        methods_by_id: by_id,
        allowed_ids,
    })
}

fn known_cap_kind_for_schema(schema_bytes: &[u8]) -> Option<MethodFilterCap> {
    if schema_bytes == schema_ids::HOST_SCHEMA {
        return Some(MethodFilterCap::Host);
    }
    if schema_bytes == schema_ids::RUNTIME_SCHEMA {
        return Some(MethodFilterCap::Runtime);
    }
    if schema_bytes == schema_ids::ROUTING_SCHEMA {
        return Some(MethodFilterCap::Routing);
    }
    if schema_bytes == schema_ids::IDENTITY_SCHEMA {
        return Some(MethodFilterCap::Identity);
    }
    if schema_bytes == schema_ids::HTTP_CLIENT_SCHEMA {
        return Some(MethodFilterCap::HttpClient);
    }
    if schema_bytes == schema_ids::STREAM_DIALER_SCHEMA {
        return Some(MethodFilterCap::StreamDialer);
    }
    if schema_bytes == schema_ids::STREAM_LISTENER_SCHEMA {
        return Some(MethodFilterCap::StreamListener);
    }
    if schema_bytes == schema_ids::VAT_CLIENT_SCHEMA {
        return Some(MethodFilterCap::VatClient);
    }
    if schema_bytes == schema_ids::VAT_LISTENER_SCHEMA {
        return Some(MethodFilterCap::VatListener);
    }
    if schema_bytes == schema_ids::EXECUTOR_SCHEMA {
        return Some(MethodFilterCap::Executor);
    }
    None
}

#[derive(Clone)]
struct MethodFilteredDynamicCap {
    inner: capnp::capability::Client,
    policy: DynamicMethodPolicy,
    path: String,
}

#[derive(Clone)]
struct DynamicDispatch(std::rc::Rc<MethodFilteredDynamicCap>);

impl std::ops::Deref for DynamicDispatch {
    type Target = MethodFilteredDynamicCap;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl capnp::capability::Server for DynamicDispatch {
    fn dispatch_call(
        self,
        interface_id: u64,
        method_id: u16,
        params: capnp::capability::Params<capnp::any_pointer::Owned>,
        results: capnp::capability::Results<capnp::any_pointer::Owned>,
    ) -> capnp::capability::DispatchCallResult {
        (*self.0)
            .clone()
            .dispatch_call(interface_id, method_id, params, results)
    }

    fn as_ptr(&self) -> usize {
        self.0.as_ptr()
    }
}

struct UntypedDynamicClient(capnp::capability::Client);

impl capnp::capability::FromClientHook for UntypedDynamicClient {
    fn new(hook: Box<dyn capnp::private::capability::ClientHook>) -> Self {
        Self(capnp::capability::Client::new(hook))
    }

    fn into_client_hook(self) -> Box<dyn capnp::private::capability::ClientHook> {
        self.0.hook
    }

    fn as_client_hook(&self) -> &dyn capnp::private::capability::ClientHook {
        self.0.hook.as_ref()
    }
}

impl capnp::capability::FromServer<MethodFilteredDynamicCap> for UntypedDynamicClient {
    type Dispatch = DynamicDispatch;

    fn from_server(
        s: capnp::capability::Rc<MethodFilteredDynamicCap>,
    ) -> Self::Dispatch {
        DynamicDispatch(s)
    }
}

impl capnp::capability::Server for MethodFilteredDynamicCap {
    fn dispatch_call(
        self,
        interface_id: u64,
        method_id: u16,
        params: capnp::capability::Params<capnp::any_pointer::Owned>,
        mut results: capnp::capability::Results<capnp::any_pointer::Owned>,
    ) -> capnp::capability::DispatchCallResult {
        if interface_id != self.policy.interface_id {
            return capnp::capability::DispatchCallResult::new(
                capnp::capability::Promise::err(capnp::Error::failed(format!(
                    "permission denied: {} rejected interface id 0x{interface_id:x} (expected 0x{:x})",
                    self.path, self.policy.interface_id
                ))),
                false,
            );
        }

        let method_name = self
            .policy
            .methods_by_id
            .get(&method_id)
            .cloned()
            .unwrap_or_else(|| format!("<unknown:{method_id}>"));

        if let Some(allowed) = &self.policy.allowed_ids {
            if !allowed.contains(&method_id) {
                return capnp::capability::DispatchCallResult::new(
                    capnp::capability::Promise::err(capnp::Error::failed(format!(
                        "permission denied: {}.{} blocked by export policy",
                        self.path, method_name
                    ))),
                    false,
                );
            }
        }

        let req = self
            .inner;
        let maybe_request = params.get().and_then(|p| {
            let mut request = req.new_call::<capnp::any_pointer::Owned, capnp::any_pointer::Owned>(
                interface_id,
                method_id,
                Some(p.target_size()?),
            );
            request.get().set_as(p)?;
            Ok(request)
        });
        let promise = match maybe_request {
            Ok(request) => capnp::capability::Promise::from_future(async move {
                let resp = request.send().promise.await?;
                results.set(resp.get()?)?;
                Ok(())
            }),
            Err(e) => capnp::capability::Promise::err(e),
        };
        capnp::capability::DispatchCallResult::new(promise, false)
    }

    fn as_ptr(&self) -> usize {
        self as *const Self as usize
    }
}

#[derive(Clone)]
struct MethodFilteredHost {
    inner: system_capnp::host::Client,
    policy: ExportCapPolicy,
}

#[allow(refining_impl_trait)]
impl system_capnp::host::Server for MethodFilteredHost {
    fn id(
        self: capnp::capability::Rc<Self>,
        _params: system_capnp::host::IdParams,
        mut results: system_capnp::host::IdResults,
    ) -> capnp::capability::Promise<(), capnp::Error> {
        if let Err(e) = allow_method(&self.policy, "host", "id") {
            return capnp::capability::Promise::err(e);
        }
        let inner = self.inner.clone();
        capnp::capability::Promise::from_future(async move {
            let resp = inner.id_request().send().promise.await?;
            results.get().set_peer_id(resp.get()?.get_peer_id()?);
            Ok(())
        })
    }

    fn addrs(
        self: capnp::capability::Rc<Self>,
        _params: system_capnp::host::AddrsParams,
        mut results: system_capnp::host::AddrsResults,
    ) -> capnp::capability::Promise<(), capnp::Error> {
        if let Err(e) = allow_method(&self.policy, "host", "addrs") {
            return capnp::capability::Promise::err(e);
        }
        let inner = self.inner.clone();
        capnp::capability::Promise::from_future(async move {
            let resp = inner.addrs_request().send().promise.await?;
            let addrs = resp.get()?.get_addrs()?;
            let mut out = results.get().init_addrs(addrs.len());
            for i in 0..addrs.len() {
                out.set(i, addrs.get(i)?);
            }
            Ok(())
        })
    }

    fn peers(
        self: capnp::capability::Rc<Self>,
        _params: system_capnp::host::PeersParams,
        mut results: system_capnp::host::PeersResults,
    ) -> capnp::capability::Promise<(), capnp::Error> {
        if let Err(e) = allow_method(&self.policy, "host", "peers") {
            return capnp::capability::Promise::err(e);
        }
        let inner = self.inner.clone();
        capnp::capability::Promise::from_future(async move {
            let resp = inner.peers_request().send().promise.await?;
            let peers = resp.get()?.get_peers()?;
            let mut out = results.get().init_peers(peers.len());
            for i in 0..peers.len() {
                let src = peers.get(i);
                let mut dst = out.reborrow().get(i);
                dst.set_peer_id(src.get_peer_id()?);
                let src_addrs = src.get_addrs()?;
                let mut dst_addrs = dst.init_addrs(src_addrs.len());
                for j in 0..src_addrs.len() {
                    dst_addrs.set(j, src_addrs.get(j)?);
                }
            }
            Ok(())
        })
    }

    fn network(
        self: capnp::capability::Rc<Self>,
        _params: system_capnp::host::NetworkParams,
        mut results: system_capnp::host::NetworkResults,
    ) -> capnp::capability::Promise<(), capnp::Error> {
        if let Err(e) = allow_method(&self.policy, "host", "network") {
            return capnp::capability::Promise::err(e);
        }
        let inner = self.inner.clone();
        let policy = self.policy.clone();
        capnp::capability::Promise::from_future(async move {
            let resp = inner.network_request().send().promise.await?;
            let src = resp.get()?;
            let mut dst = results.get();

            let stream_listener = src.get_stream_listener()?;
            let stream_listener = maybe_wrap_returned_cap(
                MethodFilterCap::StreamListener,
                "network",
                "streamListener",
                stream_listener.client,
                &policy,
                None,
            )?;
            let stream_listener: system_capnp::stream_listener::Client =
                capnp::capability::FromClientHook::new(stream_listener.hook.clone());
            dst.set_stream_listener(stream_listener);

            let stream_dialer = src.get_stream_dialer()?;
            let stream_dialer = maybe_wrap_returned_cap(
                MethodFilterCap::StreamDialer,
                "network",
                "streamDialer",
                stream_dialer.client,
                &policy,
                None,
            )?;
            let stream_dialer: system_capnp::stream_dialer::Client =
                capnp::capability::FromClientHook::new(stream_dialer.hook.clone());
            dst.set_stream_dialer(stream_dialer);

            let vat_listener = src.get_vat_listener()?;
            let vat_listener = maybe_wrap_returned_cap(
                MethodFilterCap::VatListener,
                "network",
                "vatListener",
                vat_listener.client,
                &policy,
                None,
            )?;
            let vat_listener: system_capnp::vat_listener::Client =
                capnp::capability::FromClientHook::new(vat_listener.hook.clone());
            dst.set_vat_listener(vat_listener);

            let vat_client = src.get_vat_client()?;
            let vat_client = maybe_wrap_returned_cap(
                MethodFilterCap::VatClient,
                "network",
                "vatClient",
                vat_client.client,
                &policy,
                None,
            )?;
            let vat_client: system_capnp::vat_client::Client =
                capnp::capability::FromClientHook::new(vat_client.hook.clone());
            dst.set_vat_client(vat_client);

            let http_listener = src.get_http_listener()?;
            let http_listener = maybe_wrap_returned_cap(
                MethodFilterCap::HttpListener,
                "network",
                "httpListener",
                http_listener.client,
                &policy,
                None,
            )?;
            let http_listener: system_capnp::http_listener::Client =
                capnp::capability::FromClientHook::new(http_listener.hook.clone());
            dst.set_http_listener(http_listener);
            Ok(())
        })
    }
}

#[derive(Clone)]
struct MethodFilteredRuntime {
    inner: system_capnp::runtime::Client,
    policy: ExportCapPolicy,
}

#[allow(refining_impl_trait)]
impl system_capnp::runtime::Server for MethodFilteredRuntime {
    fn load(
        self: capnp::capability::Rc<Self>,
        params: system_capnp::runtime::LoadParams,
        mut results: system_capnp::runtime::LoadResults,
    ) -> capnp::capability::Promise<(), capnp::Error> {
        if let Err(e) = allow_method(&self.policy, "runtime", "load") {
            return capnp::capability::Promise::err(e);
        }
        let inner = self.inner.clone();
        let policy = self.policy.clone();
        let wasm = match params.get() {
            Ok(p) => match p.get_wasm() {
                Ok(w) => w.to_vec(),
                Err(e) => return capnp::capability::Promise::err(capnp::Error::from(e)),
            },
            Err(e) => return capnp::capability::Promise::err(capnp::Error::from(e)),
        };
        capnp::capability::Promise::from_future(async move {
            let mut req = inner.load_request();
            req.get().set_wasm(&wasm);
            let resp = req.send().promise.await?;
            let executor = resp.get()?.get_executor()?;
            let executor = maybe_wrap_returned_cap(
                MethodFilterCap::Executor,
                "load",
                "executor",
                executor.client,
                &policy,
                None,
            )?;
            let executor: system_capnp::executor::Client =
                capnp::capability::FromClientHook::new(executor.hook.clone());
            results.get().set_executor(executor);
            Ok(())
        })
    }

    fn shutdown(
        self: capnp::capability::Rc<Self>,
        _params: system_capnp::runtime::ShutdownParams,
        _results: system_capnp::runtime::ShutdownResults,
    ) -> capnp::capability::Promise<(), capnp::Error> {
        if let Err(e) = allow_method(&self.policy, "runtime", "shutdown") {
            return capnp::capability::Promise::err(e);
        }
        let inner = self.inner.clone();
        capnp::capability::Promise::from_future(async move {
            inner.shutdown_request().send().promise.await?;
            Ok(())
        })
    }
}

#[derive(Clone)]
struct MethodFilteredRouting {
    inner: routing_capnp::routing::Client,
    policy: ExportCapPolicy,
}

#[allow(refining_impl_trait)]
impl routing_capnp::routing::Server for MethodFilteredRouting {
    fn provide(
        self: capnp::capability::Rc<Self>,
        params: routing_capnp::routing::ProvideParams,
        _results: routing_capnp::routing::ProvideResults,
    ) -> capnp::capability::Promise<(), capnp::Error> {
        if let Err(e) = allow_method(&self.policy, "routing", "provide") {
            return capnp::capability::Promise::err(e);
        }
        let inner = self.inner.clone();
        let key = match params.get() {
            Ok(p) => match p.get_key() {
                Ok(k) => match k.to_string() {
                    Ok(s) => s,
                    Err(e) => {
                        return capnp::capability::Promise::err(capnp::Error::failed(e.to_string()))
                    }
                },
                Err(e) => return capnp::capability::Promise::err(capnp::Error::from(e)),
            },
            Err(e) => return capnp::capability::Promise::err(capnp::Error::from(e)),
        };
        capnp::capability::Promise::from_future(async move {
            let mut req = inner.provide_request();
            req.get().set_key(&key);
            req.send().promise.await?;
            Ok(())
        })
    }

    fn find_providers(
        self: capnp::capability::Rc<Self>,
        params: routing_capnp::routing::FindProvidersParams,
        _results: routing_capnp::routing::FindProvidersResults,
    ) -> capnp::capability::Promise<(), capnp::Error> {
        if let Err(e) = allow_method(&self.policy, "routing", "findProviders") {
            return capnp::capability::Promise::err(e);
        }
        let inner = self.inner.clone();
        let (key, count, sink) = match params.get() {
            Ok(p) => {
                let key = match p.get_key() {
                    Ok(k) => match k.to_string() {
                        Ok(s) => s,
                        Err(e) => {
                            return capnp::capability::Promise::err(capnp::Error::failed(
                                e.to_string(),
                            ))
                        }
                    },
                    Err(e) => return capnp::capability::Promise::err(capnp::Error::from(e)),
                };
                let count = p.get_count();
                let sink = match p.get_sink() {
                    Ok(s) => s,
                    Err(e) => return capnp::capability::Promise::err(capnp::Error::from(e)),
                };
                (key, count, sink)
            }
            Err(e) => return capnp::capability::Promise::err(capnp::Error::from(e)),
        };
        capnp::capability::Promise::from_future(async move {
            let mut req = inner.find_providers_request();
            req.get().set_key(&key);
            req.get().set_count(count);
            req.get().set_sink(sink);
            req.send().promise.await?;
            Ok(())
        })
    }

    fn hash(
        self: capnp::capability::Rc<Self>,
        params: routing_capnp::routing::HashParams,
        mut results: routing_capnp::routing::HashResults,
    ) -> capnp::capability::Promise<(), capnp::Error> {
        if let Err(e) = allow_method(&self.policy, "routing", "hash") {
            return capnp::capability::Promise::err(e);
        }
        let inner = self.inner.clone();
        let data = match params.get() {
            Ok(p) => match p.get_data() {
                Ok(d) => d.to_vec(),
                Err(e) => return capnp::capability::Promise::err(capnp::Error::from(e)),
            },
            Err(e) => return capnp::capability::Promise::err(capnp::Error::from(e)),
        };
        capnp::capability::Promise::from_future(async move {
            let mut req = inner.hash_request();
            req.get().set_data(&data);
            let resp = req.send().promise.await?;
            let key = resp
                .get()?
                .get_key()?
                .to_string()
                .map_err(|e| capnp::Error::failed(e.to_string()))?;
            results.get().set_key(&key);
            Ok(())
        })
    }

    fn resolve(
        self: capnp::capability::Rc<Self>,
        params: routing_capnp::routing::ResolveParams,
        mut results: routing_capnp::routing::ResolveResults,
    ) -> capnp::capability::Promise<(), capnp::Error> {
        if let Err(e) = allow_method(&self.policy, "routing", "resolve") {
            return capnp::capability::Promise::err(e);
        }
        let inner = self.inner.clone();
        let name = match params.get() {
            Ok(p) => match p.get_name() {
                Ok(n) => match n.to_string() {
                    Ok(s) => s,
                    Err(e) => {
                        return capnp::capability::Promise::err(capnp::Error::failed(e.to_string()))
                    }
                },
                Err(e) => return capnp::capability::Promise::err(capnp::Error::from(e)),
            },
            Err(e) => return capnp::capability::Promise::err(capnp::Error::from(e)),
        };
        capnp::capability::Promise::from_future(async move {
            let mut req = inner.resolve_request();
            req.get().set_name(&name);
            let resp = req.send().promise.await?;
            let path = resp
                .get()?
                .get_path()?
                .to_string()
                .map_err(|e| capnp::Error::failed(e.to_string()))?;
            results.get().set_path(&path);
            Ok(())
        })
    }

    fn mkdir(
        self: capnp::capability::Rc<Self>,
        params: routing_capnp::routing::MkdirParams,
        mut results: routing_capnp::routing::MkdirResults,
    ) -> capnp::capability::Promise<(), capnp::Error> {
        if let Err(e) = allow_method(&self.policy, "routing", "mkdir") {
            return capnp::capability::Promise::err(e);
        }
        let inner = self.inner.clone();
        let (base, path, parents) = match params.get() {
            Ok(p) => {
                let base = match p.get_base_cid() {
                    Ok(v) => match v.to_string() {
                        Ok(s) => s,
                        Err(e) => {
                            return capnp::capability::Promise::err(capnp::Error::failed(
                                e.to_string(),
                            ))
                        }
                    },
                    Err(e) => return capnp::capability::Promise::err(capnp::Error::from(e)),
                };
                let path = match p.get_path() {
                    Ok(v) => match v.to_string() {
                        Ok(s) => s,
                        Err(e) => {
                            return capnp::capability::Promise::err(capnp::Error::failed(
                                e.to_string(),
                            ))
                        }
                    },
                    Err(e) => return capnp::capability::Promise::err(capnp::Error::from(e)),
                };
                (base, path, p.get_parents())
            }
            Err(e) => return capnp::capability::Promise::err(capnp::Error::from(e)),
        };
        capnp::capability::Promise::from_future(async move {
            let mut req = inner.mkdir_request();
            req.get().set_base_cid(&base);
            req.get().set_path(&path);
            req.get().set_parents(parents);
            let resp = req.send().promise.await?;
            let root_cid = resp
                .get()?
                .get_root_cid()?
                .to_string()
                .map_err(|e| capnp::Error::failed(e.to_string()))?;
            results.get().set_root_cid(&root_cid);
            Ok(())
        })
    }

    fn write_file(
        self: capnp::capability::Rc<Self>,
        params: routing_capnp::routing::WriteFileParams,
        mut results: routing_capnp::routing::WriteFileResults,
    ) -> capnp::capability::Promise<(), capnp::Error> {
        if let Err(e) = allow_method(&self.policy, "routing", "writeFile") {
            return capnp::capability::Promise::err(e);
        }
        let inner = self.inner.clone();
        let (base, path, data, create_parents) = match params.get() {
            Ok(p) => {
                let base = match p.get_base_cid() {
                    Ok(v) => match v.to_string() {
                        Ok(s) => s,
                        Err(e) => {
                            return capnp::capability::Promise::err(capnp::Error::failed(
                                e.to_string(),
                            ))
                        }
                    },
                    Err(e) => return capnp::capability::Promise::err(capnp::Error::from(e)),
                };
                let path = match p.get_path() {
                    Ok(v) => match v.to_string() {
                        Ok(s) => s,
                        Err(e) => {
                            return capnp::capability::Promise::err(capnp::Error::failed(
                                e.to_string(),
                            ))
                        }
                    },
                    Err(e) => return capnp::capability::Promise::err(capnp::Error::from(e)),
                };
                let data = match p.get_data() {
                    Ok(v) => v.to_vec(),
                    Err(e) => return capnp::capability::Promise::err(capnp::Error::from(e)),
                };
                (base, path, data, p.get_create_parents())
            }
            Err(e) => return capnp::capability::Promise::err(capnp::Error::from(e)),
        };
        capnp::capability::Promise::from_future(async move {
            let mut req = inner.write_file_request();
            req.get().set_base_cid(&base);
            req.get().set_path(&path);
            req.get().set_data(&data);
            req.get().set_create_parents(create_parents);
            let resp = req.send().promise.await?;
            let root_cid = resp
                .get()?
                .get_root_cid()?
                .to_string()
                .map_err(|e| capnp::Error::failed(e.to_string()))?;
            results.get().set_root_cid(&root_cid);
            Ok(())
        })
    }

    fn remove(
        self: capnp::capability::Rc<Self>,
        params: routing_capnp::routing::RemoveParams,
        mut results: routing_capnp::routing::RemoveResults,
    ) -> capnp::capability::Promise<(), capnp::Error> {
        if let Err(e) = allow_method(&self.policy, "routing", "remove") {
            return capnp::capability::Promise::err(e);
        }
        let inner = self.inner.clone();
        let (base, path, recursive) = match params.get() {
            Ok(p) => {
                let base = match p.get_base_cid() {
                    Ok(v) => match v.to_string() {
                        Ok(s) => s,
                        Err(e) => {
                            return capnp::capability::Promise::err(capnp::Error::failed(
                                e.to_string(),
                            ))
                        }
                    },
                    Err(e) => return capnp::capability::Promise::err(capnp::Error::from(e)),
                };
                let path = match p.get_path() {
                    Ok(v) => match v.to_string() {
                        Ok(s) => s,
                        Err(e) => {
                            return capnp::capability::Promise::err(capnp::Error::failed(
                                e.to_string(),
                            ))
                        }
                    },
                    Err(e) => return capnp::capability::Promise::err(capnp::Error::from(e)),
                };
                (base, path, p.get_recursive())
            }
            Err(e) => return capnp::capability::Promise::err(capnp::Error::from(e)),
        };
        capnp::capability::Promise::from_future(async move {
            let mut req = inner.remove_request();
            req.get().set_base_cid(&base);
            req.get().set_path(&path);
            req.get().set_recursive(recursive);
            let resp = req.send().promise.await?;
            let root_cid = resp
                .get()?
                .get_root_cid()?
                .to_string()
                .map_err(|e| capnp::Error::failed(e.to_string()))?;
            results.get().set_root_cid(&root_cid);
            Ok(())
        })
    }

    fn publish(
        self: capnp::capability::Rc<Self>,
        params: routing_capnp::routing::PublishParams,
        mut results: routing_capnp::routing::PublishResults,
    ) -> capnp::capability::Promise<(), capnp::Error> {
        if let Err(e) = allow_method(&self.policy, "routing", "publish") {
            return capnp::capability::Promise::err(e);
        }
        let inner = self.inner.clone();
        let (name, cid, expected_current) = match params.get() {
            Ok(p) => {
                let name = match p.get_name() {
                    Ok(v) => match v.to_string() {
                        Ok(s) => s,
                        Err(e) => {
                            return capnp::capability::Promise::err(capnp::Error::failed(
                                e.to_string(),
                            ))
                        }
                    },
                    Err(e) => return capnp::capability::Promise::err(capnp::Error::from(e)),
                };
                let cid = match p.get_cid() {
                    Ok(v) => match v.to_string() {
                        Ok(s) => s,
                        Err(e) => {
                            return capnp::capability::Promise::err(capnp::Error::failed(
                                e.to_string(),
                            ))
                        }
                    },
                    Err(e) => return capnp::capability::Promise::err(capnp::Error::from(e)),
                };
                let expected_current = match p.get_expected_current() {
                    Ok(v) => match v.to_string() {
                        Ok(s) => s,
                        Err(e) => {
                            return capnp::capability::Promise::err(capnp::Error::failed(
                                e.to_string(),
                            ))
                        }
                    },
                    Err(e) => return capnp::capability::Promise::err(capnp::Error::from(e)),
                };
                (name, cid, expected_current)
            }
            Err(e) => return capnp::capability::Promise::err(capnp::Error::from(e)),
        };
        capnp::capability::Promise::from_future(async move {
            let mut req = inner.publish_request();
            req.get().set_name(&name);
            req.get().set_cid(&cid);
            req.get().set_expected_current(&expected_current);
            let resp = req.send().promise.await?;
            let published_path = resp
                .get()?
                .get_published_path()?
                .to_string()
                .map_err(|e| capnp::Error::failed(e.to_string()))?;
            results.get().set_published_path(&published_path);
            Ok(())
        })
    }
}

#[derive(Clone)]
struct MethodFilteredIpfs {
    inner: system_capnp::ipfs::Client,
    policy: ExportCapPolicy,
}

#[allow(refining_impl_trait)]
impl system_capnp::ipfs::Server for MethodFilteredIpfs {
    fn read(
        self: capnp::capability::Rc<Self>,
        params: system_capnp::ipfs::ReadParams,
        mut results: system_capnp::ipfs::ReadResults,
    ) -> capnp::capability::Promise<(), capnp::Error> {
        if let Err(e) = allow_method(&self.policy, "ipfs", "read") {
            return capnp::capability::Promise::err(e);
        }
        let inner = self.inner.clone();
        let policy = self.policy.clone();
        let path = match params.get() {
            Ok(p) => match p.get_path() {
                Ok(v) => match v.to_string() {
                    Ok(s) => s,
                    Err(e) => {
                        return capnp::capability::Promise::err(capnp::Error::failed(e.to_string()))
                    }
                },
                Err(e) => return capnp::capability::Promise::err(capnp::Error::from(e)),
            },
            Err(e) => return capnp::capability::Promise::err(capnp::Error::from(e)),
        };
        capnp::capability::Promise::from_future(async move {
            let mut req = inner.read_request();
            req.get().set_path(&path);
            let resp = req.send().promise.await?;
            let stream = resp.get()?.get_stream()?;
            let stream = maybe_wrap_returned_cap(
                MethodFilterCap::ByteStream,
                "read",
                "stream",
                stream.client,
                &policy,
                None,
            )?;
            let stream: system_capnp::byte_stream::Client =
                capnp::capability::FromClientHook::new(stream.hook.clone());
            results.get().set_stream(stream);
            Ok(())
        })
    }
}

#[derive(Clone)]
struct MethodFilteredHttpClient {
    inner: http_capnp::http_client::Client,
    policy: ExportCapPolicy,
}

#[allow(refining_impl_trait)]
impl http_capnp::http_client::Server for MethodFilteredHttpClient {
    fn get(
        self: capnp::capability::Rc<Self>,
        params: http_capnp::http_client::GetParams,
        mut results: http_capnp::http_client::GetResults,
    ) -> capnp::capability::Promise<(), capnp::Error> {
        if let Err(e) = allow_method(&self.policy, "http-client", "get") {
            return capnp::capability::Promise::err(e);
        }
        let inner = self.inner.clone();
        let (url, headers) = match params.get() {
            Ok(p) => {
                let url = match p.get_url() {
                    Ok(v) => match v.to_string() {
                        Ok(s) => s,
                        Err(e) => {
                            return capnp::capability::Promise::err(capnp::Error::failed(
                                e.to_string(),
                            ))
                        }
                    },
                    Err(e) => return capnp::capability::Promise::err(capnp::Error::from(e)),
                };
                let headers = match p.get_headers() {
                    Ok(v) => v,
                    Err(e) => return capnp::capability::Promise::err(capnp::Error::from(e)),
                };
                let mut pairs = Vec::new();
                for i in 0..headers.len() {
                    let h = headers.get(i);
                    let name = match h.get_name() {
                        Ok(v) => match v.to_string() {
                            Ok(s) => s,
                            Err(e) => {
                                return capnp::capability::Promise::err(capnp::Error::failed(
                                    e.to_string(),
                                ))
                            }
                        },
                        Err(e) => return capnp::capability::Promise::err(capnp::Error::from(e)),
                    };
                    let value = match h.get_value() {
                        Ok(v) => match v.to_string() {
                            Ok(s) => s,
                            Err(e) => {
                                return capnp::capability::Promise::err(capnp::Error::failed(
                                    e.to_string(),
                                ))
                            }
                        },
                        Err(e) => return capnp::capability::Promise::err(capnp::Error::from(e)),
                    };
                    pairs.push((name, value));
                }
                (url, pairs)
            }
            Err(e) => return capnp::capability::Promise::err(capnp::Error::from(e)),
        };
        capnp::capability::Promise::from_future(async move {
            let mut req = inner.get_request();
            req.get().set_url(&url);
            let mut out_headers = req.get().init_headers(headers.len() as u32);
            for (i, (name, value)) in headers.iter().enumerate() {
                let mut h = out_headers.reborrow().get(i as u32);
                h.set_name(name);
                h.set_value(value);
            }
            let resp = req.send().promise.await?;
            let src = resp.get()?;
            let mut dst = results.get();
            dst.set_status(src.get_status());
            dst.set_body(src.get_body()?);
            let src_headers = src.get_headers()?;
            let mut dst_headers = dst.init_headers(src_headers.len());
            for i in 0..src_headers.len() {
                let h = src_headers.get(i);
                let mut o = dst_headers.reborrow().get(i);
                o.set_name(h.get_name()?);
                o.set_value(h.get_value()?);
            }
            Ok(())
        })
    }

    fn post(
        self: capnp::capability::Rc<Self>,
        params: http_capnp::http_client::PostParams,
        mut results: http_capnp::http_client::PostResults,
    ) -> capnp::capability::Promise<(), capnp::Error> {
        if let Err(e) = allow_method(&self.policy, "http-client", "post") {
            return capnp::capability::Promise::err(e);
        }
        let inner = self.inner.clone();
        let (url, headers, body) = match params.get() {
            Ok(p) => {
                let url = match p.get_url() {
                    Ok(v) => match v.to_string() {
                        Ok(s) => s,
                        Err(e) => {
                            return capnp::capability::Promise::err(capnp::Error::failed(
                                e.to_string(),
                            ))
                        }
                    },
                    Err(e) => return capnp::capability::Promise::err(capnp::Error::from(e)),
                };
                let mut pairs = Vec::new();
                let headers = match p.get_headers() {
                    Ok(v) => v,
                    Err(e) => return capnp::capability::Promise::err(capnp::Error::from(e)),
                };
                for i in 0..headers.len() {
                    let h = headers.get(i);
                    let name = match h.get_name() {
                        Ok(v) => match v.to_string() {
                            Ok(s) => s,
                            Err(e) => {
                                return capnp::capability::Promise::err(capnp::Error::failed(
                                    e.to_string(),
                                ))
                            }
                        },
                        Err(e) => return capnp::capability::Promise::err(capnp::Error::from(e)),
                    };
                    let value = match h.get_value() {
                        Ok(v) => match v.to_string() {
                            Ok(s) => s,
                            Err(e) => {
                                return capnp::capability::Promise::err(capnp::Error::failed(
                                    e.to_string(),
                                ))
                            }
                        },
                        Err(e) => return capnp::capability::Promise::err(capnp::Error::from(e)),
                    };
                    pairs.push((name, value));
                }
                let body = match p.get_body() {
                    Ok(v) => v.to_vec(),
                    Err(e) => return capnp::capability::Promise::err(capnp::Error::from(e)),
                };
                (url, pairs, body)
            }
            Err(e) => return capnp::capability::Promise::err(capnp::Error::from(e)),
        };
        capnp::capability::Promise::from_future(async move {
            let mut req = inner.post_request();
            req.get().set_url(&url);
            req.get().set_body(&body);
            let mut out_headers = req.get().init_headers(headers.len() as u32);
            for (i, (name, value)) in headers.iter().enumerate() {
                let mut h = out_headers.reborrow().get(i as u32);
                h.set_name(name);
                h.set_value(value);
            }
            let resp = req.send().promise.await?;
            let src = resp.get()?;
            let mut dst = results.get();
            dst.set_status(src.get_status());
            dst.set_body(src.get_body()?);
            let src_headers = src.get_headers()?;
            let mut dst_headers = dst.init_headers(src_headers.len());
            for i in 0..src_headers.len() {
                let h = src_headers.get(i);
                let mut o = dst_headers.reborrow().get(i);
                o.set_name(h.get_name()?);
                o.set_value(h.get_value()?);
            }
            Ok(())
        })
    }
}

#[derive(Clone)]
struct MethodFilteredIdentity {
    inner: auth_capnp::identity::Client,
    policy: ExportCapPolicy,
}

#[allow(refining_impl_trait)]
impl auth_capnp::identity::Server for MethodFilteredIdentity {
    fn signer(
        self: capnp::capability::Rc<Self>,
        params: auth_capnp::identity::SignerParams,
        mut results: auth_capnp::identity::SignerResults,
    ) -> capnp::capability::Promise<(), capnp::Error> {
        if let Err(e) = allow_method(&self.policy, "identity", "signer") {
            return capnp::capability::Promise::err(e);
        }
        let inner = self.inner.clone();
        let policy = self.policy.clone();
        let domain = match params.get() {
            Ok(p) => match p.get_domain() {
                Ok(v) => match v.to_string() {
                    Ok(s) => s,
                    Err(e) => {
                        return capnp::capability::Promise::err(capnp::Error::failed(e.to_string()))
                    }
                },
                Err(e) => return capnp::capability::Promise::err(capnp::Error::from(e)),
            },
            Err(e) => return capnp::capability::Promise::err(capnp::Error::from(e)),
        };
        capnp::capability::Promise::from_future(async move {
            let mut req = inner.signer_request();
            req.get().set_domain(&domain);
            let resp = req.send().promise.await?;
            let signer = resp.get()?.get_signer()?;
            let signer = maybe_wrap_returned_cap(
                MethodFilterCap::Signer,
                "signer",
                "signer",
                signer.client,
                &policy,
                None,
            )?;
            let signer: auth_capnp::signer::Client =
                capnp::capability::FromClientHook::new(signer.hook.clone());
            results.get().set_signer(signer);
            Ok(())
        })
    }

    fn verify(
        self: capnp::capability::Rc<Self>,
        params: auth_capnp::identity::VerifyParams,
        mut results: auth_capnp::identity::VerifyResults,
    ) -> capnp::capability::Promise<(), capnp::Error> {
        if let Err(e) = allow_method(&self.policy, "identity", "verify") {
            return capnp::capability::Promise::err(e);
        }
        let inner = self.inner.clone();
        let (data, sig, pubkey) = match params.get() {
            Ok(p) => {
                let data = match p.get_data() {
                    Ok(v) => v.to_vec(),
                    Err(e) => return capnp::capability::Promise::err(capnp::Error::from(e)),
                };
                let sig = match p.get_signature() {
                    Ok(v) => v.to_vec(),
                    Err(e) => return capnp::capability::Promise::err(capnp::Error::from(e)),
                };
                let pubkey = match p.get_pubkey() {
                    Ok(v) => v.to_vec(),
                    Err(e) => return capnp::capability::Promise::err(capnp::Error::from(e)),
                };
                (data, sig, pubkey)
            }
            Err(e) => return capnp::capability::Promise::err(capnp::Error::from(e)),
        };
        capnp::capability::Promise::from_future(async move {
            let mut req = inner.verify_request();
            req.get().set_data(&data);
            req.get().set_signature(&sig);
            req.get().set_pubkey(&pubkey);
            let resp = req.send().promise.await?;
            results.get().set_valid(resp.get()?.get_valid());
            Ok(())
        })
    }
}

#[derive(Clone)]
struct MethodFilteredStreamListener {
    inner: system_capnp::stream_listener::Client,
    policy: ExportCapPolicy,
}

#[allow(refining_impl_trait)]
impl system_capnp::stream_listener::Server for MethodFilteredStreamListener {
    fn listen(
        self: capnp::capability::Rc<Self>,
        params: system_capnp::stream_listener::ListenParams,
        _results: system_capnp::stream_listener::ListenResults,
    ) -> capnp::capability::Promise<(), capnp::Error> {
        if let Err(e) = allow_method(&self.policy, "stream-listener", "listen") {
            return capnp::capability::Promise::err(e);
        }
        let inner = self.inner.clone();
        let (executor, protocol) = match params.get() {
            Ok(p) => {
                let executor = match p.get_executor() {
                    Ok(v) => v,
                    Err(e) => return capnp::capability::Promise::err(capnp::Error::from(e)),
                };
                let protocol = match p.get_protocol() {
                    Ok(v) => match v.to_string() {
                        Ok(s) => s,
                        Err(e) => {
                            return capnp::capability::Promise::err(capnp::Error::failed(
                                e.to_string(),
                            ))
                        }
                    },
                    Err(e) => return capnp::capability::Promise::err(capnp::Error::from(e)),
                };
                (executor, protocol)
            }
            Err(e) => return capnp::capability::Promise::err(capnp::Error::from(e)),
        };
        capnp::capability::Promise::from_future(async move {
            let mut req = inner.listen_request();
            req.get().set_executor(executor);
            req.get().set_protocol(&protocol);
            req.send().promise.await?;
            Ok(())
        })
    }
}

#[derive(Clone)]
struct MethodFilteredStreamDialer {
    inner: system_capnp::stream_dialer::Client,
    policy: ExportCapPolicy,
}

#[allow(refining_impl_trait)]
impl system_capnp::stream_dialer::Server for MethodFilteredStreamDialer {
    fn dial(
        self: capnp::capability::Rc<Self>,
        params: system_capnp::stream_dialer::DialParams,
        mut results: system_capnp::stream_dialer::DialResults,
    ) -> capnp::capability::Promise<(), capnp::Error> {
        if let Err(e) = allow_method(&self.policy, "stream-dialer", "dial") {
            return capnp::capability::Promise::err(e);
        }
        let inner = self.inner.clone();
        let (peer, protocol) = match params.get() {
            Ok(p) => {
                let peer = match p.get_peer() {
                    Ok(v) => v.to_vec(),
                    Err(e) => return capnp::capability::Promise::err(capnp::Error::from(e)),
                };
                let protocol = match p.get_protocol() {
                    Ok(v) => match v.to_string() {
                        Ok(s) => s,
                        Err(e) => {
                            return capnp::capability::Promise::err(capnp::Error::failed(
                                e.to_string(),
                            ))
                        }
                    },
                    Err(e) => return capnp::capability::Promise::err(capnp::Error::from(e)),
                };
                (peer, protocol)
            }
            Err(e) => return capnp::capability::Promise::err(capnp::Error::from(e)),
        };
        let policy = self.policy.clone();
        capnp::capability::Promise::from_future(async move {
            let mut req = inner.dial_request();
            req.get().set_peer(&peer);
            req.get().set_protocol(&protocol);
            let resp = req.send().promise.await?;
            let stream = resp.get()?.get_stream()?;
            let stream = maybe_wrap_returned_cap(
                MethodFilterCap::ByteStream,
                "dial",
                "stream",
                stream.client,
                &policy,
                None,
            )?;
            let stream: system_capnp::byte_stream::Client =
                capnp::capability::FromClientHook::new(stream.hook.clone());
            results.get().set_stream(stream);
            Ok(())
        })
    }
}

#[derive(Clone)]
struct MethodFilteredVatListener {
    inner: system_capnp::vat_listener::Client,
    policy: ExportCapPolicy,
}

#[allow(refining_impl_trait)]
impl system_capnp::vat_listener::Server for MethodFilteredVatListener {
    fn listen(
        self: capnp::capability::Rc<Self>,
        params: system_capnp::vat_listener::ListenParams,
        _results: system_capnp::vat_listener::ListenResults,
    ) -> capnp::capability::Promise<(), capnp::Error> {
        if let Err(e) = allow_method(&self.policy, "vat-listener", "listen") {
            return capnp::capability::Promise::err(e);
        }
        let inner = self.inner.clone();
        let mut req = inner.listen_request();
        {
            let p = match params.get() {
                Ok(v) => v,
                Err(e) => return capnp::capability::Promise::err(capnp::Error::from(e)),
            };
            let handler = match p.get_handler() {
                Ok(v) => v,
                Err(e) => return capnp::capability::Promise::err(capnp::Error::from(e)),
            };
            let schema = match p.get_schema() {
                Ok(v) => v,
                Err(e) => return capnp::capability::Promise::err(capnp::Error::from(e)),
            };
            let caps = match p.get_caps() {
                Ok(v) => v,
                Err(e) => return capnp::capability::Promise::err(capnp::Error::from(e)),
            };

            let mut out = req.get();
            {
                let mut out_handler = out.reborrow().init_handler();
                match handler.which() {
                    Ok(system_capnp::vat_handler::WhichReader::Spawn(executor)) => {
                        let executor = match executor {
                            Ok(v) => v,
                            Err(e) => return capnp::capability::Promise::err(e),
                        };
                        out_handler.set_spawn(executor);
                    }
                    Ok(system_capnp::vat_handler::WhichReader::Serve(typed)) => {
                        let typed = match typed {
                            Ok(v) => v,
                            Err(e) => return capnp::capability::Promise::err(e),
                        };
                        let cap = match typed.get_cap().get_as_capability::<capnp::capability::Client>() {
                            Ok(v) => v,
                            Err(e) => return capnp::capability::Promise::err(e),
                        };
                        let mut out_typed = out_handler.init_serve();
                        out_typed
                            .reborrow()
                            .init_cap()
                            .set_as_capability(cap.hook.clone());
                        if !typed.has_schema() {
                            return capnp::capability::Promise::err(capnp::Error::failed(
                                "vat-listener.listen serve handler TypedCap missing schema".into(),
                            ));
                        }
                        let schema = match typed.get_schema() {
                            Ok(v) => v,
                            Err(e) => {
                                return capnp::capability::Promise::err(capnp::Error::from(e));
                            }
                        };
                        let root = match schema.get_root() {
                            Ok(v) => v,
                            Err(e) => {
                                return capnp::capability::Promise::err(capnp::Error::from(e));
                            }
                        };
                        let deps = match schema.get_deps() {
                            Ok(v) => v,
                            Err(e) => {
                                return capnp::capability::Promise::err(capnp::Error::from(e));
                            }
                        };
                        let mut out_schema = out_typed.reborrow().init_schema();
                        if let Err(e) = out_schema.set_root(root) {
                            return capnp::capability::Promise::err(e);
                        }
                        if let Err(e) = out_schema.set_deps(deps) {
                            return capnp::capability::Promise::err(e);
                        }
                    }
                    Err(e) => return capnp::capability::Promise::err(capnp::Error::from(e)),
                }
            }
            out.reborrow().set_schema(schema);
            let mut out_caps = out.init_caps(caps.len());
            for i in 0..caps.len() {
                let src = caps.get(i);
                let mut dst = out_caps.reborrow().get(i);
                let name = match src.get_name() {
                    Ok(v) => v,
                    Err(e) => return capnp::capability::Promise::err(capnp::Error::from(e)),
                };
                dst.set_name(name);
                if src.has_schema() {
                    let schema = match src.get_schema() {
                        Ok(v) => v,
                        Err(e) => return capnp::capability::Promise::err(capnp::Error::from(e)),
                    };
                    if let Err(e) = dst.set_schema(schema) {
                        return capnp::capability::Promise::err(capnp::Error::from(e));
                    }
                }
                let cap = match src
                    .get_cap()
                    .get_as_capability::<capnp::capability::Client>()
                {
                    Ok(v) => v,
                    Err(e) => return capnp::capability::Promise::err(e),
                };
                dst.reborrow()
                    .init_cap()
                    .set_as_capability(cap.hook.clone());
            }
        }
        let promise = req.send().promise;
        capnp::capability::Promise::from_future(async move {
            promise.await?;
            Ok(())
        })
    }
}

#[derive(Clone)]
struct MethodFilteredVatClient {
    inner: system_capnp::vat_client::Client,
    policy: ExportCapPolicy,
}

#[allow(refining_impl_trait)]
impl system_capnp::vat_client::Server for MethodFilteredVatClient {
    fn dial(
        self: capnp::capability::Rc<Self>,
        params: system_capnp::vat_client::DialParams,
        mut results: system_capnp::vat_client::DialResults,
    ) -> capnp::capability::Promise<(), capnp::Error> {
        if let Err(e) = allow_method(&self.policy, "vat-client", "dial") {
            return capnp::capability::Promise::err(e);
        }
        let inner = self.inner.clone();
        let policy = self.policy.clone();
        let (peer, schema) = match params.get() {
            Ok(p) => {
                let peer = match p.get_peer() {
                    Ok(v) => v.to_vec(),
                    Err(e) => return capnp::capability::Promise::err(capnp::Error::from(e)),
                };
                let schema = match p.get_schema() {
                    Ok(v) => v.to_vec(),
                    Err(e) => return capnp::capability::Promise::err(capnp::Error::from(e)),
                };
                (peer, schema)
            }
            Err(e) => return capnp::capability::Promise::err(capnp::Error::from(e)),
        };
        capnp::capability::Promise::from_future(async move {
            let mut req = inner.dial_request();
            req.get().set_peer(&peer);
            req.get().set_schema(&schema);
            let resp = req.send().promise.await?;
            let typed = resp.get()?.get_typed()?;
            let cap = typed.get_cap().get_as_capability::<capnp::capability::Client>()?;
            let typed_schema = typed.get_schema()?;
            let typed_schema_root = typed_schema.get_root()?;
            let typed_schema_root_bytes = canonicalize_schema_node_bytes(typed_schema_root)?;
            let cap = maybe_wrap_returned_cap(
                MethodFilterCap::DynamicAny,
                "dial",
                "cap",
                cap,
                &policy,
                Some(&typed_schema_root_bytes),
            )?;
            let mut out_typed = results.get().init_typed();
            out_typed
                .reborrow()
                .init_cap()
                .set_as_capability(cap.hook.clone());
            let deps = typed_schema.get_deps()?;
            let mut out_schema = out_typed.reborrow().init_schema();
            out_schema.set_root(typed_schema_root)?;
            out_schema.set_deps(deps)?;
            Ok(())
        })
    }
}

#[derive(Clone)]
struct MethodFilteredHttpListener {
    inner: system_capnp::http_listener::Client,
    policy: ExportCapPolicy,
}

#[allow(refining_impl_trait)]
impl system_capnp::http_listener::Server for MethodFilteredHttpListener {
    fn listen(
        self: capnp::capability::Rc<Self>,
        params: system_capnp::http_listener::ListenParams,
        _results: system_capnp::http_listener::ListenResults,
    ) -> capnp::capability::Promise<(), capnp::Error> {
        if let Err(e) = allow_method(&self.policy, "http-listener", "listen") {
            return capnp::capability::Promise::err(e);
        }
        let inner = self.inner.clone();
        let mut req = inner.listen_request();
        {
            let p = match params.get() {
                Ok(v) => v,
                Err(e) => return capnp::capability::Promise::err(capnp::Error::from(e)),
            };
            let executor = match p.get_executor() {
                Ok(v) => v,
                Err(e) => return capnp::capability::Promise::err(capnp::Error::from(e)),
            };
            let prefix = match p.get_prefix() {
                Ok(v) => v,
                Err(e) => return capnp::capability::Promise::err(capnp::Error::from(e)),
            };
            let caps = match p.get_caps() {
                Ok(v) => v,
                Err(e) => return capnp::capability::Promise::err(capnp::Error::from(e)),
            };

            let mut out = req.get();
            out.reborrow().set_executor(executor);
            out.reborrow().set_prefix(prefix);
            let mut out_caps = out.init_caps(caps.len());
            for i in 0..caps.len() {
                let src = caps.get(i);
                let mut dst = out_caps.reborrow().get(i);
                let name = match src.get_name() {
                    Ok(v) => v,
                    Err(e) => return capnp::capability::Promise::err(capnp::Error::from(e)),
                };
                dst.set_name(name);
                if src.has_schema() {
                    let schema = match src.get_schema() {
                        Ok(v) => v,
                        Err(e) => return capnp::capability::Promise::err(capnp::Error::from(e)),
                    };
                    if let Err(e) = dst.set_schema(schema) {
                        return capnp::capability::Promise::err(capnp::Error::from(e));
                    }
                }
                let cap = match src
                    .get_cap()
                    .get_as_capability::<capnp::capability::Client>()
                {
                    Ok(v) => v,
                    Err(e) => return capnp::capability::Promise::err(e),
                };
                dst.reborrow()
                    .init_cap()
                    .set_as_capability(cap.hook.clone());
            }
        }
        let promise = req.send().promise;
        capnp::capability::Promise::from_future(async move {
            promise.await?;
            Ok(())
        })
    }
}

#[derive(Clone)]
struct MethodFilteredExecutor {
    inner: system_capnp::executor::Client,
    policy: ExportCapPolicy,
}

#[allow(refining_impl_trait)]
impl system_capnp::executor::Server for MethodFilteredExecutor {
    fn spawn(
        self: capnp::capability::Rc<Self>,
        params: system_capnp::executor::SpawnParams,
        mut results: system_capnp::executor::SpawnResults,
    ) -> capnp::capability::Promise<(), capnp::Error> {
        if let Err(e) = allow_method(&self.policy, "executor", "spawn") {
            return capnp::capability::Promise::err(e);
        }
        let inner = self.inner.clone();
        let policy = self.policy.clone();
        capnp::capability::Promise::from_future(async move {
            let p = params.get()?;
            let args = p.get_args()?;
            let env = p.get_env()?;
            let caps = p.get_caps()?;
            let fuel = p.get_fuel_policy()?;

            let mut req = inner.spawn_request();
            {
                let mut out = req.get();
                let mut out_args = out.reborrow().init_args(args.len());
                for i in 0..args.len() {
                    out_args.set(i, args.get(i)?);
                }
                let mut out_env = out.reborrow().init_env(env.len());
                for i in 0..env.len() {
                    out_env.set(i, env.get(i)?);
                }
                let mut out_caps = out.reborrow().init_caps(caps.len());
                for i in 0..caps.len() {
                    let src = caps.get(i);
                    let mut dst = out_caps.reborrow().get(i);
                    dst.set_name(src.get_name()?);
                    if src.has_schema() {
                        dst.set_schema(src.get_schema()?)?;
                    }
                    let cap = src
                        .get_cap()
                        .get_as_capability::<capnp::capability::Client>()?;
                    dst.reborrow()
                        .init_cap()
                        .set_as_capability(cap.hook.clone());
                }
                let mut out_fuel = out.init_fuel_policy();
                match fuel.which()? {
                    system_capnp::fuel_policy::Which::Scheduled(()) => out_fuel.set_scheduled(()),
                    system_capnp::fuel_policy::Which::Oneshot(src) => {
                        let src = src?;
                        let mut dst = out_fuel.init_oneshot();
                        dst.set_total_budget(src.get_total_budget());
                        dst.set_max_per_epoch(src.get_max_per_epoch());
                        dst.set_min_per_epoch(src.get_min_per_epoch());
                    }
                }
            }
            let resp = req.send().promise.await?;
            let process = resp.get()?.get_process()?;
            let process = maybe_wrap_returned_cap(
                MethodFilterCap::Process,
                "spawn",
                "process",
                process.client,
                &policy,
                None,
            )?;
            let process: system_capnp::process::Client =
                capnp::capability::FromClientHook::new(process.hook.clone());
            results.get().set_process(process);
            Ok(())
        })
    }
}

#[derive(Clone)]
struct MethodFilteredProcess {
    inner: system_capnp::process::Client,
    policy: ExportCapPolicy,
}

#[allow(refining_impl_trait)]
impl system_capnp::process::Server for MethodFilteredProcess {
    fn stdin(
        self: capnp::capability::Rc<Self>,
        _params: system_capnp::process::StdinParams,
        mut results: system_capnp::process::StdinResults,
    ) -> capnp::capability::Promise<(), capnp::Error> {
        if let Err(e) = allow_method(&self.policy, "process", "stdin") {
            return capnp::capability::Promise::err(e);
        }
        let inner = self.inner.clone();
        let policy = self.policy.clone();
        capnp::capability::Promise::from_future(async move {
            let resp = inner.stdin_request().send().promise.await?;
            let stream = resp.get()?.get_stream()?;
            let stream = maybe_wrap_returned_cap(
                MethodFilterCap::ByteStream,
                "stdin",
                "stream",
                stream.client,
                &policy,
                None,
            )?;
            let stream: system_capnp::byte_stream::Client =
                capnp::capability::FromClientHook::new(stream.hook.clone());
            results.get().set_stream(stream);
            Ok(())
        })
    }

    fn stdout(
        self: capnp::capability::Rc<Self>,
        _params: system_capnp::process::StdoutParams,
        mut results: system_capnp::process::StdoutResults,
    ) -> capnp::capability::Promise<(), capnp::Error> {
        if let Err(e) = allow_method(&self.policy, "process", "stdout") {
            return capnp::capability::Promise::err(e);
        }
        let inner = self.inner.clone();
        let policy = self.policy.clone();
        capnp::capability::Promise::from_future(async move {
            let resp = inner.stdout_request().send().promise.await?;
            let stream = resp.get()?.get_stream()?;
            let stream = maybe_wrap_returned_cap(
                MethodFilterCap::ByteStream,
                "stdout",
                "stream",
                stream.client,
                &policy,
                None,
            )?;
            let stream: system_capnp::byte_stream::Client =
                capnp::capability::FromClientHook::new(stream.hook.clone());
            results.get().set_stream(stream);
            Ok(())
        })
    }

    fn stderr(
        self: capnp::capability::Rc<Self>,
        _params: system_capnp::process::StderrParams,
        mut results: system_capnp::process::StderrResults,
    ) -> capnp::capability::Promise<(), capnp::Error> {
        if let Err(e) = allow_method(&self.policy, "process", "stderr") {
            return capnp::capability::Promise::err(e);
        }
        let inner = self.inner.clone();
        let policy = self.policy.clone();
        capnp::capability::Promise::from_future(async move {
            let resp = inner.stderr_request().send().promise.await?;
            let stream = resp.get()?.get_stream()?;
            let stream = maybe_wrap_returned_cap(
                MethodFilterCap::ByteStream,
                "stderr",
                "stream",
                stream.client,
                &policy,
                None,
            )?;
            let stream: system_capnp::byte_stream::Client =
                capnp::capability::FromClientHook::new(stream.hook.clone());
            results.get().set_stream(stream);
            Ok(())
        })
    }

    fn wait(
        self: capnp::capability::Rc<Self>,
        _params: system_capnp::process::WaitParams,
        mut results: system_capnp::process::WaitResults,
    ) -> capnp::capability::Promise<(), capnp::Error> {
        if let Err(e) = allow_method(&self.policy, "process", "wait") {
            return capnp::capability::Promise::err(e);
        }
        let inner = self.inner.clone();
        capnp::capability::Promise::from_future(async move {
            let resp = inner.wait_request().send().promise.await?;
            results.get().set_exit_code(resp.get()?.get_exit_code());
            Ok(())
        })
    }

    fn bootstrap(
        self: capnp::capability::Rc<Self>,
        params: system_capnp::process::BootstrapParams,
        mut results: system_capnp::process::BootstrapResults,
    ) -> capnp::capability::Promise<(), capnp::Error> {
        if let Err(e) = allow_method(&self.policy, "process", "bootstrap") {
            return capnp::capability::Promise::err(e);
        }
        let inner = self.inner.clone();
        let policy = self.policy.clone();
        let schema = match params.get() {
            Ok(p) => match p.get_schema() {
                Ok(v) => v.to_vec(),
                Err(e) => return capnp::capability::Promise::err(capnp::Error::from(e)),
            },
            Err(e) => return capnp::capability::Promise::err(capnp::Error::from(e)),
        };
        capnp::capability::Promise::from_future(async move {
            let mut req = inner.bootstrap_request();
            req.get().set_schema(&schema);
            let resp = req.send().promise.await?;
            let typed = resp.get()?.get_typed()?;
            let cap = typed.get_cap().get_as_capability::<capnp::capability::Client>()?;
            let typed_schema = typed.get_schema()?;
            let typed_schema_root = typed_schema.get_root()?;
            let typed_schema_root_bytes = canonicalize_schema_node_bytes(typed_schema_root)?;
            let cap = maybe_wrap_returned_cap(
                MethodFilterCap::DynamicAny,
                "bootstrap",
                "cap",
                cap,
                &policy,
                Some(&typed_schema_root_bytes),
            )?;
            let mut out_typed = results.get().init_typed();
            out_typed
                .reborrow()
                .init_cap()
                .set_as_capability(cap.hook.clone());
            let deps = typed_schema.get_deps()?;
            let mut out_schema = out_typed.reborrow().init_schema();
            out_schema.set_root(typed_schema_root)?;
            out_schema.set_deps(deps)?;
            Ok(())
        })
    }

    fn kill(
        self: capnp::capability::Rc<Self>,
        _params: system_capnp::process::KillParams,
        _results: system_capnp::process::KillResults,
    ) -> capnp::capability::Promise<(), capnp::Error> {
        if let Err(e) = allow_method(&self.policy, "process", "kill") {
            return capnp::capability::Promise::err(e);
        }
        let inner = self.inner.clone();
        capnp::capability::Promise::from_future(async move {
            inner.kill_request().send().promise.await?;
            Ok(())
        })
    }
}

#[derive(Clone)]
struct MethodFilteredSigner {
    inner: auth_capnp::signer::Client,
    policy: ExportCapPolicy,
}

#[allow(refining_impl_trait)]
impl auth_capnp::signer::Server for MethodFilteredSigner {
    fn sign(
        self: capnp::capability::Rc<Self>,
        params: auth_capnp::signer::SignParams,
        mut results: auth_capnp::signer::SignResults,
    ) -> capnp::capability::Promise<(), capnp::Error> {
        if let Err(e) = allow_method(&self.policy, "signer", "sign") {
            return capnp::capability::Promise::err(e);
        }
        let inner = self.inner.clone();
        let (nonce, epoch_seq) = match params.get() {
            Ok(p) => (p.get_nonce(), p.get_epoch_seq()),
            Err(e) => return capnp::capability::Promise::err(capnp::Error::from(e)),
        };
        capnp::capability::Promise::from_future(async move {
            let mut req = inner.sign_request();
            req.get().set_nonce(nonce);
            req.get().set_epoch_seq(epoch_seq);
            let resp = req.send().promise.await?;
            results.get().set_sig(resp.get()?.get_sig()?);
            Ok(())
        })
    }
}

#[derive(Clone)]
struct MethodFilteredByteStream {
    inner: system_capnp::byte_stream::Client,
    policy: ExportCapPolicy,
}

#[allow(refining_impl_trait)]
impl system_capnp::byte_stream::Server for MethodFilteredByteStream {
    fn read(
        self: capnp::capability::Rc<Self>,
        params: system_capnp::byte_stream::ReadParams,
        mut results: system_capnp::byte_stream::ReadResults,
    ) -> capnp::capability::Promise<(), capnp::Error> {
        if let Err(e) = allow_method(&self.policy, "byte-stream", "read") {
            return capnp::capability::Promise::err(e);
        }
        let inner = self.inner.clone();
        let max_bytes = match params.get() {
            Ok(p) => p.get_max_bytes(),
            Err(e) => return capnp::capability::Promise::err(capnp::Error::from(e)),
        };
        capnp::capability::Promise::from_future(async move {
            let mut req = inner.read_request();
            req.get().set_max_bytes(max_bytes);
            let resp = req.send().promise.await?;
            results.get().set_data(resp.get()?.get_data()?);
            Ok(())
        })
    }

    fn write(
        self: capnp::capability::Rc<Self>,
        params: system_capnp::byte_stream::WriteParams,
        _results: system_capnp::byte_stream::WriteResults,
    ) -> capnp::capability::Promise<(), capnp::Error> {
        if let Err(e) = allow_method(&self.policy, "byte-stream", "write") {
            return capnp::capability::Promise::err(e);
        }
        let inner = self.inner.clone();
        let data = match params.get() {
            Ok(p) => match p.get_data() {
                Ok(v) => v.to_vec(),
                Err(e) => return capnp::capability::Promise::err(capnp::Error::from(e)),
            },
            Err(e) => return capnp::capability::Promise::err(capnp::Error::from(e)),
        };
        capnp::capability::Promise::from_future(async move {
            let mut req = inner.write_request();
            req.get().set_data(&data);
            req.send().promise.await?;
            Ok(())
        })
    }

    fn close(
        self: capnp::capability::Rc<Self>,
        _params: system_capnp::byte_stream::CloseParams,
        _results: system_capnp::byte_stream::CloseResults,
    ) -> capnp::capability::Promise<(), capnp::Error> {
        if let Err(e) = allow_method(&self.policy, "byte-stream", "close") {
            return capnp::capability::Promise::err(e);
        }
        let inner = self.inner.clone();
        capnp::capability::Promise::from_future(async move {
            inner.close_request().send().promise.await?;
            Ok(())
        })
    }
}

fn parse_policy_cap_name(v: &Val) -> Result<String, capnp::Error> {
    match v {
        Val::Str(s) | Val::Sym(s) | Val::Keyword(s) => Ok(s.clone()),
        other => Err(capnp::Error::failed(format!(
            "export policy: expected cap name string/symbol/keyword, got {other}"
        ))),
    }
}

fn convert_att_policy(policy: &AttenuationPolicy) -> ExportCapPolicy {
    let mut returns = BTreeMap::new();
    for (method, fields) in &policy.returns {
        let mut mapped_fields = BTreeMap::new();
        for (field, nested) in fields {
            mapped_fields.insert(field.clone(), convert_att_policy(nested));
        }
        returns.insert(method.clone(), mapped_fields);
    }
    ExportCapPolicy {
        allow_methods: Some(policy.allow_methods.clone()),
        returns,
    }
}

fn parse_export_cap_value(cap_name: &str, value: &Val) -> Result<ExportCapPolicy, capnp::Error> {
    let Val::Cap { name, inner, .. } = value else {
        return Err(capnp::Error::failed(format!(
            "init.glia export '{cap_name}' must map to a capability value, got {value}"
        )));
    };

    if let Some(att) = inner.downcast_ref::<AttenuatedCapInner>() {
        let base_name = match &att.base {
            Val::Cap { name, .. } => name.as_str(),
            Val::Keyword(k) if k == "self" => {
                return Err(capnp::Error::failed(format!(
                    "init.glia export '{cap_name}' cannot use :self as top-level base"
                )))
            }
            other => {
                return Err(capnp::Error::failed(format!(
                    "init.glia export '{cap_name}' attenuation base must be a cap, got {other}"
                )))
            }
        };
        if base_name != cap_name {
            return Err(capnp::Error::failed(format!(
                "init.glia export key '{cap_name}' must attenuate the '{cap_name}' cap, got base '{base_name}'"
            )));
        }
        return Ok(convert_att_policy(&att.policy));
    }

    if name != cap_name {
        return Err(capnp::Error::failed(format!(
            "init.glia export key '{cap_name}' must map to cap '{cap_name}', got '{name}'"
        )));
    }

    Ok(ExportCapPolicy::default())
}

fn is_supported_method(kind: MethodFilterCap, method: &str) -> bool {
    match kind {
        MethodFilterCap::Host => matches!(method, "id" | "addrs" | "peers" | "network"),
        MethodFilterCap::Runtime => matches!(method, "load" | "shutdown"),
        MethodFilterCap::Routing => matches!(
            method,
            "provide"
                | "findProviders"
                | "hash"
                | "resolve"
                | "mkdir"
                | "writeFile"
                | "remove"
                | "publish"
        ),
        MethodFilterCap::Identity => matches!(method, "signer" | "verify"),
        MethodFilterCap::Ipfs => method == "read",
        MethodFilterCap::HttpClient => matches!(method, "get" | "post"),
        MethodFilterCap::StreamListener => method == "listen",
        MethodFilterCap::StreamDialer => method == "dial",
        MethodFilterCap::VatListener => method == "listen",
        MethodFilterCap::VatClient => method == "dial",
        MethodFilterCap::HttpListener => method == "listen",
        MethodFilterCap::Executor => method == "spawn",
        MethodFilterCap::Process => {
            matches!(
                method,
                "stdin" | "stdout" | "stderr" | "wait" | "bootstrap" | "kill"
            )
        }
        MethodFilterCap::Signer => method == "sign",
        MethodFilterCap::ByteStream => matches!(method, "read" | "write" | "close"),
        MethodFilterCap::DynamicAny => true,
    }
}

fn return_field_cap_kind(
    kind: MethodFilterCap,
    method: &str,
    field: &str,
) -> Option<MethodFilterCap> {
    match (kind, method, field) {
        (MethodFilterCap::Host, "network", "streamListener") => {
            Some(MethodFilterCap::StreamListener)
        }
        (MethodFilterCap::Host, "network", "streamDialer") => Some(MethodFilterCap::StreamDialer),
        (MethodFilterCap::Host, "network", "vatListener") => Some(MethodFilterCap::VatListener),
        (MethodFilterCap::Host, "network", "vatClient") => Some(MethodFilterCap::VatClient),
        (MethodFilterCap::Host, "network", "httpListener") => Some(MethodFilterCap::HttpListener),
        (MethodFilterCap::Runtime, "load", "executor") => Some(MethodFilterCap::Executor),
        (MethodFilterCap::Identity, "signer", "signer") => Some(MethodFilterCap::Signer),
        (MethodFilterCap::Ipfs, "read", "stream") => Some(MethodFilterCap::ByteStream),
        (MethodFilterCap::StreamDialer, "dial", "stream") => Some(MethodFilterCap::ByteStream),
        (MethodFilterCap::Executor, "spawn", "process") => Some(MethodFilterCap::Process),
        (MethodFilterCap::Process, "stdin", "stream") => Some(MethodFilterCap::ByteStream),
        (MethodFilterCap::Process, "stdout", "stream") => Some(MethodFilterCap::ByteStream),
        (MethodFilterCap::Process, "stderr", "stream") => Some(MethodFilterCap::ByteStream),
        (MethodFilterCap::Process, "bootstrap", "cap") => Some(MethodFilterCap::DynamicAny),
        (MethodFilterCap::VatClient, "dial", "cap") => Some(MethodFilterCap::DynamicAny),
        _ => None,
    }
}

fn method_supports_cap_returns(kind: MethodFilterCap, method: &str) -> bool {
    matches!(
        (kind, method),
        (MethodFilterCap::Host, "network")
            | (MethodFilterCap::Runtime, "load")
            | (MethodFilterCap::Identity, "signer")
            | (MethodFilterCap::Ipfs, "read")
            | (MethodFilterCap::StreamDialer, "dial")
            | (MethodFilterCap::Executor, "spawn")
            | (MethodFilterCap::Process, "stdin" | "stdout" | "stderr" | "bootstrap")
            | (MethodFilterCap::VatClient, "dial")
    )
}

fn validate_cap_policy(
    cap_name: &str,
    kind: MethodFilterCap,
    policy: &ExportCapPolicy,
) -> Result<(), capnp::Error> {
    if matches!(kind, MethodFilterCap::DynamicAny) {
        return Ok(());
    }

    if let Some(allow) = &policy.allow_methods {
        for method in allow {
            if !is_supported_method(kind, method) {
                return Err(capnp::Error::failed(format!(
                    "init.glia export '{cap_name}' references unknown method '{method}'"
                )));
            }
        }
    }

    for (method, fields) in &policy.returns {
        if !is_supported_method(kind, method) {
            return Err(capnp::Error::failed(format!(
                "init.glia export '{cap_name}' :returns references unknown method '{method}'"
            )));
        }
        if let Some(allow) = &policy.allow_methods {
            if !allow.contains(method) {
                return Err(capnp::Error::failed(format!(
                    "init.glia export '{cap_name}' :returns method '{method}' must also be allowed by :allow"
                )));
            }
        }
        if !method_supports_cap_returns(kind, method) {
            return Err(capnp::Error::failed(format!(
                "init.glia export '{cap_name}' method '{method}' does not return capability fields"
            )));
        }
        for (field, nested) in fields {
            let Some(child_kind) = return_field_cap_kind(kind, method, field) else {
                return Err(capnp::Error::failed(format!(
                    "init.glia export '{cap_name}' method '{method}' references unknown return field '{field}'"
                )));
            };
            validate_cap_policy(&format!("{cap_name}.{method}.{field}"), child_kind, nested)?;
        }
    }
    Ok(())
}

fn parse_export_policy(v: &Val) -> Result<ExportPolicy, capnp::Error> {
    let root = match v {
        Val::Map(m) => m,
        other => {
            return Err(capnp::Error::failed(format!(
                "init.glia must return a map, got {other}"
            )))
        }
    };

    if root.get(&Val::Keyword("export".into())).is_some() {
        return Err(capnp::Error::failed(
            "init.glia legacy {:export {:caps ... :methods ...}} policy is no longer supported; return a bare export map {:host host-cap ...}".into(),
        ));
    }

    let mut caps = BTreeMap::new();
    for (k, v) in root.iter() {
        let cap_name = parse_policy_cap_name(k)?;
        let Some(kind) = method_filter_cap(&cap_name) else {
            return Err(capnp::Error::failed(format!(
                "init.glia export references unknown cap '{cap_name}'"
            )));
        };
        let cap_policy = parse_export_cap_value(&cap_name, v)?;
        validate_cap_policy(&cap_name, kind, &cap_policy)?;
        if caps.insert(cap_name.clone(), cap_policy).is_some() {
            return Err(capnp::Error::failed(format!(
                "init.glia export contains duplicate cap key '{cap_name}'"
            )));
        }
    }

    Ok(ExportPolicy { caps })
}

fn maybe_wrap_export_cap(
    kind: MethodFilterCap,
    base: capnp::capability::Client,
    policy: &ExportCapPolicy,
) -> Result<capnp::capability::Client, capnp::Error> {
    if policy.allow_methods.is_none() && policy.returns.is_empty() {
        return Ok(base);
    }

    match kind {
        MethodFilterCap::Host => {
            let typed = capnp::capability::FromClientHook::new(base.hook.clone());
            let wrapped: system_capnp::host::Client = capnp_rpc::new_client(MethodFilteredHost {
                inner: typed,
                policy: policy.clone(),
            });
            Ok(wrapped.client)
        }
        MethodFilterCap::Runtime => {
            let typed = capnp::capability::FromClientHook::new(base.hook.clone());
            let wrapped: system_capnp::runtime::Client =
                capnp_rpc::new_client(MethodFilteredRuntime {
                    inner: typed,
                    policy: policy.clone(),
                });
            Ok(wrapped.client)
        }
        MethodFilterCap::Routing => {
            let typed = capnp::capability::FromClientHook::new(base.hook.clone());
            let wrapped: routing_capnp::routing::Client =
                capnp_rpc::new_client(MethodFilteredRouting {
                    inner: typed,
                    policy: policy.clone(),
                });
            Ok(wrapped.client)
        }
        MethodFilterCap::Identity => {
            let typed = capnp::capability::FromClientHook::new(base.hook.clone());
            let wrapped: auth_capnp::identity::Client =
                capnp_rpc::new_client(MethodFilteredIdentity {
                    inner: typed,
                    policy: policy.clone(),
                });
            Ok(wrapped.client)
        }
        MethodFilterCap::Ipfs => {
            let typed = capnp::capability::FromClientHook::new(base.hook.clone());
            let wrapped: system_capnp::ipfs::Client = capnp_rpc::new_client(MethodFilteredIpfs {
                inner: typed,
                policy: policy.clone(),
            });
            Ok(wrapped.client)
        }
        MethodFilterCap::HttpClient => {
            let typed = capnp::capability::FromClientHook::new(base.hook.clone());
            let wrapped: http_capnp::http_client::Client =
                capnp_rpc::new_client(MethodFilteredHttpClient {
                    inner: typed,
                    policy: policy.clone(),
                });
            Ok(wrapped.client)
        }
        MethodFilterCap::StreamListener => {
            let typed = capnp::capability::FromClientHook::new(base.hook.clone());
            let wrapped: system_capnp::stream_listener::Client =
                capnp_rpc::new_client(MethodFilteredStreamListener {
                    inner: typed,
                    policy: policy.clone(),
                });
            Ok(wrapped.client)
        }
        MethodFilterCap::StreamDialer => {
            let typed = capnp::capability::FromClientHook::new(base.hook.clone());
            let wrapped: system_capnp::stream_dialer::Client =
                capnp_rpc::new_client(MethodFilteredStreamDialer {
                    inner: typed,
                    policy: policy.clone(),
                });
            Ok(wrapped.client)
        }
        MethodFilterCap::VatListener => {
            let typed = capnp::capability::FromClientHook::new(base.hook.clone());
            let wrapped: system_capnp::vat_listener::Client =
                capnp_rpc::new_client(MethodFilteredVatListener {
                    inner: typed,
                    policy: policy.clone(),
                });
            Ok(wrapped.client)
        }
        MethodFilterCap::VatClient => {
            let typed = capnp::capability::FromClientHook::new(base.hook.clone());
            let wrapped: system_capnp::vat_client::Client =
                capnp_rpc::new_client(MethodFilteredVatClient {
                    inner: typed,
                    policy: policy.clone(),
                });
            Ok(wrapped.client)
        }
        MethodFilterCap::HttpListener => {
            let typed = capnp::capability::FromClientHook::new(base.hook.clone());
            let wrapped: system_capnp::http_listener::Client =
                capnp_rpc::new_client(MethodFilteredHttpListener {
                    inner: typed,
                    policy: policy.clone(),
                });
            Ok(wrapped.client)
        }
        MethodFilterCap::Executor => {
            let typed = capnp::capability::FromClientHook::new(base.hook.clone());
            let wrapped: system_capnp::executor::Client =
                capnp_rpc::new_client(MethodFilteredExecutor {
                    inner: typed,
                    policy: policy.clone(),
                });
            Ok(wrapped.client)
        }
        MethodFilterCap::Process => {
            let typed = capnp::capability::FromClientHook::new(base.hook.clone());
            let wrapped: system_capnp::process::Client =
                capnp_rpc::new_client(MethodFilteredProcess {
                    inner: typed,
                    policy: policy.clone(),
                });
            Ok(wrapped.client)
        }
        MethodFilterCap::Signer => {
            let typed = capnp::capability::FromClientHook::new(base.hook.clone());
            let wrapped: auth_capnp::signer::Client = capnp_rpc::new_client(MethodFilteredSigner {
                inner: typed,
                policy: policy.clone(),
            });
            Ok(wrapped.client)
        }
        MethodFilterCap::ByteStream => {
            let typed = capnp::capability::FromClientHook::new(base.hook.clone());
            let wrapped: system_capnp::byte_stream::Client =
                capnp_rpc::new_client(MethodFilteredByteStream {
                    inner: typed,
                    policy: policy.clone(),
                });
            Ok(wrapped.client)
        }
        MethodFilterCap::DynamicAny => Err(capnp::Error::failed(
            "internal: dynamic AnyPointer wrapper requires schema bytes".into(),
        )),
    }
}

fn maybe_wrap_returned_cap(
    kind: MethodFilterCap,
    method: &str,
    field: &str,
    base: capnp::capability::Client,
    policy: &ExportCapPolicy,
    schema_bytes: Option<&[u8]>,
) -> Result<capnp::capability::Client, capnp::Error> {
    let Some(child_policy) = return_policy(policy, method, field) else {
        return Ok(base);
    };
    if matches!(kind, MethodFilterCap::DynamicAny) {
        let schema_bytes = schema_bytes.ok_or_else(|| {
            capnp::Error::failed(format!(
                "internal: missing schema bytes for dynamic return policy on {method}.{field}"
            ))
        })?;
        if let Some(known_kind) = known_cap_kind_for_schema(schema_bytes) {
            return maybe_wrap_export_cap(known_kind, base, child_policy);
        }
        let dyn_policy = build_dynamic_method_policy(
            "dynamic-cap",
            method,
            field,
            child_policy,
            schema_bytes,
        )?;
        let wrapped: UntypedDynamicClient = capnp_rpc::new_client(MethodFilteredDynamicCap {
            inner: base,
            policy: dyn_policy,
            path: format!("{method}.{field}"),
        });
        return Ok(wrapped.0);
    }
    maybe_wrap_export_cap(kind, base, child_policy)
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
                return capnp::capability::Promise::err(capnp::Error::failed(format!(
                    "{INIT_MEMBRANE_NOT_READY}: kernel bootstrap membrane not ready"
                )))
            }
        };
        let policy = match self.policy.borrow().clone() {
            Some(p) => p,
            None => {
                return capnp::capability::Promise::err(capnp::Error::failed(format!(
                    "{INIT_POLICY_NOT_READY}: kernel export policy not ready"
                )))
            }
        };

        capnp::capability::Promise::from_future(async move {
            let resp = membrane.graft_request().send().promise.await?;
            let src_caps = resp.get()?.get_caps()?;
            let mut export_count = 0u32;
            let mut available_caps = BTreeSet::new();

            for i in 0..src_caps.len() {
                let src = src_caps.get(i);
                let cap_name = src
                    .get_name()?
                    .to_str()
                    .map_err(|e| capnp::Error::failed(e.to_string()))?
                    .to_string();
                available_caps.insert(cap_name.clone());
                if !policy.caps.contains_key(&cap_name) {
                    continue;
                }
                export_count += 1;
            }

            let unknown_caps: Vec<String> = policy
                .caps
                .keys()
                .filter(|name| !available_caps.contains(*name))
                .cloned()
                .collect();
            if !unknown_caps.is_empty() {
                return Err(capnp::Error::failed(format!(
                    "export policy references unknown cap(s): {}",
                    unknown_caps.join(", ")
                )));
            }

            let mut dst_caps = results.get().init_caps(export_count);
            let mut dst_i = 0u32;
            for i in 0..src_caps.len() {
                let src = src_caps.get(i);
                let cap_name = src
                    .get_name()?
                    .to_str()
                    .map_err(|e| capnp::Error::failed(e.to_string()))?
                    .to_string();
                let Some(cap_policy) = policy.caps.get(&cap_name) else {
                    continue;
                };
                let base = src
                    .get_cap()
                    .get_as_capability::<capnp::capability::Client>()?;
                let kind = method_filter_cap(&cap_name).ok_or_else(|| {
                    capnp::Error::failed(format!(
                        "export policy method filter unsupported at runtime for cap '{cap_name}'"
                    ))
                })?;
                let client = maybe_wrap_export_cap(kind, base, cap_policy)?;

                let mut dst = dst_caps.reborrow().get(dst_i);
                dst.set_name(&cap_name);
                dst.reborrow()
                    .init_cap()
                    .set_as_capability(client.hook.clone());
                if src.has_schema() {
                    dst.set_schema(src.get_schema()?)?;
                }
                dst_i += 1;
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
// WASM custom section helpers
// ---------------------------------------------------------------------------

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
    t.insert("list-dir", |a, _| {
        Box::pin(std::future::ready(eval_list_dir(a)))
    });
    t.insert("path-is-dir", |a, _| {
        Box::pin(std::future::ready(eval_path_is_dir(a)))
    });
    t.insert("sort-strings", |a, _| {
        Box::pin(std::future::ready(eval_sort_strings(a)))
    });
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

fn resolve_kernel_builtin_path(path: &str) -> Result<String, String> {
    fn enforce_under_root(
        root: &std::path::Path,
        resolved: &std::path::Path,
        original: &str,
    ) -> Result<(), String> {
        let canonical_root = std::fs::canonicalize(root)
            .map_err(|e| format!("WW_ROOT '{}' is not accessible: {e}", root.display()))?;
        // Require the nearest existing ancestor to stay within WW_ROOT so
        // symlink traversal cannot escape the sandbox.
        let mut probe = resolved;
        while !probe.exists() {
            probe = probe.parent().ok_or_else(|| {
                format!("failed to resolve parent while checking path '{original}'")
            })?;
        }
        let canonical_probe = std::fs::canonicalize(probe)
            .map_err(|e| format!("failed to canonicalize '{original}': {e}"))?;
        if !canonical_probe.starts_with(&canonical_root) {
            return Err(format!(
                "path escapes WW_ROOT via symlink traversal: {original}"
            ));
        }
        Ok(())
    }

    let mut rel = std::path::PathBuf::new();
    for component in std::path::Path::new(path).components() {
        match component {
            std::path::Component::RootDir | std::path::Component::CurDir => {}
            std::path::Component::Normal(part) => rel.push(part),
            std::path::Component::ParentDir => {
                return Err(format!("path escapes root via '..': {path}"));
            }
            std::path::Component::Prefix(_) => {
                return Err(format!("path prefixes are not supported: {path}"));
            }
        }
    }

    if let Ok(root) = std::env::var("WW_ROOT") {
        let root = root.trim_end_matches('/');
        let root_path = std::path::Path::new(root);
        let resolved = if rel.as_os_str().is_empty() {
            root_path.to_path_buf()
        } else {
            root_path.join(rel)
        };
        enforce_under_root(root_path, &resolved, path)?;
        return Ok(resolved.to_string_lossy().to_string());
    }
    if rel.as_os_str().is_empty() {
        Ok("/".to_string())
    } else {
        Ok(format!("/{}", rel.to_string_lossy()))
    }
}

fn eval_list_dir(args: &[Val]) -> Result<Val, Val> {
    let path = match args.first() {
        Some(Val::Str(s)) => s.clone(),
        _ => return Err("(list-dir \"<path>\")".into()),
    };
    let resolved = resolve_kernel_builtin_path(&path)
        .map_err(|e| Val::from(format!("list-dir: {path}: {e}")))?;
    let entries = std::fs::read_dir(&resolved)
        .map_err(|e| Val::from(format!("list-dir: {resolved}: {e}")))?;

    let mut out = Vec::new();
    for entry in entries {
        let entry = match entry {
            Ok(entry) => entry,
            Err(e) => {
                log::warn!("list-dir: skipping unreadable entry in {resolved}: {e}");
                continue;
            }
        };
        let metadata = match std::fs::metadata(entry.path()) {
            Ok(metadata) => metadata,
            Err(e) => {
                log::warn!(
                    "list-dir: skipping entry in {resolved} (metadata error, possibly broken symlink): {e}"
                );
                continue;
            }
        };
        if metadata.is_file() {
            if let Some(name) = entry.file_name().to_str() {
                out.push(Val::Str(name.to_string()));
            } else {
                log::warn!("list-dir: skipping non-utf8 filename in {resolved}");
            }
        } else {
            log::debug!("list-dir: skipping non-file entry in {resolved}");
        }
    }
    Ok(Val::List(out))
}

fn eval_path_is_dir(args: &[Val]) -> Result<Val, Val> {
    let path = match args.first() {
        Some(Val::Str(s)) => s.clone(),
        _ => return Err("(path-is-dir \"<path>\")".into()),
    };
    let resolved = resolve_kernel_builtin_path(&path)
        .map_err(|e| Val::from(format!("path-is-dir: {path}: {e}")))?;
    Ok(Val::Bool(std::path::Path::new(&resolved).is_dir()))
}

fn eval_sort_strings(args: &[Val]) -> Result<Val, Val> {
    let items: &[Val] = match args.first() {
        Some(Val::List(v)) | Some(Val::Vector(v)) => v.as_slice(),
        _ => return Err("(sort-strings <list-or-vector-of-strings>)".into()),
    };

    let mut strings: Vec<String> = Vec::with_capacity(items.len());
    for item in items {
        match item {
            Val::Str(s) => strings.push(s.clone()),
            _ => return Err(Val::from("sort-strings: all elements must be strings")),
        }
    }
    strings.sort();
    Ok(Val::List(strings.into_iter().map(Val::Str).collect()))
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
                        // Cell-based registration:
                        //   (perform host :listen <cell>)          → VatListener
                        //   (perform host :listen <cell> "/path")  → HttpListener
                        if let Some(Val::Cell { wasm, schema, caps }) = rest.first() {
                            let mut load_req = runtime.load_request();
                            load_req.get().set_wasm(wasm);
                            let executor = load_req.send().pipeline.get_executor();

                            let network_resp = host
                                .network_request()
                                .send()
                                .promise
                                .await
                                .map_err(|e| Val::from(e.to_string()))?;
                            let network =
                                network_resp.get().map_err(|e| Val::from(e.to_string()))?;

                            match rest.get(1) {
                                None => {
                                    // VatListener: (perform host :listen <cell>)
                                    let listener = network
                                        .get_vat_listener()
                                        .map_err(|e| Val::from(e.to_string()))?;
                                    let mut req = listener.listen_request();
                                    {
                                        let mut handler = req.get().init_handler();
                                        handler.set_spawn(executor);
                                    }
                                    if let Some(s) = schema {
                                        req.get().set_schema(s);
                                    }
                                    // Forward captured caps from the `with` block.
                                    // Pre-filter to avoid ghost entries in the capnp list.
                                    if !caps.is_empty() {
                                        let valid_caps: Vec<(&str, capnp::capability::Client)> =
                                            caps.iter()
                                                .filter_map(|(name, val)| {
                                                    if let Val::Cap { inner, .. } = val {
                                                        if let Some(client) =
                                                            extract_capnp_client(inner)
                                                        {
                                                            return Some((name.as_str(), client));
                                                        }
                                                        // Not all caps are capnp clients (e.g. ipfs
                                                        // is a VFS placeholder). Skip silently.
                                                        log::debug!(
                                                            "host :listen — cap '{name}' is not a capnp client, skipping"
                                                        );
                                                    }
                                                    None
                                                })
                                                .collect();
                                        if !valid_caps.is_empty() {
                                            let mut caps_builder =
                                                req.get().init_caps(valid_caps.len() as u32);
                                            for (i, (name, client)) in
                                                valid_caps.into_iter().enumerate()
                                            {
                                                let mut entry =
                                                    caps_builder.reborrow().get(i as u32);
                                                entry.set_name(name);
                                                entry.init_cap().set_as_capability(client.hook);
                                            }
                                        }
                                    }
                                    req.send()
                                        .promise
                                        .await
                                        .map_err(|e| Val::from(e.to_string()))?;
                                    log::info!("host :listen — registered vat handler (cell)");
                                    return call_resume(resume, Val::Nil);
                                }
                                Some(Val::Str(prefix)) => {
                                    // HttpListener: (perform host :listen <cell> "/path")
                                    let listener = network
                                        .get_http_listener()
                                        .map_err(|e| Val::from(e.to_string()))?;
                                    let mut req = listener.listen_request();
                                    req.get().set_executor(executor);
                                    req.get().set_prefix(prefix);
                                    // Forward captured caps from the `with` block.
                                    // Mirrors the VatListener path above so WAGI cells
                                    // see only the caps the init.d author granted.
                                    if !caps.is_empty() {
                                        let valid_caps: Vec<(&str, capnp::capability::Client)> =
                                            caps.iter()
                                                .filter_map(|(name, val)| {
                                                    if let Val::Cap { inner, .. } = val {
                                                        if let Some(client) =
                                                            extract_capnp_client(inner)
                                                        {
                                                            return Some((name.as_str(), client));
                                                        }
                                                        log::debug!(
                                                            "host :listen — cap '{name}' is not a capnp client, skipping"
                                                        );
                                                    }
                                                    None
                                                })
                                                .collect();
                                        if !valid_caps.is_empty() {
                                            let mut caps_builder =
                                                req.get().init_caps(valid_caps.len() as u32);
                                            for (i, (name, client)) in
                                                valid_caps.into_iter().enumerate()
                                            {
                                                let mut entry =
                                                    caps_builder.reborrow().get(i as u32);
                                                entry.set_name(name);
                                                entry.init_cap().set_as_capability(client.hook);
                                            }
                                        }
                                    }
                                    req.send()
                                        .promise
                                        .await
                                        .map_err(|e| Val::from(e.to_string()))?;
                                    log::info!(
                                        "host :listen — registered HTTP handler at {prefix} (cell)"
                                    );
                                    return call_resume(resume, Val::Nil);
                                }
                                Some(other) => {
                                    return Err(Val::from(format!(
                                        "host :listen <cell> — optional 2nd arg must be a path string, got {other}"
                                    )));
                                }
                            }
                        }

                        // Legacy path: (perform host :listen runtime <wasm>)         → VatListener
                        //              (perform host :listen runtime "proto" <wasm>) → StreamListener
                        let runtime = match rest.first() {
                            Some(Val::Cap { name, inner, .. }) if name == "runtime" => inner
                                .downcast_ref::<system_capnp::runtime::Client>()
                                .cloned()
                                .ok_or_else(|| {
                                    Val::from("host :listen — runtime cap has wrong inner type")
                                })?,
                            Some(Val::Cap { name, .. }) => {
                                return Err(Val::from(format!(
                                    "host :listen — expected a cell or runtime cap, got cap '{name}'"
                                )))
                            }
                            Some(other) => {
                                return Err(Val::from(format!(
                                    "host :listen — expected a cell (from (cell (load ...) ...)), got {other}"
                                )))
                            }
                            None => {
                                return Err(Val::from(
                                    "host :listen — missing argument. Usage: (perform host :listen <cell>) or (perform host :listen <cell> \"/path\")"
                                ))
                            }
                        };
                        match rest.len() {
                            2 => {
                                // VatListener mode: (perform host :listen runtime <wasm>)
                                // Load wasm via Runtime → Executor, then call
                                // VatListener.listen() with the Executor.
                                let wasm = match rest.get(1) {
                                    Some(Val::Bytes(b)) => b.clone(),
                                    _ => {
                                        return Err(Val::from(
                                            "host :listen — expected wasm bytes as 2nd arg",
                                        ))
                                    }
                                };

                                // Load the wasm to get an Executor (pipelining).
                                let mut load_req = runtime.load_request();
                                load_req.get().set_wasm(&wasm);
                                let executor = load_req
                                    .send()
                                    .pipeline
                                    .get_executor();

                                let network_resp = host
                                    .network_request()
                                    .send()
                                    .promise
                                    .await
                                    .map_err(|e| Val::from(e.to_string()))?;
                                let network = network_resp
                                    .get()
                                    .map_err(|e| Val::from(e.to_string()))?;
                                let listener = network
                                    .get_vat_listener()
                                    .map_err(|e| Val::from(e.to_string()))?;
                                let mut req = listener.listen_request();
                                {
                                    let mut handler = req.get().init_handler();
                                    handler.set_spawn(executor);
                                }
                                req.send()
                                    .promise
                                    .await
                                    .map_err(|e| Val::from(e.to_string()))?;
                                log::info!("host :listen — registered vat handler");
                                Val::Nil
                            }
                            3 => {
                                // StreamListener mode: (perform host :listen runtime "proto" <wasm>)
                                // Load wasm via Runtime → Executor, then call
                                // StreamListener.listen() with Executor + protocol.
                                let protocol = match rest.get(1) {
                                    Some(Val::Str(s)) => s.clone(),
                                    _ => {
                                        return Err(Val::from(
                                            "host :listen — protocol must be a string",
                                        ))
                                    }
                                };
                                let wasm = match rest.get(2) {
                                    Some(Val::Bytes(b)) => b.clone(),
                                    _ => {
                                        return Err(Val::from(
                                            "host :listen — expected wasm bytes",
                                        ))
                                    }
                                };

                                // Load the wasm to get an Executor (pipelining).
                                let mut load_req = runtime.load_request();
                                load_req.get().set_wasm(&wasm);
                                let executor = load_req
                                    .send()
                                    .pipeline
                                    .get_executor();

                                let network_resp = host
                                    .network_request()
                                    .send()
                                    .promise
                                    .await
                                    .map_err(|e| Val::from(e.to_string()))?;
                                let network = network_resp
                                    .get()
                                    .map_err(|e| Val::from(e.to_string()))?;
                                let listener = network
                                    .get_stream_listener()
                                    .map_err(|e| Val::from(e.to_string()))?;
                                let mut req = listener.listen_request();
                                req.get().set_executor(executor);
                                req.get().set_protocol(&protocol);
                                req.send()
                                    .promise
                                    .await
                                    .map_err(|e| Val::from(e.to_string()))?;
                                log::info!(
                                    "host :listen — registered stream handler /ww/0.1.0/stream/{protocol}"
                                );
                                Val::Nil
                            }
                            _ => {
                                return Err(Val::from(
                                    "host :listen — usage: (perform host :listen runtime <wasm>) or (perform host :listen runtime \"proto\" <wasm>)",
                                ))
                            }
                        }
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
                            None => {
                                return Err(Val::from(
                                    "http-client not available (node started without --http-dial)",
                                ))
                            }
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
                        let spawn_args: Vec<String> = Vec::new();
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
  (perform host :listen runtime <wasm>)      Register RPC handler
  (perform host :listen runtime \"p\" <wasm>) Register stream handler

  (perform runtime :run <wasm> :env {})      Spawn foreground process

  (perform routing :provide \"<name>\")        Announce to DHT (hashes internally)
  (perform routing :find \"<name>\" :count N)  Discover providers (default 20)
  (perform routing :hash \"<data>\")           Hash data to CID
  (perform routing :resolve \"<ipns-name>\")   Resolve IPNS name to /ipfs/ path

Effects:
  (perform :load \"<path>\")                   Load bytes from virtual filesystem
  (perform :list-dir \"<path>\")               List directory entries
  (perform :path-is-dir \"<path>\")            True if path exists and is a directory
  (perform :sort-strings <list>)               Sort strings lexicographically

Built-ins:
  (load \"<path>\")                Load bytes (dispatch form)
  (list-dir \"<path>\")            List directory entries
  (path-is-dir \"<path>\")         True if path exists and is a directory
  (sort-strings <list>)            Sort strings lexicographically
  (cd \"<path>\")                  Change working directory
  (help)                         This message
  (exit)                         Quit

Unrecognized commands are looked up in PATH (default /bin).";

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

    let with_list_dir = Val::List(vec![
        Val::Sym("with-effect-handler".into()),
        Val::Keyword("list-dir".into()),
        Val::List(vec![
            Val::Sym("fn".into()),
            Val::Vector(vec![Val::Sym("path".into()), Val::Sym("resume".into())]),
            Val::List(vec![
                Val::Sym("resume".into()),
                Val::List(vec![Val::Sym("list-dir".into()), Val::Sym("path".into())]),
            ]),
        ]),
        with_load,
    ]);

    let with_path_is_dir = Val::List(vec![
        Val::Sym("with-effect-handler".into()),
        Val::Keyword("path-is-dir".into()),
        Val::List(vec![
            Val::Sym("fn".into()),
            Val::Vector(vec![Val::Sym("path".into()), Val::Sym("resume".into())]),
            Val::List(vec![
                Val::Sym("resume".into()),
                Val::List(vec![
                    Val::Sym("path-is-dir".into()),
                    Val::Sym("path".into()),
                ]),
            ]),
        ]),
        with_list_dir,
    ]);

    let with_sort_strings = Val::List(vec![
        Val::Sym("with-effect-handler".into()),
        Val::Keyword("sort-strings".into()),
        Val::List(vec![
            Val::Sym("fn".into()),
            Val::Vector(vec![Val::Sym("items".into()), Val::Sym("resume".into())]),
            Val::List(vec![
                Val::Sym("resume".into()),
                Val::List(vec![
                    Val::Sym("sort-strings".into()),
                    Val::Sym("items".into()),
                ]),
            ]),
        ]),
        with_path_is_dir,
    ]);

    // Wrap in cap handlers (innermost to outermost).
    let caps = ["import", "routing", "runtime", "host"];
    let mut wrapped = with_sort_strings;
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

fn resolve_boot_file_path(rel_path: &str) -> Option<String> {
    if let Ok(ww_root) = std::env::var("WW_ROOT") {
        let root = ww_root.trim_end_matches('/');
        let rooted = format!("{root}/{rel_path}");
        // Fail-closed with WW_ROOT: do not silently fall back to host /etc
        // when the rooted file is missing.
        return std::path::Path::new(&rooted).exists().then_some(rooted);
    }

    let host = format!("/{rel_path}");
    std::path::Path::new(&host).exists().then_some(host)
}

async fn run_init_glia(
    env: &mut Env,
    ctx: &RefCell<Session>,
    dispatch: &HashMap<&'static str, HandlerFn>,
) -> Result<ExportPolicy, capnp::Error> {
    let init_path =
        resolve_boot_file_path("etc/init.glia").ok_or_else(|| match std::env::var("WW_ROOT") {
            Ok(root) => {
                let root = root.trim_end_matches('/');
                capnp::Error::failed(format!(
                    "boot failed: missing required {root}/etc/init.glia"
                ))
            }
            Err(_) => capnp::Error::failed("boot failed: missing required /etc/init.glia".into()),
        })?;
    log::info!("boot: evaluating init script at {init_path}");

    let data = std::fs::read(&init_path)
        .map_err(|e| capnp::Error::failed(format!("boot failed: read {init_path}: {e}")))?;
    let content = std::str::from_utf8(&data).map_err(|e| {
        capnp::Error::failed(format!("boot failed: init.glia is not valid UTF-8: {e}"))
    })?;
    let forms = read_many(content)
        .map_err(|e| capnp::Error::failed(format!("boot failed: init.glia parse error: {e}")))?;
    if forms.is_empty() {
        return Err(capnp::Error::failed(
            "boot failed: init.glia must return an export policy map".into(),
        ));
    }

    let mut result = Val::Nil;
    for (i, form) in forms.iter().enumerate() {
        let wrapped = wrap_with_handlers(form);
        result = eval(&wrapped, env, ctx, dispatch).await.map_err(|e| {
            capnp::Error::failed(format!("boot failed: init.glia form {}: {e}", i + 1))
        })?;
    }

    parse_export_policy(&result)
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
            return entry.get_cap().get_as_capability();
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
    fn count_recursive_return_edges(policy: &AttenuationPolicy) -> usize {
        policy
            .returns
            .values()
            .map(|fields| {
                fields.len()
                    + fields
                        .values()
                        .map(count_recursive_return_edges)
                        .sum::<usize>()
            })
            .sum()
    }

    fn max_recursive_return_depth(policy: &AttenuationPolicy) -> usize {
        let Some(max_child) = policy
            .returns
            .values()
            .flat_map(|fields| fields.values())
            .map(max_recursive_return_depth)
            .max()
        else {
            return 0;
        };
        1 + max_child
    }

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
                "http" | "http-client" => {
                    "http-client — outbound HTTP requests (gated by --http-dial)"
                }
                _ => {
                    if let Some(glia_cap) = inner.downcast_ref::<GliaCapInner>() {
                        return Ok(Val::Str(format!(
                            "glia capability — local method table\n  cap-name:   {cap_name}\n  schema-cid: {schema_cid}\n  methods:    {}",
                            glia_cap.methods.len()
                        )));
                    }
                    if let Some(att) = inner.downcast_ref::<AttenuatedCapInner>() {
                        let return_edges = count_recursive_return_edges(&att.policy);
                        let return_depth = max_recursive_return_depth(&att.policy);
                        return Ok(Val::Str(format!(
                            "attenuated capability — method whitelist\n  cap-name:      {cap_name}\n  schema-cid:    {schema_cid}\n  methods:       {}\n  return-edges:  {return_edges}\n  return-depth:  {return_depth}",
                            att.policy.allow_methods.len(),
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
    let exported_policy: Rc<RefCell<Option<ExportPolicy>>> = Rc::new(RefCell::new(None));
    let bootstrap: membrane_capnp::membrane::Client = capnp_rpc::new_client(KernelBootstrap {
        membrane: Rc::clone(&exported_membrane),
        policy: Rc::clone(&exported_policy),
    });

    system::serve(bootstrap.client, move |membrane: Membrane| {
        let exported_membrane = Rc::clone(&exported_membrane);
        let exported_policy = Rc::clone(&exported_policy);
        async move {
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

            // Boot policy gate: init.glia must succeed and return a valid policy map.
            // Until this is set, KernelBootstrap::graft is fail-closed.
            let policy = match run_init_glia(&mut env, &ctx, &dispatch).await {
                Ok(p) => p,
                Err(e) => {
                    log::error!("{e}");
                    std::process::exit(1);
                }
            };
            *exported_policy.borrow_mut() = Some(policy);
            *exported_membrane.borrow_mut() = Some(membrane.clone());

            let is_tty = std::env::var("WW_TTY").is_ok();
            let result = if is_tty {
                run_shell(&mut env, ctx, &dispatch).await
            } else {
                run_daemon().await
            };

            if let Err(e) = result {
                log::error!("kernel error: {e}");
            }

            Ok(())
        }
    });
}

wasip2::cli::command::export!(Kernel);

#[cfg(test)]
mod tests {
    use super::*;
    static WW_ROOT_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

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

    #[test]
    fn eval_path_is_dir_reports_presence() {
        let dir = tempfile::tempdir().unwrap();
        let dir_path = dir.path().to_str().unwrap().to_string();
        let missing = format!("{dir_path}/does-not-exist");
        assert_eq!(
            eval_path_is_dir(&[Val::Str(dir_path)]).unwrap(),
            Val::Bool(true)
        );
        assert_eq!(
            eval_path_is_dir(&[Val::Str(missing)]).unwrap(),
            Val::Bool(false)
        );
    }

    #[test]
    fn eval_path_is_dir_rejects_parent_traversal_under_ww_root() {
        let _guard = WW_ROOT_TEST_LOCK.lock().unwrap();
        let ww_root = tempfile::tempdir().unwrap();
        std::env::set_var("WW_ROOT", ww_root.path());
        let result = eval_path_is_dir(&[Val::Str("/../../etc".into())]);
        std::env::remove_var("WW_ROOT");

        let err = result.expect_err("expected traversal to be rejected");
        let msg = format!("{err}");
        assert!(msg.contains("path escapes root"), "unexpected error: {msg}");
    }

    #[test]
    fn eval_list_dir_rejects_parent_traversal_under_ww_root() {
        let _guard = WW_ROOT_TEST_LOCK.lock().unwrap();
        let ww_root = tempfile::tempdir().unwrap();
        std::env::set_var("WW_ROOT", ww_root.path());
        let result = eval_list_dir(&[Val::Str("/../../etc".into())]);
        std::env::remove_var("WW_ROOT");

        let err = result.expect_err("expected traversal to be rejected");
        let msg = format!("{err}");
        assert!(msg.contains("path escapes root"), "unexpected error: {msg}");
    }

    #[test]
    #[cfg(unix)]
    fn eval_path_is_dir_rejects_symlink_escape_under_ww_root() {
        use std::os::unix::fs::symlink;

        let _guard = WW_ROOT_TEST_LOCK.lock().unwrap();
        let ww_root = tempfile::tempdir().unwrap();
        let link = ww_root.path().join("escape");
        symlink("/etc", &link).unwrap();

        std::env::set_var("WW_ROOT", ww_root.path());
        let result = eval_path_is_dir(&[Val::Str("/escape".into())]);
        std::env::remove_var("WW_ROOT");

        let err = result.expect_err("expected symlink escape to be rejected");
        let msg = format!("{err}");
        assert!(
            msg.contains("symlink traversal"),
            "unexpected error: {msg}"
        );
    }

    #[test]
    #[cfg(unix)]
    fn eval_list_dir_rejects_symlink_escape_under_ww_root() {
        use std::os::unix::fs::symlink;

        let _guard = WW_ROOT_TEST_LOCK.lock().unwrap();
        let ww_root = tempfile::tempdir().unwrap();
        let link = ww_root.path().join("escape");
        symlink("/etc", &link).unwrap();

        std::env::set_var("WW_ROOT", ww_root.path());
        let result = eval_list_dir(&[Val::Str("/escape".into())]);
        std::env::remove_var("WW_ROOT");

        let err = result.expect_err("expected symlink escape to be rejected");
        let msg = format!("{err}");
        assert!(
            msg.contains("symlink traversal"),
            "unexpected error: {msg}"
        );
    }

    #[test]
    #[cfg(unix)]
    fn eval_list_dir_includes_symlink_to_file() {
        use std::os::unix::fs::symlink;

        let _guard = WW_ROOT_TEST_LOCK.lock().unwrap();
        std::env::remove_var("WW_ROOT");
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("target.glia");
        let link = dir.path().join("link.glia");
        std::fs::write(&target, b"(+ 1 2)").unwrap();
        symlink(&target, &link).unwrap();

        let listed = eval_list_dir(&[Val::Str(dir.path().to_str().unwrap().to_string())]).unwrap();
        let names: Vec<String> = match listed {
            Val::List(items) => items
                .into_iter()
                .filter_map(|v| match v {
                    Val::Str(s) => Some(s),
                    _ => None,
                })
                .collect(),
            other => panic!("expected list of names, got {other}"),
        };
        assert!(
            names.iter().any(|s| s == "link.glia"),
            "expected symlinked file entry in list-dir output: {names:?}"
        );
    }

    #[test]
    #[cfg(unix)]
    fn eval_list_dir_skips_broken_symlink_instead_of_failing() {
        use std::os::unix::fs::symlink;

        let dir = tempfile::tempdir().unwrap();
        let good = dir.path().join("good.glia");
        let broken = dir.path().join("broken.glia");
        std::fs::write(&good, b"(+ 1 2)").unwrap();
        symlink(dir.path().join("missing-target.glia"), &broken).unwrap();

        let listed = eval_list_dir(&[Val::Str(dir.path().to_str().unwrap().to_string())]).unwrap();
        let names: Vec<String> = match listed {
            Val::List(items) => items
                .into_iter()
                .filter_map(|v| match v {
                    Val::Str(s) => Some(s),
                    _ => None,
                })
                .collect(),
            other => panic!("expected list of names, got {other}"),
        };
        assert!(
            names.iter().any(|s| s == "good.glia"),
            "got names: {names:?}"
        );
        assert!(
            !names.iter().any(|s| s == "broken.glia"),
            "broken symlink should be skipped: {names:?}"
        );
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
        let expected = [
            "load",
            "list-dir",
            "path-is-dir",
            "sort-strings",
            "cd",
            "help",
            "exit",
        ];
        for verb in &expected {
            assert!(table.contains_key(verb), "missing dispatch entry: {verb}");
        }
        assert_eq!(
            table.len(),
            expected.len(),
            "unexpected extra entries in dispatch table"
        );
    }

    #[test]
    fn resolve_boot_file_path_uses_ww_root_without_host_fallback() {
        let _guard = WW_ROOT_TEST_LOCK.lock().unwrap();
        let ww_root = tempfile::tempdir().unwrap();
        let host_root = tempfile::tempdir().unwrap();
        let host_rel = format!(
            "tmp/{}/init.glia",
            host_root.path().file_name().unwrap().to_string_lossy()
        );
        let host_path = std::path::Path::new("/").join(&host_rel);
        std::fs::create_dir_all(host_path.parent().unwrap()).unwrap();
        std::fs::write(&host_path, b"; host file").unwrap();

        std::env::set_var("WW_ROOT", ww_root.path());
        let resolved = resolve_boot_file_path(&host_rel);
        std::env::remove_var("WW_ROOT");

        assert!(
            resolved.is_none(),
            "WW_ROOT must not silently fall back to host path, got: {resolved:?}"
        );
    }

    #[test]
    fn resolve_boot_file_path_finds_file_under_ww_root() {
        let _guard = WW_ROOT_TEST_LOCK.lock().unwrap();
        let ww_root = tempfile::tempdir().unwrap();
        let rooted = ww_root.path().join("etc/init.glia");
        std::fs::create_dir_all(rooted.parent().unwrap()).unwrap();
        std::fs::write(&rooted, b"; rooted init").unwrap();

        std::env::set_var("WW_ROOT", ww_root.path());
        let resolved = resolve_boot_file_path("etc/init.glia");
        std::env::remove_var("WW_ROOT");

        assert_eq!(
            resolved,
            Some(rooted.to_string_lossy().to_string()),
            "expected WW_ROOT-scoped init path"
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
            r.set_http_listener(capnp_rpc::new_client(TestHttpListener));
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

    // --- Stub VatListener: asserts handler is present ---

    struct TestVatListener;

    #[allow(refining_impl_trait)]
    impl system_capnp::vat_listener::Server for TestVatListener {
        fn listen(
            self: capnp::capability::Rc<Self>,
            params: system_capnp::vat_listener::ListenParams,
            _results: system_capnp::vat_listener::ListenResults,
        ) -> Promise<(), capnp::Error> {
            let params = capnp_rpc::pry!(params.get());
            if !params.has_handler() {
                return Promise::err(capnp::Error::failed("handler not set".into()));
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

    // --- Stub StreamDialer + VatClient ---

    struct TestStreamDialer;
    impl system_capnp::stream_dialer::Server for TestStreamDialer {}

    struct TestVatClient;
    #[allow(refining_impl_trait)]
    impl system_capnp::vat_client::Server for TestVatClient {
        fn dial(
            self: capnp::capability::Rc<Self>,
            params: system_capnp::vat_client::DialParams,
            mut results: system_capnp::vat_client::DialResults,
        ) -> Promise<(), capnp::Error> {
            let p = capnp_rpc::pry!(params.get());
            let schema = capnp_rpc::pry!(p.get_schema());
            if schema.is_empty() {
                return Promise::err(capnp::Error::failed("schema is required".into()));
            }
            let host: system_capnp::host::Client = capnp_rpc::new_client(TestHost);
            let aligned = bytes_to_aligned_words(schema);
            let segments: &[&[u8]] = &[capnp::Word::words_to_bytes(&aligned)];
            let segment_array = capnp::message::SegmentArray::new(segments);
            let reader =
                capnp::message::Reader::new(segment_array, capnp::message::ReaderOptions::new());
            let schema_node: capnp::schema_capnp::node::Reader<'_> = capnp_rpc::pry!(reader.get_root());
            let mut typed = results.get().init_typed();
            typed
                .reborrow()
                .init_cap()
                .set_as_capability(host.client.hook.clone());
            let mut out_schema = typed.reborrow().init_schema();
            capnp_rpc::pry!(out_schema.set_root(schema_node));
            out_schema.init_deps(0);
            Promise::ok(())
        }
    }

    struct TestHttpListener;
    impl system_capnp::http_listener::Server for TestHttpListener {}

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
            entry
                .reborrow()
                .init_cap()
                .set_as_capability(self.runtime.client.hook.clone());
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
            let policy = Rc::new(RefCell::new(Some(ExportPolicy {
                caps: [("runtime".to_string(), ExportCapPolicy::default())]
                    .into_iter()
                    .collect(),
            })));

            let bootstrap: Membrane = capnp_rpc::new_client(KernelBootstrap {
                membrane: state,
                policy,
            });
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
    async fn test_kernel_bootstrap_errors_when_policy_references_unknown_cap() {
        run_local(async {
            let runtime: system_capnp::runtime::Client = capnp_rpc::new_client(TestRuntime);
            let upstream: Membrane = capnp_rpc::new_client(TestMembrane {
                runtime: runtime.clone(),
            });
            let state: Rc<RefCell<Option<Membrane>>> = Rc::new(RefCell::new(Some(upstream)));
            let policy = Rc::new(RefCell::new(Some(ExportPolicy {
                caps: [
                    ("runtime".to_string(), ExportCapPolicy::default()),
                    ("rutnime".to_string(), ExportCapPolicy::default()),
                ]
                .into_iter()
                .collect(),
            })));

            let bootstrap: Membrane = capnp_rpc::new_client(KernelBootstrap {
                membrane: state,
                policy,
            });

            match bootstrap.graft_request().send().promise.await {
                Ok(_) => panic!("bootstrap graft should fail for unknown policy cap"),
                Err(err) => {
                    let msg = format!("{err}");
                    assert!(
                        msg.contains("unknown cap"),
                        "expected unknown cap error, got: {msg}"
                    );
                    assert!(
                        msg.contains("rutnime"),
                        "expected offending cap name in error, got: {msg}"
                    );
                }
            }
        })
        .await;
    }

    #[tokio::test]
    async fn test_kernel_bootstrap_errors_when_membrane_not_ready() {
        run_local(async {
            let state: Rc<RefCell<Option<Membrane>>> = Rc::new(RefCell::new(None));
            let policy = Rc::new(RefCell::new(Some(ExportPolicy {
                caps: [("runtime".to_string(), ExportCapPolicy::default())]
                    .into_iter()
                    .collect(),
            })));
            let bootstrap: Membrane = capnp_rpc::new_client(KernelBootstrap {
                membrane: state,
                policy,
            });

            match bootstrap.graft_request().send().promise.await {
                Ok(_) => panic!("bootstrap graft should fail before membrane is ready"),
                Err(err) => {
                    assert!(
                        format!("{err}").contains(INIT_MEMBRANE_NOT_READY),
                        "unexpected error: {err}"
                    );
                }
            }
        })
        .await;
    }

    #[tokio::test]
    async fn test_kernel_bootstrap_errors_when_policy_not_ready() {
        run_local(async {
            let runtime: system_capnp::runtime::Client = capnp_rpc::new_client(TestRuntime);
            let upstream: Membrane = capnp_rpc::new_client(TestMembrane { runtime });
            let state: Rc<RefCell<Option<Membrane>>> = Rc::new(RefCell::new(Some(upstream)));
            let policy = Rc::new(RefCell::new(None));
            let bootstrap: Membrane = capnp_rpc::new_client(KernelBootstrap {
                membrane: state,
                policy,
            });

            match bootstrap.graft_request().send().promise.await {
                Ok(_) => panic!("bootstrap graft should fail before policy is ready"),
                Err(err) => {
                    assert!(
                        format!("{err}").contains(INIT_POLICY_NOT_READY),
                        "unexpected error: {err}"
                    );
                }
            }
        })
        .await;
    }

    #[tokio::test]
    async fn test_kernel_bootstrap_enforces_recursive_policy_on_host_network_stream_listener() {
        run_local(async {
            struct HostOnlyMembrane {
                host: system_capnp::host::Client,
            }
            #[allow(refining_impl_trait)]
            impl membrane_capnp::membrane::Server for HostOnlyMembrane {
                fn graft(
                    self: capnp::capability::Rc<Self>,
                    _params: membrane_capnp::membrane::GraftParams,
                    mut results: membrane_capnp::membrane::GraftResults,
                ) -> Promise<(), capnp::Error> {
                    let mut caps = results.get().init_caps(1);
                    let mut entry = caps.reborrow().get(0);
                    entry.set_name("host");
                    entry
                        .reborrow()
                        .init_cap()
                        .set_as_capability(self.host.client.hook.clone());
                    Promise::ok(())
                }
            }

            let upstream: Membrane = capnp_rpc::new_client(HostOnlyMembrane {
                host: capnp_rpc::new_client(TestHost),
            });
            let state: Rc<RefCell<Option<Membrane>>> = Rc::new(RefCell::new(Some(upstream)));

            let stream_listener_policy = ExportCapPolicy {
                allow_methods: Some(BTreeSet::new()),
                returns: BTreeMap::new(),
            };
            let host_policy = ExportCapPolicy {
                allow_methods: Some(["network".to_string()].into_iter().collect()),
                returns: BTreeMap::from([(
                    "network".to_string(),
                    BTreeMap::from([("streamListener".to_string(), stream_listener_policy)]),
                )]),
            };
            let policy = Rc::new(RefCell::new(Some(ExportPolicy {
                caps: BTreeMap::from([("host".to_string(), host_policy)]),
            })));

            let bootstrap: Membrane = capnp_rpc::new_client(KernelBootstrap {
                membrane: state,
                policy,
            });
            let resp = bootstrap
                .graft_request()
                .send()
                .promise
                .await
                .expect("bootstrap graft should succeed");
            let caps = resp.get().unwrap().get_caps().unwrap();
            let forwarded_host: system_capnp::host::Client =
                get_graft_cap(&caps, "host").expect("host cap should be forwarded");
            let network_resp = forwarded_host
                .network_request()
                .send()
                .promise
                .await
                .expect("host.network should be allowed");
            let stream_listener = network_resp
                .get()
                .unwrap()
                .get_stream_listener()
                .expect("stream listener should be present");
            match stream_listener.listen_request().send().promise.await {
                Ok(_) => panic!("stream-listener.listen should be denied by recursive policy"),
                Err(err) => {
                    let msg = format!("{err}");
                    assert!(
                        msg.contains("permission denied"),
                        "expected permission denied, got: {msg}"
                    );
                }
            }
        })
        .await;
    }

    #[tokio::test]
    async fn test_kernel_bootstrap_enforces_recursive_policy_on_vat_client_dial_cap() {
        run_local(async {
            struct HostOnlyMembrane {
                host: system_capnp::host::Client,
            }
            #[allow(refining_impl_trait)]
            impl membrane_capnp::membrane::Server for HostOnlyMembrane {
                fn graft(
                    self: capnp::capability::Rc<Self>,
                    _params: membrane_capnp::membrane::GraftParams,
                    mut results: membrane_capnp::membrane::GraftResults,
                ) -> Promise<(), capnp::Error> {
                    let mut caps = results.get().init_caps(1);
                    let mut entry = caps.reborrow().get(0);
                    entry.set_name("host");
                    entry
                        .reborrow()
                        .init_cap()
                        .set_as_capability(self.host.client.hook.clone());
                    Promise::ok(())
                }
            }

            let upstream: Membrane = capnp_rpc::new_client(HostOnlyMembrane {
                host: capnp_rpc::new_client(TestHost),
            });
            let state: Rc<RefCell<Option<Membrane>>> = Rc::new(RefCell::new(Some(upstream)));

            let cap_policy = ExportCapPolicy {
                allow_methods: Some(["id".to_string()].into_iter().collect()),
                returns: BTreeMap::new(),
            };
            let vat_client_policy = ExportCapPolicy {
                allow_methods: Some(["dial".to_string()].into_iter().collect()),
                returns: BTreeMap::from([("dial".to_string(), BTreeMap::from([("cap".to_string(), cap_policy)]))]),
            };
            let host_policy = ExportCapPolicy {
                allow_methods: Some(["network".to_string()].into_iter().collect()),
                returns: BTreeMap::from([(
                    "network".to_string(),
                    BTreeMap::from([("vatClient".to_string(), vat_client_policy)]),
                )]),
            };
            let policy = Rc::new(RefCell::new(Some(ExportPolicy {
                caps: BTreeMap::from([("host".to_string(), host_policy)]),
            })));

            let bootstrap: Membrane = capnp_rpc::new_client(KernelBootstrap {
                membrane: state,
                policy,
            });
            let resp = bootstrap
                .graft_request()
                .send()
                .promise
                .await
                .expect("bootstrap graft should succeed");
            let caps = resp.get().unwrap().get_caps().unwrap();
            let forwarded_host: system_capnp::host::Client =
                get_graft_cap(&caps, "host").expect("host cap should be forwarded");

            let network_resp = forwarded_host
                .network_request()
                .send()
                .promise
                .await
                .expect("host.network should be allowed");
            let vat_client = network_resp
                .get()
                .unwrap()
                .get_vat_client()
                .expect("vat client should be present");

            let mut dial_req = vat_client.dial_request();
            dial_req.get().set_peer(STUB_PEER_ID);
            dial_req.get().set_schema(schema_ids::HOST_SCHEMA);
            let dial_resp = dial_req.send().promise.await.expect("dial should be allowed");
            let typed_host: system_capnp::host::Client = dial_resp
                .get()
                .unwrap()
                .get_typed()
                .unwrap()
                .get_cap()
                .get_as_capability()
                .expect("returned cap should cast to host");

            let id_resp = typed_host.id_request().send().promise.await;
            assert!(id_resp.is_ok(), "id should be allowed");
            match typed_host.addrs_request().send().promise.await {
                Ok(_) => panic!("host.addrs should be denied by recursive dial.cap policy"),
                Err(err) => {
                    let msg = format!("{err}");
                    assert!(
                        msg.contains("permission denied"),
                        "expected permission denied, got: {msg}"
                    );
                }
            }
        })
        .await;
    }

    #[tokio::test]
    async fn test_kernel_bootstrap_enforces_recursive_policy_on_process_bootstrap_cap() {
        run_local(async {
            struct TestProcessReturnsHost;
            #[allow(refining_impl_trait)]
            impl system_capnp::process::Server for TestProcessReturnsHost {
                fn bootstrap(
                    self: capnp::capability::Rc<Self>,
                    params: system_capnp::process::BootstrapParams,
                    mut results: system_capnp::process::BootstrapResults,
                ) -> Promise<(), capnp::Error> {
                    let p = capnp_rpc::pry!(params.get());
                    let schema = capnp_rpc::pry!(p.get_schema());
                    if schema.is_empty() {
                        return Promise::err(capnp::Error::failed("schema required".into()));
                    }
                    let host: system_capnp::host::Client = capnp_rpc::new_client(TestHost);
                    let aligned = bytes_to_aligned_words(schema);
                    let segments: &[&[u8]] = &[capnp::Word::words_to_bytes(&aligned)];
                    let segment_array = capnp::message::SegmentArray::new(segments);
                    let reader =
                        capnp::message::Reader::new(segment_array, capnp::message::ReaderOptions::new());
                    let schema_node: capnp::schema_capnp::node::Reader<'_> =
                        capnp_rpc::pry!(reader.get_root());
                    let mut typed = results.get().init_typed();
                    typed
                        .reborrow()
                        .init_cap()
                        .set_as_capability(host.client.hook.clone());
                    let mut out_schema = typed.reborrow().init_schema();
                    capnp_rpc::pry!(out_schema.set_root(schema_node));
                    out_schema.init_deps(0);
                    Promise::ok(())
                }
            }

            struct TestExecutorReturnsProcess;
            #[allow(refining_impl_trait)]
            impl system_capnp::executor::Server for TestExecutorReturnsProcess {
                fn spawn(
                    self: capnp::capability::Rc<Self>,
                    _params: system_capnp::executor::SpawnParams,
                    mut results: system_capnp::executor::SpawnResults,
                ) -> Promise<(), capnp::Error> {
                    results
                        .get()
                        .set_process(capnp_rpc::new_client(TestProcessReturnsHost));
                    Promise::ok(())
                }
            }

            struct TestRuntimeReturnsExecutor;
            #[allow(refining_impl_trait)]
            impl system_capnp::runtime::Server for TestRuntimeReturnsExecutor {
                fn load(
                    self: capnp::capability::Rc<Self>,
                    _params: system_capnp::runtime::LoadParams,
                    mut results: system_capnp::runtime::LoadResults,
                ) -> Promise<(), capnp::Error> {
                    results
                        .get()
                        .set_executor(capnp_rpc::new_client(TestExecutorReturnsProcess));
                    Promise::ok(())
                }
            }

            struct RuntimeOnlyMembrane {
                runtime: system_capnp::runtime::Client,
            }
            #[allow(refining_impl_trait)]
            impl membrane_capnp::membrane::Server for RuntimeOnlyMembrane {
                fn graft(
                    self: capnp::capability::Rc<Self>,
                    _params: membrane_capnp::membrane::GraftParams,
                    mut results: membrane_capnp::membrane::GraftResults,
                ) -> Promise<(), capnp::Error> {
                    let mut caps = results.get().init_caps(1);
                    let mut entry = caps.reborrow().get(0);
                    entry.set_name("runtime");
                    entry
                        .reborrow()
                        .init_cap()
                        .set_as_capability(self.runtime.client.hook.clone());
                    Promise::ok(())
                }
            }

            let upstream: Membrane = capnp_rpc::new_client(RuntimeOnlyMembrane {
                runtime: capnp_rpc::new_client(TestRuntimeReturnsExecutor),
            });
            let state: Rc<RefCell<Option<Membrane>>> = Rc::new(RefCell::new(Some(upstream)));

            let cap_policy = ExportCapPolicy {
                allow_methods: Some(["id".to_string()].into_iter().collect()),
                returns: BTreeMap::new(),
            };
            let process_policy = ExportCapPolicy {
                allow_methods: Some(["bootstrap".to_string()].into_iter().collect()),
                returns: BTreeMap::from([(
                    "bootstrap".to_string(),
                    BTreeMap::from([("cap".to_string(), cap_policy)]),
                )]),
            };
            let executor_policy = ExportCapPolicy {
                allow_methods: Some(["spawn".to_string()].into_iter().collect()),
                returns: BTreeMap::from([(
                    "spawn".to_string(),
                    BTreeMap::from([("process".to_string(), process_policy)]),
                )]),
            };
            let runtime_policy = ExportCapPolicy {
                allow_methods: Some(["load".to_string()].into_iter().collect()),
                returns: BTreeMap::from([(
                    "load".to_string(),
                    BTreeMap::from([("executor".to_string(), executor_policy)]),
                )]),
            };
            let policy = Rc::new(RefCell::new(Some(ExportPolicy {
                caps: BTreeMap::from([("runtime".to_string(), runtime_policy)]),
            })));

            let bootstrap: Membrane = capnp_rpc::new_client(KernelBootstrap {
                membrane: state,
                policy,
            });
            let resp = bootstrap
                .graft_request()
                .send()
                .promise
                .await
                .expect("bootstrap graft should succeed");
            let caps = resp.get().unwrap().get_caps().unwrap();
            let runtime: system_capnp::runtime::Client =
                get_graft_cap(&caps, "runtime").expect("runtime cap should be forwarded");

            let mut load_req = runtime.load_request();
            load_req.get().set_wasm(b"00");
            let load_resp = load_req.send().promise.await.expect("load should be allowed");
            let executor = load_resp.get().unwrap().get_executor().unwrap();

            let spawn_resp = executor
                .spawn_request()
                .send()
                .promise
                .await
                .expect("spawn should be allowed");
            let process = spawn_resp.get().unwrap().get_process().unwrap();

            let mut boot_req = process.bootstrap_request();
            boot_req.get().set_schema(schema_ids::HOST_SCHEMA);
            let boot_resp = boot_req
                .send()
                .promise
                .await
                .expect("process.bootstrap should be allowed");
            let typed_host: system_capnp::host::Client = boot_resp
                .get()
                .unwrap()
                .get_typed()
                .unwrap()
                .get_cap()
                .get_as_capability()
                .expect("returned cap should cast to host");

            let id_resp = typed_host.id_request().send().promise.await;
            assert!(id_resp.is_ok(), "id should be allowed");
            match typed_host.peers_request().send().promise.await {
                Ok(_) => panic!("host.peers should be denied by recursive bootstrap.cap policy"),
                Err(err) => {
                    let msg = format!("{err}");
                    assert!(
                        msg.contains("permission denied"),
                        "expected permission denied, got: {msg}"
                    );
                }
            }
        })
        .await;
    }

    #[test]
    fn test_parse_export_policy_rejects_non_map() {
        let err = parse_export_policy(&Val::Int(1)).unwrap_err();
        assert!(format!("{err}").contains("must return a map"));
    }

    #[test]
    fn test_parse_export_policy_rejects_legacy_export_shape() {
        let policy = read("{:export {:caps [\"runtime\"] :methods {}}}").unwrap();
        let err = parse_export_policy(&policy).unwrap_err();
        assert!(format!("{err}").contains("legacy"));
    }

    #[test]
    fn test_parse_export_policy_accepts_bare_map() {
        let policy = Val::Map(glia::ValMap::from_pairs(vec![
            (
                Val::Keyword("runtime".into()),
                test_cap("runtime", "runtime-cid"),
            ),
            (Val::Keyword("host".into()), test_cap("host", "host-cid")),
        ]));
        let parsed = parse_export_policy(&policy).unwrap();
        assert!(parsed.caps.contains_key("runtime"));
        assert!(parsed.caps.contains_key("host"));
    }

    #[test]
    fn test_parse_export_policy_allows_empty_caps() {
        let policy = read("{}").unwrap();
        let parsed = parse_export_policy(&policy).unwrap();
        assert!(parsed.caps.is_empty());
    }

    #[test]
    fn test_parse_export_policy_rejects_unknown_export_cap() {
        let policy = Val::Map(glia::ValMap::from_pairs(vec![(
            Val::Keyword("custom".into()),
            test_cap("custom", "custom-cid"),
        )]));
        let err = parse_export_policy(&policy).unwrap_err();
        assert!(format!("{err}").contains("unknown cap"));
    }

    #[test]
    fn test_parse_export_policy_rejects_duplicate_cap_keys_after_canonicalization() {
        let policy = Val::Map(glia::ValMap::from_pairs(vec![
            (Val::Keyword("host".into()), test_cap("host", "host-cid")),
            (Val::Str("host".into()), test_cap("host", "host-cid")),
        ]));
        let err = parse_export_policy(&policy).unwrap_err();
        assert!(format!("{err}").contains("duplicate cap key"));
    }

    #[test]
    fn test_parse_export_policy_accepts_recursive_returns_under_anypointer() {
        let deny_more = AttenuationPolicy {
            allow_methods: ["id".to_string()].into_iter().collect(),
            returns: BTreeMap::from([(
                "id".to_string(),
                BTreeMap::from([("x".to_string(), AttenuationPolicy::default())]),
            )]),
        };
        let vat_client_policy = AttenuationPolicy {
            allow_methods: ["dial".to_string()].into_iter().collect(),
            returns: BTreeMap::from([(
                "dial".to_string(),
                BTreeMap::from([("cap".to_string(), deny_more)]),
            )]),
        };
        let host_policy = AttenuationPolicy {
            allow_methods: ["network".to_string()].into_iter().collect(),
            returns: BTreeMap::from([(
                "network".to_string(),
                BTreeMap::from([("vatClient".to_string(), vat_client_policy)]),
            )]),
        };
        let host_att = make_cap(
            "host",
            "host-cid",
            Rc::new(AttenuatedCapInner {
                base: test_cap("host", "host-cid"),
                policy: host_policy,
                descriptor: vec![],
            }),
        );
        let policy = Val::Map(glia::ValMap::from_pairs(vec![(Val::Keyword("host".into()), host_att)]));
        let parsed = parse_export_policy(&policy).unwrap();
        assert!(parsed.caps.contains_key("host"));
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

    // --- host listen tests ---

    #[tokio::test]
    async fn test_host_listen_vat_passes_runtime() {
        run_local(async {
            let s = test_session();
            let handler =
                make_host_handler(s.host.clone(), s.runtime.clone(), s.http_client.clone());
            let runtime_cap = make_cap("runtime", "test-runtime-cid", Rc::new(s.runtime.clone()));
            let result = call_handler(
                &handler,
                "listen",
                &[runtime_cap, Val::Bytes(b"fake-wasm".to_vec())],
            )
            .await;
            assert!(
                result.is_ok(),
                "VatListener listen failed: {:?}",
                result.unwrap_err()
            );
        })
        .await;
    }

    #[tokio::test]
    async fn test_host_listen_stream_passes_runtime() {
        run_local(async {
            let s = test_session();
            let handler =
                make_host_handler(s.host.clone(), s.runtime.clone(), s.http_client.clone());
            let runtime_cap = make_cap("runtime", "test-runtime-cid", Rc::new(s.runtime.clone()));
            let result = call_handler(
                &handler,
                "listen",
                &[
                    runtime_cap,
                    Val::Str("my-protocol".into()),
                    Val::Bytes(b"fake-wasm".to_vec()),
                ],
            )
            .await;
            assert!(
                result.is_ok(),
                "StreamListener listen failed: {:?}",
                result.unwrap_err()
            );
        })
        .await;
    }

    #[tokio::test]
    async fn test_host_listen_missing_runtime_errors() {
        run_local(async {
            let s = test_session();
            let handler =
                make_host_handler(s.host.clone(), s.runtime.clone(), s.http_client.clone());
            let err = call_handler(&handler, "listen", &[Val::Bytes(b"wasm".to_vec())]).await;
            assert!(err.is_err(), "should require runtime capability");
        })
        .await;
    }

    #[tokio::test]
    async fn test_host_listen_wrong_cap_type_errors() {
        run_local(async {
            let s = test_session();
            let handler =
                make_host_handler(s.host.clone(), s.runtime.clone(), s.http_client.clone());
            let bad_cap = make_cap("not-runtime", "test-not-runtime-cid", Rc::new(42i32));
            let err =
                call_handler(&handler, "listen", &[bad_cap, Val::Bytes(b"wasm".to_vec())]).await;
            assert!(err.is_err(), "should reject wrong capability type");
            let msg = format!("{}", err.unwrap_err());
            assert!(
                msg.contains("not-runtime"),
                "error should name the wrong cap: {msg}"
            );
        })
        .await;
    }

    #[tokio::test]
    async fn test_host_listen_forged_runtime_cap_wrong_inner_type() {
        run_local(async {
            let s = test_session();
            let handler =
                make_host_handler(s.host.clone(), s.runtime.clone(), s.http_client.clone());
            let forged_cap = make_cap("runtime", "test-runtime-cid", Rc::new(42i32));
            let err = call_handler(
                &handler,
                "listen",
                &[forged_cap, Val::Bytes(b"wasm".to_vec())],
            )
            .await;
            assert!(err.is_err(), "forged cap should be rejected");
            let msg = format!("{}", err.unwrap_err());
            assert!(msg.contains("wrong inner type"), "got: {msg}");
        })
        .await;
    }

    #[tokio::test]
    async fn test_host_listen_string_instead_of_cap_errors() {
        run_local(async {
            let s = test_session();
            let handler =
                make_host_handler(s.host.clone(), s.runtime.clone(), s.http_client.clone());
            let err = call_handler(
                &handler,
                "listen",
                &[Val::Str("runtime".into()), Val::Bytes(b"wasm".to_vec())],
            )
            .await;
            assert!(err.is_err(), "string should not pass as runtime cap");
            let msg = format!("{}", err.unwrap_err());
            assert!(msg.contains("expected a cell"), "got: {msg}");
        })
        .await;
    }

    #[tokio::test]
    async fn test_host_listen_nil_instead_of_cap_errors() {
        run_local(async {
            let s = test_session();
            let handler =
                make_host_handler(s.host.clone(), s.runtime.clone(), s.http_client.clone());
            let err = call_handler(
                &handler,
                "listen",
                &[Val::Nil, Val::Bytes(b"wasm".to_vec())],
            )
            .await;
            assert!(err.is_err(), "nil should not pass as runtime cap");
        })
        .await;
    }

    #[tokio::test]
    async fn test_host_listen_stream_wrong_cap_type_errors() {
        run_local(async {
            let s = test_session();
            let handler =
                make_host_handler(s.host.clone(), s.runtime.clone(), s.http_client.clone());
            let bad_cap = make_cap("imposter", "test-imposter-cid", Rc::new(42i32));
            let err = call_handler(
                &handler,
                "listen",
                &[
                    bad_cap,
                    Val::Str("my-protocol".into()),
                    Val::Bytes(b"wasm".to_vec()),
                ],
            )
            .await;
            assert!(err.is_err(), "wrong cap should be rejected in stream mode");
            let msg = format!("{}", err.unwrap_err());
            assert!(
                msg.contains("imposter"),
                "error should name the wrong cap: {msg}"
            );
        })
        .await;
    }

    #[tokio::test]
    async fn test_host_listen_stream_missing_runtime_errors() {
        run_local(async {
            let s = test_session();
            let handler =
                make_host_handler(s.host.clone(), s.runtime.clone(), s.http_client.clone());
            let err = call_handler(
                &handler,
                "listen",
                &[Val::Str("my-protocol".into()), Val::Bytes(b"wasm".to_vec())],
            )
            .await;
            assert!(err.is_err(), "stream listen without runtime should error");
        })
        .await;
    }

    #[tokio::test]
    async fn test_host_listen_wrong_arity_returns_error() {
        run_local(async {
            let s = test_session();
            let handler =
                make_host_handler(s.host.clone(), s.runtime.clone(), s.http_client.clone());
            // 0 args after :listen — should error
            assert!(call_handler(&handler, "listen", &[]).await.is_err());
            // 4 args after :listen — should error
            let runtime_cap = make_cap("runtime", "test-runtime-cid", Rc::new(s.runtime.clone()));
            assert!(call_handler(
                &handler,
                "listen",
                &[
                    runtime_cap,
                    Val::Str("a".into()),
                    Val::Bytes(b"b".to_vec()),
                    Val::Str("extra".into()),
                ],
            )
            .await
            .is_err());
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
                            assert_eq!(*peer_id, Val::Str(bs58::encode(b"peer-0").into_string()));
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
            let result = call_handler(&handler, "resolve", &[Val::Str("/ipns/k51qzi-test".into())])
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
            env.set(name.to_string(), make_cap(name, cid, inner));
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

    /// Eval (perform host :listen runtime (perform :load "path")) end-to-end.
    #[tokio::test]
    async fn test_chess_glia_listen_form_evals_end_to_end() {
        run_local(async {
            let ctx = RefCell::new(test_session());
            let dispatch = build_dispatch();
            let mut env = Env::new();
            bind_caps_in_env(&mut env, &ctx.borrow());

            let dir = tempfile::tempdir().unwrap();
            let wasm_path = dir.path().join("chess-demo.wasm");
            std::fs::write(&wasm_path, b"fake-wasm-bytes").unwrap();

            let script = format!(
                r#"(perform host :listen runtime (perform :load "{}"))"#,
                wasm_path.to_str().unwrap()
            );
            let form = read(&script).unwrap();
            let wrapped = wrap_with_handlers(&form);
            let result = eval(&wrapped, &mut env, &ctx, &dispatch).await;
            assert!(
                result.is_ok(),
                "chess.glia listen form failed: {:?}",
                result.unwrap_err()
            );
        })
        .await;
    }

    /// Eval the full chess.glia script through the kernel eval pipeline.
    #[tokio::test]
    async fn test_chess_glia_full_script_parses_and_first_form_evals() {
        run_local(async {
            let ctx = RefCell::new(test_session());
            let dispatch = build_dispatch();
            let mut env = Env::new();
            bind_caps_in_env(&mut env, &ctx.borrow());

            let dir = tempfile::tempdir().unwrap();
            let wasm_path = dir.path().join("chess-demo.wasm");
            std::fs::write(&wasm_path, b"fake-wasm-bytes").unwrap();

            let script = format!(
                r#"(perform host :listen runtime (perform :load "{}"))
                   (perform runtime :run (perform :load "{}"))"#,
                wasm_path.to_str().unwrap(),
                wasm_path.to_str().unwrap()
            );

            let forms = read_many(&script).unwrap();
            assert_eq!(forms.len(), 2, "chess.glia should have 2 forms");

            // First form: (perform host :listen ...) — should succeed.
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
                schema: Some(b"fake-schema".to_vec()),
                caps: vec![("http".to_string(), http_cap)],
            };
            let result = call_handler(&handler, "listen", &[cell]).await;
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
                schema: Some(b"fake-schema".to_vec()),
                caps: vec![],
            };
            let result = call_handler(&handler, "listen", &[cell]).await;
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
        for name in [
            "host",
            "runtime",
            "routing",
            "identity",
            "http",
            "http-client",
        ] {
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
        assert_eq!(glia::error::type_tag(&err), Some(glia::error::tag::ARITY));
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
    fn doc_attenuated_cap_reports_recursive_returns() {
        let builtin = make_doc_builtin();
        let base = test_cap("host-att", "host-cid");

        let mut allow_host = BTreeSet::new();
        allow_host.insert("network".to_string());
        let mut allow_vat = BTreeSet::new();
        allow_vat.insert("dial".to_string());
        let mut allow_remote = BTreeSet::new();
        allow_remote.insert("id".to_string());

        let remote_policy = AttenuationPolicy {
            allow_methods: allow_remote,
            returns: BTreeMap::new(),
        };
        let vat_policy = AttenuationPolicy {
            allow_methods: allow_vat,
            returns: BTreeMap::from([(
                "dial".to_string(),
                BTreeMap::from([("cap".to_string(), remote_policy)]),
            )]),
        };
        let host_policy = AttenuationPolicy {
            allow_methods: allow_host,
            returns: BTreeMap::from([(
                "network".to_string(),
                BTreeMap::from([("vatClient".to_string(), vat_policy)]),
            )]),
        };

        let att_cap = make_cap(
            "host-att",
            "host-cid",
            Rc::new(AttenuatedCapInner {
                base,
                policy: host_policy,
                descriptor: b"attenuated".to_vec(),
            }),
        );

        let result = call_builtin(&builtin, &[att_cap]).unwrap();
        let text = match result {
            Val::Str(s) => s,
            other => panic!("expected Val::Str, got {other:?}"),
        };
        assert!(
            text.contains("return-edges:  2"),
            "doc should include recursive return edges: {text}"
        );
        assert!(
            text.contains("return-depth:  2"),
            "doc should include recursive return depth: {text}"
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
                policy: AttenuationPolicy {
                    allow_methods: allow,
                    returns: BTreeMap::new(),
                },
                descriptor: descriptor.clone(),
            }),
        );
        let out = call_builtin(&builtin, &[cap]).unwrap();
        assert_eq!(out, Val::Bytes(descriptor));
    }

    #[test]
    fn unwrap_cap_arg_rejects_zero_args() {
        let err = unwrap_cap_arg("schema", &[]).unwrap_err();
        assert_eq!(glia::error::type_tag(&err), Some(glia::error::tag::ARITY));
    }

    #[test]
    fn unwrap_cap_arg_rejects_two_args() {
        let err = unwrap_cap_arg("schema", &[Val::Nil, Val::Nil]).unwrap_err();
        assert_eq!(glia::error::type_tag(&err), Some(glia::error::tag::ARITY));
    }
}
