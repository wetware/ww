# HTTP Capability Surface Design

## Status: WAGI listener and outbound HTTP implemented

**Supersedes** the original raw-HTTP/stdin model and long-lived cell default.
The original phase plan is historical. Current contracts are documented in the
[guest API](../api/wasm-guest.md) and [CLI reference](../cli.md#ww-run).

## Overview

HTTP interop for wetware using WAGI (WebAssembly Gateway Interface), CGI
for WASM. The host parses HTTP, injects request metadata as environment
variables (RFC 3875), pipes the request body to stdin, and reads a
CGI-formatted response from stdout. Fresh cell per request. Stateless.
This is infrastructure interop, not Wetware's primary stateful service
model.

All HTTP capabilities are epoch-scoped. Cells have no ambient node-control
authority; their fixed WASI substrate is separate from HTTP capability grants.

## The Spec

**There is only WAGI. If 5ms startup is too slow, use WebSockets.**

### Lifecycle boundary

WAGI remains a per-request adapter. Do not add FastCGI, ASGI, session
affinity, or a second stateful HTTP runtime to make WAGI behave like a
server process. If an HTTP-facing workload needs a long-lived client
session, use the stream/WebSocket path. If a Wetware-native workload
needs a long-lived service identity, publish a capability with vat RPC.

### WAGI (request/response)

Standard CGI over WASI. Host parses HTTP, injects metadata as env vars
and argv, pipes body to stdin. Fresh cell per request. Stateless.

- Environment variables = CGI headers (RFC 3875):
  - `REQUEST_METHOD`, `PATH_INFO`, `QUERY_STRING`
  - `CONTENT_TYPE`, `CONTENT_LENGTH`
  - `HTTP_HOST`, `HTTP_ACCEPT`, `HTTP_*` (one per header)
  - `SERVER_NAME`, `SERVER_PORT`, `SERVER_PROTOCOL`
  - `GATEWAY_INTERFACE=CGI/1.1`
- `stdin` = request body only
- `stdout` = CGI response (`Status: 200 OK\r\nContent-Type: text/plain\r\n\r\nHello`)

Guest code is a finite CLI program using the `wagi-guest` crate:

```rust
use wagi_guest as wagi;

struct Counter;

impl wagi::AsyncGuest for Counter {
  async fn run() -> Result<(), ()> {
    let ct = ("Content-Type", "text/plain");
    let (status, body) = match wagi::method().as_str() {
        "GET"  => (200, "0"),
        "POST" => (200, "1"),
        _      => (405, "Method Not Allowed"),
    };
    wagi::respond_bytes_async(status, &[ct], body.as_bytes())
      .await
      .map_err(|_| ())
  }
}

wagi::export_async!(Counter);
```

That's ~8 lines. Down from the 306-line FastCGI implementation.

### WebSocket (proposed persistent bidirectional stream)

The following upgrade flow remains a proposal:

- `HTTP_UPGRADE=websocket` and `HTTP_CONNECTION=Upgrade` in env vars
- Cell writes `101 Switching Protocols` + WebSocket accept headers to stdout
- After the 101 response, stdin/stdout become a bidirectional WebSocket
  frame stream
- Cell stays alive for the connection lifetime
- Host handles WebSocket framing (masking, fragmentation)

Statefulness is client-driven (hold the connection open), not a
server-side mode flag.

## Why WAGI, Not FastCGI

FastCGI was designed to solve fork-per-request cost and connection
multiplexing. Neither applies to WASM sandboxes (~5ms instantiation,
isolated stdio per cell). The counter example was 306 lines of binary
protocol parsing. Nobody in the WASM ecosystem uses FastCGI. WAGI,
WCGI, and Spin all chose simpler models.

WAGI makes the guest a boring local process. No framework, no binary
protocol, no custom exports. Every language already knows how to read
env vars and print to stdout.

## Architecture

### Host-side: WAGI functions (`crates/rpc/src/wagi.rs`)

The HTTP listener calls standalone CGI environment and response functions.

- `build_cgi_env(method, path, query, headers, server_name, server_port)`
  constructs RFC 3875 env vars
- `parse_cgi_response(stdout)` parses CGI output into status + headers + body

### Guest-side: wagi-guest crate (`crates/wagi-guest/`)

Thin wrapper (~100 lines, zero deps beyond std):

- `wagi::method()`, `wagi::path()`, `wagi::query()`, `wagi::header(name)`
- `wagi::body()`, `wagi::body_string()`
- `wagi::respond_bytes_async(status, headers, body)` for finite P3 handlers
- `wagi::respond` and `wagi::respond_bytes` as unacknowledged compatibility helpers

`respond_bytes_async` builds one contiguous CGI response. Success means that P3
stdout accepted all bytes and the host writer flush completed. The boundary
does not cover network or HTTP client consumption. A failure can leave an
accepted prefix in stdout.

### Spawn Path

`Executor.spawn` supplies data streams and the explicit parent-selected
`Membrane` through the process-local P3 RPC transport. The HTTP listener passes
its registration-time `Membrane` unchanged. Stdio semantics vary by cell type:

- **Raw cells:** stdin/stdout carry wire protocol bytes.
- **HTTP/WAGI cells:** stdin/stdout carry CGI request/response.
- **Cap'n Proto cells:** stdio is application-defined; RPC uses the separate
  `wetware:transport` connection.

Stdin EOF is not a universal shutdown request. `Process` references own ordinary
child execution. Final `Process` owner loss or `Process.kill()` requests
termination; the backend performs asynchronous teardown. `Process.wait()`
observes cleanup completion without retaining execution ownership. Stdio and
bootstrap capabilities alone do not own execution. See
[Process lifecycle](../api/wasm-guest.md#process).

### Per-request Flow

```
Before requests: register prefix, Executor, and Membrane with HttpListener
    |
HTTP request arrives through the axum WagiService
    |
    +-- 1. Dispatch to the current valid prefix registration
    |
    +-- 2. build_cgi_env("GET", "/counter", "", headers, ...)
    |       -> ["REQUEST_METHOD=GET", "PATH_INFO=/counter", ...]
    |
    +-- 3. executor.spawn(args, env=cgi_env, membrane=registration_membrane)
    |       -> Creates duplex pipes (stdin/stdout/stderr)
    |       -> Establishes P3 capability transport with that Membrane
    |       -> Schedules the child lifecycle on the executor worker
    |       -> Returns Process client
    |
    +-- 4. tokio::time::timeout(30s, run_spawned_process)
    |       |
    |       +-- Write body to stdin, close stdin, read stdout, wait for cleanup
    |       +-- Success: parse_cgi_response(stdout) -> HTTP response
    |       +-- Timeout: request best-effort Process.kill() -> 504 Gateway Timeout
    |
    +-- 5. Return response; release the request's Process reference
```

### Error-to-HTTP Status Mapping

| Condition | HTTP status |
|---|---|
| Valid CGI response (any exit code) | Status from CGI output |
| No Status line in stdout | 200 OK (CGI default) |
| Empty stdout | 502 Bad Gateway |
| Malformed headers | 502 Bad Gateway |
| WASM trap (OOM, stack overflow) | 502 Bad Gateway |
| Stdin write failure | 502 Bad Gateway |
| Process.kill() triggered (timeout) | 504 Gateway Timeout |

**Stdout is authoritative:** if `parse_cgi_response()` finds a valid
response, it's returned regardless of exit code. Exit code is logged
for observability only. Intentional deviation from CGI spec.

### Cell Isolation

| Threat | Mitigation | Status |
|--------|-----------|--------|
| Guest escapes sandbox | wasmtime memory isolation | Built |
| WASM fault (trap, OOB) | `Result`, no panic | Built |
| Host-side glue panics | `tokio::spawn` per cell | Built |
| Cell floods stdout | `MAX_RESPONSE_BYTES` (16 MiB) | Built |
| Cell eats all CPU | EWMA fuel scheduler (wasmtime fuel + call-hook) | Built |
| Cell eats all memory | wasmtime `StoreLimits` | TODO |
| Spawned cell hangs | request I/O and wait timeout (30s) | Built |
| Concurrent request flood | max cells per prefix | TODO (Phase 2) |

## Cap'n Proto Schema

No schema change needed. HTTP listener registration receives the path prefix
explicitly; WASM custom sections do not select HTTP routing.

### Outgoing HTTP (HttpClient)

```
WASI Guest --Cap'n Proto RPC--> EpochGuardedHttpProxy (reqwest) --> Internet
```

- `--http-dial <HOST>` enables an epoch-guarded `HttpClient` with a host allowlist.
- The proxy checks `EpochGuard` and the destination host before sending a request.
  Redirects must also pass the host allowlist.
- The root `Membrane` exposes the client as `network.http.dialer`; without
  `--http-dial`, that field is null. A parent can omit it from a child `Membrane`.

### Membrane Integration

`--http-listen <ADDR>` enables the WAGI server and root
`network.http.listener`. A guest registers a prefix with an `Executor` and an
explicit child `Membrane`. Each request uses that same `Membrane`.
The dispatch task owns the route registration. Epoch or registration-scope
expiry removes that registration; a stale registration cannot remove its
replacement.

## Implementation Status

Implemented: CGI environment/response handling, awaited finite P3 output,
explicit spawn Membranes, Process termination and cleanup observation,
request I/O/wait timeout, axum route dispatch, and scoped outbound HTTP.

The earlier phase checklist is superseded. It does not define current CLI
commands or a second spawn path. WebSocket upgrades and additional language
examples remain proposals.

## Prior Art

- **WAGI** (Deislabs): CGI for WASM, same model we adopted
- **Spin** (Fermyon): similar CGI-inspired model with custom trigger system
- **WCGI** (Wasmer): CGI variant for WebAssembly
- **Sandstorm**: C++ HTTP proxy forwards to grain sandbox
- **BEAM/Cowboy**: Process-per-request via Ranch acceptor pool
- **CGI (RFC 3875)**: The original. Env vars + stdin + stdout.
