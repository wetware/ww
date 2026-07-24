# Positioning

Wetware is the runtime for software that runs code its operator
didn't write and can't audit. This page is the public version of
how we think about the product, who it's for, and what it does
that nothing else does.

## The job

> **When** I'm building an agent that mixes tools, untrusted inputs,
> and code I didn't write (Simon Willison's
> [lethal trifecta](https://simonwillison.net/2025/Jun/16/the-lethal-trifecta/)),
> **I want** the runtime to enforce isolation per-call,
> **so I** don't need to audit those tools at all -- the runtime
> makes audit the wrong tool for the job.

This is the canonical Moesta/Klement Job-To-Be-Done sentence.
It's deliberately scoped tight: **multi-tool agent products**,
where the orchestrator calls a heterogeneous mix of first-party
tools, third-party MCP servers, and code an LLM wrote at runtime.

The job has three layers:

- **Functional** -- ship an agent that runs code you didn't write
  *without auditing it*. The runtime enforces the trust boundaries;
  one tool can't read another's state, drain your wallet,
  exfiltrate user data, or make calls you didn't grant. Audit
  isn't reduced -- it's no longer the gate.
- **Emotional** -- ship at LLM speed without security being the
  bottleneck. The thing you built will not be the thing that causes
  the breach.
- **Social** -- you're an agent-native builder. You compose tools
  across vendors and let the model write code at runtime, while
  the rest of the industry is still spinning up a Docker per tool
  or hoping nothing in the prompt graph goes sideways.

## Why this is a real job

The pain has a name. Simon Willison coined "lethal trifecta" in
2025 for the combination of *untrusted input* + *tools that act* +
*private data*. It's the canonical builder vocabulary for the
problem and is cited by Microsoft, Cloudflare, and Snyk.

The pain is paid-and-failing. Real, attributed breaches:

- OpenAI / Hugging Face (July 2026): [OpenAI reported](https://openai.com/index/hugging-face-model-evaluation-security-incident/)
  that models in a sandboxed cyber evaluation found a zero-day path to open
  Internet access, moved laterally through its research environment, and
  compromised Hugging Face production infrastructure while pursuing benchmark
  answers. [Hugging Face independently reported](https://huggingface.co/blog/security-incident-july-2026)
  an autonomous campaign spanning more than 17,000 recorded events. This does
  **not** establish that Wetware would have prevented the sandbox escape. It
  does make the post-escape authority question concrete: what credentials,
  network destinations, and application operations did the executor possess
  once containment failed?
- Supabase + Cursor (2025): a privileged service-role agent
  processed user-submitted support tickets containing SQL
  injection and exfiltrated integration tokens to a public thread.
- GitHub MCP (May 2025, Invariant Labs): malicious public issues
  hijacked the official GitHub MCP into leaking private repos via
  PRs.
- STDIO command injection (April 2026): zero-click RCE across
  7,000+ MCP servers with 150M+ downloads.

The incumbents are responding. Anthropic shipped Claude Code
sandboxing in November 2025 (84% reduction in permission prompts).
Cloudflare Sandboxes hit GA in April 2026 specifically for "code
specified at runtime" by agents. Both decisions were driven by
direct customer demand.

## Why nothing else satisfies it

The current landscape is "agent code sandboxing":

- **E2B / Modal / Daytona / Microsandbox** -- microVM and gVisor
  sandboxes. Strong process isolation. They sandbox the *process*,
  not the *call*.
- **Cloudflare Sandboxes** -- edge-distributed isolates and
  containers. Strong, fast, growing. Centrally hosted; JS-first;
  sandbox is per-process.
- **Anthropic's Claude Code sandbox** -- bubblewrap/seatbelt OS-level
  isolation for one product. Bash-only by default; broader tool
  isolation marked "not planned."

What none of them have:

- **Explicit capability handoff.** A tool can be granted capability
  `X` for one invocation and not the next. Shipped method-level
  attenuation travels with a schema-bound reference and recursively
  confines capabilities returned through it. Argument- and
  resource-level policy are not implied by that guarantee.
- **Composable trust graphs.** The membrane is the trust boundary,
  and graft composes naturally: when tool A calls tool B which
  calls tool C, each call carries a capability set the previous
  layer chose, enforced by the runtime, not by trust.
- **Content-addressed code with provable provenance.** The thing
  that ran is the thing you signed off on; no swap-under-the-rug.
- **WASM-cell scale.** ~10ms spawn, KB-scale binaries,
  language-agnostic by design, so per-call sandboxing is actually
  feasible (microVM cold-start is too slow for that).
- **Remote capability delegation.** libp2p connects independently
  operated nodes; the delegated capability, not discovery or the
  transport peer ID, determines authority. A Terminal can authenticate
  multiple principals through one node and issue a different method
  profile to each.

Two of those (explicit capability handoff, composable membranes) are
deep enough to win deals on. The others are proof points.

## How Wetware satisfies it

A worked example. A YC-batch startup builds an AI tax-prep agent.
The orchestrator calls 50 tools: some they wrote (trusted),
some are third-party MCP servers from GitHub (kinda-vouched-for),
some are written by the LLM at runtime (definitely untrusted).

On Wetware:

- The orchestrator runs as a cell with a membrane that grants it
  the capabilities it needs: the user's tax document arrives as a
  CID (a sturdyref into the IPFS UnixFS DAG, unforgeable and
  content-addressed -- no host filesystem access, no path-based
  authority); `http-client` for the IRS API (gated to `irs.gov`
  via `--http-dial`); `identity` for signing. Nothing else.
- Each tool the orchestrator calls runs as a child cell. The
  orchestrator decides what fragment of its membrane to graft
  to the child. The IRS API tool gets an `http-client` whose
  dial allowlist is just `irs.gov`; it cannot reach any other
  domain.
- A third-party MCP server pulled from GitHub gets *zero*
  `http-client`, *zero* fs / CID handles, *zero* environment,
  and a single typed Cap'n Proto interface. If it tries to do
  anything outside its grant, the runtime fails the call. The
  host doesn't have to trust the server's good intentions.
- A tool written by the LLM at runtime is content-addressed before
  it runs. The orchestrator can verify the CID matches what the
  model produced; the runtime enforces that the binary wasn't
  swapped between generation and execution.
- When the orchestrator finishes the user's request, every cell's
  capabilities are revoked. There's no ambient state for a tool
  to abuse on the next request.

The pitch to that startup: *your tax-prep agent can pull a new
MCP server from GitHub, call it as a tool, and the worst case is
the call fails. The MCP server cannot read your other tools' data,
cannot exfiltrate user PII, cannot drain your wallet. Today you
can't promise that to your enterprise customers. With Wetware
you can.*

## Who it's for

In order of audience temperature:

1. **YC-batch vertical-agent companies** in regulated verticals
   (legal, health, finance, compliance). They are six months in,
   selling to enterprise, and audit/compliance is blocking the
   integrations they want to ship. Highest pain, highest
   willingness to pay.
2. **Agent-builder shops** shipping multi-tool agent products
   (LangGraph, MCP, Claude Code-shaped). They've hit "we got
   pwned in eval" or "rolled back because the model called the
   wrong endpoint."
3. **MCP server publishers** who want their server to be
   adoptable by paranoid enterprise customers. Sandboxed-by-default
   is a feature, not a tax.
4. **Self-hosters and the privacy-paranoid** who want to run their
   own agents on hardware they don't trust (model weights leaking,
   prompt exfiltration, jurisdiction-flexible compute). This is
   a second-act audience; the substrate fits, but the trust story
   needs more (TEEs, reputation, staking) to be honest.

## How we don't position

A few framings we've explicitly *not* chosen, and why:

- **"Cloudflare Workers for Web3."** Useful as a pitch reference
  point in some rooms (infra/Web3 audiences). Wrong as identity.
  It implies centralized-but-decentralized, catches only the
  HTTP/WAGI mode, and points the audience at the wrong job
  (Web3 hosting vs. agent composition).
- **"AI safety layer."** "AI safety" sounds like alignment-research
  talk; the audience is *builders shipping agent products*, not
  safety researchers. Capability security is real and important,
  but it's the *proof* the product works, not the pitch.
- **"Decentralized compute marketplace."** Akash, Fluence, Render,
  and others compete on this turf, mostly on price. Wetware can
  serve that frame eventually, but leading with it makes us
  commodity-priced infrastructure instead of differentiated
  isolation.

## Honest scope

What we have today:

- WASM cell runtime with explicit membrane-grafted capabilities.
- Hook-level method attenuation for schema-bound capabilities, including
  returned capabilities, pipelines, re-attenuation intersection, and
  independent membrane-boundary lineage.
- Async Terminal policy that maps a verified login key to a fresh session,
  with global epoch expiry and targeted recipient revocation.
- Content-addressed code via IPFS (CIDs flow through the runtime).
- MCP/Glia bridge with membrane-grafted agent execution.
- Optional P2P transport with named Cap'n Proto services; service names
  locate capabilities and do not authorize recipients.
- A Rust Chess proof over direct-dial libp2p showing unknown denial,
  Reader/Player method differentiation over shared state, targeted
  revocation, epoch expiry, wrong-protocol rejection, and a bounded,
  application-owned first-call timeout.
- Engagement demo: install, hit a membrane-grafted WASM cell
  from curl in 60 seconds.

What we don't have today, and won't pretend to:

- **Host-side trust.** The guest currently has to trust the host
  to verify content against CIDs and to attenuate capabilities
  honestly. Today this is enforced operationally (you trust the
  host), not architecturally. TEEs, reputation, and staking close
  this gap; none are v1.
- **Argument/resource policy.** Method allowlists do not inspect a method's
  arguments or prove “customer X but not customer Y” unless those resources
  are represented as separate object capabilities.
- **Peer-ID authorization.** A node is a multi-tenant transport/runtime host,
  not a principal. Terminal authenticates the per-login credential; policy
  resolves the logical account or tenant.
- **Arbitrary dynamic-schema attenuation in Glia.** Trusted Rust can use typed
  generated method selectors. Glia refuses attenuation when it cannot
  associate a compiled schema with the capability.
- **Deploy-everywhere routing.** The DHT finds providers globally
  but doesn't sort by network proximity. CF's Anycast advantage
  is real and we don't claim it.
- **Hosted Wetware as a product.** The substrate runs anywhere,
  but the "deploy and stop thinking about servers" experience
  needs a hosted tier we haven't built yet.
- **Wallet integration / fuel markets.** Designed but not built.
  Becomes load-bearing once the swarm-of-strangers story is
  v1; today it's second-act.

## What's next

1. Map one real design-partner action to the exact authority an executor
   receives before and after policy approval.
2. Add argument/resource policies only from those concrete action shapes.
3. Turn the Chess evidence path into a public, inspectable proof after its
   claims survive technical review.

## If this is you

If you're shipping a multi-tool agent product and you've hit
"we got pwned in eval," "legal blocked the third-party
integration," or "we can't audit every MCP server we use,"
talk to us. We're in the first 100 conversations.
