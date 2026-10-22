#!/usr/bin/env bash
# Boot one actual image with its configured CMD, non-root HOME volume and Kubo.
# Every recipe declares bare installed-state startup.
set -euo pipefail
image="${1:?image required}"
fhs="${2:?canonical FHS required}"
engine="${CONTAINER_ENGINE:-docker}"
work="$(mktemp -d)"
prefix="ww-package-$$"
# Invoked by the EXIT trap.
# shellcheck disable=SC2317,SC2329
cleanup() {
  "$engine" rm -f "$prefix-host" "$prefix-kubo" >/dev/null 2>&1 || true
  "$engine" volume rm "$prefix-home" >/dev/null 2>&1 || true
  "$engine" network rm "$prefix" >/dev/null 2>&1 || true
  rm -rf "$work"
}
trap cleanup EXIT
"$engine" info >/dev/null
"$engine" network create --internal "$prefix" >/dev/null
"$engine" volume create "$prefix-home" >/dev/null
"$engine" run -d --name "$prefix-kubo" --network "$prefix" \
  -e IPFS_PROFILE=test ipfs/kubo:v0.33.0 daemon --offline >/dev/null
for _ in $(seq 1 60); do
  if "$engine" exec "$prefix-kubo" ipfs --api=/ip4/127.0.0.1/tcp/5001 id >/dev/null 2>&1; then break; fi
  sleep 1
done
"$engine" exec "$prefix-kubo" ipfs --api=/ip4/127.0.0.1/tcp/5001 id >/dev/null
# Docker populates an empty named HOME volume from the image. This checks its
# owner and the required private directory using a real non-root write.
"$engine" run --rm --read-only --mount "type=volume,src=$prefix-home,dst=/home/ww" \
  "$image" keygen --output /home/ww/.ww/identity >/dev/null
"$engine" run -d --name "$prefix-host" --network "$prefix" --read-only \
  --mount "type=volume,src=$prefix-home,dst=/home/ww" --tmpfs /tmp \
  -e WW_HTTP_ADMIN=0.0.0.0:2026 \
  -e "IPFS_API=http://$prefix-kubo:5001" -e "IPFS_ROUTING_API=http://$prefix-kubo:8080" \
  -e WW_EXECUTOR_THREADS=2 -e WW_COMPILE_WORKERS=2 "$image" >/dev/null
for relative in boot/kernel.wasm bin/status.wasm; do
  "$engine" cp "$prefix-host:/usr/share/wetware/kernel/$relative" "$work/$(basename "$relative")" >/dev/null
  cmp "$fhs/$relative" "$work/$(basename "$relative")"
done
health_args=(--ready --admin 127.0.0.1:2026)
if [ -n "${WW_BUILD_GIT_SHA:-}" ]; then health_args+=(--expect-git-sha "$WW_BUILD_GIT_SHA"); fi
wait_ready() {
  for _ in $(seq 1 "${WW_CONTAINER_BOOT_TIMEOUT:-180}"); do
    if "$engine" exec "$prefix-host" /usr/local/bin/ww healthcheck "${health_args[@]}" >/dev/null 2>&1; then
      return 0
    fi
    if [ "$("$engine" inspect -f '{{.State.Running}}' "$prefix-host")" != true ]; then break; fi
    sleep 1
  done
  "$engine" logs "$prefix-host" >&2
  return 1
}
snapshot() {
  local destination="$1"
  # The internal network need not publish ports to the host. Read admin identity
  # through the existing Kubo container, which has BusyBox wget.
  "$engine" exec "$prefix-kubo" wget -T 10 -qO- "http://$prefix-host:2026/host/id" > "$work/peer"
  "$engine" exec "$prefix-kubo" wget -T 10 -qO- "http://$prefix-host:2026/version" > "$work/version.json"
  python3 - "$work/peer" "$work/version.json" "$destination" <<'PY'
import json, sys
peer_path, version_path, destination = sys.argv[1:]
with open(peer_path) as source:
    peer = source.read().strip()
with open(version_path) as source:
    version = json.load(source)
root = version['kernel_effective_root']
assert peer and root and version['kernel_source_cid'] and version['kernel_cid'], version
assert version['kernel_source'] == f'/ipfs/{root}/boot/kernel.wasm', version
with open(destination, 'w') as output:
    json.dump({'peer': peer, 'root': root, 'kernel': version['kernel_cid'],
               'source': version['kernel_source_cid']}, output, sort_keys=True)
PY
}
wait_ready
snapshot "$work/before.json"
"$engine" cp "$prefix-host:/home/ww/.ww/identity" "$work/identity-before" >/dev/null
"$engine" restart "$prefix-host" >/dev/null
wait_ready
snapshot "$work/after.json"
"$engine" cp "$prefix-host:/home/ww/.ww/identity" "$work/identity-after" >/dev/null
cmp "$work/before.json" "$work/after.json"
cmp "$work/identity-before" "$work/identity-after"
echo "PASS: $image canonical bytes, bare startup, non-root readiness, and stable PeerId after restart with whole-HOME volume"
