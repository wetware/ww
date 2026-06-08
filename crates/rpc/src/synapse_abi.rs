use capnp::capability::Promise;
use capnp::traits::HasTypeId;
use capnp_rpc::new_client;
use membrane::synapse_capnp;

#[derive(Clone)]
pub struct OwnedSynapse {
    pub display_name: String,
    pub interface_id: u64,
    pub schema_cid: String,
    pub invokable: synapse_capnp::invokable::Client,
}

impl OwnedSynapse {
    pub fn placeholder(display_name: impl Into<String>) -> Self {
        Self {
            display_name: display_name.into(),
            interface_id: 0,
            schema_cid: String::new(),
            invokable: new_client(NoopInvokable),
        }
    }
}

pub fn read_owned_synapse(
    reader: synapse_capnp::synapse::Reader<'_>,
) -> capnp::Result<OwnedSynapse> {
    let descriptor = reader.get_descriptor()?;
    Ok(OwnedSynapse {
        display_name: descriptor
            .get_display_name()
            .map(|name| name.to_string().unwrap_or_default())
            .unwrap_or_default(),
        interface_id: descriptor.get_interface_id(),
        schema_cid: descriptor
            .get_schema_cid()
            .map(|cid| cid.to_string().unwrap_or_default())
            .unwrap_or_default(),
        invokable: reader.get_invokable()?,
    })
}

pub fn write_owned_synapse(
    mut builder: synapse_capnp::synapse::Builder<'_>,
    synapse: &OwnedSynapse,
) {
    let mut descriptor = builder.reborrow().init_descriptor();
    descriptor.set_display_name(&synapse.display_name);
    descriptor.set_interface_id(synapse.interface_id);
    descriptor.set_schema_cid(&synapse.schema_cid);
    descriptor.set_payload_codec(synapse_capnp::PayloadCodec::Capnp);
    descriptor.reborrow().init_methods(0);
    descriptor
        .reborrow()
        .init_invoker_interface_ids(1)
        .set(0, synapse_capnp::invokable::Client::TYPE_ID);
    descriptor.init_schema_nodes(0);
    builder.set_invokable(synapse.invokable.clone());
}

pub fn write_placeholder_synapse(
    builder: synapse_capnp::synapse::Builder<'_>,
    display_name: impl Into<String>,
) {
    write_owned_synapse(builder, &OwnedSynapse::placeholder(display_name));
}

struct NoopInvokable;

impl synapse_capnp::invokable::Server for NoopInvokable {
    fn invoke(
        self: capnp::capability::Rc<Self>,
        _params: synapse_capnp::invokable::InvokeParams,
        _results: synapse_capnp::invokable::InvokeResults,
    ) -> impl std::future::Future<Output = Result<(), capnp::Error>> + 'static {
        Promise::err(capnp::Error::unimplemented(
            "placeholder Synapse invokable has no backend".into(),
        ))
    }
}

pub(crate) struct BootstrapServer {
    synapse: OwnedSynapse,
}

impl BootstrapServer {
    pub(crate) fn new(synapse: OwnedSynapse) -> Self {
        Self { synapse }
    }
}

impl synapse_capnp::bootstrap::Server for BootstrapServer {
    fn get(
        self: capnp::capability::Rc<Self>,
        _params: synapse_capnp::bootstrap::GetParams,
        mut results: synapse_capnp::bootstrap::GetResults,
    ) -> impl std::future::Future<Output = Result<(), capnp::Error>> + 'static {
        write_owned_synapse(results.get().init_synapse(), &self.synapse);
        Promise::ok(())
    }
}
