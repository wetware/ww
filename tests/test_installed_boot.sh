#!/usr/bin/env bash
# Fail closed: this acceptance requires a real Kubo and real Host artifacts.
set -euo pipefail
exec python3 "$(dirname "$0")/test_installed_boot.py" "$@"
