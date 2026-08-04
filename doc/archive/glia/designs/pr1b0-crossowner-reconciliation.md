# Sol Review 2 reconciliation — cross-owner closure graphs and executable payloads

Status: COMPLETE (design study; no production edits, nothing committed). Evidence: production probe `.context/spike/leak-probe/` (public-API reproduction against unmodified glia); reconciliation model `.context/spike/ownership-spike/src/crossowner.rs` + `tests/crossowner.rs` (7 proofs, all green; original spike suite still 31/0 total).

## 1. Independent reproductions (production runtime, public API only)

Probe technique: an atom canary defined into each env's `Defs`; inner-`Rc` strong count > 1 after all root drops ⇔ the owner leaked.

| Case | Result |
|---|---|
| P1-B body-hidden (`(def g …) (defmacro make-f [] (list 'fn [] g)) (def f (make-f))`) | canary 3 after env drop — **Defs LEAKED** |
| P1-A cross-owner factory (B's `make`, A stores `(make g)`) | canaries A=2, B=2 after all drops — **both Defs LEAKED** |
| Control (`(def f (fn [] 1))`) | canary 1 — reclaimed (Stage C's proven simple shape) |

Spike reproduction: `p1a_shallow_leaks_both_owners`, `p1b_shallow_leaks_owner` — the reviewed shallow model leaks exactly as Sol found.

## 2. Exact graph analysis

**P1-A:** `DefsA → Binding(f) → f{owner: Strong(DefsB), captured → g{owner: Strong(DefsA)}}`. The cycle is `A → f → capture → g → A` — a **SELF-cycle of A routed through foreign structure**. B is not on the cycle; B leaks derivatively (held by the immortal f). **P1-B:** `DefsA → Binding(f) → FnArity.body → Const(g{Strong(A)})` — the same self-cycle class via executable payload.

Neither is a multi-owner cycle. Both are failures of DEPTH-COMPLETENESS: the reviewed barrier rewrites owner references only at the callable's outer position, while the positional rule ("rest references matching the storing owner; preserve foreign") is depth-agnostic in principle.

**The genuinely irreducible class** (spike `mutual_multi_owner_cycle_is_positionally_irreducible`): A stores a B-owned callable AND B stores an A-owned callable, with zero self-references at any depth. Every owner edge is foreign; weakening any of them violates F1 and escapee semantics (a foreign holder MUST keep the foreign module alive — that is the exported-value lifetime guarantee). No positional rule of any depth closes it. It requires bidirectional value flow between two simultaneously-live owners (embedder cross-injection today; handler-mediated flow during module init under future B3) — NOT the routine factory/higher-order pattern.

## 3. Core-invariant re-evaluation

1. The reviewed rule is insufficient **as implemented** (leaf-shallow); the SAME rule applied at all depths (captures + executable payloads, F1 ptr-eq scoping unchanged) closes the routine class. 2. Yes — cycles can span owners even with all direct self-edges weak (mutual class). 3. The GENERAL problem is equivalent to cycle collection over the owner graph; the ROUTINE class is not — it is depth-completeness of positional normalization. 4. Positional normalization solves the routine class WITHOUT changing observable callable identity — but only by decoupling the identity anchor from the rewritten capture. 5. Shared `Rc<CapturedEnv>` makes in-place rewriting impossible (aliases) and identity-anchored CoW self-contradictory; with a dedicated identity token, copy-on-write works and aliases are untouched (spike-proven). 6. Required: identity-anchor relocation (internal; observable semantics identical). Not required: hidden roots, owner sets, graph-wide analysis, HOF restrictions, cycle collection/tracing.

**Amended Graph 4 remains viable, revised as: deep positional normalization with copy-on-write payloads and token identity, plus two NAMED residual leak classes (atom-opaque cycles; mutual cross-owner storage).**

## 4. Candidate comparison

| Candidate | Verdict | Reasoning |
|---|---|---|
| **A. Payload traversal only** | necessary, insufficient | fixes P1-B; cannot fix P1-A (shared capture unrewritable without the identity split). Subsumed by B. |
| **B. Position-normalized copies + identity token** | **CHOSEN** | fixes both P1s (spike-proven reclamation); aliases/equality/hash/map-keys preserved (proven); captures immutable so no mutation hazard; nested graphs via recursion into callables; bounded scope (Closure {+ident, +body_bears}, transforms recurse callable captures/payloads, CoW); NO PR-4 overlap — observable identity semantics are bit-for-bit the current ones (alias equality, per-construction distinctness, hash stability); the token is an internal anchor, not a guest-visible identity feature. |
| **C. Multi-owner metadata (owner sets)** | reject | describes reachability, prevents nothing: foreign edges still cannot be weakened without breaking escapee semantics. Per-value set maintenance is ownership-by-bookkeeping — the standing structural-ownership rule's named smell. |
| **D. Anchors on ordinary values** | reject | reintroduces hidden roots (Rune's leak-by-anchor shape); rejected twice already with evidence. |
| **E. Narrow tracing / cycle collection** | fallback, unchanged | the mutual class sharpens its motivation but every Stage-2 blocker stands (unretrofittable on `std::rc::Rc` — batch7; brand/async — batch9; magnitude). New evidence does not change the PR-1b.0 tradeoff: the routine class closes without it; the mutual class is rare, constructible-only-via-bidirectional-flow, and detectable. Its trigger list gains: "mutual cross-owner storage observed in real workloads". |
| **F. Runtime/process-lifetime owners** | park | unbounded under per-import instantiation (every import pins a Defs forever). Becomes acceptable exactly when a process model provides wholesale teardown — i.e., it collapses into the studied BEAM destination. |
| **G. Language restriction** | reject | banning foreign callables in captures breaks ordinary higher-order composition (imported fns in `map`, factories) — incompatible with Lisp semantics and the whole B3 direction. |

## 5. Focused precedent recap (existing evidence, no new survey)

Rune is the only studied non-tracing owner-handle system — it ships Strong-always and **documents that cycles leak**, i.e. it accepts precisely this class. Boa (trial deletion by subtraction), CPython (trial deletion), gc-arena/Ruffle (tracing), BEAM (per-process tracing), liveslots (host JS GC): every system that closes arbitrary multi-owner cycles does it by collection. **No precedent supports arbitrary multi-owner closure cycles on pure positional RC.** This confirms both halves of the verdict: the routine-class positional closure is novel (validated by spike, as before), and the mutual class genuinely requires collection — or explicit acceptance with detection, which is what we choose for stage one.

## 6. Identity decision

REQUIRED CONSEQUENCE — not avoidable for candidate B, and candidate B is the only viable repair. Comparison: capture-pointer identity (breaks under CoW — rewriting storage copies would change identity); **explicit token (chosen — spike-proven: aliases equal, separate constructions distinct, hashes stable, map keys survive store/lookup/activation)**; code+capture composite (adds nothing over the token; more surface); semantically-unspecified identity (loses callable-map-key semantics — a regression). This is NOT PR-4 (guest-visible identity redesign): no observable behavior changes; the anchor moves from one internal Rc to another.

## 7. Spike results (candidate B model)

Reclamation: both P1 graphs reclaim fully; lifecycle: B frees when A's env drops (last holder). Transitions: exactly ONE CoW rewrite at store and ONE at lookup-escape; unchanged captures/bodies keep their Rc (aliases share). Identity: token ptr-eq across alias/stored/looked-up copies. Deep body traversal: required only when payloads embed runtime values (flag-gated in production: `body_bears` computed once at construction; reader-produced bodies can never embed callables, so the common case is zero-cost). Multi-owner: mutual class proven irreducible. New invalid states: none — weak references appear only inside owner storage; escaped values are fully strong (asserted); activation always witnesses. Complexity: the production delta over reviewed Stage C is ≈ +250/−80 (Closure two fields; transforms gain callable recursion + CoW; eq/hash one-line anchor change; tests).

## 8. Verdict: **GO — GRAPH 4 PLUS IDENTITY SPLIT**

1. **Stage-C code valid:** ~95% — Defs/define/lookup, fault plumbing, `for_call`, capture constructors, invoke paths, all Stage-B semantics, every passing test. 2. **Disposable/revised:** `rest_leaf`/`escape_leaf` (become callable-recursive with CoW), the `Closure` shape (+`ident`, +`body_bears`), the eq/hash anchor lines, plus new regressions. 3. **Stage B sound independently: yes** — definition semantics never depended on the barrier's depth. 4. **Do not revert Stage C** — extend in place; reverting discards the validated 95%. 5. **PR split not needed**: keep one PR; record the semantic-vs-mechanism layer line (already the deletion inventory) in the PR description for reviewability. 6. **Implementation remains paused** until the §9 approvals.

Sequencing: approve → implement the revision (bounded, one session: identity token, deep CoW transforms, `body_bears`, Sol's §18 regression list, rustdoc fix) → Sol Review 2b → Stage D.

## 9. Decisions requiring Louis's approval

1. Adopt candidate B: deep positional normalization with copy-on-write captures/payloads and a split identity token (the one item on the do-not-implement list this study must unlock: internal identity-anchor relocation, observable semantics unchanged).
2. Name and accept the residual class: mutual cross-owner storage joins atom-opaque cycles as a documented accepted leak class, with (a) a pinned irreducibility test, (b) a revisit trigger added to the memory-model study's §12 list, and (c) the B3 design note that handler-mediated value flow during module init can construct it (input to PR-1b review).
3. Resume implementation under the existing Stage-C protocol (extend in place; Sol Review 2b after).

## Drift classification

REQUIRED PR-1B.0 SEMANTIC: none (Stage B untouched). REQUIRED PR-1B.0 MEMORY MECHANISM: deep CoW normalization + identity token + payload traversal + regressions (on approval). FOUNDATIONAL REDESIGN: none required — the positional model survives, revised. ADJACENT — PARK: candidate E triggers; candidate F under a process model; Stage-E advisory fields. DRIFT — NOT IMPLEMENTED: Stage D, Stage E, B3, PR-2, macro staging, durable data, portable callables, process heaps, GC, and (pending approval) the identity split itself.
