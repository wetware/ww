#!/usr/bin/env bash
set -euo pipefail

# Exercise native library and integration targets that need no generated WASM,
# canonical FHS, Kubo daemon, or Foundry node. These include RPC invocation
# origin and shutdown, capability checks, IPFS decoding, and release signatures.
# The full Host suite supplies the native P3, authority, Gate 2B, routing-key
# and stream-lifecycle probes, first-party components, canonical FHS and Kubo.
# It also tests the release package feature.
cargo test --locked --lib --tests \
    -p auth -p routing-key -p wetware-authority -p wetware-membrane \
    -p ww-cache -p ipfs -p release -p rpc -- --test-threads=2

# Each quota test exhausts process-wide resources and requires a fresh process.
for quota_test in \
    process_owner_limit_spans_threads_and_recovers_after_idempotent_close \
    default_budget_bounds_work_across_registry_recreation \
    explicit_host_budgets_cannot_bypass_process_limit
do
    cargo test --locked -p rpc --lib "outbound::tests::${quota_test}" -- --ignored --exact
done
