# HTTP Capability Surface Design

## Status: Phase 1 complete (WAGI adapter + lightweight spawn)

**Supersedes** the original raw-HTTP/stdin model and long-lived cell default.
See `~/.gstack/projects/wetware-ww/ceo-plans/2026-03-31-wagi-http-cells.md`
for the full Phase 1 plan with CEO review.

## Overview

HTTP interop for wetware using WAGI (WebAssembly Gateway Interface), CGI
for WASM. The host parses HTTP, injects request metadata as environment
variables (RFC 3875), pipes the request body to stdin, and reads a
CGI-formatted response from stdout. Fresh cell per request. Stateless.

All HTTP capabilities are epoch-scoped and follow wetware's
zero-ambient-authority model.

## The Spec

**There is only WAGI. If 5ms startup is too slow, use WebSockets.**

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

Guest code is a boring CLI program using the `wagi-guest` crate:

```rust
use wagi_guest as wagi;

fn handle() {
    let ct = ("Content-Type", "text/plain");
    match wagi::method().as_str() {
        "GET"  => wagi::respond(200, &[ct], "0"),
        "POST" => wagi::respond(200, &[ct], "1"),
        _      => wagi::respond(405, &[ct], "Method Not Allowed"),
    }
}
```

That's ~8 lines. Down from the 306-line FastCGI implementation.

### WebSocket (persistent bidirectional stream, Phase 2)

The upgrade request arrives as a normal WAGI invocation:

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

### Host-side: WagiAdapter (`src/dispatcher/wagi.rs`)

Standalone functions, NOT a ProtocolAdapter impl (ProtocolAdapter's
`request_body()` returns only `Vec<u8>` and WAGI needs env vars too).

- `build_cgi_env(method, path, query, headers, server_name, server_port)`
  constructs RFC 3875 env vars
- `parse_cgi_response(stdout)` parses CGI output into status + headers + body

### Guest-side: wagi-guest crate (`crates/wagi-guest/`)

Thin wrapper (~100 lines, zero deps beyond std):

- `wagi::method()`, `wagi::path()`, `wagi::query()`, `wagi::header(name)`
- `wagi::body()`, `wagi::body_string()`
- `wagi::respond(status, headers, body)`

### Spawn Path

All cell types get `with_data_streams()` + membrane RPC. The WIT
membrane channel is universal. stdin/stdout semantics vary by cell type:

- **Raw cells:** stdin/stdout carry wire protocol bytes.
- **HTTP/WAGI cells:** stdin/stdout carry CGI request/response.
- **Cap'n Proto cells:** stdin close = graceful shutdown signal,
  stdout unused. All I/O goes through the WIT side-channel.

### Per-request Flow

```
HTTP request arrives (Phase 2: axum, Phase 1: ww test http)
    |
    +-- 1. build_cgi_env("GET", "/counter", "", headers, ...)
    |       -> ["REQUEST_METHOD=GET", "PATH_INFO=/counter", ...]
    |
    +-- 2. runtime.load(wasm_bytes) -> Executor
    |       (Arc<bytecode> clone, cached by BLAKE3 hash)
    |
    +-- 3. executor.spawn(args, env=cgi_env) [lightweight path]
    |       -> Creates duplex pipes (stdin/stdout/stderr)
    |       -> Skips membrane, skips RPC system
    |       -> tokio::task::spawn_local(proc.run())
    |       -> Returns Process client
    |
    +-- 4. Write request body to stdin, close stdin
    |
    +-- 5. tokio::time::timeout(30s, read_stdout + wait)
    |       |
    |       +-- Success: parse_cgi_response(stdout) -> HTTP response
    |       +-- Timeout: Process.kill() -> 504 Gateway Timeout
    |
    +-- 6. Return response
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
| Cell hangs forever | per-request timeout (30s) | TODO |
| Concurrent request flood | max cells per prefix | TODO (Phase 2) |

## Cap'n Proto Schema

No schema change needed. HTTP listener registration receives the path prefix
explicitly; WASM custom sections do not select HTTP routing.

### Outgoing HTTP (HttpClient, deferred)

```
WASI Guest --Cap'n Proto RPC--> EpochGuardedHttpClient (reqwest) --> Internet
```

- HttpClientBuilder.build() returns an epoch-guarded HttpClient
- Every RPC method calls EpochGuard::check()
- Membrane attenuation: httpClient can be withheld from remote peers

### Membrane Integration (Phase 2)

HttpClientBuilder added to graft() return. HttpServer null without
`--with-http`.

## Implementation Status

### Phase 1: WAGI Adapter + Lightweight Spawn (done)
- [x] WagiAdapter: `build_cgi_env()`, `parse_cgi_response()`, 16 unit tests
- [x] wagi-guest crate: env var helpers + CGI response formatting
- [x] Counter example rewritten as WAGI cell (306 -> 32 lines)
- [x] Lightweight spawn path in `ExecutorImpl::spawn()`
- [x] EWMA fuel scheduler (call-hook refueling at host boundaries, 10 unit tests)
- [x] Process.kill() via watch channel + tokio::select!
- [ ] Per-request timeout (30s)
- [ ] `ww test http` CLI subcommand
- [ ] `ww new http` CLI scaffolding
- [ ] Three-point benchmark
- [ ] Quickstart doc

### Phase 2: Production HTTP Server
- `--with-http host:port` flag, axum router, route table
- WebSocket upgrade support
- HttpClient capability (outgoing HTTP via reqwest)

### Phase 3: Multi-Language
- Go (TinyGo), Python (componentize-py) WAGI examples

## Prior Art

- **WAGI** (Deislabs): CGI for WASM, same model we adopted
- **Spin** (Fermyon): similar CGI-inspired model with custom trigger system
- **WCGI** (Wasmer): CGI variant for WebAssembly
- **Sandstorm**: C++ HTTP proxy forwards to grain sandbox
- **BEAM/Cowboy**: Process-per-request via Ranch acceptor pool
- **CGI (RFC 3875)**: The original. Env vars + stdin + stdout.
