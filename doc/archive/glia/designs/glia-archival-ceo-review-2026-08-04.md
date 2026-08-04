# Glia Archival — CEO Strategic Review

**Date:** 2026-08-04
**Repo:** wetware/ww @ `f1365b6` (branch `glia-control-extraction`, uncommitted +4,663/−1,229)
**Decision under review:** whether to archive Glia (the embedded Lisp) and make Wetware WASM-first
**Method:** `/plan-ceo-review` (maximum-rigor challenge of a scope-reduction proposal) + three-agent evidence sweep (memory-model artifacts, repo coupling map, product/discovery corpus) + gbrain retrieval + two independent outside-voice challenges (Claude subagent; OpenAI Codex, run manually)
**Status:** review complete; **7 decisions await Louis's ratification** (§18)

---

## 1. Executive verdict

**Archive Glia. Wetware is WASM-first.** Adopt disposition 5 — *extract substrate improvements, preserve Glia on `archive/glia-2026-08`, remove from main* — with the amended sequencing in §12 (dual-path pid0 + canary gate, per Codex) and the demand gates in §16.

The decision rests on:

- **Demand:** zero user-demand artifacts for Glia in ~5 months of records. Its constituency is five named individuals ("Lisp audience": Blair/Jinsuk/Mikel/Yorgos/Jorge) with no recorded feedback from any of them. Every real external signal (Cerebral call, chess X-post hook, NorSwap DMs) attaches to the WASM+ocap substrate, not the language.
- **Prior decision:** Louis already removed Glia from the product path on **2026-07-23** ("Glia is not part of the public product path for this launch; do not present Glia as production-ready" — recorded, confidence 10), and abandoned the only Glia-centered wedge demo on **2026-05-22** as "structurally illusory."
- **Independence of the security thesis:** the enforcement substrate (membrane, InitialAuthorityRecord, Terminal auth, wire protocol) and the confinement proof (24/26 tests, real WASM adversarial probe) are entirely Glia-free.
- **Priced cost of continuing:** the memory-model arc (2026-08-01→04) ran 7 adversarial Sol reviews in 4 days; **both** candidate memory designs were rejected (Graph 4 repair repriced at 900–1,300 prod + 700–1,000 test lines with 13+ coupled invariants; the 565-LOC cycle collector failed 13/30 audit items with a Miri-proven safe use-after-free and Kani-proven count overflow). Two live, non-reclassifiable leaks sit in the uncommitted tree. PR-2/3/4, the durable-value plane, and process heaps are unbuilt.

**Honest framings the decision carries:**

- The recent technical difficulty is *context, not cause* — but removal is also a **commitment device**, stated plainly: Glia-in-tree re-captured focus twice after the deprioritization decision (the 4-day memory-model arc; catalog+lint built then reverted within 48h, #630→#631).
- **Archiving Glia does not validate WASM-first** (Codex's headline point, accepted). WASM-first is a hypothesis with one discovery call behind it and a follow-up that never happened. The point of clearing the deck is to *run the validation*, not to build the substrate out further. Kill criteria in §15 make it falsifiable; demand gates in §16 make it enforced.
- Where this is evidence vs judgment: the demand audit, coupling map, and cost pricing are evidence; the call that a solo founder's next 90 days are better spent on discovery + the authority wedge than on completing the language is judgment.

---

## 2. Evidence: product thesis and discovery record

### 2.1 The canonical thesis (committed, `doc/positioning.md`)

> "Wetware is the runtime for software that runs code its operator didn't write and can't audit."

JTBD: *when building an agent mixing tools, untrusted inputs, and unaudited code, the runtime enforces isolation per-call so audit becomes the wrong tool for the job.* The doc's own admission: "We're in the first 100 conversations." Differentiators deep enough to win deals (its own ranking): explicit capability handoff, composable membranes — both Glia-free.

### 2.2 The discovery record (everything that exists)

| Signal | What it is | Verdict on file |
|---|---|---|
| **Cerebral Systems / Warrant (Prit), call 2026-07-17** | The only real user conversation in the corpus | "STRONG FOLLOW-UP SIGNAL, NOT YET VALIDATED DEMAND." Origin of Warrant = sales pressure, not incident ("thankfully not!" re near-misses). "It's not necessarily running untrusted code" — recorded as weakening the core wedge. Follow-up scheduled "~two weeks"; **never happened** (18+ days stalled as of review date). |
| **The attention hook** | X post: chess over Amino DHT — "content-addressed WASM… object capability invocations… No server. No SPoF." | The proven hook. **No Glia in it** (though the demo *driver* is Glia-shell-forward — see §17). |
| **NorSwap DMs (2026-07-25/27)** | Second-order signal | Converted to thesis: the wedge begins where agents reach durable external systems/credentials — not inside the disposable sandbox. |
| Dutch AI Agents, Jeet (VC), Jonathan Colton, YC target list | Collateral / polite-no / peer challenge / never contacted | Explicitly "not customer evidence" in the corpus's own words. |
| **Revenue/adoption** | — | "No one is using it, paying for it, or building workflows on it" (2026-04-09). Nothing later contradicts this. |

The open discovery task — "map one real design-partner action to the exact authority an executor receives before and after policy approval" — recurs unclosed across three planning cycles; the Cerebral authority-map table shipped **blank**.

### 2.3 Glia demand audit — every stated justification, classified

| Thesis / feature | Evidence class | Basis |
|---|---|---|
| Lisp syntax | FOUNDER PREFERENCE | Premise 6 (2026-04-03): "Lisp is for thinking" — asserted, never user-tested |
| Dynamic embedded programming | SPECULATIVE | Production usage = a 14-line linear boot script; no shipped script uses loops/conditionals |
| AI-oriented language design | PLAUSIBLE HYPOTHESIS, **unmeasured** | No benchmark/experiment/user anywhere; the load-bearing use (D6 no-escape-hatch justification) was called "vapor" by the project's own DX review; supporting catalog+lint reverted in 48h |
| Effects; exception/effect unification | TECHNICAL ELEGANCE | "First language where ocap + effects + pipelining are the same mechanism" — novelty claim, not demand |
| Runtime macros | FOUNDER PREFERENCE | No user artifact |
| Module/import semantics (B3) | TECHNICAL ELEGANCE | Good internal work; no user in the loop |
| Persistent collections; code-as-data; portable callables; proof-friendly code | SPECULATIVE | value-contract.md cites no user, no wedge in 16.5 KB; portable callables gated on parked macro-staging fix + unbuilt durable plane |
| IPFS-backed durable data | PLAUSIBLE at substrate level | Real for WASM images/CAS today; the Glia *value-plane* version is speculative |
| Orchestration through Glia | CONTRADICTED | Own notes: "control logic should not be assumed to live only in Glia"; adoption research: "**Do not require a new language**" |
| Structural authority via Glia | CONTRADICTED | Glia was the *leak* (`collect_caps` ambient capture; `isolate` removed as unsound 2026-07-18); enforcement deliberately moved to membrane/record |
| Human ergonomics | FOUNDER PREFERENCE | Constituency = 5 named people, zero recorded feedback. The one real signal (Jinsuk, 2026-04) was for an *interaction surface*, not a Lisp |

**No artifact anywhere shows users needing Glia rather than compiled-to-WASM languages, WIT, SDKs, config, or host-side composition.**

---

## 3. Technical findings

### 3.1 The memory-model arc (2026-08-01 → 08-04)

- **Scope growth:** PR-1 "control extraction" (designed as "mechanical") → three-way exception/fault/control model → B3 imports → definition ownership → Graph 4 resting-weak/escaping-strong → cycle collection. Estimate trajectory: +350/−150 → +550/−250 → +900/−350 ("roughly double") → +1,410/−500 → repair delta +250/−80 **withdrawn by Sol** → repriced at **900–1,300 prod + 700–1,000 test lines, 13+ coupled invariants** ("tracer-shaped complexity").
- **7 Sol adversarial reviews in 4 days; 5 self-imposed pauses.** Net: Stage-B semantics validated; Stage-C mechanism REJECTED; the repair REJECTED; the collector REJECTED.
- **Collector spike:** 565 LOC, Miri/Kani/fuzz/mutation/WASM harness. The *algebra* passed (scan_black, omission-conservatism, exactness, all 6 graph classes machine-checked). The *implementation* failed: P0 safe use-after-free (Miri-proven), P0 count overflow/underflow (Kani-proven), P1 panic-poisoning/re-entry; 13 FAIL / 7 UNPROVEN / 10 PASS on the 30-item audit; 3 mutants survived the suite.
- **Two live, non-reclassifiable leaks** (cross-owner factory; body-hidden) in the uncommitted tree. Sol Review 2 §20: these cannot be filed under the accepted atom/host leak classes.
- **Precedent study (19 systems):** no inspected system flips reference strength dynamically; RustPython leaked module-graph cycles for ~7 years then capitulated to a full backup collector; CPython's cycle correctness is a "whole-system proof" with 20 years of scar tissue at exactly Glia's boundaries; the one working Rust arena (gc-arena) requires a whole-crate rewrite incompatible with the async evaluator.
- The chosen mechanism was formally classified **"consciously transitional"** — planned to be thrown away even under continuation.

### 3.2 Coupling map (what Glia actually touches)

- **Size:** `crates/glia` = 18,064 LOC Rust (~23% of the repo's 79,671); **~880 of ~1,590 tests (≈55%)** are Glia-attributable; compiled in the host workspace + two wasm32 guest builds + four release-matrix builds.
- **Dependents:** `ww` binary, `std/kernel` (pid0, 5,243 LOC — the real replacement scope), `std/caps`, `std/shell` (already semi-vestigial), `src/cli/shell.rs` (~800 of 1,624 prod lines), MCP (all 4 tools Glia-shaped), `ww init` scaffolding, ~10 of 46 TODOs (incl. defcap-export bridge L/P1).
- **Glia-free and untouched:** `crates/membrane` (hook-level attenuation — the actual enforcement engine), `crates/rpc` (InitialAuthorityRecord, grants bootstrap), `crates/cell` (wasmtime), `crates/authority` (Terminal challenge-response), `crates/ipfs`/`cache`/`stem`/`atom`, libp2p host, the entire capnp wire protocol, 24/26 confinement tests (real WASM adversarial probe), the release/IPFS pipeline.
- **Production-critical Glia surface:** ~61 lines of boot scripts (14-line `05-status.glia` registers the README's headline `/status` demo) — **but** those lines sit atop the 5,243-line Glia pid0 that owns reverse graft, bootstrap publication, route readiness, epoch staleness, re-graft, and shutdown. Replacement = a Rust pid0 (~600–1,200 LOC) plus lifecycle parity, not a 61-line port (Codex correction, accepted).
- **Key structural fact:** grant maps and `attenuate` are Glia *front-ends to Glia-free machinery*. The canonical architecture doc states the security claim entirely in capnp/host terms.

---

## 4. Thesis with and without Glia

**Without Glia:** *Wetware is the runtime that turns an approved action into the only thing an executor can do.* Policy engines decide "may this happen"; Wetware makes everything else unreachable — for code the operator didn't write, across processes, machines, and trust domains. Users need it (per the only real evidence) where approved authority must be handed to generated/untrusted executor code, span multi-step tasks, cross trust domains, be delegated/attenuated, or carry independent revocation. Wetware provides capability issuance from authorization context, membrane attenuation, immutable initial-authority records, revocation/epochs, denial receipts. Existing toolchains provide the languages, WASI, wasmtime, packaging. **Yes — raw WASM + the capability/networking/content/authority/execution substrate forms a coherent product without Glia.**

**With Glia:** Glia uniquely adds (a) the only *interactive composition surface* (the live REPL where the capability model "clicks"), (b) dynamic boot composition, (c) the long-horizon vision (mobile code, "security model = programming model"). Classification: (a) useful-but-optional — its replacement (denial-receipt demo channel + scripted drivers) must be first-class scope; (b) speculative — nothing shipped needs more than a static list; (c) distracting — it is the scope that produced the tarpit.

---

## 5. Dual-interface cost (measured)

Two execution models, module systems, value models, error taxonomies, debugging stories, doc paths, and security-review surfaces (the `isolate` hole and `collect_caps` leak were both Glia-side security work). 23% of the codebase; 55% of tests; ~1–3 min of the 11.5-min CI test job plus compile time across seven other jobs; ~22% of open TODOs; an MCP surface entirely Glia-shaped; and — demonstrated, not hypothetical — a 4-day/7-review/2-rejected-designs maintenance cost for the second runtime's semantics *when done rigorously*.

---

## 6. Opportunity-cost comparison

| Horizon | Path A: Glia first-class | Path B: archive, WASM-first |
|---|---|---|
| 4 weeks | Resolve Stage-C cure (both options already rejected as-is); hardening; zero external evidence | Archive committed; Cerebral loop reopened; approval→keyring demo running; Rust pid0 at /status parity |
| 3 months | PR-1b/2/3 partial; collector question open (redesign must re-pass a 30-item audit); discovery still stalled | Glia removed, main green; 2–3 more discovery conversations; wedge resonating **or falsified with data** |
| 6 months | Plausibly a leak-accepted Glia 1.0 (≥1,600–2,300 more lines or a collector rewrite) — before PR-2/3/4, durable plane, process heaps | Either a design-partner integration on membrane+record+keyring, or a documented kill and a better-informed next thesis |

**Path B produces the strongest real-world evidence at every horizon because Path A produces none at any horizon — by construction.** That asymmetry, not the LOC, is the decision.

---

## 7. Best WASM-first product shape

- **Abstraction:** an executor process that can only do what its approval says. Inputs: WASM image (any language), authorization artifact, grant set. Outputs: effects through granted caps; **integrity-protected denial receipts** for everything else. Receipts are the product's voice and replace the REPL as the "watch the model click" channel — first-class scope, not an afterthought.
- **Exists today, Glia-free:** `Runtime.load → Executor.spawn(args, env, grants) → Process`; CID module identity; InitialAuthorityRecord; recursive membrane; Terminal node boundary; epochs; `std/system` + `wagi-guest` SDKs.
- **New piece:** an AuthorityIssuer compiling an approval artifact into a session keyring (the Warrant seam).
- **`approval.json` is a privilege-escalation surface** (Codex, accepted): issuer, signature, subject, action, resource binding, executor digest, expiry, and replay semantics are day-one requirements, not hardening.
- **Networking:** keep libp2p/DHT/Terminal, but *off the demo's critical path*. The wedge demo is local-first; remote handoff is proof of boundary portability, shown second. Building networking features "for the demo" is the named substrate-distraction failure mode.
- **The narrow sellable object** (sharpened by Codex, accepted): *a policy-gated action runner that converts one authenticated approval into a one-action capability set, runs one WASM executor, and emits an integrity-protected execution-or-denial receipt.* One buyer, one sensitive action, one policy provider, one connector. "Run WASM with capabilities" alone is ecosystem baseline (wasmCloud, WIT worlds) — the seam is the differentiator.
- **Smallest compelling demo:** one executor, two approvals, two observably different authority envelopes, denials as receipts, <2 minutes. Then chess re-recorded with a non-shell driver as the distributed proof.
- **AI workflow:** AI writes ordinary Rust/AssemblyScript against typed capability interfaces; no bespoke language for agents to learn; MCP resurfaces later as typed per-capability tools, not eval.

---

## 8. Extraction ledger

| Item | Disposition |
|---|---|
| Three-domain value model (local/durable/external); durable content handles; CID-handles-as-opaque-leaves; content-vs-authority separation; module CIDs; shared frozen foundations | **EXTRACT NOW — as documented design rules** (`doc/value-domains.md` + architecture sections). Improves CAS jurisdiction, image identity, serialization boundaries. No Glia dependency. Small effort. On the critical path (the retro cites it). |
| Authority-travels-as-requirements-never-serialized; effect-driven authority rebinding (principle) | **EXTRACT NOW (rule)** — the rule for shipping any portable unit incl. WASM module + grants manifest. Mechanism (handler stack) stays archived. |
| Capability identity (CapId, sealed internals) | **EXTRACT NOW** — already a substrate property consumed by rpc/membrane; zero extra scope. |
| Structural-ownership design rule ("represent ownership structurally, not via bookkeeping") | **EXTRACT NOW** — one paragraph, repo-level principle. |
| Kani/Miri/mutation/fuzz audit discipline | **EXTRACT NOW (process)** — adopt for membrane/rpc hardening. |
| Module/import semantics (per-import instantiation, source-only cache, chain cycle detection); importer-granted authority; B3 reasoning | PRESERVE AS DESIGN RESEARCH — thin general core maps to WASM module policy. |
| Exception/fault/control distinctions; abortive vs resumptive handlers; throw-as-effect | ARCHIVE AS GLIA-SPECIFIC (one general residue: embedder-visible boundary error types — note in retro). |
| Graph 4; reference counting; cycle collection; GC/process-heap research | ARCHIVE — with the cc-spike audit corpus preserved as reusable engineering IP. BEAM per-process-heap ↔ per-cell memory insight goes in the retro. |
| Persistent collections; deterministic equality/hash; IPFS/Bitswap value-plane; portable callable specs; formal verification of language semantics | PRESERVE AS DESIGN RESEARCH / REQUIRES MORE EVIDENCE — revival-criteria material. |
| defcap / sealed Glia caps; module-instance export projections | DISCARD (defcap cannot cross boundaries; the P1 bridge TODO dies with it). |

---

## 9. Repository disposition — comparison and choice

1. **Continue unchanged** — fails: no demand; unbounded priced road.
2. **Pause in main** — fails the stated standard (repo structure should communicate status); dormant-in-main demonstrably re-captured focus twice; 55% of CI stays tied to a paused component.
3. **Feature-flag** — worst of both: all the maintenance, none of the clarity.
4. **Archive without extraction** — breaks the shipped `/status` demo and deletes the demo driver with no replacement.
5. **Extract, archive, remove** — ✅ **CHOSEN**, with §12 sequencing.
6. **Delete** — destroys validated research for zero additional focus benefit.

Reversibility of (5) is high *iff* the archive README is written while context is fresh — reversibility decays with documentation, not with git.

---

## 10. Archival branch spec — `archive/glia-2026-08`

**Verified constraint:** `.context/` is **4.7 GB / 60,108 files**, almost entirely `spike/*/target/` build outputs, with **two nested git repos**. The curated corpus is ~750 KB. **Allowlist, never wholesale.**

1. **Commit 1 — the dirty tree, as-is.** All 11 modified files in one WIP commit. Do not polish; do not fix the two leaks; freeze them. (**Never stash this tree** — it already survived one near-loss.) Banner the commit message and branch README: `REJECTED / UNSAFE / TWO KNOWN LEAKS`.
2. **Commit 2 — curated research corpus** under `doc/archive/glia/`: the 21 `.context` design docs (~404 KB), `preflight-studies/batch1..9` (~336 KB), spike **source only** (no `target/`; nested repos flattened or `git bundle`d), Sol verdict attachments that are design content, `doc/designs/value-contract.md` state.
3. **Branch README/index:** what this is; why archived (demand, not failure); tree state (leaks live, Stage C uncured, Stage D frozen); document map; unresolved-decision list; revival instructions ("start from the retrospective, not from the tree").

**Excluded:** `target/`, generated fixtures, raw `.context/attachments/` (screenshots/pasted transcripts; machine-specific paths, third-party content), anything matching a secrets grep. **Summarize rather than preserve raw:** the substrate rules (→ main docs) and the estimate-trajectory history (→ retro).

---

## 11. Main-branch removal plan — final sequence (Codex-amended)

> Ordering principle: **build the replacement beside the old system; gate deletion on a production canary; delete consumers first, `crates/glia` last.**

1. **Extract** `std/kernel/src/attenuate.rs`'s schema/method-resolution (~310 lines) into a substrate crate.
2. **Rust pid0 beside Glia** — hardcoded `/status` registration (no manifest language until a second consumer exists); dual-path behind a rollback flag.
3. **Port lifecycle/boot-parity tests** *before* any test deletion: reverse graft, bootstrap publication, route readiness, epoch staleness, re-graft, listener replacement, shutdown, init-failure handling (the real contents of `std/kernel/src/lib.rs:1735–2460`). This prevents "green but meaningless" after removing 55% of tests. Audit e2e paths whose only coverage is Glia-driven; port them.
4. **Canary a release.** **Do not delete the Glia pid0 until one real deployment has restarted through an epoch change on the Rust pid0.**
5. **Re-drive chess** with a non-shell driver — as a migration regression test (and re-record the demo).
6. **Retire `ww shell` REPL + MCP** with explicit errors and release notes (MCP declared dead for now; typed per-capability tools are the revival path). Migrate `ww init`/`ww setup` scaffolding, embedded assets, installer, release packaging, daemon readiness.
7. **Delete consumers first, `crates/glia` last:** std/kernel glue, `std/caps`, `std/shell` (already semi-vestigial), shell.rs Glia glue (~800 lines), `tests/shell_e2e.rs`, 2/26 confinement tests, both Glia benches, 30 `.glia` files, `examples/*/glia/`, `examples/grants/`. Move `GIT_COMMIT` embedding from `crates/glia/build.rs` to root `build.rs`.
8. **Docs + CI:** README (2 code blocks, 1 feature bullet, both roadmap items), delete/rewrite 4 Glia docs, re-spell `doc/capabilities.md` error schema + "MCP = Glia eval", drop `check-glia-effects`, fix stale `Containerfile:98` + dead CI copy paths, prune ~10 TODOs.
9. **Retrospective (timeboxed: 2 hours)** + extracted design rules land on main.

Wire compatibility: nothing changes — Glia was never in the protocol.

---

## 12. Retrospective outline (2-hour timebox)

Origin & lineage (Circuit/CASM; "Python of distributed systems") → hypotheses with provenance (founder-driven; 5-person audience; Jinsuk's ergonomics signal) → language thesis → AI-coder thesis (*untested, not falsified*; the D6 use; the 48h revert) → architectural role and its migration (Glia-native caps → single-authority membrane; isolate removal; collect_caps → constructive grants) → implementation history with the estimate-trajectory table → discoveries (broken named recursion; validated Defs/late-binding semantics; three-domain model; the precedent-free strength-flip; the irreducible cross-owner SCC; collector algebra-pass/implementation-fail; 19-system study) → capability findings (defcap boundary gap; grant map = front-end) → evidence gaps → opportunity cost (the 4-day arc vs the 18-day-stalled discovery loop) → reasons for archival (demand + focus, **not** "the work failed") → what not to repeat (foundational rewrites on an uncommitted tree; language scope before demand; "someday" as a scope driver) → extracted value → unresolved promising ideas → revival criteria. Tone: a research program that produced real findings and no product evidence — both true.

---

## 13. Revival scorecard (Glia)

| Signal | Minimum evidence | Source | Threshold | Prerequisites | Action |
|---|---|---|---|---|---|
| Users ask for runtime scripting | ≥3 independent, unprompted requests | Discovery notes / issues | 3 in any 6-mo window | Boot/config layer stable | Design pass on **restricted Glia** (session-scoped, acyclic) |
| Partner blocked by compile/deploy cycle | Named partner, measured time/task data | Design-partner engagement | 1 concrete workflow | Wedge live with partner | Prototype REPL-over-existing-caps; **no new memory model** |
| AI agents outperform with Glia | Controlled benchmark vs Rust→WASM, same tasks | A benchmark actually run | Statistically credible win | MCP rebuilt either way | Revive AI-authoring thesis with data |
| Code-as-data load-bearing | A feature that cannot ship without runtime-constructed code | Roadmap + committed user | 1 | Durable-plane design | Revive portable-callable track |
| WASM/WIT expressiveness failure | Documented attempt + written post-mortem | Engineering artifact | 1 | — | Language design scoped to the gap |
| Funded validation | Partner funds language work explicitly | Contract | any $ | — | Full revival under partner scope |

**Revival targets the restricted profile only** — session-scoped, cycle-free wiring/REPL DSL. Not the archived tree's full ambitions. "Glia still seems promising" is not a trigger. The experiment is preserved, **not funded** until a live buyer proves hardcoded Rust is the bottleneck (three-model consensus).

---

## 14. WASM-first kill criteria

Falsified if, with the wedge demo built and shown:

1. ≥8 qualified discovery conversations in 90 days → zero second calls or design-partner motion.
2. The seam is dry: Cerebral-class buyers confirm every approved action collapses into one trusted actuator call and no customer/generated code executes post-approval.
3. Onboarding fails structurally: competent target users can't run an executor + grants in <1 day after two doc iterations.
4. Users consistently ask for runtime scripting (simultaneously a Glia revival signal).
5. Every reference executor needs bespoke Rust host glue (the "runtime" is actually a consultancy).
6. AI codegen can't produce workable modules against the typed capability SDK.
7. **Substrate-distraction tripwire:** building libp2p/DHT/IPNS features *for the wedge demo* = the failure mode is active.

---

## 15. Sequencing note — what stops immediately

All Glia implementation and design work: the Stage-C cure decision (**closed as "archived uncured"**, documented in the branch README), PR-1b/2/3/4, collector follow-ups, Boa-tracer comparison, defcap bridge, Snap v2, `dosync` roadmap item. Roadmap/docs/demos/feature work on Glia: stopped now; physical deletion waits for the §11 canary gate.

---

## 16. Next 30 days (final, gated)

- **Days 1–2:** freeze + curate the archive (§10). Send **nothing new** into Glia.
- **Day 3 (not after the funeral):** Cerebral re-engagement email — 18+ days stalled, nothing in the repo blocks it — plus outreach to **15 new prospects** (YC target table actually contacted this time; NorSwap credential-boundary angle).
- **Days 3–7:** dual-path Rust pid0, boot parity, rollback flag, production canary.
- **Build gate (hard):** do **not** start the action runner until at least **one real action + approval artifact** from a live prospect is in hand. No synthetic-approval demos in a vacuum.
- **Days 11–20:** if gated-in — build only that action runner: authenticated approval → one-action keyring → membrane-enforced execution → integrity-protected receipts. Local-first; no new networking.
- **Days 21–30:** run it live with the prospect; **ask for money**.
- **Continue/stop gate (hard): two committed pilots by day 30, or stop building** — §14 fires. (Codex proposed day 10; day 30 is chosen because a gate designed to be missed gets waived, which is worse. Louis calibrates — see §18.6.)
- **Measure:** integration time, permitted-action success, unrelated-action denial, receipt usefulness, repeat use, willingness to pay.
- **Explicit non-goals:** manifest/config language; WIT tooling beyond demo needs; any libp2p feature work; MCP rebuild; Glia in any form.
- Retro (2h) + revival scorecard land whenever the canary gate clears — off the critical path.

---

## 17. Risks, steelman, and the cross-model record

**The steelman (presented at full strength, then answered):** Glia is the system's only interactive composition surface, and every demo ever shown ran through it — "demos are shell-forward by default" is written into the chess example. Capability systems historically sell through live sessions (E's REPL, Goblins): someone attenuates a cap, watches the denial, and the model clicks. Deleting the REPL at the exact moment the founder must be *showing* the system removes the show-don't-tell channel. The AI thesis arguably favors one composable eval surface over N rigid tools, and the #630 revert was a 48h execution failure, not a falsification. The memory catastrophe argues for a *smaller* Glia, not zero — boot scripts and REPL sessions never needed cycle collection; a restricted acyclic session DSL dodges the entire priced road. **Answer:** revealed preference — the small-frozen-Glia option was available for months and the record shows escalation out of it every time, including *after* the confidence-10 deprioritization. The steelman describes a discipline the record shows is unavailable while the code is in-tree. Its core is honored structurally: the denial-receipt channel and scripted demo drivers are first-class scope (§7, §11.5), and the restricted profile is the sole revival shape (§13).

**Other named risks:** demo-regression window (closed by §11 ordering); WASM-first itself unvalidated — do not launder the Glia decision through Cerebral (§1, §14); component-sale market risk for a solo founder in a crowded 2026 agent-security space (the 30-day discovery volume is the test); std/kernel deletion must not orphan load-bearing logic (§11.1); archive hygiene (§10 exclusions); `approval.json` escalation surface (§7).

**Cross-model record (3 voices: this review, Claude outside-voice, Codex manual run):**

- **Unanimous:** archive Glia; WASM-first direction; no manifest language until a second consumer; restricted-profile-only revival, unfunded until buyer evidence; discovery is the bottleneck, not construction.
- **Adopted from Claude outside-voice:** chess demo driver is Glia-shell-forward (sequencing consequence); hardcode-don't-manifest; MCP must be explicitly decided, not silently dropped; removal-as-commitment-device honesty; Cerebral email is day-1 class.
- **Adopted from Codex:** allowlist archive (verified: 4.7 GB / 60,108 files / 2 nested repos vs ~750 KB curated); dual-path pid0 + rollback + **epoch-restart canary gate**; boot-parity tests before test deletion; `approval.json` threat model; 2h retro; demand gates; "narrow sellable object" phrasing; wasmCloud/WIT baseline framing.
- **Calibration retained against Codex:** archive-first ordering (minutes of work protecting the only copy of the research arc); pilot gate at day 30 rather than day 10 (honest gates > heroic gates).

---

## 18. Decisions for Louis (ratification list)

1. **Ratify disposition 5** with the §11 dual-path/canary sequencing. *Recommended: yes.*
2. **Archive mechanics:** WIP commit of the dirty tree + ~750 KB allowlisted corpus, `REJECTED/UNSAFE/TWO KNOWN LEAKS` banner. *Recommended: yes.* (One-way once main deletion lands.)
3. **Stage-C cure:** close as "archived uncured" — deciding it would be Glia work. *Recommended: yes.*
4. **Boot replacement:** hardcoded Rust pid0, dual-path behind rollback flag; manifest deferred to a second consumer. *Recommended: yes (three-model consensus).*
5. **MCP:** retire with explicit errors + release notes at the shell-retirement step; typed per-capability tools noted as the revival path. *Recommended: yes.*
6. **Demand-gate calibration:** build gate = one real approval artifact in hand; continue/stop gate = two committed pilots by **day 30** (Codex: day 10). *Recommended: day 30, enforced honestly.*
7. **Canary gate:** no old-pid0 deletion until one production deployment restarts through an epoch change on the Rust pid0. *Recommended: yes.*

---

## 19. One-page CEO memo (for gbrain)

> **Decision: Archive Glia; Wetware is WASM-first.** (2026-08-04)
>
> **What we decided.** Glia — the embedded Lisp, 18k LOC, 23% of the repo, 55% of tests — moves to `archive/glia-2026-08` (dirty tree frozen with two known leaks + ~750 KB curated research corpus) and leaves main behind a dual-path migration: Rust pid0 beside the Glia pid0, boot-parity tests, rollback flag, and no deletion until a production deployment restarts through an epoch change. Wetware's product is the runtime authority layer: policy engines decide *may this happen*; Wetware makes everything else unreachable for executors — including generated and untrusted code — locally and across trust domains. Enforcement (membrane, initial-authority record, Terminal boundary, wire protocol) never depended on Glia.
>
> **Why.** No user-demand artifact for Glia exists after ~5 months: its constituency was five named people with no recorded feedback; its only wedge demo was abandoned as "structurally illusory" (2026-05-22); it was removed from the product path on 2026-07-23. The real external signals — Cerebral/Warrant and the chess hook — attach entirely to WASM + ocap. The deciding asymmetry: six months of finishing Glia produces zero buyer-facing evidence by construction; the WASM-first path produces evidence at every checkpoint. The memory-model arc (7 adversarial reviews in 4 days; both candidate designs rejected; ≥1,600–2,300 lines or a collector rewrite remaining) priced the road honestly — context, not cause. Removal is also a commitment device, and we say so plainly: dormant-in-main re-captured focus twice after deprioritization.
>
> **What archiving does NOT mean.** It does not validate WASM-first, which is a hypothesis with one discovery call behind it. The deck is cleared to *run the validation*: Cerebral re-engaged + 15 prospects by day 3; no action-runner construction until a real approval artifact is in hand; two committed pilots by day 30 or the wedge is declared unvalidated and the kill criteria fire. The narrow sellable object: *a policy-gated action runner converting one authenticated approval into a one-action capability set, running one WASM executor, and emitting an integrity-protected execution-or-denial receipt.*
>
> **What survives.** The archive branch; extracted design rules on main (three-domain value model; authority travels as requirements, never serialized; CID handles are opaque leaves; ownership is structural); the Kani/Miri/mutation audit discipline; Stage-B's validated semantics — frozen for the only permissible revival shape: a restricted, session-scoped, acyclic wiring DSL, unfunded until a live buyer proves hardcoded Rust is the bottleneck.
>
> **Falsifiers.** Glia revives on evidence only (repeated unprompted scripting requests; a partner measurably blocked by compile cycles; a measured AI-authoring win). WASM-first dies on evidence too (90 days of discovery without a second call; confirmation that approved actions never leave one trusted actuator call). We are in the first 100 conversations; the point of this decision is to go have them.

---

*Prepared by /plan-ceo-review, 2026-08-04. Evidence: 3-agent sweep (memory-model artifacts; coupling map; product/discovery corpus), gbrain retrieval, 2 outside-voice challenges. Logged to gstack (review, decision, learning). Nothing edited, committed, branched, or sent externally.*
