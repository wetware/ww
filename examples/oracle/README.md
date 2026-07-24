# Oracle -- Gas Price Feed

Decentralized gas price oracle over named vat RPC and HTTP.
Fetches live gas prices from Blocknative via `HttpClient`, serves
them over typed Cap'n Proto RPC **and** HTTP/JSON, and advertises
on the DHT for peer discovery.

## What it demonstrates

- **Dual transport** -- one binary, two transports (vat RPC + HTTP)
- **Cap'n Proto cell** (`WW_CELL_MODE=vat`) -- named vat RPC
- **WAGI cell** (`WW_CELL_MODE=http`) -- CGI, curl-friendly JSON
- `HttpClient` capability for outbound HTTP (domain-scoped)
- `with`/`cell`/`listen` DX for capability-scoped cell definitions
- Service-name DHT discovery via `routing.provide()` / `findProviders()`
- Four-mode binary: vat cell, WAGI cell, service (DHT), consumer (query)

## Prerequisites

- Rust toolchain with `wasm32-wasip2` target:
  ```sh
  rustup target add wasm32-wasip2
  ```
- A running Kubo node for DHT bootstrap:
  ```sh
  ipfs daemon
  ```

## Building

```sh
make oracle
```

This compiles the WASM guest and generates build-time schema-byte and CID
metadata for introspection.
Vat publication uses the service name `oracle`.

## Running

### Step 1: Run the oracle node (daemon terminal)

Start a host with HTTP enabled:

```sh
ww run --http-listen 127.0.0.1:2080 --listen /ip4/127.0.0.1/tcp/2025 --with-http-admin off std/kernel
```

Leave this process running.

### Step 2: Connect with `ww shell` (provider shell)

In a second terminal:

```sh
cd examples/oracle
ww shell
```

If multiple local nodes are running, use `ww shell --select <index|peer-id>`.

### Step 3: Load snippets to register and serve

From the Glia prompt:

```clojure
/ > (perform :load "glia/register.glia")
/ > (perform :load "glia/serve.glia")
```

### Step 4: Query via curl

```sh
# All pairs
curl http://127.0.0.1:2080/oracle

# Single pair
curl 'http://127.0.0.1:2080/oracle?pair=ETH%2Fgas'
```

Example response:

```json
{
  "pairs": {
    "ETH/gas": {
      "price": 30.12,
      "unit": "gwei",
      "confidence": 0.99,
      "timestamp": 1700000000
    },
    "POLYGON/gas": { ... },
    "BASE/gas": { ... }
  }
}
```

### Step 5: Query from a consumer (optional)

Open a second terminal and boot a consumer node:

```sh
# Terminal 3 -- consumer daemon
ww run --listen /ip4/127.0.0.1/tcp/2026 --with-http-admin off std/kernel
```

From another terminal, connect to that node and run consume mode:

```sh
# Terminal 4 -- consumer shell
cd examples/oracle
ww shell
```

If prompted, select the host for the `--port=2026` node.

Then in Glia:

```clojure
/ > (perform :load "glia/register.glia")
/ > (perform :load "glia/consume.glia")
```

The consumer discovers the oracle provider via DHT, dials it with
`VatClient`, and queries gas prices:

```
[INFO] consumer: peer ..a1b2c3d4
[INFO] consumer: looking for oracle providers...
[INFO] consumer: found 1 oracle provider(s)
[INFO] ..e5f6g7h8: ETH/gas = 30.12 gwei (confidence 99%)
```

## How it works

### Architecture

```
ORACLE NODE:                            CURL CLIENT:
  glia/register.glia publishes vat      curl http://localhost:2080/oracle
  service + HTTP/WAGI adapter              |
                                           v
  HttpListener accepts     <---HTTP---  axum server
  spawns WAGI cell per request          routes by prefix
    membrane.graft()                       |
    http_client.get(blocknative)           v
    build JSON response                 CGI response -> JSON
    write to stdout

                                        CONSUMER NODE:
  VatListener serves raw   <--libp2p--  vat_client.dial(oracle, "oracle")
  persistent PriceOracle cap            bootstrap --> PriceOracle cap
                                        oracle.get_pairs() -> ["ETH/gas", ...]
                                        oracle.get_price("ETH/gas")
                                           |
                            --RPC-->    display prices
    refresh loop (30s)
```

### Four execution modes

The same binary serves all modes. Detection:

| Mode | Trigger | Transport |
|------|---------|-----------|
| **Vat cell** | No args, no `REQUEST_METHOD` | Cap'n Proto RPC over libp2p |
| **WAGI cell** | `REQUEST_METHOD` env var present | CGI over stdin/stdout |
| **Service** | `serve` subcommand | DHT provide loop |
| **Consumer** | `consume` subcommand | DHT discover + RPC query |

- **Vat cell mode** (no args): spawned by Glia before publication.
  Creates a `PriceOracleImpl`, grafts to obtain `HttpClient`, fetches
  prices from Blocknative, and exports the oracle as the bootstrap
  capability. This compatibility demo uses `host :serve-raw-vat` to publish
  that capability under `oracle` without recipient authentication.
- **WAGI cell mode** (`WW_CELL_MODE=http`): spawned by `HttpListener`
  per HTTP request. Grafts the membrane over `wetware:streams`
  (side-channel), fetches prices via `HttpClient`, writes a JSON
  response to stdout via CGI. Stateless -- one cell per request.
- **Service mode**: long-running DHT provider loop. Provides the
  service-name routing key on the DHT and re-provides periodically
  (records expire).
- **Consumer mode**: discovers oracle providers via DHT, dials them
  with `VatClient`, queries prices. Exponential backoff (2 s to 60 s).

### Transport lifecycle boundary

The example intentionally keeps HTTP boring. The vat capability is the
long-lived service object: it owns in-process cache, RPC ordering, and
service identity. `host :serve-raw-vat` publishes that already-exported
capability under a service name without recipient authentication; it does not
install a per-request handler. Production recipient-gated services should use
`host :serve-vat ... :auth policy`.

The HTTP/WAGI path is a per-request adapter. `HttpListener` spawns a
fresh CGI/WAGI cell for each matching request, so HTTP should not be
treated as the primary stateful service runtime. For browser-facing
long-lived sessions, use the stream/WebSocket path. For Wetware-native
stateful services, use vat RPC.

### Schema

```capnp
interface PriceOracle {
  getPrice @0 (pair :Text) -> (price :Int64, decimals :UInt8,
                                timestamp :Int64, confidence :Float64);
  getPairs @1 () -> (pairs :List(Text));
}
```

Supported pairs: `ETH/gas`, `POLYGON/gas`, `BASE/gas`.

### Price fetching

The cell uses the `HttpClient` capability (obtained via
`membrane.graft()`) to call the Blocknative gas price API.
`HttpClient` is domain-scoped -- the host controls which domains
the cell can reach. Prices are cached in-process and served via
RPC. Confidence decays toward 0.0 if data goes stale.

## Demo snippets

`glia/register.glia`:

```clojure
(def oracle-wasm (perform :load "bin/oracle.wasm"))
(def oracle-http (cell oracle-wasm))

(def oracle-executor (perform runtime :load oracle-wasm))
(def oracle-process (perform oracle-executor :spawn))
(def oracle-cap (perform oracle-process :bootstrap))

(perform host :serve-raw-vat oracle-cap "oracle")
(perform host :listen oracle-http "/oracle")
```

`glia/serve.glia`:

```clojure
(perform runtime :run (perform :load "bin/oracle.wasm") "serve")
```

`glia/consume.glia`:

```clojure
(perform runtime :run (perform :load "bin/oracle.wasm") "consume")
```

The host registration forms split by transport:
- `:serve-vat ... :auth policy` publishes through a per-stream Terminal
- `:serve-raw-vat` explicitly publishes an ungated capability over named vat RPC
- Cell + path -> HttpListener, which runs WAGI at the prefix per request

The same binary handles both transports. It detects HTTP mode via
the `REQUEST_METHOD` CGI env var (injected by HttpListener).

`etc/init.d/oracle.glia` is now a deployment-only hook. Keep
init-based boot scripts for packaged images, but use snippets as the
default demo flow.

## Tests

```sh
cargo test --manifest-path examples/oracle/Cargo.toml
```

Runs unit tests for cache initialization, JSON parsing, JSON
response building, and RPC round-trip tests over in-memory Cap'n
Proto duplex (get_price, get_pairs, unknown pair error).

## Files

```
examples/oracle/
├── Cargo.toml
├── Makefile              # make oracle
├── README.md             # this file
├── oracle.capnp          # PriceOracle schema source
├── bin/                  # build output (gitignored)
│   ├── oracle.wasm
│   └── oracle.wasm
├── glia/
│   ├── register.glia     # shell-loaded registration
│   ├── serve.glia        # DHT provide loop
│   └── consume.glia      # discover + query loop
├── etc/
│   └── init.d/
│       └── oracle.glia   # deployment-only hook
└── src/
    └── lib.rs            # guest implementation
```
