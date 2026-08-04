# Glia archive — August 2026

> **REJECTED / UNSAFE / TWO KNOWN LEAKS**
>
> This archive preserves research, not production-ready code. Stage C ownership
> work was rejected. Do not merge, ship, or use an archived implementation as a
> design baseline without a fresh design and independent review.

## What this is

This branch preserves the Glia implementation and research state as it existed
in August 2026. The first archival commit freezes the uncommitted PR-1 / PR-1b.0
Stages A–C production-tree work byte-for-byte. The second archival commit adds
the curated design, review, comparative-study, and spike-source corpus indexed
in [INDEX.md](INDEX.md).

Glia was archived because it had no validated product demand and repeatedly
distracted from validating Wetware's WASM-first authority substrate. It was not
archived merely because its engineering work was difficult. The difficulty
priced the remaining road; zero demonstrated demand, the cost of maintaining
two runtimes and interfaces, opportunity cost, and the need to test the
WASM-first product hypothesis drove the decision.

## Safety and status

**REJECTED / UNSAFE / TWO KNOWN LEAKS**

- Stage C's ownership implementation was rejected by Sol Review 2 and remains
  uncured.
- A cross-owner factory self-cycle leaks.
- A body-hidden self-cycle leaks.
- The cycle-collector implementation was rejected. Its audit found a
  Miri-proven safe-code use-after-free, count overflow/underflow defects,
  panic-poisoning and re-entry hazards, incomplete proof coverage, and surviving
  mutants.
- Stage D was frozen and was not implemented.
- No archived implementation should be merged or shipped without a new design,
  new implementation review, and new memory-safety evidence.
- Passing CI, compiling successfully, or passing the archived spike tests would
  not establish semantic correctness or memory safety.

## Why it was archived

- No artifact demonstrated Glia demand after roughly five months of product
  work. External signals attached to WASM and the object-capability substrate,
  not to the language.
- Glia imposed a second runtime, module system, value model, error taxonomy,
  debugging story, documentation path, and security-review surface.
- Continuing the language work delayed discovery and buyer-facing validation.
- Archival is a commitment to validate the WASM-first product thesis. It does
  not itself validate that thesis.

## What was learned

- Definition ownership must be represented structurally. Lexical capture and
  definition ownership are different relations and must not be conflated.
- Per-import module-instance isolation, source-only caching, and explicit cycle
  detection are safer than caching authority-bearing evaluated environments.
- Abortive exceptions, resumptive effects, evaluator control flow, and
  embedder-visible faults need distinct semantics even when they share syntax.
- Runtime values divide into local/executable, durable/content-addressed, and
  external/authority-bearing domains. Durable values and executable values are
  not interchangeable.
- The Graph 4 resting-weak/escaping-strong ownership model exposed real
  cross-owner strongly connected components. Collector-aware RC preserved useful
  algebraic insights, but the implementation audit showed that correctness is a
  whole-system obligation.
- GC and per-process-heap precedents favor explicit heap jurisdictions over
  local reference-strength bookkeeping.
- Authority must remain separate from memory-management state. Authority travels
  as requirements or grants; it is never serialized or inferred from collector
  reachability.
- Portable callables and effect-driven authority rebinding remain possible
  research ideas, not validated product requirements.

## Branch state

- Branch: `archive/glia-2026-08`
- Base commit: `f1365b609f0ff2c3fbb8262b6e9df6d88d6ca83d`
- Preserved implementation commit:
  `f448a5d8e340a284d70e4c7706f3843834f0b576`
- Curated corpus commit: the commit containing this README and [INDEX.md](INDEX.md)
  (`archive/glia-2026-08` at archival completion). Its immutable hash is reported
  in the archival handoff; a commit cannot embed its own hash.
- Document map and original-source inventory: [INDEX.md](INDEX.md)

Known unresolved research decisions include the ownership mechanism for
cross-owner executable graphs, whether any future Glia requires tracing GC or a
restricted acyclic profile, the exact durable/executable value boundary,
portable callable representation, effect-handler portability, and the
relationship between module instances and future durable code. None should be
resolved by treating the archived Stage C or collector as a default.

## Revival instructions

Start from the retrospective outline and revival scorecard in
[the archival CEO review](designs/glia-archival-ceo-review-2026-08-04.md), not
from the archived implementation.

Revival requires evidence, such as repeated unprompted requests for runtime
scripting, a named partner measurably blocked by compile/deploy cycles, a
controlled AI-authoring advantage, a load-bearing code-as-data use case, a
documented WASM/WIT expressiveness failure, or funded language work. "Glia still
seems promising" is not evidence.

The preferred revival shape is restricted, session-scoped, and acyclic. Expand
beyond that only when new evidence requires it. Rerun relevant Sol reviews,
Miri, Kani, fuzzing, mutation testing, and semantic reviews from scratch. Do not
assume any archived memory-model decision remains valid.

## Inspecting the spike source

The spike directories contain source, Cargo manifests and locks, tests,
benchmark source, audit source, fuzz-target source, and small minimized
property-test regression seeds. They intentionally contain no build output,
nested Git metadata, fuzz corpus, or generated audit target.

The archived leak probe's local `glia` dependency path was adjusted only to
remain repository-relative after relocation into this archive. Its Rust source
and technical behavior were not changed.

If a historical spike must be compiled, keep generated files outside the
archive, for example:

```sh
CARGO_TARGET_DIR="$(mktemp -d)" \
  cargo test --manifest-path doc/archive/glia/spikes/ownership-spike/Cargo.toml
```

The collector spike is retained to study the rejected design and audit. A green
build or test run does not override its rejected status or prove memory safety.

## Deliberate exclusions

The archive excludes every `target/` directory, generated binaries and compiler
state, Miri/Kani output trees, mutation clones, fuzz corpora and artifacts,
`.context/attachments/`, screenshots, pasted third-party material, nested
`.git/` directories, machine-specific generated state, and anything resembling
a credential or secret. The two nested repositories were flattened by copying
only their durable source files; their Git metadata was not preserved.
