# CLI Reference

## ww init

Scaffold a new typed cell guest project.

```
ww init <NAME>
```

Creates a Rust project with a Cap'n Proto schema, build script, and guest source.
The scaffold uses repository-relative dependencies: create the project under
`examples/` so `../../std/system` and `../../capnp` resolve.

## ww build

Compile a guest project to WASM.

```
ww build [PATH]
```

Targets native `wasm32-wasip3`, validates the component and its imports, then
places the artifact at `bin/<project-directory>.wasm`. Defaults to the current
directory. The resolved directory name determines the output filename; `ww build
app`, `ww build /path/to/app`, and `ww build` inside `app` all write `bin/app.wasm`.
The Cargo invocation must report exactly one WASM artifact for the project.

`ww build` rejects WASI 0.2 and socket imports. The command uses the pinned P3
toolchain described below.

The project directory is the image tree. Build does not create an image manifest
or calculate an image-root CID. Existing supporting files stay in place. See
[build and publication](images.md#build-and-publication) for the import rules.

### Native WASI P3 toolchain

Install Rust nightly `nightly-2026-08-30` with `rust-src`:

```sh
rustup toolchain install nightly-2026-08-30 \
  --profile minimal \
  --component rust-src
```

Download and extract WASI SDK 34.0. Download `wasm-tools` 1.258.0. Then set:

```sh
export WASI_SDK_PATH=/path/to/wasi-sdk-34.0-<arch>-<os>
export WASM_TOOLS=/path/to/wasm-tools
```

`WASM_TOOLS` is optional when the pinned executable is on `PATH`. Do not run
`rustup target add wasm32-wasip3`. The build compiles the Tier 3 standard
library from `rust-src`.

## ww run

Boot a wetware node.

```
ww run [OPTIONS] [MOUNT...]
```

Every positional argument is a mount: `source[:target]`.
Without `:target`, the source is mounted at `/` (image layer).
With `:target`, the source is overlaid at that guest path.
Directory targets merge recursively; files and other type replacements use the later source.
Operations keep argument order, including repeated and overlapping targets.
Local sources use `.`, `./path`, `/absolute/path`, or `~/path`. Bare names must
be complete CIDs; use `./name` to select a local name. Bare `ww run` uses the
installed FHS at `~/.ww/fhs`, independently of CWD. `--fhs-root` or `WW_FHS_ROOT`
selects a different baseline directory. The Host never imports the private
`~/.ww` directory as its baseline. Without a usable installation, explicit
sources must supply a complete bootable FHS.

Every epoch composes installed baseline, authoritative Stem head, frozen namespace
images, and frozen CLI operations in that order. PID0 comes exclusively from
`/boot/kernel.wasm` in the resulting root. A valid head without that path can
inherit the installed kernel through ordinary directory composition. Invalid or
unavailable heads cannot become baseline-only deployments. Invalid selected
kernels fail without falling back to installed, embedded, or previous bytes.

Targets must be absolute and canonical: no trailing slash, repeated slash, `.`,
`..`, or control characters. Root mounts require directories. Files require an
explicit target. Source paths reject symlink traversal. A colon separates the
source from its target and cannot appear in a source operand.

All local sources are captured in one bounded batch before publication. The
resulting retained source CIDs and targets replay unchanged after Stem updates.
Editing local paths after capture does not update the running deployment.
See [local snapshots](local-snapshots.md) for exclusions, limits, and failure behavior.

### Options

| Flag | Default | Description |
|------|---------|-------------|
| `--port <PORT>` | `2025` | libp2p swarm listen port |
| `--identity <PATH>` | `~/.ww/identity` | Ed25519 identity file path. Also reads `WW_IDENTITY` env. |
| `--insecure-ephemeral` | off | Allow ephemeral identity fallback if identity file is missing (insecure; for quick trial runs). |
| `--http-listen <ADDR>` | none | Enable WAGI HTTP server (e.g. `127.0.0.1:2080`) |
| `--http-dial <HOST>` | none | Allow outbound HTTP to host. Repeatable. Supports exact hosts, `*.example.com`, or `*`. Without this flag, `network.http.dialer` is null. |
| `--with-http-admin <ADDR>` | `127.0.0.1:2026` | Local HTTP admin endpoint (`/healthz`, `/readyz`, `/version`, `/metrics`, `/host/id`, `/host/addrs`); set `off` to disable (case-insensitive). Also reads `WW_HTTP_ADMIN`. A non-loopback address exposes unauthenticated host diagnostics. |
| `--fhs-root <PATH>` | `~/.ww/fhs` | Installed baseline directory, captured once below Stem, namespace, and CLI content. Requires an absolute path or `~/path`. Also reads `WW_FHS_ROOT`. |
| `--wasm-debug` | off | Enable WASM debug info for guest processes |
| `--executor-threads <N>` | `0` | Executor worker threads (0 = auto-detect, one per CPU core) |
| `--runtime-cache-policy` | `shared` | `shared`: same WASM bytes share Executor. `isolated`: always fresh. |
| `--ipfs-url <URL>` | `http://localhost:5001` | IPFS HTTP API endpoint. Also reads `IPFS_API` env. |
| `--stem <ADDR>` | none | Atom contract address (hex, 0x-prefixed). Enables authoritative deployment following. |
| `--ipns-stem <IPNS_NAME>` | none | Follow an IPNS name as the authoritative deployment Stem. Conflicts with `--stem`. |
| `--ipns-routing-url <URL>` | `http://localhost:8080` | Kubo Gateway listener for raw HTTP Routing V1 records. Also reads `IPFS_ROUTING_API`. |
| `--rpc-url <URL>` | `http://127.0.0.1:8545` | HTTP JSON-RPC for finalized-depth `eth_blockNumber` and `eth_call` polling. |
| `--ws-url <URL>` | `ws://127.0.0.1:8545` | Atom event endpoint for optional advisory pre-finality preparation. Authoritative following uses HTTP polling through `--rpc-url`; event subscriptions are not required for correctness. |
| `--confirmation-depth <N>` | `6` | Chain depth used when reading authoritative `Atom.head()` state. |

### Examples

```sh
# Installed state, independent of current directory
ww run

# Dev mode (current directory)
ww run .

# HTTP endpoint with outbound access
ww run . --http-listen 0.0.0.0:2080 --http-dial api.example.com

# Admin metrics on a custom local port
ww run . --with-http-admin 127.0.0.1:3030

# Explicit identity path + image layers
ww run --identity ~/.ww/identity ./images/app /ipfs/QmDataLayer

# Run from IPFS
ww run /ipfs/QmHash...

# Targeted local file and directory snapshots
ww run ./kernel.wasm:/boot/kernel.wasm ./config:/etc/myapp

# Select PID0 through an immutable file override
ww run /ipfs/<file-cid>:/boot/kernel.wasm

# Atom-backed deployment lifecycle
ww run . --stem 0x1234...abcd --rpc-url http://rpc.example.com:8545

# IPNS-backed deployment lifecycle
ww run --ipns-stem k51qzi5uqu5... --ipns-routing-url http://localhost:8080
```

Embedded payloads seed installation only.
Files under `/bin` do not execute merely because they are present.

The IPNS Stem accepts canonical Base36 names, legacy base58 Peer IDs, and
`/ipns/`-prefixed forms. It is not an `/ipns/...` image mount. The Stem follows
signed record changes and revokes the deployment at signed EOL when no valid
replacement exists.

Kubo 0.33 does not expose Routing V1 on the Gateway by default. Configure and
restart Kubo before using an IPNS Stem or the installed default publisher:

```sh
ipfs config --json Gateway.ExposeRoutingAPI true
```

Use the Gateway listener, normally port 8080. Port 5001 is the administrative
RPC listener and does not serve this path. Wetware does not fall back to Kubo's
`name/resolve` or `name/publish` APIs for host-owned IPNS records.

### Environment variables

| Variable | Set by | Description |
|----------|--------|-------------|
| `WW_IDENTITY` | user | Default identity file path |
| `WW_FHS_ROOT` | user | Installed FHS baseline directory; accepts an absolute path or `~/path` |
| `WW_TTY` | host | Set to `1` when stdin is a terminal (triggers interactive shell mode) |
| `WW_CELL_MODE` | host | Cell transport mode: `vat`, `raw`, `http`, or absent (kernel) |
| `IPFS_API` | user | Default IPFS HTTP API endpoint |
| `IPFS_ROUTING_API` | user | Kubo Gateway listener for HTTP Routing V1. Default: `http://localhost:8080`. |
| `WW_HTTP_ADMIN` | user | Admin listener address, or `off` to disable it |
| `WW_CWASM_DIR` | operator | Optional directory for Wasmtime's native compilation cache. Wasmtime owns artifact compatibility; an unavailable directory falls back to uncached compilation. |
| `WW_CWASM_CACHE_MAX_BYTES` | operator | Wasmtime cache cleanup threshold in bytes (default: `335544320`). This is a soft limit, so leave filesystem headroom. |
| `WW_OCI_IMAGE_ID` | operator | Optional immutable OCI image identity reported by `/version`. Deployment verification uses Kubernetes `pod.status.imageID` as the authority. |
| `RUST_LOG` | user | Host-side tracing verbosity |
| `WW_LOG_FORMAT` | user | Host log format: `auto` (default), `compact`, or `json` |
| `WW_OTLP_ENDPOINT` | user | Opt-in local OTLP/HTTP trace endpoint; absent disables export |
| `WW_TRACE_SAMPLE_RATIO` | user | Eligible Cell root sampling probability, `0` through `1`; enabled default `0.01` |
| `WW_TRACE_MAX_SPANS_PER_SECOND` | user | Sampled span admission cap, `1` through `10000`; default `100` |
| `NO_COLOR` | user | A nonempty value disables ANSI color in Host logs |

### Host logging

Host diagnostics go to stderr, leaving stdout for command results. With
`WW_LOG_FORMAT=auto`, an interactive **stderr** selects compact structured text;
redirected stderr selects newline-delimited JSON. Redirecting stdout alone does
not change the format. `compact` and `json` override format selection without
changing filtering. Compact output uses ANSI only on a terminal when `NO_COLOR`
is absent or empty. JSON never adds ANSI color.

Without a valid `RUST_LOG`, internal targets (`ww`, `atom`, `cache`, `cell`, `ipfs`,
`membrane`, `rpc`, `stem`) default to `warn` on a terminal and `info` when stderr
is redirected. A valid `RUST_LOG` replaces these defaults, including target and
span-field directives. For example:

```sh
WW_LOG_FORMAT=json RUST_LOG=ww=debug,rpc=info ww run 2>host.jsonl
NO_COLOR=1 WW_LOG_FORMAT=compact ww run
```

Each JSON line contains `timestamp`, `level`, `target`, `line`, and `fields`.
Recognized event codes select a literal `fields.message` and typed contract
fields. Approved numeric and boolean fields retain their JSON types. Other values,
including interpolated messages and arbitrary strings, are redacted.

When available, `span` contains the current span's name and safe fields. The
formatter reserves `span.name` for the span name (`cell` for a Cell span).
ExecutorPool owners use numeric `span.owner_id`; ordinary children use
`span.cell_id`; managed RPC workers use `span.rpc_id`. VAT and stream handlers use
`span.connection_id`. These identity namespaces are distinct and local to one
Host process. `cell_name`, `name`, protocol names, and service names remain
available to existing span-field filters but are redacted in output.

Ancestor spans are not repeated in a `spans` list. Filtering can remove span
context, and separately spawned tasks need explicit span propagation. RPC spans
describe connections, not the caller that opened them.
See [async tracing ownership](runtimes.md#async-tracing-ownership) for propagation
and filtering boundaries.

Unknown, empty, or non-Unicode `WW_LOG_FORMAT` values fall back to `auto` and
produce a warning, subject to `RUST_LOG`. The warning does not echo the value.
Errors recorded as `dyn Error` retain bounded source relationships with redacted
messages. Both output formats enforce the same field and record limits, including
when TRACE is enabled. See the [operational logging contract](logging.md) for event
codes, privacy policy, correlation fields, error chains, and migration rules.

This policy covers Host tracing. Interactive shell rendering, progress output,
and direct guest/PID0 stderr keep their existing behavior (child-output ownership:
#155). Ordinary child stderr forwarded through tracing emits TRACE byte counts;
the log record does not include the child output.

Trace export accepts only an HTTP loopback IP endpoint with path `/v1/traces`.
Credentials, query strings, DNS names, and redirects are not supported.
Invalid configuration disables export and emits event `111`. Keep `cell` and
`ww` INFO spans enabled in `RUST_LOG` to retain the Cell lifecycle boundaries.
The [local observability recipe](observability.md) documents sampling, queue
bounds, shutdown behavior, and the Grafana incident walkthrough.

## ww image check

Check whether the effective FHS satisfies the current Host's static PID0 boot
requirements without starting a deployment.

```sh
ww image check [OPTIONS] [SOURCE[:TARGET] ...]
```

The command uses the `ww run` mount parser, source capture, composition, and
kernel preparation. Precedence is installed baseline, authoritative Stem head,
frozen namespace images, then positional CLI mounts. Arguments retain their
order. Bare `ww image check` uses installed/configured startup state without
implicitly mounting CWD. Explicit local paths and immutable `/ipfs/<CID>`
sources follow the same path, symlink, target, and resource rules as `ww run`.

| Flag | Default | Description |
|------|---------|-------------|
| `--fhs-root <PATH>` | `~/.ww/fhs` | Installed baseline directory; also reads `WW_FHS_ROOT`. |
| `--namespace-root <PATH>` | startup defaults | Host-only root containing `etc/ns` configuration; repeatable. |
| `--identity <PATH>` | `~/.ww/identity` | Identify a host-owned `ww` namespace; also reads `WW_IDENTITY`. Read only when publisher ownership must be determined. |
| `--stem <ADDR>` | none | Read one finalized-depth Atom head; conflicts with `--ipns-stem`. |
| `--ipns-stem <NAME>` | none | Read one verified IPNS head; conflicts with `--stem`. |
| `--ipns-routing-url <URL>` | `http://localhost:8080` | Routing V1 endpoint; also reads `IPFS_ROUTING_API`. |
| `--rpc-url <URL>` | `http://127.0.0.1:8545` | Atom JSON-RPC endpoint. |
| `--confirmation-depth <N>` | `6` | Depth used to read the Atom head. |
| `--ipfs-url <URL>` | `http://localhost:5001` | Kubo API endpoint; also reads `IPFS_API`. |
| `--json` | off | Print the complete successful result as JSON. |

```sh
# Check installed startup state
ww image check

# Check a complete local image or targeted overrides
ww image check ./image
ww image check ./kernel.wasm:/boot/kernel.wasm ./config:/etc/myapp

# Check immutable content and report identities as JSON
ww image check /ipfs/<directory-cid> --json
ww image check /ipfs/<file-cid>:/boot/kernel.wasm

# Check the composition for one authoritative head
ww image check --stem 0x1234...abcd --rpc-url http://127.0.0.1:8545
ww image check --ipns-stem <name> --ipns-routing-url http://localhost:8080
```

Success reports the effective root CID, kernel source-file CID, exact-byte
kernel runtime CID, `wetware-composer-v1` profile, SHA-256 composition hash,
static validation result, and completed stages. The source-file CID identifies
the UnixFS file. The runtime CID identifies the verified WASM bytes using
CIDv1/raw/BLAKE3-256; it is not the Composer hash profile.

JSON fields are `effective_root`, `kernel_source`, `kernel_runtime`,
`composer_profile`, `hash_algorithm`, `static_validation`, `stages`, and
`retained_roots`. `hash_algorithm` is `sha2-256`; `static_validation` is `PASS`
on success. `stages` lists `source-resolution`, `filesystem-composition`,
`kernel-selection`, `verified-component-bytes`, and
`production-p3-static-validation`. Exit status is zero only after all validation
stages succeed.
Failures produce a nonzero exit status and identify the failing stage; partial
results are not successful checks.

The check resolves and verifies sources, composes the effective FHS, selects
`/boot/kernel.wasm`, verifies its UnixFS bytes and size, compiles the component,
and validates the production P3 imports, exports, transport, readiness, and
static command linker. A missing or invalid requested head fails without
substituting an installed-baseline-only image.

Unlike `ww run`, the check reads one authoritative head and does not follow
later changes. IPNS checks read the persisted ordering floor without advancing
it. An unexpired persisted IPNS head takes precedence over a network fetch,
matching initial boot selection. A later boot can observe a different head or
changed local content.

Authoritative-head lookup has a 30-second deadline. Namespace resolution uses
boot's existing per-name deadlines and bootstrap fallback. If a
configured namespace still has no resolved source, the check fails instead of
reporting a partial image. `ww run` can continue in a degraded state or retry
while following a Stem. These availability differences do not change static
validation of identical frozen inputs.

For a `ww` namespace with a Peer ID and bootstrap CID, the check reads the Host
identity to determine publisher ownership. If the Host owns that name, the
resolved namespace must already match the configured bootstrap. Otherwise the
check fails because `ww run` would publish the bootstrap before resolution.
The check does not perform that publication. Use explicit immutable mounts when
the required publisher state is unavailable.

The command is non-executing, but it is not read-only. It can retrieve remote
immutable content, import local snapshots, insert Kubo blocks, publish composition
CARs, acquire recursive pins, write the Wasmtime compilation cache, and create
temporary local files. It does not publish namespace/IPNS state, instantiate
PID0, execute guest start functions or `/bin` files, grant root Membrane
authority, advance deployment epochs, commit readiness, or start runtime services.

Kubo pins are CID-based and do not identify independent owners. The check
therefore retains Kubo pins, including acknowledged source and effective-root
pins, rather than issuing compensating unpins. `retained_roots` reports known
retained roots; interrupted or uncertain publication can leave additional pins
or blocks. Existing owners keep their content. Operator cleanup requires
separate knowledge that no owner needs the content.

Capture and composition use the existing [snapshot limits](local-snapshots.md#bounds)
and [Composer limits](composer-v1.md#supported-inputs-and-limits). Cancellation
waits for local capture cleanup; a blocked filesystem syscall has no hard
cancellation deadline. Cancellation does not roll back Kubo imports or pins.
There is no offline mode: missing blocks can trigger remote retrieval.

A passing check establishes static compatibility only. It does not establish
guest startup, root graft, readiness, routes, Cap'n Proto semantics, or application
behavior. Use runtime readiness and application checks after starting the Host.

## `ww healthcheck`

Probe the admin endpoint without relying on shell tools. This is suitable for
exec probes in the distroless deployment image.

```sh
ww healthcheck
ww healthcheck --ready
ww healthcheck --ready --expect-git-sha "$EXPECTED_SHA"
ww healthcheck --ready --require-cache-enabled
```

| Flag | Default | Description |
|------|---------|-------------|
| `--admin <ADDR>` | `127.0.0.1:2026` | Admin listener to probe; also reads `WW_HTTP_ADMIN` |
| `--ready` | off | Probe `/readyz` instead of `/healthz` |
| `--expect-git-sha <SHA>` | unset | Also require `/version` to report the exact source revision |
| `--require-cache-enabled` | off | Also require `/version` to report an enabled persistent Wasmtime cache with at least one hit or store |
| `--timeout-secs <N>` | `2` | Per-request timeout |

The command prints `ok` and exits zero only when every requested check passes.

## ww push

Publish a built image tree to IPFS.

```
ww push [PATH] [OPTIONS]
```

Requires at least one regular `bin/*.wasm` file. Imports the tree using the same
rules as local runtime image mounts. It does not select an entrypoint, rename
components, or modify the input tree. Kubo pins the imported directory by default.

The `CID:` line identifies the imported directory root. It does not identify
only the contained WASM bytes or a deployment. The command does not update Atom
or IPNS. Removed flags `--stem`, `--rpc-url`, and `--private-key` are rejected.

| Flag | Default | Description |
|------|---------|-------------|
| `--ipfs-url <URL>` | `http://localhost:5001` | IPFS HTTP API endpoint |

`PATH` defaults to the current directory. `--help` prints command help. See the
[image lifecycle](images.md#build-and-publication) for a complete example and
the files excluded from publication.

## ww keygen

Generate a new Ed25519 identity.

```
ww keygen [--output PATH]
```

Prints the base58btc secret key to stdout, peer ID to stderr.

```sh
ww keygen > ~/.ww/identity
ww keygen --output ~/.ww/identity   # equivalent
```

## ww doctor

Check the development environment for required and optional tools.

```
ww doctor
```

Verifies Cargo and the pinned native WASI P3 toolchain. The toolchain check
includes Rust nightly, `rust-src`, WASI SDK, `wasm-component-ld`, LLD, and
`wasm-tools`. The command also checks optional Kubo and Ollama installations.
Exit 0 if all required checks pass.

## ww perform

Effectful operations that mutate state beyond the current directory.

### ww perform install

Bootstrap `~/.ww` and the daemon.

Validates the payload and managed-file ownership before replacing files.
Reruns preserve existing identity and refresh only owned managed files.

1. Creates `~/.ww` directory structure
2. Generates `~/.ww/identity` if missing
3. Derives the host Peer ID and default Base36 IPNS name from that identity
4. Installs the validated kernel and status components under `~/.ww/fhs`
5. Refreshes the default namespace when payloads change or its bootstrap is absent
6. Registers the background daemon (launchd/systemd) and prints its activation command

The command does not create a Kubo `"ww"` signing key. Kubo never receives the
Wetware identity. Installation does not start the service. See
[installation and recovery](installation.md) for activation and ownership rules.

### ww perform update

Revalidate the installed payload and refresh the managed installation.

```sh
ww perform update
```

The command preserves `~/.ww/identity` and reuses the default namespace unless
its bootstrap is absent.
Installed payloads take precedence over embedded installation seeds. Generated
services retain the selected Kubo API and Routing V1 URLs. If files changed, the
command attempts to restart the service; otherwise it leaves the service running.
The daemon signs and publishes the configured default namespace when it starts.

### ww perform upgrade

Self-update the `ww` binary via IPNS.

```
ww perform upgrade [--ipfs-url URL]
```

Resolves `/ipns/releases.wetware.run/Cargo.toml` for the latest
version, fetches the platform binary, and atomically replaces the
running executable.

### ww perform uninstall

Remove the daemon and optionally `~/.ww`.

## ww daemon

Manage the background daemon.

### ww daemon install

Register wetware as a user-level background service (launchd on
macOS, systemd on Linux).

```
ww daemon install [--identity PATH] [--listen MULTIADDR ...] [--images PATH...] [--ipns-routing-url URL]
```

`--ipns-routing-url` defaults to `http://localhost:8080` and reads
`IPFS_ROUTING_API`. Generated launchd and systemd definitions preserve the
selected value.

`IPFS_API` supplies the install-time Kubo endpoint. The generated service records
it as `--ipfs-url`, so startup does not depend on inheriting that variable.
Without `--images`, the service uses the installed FHS baseline and reads
`~/.ww/etc/ns` through a separate host-only configuration argument.

### ww daemon uninstall

Remove the platform service file.

## ww ns

Manage namespaces (IPFS mount layers).

### ww ns list

List configured namespaces.

### ww ns add

Add or update a namespace.

```
ww ns add <NAME> [--ipns KEY]
```

Writes a config file to `~/.ww/etc/ns/<name>`.

## ww oci import

Pull the container image from IPFS into Docker/podman.

```
ww oci import [--cid CID] [--stdout] [--ipfs-url URL]
```

Resolves the OCI tar from `/ipns/releases.wetware.run/oci/image.tar`
and pipes it to `docker load` or `podman load`.

```sh
ww oci import                     # auto-detect and load
ww oci import --cid QmHash...    # specific CID
ww oci import --stdout | podman load
```
