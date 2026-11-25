#!/usr/bin/env bash
set -euo pipefail

packages=(auth routing-key wetware-authority wetware-membrane ww-cache ipfs release rpc system discovery)
python3 scripts/check_fast_selection.py inventory "${packages[@]}"

package_args=()
for package in "${packages[@]}"; do
    package_args+=(-p "$package")
done
cargo test --locked --lib --tests "${package_args[@]}" -- --test-threads=2

# Each quota test exhausts process-wide resources and requires a fresh process.
for quota_test in \
    process_owner_limit_spans_threads_and_recovers_after_idempotent_close \
    default_budget_bounds_work_across_registry_recreation \
    explicit_host_budgets_cannot_bypass_process_limit
do
    name="outbound::tests::${quota_test}"
    cargo test --locked -p rpc --lib "$name" -- --ignored --exact --list | grep -Fx "$name: test"
    cargo test --locked -p rpc --lib "$name" -- --ignored --exact
done
