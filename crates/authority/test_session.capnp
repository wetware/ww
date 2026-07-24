@0xc91f5a50be8877d2;

# Test-only structured session fixture for async AuthPolicy feasibility.
interface Leaf {
  read  @0 () -> (value :Text);
  write @1 (value :Text);
}

interface StructuredSession {
  capabilities @0 () -> (
    first :Leaf,
    second :Leaf
  );
}
