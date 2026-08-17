---
name: ww-review-app
description: Audit a Wetware app for security and correctness
reads:
  - doc/ai-context.md
  - doc/capabilities.md
  - doc/architecture.md
  - doc/images.md
---

# Review an App

Audit a Wetware application for capability hygiene, security, and
correctness.  The user may point you at their own code or at an
example in this repo.

## Start with their concerns

Don't jump straight into a checklist.  Ask:

> What are you most worried about?  Or should I do a full sweep?

If they have a specific concern, start there — it shows you're
listening and gives them a quick win.  Then offer to check the
rest.

If they want a full sweep, tell them what to expect:

> I'll check seven things: transport registration, least authority,
> trust boundaries, image layout, protocol correctness, boundary
> I/O, and epoch safety.  I'll flag anything I find as
> critical / warning / suggestion.  Should take a few minutes.

## What to check

Work through these in order of impact.  **Report findings as you
go** — don't save everything for the end.  Each finding is a small
deliverable that keeps the review feeling productive.

### 1. Transport registration

- Does the application use `StreamListener.listen()`, `HttpListener.listen()`,
  `VatListener.serveRaw()`, or `VatListener.serveAuthenticated()` for the
  intended service?
- For a vat, does the published capability match the schema?
- For HTTP, does the path prefix match host routing?
- For stream or vat protocols, is the name non-empty and free of `/`?
- Does the application keep service publication separate from process spawn?

### 2. Principle of least authority

For each agent:
- What capabilities does it hold?
- Does it need all of them?
- Could any be attenuated further?

Read `doc/capabilities.md` and `doc/architecture.md` (Membrane
pattern section).

### 3. Trust boundaries

- Does pid0 give children more authority than needed?
- Could a compromised child escalate?
- Does each published vat service use `Terminal` authentication where needed?
- Does any raw vat publication expose more authority than intended?

### 4. Image layout

- FHS conventions followed?  Read `doc/images.md`.
- Layers composed correctly (override, not duplicate)?
- `bin/main.wasm` present in the union?

### 5. Protocol correctness

- Schemas match implementation?
- Streams registered and discovered correctly?
- RPC bidirectionality used appropriately?
- WAGI cells handle all expected methods?  Error codes correct?

### 6. Boundary I/O

Read the app's source and trace every operation that reaches
beyond the process.  Do this yourself — don't ask the user.

- Trace Cap'n Proto calls, WASI I/O, and stdin/stdout protocol handling.
- Confirm that each network operation uses an explicitly granted capability or
  the cell's declared transport.
- Confirm that capability references passed to children or peers have the
  smallest required method surface.
- Check timeouts, response bounds, and error handling at remote boundaries.

### 7. Epoch safety

- Does the application treat `staleEpoch` as terminal for the old authority?
- Do callers stop using a capability after `staleEpoch`?
- Does the Host replace PID0 rather than relying on guest re-grafting?
- Which application state does not survive epoch transitions?

## Output

After each area, share what you found.  Then compile a summary:

1. **Summary** — overall assessment (1-2 sentences)
2. **Findings** — numbered, each with severity
   (critical / warning / suggestion) and a concrete fix
3. **Transport audit** — confirm the registration or publication API matches
   the intended protocol
4. **Capability map** — table: current capabilities vs. recommended
   minimum

**Start with quick wins** — if there's a one-line fix, surface
it first.  Momentum matters.

When done:

> That's the review.  Want to dig into any of these findings?
> Or try something else?

Suggest other `/ww-*` skills as appropriate.
