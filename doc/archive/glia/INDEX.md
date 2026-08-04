# Glia archive index

This inventory maps each archived document to its original source, purpose, and
status at archival time. Status describes historical standing; it is not an
endorsement for production use. Read [README.md](README.md) first.

## Recommended reading order

1. [README.md](README.md) — safety warning, rationale, and revival rules.
2. [Glia archival CEO review](designs/glia-archival-ceo-review-2026-08-04.md)
   — strategic disposition, retrospective outline, and revival scorecard.
3. [Glia-ectomy engineering plan](designs/glia-ectomy-eng-plan-2026-08-04.md)
   — binding archive/removal sequencing and later amendments.
4. [Collector audit reconciliation](designs/pr1b0-collector-audit-reconciliation.md)
   and [cross-owner reconciliation](designs/pr1b0-crossowner-reconciliation.md)
   — why the archived mechanisms are unsafe.
5. [Memory-model study](designs/pr1b0-memory-model-study.md),
   [ownership resolution](designs/pr1b0-ownership-resolution.md), and
   [implementation map](designs/pr1b0-implementation-map.md) — the integrated
   technical model and its intended application.
6. PR-1 and PR-1b design/review records — semantic and ownership history.
7. Comparative preflight studies — pinned-source evidence and precedent.
8. `spikes/` — executable experiments and rejected collector audit material.

## Strategic and value-model documents

| Archived document | Original source | Purpose | Status |
|---|---|---|---|
| [glia-archival-ceo-review-2026-08-04.md](designs/glia-archival-ceo-review-2026-08-04.md) | `.context/glia-archival-ceo-review-2026-08-04.md` | Strategic archive decision, evidence audit, retrospective outline, revival scorecard | normative at archival time |
| [glia-ectomy-eng-plan-2026-08-04.md](designs/glia-ectomy-eng-plan-2026-08-04.md) | `.context/glia-ectomy-eng-plan-2026-08-04.md` | PR-0 archive mechanics, removal sequencing, risk register, binding amendments | normative at archival time |
| [value-contract.md](designs/value-contract.md) | `doc/designs/value-contract.md` | Snapshot of proposed equality, hashing, durability, callable, and capability semantics | superseded |

## PR-1 control and effect work

| Archived document | Original source | Purpose | Status |
|---|---|---|---|
| [pr1-design-report.md](designs/pr1-design-report.md) | `.context/pr1-design-report.md` | Initial control-state extraction and capability-identity design | superseded |
| [pr1-design-report-v2.md](designs/pr1-design-report-v2.md) | `.context/pr1-design-report-v2.md` | Revised exception/effect/fault model | normative at archival time |
| [pr1-final-contract.md](designs/pr1-final-contract.md) | `.context/pr1-final-contract.md` | Frozen PR-1 implementation contract | normative at archival time |
| [pr1-implementation-log.md](designs/pr1-implementation-log.md) | `.context/pr1-implementation-log.md` | Checkpoint record for the archived core implementation | experiment |
| [pr1-import-design-estimate.md](designs/pr1-import-design-estimate.md) | `.context/pr1-import-design-estimate.md` | Import-semantics alternatives and scope estimate | superseded |
| [pr1-sol-reconciliation.md](designs/pr1-sol-reconciliation.md) | `.context/pr1-sol-reconciliation.md` | First Sol review reconciliation | adversarial review |
| [pr1-sol-reconciliation-v2.md](designs/pr1-sol-reconciliation-v2.md) | `.context/pr1-sol-reconciliation-v2.md` | Revised Sol reconciliation for the semantic model | adversarial review |

## PR-1b and PR-1b.0 ownership work

| Archived document | Original source | Purpose | Status |
|---|---|---|---|
| [pr1b-cache-audit.md](designs/pr1b-cache-audit.md) | `.context/pr1b-cache-audit.md` | Module cache and authority-flow audit | adversarial review |
| [pr1b-definition-ownership.md](designs/pr1b-definition-ownership.md) | `.context/pr1b-definition-ownership.md` | Foundational comparison of lexical capture and definition ownership | normative at archival time |
| [pr1b-export-boundary.md](designs/pr1b-export-boundary.md) | `.context/pr1b-export-boundary.md` | Module export-boundary and instance-isolation design | normative at archival time |
| [pr1b0-amended-contract.md](designs/pr1b0-amended-contract.md) | `.context/pr1b0-amended-contract.md` | Post-Sol Graph 4 implementation contract | superseded |
| [pr1b0-ownership-resolution.md](designs/pr1b0-ownership-resolution.md) | `.context/pr1b0-ownership-resolution.md` | Definition-ownership semantics and Graph 4 selection | normative at archival time |
| [pr1b0-preflight-report.md](designs/pr1b0-preflight-report.md) | `.context/pr1b0-preflight-report.md` | Comparative evidence and initial spike go/no-go | experiment |
| [pr1b0-implementation-map.md](designs/pr1b0-implementation-map.md) | `.context/pr1b0-implementation-map.md` | File/symbol implementation map for the rejected ownership work | rejected |
| [pr1b0-memory-model-study.md](designs/pr1b0-memory-model-study.md) | `.context/pr1b0-memory-model-study.md` | Three-domain value model, Graph 4 jurisdiction, GC/process-heap research | normative at archival time |
| [pr1b0-sol-handoff.md](designs/pr1b0-sol-handoff.md) | `.context/pr1b0-sol-handoff.md` | Complete adversarial-review handoff and review criteria | adversarial review |
| [pr1b0-crossowner-reconciliation.md](designs/pr1b0-crossowner-reconciliation.md) | `.context/pr1b0-crossowner-reconciliation.md` | Reproduction and graph analysis of the two live ownership leaks | adversarial review |
| [pr1b0-collector-pivot-plan.md](designs/pr1b0-collector-pivot-plan.md) | `.context/pr1b0-collector-pivot-plan.md` | Pivot from identity split toward RC plus cycle collection | superseded |
| [pr1b0-cc-spike-report.md](designs/pr1b0-cc-spike-report.md) | `.context/pr1b0-cc-spike-report.md` | Initial collector-spike report before the full audit rejection | superseded |
| [pr1b0-collector-audit-reconciliation.md](designs/pr1b0-collector-audit-reconciliation.md) | `.context/pr1b0-collector-audit-reconciliation.md` | Reconciles the collector's Miri/Kani/fuzz/mutation audit and rejection | adversarial review |

## Comparative preflight studies

All nine records are experiments: pinned-source comparative research, not Glia
specifications. Machine-specific temporary paths were normalized in the archive;
pins, filenames, citations, and conclusions were preserved.

| Archived document | Original source | Purpose | Status |
|---|---|---|---|
| [batch1-racket-lua.md](preflight-studies/batch1-racket-lua.md) | `.context/preflight-studies/batch1-racket-lua.md` | Namespace, environment, and closure precedents in Racket and Lua | experiment |
| [batch2-ses-swingset-joee-pony.md](preflight-studies/batch2-ses-swingset-joee-pony.md) | `.context/preflight-studies/batch2-ses-swingset-joee-pony.md` | Compartments, vats, ownership, and authority precedents | experiment |
| [batch3-steel-rhai-rune-gluon.md](preflight-studies/batch3-steel-rhai-rune-gluon.md) | `.context/preflight-studies/batch3-steel-rhai-rune-gluon.md` | Closure and module implementation comparisons | experiment |
| [batch4-e-monte-newspeak.md](preflight-studies/batch4-e-monte-newspeak.md) | `.context/preflight-studies/batch4-e-monte-newspeak.md` | Definition and module ownership in E, Monte, and Newspeak | experiment |
| [batch5-clojure-chez.md](preflight-studies/batch5-clojure-chez.md) | `.context/preflight-studies/batch5-clojure-chez.md` | Definition semantics and runtime representation in Clojure and Chez | experiment |
| [batch6-boa-gc.md](preflight-studies/batch6-boa-gc.md) | `.context/preflight-studies/batch6-boa-gc.md` | Boa tracing-GC implementation and risk study | experiment |
| [batch7-cpython-baconrajan.md](preflight-studies/batch7-cpython-baconrajan.md) | `.context/preflight-studies/batch7-cpython-baconrajan.md` | Production RC cycle collectors and Bacon–Rajan lineage | experiment |
| [batch8-beam-erts.md](preflight-studies/batch8-beam-erts.md) | `.context/preflight-studies/batch8-beam-erts.md` | BEAM/ERTS per-process heaps and message-copying jurisdiction | experiment |
| [batch9-rustpython-gcarena.md](preflight-studies/batch9-rustpython-gcarena.md) | `.context/preflight-studies/batch9-rustpython-gcarena.md` | RustPython cycle history and gc-arena tradeoffs | experiment |

## Spike source packages

| Archived package | Original source | Preserved material | Status |
|---|---|---|---|
| [`spikes/ownership-spike/`](spikes/ownership-spike/) | `.context/spike/ownership-spike/` | Cargo manifest/lock, model source, benchmarks, Graph 4 proofs, properties, cross-owner tests | experiment |
| [`spikes/leak-probe/`](spikes/leak-probe/) | `.context/spike/leak-probe/` | Public-API reproduction source for ownership leaks | adversarial review |
| [`spikes/cc-spike/`](spikes/cc-spike/) | `.context/spike/cc-spike/` | Cargo manifests/locks, collector/model source, benchmarks, deterministic/property/adversarial/WASM tests, Kani model, mutation-preparation script, fuzz-target source, minimized property-test seeds | rejected |

The two nested repositories were flattened into ordinary archive directories.
No nested `.git/` directory or history bundle is included because the durable
state is fully represented by the selected source snapshot and accompanying
design/audit records.

## Exclusion record

- Every `target/` and generated compiler/audit/Miri/Kani tree.
- `.context/attachments/`, screenshots, pasted transcripts, and third-party raw
  material.
- `cc-spike-mutations/` mutation clones; the generator/audit source is retained.
- Fuzz corpora and fuzz artifacts; only fuzz-target source is retained.
- Nested `.git/` metadata, caches, binaries, shell-history-like artifacts, and
  machine-specific generated state.
- Temporary source-study mirrors; pinned citations and durable summaries remain.
