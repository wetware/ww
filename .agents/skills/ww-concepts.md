---
name: ww-concepts
description: Deep-dive into why Wetware exists and how it thinks
reads:
  - doc/ai-context.md
  - doc/architecture.md
  - doc/capabilities.md
  - doc/rpc-transport.md
  - doc/images.md
---
# Explain Concepts

Walk the user through *why* Wetware exists and the mental model
behind it.

## Start with their question

Don't follow a fixed order.  Ask:

> What are you most curious about?  Some starting points:
>
> 1. **Cells** — the unit of computation (what makes Wetware different)
> 2. **Capabilities** — why no ambient authority, and what replaces it
> 3. **Architecture** — the three layers (host, kernel, children)
> 4. **The Membrane** — how capabilities flow and get attenuated
> 5. **Concurrency** — how race conditions disappear (E-ordering)
> 6. **Epochs** — on-chain coordination and capability lifecycle
> 7. **Images** — how code is packaged and layered
>
> Or just ask a question and I'll find the right thread.

If they pick one, cover that topic (see below), then check in.
Don't automatically proceed to the next topic — ask what they
want.

## Topic guide

For each topic, read the referenced files and explain in plain
language.  **Lead with the problem it solves**, then show how
Wetware addresses it.  One concept at a time.

### Cells

Key files: `capnp/cell.capnp`, `doc/architecture.md` section
"Cell types"

A Cell is a WASM binary with a type tag stored in a **WASM custom
section** named `"cell.capnp"`.  The tag is a Cap'n Proto union
injected post-build by `schema-inject` — the WASM itself doesn't
know about it.  When the host loads a Cell, it reads the custom
section to decide what plumbing to wire up (network listeners,
protocol bridges, etc.).  No section = pid0.

Read `capnp/cell.capnp` and show the actual schema.  Then walk
through the four variants **one at a time**, checking in between
each:

1. **`raw`** — raw libp2p stream bytes.  Think: custom binary
   protocols.  "Make sense?  Next one's more familiar."
2. **`http`** — FastCGI.  Think: REST APIs with familiar tooling.
3. **`capnp`** — Cap'n Proto RPC.  Think: typed, schema-addressed
   capabilities.
4. **absent** — pid0 mode.  The kernel itself.  "This one's rare —
   it's only for when you ARE the kernel."

Emphasize: the cell type determines how a process talks to the
*network*, not what it can do internally.

### Capabilities (ambient authority problem)

Key files: `doc/architecture.md` (section "No ambient authority"),
`doc/capabilities.md`

Start with the problem: agentic frameworks give agents ambient
authority — any code can call any API, read any secret.  Then
show the comparison table and explain ocap: having a reference
IS authorization.

**OS analogy** (use this if the user has systems background):
draw the parallel to Unix, then show how Wetware differs.
Present one row at a time, explain each, check in.

| Unix | Wetware | Key difference |
|------|---------|---------------|
| process | Cell | Cell = WASM binary in a sandbox.  No ambient env, no fs, no sockets.  A process can do anything the OS allows; a Cell can only do what its capabilities permit. |
| fork/exec | `runtime.load(wasm)` → `executor.spawn()` | Parent explicitly passes capabilities to child.  No inheritance of open fds, env vars, or fs access — you grant exactly what the child needs. |
| file descriptor | Cap'n Proto client | Both are opaque handles.  But Unix fds live in a global namespace (paths) — any process can `open("/etc/passwd")`.  A capnp client is unforgeable and can only be obtained by explicit handoff. |
| syscall table | Membrane → `graft()` | Both are the interface to kernel services.  But the syscall table is fixed and ambient — every process gets all of them.  `graft()` returns *only* the capabilities you were granted, and they can differ per Cell. |
| `ioctl(fd, ...)` | method call on cap | Both operate on a handle.  But capnp calls are typed, async, and pipelined — not a bag-of-bytes command code.  `runtime.load(...).executor()` resolves in one round-trip via pipelining. |
| filesystem | WASI VFS over IPFS + `$WW_ROOT` | No writable local fs in the sandbox.  Content is read through the WASI virtual filesystem — `open("$WW_ROOT/bin/foo.wasm")` transparently resolves `/ipfs/<cid>/...` paths.  Content-addressed, not capability-gated. |
| `open()` returns fd | `graft()` returns Session | `open()` grants access to anything the path resolves to.  `graft()` returns a Session with specific named capabilities: Host, Executor, Routing, Identity — each of which can be null (withheld). |
| signals | epoch lifecycle | Unix signals are fire-and-forget. An epoch advance makes host-issued capabilities stale. The Host terminates the old PID0 and starts a fresh PID0 for the new generation. |
| pipe | `ByteStream` (`capnp/system.capnp`) | Both connect two processes via read/write.  But ByteStream is a capability — it can be passed to third parties, attenuated, or revoked. |
| `bind()`/`listen()` | `StreamListener.listen()` / `VatListener.listen()` | Unix: any process can bind any port.  Wetware: listening requires the StreamListener or VatListener capability, scoped to a specific protocol.  No ambient network access. |
| semaphore / mutex | E-ordering (capnp objects) | No explicit locks.  Each capnp object serializes its own method calls — the object IS the lock.  Cross-object calls are concurrent; use pipelining to express ordering. |
| ring 0 / ring 3 boundary | Membrane | The Membrane is the ring transition.  In x86 the `syscall` instruction crosses from ring 3 to ring 0.  In Wetware, `graft()` crosses from Cell to host.  The Membrane controls what's on the other side — like the IDT controls which kernel handlers userspace can invoke. |
| init (pid 1) | pid0 (kernel Cell) | Both are the first process that sets up everything else.  pid0 receives the Membrane, decides policy, spawns children with attenuated caps.  The kernel IS a Cell. |

**The punchline:** the fd analogy is the closest match —
Cap'n Proto clients really are like userspace file descriptors.
But the critical difference is there's no filesystem namespace
that lets any process open any path.  You can only get a client
if someone hands it to you.  That's the whole security model.

**"Can't we just do this with file descriptors?"**  If the user
has this instinct, validate it hard — they're exactly right.
Cap'n Proto clients basically ARE userspace file descriptors
with async pipelining and a really nice typed API.

Walk through the similarities first — build on what they know:

- **Opaque, unforgeable handles** — you can't forge an fd, you
  can't forge a capnp client.  Having the reference IS the
  authorization.
- **Passable between processes** — Unix has `sendmsg` /
  `SCM_RIGHTS`.  Cap'n Proto passes capability references as
  method arguments — same idea, better ergonomics.
- **Revocable** — close an fd, it's gone.  Revoke a capnp
  client (epoch advance), it's gone.

Then show what capnp adds on top:

1. **No ambient namespace.**  The one thing Unix gets wrong:
   any process can `open("/etc/shadow")`.  The path namespace
   is a global back-channel for minting new fds.  Capnp clients
   have no equivalent — you can't conjure one from a string.
2. **Typed + composable.**  Fds are bags of bytes with `ioctl`.
   Capnp clients have typed methods — you can wrap a Host cap
   to remove `network()` and hand the restricted version to a
   child.  Same interface, fewer methods.
3. **Async pipelining.**  Every fd `read()`/`write()` is a
   blocking round-trip.  Capnp lets you chain calls on promises:
   `runtime.load(cid).executor().spawn()` — one round-trip, not three.
4. **Network-transparent.**  A capnp client can point at a local
   object or a remote peer.  Same API, same types.  Passing an
   fd across machines requires bespoke plumbing.

**Punchline:** "capnp is basically fds in userspace, plus async
pipelines, plus a typed API, minus the filesystem escape hatch.
That's the whole upgrade."

### Architecture (three layers)

Key files: `doc/architecture.md` (section "Layers")

Host → Kernel → Children.  The host is deliberately simple (it's
the sandbox).  The kernel is the policy engine.  Children get only
what pid0 hands them.

The Rust kernel in `std/kernel` is the active PID0 implementation.

### The Membrane

Key files: `doc/architecture.md` (section "The Membrane pattern")

How PID0 receives host capabilities and how hook-level policies attenuate
references passed across process or vat boundaries.

### Concurrency

Key files: `doc/rpc-transport.md`, `doc/architecture.md`

**Lead with the question:** "How do you prevent race conditions
in a distributed system without locks?"

**E-ordering** (from the E programming language, Cap'n Proto's
intellectual ancestor):

- Method calls on a **single** Cap'n Proto object are serialized.
  One at a time, in order.  No races within an object.
- Method calls **across** objects are independent and concurrent.
  This is where you *could* have races — but pipelining usually
  eliminates the need for coordination.

Draw the analogy: this is like goroutines communicating over
channels, or Erlang actors with mailboxes.  Each object IS the
lock.  You don't need semaphores because the concurrency boundary
is the object boundary.

**Pipelining** is the key trick: instead of waiting for a result
before making the next call, you can chain calls on *promises*.
`runtime.load(cid).executor().spawn()` resolves in a single
round-trip, not three.  This lets you express ordering constraints
declaratively rather than with locks.

If the user has blockchain background: "This is like how each
smart contract serializes its own state transitions, but without
a global block ordering.  There's no block builder because
there's no global state to sequence — just objects with local
ordering."

### Epochs

Key files: `doc/capabilities.md`, `doc/architecture.md`
(section "Epoch lifecycle")

On-chain coordination: when the epoch advances, host-issued capabilities
become stale. The Host terminates the old PID0, prepares the effective root,
and starts one new PID0 for the accepted generation. Ordinary children cannot
re-graft.

### Images

Key files: `doc/images.md`

FHS convention, layer stacking, per-file union.  How code gets
packaged and deployed.

## After each topic

Check in:

> Make sense?  Want to go deeper on this, try a different topic,
> or move on to something else?

Suggest other `/ww-*` skills as appropriate.
