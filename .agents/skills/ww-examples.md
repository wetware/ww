---
name: ww-examples
description: Walk through echo, counter, and chess example apps
reads:
  - doc/ai-context.md
  - examples/echo/src/lib.rs
  - examples/counter/src/lib.rs
  - examples/chess/src/lib.rs
  - examples/chess/chess.capnp
---
# Study Examples

Walk through real examples together to see how Cells work in
practice.

## Start with what they want to learn

Don't just present a list — ask what they're after:

> We have three examples that show different cell types.  What
> sounds most useful to you?
>
> 1. **Echo** — simplest possible cell.  Good if you want to see
>    the bare minimum.  *(~5 min walkthrough)*
> 2. **Counter** — WAGI cell with FastCGI.  Good if you're building
>    a web service.  *(~10 min walkthrough)*
> 3. **Chess** — full Cap'n Proto vat cell over libp2p.  Good if you want
>    to see a real multi-node app.  *(~15 min walkthrough)*
>
> Or tell me what you're trying to build and I'll pick the most
> relevant one.

---

## 1. Echo (raw cell) — ~5 min

Read files from `examples/echo/`.

| What to read | Path |
|------|------|
| Source | `examples/echo/src/lib.rs` |
| Build | `examples/echo/Makefile` |
| README | `examples/echo/README.md` |

Walk through together:

1. **What it does**: reads stdin, writes it back to stdout.  That's it.
2. **Why it matters**: this is the stdin/stdout convention that *all*
   cell types share.  Everything else builds on this.
3. **Build it**: run `make -C examples/echo` yourself and show the
   output.  No schema needed (raw cell, no typed RPC).
4. **See it tested**: `examples/echo_handler_e2e.rs` shows how the
   host spawns and exercises it.

⚗️ **Name the win**: "That's a complete cell.  Everything else is
just fancier plumbing on top of this pattern."

5. **The effects angle**: point out that echo's stdin/stdout I/O
   is the simplest possible effect boundary — data comes in from
   the network (effect), gets processed locally (pure), goes back
   out (effect).  The cell itself has no way to reach the network
   except through this boundary.

Check in: "Make sense?  Want to see it run, or move on to something
with more moving parts?"

---

## 2. Counter (HTTP/FastCGI cell) — ~10 min

Read files from `examples/counter/`.

| What to read | Path |
|------|------|
| Source | `examples/counter/src/lib.rs` |
| Build | `examples/counter/Makefile` |
| README | `examples/counter/README.md` |

Walk through together:

1. **What it does**: serves `GET /counter` (returns count) and
   `POST /counter` (increments).  405 for everything else.
2. **The key difference**: this cell has a *type tag*.  Run
   `make -C examples/counter` yourself and show the output.
   The repository does not currently ship runtime composition for the
   counter after removal of the legacy Glia PID0.
3. **FastCGI protocol**: the cell speaks binary FastCGI over stdio.
   The host translates HTTP ↔ FastCGI.  Simpler than parsing HTTP/1.1.
4. **Per-request spawn**: each request gets a fresh instance.  Counter
   resets — that's expected for the demo.

⚗️ **Name the win**: "You've seen the full build pipeline: compile
WASM, inject cell type, host routes traffic.  That's how WAGI cells
work."

Check in: "Ready for the big one (Chess), or want to dig into
something here first?"

---

## 3. Chess (Cap'n Proto vat cell) — ~15 min

Read files from `examples/chess/`.

| What to read | Path |
|------|------|
| Overview | `examples/chess/README.md` |
| Source | `examples/chess/src/lib.rs` |
| Schema | `examples/chess/chess.capnp` |
| Replay design | `examples/chess/doc/replay.md` |

This is the big one.  Walk through in layers — don't dump
everything at once:

1. **The pitch** (~2 min): Two nodes play chess over libp2p.  Moves
   flow over Cap'n Proto.  Game replay published to IPFS.
   "This shows what a real multi-node Wetware app looks like."
2. **The schema** (~3 min): Read `chess.capnp`.  Show the interface.
   "This is the contract between the two nodes."
3. **The code** (~5 min): Walk through `src/lib.rs`.  Focus on: how
   it registers a listener, accepts connections, manages game state.
   Don't read every line — hit the interesting parts.
4. **The image layout** (~2 min): show the built component under `bin/`.

5. **Local vs. global** (~3 min): This is the payoff.  Walk through
   which operations are local (validating a move, updating the board)
   vs. which cross the process boundary as effects (sending a move
   to the opponent, publishing replay to IPFS, discovering peers via
   DHT).  "Anything that can't affect the swarm isn't an effect.
   Anything that can, must be."

⚗️ **Name the win**: "That's a full peer-to-peer application: typed
RPC, DHT discovery, IPFS publishing, image packaging."

---

## After each example

Summarize what they just learned in one line, then offer the next
step:

> Want to dig into a specific pattern?  Try another example?
> Or move on to building something of your own?

Suggest `/ww-build-app` or other `/ww-*` skills.
