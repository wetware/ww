# Greeter capability — minimal RPC interface for discovery demo.
#
# One method. The demo focuses on service-name discovery plus typed vat RPC.

@0xa9134eb34ed79666;

interface Greeter {
  greet @0 (name :Text) -> (greeting :Text);
}
