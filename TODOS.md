# TODOs

## Custom domain for the architecture map
**What:** Serve the GitHub Pages architecture map at `map.wetware.run`.
**Why:** The default project URL, `https://wetware.github.io/ww/`, is suitable for
the initial deployment. A `wetware.run` subdomain provides a first-party address
without replacing the existing VPS landing page at `wetware.run`.
**Context:** Complete only after the Pages artifact deployment works. Configure the
custom domain in GitHub Pages first, then add `map.wetware.run` as a CNAME for
`wetware.github.io`, complete GitHub domain verification, and enable HTTPS. Do not
change the apex `wetware.run` DNS record or add a VPS reverse proxy for this work.
**Effort:** XS-S
**Priority:** P3
**Depends on:** Interactive architecture-map Pages deployment

## Recursive attenuation for named vat services — SHIPPED 2026-07
**What:** Done via the single-authority capability model (eng review
2026-07-17; PRs #563–#568 plus the attenuation reification PR).
`crates/membrane` applies hook-level allowlists keyed by `(interfaceId,
ordinal)`. The policy travels with the capability across process and vat
boundaries. Nested static allowlists intersect into one membrane layer.
**Remaining:** Attenuating schema-less capabilities, such as dialed generic
capabilities, fails closed pending the deferred schema-association design (D24).
**Priority:** —

## Action-scoped AuthorityIssuer for approved agent actions
**What:** Define a typed capability issuer that accepts an authenticated principal plus an approved action or decision receipt and mints only the authority required for that action.
**Why:** Login-time `AuthPolicy` can construct an identity- or session-scoped authority environment, but it has no proposed-action context and therefore cannot express “refund order 123 and nothing else.”
**Context:** The async `AuthPolicy` session may provision an `AuthorityIssuer` capability. A policy engine such as Warrant can approve an action; the executor presents that decision to the issuer; Wetware constructs the action-scoped capability so unrelated executor authority is structurally absent. This is also a plausible paid-product seam for decision receipts, issuance, revocation, and audit. Do not design the generic action or receipt schema from the Chess fixture: first use the Cerebral discovery call to map one real Warrant-approved action, the approval output, the capabilities the executor receives today, and its residual ambient paths. ICME is analogous at the decision-proof layer; neither policy approval nor proof substitutes for Wetware's capability issuance.
**Effort:** L
**Priority:** P2
**Depends on:** asynchronous `AuthPolicy` session binding; Cerebral authority map

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
**What:** The deferred half of the Farcaster Snap protocol. Parse
`X-Snap-Payload`, JFS-verify the Ed25519 signature over canonical JSON, extract
the viewer FID, render a personalized response, and add a POST handler for
button presses. JFS verification belongs at the listener level so future
handlers receive verified viewer context.
**Why:** v0 POC says "Hello, @stranger" to everyone. v1 makes the snap viewer-aware, which is required for any non-trivial use case (counters, forms, anything interactive). Originally promoted to "Phase 1.5 in same PR" then demoted back to a follow-up after user re-anchored: "Overall priority is to ship a proof of concept that snaps can be hosted on ww."
**Context:** Full design preserved in `~/.gstack/projects/wetware-ww/lthibault-lthibault-farcaster-snaps-design-20260502-173810.md` under "Documented Future Scope: Phase 1.5." Cost: JFS = real Ed25519 + canonical JSON encoding work; FID → handle resolution probably needs Hub or Neynar API client. Estimate 2-3 days.
**Effort:** M (human) → S-M (CC)
**Priority:** P2
**Depends on:** Phase 1 POC ships first (lthibault/farcaster-snaps branch)

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
**Why:** `VatListener.serveAuthenticated` compiles the deployer's initial policy and
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
**RESOLVED:** `kad_lan` field added to `host::Behaviour` running `/ipfs/lan/kad/1.0.0` in server mode. Dual-dispatch provide/findProviders with cross-DHT PeerId dedup via `FindRequest`. Kubo peers classified by `is_lan_addr()` into WAN/LAN routing tables. 10 unit tests for extracted helpers. Design doc at `~/.gstack/projects/wetware-ww/lthibault-feat-local-routing-design-20260329-131709.md`.

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
**What:** Add `libp2p::mdns::tokio::Behaviour` to `host::Behaviour` to discover LAN peers without Kubo. mDNS is a **peer discovery source** that feeds the LAN DHT routing table — not a routing primitive. It does not touch Cap'n Proto or the guest API.
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
**Context:** This is a staged-compat migration, not a redesign. Keep authority semantics unchanged (`Terminal(Membrane)` and no new ambient privileges), preserve runtime behavior, and plan explicit compatibility for schema type IDs. Vat addresses are service-name locators and should not be coupled back to schema CIDs. Must audit all capnp build scripts and generated-module consumers (`crates/authority`, `std/kernel`, `std/status`, examples, CLI template scaffolding) plus Synapse descriptor introspection paths.
**Effort:** L
**Priority:** P2
**Depends on:** issue #509 design approval, cross-crate capnp migration plan, compatibility decision for schema/type IDs

## `ww perform upgrade` — self-update from GitHub Releases
**What:** `ww perform upgrade` hits the GitHub Releases API, compares semver against the running binary version, downloads the latest release binary, verifies SHA256 checksum, atomically replaces the binary, clears macOS quarantine, and restarts the daemon.
**Why:** Cohort testers shouldn't have to manually re-download and replace the binary on every release. Self-update is table stakes for CLI tools.
**Context:** Deferred from the Phase 1 DX pass. Complexity areas: SHA256 verification relies on checksums.txt in the release (no binary signing yet), atomic replacement via rename(2) is safe on Unix but needs macOS quarantine clearing, and daemon restart must handle in-flight connections. `reqwest` and `serde_json` are already in Cargo.toml. Consider adding binary signing (Apple notarization, GPG) before enabling auto-update for a broader audience.
**Effort:** M
**Priority:** P2
**Depends on:** DX pass Phase 1, GitHub Releases with consistent tag naming

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
**What:** A 10-minute-readable overview of the daemon's runtime topology for new contributors and re-onboarding founders. Cover: (a) the Service-based pattern (`src/services.rs`) — each long-lived component on its own thread with `current_thread + LocalSet`; (b) the singleton-backing-state + per-connection-dispatcher pattern (`HostImpl`, `RuntimeImpl` are thin dispatchers; expensive state in shared `Send + Clone` references); (c) ExecutorPool — M workers, mpsc-distributed `SpawnRequest`s, shared `Arc<Engine>`; (d) fuel/epoch scheduling — cooperative yield, atomic epoch bumps, refuel via `epoch_deadline_callback`; (e) membrane graft model — `HostGraftBuilder` assembles each graft, and Cap'n Proto clients are `!Send`, so capability routing is single-threaded; (f) the Cap'n Proto surface exposed by `Host::network()`, including HTTP, byte-stream, and authenticated vat transport. Diagrams in ASCII per project convention.
**Why:** Three architectural mistakes in the lthibault/ww-shell-usable design session were re-derivations of things the codebase already knows but doesn't document: (1) wrongly assumed daemon main was the runtime everything lived on (true in form, but every long-lived component is on its own thread); (2) muddled the "where does cap state live" question (HostImpl per-connection vs. singleton state); (3) framed pre-warm as "spawn idle cell" rather than "compile cache at startup." All three would have been caught by a 10-minute architecture overview. Each subsequent contributor saves the re-derivation cost.
**Context:** Reference points: `src/services.rs` (Service trait, ExecutorPool, worker_loop); `crates/rpc/src/lib.rs` (`HostImpl` and the test-only `build_test_peer_rpc` fixture); `src/launcher.rs:42-130` (RuntimeImpl singleton); `std/system/src/lib.rs:570-680` (cell-side serve() and poll_loop). Existing `doc/architecture.md` covers the conceptual stack (cells/membranes/ocap) — the new doc complements it with daemon runtime mechanics, doesn't duplicate.
**Effort:** M (human) → S-M (CC, with a /design-consultation pass to set scope)
**Priority:** P2 (offsets onboarding cost; each deferred day is another contributor onboarding into ambiguity)
**Depends on:** none
