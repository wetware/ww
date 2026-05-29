# Wetware peer interfaces.
#
# These capabilities are surfaced to WASM guests through the Membrane's
# epoch-scoped session (see membrane.capnp).  Each capability wrapper
# holds an EpochGuard and fails with a stale-epoch error once the guard
# detects the epoch has advanced.

@0xbf5147b78c0e6a2f;

using MembraneSchema = import "membrane.capnp";
using Schema = import "/capnp/schema.capnp";

struct SchemaBundle {
  root @0 :Schema.Node;
  deps @1 :List(Schema.Node);
}

struct TypedCap {
  cap @0 :Capability;
  schema @1 :SchemaBundle;
}

struct VatDescriptor {
  wasiCid @0 :Data;
  schemaCid @1 :Data;
}

struct PeerInfo {
  peerId @0 :Data;       # libp2p peer ID, serialized.
  addrs @1 :List(Data);  # Multiaddrs for this peer, each serialized.
}

interface Host {
  id @0 () -> (peerId :Data);
  # Return this node's libp2p peer ID.

  addrs @1 () -> (addrs :List(Data));
  # Return the multiaddrs this node is listening on.

  peers @2 () -> (peers :List(PeerInfo));
  # List currently connected peers.

  network @3 () -> (streamListener :StreamListener, streamDialer :StreamDialer,
                    vatListener :VatListener, vatClient :VatClient,
                    httpListener :HttpListener);
  # Obtain StreamListener/StreamDialer (libp2p byte-stream mode),
  # VatListener/VatClient (Cap'n Proto capability mode), and
  # HttpListener (WAGI/CGI mode) for subprotocol I/O.
}

interface Runtime {
  load @0 (wasm :Data) -> (executor :Executor);
  # Compile (or cache-hit) the WASM bytes and return an Executor bound
  # to that binary.
  #
  # Cache policy is set at the Runtime level (--runtime-cache-policy),
  # not per-call. Default is "shared": if the same bytes were loaded
  # before, return a clone of the existing Executor client (same
  # underlying server object, same spawn bookkeeping).
  #
  # "isolated" policy: always create a fresh Executor server, even for
  # previously-loaded bytes.

  shutdown @1 () -> ();
  # Terminate all tasks spawned through this Runtime.
}

interface Ipfs {
  read @0 (path :Text) -> (stream :ByteStream);
  # Read bytes from an IPFS-family path as a stream via the daemon backend.
  # Accepts `/ipfs/<cid>`, `/ipns/...`, `/ipld/...`.
  # Used by non-WASI clients (e.g. process-local `ww shell` eval) to
  # preserve content-path semantics without direct shell→Kubo coupling.
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
  spawn @0 (args :List(Text), env :List(Text),
            caps :List(MembraneSchema.Export),
            fuelPolicy :FuelPolicy) -> (process :Process);
  # Spawn a new instance of the bound WASM binary with the given
  # args and env.  Late-binding args/env is required for WAGI, which
  # injects per-request CGI env vars (REQUEST_METHOD, PATH_INFO, etc.).
  #
  # caps: optional named capabilities to inject into the child's
  # membrane graft as extras.  Forwarded from init.d `with` blocks
  # via VatListener.listen().
}

interface StreamListener {
  listen @0 (executor :Executor, protocol :Text) -> ();
  # Accept incoming libp2p streams on /ww/0.1.0/stream/{protocol}.
  # For each stream, spawn a cell process via Executor
  # and wire stdin/stdout to the stream.
}

interface HttpListener {
  listen @0 (executor :Executor, prefix :Text,
             caps :List(MembraneSchema.Export)) -> ();
  # Accept HTTP requests matching the path prefix.
  # For each request, spawn a cell process via Executor.
  # CGI env vars are passed as environment, request body to stdin,
  # CGI response read from stdout.
  #
  # caps: optional named capabilities from the init.d `with` block.
  # Forwarded into spawned cells' membranes as graft extras.
  # Empty list (default) = no extra caps.
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

  bootstrap @4 () -> (typed :TypedCap);
  # Return the capability exported by the guest via system::serve() with
  # producer-attached schema metadata required for recursive attenuation.
  # Errors if the guest didn't export a capability.

  kill @5 () -> ();
  # Terminate the process immediately. Fuel is revoked and the cell traps.
}

struct VatHandler {
  union {
    spawn @0 :Executor;
    # Stateless: spawn a fresh cell per connection.
    serve @1 :TypedCap;
    # Stateful: bootstrap all connections with this persistent capability.
  }
}

interface VatListener {
  listen @0 (handler :VatHandler, descriptor :VatDescriptor,
             caps :List(MembraneSchema.Export)) -> ();
  # Accept incoming Cap'n Proto RPC connections on /ww/0.1.0/vat/{cid}
  # where cid = CIDv1(raw, BLAKE3(canonical VatDescriptor)).
  #
  # handler.spawn: for each connection, spawn a cell via the Executor.
  # The cell calls system::serve() to export a bootstrap capability.
  #
  # handler.serve: bootstrap each connection with the provided capability.
  # No cell spawning — one persistent capability serves all connections.
  #
  # Routing identity is descriptor-authoritative; recursive attenuation
  # authority comes from producer-returned TypedCap.schema.
  #
  # caps: optional named capabilities from the init.d `with` block.
  # Forwarded into spawned cells' membranes as graft extras.
  # Empty list (default) = no extra caps.
}

interface VatClient {
  dial @0 (peer :Data, descriptor :VatDescriptor) -> (typed :TypedCap);
  # Open a Cap'n Proto RPC connection to peer on /ww/0.1.0/vat/{cid}
  # where cid = CIDv1(raw, BLAKE3(canonical VatDescriptor)).
  # Bootstraps a Cap'n Proto vat over the stream and returns the remote
  # cell's bootstrap capability.
  #
  # Returns a capability plus schema metadata for recursive attenuation.
}

interface ByteStream {
  read @0 (maxBytes :UInt32) -> (data :Data);
  # Read up to maxBytes from the stream.  Returns empty data at EOF.

  write @1 (data :Data) -> ();
  # Write data to the stream.

  close @2 () -> ();
  # Close the stream.  Further reads return EOF; further writes fail.
}
