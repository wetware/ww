# PR-1b.0 preflight — comparative study, spikes, and go/no-go

Status: COMPLETE — all five study batches integrated. Full evidence records (pinned SHAs, traced code quotes) archived under `.context/preflight-studies/batch{1..5}-*.md` (throwaway). Spike artifacts under `.context/spike/ownership-spike/`. No production code or docs modified. Nothing committed.

## 6. Spike A results — amended Graph 4 (all 17 proofs pass)

Crate: `.context/spike/ownership-spike` (lib + `tests/proofs.rs` + `tests/props.rs` + `src/bin/bench.rs`). Model implemented exactly as amended: `Defs{bindings, inherited, frozen}`, per-value `OwnerRef{Strong|Weak}` on Fn/Macro and `Option<OwnerRef>` on caps, **iterative** `rest_for`/`escape_with` (explicit work/output stacks, post-order rebuild), F1 ptr-eq scoping, sealed cap inners with rested methods + outer witness, witness-based upgrades, `has_resting_owner_refs` flag, frozen-bit prelude.

| # | Proof | Result |
|---|---|---|
| 1 | plain fn storage no cycle (`strong_count==1`, frees on drop) | PASS |
| 2 | named recursion no leak | PASS |
| 3 | mutual recursion; extract one; other resolvable via witness | PASS |
| 4 | same-owner callable in captured lexicals no leak (Sol P0-2) | PASS |
| 5 | foreign-owner captures stay strong/alive | PASS |
| 6 | nested module map preserves foreign owner (F1 regression) | PASS |
| 7 | defcap methods no cycle (rested inside sealed inner) | PASS |
| 8 | exported cap survives env drop; dispatch escapes via witness | PASS |
| 9 | attenuation transfers owner; lifetime preserved | PASS |
| 10 | ordinary maps carry lifetime; no hidden metadata | PASS |
| 11 | data-only owner frees immediately | PASS |
| 12 | last escapee controls reclamation | PASS |
| 13 | unmatched weak → explicit fault; frozen mutation → fault | PASS |
| 14 | 200k-deep containers, iterative transforms (see note) | PASS |
| 15 | identity/hash (captured-env ptr) preserved by rest/escape | PASS |
| 16 | callable map keys valid through transforms | PASS |
| 17 | atom cycle isolated, measurable, breakable | PASS |

**Spike-discovered facts:** (a) recursive `Drop`/`Clone` of deep values overflows independently of the transforms — a pre-existing property of any recursive `Val` (production too); the transforms are iterative, but PR-1b.0 must not introduce *new* deep-value paths that clone-then-drop whole trees in one call (`define` takes borrowed input in the spike for this reason). (b) `for_call`-escaped captures are themselves escapees (hold the owner) — the invariant ledger wording must count them.

## 7. Spike B results — binding cells competitor

Chosen competitor (per source study, see §5): **stable binding cells + owner anchor** (`Defs2{name→Rc<BindCell{value,version}>}`, closures capture `Weak<BindCell>` + one anchor). Results (`b1..b3` tests): cells deliver O(1) late lookup after capture and per-cell versioning — but **the anchor requires the identical positional barrier** (stored values still rest, escapes still upgrade, containers still rewritten, F1 still applies: b1/b3 prove `strong_count==1` only because the same rest step ran). Against the six mandated reduction criteria: transition count — unchanged; unrepresentable states — unchanged; container reconstruction — unchanged; host obligations — unchanged; failure modes — +2 (dead-cell vs dead-anchor); hidden roots — cells pin one Rc per name forever. **No material reduction → Graph 4 proceeds.** Cells remain a future *performance/versioning* refinement, not a lifetime alternative (Steel's evidence: adopting cells wholesale required a mark&sweep pass over the cell heap to reclaim cell cycles).

## 10. Performance results (release; thresholds set in `bench.rs` header before first run)

| Case | Result | Threshold verdict |
|---|---|---|
| data-only fast path (10k map lookup vs plain clone) | ratio **1.19** | T1 PASS (≤2.0) |
| 100k-deep define+lookup | 5.1 ms, no stack growth | T2 PASS |
| scaling 20k vs 10k nodes | ratio **2.00** | T3 PASS (linear) |
| one callable under 10k data nodes: lookup | **0.058 ms/iter** (10,001 nodes walked) | T4 **ACCEPTABLE** (≤1 ms) |
| 1,000 duplicate callables | 1,001 transform nodes; 0.0084 ms/iter | T5 PASS (linear) |
| `local_bindings` 1,000 defs (10% fns) | 0.059 ms/iter | T6 PASS |
| rest+escape small value | 0.00026 ms/iter | T7 PASS |
| cap dispatch (sealed 100-method inner) | 0.00018 ms/iter | — |

Documented costs: an upgrading lookup of a binding with resting refs is O(nodes of that binding's value) and rebuilds containers (structural-sharing loss per read — assoc-list here; PR-2's CHAMP will want seam-level transforms, flagged ADJACENT); data-only and foreign-only bindings take the flag fast path (measured ≈ plain clone).

## 11. Property-test results (`proptest`, 200 cases/law)

All pass: rest weakens exactly ptr-eq-owner refs; foreign strong preserved, no foreign weak ever introduced; rest and escape idempotent; escape∘rest preserves identity multiset (equality/hash); no weak refs outside resting storage in any escaped output; unmatched witness faults deterministically on every generated resting value; last-escapee reclamation with the atom class as the only surviving leak family; 1k–5k depth sweeps stack-safe. No counterexamples recorded.

## 8. Ownership invariant ledger (normative)

**Resting-weak locations (exhaustive):** (1) `Defs.bindings` values, self-refs only; (2) `CapturedEnv` slots, self-refs only; (3) sealed evaluator-owned cap inners (methods/base/handler), self-refs only. Nowhere else, ever.
**Escaped-strong:** everything observable/retainable by evaluation or hosts carries strong witnesses — "escaped" = *left the ownership subtree of its own `Defs`* (via lookup, `local_bindings`, `for_call`, cap dispatch, or fresh construction).
**Constructors (all start Strong, resting their self-owned interiors):** `fn`/`macro` creation (analyzed + raw paths); `defcap`; evaluator-local attenuation (owner transfer); `HandledCapInner` construction (owner-free handler or transfer); container literals holding owner-bearing values (no action — components already strong).
**Transitions:** `Defs::define` → rest (self only); `lookup`/`local_bindings` → escape; `for_call` → escape captured slots with the callable's witness; cap dispatch → escape with the cap's witness; attenuation → owner transfer; everything else (args, returns, lexical `set`, atoms, effect payloads, resume values, `Control` transport, natives) → **no transition** (values must already be strong — verified by property law 4).
**Failures:** unmatched weak at any escape boundary → **internal fault** (release-checked); frozen-owner mutation → internal fault; guest non-top-level `def` → catchable `glia.error/def-not-top-level`; host retaining undeclared guest values → embedder-contract violation (documented trust boundary).

## 9. Repository storage-site audit (every place a `Val` rests)

| Site | Location | Classification |
|---|---|---|
| Lexical frames | `eval.rs Env.frames` | ESCAPED STRONG STORAGE (values arrive strong; frames pop) |
| `Defs` (new) | PR-1b.0 | RESTING OWNER STORAGE |
| Closure captures | `capture_closure`/`snapshot`/`filter_to` → becomes `CapturedEnv` | RESTING OWNER STORAGE (self-refs) — REQUIRES DESIGN CHANGE (planned: Sol #1/#3) |
| Macro captures | same paths | same as closures |
| Atoms | `Val::Atom(Rc<RefCell<Val>>)` | OPAQUE stop; ESCAPED STRONG contents; accepted cycle class |
| Map keys / values, sets, vectors, lists | `ValMap`, `Vec<Val>` | ESCAPED STRONG in evaluated values; resting copies only inside `Defs` |
| Native fn captures | `NativeFnImpl` closures (kernel/caps/cli, `make_resume_fn`) | OPAQUE HOST TRUST BOUNDARY (documented; receive only strong values) |
| Async-native captures | `AsyncNativeFnImpl` closures | OPAQUE HOST TRUST BOUNDARY |
| Effect payloads in flight | `EffectSlot.pending: (EffectTarget, Val, Sender)` (`effect.rs:112`) | ESCAPED STRONG (transient) |
| Effect handler values | held by `with-effect-handler` machine (`handler_val`) | ESCAPED STRONG (looked up before installation) |
| Resume values | `oneshot::Slot.value: RefCell<Option<Val>>` (`oneshot.rs:19`) | ESCAPED STRONG (transient) |
| Continuation state | machine locals (`HandlerState::Handling` futures) | ESCAPED STRONG (owned by futures; drop-safe) |
| `GliaCapInner.methods: HashMap<String, Val>` (`lib.rs:198`) | defcap tables | RESTING OWNER STORAGE (sealed; Sol #4) — REQUIRES DESIGN CHANGE (planned) |
| `AttenuatedCapInner.base: Val` (`lib.rs:213`) | local attenuation chain | owner-transfer wrapper (planned) |
| `HandledCapInner.handler: Val` (`lib.rs:232`) | kernel/shell caps | owner-free natives today → OWNER-FREE; rule pinned in ledger |
| Expr AST: `Expr::Const(Val)`, `Quote(Val)`, `raw_args: Vec<Val>`, `FnBody::Raw` (`expr.rs:22/47/63/108`) | analyzed code retains source Vals | OWNER-FREE (source data cannot contain owner-bearing values — reader-constructed only) |
| `IMPORT_CACHE: thread_local HashMap<String, Val>` (`caps:264`) | evaluated module maps | REQUIRES DESIGN CHANGE — deletion already approved (PR-1b) |
| `LOAD_CACHE` (`caps:66`) | bytes only | OWNER-FREE |
| Kernel `Session`/env, cli `LocalShellRuntime.env`, shell cell env | session lifetime | ESCAPED STRONG STORAGE (the REPL strong root) |
| ww/test `*tests*` registry | guest-level atom | atom class (guest data) |
| WASM bridge / graft values | kernel graft loop → env bindings | ESCAPED STRONG STORAGE |
| Host callbacks (`HostEffectHandler`) | closures over embedder state | OPAQUE HOST TRUST BOUNDARY |

**No UNCLEAR items.** The two REQUIRES-DESIGN-CHANGE rows are exactly the already-approved Sol changes (CapturedEnv split; cap owner) — no new blockers.

## 12. Live capability-analysis mini-design

Roots: the value being granted/checked (cell grants, `is_authority_free` call sites). Reachable variants: all data recursively; Fn/Macro → captured lexical slots **plus the owner chain** (self-owner reachable ⇒ everything the owner holds at check time); caps → authority by definition; atoms → contents (cycle-guarded via visiting set, as today); natives → not authority-free (as today); opaque host payloads → not authority-free. Inherited traversal: walk `inherited` chain; the frozen prelude is certified authority-free **once** (SES-hardener-style ledger: one certification pass at freeze, O(1) thereafter). Memoization within a check: visited-set on (value ptr / owner ptr). **Across checks — compared options:** (A) full traversal each check: correct, O(reachable), no state — acceptable for cell-grant frequency but wasteful for REPL-heavy flows; (B) **owner generation versioning (recommended)**: `Defs.version: Cell<u64>` bumped by `define`; summaries cached keyed by the version vector of the owner chain (prelude frozen ⇒ constant; module owners sealed post-eval under top-level-only `def` ⇒ constant after import ⇒ summaries stabilize; REPL owner bumps invalidate); (C) per-cell versioning: strictly finer, only pays off with binding cells — deferred with them; (D) finalized module summaries: subsumed by B given sealing. Failure lane: traversal invariant break → internal fault. Cancellation: checks are synchronous walks — nothing suspends. **Placement: PR-1b.0** (it replaces the stale `is_cap_free` fields — Sol required change #12; B's machinery is a version counter + a small memo).

## 13. Revised risk register

| Risk | Invariant | Likelihood | Impact | Evidence | Mitigation | Test/spike | Blocks? |
|---|---|---|---|---|---|---|---|
| Weak value escaping | ledger §escaped-strong | low | high | property law 4 (0 counterexamples); centralized barrier | witness-based upgrades; release fault | props + P13 | no |
| Missed storage boundary | ledger completeness | low-med | high | audit table complete, 0 UNCLEAR | audit is normative; new storage sites must classify | audit review in PR | no |
| Opaque host retention | host trust boundary | med | med | natives receive only strong values | documented embedder contract | ledger §failures | no (contract) |
| defcap cycles | P7/P8 | resolved | — | spike P7/P8 pass with cap-owner design | Sol #4 implemented | P7/P8 | no |
| Capture cycles | P4 | resolved | — | spike P4 | Sol #3 | P4 | no |
| Foreign-owner corruption | F1 | resolved | — | spike P6 + property law 2 | ptr-eq scoping | P6 | no |
| Map-key hash changes | identity | low | high | P16 + prop law 3 (ptr identity preserved) | conversion never touches captured-env Rc | P15/P16 | no |
| Structural-sharing loss | perf | certain | low-med | bench: rebuild per upgrading read | flag fast path; PR-2 seam transforms (ADJACENT) | bench 4/9 | no |
| Large-value lookup cost | perf | med | low | T4 = 0.058ms @10k nodes, linear | `has_resting_owner_refs` gating | bench T1/T3/T4 | no |
| Authority-analysis cost | perf | med | med | mini-design B versioning | sealed owners ⇒ stable summaries | new regression | no |
| Stale authority summaries | soundness | resolved by design | high | Sol #12 + mini-design | version-keyed cache, no construction-time fields | data↔cap redef tests | no |
| Prelude mutation | frozen floor | low | high | frozen bit faults (P13) | freeze-before-share (SES lockdown pattern) | P13 + freeze tests | no |
| Cancellation privilege leak | def gate | low | med | `defining` is env-invariant, never toggled | Sol P1-4 rule | gate tests | no |
| Atom cycles | accepted class | certain | low (bounded) | P17: isolated, breakable, measured | documented; only non-host cycle family (prop law 9) | P17 | no |
| Nested import lifetime | F1 | resolved | — | P6 | — | P6 + import tests | no |
| WASM stack/alloc | T2 | low | med | iterative transforms; PR-1 stack history | constrained-stack + wasm e2e in PR-1b.0 matrix | planned tests | no |
| PR-2 incompatibility | seams | med | med | assoc-list rebuild ≠ CHAMP sharing | transforms behind ValMap/ValSet seams; no `im` internals | flagged ADJACENT | no |
| Deep-value Drop/Clone recursion | pre-existing | low | med | spike fact (a) | don't add clone-then-drop-whole-tree paths; note for reader/PR-2 | p14 note | no |

## 5. Binding-representation decision — **Model V (raw values) now; cells as a future perf/versioning track**

Evidence across nine systems: Racket CS `variable` records and BC buckets are **cells with weak owner backlinks** (strength fixed at creation, GC backstop); Lua closed upvalues are cells for *captured locals* while globals are late map lookups through `_ENV`; Clojure Vars are cells (`volatile Object root`, interned in the namespace map — confirmed at pinned SHA 2cea6925) read per call via `getRawRoot()` after one compile-time name→cell resolution; Chez user globals are cells (symbol value slots) while primitives are snapshotted/inlined — a *stability-class split*; Steel proves **raw values + late slot reads suffice** for top-level late binding and recursion; Rhai uses cells only for mutated captures; Rune's `Globals` slot block is Graph 4-shaped with Strong-always (and leaks on closure-in-static); Gluon's link-time snapshots are type-system-driven. Spike B proved cells **do not remove one obligation of the barrier** (same rest/escape, same container rewriting, same F1) while adding two failure modes and per-name pinned allocations; Steel's history shows wholesale cells demanded a mark&sweep pass over the cell heap. Model S (slots/indexes) is premature compilation machinery for a runtime whose analyzer is deliberately thin. **Decision: Model V — `Defs { name → Val }`** with late lookup through the owner; Model B remains additive later if lookup profiling or per-cell capability versioning demands it (not chosen merely to minimize diff — chosen because the competitor spike showed no invariant reduction, per the decision standard).


**Honest classification:** the Clojure and Chez records formally CHALLENGE raw-value `Defs` — both flagship systems converge on cells for *live* top-level namespaces. The decision survives because every cell-dependent behavior they trace maps onto Glia differently: (a) redefinition visibility, forward reference, and top-level mutual recursion depend on cells *only when name→location resolution happens once at compile time* — Glia's interpreter resolves names through `Defs` at call time, so raw values give the same observable semantics (the studies' own verdicts list these as "satisfied by late lookup by name"); (b) the remaining cell-only behaviors are deliberately foreclosed or absent in Glia: live re-export aliasing (frozen snapshot exports chosen in the export-boundary design), `declare`-before-define with idempotent identity, `ns-unmap`, `with-redefs`; (c) Spike B proved cells do not reduce the ownership barrier by even one obligation; (d) Chez's primref snapshotting legitimizes raw storage for exactly Glia's dominant stratum — the frozen prelude and closed module `Defs`. **Flip triggers** (revisit Model B if any lands): adding `declare`/forward declaration with stable identity; adding live re-export; per-call name-hash lookup showing up in profiles; per-binding capability versioning (mini-design B's per-owner counter has a cheaper Clojure-precedented variant: one global `rev` bumped on every rebind).

## The five Glia decisions

1. **Is `Defs` the correct structural unit?** Yes — it is Racket's `definitions`/instance, SES's compartment module-state, Monte's imports→exports instantiation, Newspeak's module instance, in Rc form. One caveat adopted from Newspeak/N-1: the live `Defs` is **never handed to guest code**; only the frozen export projection is.
2. **Raw values or binding cells?** Raw values (above).
3. **Ordinary value-map exports, or live binding views?** Ordinary maps — SES's live namespace objects exist to satisfy ESM live-binding semantics and cost an updater/TDZ graph plus a confinement subtlety ("frozen surface, live interior"); Monte gate-checks plain maps; Newspeak projects through `public`. Snapshot maps at module close are simpler and safer. (S3's challenge noted: per-import instantiation forfeits shared module identity — reaffirmed as the deliberate, capability-motivated choice; Newspeak's "module definitions are re-entrant, instances independent" is the working precedent.)
4. **Is live authority traversal the correct semantic model?** Yes, with Monte's refinement as the roadmap: conservative construction/check-time certification (fail-closed, like E's `isDeepFrozen` approximation) now — mini-design B versioning makes it cheap; a per-binding guard/tag scheme (Monte M-3) is the future upgrade that turns deep walks into free-name tag checks. Joe-E's split is adopted vocabulary: **DeepFrozen ≠ Powerless** — the prelude must be certified for both.
5. **Is amended Graph 4 a memory mechanism or accidental authority semantics?** The studies are unanimous and normative: **memory only.** E revokes by mutating captured cells; Monte certifies; Newspeak mediates; none uses weakness for authority. Therefore (a) a dead weak edge is an *internal fault*, never an implicit revoke (already the design); (b) revocation/attenuation features must never be built on OwnerRef; (c) `Strong(Rc<Defs>)` means "keep alive," and authority analysis discovers reachable authority separately (the two ledgers never merge). This rule is added to the invariant ledger.

## 14. Scope / drift report

**REQUIRED BEFORE IMPLEMENTATION** — none remaining: the storage audit has zero UNCLEAR/blocking rows; both spikes green; verdict below.
**REQUIRED CONSEQUENCE OF PR-1B.0** — everything in the amended contract, now evidence-annotated: centralized barrier at the escape choke points (liveslots/Pony/SES convergence: the flip lives where references cross an ownership boundary, nowhere else); frozen prelude before first module birth (SES lockdown ordering) with a one-pass certification ledger; `defining` gate; live capability accounting via owner versioning (mini-design B); Drop-must-not-exercise-authority rule (Joe-E finalizer ban + SwingSet BOYD — Glia `Drop` impls only release, never act).
**ADJACENT — PARK**: PR-2 CHAMP-preserving seam transforms; per-binding guard tags (Monte-style) for O(names) certification; binding cells; drop-vs-retire two-state edges (SwingSet A2) for future module-unload UX; membrane/uncooperative revocation.
**FUTURE TRACK**: vat/turn discipline mapping; durable-object authority ledgers (SwingSet A4, feeds the child-bootstrap record design).
**DRIFT — DO NOT IMPLEMENT**: namespaces/aliases, explicit exports, map invocation, macro staging, GC/arena, host ownership declarations, PR-2 collections, callable identity work.

## 15. Final verdict: **GO — AMENDED GRAPH 4** (as amended by Sol + this preflight's normative additions)

1. **Frozen semantics**: lexical/ownership split; late top-level binding through `Defs`; top-level-only `def`; per-import module instantiation with ordinary-map exports (identity trade-off reaffirmed); frozen shared prelude; exceptions/faults/controls per PR-1; authority = possession, analyzed live.
2. **Lifetime architecture**: amended Graph 4 — per-value `OwnerRef` weak-at-rest/strong-when-escaped, centralized iterative `rest_for`/`escape_with`, `CapturedEnv` split, cap-owner transfer, witness-based upgrades, frozen prelude bit.
3. **Why better than the strongest competitor**: Spike B (cells+anchor) reduced none of the six mandated surfaces and added failure modes; all other alternatives either hide roots (arena, registry), leak by policy (runtime-lifetime owners, Rune's Strong-always), or are GC (excluded).
4. **Strongest supporting evidence**: Agoric liveslots' `exportedRemotables` — a production capability runtime independently running weak-at-rest/strong-on-escape with the flip at one choke point; Rune `Globals` — the same owner-handle shape, whose Strong-always variant demonstrably leaks on exactly the case our weak-resting fixes; Racket's weak cell→owner backlinks.
5. **Strongest challenging evidence**: no inspected system flips reference strength dynamically (Racket fixes it at creation; everything else rides GC) — the barrier discipline is genuinely novel, which is why it is fenced by the ledger, the property laws (0 counterexamples), release-checked faults, and the 17 proofs rather than by precedent.
6. **Do binding cells change the answer?** No (Spike B); they remain an additive refinement.
7. **Ordinary-map exports sound?** Yes — P6/P10 + SES/Monte/Newspeak precedent; the live `Defs` is never exposed.
8. **Live capability analysis tractable?** Yes — mini-design B (owner versioning + sealed module owners + frozen-prelude certification); Monte's tag scheme as the upgrade path.
9. **Performance acceptable?** All seven pre-set thresholds passed; the one structural cost (O(value) upgrading reads, sharing loss per read) is measured, gated by the fast-path flag, and parked as a PR-2 seam optimization.
10. **Remaining uncertainties**: production-scale behavior of the barrier under the full evaluator (spike ≠ evaluator); ValMap rebuild costs on real CHAMP (PR-2); no open study batches — the final two arrived confirmatory on capture, late binding, module shape, and the frozen/live stratum split; their cell evidence challenges only the binding representation, resolved in §5 with explicit flip triggers, and does not touch the ownership design.
11. **Work that may begin next (on approval)**: PR-1b.0 implementation per the amended contract + this report's normative additions (authority-invisibility rule; Drop-releases-only rule; freeze-before-first-module ordering; export-projection-only rule).
12. **Parked**: everything in §14 ADJACENT/FUTURE.

## 16. Decisions still requiring Louis's approval

1. Adopt the GO verdict and begin PR-1b.0 (sequence unchanged: PR-1 merges first).
2. Ratify the four normative additions from the studies: barrier is authority-invisible (dead weak = fault, never revoke); `Drop` releases, never acts; prelude freeze+certify before any module `Defs` exists; guest code never receives the live `Defs` (exports are the only projection).
3. Reaffirm per-import instantiation against SES's shared-instance precedent (S3) — the capability-motivated trade already chosen in the cache audit.
4. Model V (raw values) for `Defs` in PR-1b.0, cells parked behind the §5 flip triggers — noting honestly that the strongest cross-system precedent (Clojure Vars, Chez symbol slots, Racket variables/buckets) favors cells as the eventual endpoint for live namespaces.
5. Keep the spike crate under `.context/spike/` during PR-1b.0 implementation as the reference model, delete on merge.

## 1. Executive conclusion

**GO — AMENDED GRAPH 4.** All 17 Spike A proofs pass; the strongest competitor (binding cells) eliminates none of the barrier's obligations; all seven pre-set performance thresholds pass; 800 property-test cases across four ownership laws find no counterexample; the storage audit has zero UNCLEAR sites; and the fifteen-system source study yields two independent production precedents for the barrier's exact shape (Agoric liveslots `exportedRemotables`; Rune `Globals` owner handles, whose Strong-always variant leaks precisely where our weak-resting refinement fixes it). One discipline is genuinely precedent-free — no studied system flips reference strength dynamically — and it is fenced by the invariant ledger, release-checked faults, and the proof/property suites rather than by precedent. The OCAP track adds one normative rule the design must carry forward: reference weakness must remain invisible to authority semantics.

## 2. Track A — runtime/binding matrix (8 language runtimes, source-pinned)

Full records with traced code quotes: `.context/preflight-studies/batch1-racket-lua.md`, `batch3-steel-rhai-rune-gluon.md`, `batch5-clojure-chez.md`.

| System @ pin | Top-level definition storage | Lexical capture | Top-level late binding | Recursion mechanism | Export representation | Lifetime / cycles | Transferable lesson |
|---|---|---|---|---|---|---|---|
| Racket (CS+BC) @ 2706d5c2 | **cells** (CS `variable` records / BC buckets) with weak owner backlink; strong by explicit flag fixed at creation | flat closures copy values; `set!`-ed locals pre-boxed (`bangboxenv`) | cell deref per reference; unbound checked at use | top-level via late cells | imports link to the exporter's cell (`GLOB_IS_LINKED`) | tracing GC; weak backlinks tune retention | cells + weak owner backlinks; owner-link strength is a per-cell bit, never flipped |
| Lua @ 7579fc9 | `_ENV` table entries (map, not per-name cells) | per-variable `UpVal`: open = stack location, closed = owned value in place | every global access is a table lookup (`OP_GETTABUP`) | global recursion via late `_ENV` lookup | module = table fields (convention) | tracing GC; open-upvalue fixup in atomic phase | capture the location only while mutation must be observable, then close in place |
| Clojure @ 2cea6925 | **cells** (`Var.root`, volatile) interned in namespace CAS map | flat by-value copy into closure fields at construction | name→cell once at compile; `getRawRoot()` per call; opt-out = direct linking | self: `thisName` self-ref; `letfn`: construct-then-patch; top-level mutual: unbound-Var cell | import = sharing the identical Var object (no copy) | JVM tracing GC | resolve name→cell once, read cell per call; redefinition = cell mutation + version bump (global `rev`) |
| Chez @ 814fa4e0 | **cells** = symbol value slots (slot 0 of symbol layout); in-cell unbound sentinel | flat closures copy values; assignment conversion boxes only mutated vars | every reference loads `symbol-value-disp`; primitives snapshotted/inlined (frozen stratum) | letrec: allocate-all-then-patch (real cycles); top-level mutual via cells | library export = `(uid . label)` cell indirection + write ban — never a value copy (boot-time excepted) | tracing GC; weak pairs via post-pass resweep; ephemerons | box only what mutates; export = indirection + write prohibition; split binding policy by frozen vs live stratum |
| Steel @ 3a418c9 | **raw values** in flat `Env.bindings_vec` slots | flat `CaptureVec` snapshot; cells only for mutated captures | `CALLGLOBAL` re-reads slot per call | global-slot re-read; local letrec via mutable cell | compile-time name mangling into one env | Rc + mark&sweep over the mutable-cell heap | snapshot capture + late slot reads + owner-strong/user-weak cells: a proven no-GC Lisp recipe |
| Rhai (pin in batch3) | `Scope{names,values}` parallel vectors; module `BTreeMap` | no true closures: captured names become params; `Share` promotes to `Rc<RefCell>` cells curried in | name+arity hash resolved per call (cached) | per-call re-resolution; depth-capped | module = value map + hash-keyed fn table | Rc/Arc; no cycle collection — clone-heavy culture dodges cycles | binding-cell capture works on Rc but only because the data culture avoids cycles |
| Rune (pin in batch3) | `GlobalsInner.slots`: Rc-shared slot block, one slot per static | `Box<[Value]>` snapshot delivered as hidden tuple arg | statics read slot at runtime; fn-by-hash fixed at compile | fn hash lookup at fixed offsets | items by hash in `Unit`/`Context` | hand-rolled non-atomic refcount; **cycles leak** | escaped callables carrying an Rc owner handle = OwnerRef, field-tested; Strong-always leaks on closure-in-static |
| Gluon (pin in batch3) | query-database globals, baked in at link time | GC-heap upvar array | none — link-time snapshot (early binding) | cyclic closure via allocate-dummy-then-patch (needs GC) | record fields of a typed module value | tracing mark&sweep, generation tree | recursion-as-cyclic-capture and link-time snapshots both require what Glia lacks (GC / static types) |

## 3. Track B — OCAP matrix (7 capability systems, source-pinned)

Full records: `.context/preflight-studies/batch2-ses-swingset-joee-pony.md`, `batch4-e-monte-newspeak.md`.

| System @ pin | Authority source | Capture carries authority? | Module/compartment owner | Kept-alive vs authority separation | Export form | Transferable lesson |
|---|---|---|---|---|---|---|
| SES/Endo | endowments passed into Compartment; intrinsics frozen at lockdown | yes — closures over endowments are the grant mechanism | Compartment instance (private WeakMap fields) | not separated at ref level (hardened ≠ authority-free is the analogous split) | frozen namespace with LIVE getter bindings; instances memoized per (compartment, specifier) | freeze the shared floor first; one hardened ledger; decide instance-identity semantics explicitly |
| Agoric SwingSet / liveslots | c-list entries, kernel-mediated | yes — Presences/Remotables in closures | vat (one liveslots instance) | fully explicit, three ledgers: REACHABLE vs RECOGNIZABLE vs none; **strong `exportedRemotables` for escaped, weak for everything else**; BOYD quarantines death-observation | `o+NN` slots in c-lists | put the weak→strong flip at ONE escape choke point; track drop (authority) and retire (identity) separately |
| Joe-E | constructor-passed refs; taming DB default-denies host statics | yes — statics banned, so ONLY capture carries it | none at runtime; class = verified unit | orthogonal by construction: liveness never grants (no statics), death never acts (finalizers banned) | public methods of verified classes | Powerless (authority-free) ≠ merely frozen; ban authority in destructors |
| Pony (contrast) | any non-`tag` reference; actors as capabilities | yes, but the reference capability bounds use (viewpoint adaptation) | actor (own heap) | statically: `tag` = alive-addressable-unreadable; refcount ≠ rights | sendable refs {iso,val,tag} | name your reference-strength states and transitions; reserve an identity-only state |
| E-on-Java @ a0b3b599 | object refs; pruned captured field/outer arrays; safe scope ambient | yes — method bodies read frame slots, no per-call check | none formal; Vat is the compartment | separated **by mutation**: revoke = assign the captured slot to a broken ref (caretaker); capture pruned statically | facets returned by maker functions | revocation = cell-content swap, never reference weakness |
| Monte/Typhon @ 92d70fbc | object refs; per-object immutable pruned `frame[]` | yes — `runMethod` evaluates against the stored frame | module = DeepFrozen singleton fn: imports-map → exports-map | separated **by proof**: DeepFrozen certified at construction from the guards of the closure's free names | guard-checked `Map[Str, DeepFrozen]` | per-name construction-time certification beats runtime deep walks |
| Newspeak @ 945b81e8 / psoup 9ac43bea | slots injected via factory args (`usingPlatform:`); zero ambient | yes, but mediated — every outer read is a virtual send climbing `enclosing_object_`, filtered by access modifiers | module = instance of a top-level class; slots own imports | separated **by mediation**: the pointer exists but only `public` names resolve | `public` accessors/classes on the module instance | module authority = its slot list; two-phase definition-vs-instantiation linking |

## 4. Findings digest (classified, cross-cutting)

1. **Cells are the mature convergence for LIVE top-level namespaces** (Racket variables/buckets, Clojure Vars, Chez symbol slots) — formally CHALLENGES raw-value `Defs`; resolved in §5 (Glia's call-time name resolution supplies the same observable semantics; flip triggers recorded). Raw values + late lookup are proven sufficient in the Rust-interpreter class (Steel) and for Lua globals (map entries).
2. **No studied system flips reference strength dynamically.** Racket fixes owner-backlink strength per cell at creation; everything else rides tracing GC. The two precedents for the barrier's *shape*: liveslots (weak-at-rest / strong-when-escaped at exactly one marshalling choke point — CONFIRMS, strongest) and Rune (owner handles on escaped callables, Strong-always — CONFIRMS the shape, and its leak on closure-in-static is exactly what weak-resting fixes). The dynamic flip itself is precedent-free → validated by spike + property laws instead.
3. **Authority is never expressed via weakness** in any OCAP system: E separates by mutation, Monte by proof, Newspeak by mediation, Joe-E by construction, liveslots by explicit ledgers. Normative rule adopted: the OwnerRef barrier is Rc-cycle management only; a dead weak edge is an internal fault, never an implicit revoke; revocation/attenuation must never be built on OwnerRef.
4. **Value-snapshot lexical capture is universal** for immutable locals (Chez assignment conversion, Racket bangboxenv, Clojure final fields, Steel/Rune/Gluon snapshots, Lua UpVal close); locations are boxed ONLY where mutation must be observed — CONFIRMS Glia's snapshot capture and prescribes the `set!`-locals refinement if ever wanted.
5. **Recursion decision table** (Clojure C3 + Chez Z3): self-recursion = self-reference or late lookup (no cell needed); *local* mutual recursion = construct-then-patch, which creates real object cycles (safe only under GC — Chez letrec, Gluon) or must route through the definition owner; top-level mutual recursion falls out free from late binding. Glia routes recursion through `Defs` — the only no-GC-safe option — CONFIRMS Graph 4's architecture.
6. **Exports**: Chez (cell indirection + write ban; value-copy is its boot-time special case), Clojure (shared Var identity), SES (live-view namespaces) all keep exports live; Glia's frozen snapshot maps are a deliberate, capability-motivated divergence (reaffirmed; forecloses live re-export — recorded as a §5 flip trigger).
7. **SES instance memoization per (compartment, specifier)** challenges per-import instantiation — answered: deliberate divergence, already decided in the cache audit (policy B); Newspeak's "module definitions are re-entrant, instances independent" is the working precedent for our side.
8. **Chez Z6 (ephemerons) adds one required test** for the PR-1b.0 suite: the key-in-value shape — an atom resting in frozen module A's `Defs` later mutated to hold a module-B-owned value while B holds A-owned escapees — must land in the accepted, breakable atom-cycle class (spike p17 analog, cross-module variant). Rc has no ephemeron; the write barrier plus the atom-opacity rule is the only defense, so it gets a regression test.
9. **Clojure C4 gives precedent for mini-design B's version counters** — with a cheaper variant (one global `rev` bumped on every rebind, instead of per-owner counters) if versions are used only for cache invalidation.
10. **Frozen/live stratum split is Chez-precedented** (Z5: primref snapshot vs user-global cell read, chosen by stability class) — CONFIRMS the frozen-prelude design and the freeze-before-first-module ordering.
