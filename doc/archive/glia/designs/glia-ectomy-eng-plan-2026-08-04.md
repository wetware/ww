# Glia-ectomy + WASM pid0 Migration — Engineering Plan

**Date:** 2026-08-04
**Repo:** wetware/ww @ `f1365b6` (branch `glia-control-extraction`; working tree dirty +4,663/−1,229 — archived by PR-0, untouched by all later PRs)
**Method:** `/plan-eng-review`, constrained session. Inputs: CEO strategic review (`.context/glia-archival-ceo-review-2026-08-04.md`), repo coupling map, fresh pid0 responsibility inventory (file:line-verified), prior learnings.
**Audience:** implementation model (mechanical PRs), Sol (adversarial checkpoints), Louis (decisions), Fable (high-risk checkpoints only).

> **v4 NOTICE — read §23, §24, AND §25 before implementing.** Two adversarial outside-voice passes (both repo-verified) produced 11 + 9 findings; §23 and §24 record them and their amendments **override** the base text wherever they conflict (later sections override earlier). Verdict: **PROCEED WITH REQUIRED CHANGES**. **D9 is RESOLVED (2026-08-04, Louis): KEEP PID0 AS A WASM COMPONENT** — rationale and guardrails in §25; PR-2+ is unblocked. All §24 amendments remain binding in full.

---

## 1. Executive recommendation

**PROCEED WITH WASM PID0 MIGRATION** — with one reframing that shrinks the work:

**pid0 is already a WASM component.** `std/kernel` builds as a wasip2 component (`Makefile:50-53`, plain `cargo build --target wasm32-wasip2`; no cargo-component), the host instantiates it via `WasiCliCommandPre` (`crates/cell/src/proc.rs:723-741`), and the entire host↔pid0 contract is four points:

1. export `wasi:cli/run`;
2. receive the `Membrane` bootstrap over the `wetware:streams/streams@0.1.0` WIT resource + capnp-rpc (`std/system`, Glia-free, reusable unchanged);
3. reverse-graft a `Membrane` back within `WW_EXPORT_POLICY_READY_TIMEOUT_SECS` (default 120 s, `src/executor.rs:682-749`);
4. register ≥1 HTTP route so `/readyz` opens (`src/cli/main.rs:1981-1997`, `src/metrics.rs:423-459`).

**D9 resolved — pid0 stays a WASM component (§25).** Not as a permanent minimalist philosophy, but because **deployment composition is expected to vary and belongs outside the native trust root**: pid0 is where a deployment says which cells start, what grants they receive, what is exported and over what transport, which routes exist, and how epoch-driven reconfiguration behaves. For now, *editing and recompiling the small reference Rust kernel* is the accepted temporary authoring workflow — path-or-CID execution makes that loop practical — and we deliberately do not invent a JSON/TOML/DSL configuration layer before real usage reveals what is actually stable and declarative (progression ladder + hard stop conditions in §25).

Everything else is kernel-internal. Glia's load-bearing surface inside pid0 is **only boot step 7** — `run_initd` + the `host`/`runtime`/`routing`/`import` effect handlers (`std/kernel/src/lib.rs:990-1699`, `1792-1900`); steps 1–6 and 8–9 (graft extraction, reverse graft, epoch watch, generation loop) are capnp/lifecycle Rust that ports nearly verbatim. So this migration is a **kernel swap behind an existing, stable boundary**, not a new architecture.

Two defects discovered during grounding are pulled into scope because the migration is unsafe without them:

- **No pid0 end-to-end test exists.** `grep 'main.wasm' tests/*.rs` → 0 hits. The entire boot contract lives in 10 stub-based unit tests inside `std/kernel/src/lib.rs:3084-3364`. Parity cannot be "proven" without an e2e harness; PR-3 builds it against the *current* kernel first (baseline before change).
- **`/version` kernel identity is wrong today.** It reports blake3 of the *embedded* kernel blob (`src/cli/main.rs:77-79`, `src/metrics.rs:469`) even when the loader chain (HostPath > Embedded > IPFS) resolved different bytes. The brief's invariant ("CID is always the runtime identity of the loaded kernel") requires fixing this in PR-2.

Deletion gates: parity matrix green on both kernels → default flip → **production epoch-restart canary** → consumers deleted → `crates/glia` last. End state: handoff to business-focused work.

---

## 2. Current pid0 responsibility inventory

Owners: **NH** = native host, **K** = kernel WASM (Glia today), **G** = Glia script/eval, **SUB** = substrate crate, **CFG** = configuration, **RM** = remove.

| # | Responsibility | Current location (file:symbol) | Owner now | Target | Capability required | Current tests | Missing tests |
|---|---|---|---|---|---|---|---|
| 1 | Kernel image resolution + load | `src/cli/main.rs:1529-1540` ChainLoader (HostPath>Embedded>IPFS); `crates/cell/src/loaders.rs:18,41,64,108`; hardcoded `bin/main.wasm` at `src/executor.rs:428` | NH | NH (extend: `KernelSource`, §4) | Kubo API (IPFS source only) | loaders unit tests | `--kernel` path/CID/precedence/mismatch tests (PR-2) |
| 2 | Kernel CID identity | blake3→CIDv1(raw) at `src/executor.rs:433-441`; `WW_CELL_CID` env `:510-512`; `/version` hash of **embedded** blob `src/cli/main.rs:1565`, `src/metrics.rs:469` | NH | NH (fix `/version` to report **loaded** bytes) | — | `src/metrics.rs:800` | identity-matches-loaded-bytes test (PR-2) |
| 3 | Component validation + instantiation | `crates/cell/src/proc.rs:697-741` (compile, `WasiCliCommandPre`, 5 s instantiation timeout) | NH | NH (unchanged) | — | engine tests `crates/cell/src/engine.rs:239-304` | incompatible-component error-path e2e (PR-3) |
| 4 | pid0 privilege construction | `build_pid0_membrane_rpc` call `src/executor.rs:603`, def `crates/rpc/src/graft.rs:498`; 7-name graft `graft.rs:307-395`; pid0-vs-child split = constructor choice (`spawn_serving_with_ready` vs `ExecutorImpl::spawn` `src/launcher.rs:422`) | NH | NH (unchanged — **no new caps for new kernel**) | — | `graft.rs:601,793,876` | none |
| 5 | Graft extraction into session | `run_impl` steps 1–3, `std/kernel/src/lib.rs:2301-2329`; `get_graft_cap` `:1993` | K | K (port; `get_graft_cap` → std/system, §7) | membrane.graft | `:3084-3134` | ported to new kernel (PR-4) |
| 6 | Env binding + prelude + Glia eval | `:2341-2416` | K+G | **RM** (typed Rust replaces Env) | — | many (Glia) | n/a |
| 7 | Reverse graft (bootstrap publication) | `KernelBootstrap` `:79-118`; `publish_bootstrap_membrane` `:2189-2194`, called `:2429` **before** init.d; host poll `wait_for_export_policy_ready` `src/executor.rs:682-749` | K | K (port ~60 lines verbatim) | membrane | `:3084,:3115,:3134`; host `src/executor.rs:974-1008` | ported + e2e (PR-3/4) |
| 8 | Boot policy (init.d) | `run_initd` `:1792-1900`; SysV best-effort; `wrap_with_handlers` `:1766` | K+G | **K hardcoded** (`/status` registration in Rust; no manifest — CEO decision D4) | runtime.load, host.network | `:2486-2530,:2517,:4859,:4874` | Rust-kernel boot-policy unit tests (PR-4) |
| 9 | `/status` registration | `std/status/etc/init.d/05-status.glia:11-14` → `host :listen` handler `:1134-1191` → `HttpListener.listen` | G | K (typed capnp calls: `runtime.load(status.wasm)` → executor → `listener.listen(executor, "/status", [host])`) | runtime, host.network | `:4930,:4952`; `tests/status_cell_http_listener_e2e.rs:41` | same flow from new kernel (PR-4/parity) |
| 10 | Listener/route install + replacement | NH: `crates/rpc/src/http_listener.rs:81-176`, registry `dispatch.rs:40-108`; expiry via epoch or `Pid0RegistrationScope` (`graft.rs:485-494`, dropped `src/executor.rs:671`) | NH | NH (unchanged) | — | `http_listener.rs:873-1189` (9 tests) | none |
| 11 | Route/daemon readiness | NH: `RuntimeStatus` `src/metrics.rs:31-101`; phases `src/cli/main.rs:1604-1999`; `/readyz` re-checks `live_route_count` per request | NH | NH (unchanged) | — | `src/metrics.rs:714-800` (`:735` is load-bearing) | e2e readiness via real kernel (PR-3) |
| 12 | Epoch tracking + stale handling | K: `wait_for_stale_epoch` `:2264-2283` (5 s probe on `host.id()`, only `StaleEpoch` code restarts); NH: `crates/authority/src/epoch.rs:24-53`, stem `src/services.rs:578-611`, `crates/stem/` | K+NH | K (port) + NH (unchanged) | host (probe) | `:3153,:3197,:3316`; `crates/rpc/src/graft.rs:667`; `crates/rpc/src/lib.rs:1087` | epoch restart **e2e** (PR-3; today unit-only) |
| 13 | Re-graft / generation loop | `:2441-2472`: drop env/ctx/dispatch → generation++ → re-graft; replacement-init failure ⇒ `EPOCH_RESTART_INIT_FAILED` ⇒ non-zero exit (`:2196-2233`) | K | K (port policy exactly: gen-0 tolerant, replacement strict) | — | `:3220,:3238,:3286,:3316` | ported (PR-4); note `:3286` is Glia-shaped (NativeFn drop) → re-express as handler-teardown-order in Rust terms |
| 14 | Child supervision + failure propagation | NH: `OwnedChildLifecycle` `src/launcher.rs:285-384` (exit 0/1/137), `SpawnHandoffGuard` `:390-418`; pid0 exit → `src/cli/main.rs:2021-2054` | NH | NH (unchanged) | — | `src/launcher.rs:607`; `tests/runtime_spike_test.rs` | none |
| 15 | Shutdown | K: TTY stdin EOF (`run_shell` `:1922-1926`); daemon loop is infinite. NH: **no signal handler anywhere**; teardown `src/cli/main.rs:2041-2054`; stdin bridge `src/executor.rs:462-490` | K+NH | K (stdin-EOF exit, no REPL — D1 §22) + NH unchanged; SIGTERM = tightening backlog | — | `tests/stdin_shutdown_integration.rs:36` (echo cell only) | pid0 stdin-close e2e (PR-3); SIGTERM parked |
| 16 | Init failure / retries | K: per-script continue, `failures` counted, **no retries**; policy split gen-0 vs replacement `:2206-2224` | K | K (port policy; scripts→steps) | — | `:2517,:3220,:3238` | ported (PR-4) |
| 17 | Logging/observability | K: `StderrLogger` `:120-148`, **Warn default**; NH: child stderr→tracing `src/launcher.rs:515-523`; metrics/admin `src/metrics.rs:406-610` | K+NH | K (port Warn default) + NH unchanged; add `kernel_source`/`kernel_cid` to `/version` (PR-2) | — | `src/metrics.rs:800,910` | /version new-fields test (PR-2) |
| 18 | Content/CID handling (mounts, VFS) | NH: `crates/cell/src/image.rs:314,537,571,630`; `CidTree` VFS + `fs_intercept`; `WW_ROOT=/ipfs/<cid>` `src/executor.rs:507-509` | NH | NH (unchanged) | Kubo | image/vfs tests | none |
| 19 | Shell/MCP | Decoupled since #506: `ww shell` evaluates in CLI (`src/cli/shell.rs:519-666`), zero kernel dependence. **But** `WW_TTY=1` (`src/executor.rs:441,501-503`) still drops pid0 into in-kernel Glia REPL (`run_shell` `:1911-1986`, reached `:2444-2453`) | K+CLI | **RM** (REPL dies with Glia; TTY behavior → D1 §22). CLI shell/MCP retirement per CEO plan | — | `:3262` | TTY-mode behavior test for new kernel (PR-4) |
| 20 | attenuation schema/method resolution | `std/kernel/src/attenuate.rs` (~310 lines; kebab→camel vs compiled schema → `membrane::Allowlist`) | K | **SUB** → `crates/membrane` (§7; membrane already builds for wasm32 — kernel depends on it today) | — | attenuate tests in kernel | migrated with code (PR-1) |
| 21 | Test-only behavior | native `wait_monotonic` sleep `:2259-2261`; stub harness `TestMembrane :2914`, `EpochMembrane :2954`, `ScriptedProbeHost :2984`, `TestRuntime :2692`, `TestHost :2637`; kernel tests are a standalone workspace, CI at `rust.yml:264` | K | K (port stub harness — it is the highest-value asset for PR-4) | — | 10 lifecycle tests `:3084-3364` | port all; re-express `:3286` |

**Also inventoried, vestigial (→ §14 tightening):** `DEFAULT_KERNEL_CID` (`src/default_kernel.rs:29`, no non-doc consumer); dead `std/kernel/wit/kernel.wit`; `doc/architecture.md:153` documents `boot/main.wasm` while runtime loads `bin/main.wasm` (dev shim copies boot→bin, `src/cli/main.rs:1502-1516`); stale `Containerfile.deploy:8` "AdminUdsService" comment; stale `Makefile:181` "no test suite yet".

---

## 3. Native bootstrap boundary

**Principle: the native host owns mechanism that must exist before pid0 exists; nothing else.** Almost all of it already exists.

| Concern | Spec | Status |
|---|---|---|
| Source parsing | `KernelSource::{Path, Cid}` (§5). `--kernel <arg>` / `WW_KERNEL=<arg>`. Disambiguation: parse as CID first (multibase/multicodec validity); else treat as path; `file:`/`cid:` prefixes accepted as explicit overrides but not required | NEW (PR-2) |
| Precedence | CLI `--kernel` > `WW_KERNEL` env > embedded default (`EMBEDDED_KERNEL`, `src/cli/main.rs:29-31`). Container/installer keep working with zero flags | NEW (PR-2) |
| Local file load | direct read; reject dirs/missing with named error | NEW (thin; PR-2) |
| CID fetch | reuse `IpfsLoader` (Kubo `/api/v0/cat`, `crates/cell/src/loaders.rs:18`); gated behind existing `waiting-for-kubo` phase (`src/cli/main.rs:1604`) — **no new fetch stack** | EXISTS |
| CID cache | Kubo blockstore is the cache; wasmtime persistent compile cache (`crates/cell/src/engine.rs`) covers compilation. No new cache layer | EXISTS |
| CID computation (local) | blake3 → CIDv1(raw 0x55, mh 0x1e), the existing convention (`src/executor.rs:433-440`) | EXISTS |
| CID verification (fetched) | IPFS-CID sources: content addressing is verified by Kubo on cat; host additionally recomputes and logs the runtime blake3 CID. Raw-CID (blake3) sources: recompute and **hard-fail on mismatch** | NEW (PR-2) |
| Component/world validation | existing: `compile_component` + `WasiCliCommandPre::new` fails on missing `wasi:cli/run`; 5 s instantiation timeout (`proc.rs:729-741`). Sufficient — a wrong-world artifact fails closed with a named error | EXISTS |
| Kernel ABI/version validation | host sets `WW_KERNEL_ABI=1` env; kernel checks and exits non-zero with a clear stderr line on mismatch. No WIT version negotiation — the ABI is the 4-point contract and it is frozen by this plan | NEW (2 lines each side; PR-2/PR-4) |
| Startup inputs | env only, existing: `WW_ROOT`, `WW_TTY`, `PATH`, `WW_CELL_CID` (`src/executor.rs:501-512`) + `WW_KERNEL_ABI`. **No config language** | EXISTS |
| Bootstrap capability construction | `HostGraftBuilder` 7-name graft (`crates/rpc/src/graft.rs:307-395`) — **frozen; the new kernel gets exactly the same graft, nothing more** | EXISTS |
| pid0 identity | structural: the one cell spawned via `Cell::spawn_serving_with_ready` (`src/executor.rs:374`, called once `src/cli/main.rs:2003`); everything else goes through `Executor.spawn` → `InitialGrants` | EXISTS |
| Supervision / crash-restart | pid0 exit → host exit with same code (`src/cli/main.rs:2021-2054`); restart policy belongs to the *outer* supervisor (systemd/k8s), not the host. Unchanged | EXISTS |
| Startup timeout | 5 s instantiation + 120 s export-policy readiness (`WW_EXPORT_POLICY_READY_TIMEOUT_SECS`) | EXISTS |
| Shutdown | stdin bridge + process teardown (`src/cli/main.rs:2041-2054`); no signal handler (parked, §14) | EXISTS |
| Error reporting | named errors per failure (source parse, load, CID mismatch, component invalid, readiness timeout); all logged with the resolved source | PR-2 |
| Resolved-CID logging | log line at kernel spawn: `kernel_source=<path|cid> kernel_cid=<blake3 cidv1> embedded=<bool>`; `/version` gains `kernel_source`, `kernel_cid` (of **loaded** bytes — fixes the stale-identity defect) | PR-2 |

**Irreducible native trust root after migration:** the loader chain + CID computation/verification; the wasmtime engine + component validation; `HostGraftBuilder` (the 7 capabilities and their epoch guards); `Pid0RegistrationScope`; the epoch service (`--stem` pipeline); `OwnedChildLifecycle` + exit propagation; the admin plane (`/healthz /readyz /version /metrics`); the libp2p host + Terminal boundary. Everything above that line — boot order, what runs, what routes exist, restart-on-epoch policy — lives in the kernel component.

---

## 4. Kernel component contract

**Do not invent a new WIT world.** The contract is the existing one, now written down and version-stamped:

| Element | Spec | Concrete current code |
|---|---|---|
| World/interface | wasip2 component exporting `wasi:cli/run`; imports: `wasi:cli/{stdin,stdout,stderr}`, `wasi:io/{streams,poll,error}`, `wasi:clocks/monotonic-clock`, `wasi:filesystem` (VFS-intercepted), `wetware:streams/streams@0.1.0` | `crates/cell/wit/streams.wit:4-20`; `crates/cell/src/proc.rs:554-595,757-765`; `std/system/src/lib.rs:63-73` |
| Entrypoint | `wasi:cli/run#run`; return `Ok` ⇒ exit 0, `Err` ⇒ non-zero | `proc.rs:757-765`; `std/kernel/src/lib.rs:2477 export!` |
| Startup inputs | env: `WW_ROOT` (`/ipfs/<cid>`), `WW_TTY`, `WW_CELL_CID`, `WW_KERNEL_ABI=1`; args unused | `src/executor.rs:501-512` |
| Authority delivery | capnp-rpc over `wetware:streams` duplex; host serves `GuestMembrane`; kernel: `system::serve(bootstrap.client, ...)` → `membrane.graft_request()` → named caps `identity? host runtime routing authority ipfs http-client?` | `crates/rpc/src/graft.rs:588-594,307-395`; `std/kernel/src/lib.rs:2296-2318` |
| Reverse graft | kernel exports its own `Membrane` proxy as its RPC bootstrap; must answer `graft()` (or return `INIT_MEMBRANE_NOT_READY` until ready) within the 120 s host poll | `KernelBootstrap` `:79-118`; `src/executor.rs:682-749` |
| Readiness | reverse graft answering + ≥1 live HTTP route ⇒ host `/readyz` 200 | `src/cli/main.rs:1981-1999`; `src/metrics.rs:423-459` |
| Error model | WASI exit codes: 0 clean; non-zero failure; replacement-generation init failure ⇒ error exit (never a silent retry loop). stderr is the diagnostic channel (host bridges to tracing) | `:2196-2233`; `src/launcher.rs:515-523` |
| Shutdown protocol | daemon: run until host process ends; TTY/stdin-EOF: exit 0 (D1 §22 sets exact new-kernel TTY behavior) | `:1922-1926`; `src/executor.rs:462-490` |
| Restart/recovery | epoch: probe `host.id()` every 5 s; on `StaleEpoch` **only**, drop all session state, re-graft, re-run boot policy; failure of replacement init ⇒ exit non-zero. Host-side restart = outer supervisor | `:2264-2283,:2441-2472` |
| Version negotiation | `WW_KERNEL_ABI` check, exit-with-message on mismatch. Contract changes bump the integer; dual-support windows are explicit | NEW |
| Identity reporting | host-side only (resolved CID in logs + `/version`); kernel does not self-report | PR-2 |

**Boundary among layers (normative):** WASI/component imports = execution substrate only (clocks, streams, fs-intercept). `wetware:streams` = transport for exactly one duplex connection. **All authority is Cap'n Proto capabilities over that connection** — WIT carries no authority, ever (this preserves the CEO-review rule: authority travels as capnp references, never as interface surface). Native bootstrap APIs = env vars + exit codes only.

TinyGo note (no design, per scope controls): the contract above is language-neutral — any toolchain that emits a wasip2 component with `wasi:cli/run` and speaks capnp over `wetware:streams` qualifies. Nothing in this plan narrows that.

---

## 5. Kernel path-or-CID design

```rust
enum KernelSource { Path(PathBuf), Cid(Cid), Embedded(&'static str) }  // --kernel / WW_KERNEL;
                                                        // "embedded:<name>" selects a compiled-in artifact
                                                        // (the dual-path selector §8/§10 depends on)
struct ResolvedKernel { bytes: Vec<u8>, cid: Cid,      // cid = blake3 CIDv1(raw) of bytes (runtime identity)
                        source: KernelSourceRecord,     // Path|Cid|Embedded + original string
                        metadata: KernelMeta }          // size, source_cid (if CID-sourced), load duration
```

**Load-bearing fact (outside-voice finding 1, repo-verified):** `EmbeddedLoader` matches by path *suffix* (`crates/cell/src/loaders.rs:94-98`) and the kernel is registered under `bin/main.wasm` (`src/cli/main.rs:56`), while pid0 is always requested as `/ipfs/<merged-root>/bin/main.wasm`. So in release builds **the embedded kernel already silently shadows any image-delivered kernel** — the deploy container's copied `wetware/kernel/bin/main.wasm` is dead weight today. Consequences: the parity baseline is the *embedded* kernel path; PR-5 must keep the embed set and deploy-context copies in lockstep (or delete the dead copies); `KernelSource::Embedded` is the real production selector and must be first-class, not an afterthought.

- **Syntax:** bare string. Valid CID (multibase parse) ⇒ `Cid`; else ⇒ `Path`. Explicit `file:`/`cid:` prefixes accepted for the pathological case (a local file literally named like a CID); not required, documented.
- **Ambiguity:** CID-parse wins; a file whose name parses as a CID needs `file:`. Error messages name both interpretations.
- **Precedence:** `--kernel` > `WW_KERNEL` > embedded default. Absent both, behavior is byte-identical to today (Embedded under `bin/main.wasm` suffix; mounts unchanged).
- **Cache:** none new. CID fetch via existing `IpfsLoader`/Kubo (blockstore = cache); compile cache = existing wasmtime persistent cache with its degraded-state reporting.
- **Offline:** Path/Embedded work with no network. `Cid` source with Kubo unreachable ⇒ named startup error after the existing `waiting-for-kubo` gate — **no fallback to embedded** (silent substitution of a different kernel than requested is worse than failing; operator asked for specific bytes).
- **Pre-pid0 remote fetch:** allowed only through the Kubo gate that already precedes kernel start; circular-fetch impossibility: the fetch uses the host's Kubo client, never a cell.
- **Fallback:** none (see offline). Embedded is a *default*, not a fallback for explicit sources.
- **File-change semantics:** bytes read once at startup; identity = that read. No hot reload; a changed file needs restart (matches today).
- **Upgrade/rollback:** ship new kernel → `--kernel`/`WW_KERNEL` selects it → default flip changes only which artifact is embedded/default. Rollback = point the flag at the previous artifact (dual-path window keeps both shippable, §8).
- **Identity in logs/status:** every boot logs `kernel_source`, `kernel_cid`, `embedded`; `/version` reports `kernel_cid` (loaded bytes — bug fix), `kernel_source`, plus existing fields.

> **Normative invariant:** Source selects where bytes come from; **the blake3 CIDv1 of the exact loaded bytes is always the runtime identity** — reported even when the source was an IPFS unixfs CID (both are logged; they differ by construction).

**Why path-or-CID is load-bearing (D9 rationale, §25):** `--kernel ./kernel.wasm` / `--kernel bafy...` is the deployment-authoring loop itself — edit the reference Rust kernel, compile to wasip2, run by local path during development, pin by CID for deployment. The selector is not an ops convenience bolted onto the migration; it is the mechanism that lets deployment composition live outside the native trust root without inventing a configuration language.

---

## 6. Rust WASM kernel plan

- **Crate:** `std/kernel-next` (standalone `[workspace]`, mirroring `std/kernel/Cargo.toml:6`); renamed to `std/kernel` in PR-11. Same layout: `cdylib`, `wasip2::cli::command::export!`, build.rs doing capnp codegen only (copy `std/kernel/build.rs:16-52` pattern incl. schema-CID constants). **Must carry the `[patch.crates-io]` capnp-rpc pin** (`github.com/wetware/capnproto-rust` branch `ww/import-fix-0.25-consume`, per `std/kernel/Cargo.toml:29-36`; enforced by `tests/child_authority_confinement.rs:37`).
- **Target/toolchain:** `wasm32-wasip2`, pinned in `rust-toolchain.toml` already. Makefile gains `kernel-next` target cloning `Makefile:50-53`.
- **WIT:** none generated, none new. `std/system`'s existing `wit_bindgen::generate!` against `crates/cell/wit` is the only binding (delete dead `std/kernel/wit/kernel.wit` in §14).
- **Reuse (unchanged):** `std/system` (serve/poll-loop/RpcSession — Glia-free, verified); `crates/membrane` (+ the schema-resolution code arriving in PR-1); generated capnp modules; `crates/rpc` named-cap encoding.
- **Extract-and-port (source: `std/kernel/src/lib.rs`):** `KernelBootstrap` (`:79-118`, ~60 lines, verbatim); `get_graft_cap` (`:1993` → std/system, PR-1); graft/session assembly (`:2301-2329`, minus Glia env); epoch/generation state machine (`:2196-2283`, `:2441-2472`); logging init (`:120-148`, Warn default).
- **Lifecycle state machine (kernel main):**

```
run() ─► graft_request ─► extract caps ─► publish reverse graft
   ─► boot_policy()  ── register /status (runtime.load(status.wasm) ─► executor
   │                     ─► host.network().http_listener.listen(executor,"/status",[host]))
   ─► generation_action:
        WW_TTY?  ─► idle-wait (no REPL; D1)          daemon ─► watch_epoch
                                                        │ StaleEpoch only
        exit 0 ◄─ stdin EOF (TTY)                       ▼
                                            drop session state ─► gen+1 ─► re-graft
                                            replacement boot failure ─► exit(!=0)
```

- **`/status`:** hardcoded typed-capnp reimplementation of the 14-line Glia flow (inventory row 9). Bytes come from `$WW_ROOT/bin/status.wasm` via the same VFS read used by `std/caps::read_default_path` logic (reimplement the ~20-line path resolution; do not depend on `std/caps`).
- **Classification of every current pid0 behavior:** HARDCODE NOW: /status registration, boot order, epoch policy, logging default — hardcoding *in the kernel* is the step-1 authoring surface per the §25 ladder, not a shortcut. PARAMETERIZE (env, existing only): `WW_ROOT`, `WW_TTY`, `WW_KERNEL_ABI`. SMALL STRUCTURED CONFIG: none during the Glia-ectomy (ladder step 2 requires *stable scalar parameters* to have emerged; CEO D4 binding). DEFER: SIGTERM handling, multi-route boot policy, TinyGo. REMOVE: `run_initd`/`.glia` eval, in-kernel REPL (`run_shell`), Glia env/prelude/effect handlers, `import` machinery, `schema/doc/help` builtins.
- **Tests:** port the stub harness (`TestMembrane`/`EpochMembrane`/`ScriptedProbeHost`/`TestRuntime`/`TestHost`, `std/kernel/src/lib.rs:2637-2984`) into `kernel-next` and re-run all 10 lifecycle tests (`:3084-3364`) against the new kernel; `:3286` re-expressed as "old generation's resources drop before replacement activation" without `glia::Val`. CI: add the standalone-workspace test step mirroring `rust.yml:264`.
- **Packaging/content addressing/release:** `make std` builds both kernels during dual-path; embedded registration for `kernel-next` under its own suffix; CI deploy context (`rust.yml:444-451`) ships both during the window; release artifacts unchanged otherwise. Runtime identity via §5.
- **Estimated size:** ~700–1,000 LOC production + ported tests (the Glia kernel's non-test surface is ~2,479 LOC, of which ~1,700 is Glia env/handlers/REPL being removed).

---

## 7. Shared substrate extraction ledger

Deliberately small — only two items clear the "clearly belongs outside Glia" bar:

| Item | Current location | Target | API shape | Glia dep removed | Tests | Critical path? |
|---|---|---|---|---|---|---|
| Attenuation schema/method resolution (kebab→camel over compiled `schema.Node` → `Allowlist`) | `std/kernel/src/attenuate.rs` (~310 lines) | `crates/membrane::schema` (membrane already compiles for wasm32 — std/kernel depends on it today) | `fn resolve_allowlist(schema: &CompiledSchema, methods: &[&str]) -> Result<Allowlist, ResolveError>` + typed errors | yes — resolution logic keeps zero `glia::` imports; the thin `Dispatch::reify_attenuation` shim stays behind in std/kernel until PR-10 deletes it | move existing attenuate tests; add wasm32 build check | **Yes** — PR-1, prerequisite for kernel-next and for any future typed attenuation surface |
| `get_graft_cap` (named-cap extraction, generic over `FromClientHook`) | `std/kernel/src/lib.rs:1993` | `std/system` | `fn get_graft_cap<C: FromClientHook>(caps: &Caps, name: &str) -> Result<C, GraftError>` | yes | new unit tests in std/system | Yes — PR-1 (tiny) |

**Inspected and rejected for extraction:** grant-map construction (Glia-side parsing dies; wire encoding already in `crates/rpc/named_capability.rs`); route setup/bootstrap publication/epoch/lifecycle (already native-host substrate, rows 10–14); status reporting (host-side, `src/metrics.rs`); error schemas (`:glia.error/*` dies with Glia; host taxonomy exists); capability sealing/identity (already substrate per CEO ledger); module/CID loading (already `crates/cell`). `KernelBootstrap` has exactly one consumer ⇒ port, don't extract.

---

## 8. Dual-path migration

- **Selection:** the §5 mechanism is the dual-path switch — no second flag. During the window, both kernels are embedded: Glia kernel at `bin/main.wasm` (default), `kernel-next` under `bin/kernel-next.wasm`; `WW_KERNEL=embedded:kernel-next` (or a path/CID) selects the new one. After the default flip (PR-7), positions swap; after deletion, the old artifact is gone.
- **Migration default:** Glia kernel until the canary gate passes; then kernel-next by default with Glia kernel still selectable for one release; then deletion.
- **Packaging:** both artifacts in the container deploy context and embedded set during the window (size cost: one extra wasm, bounded; verified in PR-5). `ww perform install` unchanged until PR-9/10 (it only writes `etc/init.d` + hashes embedded blobs — hash set updated in PR-5).
- **Runtime branch point:** exactly one — `ResolvedKernel` in `Commands::run` (`src/cli/main.rs:1529-1540` region). No branches inside executor/launcher/rpc.
- **Telemetry:** `/version` `kernel_cid` + `kernel_source` (PR-2) distinguishes flavors; canary compares phases + `readyz` transitions + route counts.
- **Parity checks:** §9 matrix run against both kernels via the PR-3 harness (parameterized over kernel artifact).
- **Rollback:** set `WW_KERNEL` back / redeploy previous default. One env var, no rebuild.
- **Maximum dual-path lifetime:** 3 weeks or two releases, whichever first. Exceeding it is an escalation to Louis (risk: dual-path lingering, §19).
- **Exact deletion gate:** parity matrix green on both kernels AND canary evidence artifact signed off (§10) AND Sol deletion-safety review of PR-9/10. No Glia feature work at any point in the window.

---

## 9. Behavioral parity matrix

Harness (PR-3): a new integration test binary `tests/pid0_e2e.rs` that boots the **real host + real kernel wasm** (closing the discovered gap: today zero `main.wasm` references in `tests/`), parameterized by kernel artifact via `WW_KERNEL`; plus a scripted epoch-advance fixture (test stem backend). Rows marked ☆ have **no existing automated coverage at the claimed level** — the harness adds them.

| # | Behavior | Existing test (level) | New test (level) | Success criterion | Rollback trigger |
|---|---|---|---|---|---|
| 1 | Cold boot to ready | unit stubs `std/kernel/src/lib.rs:3134` | ☆ e2e both kernels | `/readyz` 200 within timeout; phase sequence matches | any e2e divergence |
| 2 | `/status` serves | `tests/status_cell_e2e.rs:37`, `..._http_listener_e2e.rs:41` (component-level) | ☆ e2e through real kernel | 200 + non-null peer_id | divergence |
| 3 | Bootstrap publication before boot policy | `:3134` (unit) | ported to kernel-next (unit) | reverse graft answers before first route work | divergence |
| 4 | Reverse graft not-ready → ready | `:3084,:3115` (unit); host `src/executor.rs:974-1008` | ported (unit) | `INIT_MEMBRANE_NOT_READY` then success; host poll passes | divergence |
| 5 | Route readiness gating | `src/metrics.rs:724,:735` (unit) | ☆ e2e: `/readyz` 503→200 | matches current transitions | divergence |
| 6 | Epoch transition → re-graft | `:3153` (unit, asserts grafts==2) | ☆ e2e epoch restart via test stem | exactly one re-graft; routes replaced; `/status` 200 after | **hard fail** |
| 7 | Stale-epoch text spoof ignored | `:3197` (unit) | ported | only `StaleEpoch` code restarts | divergence |
| 8 | Repeated epoch events serialized | `:3316` (unit) | ported | one regraft at a time | divergence |
| 9 | Replacement-init failure surfaces | `:3238,:3220` (unit); `http_listener.rs:1189` | ported + e2e exit-code check | non-zero exit; no stale readiness route | hard fail |
| 10 | Old-generation teardown before activation | `:3286` (unit, Glia-shaped) | re-expressed for kernel-next | old session resources dropped first | divergence |
| 11 | Listener replacement / exactly-one-live-route | `http_listener.rs:1041-1161` (unit, host-side) | unchanged (host machinery untouched) | pass | n/a |
| 12 | Child failure isolation | `src/launcher.rs:607`; confinement suite | unchanged | pass | n/a |
| 13 | pid0 exit propagation | unit `src/executor.rs` region | ☆ e2e: kill kernel → host exit code | codes 0/1/137 preserved | divergence |
| 14 | Startup failure (bad wasm) | engine unit tests | ☆ e2e: `--kernel` garbage file | named error, non-zero exit, no hang | divergence |
| 15 | Shutdown via stdin EOF (TTY) | `stdin_shutdown_integration.rs:36` (echo only) | ☆ pid0 variant | exit 0 | divergence |
| 16 | Restart (outer supervisor) | none (systemd/k8s domain) | canary observation | service returns to ready | canary fail |
| 17 | Local path source | — | ☆ PR-2 unit+e2e | loads, logs `kernel_cid` | n/a (new) |
| 18 | CID source | — | ☆ PR-2 e2e (local Kubo) | loads, logs both CIDs | n/a (new) |
| 19 | Cache hit/miss (compile cache) | `engine.rs:239-304`; `metrics.rs:910` | unchanged | cache states reported | n/a |
| 20 | Fetch failure (Kubo down, CID source) | — | ☆ PR-2 | named error, no embedded fallback | n/a (new) |
| 21 | CID mismatch (raw-CID source) | — | ☆ PR-2 | hard fail, both CIDs in message | n/a (new) |
| 22 | Incompatible component | 5 s timeout path (unit) | ☆ e2e wrong-world artifact | named error | n/a (new) |
| 23 | Logging parity | manual | log-snapshot comparison in harness (best-effort) | phase lines present; kernel stderr bridged | advisory |
| 24 | Status identity (`/version`) | `metrics.rs:800` | ☆ PR-2: identity == loaded bytes for all 3 sources | `kernel_cid` correct | hard fail |
| 25 | Offline boot (Path/Embedded) | implicit | ☆ e2e with Kubo gate satisfied, no network fetch | boots | divergence |
| 26 | Resource/fuel limits | `sched.rs` EWMA + `proc.rs:617` yield | unchanged (host-side) | fuel metrics present | n/a |

"Divergence" = matrix row differs between Glia kernel and kernel-next ⇒ fix kernel-next (or document intentional delta with Louis sign-off) before PR-7.

---

## 10. Canary definition

Executable gate for: *do not delete Glia pid0 until a real deployment restarts through an epoch change on the WASM pid0.*

- **Environment:** the production cluster node (the `master.wetware.run` deployment; SSH to the cluster host per existing ops runbook). If Louis prefers a lower-blast-radius first pass, a staging namespace on the same cluster qualifies only if it runs the same container, stem backend, and Kubo topology; production remains the final gate.
- **Source form:** the container's embedded kernel-next selected via `WW_KERNEL` env in the deployment manifest (rollback = remove the env var). Deliberately *not* a CID source for the first canary — one variable at a time; a follow-up canary may exercise `--kernel bafy...`.
- **Duration:** 48 h soak minimum, including ≥1 deliberate epoch advance.
- **Deployment steps:** (1) capture baseline `/version`, `/readyz`, `/status`, `live_route_count`, logs; (2) apply manifest with `WW_KERNEL`; (3) verify ready within normal window; (4) soak 24 h; (5) trigger epoch advance via the deployed stem backend (IPNS republish or atom write — D7 §22 picks the mechanism and owner); (6) observe restart; (7) soak remaining window.
- **Expected epoch transition evidence:** kernel log shows stale-epoch detection + exactly one re-graft; `/readyz` transitions per `metrics.rs:735` semantics (old route expires, replacement installs, never zero-live-route flap longer than the replacement window); `/status` 200 after; `/version` `kernel_cid` unchanged across the restart.
- **Route/readiness/graft/restart checks:** scripted probe (extend `ww healthcheck` usage in `scripts/deploy_verify.sh` style): `/readyz`, `/status`, `/version` fields, route count, plus log-grep for the re-graft line. Restart check: `systemctl`/pod restart during soak must return to ready.
- **Rollback conditions:** `/readyz` unready > normal boot window; `/status` non-200 post-restart; route flap; kernel crash loop (>2 restarts/hour); any authority anomaly (unexpected graft names in logs). Rollback = remove `WW_KERNEL`, redeploy; capture logs first.
- **Evidence artifact:** `.context/canary-wasm-pid0-<date>.md` — manifest diff, timestamps, before/after captures, epoch-advance record, log excerpts, pass/fail per check.
- **Sign-off checklist:** all checks pass; evidence artifact written; Sol reviews the artifact + parity matrix; Louis signs the deletion gate.

---

## 11. Archive plan (`archive/glia-2026-08`)

Unchanged from the CEO review (§10 there), restated as an executable checklist — **PR-0, before any other PR touches the tree**:

1. From the current dirty workspace: `git checkout -b archive/glia-2026-08` (no stash — **never stash this tree**); commit **all 11 modified files** as one WIP commit, message bannered `REJECTED / UNSAFE / TWO KNOWN LEAKS — Glia PR-1 + PR-1b.0 Stages A-C, Stage C REJECT uncured (archived)`.
2. Second commit: curated corpus under `doc/archive/glia/` — allowlist only (~750 KB): the 21 `.context/pr1*|pr1b*` docs, `preflight-studies/batch1..9`, spike **source only** (no `target/` — `.context/spike/` is 4.7 GB of build outputs; two nested git repos at `.context/spike/ownership-spike/.git` and `.context/spike/cc-spike-mutations/ownership-spike/.git` → flatten by copying source files, or `git bundle` each), Sol verdict texts that are design content, `doc/designs/value-contract.md` snapshot.
3. Secret/path scrub: grep the curated set for tokens/keys/absolute home paths; strip `.context/attachments/` entirely (screenshots, pasted third-party content).
4. Branch README/index: what/why (demand, not failure), tree state (two live leaks named: cross-owner factory + body-hidden; Stage C Sol-R2 REJECT uncured; Stage D frozen), document map, unresolved decisions (the approval backlog from the CEO review §4.2 citation), revival instructions ("start from the retrospective + revival scorecard, not from this tree; restricted acyclic profile only").
5. Push branch; verify CI is **not** triggered on archive branches (or mark skip-ci); return to a clean master checkout for PR-1+.
6. Exclusions recorded in the README: build outputs, generated fixtures, attachments, anything failing the scrub.

Do not perform any of this until Louis ratifies (already ratified in strategic direction; PR-0 executes it).

---

## 12. Consumer-removal dependency graph

Order: replacement → flip → consumers → `crates/glia` last. Stages refer to §16 PRs.

| Consumer | Depends on Glia via | Replacement | Tests affected | Prerequisite | Stage | Fallout |
|---|---|---|---|---|---|---|
| `std/kernel` (Glia kernel) | entire pid0 | `std/kernel-next` | 10 lifecycle tests ported; init.d/Glia-form tests (`:2486-2530,:4785-:5008`) deleted | canary gate | PR-10 | none post-canary |
| `std/status/etc/init.d/05-status.glia` + installer `include_str!` (`src/cli/main.rs:2237,:2476`) + CI staging (`rust.yml:451`) + `tests/test_deploy_context.sh:25` | boot policy | hardcoded in kernel-next; installer stops writing the file; deploy context drops it; **update test_deploy_context.sh assertions in the same PR** | test_deploy_context.sh | PR-7 flip | PR-9 | `~/.ww/etc/init.d` leftover files ignored by new kernel (log once) |
| `ww init` scaffold (`src/cli/main.rs:1221-1254`) | writes `.glia` template | scaffold without init.d template (or minimal README note) | cli tests | PR-7 | PR-9 | doc updates |
| `ww shell` + MCP (`src/cli/shell.rs` ~800 Glia lines; 4 MCP tools) | Glia eval in CLI | retire with explicit errors + release notes (CEO decision D5: MCP dead for now); keep discovery/dial code paths that are Glia-free | `tests/shell_e2e.rs` deleted; `cli_shell_daemon_integration` kept (service file, no Glia) | demo migration (PR-8) done | PR-9 | README/MCP install flow (`:2403`) removed |
| `std/shell` + embedded shell.wasm + metrics hash + Makefile publish + `Containerfile.deploy` layer | shell.wasm cell | delete (already semi-vestigial — `ww shell` no longer spawns it, #506) | `shell_e2e` | none (independent) | PR-9 | `/version` drops `shell_wasm_blake3`; std namespace tree shrinks |
| `std/caps` | module import/eval for kernel+shell | delete (kernel-next reimplements the ~20-line path resolution) | caps tests deleted | PR-10 same PR as kernel swap | PR-10 | none |
| `std/lib/ww/*.glia` (692 lines) + `Makefile:118,:121` publish | stdlib | delete; std namespace = kernel+status only | — | PR-10 | PR-10 | namespace CID changes (WW_STD_CID rebuild) |
| Examples: `examples/*/glia/`, `etc/init.d/*.glia`, `examples/grants/` | demo drivers + grant pedagogy | §13 demo migration (Rust/CLI drivers); grants pedagogy moves into confinement-test docs + example READMEs | `child_authority_confinement.rs` t1/t4 (2 of 26) deleted or re-pointed | PR-8 | PR-8/PR-10 | 5 READMEs rewritten |
| WW_TTY in-kernel REPL | `run_shell` | idle-wait (D1) | `:3262` re-expressed | PR-4 | PR-4 | operator habit change; release note |
| Benches (`benches/glia_map.rs`, `kernel_dispatch.rs`) | `use glia` | delete + `Cargo.toml:129-133` entries | — | none | PR-10 | none |
| CI: `check-glia-effects` (`Makefile:9,36-44`, `rust.yml:79`), `.glia` copies (`rust.yml:451,666-692` — note `:689-692` copies an already-deleted path), kernel test step (`rust.yml:264` → re-point to kernel-next) | build plumbing | remove/repoint | test_deploy_context.sh | PR-9/10 | PR-10 | CI time drops |
| Docs: `doc/shell.md`, `doc/glia-cell-grants.md`, `doc/designs/glia-*.md`, `doc/capabilities.md` (error schema + "MCP = Glia eval"), README (2 code blocks, feature bullet, both roadmap items), `doc/architecture.md` (2 lines) | — | rewrite/delete per CEO plan §11.8 | — | PR-10 | PR-10 | — |
| TODOs (~10 Glia items incl. defcap-export L/P1, Snap v2 L/P2) | — | prune with one-line dispositions | — | PR-10 | PR-10 | — |
| `crates/glia` + `src/lib.rs` re-export + `GIT_COMMIT` (`crates/glia/build.rs` → root `build.rs`) | — | delete last | 732 tests removed | everything above | **PR-11** | `git mv std/kernel-next std/kernel` |

---

## 13. Demo migration

Scope-guarded: prove the substrate, do not design the action runner.

- **`/status`:** parity by construction (kernel-next registers it; canary proves it live). This remains the 60-second README demo unchanged from the user's side (`curl /status`).
- **Chess:** driver moves off `ww shell`. `examples/chess/proof/authority_proof.rs` already exists on master as a typed-capnp Rust proof (10 reproducible runs, commit-pinned). Extend it into the demo driver: a `cargo run -p chess --bin demo` (or small script wrapping it) that (1) spawns both cells with explicit grants, (2) plays the game over typed capnp, (3) prints each capability handoff. Re-record the asciinema. Distributed (two-node, DHT) variant second, reusing the existing libp2p path — no new networking.
- **Attenuation + observable denial:** the show-don't-tell replacement for the REPL moment. Extend the chess driver (or a 50-line sibling example) to: attenuate a cap to a method allowlist via `crates/membrane` (using the PR-1 resolution API), invoke an allowed method (succeeds), invoke a denied method (fails closed), print both results as **denial receipts** (log lines naming the missing capability — receipts here are logs, not product artifacts).
- **Scripted driver:** one `scripts/demo.sh` that runs status + chess + denial locally against `ww run`; used in READMEs and by canary smoke.
- **Local-first; distributed proof second** (matches CEO plan §7).

---

## 14. Post-removal tightening pass (bounded)

| Item | Class |
|---|---|
| Remove `check-glia-effects`, `.glia` CI copies, dead CI copy path (`rust.yml:689-692`), glia crate layers in Containerfiles | REQUIRED TO COMPLETE GLIA-ECTOMY |
| Delete `std/shell` artifact chain (embed, metrics hash, Makefile, Containerfile.deploy layer + stale `AdminUdsService` comment `Containerfile.deploy:8`) | REQUIRED TO COMPLETE GLIA-ECTOMY |
| README/docs WASM-first rewrite; prune Glia TODOs | REQUIRED TO COMPLETE GLIA-ECTOMY |
| `/version` kernel identity fix (loaded bytes) + `kernel_source` field | REQUIRED TO MAKE WASM-FIRST COHERENT (PR-2, already sequenced) |
| pid0 e2e harness in CI (PR-3 artifact kept permanently) | REQUIRED TO MAKE WASM-FIRST COHERENT |
| Fix `doc/architecture.md:153` `boot/` vs `bin/` entrypoint mismatch (document the dev shim `src/cli/main.rs:1502-1516`) | REQUIRED TO MAKE WASM-FIRST COHERENT |
| Delete dead `std/kernel/wit/kernel.wit`; delete vestigial `DEFAULT_KERNEL_CID` (`src/default_kernel.rs`) or wire it into §5 defaults — pick one, don't keep dead | REQUIRED TO MAKE WASM-FIRST COHERENT |
| Stale `Makefile:181` note; `Makefile` kernel targets consolidation | REQUIRED TO MAKE WASM-FIRST COHERENT |
| SIGTERM/graceful-shutdown handling in host (no signal handler exists today) | NICE TO HAVE — PARK |
| `ww healthcheck` canary-probe subcommand consolidation | NICE TO HAVE — PARK |
| Typed MCP-successor surface; WIT-typed grant descriptions | BUSINESS-PHASE BACKLOG |
| CID-source kernel canary (follow-up to §10) | BUSINESS-PHASE BACKLOG |
| `std/kernel` (Glia) sources, `std/caps`, `std/shell`, benches, shell_e2e | DELETE (PR-9/10/11) |

Hard boundary: nothing in this pass adds a config language, a new networking feature, or an orchestration surface.

---

## 15. Definition of done

- `archive/glia-2026-08` exists, indexed, scrubbed; dirty tree preserved as WIP commit.
- Main contains no Glia runtime/compiler; `crates/glia` removed; `std/kernel-next` renamed `std/kernel`.
- WASM pid0 (Rust kernel) is default; `--kernel`/`WW_KERNEL` path **and** CID sources work; resolved CID reported in logs and `/version` (loaded-bytes identity).
- Parity matrix (§9) green; all ☆ tests exist and run in CI; epoch-restart canary evidence artifact signed off.
- Chess and `/status` run without Glia; denial demo scripted; shell/MCP retired with explicit errors + release notes.
- Tests/CI green; release packaging Glia-free; docs WASM-first; extracted design rules (CEO ledger §8) recorded in `doc/`.
- No Glia implementation work active; backlog triaged per §14 classes.
- **Final state:** Wetware is technically coherent enough to switch primary focus to business validation (per CEO plan §16 gates, which resume the day the canary passes — earlier for discovery outreach, which is not blocked by any of this).

---

## 16. PR/branch sequence

All PRs stack on master (post-PR-0), each green before merge. Effort = CC-assisted implementation model.

| PR | Scope | Files/crates | Tests | Effort | Rollback | Depends | Hard stops | Handoff class |
|---|---|---|---|---|---|---|---|---|
| **PR-0** | Archive branch (§11) | archive branch only | n/a | 0.5 d | delete branch | — | never stash; scrub before push | LOWER-COST + **LOUIS verifies** |
| **PR-1** | Substrate extraction: attenuate resolution → `crates/membrane::schema`; `get_graft_cap` → `std/system` | `std/kernel/attenuate.rs`, `crates/membrane`, `std/system` | move + new unit tests; wasm32 build check | 0.5–1 d | revert | PR-0 | API frozen before PR-4 starts | LOWER-COST (interface frozen by this plan) |
| **PR-2** | `KernelSource`/`ResolvedKernel`; `--kernel`/`WW_KERNEL`; precedence; CID verify; resolved-CID logging; `/version` identity fix + new fields; `WW_KERNEL_ABI` host side | `src/cli/main.rs`, `crates/cell/src/loaders.rs`, `src/executor.rs`, `src/metrics.rs` | §9 rows 17,18,20,21,24 | 1–1.5 d | revert (flag unused by default) | PR-0 | no behavior change with no flag (row: byte-identical default) | **FABLE CHECKPOINT** (boundary spec) → LOWER-COST implement |
| **PR-3a** | pid0 e2e parity harness (`tests/pid0_e2e.rs`), non-epoch rows; **CI must gain `make std` in the test job** (today std wasm is never built there — status/kernel e2e silently skip, `rust.yml:215-218`) | `tests/`, `rust.yml` | §9 rows 1,2,5,13,14,15,22,25 vs current kernel | 2–2.5 d (incl. CI build-order + cache work) | n/a (tests + CI only) | PR-2 | harness must pass on **embedded** Glia kernel (the real production path, §5) before kernel-next exists | **FABLE CHECKPOINT** (harness design) → LOWER-COST |
| **PR-3b** | Epoch-advance e2e: file-based test stem backend (explicit, named test seam in `crates/stem`) OR anvil+contract fixture (Foundry is in the CI test job); Fable picks at checkpoint | `crates/stem` or `tests/fixtures` | §9 row 6 | 1–1.5 d (this is host-surface work, **not** "tests only") | revert seam | PR-3a | seam must be unreachable in production config | **FABLE CHECKPOINT** (seam design) → LOWER-COST |
| **PR-4** | `std/kernel-next` crate (§6): graft/session, reverse graft, boot policy (/status), epoch/generation loop, TTY idle-wait, logging; ported stub harness + 10 lifecycle tests; `WW_KERNEL_ABI` guest side | new `std/kernel-next` | ported unit tests + harness green | 3–4 d | crate unused until selected | PR-1,2,3 | graft names frozen (no new caps); capnp-rpc pin present | **SOL REVIEW** (kernel contract + authority) |
| **PR-5** | Dual-path packaging: embed both, Makefile/CI/deploy-context ship both; installer hash set | `build.rs`, Makefile, `rust.yml:444-451`, `src/cli/main.rs` embed list | deploy-context test updated (both artifacts) | 0.5–1 d | revert | PR-4 | binary-size check | LOWER-COST |
| **PR-6** | Canary (§10): manifest change, probes, evidence artifact | infra repo + `.context` artifact | canary checklist | 0.5 d + 48 h soak | remove `WW_KERNEL` | PR-5 | rollback conditions §10 | **LOUIS** (deploy) + **FABLE** (interpretation) + **SOL** (gate review) |
| **PR-7** | Default flip: kernel-next = `bin/main.wasm` default; Glia kernel selectable one release | Makefile, embed registration | full parity matrix re-run | 0.5 d | flip back | PR-6 signed | — | **LOUIS DECISION** → LOWER-COST |
| **PR-8** | Demo migration (§13): chess driver from authority_proof, denial demo, scripts/demo.sh, re-record | `examples/chess`, scripts, READMEs | driver runs in CI (smoke) | 1–2 d | keep old snippets until merge | PR-4 (local), not gated on canary | no new networking | LOWER-COST |
| **PR-9** | Consumer removals wave 1: shell/MCP retirement (explicit errors + release notes), `std/shell` chain (incl. `Containerfile.deploy` CMD dropping the shell layer — coordinate wetware/infra manifests), `ww init` template, installer init.d write, deploy-context `.glia` drop + test_deploy_context.sh update | `src/cli/shell.rs`, `src/cli/main.rs`, `std/shell`, CI, Containerfiles | shell_e2e deleted; cli tests updated | 1.5–2 d | revert | **PR-7 + Glia-selectable rollback window CLOSED** (removing init.d delivery while the Glia kernel is still the rollback target would boot it route-less — finding 3) | error messages + release notes reviewed; infra-repo manifest PR linked | LOWER-COST + **SOL** (deletion safety) |
| **PR-10** | Consumer removals wave 2: `std/kernel` (Glia), `std/caps`, `std/lib/ww/*.glia`, examples `.glia`, grants fixtures, confinement t1/t4, benches, CI glia steps, docs, TODOs | broad | test count drops ~880; boot-parity + confinement (24) must stay green | 1.5–2 d | revert | PR-9 | **false-green check:** parity harness + confinement + status e2e green before merge | LOWER-COST + **SOL** (final diff) |
| **PR-11** | `crates/glia` deletion; `git mv std/kernel-next std/kernel`; `GIT_COMMIT` → root build.rs; workspace manifest cleanup | root | workspace green | 0.5 d | revert | PR-10 | — | LOWER-COST |
| **PR-12** | Tightening (§14 REQUIRED classes not already landed) | misc | — | 1 d | revert | PR-11 | scope classes only | LOWER-COST |

---

## 17. Model/agent handoff allocation

| Stage | Model class | Min reasoning | Self-contained artifact required | Stop/escalate when |
|---|---|---|---|---|
| PR-0 archive | lower-cost | low | §11 checklist | any scrub hit; any git anomaly on the dirty tree → **stop, Fable** |
| PR-1 extraction | lower-cost | low | §7 table (API shapes frozen) | membrane wasm32 build fails; any `glia::` import needed → Fable |
| PR-2 boundary | Fable spec (done — §3/§5), lower-cost implement | medium | §3+§5 | any change to graft contents or spawn lattice → Fable |
| PR-3 harness | Fable spec (done — §9), lower-cost implement | medium | §9 matrix | harness can't reproduce a row against Glia kernel (hidden behavior!) → **Fable, mandatory** |
| PR-4 kernel-next | lower-cost implement, **Sol review** | medium-high | §4 contract + §6 plan + ported stubs | any need for a capability not in the 7-name graft; any `wetware:streams` change; any WW_KERNEL_ABI bump → Fable+Sol |
| PR-5 packaging | lower-cost | low | §8 | binary size anomaly → Louis |
| PR-6 canary | Louis executes; Fable interprets; Sol reviews evidence | high (interpretation) | §10 checklist + evidence template | any rollback condition fires → Fable before retry |
| PR-7 flip | Louis decision | — | canary artifact | — |
| PR-8 demos | lower-cost | low | §13 | any new networking need → stop (scope control) |
| PR-9/10 removals | lower-cost, **Sol** deletion-safety + final diff | medium | §12 graph | any test that can't be ported/deleted cleanly; any only-Glia coverage discovered → Fable |
| PR-11/12 | lower-cost | low | §12/§14 | — |

Fable is **not** used for routine coding. Sol checkpoints: PR-4 (kernel contract/adversarial authority), PR-3+PR-6 (lifecycle parity + canary gate), PR-9/10 (removal safety), PR-11 (final main diff).

---

## 18. Implementation handoff package

**This document is the package.** An implementation model needs, in order: §16 (PR order + hard stops), §2 (file/symbol map), §3–§5 (frozen interfaces), §6 (kernel plan), §9 (acceptance tests), §11–§12 (archive + removal mechanics), §19 (risks), plus these frozen decisions:

1. pid0 stays a wasip2 component behind the existing 4-point contract (D9 RESOLVED, §25 — deployment composition lives in the kernel, outside the native trust root; kernel-editing is the step-1 authoring workflow); **no new WIT world; no authority in WIT; no new graft capabilities; no manifest/config language during the Glia-ectomy** unless a second concrete deployment requires materially different policy.
2. `KernelSource` per §5; runtime identity = blake3 CIDv1(raw) of loaded bytes, always.
3. No manifest/config language (env vars only); `/status` hardcoded in kernel-next.
4. Dual-path via `WW_KERNEL`; Glia default until canary; max window 3 weeks/2 releases.
5. capnp-rpc fork pin carried into kernel-next; enforced by `tests/child_authority_confinement.rs:37`.
6. Exclusions (do not build): action runner, approval semantics, product receipts, portable callables, durable values, GC, Glia revival, manifest language, orchestration, new networking, MCP replacement, Go/TinyGo SDK.
7. Rollback procedures: per-PR revert; canary rollback = remove `WW_KERNEL`; flip rollback = re-flip.
8. Escalate to **Fable**: graft/spawn-lattice changes; hidden pid0 behavior found by the harness; ABI bump; canary anomalies; architecture drift. Review by **Sol**: PR-3 harness adequacy, PR-4, PR-6 evidence, PR-9/10, PR-11 final diff.
9. Working-tree rule: the dirty `glia-control-extraction` tree is archived by PR-0 and **never modified again**; all PRs branch from clean master (`f1365b6` or later).

---

## 19. Risk register

| Risk | L | I | Mitigation | Test/gate | Blocks deletion? | Escalation |
|---|---|---|---|---|---|---|
| Archival loss (dirty tree damaged before PR-0) | M | H | PR-0 first; no stash; no rebase; commit-as-is | branch pushed + verified | YES | Louis |
| Hidden pid0 behavior (beyond inventory) | M | H | PR-3 baselines harness against **current** kernel before kernel-next exists; §2 built from file:line inventory, not docs | harness green on Glia kernel | YES | Fable (mandatory stop) |
| Circular kernel fetch (CID source needs IPFS before pid0) | L | M | fetch via host Kubo client behind existing `waiting-for-kubo` gate; embedded default needs no network; no fallback masking | §9 rows 18,20,25 | no | — |
| ABI instability (contract drifts during window) | L | H | 4-point contract frozen; `WW_KERNEL_ABI=1`; changes bump integer + Fable | ABI mismatch test | YES | Fable |
| Capability overgrant to new kernel | L | H | same `HostGraftBuilder`, zero new names; `graft.rs:601/793` name-set tests unchanged | graft name-set tests | YES | Sol |
| Boot deadlock (reverse-graft ordering) | M | M | port publish-before-boot-policy ordering (`:2429` before `:2431`); host 120 s timeout aborts | §9 rows 3,4; e2e row 1 | YES | Fable |
| Lifecycle regressions (epoch/replacement) | M | H | port generation policy exactly; §9 rows 6–10; canary epoch restart | matrix + canary | YES | Sol |
| False-green after deletion (55% of tests removed) | M | H | PR-3 harness + ported lifecycle tests land **before** any deletion; PR-10 hard stop re-runs harness + confinement (24) + status e2e | PR-10 gate | YES | Sol |
| Glia surviving indirectly (stray dep/script) | M | L | PR-11 workspace grep `glia` (code, Cargo.toml, CI, Makefile, Containerfiles); CHANGELOG note | grep clean | YES (PR-11) | — |
| Dual-path lingering | M | M | 3-week/2-release hard window; exceed ⇒ Louis escalation | calendar gate | — | Louis |
| Config-language creep | M | M | frozen decision 3; any structured-config proposal requires a named second consumer + Louis | review checklist | no | Louis |
| Host-bootstrap creep (policy leaking into native host) | M | M | §3 exclusion list; boot order/routes/restart policy live in kernel only | PR-2/4 review | no | Fable |
| Privileged-kernel ambient authority (new kernel asks for more) | L | H | same graft; Sol adversarial review of PR-4 | Sol checkpoint | YES | Sol |
| Upgrade/rollback failures | L | M | one-env-var rollback; both artifacts shipped during window | canary rollback drill (step 2→rollback→redeploy once) | YES (canary) | Louis |
| Future TinyGo compatibility | L | L | contract is language-neutral (§4 note); no action | — | no | — |
| Business focus delayed by perfectionism | M | H | §14 classes; NICE/BACKLOG parked; DoD §15 is the finish line; discovery outreach (CEO plan) is explicitly **not blocked** by this work | Louis reviews scope weekly | — | Louis |

---

## 20. Scope/drift controls

Not implemented or deeply designed here (verbatim from brief, binding): policy-gated action runner; approval semantics; product receipts; portable callables; durable value plane; GC/process heaps; cycle collector; Glia revival; manifest language; orchestration platform; new networking; MCP replacement; Go/TinyGo SDK. Additional controls: no new graft capabilities; no WIT authority; no second config consumer; denial "receipts" in PR-8 are log lines, not product artifacts; any PR whose diff touches `crates/authority`, `crates/membrane` policy logic, or `crates/rpc/graft.rs` beyond the listed changes stops for Fable.

**D9 guardrails (hard stop conditions, binding):**

> If the reference WASM kernel grows into a generic orchestration framework, or common deployment changes require repeatedly editing low-level reverse-graft/Cap'n Proto/runtime internals, **stop and reassess the authoring surface**. That is evidence for a higher-level configuration or scripting layer.

> **Do not add a manifest/config language during the Glia-ectomy** unless a second concrete deployment requires materially different policy.

---

## 21. Estimated effort and critical path

**Critical path:** PR-0 → PR-2 → PR-3 → PR-4 → PR-5 → PR-6 (48 h soak + epoch event) → PR-7 → PR-9 → PR-10 → PR-11.

- Engineering (CC-assisted, per §16): ~11–15 working days total; critical-path engineering ~8–10 days.
- Wall clock dominated by: canary soak (≥48 h + scheduling the epoch advance) and Sol review turnarounds (4 checkpoints).
- **Realistic calendar: 3–4 weeks** to Definition of Done, matching the strategic direction's "bounded engineering reset." PR-8 (demos) runs off-critical-path in parallel after PR-4.
- Business-phase handoff: discovery outreach per the CEO plan is independent and can start immediately; full focus switch at DoD.

---

## 22. Decisions Louis must make

1. **D1 — New kernel TTY behavior** (`WW_TTY=1`, today: in-kernel Glia REPL). Options: (a) idle-wait like daemon, log "REPL removed; use logs/`/status`" *(recommended — smallest surface, no hidden mode)*; (b) exit immediately with message. Affects PR-4.
2. **D2 — Canary environment**: straight to production `master.wetware.run` *(recommended — it is the qualifying environment per brief)* vs staging-first on same cluster. Affects PR-6.
3. **D3 — Default-flip timing**: immediately on canary sign-off *(recommended)* vs one extra release of opt-in.
4. **D4 — Crate naming**: `std/kernel-next` → rename to `std/kernel` at PR-11 *(recommended)* vs keep permanent new name.
5. **D5 — `DEFAULT_KERNEL_CID` vestige**: delete *(recommended)* vs wire into §5 as a pinned-CID default.
6. **D6 — Shell/MCP retirement messaging** (carried from CEO review D5): confirm release-note wording + explicit-error text at PR-9.
7. **D7 — Epoch-advance mechanism for the canary**: IPNS stem republish vs atom write; and who executes it (Louis has cluster SSH). Affects §10 step 5.

---

*Prepared by /plan-eng-review, 2026-08-04. Grounding: fresh file:line pid0 inventory (this session), CEO strategic review, coupling map. Outside voice: see §23 + review report below. Nothing modified, no branches created, no commits.*

---

## 23. Outside-voice reconciliation (v2 amendments — BINDING, override base text)

Adversarial pass by an independent, fresh-context reviewer that verified every claim against the repo. All 11 findings accepted (none refuted). Amendments:

**A1 (finding 1 — embedded shadowing).** Folded into §5 in place. Additional binding rules: PR-5 either keeps embed-set and deploy-context kernel copies byte-identical **or deletes the dead container copies** (recommended: delete — one source of truth); §16 PR-5 acceptance adds an "embed/deploy lockstep or removal" check. The §9 baseline column is amended: *baseline = embedded kernel path* for every row.

**A2 (finding 2 — init.d is mounted-layer boot policy, not just /status).** The release pipeline builds per-example subtrees `examples/<name>/etc/init.d/` so `ww run /ipns/<key>/examples/<name>` boots that layer's routes (`rust.yml:664-692`); epoch restarts re-run init.d against the new head, so boot policy is epoch-updatable *data* today. Hardcoding `/status` deletes that mechanism, freezing boot policy into the kernel binary. **Amendments:** (a) PR-3a adds an inventory step: enumerate the *production head's* actual init.d contents and every `ww run` invocation documented in READMEs/infra — the claim "only /status is production boot policy" must be verified, not assumed; (b) §9 gains row 27: "mounted-layer contributes routes" — classified **INTENTIONAL DELTA** (examples migrate to explicit drivers in PR-8; boot-policy changes post-migration require redeploy, not head republish) — requiring explicit sign-off as **D8** (§22); (c) if the PR-3a inventory finds any non-example production consumer of mounted init.d, that is a **hard stop → Fable** before PR-4 proceeds.

**A3 (finding 3 — PR-9 destroyed the rollback window).** Fixed in the PR-9 row: PR-9 is gated on the Glia-selectable window *closing*, not on PR-7 merging. During the window, the installer keeps writing `05-status.glia` and the deploy context keeps shipping it, so a rollback to the Glia kernel boots with routes.

**A4 (finding 4 — epoch fixture is host-surface work).** PR-3 split into PR-3a (non-epoch rows, tests+CI) and PR-3b (epoch seam: file-based test stem backend in `crates/stem` or anvil+contract fixture; Fable picks; seam must be unreachable in production config). Reflected in §16.

**A5 (finding 5 — CI never builds std for the test job).** PR-3a explicitly adds `make std` to the test job with correct build order (std wasm before the host test binary compiles, so the embedded path is exercised) + cache keys. Until then, `status_cell_*_e2e` silently skip in CI (`tests/status_cell_http_listener_e2e.rs:33-46`) — treat their local-only status as a known gap, not coverage.

**A6 (finding 6 — /version is an admin-plane refactor).** `VersionInfo` is built by value before Kubo wait/kernel resolution (`src/cli/main.rs:1557-1602`) so `/version` serves during outages — preserve that property. Amended PR-2 spec: kernel identity fields are late-bound shared state (e.g. `Arc<OnceLock<KernelIdentity>>`); before resolution `/version` reports `kernel_cid: null, kernel_source: "<pending: requested source>"`. PR-2 estimate: 1.5–2.5 d.

**A7 (finding 7 — branch point is in the executor).** Corrected claim: `ResolvedKernel` must be threaded through `CellBuilder`/`Cell::spawn_with_streams` (`src/executor.rs:428-441,507-512`) — an executor API change. Still exactly one *decision* point (resolution in `Commands::run`), but the plumbing touches the executor; PR-2 files list gains `src/executor.rs` prominently (already listed) and its Fable checkpoint covers this API.

**A8 (finding 8 — workspace/rename churn).** kernel-next standalone workspace needs its own clippy/fmt/lock/cache CI steps (mirroring `rust.yml:264`); the `has_wasm_*` cfg chain, Makefile targets, deploy-context paths, `Containerfile.deploy` CMD (which also mounts the shell layer PR-9 deletes), and **wetware/infra k8s manifests** all change across PR-5/9/11. PR-11 re-estimated **1–1.5 d**; PR-9 row updated with the infra-repo coordination requirement.

**A9 (finding 9 — --kernel does not decouple from the image; offline claim corrected).** New §5 invariant: **the merged image must still supply `$WW_ROOT/bin/status.wasm`** (and whatever future boot policy reads); `--kernel` replaces the *kernel*, not the image. §9 rows 17/18 success criteria amended: "loads, logs kernel_cid, **and reaches ready**" (a kernel that boots route-less passes nothing). Offline claim corrected: `ww run` unconditionally requires a local Kubo daemon (`wait_for_kubo_ready` + MFS mount resolution, `src/cli/main.rs:1605-1633,1726`); "offline" means only *no remote fetch*, never *no Kubo*.

**A10 (finding 10 — contract gaps).** §4 amendments: (a) kernel obligation: create exactly **one** `wetware:streams` connection (WIT permits N; the contract does not); (b) kernel obligation: the reverse-graft `Membrane.graft()` must be **repeatable and idempotent** — the host polls it every 100 ms until ready and calls it again for later Terminal logins; (c) contract point 4 (route ⇒ readiness) holds only when `--http-listen` is set (`route_registry` is `None` otherwise) — the no-HTTP mode's readiness semantics are "reverse graft only"; (d) §9 row 13 corrected: pid0 exit propagation covers wasm exit status (0/non-zero) only — 137 belongs to `OwnedChildLifecycle` child cells, not pid0; (e) `WW_TTY` is truthiness-blind today (`is_ok()`, `src/executor.rs:441` — `WW_TTY=0` enables TTY mode); D1's test must pin current semantics; changing it is §14 tightening, not parity.

**A11 (finding 11 — canary hardening).** §10 amendments: (a) after the PR-7 flip, a **24 h post-flip production soak on the embedded-default path** is required before PR-9 (the pre-flip canary soaks only the explicit-`WW_KERNEL` path); (b) drop the vacuous "kernel_cid unchanged across restart" check; replace with "kernel_cid matches the deployed artifact's expected CID"; (c) the scripted probe monitors `live_route_count` and the full route set the head registers, not `/status` alone; (d) D7 corrected: the epoch-advance mechanism is whichever stem backend production actually runs (`crates/stem/{atom.rs,ipns.rs}`); if atom, the advance is an on-chain write needing a funded signer — D7 must name the signer and the exact command; (e) the rollback drill (deploy → rollback → redeploy once) is now step 4b of §10's deployment steps, not just a risk-register line.

**Net effect on estimates (§21):** critical-path engineering ~10–13 days (was 8–10); calendar 3.5–4.5 weeks including the post-flip soak. The strategic direction ("bounded engineering reset") still holds.

**New decision for Louis — D8 (mounted-layer boot policy delta):** accept that post-migration, boot policy is compiled into the kernel (changes require redeploy, not head republish), with examples migrated to explicit drivers — OR direct a follow-up design for data-driven boot policy (which reopens the config-format question CEO-D4 closed). *Recommended: accept the delta now; revisit only if a real deployment needs head-updatable boot policy (that would be the "second consumer" CEO-D4 requires).* Gated by the PR-3a inventory (A2c).

## 24. Codex reconciliation (v3 amendments — BINDING; override §23 where they conflict)

Manual Codex run against the v2 plan (verdict: "not ready — can still false-green on exported authority, baseline attribution, and deletion ordering"). All nine findings accepted in substance; finding 8/9 becomes a decision, not an amendment. Estimates corrected.

**A12 (finding 1 — reverse-graft contract must freeze content, not success).** The host readiness poll accepts *any* successful `graft()` and discards its contents (`src/executor.rs:688-689`); today's kernel forwards the full upstream graft (`std/kernel/src/lib.rs:99-113`). A kernel serving an empty or wrong cap set would pass readiness and `/status` while every external Terminal client breaks. §4 amendment: the reverse-graft contract is **"forward the complete upstream graft name-set with unchanged capability semantics, every generation"** — frozen alongside idempotence. Enforced by new parity row 28 (external Terminal login receives the exact usable cap set, before and after an epoch restart), asserted in the harness by a real Terminal-path client, not by `graft()` success.

**A13 (finding 2 — exit semantics are a hidden contract).** `std/system::serve` logs closure errors and returns success (`std/system/src/lib.rs:640-647`); the Glia kernel signals replacement-init failure only via an out-of-band flag. A naive port turns graft loss / malformed caps / RPC transport death into exit 0. §4/§6 amendment: kernel-next enumerates fatal vs nonfatal classes — fatal (non-zero exit): bootstrap/graft acquisition failure, malformed or incomplete graft, RPC transport death outside clean shutdown, replacement-generation init failure, ABI mismatch; nonfatal (log + continue): individual boot-step failure in generation 0, transient epoch-probe errors. Implementation: kernel-next wraps `serve` with an outcome channel (or uses a `run_with_session` variant returning the closure/transport result) — the swallow-and-return-success behavior of `std/system::serve` must not be inherited. New parity row 29: transport death ⇒ specified non-zero exit, never 0.

**A14 (finding 3 — `WW_KERNEL_ABI=1` is ceremonial without a real fingerprint).** The actual ABI includes the capnp schema IDs/ordinals and the patched capnp-rpc fork. Phase policy: **kernel artifacts are version-locked to the host build** — cross-version loading (old host + new path/CID kernel or vice versa) is unsupported in this phase and must fail closed. Mechanism: the host passes `WW_KERNEL_ABI_FPR` = hash over the schema-CID set (already computed by `schema-id` / `std/kernel/build.rs:34-52`) + the capnp-rpc fork revision; kernel-next verifies and exits with a named error on mismatch. Compatibility fixture matrices are explicitly deferred until independent kernel rollout becomes a product need (see D9). New parity row 30: ABI absent / malformed / mismatched.

**A15 (finding 4 — baseline attribution).** PR-3a as sequenced baselines the Glia kernel on the *post-PR-2* host, so harness failures can't be attributed. Resequenced: **PR-3a0** — minimal harness (boot-to-ready, `/status`, readiness transitions, stdin-EOF, exit propagation) against the **current unmodified host + embedded Glia kernel**, lands **before PR-2**; **PR-3a1** — selector/source/identity rows after PR-2. Critical path becomes PR-0 → PR-3a0 → PR-2 → PR-3a1 → PR-3b → PR-4 → …

**A16 (finding 5 — parity rows 28–33 added).** 28: Terminal-visible cap set pre/post-epoch (A12). 29: transport-failure exit semantics (A13). 30: ABI fingerprint absent/malformed/mismatch (A14). 31: CLI > env > embedded precedence + explicit `file:`/`cid:` prefixes + pre/post-flip default mapping. 32: missing/corrupt `$WW_ROOT/bin/status.wasm` ⇒ deterministic named failure or **bounded** unready state — note route-waiting is unbounded today (`src/cli/main.rs:1981-1999`); bound it in PR-2 (startup route-readiness timeout, named error). 33: no-HTTP mode readiness semantics (A10c) as an automated row.

**A17 (finding 6 — two deletion-order holes closed).** (a) New **PR-7b**: after the flip, remove the `WW_KERNEL` override from the production manifest, deploy, and record the 24 h soak on the true embedded-default path — the §10/A11a soak is only valid via PR-7b, since the canary manifest pins `WW_KERNEL`. (b) Glia init.d delivery (installer write + deploy-context copy) is retained through **PR-10** and removed in the same PR that deletes the Glia kernel — never before — so intermediate master cannot expose a selectable route-less kernel. PR-9's scope note in §12/§16 is amended accordingly (PR-9 keeps `05-status.glia` delivery intact).

**A18 (finding 7 — estimates corrected).** PR-3b: **3–5 d** (the CLI wires the atom-specific `EpochService` directly, `src/cli/main.rs:1865-1879`; `StemSource` exists but is not seam-shaped — this is real host surgery). PR-9: **4–5 d**. PR-10: **4–6 d**. §21 restated: summed engineering ≈ **20–28 CC-assisted days**; critical path ≈ 14–19 days; **calendar 5–7 weeks** including both soaks and Sol turnarounds. This is materially more than v1's 3–4 weeks and is an input to D9. "Bounded engineering reset" still holds only if D9 is resolved promptly and scope controls are enforced.

**A19 (findings 8/9 — KEEP PID0 NATIVE steelman → D9, blocking).** Codex's alternative: delete the WASM kernel entirely; a native `Pid0` host task reuses `HostGraftBuilder`'s session directly, exposes the membrane to Terminal, registers `/status` through the typed listener, consumes the epoch watch in-process; canary via `WW_PID0=glia-wasm|native`. Claimed savings: KernelSource/CID identity, WIT transport, reverse-graft polling, kernel packaging, capnp cross-version ABI, and rename churn — ~4–7 days cheaper before shared work — and pid0 failure already terminates the host, so WASM buys less fault independence than advertised. **Counter-analysis (this review):** (i) native pid0 is *not* the conservative option — pid0 has been a WASM cell in every shipped configuration; host-process pid0 is a **new** architecture requiring its own rewiring of `spawn_serving_with_ready`, export-policy readiness, `Pid0RegistrationScope`, and Terminal serving — much of the claimed saving is spent there, inside the trust root; (ii) it moves boot policy *into* the native host, violating the brief's own boundary ("the native host owns only the irreducible bootstrap mechanism") and growing the native TCB; (iii) A14's version-locking neutralizes most of the cross-version ABI risk cheaply; (iv) what WASM-pid0 uniquely preserves: minimal host trust root, the content-addressed-kernel product story, fuel/memory containment of boot policy, independent kernel rollout later, language-neutral replaceability. What native uniquely preserves: fewer moving parts, no packaging/selector/ABI machinery at all. **Recommendation: stay with WASM pid0** (it is the incumbent boundary and keeps the trust root minimal), accepting A12–A16 as the price of making that boundary real. But this is a genuine fork with a credible cross-model dissent and a ~15–25% total-effort delta — **D9, Louis decides**; PR-2 and beyond are blocked on it (PR-0, PR-1, PR-3a0, PR-8 are path-independent and may proceed).

**A20 (verdict).** Plan verdict changes from PROCEED to **PROCEED WITH REQUIRED CHANGES**: required = A12–A17 folded (done, this section), estimates A18 adopted, D9 resolved before PR-2. *(D9 resolution recorded in §25.)*

## 25. D9 resolution (v4 — BINDING; supersedes A19's open status)

**Decision (Louis, 2026-08-04): KEEP PID0 AS A WASM COMPONENT.** PR-2 and beyond are unblocked.

**Rationale (corrected — this, not a permanent minimalist/"suckless" philosophy, is the recorded ground):**

> Deployment composition is expected to vary and belongs outside the native trust root. For now, editing and recompiling a small Rust WASM kernel is an acceptable **temporary authoring workflow** while real usage reveals the stable configuration surface. We do not invent a JSON/TOML/DSL configuration layer before we know what is actually stable and declarative.

pid0 is expected to specify deployment policy: which cells start by default; what grants they receive; which capabilities/services are exported; which exports are exposed over libp2p; which WAGI/HTTP routes exist; supervision/restart behavior; epoch-driven boot/reconfiguration policy. Therefore: the native host owns **irreducible bootstrap mechanism only**; pid0 owns **deployment composition policy**; making pid0 native would either move variable policy into the trust root or force premature design of a configuration/orchestration language. This is the principal answer to the KEEP-PID0-NATIVE steelman (§24 finding 8/9), which remains preserved for the record.

**Expected authoring progression (evidence-gated ladder — each step requires the prior step's evidence, never anticipation):**

1. **Now:** edit the reference Rust kernel; compile to WASI P2; run by local path or CID.
2. **Later, if stable scalar parameters emerge:** flags, args, env, or a very small structured config.
3. **Later, if repeated deployment patterns emerge:** a builder, generated kernel code, TOML, or another constrained configuration format.
4. **Only with evidence:** scripting or dynamic composition.

**Hard guardrails** (duplicated in §20 scope controls; binding on every PR):

> If the reference WASM kernel grows into a generic orchestration framework, or common deployment changes require repeatedly editing low-level reverse-graft/Cap'n Proto/runtime internals, stop and reassess the authoring surface. That is evidence for a higher-level configuration or scripting layer.

> Do not add a manifest/config language during the Glia-ectomy unless a second concrete deployment requires materially different policy.

**Sections amended by this resolution:** §1 (executive recommendation — D9 paragraph), §5 (path-or-CID as the authoring loop), §6 (behavior classification tied to ladder step 1), §18 (frozen decision 1), §20 (guardrails), §24 A19/A20 (status), review report (verdict + unresolved list). **All §23 and §24 technical amendments remain binding in full** — reverse-graft content/semantics parity (A12, row 28), fatal/nonfatal exit contract (A13, row 29), schema/fork fingerprinting (A14, row 30), PR-3a0 baseline before source-selection changes (A15), parity rows 28–33 (A16), PR-7b true-default soak and atomic Glia selector/init.d removal in PR-10 (A17), corrected estimates (A18: ~20–28 CC-days, critical path 14–19 days, 5–7 weeks calendar). PR sequencing is unchanged from v3; the only sequencing effect of D9's resolution is that nothing blocks PR-2+ anymore. D8's framing is refined by the ladder: the frozen-boot-policy delta is not merely accepted — kernel-editing *is* the intended step-1 authoring surface; D8's PR-3a0 inventory gate stands unchanged.

## GSTACK REVIEW REPORT

| Review | Trigger | Why | Runs | Status | Findings |
|--------|---------|-----|------|--------|----------|
| CEO Review | `/plan-ceo-review` | Scope & strategy | 1 (2026-08-04) | issues_open | mode: SCOPE_REDUCTION, 0 critical gaps; 7 ratification items |
| Codex Review | `/codex review` | Independent 2nd opinion | 2 (manual: CEO phase + eng plan v2) | issues_found | eng pass: 9 findings → §24 amendments A12–A20 + D9 |
| Eng Review | `/plan-eng-review` | Architecture & tests (required) | 1 (2026-08-04) | issues_open | 11 + 9 outside-voice findings, all accepted (§23/§24); 0 critical gaps unresolved |
| Design Review | `/plan-design-review` | UI/UX gaps | 0 | — | no UI scope |
| DX Review | `/plan-devex-review` | Developer experience gaps | 0 | — | not run |

**CODEX:** eng-plan pass verdict "not ready" → all 9 findings folded (§24); the reverse-graft content contract, exit-semantics contract, ABI fingerprint, baseline resequencing, PR-7b, and PR-9/PR-10 init.d retention are its direct products.

**CROSS-MODEL:** Claude outside-voice: 11 findings, zero refuted, folded as §23. Codex: 9 findings, zero refuted in substance, folded as §24; the one recommendation this review declines to auto-adopt is KEEP PID0 NATIVE, elevated to D9 with a stay-WASM recommendation and the full steelman preserved. CEO-phase Codex corrections remain binding upstream.

**VERDICT:** ENG review complete — **PROCEED WITH REQUIRED CHANGES** (required changes = §23 + §24, all folded; **D9 RESOLVED → WASM pid0, §25**; PR-0 onward may start on ratification of the remaining list; nothing blocks PR-2+).

**UNRESOLVED DECISIONS:**
- D1 new-kernel TTY behavior (recommend idle-wait, no REPL)
- D2 canary environment (recommend production)
- D3 default-flip timing (recommend on canary sign-off)
- D4 crate naming (recommend rename at PR-11)
- D5 DEFAULT_KERNEL_CID vestige (recommend delete)
- D6 shell/MCP retirement messaging (wording sign-off at PR-9)
- D7 epoch-advance mechanism + signer for the canary
- D8 mounted-layer boot policy delta (recommend accept — refined by §25 ladder; gated by PR-3a0 inventory)
- + 7 unresolved from prior reviews (CEO ratification list)
