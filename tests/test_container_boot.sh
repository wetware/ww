#!/usr/bin/env bash
# Boot one actual image with its configured CMD, non-root HOME volume and Kubo.
# Containerfile.release has no CMD; pass "release" to supply its caller command.
set -euo pipefail
image="${1:?image required}"
fhs="${2:?canonical FHS required}"
mode="${3:-default}"
engine="${CONTAINER_ENGINE:-docker}"
work="$(mktemp -d)"
prefix="ww-package-$$"
# Invoked by the EXIT trap.
# shellcheck disable=SC2329
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
set --
if [ "$mode" = release ]; then set -- run /usr/share/wetware/kernel; fi
"$engine" run -d --name "$prefix-host" --network "$prefix" --read-only \
  --mount "type=volume,src=$prefix-home,dst=/home/ww" --tmpfs /tmp \
  -e "IPFS_API=http://$prefix-kubo:5001" -e "IPFS_ROUTING_API=http://$prefix-kubo:8080" \
  -e WW_EXECUTOR_THREADS=2 -e WW_COMPILE_WORKERS=2 "$image" "$@" >/dev/null
for relative in boot/kernel.wasm bin/status.wasm; do
  "$engine" cp "$prefix-host:/usr/share/wetware/kernel/$relative" "$work/$(basename "$relative")" >/dev/null
  cmp "$fhs/$relative" "$work/$(basename "$relative")"
done
health_args=(--ready)
if [ -n "${WW_BUILD_GIT_SHA:-}" ]; then health_args+=(--expect-git-sha "$WW_BUILD_GIT_SHA"); fi
for _ in $(seq 1 "${WW_CONTAINER_BOOT_TIMEOUT:-180}"); do
  if "$engine" exec "$prefix-host" /usr/local/bin/ww healthcheck "${health_args[@]}" >/dev/null 2>&1; then
    echo "PASS: $image canonical bytes and non-root readiness with whole-HOME volume"
    exit 0
  fi
  if [ "$("$engine" inspect -f '{{.State.Running}}' "$prefix-host")" != true ]; then break; fi
  sleep 1
done
"$engine" logs "$prefix-host" >&2
exit 1
