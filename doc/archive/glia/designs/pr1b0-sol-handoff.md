# PR-1b.0 Graph 4 — Sol adversarial-review handoff

Prepared 2026-08-02. No code or docs modified. Nothing committed.

## 1. Handoff summary

Sol reviews **only** the PR-1b.0 ownership/lifetime design ("Graph 4: resting-weak / escaping-strong"): `Val::Fn`/`Val::Macro` carry a per-value `OwnerRef { Strong(Rc<Defs>) | Weak(Weak<Defs>) }`; `Defs::define` stores copies whose **self-referencing** owner refs are downgraded to weak (container walk; stops at atoms/capabilities); `lookup`/`local_bindings` upgrade on read; escaped callables are what keep a module's `Defs` alive; ordinary maps carry module lifetime with no hidden handles. The surrounding model (lexical/ownership split, late binding, top-level-only `def`, shared prelude owner, `local_bindings` exports) is approved and is context, not target.

Building the phase model for this handoff surfaced **two defects in the design as previously written**, folded in below so Sol reviews the corrected shape and adjudicates the残 open hole:

- **F1 (fixed in the spec Sol receives):** the downgrade walk must weaken only refs where `Rc::ptr_eq(owner, self)` — the *storing* owner. Unscoped downgrade would weaken a nested imported module's callables when its map is `def`'d into the importer's module, dropping the inner module's `Defs` and breaking later calls (attack case 20/A8 would have caught it).
- **F2 (open — Sol must rule):** `defcap` interns a capability whose `GliaCapInner.methods` hold closures. The cap's `inner` is a shared `Rc<dyn Any>` the walk cannot rewrite: strong method-owners cycle through the opaque cap (routine, since `defcap` interns by design); weak method-owners break exported caps (nothing strong survives export). Candidate resolutions are listed in the prompt (§I of ambiguities); this is the same shared-identity class as atoms but, unlike atoms, `defcap` is routine.

The standing design rule is itself in play: Graph 4's ownership *field* is structural, but ownership *strength* is maintained by position-dependent rewriting — arguably the "context-dependent rewriting" the rule names. Sol must rule on the spirit question explicitly.

## 2. Exact copy-paste Sol review prompt

```
You are Sol, performing an adversarial design review. Target: the PR-1b.0
"Graph 4" ownership/lifetime design for Glia definition ownership, in the
wetware/ww repository, branch glia-control-extraction (PR-1 implemented,
uncommitted; PR-1b.0 not yet implemented).

Read first:
- .context/pr1b-definition-ownership.md
- .context/pr1b0-ownership-resolution.md
- .context/pr1b0-sol-handoff.md   (this handoff; includes the corrected
  downgrade-scoping rule F1 and the open defcap hole F2)
Then inspect at minimum:
- crates/glia/src/eval.rs   — Env (~lines 40-250), set_root and its callers,
  snapshot/filter_to/capture_closure/for_call (~150-240), compute_cap_status,
  is_authority_free (walk precedent, ~340-400)
- crates/glia/src/lib.rs    — Val enum, Fn/Macro variants, PartialEq/Hash
  (env-pointer identity), GliaCapInner/HandledCapInner/AttenuatedCapInner,
  CapHandle
- crates/glia/src/valmap.rs — current map representation
- crates/glia/src/prelude.glia and std/lib/ww/test.glia (ww#574 comment)
- doc/designs/value-contract.md §2-§5, §11 (PR-2 collections, PR-4 callables)

DESIGN UNDER REVIEW (normative statement):
1. Env { lexical frames, defs: Rc<Defs>, handler_stack, defining: bool }.
2. Defs { bindings: RefCell<HashMap<String, Binding>>, inherited:
   Option<Rc<Defs>> }; Binding { value: Val, has_callables: bool }.
3. Val::Fn / Val::Macro gain a per-value field
   OwnerRef { Strong(Rc<Defs>) | Weak(std::rc::Weak<Defs>) }; the shared
   Rc<Env> captured-lexical field is unchanged and remains the identity/
   equality bearer (PR-4 owns final identity).
4. Defs::define(name, val): stores a copy of val in which every embedded
   callable whose OwnerRef is Strong(o) WITH Rc::ptr_eq(o, self) is
   downgraded to Weak (corrected rule F1 — foreign owners are never
   touched). The rewrite walks List/Vector/Map/Set by rebuilding them with
   per-Val copies; it stops at Atom and Cap (shared identity, opaque).
5. Defs::lookup / Defs::local_bindings: return copies in which embedded
   callables whose OwnerRef is Weak(self) are upgraded to Strong(self);
   gated by Binding.has_callables. lookup walks own bindings then the
   inherited chain read-only.
6. Module exports = local_bindings() at import completion, an ordinary
   Val::Map. No ModuleValue, no hidden owner handle, no evaluated-module
   cache; modules instantiate per import; nested module Defs inherit the
   shared prelude Defs, never the parent module.
7. Escaped callables (via lookup, exports, or fresh construction during
   evaluation, which yields Strong) are the sole strong owners of a module
   Defs after import returns.

STANDING RULE (apply aggressively): "When semantic ownership is
reconstructed through logging, tags, side tables, phase-sensitive
bookkeeping, or context-dependent rewriting, stop and ask whether the
runtime should represent that ownership structurally." Rule explicitly on
whether Graph 4's position-dependent strength rewriting violates the
spirit of this rule or is a sound structural design.

ANSWER ALL OF THE FOLLOWING EXPLICITLY.

A. Routine cycle elimination
A1 Does any ordinary top-level function create a strong cycle under any
   path? A2 Recursive functions? A3 Mutual recursion? A4 Can a macro cycle
   through its captured owner? A5 A callable nested in a stored
   map/vector/list? A6 Aliases/duplicate references? A7 A callable captured
   inside another callable? A8 An imported module map stored as a
   definition (verify fix F1 suffices)? Also rule on F2: defcap interning
   a cap whose GliaCapInner methods reference the same owner.

B. Weak-upgrade soundness
B1 Enumerate every upgrade site. B2 Prove upgrade cannot fail during
   legitimate execution, or B3 give a minimal counterexample. B4 Specify
   the failure lane if it can fail (internal fault / catchable exception /
   assertion / silent corruption) — the design claims failure is
   unreachable; test that claim. B5 Can cancellation or async import
   suspension invalidate an owner mid-use? B6 Can a weak callable remain
   reachable after its owner drops? B7 Can atoms or capabilities retain a
   weak callable beyond the owner's lifetime (note: only self-downgrade
   produces weak refs, and the walk stops at atoms/caps — verify that
   stored-weak values genuinely cannot enter atoms/caps)?

C. Escaping-strong semantics — produce a COMPLETE boundary inventory (not
   examples): define "escape" precisely; classify lookup, argument passing,
   returning a callable from a function, storing in a lexical frame,
   placing in an atom, sending through a capability, returning a container
   holding a callable; state for each whether the value is guaranteed
   Strong; identify any path where a Weak-owner callable crosses a
   boundary without upgrade (candidate: values reaching natives or
   HandledCapInner handlers; candidate: with-effect-handler handler values
   resolved from Defs).

D. Recursive container rewriting
D1 Traversed variants; D2 opaque stopping points; D3 why atoms/caps are
   safe to stop at (or aren't — see F2); D4 cycles through atoms; D5 very
   deep containers; D6 stack behavior of the walk (compare
   is_authority_free's explicit-visiting-stack precedent; the evaluator
   just had a stack-budget regression fixed — treat stack as scarce);
   D7 time complexity of define/lookup/local_bindings with concrete worst
   cases; D8 does lookup become O(size of value) and is has_callables
   gating sound (can a binding's flag go stale)? D9 does repeated export
   clone nested structures; D10 does structural sharing survive; D11 are
   maps/sets rehashed when values are rebuilt (inspect valmap.rs); D12 can
   callables be map KEYS today or under value-contract §5 (identity-keyed),
   and does the rewrite preserve their hash/equality (env-pointer) exactly;
   D13 compatibility with PR-2 persistent collections (im-rc CHAMP,
   u64-bucket key engine — flag incompatibilities, do not design PR-2);
   D14 does the walk preserve sharing between aliases; D15 can it duplicate
   atoms/caps/closures/native objects.

E. Callable identity and equality
E1 Does strength conversion allocate a new callable value, and what is
   shared vs copied? E2 Which field bears identity today (lib.rs PartialEq:
   Rc::ptr_eq on env)? E3 Is it preserved exactly through
   downgrade/upgrade? E4 Can repeated lookup yield language-non-identical
   callables? E5 Does aliasing stay stable? Pin: (def f (fn [] 1)) (= f f);
   (def g f) (= f g); (= (:f module) (:f module)). State whether Graph 4
   changes ANY current equality behavior before PR-4.

F. Closure semantics
F1 Lexical locals still snapshot-captured? F2 Only top-level names
   late-bound? F3 Does strength conversion disturb captured lexical envs?
   F4 Named recursion via late lookup? F5 Mutual recursion? F6 Redefinition
   visibility exactly as intended? F7 Closure returned from a closure
   retains all needed owners (including a closure over ANOTHER module's
   callables)? F8 Closure in nested immutable structure survives? F9
   Closure extracted from an atom survives (atoms hold Strong copies —
   verify)? F10 Closure returned by a capability survives?

G. Module lifetime — validate each, stating what strongly owns the module
   Defs at the end:
   (def imported (import "m")) (def f (imported :f)) ;drop imported; (f)
   (def f ((import "m") :f)) (f)
   (def registry {:f ((import "m") :f)}) ((registry :f))
   (def a (atom ((import "m") :f))) (@a … call it)

H. Prelude sharing
H1 Actually immutable? H2 Exact mutating APIs of Defs; H3 can any code
   retain a writable handle to the prelude Defs; H4 can macro expansion
   mutate it; H5 can def target the inherited owner accidentally; H6 tests/
   embedders; H7 is crate-private discipline enough; H8 choose the smallest
   robust mechanism: frozen bit / sealed read-only type / MutableDefs+
   FrozenDefs / nothing.

I. Top-level-only definition gate
I1 Is Env.defining sufficient? I2 Can macros retain/re-enter definition
   privilege? I3 Top-level macro expanding to def (must work)? I4 Function
   invoked during module init calling def (must raise)? I5 Define
   "top-level" exactly (syntactic / dynamic / evaluation-context); the
   design intends: evaluation-context — true for module/REPL form
   evaluation including re-evaluation of top-level macro expansions, false
   inside invoke_fn — verify that is implementable and unambiguous with
   eval.rs's expansion re-entry (eval.rs Expr::Call macro path). I6 Can
   async suspension preserve the wrong flag? I7 Can natives invoke
   evaluation with definition privilege (NoopDispatch, HandledCapInner
   handlers)? I8 Analysis-time vs runtime detection (staging model says
   runtime)? I9 Is glia.error/def-not-top-level catchable everywhere it
   can fire? I10 Interaction with the deferred macro-staging bug
   (doc: .context/pr1-sol-reconciliation-v2.md §6).

ALTERNATIVES — compare Graph 4 against ALL of these on: conceptual
simplicity, runtime invariants, implementation size, hidden state, cycle
behavior, export-map behavior, ordinary-value compatibility, PR-2
collections, PR-4 callables, WASM, debugging, failure modes. Do not prefer
an alternative for familiarity; require a concrete reduction in invariants
or failure surface.
  Alt1 Strong owner + explicit cycle breaking (teardown clears bindings /
       arena / custom drop).
  Alt2 Weak owner + external strong handle (hidden map metadata, wrapper,
       registry, or container-carried handle) — does it violate the
       ordinary-map decision?
  Alt3 FnCode/FnValue split (code: Rc<FnCode>; handle carries owner
       strength) — can strength change without identity churn, and how
       does it differ materially from the per-Val OwnerRef already
       proposed?
  Alt4 Owner arena / generational handles (IDs into runtime-owned arena) —
       lifecycle, authority, global state, stale handles, WASM.
  Alt5 Accept process/runtime-lifetime owners (never reclaim module Defs) —
       is the memory model acceptable and materially simpler, given
       per-import instantiation?
  Alt6 GC-managed environment graph — acknowledge only; do not recommend
       unless all non-GC options fail.

ATTACK CASES — propose as tests or minimal evaluations; state expected
outcome under the corrected design:
 1 plain top-level closure, drop module map, call it
 2 named recursive fn, drop map, call
 3 mutual recursion, extract only one, drop map, call
 4 closure nested five levels in maps/vectors/lists, drop map, call
 5 two aliases to one callable — identity + lifetime
 6 container repeating the same callable 1,000 times — define/export cost
 7 large container, one callable leaf — has_callables gating cost
 8 callable inside an atom stored in Defs — lifetime + cycle statement
 9 atom containing a callable that references its own owner — accepted
   cycle class: confirm bounded/documented
10 callable returned from a capability — survives?
11 callable passed through a capability and returned later — survives?
12 exported macro used after map drop
13 top-level macro expanding to def — allowed
14 function called during module init attempting def — raises catchable
15 prelude shadowing without prelude mutation
16 weak/strong-count lifetime probe (owner freed at last escapee drop)
17 repeated-lookup identity probe
18 deep-container stack-safety probe (define walk + eval)
19 WASM execution path (wasm32-wasip2 harness)
20 nested + diamond imports, including a nested module map def'd into the
   parent module (F1 regression case)

VERDICT — exactly one of: ACCEPT / ACCEPT WITH REQUIRED CHANGES / REJECT.

Return ONLY, in order:
 1 Verdict
 2 Executive rationale
 3 Broken invariants ordered P0/P1/P2
 4 Ownership-graph validation (per the 8 phases in the handoff diagram)
 5 Weak-upgrade analysis
 6 Escape-boundary audit (complete inventory)
 7 Recursive-walk complexity & correctness
 8 Callable identity analysis
 9 Module-lifetime validation (the four G sequences)
10 Prelude immutability verdict (H8 choice)
11 Top-level definition-gate verdict (exact "top-level" definition)
12 Comparison with alternatives (Alt1-Alt6)
13 Required design changes
14 Required tests
15 Drift findings, each classified REQUIRED CONSEQUENCE /
   ADJACENT FIX — APPROVAL REQUIRED / DRIFT — DO NOT IMPLEMENT
16 Final implementation recommendation

Every rejection or required change must cite file paths, symbols, and a
minimal counterexample. Excluded from scope (may be flagged, must not be
designed): macro-staging implementation, explicit export syntax, namespace
aliases/refers, callable identity redesign beyond preserving current
behavior, PR-2 collection semantics, printer/reader redesign, full GC,
unrelated refactors.
```

## 3. Supporting ownership diagram

Edge legend: `══>` strong (`Rc`), `--->` weak (`Weak`), `···>` opaque (`Rc<dyn Any>`, not walkable).

```
                    thread-local prelude memo ══> P:Defs(prelude)
   session Env.defs ══> R:Defs(REPL) ══inherited══> P
   eval_import env.defs ══> M:Defs(module) ══inherited══> P   (never → parent module)

   STORED position (inside M.bindings):
       M ══bindings══> Fn{owner ---> M}            ← self-ref downgraded (F1: only if ptr_eq)
       M ══bindings══> Map ══> Fn{owner ---> M}    ← rebuilt copies, self-refs weak
       M ══bindings══> Map ══> Fn{owner ══> M2}    ← FOREIGN owner stays strong (F1)
       M ══bindings══> Atom ···> Fn{owner ══> M}   ← walk stops: accepted cycle class
       M ══bindings══> Cap  ···> methods ══> Fn{owner ? M}   ← F2 OPEN HOLE (defcap)

   ESCAPED position (lookup / export / fresh construction):
       Fn{owner ══> M},  env: Rc<Env>(lexical copies only — never holds Defs)
```

Phase table — what strongly owns each `Defs`:

| Phase | Strong owners of M (module Defs) |
|---|---|
| 1 init (evaluating forms) | `eval_import`'s `module_env.defs`; transient freshly-constructed callables |
| 2 callable stored | same (stored copies are weak — no self-cycle) |
| 3 export-map construction | `module_env` + each upgraded callable inside the map |
| 4 callable extracted | map's copies + extracted copy (importer env holds the map) |
| 5 map dropped, callable kept | the extracted callable(s) alone |
| 6 all escapees dropped | none → M freed; its weak self-refs die with it |
| 7 REPL | session `Env.defs` holds R for session lifetime; escapees may outlive it |
| 8 nested import | M2 owned by its escaped callables, incl. those inside maps stored in M (foreign-strong per F1); P owned by the memo + every inheriting Defs — process-lived |

## 4. Minimal inspection set for Sol

`.context/pr1b-definition-ownership.md`; `.context/pr1b0-ownership-resolution.md`; this handoff. Code: `crates/glia/src/eval.rs` (Env :40-250, capture fns :150-240, `set_root` callers :928/:1342/:2189/:2762/:2833, `is_authority_free` :340-400, macro-expansion re-entry in `Expr::Call`), `crates/glia/src/lib.rs` (Val + Fn/Macro variants, PartialEq/Hash env-pointer identity, `GliaCapInner`/`HandledCapInner`/`CapHandle`), `crates/glia/src/valmap.rs`, `crates/glia/src/prelude.glia`, `std/lib/ww/test.glia` (ww#574), `doc/designs/value-contract.md` §2-§5/§11.

## 5. Ambiguities that could prevent a fair review

1. **F2 — `defcap` ownership is unresolved by design**: cap `inner` is shared/opaque; strong method-owners = routine cycle (defcap interns by design), weak = broken exported caps. Candidate rulings for Sol: (a) treat `defcap`-in-own-module as the accepted atom-class cycle (bounded, documented); (b) per-`Val` owner handle on `CapHandle` mirroring `OwnerRef` (extends the mechanism to caps); (c) forbid method bodies from referencing module top-level names (analyzer-ish — likely excluded); (d) `defcap` methods capture owner-free by construction. The handoff does not pre-select.
2. **Rule-spirit tension is real, not rhetorical**: the strength rewrite is position-dependent bookkeeping; the ownership *field* is structural. Alt3 (FnCode/FnValue) is the closest "fully structural" competitor and materially overlaps the per-Val `OwnerRef` — Sol should say whether they differ enough to matter.
3. **`has_callables` staleness**: the flag is set at define-time; atoms inside a binding can gain callables later (mutation) without re-flagging — read-path upgrade would skip them (they hold Strong copies anyway per the atom rule, so likely benign — Sol verify, D8).
4. Prior docs (`pr1b0-ownership-resolution.md` §2/§6) state the *unscoped* downgrade rule; **F1's ptr_eq scoping in this handoff supersedes it**. Sol should review the corrected rule and flag anywhere the older text leaks assumptions.
5. PR-1b.0 is unimplemented — Sol reviews design + current-code substrate, not a diff; attack cases are proposals, not runnable tests.
