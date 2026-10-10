# Accepted-stream lifecycle fixture

Build with `make stream-lifecycle-probe` using the pinned P3 toolchain documented
in `scripts/build_wasip3_component.sh`, then run:

```sh
WW_REQUIRE_P3_FIXTURES=1 cargo test --test stream_listener_integration --locked
```

The compiled child reads stdin through EOF and calls the supplied Membrane.
The host test holds that RPC until it releases the response prefix through
`peerId`. The child appends the input bytes, awaits P3 stdout acceptance and
flush, then calls the Membrane again and waits for permission to exit.

Pending RPC calls provide deterministic gates for delayed output, ignored EOF,
completion, and cancellation. The host observes actual backend cleanup through
a dispatched public `Process.wait()` without retaining Process ownership.
Each test registers the real StreamListener, connects loopback libp2p swarms,
and forwards spawn to the ordinary compiled executor through a MembraneHook
that denies `kill()`. Failure cases must therefore release Process ownership to
terminate execution. No public network or external daemon is involved.

`WW_STREAM_LIFECYCLE_FIXTURE` overrides the artifact path. Host-only lanes report
a missing artifact explicitly; `WW_REQUIRE_P3_FIXTURES=1` makes its absence fail.
