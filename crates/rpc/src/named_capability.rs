//! Validated named Cap'n Proto capability references and `Export` wire helpers.
//!
//! Names are labels only. Capability authority and routing remain entirely in
//! the opaque client hook; these helpers never resolve, invoke, or wrap it.

use std::collections::HashSet;
use std::sync::Arc;

use authority::membrane_capnp;

/// One validated name bound to an opaque Cap'n Proto capability reference.
#[derive(Clone)]
pub struct NamedCapability {
    name: String,
    capability: capnp::capability::Client,
}

impl NamedCapability {
    /// Validate and construct one named capability.
    pub fn new(
        name: impl Into<String>,
        capability: capnp::capability::Client,
    ) -> Result<Self, capnp::Error> {
        let name = name.into();
        validate_name(&name)?;
        Ok(Self { name, capability })
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn capability(&self) -> &capnp::capability::Client {
        &self.capability
    }
}

/// An immutable, validated, duplicate-free collection of named capabilities.
#[derive(Clone, Default)]
pub struct NamedCapabilities {
    entries: Arc<[NamedCapability]>,
}

impl NamedCapabilities {
    /// Validate a collection, rejecting duplicate names.
    pub fn try_from_iter(
        entries: impl IntoIterator<Item = NamedCapability>,
    ) -> Result<Self, capnp::Error> {
        let entries: Vec<_> = entries.into_iter().collect();
        let mut names = HashSet::with_capacity(entries.len());
        for entry in &entries {
            if !names.insert(entry.name()) {
                return Err(capnp::Error::failed(format!(
                    "duplicate capability name '{}'",
                    entry.name()
                )));
            }
        }
        Ok(Self {
            entries: entries.into(),
        })
    }

    /// Validate names and construct a collection from `(name, capability)` pairs.
    pub fn try_from_pairs<N>(
        entries: impl IntoIterator<Item = (N, capnp::capability::Client)>,
    ) -> Result<Self, capnp::Error>
    where
        N: Into<String>,
    {
        entries
            .into_iter()
            .map(|(name, capability)| NamedCapability::new(name, capability))
            .collect::<Result<Vec<_>, _>>()
            .and_then(Self::try_from_iter)
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    pub fn iter(&self) -> impl ExactSizeIterator<Item = &NamedCapability> {
        self.entries.iter()
    }
}

/// Decode and validate a wire `List(Export)` without resolving capabilities.
pub fn decode_exports(
    reader: capnp::struct_list::Reader<'_, membrane_capnp::export::Owned>,
) -> Result<NamedCapabilities, capnp::Error> {
    let mut entries = Vec::with_capacity(reader.len() as usize);
    for (index, entry) in reader.iter().enumerate() {
        if !entry.has_name() {
            return Err(capnp::Error::failed(format!(
                "capability export {index} is missing its name"
            )));
        }
        let name = entry
            .get_name()?
            .to_str()
            .map_err(|error| {
                capnp::Error::failed(format!(
                    "capability export {index} has an invalid UTF-8 name: {error}"
                ))
            })?
            .to_owned();
        if !entry.has_cap() {
            return Err(capnp::Error::failed(format!(
                "capability export '{name}' is missing its capability"
            )));
        }
        let capability = entry
            .get_cap()
            .get_as_capability::<capnp::capability::Client>()
            .map_err(|error| {
                capnp::Error::failed(format!(
                    "capability export '{name}' does not contain a capability: {error}"
                ))
            })?;
        entries.push(NamedCapability::new(name, capability)?);
    }
    NamedCapabilities::try_from_iter(entries)
}

/// Encode validated capabilities into an exactly-sized wire `List(Export)`.
pub fn encode_exports(
    capabilities: &NamedCapabilities,
    mut builder: capnp::struct_list::Builder<'_, membrane_capnp::export::Owned>,
) -> Result<(), capnp::Error> {
    if builder.len() as usize != capabilities.len() {
        return Err(capnp::Error::failed(format!(
            "capability export builder length {} does not match collection length {}",
            builder.len(),
            capabilities.len()
        )));
    }
    for (index, capability) in capabilities.iter().enumerate() {
        encode_export(capability, builder.reborrow().get(index as u32));
    }
    Ok(())
}

pub(crate) fn encode_export(
    capability: &NamedCapability,
    mut builder: membrane_capnp::export::Builder<'_>,
) {
    builder.set_name(capability.name());
    builder
        .init_cap()
        .set_as_capability(capability.capability().clone().hook);
}

fn validate_name(name: &str) -> Result<(), capnp::Error> {
    if name.is_empty() {
        return Err(capnp::Error::failed(
            "capability name must not be empty".into(),
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use authority::system_capnp;
    use capnp::capability::Promise;
    use capnp::private::capability::{ClientHook, ParamsHook, ResultsHook};
    use capnp::traits::{Imbue, ImbueMut};
    use std::cell::Cell;
    use std::rc::Rc;
    use std::time::Duration;

    struct HostStub {
        calls: Rc<Cell<u32>>,
    }

    #[allow(refining_impl_trait)]
    impl system_capnp::host::Server for HostStub {
        fn id(
            self: capnp::capability::Rc<Self>,
            _params: system_capnp::host::IdParams,
            mut results: system_capnp::host::IdResults,
        ) -> Promise<(), capnp::Error> {
            self.calls.set(self.calls.get() + 1);
            results.get().set_peer_id(b"named-capability");
            Promise::ok(())
        }
    }

    struct RejectingRuntime;

    #[allow(refining_impl_trait)]
    impl system_capnp::runtime::Server for RejectingRuntime {
        fn load(
            self: capnp::capability::Rc<Self>,
            _params: system_capnp::runtime::LoadParams,
            _results: system_capnp::runtime::LoadResults,
        ) -> Promise<(), capnp::Error> {
            Promise::err(capnp::Error::failed("already broken".into()))
        }
    }

    struct PendingRuntime {
        calls: Rc<Cell<u32>>,
    }

    #[allow(refining_impl_trait)]
    impl system_capnp::runtime::Server for PendingRuntime {
        fn load(
            self: capnp::capability::Rc<Self>,
            _params: system_capnp::runtime::LoadParams,
            _results: system_capnp::runtime::LoadResults,
        ) -> Promise<(), capnp::Error> {
            self.calls.set(self.calls.get() + 1);
            Promise::from_future(std::future::pending())
        }
    }

    struct ResolutionObservedClient {
        inner: Box<dyn ClientHook>,
        when_resolved_calls: Rc<Cell<u32>>,
    }

    impl ClientHook for ResolutionObservedClient {
        fn add_ref(&self) -> Box<dyn ClientHook> {
            Box::new(Self {
                inner: self.inner.add_ref(),
                when_resolved_calls: self.when_resolved_calls.clone(),
            })
        }

        fn new_call(
            &self,
            interface_id: u64,
            method_id: u16,
            size_hint: Option<capnp::MessageSize>,
        ) -> capnp::capability::Request<capnp::any_pointer::Owned, capnp::any_pointer::Owned>
        {
            self.inner.new_call(interface_id, method_id, size_hint)
        }

        fn call(
            &self,
            interface_id: u64,
            method_id: u16,
            params: Box<dyn ParamsHook>,
            results: Box<dyn ResultsHook>,
        ) -> Promise<(), capnp::Error> {
            self.inner.call(interface_id, method_id, params, results)
        }

        fn get_brand(&self) -> usize {
            self.inner.get_brand()
        }

        fn get_ptr(&self) -> usize {
            self.inner.get_ptr()
        }

        fn get_resolved(&self) -> Option<Box<dyn ClientHook>> {
            self.inner.get_resolved()
        }

        fn when_more_resolved(&self) -> Option<Promise<Box<dyn ClientHook>, capnp::Error>> {
            self.inner.when_more_resolved()
        }

        fn when_resolved(&self) -> Promise<(), capnp::Error> {
            self.when_resolved_calls
                .set(self.when_resolved_calls.get() + 1);
            self.inner.when_resolved()
        }
    }

    fn host_cap(calls: Rc<Cell<u32>>) -> capnp::capability::Client {
        let host: system_capnp::host::Client = capnp_rpc::new_client(HostStub { calls });
        host.client
    }

    fn roundtrip(capabilities: &NamedCapabilities) -> NamedCapabilities {
        let mut message = capnp::message::Builder::new_default();
        let mut cap_table = Vec::new();
        {
            let mut results =
                message.init_root::<membrane_capnp::membrane::graft_results::Builder<'_>>();
            results.imbue_mut(&mut cap_table);
            let exports = results.reborrow().init_caps(capabilities.len() as u32);
            encode_exports(capabilities, exports).expect("encode exports");
        }
        let mut results = message
            .get_root_as_reader::<membrane_capnp::membrane::graft_results::Reader<'_>>()
            .expect("graft results reader");
        results.imbue(&cap_table);
        decode_exports(results.get_caps().expect("exports reader")).expect("decode exports")
    }

    #[test]
    fn empty_collection_roundtrips() {
        let capabilities = NamedCapabilities::default();
        let decoded = roundtrip(&capabilities);
        assert!(decoded.is_empty());
    }

    #[test]
    fn encode_decode_preserves_exact_names_and_callable_references() {
        let calls = Rc::new(Cell::new(0));
        let capabilities =
            NamedCapabilities::try_from_pairs([("host", host_cap(calls.clone()))]).unwrap();
        let decoded = roundtrip(&capabilities);
        let entry = decoded.iter().next().unwrap();
        assert_eq!(entry.name(), "host");
        assert_eq!(
            calls.get(),
            0,
            "encoding and decoding must not invoke the retained capability"
        );

        let host = system_capnp::host::Client {
            client: entry.capability().clone(),
        };
        let local = tokio::task::LocalSet::new();
        local.block_on(&tokio::runtime::Runtime::new().unwrap(), async move {
            host.id_request().send().promise.await.unwrap();
        });
        assert_eq!(calls.get(), 1);
    }

    #[test]
    fn repeated_encoding_returns_the_same_named_set() {
        let calls = Rc::new(Cell::new(0));
        let capabilities = NamedCapabilities::try_from_pairs([
            ("first", host_cap(calls.clone())),
            ("second", host_cap(calls)),
        ])
        .unwrap();
        for decoded in [roundtrip(&capabilities), roundtrip(&capabilities)] {
            assert_eq!(
                decoded
                    .iter()
                    .map(NamedCapability::name)
                    .collect::<Vec<_>>(),
                ["first", "second"]
            );
        }
    }

    #[test]
    fn empty_names_fail_closed_without_treating_labels_as_paths() {
        let error = NamedCapabilities::try_from_pairs([("", host_cap(Rc::new(Cell::new(0))))])
            .err()
            .expect("empty name must fail");
        assert!(error.to_string().contains("capability name"));

        let labels = NamedCapabilities::try_from_pairs([
            ("path/like", host_cap(Rc::new(Cell::new(0)))),
            ("two words", host_cap(Rc::new(Cell::new(0)))),
            ("κλειδί", host_cap(Rc::new(Cell::new(0)))),
        ])
        .expect("nonempty UTF-8 labels are valid");
        assert_eq!(
            labels.iter().map(NamedCapability::name).collect::<Vec<_>>(),
            ["path/like", "two words", "κλειδί"]
        );
    }

    #[test]
    fn invalid_utf8_wire_names_fail_closed() {
        let cap = host_cap(Rc::new(Cell::new(0)));
        let mut message = capnp::message::Builder::new_default();
        let mut cap_table = Vec::new();
        {
            let mut results =
                message.init_root::<membrane_capnp::membrane::graft_results::Builder<'_>>();
            results.imbue_mut(&mut cap_table);
            let mut entry = results.reborrow().init_caps(1).get(0);
            entry.set_name(capnp::text::Reader(&[0xff]));
            entry.init_cap().set_as_capability(cap.hook);
        }
        let mut results = message
            .get_root_as_reader::<membrane_capnp::membrane::graft_results::Reader<'_>>()
            .unwrap();
        results.imbue(&cap_table);
        let error = decode_exports(results.get_caps().unwrap())
            .err()
            .expect("invalid UTF-8 names must fail");
        assert!(error.to_string().contains("invalid UTF-8 name"));
    }

    #[test]
    fn duplicate_wire_names_fail_closed() {
        let cap = host_cap(Rc::new(Cell::new(0)));
        let mut message = capnp::message::Builder::new_default();
        let mut cap_table = Vec::new();
        {
            let mut results =
                message.init_root::<membrane_capnp::membrane::graft_results::Builder<'_>>();
            results.imbue_mut(&mut cap_table);
            let mut exports = results.reborrow().init_caps(2);
            for index in 0..2 {
                let mut entry = exports.reborrow().get(index);
                entry.set_name("duplicate");
                entry.init_cap().set_as_capability(cap.clone().hook);
            }
        }
        let mut results = message
            .get_root_as_reader::<membrane_capnp::membrane::graft_results::Reader<'_>>()
            .unwrap();
        results.imbue(&cap_table);
        let error = decode_exports(results.get_caps().unwrap())
            .err()
            .expect("duplicate names must fail");
        assert!(error.to_string().contains("duplicate capability name"));
    }

    #[test]
    fn missing_and_malformed_capability_fields_fail_closed() {
        let mut missing = capnp::message::Builder::new_default();
        {
            let mut results =
                missing.init_root::<membrane_capnp::membrane::graft_results::Builder<'_>>();
            results.reborrow().init_caps(1).get(0).set_name("missing");
        }
        let results = missing
            .get_root_as_reader::<membrane_capnp::membrane::graft_results::Reader<'_>>()
            .unwrap();
        let error = decode_exports(results.get_caps().unwrap())
            .err()
            .expect("missing capability must be rejected");
        assert!(error.to_string().contains("missing its capability"));

        let mut malformed = capnp::message::Builder::new_default();
        {
            let mut results =
                malformed.init_root::<membrane_capnp::membrane::graft_results::Builder<'_>>();
            let mut entry = results.reborrow().init_caps(1).get(0);
            entry.set_name("malformed");
            entry
                .init_cap()
                .set_as::<capnp::text::Owned>("not a capability")
                .unwrap();
        }
        let results = malformed
            .get_root_as_reader::<membrane_capnp::membrane::graft_results::Reader<'_>>()
            .unwrap();
        assert!(decode_exports(results.get_caps().unwrap()).is_err());
    }

    #[test]
    fn same_capability_under_two_names_succeeds_and_invokes_one_server() {
        let calls = Rc::new(Cell::new(0));
        let cap = host_cap(calls.clone());
        let decoded = roundtrip(
            &NamedCapabilities::try_from_pairs([("alias-a", cap.clone()), ("alias-b", cap)])
                .unwrap(),
        );
        let local = tokio::task::LocalSet::new();
        local.block_on(&tokio::runtime::Runtime::new().unwrap(), async move {
            for entry in decoded.iter() {
                let host = system_capnp::host::Client {
                    client: entry.capability().clone(),
                };
                host.id_request().send().promise.await.unwrap();
            }
        });
        assert_eq!(calls.get(), 2);
    }

    #[test]
    fn unresolved_pipelined_capability_remains_unresolved() {
        let calls = Rc::new(Cell::new(0));
        let runtime: system_capnp::runtime::Client = capnp_rpc::new_client(PendingRuntime {
            calls: calls.clone(),
        });
        let response = runtime.load_request().send();
        let executor = response.pipeline.get_executor();
        let _response_promise = response.promise;
        let when_resolved_calls = Rc::new(Cell::new(0));
        let observed_executor = capnp::capability::Client {
            hook: Box::new(ResolutionObservedClient {
                inner: executor.client.hook,
                when_resolved_calls: when_resolved_calls.clone(),
            }),
        };
        let decoded = roundtrip(
            &NamedCapabilities::try_from_pairs([("executor", observed_executor)]).unwrap(),
        );
        let capability = decoded.iter().next().unwrap().capability().clone();
        assert_eq!(
            calls.get(),
            0,
            "wire helpers must neither invoke nor await resolution"
        );
        assert_eq!(
            when_resolved_calls.get(),
            0,
            "wire helpers must not call when_resolved"
        );

        let local = tokio::task::LocalSet::new();
        local.block_on(&tokio::runtime::Runtime::new().unwrap(), async move {
            assert!(
                tokio::time::timeout(Duration::from_millis(10), capability.when_resolved())
                    .await
                    .is_err(),
                "wire helpers must not resolve a pipelined capability"
            );
        });
        assert_eq!(calls.get(), 1, "only the explicit resolution probe ran");
        assert_eq!(
            when_resolved_calls.get(),
            1,
            "only the explicit resolution probe called when_resolved"
        );
    }

    #[test]
    fn broken_pipelined_capability_remains_broken() {
        let runtime: system_capnp::runtime::Client = capnp_rpc::new_client(RejectingRuntime);
        let response = runtime.load_request().send();
        let executor = response.pipeline.get_executor();
        let local = tokio::task::LocalSet::new();
        local.block_on(&tokio::runtime::Runtime::new().unwrap(), async move {
            assert!(response.promise.await.is_err());
            let decoded = roundtrip(
                &NamedCapabilities::try_from_pairs([("executor", executor.client)]).unwrap(),
            );
            let executor = system_capnp::executor::Client {
                client: decoded.iter().next().unwrap().capability().clone(),
            };
            executor
                .cid_request()
                .send()
                .promise
                .await
                .err()
                .expect("the retained broken capability must reject when explicitly invoked");
        });
    }
}
