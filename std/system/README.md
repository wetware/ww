# system — Guest Runtime SDK

The SDK for WASM agents running inside the wetware host environment.

## What it is

When a WASM agent is executed by `ww`, it runs inside a sandbox that communicates
with the host over a WASI stream pair. This crate abstracts that connection into a
Cap'n Proto RPC session, letting guest code call host capabilities using ordinary
`async/await`.

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

The trusted pid0 kernel is the exception: it receives a process-local root
`Membrane`, whose graft includes a distinct ordinary `Membrane` for publication.
PID0 uses `serve()` to export that ordinary membrane policy surface. Its private
`kernel_ready()` host import is separate from `system`, absent from ordinary-cell
linkers, and cannot be re-exported as a capability. Ordinary agents receive only
`InitialGrants` and use `run()` unless they also export a capability.
