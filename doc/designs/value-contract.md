# Glia Value Contract

Status: PARTIALLY SHIPPED / REMAINDER REJECTED — PR-0 shipped as #632. PR-1
through PR-4 did not ship and were superseded by the August 2026 decision to
archive the Glia language work on `archive/glia-final`. This document is
retained as design history, not an active roadmap.
Scope: the invariants shared by all Glia runtime values, and the staged plan
that makes them true. Individual collection designs (vector, seq API,
strings, laziness, durable encoding) build on this contract and are
specified separately. Supersedes the problem statement in `ipld-hamt.md`.

## 1. Motivation

Glia's collections are growing from one persistent structure (`ValMap`) into
a suite. Representation can be swapped behind seams forever; equality,
hashing, and printing semantics cannot — they leak into stored hashes,
golden tests, and eventually content-addressed encodings. This document
freezes the semantics deliberately, and only the semantics. The eng review
additionally found that most historical value-model pathologies came from
non-values living inside `Val` (evaluator control states, a host transport
record), so the contract includes their removal.

## 2. Value roles

Target shape (after the §11 roadmap): **15 variants in four roles**.

| Role | Variants | Equality | Keyable | Readable | Durable |
|---|---|---|---|---|---|
| **Data** | `Nil Bool Int Float Str Sym Keyword List Vector Map Set Bytes` | structural | ✓ | recursively (§7) | recursively (§9) |
| **Mutable reference** | `Atom` | allocation identity | ✓ (identity) | no | no |
| **Callable** | `Callable` (today: `Fn`/`Macro`/`NativeFn`/`AsyncNativeFn`) | identity (settled in PR-4) | ✓ (identity) | no | no |
| **Capability** | `Cap` | instance identity | ✓ (identity) | no | no |

Removed from `Val` by the roadmap:

- **`Cell`** (PR-0): a passive wasm+grants registration record with no
  consumer of its equality, identity, or key use anywhere in the tree. It
  becomes ordinary data: the `cell` builtin keeps its syntax and early
  validation but returns a tagged map
  `{:ww/type :cell, :wasm <bytes>, :grants {<keyword> <cap>}}`. The
  authority-bearing consumer (`host :listen` / `:listen-stream`) validates
  at activation through one canonical `parse_cell_spec`: required keys
  exactly as above, unknown keys rejected with a structured error, grant
  keys keywords, grants sorted by name before wire encoding (preserving
  today's observable wire order), and full validation strictly **before**
  `runtime.load_request()`. A `cell?` predicate tests the tag. Deliberate
  observable changes: `(type c)` is `:map`; a zero-grant cell-map is
  authority-free (the old unconditional not-authority-free classification
  contradicted the same bytes held as `Val::Bytes`); cell specs are
  inspectable and transformable as ordinary data (activation re-validates,
  so transformed specs cannot smuggle authority); the MCP adapter emits a
  structured JSON object instead of an opaque display string.
- **`Recur` / `Effect` / `Resume`** (PR-1): evaluator control flow, not
  values. Today they can leak into collections (`(loop [] [(recur)])`
  stores a `Recur`), which is why extraction precedes the collections work.
  Control moves to a crate-private `Control` channel
  (`Result<Val, Control>` inside the evaluator); native functions see only
  a narrow public signal type (`Raise(Val) | Resume(Val)`) — they can raise
  errors and propagate resumption but cannot synthesize lexical `Recur`.
  `Dispatch::call` and `error::unwrap_thrown` migrate to the same boundary
  type. Unhandled effects reach embedders as boundary data, not as storable
  values.

After PR-1, every remaining `Val` is a language value: keyability is total,
and `try_hash` (§5) has no rejection arms. It remains the seam where any
future non-keyable variant would be rejected with a structured error.

## 3. Equality (`=`)

Clojure's equality categories:

1. **Data** compares structurally and recursively:
   - `List`/`Vector`: sequential, in order, **across the two types**
     (`(= '(1 2) [1 2])` is true). Requires the hash unification in §4;
     they ship together.
   - `Map`/`Set`: contents, independent of order. Maps equal only maps;
     sets only sets.
   - `Str Sym Keyword Bytes Bool Nil`: by content. `Int` and `Float` never
     equal each other.
2. **Floats are IEEE 754** (exactly Rust `f64 ==`): `NaN` unequal to
   everything including itself; `0.0` equals `-0.0`; ordered comparisons
   involving `NaN` return `false` (today they raise an internal error;
   this contract changes that).
3. **Identity values** compare by identity: `Atom` by allocation, `Cap` by
   instance, callables by identity settled in PR-4 (the current
   captured-env-pointer behavior, pinned by a test at `eval.rs:4312`, is
   transitional and explicitly unfrozen).
4. **Collections short-circuit on shared structure**: wrapper-owned
   equality first checks root identity (O(1)), then falls back to
   len + per-entry lookup. This is the boxed-collection analogue of
   Clojure's identity-then-equiv order, made deliberate and
   toolchain-independent (never delegated to the backing library, whose
   equality is channel-dependent under specialization).

Equality is symmetric and transitive; it is **not reflexive** (`NaN`), and
the implementation accommodates that lawfully (§5) — Rust `impl Eq for Val`
and `impl Eq for ValMap` are removed; there is no hidden key-equivalence
relation anywhere.

### NaN pathology (documented, permitted)

Any hashable value may be a map key or set member, including `NaN` and
composites containing it (`[##NaN]` is equally non-reflexive; the pathology
is recursive and `nan?` cannot guard composites). Such entries cannot be
looked up, removed, or deduplicated; a set may hold several; a map holding
one is not `=` to a separately built equal map. Precision note: this is a
**deliberate departure from Clojure**, not a mirror — Clojure's `equiv` is
identity-first, so the *same boxed* NaN key is findable there; Glia floats
are unboxed, so no NaN is ever findable. `nan?` and `finite?` ship with the
collections PR. Repeated NaN insertion degrades to a linear collision
bucket (O(n²) build); this is bounded by fuel inside cells and documented
as an open exposure for unmetered host-side evaluation (TODOS: collision
cap trigger).

## 4. Hashing

- Law: `(= a b)` implies `hash(a) == hash(b)` for all values.
- `List` and `Vector` hash identically as one ordered sequence under a
  shared kind tag (the current discriminant-prefixed hash violates the law
  once sequential equality lands; corrected in the same change).
- Floats hash by bit pattern after normalizing `-0.0` to `0.0`. `NaN`
  hashes by bits.
- Map and set hashes are order-independent (wrapper-owned XOR of entry
  digests; never the backing library's iteration-order hash).
- Runtime hashing is implementation-private: never serialized, never
  compared across processes, no cross-version guarantee.

## 5. Key engine (lawful; no `Eq` bending)

Persistent maps and sets are backed by
`im_rc::HashMap<u64, Bucket>` behind the `ValMap`/`ValSet` seams:

- The `u64` is the language hash of the key — a **private runtime bucket
  selector only**, computed by `try_hash` before the backing structure is
  touched. The CHAMP sees only lawful integer keys; no Rust trait contract
  is violated and no unsafe cast exists.
- Buckets hold `(key, value)` pairs compared explicitly with language
  equality. NaN queries miss; NaN inserts append; removal of a
  non-reflexive key is a no-op.
- **Original representative retained**: updating an existing equal key
  keeps the stored key and replaces only the value; `conj` of an equal set
  member returns the set unchanged. `(assoc {[1 2] :a} '(1 2) :b)` keeps
  the vector key. (Matches Clojure; the bucket scan makes it free.)
- Wrapper-owned equality (§3.4), hash (§4), and iteration: public iterator
  wrappers only; no backing-library types, no `Deref`, no derived
  collection traits leak. Map merge is an equiv-aware bucket merge; the
  backing library's `union`/`entry` family is never used (its vacant-entry
  path re-looks-up and unwraps — a panic under non-reflexive keys).
- The backing dependency is pinned exactly (`im-rc = "=15.1.0"`, archived
  upstream); upgrades require source review.

## 6. Immutability, purity, iteration

- Data values are deeply immutable; every "mutation" returns a new value;
  builder mutation is never observable. The only mutable holder is `Atom`.
- Collection operations are pure: no effects, no ambient authority, never
  capability calls. Cost is bounded by wasmtime fuel when evaluation runs
  inside a cell; host-side evaluation (CLI shell, init.d) is currently
  unmetered and nothing in this contract may assume otherwise.
- Map/set iteration order is semantically unordered and
  implementation-private. Deterministic ordering exists in exactly two
  places — readable printing (§7) and the durable encoder (§9) — and
  neither is a language-level collection-order or comparison guarantee.

## 7. Printing

Readable printing (`pr` intent; `str` remains print-like for strings):

- **Ordering.** Maps and sets print via a dedicated print-ordering step
  using a **recursive sort key over the full entry**: for every value, the
  key carries variant rank, content bytes (recursively for composites,
  including `Bytes` content), and identity tokens at every nested
  position; map entries sort by (key sort-key, then value sort-key). This
  ordering is private, unversioned, never a public comparison operator,
  and never reused for hashing or durable encoding.
- **Guaranteed domain, stated exactly:** values that are recursively
  ordinary data (all components data, including `Bytes`) print
  **identically across runs and processes**. Values containing any
  identity-bearing element (`Atom`, callable, `Cap`) are print-stable
  **within a process only** until real identity tokens exist (PR-1/PR-4);
  no process-local token is ever described as cross-run deterministic.
- **Escaping and literals.** Strings print with the reader's escape set
  (`\"`, `\\`, `\n`, `\t`) and round-trip. Non-finite floats print
  `##NaN` / `##Inf` / `##-Inf` and the reader accepts these; float parsing
  is restricted to a numeric grammar, so alphabetic spellings (`-inf`,
  `+nan`, `-Infinity`, case variants) become ordinary symbols by
  construction; numeric overflow such as `1e999` still parses to an
  infinity. NaN round-trips modulo payload bits.
- **Recursive `readable(v)`.** A value is readable iff it is data, all its
  components are readable, and symbols/keywords have token-safe spellings
  (a programmatic `Sym("nil")` or a symbol containing whitespace is not
  readable; the printer PR settles their printed form). `Bytes` is
  excluded from readability until a literal is chosen (§9); its interim
  form is `#<bytes N>`, which the reader rejects (replacing today's
  `<N bytes>`, which silently re-reads as symbols).
- **Identity values** print as reader-rejected `#<...>` forms. `Atom`
  prints content-free (`#<atom>`) so mutation cannot destabilize print
  order. Callable print forms (`#<fn ...>`, `#<macro ...>`, native
  variants) are **explicitly unfrozen** until PR-4. Printing never leaks
  authority: a cap prints its name, never its descriptor or payload.

## 8. Nesting and storability

Any value may appear in any collection, including as map key or set member
(identity values key by identity). Cycles are unconstructible among data
values; `Atom` can create reference cycles and is identity-only. After
PR-1 there are no storability exceptions; until then, leaked control states
are inert, never-equal, non-keyable debris, and debug assertions at the
collection seams act as tripwires, not guarantees.

## 9. Durability boundary

Runtime values are never eagerly canonicalized, hashed into CIDs, or
coupled to a durable layout. Canonical encoding is a boundary function,
specified (not implemented) with these requirements: deterministic;
injective over its supported data domain; versioned; independent of
runtime representation and iteration order; explicit about floats (`NaN`
bit policy, signed zero), bytes, symbols, keywords, and map/set ordering;
restricted to durable **data** values via a recursive `durable(v)`
predicate. Live capabilities, callables, atoms, and evaluator machinery
are not durable data; encoding one at a durable boundary is an error
unless a later design introduces a distinct durable description. DAG-CBOR
is a candidate, evaluated in a dedicated boundary-format decision, not
frozen here. The RPC wire (text `eval` today, per-method capnp
marshalling) is a separate surface and may diverge.

## 10. Deliberate departures from Clojure

| Behavior | Clojure | Glia | Rationale |
|---|---|---|---|
| NaN key findability | same boxed NaN findable (identity-first equiv) | never findable (floats unboxed) | no float identity exists; benign, documented |
| Duplicate elements in a source set literal | read-time error | read-time error (existing) | matches; unchanged |
| Set elements that evaluate equal (`#{1 (+ 0 1)}`) | runtime duplicate-key error | dedup at the `ValSet` seam | computed literals should not hard-fail; revisit if it hides AI-authored bugs |
| Duplicate map-literal keys | throws | last-wins (today), except literal `cell :grants` maps, which the reader rejects | undecided; settled with the durable encoder; the grants carve-out survives |
| Laziness | core to seqs | none yet | effects/one-shot-resume interaction needs its own design; eagerness is not a permanent principle |
| String seqability / `Char` | seqable chars | deferred | deliberate Unicode design first; no accidental one-char-string contract |
| `rest`/`cons` cost | lazy/O(1) | eager/O(n) today | representation work, later tranche |

Equal-key representative retention **matches** Clojure (deliberate
alignment, §5).

## 11. Historical roadmap

| PR | Content | Gate |
|---|---|---|
| **PR-0** | Remove `Val::Cell` → tagged map + kernel `CellSpec` via canonical `parse_cell_spec` (validate-before-load, unknown keys rejected, sorted wire grants); `cell?` predicate; migrate both evaluator paths (`eval_cell_expr`, `eval_cell_raw`), kernel consumers and tests; two cap-status tests flip deliberately (zero-grant cell becomes authority-free) | **shipped (#632)** |
| **PR-1** | Control-state extraction (private `Control`; narrow public native signal `Raise/Resume`; `Dispatch::call` + `unwrap_thrown` migration across the four dependent packages); `Cap` + `EffectTarget` encapsulation (opaque identity type, constructor-only minting, kernel accessor/downcast API) | **rejected / archived** |
| **PR-2** | Collections core on the final control-free `Val`: float + sequential equality with unified hashing; `Eq` removals; lawful `u64`-bucket key engine; `ValSet` + `conj`/`disj`/`contains?`/`count`/`empty?` + seam dedup; representative retention; wrapper-owned equality/hash/iterators; `nan?`/`finite?`; deterministic set/map print order + string escaping (needed by the round-trip suite); property + im-pinning tests (NaN duplicate accumulation, no-op removal, drop-count, set-op amplification, retention); NaN-flood stress and hit/miss retention benches; `im-rc = "=15.1.0"`; kernel WASM size measurement | **superseded** |
| **PR-3** | Printer/reader: `##` literals, numeric float grammar, recursive full-entry sort key, token-safety, `Bytes`/`Atom` forms; round-trip tests extended to non-finite floats | **superseded** |
| **PR-4** | **Callable semantics and representation.** First settle the user-visible contract: HOF admission of native callables (today `map`/`filter`/`reduce` reject them via `extract_fn`), `type` results (today four tags), sync/async observability, macro as a semantic kind, callable identity semantics (replacing incidental env-pointer identity with minted tokens), Display/error forms. Representation unification behind one sealed `Callable` is then an implementation consequence, not the premise | **superseded** |

## 12. Historical open questions register

These remained unresolved when the roadmap was superseded:

- PR-4 callable decisions (listed above).
- `Bytes` readable literal and map-literal duplicate-key policy.
- NaN collision-flood cap.
- Map seq ordering semantics.
