# system — Guest Runtime SDK

The SDK for WASM agents running inside the wetware host environment.

## What it is

When `ww` executes a WASM agent, the agent receives a capability-granted
`wetware:transport/connection@0.2.0` resource. This crate converts that P3
connection into a Cap'n Proto RPC session for ordinary `async/await` code.

## Entry points

```rust
// Ordinary child: receive exactly the immutable parent-selected grants.
system::run(|grants: InitialGrants| async move {
    let initial = grants.get_request().send().promise.await?;
    // ...
    Ok(())
});

// Receive initial grants AND export `my_capability` back to the parent.
// Use this when the agent needs to surface a capability to external peers.
system::serve(my_capability, |grants: InitialGrants| async move {
    // ...
    Ok(())
});
```

`run()` is suitable for agents that consume capabilities but don't export any.
`serve()` is the pattern for agents that export a guest capability. The
parent-held `Process.bootstrap()` retrieves that export; it is distinct from
the host-provided `InitialGrants` received by the child.

## Relationship to the kernel

The trusted PID0 kernel receives a process-local root `Membrane` in its initial
grants. PID0 calls `system::run()` and exports no guest bootstrap capability
after initialization. Its private `kernel_ready()` host import is separate from
`system` and absent from ordinary-Cell linkers. Ordinary agents receive only
`InitialGrants` and use `run()` unless they also export a capability.
