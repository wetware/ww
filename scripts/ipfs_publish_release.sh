#!/usr/bin/env bash
# Historical kubectl/pin-pruning publication is retired in this checkout.
set -euo pipefail
printf '%s\n' 'Official release publication requires ww-release-publisher through the restricted infra worker. Production publication remains on hold; see doc/release-publication.md.' >&2
exit 1
