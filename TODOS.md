# TODOs

## Recursive attenuation for named vat services — SHIPPED 2026-07
**What:** Done via the single-authority capability model (eng review 2026-07-17; PRs #563–#568 plus the attenuate-reification PR). `(attenuate cap [:method ...])` reifies into a hook-level `wetware-membrane` allowlist keyed by `(interfaceId, ordinal)`; the policy travels with the capability through export/serve-vat, nested attenuation intersects into a single membrane layer, and denials fail closed with structured errors. See `doc/designs/single-authority-capability-model.md`.
**Remaining:** attenuating schema-less caps (e.g. dialed generic caps) is fail-closed pending the deferred schema-association design (D24); the defcap-export bridge item carries the reification invariant for pure-Glia caps.
**Priority:** —

## defcap-export bridge (GliaCapInner → capnp server)
**What:** Let a pure-Glia `defcap` capability cross a process/vat boundary by bridging its method table to a capnp server. This is the single remaining place where the Glia/capnp capability split could re-open, so it is a committed future workstream, not a maybe. Binding invariants (from the 2026-07-17 review, D28, recorded in `doc/designs/single-authority-capability-model.md`): (1) the exported cap crosses the boundary as an ordinary capnp capability governed by the SAME hook-level membrane as every other cap — no second enforcement path; (2) `attenuate` on such a cap reifies into the membrane exactly as for grafted caps; (3) it reuses `crates/membrane` — anything the crate can't express is a signal to fix the crate, not to fork a path.
**Why:** Until built, defcap caps remain cell-local (fully usable via `perform`), which is why deferral is safe. Gate to build it: a concrete consumer needs a Glia-defined cap to cross a boundary.
**Effort:** L
**Priority:** P1
**Depends on:** single-authority capability model (shipped 2026-07)

## Reconsider a lightweight in-process isolation/attenuation primitive (ex-`isolate`)
**What:** `isolate` was removed 2026-07 (weak isolation-vs-attenuation separation; confinement leak unfixable under dynamic effect scope). Revisit only if a concrete need arises that neither the capnp membrane (attenuation) nor a spawned cell (isolation) serves, and only with membrane-backed cap authority (a cap carries its granted authority explicitly, independent of definition-site and call-site handler stacks).
**Priority:** P3

## AutoNAT v2: expose per-address reachability (follow-up after node-level parity)
**What:** Extend runtime network state to expose per-address reachability outcomes from AutoNAT v2 probes, instead of only the node-level `NatReachability` enum.
**Why:** Node-level state is enough for current relay/Kad policy, but operators and future policy layers may need richer diagnostics (which address was tested, by which server, and why it failed).
**Context:** Current milestone intentionally keeps external API stable and projects v2 signals into node-level transitions with hysteresis. This follow-up should evaluate adding a structured address-level surface without destabilizing existing consumers.
**Effort:** M
**Priority:** P3
**Depends on:** AutoNAT v2 node-level parity wiring

## Status cell: host.peers() blocks for ~20s on first request
**What:** The std/status cell's first GET response takes ~20 seconds before returning. After that, subsequent responses are presumably fast (didn't measure). The latency is from `host_peer_count()` (`std/status/src/lib.rs:110-114`) calling `host.peers_request().send().promise.await` which blocks until the libp2p swarm has populated peer counts. On a freshly-deployed pod the swarm needs time to bootstrap to 300+ peers, so the first `host.peers()` call sits there.
**Why:** Discovered while verifying the snap-hello-rs deploy on master.wetware.run (lthibault/ipns-mount-fix branch, 2026-05-04). curl to `/status` timed out at 15s; bumping curl timeout to 30s revealed the response did eventually return (200, peer_count=317, time_total=20.4s). The snap cell next door responds instantly because it doesn't make host calls. The 20s latency is invisible during normal operation but pathological at cold-start: any monitoring or readiness check that hits /status with a sub-15s timeout will flap.
**Context:** Three plausible fixes, in order of effort:
  1. **Bound the timeout in the cell.** Wrap `host_peer_count` (and `host_id`, `host_addrs`) in a `tokio::time::timeout(Duration::from_millis(500), ...)`. Returns `null` on timeout per the existing graceful-degradation contract. ~10 lines, no host changes. Best for v1.
  2. **Cache peer count at the host.** `host.peers()` capability returns a snapshot stored on the swarm side, refreshed periodically rather than computed per-call. Bigger change, helps any cell that calls peers().
  3. **Wait-for-bootstrap signal.** Don't register the /status route until the swarm has at least N peers OR a bootstrap timeout elapsed. Cleanest semantics but requires plumbing a readiness channel into HttpListener.
The cleanest near-term fix is (1). The latency was hidden in production until master.wetware.run actually got traffic on /status, which only started happening after the lthibault/ipns-mount-fix deploy registered the route.
**Effort:** S (option 1) → L (option 2 or 3)
**Priority:** P2 (visible, but only on first request after pod restart)
**Depends on:** none

## Revisit automated release promotion after the manual-promotion POC
**What:** Consider bot-created promotion PRs, artifact attestations, a restricted deploy identity, drift detection, and deliberate auto-merge/rollback criteria only after the manual POC has generated real operational signal.
**Why:** The POC intentionally favors a small, legible manual digest promotion and Git revert for a personal VPS. The automation would add credentials and failure modes without current product value.
**Context:** Start from `~/.gstack/projects/wetware-ww/lthibault-gitops-release-promotion-design-20260723.md`, especially "Final Scope Reset: Manual Promotion POC" and "Future Hardening." Do not describe the POC as a security boundary: ww retains its existing VPS/IPFS credential, and IPNS may lead or lag the manually deployed digest. Reassess after a successful promotion/revert plus users, collaborators, multiple services/clusters, or a real drift/security incident.
**Effort:** M
**Priority:** P3
**Depends on:** Manual promotion POC complete

## Snap v1 — JFS verify + POST + viewer-aware (separate follow-up branch)
**What:** The deferred half of the Farcaster Snap protocol. Parse `X-Snap-Payload` header, JFS-verify the Ed25519 signature over canonical JSON, extract viewer FID, render personalized response (`Hello, @{handle}`). Add POST handler for button presses (`submit` action), 5s timeout per spec. JFS verification lives at the listener level so future Glia handlers (separate effort) inherit verified-FID context for free.
**Why:** v0 POC says "Hello, @stranger" to everyone. v1 makes the snap viewer-aware, which is required for any non-trivial use case (counters, forms, anything interactive). Originally promoted to "Phase 1.5 in same PR" then demoted back to a follow-up after user re-anchored: "Overall priority is to ship a proof of concept that snaps can be hosted on ww."
**Context:** Full design preserved in `~/.gstack/projects/wetware-ww/lthibault-lthibault-farcaster-snaps-design-20260502-173810.md` under "Documented Future Scope: Phase 1.5." Cost: JFS = real Ed25519 + canonical JSON encoding work; FID → handle resolution probably needs Hub or Neynar API client. Estimate 2-3 days.
**Effort:** M (human) → S-M (CC)
**Priority:** P2
**Depends on:** Phase 1 POC ships first (lthibault/farcaster-snaps branch)

## Snap v2 — Glia handler dispatch + ww.gestalt DSL (separate follow-up branch)
**What:** Implement the planned-but-unshipped Glia handler dispatch from `lthibault-master-design-20260324-134001.md` (lines 354-366). Includes: HandlerSpec union (path/wasmBinary/wasmCid/gliaCid) in capnp schema, kernel form to register Glia handlers, HTTP listener routes `.glia`-mounted requests to the Glia evaluator with script-as-arg invocation, evaluator emits CGI on return. Also ships `std/lib/ww/gestalt.glia` — a generic Hiccup-style data-tree DSL (`ww.gestalt`) — and `examples/snap-hello-glia/main.glia` consuming it via the new dispatch. Demo artifact = side-by-side Rust vs. Glia source diff (the tweetable comparison).
**Why:** Drives planned platform work (Glia HTTP handler dispatch) by giving it a concrete first consumer. `ww.gestalt` is a generic primitive (HTML, OpenGraph, RSS, JSON-LD all consume it eventually); Hiccup precedent in Clojure validates the design. Per user (eng review): "Hiccup is hugely successful in Clojure, willing to own consequences" + "gestalt IS stdlib." Originally Phase 2 of farcaster-snaps branch, demoted to follow-up after user re-anchored on minimal POC.
**Context:** Full design + 12-row test-case table for `ww.gestalt` preserved in design doc under "Documented Future Scope: Phase 2." Snap-flattening helper stays inline in `examples/snap-hello-glia/`, NOT in std (per user: "snap is a demo, not part of stdlib"). Estimate 3-5 days, with parallelization possible (Lane A: kernel HandlerSpec; Lane B: ww.gestalt). Total work crosses capnp/, std/kernel/, src/rpc/http_listener.rs, crates/glia/, std/lib/ww/, examples/.
**Effort:** L (human) → M (CC)
**Priority:** P2
**Depends on:** Phase 1 POC ships first; ideally Snap v1 (JFS) lands first so Glia handlers inherit verified-FID context

## X-Wetware-Cell response header (Farcaster Snap provenance)
**What:** Add an `X-Wetware-Cell: bafy...` response header on every snap (and any wetware HTTP cell) response, exposing the cell's CID. Anyone curling the URL can verify "this snap was generated by cell bafy..." independently of the operator.
**Why:** Closes the lethal-trifecta JTBD loop visibly at the wire. v0 of the snap demo has operator-side provenance (the std cell is content-addressed by construction); this surfaces it client-side. Trivial cost, high signal.
**Context:** Plumbing belongs in the HTTP listener at response-construction time (`src/dispatcher/server.rs:174-178`), reading the cell CID from the executor / route registry rather than asking each cell to set it. Header name `X-Wetware-Cell` (or whatever bikeshed wins). ~10-30 min of plumbing.
**Effort:** XS-S
**Priority:** P2
**Depends on:** none (Phase 1 of farcaster-snaps branch ships first)

## IPFS primary distribution (release pipeline follow-up)
**What:** After the GitHub-based release pipeline ships, add IPFS as the primary distribution channel. Includes: `publish-ipfs` CI job (ipfs add release dir, pin on persistent node, ipns publish), `oci-export` WASM cell (`std/oci-export/`) that reads OCI layout from VFS and tars to stdout, IPNS release tracking, and IPFS-first path in `scripts/install.sh`.
**Why:** The p2p runtime should distribute itself via p2p. Eliminates GHCR as single point of failure. Content-addressed distribution. Dogfoods IPFS.
**Context:** Full design in CEO plan at `~/.gstack/projects/wetware-ww/ceo-plans/2026-04-06-release-pipeline.md`. Key decisions: IPNS points directly to latest release dir, older releases accessible by immutable CID. Need `skopeo copy --format oci` or `crane export` for OCI layout (not `docker save`, which produces legacy Docker format). Persistent IPFS node connectivity from CI is TBD (user to configure `IPFS_PIN_API_URL` secret).
**Effort:** M (human) -> S-M (CC)
**Priority:** P2
**Depends on:** Release pipeline (feat/release-pipeline), persistent IPFS node setup

## Float equality: align with Clojure semantics (0.0 == -0.0)
**What:** Normalize -0.0 to 0.0 in both `PartialEq` and `Hash` impls for `Val::Float`. Currently `0.0 != -0.0` because `PartialEq` uses `f64::to_bits()` (bitwise comparison). Clojure treats `(= 0.0 -0.0)` as true (IEEE 754 semantics).
**Why:** Deviation from Clojure semantics. A map with key `0.0` won't find entries inserted with key `-0.0`. The current behavior is consistent (`Hash` and `PartialEq` agree on `to_bits()`), so no HashMap invariant violation, but it's a footgun for users coming from Clojure.
**Context:** Fix is small: in `PartialEq` for Float, treat `a.to_bits() == b.to_bits() || (a == 0.0 && b == 0.0)`. In `Hash`, normalize `-0.0` to `0.0` before `to_bits()`. NaN handling (NaN != NaN) can stay as-is (matches both IEEE 754 and Clojure). File: `crates/glia/src/lib.rs` PartialEq and Hash impls.
**Effort:** XS
**Priority:** P3
**Depends on:** Hybrid ValMap (done)

## Shell session memory limits / TTL
**What:** Bound memory growth in long-lived shell cell sessions. Each `def` grows the Glia `Env`, each `load` caches bytes in a `thread_local! HashMap` with no eviction. A long-lived session or malicious client can grow WASM linear memory until the host OOMs.
**Why:** Each shell session is a separate WASM process, but there's no ceiling on how large that process can grow.
**Context:** Options: (a) WASM linear memory limit via wasmtime config, (b) session TTL (kill after N minutes), (c) Env size limit in glia, (d) `load` cache eviction. This TODO handles per-session growth. Design doc: `~/.gstack/projects/wetware-ww/lthibault-master-design-20260402-192805.md`.
**Effort:** S
**Priority:** P2
**Depends on:** Shell cell (ww shell)

## Glia-level finally / resource cleanup via effects
**What:** `with-resource` or `finally` pattern — cleanup handlers that run on scope exit.
**Why:** Rust Drop handles Rust-side cleanup, but Glia code can't hook into scope exit.
**Context:** Design doc notes this as follow-up. Key question: does `finally` run if effect handler resumes?
**Depends on:** #247 (needs with-handler resume infrastructure)

## `glia lint` — static analysis for effect type consistency
**What:** A lint pass that checks effect type keywords used in `perform` against those handled in `match`/`with-effect-handler`. Catches typos like `:typo` vs `:fail` at dev time.
**Why:** Runtime can't warn without noise — retry handlers that succeed on first try would warn every time. Static analysis catches the real bugs before production.
**Context:** Erlang has Dialyzer for this. Glia's dynamic typing limits what's statically checkable, but keyword constants in perform/effect clauses are low-hanging fruit. Start with: collect all `perform :X` and `(effect :X ...)` in a file/module, flag mismatches.
**Depends on:** match + pattern matching

## Guard clauses for `match`
**What:** `(pattern :when guard-expr) body` — conditional pattern matching.
**Why:** Completes the pattern matching story. Clojure's core.match and Erlang both have guards.
**Context:** Guards evaluate in the scope of the pattern's bindings. If guard is falsy, fall through to next clause. The pattern module's `match_pattern` is currently pure (no eval dependency); guards would require threading the evaluator through pattern matching. Design doc has full syntax spec in Deferred Work section.
**Depends on:** match + pattern matching

## Cache: bloom filter for mutex contention reduction
**What:** Add a lock-free bloom filter in front of `Mutex<ArcInner>` in `PinsetCache`. Definite-miss CIDs skip the mutex entirely.
**Why:** Under adversarial guest load, many concurrent `ensure()` calls for uncached CIDs contend on the mutex. Bloom absorbs misses without touching the lock.
**Context:** Size generously (100K entries at 0.001% FPR = ~244KB, ~20 hash functions, ~40ns per check). Never rebuild — stale bits just mean spurious lock acquisitions, not correctness issues. Study `quick_cache` source for concurrent bloom patterns.
**Depends on:** `crates/cache` (weighted ARC)

## Cache: metrics and observability
**What:** Hit rate, eviction count, weight utilization, inflight count. Expose via `tracing` spans or a `CacheStats` struct.
**Why:** Can't tune `budget` or `inline_threshold` without visibility into cache behavior.
**Context:** Pure additive — no runtime impact on existing code paths. Add counters to `ensure()` hot path.
**Depends on:** `crates/cache` (weighted ARC)

## Cache: mutable path caching (`/ipns/`, `/p2p/`)
**What:** Support caching mutable paths with TTL-based invalidation.
**Why:** v1 only caches content-addressed paths (`/ipfs/`). Mutable paths need TTL and re-resolution.
**Context:** IPNS records have a TTL field. `/p2p/` paths resolve via DHT with its own caching semantics. Needs design work around invalidation strategy.
**Depends on:** `crates/cache` (weighted ARC)

## Import caching (idempotent require)
**What:** Make `(import "foo")` idempotent. Second call returns cached bindings instead of re-evaluating the file. Like Clojure's `require`.
**Why:** Without caching, every `(import "utils")` re-reads `/lib/utils.glia`, re-evals it in a fresh Env, and re-binds all prefixed names. Wasteful if called from multiple modules. Also a correctness question: if `utils.glia` has side effects, re-import runs them again.
**Context:** Cache key options: module name (simple), or CID of the underlying file (content-addressed, survives layer changes). Start with module name. For .glia: cache the resulting bindings map. For .wasm: cache the capability reference (if the process is still alive). Need to decide what happens if the underlying file changes between imports (hot reload?). v1 re-evals every time.
**Effort:** S
**Priority:** P2
**Depends on:** import system (#166)

## ~~RPC handshake timeout for VatClient.dial()~~ ✅
**RESOLVED (corrected #450):** A prior attempt at this wrapped `remote_cap.when_resolved()` in a 30s `tokio::time::timeout` after `rpc_system.bootstrap()` — **that pattern was the source of #450**, not its resolution. Two compounding bugs: (1) the await came before `tokio::task::spawn_local(rpc_system)`, so the system was never polled and the wait deadlocked; (2) even with correct ordering, `when_resolved()` on a fresh `PromiseClient` does not reliably fire in capnp-rpc-rust 0.25 (`when_more_resolved` keeps appending waiters to an already-drained queue). Actual resolution: `VatClient::dial()` now uses the `crates/rpc/src/vat_dial.rs::connect` paved-path helper, which spawns the `RpcSystem` driver before returning and exposes no `when_resolved`-based handshake check. The canonical capnproto-rust pattern (hello-world client) doesn't use `when_resolved` either; a non-responsive remote surfaces on the guest's first method call through its own response timeout. See the module docs on `vat_dial` for the full rationale.

## ~~Epoch-watching in accept loops (VatListener + StreamListener)~~ ✅
**RESOLVED:** Both accept loops now use `tokio::select!` to watch the epoch guard's `watch::Receiver` for changes. When the epoch sequence advances past the issued sequence, the loop breaks with a log warning. Same pattern in both `vat_listener.rs` and `stream_listener.rs`.

## ~~Protocol namespace collision between StreamListener and VatListener~~
**RESOLVED:** Stream and vat protocols now use distinct prefixes:
`/ww/0.1.0/stream/{name}` vs `/ww/0.1.0/vat/{name}`.

## ~~Connection rate limiting for VatListener~~ ✅
**RESOLVED:** Named raw and authenticated VAT serving now share the
operator-configured `ConnectionBudget`. Authenticated streams must complete
Terminal login before `WW_TERMINAL_LOGIN_TIMEOUT_SECS` or the listener closes
the stream and releases its permit. This bounds per-connection resource use;
per-peer/per-principal quotas and Sybil-resistant fairness remain deferred.

## Authenticated VAT policy-management handle
**What:** Return or provision an operator capability that can update recipient
bindings and trigger key-scoped `RevocationGuard`s for a running authenticated
VAT service.
**Why:** `serve-vat ... :auth policy` compiles the deployer's initial policy and
enforces epoch expiry, but the public serving call does not yet expose
`KeyMethodAuthorization::revoke` or binding replacement. Operators currently
need an epoch advance to invalidate authority through this generic path.
**Context:** Keep the publication API direct; do not reintroduce a
deployer-visible `AuthenticatedVatService` wrapper merely to obtain the handle.
The management capability should be explicit, separately attenuable, and
usable by trusted FHS configuration or a future Warrant/ICME adapter.
**Effort:** M
**Priority:** P1
**Depends on:** authenticated per-stream VAT serving

## ~~Bootstrap timeout in handle_vat_connection~~ ✅
**RESOLVED:** `handle_vat_connection()` now wraps `bootstrap_request()` in a 10s `tokio::time::timeout`. Produces a clear error referencing `system::serve()`.

## ~~Dual DHT — LAN + WAN content routing~~ ✅
**RESOLVED:** `kad_lan` field added to `WetwareBehaviour` running `/ipfs/lan/kad/1.0.0` in server mode. Dual-dispatch provide/findProviders with cross-DHT PeerId dedup via `FindRequest`. Kubo peers classified by `is_lan_addr()` into WAN/LAN routing tables. 10 unit tests for extracted helpers. Design doc at `~/.gstack/projects/wetware-ww/lthibault-feat-local-routing-design-20260329-131709.md`.

## ~~Thread-per-subsystem runtime (Pingora-inspired) (#302)~~ ✅
**RESOLVED:** Service trait + Host supervisor + ExecutorPool (M:N cell scheduling). SwarmService, EpochService, WagiService, MetricsService each on dedicated OS threads. EWMA fuel scheduler for cooperative yielding. Design doc: `doc/designs/fuel-scheduling.md`.

## Metrics-over-WAGI cell
**What:** A `Cell::http("/metrics")` that exposes executor pool stats (cell counts per worker, spawn channel depth, compilation cache hit rate) as Prometheus-format metrics over the WAGI HTTP path.
**Why:** Operators need visibility into runtime health without attaching a debugger. Standard Prometheus scraping works with existing monitoring stacks.
**Context:** MetricsService already serves `/metrics` on `--metrics-addr`. This TODO is about a *WAGI cell* that serves metrics over the HTTP capability path, complementing the admin metrics endpoint. The executor pool exposes `cell_counts` and `worker_count()` already.
**Effort:** S
**Priority:** P3
**Depends on:** CompilationService (for cache hit/miss stats)

## Worker health monitoring / heartbeats
**What:** Each executor worker thread emits periodic heartbeat timestamps. A monitor checks for stale workers (no heartbeat in N seconds) and logs warnings.
**Why:** A stuck WASM cell (infinite loop that doesn't yield fuel) silently blocks its worker thread. Without heartbeats, the operator can't tell which worker is stuck or that capacity is degraded.
**Context:** Deferred from thread-per-subsystem scope (#302). Implementation: each worker updates an `AtomicU64` timestamp after each fuel yield. A lightweight monitor thread (or the Host supervisor) periodically scans timestamps. Stale = no update in 5s. Log warning with worker ID and last-known cell name.
**Effort:** S
**Priority:** P2
**Depends on:** Thread-per-subsystem runtime (done)

## ~~Nested LocalSet cleanup in spawn_rpc_inner~~ ✅
**RESOLVED:** `spawn_rpc_inner()` in `src/cell/executor.rs` and both spawn paths in `src/rpc/mod.rs` now use `tokio::task::spawn_local()` targeting the ambient worker `LocalSet` instead of creating nested `LocalSet`s. RPC systems and stderr drains run as sibling tasks on the worker, enabling proper M:N cooperative scheduling.

## ~~WAGI host-side implementation (axum + route table, Phase 2)~~ ✅
**RESOLVED:** `--http-listen` flag, WagiService on dedicated thread, axum router with route registry, CGI dispatch to ExecutorPool. Code: `src/dispatcher/server.rs`, `src/rpc/http_listener.rs`.

## HTTP-to-capnp bridge module
**What:** A capnp cell that translates HTTP requests into capability invocations. This is an application-level module, not a runtime feature. An HTTP/WAGI cell (Cell::http) that reads CGI env vars from the host, dials a capnp service via VatClient, invokes a method, and returns the result as a CGI response on stdout.
**Why:** Enables HTTP clients to interact with typed capabilities without speaking capnp-rpc. The bridge is a regular cell, not special runtime machinery.
**Context:** This is intentionally application-level. The bridge cell would be a WASM binary with `Cell::http` that uses the guest Membrane to dial capnp services. It translates REST-style routes to capability method calls. Could be generic (schema-driven routing) or hand-written per service. Uses wagi-guest crate for CGI env var reading and response formatting.
**Effort:** M
**Priority:** P3
**Depends on:** WAGI host implementation (done), VatClient guest-side

## mDNS for Kubo-less LAN peer discovery
**What:** Add `libp2p::mdns::tokio::Behaviour` to `WetwareBehaviour` to discover LAN peers without Kubo. mDNS is a **peer discovery source** that feeds the LAN DHT routing table — not a routing primitive. It does not touch Cap'n Proto or the guest API.
**Why:** The dual DHT bootstraps the LAN routing table from Kubo's swarm peers. Without Kubo (or in environments where Kubo has no private-address peers), the LAN DHT starts empty. mDNS enables zero-config LAN discovery. Note: mDNS does NOT work in cloud/container environments (no multicast). Kubo bootstrap is the fallback/primary for those environments. Dual DHT and mDNS are orthogonal — can be built and merged independently.
**Context:** mDNS adds ~25-40 lines (config, event handling, address reconciliation). CI consideration: GitHub Actions runners may not support mDNS multicast, so mDNS-dependent tests should be `#[ignore]` or gated behind an env check. All critical logic remains testable via `LocalRouting` and mock swarm channels.
**Effort:** S (CC: ~30 min)
**Priority:** P3
**Depends on:** Dual DHT (architecturally orthogonal but LAN DHT should exist first so mDNS has a routing table to feed)

## Multi-language WAGI examples (Go, Python)
**What:** WAGI cell examples in Go (via TinyGo) and Python (via componentize-py). Proves that any language compiling to wasm32-wasip2 can serve HTTP through Wetware.
**Why:** The WAGI model's main selling point is language-agnostic WAGI cells. Rust-only examples don't demonstrate this.
**Context:** TinyGo targets wasm32-wasip2 natively. componentize-py wraps CPython into a WASI component. Both toolchains are maturing but have sharp edges. Defer until toolchains stabilize and the Rust WAGI path is proven in production.
**Effort:** M
**Priority:** P3
**Depends on:** WAGI host implementation (done)

## CidTree: concurrent directory listing cache
**What:** Replace `Mutex<LruCache>` in `CidTree` with a concurrent cache (`dashmap` or `quick_cache`) to reduce contention under high concurrent cell load.
**Why:** Every path resolution for every guest call acquires the dir_cache mutex for each directory level. With many cells sharing a CidTree, this serializes all FS operations at the lock.
**Context:** CID-keyed entries are immutable, making this a read-mostly workload. `dashmap` or `quick_cache` would allow concurrent reads without lock contention. Profile first to confirm this is actually a bottleneck before migrating.
**Effort:** S
**Priority:** P3
**Depends on:** CidTree virtual filesystem (src/vfs.rs)

## CidTree: streaming reads for large files
**What:** Add a streaming read path for CidTree-backed files that pipes IPFS content directly to the WASI read buffer instead of materializing the entire file to staging first.
**Why:** Current approach fetches full file content to `staging_dir/CID` on `open_at`. For large files (ML models, datasets), this blocks the open call until the entire file is downloaded.
**Context:** Requires implementing custom `read_via_stream` in `fs_intercept.rs` instead of delegating to wasmtime-wasi's standard impl. This breaks the "delegate everything" pattern which is the current design's main simplicity win. Only worth doing when large-file workloads exist.
**Effort:** M
**Priority:** P3
**Depends on:** CidTree virtual filesystem (src/vfs.rs)

## Cap'n Proto schema-boundary refactor (stem/auth/membrane/system) (#509)
**What:** Refactor schema ownership so epoch/provenance types stay in `stem.capnp`, auth/session types move to `auth.capnp`, membrane transport types (`Membrane`, `Export`) move to `membrane.capnp`, and core host/runtime/listener contracts remain in `system.capnp`.
**Why:** `stem.capnp` currently mixes unrelated concerns and `system.capnp` imports `stem.Export` for core spawn/listener surfaces, which obscures ownership boundaries and complicates protocol evolution.
**Context:** This is a staged-compat migration, not a redesign. Keep authority semantics unchanged (`Terminal(Membrane)` and no new ambient privileges), preserve runtime behavior, and plan explicit compatibility for schema type IDs. Vat addresses are service-name locators and should not be coupled back to schema CIDs. Must audit all capnp build scripts and generated-module consumers (`crates/authority`, `std/kernel`, `std/caps`, `std/status`, examples, CLI template scaffolding) plus Synapse descriptor introspection paths.
**Effort:** L
**Priority:** P2
**Depends on:** issue #509 design approval, cross-crate capnp migration plan, compatibility decision for schema/type IDs

## MCP resources (Filesystem capability)
**What:** Expose the merged FHS filesystem as MCP resources via a `Filesystem` capability in the membrane. The MCP cell would serve `resources/list` and `resources/read` requests by delegating to the host-provided Filesystem cap.
**Why:** Claude Code needs to browse files (init.d scripts, WASM binaries, Glia modules) without writing Glia expressions. MCP resources is the standard mechanism for this.
**Context:** The MCP cell is a WASM process with zero ambient filesystem access (by design). Workaround: `eval` with `(perform fs :list ...)` and `(perform fs :read ...)`. The clean solution requires a new Filesystem capability interface in the membrane graft, specifically for MCP cells.
**Effort:** S-M
**Priority:** P2
**Depends on:** MCP dynamic tools (done)

## MCP prompts (guided workflows)
**What:** Pre-built MCP conversation starters for common workflows: `create-cell` (scaffold a new cell project), `deploy-script` (write and deploy a Glia script to ~/.ww), `connect-peer` (guide peer connection setup).
**Why:** Discoverability for AI agents. Teaches the right workflow patterns. MCP prompts are the standard mechanism for guided interactions.
**Context:** Tools + eval are sufficient for v1. Prompts become valuable once we see which workflows Claude Code actually uses most. Should be informed by real usage patterns.
**Effort:** M
**Priority:** P2
**Depends on:** MCP dynamic tools (done)

## Eval error improvements + Glia introspection
**What:** Structured error messages from `eval` (not opaque strings). New Glia forms: `(doc cap)` for capability documentation, `(schema cap)` for schema introspection, enhanced `(help)` with per-cap info.
**Why:** AI clients need actionable errors to recover from failures. Introspection makes the eval interface self-documenting, reducing the need for hardcoded MCP tool descriptions.
**Context:** Current eval returns string errors. Structured errors (with error type, capability name, action, and recovery hints) help AI clients decide whether to retry, use a different approach, or escalate. The `(doc)` and `(schema)` forms complement MCP tools by making eval itself discoverable.
**Effort:** S-M
**Priority:** P2
**Depends on:** none

## MCP server over HTTP+SSE (Mode 2)
**What:** Run the McpAdapter as an HTTP+SSE endpoint on the node, so remote MCP clients can connect without a local `ww mcp` process. Same adapter code, different transport.
**Why:** Enables web-based MCP clients and remote LLM-to-node connections without requiring the LLM to run on the same machine as the node.
**Context:** Mode 1 (`ww mcp` over stdio) is the primary interface. Mode 2 reuses the same `ProtocolAdapter` trait with an HTTP+SSE transport instead of stdio. The design doc (`~/.gstack/projects/wetware-ww/lthibault-master-design-20260326-223714.md`) already accounts for this. Not needed for the initial demo since Claude connects to a local host and dials remote nodes via capabilities.
**Effort:** M
**Priority:** P3
**Depends on:** `ww mcp` (Mode 1, stdio transport)

## `ww perform upgrade` — self-update from GitHub Releases
**What:** `ww perform upgrade` hits the GitHub Releases API, compares semver against the running binary version, downloads the latest release binary, verifies SHA256 checksum, atomically replaces the binary, clears macOS quarantine, and restarts the daemon.
**Why:** Cohort testers shouldn't have to manually re-download and replace the binary on every release. Self-update is table stakes for CLI tools.
**Context:** Deferred from the Phase 1 DX pass (PR for install/uninstall/MCP/CID). Complexity areas: SHA256 verification relies on checksums.txt in the release (no binary signing yet), atomic replacement via rename(2) is safe on Unix but needs macOS quarantine clearing, daemon restart needs to handle in-flight MCP connections gracefully. `reqwest` and `serde_json` are already in Cargo.toml. Consider adding binary signing (Apple notarization, GPG) before enabling auto-update for a broader audience.
**Effort:** M
**Priority:** P2
**Depends on:** DX pass Phase 1 (install/uninstall), GitHub Releases with consistent tag naming

## macOS binary notarization
**What:** Sign and notarize the macOS binary with an Apple Developer certificate so Gatekeeper doesn't block it on download.
**Why:** Every macOS user who downloads an unsigned binary from GitHub Releases will hit the "Apple cannot verify this app" dialog. For a small cohort, `xattr -d com.apple.quarantine` works. For broader distribution, notarization is required.
**Context:** Requires an Apple Developer account ($99/yr) and CI integration (Xcode command-line tools, `codesign`, `xcrun notarytool`). The CI workflow already builds on macos-14 (when macOS binary builds are added). Notarization adds ~2-3 minutes to the release job. Consider signing the Linux binary with GPG at the same time.
**Effort:** S-M
**Priority:** P3
**Depends on:** macOS pre-built binary in CI (Phase 2)

## Release stem (on-chain distribution anchoring)
**What:** Atomic stem type holding source + binary distribution trees, anchored on-chain via EVM. IPNS is the v1 coordination primitive; the release stem replaces it with on-chain anchoring. Publishing a release = updating the stem. Every node watching the stem sees the new release.
**Why:** Completes the dogfooding story. Distribution becomes a first-class primitive in the runtime, not external tooling. Source and binaries share a CID root, providing provenance by construction.
**Context:** The IPFS-first distribution plan (CEO plan: `2026-04-10-ipfs-distribution.md`) establishes the repo-tree-as-artifact layout and IPNS as v1. The release stem preserves the same directory layout but changes the update mechanism from IPNS to on-chain state. Connects to the stem taxonomy (atomic stems = on-chain coordination primitives).
**Effort:** L (human) → M (CC)
**Priority:** P3
**Depends on:** IPFS-first distribution (this plan), stem infrastructure

## Write doc/ARCHITECTURE.md (daemon runtime topology overview)
**What:** A 10-minute-readable overview of the daemon's runtime topology for new contributors and re-onboarding founders. Cover: (a) the Service-based pattern (`src/services.rs`) — each long-lived component on its own thread with `current_thread + LocalSet`; (b) the singleton-backing-state + per-connection-dispatcher pattern (`HostImpl`, `RuntimeImpl` are thin dispatchers; expensive state in shared `Send + Clone` references); (c) ExecutorPool — M workers, mpsc-distributed `SpawnRequest`s, shared `Arc<Engine>`; (d) fuel/epoch scheduling — cooperative yield, atomic epoch bumps, refuel via `epoch_deadline_callback`; (e) membrane graft model — `HostGraftBuilder` assembles per-graft, capnp clients are `!Send` so cap routing is single-threaded; (f) capnp surface map (`Host::network()` returns listener/dialer clients; Glia routes HTTP via `host :listen`, byte streams via `host :listen-stream`, and vat publication via `host :serve-vat`). Diagrams in ASCII per project convention.
**Why:** Three architectural mistakes in the lthibault/ww-shell-usable design session were re-derivations of things the codebase already knows but doesn't document: (1) wrongly assumed daemon main was the runtime everything lived on (true in form, but every long-lived component is on its own thread); (2) muddled the "where does cap state live" question (HostImpl per-connection vs. singleton state); (3) framed pre-warm as "spawn idle cell" rather than "compile cache at startup." All three would have been caught by a 10-minute architecture overview. Each subsequent contributor saves the re-derivation cost.
**Context:** Reference points: `src/services.rs` (Service trait, ExecutorPool, worker_loop); `crates/rpc/src/lib.rs:684-710` (build_peer_rpc, HostImpl); `src/launcher.rs:42-130` (RuntimeImpl singleton); `std/system/src/lib.rs:570-680` (cell-side serve() and poll_loop). Existing `doc/architecture.md` covers the conceptual stack (cells/membranes/ocap) — the new doc complements it with daemon runtime mechanics, doesn't duplicate.
**Effort:** M (human) → S-M (CC, with a /design-consultation pass to set scope)
**Priority:** P2 (offsets onboarding cost; each deferred day is another contributor onboarding into ambiguity)
**Depends on:** none

## Wire CompilationService through ProcBuilder for shared compile cache
**What:** `CompilationService` (`src/services.rs:540-610`) is implemented but not integrated. Today, `Component::from_binary` runs inline at `crates/cell/src/proc.rs:651` per-spawn on the executor worker thread; each cell-spawn pays compile cost (modulo the per-RuntimeImpl `executor_cache` at `src/launcher.rs:65`). Wire `CompilationService` in by adding `ProcBuilder::with_component(Component)` and routing `Runtime.load(bytes)` through the compilation channel: submit, await `Arc<Component>`, hand to ProcBuilder. Per-thread RuntimeImpl caches collapse to one shared cache.
**Why:** Singleton compile cache. Eliminates duplicate compiles when multiple Services (kernel + AdminUdsService, etc.) load the same bytecode. Surfaced during the lthibault/ww-shell-usable design — UDS admin service needs to pre-load shell.wasm at startup; without CompilationService, kernel + UDS each compile their own copy.
**Context:** TODO is in-place in `services.rs:548-553` ("ProcBuilder needs a `with_component(Component)` method to accept pre-compiled components"). Full plumbing: (a) ProcBuilder API change, (b) RuntimeImpl::load routes through CompilationService channel, (c) channel sender held in shared backing state (alongside network_state etc.). Touches `src/launcher.rs`, `crates/cell/src/proc.rs`, `src/services.rs`. Design context in `~/.gstack/projects/wetware-ww/lthibault-lthibault-ww-shell-usable-design-20260509-152936.md` under "Eng Review Findings" (E2 discussion).
**Effort:** S-M (human) → S (CC)
**Priority:** P3 (UDS shell ships without it by accepting one extra startup compile)
**Depends on:** none
