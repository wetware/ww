#!/usr/bin/env bash
set -euo pipefail

script_dir="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
repo_root="$(cd -- "$script_dir/.." && pwd)"
cd "$repo_root"

wasm_tools="${WASM_TOOLS:-wasm-tools}"
command_artifacts=(
  std/kernel/bin/main.wasm
  std/status/bin/status.wasm
  examples/chess/bin/chess-demo.wasm
  examples/counter/bin/counter.wasm
  examples/discovery/bin/discovery.wasm
  examples/echo/bin/echo.wasm
  examples/oracle/bin/oracle.wasm
  examples/snap-hello-rs/bin/snap-hello-rs.wasm
  target/authority-probe/wasm32-wasip3/release/authority_probe.wasm
)

for artifact in "${command_artifacts[@]}"; do
  if [[ ! -s "$artifact" ]]; then
    echo "first-party P3 artifact is missing or empty: $artifact" >&2
    exit 1
  fi

  "$wasm_tools" validate --features all "$artifact"
  component_wit="$("$wasm_tools" component wit "$artifact")"

  if printf '%s\n' "$component_wit" | grep -Eq 'wasi:[^[:space:];]*@0\.2\.'; then
    echo "$artifact contains a WASI 0.2 reference" >&2
    exit 1
  fi
  if printf '%s\n' "$component_wit" | grep -Eq '(^|[[:space:]])wasi:sockets[/@:]'; then
    echo "$artifact unexpectedly imports WASI sockets" >&2
    exit 1
  fi
  if ! printf '%s\n' "$component_wit" | grep -Eq \
    '^[[:space:]]*export[[:space:]]+wasi:cli/run@0\.3\.0;'; then
    echo "$artifact does not export wasi:cli/run@0.3.0" >&2
    exit 1
  fi
  if ! printf '%s\n' "$component_wit" | grep -Eq \
    '^[[:space:]]*run:[[:space:]]+async[[:space:]]+func\(\)[[:space:]]+->[[:space:]]+result;'; then
    echo "$artifact does not declare an asynchronous P3 command entry point" >&2
    exit 1
  fi

  echo "$artifact has the expected P3 command architecture"
done

routing_probe=target/routing-key-probe/wasm32-wasip3/release/routing_key_probe.wasm
if [[ ! -s "$routing_probe" ]]; then
  echo "first-party P3 artifact is missing or empty: $routing_probe" >&2
  exit 1
fi
"$wasm_tools" validate --features all "$routing_probe"
routing_wit="$("$wasm_tools" component wit "$routing_probe")"
if printf '%s\n' "$routing_wit" | grep -Eq 'wasi:[^[:space:];]*@0\.2\.'; then
  echo "$routing_probe contains a WASI 0.2 reference" >&2
  exit 1
fi
if printf '%s\n' "$routing_wit" | grep -Eq '(^|[[:space:]])wasi:sockets[/@:]'; then
  echo "$routing_probe unexpectedly imports WASI sockets" >&2
  exit 1
fi
echo "$routing_probe has the expected P3 fixture architecture"

echo_wit="$("$wasm_tools" component wit examples/echo/bin/echo.wasm)"
if printf '%s\n' "$echo_wit" | grep -q 'wetware:transport/'; then
  echo "the synchronous echo component unexpectedly imports Wetware transport" >&2
  exit 1
fi

if grep -Eq '^[[:space:]]*(capnp-rpc|system|tokio)[[:space:]]*=' examples/echo/Cargo.toml; then
  echo "the synchronous echo manifest unexpectedly depends on RPC, system, or Tokio" >&2
  exit 1
fi

echo "synchronous echo component is runtime-free"
