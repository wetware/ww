# Counter -- WAGI Cell

Stateful WAGI cell that counts requests. Proves the `Cell::http`
pipeline end-to-end: host receives HTTP, spawns a cell, pipes
WAGI over stdio, returns the response.

## What it demonstrates

- **WAGI cell** (`WW_CELL_MODE=http`) -- CGI for WASM
- `HttpListener` routes by path prefix
- FastCGI binary protocol over stdin/stdout
- Per-request cell spawning (stateless from the host's view)

## Prerequisites

- Rust toolchain with `wasm32-wasip2` target:
  ```sh
  rustup target add wasm32-wasip2
  ```

## Building

```sh
make counter
```

## Running

### Step 1: Run a node (daemon terminal)

Start a host with HTTP enabled:

```sh
ww run --port=2025 --http-listen 127.0.0.1:2080 std/kernel
```

Leave this process running.

### Step 2: Connect with `ww shell` (shell terminal)

In a second terminal:

```sh
cd examples/counter
ww shell
```

If multiple local nodes are running, use `ww shell --select <index|peer-id>`.

### Step 3: Load the demo snippet to register the route

From the Glia prompt:

```clojure
/ > (load "glia/register.glia")
```

### Step 4: Test with curl

```sh
curl http://127.0.0.1:2080/counter         # GET  -> "0"
curl -X POST http://127.0.0.1:2080/counter # POST -> "1"
```

## How it works

### Cell lifecycle

```
HTTP request arrives at host
        │
        ▼
  ┌─────────────┐
  │ HttpListener │  routes by path prefix -> finds "/counter"
  └──────┬──────┘
         │
         ▼
  ┌─────────────┐
  │  Executor   │  spawns counter.wasm as WASI process
  └──────┬──────┘
         │
         ▼
  ┌─────────────┐     stdin: FastCGI records (BEGIN_REQUEST, PARAMS, STDIN)
  │ Counter Cell│  <──────────────────────────
  │  (WASI P2)  │  ──────────────────────────>
  └─────────────┘     stdout: FastCGI records (STDOUT, END_REQUEST)
         │
         ▼
  Host translates stdout -> HTTP response -> client
```

**Mode A (v1):** One cell per request. Cell starts, handles request,
exits. No connection pooling, no keepalive. Simple, correct, easy
to reason about.

**Mode B (future):** Cell pool with keepalive. Host reuses cells
across requests. Requires a "ready" signal from the cell. Deferred.

### Counter logic

- `GET /counter`  -> responds with the current count (starts at 0)
- `POST /counter` -> increments and responds with the new count
- Any other method -> 405 Method Not Allowed

In Mode A (one cell per request), the counter resets every request
(always returns 0 for GET, 1 for POST). Persistent state requires
either Mode B (keepalive) or external state via IPFS.

### Wire protocol: FastCGI (spec v1)

The cell speaks real FastCGI binary protocol (version 1) over
stdin/stdout. The host acts as the FastCGI client, the guest as
the FastCGI server.

```
stdin  (host -> guest):  FCGI_BEGIN_REQUEST -> FCGI_PARAMS* -> empty FCGI_PARAMS -> FCGI_STDIN* -> empty FCGI_STDIN
stdout (guest -> host):  FCGI_STDOUT (CGI headers + body) -> empty FCGI_STDOUT -> FCGI_END_REQUEST
```

Each record has an 8-byte header: version (1), type, request ID
(big-endian u16), content length (big-endian u16), padding length,
reserved. CGI params (REQUEST_METHOD, REQUEST_URI, etc.) are
encoded as FastCGI name-value pairs with length-prefixed fields
(1-byte length if <128, 4-byte with high bit set otherwise).

The guest responds with a CGI-style response in FCGI_STDOUT:
`Status: 200 OK\r\nContent-Type: text/plain\r\n\r\nbody`

Using real FastCGI means any standard FastCGI library can test or
interact with cells. No bespoke framing to maintain.

**Why not raw HTTP on stdio?** Parsing HTTP/1.1 in the guest adds
a dependency (or a hand-rolled parser). FastCGI is a well-defined
binary protocol that's simpler to parse than HTTP. The host already
has an HTTP parser and translates between HTTP and FastCGI.

**Why not Cap'n Proto RPC?** WAGI cells are intentionally simpler
than capnp cells. They target developers who want to expose a REST
endpoint without learning Cap'n Proto. FastCGI is the simplest
well-specified binary protocol for this job.

## Demo snippet

`glia/register.glia`:

```clojure
; Register the counter cell as an HTTP handler at /counter.
; HttpListener spawns a cell per request and pipes FastCGI.
(def counter (cell (load "bin/counter.wasm")))

(perform host :listen counter "/counter")
```

This snippet defines the counter cell, then registers it with the
host's `HttpListener` under the path prefix `"/counter"`.

`etc/init.d/counter.glia` is now a deployment-only hook. Keep
init-based boot scripts for packaged images, but use snippets as the
default demo flow.

## Tests

Until the host-side handler is fully wired, test the cell directly
by writing FastCGI records to its stdin and reading the response
from stdout:

```rust
// Pseudocode: spawn counter.wasm, send FastCGI request, read response
let executor = runtime.load(counter_wasm).await;
let process = executor.spawn(args, env).await;

// Send FCGI_BEGIN_REQUEST (type=1, role=RESPONDER, flags=0)
process.stdin.write(&fcgi_header(1, 1, 8));  // type=BEGIN, id=1, len=8
process.stdin.write(&[0, 1, 0, 0, 0, 0, 0, 0]);  // role=1(responder), flags=0

// Send FCGI_PARAMS with REQUEST_METHOD=GET
process.stdin.write(&fcgi_params(1, &[("REQUEST_METHOD", "GET")]));
process.stdin.write(&fcgi_header(4, 1, 0));  // empty PARAMS = end

// Send empty FCGI_STDIN
process.stdin.write(&fcgi_header(5, 1, 0));  // empty STDIN = end

// Read FCGI_STDOUT records, parse CGI response
let response = read_fcgi_stdout(&mut process.stdout);
assert!(response.contains("Status: 200 OK"));
assert!(response.contains("0"));
```

## Files

```
examples/counter/
├── Cargo.toml          # standalone WASI P2 crate
├── Makefile            # make counter
├── README.md           # this file
├── bin/                # build output (gitignored)
│   └── counter.wasm
├── glia/
│   └── register.glia   # shell-loaded demo registration
├── etc/
│   └── init.d/
│       └── counter.glia # deployment-only hook
└── src/
    └── lib.rs          # guest implementation
```
