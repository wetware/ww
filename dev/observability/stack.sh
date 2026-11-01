#!/bin/sh
set -eu
cd "$(dirname "$0")/../.."
mkdir -p .context/observability
touch .context/observability/runtime.jsonl
exec docker compose -f dev/observability/compose.yaml "$@"
