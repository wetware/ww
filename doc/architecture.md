# Architecture

This is the current architecture reference for Wetware's authority model.
For Cap'n Proto stream plumbing, polling, and deadlock analysis, see
[rpc-transport.md](rpc-transport.md). For the capability surface and
attenuation rules, see [capabilities.md](capabilities.md).

## Authority model

Wetware runs WASM cells with no ambient node authority. A **grant** is an
explicit delegation of a Cap'n Proto capability reference. A grant name is a
parent-chosen local label; it is neither an authorization key nor a service
lookup mechanism.

The important distinction is between the trusted root, the membrane it may
publish, and ordinary children:

- **pid0** is trusted by construction. It alone receives the process-local
  root `Membrane` and calls `Membrane.graft()`. That graft records the current
  generation in trusted host state and includes a distinct ordinary
  `Membrane` that pid0 may publish.
- A **network client** may receive the published `Membrane`. Its grafts
  provision ordinary epoch-guarded capabilities but cannot bind or commit
  PID0 readiness.
- An **ordinary child** receives `InitialGrants`, a host-provided,
  grants-only bootstrap. `InitialGrants.get()` returns exactly the immutable
  `InitialAuthorityRecord` selected by its parent.

`Membrane` is therefore an authority-issuance interface for trusted pid0 (and
for the separate, authenticated remote `Terminal(Membrane)` path). It is not
the ordinary-child bootstrap. `Process.bootstrap()` is also distinct: it is
the parent-held capability exported by a guest that uses `system::serve()`.

```
HOST PROCESS
│
├─ pid0 boot
│  process-local root Membrane → trusted pid0
│       ├─ Membrane.graft() → host grants + publishable Membrane
│       └─ private WIT kernel-ready() → commit bound generation
│
├─ network export
│  publishable Membrane → ordinary epoch-guarded grafts
│
└─ ordinary child spawn
   Executor.spawn(explicit named grants)
       → InitialAuthorityRecord → InitialGrants → ordinary child
       → fixed substrate + exact grants
       → (only if granted) Executor → grandchild with another exact grant set
```

The host graft contains the host-provided capabilities appropriate for the
node configuration, including identity, host, runtime, routing, authority,
IPFS, and optional HTTP client. PID0's process-local graft also contains the
ordinary membrane handoff, whose internal name is part of the private PID0 ABI.
Local pid0 grafting has no `AuthPolicy`. Remote authenticated issuance remains
separate: a `Terminal` verifies a login identity and returns the
policy-selected session authority.

## Boot and child creation

`ww run` resolves image layers, starts the host services, and loads trusted
pid0 from `boot/main.wasm`. The host does not interpret the rest of the image
as application policy. pid0 uses its graft to run Glia init scripts and build
the authority graph: load bytes, obtain an `Executor`, construct a named grant
map, spawn a process, and optionally publish the capability the process
exports.

The spawn lattice is deliberately small:

```
Runtime ──load(WASM bytes)──> Executor ──spawn(args, env, grants)──> Process
```

`Runtime` authorizes selecting and loading arbitrary code. `Executor` is
image-bound spawn authority. `Process` is authority over one running child
(stdio, lifecycle, and optionally its guest export through
`Process.bootstrap()`). A child receives `Runtime`, an `Executor`, or neither
only through an explicit grant.

Glia follows the same rule:

- `(cell image)` gives the cell zero application capabilities.
- `(cell image :grants {"name" cap ...})` gives it exactly that named map.
- `with` is ordinary lexical composition; it does not grant authority.
- Lexical capability capture is not a child-authority path. Late delegation
  uses an explicitly granted conduit and does not change the child's
  `InitialAuthorityRecord`.

The initial record is closed and idempotent. `InitialGrants` exposes no graft,
refresh, append, arbitrary-name resolution, policy, or parent-channel API.
Consequently an ordinary child cannot reacquire host authority through its
bootstrap, lexical capture, runtime propagation, arbitrary-name resolution,
or the known fallback paths.

## Routing and interposition

Grants are opaque Cap'n Proto references. The grants-only bootstrap forwards
neither calls nor authority: it returns the selected references as-is.

- A host-implemented granted capability routes directly to the host.
- A sibling-implemented capability routes through the host hub to that
  sibling.
- A parent-implemented or intentionally wrapped capability routes through the
  parent.

The local `receiverHosted` path collapse is preserved. An attenuation wrapper
is an intentional interposition point; it restricts a granted reference rather
than converting the child bootstrap into a forwarding membrane. Cross-node
three-phase handoff remains an upstream limitation.

## Epoch and listener lifecycle

An **epoch** is the host-issued authority timeslot (revocation generation).
Host-issued delegated references retain their epoch guards, so an epoch advance
makes stale references fail stale; it does not magically revoke arbitrary
capabilities a program may hold. Ordinary children cannot re-graft to refresh
them. pid0 re-grafts and reruns init for affected long-running services, then
explicitly re-delegates fresh references or replaces children.

Route registrations are epoch-scoped and identity-owned. A stale registration
stops dispatching immediately, and cleanup from an old registration cannot
delete a fresh replacement. Route liveness is not a kernel-readiness signal.

Readiness has one commit event: after init/init.d, trusted PID0 calls the
argument-free private Component Model import
`wetware:kernel-runtime/readiness@1.0.0` function `kernel-ready`. The host
derives the generation from the process-local graft, rejects a stale
generation, and commits `KernelReadyGate`; `/readyz` reads that gate directly.
The import is installed only on PID0's linker, is not a Cap'n Proto capability
value, and therefore cannot be delegated to children or transferred over the
network.

## Fixed execution substrate

Every child has local computation, args and environment selected at spawn,
stdio, clocks, randomness, and process lifecycle. These are substrate
facilities, not application grants.

An image-backed cell retains an image-rooted read-only filesystem. A
byte-loaded `Executor` gets a private empty read-only root. Every child has a
private writable `/tmp`, cleaned up with its lifecycle.

Optional host CAS wiring can make known-CID content readable. It does not
provide enumeration, mutation, MFS/IPNS, pin management, publishing, routing,
or arbitrary dialing. A read may nevertheless use node network, disk, cache,
and eviction resources. Known CIDs are copyable bearer locators, not
confidential object references.

## Security claim and boundary

The supported claim is precise:

> Spawned local children receive only explicit parent grants and cannot
> reacquire the enumerated node-authority capabilities through bootstrap,
> lexical capture, Runtime propagation, arbitrary-name resolution, or known
> fallback paths.

This is not a claim of total confinement, node-wide least authority, complete
information-flow security, absence of covert channels, or complete resource
isolation. Documentation and deployments must distinguish intentionally granted
capabilities, accepted substrate channels, CID-bearing information and resource
effects, remote authenticated authority issuance, and explicitly insecure modes
such as `--insecure-ephemeral`.

## Image layout

The image root visible to pid0 follows the FHS convention:

```
<image>/
  boot/main.wasm    # trusted pid0 entrypoint, consumed by the host
  bin/              # executables selected by pid0
  svc/<name>/       # service images selected by pid0
  etc/              # init and configuration selected by pid0
```

Image layers may be merged per file. Their composition and Stem integration
are described in [images.md](images.md); neither changes the ordinary-child
grant rule described above.
