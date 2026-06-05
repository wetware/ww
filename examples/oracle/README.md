# Oracle -- Gas Price Feed

Decentralized gas price oracle over service-name vat RPC and HTTP. Fetches live
gas prices from Blocknative via `HttpClient`, serves them over typed Cap'n Proto
RPC **and** HTTP/JSON, and advertises on the DHT for peer discovery.

## What it demonstrates

- **Dual transport** -- one binary, two transports (vat RPC + HTTP)
- **Cap'n Proto cell** (`WW_CELL_MODE=vat`) -- service-name vat RPC
- **WAGI cell** (`WW_CELL_MODE=http`) -- CGI, curl-friendly JSON
- `HttpClient` capability for outbound HTTP (domain-scoped)
- `with`/`cell`/`listen` DX for capability-scoped cell definitions
- DHT discovery via `routing.provide()` / `findProviders()`
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

This compiles the WASM guest and embeds canonical `SchemaBundle` bytes in the
`ww.schema.v1` WASM custom section. The vat route uses the service name
`oracle`; schema and WASM CIDs are metadata returned by `VatConnection`.

## Running

### Step 1: Run the oracle node (daemon terminal)

Start a host with HTTP enabled:

```sh
ww run --http-listen 127.0.0.1:2080 --port=2025 std/kernel
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
/ > (load "glia/register.glia")
/ > (load "glia/serve.glia")
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
ww run --port=2026 std/kernel
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
/ > (load "glia/register.glia")
/ > (load "glia/consume.glia")
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
  glia/register.glia mounts cell on     curl http://localhost:2080/oracle
  two transports (vat + http)              |
                                           v
  HttpListener accepts     <---HTTP---  axum server
  spawns cell (http mode)               routes by prefix
    membrane.graft()                       |
    http_client.get(blocknative)           v
    build JSON response                 CGI response -> JSON
    write to stdout

                                        CONSUMER NODE:
  VatListener accepts      <--libp2p--  vat_client.dial(peer, "oracle")
  spawns cell (cell mode)               bootstrap --> PriceOracle cap
    membrane.graft()                    oracle.get_pairs() -> ["ETH/gas", ...]
    http_client.get(blocknative)        oracle.get_price("ETH/gas")
    cache prices                           |
    serve PriceOracle       --RPC-->    display prices
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

- **Vat cell mode** (`WW_CELL_MODE=vat`): spawned by `VatListener`
  per incoming RPC connection. Creates a `PriceOracleImpl`, grafts
  to obtain `HttpClient`, fetches prices from Blocknative, and
  exports the oracle as the bootstrap capability. Refreshes prices
  every 30-60 seconds while the connection is alive.
- **WAGI cell mode** (`WW_CELL_MODE=http`): spawned by `HttpListener`
  per HTTP request. Grafts the membrane over `wetware:streams`
  (side-channel), fetches prices via `HttpClient`, writes a JSON
  response to stdout via CGI. Stateless -- one cell per request.
- **Service mode**: long-running DHT provider loop. Provides the service
  locator on the DHT and re-provides periodically (records expire).
- **Consumer mode**: discovers oracle providers via DHT, dials them with
  `VatClient`, binds the returned `VatConnection`, and queries prices.
  Exponential backoff (2 s to 60 s).

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
;; Define the oracle cell (HttpClient arrives via membrane graft in-cell).
(def oracle
  (cell (load "bin/oracle.wasm")))

;; Mount on both transports.
(perform host :listen :vat "oracle" oracle)
(perform host :listen :http "/oracle" oracle)
```

`glia/serve.glia`:

```clojure
(perform runtime :run (load "bin/oracle.wasm") "serve")
```

`glia/consume.glia`:

```clojure
(perform runtime :run (load "bin/oracle.wasm") "consume")
```

`(perform host :listen ...)` registers the cell with the host:
- `:vat "oracle"` -> VatListener on `/ww/0.1.0/vat/oracle`
- `:http "/oracle"` -> HttpListener (WAGI at the given prefix)

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
│   └── oracle.wasm       # final WASM with ww.schema.v1
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
