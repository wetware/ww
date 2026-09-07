# Dormant P3 host WIT

`transport.wit` is the production contract for one authority-granted ordered
bidirectional byte connection. `fixture.wit` adds test-only exports for the
native P3 regression component.

Do not add address selection, sockets, pollables, scheduler state, or an
explicit flush verb to the transport interface. The Rust host adapter owns
bounded buffering, backpressure, flush-gated completion, half-close, and
sanitized failure behavior.

Current production Cells still import `wetware:streams@0.1.0` through WASI P2.
The P3 package remains dormant until the atomic guest cutover.
