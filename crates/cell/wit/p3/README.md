# Production P3 host WIT

`transport.wit` is the production contract for one authority-granted ordered
bidirectional byte connection. `fixture.wit` adds test-only exports for the
native P3 regression component.

Do not add address selection, sockets, pollables, scheduler state, or an
explicit flush verb to the transport interface. The Rust host adapter owns
bounded buffering, backpressure, flush-gated completion, half-close, and
sanitized failure behavior.

Production Cells import this interface through `std/system`. `Proc` places one
`GrantedTransport` in each Store and returns the matching `HostTransport` to
the host-side Cap'n Proto driver.
