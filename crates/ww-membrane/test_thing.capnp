@0xe5c0d1a9b7f34602;

# Toy interface for ww-membrane's own integration tests. `forbidden` is the
# method the tests deny; `child` returns a capability, exercising recursive
# rewrap. Not part of the public ABI — test fixture only.
interface Thing {
  ping      @0 () -> (msg :Text);
  forbidden @1 () -> (msg :Text);
  child     @2 () -> (thing :Thing);
}
