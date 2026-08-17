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

`ww run` resolves image layers, starts the host services, and loads trusted
PID0 from `boot/main.wasm`. `std/kernel` is the only active PID0 implementation.
The Host↔PID0 ABI is version 3; no ABI-v2 compatibility shim exists.

The Rust PID0 follows one straight-line sequence for each generation:

1. Call `Membrane.graft()` once.
2. Read `$WW_ROOT/bin/status.wasm`.
3. Load the status component, grant `host`, and register `/status`.
4. Call the private `kernel_ready()` import.
5. Remain alive until the Host terminates the generation.

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

An **epoch** is the host-issued authority timeslot and PID0 deployment
generation. One PID0 instance belongs to one generation. An epoch advance
first broadcasts the authoritative head with no effective root. This broadcast
closes readiness, invalidates host-issued references, expires route leases, and
causes the host to terminate the current PID0 instance. The old generation is
never restored after this broadcast.

The host then applies the boot filesystem composition to the new head. The
composition order is the head, frozen namespace layers, then frozen user root
mounts. Later layers retain precedence. The host pins the head and effective
root, pre-warms the effective root, swaps `CidTree`, and broadcasts the rooted
epoch. A non-interactive daemon starts PID0 only from this rooted epoch. An
interactive `ww run` invocation exits successfully at the authority broadcast.

The Host retries only positively classified transient preparation failures.
Transport failures, Kubo 5xx responses, unavailable content, and the operation
timeout use jittered exponential backoff. Readiness stays closed, and the old
PID0 stays terminated, during every retry. A newer authoritative epoch replaces
the pending target and resets the backoff. Pre-network input failures and
unclassified failures stop `EpochService`, which makes the daemon exit nonzero.
PID0 owns failures from guest composition and service setup. The Host does not
inspect PID0 errors to decide whether to retry.

The Host captures each generation's epoch sequence and filesystem root before
spawn. PID0's process-local graft uses that captured sequence. The graft fails
if the live epoch has changed. The private readiness commit also rejects a
captured sequence that is no longer live. PID0 therefore cannot combine one
generation's filesystem root with another generation's authority or readiness.
Rapid advances converge to the newest rooted epoch. An epoch superseded during
Host preparation does not activate.

Route registrations are epoch-scoped and identity-owned. A stale registration
stops dispatching immediately, and cleanup from an old registration cannot
delete a fresh replacement. Route liveness is not a kernel-readiness signal.

Readiness has one commit event. After composition, trusted PID0 calls the
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
  bin/status.wasm   # status component loaded by the shipped Rust PID0
```

The shipped PID0 reads only `bin/status.wasm` from the effective root. It does
not evaluate an init directory or compose arbitrary application components.
Image layers may be merged per file. Their composition and Stem integration
are described in [images.md](images.md); neither changes the ordinary-child
grant rule described above.
