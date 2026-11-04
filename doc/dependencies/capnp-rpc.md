# capnp-rpc dependency patches

Wetware pins `capnp-rpc` 0.25.1 from
[wetware/capnproto-rust](https://github.com/wetware/capnproto-rust) at
`25261268e254b2d45283233e7feebe79389a1509`.
Every host and standalone guest uses that exact RPC revision.
The dependency contains no Wetware Cell ownership or admission policy.

The typed invocation hooks also require a coordinated `capnp` patch.
Each standalone graph retains its existing locked package version:

| Graph | `capnp` | Exact Git revision |
| --- | --- | --- |
| Host workspace, fuzz, status, new scaffold | 0.25.4 | `25261268e254b2d45283233e7feebe79389a1509` |
| Oracle example | 0.25.3 | `52cb94dcb1e7c24e863ada2eca5e454a842ed51a` |
| Kernel, authority, guest cancellation, stream lifecycle, root probe | 0.25.6 | `155c6f536de545810eb8e0436945bac3ad6df933` |

Each `capnp` variant starts from its exact crates.io package source and adds
only the typed hooks in `src/private/capability.rs`. The host package keeps
the exact 0.25.4 source previously selected by Cargo.lock. The unused `capnp`
source in the old fork differed in six files despite declaring that version.
The new fork reconciles those files before applying the hooks.
`capnp-futures` and `capnpc` keep their existing registry versions.

These integration revisions are prepared locally and remain unpublished.
Publish the approved fork commits before expecting a clean checkout to
resolve the new pins. Local qualification uses a private Git URL mapping;
production manifests contain no local path overrides. This is not the
Cap'n Proto 0.27 migration described in #795.

The upstream base is
[`61f2c7640516f6d74c4b7d6e67257fae4fff9bda`](https://github.com/capnproto/capnproto-rust/commit/61f2c7640516f6d74c4b7d6e67257fae4fff9bda)
in `capnproto/capnproto-rust`.
The patch history retains the original changes and adds the integration:

1. Import-wrapper reuse and its regression (`1fe72076` and `04626090`).
2. Connection terminalization and six private regressions (`0b4316f3`).
3. Cargo packaging for Git consumers (`b3befb30`): resolve `capnp` and
   `capnp-futures` from crates.io to avoid duplicate crate identities.
4. Exact locked `capnp` source reconciliation (`7d837346`).
5. Typed local invocation provenance and the separately qualified
   `add1c014` descriptor-preparation backport (`25261268`).

The runtime and regression source matches the previously validated vendored
crate. Vendoring-specific workspace declarations, copied license content,
standalone lockfile, and example whitespace changes are excluded from the
original three-change baseline.

## Local invocation provenance

`InvocationOrigin` is an opaque, process-local `Rc` identity. Creating a
token grants no authentication, capability authority, or Cell liveness.
The token has no payload, callback, numeric constructor, or wire encoding.
The token is neither `Send` nor `Sync`.

Trusted Cell ingress supplies `Connection::incoming_invocation_origin()`.
The connection captures the identity once and copies it into each incoming
call's `ParamsHook`. `RequestHook::set_invocation_origin()` transfers the
identity through ordinary local requests, private request reconstruction,
ordinary and streaming redirects, and supported reconnect wrappers.
Queues retain the existing Params. Capabilities and pipelines do not carry
an owner identity. A wire receiver uses its own ingress classification.

Repeated assignment of the same token succeeds. Conflicting assignment
permanently rejects the request, including sends that ignore the setter's
error. Unknown request wrappers reject explicit transfer. Params wrappers
must delegate the getter. Native forwarders must transfer the token when
they construct a new request and must propagate transfer errors.

Membranes retain their existing identity, policy, parameter transformations,
and result/pipeline wrapping. Existing broken requests preserve their
original denial or disconnect error. Descriptor preparation and redirects
recheck disconnect after callbacks before publishing new RPC state.

Wetware resolves the token to a live initiating Cell at the outbound effect
sink. The carrier itself supplies no admission budget, cancellation
acknowledgement, remote rollback, or cleanup certificate.

## Import-wrapper reuse

Re-importing a held capability reuses its live wrapper. This avoids a duplicate
wrapper invalidating the connection's downcast map when the duplicate drops.
A stale-map lookup in `write_descriptor` falls back to exporting the capability.

This correction and `reimport_then_resend_does_not_poison_downcast_map` were
merged upstream in [capnproto-rust#672](https://github.com/capnproto/capnproto-rust/pull/672).
The pinned release base still needs the backport.

## Connection terminalization

Disconnect publishes the first terminal error before application destructors
can reenter. It rejects pending questions, retained pipelines, promised
capabilities, and embargoes without another RPC driver poll. It rejects
ordinary and streaming requests constructed before disconnect.

Disconnect drains exports and resolve operations even when individual
destructors panic. Cleanup preserves the first panic and completes disconnect
bookkeeping. Repeated disconnect and late pipeline resolution preserve the
terminal result. No public API or wire format changes.

`when_resolved()` reports that a capability has settled, not that it is callable.
It may return `Ok(())` for a capability that is already broken, while a
subsequent method call fails.

## Validation and removal

In the dependency's upstream patch checkout, run:

```sh
cargo test -p capnp-rpc -p capnp-rpc-test
```

In Wetware, run:

```sh
cargo test -p rpc --lib --locked
cargo test -p wetware-membrane --locked
cargo test -p rpc --test typed_invocation_origin --locked
cargo test -p system --locked
cargo test --test child_authority_confinement capnp_fork_gate --locked
```

In the four-PR #673 stack, PR3 (Process/RPC lifecycle) adds RPC coverage for
F1/F3 dispatch and receive panic cases, retained responses/pipelines, promised
capabilities, destructor panic handling, and independently observed export
release. PR2 (guest cancellation) adds the actual
guest-session P3 cancellation matrix.

Replace this pin only after the candidate release passes those regressions
and the actual guest-session P3 cancellation matrix. Change all explicit
Cargo patches, the CLI scaffold, and their affected lockfiles together.
