# Membrane transport schema: exported capabilities + membrane graft contract.
#
# Split from stem.capnp to separate capability transport metadata from
# auth/session and epoch/provenance concerns.

@0xa4f0c87b5de91236;

using Schema = import "/capnp/schema.capnp";

struct Export @0xbb8d5590cb2f3d2e {
  name   @0 :Text;
  cap    @1 :Capability;
  schema @2 :Schema.Node;
  # An exported capability with its schema for runtime introspection.
}

interface Membrane @0xdb52c25106bc2c5e {
  graft @0 () -> (
    caps :List(Export)
  );
  # Pure capability provisioning (ocap model). Having a Membrane reference IS
  # authorization — no signer needed. Wrap in Terminal(Membrane) to gate access.
  #
  # Canonical names: "identity", "host", "runtime", "routing", "http-client", "ipfs".
  # Init.d-scoped grants (from `with` blocks) are appended after the core caps.
  #
  # Listener/Dialer accessed via host.network().
  # WASI guests resolve content via the virtual filesystem (CidTree).
  # Non-WASI clients (for example process-local `ww shell`) may also receive
  # the `ipfs` cap and call `system.Ipfs.read` for `/ipfs`/`/ipns`/`/ipld`.
}
