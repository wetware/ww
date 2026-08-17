---
name: ww-build-app
description: Design a new Wetware app with structured guidance
reads:
  - doc/ai-context.md
  - doc/capabilities.md
  - doc/architecture.md
  - doc/images.md
---

# Build an App

Design a Wetware application together.  This is a conversation,
not a questionnaire.

## Start with their idea

Ask one open question:

> What do you want to build?  Even a rough idea is fine — we'll
> figure out the details together.

Listen.  Then mirror it back in one sentence: "So you want to
build X that does Y — sound right?"  Get a yes before proceeding.

## Phase 1: Discovery (conversation)

Work through these questions *conversationally* — don't present
them as a checklist.  Weave them into the discussion as they
become relevant.  One at a time.

- **What service transport fits?**  This is the first network decision.
  Explain the options briefly and help them choose:
  - byte stream — custom binary protocols through `StreamListener.listen()`
  - HTTP/WAGI — HTTP routes through `HttpListener.listen()`
  - Cap'n Proto vat — typed capabilities published through
    `VatListener.serveAuthenticated()` or the explicit raw escape hatch
  - no network publication — an ordinary child used only through delegated
    capabilities

  If they're unsure: "What does your app's network interface look
  like?  HTTP endpoints?  Binary streams?  Typed RPC?"

- **Who are the agents?**  How many, what roles, do they trust
  each other?
- **What capabilities do they need?**  Networking, storage,
  identity, custom protocols?
- **What are the trust boundaries?**  What should agents NOT be
  able to do?
- **What crosses the process boundary?**  Anything that affects
  state beyond the local process needs to be an effect.  Help them
  identify which operations are local computation vs. which need
  to reach the network/swarm.  This shapes the handler design.
- **How do they coordinate?**  Single node?  Multi-node?  On-chain?

After each question, confirm you understood before moving on.
When you have enough to sketch an architecture, say so:

> I think I have enough to sketch this out.  Ready to see the
> architecture, or is there more to add?

## Phase 2: Architecture (tangible output)

Based on discovery, produce a concrete design.  Cover:

- **Service transport and protocol**: confirm the registration or publication
  API. For Cap'n Proto vats, sketch the `.capnp` interface. For HTTP, define
  endpoints. For byte streams, define the wire format.
- **Image layout**: what goes in `bin/`, `svc/`, `etc/`.
  Reference `doc/images.md`.
- **Build pipeline**: source → WASI P2 component. Reference
  `examples/counter/Makefile` as a Rust template.
- **Capability map**: which capabilities each agent needs.  Flag
  anything that could be attenuated.  Reference `doc/capabilities.md`.
- **Membrane design**: what pid0 exports, what children receive.
  Reference `doc/architecture.md`, section "The Membrane pattern".
- **Coordination model**: standalone, multi-node, or epoch-managed?

Present the architecture as a structured outline or diagram.

⚗️ Milestone: "Design phase done — we've mapped out agents, transports,
capabilities, and coordination."  Then ask:

> Does this match what you had in mind?  Anything to change before
> we write it up?

Get sign-off before moving to the handoff.

## Phase 3: Handoff (design document)

Produce a design document another human (or AI) can execute from:

- System diagram showing agents, capabilities, and data flow
- Service transport and registration API for each agent
- Capability map: per-agent capabilities and attenuations
- Image layout: directory tree
- Build pipeline: component compilation steps
- Protocol specs: interface sketches, endpoints, or wire format
- Open questions and risks

**Do not write code in this skill.**  The output is a design, not
an implementation.

When done:

> ⚗️ Design's ready.  Want to review it for security issues?  Or
> head back to the main menu?

Suggest `/ww-review-app` or other `/ww-*` skills.
