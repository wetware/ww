# Collector pivot — finding reconciliation and Cc spike plan

Status: DESIGN ONLY (accepted direction: PIVOT TO RC + CYCLE COLLECTION; KEEP STAGE B; PARTIALLY KEEP STAGE C — provisional pending Louis's approvals in §17). No production/doc edits. Nothing committed. Supersedes `.context/pr1b0-crossowner-reconciliation.md`'s GO — IDENTITY SPLIT verdict.

## 1. Finding-by-finding response (verified against production tree, Stage-C spike, reconciliation spike)

| Sol finding | Position | Verification and consequence |
|---|---|---|
| §3 reconciliation spike faults on sealed foreign weak captures (single global witness) | **ACCEPT** | Confirmed in my own model: `deep_escape_inner` recurses the ONE witness through nested captures (crossowner.rs, 5 sites); production `capture_free` rests same-owner slots at construction (eval.rs:475/501), so a B-owned callable with `h: Weak(B), g: Strong(A)` is a valid production state; escaping through A faults on `h`. The repair needs per-callable owner-context traversal — the tracer-shaped complexity Sol names. Consequence: deep Graph 4's true cost was understated; my +250/−80 estimate is withdrawn. |
| §3 spike gaps (no map/Eq/Hash in XVal, no final-escape test, no allocation instrumentation, recursive transforms, bench never exercises deep ops, body-sharing assumed) | **ACCEPT** | All verified by inspection of `crossowner.rs`/`bench.rs`. The spike proved less than I claimed. |
| §4 classification of the two P1s as routed self-cycles; true mutual SCC distinct | **ACCEPT** (it confirms my analysis) | Matches the reconciliation. The correction is what follows from it, not the classification. |
| §5 traversal inventory incomplete (patterns incl. map keys, LetBinding, FnParam; no metadata fields; field-addition not compile-caught) | **ACCEPT** | Verified: `analyze_pattern`'s Map arm stores raw key `Val`s — macro-produced match forms can embed callables as pattern map keys (my earlier "patterns structurally excluded" held only for `Literal`); `FnParam` exists (6 refs). Consequence: deep Graph 4 is an AST/object-graph tracer over ≥7 representation families. |
| §6 identity split = deferred callable-representation redesign; unnecessary under collection | **ACCEPT** | Under `Cc`, identity = stable collector-pointer of the callable/capture allocation — no token, no divergent-representation hazard, no new allocation policy. My "not PR-4" framing understated the representational consequences (N stored copies sharing one token). |
| §7 CoW cost model mismatch (production bodies unshared; per-lookup copies; path-multiplication on shared DAGs; not WASM-stack-safe as modeled) | **ACCEPT** | Verified against `Val::Fn`/`FnArity` (bodies are owned `Vec`s) and the recursive spike transforms. |
| §8 residual mutual-SCC class is NOT rare (atom callback registries today — `ww/test` `*tests*` at test.glia:20; B3 handler injection; Stage-D service wiring; no `Defs` delete; teardown unreliable) | **ACCEPT** | Registry verified in-tree. My "rare, bidirectional-flow-only" claim was wrong: `deftest` is exactly the registration pattern, available today. This breaks the accepted-class premise of GO — IDENTITY SPLIT. |
| §9 Stage D compounds SCC formation and traversal heterogeneity | **ACCEPT** | Follows from the cap-inner payload shapes (verified earlier: methods/base/handler hold `Val`s). Stage D under Graph 4 fails the rarity threshold. |
| §10 complexity ledger (own = 381 lines; repaired total ~900–1,300 RC-specific production lines; 13+ coupled invariants) | **ACCEPT** | 381 verified exactly. The invariant list matches the ledger + this review's additions. |
| §11 strongest architecture: single-threaded non-moving collector-aware `Cc` executable heap; Bacon–Rajan as reference algorithm, not dependency; Boa-style tracer second; BEAM stays destination | **ACCEPT** | Consistent with batches 6–9 evidence. |
| §12 async/WASM feasibility (unbranded `Cc` across await; safepoints at borrow-free turn boundaries; iterative phases) | **ACCEPT** | Verified against evaluator future shapes (Vals across awaits, oneshot slot state, `'static` native futures). |
| §13 authority rules preserved; TCB shifts to trace correctness (over-report = premature free) | **ACCEPT** | Drives the §11 property tests and the exactness contract below. |
| §14 Stage-B salvage (all semantics; drop `has_resting_owner_refs`; `Cc<Defs>` handle; witness faults disappear) | **ACCEPT** | Inventory below. |
| §15 Stage-C salvage/delete; edit-forward, no git reset; never restore `Rc<Env>` callables | **ACCEPT** | Inventory below. The no-post-B-checkpoint observation is correct (nothing committed by design). |
| §16 cost comparison favors pivot on ≥1-year horizon | **ACCEPT** | With §3/§5/§7 verified, Graph 4's remaining cost is not my earlier estimate; the comparison holds. |

No DISPUTED findings. One nuance recorded, not disputed: `Pattern::Literal` itself only ever holds scalar reader values (analyzer-restricted); the macro-reachable pattern hole is specifically **map-pattern keys** — the trace inventory must cover both anyway.

## 2. Stage-B salvage inventory (exact)

KEEP UNCHANGED (semantics + tests): `Defs` (bindings/inherited/frozen/version), `Env::define` + `define_or_throw` + `DefineError`, top-level privilege (`defining` invariant, all privilege-matrix tests), late binding via `Env::get` fallthrough, `get_lexical`, named/mutual recursion, `local_bindings` export projection + the caps:484 switch, `load_prelude` lifecycle (memoize/certify/freeze/`publish_or_adopt_prelude`) and its tests, `def-not-top-level` error machinery, frozen-owner fault classification, all 22+ `stage_b_semantics` tests, prelude-lifecycle tests, recursion budget tests, collection/provenance semantics tests.
KEEP WITH ADAPTATION: `Rc<Defs>` → `Cc<Defs>` (mechanical); `Binding` loses `has_resting_owner_refs` (values stored as-is — all-strong model); `Env::get`/`local_bindings` return to infallible signatures (witness faults cease to exist) while the `resolve` choke point remains a named seam; `Defs::define/lookup` drop rest/escape calls.
DELETE: nothing semantic.

## 3. Stage-C salvage/delete inventory (exact)

KEEP: `CapturedEnv` lexical-only snapshot + `capture_all`/`capture_free` (minus the `rest_frame_for` call and `has_resting` field); opaque `Closure` (fields become `captured: Cc<CapturedEnv>`, owner edge becomes plain strong `Cc<Defs>` — a traced edge, not an `OwnerRef`); every constructor/consumer/invocation migration (the flag-day work); `Env::for_call` semantics (caller handler stack, `defining=false`, defining-owner `defs`) minus witness/escape logic; callable identity/hash as stable pointer (now the `Cc` allocation); all SEMANTIC Stage-C tests (survival, recursion, handler, suspension, imports, map keys); budget-stack infrastructure; `capture_all_merges_frames`.
DELETE (edit-forward, ~480–550 production lines + ~360–460 test lines): `mod own` entire (381 lines: `OwnerRef`, `rest_for`/`escape_with`/`rest_leaf`/`escape_leaf`/`enter`/`build`/`rest_frame_for`/`seal_cap_inner`/`transfer_owner`/`OwnFault`), `own_invariant_fault`, witness plumbing in `for_call`, `has_resting_owner_refs`, `fault_plumbing_tests` (witness class), RC count/transform tests (`ownership_tests` transform + count suites — replaced by collector equivalents), Stage-D shells.
FOUNDATIONAL REDESIGN (new): the `cc` module (below).
PARK: Stage-E advisory fields (per Sol §19 of Review 2); `map-literal` provenance interplay re-checked once transforms disappear (no transforms → no provenance hazard at all — the CoW-provenance machinery in `build` deletes with it).
Never restore `Rc<Env>` callables.

## 4. Collector participation boundary

PARTICIPANTS (Cc-managed allocations, traced): `Defs`, `CapturedEnv`, atom cells (`Atom(Cc<RefCell<Val>>)`), known evaluator-owned cap inners (a private traceable enum over Glia/Attenuated/Handled payload shapes), and the callable capture allocation (identity anchor). `Val` itself stays a plain enum VALUE; participation flows through the `Cc` handles it contains.
TRACED-THROUGH (edge enumeration, not managed): `Val` (all four transparent containers + participating handles), `FnArity`/`FnBody`, `Expr` (Const/Quote/DefMacro.raw_args/Call.raw_args + all recursive positions), `Pattern` (Literal + map keys + nested), `LetBinding`, handler-machine state that owns `Val`s, oneshot slots, `Env` frames (host-side root via residual counts).
CONSERVATIVE LEAVES (never traced, never premature-collected — untraced edges only ever under-subtract, which classifies suspects as externally rooted): `NativeFn`/`AsyncNativeFn` closures, unknown `Rc<dyn Any>` cap payloads, `Bytes` and all domain-B data, future durable handles. The three-domain model and the jurisdiction leaf rule carry over verbatim: durable/IPFS data is collector-inert and opaque; authority remains separate from reachability; `Drop` releases only.

## 5. `Cc<T>` / `Trace` design surface (crate-private, in-tree; Bacon–Rajan as reference algorithm — no dependency on the stale crate)

```rust
pub(crate) struct Cc<T: Trace + 'static> { ptr: NonNull<CcBox<T>> }   // !Send !Sync
#[repr(C)] struct CcBox<T> { meta: CcMeta, value: T }
struct CcMeta { strong: Cell<u32>, color: Cell<Color>, buffered: Cell<bool>, vt: &'static CcVt }
enum Color { Black, Gray, White, Purple }
pub(crate) unsafe trait Trace { fn trace(&self, t: &mut Tracer); }
// CONTRACT: enumerate every owned participating edge EXACTLY once; never
// clone/allocate/drop inside trace; RefCell contents traced via try_borrow
// (borrowed-at-safepoint is impossible by the safepoint rule, enforced by
// debug_assert).
pub(crate) struct Tracer { work: Vec<ErasedCc> }   // iterative worklists ONLY
```
Decrement path: `strong>0` after dec → color Purple + buffer push (dedup via `buffered`). `collect_cycles()`: purge dead buffer entries → iterative mark_gray (trial subtraction) → iterative scan/scan_black (restore externally-referenced) → collect_white into a Vec first (drop outside traversal, count-compensated — the two documented Rust-specific corrections from batch7) → exactly-once value drop guarded by a freed bit. No weak handles in v1 (no weak states exist in the model). No finalizers, ever. Unsafe confined to: box alloc/dealloc, erasure vtable, pointer deref — all in one module with SAFETY comments.

## 6. Safepoint model

Collection may run ONLY at: (a) top-level turn boundaries — `eval_toplevel`/`eval_toplevel_expr` return edges in the REPL/kernel/module loops; (b) explicit host calls. Trigger: buffered-suspect count threshold (tunable const) checked at safepoints; plus `collect_now()` for tests. NEVER inside evaluation, handler polls, native calls, or while any participating `RefCell` borrow can be live — guaranteed structurally because safepoints sit outside `poll` frames in the single-threaded runtime.

## 7. Async-rooting model

None required — that is the point of unbranded non-moving `Cc`. A suspended future owning `Cc` handles (call envs, Vals, resume state, oneshot slots) contributes residual strong counts; trial deletion therefore classifies everything it reaches as externally rooted. Spike experiment 3 proves it: suspend holding the SOLE handle, collect at a safepoint, resume, invoke. Cancellation = ordinary drops (possibly buffering suspects for the next safepoint). No rooting API for embedders: host-held `Val`s root automatically (experiment 4).

## 8. Host/native-edge policy (options for decision 5)

(a) RECOMMENDED NOW — conservative leaves: host closures/payloads are never traced; anything they hold leaks only if it participates in a cycle through them; premature collection is impossible by construction (under-subtraction ⇒ rooted). Documented as the trust-boundary rule's collector form. (b) LATER OPT-IN — a `HostTrace` registration for embedder payloads that want cycle participation (Stage-D-adjacent, parked). (c) REJECTED — mandatory host trace contract (unenforceable over `Rc<dyn Fn>`, exactly CPython's untracked-container failure mode).

## 9. Capability cleanup semantics

Acyclic caps: deterministic release at refcount zero (unchanged today). Cyclic caps: memory reclaimed at the next collection; `Drop` remains release-only; anything timing-sensitive (sessions, epochs, membranes, host resources) REQUIRES explicit close/revoke — which is already the authority model (revocation is never GC). Death observation, if ever needed for bookkeeping, happens only at safepoints (liveslots BOYD precedent). No guest-visible finalization, no resurrection.

## 10. Collector spike plan (`.context/spike/cc-spike/`, standalone workspace)

S1 core: `Cc`/`Trace`/buffer/`collect_cycles` fully iterative; drop-counter harness. S2 model graphs over `MDefs`/`MClosure`/`MCaptured`/`MAtom`/`MCapInner` (mirroring §4): both Sol P1s, TRUE mutual SCC, atom-registry SCC (the `*tests*` shape), final-escape reclamation, exactly-once drops, acyclic-fast-path. S3 identity: eq/hash/map-key semantics on `Cc` pointers incl. through storage/lookup (no transforms exist — trivially stable; test pins it). S4 async: hand-rolled future suspends holding the sole callable; collect; resume; invoke. S5 host roots: `Vec<Val>`-shaped external retention across collections. S6 conservative-edge safety + property tests (§11). S7 Miri over the whole spike suite + unsafe review checklist (§12). S8 WASM (§13). S9 benches vs the deep-CoW comparator already in `crossowner.rs` (§14). Exit: all green + budgets met → foundational-PR approval; unacceptable unsafe complexity → comparative Boa-style-tracer spike before any Graph 4 reconsideration.

## 11. Required property tests (proptest over generated object graphs)

(1) Soundness under omission: for random graphs with randomly OMITTED trace edges — collection never frees a reachable object (leak-only). (2) Duplicate-edge detection: a debug Tracer asserts per-object edge multiset exactness; property: any duplicated edge is caught in debug. (3) Exactly-once drops for arbitrary SCC mixes (drop counters). (4) Collect idempotence: second collect at same state frees nothing. (5) Safepoint invariance: interleaving clones/drops/stores between collects never changes final liveness. (6) Acyclic graphs never enter the collector (buffer purged, no trace).

## 12. Miri / unsafe review

Run Miri on the entire cc-spike suite (host target). Unsafe inventory reviewed line-by-line: box layout/alloc/free, vtable erasure, `NonNull` derefs, freed-bit discipline, buffer eviction on re-increment (the crate's historical `extra_free`/`scan_black` bug shapes become pinned regression tests). No `unsafe` outside the `cc` module. Trace impls are safe code over exhaustive destructuring (no `..` on participating shapes, per Sol §5's field-addition hazard).

## 13. WASM test plan

Build cc-spike for wasm32-wasip2; run under wasmtime at the 2 MiB budget: 100k-deep chain and 10k-SCC batch collections — all phases iterative, flat stack asserted by completion; plus the S4 async test compiled to wasm. Production stage later re-runs kernel wwtest suites unchanged.

## 14. Performance benchmark plan (thresholds preset before measurement)

Mutator: `Cc` clone/drop ≤ 1.5× `Rc`; decrement-with-buffering ≤ 2× `Rc` drop. Collector: buffer purge for 100k acyclic drops ≤ 5 ms; 10k-SCC batch collect ≤ 50 ms release. The headline comparisons vs deep-CoW (crossowner comparator): repeated lookup of a stored callable — O(1) vs per-lookup copy; shared callable DAG (2^k paths, k≤16) — linear in nodes vs path-multiplied; 100k callables define+lookup; large macro-produced bodies (store/lookup cost independent of body size vs O(body)). Breach = stop-and-report.

## 15. Rollback / edit-forward plan

Edit-forward only (no git reset; nothing committed; Stages B/C interleaved by design). Order inside the foundational PR: (1) land `cc` module + traces compile-clean and UNCALLED (Stage-A discipline); (2) swap `Rc<Defs>`→`Cc<Defs>`, `Rc<CapturedEnv>`→`Cc<CapturedEnv>`, atom cells, cap-inner enum — behavior-neutral while `collect_cycles` is never called; (3) delete `mod own` + witness plumbing + flags; revert `Env::get`/`local_bindings` to infallible; (4) enable safepoint collection; (5) convert RC-mechanism tests to collector tests (leak probes become: collect-then-assert-dead); (6) full gates incl. the production leak-probe (both P1s must reclaim POST-collect) and Sol's Review-2 §18 regression list re-expressed for the collector.

## 16. PR sequencing recommendation

(1) cc-spike (this plan) → approval gate on results. (2) FOUNDATIONAL COLLECTOR PR (the §15 sequence) — before B3 and before any capability-ownership work, exactly as Sol §18.8 requires; it subsumes what remains of PR-1b.0's mechanism layer while PR-1b.0's semantic layer (Stage B + Stage-C semantics) rides along unchanged. (3) Stage D′ becomes small: cap inners join the traceable enum (edges, not ownership mechanics). (4) Stage E advisory-field work as planned. (5) PR-1b (B3) after the collector lands — its handler-mediated injection is then safe by construction. BEAM-style process heaps stay the recorded destination; the `Trace` inventory, safepoints, durable-leaf partition, and explicit lifecycle rules all carry forward to it.

## 17. Decisions requiring Louis's approval

1. Confirm the pivot: RC + cycle collection; keep Stage B; partial Stage C salvage per §2/§3.
2. Confirm stopping Stage D/E under Graph 4 and declining the identity split (supersedes the prior reconciliation's §9).
3. Approve an in-tree, closely audited custom `Cc` core (reference algorithm Bacon–Rajan; no dependency on the unmaintained crate), spike-first.
4. Approve the §4 participation boundary (Defs, callables/captures, atoms, known cap inners; conservative host leaves).
5. Choose host-edge policy §8(a) now with §8(b) as parked opt-in.
6. Approve collector-timed cleanup for cyclic resources with explicit close/revoke for timing-sensitive authority (§9).
7. Approve the separate foundational collector PR positioned before B3/Stage-D (§16).
8. Keep BEAM process heaps as destination, not implementation.
