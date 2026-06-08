# Uniform Wetware capability ABI.
#
# A Synapse is the only public capability currency that crosses WW membranes,
# process bootstrap, and vat serve/dial boundaries. Authority is carried by the
# invokable capability reference. Descriptor data is declaration/introspection
# metadata, not routing, provenance, or proof of behavior.

@0xf2a6b7d9c4e35180;

struct Synapse {
  descriptor @0 :Descriptor;
  invokable  @1 :Invokable;
}

struct Descriptor {
  displayName         @0 :Text;
  interfaceId         @1 :UInt64;
  schemaCid           @2 :Text;
  methods             @3 :List(Method);
  payloadCodec        @4 :PayloadCodec;
  invokerInterfaceIds @5 :List(UInt64);
  schemaNodes         @6 :List(Data);
}

enum PayloadCodec {
  capnp    @0;
  gliaValue @1;
}

struct Method {
  interfaceId @0 :UInt64;
  ordinal     @1 :UInt16;
  name        @2 :Text;
}

struct MethodKey {
  interfaceId @0 :UInt64;
  ordinal     @1 :UInt16;
}

struct Payload {
  union {
    capnp @0 :AnyPointer;
    value @1 :Value;
  }
}

struct Result {
  union {
    capnp @0 :AnyPointer;
    value @1 :Value;
  }
}

struct Value {
  union {
    void  @0 :Void;
    bool  @1 :Bool;
    int   @2 :Int64;
    uint  @3 :UInt64;
    float @4 :Float64;
    text  @5 :Text;
    data  @6 :Data;
    list  @7 :List(Value);
  }
}

interface Invokable {
  invoke @0 (method :MethodKey, payload :Payload) -> (result :Result);
}

# Internal transport helper for Cap'n Proto vat bootstrap, whose bootstrap slot
# can carry only a capability. Public WW APIs still serve/dial Synapse structs.
interface Bootstrap {
  get @0 () -> (synapse :Synapse);
}
