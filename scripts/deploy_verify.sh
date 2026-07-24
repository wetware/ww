#!/usr/bin/env bash
# Verify that a ww deployment serves the expected immutable image and source.

set -euo pipefail

NAMESPACE="${WW_DEPLOY_NAMESPACE:-default}"
DEPLOYMENT="${WW_DEPLOYMENT:-ww-master}"
SELECTOR="${WW_DEPLOY_SELECTOR:-app=ww-master}"
CONTAINER="${WW_DEPLOY_CONTAINER:-ww}"
ROLLOUT_TIMEOUT="${WW_ROLLOUT_TIMEOUT:-5m}"
KUBECTL="${KUBECTL:-kubectl}"

EXPECTED_IMAGE_DIGEST="${WW_EXPECTED_IMAGE_DIGEST:?set WW_EXPECTED_IMAGE_DIGEST}"
EXPECTED_GIT_SHA="${WW_EXPECTED_GIT_SHA:?set WW_EXPECTED_GIT_SHA}"
EXPECTED_IMAGE_DIGEST="${EXPECTED_IMAGE_DIGEST##*@}"

case "$EXPECTED_IMAGE_DIGEST" in
  sha256:*) ;;
  *)
    echo "ERROR: expected image identity must contain a sha256 digest" >&2
    exit 1
    ;;
esac

"$KUBECTL" -n "$NAMESPACE" rollout status "deployment/$DEPLOYMENT" \
  "--timeout=$ROLLOUT_TIMEOUT"

pod_rows="$(
  "$KUBECTL" -n "$NAMESPACE" get pods -l "$SELECTOR" \
    -o 'jsonpath={range .items[?(@.status.phase=="Running")]}{.metadata.name}{"|"}{.metadata.deletionTimestamp}{"|"}{range .status.conditions[?(@.type=="Ready")]}{.status}{end}{"\n"}{end}'
)"
POD="$(awk -F '|' '$2 == "" && $3 == "True" { print $1; exit }' <<<"$pod_rows")"
if [ -z "$POD" ]; then
  echo "ERROR: no non-terminating Ready pod matched $SELECTOR" >&2
  exit 1
fi

IMAGE_ID="$(
  "$KUBECTL" -n "$NAMESPACE" get pod "$POD" \
    -o "jsonpath={.status.containerStatuses[?(@.name=='$CONTAINER')].imageID}"
)"
ACTUAL_IMAGE_DIGEST="${IMAGE_ID##*@}"
if [ "$ACTUAL_IMAGE_DIGEST" != "$EXPECTED_IMAGE_DIGEST" ]; then
  echo "ERROR: image digest mismatch for $POD/$CONTAINER" >&2
  echo "expected: $EXPECTED_IMAGE_DIGEST" >&2
  echo "actual:   $IMAGE_ID" >&2
  exit 1
fi

"$KUBECTL" -n "$NAMESPACE" exec "$POD" -c "$CONTAINER" -- \
  /usr/local/bin/ww healthcheck --ready --expect-git-sha "$EXPECTED_GIT_SHA"

echo "verified deployment/$DEPLOYMENT pod=$POD image=$EXPECTED_IMAGE_DIGEST git=$EXPECTED_GIT_SHA"
