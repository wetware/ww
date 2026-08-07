# Capability export transport plus trusted-root and ordinary-child bootstraps.
#
# Split from stem.capnp to separate capability transport metadata from
# auth/session and epoch/provenance concerns.

@0xa4f0c87b5de91236;

struct Export @0xbb8d5590cb2f3d2e {
  name @0 :Text;
  cap  @1 :Capability;
  # An exported capability bound to a local name. Authority is carried by the
  # capability reference itself; the name is a local binding key, not authority.
}

interface InitialGrants @0xae9edc968ee787fe {
  get @0 () -> (
    caps :List(Export)
  );
  # Closed, idempotent delivery of an ordinary child's immutable initial
  # grants. The server returns exactly the parent-delegated Export list and
  # exposes no graft, lookup, refresh, append, policy, or parent-channel API.
}

interface GenerationActivator @0x94945a0fc6d68018 {
  activate @0 () -> ();
  # Commits the single pid0 generation captured when this capability was
  # minted. The caller supplies no epoch; stale generations fail closed.
}

interface Membrane @0xdb52c25106bc2c5e {
  graft @0 () -> (
    caps :List(Export)
  );
  # Pure capability provisioning (ocap model). Having a Membrane reference IS
  # authorization — no signer needed. Wrap in Terminal(Membrane) to gate access.
  #
  # Canonical names: "identity", "host", "runtime", "routing", "http-client", "ipfs".
  # Trusted pid0 may also receive explicitly configured extras.
  #
  # Listener/Dialer accessed via host.network().
  # WASI guests resolve content via the virtual filesystem (CidTree).
  # Non-WASI clients (for example process-local `ww shell`) may also receive
  # the `ipfs` cap and call `system.Ipfs.read` for `/ipfs`/`/ipns`/`/ipld`.

  graftPid0 @1 () -> (
    caps :List(Export),
    activator :GenerationActivator
  );
  # Trusted pid0-only provisioning. Each call returns a fresh activator bound
  # immutably to the same authoritative epoch as the returned capabilities.
  # Non-pid0 membrane servers intentionally leave this method unimplemented.
}
