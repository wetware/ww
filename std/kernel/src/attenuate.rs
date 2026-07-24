//! Membrane-backed attenuation for capnp-backed capabilities.
//!
//! Implements the kernel side of `glia::eval::Dispatch::reify_attenuation`:
//! `(attenuate cap [:method ...])` on a capnp-backed cap wraps the underlying
//! client hook in a `wetware-membrane` allowlist keyed by `(interface_id, ordinal)`,
//! so the attenuation is enforced at the capability hook itself and travels
//! with the cap across process/vat boundaries (export, serve, dial). The
//! evaluator-local string check in glia remains only for `defcap` caps, which
//! cannot cross a boundary.
//!
//! The reified cap is a [`glia::HandledCapInner`]:
//! * `handler` — the cap's own dispatch, rebuilt over the MEMBRANED typed
//!   client and gated by the same method allowlist, so local `perform` gets
//!   the same policy (and the same structured denial) as remote callers;
//! * `export` — a [`MembranedCap`] carrying the membraned
//!   `capnp::capability::Client`, which `extract_capnp_client` returns when
//!   the cap crosses a boundary.
//!
//! Method-name mapping: Glia keywords are kebab-case (`:http-client`), capnp
//! method names are camelCase (`httpClient`). Names are resolved against the
//! cap's compiled schema; unknown names fail closed at attenuation time.

use std::cell::RefCell;
use std::collections::BTreeSet;
use std::rc::Rc;

use capnp::capability::FromClientHook;
use glia::{extract_method, make_cap, HandledCapInner, Val};
use membrane::{membrane_hook, Allowlist, DENIED_MARKER};

use crate::{
    extract_capnp_client, make_host_handler, make_routing_handler, make_runtime_handler,
    schema_bytes_for_cap, Session,
};

/// Kernel export payload for a membrane-attenuated cap, stored in
/// [`HandledCapInner::export`]. `client` is the membraned client that crosses
/// boundaries; `allow` is the (glia-name) method allowlist, kept so
/// re-attenuation can intersect and introspection can report it.
pub struct MembranedCap {
    pub client: capnp::capability::Client,
    pub allow: BTreeSet<String>,
}

/// If this cap inner is a kernel membrane-attenuated cap, return its export
/// payload (for introspection and re-attenuation).
pub fn membraned_cap_of(inner: &Rc<dyn std::any::Any>) -> Option<&MembranedCap> {
    inner
        .downcast_ref::<HandledCapInner>()
        .and_then(|h| h.export.downcast_ref::<MembranedCap>())
}

/// Convert a Glia kebab-case method keyword to a capnp camelCase method name.
fn to_capnp_method_name(glia_name: &str) -> String {
    let mut out = String::with_capacity(glia_name.len());
    let mut upper_next = false;
    for c in glia_name.chars() {
        if c == '-' {
            upper_next = true;
        } else if upper_next {
            out.extend(c.to_uppercase());
            upper_next = false;
        } else {
            out.push(c);
        }
    }
    out
}

/// Resolve glia method names against a canonical `schema.Node` (single raw
/// segment, as produced by the schema-id build step). Returns the interface
/// type id plus `(glia_name, ordinal)` for each requested method. Unknown
/// method names fail closed with the schema's available methods in the hint.
fn resolve_method_keys(
    schema: &[u8],
    allow: &BTreeSet<String>,
) -> Result<(u64, Vec<(String, u16)>), Val> {
    // Schemas are embedded as byte slices, whose link-time alignment is not
    // guaranteed to meet Cap'n Proto's eight-byte segment requirement. Copy
    // them into Cap'n Proto's aligned Word storage before creating a reader.
    let mut words = capnp::Word::allocate_zeroed_vec(schema.len().div_ceil(8));
    capnp::Word::words_to_bytes_mut(&mut words)[..schema.len()].copy_from_slice(schema);
    let segments = [&capnp::Word::words_to_bytes(&words)[..schema.len()]];
    let segment_array = capnp::message::SegmentArray::new(&segments);
    let reader = capnp::message::Reader::new(segment_array, capnp::message::ReaderOptions::new());
    let node: capnp::schema_capnp::node::Reader = reader
        .get_root()
        .map_err(|e| glia::error::internal("attenuate schema", e.to_string()))?;

    let interface_id = node.get_id();
    let iface = match node.which() {
        Ok(capnp::schema_capnp::node::Which::Interface(i)) => i,
        _ => {
            return Err(glia::error::internal(
                "attenuate schema",
                "schema node is not an interface",
            ))
        }
    };
    let methods = iface
        .get_methods()
        .map_err(|e| glia::error::internal("attenuate schema", e.to_string()))?;

    let mut names: Vec<String> = Vec::with_capacity(methods.len() as usize);
    for m in methods.iter() {
        let n = m
            .get_name()
            .and_then(|n| n.to_str().map_err(|e| capnp::Error::failed(e.to_string())))
            .map_err(|e| glia::error::internal("attenuate schema", e.to_string()))?;
        names.push(n.to_string());
    }

    let mut resolved = Vec::with_capacity(allow.len());
    for glia_name in allow {
        let capnp_name = to_capnp_method_name(glia_name);
        match names.iter().position(|n| *n == capnp_name) {
            // Method ordinals are list positions in the interface node.
            Some(ordinal) => resolved.push((glia_name.clone(), ordinal as u16)),
            None => {
                return Err(glia::error::permission_denied(
                    &format!("attenuate: method :{glia_name} not found on interface"),
                    Some(&format!("schema methods: {}", names.join(", "))),
                ))
            }
        }
    }
    Ok((interface_id, resolved))
}

/// Map an error produced behind the membrane into the structured Glia
/// permission-denied error when it carries the membrane's denial marker.
/// Anything else passes through untouched.
fn map_membrane_denial(err: Val, cap_name: &str) -> Val {
    let text = match &err {
        Val::Str(s) => s.clone(),
        other => format!("{other}"),
    };
    if text.contains(DENIED_MARKER) {
        glia::error::permission_denied(
            &format!("method denied by membrane on '{cap_name}'"),
            Some(&text),
        )
    } else {
        err
    }
}

/// Wrap a rebuilt typed handler with the method-name gate. The gate produces
/// the same structured denial locally that the membrane produces at the wire,
/// including the resolved `(interface_id, ordinal)` key (roadmap D9).
fn make_gated_handler(
    cap_name: &str,
    interface_id: u64,
    allowed: Rc<Vec<(String, u16)>>,
    inner: Option<Val>,
) -> Val {
    let cap_name = cap_name.to_string();
    Val::AsyncNativeFn {
        name: format!("{cap_name}-attenuated-handler"),
        func: Rc::new(move |args: Vec<Val>| {
            let cap_name = cap_name.clone();
            let allowed = allowed.clone();
            let inner = inner.clone();
            Box::pin(async move {
                if args.len() != 2 {
                    return Err(glia::error::arity(
                        "attenuated cap handler",
                        "2",
                        args.len(),
                    ));
                }
                let (method, _) = extract_method(&args[0])?;
                if !allowed.iter().any(|(n, _)| n == method) {
                    return Err(glia::error::permission_denied(
                        &format!("method :{method} denied by attenuation policy on '{cap_name}'"),
                        Some(&format!(
                            "interface 0x{interface_id:016x}; allowed: {}",
                            allowed
                                .iter()
                                .map(|(n, o)| format!(":{n}@{o}"))
                                .collect::<Vec<_>>()
                                .join(" ")
                        )),
                    ));
                }
                let Some(handler) = inner else {
                    return Err(glia::error::permission_denied(
                        &format!("no local dispatch for attenuated capability '{cap_name}'"),
                        Some("the cap is still enforceable across boundaries (export/serve)"),
                    ));
                };
                let result = match &handler {
                    Val::AsyncNativeFn { func, .. } => func(args).await,
                    Val::NativeFn { func, .. } => func(&args),
                    other => Err(glia::error::internal(
                        "attenuated cap handler",
                        format!("inner handler is not callable: {other}"),
                    )),
                };
                result.map_err(|e| map_membrane_denial(e, &cap_name))
            })
        }),
    }
}

/// The kernel's `Dispatch::reify_attenuation` implementation. Returns `None`
/// for caps that are not capnp-backed (glia then applies its local path).
pub fn reify(
    ctx: &RefCell<Session>,
    cap: &Val,
    allow: &BTreeSet<String>,
) -> Option<Result<Val, Val>> {
    let Val::Cap {
        name,
        schema_cid,
        inner,
        ..
    } = cap
    else {
        return None;
    };

    // Re-attenuation of one of our membraned caps intersects the name sets;
    // the hook-level wrap below folds into a single membrane layer via
    // wetware-membrane's allowlist collapse.
    let (client, effective_allow): (capnp::capability::Client, BTreeSet<String>) =
        if let Some(handled) = inner.downcast_ref::<HandledCapInner>() {
            match handled.export.downcast_ref::<MembranedCap>() {
                Some(m) => (
                    m.client.clone(),
                    allow.intersection(&m.allow).cloned().collect(),
                ),
                // A handled cap without a kernel export payload is not ours.
                None => return None,
            }
        } else if let Some(c) = extract_capnp_client(inner) {
            (c, allow.clone())
        } else {
            // Pure-Glia cap (defcap): evaluator-local attenuation applies.
            return None;
        };

    let Some(schema) = schema_bytes_for_cap(name) else {
        // capnp-backed but no compiled schema (e.g. a dialed generic cap):
        // we cannot resolve method ordinals, so we cannot build the membrane
        // allowlist. Fail closed rather than fall back to a string check the
        // wire would not enforce. (Schema association for dialed caps is the
        // deferred D24 design.)
        return Some(Err(glia::error::permission_denied(
            &format!("cannot attenuate '{name}': no compiled schema for this capability"),
            Some("only caps with build-time schemas (host, runtime, routing, identity, http-client) can be attenuated"),
        )));
    };

    let (interface_id, resolved) = match resolve_method_keys(schema, &effective_allow) {
        Ok(v) => v,
        Err(e) => return Some(Err(e)),
    };

    let mut allowlist = Allowlist::new();
    for (_, ordinal) in &resolved {
        allowlist = allowlist.allow(interface_id, *ordinal);
    }
    let membraned_hook = membrane_hook(client.hook, Rc::new(allowlist));
    let export_client = capnp::capability::Client::new(membraned_hook.add_ref());

    // Rebuild the cap's local dispatch over the MEMBRANED client, so local
    // perform routes through the same enforcement as remote callers. Caps
    // without a kernel handler stay export-enforceable only.
    let typed_handler = match name.as_str() {
        "host" => {
            let s = ctx.borrow();
            Some(make_host_handler(
                FromClientHook::new(membraned_hook.add_ref()),
                s.runtime.clone(),
                s.http_client.clone(),
            ))
        }
        "runtime" => Some(make_runtime_handler(FromClientHook::new(
            membraned_hook.add_ref(),
        ))),
        "routing" => Some(make_routing_handler(FromClientHook::new(
            membraned_hook.add_ref(),
        ))),
        _ => None,
    };

    let allowed = Rc::new(resolved);
    let handler = make_gated_handler(name, interface_id, allowed.clone(), typed_handler);

    let descriptor = format!(
        "ww.attenuated.v1\ninterface=0x{interface_id:016x}\nmethods={}\n",
        allowed
            .iter()
            .map(|(n, o)| format!("{n}@{o}"))
            .collect::<Vec<_>>()
            .join(",")
    )
    .into_bytes();

    Some(Ok(make_cap(
        name.clone(),
        schema_cid.clone(),
        Rc::new(HandledCapInner {
            handler,
            export: Rc::new(MembranedCap {
                client: export_client,
                allow: effective_allow,
            }),
            descriptor,
        }),
    )))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_method_keys_accepts_unaligned_schema_bytes() {
        let schema = schema_bytes_for_cap("host").expect("host schema");
        let mut unaligned = vec![0_u8; schema.len() + 1];
        unaligned[1..].copy_from_slice(schema);
        let allow = BTreeSet::from(["id".to_string()]);

        let (_, methods) = resolve_method_keys(&unaligned[1..], &allow)
            .expect("an aligned copy must make schema parsing independent of source alignment");
        assert_eq!(methods, vec![("id".to_string(), 0)]);
    }
}
