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

The important distinction is between trusted PID0 and ordinary children:

- **pid0** is trusted by construction. It alone receives the process-local
  root `Membrane` and calls `Membrane.graft()`. That graft records the current
  generation in trusted host state.
- An **ordinary child** receives `InitialGrants`, a host-provided,
  grants-only bootstrap. `InitialGrants.get()` returns exactly the immutable
  `InitialAuthorityRecord` selected by its parent.

The PID0 `Membrane` is process-local. PID0 does not publish a base compatibility
payload on bare `/ww/0.1.0`. Authenticated vat services and byte streams remain
available under `/ww/0.1.0/vat/*` and `/ww/0.1.0/stream/*` respectively.
`Process.bootstrap()` is distinct: it is the parent-held capability exported
by a guest that uses `system::serve()`.

```
HOST PROCESS
│
├─ pid0 boot
│  process-local root Membrane → trusted pid0
│       ├─ Membrane.graft() → epoch-scoped host grants
│       └─ private WIT kernel-ready() → commit bound generation
│
└─ ordinary child spawn
   Executor.spawn(explicit named grants)
       → InitialAuthorityRecord → InitialGrants → ordinary child
       → fixed substrate + exact grants
       → (only if granted) Executor → grandchild with another exact grant set
```

The PID0 graft contains the host-provided capabilities appropriate for the
node configuration, including identity, host, runtime, routing, authority,
IPFS, and optional HTTP client. Local PID0 grafting has no `AuthPolicy`.
Authenticated vat publication remains separate: each inbound stream receives
a fresh `Terminal` that verifies login before returning policy-selected service
authority.

## Boot and child creation

`ww run` resolves image layers and starts the host services. The Host selects
trusted PID0 through `KernelSource`: `--kernel` takes precedence over
`WW_KERNEL`, and the default is the embedded `std/kernel` `main` component.
The Host↔PID0 ABI is version 3; no ABI-v2 compatibility shim exists.

The Rust PID0 follows one straight-line sequence for each generation:

1. Call `Membrane.graft()` once.
2. Read `$WW_ROOT/bin/status.wasm`.
3. Load the status component, grant `host`, and register `/status`.
4. Call the private `kernel_ready()` import.
5. Normally remain alive until the Host terminates the generation. With
   `WW_TTY`, stdin EOF also ends PID0.

PID0 does not poll for stale epochs and does not re-graft. The Host owns epoch
awareness, PID0 termination, and PID0 replacement.

The spawn lattice is deliberately small:

```
Runtime ──load(WASM bytes)──> Executor ──spawn(args, env, grants)──> Process
```

`Runtime` authorizes selecting and loading arbitrary code. `Executor` is
image-bound spawn authority. `Process` is authority over one running child
(stdio, lifecycle, and optionally its guest export through
`Process.bootstrap()`). A child receives `Runtime`, an `Executor`, or neither
only through an explicit grant.

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

## Epoch and PID0 lifecycle

An **epoch** is a host-local authority generation. Every host process assigns
epoch `0` to its boot deployment. Each accepted authoritative transition uses
checked local increment. Backend revisions, Atom blocks, timestamps, and
provenance do not become `Epoch.seq` and do not survive a process restart.

An optional `stem::Source` establishes the authoritative mutable base head.
The backend adapter applies its consistency rule before returning an update.
The Atom adapter polls `Atom.head()` at `tip - confirmation_depth`; contract
events are not part of its correctness path. Without a Stem, deployment
composes the configured frozen layers at epoch `0` and starts no source task.

`deployment` owns every transition. After it accepts a Source update, it first
publishes the incremented epoch with `root: None`. That publication closes
readiness, invalidates host-issued references, and expires route leases.
Deployment then terminates the current PID0 while it pins, merges, and pre-warms
the candidate root. The physical `CidTree` root remains unchanged during this
work.

After preparation and old-generation teardown both complete, deployment drains
queued Source updates. A newer update discards the prepared candidate and
starts preparation for the newer local epoch. Otherwise deployment swaps
`CidTree` and synchronously publishes the same epoch with `root: Some(...)`.
No await occurs between those operations. Deployment starts the next
`kernel::Generation` only after rooted publication, so two kernel generations
cannot be live at the same time.

An `InvalidHead` is an authoritative transition. Deployment publishes
`root: None`, terminates old PID0, remains alive without a replacement, and
waits for a later valid update. A Source error does not advance the epoch and
does not revoke the current deployment.

Deployment retries classified transient preparation failures with jittered
exponential backoff. Readiness stays closed and the old PID0 stays terminated
during retry. A newer authoritative update supersedes in-progress preparation
or retry. PID0 owns guest composition and service-setup failures; deployment
does not reinterpret a kernel result as a preparation failure.

The Host captures each generation's epoch sequence and filesystem root before
spawn. PID0's process-local graft uses that captured sequence. The graft fails
if the live epoch has changed. The private readiness commit also rejects a
captured sequence that is no longer live. PID0 therefore cannot combine one
generation's filesystem root with another generation's authority or readiness.
Rapid advances converge to the newest rooted epoch. A superseded
`PreparedRoot` does not activate.

Route registrations are epoch-scoped and identity-owned. A stale registration
stops dispatching immediately, and cleanup from an old registration cannot
delete a fresh replacement. Route liveness does not change
`KernelReadyGate`.

Readiness has one commit event. After composition, trusted PID0 calls the
argument-free private Component Model import
`wetware:kernel-runtime/readiness@1.0.0` function `kernel-ready`. The host
derives the generation from the process-local graft, rejects a stale
generation, and commits `KernelReadyGate`. Kernel readiness is this gate alone.
Externally observed `/readyz` requires both kernel readiness and, when the HTTP
route registry is configured, at least one live route. Without a route
registry, the route condition is satisfied.

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
  boot/main.wasm    # conventional application entrypoint produced by ww build
  bin/status.wasm   # status component loaded by the shipped Rust PID0
```

The shipped PID0 reads only `bin/status.wasm` from the effective root. It does
not evaluate an init directory or compose arbitrary application components.
Image layers may be merged per file. Their composition and Stem integration
are described in [images.md](images.md); neither changes the ordinary-child
grant rule described above.
