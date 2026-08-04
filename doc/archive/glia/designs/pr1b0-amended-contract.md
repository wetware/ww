# PR-1b.0 — amended Graph 4 implementation contract (post-Sol)

Status: Sol verdict **ACCEPT WITH REQUIRED CHANGES** (2026-08-03) digested and verified; all 14 required changes folded in. No code edited; nothing committed. Awaiting go-ahead.

## Verification of Sol's findings against the tree

Every load-bearing claim checks out; nothing is contested:

- **P0-1 (captured-env cycle)** — confirmed. My resolution doc marked `Val::Fn.env` "lexical-only" as a comment, not a structure; with `Env { defs: Rc<Defs>, … }` captured as `Rc<Env>`, every stored function cycles regardless of a weak `OwnerRef`. Sol's `CapturedEnv` split makes the intent structural.
- **P0-2 (cycle via captured lexical values)** — confirmed; genuine miss in my design. `(def f (let [g (fn [] 1)] (fn [] (g))))`: `f`'s own ref rests weak, but its shared capture holds `g{Strong(M)}` → `M → f → capture → g → M`.
- **P0-3 (defcap)** — matches handoff F2; Sol ruled option (b): `Option<OwnerRef>` on `CapHandle`, methods rested weak inside the sealed inner, outer cap carries the witness.
- **P1-1** — confirmed: `is_authority_free`'s `visiting` vec guards atom cycles but recursion is Rust-stack-bound; both transforms must be iterative (and we *just* fought a stack regression in PR-1).
- **P1-2 (stale `is_cap_free`)** — verified live: `compute_cap_status` runs only at construction (eval.rs:1128/:1335/:2264/:2755) and `is_authority_free` trusts the cached flag (eval.rs:556). Late binding breaks it; authority accounting must become live/finalized-owner based.
- **P1-3/4/5** — accepted: frozen bit on the prelude owner (`freeze()`, one-way; post-freeze `define` = internal fault); `defining` as an environment invariant, never an awaited toggle; `IMPORT_CACHE` removal rides PR-1b as already planned.
- **P2-4 (map-call syntax)** — verified: analyzed call heads must be symbols (`expr.rs:362`); `(t :assert= …)`-style doc comments in std/lib are aspirational. All ownership tests use `(get m :f)`. Adding map-call syntax stays DRIFT.

## Amended design deltas (Sol §13, restated as the build list)

1. **`CapturedEnv` split**: `Val::Fn`/`Val::Macro` capture `Rc<CapturedEnv>` (lexical frames only). Runtime `Env { frames, defs: Rc<Defs>, handler_stack, defining }` is never captured. Equality/hash keep the captured-env pointer (PR-4 untouched).
2. **Centralized barrier**: `rest_for(owner, value)` / `escape_with(owner, value)` are the only places `OwnerRef` strength changes; both **iterative** (explicit work stack, post-order rebuild) over List/Vector/Map keys+values/Set + callable owner fields + `CapHandle` owner field; opaque at atom contents, cap inners, native state, scalars.
3. **Capture normalization**: building a `CapturedEnv` rests self-owned nested values; `for_call` escapes them with the enclosing callable's strong owner before body execution.
4. **Owned caps**: `CapHandle` gains `Option<OwnerRef>`; `defcap` rests self-owned methods before sealing `GliaCapInner` and the cap carries `Strong(current_defs)`; cap dispatch escapes methods via the cap's witness; evaluator-local attenuation transfers the owner when wrapping an owned cap; known `HandledCapInner` constructors use owner-free handlers or transfer owners.
5. **Witness-based upgrades**: escapes clone a live `Rc<Defs>` witness — never bare `Weak::upgrade`. An unmatched weak ref at any escape boundary is an **internal fault** (release-checked, not debug-only).
6. **Flag**: `has_resting_owner_refs`, computed as an output of `rest_for` (stable: collections immutable; atoms never receive rested values; cap inners sealed before storage).
7. **Owner invariant broadened**: escaped *owner-bearing values* — functions, macros, evaluator-owned caps — are the sole routine owners of a module `Defs`.
8. **Prelude freeze**: build once per thread, `freeze()`; the only mutating APIs are `define` (own bindings, pre-freeze) and `freeze` itself.
9. **Definition gate**: Sol's exact top-level definition adopted (evaluation-context: direct module/REPL form evaluation, incl. top-level macro *expansions*; `for_call`/`invoke_macro`/cap-method/handler invocation always `defining = false`; violation → catchable `glia.error/def-not-top-level`); enforced at one checked `Env::define` used by all five definition paths (raw/analyzed `def`, raw/analyzed `defmacro`, analyzed `defcap`).
10. **Live authority accounting**: retire construction-time `is_cap_free`/`cap_violation` staleness — cap-status walks live owner chains (and captured lexicals) at check time; the value-contract's cap-status tests flip accordingly (data→cap and cap→data redefinition regressions added).
11. **Docs corrected**: `.context/pr1b0-ownership-resolution.md`'s unscoped downgrade superseded (F1 ptr_eq scoping), captured-storage resting, cap-owner rule, broadened owner invariant.

## Test matrix

Sol's 20 tests + 13 release gates adopted verbatim (contract keeps his numbering; all executable forms use `get`). Notables beyond the prior matrix: literal top-level fn has no `CapturedEnv → Defs` edge; same-owner capture doesn't leak; foreign capture retains foreign owner; exported/stored defcap lifetime + recursive method lookup; attenuated owned caps; callable map keys through both transforms; inherited-owner upgrades; freeze rejection; cancellation can't leave `defining` set; double-import distinct identities.

## Sequencing (per Sol §16)

PR-1 merges first, unchanged. PR-1b.0 lands amended Graph 4 in Sol's order: (1) `CapturedEnv` split + centralized iterative barrier → (2) fn/macro owner handling + capture normalization → (3) owned-cap transfer + defcap fix → (4) prelude freeze + definition gate + live cap accounting → (5) full lifetime/identity/constrained-stack/import/WASM matrix. Only then PR-1b consumes `local_bindings`, removes `IMPORT_CACHE`, and builds B3 imports. Original Graph 4 is not implemented verbatim.

## Revised scope estimate

The amendments roughly double PR-1b.0: `CapturedEnv` type + capture/`for_call` rewrite, two iterative transforms, `CapHandle` owner + defcap/attenuation/dispatch paths, live cap-status, freeze + gate. Estimate ≈ **+900/−350 glia-only**, ~35 existing tests updated, ~45 new tests, 1–2 compile stages, embedders still untouched (verified: no external `Val::Fn`/`Macro`/`CapHandle`-field users). Risk concentrates in the barrier and capture normalization — fenced by Sol's release gates.

## Approval items before implementation

1. Adopt this amended contract as the PR-1b.0 implementation target (supersedes the §2/§6 shapes in `pr1b0-ownership-resolution.md`).
2. Sol's ADJACENT items stay parked (host-payload ownership declarations; CHAMP-preserving transform optimizations; macro-staging fix) — confirm.
3. Live cap-status semantics (item 10) is the one user-visible behavior delta beyond the already-approved set: authority-freeness of a closure is now judged at check time against live owners. Confirm.
4. Scope growth acknowledged (~2× the pre-Sol PR-1b.0 estimate) — confirm PR-1b.0 remains a single PR rather than splitting the barrier work.
