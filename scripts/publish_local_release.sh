#!/usr/bin/env bash
# A developer Host must not publish official Wetware releases.
set -euo pipefail
printf '%s\n' 'Official release publication requires ww-release-publisher through the restricted infra worker. Use prepare/finalize/verify for local qualification; production publication remains on hold.' >&2
exit 1
