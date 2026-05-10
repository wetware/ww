# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/).

## [Unreleased]

### Changed
- **Snap v1.5 — interactive "Ping me" button + per-click timestamp.** `examples/snap-hello-rs/` now renders a `stack` containing a `text` greeting + a `button` element ("Ping me", primary variant). Button's `on.press` is a `submit` action whose `params.target` is the cell's own URL (built at runtime from `HTTP_HOST` + `PATH_INFO`, assuming HTTPS termination). On POST, the cell renders `Hello, {viewer} — pinged at <unix> UTC (unix)` — `viewer` is `@stranger` for anonymous POSTs and `FID #N` for JFS-verified ones (POST is REQUIRED to be JFS-signed per spec, so real client button-presses always exercise the viewer-aware path). POST responses now use `Cache-Control: private, no-store` (per-viewer + freshness-sensitive); GET stays at `public, max-age=300` for the anonymous render. Why this matters: Farcaster's web + mobile renderers fetch snap GETs server-side without `X-Snap-Payload`, so the v1.0 viewer-aware GET path never fired in practice. The button gives every clicking user a way to trigger a JFS-signed POST end-to-end, which is the only way to actually demo viewer-awareness on first-party Farcaster clients today. Cell tests grew from 9 to 15 (new: stack/button/submit/target shape; anonymous + viewer-aware POST text rendering with timestamp marker; 320-char limit under worst-case FID + timestamp). E2E tests grew from 5 to 6 — added `snap_cell_post_with_verified_jfs_renders_fid_and_timestamp` covering the verified-POST path end-to-end through the full HttpListener dispatch + executor spawn + CGI env passthrough.

### Fixed
- **Default tracing filter silenced workspace-crate logs after #444 (#448).** PR #444 moved `src/rpc/`, `src/cell/`, `src/ipfs/` (and friends) into top-level workspace crates, so their `tracing::*` events now emit at targets like `rpc::vat_listener` and `cell::executor` instead of `ww::rpc::*` / `ww::cell::*`. The default filter in `src/config.rs` (`ww=warn`/`ww=info` depending on TTY) only matches the binary crate, so every log line from the split crates was filtered out at runtime — including the load-bearing `Registered vat subprotocol cell` and `registered HTTP route` messages emitted from `crates/rpc/src/{vat,http}_listener.rs`. Registration itself was unaffected (`/status` continued to answer JSON), but the silence made it look like the listeners weren't wiring up, blocked diagnosis of the separate 30s `ww shell` bootstrap timeout, and hid any new `tracing::info!` instrumentation added inside `handle_vat_connection_spawn`. Fix: `src/config.rs::init_tracing_to_stderr` now expands the default to a comma-separated list of all wetware-internal workspace crates (`ww`, `atom`, `cache`, `cell`, `glia`, `ipfs`, `membrane`, `rpc`, `stem`), each set to the same level (`warn` on TTY, `info` otherwise). `RUST_LOG` overrides remain untouched.
- **`std/system` `wit_bindgen!` path after workspace split.** PR #444 moved `wit/` -> `crates/cell/wit/` but missed the relative path in `std/system/src/lib.rs:65`'s `wit_bindgen::generate!` macro, which was looking at `<repo>/wit/` (gone). Master CI's `Build WASM components` job has been failing on every push since #444 merged ("failed to read path for WIT [...]: No such file or directory"), blocking IPFS publish + deploy — so master.wetware.run is still running the pre-#444 image and #445's snap v1 deploy hasn't happened. Fix: `path: "../../wit"` -> `path: "../../crates/cell/wit"`. The `Build WASM components` job is conditional on the PR-level `Detect changes` matrix, so this kind of cross-crate path breakage only fails on master push, not on the originating PR — worth a follow-up to broaden coverage.

### Added
- **Farcaster Snap v1.0 — JFS-verified viewer awareness + POST.** New `crates/rpc/src/jfs.rs` module verifies the `X-Snap-Payload` header per the JSON Farcaster Signatures spec (https://github.com/farcasterxyz/protocol/discussions/208 + https://docs.farcaster.xyz/snap/auth): JWT-style compact serialization (`BASE64URL(header) . BASE64URL(payload) . BASE64URL(signature)`) split + decode, EdDSA signature verify against the embedded `app_key` (32-byte Ed25519 hex pubkey), `header.fid == payload.fid` consistency check, audience match against the server's expected origin, and ±5 min timestamp window (default per spec, configurable). 16 unit tests cover happy path, inputs/b64url passthrough, audience mismatch, expired/future-skew rejection, within-skew accept, tampered payload + tampered header rejection, FID mismatch rejection, non-`app_key` type rejection, malformed compact, pubkey 0x prefix and wrong-length handling. The HTTP listener's host-bin handler (`src/dispatcher/server.rs`) now calls `rpc::jfs::verify` on incoming `X-Snap-Payload` headers before constructing `CgiRequest` (whose new `verified_snap: Option<rpc::jfs::VerifiedJfs>` field — added to `crates/rpc/src/dispatch.rs::CgiRequest` — carries the result downstream); verified payloads flow through to cells as new CGI env vars `X_SNAP_FID_CLAIMED`, `X_SNAP_TIMESTAMP`, `X_SNAP_AUDIENCE`, `X_SNAP_PAYLOAD_B64URL` (emitted by `crates/rpc/src/wagi.rs::build_cgi_env` when `verified_snap` is `Some`). The `examples/snap-hello-rs/` cell now reads `X_SNAP_FID_CLAIMED` and renders `Hello, FID #N` when present, falling back to `Hello, @stranger` when absent. POST requests are acknowledged with the same UI tree per the snap spec's submit-action contract. Two new e2e tests in `tests/snap_hello_rs_http_listener_e2e.rs` cover the JFS-verified viewer-aware GET path and the POST ack (test file now serializes its 5 tests via a `Mutex` guard since each test spins up its own Runtime + executor pool + libp2p stack and parallel runs collide on shared resources). **NOT in v1.0 (deferred to v1.1):** Hub round-trip to confirm the embedded key is currently registered to the claimed FID. Without that step, the FID is *cryptographically signed* against the embedded key, but the key↔FID binding is *not Hub-verified* — an attacker can sign a payload claiming any FID with their own keypair and the signature verifies. The CGI env var name `X_SNAP_FID_CLAIMED` makes this explicit. Cells that grant authority based on FID identity SHOULD wait for v1.1. The listener is also currently permissive on verification failure (logs warn, treats as anonymous) rather than spec-strict (`MUST 4xx on malformed/expired/invalid`); v1.1 will tighten this in lockstep with the Hub check.

### Changed
- **Cargo workspace split: `crates/cell`, `crates/ipfs`, `crates/rpc`.** The root `ww` crate was a 20k-LOC kitchen sink with `wasmtime` + `libp2p` (kitchen-sink feature list) + `capnp-rpc` + `axum` + `reqwest` all bolted onto one Cargo.toml — every edit to `cli/main.rs` invalidated translation units that pulled `cranelift-codegen` (304s baseline self-time), `wasmtime` (183s), and `wasmtime-wasi` (109s). Split the bulk into three workspace crates along natural seams: `crates/cell` for WASM execution (`wasmtime` + `wasmtime-wasi` + `wasmtime-wasi-io` + `cap-std`; absorbs `cell/`, `vfs.rs`, `mount.rs`, `sched.rs`, `epoch.rs`, `loaders.rs`, `image.rs`, `fs_intercept.rs`, plus `wit/` for `bindgen!`), `crates/rpc` for libp2p + capnp-rpc protocol (absorbs `rpc/`, `keys.rs`, plus the WAGI dispatch types and CGI parsing), `crates/ipfs` for the Kubo HTTP client (`reqwest` + `tar`). Result: editing `cli/main.rs` rebuilds in ~10s instead of ~30s+, and edits to `cell` no longer invalidate `rpc` (and vice versa). Required breaking a `rpc <-> cell` cycle first by lifting two wiring files (`RuntimeImpl`/`ExecutorImpl` capnp Server impls, and `cell/executor.rs`) out of both subtrees and into the bin layer where they belong (`src/launcher.rs`, `src/executor.rs`). Also: renamed `runtime.rs` to `services.rs` (it's the `Service` trait + supervisor, never the WASM runtime), and renamed the rpc submodule `membrane` to `graft` to stop shadowing the workspace `membrane` crate. No behavior change. `pub use cell;`, `pub use ipfs;`, `pub use rpc;` re-exports in `src/lib.rs` keep external paths (`ww::cell::*`, `ww::ipfs::*`, `ww::rpc::*`) stable.

### Fixed
- **`/ipns/<hash>[/<subpath>]` mount paths now work for `ww run`** (`src/image.rs`). Previously, `resolve_mounts_virtual` accepted `/ipns/` paths via `is_ipfs_path()` (which matches `/ipfs/`, `/ipns/`, `/ipld/` per `src/ipfs.rs:627-629`) but then crashed on `strip_prefix("/ipfs/")?` with "IPFS path must start with /ipfs/". Bug was hidden in production because the deployment.yaml on disk had aspirational `/ipns/.../examples/<name>` args that had never been `kubectl apply`'d to the cluster — the actual running pod had only kernel + shell mounts. Now: `/ipns/<hash>[/<subpath>]` paths resolve to `/ipfs/<cid>[/<subpath>]` via Kubo's `name/resolve` (mirrors the `/ipns/releases.wetware.run` upgrade-flow pattern in `src/cli/main.rs:2556-2565` and the namespace pattern in `src/ns.rs:131`). New `split_ipns_path` helper splits the IPNS hash from any subpath and is unit-tested across happy path, no-subpath, trailing slash, empty hash, missing prefix, and nested subpath cases (6 tests).

### Changed
- **CI IPFS publish layout: per-example cell-shaped subtrees added** (`.github/workflows/rust.yml`). Existing flat `bin/<name>.wasm` + `lib/init.d/<name>.glia` copies preserved for reference; new per-example `examples/<name>/{etc/init.d,bin}/` subtrees mirror the kernel/shell `<name>/{etc/init.d,bin}/` convention so `ww run /ipns/<key>/examples/<name>` (or `/ipfs/<cid>/examples/<name>`) mounts the cell correctly: init.d glia ends up at the merged tree's `/etc/init.d/` where the kernel discovers it (`std/kernel/src/lib.rs:1148-1188`), and `(load "bin/<name>.wasm")` inside init.d resolves relative to the merged tree root. Together with the `/ipns/` resolution fix above, this enables ww-master's `~/Infra/wetware/k8s/ww-master/deployment.yaml` to mount published example cells (counter, oracle, snap-hello-rs, etc.) via stable IPNS paths instead of stale per-release CIDs.

### Removed
- **Stale `capnp/auction.capnp` and `capnp/oracle.capnp`.** Both were orphaned: nothing in the build references them. `capnp/oracle.capnp` was a byte-identical duplicate of `examples/oracle/oracle.capnp` (the example's `build.rs` reads its local copy). `capnp/auction.capnp` was a divergent earlier draft (different schema ID, different struct shapes) — the live one is `examples/auction/auction.capnp`, used by both `examples/auction/build.rs` and `std/shell/build.rs`. Also removed the now-stale Makefile comment ("auction.capnp lives in capnp/ but is compiled by the example crate") that pointed at the deleted file. Demo-specific schemas now live exclusively under their `examples/<demo>/` subfolders; `capnp/` is core-only (`cell`, `http`, `routing`, `shell`, `stem`, `system`).

### Added
- **`examples/snap-hello-rs/` — Farcaster Snap POC over WAGI.** Static `Hello, @stranger` snap-hello cell that proves Farcaster Snaps can be hosted on a wetware HTTP cell. Implements the spec's content-negotiation contract: `GET` with `Accept: application/vnd.farcaster.snap+json` returns the snap's UI tree (`version: "2.0"`, `text` element with `content` prop, max 320 chars per spec); any other Accept returns minimal HTML with a `Link: <>; rel="alternate"; type="application/vnd.farcaster.snap+json"` header pointing back at the same URL (per RFC 3986 same-document reference) so non-snap-aware visitors and link previewers (Slack, iMessage, Discord) still get something usable. All 4 snap-spec headers always shipped: `Content-Type`, `Vary: Accept` (REQUIRED by spec), `Cache-Control: public, max-age=300`, `Access-Control-Allow-Origin: *`. Stateless cell, no graft caps used. Mounted at `/snaps/hello` via `(perform host :listen ... "/snaps/hello")` in the cell's init.d. Built via dedicated `make snap-hello-rs` target and bundled with `make examples`. Default `ww install` is unchanged (kernel + shell only — examples never ship in the user installer; same posture as `examples/counter`, `examples/oracle`).
- **`tests/snap_hello_rs_http_listener_e2e.rs` — three HttpListener-routed e2e tests** mirroring `tests/status_cell_http_listener_e2e.rs`. Routes through the full dispatch chain (`HttpListener.listen` → `route_registry` → `dispatch_loop` → `executor.spawn`). Coverage: (1) `Accept: application/vnd.farcaster.snap+json` returns 200 + valid snap JSON with all 4 required headers asserted by name+value, (2) `Accept: text/html` returns 200 + `Link rel="alternate"` + HTML body, (3) empty Accept defaults to the HTML fallback (sanity guard against bare crawlers seeing snap JSON). Skips with a clear message when `examples/snap-hello-rs/bin/snap-hello-rs.wasm` is missing. Bug surfaced during first run: cell originally used `wagi::respond` which uses unflushed `print!` — the JSON body sat in stdout buffer and never shipped (HTML body got out by accident because its newlines triggered line-buffer flushes). Fix: switched to `wagi::respond_bytes` (explicit flush) per the documented pattern in `std/status/src/lib.rs:151-153`.

- **`doc/positioning.md` — JTBD-anchored public positioning doc.** Captures the lead Job-To-Be-Done ("compose code I didn't write and don't trust"), anchored on Simon Willison's "lethal trifecta" framing, with a worked tax-prep agent example showing per-call capability attenuation (CID sturdyrefs into the UnixFS DAG, `http-client` gated by `--http-dial`, `identity` for signing). Names the structural differentiation vs. process-level sandbox alternatives (E2B, Modal, Daytona, Microsandbox, Cloudflare Sandboxes, Anthropic Claude Code) on five axes: per-call attenuation, composable membranes, content-addressed code, WASM-cell scale, P2P-optional substrate. Demotes capability security from pitch to proof. Honest about what we don't have yet: host-side trust, RTT-aware routing, hosted Wetware tier, wallet/fuel auctions. Public artifact for build-in-public; deeper dives tracked as follow-up issues (#437 per-call attenuation deep dive, #438 composable membranes doc) plus README overhaul (#436).

- **`tests/status_cell_http_listener_e2e.rs` — HttpListener-routed integration test.** Sibling to `status_cell_e2e.rs`. Where the existing test spawns the WASM directly via Runtime/Executor, this one routes through the full HttpListener dispatch chain: `HttpListener.listen(executor, "/status", caps)` → `route_registry` → `dispatch_loop` → `spawn_and_run` → `executor.spawn`. Seeds a non-empty `caps` list (mirrors what the kernel emits when an init.d author wraps `(perform host :listen ...)` in a `with` block) so a regression in caps forwarding through dispatch surfaces here. Asserts non-null `peer_id` — same load-bearing check as the direct-spawn test, applied to the HTTP-routed code path. Runs in ~8s. Closes the integration-coverage gap flagged when the original attempt was deleted from #430.

### Changed
- **`README.md` rewrite to align with `wetware.run` positioning.** Intro leads with the engineer-resonant rule-of-three ("Wetware lets you safely run code you didn't write, don't trust, and cannot see") plus the locked landing-page category claim ("decentralized operating system for multi-tool agent swarms"). Replaces the marketing-leaning `## Why?` section with mechanism-first `## Features` bullets (per-call attenuation with per-method granularity, composable membranes, content-addressed code, WASM cell scale, P2P capability sharing, MCP integration, Glia shell with effect system) and a tighter `## How it works` covering graft, epoch revocation, and the unified mechanism for cross-cell capability flow. Quickstart split into Install / Run a node / Boot a cell / Use it from an LLM. Container section dropped. New `doc/positioning.md` link in Learn more. The 60-second install + curl `/status` demo is preserved (JSON shape verified against `std/status/src/lib.rs`); all `doc/` links resolve; em dashes purged from prose; no AI-slop vocabulary. No source code changes.

- **`README.md` "Try it in 60 seconds" section** at the top of the file (right after "Why?"). Inlines the install + curl recipe, the JSON response, the capability-attenuation pitch, and the "your LLM can do this too" bonus footnote. Links into existing docs (`doc/capabilities.md`, `doc/architecture.md`, `.agents/prompt.md`) for readers who want the deep dive — no separate per-feature doc. Replaces the prior "Engagement starter kit -- compose-based demo" roadmap bullet (the demo shipped in #430; the compose path was cut). Roadmap now lists `ww shell` capability discovery and IPNS hot-reload as the next engagement beats.

### Added
- **`std/status/` cell + WAGI status endpoint (engagement starter kit Phase 0).** New `std/status/` crate ships a minimal HTTP-only WAGI cell that serves `GET /status` with JSON describing the running node:
  ```json
  {"status":"ok","version":"...","peer_id":"12D3Koo...","listen_addrs":[...],"peer_count":...}
  ```
  `status` and `version` are always populated. `peer_id`, `listen_addrs`, `peer_count` come from the `host` capability if it's in the cell's graft; if the cap is withheld they degrade to `null`. Cell uses `system::run` + `membrane.graft()` and looks up `host` by name with `graft_cap_opt`. Response is written via `wagi::respond_bytes` (explicit flush — `print!` would lose the body to stdout buffering on cell teardown).
- **`bin/status.wasm` embedded into the host binary** via `include_bytes!` next to `kernel.wasm` / `shell.wasm` / `mcp.wasm`. Resolved at runtime through `embedded_loader()` so init.d's `(load "bin/status.wasm")` finds it without an on-disk write or IPFS fetch. Also added to the `embedded_cells` array in `perform_install`/`perform_update` so the published namespace tree includes it.
- **`etc/init.d/05-status.glia` ships in the install layer.** `ww perform install` and `ww perform update` write it into `~/.ww/etc/init.d/` next to `50-shell.glia`. Single line: `(perform host :listen (cell (load "bin/status.wasm")) "/status")`. Default registration uses the kernel's default membrane; an init.d author can attenuate further by wrapping in a `with` block (now real after #429).
- **`DaemonConfig.http_listen: Option<String>` field**, written to `~/.ww/config.glia` as `:http-listen "host:port"` and emitted into the launchd plist's `ProgramArguments` and the systemd unit's `ExecStart` as `--http-listen <addr>`. `daemon_install` defaults this to `127.0.0.1:2080` when not already set, so a fresh `ww perform install` brings up the WAGI HTTP server on the standard port without manual configuration. Four new unit tests cover serialize, parse, default-omission, and roundtrip.
- **Post-install signpost.** `perform_install`'s post-install summary now ends with `curl http://localhost:2080/status` (was `ww shell`). The "wait for the daemon to actually answer" probe lives in `scripts/install.sh` — that's the cold-install entry point and the natural owner of "the daemon takes a few seconds to bind, then point the user at curl." Manual invocations of `ww perform install` outside the install script just print the URL and let the user retry as needed.
- **`scripts/install.sh` waits for `/status` post-install.** After `ww perform install` returns, the install script polls `http://127.0.0.1:2080/status` once a second for ~10 s. On success: prints "ready" and the curl command. On timeout: warns the user and points at `~/.ww/logs/ww.log`. Lets every distribution path (install.sh, manual `ww perform install`, future package managers) keep the simple "print the URL" semantics in the binary, with the wait-and-confirm UX layered on by whichever entry point owns the cold path.
- **`write_launchd_plist` / `write_systemd_unit` unit tests.** Four new tests in `src/cli/main.rs::tests` verify `--http-listen` flag emission: each writer emits the flag with the configured address when `DaemonConfig.http_listen` is `Some(_)`, and omits it entirely when `None`. The `tests/status_cell_e2e.rs` integration path bypasses these writers (it spawns via Runtime/Executor in-process), so a regression that drops the flag from `ProgramArguments` / `ExecStart` would silently disable the engagement starter kit demo without these guards firing.
- **`tests/status_cell_e2e.rs` integration test (CRITICAL gate).** Spawns the real `status.wasm` via Runtime/Executor with WAGI CGI env (`REQUEST_METHOD=GET`, `PATH_INFO=/status`), reads the CGI response, and asserts the JSON body's shape. The load-bearing assertion is that `peer_id` is a non-null base58 string starting with `12D` or `Qm` — a regression that broke capability propagation would surface as `null` here, which is exactly what the engagement starter kit's pitch can't tolerate. Skips with a clear message when `std/status/bin/status.wasm` is missing (run `make -C std/status` first).
- **`HttpListener.listen` accepts a `caps :List(Export)` parameter (engagement starter kit prerequisite).** The capnp schema (`capnp/system.capnp:88-94`) now mirrors `VatListener.listen`: WAGI cells can receive named capabilities from the init.d `with`-block, just like vat cells already could. `HttpListenerImpl` (`src/rpc/http_listener.rs`) reads the caps list, threads it through `dispatch_loop` → `handle_one_request` → `spawn_and_run`, and forwards each grant (name + capnp client + canonical `Schema.Node` bytes) into the spawned cell's `executor.spawn` request. The kernel's `host :listen` handler was updated to populate the new field, replacing the prior `// TODO: thread _caps` placeholder. Empty caps (the no-`with`-block case) is the default; existing init.d scripts continue to work unchanged. Three new unit tests in `src/rpc/http_listener.rs` cover the empty-caps, non-empty-caps, and stale-epoch paths. Until this landed, WAGI cells received the kernel's default membrane regardless of the init.d author's intent — capability attenuation for HTTP cells now matches the design.
- **Effect-based Glia exceptions (#389, Step 2).** Errors are now an effect with target `:glia.exception` over the existing `perform` / `with-effect-handler` machinery. `(throw err)` desugars to `(perform :glia.exception err)`; `(try EXPR (catch :glia.error/tag e BODY) ...)` installs a handler that dispatches on `:glia.error/type`. A non-matching catch falls through to the next clause; without a wildcard `(catch _ e ...)` the dispatcher re-throws to the next outer handler. With no handler in scope, an unhandled throw escapes eval as `Err(Val::Effect{ effect_type: "glia.exception", data: <err> })` — outer callers (kernel REPL, MCP cell, shell) unwrap via the new `glia::error::unwrap_thrown` helper.
- **`GliaError` typed enum** (`crates/glia/src/error.rs`) with `From<GliaError> for Val` as the single source of truth for the canonical schema. Variants: `Parse`, `UnboundSymbol`, `Arity`, `TypeMismatch`, `CapCall`, `Rpc`, `EpochExpired`, `PermissionDenied`, `FuelExhausted`, `Internal`, plus `User` (the construction path for `ex-info`-style user errors). The compiler enforces variant fields at every construction site; adding or renaming a variant surfaces all callers automatically.
- **`ex-info` extended to stamp the canonical schema.** `(ex-info "msg" {:type :foo, :extra 1})` now produces `{:glia.error/type :foo, :glia.error/message "msg", :type :foo, :message "msg", :extra 1}`. The user's `:type` becomes the canonical dispatch tag (so `(catch :foo e ...)` matches user-thrown errors), and the user's bare `:type` / `:message` are preserved for back-compat readers.
- **`map?` Glia builtin** — predicate that returns true if the argument is a `Val::Map`. Used internally by the new `try` dispatcher; also a Clojure-idiomatic predicate that was missing.

### Changed
- **`examples/oracle/README.md` port alignment.** All `8080` references updated to `2080` (lines 48, 53, 59, 62, 122) to match `README.md:103`'s standard ports table and the engagement starter kit's WAGI listener default. The README's worked examples now compose with the daemon's default `--http-listen 127.0.0.1:2080`.
- **Glia `ww/fs` and `ww/routing` stdlib modules.** New `std/lib/ww/fs.glia` exposes `fs/read`, `fs/read-str`, `fs/ls`, `fs/stat`, `fs/exists?` (the last never errors on missing paths). New `std/lib/ww/routing.glia` exposes `routing/resolve`, `routing/provide`, `routing/find`, `routing/hash`. The pure-Glia helpers `ipfs-path`, `ipns-path`, `cid?` move from `ww/ipfs` into `ww/fs` (path helpers belong with the filesystem module).
- **`make_fs_handler` host-side effect handler** (replaces `make_ipfs_handler`). Exposes `:read`, `:read-str`, `:ls`, `:stat`, `:exists?` methods on the `fs` cap. All error paths produce structured errors via the `glia::error::*` constructors landed in #419 (`:glia.error/cap-call-failed` for IO, `:glia.error/type-mismatch` for non-string args, `:glia.error/arity-mismatch` for missing args).
- **Core capability schemas in `Export.schema` (#386, partial — Item 1a).** The membrane graft now populates `Export.schema` with canonical `Schema.Node` bytes for the five core capabilities (`identity`, `host`, `runtime`, `routing`, `http-client`). Bytes are extracted at build time by `crates/membrane/build.rs` via the existing `schema-id` pipeline and exposed through a new `membrane::schema_registry` module. Guest-side introspection (and the future MCP tool description generator) can now parse a real `Schema.Node` per cap instead of receiving an empty stub. Guest-contributed extras still get an empty schema — that's Item 1b (FHS-loaded `.capnpc` files).
- **Structured Glia errors (#389, Item 2).** New `glia::error` module with typed constructor functions (`unbound_symbol`, `arity`, `type_mismatch`, `cap_call`, `epoch_expired`, `permission_denied`, `parse`, `rpc`, `fuel_exhausted`, `internal`) that return canonical `Val::Map` errors with namespaced keyword keys (`:glia.error/type`, `:glia.error/message`, `:glia.error/hint`, plus variant-specific fields). Inspection accessors `data`, `message`, `type_tag`, and `hint` mirror Clojure's `ex-data`/`ex-message`. Eight high-value `eval_err!` call sites in `crates/glia/src/eval.rs` (`def`, `if`, `let`, `fn` parameter, `defmacro`, `map`, `filter`, `reduce`) migrated to typed constructors. Legacy `eval_err!` macro continues to work; remaining call sites will be swept during the upcoming effect-based exceptions migration.
- **MCP structured error envelope.** The MCP cell preserves the error `Val` end-to-end and serializes it into the JSON-RPC response: structured errors gain a `structuredContent` payload with the full schema map, prefixed text with the `:glia.error/type` tag, and the recovery hint inline. Plain-string (legacy) errors fall through unchanged.
- **Glia introspection builtins `(schema cap)`, `(doc cap)`, `(help cap)`.** Pure data-lookup builtins registered by the kernel after graft. `(schema cap)` returns the cap's canonical `Schema.Node` bytes (`Val::Bytes`), sourced from the build-time registry. `(doc cap)` returns a human-readable summary. `(help cap)` returns a multi-line cap reference. Backed by 10 unit tests covering each variant's success and structured-error paths.

### Removed
- **Pre-CidTree image-materialization API (`apply_mounts`, `merge_layers`, `MergedImage`, `ImageRoot`, `try_dag_merge`, `copy_merge`, `apply_local_layer`, `apply_local_mount`, `apply_ipfs_layer`, `resolve_target`).** Production switched to `resolve_mounts_virtual` in #416 and the materializing surface had zero non-test callers since. The merge algorithm itself (`dag_merge` + `merge_overlay_recursive`) is preserved, used by `resolve_mounts_virtual`. `merge_layers` was a deprecated `pub` wrapper; deleted outright. The dropped `apply_mounts`/`merge_layers` test cases were ported to `resolve_mounts_virtual` test cases (T2/T3 audit) — exercising the real `dag_merge` path against a Kubo daemon (CI provisions one; tests print "skipping: kubo not running" locally if the daemon isn't up).
- **`CellBuilder::with_image_root`, `Cell.image_root`, `ProcBuilder::with_image_root`, `ProcInit.image_root`, and the plain-mode preopen branch in `src/cell/proc.rs`.** Pre-#416 the host materialized a merged FHS into a `TempDir` and preopened it directly to the WASI guest. After #416 every production cell uses `CidTree`, but the `image_root` plumbing stayed in code as an unreachable fallback. Removed entirely. **`Cell::spawn` now requires a `CidTree`** and surfaces a documented `bail!()` if `with_cid_tree(...)` was not called on the builder, pointing the developer at `doc/capabilities.md` for the architecture. Regression test in `src/cell/executor.rs::tests::spawn_without_cid_tree_returns_documented_error`. Performance note: an all-local-disk boot (every layer a host directory) used to skip IPFS via `copy_merge`; it now always goes through `dag_merge`, paying one `ipfs add -r` per layer at startup. Sub-second on typical image sizes.
- **`eval_err!` macro and the `:fail` effect target.** The macro and its ~107 call sites in `crates/glia/src/eval.rs` are gone, swept to typed `GliaError` constructors as part of the effect-based exceptions migration. The `:fail` keyword as an effect target is gone too — only the four prelude lines that referenced it (rewritten in this PR). Other `:fail` uses in user data (e.g. `(:status :fail)`) are unaffected.
- **`Phase 2 design doc supersession.**` `doc/designs/glia-effects.md` Phase 2 description previously claimed `try` would keep `{:ok}/{:err}` shape over `:fail`; the actual landed design uses `:glia.exception` and dispatch-by-tag catch clauses. Doc updated.
- **`std/lib/ww/ipfs.glia`** — superseded by `ww/fs` (and `ww/routing` for `resolve`). The `ls` function in the old module called `(perform :fs-readdir path)`, an effect with no host-side handler — it never worked. The new `ww/fs` module wires through the existing `make_fs_handler` (formerly `make_ipfs_handler`) and adds the missing `:stat` / `:exists?` / `:read-str` methods. No internal consumers used `ww/ipfs`, so migration is zero-burden.
- **`make_ipfs_handler`** in `std/caps/src/lib.rs` — renamed to `make_fs_handler`. Same host-side mechanism (path resolution against `$WW_ROOT`, WASI-fronted reads), broader API.
- **IPFS `UnixFS` capability.** The `ipfs.capnp` schema, `ipfs_capnp` module, `UnixFS`/`UnixFSRef` structs, `ContentStore` trait, `MemoryStore` test double, and `CellBuilder::with_content_store()` are all gone. Guests no longer see an IPFS capability; content-addressed reads flow through the WASI virtual filesystem (`CidTree`). `HttpClient::cat()` and `HttpClient::get_dir()` remain as internal host helpers. The `content_store` field on `Cell` was unread — it has been deleted along with its builder setter. `IpfsUnixfsLoader` renamed to `IpfsLoader`.
- `tests/kernel_initd_integration.rs`: test exercised `with_content_store()` but the Cell never read that field, so the test was not actually verifying init.d behavior. Deleted; proper init.d coverage belongs on the CidTree path.

### Changed
- **`doc/architecture.md` "VFS & capability model" section gains a "Two host caches operate together" subsection** with an ASCII diagram showing how `PinsetCache` (raw bytes, ARC-managed, 128 MiB budget) and `CidTree.staging_dir` (per-process dir-listing stubs) divide responsibilities behind `fs_intercept`. Makes the WASI preopen story explicit: the preopen is a protocol anchor on `staging_dir`, not a guest-visible filesystem.
- **`CidTree::staging_dir()` doc comment** clarifies the function's invariant (the path WASI-preopens as `/`; never read by callers outside `fs_intercept`; contents are dir-listing stubs, not a stable filesystem view).
- **Capability-model docs (`doc/capabilities.md`, `doc/architecture.md`).** Docs caught up to the architecture that's now real in code: CID-as-capability, the three attenuation points (membrane graft / root Atom binding / Glia env bindings), `LocalOverride` as the principled exception for host-private files, revocation = epoch advance + respawn, structured Glia errors with `:glia.error/*` schema, the `(schema cap)` / `(doc cap)` / `(help cap)` introspection builtins, and "MCP = Glia eval" framing. Added a new "VFS & capability model" section to `architecture.md` with an ASCII diagram of `Atom → CidTree root → guest paths → content`, made it explicit that WASI preopens are a protocol detail rather than a security boundary, and updated the stale `--port` reference to `--listen`.
- **Glia capability rename: `ipfs` → `fs`.** The cap name `ipfs` is gone everywhere it appeared: `wrap_with_handlers` (caps crate), the kernel/shell/MCP cell handler wirings, and the cap list `["import", "routing", "fs", "host"]`. After PRs #415/#416, all content access flows through the content-addressed VFS — there's no separate "IPFS filesystem", and naming the cap `fs` matches reality. Existing init.d scripts that imported `ww/ipfs` need to switch to `ww/fs` (no internal consumers; external scripts must update).
- **Listen addresses are now hard-fail.** Previously, `ww run` bound IPv4 TCP as required and treated IPv6 TCP, IPv4 QUIC, and IPv6 QUIC as best-effort — bind failures (e.g. UDP port already in use) were silently downgraded to warnings, masking situations like two nodes coexisting with overlapping intent on the same port. Every requested listen address now must succeed; bind errors abort startup. To opt out of IPv6 or QUIC, pass an explicit `--listen` set instead of the defaults.
- **`--port` replaced with `--listen`.** `ww run` and `ww daemon install` no longer accept `--port`. Use `--listen <multiaddr>` (repeatable) or `WW_LISTEN=addr1,addr2` instead. Default unchanged: TCP+QUIC on IPv4+IPv6 at port 2025. Daemon config (`~/.ww/config.glia`): `:port 2025` is replaced by `:listen [...]` — old configs error on load with a hint to re-run `ww daemon install`.
- **Shell discovery:** `ww shell` now discovers local nodes via lockfiles in `~/.ww/run/` instead of querying Kubo's LAN DHT. Running nodes write their listen multiaddrs to `~/.ww/run/<peer_id>`. No Kubo dependency for local shell access. Interactive picker when multiple nodes are running.
- **Daemon fd limit:** launchd plist now sets `SoftResourceLimits/NumberOfFiles` to 4096 (was default 256), preventing fd exhaustion from DHT peer connections.
- **Kernel (pid0) log level:** default filter lowered to `Warn` so the operator console only sees warnings/errors. Info/debug/trace from init.d, vat registration, and runtime events no longer spam stdout by default.
- **Shell RPC handshake timeout:** raised from 10s to 30s to accommodate fresh cell spawns on cache-cold workers (WASM compile + membrane graft can take a few seconds).
- **Distribution architecture:** IPFS release tree restructured to follow Unix FHS conventions. No more full repo dump. Release tree contains only artifacts: `bin/` (WASM cells + host binary), `lib/ww/` (Glia stdlib), `lib/init.d/` (reference init scripts), `include/schema/` (.capnp source), `share/schema/` (.capnpc compiled type descriptors).
- **CI pipeline split:** Two independent lanes (std and host) trigger based on what changed. Deploy uses pre-built musl binary + WASM artifacts (~30s) instead of full Docker multi-stage build (~10 min). Cap'n Proto build extracted to composite action.
- **Compiled schema extension:** `.schema` files renamed to `.capnpc` (capnp-compiled), following the `.py`/`.pyc` pattern. Avoids naming collision with human-readable `.capnp` schema source files.
- `perform_upgrade` reads a `VERSION` file from the release tree instead of parsing `Cargo.toml`. Host binaries at `bin/ww/{os}/{arch}/ww` (was `bin/{os}/{arch}/ww`).
- `perform_update` publishes flat `bin/*.wasm` layout to IPNS (was `kernel/bin/main.wasm`, `shell/bin/shell.wasm`). No longer writes WASM to disk; embedded blobs served by EmbeddedLoader at runtime.
- Daemon mounts `~/.ww/` as a single root layer instead of separate `kernel/` and `shell/` layers.
- **Lint hygiene (#424).** Workspace-wide `cargo fmt` and `cargo clippy --fix` pass. Drops unnecessary `&mut` on `RecordingDispatch` refs in glia tests (`eval`/`eval_str`/`eval_blocking` take `&D`), uses the existing `NativeFnImpl` alias instead of an inline `Rc<dyn Fn(...)>` in one test, replaces a `paths.iter().map(|p| *p).collect()` with `paths.to_vec()` in `src/ns.rs`, collapses a single-arm `match` into `if let` in `tests/shell_e2e.rs`, and switches glia's float parse/display tests off the `3.14` literal (clippy::approx_constant treats it as PI). No behavior change.

### Fixed
- **`status` cell missing from `make std` and CI artifacts.** `build.rs` requires `std/status/bin/status.wasm` for release builds (added in #430), but the root `Makefile`'s `std:` target only built `kernel shell mcp`, and the workflow's `wasm-artifacts` upload list didn't include `status.wasm`. Result: every host-binary build matrix entry on master panicked with `Missing WASM files for embedding: std/status/bin/status.wasm`. Added `status` to the `Makefile`'s `std:` target with a parallel target stanza, added `std/status/bin/status.wasm` to the CI artifact upload, and added it to the IPFS release tree assembly so installer users get the cell too.
- **CI publish step silently no-op'd on kubectl errors.** The `Publish to IPFS via VPS` step ran an SSH-wrapped sequence of `kubectl cp` + `kubectl exec ipfs add/pin/publish` and piped the whole thing through `tee`. Because `tee` always exits 0, any failure inside (e.g. `kubectl cp` hitting `context deadline exceeded`, leaving `/tmp/release-tree` absent in the pod) was swallowed: every subsequent `ipfs` invocation failed with empty input, but the step exited 0 and the job was marked success. IPNS was never updated for several recent runs while CI claimed otherwise. Fixed by adding `set -e` inside the SSH script (so kubectl-exec failures propagate through the inner script), `set -o pipefail` in the outer step (so the SSH exit status reaches the runner past `tee`), and an explicit `[ -z "$CID" ] && exit 1` assertion after CID extraction so an empty publish fails loudly.
- **`ww run std/kernel` fails after install.** The install script only fetched the host binary; WASM cells and glia stdlib were never placed on disk. The mount resolver only checked CWD, so `std/kernel` was unresolvable from any directory other than the repo root. Fixed in three parts: (1) `resolve_source()` in `src/mount.rs` falls back to `~/.ww/<path>` when a relative mount source does not exist at CWD (resolution order: CWD → ~/.ww/ → unchanged); (2) `scripts/install.sh` now fetches kernel, shell, mcp WASM cells and glia stdlib into `~/.ww/std/` from the same IPFS release tree used for the binary; (3) `fetch_to()` cleans up empty files on `ipfs cat` failure to prevent ghost 0-byte artifacts.
- **CHECKSUMS.txt missing section headers.** The CI publish job wrote bare `sha256sum` output without `# sha256` / `# blake3` section headers, but `install.sh` greps for `^# sha256` to locate checksums — so verification was silently skipped for every IPNS installation. Added section headers matching the format used by `make publish`.
- **Swarm startup errors now reach the operator.** When `Libp2pHost::new` failed (e.g. `listen on /ip4/0.0.0.0/tcp/2025: address already in use`), the swarm thread dropped its readiness oneshot and the main thread reported only `Swarm service failed to start: channel closed`, hiding the real cause. The readiness channel now carries `Result<SwarmReady>`, so bind failures and other construction errors are forwarded verbatim with full context.
- **CI release pipeline ordering hazards (`.github/workflows/rust.yml`).**
  - **Concurrent runs raced on IPNS.** Added a workflow-level `concurrency` group (`ww-release-${{ github.ref }}`, `cancel-in-progress: false`) so master pushes serialize through deploy + IPNS publish in commit order. Previously two pushes in quick succession could land on the `ww-release` IPNS key out of order, silently rolling installer users back to an older release.
  - **`publish` raced `deploy`.** `publish` now `needs: [deploy]`, so installer-facing IPNS only flips after the VPS is serving the new code. Eliminates the window where `install.sh` could fetch a binary version different from what production is running.
  - **`host=false` republish silently degraded.** When std/ changed but host/ didn't, the publish job fetched previous binaries from IPNS via `ipfs name resolve` / `ipfs cat`, swallowing failures with `2>/dev/null || true`. A failed resolve now aborts the run instead of publishing a partial tree; a sanity loop verifies every platform binary exists and is non-empty before checksumming.
  - **`kubectl rollout restart` could pull a stale `:master` image.** Replaced with `kubectl set image deployment/ww-master '*=ghcr.io/wetware/ww:master-${{ github.sha }}'`, which pins the rollout to the SHA-tagged image. A new pre-deploy step also probes `docker manifest inspect` against GHCR (with backoff) so we don't kick a rollout before the registry replica has the manifest.
  - **DHT propagation gap after `ipfs name publish`.** The publish step now follows up with `ipfs routing provide -r $CID` (60s timeout) so external installers' `ipfs cat` doesn't stall waiting for provide records.
- **`/oracle` (and any init.d-registered HTTP route) returned "no handler":** `ww run` materialized the image to a tempdir and preopened it at `/` in the guest, but wasi-libc's absolute-path lookup through a `/` preopen was unreliable, so the kernel's `std::fs::read_dir("/etc/init.d")` failed and init.d scripts never ran. `(perform host :listen oracle "/oracle")` was silently skipped. Fixed by wiring the `CidTree` virtual filesystem (`resolve_mounts_virtual` + shared `PinsetCache`) through `run_with_mounts` so content-addressed reads route through `fs_intercept` instead of tempdir preopens. Also prefer CidTree over `open_ipfs` for paths referencing the current root CID so directory CIDs aren't mistaken for leaf blobs.
- **Install script `TMPDIR` collision:** `scripts/install.sh` shadowed the macOS system `TMPDIR` env var. If the script exited early (e.g. IPNS timeout), the cleanup trap ran `rm -rf` against the user's system temp directory (`/var/folders/.../T/`). Renamed to `WW_TMPDIR`.
- `ww perform install` now symlinks the binary to `~/.ww/bin/ww`. Previously the directory was created empty, leaving `ww` off the user's PATH.
- `ww perform install` now writes a default `50-shell.glia` init script to `~/.ww/etc/init.d/`. Previously no init scripts were installed, so the daemon had nothing to boot.
- `tests/discovery_integration.rs`: raised capnp-rpc timeouts from 10s to 60s so the two integration tests survive `cargo test`'s default parallel execution. Each test builds its own wasmtime Engine and triggers a fresh cranelift compile of the discovery component; under concurrent load the compile exceeded the old budget and `greet()` timed out before the cell served.

### Breaking
- Release tree layout changed. Users on old binaries must reinstall: `ww perform uninstall -y` then re-run the install script.

## [0.0.1.0] - 2026-04-12

### Added
- `ww perform update` refreshes WASM images, daemon config, service file, and MCP wiring to match the current binary. Safe to run repeatedly. Does not touch identity or directory structure.
- `ww shell` now auto-discovers local nodes via Kubo's LAN DHT when no address is given. The daemon advertises a well-known discovery CID; the shell queries Kubo's `findprovs` API to find it.
- `ww shell` accepts `/dnsaddr/` multiaddrs (e.g. `ww shell /dnsaddr/master.wetware.run`). Address is now a positional argument instead of `--addr`.
- Admin HTTP server (`--with-http-admin`) now exposes `GET /host/id` (peer ID) and `GET /host/addrs` (listen addresses). `MetricsService` renamed to `AdminService`.

### Changed
- `ww perform install` now detects an existing `~/.ww` and delegates to `perform_update` instead of re-running the full bootstrap. First-time install still creates directories, generates identity, provisions IPNS keys.
- `ww perform upgrade` now automatically runs `perform_update` after replacing the binary, so WASM images, daemon, and MCP wiring are refreshed without a manual step.
- WASM images use CID comparison (BLAKE3) instead of file-existence checks, so stale images from a previous install are always replaced.
- Daemon image layers no longer include the MCP cell. The MCP cell's `bin/main.wasm` was clobbering the kernel's entry point, causing the daemon to crash-loop.
- Outbound HTTP access for cells now requires explicit `--http-dial` flag. No flag means no `http-client` capability. Supports exact hosts, subdomain globs (`*.example.com`), and `*` for unrestricted access.
- Documentation overhaul: README rewritten with quick start, cell modes, AI integration, roadmap. CLI reference now covers all 12 commands. Architecture doc updated for `List(Export)` membrane, virtual WASI FS, state management, and distribution model.

### Fixed
- **`ww run std/kernel` fails after install.** The install script only fetched the host binary; WASM cells and glia stdlib were never placed on disk. The mount resolver only checked CWD, so `std/kernel` was unresolvable from any directory other than the repo root. Fixed in three parts: (1) `resolve_source()` in `src/mount.rs` falls back to `~/.ww/<path>` when a relative mount source does not exist at CWD (resolution order: CWD → ~/.ww/ → unchanged); (2) `scripts/install.sh` now fetches kernel, shell, mcp WASM cells and glia stdlib into `~/.ww/std/` from the same IPFS release tree used for the binary; (3) `fetch_to()` cleans up empty files on `ipfs cat` failure to prevent ghost 0-byte artifacts.
- **CHECKSUMS.txt missing section headers.** The CI publish job wrote bare `sha256sum` output without `# sha256` / `# blake3` section headers, but `install.sh` greps for `^# sha256` to locate checksums — so verification was silently skipped for every IPNS installation. Added section headers matching the format used by `make publish`.
- Daemon no longer crash-loops on startup. The MCP mount layer was overwriting the kernel binary; removed from daemon image layers.
- Kernel no longer crashes when `--http-dial` is not passed. The `http-client` capability is now optional in the membrane graft.
- `host :listen` now gives a clear error when passed an undefined variable instead of a cell (e.g. when `load` fails). Previously showed misleading "runtime capability required".
- IPFS release tree now includes all example WASM binaries (oracle, counter, chess, etc.), not just echo. Previously `make examples` built them but the CI artifact upload and publish steps dropped them, so `ww run /ipns/<key>/examples/oracle` failed with missing `bin/oracle.wasm`.

## [0.1.2] - 2026-04-12

### Changed
- Install is now a single command. `curl | sh` calls `ww perform install` automatically — no separate setup step. Identity, namespace, daemon, and MCP wiring all happen in one shot.
- `ww perform install` auto-starts the daemon via launchd (macOS) or systemd (Linux) instead of printing manual activation commands.
- "Publishing standard library" spinner renamed to "Indexing standard library" (it's a local IPFS operation, not a network publish).
- Post-install summary now shows `ww shell` as the next step.

### Fixed
- **`ww run std/kernel` fails after install.** The install script only fetched the host binary; WASM cells and glia stdlib were never placed on disk. The mount resolver only checked CWD, so `std/kernel` was unresolvable from any directory other than the repo root. Fixed in three parts: (1) `resolve_source()` in `src/mount.rs` falls back to `~/.ww/<path>` when a relative mount source does not exist at CWD (resolution order: CWD → ~/.ww/ → unchanged); (2) `scripts/install.sh` now fetches kernel, shell, mcp WASM cells and glia stdlib into `~/.ww/std/` from the same IPFS release tree used for the binary; (3) `fetch_to()` cleans up empty files on `ipfs cat` failure to prevent ghost 0-byte artifacts.
- **CHECKSUMS.txt missing section headers.** The CI publish job wrote bare `sha256sum` output without `# sha256` / `# blake3` section headers, but `install.sh` greps for `^# sha256` to locate checksums — so verification was silently skipped for every IPNS installation. Added section headers matching the format used by `make publish`.
- Checksum verification during install. CHECKSUMS.txt now includes both SHA-256 (universal) and BLAKE3 (when available) under labeled sections. The install script prefers BLAKE3 when `b3sum` is present, falling back to SHA-256. Previously only one format was written with no section markers, so verification silently failed on machines missing `b3sum`.

## [0.1.1] - 2026-04-10

### Added
- IPFS-first distribution: install script fetches binaries from `/ipns/releases.wetware.run` with gateway fallback. Install to `~/.ww/` directory convention.
- `ww oci import`: pull container images from IPFS and load into Docker/podman. Supports `--cid` for version pinning and `--stdout` for manual piping.
- `ww perform upgrade`: self-update via IPNS. Compares running version against Cargo.toml on IPNS, fetches platform binary, verifies blake3 checksum, atomic replace.
- CI `publish-ipfs` job: assembles repo working tree as IPFS release artifact with binaries by os/arch and container tar, pins to K8s Kubo pod via SSH proxy, publishes IPNS.
- DNSLink at `releases.wetware.run` for human-readable IPFS install path.
- `scripts/uninstall.sh` for clean removal of `~/.ww/`.
- Release stem TODO for future on-chain distribution anchoring.

### Removed
- `VERSION` file (redundant with `Cargo.toml` as single source of truth).

## [0.0.1.2] - 2026-04-10

### Added
- `ww run` now accepts `--ipfs-url` (env: `IPFS_API`) to configure the IPFS HTTP API endpoint. Defaults to `http://localhost:5001`. Enables k8s deployments where IPFS runs in a separate pod.

## [0.0.1.1] - 2026-04-09

### Fixed
- **`ww run std/kernel` fails after install.** The install script only fetched the host binary; WASM cells and glia stdlib were never placed on disk. The mount resolver only checked CWD, so `std/kernel` was unresolvable from any directory other than the repo root. Fixed in three parts: (1) `resolve_source()` in `src/mount.rs` falls back to `~/.ww/<path>` when a relative mount source does not exist at CWD (resolution order: CWD → ~/.ww/ → unchanged); (2) `scripts/install.sh` now fetches kernel, shell, mcp WASM cells and glia stdlib into `~/.ww/std/` from the same IPFS release tree used for the binary; (3) `fetch_to()` cleans up empty files on `ipfs cat` failure to prevent ghost 0-byte artifacts.
- **CHECKSUMS.txt missing section headers.** The CI publish job wrote bare `sha256sum` output without `# sha256` / `# blake3` section headers, but `install.sh` greps for `^# sha256` to locate checksums — so verification was silently skipped for every IPNS installation. Added section headers matching the format used by `make publish`.
- Container build: add `crates/schema-id` to Containerfile dependency cache layer and dummy source block, fixing workspace resolution failure during Docker builds.
- Container build: declare `wasm32-wasip2` target in `rust-toolchain.toml` so rustup installs it for whichever stable toolchain is active, fixing `can't find crate for core` during WASM compilation.

## [0.0.1.0] - 2026-04-08

### Added
- **NAT traversal stack**: AutoNAT v1/v2 client detects public reachability and promotes WAN Kad to server mode. Relay client obtains relayed addresses from Amino DHT peers (max 2 concurrent reservations with lifecycle tracking). DCUtR upgrades relayed connections to direct via hole-punching.
- **QUIC transport**: UDP-based connections alongside TCP, with ~80-90% hole-punch success rate vs ~30-40% for TCP only.
- **IPv6 dual-stack**: listens on both IPv4 and IPv6 for TCP and QUIC.
- **NAT status introspection**: `NatReachability` exposed via `NetworkState` for cells to query.
- `routing.resolve`: resolve IPNS names to `/ipfs/` paths via Kubo. Available as `(perform routing :resolve "/ipns/...")` in Glia and `(resolve ...)` in the `ww/ipfs` stdlib.

### Changed
- WAN Kad periodic re-bootstrap enabled (5-minute interval) to keep DHT routing table fresh.
- ClientSwarm (`ww shell`) gains relay + QUIC transport for dialing NATted nodes.
- TTY-aware log levels: interactive shell defaults to `ww=warn` (clean REPL), daemon/pipe defaults to `ww=info`. `RUST_LOG` overrides both.
- Kernel capability binding: membrane graft caps are now iterated directly instead of via a skip-list. `http-client` carries its real capnp client.

### Fixed
- **`ww run std/kernel` fails after install.** The install script only fetched the host binary; WASM cells and glia stdlib were never placed on disk. The mount resolver only checked CWD, so `std/kernel` was unresolvable from any directory other than the repo root. Fixed in three parts: (1) `resolve_source()` in `src/mount.rs` falls back to `~/.ww/<path>` when a relative mount source does not exist at CWD (resolution order: CWD → ~/.ww/ → unchanged); (2) `scripts/install.sh` now fetches kernel, shell, mcp WASM cells and glia stdlib into `~/.ww/std/` from the same IPFS release tree used for the binary; (3) `fetch_to()` cleans up empty files on `ipfs cat` failure to prevent ghost 0-byte artifacts.
- **CHECKSUMS.txt missing section headers.** The CI publish job wrote bare `sha256sum` output without `# sha256` / `# blake3` section headers, but `install.sh` greps for `^# sha256` to locate checksums — so verification was silently skipped for every IPNS installation. Added section headers matching the format used by `make publish`.
- Stop promoting unspecified addresses (0.0.0.0, ::) as external. Only routable addresses are advertised.
- Containerfile: add `linux-headers` for Cap'n Proto compilation on Alpine (fixes missing `<linux/futex.h>`).
- `discovery_integration` tests rewritten to use `ExecutorPool` (matching prod topology), fixing a deadlock.

### Removed
- Phantom `ipfs` capability from kernel. IPFS content access goes through WASI VFS only.

### Added
- **AI skill system**: `.agents/skills/` restructured as vendor-neutral skills with YAML frontmatter. `generate.sh` produces `.claude/skills/ww-*/SKILL.md` for native `/ww-*` slash commands. Archived skills revived. Embedded encyclopedia extracted to `doc/ai-context.md`. `make agent-skills` target.
- **Namespace resolution**: `ww` standard library ships as an IPFS UnixFS tree under an IPNS name. On boot, namespaces configured in `etc/ns/` are resolved and mounted as FHS layers. Local dev builds fall back to HostPathLoader.
- `ww ns` CLI: `list`, `add`, `remove`, `resolve` subcommands for managing namespaces.
- `make publish-std` target: assembles the `ww` namespace tree, publishes to IPFS, writes CID for embedding in the host binary.
- `ww doctor` now checks namespace config and Kubo daemon reachability.
- `ww perform install` writes `~/.ww/etc/ns/ww` with the build-time bootstrap CID, and checks for Kubo availability.
- Glia standard library (`std/lib/ww/`): 7 Clojure-aligned modules — core (combinators, threading macros), string, coll (collections), test (framework), json, ipfs, evm. Imported via `(perform import "ww/core")`.
- `crates/README.md` and updated `std/README.md` documenting the std/ vs crates/ split.
- Release pipeline: 4-platform binary matrix (linux x86_64/aarch64, macOS x86_64/aarch64) with GitHub Actions
- Multi-arch container images (linux/amd64 + linux/arm64) pushed to ghcr.io/wetware/ww
- Cosign keyless container image signing via GitHub OIDC
- CHECKSUMS.txt with multihash (BLAKE3 + SHA2-256) and raw sha256sum-compatible section
- Install script (`scripts/install.sh`): one-liner installer with OS/arch detection and checksum verification
- `Cross.toml` for Cap'n Proto cross-compilation on aarch64-unknown-linux-gnu
- `Containerfile.release` for fast container builds from pre-built binaries
- Install script test suite (`tests/test_install.sh`)

### Fixed
- **`ww run std/kernel` fails after install.** The install script only fetched the host binary; WASM cells and glia stdlib were never placed on disk. The mount resolver only checked CWD, so `std/kernel` was unresolvable from any directory other than the repo root. Fixed in three parts: (1) `resolve_source()` in `src/mount.rs` falls back to `~/.ww/<path>` when a relative mount source does not exist at CWD (resolution order: CWD → ~/.ww/ → unchanged); (2) `scripts/install.sh` now fetches kernel, shell, mcp WASM cells and glia stdlib into `~/.ww/std/` from the same IPFS release tree used for the binary; (3) `fetch_to()` cleans up empty files on `ipfs cat` failure to prevent ghost 0-byte artifacts.
- **CHECKSUMS.txt missing section headers.** The CI publish job wrote bare `sha256sum` output without `# sha256` / `# blake3` section headers, but `install.sh` greps for `^# sha256` to locate checksums — so verification was silently skipped for every IPNS installation. Added section headers matching the format used by `make publish`.
- **Security:** Identity private key no longer exposed to WASM guests. `resolve_identity()` reads identity directly from `--identity` path, never from the merged FHS tree (which is preopened to guests via WASI and published to IPFS).
- MCP wiring uses absolute binary path via `current_exe()`, fixing PATH ambiguity.
- `claude mcp add/remove` now handles idempotent exit codes (server already exists / not found) without erroring.

### Changed
- Cap'n Proto crates bumped from 0.23 to 0.25 (capnp, capnp-rpc, capnpc). Generated code now uses `GeneratedCodeArena`, removing `unsafe` from all `*_capnp.rs` output.
- **Breaking:** Kernel crate moved from `crates/kernel/` to `std/kernel/`. Now a standalone workspace-excluded crate (like shell/mcp). All path references updated.
- Moved release builds from `rust.yml` to dedicated `release.yml` workflow
- `ww perform install` suppresses internal daemon_install noise, shows clean checklist output.
- Daemon plist/systemd unit passes `--identity` as a CLI flag instead of a `path:/etc/identity` mount.
- `~/.ww/{kernel,shell,mcp}/bin/` image roots created and populated with embedded WASM on install.

## [0.0.5.0] - 2026-04-06

### Added
- `crates/stem/` crate: `StemSource` async trait abstracting epoch sources, enabling both on-chain (Atom contract) and off-chain (IPNS) epoch anchors behind a common interface.
- `AtomSource`: wraps existing `AtomIndexer` + `Finalizer` behind the `StemSource` trait.
- `IpnsSource`: polls IPNS names via IPFS HTTP API, emitting `Provenance::Timestamp` epochs for off-chain deployments.
- `StemEvent`: backend-agnostic epoch event type for the shared pin/swap/broadcast pipeline.
- `HttpClient.name_resolve()` and `name_publish()` for IPNS operations via IPFS HTTP API.

### Changed
- **Breaking:** `Epoch.adopted_block` replaced with `Epoch.provenance: Provenance` enum (variants: `Block(u64)` for on-chain, `Timestamp(u64)` for off-chain). Cap'n Proto schema updated with union-based provenance and literate documentation.
- `src/epoch.rs` refactored: `handle_epoch_advance()` is now source-agnostic, shared by all epoch backends. Legacy `run_epoch_pipeline()` preserved for backward compatibility.

## [Unreleased]

### Added
- `ww perform install`: full bootstrap (dirs, identity, daemon, MCP wiring, summary).
- `ww perform uninstall`: clean removal of daemon, MCP config, optional ~/.ww deletion.
- `EmbeddedLoader`: WASM images embedded in binary via `include_bytes!()` for pre-built distribution. ChainLoader priority: HostPath > Embedded > IPFS (local files override embedded for hot-patching).
- MCP tool responses now include `[CID: ...]` content hash for provenance tracking. CID computed from WASM bytecode via blake3, passed to guest via `WW_CELL_CID` env var.
- `ww doctor` install state checks: identity, daemon running, Claude Code MCP configured.
- CI: WASM images built before host binary (embedded via include_bytes). Container pushed to ghcr.io.
- `.agents/skills/mcp-quickstart.md`: 3-minute quickstart demonstrating provenance and capability security.
- build.rs: clear compile error when WASM files missing in release mode, empty stubs in debug mode.

### Changed
- `ww perform install` now generates identity at `~/.ww/identity` (was `~/.ww/etc/identity`).
- CI container registry switched from private registry to ghcr.io (uses GITHUB_TOKEN).
- `.agents/skills/onboard-new-user.md` rewritten for binary-first install flow.

### Removed
- `VERSION` file (version source of truth is now `Cargo.toml` only).
- `.agents/skills/{explain-concepts,browse-reference,study-examples}.md` moved to archive.

### Changed
- Val::Map now backed by persistent CHAMP trie (im::HashMap) via ValMap wrapper. O(1) clone, O(log N) insert, O(1) lookup for maps with 32+ entries (77-92% improvement). ValMap is the future seam for IPLD-backed persistent maps.
- `extract_method` moved to glia crate (was duplicated in kernel and caps), returns `(&str, &[Val])` instead of `(String, Vec<Val>)` for zero-alloc capability dispatch.
- Fn/Macro equality: `Rc::ptr_eq` identity semantics (was always-false). Functions are now equal to themselves.
- Hash + Eq implemented for Val (enables use as map keys, set elements).

### Added
- `AtomicBloom`: lock-free bloom filter for ARC cache (100K capacity, 0.001% FPR, ~244KB). `PinsetCache::probably_cached()` public API for lock-free presence checks.
- `doc/designs/fuel-scheduling.md`: EWMA ratio estimator design doc.
- Criterion benchmarks for streams, glia map, ARC cache, kernel dispatch.
- ARC cache hit/miss/eviction Prometheus counters.
- Tracing spans on RPC listeners.

### Added
- Epoch-bound Terminal login: challenge-response now signs `nonce || epoch_seq` (16 bytes), preventing both same-epoch and cross-epoch replay attacks
- Graceful epoch shutdown: configurable drain delay before epoch broadcast (SIGTERM/SIGKILL model for in-flight operations)
- `doc/replay-protection.md`: four-layer defence model documentation (domain separation, epoch binding, epoch guards, on-chain finality)

### Changed
- `Signer.sign()` now takes `(nonce, epochSeq)` instead of just `(nonce)` — old clients will fail auth (correct behavior)
- TerminalServer requires `epoch_rx: watch::Receiver<Epoch>` at construction
- EpochService accepts `drain_duration: Duration` (default 1s via `--epoch-drain-secs`)

## [0.0.5.0] - 2026-04-06

### Added
- Terminal login unit tests: matching epoch, wrong epoch_seq, epoch-advance race condition
- Epoch drain delay unit tests: deferred broadcast timing and zero-drain regression
- `EpochAdvancingSigner` test helper for simulating epoch races during auth

### Changed
- SigningDomain payload_type renamed from `/{domain}/nonce` to `/{domain}/challenge` (reflects 16-byte `nonce || epoch_seq` payload)

### Added
- `PollSet` for multiplexing extra WASI pollables (stdin, listeners) alongside RPC in guest poll_loop
- `system::run_with(poll_set, f)` entry point for guests needing concurrent async I/O
- `PollLoopExit` with cycle counter for diagnosing RPC connection drops
- 51 new tests: MCP tool dispatch, caps effect handlers, ByteStream I/O, HttpClient allowlist, membrane E2E
- `ww perform install` — bootstrap ~/.ww user layer (boot, bin, lib, etc/init.d). Idempotent.
- `ww doctor` — environment health check (rustc, cargo, wasm32-wasip2, optional Kubo/Ollama)
- MCP dynamic tools: per-capability MCP tools generated from membrane graft (host, routing, runtime, identity, http-client, import). Each tool has per-action parameter schemas. eval remains primary interface.
- MCP security: input escaping (glia_escape), action allowlisting, no generic tools for unknown capabilities
- TODOS.md: Export.schema population, MCP resources, MCP prompts, eval error improvements

### Changed
- MCP cell: async stdin via StreamReader + PollSet (fixes host:peers connection drop)
- MCP cell: tools/list returns per-cap tools dynamically instead of single static eval tool
- .agents/prompt.md: document MCP tools and ~/.ww workflow for AI agents
- Extract shared effect handlers into std/caps crate (shell + MCP share, no duplication)
- Rename NamedCap to Export in stem.capnp (membrane exports capabilities)
- Export: use Capability + Schema.Node types instead of AnyPointer + Data
- Membrane.graft() returns `List(Export)` instead of named typed fields; capabilities looked up by name
- Guest runtime: unify three duplicate poll loops (drive_rpc_only, drive_rpc_with_future, block_on) into a single generic `poll_loop<T>()`
- Guest runtime: replace `futures::noop_waker`/`poll_unpin` with `std::task::Waker::noop()`/`Pin::new().poll()`
- Glia effect handler: simplify state machine (factor out repeated handler stack push, remove no-op match)

### Fixed
- **`ww run std/kernel` fails after install.** The install script only fetched the host binary; WASM cells and glia stdlib were never placed on disk. The mount resolver only checked CWD, so `std/kernel` was unresolvable from any directory other than the repo root. Fixed in three parts: (1) `resolve_source()` in `src/mount.rs` falls back to `~/.ww/<path>` when a relative mount source does not exist at CWD (resolution order: CWD → ~/.ww/ → unchanged); (2) `scripts/install.sh` now fetches kernel, shell, mcp WASM cells and glia stdlib into `~/.ww/std/` from the same IPFS release tree used for the binary; (3) `fetch_to()` cleans up empty files on `ipfs cat` failure to prevent ghost 0-byte artifacts.
- **CHECKSUMS.txt missing section headers.** The CI publish job wrote bare `sha256sum` output without `# sha256` / `# blake3` section headers, but `install.sh` greps for `^# sha256` to locate checksums — so verification was silently skipped for every IPNS installation. Added section headers matching the format used by `make publish`.
- Shell cell: missing import handler (broke all eval with "target must be a keyword or cap")
- Shell E2E tests: WASM path mismatch (tests were silently skipping)

### Removed
- Dead code: `RpcDriver`, `DriveOutcome`, `drive_until`, `block_on` (zero callers)

### Added
- Glia: `(def m (perform import "path"))` loads and caches modules as a capability-gated effect
- `ww run --mcp`: MCP server cell (std/mcp/) with shared caps crate, Claude Code integration
- Auction example: HTTP/WAGI endpoint at /auction (curl-able JSON status)
- `HttpClient.post()`: outbound HTTP POST capability for WASM guests (domain-scoped, epoch-guarded)
- Mindshare schema + project scaffold: symmetric p2p context sharing for LLMs (`examples/mindshare/`)
- Glia shell: `(perform auction :compare)` discovers providers and compares fuel prices
- `--metrics-addr` flag: optional Prometheus metrics endpoint for fuel observability
- Fuel auction example: ComputeProvider vat cell with RFQ protocol
- `doc/guest-runtime.md`: design spec for the hand-rolled single-threaded async runtime
- `FuelPolicy` schema: `Executor.spawn()` accepts a fuel allocation policy (scheduled or oneshot)
- `FuelEstimator::new_oneshot()`: spawn cells with fixed fuel budgets that trap at exhaustion
- `Identity.verify()`: Ed25519 signature verification on the membrane (symmetric with sign)
- Init.d scripts for auction, echo, counter, and mindshare examples (all 7 examples now bootable)

### Fixed
- **`ww run std/kernel` fails after install.** The install script only fetched the host binary; WASM cells and glia stdlib were never placed on disk. The mount resolver only checked CWD, so `std/kernel` was unresolvable from any directory other than the repo root. Fixed in three parts: (1) `resolve_source()` in `src/mount.rs` falls back to `~/.ww/<path>` when a relative mount source does not exist at CWD (resolution order: CWD → ~/.ww/ → unchanged); (2) `scripts/install.sh` now fetches kernel, shell, mcp WASM cells and glia stdlib into `~/.ww/std/` from the same IPFS release tree used for the binary; (3) `fetch_to()` cleans up empty files on `ipfs cat` failure to prevent ghost 0-byte artifacts.
- **CHECKSUMS.txt missing section headers.** The CI publish job wrote bare `sha256sum` output without `# sha256` / `# blake3` section headers, but `install.sh` greps for `^# sha256` to locate checksums — so verification was silently skipped for every IPNS installation. Added section headers matching the format used by `make publish`.
- Example Makefiles: `make -C examples/foo` works from project root (CARGO variable)
- Oracle init.d: replace invalid `(with ...)` syntax with `(def http ...)` cap binding
- Counter example: remove stale schema-inject step (removed in #313)
- Shell cell: zero warnings (fix unused mut, duplicate build_dispatch call, allow dead_code on scaffolding)

## [0.0.4.1] - 2026-04-03

### Fixed
- **`ww run std/kernel` fails after install.** The install script only fetched the host binary; WASM cells and glia stdlib were never placed on disk. The mount resolver only checked CWD, so `std/kernel` was unresolvable from any directory other than the repo root. Fixed in three parts: (1) `resolve_source()` in `src/mount.rs` falls back to `~/.ww/<path>` when a relative mount source does not exist at CWD (resolution order: CWD → ~/.ww/ → unchanged); (2) `scripts/install.sh` now fetches kernel, shell, mcp WASM cells and glia stdlib into `~/.ww/std/` from the same IPFS release tree used for the binary; (3) `fetch_to()` cleans up empty files on `ipfs cat` failure to prevent ghost 0-byte artifacts.
- **CHECKSUMS.txt missing section headers.** The CI publish job wrote bare `sha256sum` output without `# sha256` / `# blake3` section headers, but `install.sh` greps for `^# sha256` to locate checksums — so verification was silently skipped for every IPNS installation. Added section headers matching the format used by `make publish`.
- Chess and discovery examples: add missing `http.capnp` to build (required by stem.capnp import)
- Chess example: remove stale IPFS graft dependency, replay logging is now local
- Remove unused `ipfs_capnp` module from chess and discovery (stem.capnp doesn't import it)

## [0.0.4.0] - 2026-04-03

### Added
- `ww shell` CLI: connect to a running node and evaluate Glia expressions remotely
- Shell cell (`std/shell/`): WASM guest that evaluates Glia over Cap'n Proto RPC
- `Shell.eval()` interface in `shell.capnp`: send text, get result + error flag
- Client-mode libp2p swarm (`ClientSwarm`): identify + stream only, no listeners
- Shell init.d registration via VatListener spawn mode
- Prelude loaded at shell cell startup (when, and, or, defn, cond, with)
- Cap handlers for host (:id, :addrs, :peers), routing (:provide, :hash), ipfs (:cat, :ls)
- rustyline REPL with 30s eval timeout and Ctrl-D/exit support

## [0.0.3.0] - 2026-04-03

### Added
- Capability threading: `with` block grants in init.d scripts now flow into spawned cells' membranes as `extras` in the graft response
- `NamedCap` schema type for forwarding type-erased named capabilities across the spawn pipeline
- `Membrane.graft()` returns an `extras` field containing init.d-scoped capability grants
- `VatListener.listen()` and `Executor.spawn()` accept optional `caps` parameter for capability forwarding
- Dual-transport cell registration: one binary can serve both vat RPC (libp2p) and HTTP/WAGI from a single init.d script
- `with` prelude macro for capability grant bindings in glia scripts
- `Val::Cell` type: bundles wasm + schema + captured capabilities from lexical scope
- `cell` builtin: constructs Cell values, scanning the environment for `Val::Cap` bindings
- `(perform host :new-http-client)` returns an HttpClient capability to glia scripts
- `(perform host :listen <cell>)` for VatListener and `(perform host :listen <cell> "/path")` for HttpListener
- Oracle example HTTP mode: stateless per-request JSON endpoint via `curl`

## [0.0.2.0] - 2026-04-02

### Added
- Ratio-based EWMA fuel estimator replacing the binary AIMD scheduler. Tracks consumed/budget ratio via exponential moving average, sizes budgets inversely: I/O-bound cells get large budgets, compute-heavy cells get small ones.
- `src/sched.rs` module with shared scheduling constants (fuel limits, yield interval, epoch tick rate)
- Epoch-based refueling via `epoch_deadline_callback`. Compute-bound cells that don't make host calls get refueled every 10ms, preventing `Trap::OutOfFuel`. The epoch callback only updates the EWMA for cells with zero host calls that epoch, avoiding false observations for I/O cells that straddle epoch boundaries.
- Epoch tick task on executor worker 0 (calls `Engine::increment_epoch()` every 10ms on the shared Engine)
- Shared `Arc<Engine>` in `ExecutorPool` with `engine()` accessor for callers

### Changed
- AIMD fuel scheduler (`FuelScheduler`) replaced by `FuelEstimator` in `ComponentRunStates`
- Fuel budget is now the scheduling quantum: larger budget = higher effective priority
- Wasmtime engine config now enables `epoch_interruption(true)` alongside existing fuel support
- Call hook logs EWMA ratio alongside budget for observability

### Removed
- AIMD constants (`ADDITIVE_INCREMENT`, `DECREASE_FACTOR_NUM/DEN`)
- Binary 50% threshold classification (replaced by continuous ratio tracking)

## [0.0.5.0] - 2026-04-02

### Added
- `Runtime` capability with system-wide WASM compilation caching (BLAKE3-keyed, shared across all cells)
- `--runtime-cache-policy` CLI flag (`shared`/`isolated`, default `shared`, env `WW_RUNTIME_CACHE_POLICY`)
- `Executor.spawn(args, env)` now accepts per-request arguments and environment variables
- WAGI cells receive proper CGI env vars (`REQUEST_METHOD`, `PATH_INFO`, etc.) at spawn time

### Removed
- Old `Executor` interface (runBytes, echo, bind)
- `BoundExecutor` interface (collapsed into new `Executor`)
- `Host.executor` method (Runtime comes from membrane graft, not Host)
- Glia shell `(perform executor :echo ...)` command

### Changed
- Membrane graft returns `runtime :Runtime` instead of `executor :Executor`
- `Runtime.load(wasm)` is the OCAP attenuation boundary: returns a scoped `Executor` bound to one binary
- One RuntimeImpl per worker thread, shared across all cells via client cloning
- Listeners (StreamListener, VatListener, HttpListener) take `Executor` instead of `BoundExecutor`
- Kernel `:run` and `:listen` handlers use two-step `runtime.load()` → `executor.spawn()` pattern
- Cap'n Proto pipelining resolves `load()` → `spawn()` in one round-trip
- All documentation, agent prompts, example READMEs, and init.d scripts updated for Runtime API

## [0.0.4.0] - 2026-04-02

### Added
- Lazy virtual filesystem (`CidTree`) resolves guest paths through IPFS directory DAGs on demand
- 3-tier directory listing cache: in-memory LRU → staging disk → IPFS daemon
- `resolve_mounts_virtual()` produces a merged root CID without materializing files
- Atomic root-CID swap via `ArcSwap` for epoch updates (FS swap happens-before capability death)
- Pre-warm root directory listing before epoch swap

### Removed
- IPFS capability (`ipfs` field) from membrane graft response (stem.capnp)
- `EpochGuardedIpfsClient` and `EpochGuardedUnixFS` from host RPC layer
- 7 IPFS capability tests (replaced by VFS tests in fs_intercept and vfs modules)

### Changed
- Kernel ipfs handler reads through WASI virtual FS instead of Cap'n Proto RPC
- `(perform ipfs :cat path)` and `(perform ipfs :ls path)` now use `std::fs`
- `(perform ipfs :add)` returns error (deferred to stem contract)
- Kernel boot sequence (`run_initd`) uses WASI `read_dir` + `read` instead of IPFS ls/cat
- One filesystem surface: all guest content access goes through WASI virtual FS

## [0.0.3.3] - 2026-04-02

### Fixed
- **`ww run std/kernel` fails after install.** The install script only fetched the host binary; WASM cells and glia stdlib were never placed on disk. The mount resolver only checked CWD, so `std/kernel` was unresolvable from any directory other than the repo root. Fixed in three parts: (1) `resolve_source()` in `src/mount.rs` falls back to `~/.ww/<path>` when a relative mount source does not exist at CWD (resolution order: CWD → ~/.ww/ → unchanged); (2) `scripts/install.sh` now fetches kernel, shell, mcp WASM cells and glia stdlib into `~/.ww/std/` from the same IPFS release tree used for the binary; (3) `fetch_to()` cleans up empty files on `ipfs cat` failure to prevent ghost 0-byte artifacts.
- **CHECKSUMS.txt missing section headers.** The CI publish job wrote bare `sha256sum` output without `# sha256` / `# blake3` section headers, but `install.sh` greps for `^# sha256` to locate checksums — so verification was silently skipped for every IPNS installation. Added section headers matching the format used by `make publish`.
- All documentation uses correct `(perform cap :method ...)` Glia syntax
- Removed last stale references to schema-inject and custom sections from embedded context
- Example READMEs match actual init.d scripts (schema arg, subcommands)

## [0.0.3.2] - 2026-04-02

### Changed
- Cell guests dispatch on subcommands instead of envvars: no args = cell mode, `serve` / `consume` for application roles
- Init.d scripts only register cells; user starts services from the Glia shell with `(perform executor :run wasm "serve")`
- `WW_CELL_MODE` envvar is now informational only (set by kernel, not used for dispatch)
- `WW_CELL=1` envvar removed entirely

## [0.0.3.1] - 2026-04-02

### Changed
- All example READMEs rewritten with consistent hand-holding structure

## [0.0.3.0] - 2026-04-02

### Added
- `ipfs :add` handler: `(perform ipfs :add <bytes>)` returns CID
- `ww init <name>` scaffolds typed cell guest projects
- `ww build` places artifacts in bin/ (wasm + schema)
- Oracle example README
- Init.d scripts for all examples (chess, discovery, oracle, echo, counter)

### Changed
- Kernel reads schema from explicit RPC params (not WASM custom sections)
- Example Makefiles simplified (no schema-inject step)

### Removed
- `schema-inject` binary and `inject` feature from schema-id crate
- WASM custom section extraction from kernel
- Cell-building functions from schema-id (build_cell_capnp_message, etc.)
- ~900 lines of custom section infrastructure and tests

## [0.0.2.0] - 2026-04-01

### Added
- Price oracle demo — end-to-end multi-agent example with capability-scoped HTTP, Cap'n Proto RPC, and DHT discovery (#171)
  - `HttpClient` capability for domain-scoped outbound HTTP
  - `WagiService` — axum HTTP server on dedicated OS thread with channel-based CGI dispatch
  - `VatListener.serve()` for persistent capability export (no per-connection cell spawning)
  - `VatHandler` union: `spawn` (BoundExecutor) vs `serve` (AnyPointer) in system.capnp
  - `HttpListener` with `RouteRegistry` bridging axum threads to Cap'n Proto event loops
  - `--http-listen` CLI flag for enabling the HTTP server
  - Oracle example guest: dual-mode WASM binary (service + consumer), Blocknative gas price feed, schema CID pipeline, 7 unit tests
  - `AuthPolicy` trait stub for pluggable authentication (Terminal challenge-response)

## [0.0.1.1] - 2026-04-01

### Changed
- AIMD fuel scheduler uses classic `budget * 3/4` decrease instead of `consumed * 3/4`. Smoother convergence, no oscillation for guests alternating between I/O and compute.

## [0.0.1.0] - 2026-04-01

### Changed
- Every cell type now gets membrane RPC and WIT data_streams. HTTP/WAGI cells can access host capabilities (IPFS, routing, identity) through the WIT side-channel while using stdin/stdout for CGI I/O. No more "lightweight" cells that miss out on the capability system.
- One spawn path for all cell types. The `lightweight` flag and `new_lightweight()` are gone. Cell types are differentiated by stdin/stdout semantics, not by which host plumbing they get.
- Vat cells use stdin as a shutdown signal: closing stdin tells the cell to drain gracefully. No bytes are ever written (equivalent to Go's `<-chan struct{}`). `handle_vat_connection` closes stdin on all exit paths (peer disconnect, bootstrap timeout, capability extraction failure) to prevent orphaned processes.

## [Unreleased]

### Added
- Thread-per-subsystem runtime inspired by Cloudflare Pingora (#302)
  - Each subsystem (libp2p swarm, epoch pipeline, WASM executor) runs on its own OS thread with its own single-threaded tokio runtime
  - `Service` trait + `Host` supervisor for lifecycle management and coordinated shutdown
  - `ExecutorPool` with M:N cell scheduling: N worker threads, each `current_thread` + `LocalSet`, least-loaded assignment with round-robin fallback
  - `SwarmService` and `EpochService` run on dedicated threads, isolated from cell execution
  - `--executor-threads` CLI flag (0 = auto-detect CPU cores)
  - Kernel cell runs inside ExecutorPool instead of on the CLI thread
  - `SpawnRequest` struct with cell name, factory, and optional result channel for exit code piping
  - Per-cell tracing spans for readable multi-cell `RUST_LOG` output
  - Cell panic detection and logging via JoinHandle monitoring
  - `CompilationService` stub for off-thread WASM compilation with blake3-keyed cache
  - Bounded spawn channel (depth 64) with `try_send` to prevent self-deadlock
  - 14 unit tests covering host lifecycle, executor pool scheduling, round-robin distribution, panic handling, exit code piping, and bounded channel backpressure

### Changed
- `spawn_rpc_inner` and child cell spawn paths use ambient `LocalSet` instead of nested `LocalSet`, enabling proper M:N cooperative scheduling across cells on the same worker thread
- `SwarmService` and `EpochService` now respect shutdown signal via `tokio::select!`
- `ExecutorPool` stores worker `JoinHandle`s and joins them on drop for clean shutdown
- Process.kill() RPC for cell termination (#305)
  - Kill signal via watch channel, exit code 137 (SIGKILL convention)
  - Both lightweight and full spawn paths support kill via `tokio::select!`
- Lightweight spawn path for ephemeral cells (#305)
  - Skips membrane/RPC setup for WAGI/CGI cells, reducing per-request overhead
- Prerequisite spikes for thread-per-subsystem runtime (#306, refs #302)
  - Spike 1: Two cells interleave on shared LocalSet via fuel yields
  - Spike 2: Two Cap'n Proto RPC systems coexist on shared LocalSet
  - Spike 3: Off-thread WASM compilation (267x speedup via serialize/deserialize)
  - Bonus: current_thread runtime in std::thread (worker thread topology)
- WAGI adapter and guest crate for WAGI cells (#304)
  - `WagiAdapter` with `build_cgi_env()` and `parse_cgi_response()` (16 unit tests)
  - `wagi-guest` crate: zero-dependency helper library for WAGI cells
  - Counter example rewritten from 305 lines of FastCGI to 32 lines using `wagi-guest`
- AIMD fuel scheduler for cooperative M:N cell scheduling (#303)
  - Additive-increase multiplicative-decrease fuel budgeting at wasmtime host call boundaries
  - Cells yield every 10K instructions; I/O-efficient cells converge to 10M fuel, compute-heavy to 10K
  - 10 unit tests for FuelScheduler convergence and clamping behavior
