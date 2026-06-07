# Greeter capability — minimal RPC interface for discovery demo.
#
# One method. The point is named vat discovery, not the service.

@0xa9134eb34ed79666;

interface Greeter {
  greet @0 (name :Text) -> (greeting :Text);
}
