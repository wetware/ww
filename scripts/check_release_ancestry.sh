#!/usr/bin/env bash
# Decide whether a Git revision may advance the installer-facing IPNS release.
#
# Prints "publish" when the candidate is a master commit newer than the
# currently published revision, and "skip" when it is already current.
# Stale, divergent, malformed, or non-master candidates fail closed.

set -euo pipefail

candidate="${1:-}"
current="${2:-}"
master_ref="${WW_RELEASE_MASTER_REF:-origin/master}"

die() {
  echo "release-ancestry: ERROR: $*" >&2
  exit 1
}

valid_revision() {
  [[ "$1" =~ ^[0-9a-f]{40}$ ]]
}

valid_revision "$candidate" \
  || die "candidate revision must be a full lowercase Git SHA: ${candidate:-<empty>}"
git cat-file -e "$candidate^{commit}" 2>/dev/null \
  || die "candidate revision is not available as a commit: $candidate"
git rev-parse --verify "$master_ref^{commit}" >/dev/null 2>&1 \
  || die "master reference is not available: $master_ref"

# A workflow_dispatch run can target any branch. Only commits contained in
# master may move the production release pointer.
if ! git merge-base --is-ancestor "$candidate" "$master_ref"; then
  die "candidate revision is not contained in $master_ref: $candidate"
fi

# Migration path for the first release carrying REVISION metadata.
if [ -z "$current" ]; then
  echo "publish"
  exit 0
fi

valid_revision "$current" \
  || die "published revision is malformed: $current"
git cat-file -e "$current^{commit}" 2>/dev/null \
  || die "published revision is not available as a commit: $current"

if [ "$candidate" = "$current" ]; then
  echo "skip"
elif git merge-base --is-ancestor "$current" "$candidate"; then
  echo "publish"
elif git merge-base --is-ancestor "$candidate" "$current"; then
  die "candidate revision is older than the published revision: $candidate < $current"
else
  die "candidate revision diverges from the published revision: $candidate !~ $current"
fi
