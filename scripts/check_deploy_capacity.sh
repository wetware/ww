#!/usr/bin/env bash
# Fail a rollout before it can Recreate ww-master on a pressured node.
set -euo pipefail

min_free_bytes="${MIN_FREE_BYTES:-10737418240}" # 10 GiB
min_free_percent="${MIN_FREE_PERCENT:-25}"

fail() {
  echo "ERROR: $*" >&2
  exit 1
}

command -v kubectl >/dev/null || fail "kubectl is required for rollout capacity checks"
command -v jq >/dev/null || fail "jq is required for rollout capacity checks"

[[ "$min_free_bytes" =~ ^[0-9]+$ ]] || fail "MIN_FREE_BYTES must be an integer"
[[ "$min_free_percent" =~ ^[0-9]+$ ]] || fail "MIN_FREE_PERCENT must be an integer"

# ww-master has no tolerations or node affinity. Only a node the scheduler can
# currently consider is relevant to this Recreate rollout; a cordoned or
# NoSchedule/NoExecute-tainted node cannot receive it.
nodes="$(kubectl get nodes -o json | jq -r '
  .items[]
  | select(.spec.unschedulable != true)
  | select(any(.spec.taints[]?; .effect == "NoSchedule" or .effect == "NoExecute") | not)
  | .metadata.name
')"
[[ -n "$nodes" ]] || fail "cluster has no schedulable nodes to receive the rollout"

check_filesystem() {
  local node="$1"
  local label="$2"
  local selector="$3"
  local available capacity free_percent

  available="$(jq -r "$selector.availableBytes // empty" <<<"$summary")"
  capacity="$(jq -r "$selector.capacityBytes // empty" <<<"$summary")"
  [[ "$available" =~ ^[0-9]+$ && "$capacity" =~ ^[1-9][0-9]*$ ]] \
    || fail "node $node did not return usable $label capacity"

  free_percent=$((available * 100 / capacity))
  (( available >= min_free_bytes )) || fail \
    "node $node has only $available free bytes on $label (need $min_free_bytes)"
  (( free_percent >= min_free_percent )) || fail \
    "node $node has only ${free_percent}% free space on $label (need ${min_free_percent}%)"

  echo "node $node $label capacity check passed: $available bytes free (${free_percent}%)"
}

while IFS= read -r node; do
  [[ -n "$node" ]] || continue

  disk_pressure="$(kubectl get node "$node" -o json | jq -r '
    [.status.conditions[] | select(.type == "DiskPressure") | .status][0] // "Unknown"
  ')"
  [[ "$disk_pressure" == "False" ]] || fail "node $node has DiskPressure=$disk_pressure"

  summary="$(kubectl get --raw "/api/v1/nodes/$node/proxy/stats/summary")"
  check_filesystem "$node" "node filesystem" ".node.fs"
  check_filesystem "$node" "image filesystem" ".node.runtime.imageFs"
done <<<"$nodes"

echo "rollout capacity check passed"
