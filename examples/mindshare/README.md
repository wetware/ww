# Mindshare

Symmetric peer-to-peer context sharing for LLMs.

## What it demonstrates

- **Bidirectional capability exchange** -- both peers export a Mindshare to each other, not client-server
- **Sub-capability attenuation** -- Mindshare returns separate ContextWriter and Prompt capabilities, each independently grantable and revocable
- **Rate limiting as capability wrapper** -- token bucket intrinsic to the Prompt object, not an external policy check
- **Content-addressed push** -- context pushed as IPFS CIDs, resolved from the receiver's local content store
- **Local LLM integration** -- HttpClient POST to Ollama/llama-server, no cloud APIs
- **Named vat discovery** -- peers running Mindshare publish and dial a named vat service

## How it works

```
Louis                        DHT                        Casey
  |                           |                           |
  |-- find_providers(BD) ---->|                           |
  |<-- [casey, ...] ----------|                           |
  |                           |                           |
  |-- dial + trust ceremony --------------------------->  |
  |<----- exchange Mindshare capabilities --------------->|
  |                           |                           |
  |-- push(notes.md CID) -------------------------------->|
  |                           |        [stored at /louis/] |
  |                           |                           |
  |-- ask("what's the key insight?") -------------------->|
  |<-- "The main finding is..." --------------------------|
  |                           |                           |
  |        [stored at /casey/]|<-- push(reply.md CID) ----|
  |<----------------------------------------------------- |
  |                           |                           |
  |  ... async, bidirectional, long-lived ...              |
```

Both sides push content freely. Both sides prompt freely. Connections survive
disconnects. Each peer's LLM reads across all context pushed into its store.

## Intended use

Cardinality reduction during high-divergence conversations. Two researchers
dump context at each other, and the LLM on each side helps elicit signal
from noise across the shared context surface.

## Prerequisites

- Rust toolchain with `wasm32-wasip2` target:
  ```sh
  rustup target add wasm32-wasip2
  ```
- A local LLM server (e.g. [Ollama](https://ollama.com)) running on `localhost:11434`

## Building

```sh
make mindshare
```

## Schema

`mindshare.capnp` defines:

- **`Mindshare`** -- top-level capability with two sub-capabilities:
  - `context()` -- returns a ContextWriter for pushing content
  - `prompt()` -- returns a rate-limited Prompt for querying the LLM
- **`ContextWriter`** -- single method:
  - `push(cid, meta, inlineContent)` -- push content by CID or inline bytes
- **`Prompt`** -- two methods:
  - `ask(text)` -- prompt the local LLM with all pushed context
  - `remaining()` -- check rate limit (calls left, reset time)
- **`Metadata`** -- optional fields on pushed content:
  - `contentType` -- MIME type hint
  - `tags` -- freeform tags
  - `relation` -- future: supports/contradicts/refines/supersedes
  - `supersedes` -- future: CID of content this replaces

## Security

- **Capability IS access control.** No ACLs, no auth tokens. You hold a Mindshare
  capability or you don't. Revocation = drop the capability.
- **Per-peer isolation.** Each sender writes to a jailed prefix. No cross-contamination.
- **Rate limiting intrinsic to Prompt.** The token bucket travels with the capability.
  Attenuate by wrapping with a smaller bucket. No external rate limiter to bypass.
- **Domain-scoped HTTP.** HttpClient enforces host allowlist. The LLM call stays local.
- **Epoch-guarded.** All capabilities revoke on epoch advance. Peers re-graft automatically.

## Running

### Step 1: Run the first node (daemon terminal)

Start a host:

```sh
ww run --port=2025 std/kernel
```

Leave this process running.

### Step 2: Connect with `ww shell` (first node shell)

In a second terminal:

```sh
cd examples/mindshare
ww shell
```

Load snippets:

```clojure
/ > (load "glia/register.glia")
/ > (load "glia/serve.glia")
```

### Step 3: Connect a second peer

Open a second terminal:

```sh
ww run --port=2026 std/kernel
```

From another shell terminal:

```sh
cd examples/mindshare
ww shell
```

If prompted, select the host for the daemon you want to control.

Then load the same snippets:

```clojure
/ > (load "glia/register.glia")
/ > (load "glia/serve.glia")
```

The two peers will discover each other via DHT and exchange Mindshare
capabilities automatically.

> **Note:** Cell logic is currently a stub. The snippet wiring and
> schema are ready; the runtime behavior will land in a follow-up PR.

## Demo snippets

`glia/register.glia`:

```clojure
(def mindshare-wasm (load "bin/mindshare.wasm"))
(def mindshare-executor (perform runtime :load mindshare-wasm))
(def mindshare-process (perform mindshare-executor :spawn))
(def mindshare-cap (perform mindshare-process :bootstrap))

(perform host :serve-vat mindshare-cap "mindshare")
```

`glia/serve.glia`:

```clojure
(perform runtime :run (load "bin/mindshare.wasm") "serve")
```

`etc/init.d/mindshare.glia` is now a deployment-only hook. Keep
init-based boot scripts for packaged images, but use snippets as the
default demo flow.

## From the shell

```clojure
;; Connect to a peer
(perform host :connect "/ip4/192.168.1.42/tcp/2025/p2p/12D3Koo...")

;; Get their Mindshare capability
(def bd (perform mindshare :open "12D3Koo..."))

;; Push context — IPFS paths are first-class
(perform bd :push "/ipfs/QmNotes.../idea.md")
(perform bd :push "/ipfs/QmData.../results.csv" {:tags ["experiment" "v2"]})

;; Prompt against their context
(perform bd :ask "what's the strongest objection to this?")
;; => "The main gap is..."

;; Check rate limit
(perform bd :remaining)
;; => {:calls 8 :reset-in 3600}

;; Read what they've pushed to you via the virtual filesystem
(slurp "/ipfs/QmCasey.../reply.md")
```

## Files

```
examples/mindshare/
├── Cargo.toml
├── Makefile               # make mindshare
├── README.md              # this file
├── mindshare.capnp        # Mindshare schema source
├── bin/                   # build output (gitignored)
│   ├── mindshare.wasm
│   └── mindshare.capnpc   # compiled schema bytes
├── glia/
│   ├── register.glia      # shell-loaded registration
│   └── serve.glia         # DHT provide loop
├── etc/
│   └── init.d/
│       └── mindshare.glia # deployment-only hook
└── src/
    └── lib.rs             # guest implementation
```

## Roadmap

- **Trust ceremony** -- identity verification before capability export (TBD)
- **Reconnect persistence** -- Mindshare survives disconnection, context persists
- **Context window management** -- summarization/focus when context exceeds LLM limits
- **Epistemic graphs** -- Metadata `relation` and `supersedes` fields enable structured
  reasoning over a DAG of claims (supports, contradicts, refines)
- **Multi-party research rooms** -- N peers, shared LLM surface, capability-scoped views
- **notes.alembic.network** -- Obsidian-style web explorer for mindshare content
