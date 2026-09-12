# Wetware transport and process interfaces.

@0xbf5147b78c0e6a2f;

using AuthSchema = import "auth.capnp";
using RoutingSchema = import "routing.capnp";
using HttpSchema = import "http.capnp";

struct Export @0xbb8d5590cb2f3d2e {
  name @0 :Text;
  cap  @1 :Capability;
  # An application-defined capability bound to a local name. Fixed Wetware
  # platform authority uses typed fields.
}

struct NodeStat @0xa00344c466fb1f93 {
  listenAddrs         @0 :List(Data);
  connectedPeerCount @1 :UInt32;
}

interface Stat @0xa7b9b759cc17f8ca {
  snapshot @0 () -> (stat :NodeStat);
  # Return one current observation without exposing peer identities or
  # connected-peer endpoint addresses.
}

struct Network @0xebd3804c63534aaf {
  stream :group {
    listener @0 :StreamListener;
    dialer   @1 :StreamDialer;
  }

  vat :group {
    listener @2 :VatListener;
    dialer   @3 :VatClient;
  }

  http :group {
    listener @4 :HttpListener;
    dialer   @5 :HttpSchema.HttpClient;
  }
}

struct Routing @0xbd9034a6f00b9064 {
  finder    @0 :RoutingSchema.Finder;
  announcer @1 :RoutingSchema.Announcer;
}

interface Membrane @0xdb52c25106bc2c5e {
  graft @0 () -> (
    peerId :Data,

    stat    :Stat,
    network :Network,
    routing :Routing,

    runtime   :Runtime,
    authority :AuthSchema.Authority,
    identity  :AuthSchema.Identity,
    ipfs      :Ipfs,

    extras :List(Export)
  );
  # Having a Membrane reference is authorization. Every successful graft has
  # a non-empty peerId. A withheld capability is a null capability pointer.
  # Repeated graft calls expose only the authority held by this Membrane.
}

interface Runtime {
  load @0 (wasm :Data) -> (executor :Executor);
  # Compile or retrieve a cached executor bound to the supplied WASM bytes.

  shutdown @1 () -> ();
  # Terminate tasks spawned through this Runtime.
}

interface Ipfs {
  read @0 (path :Text) -> (stream :ByteStream);
  # Read bytes from an IPFS-family path as a stream via the daemon backend.
  # Accepts `/ipfs/<cid>`, `/ipns/...`, `/ipld/...`.
  # Used by non-WASI clients to preserve content-path semantics without
  # direct client-to-Kubo coupling.
}

struct FuelPolicy {
  union {
    scheduled @0 :Void;
    # System thread. Fuel is a scheduler signal, not a budget.
    # EWMA auto-adjusts. Runs indefinitely. Current behavior.

    oneshot @1 :OneshotFuel;
    # Fixed budget. Trap at exhaustion (Trap::OutOfFuel).
    # Auction-metered cells. "Prepaid card."
  }
}

struct OneshotFuel {
  totalBudget @0 :UInt64;
  maxPerEpoch @1 :UInt64;   # 0 = use MAX_FUEL default
  minPerEpoch @2 :UInt64;   # 0 = use MIN_FUEL default
}

interface Executor {
  spawn @0 (
    args :List(Text),
    env :List(Text),
    membrane :Membrane,
    fuelPolicy :FuelPolicy
  ) -> (process :Process);
  # Spawn one child with the supplied Membrane as its bootstrap capability.

  cid @1 () -> (cid :Text);
}

interface StreamListener {
  listen @0 (
    executor :Executor,
    protocol :Text,
    membrane :Membrane
  ) -> ();
  # Each accepted stream receives the registration-time Membrane.
}

interface HttpListener {
  listen @0 (
    executor :Executor,
    prefix :Text,
    membrane :Membrane
  ) -> ();
  # Each matching request receives the registration-time Membrane.
}

interface StreamDialer {
  dial @0 (peer :Data, protocol :Text) -> (stream :ByteStream);
  # Open a libp2p stream to peer on /ww/0.1.0/stream/{protocol}.
  # Returns a bidirectional ByteStream: read() pulls from the remote,
  # write() pushes to the remote, close() shuts down both directions.
}

interface Process {
  stdin @0 () -> (stream :ByteStream);
  # Writable stream connected to the guest's standard input.

  stdout @1 () -> (stream :ByteStream);
  # Readable stream connected to the guest's standard output.

  stderr @2 () -> (stream :ByteStream);
  # Readable stream connected to the guest's standard error.

  wait @3 () -> (exitCode :Int32);
  # Block until the process exits and return its exit code.

  bootstrap @4 () -> (cap :Capability);
  # Return the capability exported by the guest via system::serve().
  # Errors if the guest didn't export a capability.

  kill @5 () -> ();
  # Terminate the process immediately. Fuel is revoked and the cell traps.
}

interface VatListener {
  serveRaw @0 (cap :Capability, protocol :Text) -> ();
  # Accept incoming Cap'n Proto RPC connections on /ww/0.1.0/vat/{protocol}.
  # Each connection bootstraps with the provided capability. The protocol is a
  # locator only; capability authority comes from the cap, not from the name.
  # This is an explicit raw escape hatch and performs no recipient authentication.

  serveAuthenticated @1 (
    cap :Capability,
    protocol :Text,
    policy :AuthSchema.AuthorityPolicy
  ) -> ();
  # Accept authenticated Cap'n Proto RPC connections. Each inbound libp2p
  # stream receives a fresh, single-use Terminal. The stream remains admitted
  # only after login succeeds before the configured pre-authentication deadline.
}

interface VatClient {
  dial @0 (peer :Data, protocol :Text) -> (cap :Capability);
  # Open a Cap'n Proto RPC connection to peer on /ww/0.1.0/vat/{protocol}.
  # Bootstraps a Cap'n Proto vat over the stream and returns the remote cap.
}

interface ByteStream {
  read @0 (maxBytes :UInt32) -> (data :Data);
  # Read up to maxBytes from the stream.  Returns empty data at EOF.

  write @1 (data :Data) -> ();
  # Write data to the stream.

  close @2 () -> ();
  # Close the stream.  Further reads return EOF; further writes fail.
}
