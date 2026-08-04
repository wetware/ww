# Definition ownership — foundational design comparison (design only; no code/docs edited)

Trigger: the def-log proposal reconstructed ownership from evaluation history — the standing rule fires. Guiding invariant adopted: **every top-level definition has an explicit owning definition context; lookup may consult inherited contexts; ownership is never reconstructed from history.**

## 1. Current Env and definition-ownership diagnosis

`Env { frames: Vec<Frame>, handler_stack, root_frame_is_lexical }` (`eval.rs:48`) — a flat frame vector, no parent links. `set` → newest frame; `set_root` → `frames[0]` (all of `def`/`defn`/`defmacro`/`defcap`: :928, :1342, :2189, :2762, :2833); lookup innermost-out. Closure capture is **by-copy at definition time**: `capture_closure` (slim: free vars + all macros, :196-215), `snapshot` (raw path, full merge, :154-168), `filter_to` (:172). `Env::for_call` (:224-240) **copies the captured frame into a fresh env per call**. Macro definition envs are snapshots. REPL/toplevel = one long-lived `Env` per session; module eval (current) = fresh `Env`.

Answers: (1) one `Env` currently represents **four** concepts — lexical scope, top-level namespace, module ownership boundary, and closure capture store; (2) yes, fully complected; (3) "root" = `frames[0]` of whichever env is executing — for a function call that is a **per-call copy** of the captured snapshot; (4) root is local to each env instance; captured envs make it effectively a disconnected archipelago of stale copies; (5) `set_root` callers plus every capture/`for_call` path assume the flat model; `compute_cap_status`/`bindings()` read it; (6/7) covered in §3 (parents don't create cycles by themselves; live owner references in stored closures do — same class as today's accepted atom cycles, see §3); (8) pinned-accidental behavior: `eval.rs:4487-4541` slim-capture count tests (pin copy-capture), the ww#574 comment + `*tests*` atom workaround (pins def-inside-fn evaporation), caps "idempotent re-import" (already sentenced).

**Empirical diagnosis (probed against the live tree, prelude loaded):**
- `(defn fact [n] (if (= n 1) 1 (* n (fact (- n 1))))) (fact 5)` → **type error** — named self-recursion is *broken*; only `recur` works.
- `(def a 1) (defn g [] a) (def a 2) (g)` → `1` — snapshot capture; redefinition invisible to existing closures.
- `(defn f [] (def hidden 42)) (f) hidden` → unbound — def-inside-fn writes a per-call copy and evaporates (the ww#574 atom workaround exists precisely for this).

These are not intended semantics; they are the flat model's shadow. A structural owner is a language fix, not architecture hygiene.

## 2. Deferred macro-staging design note (proposed content)

**Proposed path: `doc/designs/macro-staging.md`** — full draft content in §10 below. Covers: the standard rule (raw operands → expansion → analyze/evaluate); the confirmed divergence — `analyze_list`'s generic arm (`expr.rs:390-399`) eagerly analyzes operands before the head's macro-ness (a runtime env fact) is known, while the legacy raw path defers; reproducer `(defmacro m [x] 1)` + `(m (let [y] y))` (fails analyzed, succeeds raw); why this is distinct from `try` (the `try` case involves genuinely malformed *expression* operands rejected pre-expansion — correct under the locked model) and why `try` must never be special-cased; smallest principled directions (defer operand analysis for non-special-form heads until macro resolution; or re-analyze from `raw_args` on macro hit); tests that expose the divergence today. Not fixed in this workstream.

## 3. Structural-model comparison

**A — parent-linked Env with owned roots.** Every env owns a root + `parent: Option<Rc<Env>>`. Fatal conceptual flaw: it re-complects "being an environment" with "owning definitions" — *every* env (including per-call fn envs) gets an owned root, so `def` inside a function writes an ephemeral owner again unless fn envs are special-cased to *reference* their definer's owner instead of owning — which is exactly Model C's separation. Parent-chain lookup and REPL work; recursion fixed only if capture holds parent links (late binding). Verdict: C's insight expressed less cleanly; superseded by C.

**B — explicit Namespace objects** (named namespaces, referred lists, aliasing). Semantically closest to Clojure (Vars in namespaces); everything C gives plus naming/referring machinery and public language concepts (namespace identity, `*ns*`-like questions) that nothing yet needs. Verdict: premature; C is its skeleton and grows into it additively.

**C — definition-owner object layered onto Env (recommended).**
```rust
pub(crate) struct Defs {                       // the owning definition context
    bindings: RefCell<Frame>,
    inherited: Option<Rc<Defs>>,               // module → prelude → (none)
}
pub struct Env {
    frames: Vec<Frame>,                        // lexical scopes ONLY
    defs: Rc<Defs>,                            // current definition owner
    handler_stack: HandlerStack,
}
```
Lookup: lexical frames → `defs` chain. `def`/`defn`/`defmacro`/`defcap` → `defs.bindings` (current owner — structural, no history). Capture: lexical free vars by value (slim, as today) **plus the owner by `Rc`** — top-level names resolve *late* through the live owner. `for_call` copies lexical bindings only and carries the closure's captured `defs`. Exports = snapshot of the module owner's `bindings` at import completion — *the export map is the module's owned definition context*, no logging.
What this buys, all structurally: named self- and mutual recursion **work**; REPL redefinition visible to existing closures (Clojure-Var-like); def-inside-fn persists to the **defining** owner; true layered shadowing (module owner entry shadows, prelude untouched); prelude evaluated **once per runtime** into a shared immutable owner (perf win over per-import re-evaluation); module exports fall out of ownership. Costs: evaluator-core churn (§7); a real-but-precedented cycle class — a closure stored in its own owner references that owner (`Rc` cycle → module instances/REPL envs are not reclaimed while self-referential); **precedent**: Glia already accepts exactly this via atoms (`(reset! a (fn [] a))` — the `is_authority_free` comment documents value-graph cycles). Documented; `Weak`/arena/GC noted as a future track, not now.

**D — def-log (fallback only).** Fails the invariant by construction: ownership is *reconstructed from write history*, phase-sensitive (arm/disarm ordering is load-bearing), represents "what happened" not "who owns"; leaves recursion/redefinition/def-inside-fn broken; would be disposable the day ownership becomes structural. Rejected unless C hits a concrete conflict — none found.

**E — snapshot/diff.** Rejected explicitly: ownership inferred from value inequality (heuristic identity; misses redefine-to-equal), doubly historical.

## 4. Lexical scope vs definition ownership

They are different concepts and stay separate: **frames = lexical; `Defs` = ownership.** (1) A new lexical scope never owns top-level definitions. (2) `def` inside a function writes the function's **defining owner** (carried lexically in the capture) — not the lexical frame, not the caller's env. (3) Clojure: `def` interns a Var in the *dynamic* current namespace `*ns*`; lookup is late through Vars. (4) Glia deliberately diverges to **lexical ownership**: no ambient dynamic namespace state — the owner travels with the code that was defined in it, which is the possession principle applied to definitions (documented divergence). (5/6) Closures and macros capture the same two things: lexical values by copy, owner by reference. (7) `defcap` needs no special ownership — authority is possession of the produced value; interning it is like any def. (8) REPL state = an owner (the session's `Defs`) + a thin lexical env; kernel/cli sessions hold both via their existing `Env`.

No new complection: the pair (copy-captured lexicals, referenced owner) is exactly the two concepts, one mechanism each.

## 5. Proposed language semantics (with the required examples)

- Module top-level defs intern in the module owner; prelude inherited (shared, immutable); **`(def try 42)` → export contains `:try 42`, prelude owner untouched, module code after the def sees 42, closures defined before it captured nothing (late lookup finds 42 too — late binding applies; deliberate, Clojure-like)**.
- `(def x 1) (def x 2)` → owner holds final value → export `{:x 2}`.
- **`(defn f [] (def x 1))` → calling `f` interns `x` in f's *defining* owner** (module/REPL). If called during module init, `x` exports; if called post-import, the export snapshot is unchanged but module closures see `x`. Rationale: def is namespace intern, lexically owned; the current evaporation is the accident.
- Recursive/mutually recursive defns work by late owner lookup. Conditional top-level def (`(when c (def x 1))`) interns on execution. Macro defs intern like fns and export. `defcap` interns and exports its cap. Imported module maps are ordinary values; nested lexical scopes never intern. REPL defs intern in the session owner; redefinition is visible to previously defined closures (pinned by test — replaces the accidental snapshot pins). Separate module instances = separate owners inheriting the shared prelude owner. Closures/macros returned from modules keep lexical copies + their module owner (self-contained after the import future is gone).

## 6. Repository dependency audit

| Dependency | Evidence | Classification |
|---|---|---|
| Root-writing `def` persisting across REPL forms | kernel/cli session envs | **intended** — preserved (owner = session) |
| def-inside-fn evaporating per call | `for_call` copy; ww#574 comment; probe | **accidental** (the comment itself calls the surrounding behavior a bug workaround) — replaced by defining-owner intern |
| Snapshot capture / redefinition invisibility | slim-capture tests `eval.rs:4487-4541`; probe | **implementation artifact** pinned by tests — tests updated deliberately |
| Named self-recursion broken | probe; only `recur` tests exist | **accidental gap** — fixed |
| Prelude overwrite within an env | same-frame overwrite | **artifact** — becomes true shadowing |
| Module maps containing prelude names | caps `bindings()` | **accidental** (already sentenced in export-boundary doc) |
| Env identity in `Val::Fn` equality (`Rc::ptr_eq(env)`) | `lib.rs` PartialEq; pinned test at eval.rs~4312 (transitional per value-contract §3) | **explicitly unfrozen until PR-4** — capture struct change must keep *some* stable Rc for identity; use the captured lexical env Rc (unchanged shape) |
| `compute_cap_status` walking bindings | eval.rs:390/564 | intended (authority accounting) — must walk owner chain too (excluding shared authority-free prelude) |
| Macro captured envs (expansion-time resolution incl. `try-catches`) | capture_closure macro rule | intended — owner reference makes it *more* robust (macros resolve through live owner) |

## 7. Migration / scope estimates

**Model C (recommended), as its own prerequisite PR ("PR-1b.0 — definition ownership"):** crates: glia only (embedder API `Env::new()` unchanged — it mints a fresh owner inheriting a lazily-built shared prelude owner is *optional*; simplest: owner built per `Env::new` as today, prelude sharing as a follow-up). Files: `eval.rs` (Env internals: `get`/`set_root`/`for_call`/`snapshot`/`filter_to`/`capture_closure`/`bindings`/`compute_cap_status`; ~15 functions), `lib.rs` (none required: `Val::Fn { env: Rc<Env> }` unchanged — the env *contains* the owner). Production diff ≈ **+350/−150**; test migration ≈ 15–25 existing (capture-count pins, redefinition pins) + ~20 new semantic tests (recursion, late binding, def-inside-fn, shadowing); 1 compile-break stage; performance: one extra probe through `Rc<Defs>` per top-level miss, capture gets cheaper (top-level names no longer copied); memory: cycle class as §3 (precedented). PR-1: **no interaction — merges first, unchanged**. PR-1b: **shrinks** — module eval = fresh owner over prelude owner; exports = owner snapshot; def-log never exists.
Model A ≈ C+20% churn with the fn-env special case; Model B ≈ C + namespace registry/API (~2× C, public concepts); D ≈ +45 lines but disposable; E ≈ +30 lines, disposable.

## 8–9. Recommendation

1. **Long-term model: C** — explicit definition owner, lexical/ownership separation, growable into B when naming/referring is actually needed.
2. **Implement now: C**, in full (it is small enough that a reduced interim variant saves nothing).
3. **Yes — a new prerequisite PR-1b.0 before PR-1b**, glia-only.
4. **PR-1 does not wait** — merge as designed; PR-1b.0 stacks on it.
5. **Nothing temporary is built**: def-log and snapshot/diff are cancelled, not deferred.
6. Disposable architecture avoided by construction — the owner is the same object a future namespace system, explicit exports, and REPL tooling would use.

## 10. Proposed documentation updates (not yet applied)

- **New `doc/designs/definition-ownership.md`**: the invariant; the `Defs`/`Env` split; lookup and intern rules; capture = lexical-copies + owner-ref; late binding for top-level names; lexical-owner divergence from Clojure's dynamic `*ns*` (rationale: no ambient state, possession principle); shadowing/redefinition semantics with the §5 examples; export rule ("the export map is the module's owned definition context"); cycle class and precedent (atoms), future Weak/GC note; rejected alternatives (def-log — historical reconstruction; snapshot/diff — heuristic identity; parent-owned envs — re-complection) and why.
- **New `doc/designs/macro-staging.md`** (deferred track, §2 above): rule, reproducer, pipeline divergence table, distinctness from `try`, no-special-casing commitment, candidate fixes (deferred operand analysis / raw re-analysis on macro hit), exposure tests.
- **`doc/architecture.md`** — add to a "Design rules" section: *"When semantic ownership is reconstructed through logging, tags, side tables, or phase-sensitive bookkeeping, stop and evaluate whether the runtime should represent that ownership structurally."* Plus one paragraph linking the two new design docs and noting explicit export syntax as a separate, non-required future track.
- **`doc/designs/value-contract.md`** roadmap table: insert PR-1b.0 row between PR-1 and PR-2/PR-1b.

## 11. Drift report

**REQUIRED CONSEQUENCE**: Model C implementation (PR-1b.0), test updates for the three accidental-behavior families, doc additions above, PR-1b consuming the owner for exports.
**ADJACENT — APPROVAL REQUIRED**: shared once-per-runtime prelude owner (perf; can land inside PR-1b.0 or after); macro-staging fix (separate track, unchanged status).
**DRIFT — DO NOT IMPLEMENT**: namespace naming/aliasing/referring (Model B extras), explicit exports, visibility modifiers, `Weak`/GC work, macro-staging fix in this workstream, `try` special-casing, callable/PR-2/printer work, unrelated cleanup.

## 12. Decisions still requiring approval

1. Model C as the structural model, in a prerequisite **PR-1b.0** (PR-1 merges first; PR-1b rebases on it).
2. **Late binding for top-level names** (closures see redefinitions; fixes recursion) — the headline semantic change vs today's snapshot capture.
3. **def-inside-fn interns in the lexical defining owner** (vs alternative: restrict `def` to top level and error elsewhere — clean but forbids conditional/staged definition patterns; not recommended).
4. Accept the closure↔owner `Rc` cycle class with the existing atom-cycle precedent (leak-on-self-reference, documented; Weak/GC deferred).
5. Shared prelude owner (evaluate prelude once per runtime) — now or follow-up.
6. Capture rule for `Val::Fn` identity remains the captured-env `Rc` (PR-4 owns the final identity story).
