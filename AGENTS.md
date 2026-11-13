# Repository agent instructions

## Permanent rules and task context

This file defines permanent repository rules. Keep task objectives, progress,
decisions, and evidence in local, untracked files. This repository offers
`.context/` as an ignored storage convention; no particular directory layout is
required. Never commit temporary handoffs. Use
[the handoff contract](doc/agent-handoff.md) for new sessions or delegated tasks.

Read relevant existing guidance, including [AI context](doc/ai-context.md),
[the agent reference](.agents/prompt.md), and applicable `.agents/skills/` files.
These rules do not waive existing safety requirements, approval gates, hooks,
or required checks.

## Scope and execution

- Where the execution environment supports selection, prefer the least expensive
  capable model and adequate reasoning effort. Reserve high effort for
  architecture, security, concurrency, recovery invariants, and consequential
  decisions. Escalate when the available capability is insufficient; request
  changes from the user when needed. Do not assume agents can switch models.
- Work on one independently testable objective at a time. Prefer thin vertical
  slices with explicit acceptance criteria.
- Read only relevant context and source files. Avoid unnecessary history
  reconstruction. Treat approved architectural decisions as settled unless
  new evidence invalidates them.
- Escalate unexpected scope, architectural choices, or security implications
  before expanding the task.
- Prefer deterministic tools, existing implementations, and targeted tests.
  Reuse existing build outputs and test evidence when valid for the current
  revision and environment.
- Avoid unnecessary full builds, repeated CI campaigns, and speculative
  abstractions. Never weaken verification or bypass approval gates to save cost.
- Do not delegate additional tasks or request independent reviews by default.
  Use them only when risk, repository requirements, or demonstrated uncertainty
  warrants them, within the approval boundary and available environment controls.
- Prefer fresh sessions for independent work where supported. Do not rename the
  current branch without explicit user instruction. Use `origin/master` as the
  comparison base unless the task specifies another base.
- Stop when the assigned objective is complete or blocked. Do not invent
  follow-up work. Do not change execution environment settings without
  authorization.

## Durable context

Before material claims or decisions about existing architecture, prior decisions,
owner intent, reusable machinery, or production behavior, search durable internal
context. When available, run
`gbrain search "<specific systems, identifiers, and decision terms>" --source default`
and read relevant results. Verify historical evidence against current code and
fresh read-only evidence. If evidence changes a durable conclusion, update the
canonical page within the approved scope and authority; do not create a competing
note. If `gbrain` is unavailable or authentication fails, state the limitation
and use repository history and other available read-only sources.

## Safety and external approval

Never create, edit, delete, submit, or reply to human-visible content in external
systems without explicit user approval in the current conversation. Approval
must cover the exact destination, action, and body, or give limited pre-approval
that names the destination, action, trigger, and constraints. This includes PR
titles, descriptions, comments, reviews, issues, messages, and remote documents,
through any tool. Read-only fetches and local drafts are allowed. Without active
approval, draft the content in chat and wait. Ask again before expanding approval.

Preserve [the legacy publication hold](doc/legacy-publication-hold.md) and
[release qualification requirements](doc/release-publication.md#qualification-and-holds).
Do not activate publication or deployment as an incidental task step. Preserve
the migrated Kubo node and release identities. Publication, DNSLink changes, key
provisioning, and `/install` activation require separate approval.

## Reporting

Use precise identifiers and concrete behavioral claims. Distinguish observed
facts, inferences, proposals, and unknowns when needed. State what was validated,
what was not validated, and what could not be validated in the environment.

Keep routine reports under 200 words: commit and changes, tests and results,
remaining blockers, and an acceptance verdict. Store detailed evidence in files.
Use the handoff contract's 300–500-word budget for session handoffs. Optimize for
verified engineering progress per token, not minimum tokens at any cost.
