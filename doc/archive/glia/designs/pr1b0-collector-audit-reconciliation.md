# Collector-audit reconciliation — Sol's cc-spike REJECT vs the approved PR-1b.0 direction

Status: DESIGN ONLY. No production edits, no spike edits (Sol's audit artifacts preserved as-is: `tests/audit_adversarial.rs`, `tests/audit_differential.rs`, `tests/audit_wasm.rs`, `audit/kani_model.rs`, `audit/prepare_mutations.sh`). Nothing committed. Production tree verified unchanged (11 files, HEAD f1365b6 = Sol's frozen identity).

## 1. Purpose and status of the collector spike

The spike was an **approved, gated experiment** — authorized explicitly as "approved for a spike" with a decision gate ("The spike must determine whether … is a safe and practical stage-one memory layer"), testing the **strongest alternative** to Graph 4: the "Rc + cycle collection" option the memory-model study had originally rejected as unretrofittable, revived from scratch (new pointer type, not a retrofit) after the cross-owner findings sharpened its motivation. It was **never an authorized production replacement**: every migration step was gated behind approvals that were never given, and zero production code was changed in reliance on it (verified continuously and confirmed independently by Sol's audit: "Production worktree was already dirty in eleven files; none were modified"). Sol's REJECT therefore rejects a spike implementation, not the production plan of record.

## 2. Finding-by-finding classification

| Sol finding | Class | Verified | Consequence |
|---|---|---|---|
| P0 safe UAF: phase-4 destruction lets a destructor deref an already-dropped white neighbor through safe `Cc::deref` (Miri-proven) | COLLECTOR-SPECIFIC + FUTURE-GC-RELEVANT | Accepted — my phase-4 design (freed-all-first + guard-alive handles) conflates Condemned/Dropping/Dropped | No Graph 4 analogue: Graph 4 has no destruction choreography — values die through std `Rc`, where safe code cannot reach a dropped value. Future collector: 3-state discipline mandatory. |
| P0 strong-count overflow; P0/P1 trial underflow (unchecked arithmetic; Kani counterexamples) | COLLECTOR-SPECIFIC + GENERAL LESSON | Accepted | Graph 4 does no hand-rolled count arithmetic (std `Rc` aborts on overflow). Standing rule adopted: no unchecked count arithmetic in runtime-internal code; `Defs.version: u64` wrap noted as theoretical. |
| P1 destructor-panic pump poisoning; P1 Trace-panic root loss; P1 collector re-entry from Drop | COLLECTOR-SPECIFIC + GENERAL LESSON | Accepted (incl. the fuzz one-byte artifact) | No pump/roots/collect exists in Graph 4. General lesson carried: runtime-internal machinery needs an explicit panic policy; Graph 4's transforms are transactional by construction (out-of-place rebuild, define-after-transform) — to be STATED and TESTED, not assumed. |
| P1 `meta<'a>` unconstrained lifetime | COLLECTOR-SPECIFIC | Accepted — my shortcut | Production Graph 4 contains no unsafe code at all; lesson recorded for any future unsafe module. |
| Mutation-testing gaps (mutants 5, 11, 15 survived ordinary suites) | COLLECTOR-SPECIFIC + GENERAL LESSON | Accepted | Adopt mutation testing as a review gate for memory-mechanism code, including Graph 4's `own` module tests. |
| Debug-WASM panic trap (r09 aborts wasmtime; panic=abort on wasip2) | GENERAL LESSON | Accepted | Target-wide test policy: no deliberate debug panics on wasm paths (production has none today; keep it that way, documented). |
| Host boundary: opaque hosts may hold counted handles, NEVER raw/uncounted participating pointers | GENERAL MEMORY-MODEL LESSON | Accepted | True under Rc/Graph 4 as well — add the no-raw-participating-pointer clause to the host trust-boundary documentation now. |
| Performance methodology critique (no warmup/black_box/pinning/tails) | GENERAL LESSON | Accepted | Applies to Graph 4's Stage-G benches: adopt Sol's harness discipline (warmups, black_box, multi-sample, medians+tails). |
| Kani/differential/fuzz PASSES: scan_black, omission-conservatism arithmetic, SCC selection, exactness, idempotence; all six graph classes; identity/map-keys; deep iterative behavior | FUTURE-GC-RELEVANT (positive) | — | The ALGEBRA of collector-aware RC is now machine-checked; what failed is destruction engineering, panic policy, and arithmetic hygiene. The fallback remains real in principle. |
| Trace-completeness / safepoints / production host API UNPROVEN | FUTURE-GC-RELEVANT | Accepted | These are migration prerequisites for a path not being taken now; the §16 inventory is preserved as the future-collector edge list. |
| Generational-seam caveat: BEAM does not justify barrier-free mutation for this shared mutable graph | FUTURE-GC-RELEVANT | Accepted | Fold into `process-memory.md` skeleton: atoms, `Defs.bindings`, cap inners, handler/oneshot slots, import cache = the write-barrier site list. |
| IRRELEVANT TO CURRENT PR-1B.0 | — | — | Nothing in the verdict touches `Defs`, `CapturedEnv`, late binding, the top-level gate, the prelude lifecycle, exports, or the three-domain model — Sol's own §19 lists the executable/durable split and the type seams as what SURVIVES. |

## 3. P0/P1 impact on the seven Graph-4 dimensions

Graph 4 implementation contract: UNCHANGED (no finding maps onto its mechanics). Five choke points: UNCHANGED. Three-domain model: UNCHANGED (explicitly listed by Sol as surviving). Host-boundary rules: STRENGTHENED (no-raw-pointer clause). Drop/revocation semantics: REAFFIRMED (Drop release-only; explicit close/revoke — Sol's residual list restates it). Future process-heap direction: UNCHANGED, with the write-barrier caveat recorded. PR-1b.0 sequencing: one real change — **the collector is not a near-term fallback until redesigned and fully re-audited**, which restores the original sequencing (Graph 4 stage-one) while lengthening the fallback lead time.

## 4. Reconciliation with the prior stage-one rule

The rule — Graph 4 stage-one; cycle collection a later fallback if the accepted leak class or performance becomes problematic — emerges CONFIRMED IN SHAPE, AMENDED IN DETAIL:
(a) the spike proved the fallback is real in principle (every Graph-4-defeating class collects; the algebra is Kani/fuzz/differential-validated);
(b) it also proved the engineering bar is high exactly where Sol predicted ("unsafe destructor choreography" was the pre-declared Boa-tracer trigger — that trigger has now FIRED);
(c) the fallback path is therefore "redesigned collector OR Boa-style narrow tracer, then the full 30-item audit again" — longer lead time, so Graph 4's leak-class diagnostics matter more (earlier warning needed);
(d) the honest open item this reconciliation cannot close: **Stage C's Sol-R2 REJECT remains uncured.** The two routine self-cycle leaks (foreign-factory, body-hidden) are still in the tree. Continuing Graph 4 requires re-deciding the cure — that decision is presented in §7, not made here.

## 5. Lessons carried into Graph 4 now (none import collector architecture)

1. Transactional unwind-safety of the transforms: state it in the design doc and pin it with panic-injection tests (a panicking user `Drop` inside a value during define/lookup leaves `Defs` unchanged and consistent).
2. Host trust boundary gains the explicit clause: opaque host payloads may hold ordinary strong `Val`s/handles; never raw or uncounted pointers to runtime-managed allocations.
3. Standing rule: no unchecked count arithmetic in runtime-internal code (Graph 4 currently has none; the rule guards future edits).
4. Drop = release-only reaffirmed with Sol's destructor-observation hazard as written rationale; no destructor may run guest code or observe runtime-internal state.
5. Mutation testing as a review gate for the `own` module's test suite; Miri run over glia's ownership tests added to the Stage-G gate list.
6. WASM panic policy documented: wasip2 is effectively panic=abort; no deliberate debug panics on wasm-exercised paths.
7. Stage-G benchmark harness adopts warmup + black_box + multi-sample + median/p95 discipline.

## 6. Collector disposition

**Archive as a rejected implementation with validated algebra.** Keep `.context/spike/cc-spike/` intact (including Sol's audit artifacts — they are the regression corpus for any successor). Do not delete; do not redesign now. If the fallback trigger fires: run the Boa-style narrow-tracer comparison FIRST (per Sol §24 — the destructor-choreography trigger already fired), and any successor implementation must pass the complete 30-item matrix from scratch.

## 7. Updated risk register (delta)

TOP: Stage-C R2 cure undecided — the two routine self-cycle leaks are live in the tree; the two candidate cures are (i) the deep-CoW + identity-split repair, amended with the owner-context fix for sealed foreign weak captures (Sol's pivot review identified the defect precisely; cost re-estimated honestly at tracer-shaped complexity), or (ii) stage-one acceptance of the two routed self-cycle classes alongside atoms, with diagnostics + documented teardown idioms (`reset-tests`-style) + the fallback trigger armed. NEW: fallback lead-time is long (redesign + full re-audit); the `ww/test` registry pattern means the mutual-SCC class is present today — document the teardown idiom regardless of the cure chosen. CARRIED: trace-inventory (§16 of Sol's verdict) preserved as the future-collector edge list; write-barrier site list recorded for process-memory.

## 8. Decision

**CONTINUE GRAPH 4 WITH REQUIRED HARDENING** — the hardening being §5's seven items plus the explicit re-decision of the R2 cure (§7), which is Louis's call and the single blocking item for resuming implementation.
