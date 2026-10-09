# capnp-rpc dependency patches

Wetware pins `capnp-rpc` 0.25.1 from
[wetware/capnproto-rust](https://github.com/wetware/capnproto-rust) at
`b3befb30fa1cb17b3b49f278d1a6f02b2ef2860a`.
Every host and standalone guest patch uses that exact Git revision.
The dependency contains no Wetware Process policy.

The upstream base is
[`61f2c7640516f6d74c4b7d6e67257fae4fff9bda`](https://github.com/capnproto/capnproto-rust/commit/61f2c7640516f6d74c4b7d6e67257fae4fff9bda)
in `capnproto/capnproto-rust`.
The patch history separates three changes:

1. Import-wrapper reuse and its regression (`1fe72076` and `04626090`).
2. Connection terminalization and six private regressions (`0b4316f3`).
3. Cargo packaging for Git consumers (`b3befb30`): resolve `capnp` and
   `capnp-futures` from crates.io to avoid duplicate crate identities.

The runtime and regression source matches the previously validated vendored
crate. Vendoring-specific workspace declarations, copied license content,
standalone lockfile, and example whitespace changes are excluded.

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
