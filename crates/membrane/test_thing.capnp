@0xe5c0d1a9b7f34602;

# Toy interface for wetware-membrane's own integration tests. `forbidden` is the
# method the tests deny; `child` returns a capability, exercising recursive
# rewrap. Not part of the public ABI — test fixture only.
interface Thing {
  ping      @0 () -> (msg :Text);
  forbidden @1 () -> (msg :Text);
  child     @2 () -> (thing :Thing);
  # Takes a capability as a parameter and calls `forbidden` on it, echoing the
  # result. Exercises reverse-direction (param) handling: a membrane of ours
  # passed back in should be unwrapped so the backend sees the bare cap.
  echo      @3 (thing :Thing) -> (msg :Text);
}

# Feasibility fixtures for typed method capture. These are intentionally
# test-only: they exercise the two generated request shapes and prove that an
# inherited method retains the interface ID of the interface that declared it.
interface StreamThing {
  notify @0 (msg :Text) -> stream;
}

interface BaseThing {
  base @0 ();
}

interface DerivedThing extends(BaseThing) {
  derived @0 ();
}
