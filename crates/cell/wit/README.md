# WIT Interface Definitions

This directory contains WebAssembly Interface Types (WIT) definitions for custom host-guest interfaces in Wetware.

## Overview

WIT files define the contract between WASM guests and the host runtime. The `bindgen!` macro from `wasmtime` generates Rust types from these WIT definitions, enabling type-safe host-guest communication.

## Current Interfaces

### `streams.wit`

Defines a bidirectional data stream interface for host-guest communication, separate from stdio.

**Package:** `wetware:streams`

**Resources:**
- `connection`: A resource that provides access to input and output stream handles

**Functions:**
- `create-connection`: Creates a new connection resource with bidirectional streams
- `connection.get-input-stream-handle`: Returns a `u32` handle for the input stream
- `connection.get-output-stream-handle`: Returns a `u32` handle for the output stream

**Design Notes:**
- Returns `u32` handles instead of WASI stream types to avoid `bindgen!` resolution issues with external WASI packages
- Guests can use these handles with standard WASI stream APIs (`wasi:io/streams`)
- Streams are backed by in-memory tokio channels on the host side

### `routing-key/wit/key.wit`

The canonical routing-key WIT source lives in the publishable
`crates/guest/routing-key` package. `crates/cell/src/proc.rs` reads the same
file for its host binding.

**Package:** `wetware:routing@0.1.0`

**Interface:** `key`

**Function:** `derive: func(data: list<u8>) -> string`

**World:** `key-client`, which imports `key`

The host registers `key-client` in the common linker path for ordinary and
PID0 components. The import remains optional. A component that does not select
`key-client` has no routing-key binding or component metadata.

`routing_key_runtime` generates the host `Host` trait. `ComponentRunStates`
implements the trait, and `KeyClient::add_to_linker` registers the import.

## How bindgen! Works

### Configuration

The `bindgen!` macro is used in `src/cell/proc.rs`:

```rust
bindgen!({
    world: "streams-world",
    path: "wit",
});
```

- `world`: Matches the `world` name in the WIT file
- `path`: Directory containing WIT files (relative to crate root)

### Generated Types

`bindgen!` generates types under `exports::wetware::streams::streams::`:

- `Connection`: A resource type alias (`ResourceAny`) representing the `connection` resource
- Helper methods: `Connection::try_from_resource()` for converting `Resource<T>` to `Connection`

### Important Limitation for `streams-world`

The `streams-world` binding does not generate a host trait for its exported
interface. Register those functions with `linker.root().func_wrap_async()`.
An import world such as `key-client` generates a `Host` trait and an
`add_to_linker` helper.

## Host Function Implementation

### Function Naming Convention

Host functions use fully qualified names:

- Interface function: `"wetware:streams/streams#create-connection"`
- Resource method: `"wetware:streams/streams#connection.get-input-stream-handle"`

Format: `{package}/{interface}#{function-name}` or `{package}/{interface}#{resource-type}.{method-name}`

### Implementation Pattern

```rust
linker.root().func_wrap_async(
    "wetware:streams/streams#create-connection",
    |mut store: StoreContextMut<'_, ComponentRunStates>, (): ()| {
        Box::new(async move {
            // Implementation
            Ok((connection,))
        })
    },
)?;
```

### Resource Parameter Handling

Resource parameters must be wrapped in a tuple to satisfy `ComponentNamedList`:

```rust
linker.root().func_wrap_async(
    "wetware:streams/streams#connection.get-input-stream-handle",
    |store: StoreContextMut<'_, ComponentRunStates>, 
     (connection,): (Resource<ConnectionState>,)| {
        // connection is Resource<ConnectionState>
        // Access via store.data().resource_table.get(&connection)?
    },
)?;
```

## Resource Management Architecture

### Two-Level Resource Tables

1. **Component ResourceTable** (`store.data().resource_table`):
   - Stores host-defined resources (e.g., `ConnectionState`)
   - Managed via `ResourceTable::push()`, `get()`, `get_mut()`, `delete()`
   - Returns `Resource<T>` handles

2. **WASI ResourceTable** (`wasi_view.table`):
   - Stores WASI stream resources (`InputStream`, `OutputStream`)
   - Managed via `table.push()`
   - Returns `Resource<T>` handles; extract `u32` via `.rep()`

### Connection State Storage

```rust
struct ConnectionState {
    input_stream_handle: u32,   // Handle from WASI table
    output_stream_handle: u32,  // Handle from WASI table
}
```

`ConnectionState` stores `u32` handles to WASI streams, not the streams themselves. The WASI table owns the stream resources.

### Resource Conversion

To return a `Connection` resource to the guest:

```rust
// 1. Create ConnectionState and push to component table
let conn_resource = state.resource_table.push(conn_state)?;

// 2. Convert Resource<ConnectionState> to Connection (ResourceAny)
let connection = Connection::try_from_resource(conn_resource, &mut store)?;

// 3. Return as tuple
Ok((connection,))
```

## Key Implementation Details

### `streams-world` Async Function Wrapping

The manually registered `streams-world` host functions use `func_wrap_async`:

- Closure must return `Box<dyn Future<Output = Result<T>> + Send>`
- Use `Box::new(async move { ... })` for async closures
- Return values must be tuples: `Ok((value,))` for single values

### Store Context Access

- `store.data()`: Immutable access to `ComponentRunStates`
- `store.data_mut()`: Mutable access
- `store.data_mut().ctx()`: Access to `WasiCtxView` for WASI table operations

### Error Handling

- Use `anyhow::Result` for host function errors
- Errors are automatically converted to guest-side error types by wasmtime

## Common Pitfalls and Solutions

### Issue: bindgen! cannot resolve external WASI packages

**Solution:** Return `u32` handles instead of WASI stream types. Guests can use these handles with WASI stream APIs directly.

### Issue: Resource parameters don't implement ComponentNamedList

**Solution:** Wrap resource parameters in tuples: `(connection,): (Resource<ConnectionState>,)`

### Issue: Cannot convert Resource<T> to generated Connection type

**Solution:** Use `Connection::try_from_resource(resource, &mut store)?` provided by bindgen.

## Adding New Interfaces

To add a new WIT interface:

1. **Choose one canonical WIT location.** Put a published guest binding's WIT
   inside that package. Otherwise, create the WIT file in this directory.
2. **Define the interface** with package, interface, and world
3. **Add `bindgen!` macro** in the host code (`crates/cell/src/proc.rs`)
4. **Implement the generated `Host` trait** for an import world, or register
   exported-world functions with `linker.root().func_wrap_async()`
5. **Add the interface to each applicable linker** before instantiation

Example WIT structure:

```wit
package wetware:loader;

interface loader {
    load: func(path: string) -> result<list<u8>, string>;
}

world loader-world {
    export loader;
}
```

Then implement in Rust:

```rust
bindgen!({
    world: "loader-world",
    path: "wit",
});

// Implement host function
linker.root().func_wrap_async(
    "wetware:loader/loader#load",
    |mut store: StoreContextMut<'_, ComponentRunStates>, (path,): (String,)| {
        Box::new(async move {
            // Implementation
            Ok((data,))
        })
    },
)?;
```

## References

- [WIT Specification](https://github.com/WebAssembly/component-model/blob/main/design/mvp/WIT.md)
- [wasmtime Component Model](https://docs.rs/wasmtime/latest/wasmtime/component/index.html)
- [wasmtime bindgen! macro](https://docs.rs/wasmtime/latest/wasmtime/component/macro.bindgen.html)

## Summary

- **WIT defines the interface contract** between host and guest
- **`bindgen!` generates guest-side types** and resource conversion helpers
- **Import worlds generate host traits**; exported `streams-world` functions use manual linker registration
- **Resources are managed via two tables**: component table for host resources, WASI table for WASI streams
- **Function names must be fully qualified**: `{package}/{interface}#{name}`
- **Resource parameters must be wrapped in tuples** to satisfy trait bounds

This approach provides type-safe, async-capable bidirectional communication between host and WASM guests while maintaining clear separation of concerns between component resources and WASI resources.
