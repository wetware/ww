#!/usr/bin/env bash
# Publish the already-assembled Wetware release tree from a VPS into the IPFS
# pod, then prune only CIDs recorded in the CI-managed release pin state file.

set -euo pipefail

: "${POD:?POD is required}"

REMOTE_RELEASE_TREE="${REMOTE_RELEASE_TREE:-/tmp/ww-release-tree}"
POD_RELEASE_TREE="${POD_RELEASE_TREE:-/tmp/ww-release-tree-publish-$(date +%s)-$$}"
STATE_FILE="${WW_RELEASE_PIN_STATE:-/data/ipfs/ww-release-pins.txt}"
RETAIN="${WW_RELEASE_PIN_RETAIN:-10}"
KUBECTL_TIMEOUT="${KUBECTL_TIMEOUT:-10m}"
KUBECTL_BEST_EFFORT_TIMEOUT="${KUBECTL_BEST_EFFORT_TIMEOUT:-45s}"
IPFS_ADD_TIMEOUT="${IPFS_ADD_TIMEOUT:-5m}"
IPFS_PIN_TIMEOUT="${IPFS_PIN_TIMEOUT:-2m}"
IPFS_NAME_PUBLISH_TIMEOUT="${IPFS_NAME_PUBLISH_TIMEOUT:-5m}"
IPFS_PROVIDE_TIMEOUT="${IPFS_PROVIDE_TIMEOUT:-60s}"

case "$RETAIN" in
  ''|*[!0-9]*)
    echo "ERROR: WW_RELEASE_PIN_RETAIN must be a positive integer, got: $RETAIN" >&2
    exit 2
    ;;
esac
if [ "$RETAIN" -lt 1 ]; then
  echo "ERROR: WW_RELEASE_PIN_RETAIN must be at least 1" >&2
  exit 2
fi

k() {
  kubectl --request-timeout="$KUBECTL_TIMEOUT" "$@"
}

best_effort_k() {
  kubectl --request-timeout="$KUBECTL_BEST_EFFORT_TIMEOUT" "$@"
}

pod() {
  k exec "$POD" -- "$@"
}

log() {
  printf 'ipfs-publish: %s\n' "$*" >&2
}

cleanup() {
  best_effort_k exec "$POD" -- rm -rf "$POD_RELEASE_TREE" >/dev/null 2>&1 || true
}
trap cleanup EXIT

repo_stat_size() {
  best_effort_k exec "$POD" -- sh -c 'if command -v timeout >/dev/null 2>&1; then timeout 30 ipfs repo stat --size-only; else ipfs repo stat --size-only; fi' 2>/dev/null \
    | tail -n 1 \
    | tr -d '\r' \
    || true
}

copy_release_tree() {
  local attempt backoff

  for attempt in 1 2 3; do
    log "streaming release tree into pod staging path $POD_RELEASE_TREE (attempt $attempt)"
    # shellcheck disable=SC2016
    if tar -C "$REMOTE_RELEASE_TREE" -cf - . \
      | k exec -i "$POD" -- sh -c 'dest="$1"; rm -rf "$dest"; mkdir -p "$dest"; tar -C "$dest" -xf -' sh "$POD_RELEASE_TREE"; then
      return 0
    fi

    if [ "$attempt" -lt 3 ]; then
      backoff="$((attempt * 20))"
      log "release tree stream failed; retrying in ${backoff}s"
      sleep "$backoff"
    fi
  done

  return 1
}

if [ ! -d "$REMOTE_RELEASE_TREE" ]; then
  echo "ERROR: release tree is missing on VPS: $REMOTE_RELEASE_TREE" >&2
  exit 1
fi

log "collecting repo stat before publish (best effort)"
repo_stat_before="$(repo_stat_size)"

copy_release_tree

log "adding release tree to IPFS with implicit pinning disabled"
CID="$(pod sh -c "if command -v timeout >/dev/null 2>&1; then timeout '$IPFS_ADD_TIMEOUT' ipfs add --pin=false -rQ --cid-version=1 '$POD_RELEASE_TREE'; else ipfs add --pin=false -rQ --cid-version=1 '$POD_RELEASE_TREE'; fi" | tail -n 1 | tr -d '\r')"
if [ -z "$CID" ]; then
  echo "ERROR: ipfs add produced an empty CID" >&2
  exit 1
fi

echo "CID=$CID"
log "pinning release CID $CID"
pod sh -c "if command -v timeout >/dev/null 2>&1; then timeout '$IPFS_PIN_TIMEOUT' ipfs pin add '$CID'; else ipfs pin add '$CID'; fi"
log "publishing IPNS ww-release to $CID"
pod sh -c "if command -v timeout >/dev/null 2>&1; then timeout '$IPFS_NAME_PUBLISH_TIMEOUT' ipfs name publish --key=ww-release '/ipfs/$CID'; else ipfs name publish --key=ww-release '/ipfs/$CID'; fi"

log "announcing release CID to the DHT (best effort)"
if ! pod sh -c "if command -v timeout >/dev/null 2>&1; then timeout '$IPFS_PROVIDE_TIMEOUT' ipfs routing provide -r '$CID'; else ipfs routing provide -r '$CID'; fi"; then
  echo "WARNING: provide announce timed out or failed; DHT propagation may lag" >&2
fi

log "updating managed release pin state"
state_output="$(
  k exec "$POD" -- sh -s -- "$CID" "$RETAIN" "$STATE_FILE" <<'POD_STATE_SH'
set -eu

cid="$1"
retain="$2"
state_file="$3"

case "$retain" in
  ''|*[!0-9]*)
    echo "ERROR: retain must be a positive integer, got: $retain" >&2
    exit 2
    ;;
esac
if [ "$retain" -lt 1 ]; then
  echo "ERROR: retain must be at least 1" >&2
  exit 2
fi

state_dir="$(dirname "$state_file")"
mkdir -p "$state_dir"

if [ ! -f "$state_file" ]; then
  printf '%s\n' "$cid" > "$state_file"
  echo "STATE_CREATED=true"
  echo "RETAINED_COUNT=1"
  echo "UNPINNED_COUNT=0"
  echo "UNPINNED_CIDS="
  echo "FAILED_UNPIN_CIDS="
  exit 0
fi

work="$(mktemp)"
desired="$(mktemp)"
stale_file="$(mktemp)"
failed_file="$(mktemp)"
unpin_file="$(mktemp)"
final_state="$(mktemp)"
trap 'rm -f "$work" "$desired" "$stale_file" "$failed_file" "$unpin_file" "$final_state"' EXIT

awk -v cid="$cid" 'NF && $0 != cid && !seen[$0]++ { print } END { print cid }' "$state_file" > "$work"

total="$(wc -l < "$work" | tr -d ' ')"
if [ "$total" -gt "$retain" ]; then
  stale_count="$((total - retain))"
  head -n "$stale_count" "$work" > "$stale_file"
  tail -n "$retain" "$work" > "$desired"
else
  : > "$stale_file"
  cp "$work" "$desired"
fi

while IFS= read -r stale; do
  [ -n "$stale" ] || continue
  if [ "$stale" = "$cid" ]; then
    printf '%s\n' "$stale" >> "$desired"
    continue
  fi

  if ipfs pin rm "$stale" >/dev/null 2>&1; then
    printf '%s\n' "$stale" >> "$unpin_file"
  else
    echo "WARNING: failed to unpin managed stale release CID $stale" >&2
    printf '%s\n' "$stale" >> "$failed_file"
  fi
done < "$stale_file"

cat "$failed_file" "$desired" | awk 'NF && !seen[$0]++ { print }' > "$final_state"
if ! grep -Fxq "$cid" "$final_state"; then
  printf '%s\n' "$cid" >> "$final_state"
fi
cp "$final_state" "$state_file"

retained_count="$(wc -l < "$state_file" | tr -d ' ')"
unpinned_count="$(wc -l < "$unpin_file" | tr -d ' ')"
unpinned_cids="$(tr '\n' ' ' < "$unpin_file" | sed 's/[[:space:]]*$//')"
failed_cids="$(tr '\n' ' ' < "$failed_file" | sed 's/[[:space:]]*$//')"

echo "STATE_CREATED=false"
echo "RETAINED_COUNT=$retained_count"
echo "UNPINNED_COUNT=$unpinned_count"
echo "UNPINNED_CIDS=$unpinned_cids"
echo "FAILED_UNPIN_CIDS=$failed_cids"
POD_STATE_SH
)"

printf '%s\n' "$state_output"

unpinned_count="$(printf '%s\n' "$state_output" | awk -F= '$1 == "UNPINNED_COUNT" { value=$2 } END { print value + 0 }')"
if [ "$unpinned_count" -gt 0 ]; then
  log "running IPFS repo GC after managed stale release unpins"
  if ! pod sh -c 'if command -v timeout >/dev/null 2>&1; then timeout 120 ipfs repo gc; else ipfs repo gc; fi'; then
    echo "WARNING: ipfs repo gc timed out or failed after stale release unpins" >&2
  fi
fi

log "collecting repo stat after cleanup (best effort)"
repo_stat_after="$(repo_stat_size)"
rm -rf "$REMOTE_RELEASE_TREE"

echo "STATE_FILE=$STATE_FILE"
echo "REPO_STAT_BEFORE=$repo_stat_before"
echo "REPO_STAT_AFTER=$repo_stat_after"
