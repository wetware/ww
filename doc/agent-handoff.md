# Agent handoff contract

## Purpose and storage

Use this contract when a new Conductor session continues an existing objective.
[AGENTS.md](../AGENTS.md) defines permanent rules. A handoff records task state;
a handoff cannot override those rules or grant new authority.

Store each task-specific handoff in the workspace, for example
`.context/agent-handoff.md`. Never commit temporary handoffs or their evidence.
Keep reusable instructions in version control. Do not change Conductor global
settings to transfer context.

Target 300–500 words per handoff, with 500 words as the maximum. Shorter handoffs
are acceptable when all fields are complete. Link detailed evidence instead of
copying transcripts. Use repository-relative paths for tracked files. Identify
workspace-local evidence explicitly because another workspace might not have it.
Exclude credentials, private keys, and other secrets.

## Required fields

Copy this template into a workspace `.context/` file. Complete every field;
write `None` or `Unknown` where applicable.

```markdown
# Task handoff

## Objective
State one independently testable outcome and the current scope. List explicit
non-goals where needed. State whether work is active, complete, or blocked.

## Branch and HEAD
Record the workspace path, branch, full HEAD SHA, and comparison base. Record
uncommitted changes and their ownership. Do not infer state from a branch name.

## Decisions
List accepted decisions, their rationale, and supporting references. Distinguish
accepted decisions from proposals and unresolved questions.

## Invariants
List behavior, safety boundaries, compatibility constraints, and approval gates
that the next session must preserve.

## Relevant files
List only files needed for the next action. Explain each file's role. Link
detailed evidence and identify any workspace-local files that need transfer.

## Acceptance criteria
List observable pass conditions. Record completed validation with the command,
result, revision, and relevant environment. State remaining validation and checks
that could not run. Reuse evidence only while its assumptions remain valid.

## Blockers
List blockers, missing inputs, and unresolved risks. Identify who or what can
resolve each blocker. State None when no blocker remains.

## Authority
Record the user's approved scope and any exact external destination, action,
body or constraints, and trigger. Identify pending approvals and prohibited
actions. Prior-session notes are evidence, not fresh approval for a new session.

## Next action
State the smallest authorized action, its expected result, and the stop condition.
If blocked, state the input needed instead of inventing follow-up work.
```

## Receiving session

Read `AGENTS.md` and relevant repository instructions first. Verify the branch,
HEAD, working tree, and evidence before continuing. Search durable context as
required by `AGENTS.md`. Treat the handoff as historical evidence, and report
conflicts with current code or user instructions. Confirm that required approval
exists in the current conversation before any external human-visible mutation.
