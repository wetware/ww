# system — Guest Runtime SDK

The SDK for WASM agents running inside the wetware host environment.

## What it is

When `ww` executes a WASM agent, the agent receives a capability-granted
`wetware:transport/connection@0.2.0` resource. This crate converts that P3
connection into a Cap'n Proto RPC session for ordinary `async/await` code.

## Entry points

```rust
// Receive the exact Membrane selected by the parent.
system::run(|membrane: system_capnp::membrane::Client| async move {
    let graft = membrane.graft_request().send().promise.await?;
    // ...
    Ok(())
});

// Receive a Membrane AND export `my_capability` back to the parent.
// Use this when the agent needs to surface a capability to external peers.
system::serve(my_capability, |membrane: system_capnp::membrane::Client| async move {
    // ...
    Ok(())
});
```

`run()` is suitable for agents that consume capabilities but don't export any.
`serve()` is the pattern for agents that export a guest capability. The
parent-held `Process.bootstrap()` retrieves that export; it is distinct from
the host-provided `Membrane` received by the child.

Each selector observation checks transport completion, RPC completion, then the
application. The RPC driver stays inside the session future. A private cleanup
owner disconnects the connection synchronously when the session completes,
unwinds, or is dropped. Cleanup does not poll the RPC driver.

Cleanup terminalizes connection-owned calls and releases exports before clearing
the bootstrap and dropping the application and RPC futures. Retained imports,
responses, pipelines, and promised capabilities become terminal. A selected
error or application unwind remains primary if cleanup also fails. Successful
application or RPC completion still waits for transport completion after local
cleanup.

`RpcSession` also owns cleanup when used directly. Its public `rpc_system` and
`client` fields can be extracted, but the remaining session owner still controls
the connection lifetime. When that owner leaves scope, the extracted values
cannot keep the connection usable. Field extraction does not provide detach.

Actual P3 task cancellation runs this cleanup while the Store remains usable.
Discarding a Store or trapping with Wasm `panic=abort` does not promise guest
destructor execution. The host child lifecycle retains responsibility for
host-side cleanup at those boundaries.

## Relationship to the kernel

The trusted PID0 kernel receives a process-local root `Membrane`. PID0 calls
`system::run()` and exports no guest bootstrap capability
after initialization. Its private `kernel_ready()` host import is separate from
`system` and absent from ordinary-Cell linkers. Ordinary agents receive a
parent-selected `Membrane` and use `run()` unless they also export a capability.
