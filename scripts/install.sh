#!/bin/sh
# Wetware installer (IPFS-first)
# Usage: curl -sSf https://raw.githubusercontent.com/wetware/ww/master/scripts/install.sh | sh
#   or:  curl -sSf ... | sh -s -- --version <CID>
set -eu

IPNS_NAME="/ipns/releases.wetware.run"
IPNS_TIMEOUT=60
VERSION_CID=""
WW_HOME="${HOME}/.ww"

# --- TTY detection & output helpers ---

IS_TTY=false
if [ -t 1 ]; then
  IS_TTY=true
fi

SPINNER_PID=""

_spinner_frame() {
  case $(($1 % 10)) in
    0) printf '\342\240\213' ;;  # ⠋
    1) printf '\342\240\231' ;;  # ⠙
    2) printf '\342\240\271' ;;  # ⠹
    3) printf '\342\240\270' ;;  # ⠸
    4) printf '\342\240\274' ;;  # ⠼
    5) printf '\342\240\264' ;;  # ⠴
    6) printf '\342\240\246' ;;  # ⠦
    7) printf '\342\240\247' ;;  # ⠧
    8) printf '\342\240\207' ;;  # ⠇
    9) printf '\342\240\217' ;;  # ⠏
  esac
}

# Start a spinner with a message.  Usage: spin "Doing thing..."
spin() {
  if $IS_TTY; then
    _spin_msg="$1"
    (
      i=0
      while true; do
        frame=$(_spinner_frame $i)
        printf '\r  %s %s' "$frame" "$_spin_msg" >&2
        i=$((i + 1))
        sleep 0.1
      done
    ) &
    SPINNER_PID=$!
  else
    printf '%s' "$1" >&2
  fi
}

# Stop spinner and show success.  Usage: spin_ok "Done thing"
spin_ok() {
  if $IS_TTY; then
    if [ -n "$SPINNER_PID" ]; then
      kill "$SPINNER_PID" 2>/dev/null || true
      wait "$SPINNER_PID" 2>/dev/null || true
      SPINNER_PID=""
    fi
    printf '\r\033[K  \342\234\223 %s\n' "$1" >&2
  else
    printf ' ok\n' >&2
  fi
}

# Stop spinner and show failure.  Usage: spin_fail "Failed thing"
spin_fail() {
  if $IS_TTY; then
    if [ -n "$SPINNER_PID" ]; then
      kill "$SPINNER_PID" 2>/dev/null || true
      wait "$SPINNER_PID" 2>/dev/null || true
      SPINNER_PID=""
    fi
    printf '\r\033[K  \342\234\227 %s\n' "$1" >&2
  else
    printf ' FAILED\n' >&2
  fi
}

# Dim warning (single line, non-fatal)
warn() {
  if $IS_TTY; then
    printf '  \033[2m%s\033[0m\n' "$1" >&2
  else
    printf '  %s\n' "$1" >&2
  fi
}

# Fatal error
die() {
  spin_fail "$1"
  shift
  for line in "$@"; do
    printf '  %s\n' "$line" >&2
  done
  exit 1
}

# Clean up spinner on exit
cleanup() {
  if [ -n "$SPINNER_PID" ]; then
    kill "$SPINNER_PID" 2>/dev/null || true
    wait "$SPINNER_PID" 2>/dev/null || true
  fi
  rm -rf "${WW_TMPDIR:-}"
}
trap cleanup EXIT

# --- Parse arguments ---
while [ $# -gt 0 ]; do
  case "$1" in
    --version)
      if [ $# -lt 2 ] || [ -z "$2" ]; then
        echo "Error: --version requires a CID argument"; exit 1
      fi
      VERSION_CID="$2"; shift 2 ;;
    --help)
      echo "Usage: install.sh [--version CID]"
      echo "  --version  Install a specific release by immutable CID"
      echo "             (default: resolve latest via IPNS)"
      exit 0
      ;;
    *) echo "Unknown option: $1"; exit 1 ;;
  esac
done

# --- Detect platform ---
OS="$(uname -s)"
case "$OS" in
  Linux)  OS_NAME="linux" ;;
  Darwin) OS_NAME="macos" ;;
  *) die "Unsupported OS: $OS" "Supported: Linux, macOS" ;;
esac

ARCH="$(uname -m)"
case "$ARCH" in
  x86_64|amd64)  ARCH_NAME="x86_64" ;;
  aarch64|arm64) ARCH_NAME="aarch64" ;;
  *) die "Unsupported architecture: $ARCH" "Supported: x86_64, aarch64" ;;
esac

# --- Check IPFS ---
if ! command -v ipfs >/dev/null 2>&1; then
  die "IPFS not found" \
    "Wetware requires IPFS.  Install Kubo:" \
    "  https://docs.ipfs.tech/install/"
fi

if ! ipfs id >/dev/null 2>&1; then
  die "IPFS daemon not running" \
    "Start it with: ipfs daemon &" \
    "Install Kubo:  https://docs.ipfs.tech/install/"
fi

# --- Header ---
if $IS_TTY; then
  printf '\n\342\232\227\357\270\217  Installing wetware...\n'
else
  printf 'Installing wetware...\n'
fi

# --- Resolve release ---
if [ -n "$VERSION_CID" ]; then
  IPFS_BASE="/ipfs/${VERSION_CID}"
else
  spin "Resolving latest release..."

  RESOLVED_CID=""
  WW_RESOLVE_TMP=$(mktemp /tmp/ww-ipns-resolve.XXXXXXXX)
  ipfs name resolve "$IPNS_NAME" > "$WW_RESOLVE_TMP" 2>/dev/null &
  RESOLVE_PID=$!

  i=0
  while [ $i -lt $IPNS_TIMEOUT ]; do
    if ! kill -0 "$RESOLVE_PID" 2>/dev/null; then
      RESOLVED_CID=$(cat "$WW_RESOLVE_TMP" 2>/dev/null || true)
      break
    fi
    i=$((i + 1))
    sleep 1
  done

  kill "$RESOLVE_PID" 2>/dev/null || true
  wait "$RESOLVE_PID" 2>/dev/null || true
  rm -f "$WW_RESOLVE_TMP"

  if [ -z "$RESOLVED_CID" ]; then
    die "IPNS resolution failed" \
      "Your IPFS node could not resolve releases.wetware.run." \
      "Try again in a few minutes, or install by CID:" \
      "  curl -sSf .../install.sh | sh -s -- --version <CID>" \
      "Release CIDs: https://github.com/wetware/ww/releases"
  fi

  # Validate resolved CID looks like an IPFS path
  case "$RESOLVED_CID" in
    /ipfs/bafy*|/ipfs/Qm*) ;;
    *) die "IPNS resolved to unexpected value" \
         "Got: ${RESOLVED_CID}" \
         "Expected /ipfs/bafy... or /ipfs/Qm..." ;;
  esac

  IPFS_BASE="$RESOLVED_CID"
  spin_ok "Resolved latest release"
fi

WW_TMPDIR=$(mktemp -d)

# --- Fetch binary ---
BIN_PATH="/bin/ww/${OS_NAME}/${ARCH_NAME}/ww"
spin "Fetching binary (${OS_NAME}/${ARCH_NAME})..."

if ! ipfs cat "${IPFS_BASE}${BIN_PATH}" > "${WW_TMPDIR}/ww" 2>/dev/null; then
  die "Could not fetch binary" \
    "No binary for ${OS_NAME}/${ARCH_NAME} in this release." \
    "Download manually: https://github.com/wetware/ww/releases"
fi

spin_ok "Fetched binary (${OS_NAME}/${ARCH_NAME})"

# --- Verify checksum ---
CHECKSUM_ALGO=""
ipfs cat "${IPFS_BASE}/CHECKSUMS.txt" > "${WW_TMPDIR}/CHECKSUMS.txt" 2>/dev/null || true

if [ -f "${WW_TMPDIR}/CHECKSUMS.txt" ] && [ -s "${WW_TMPDIR}/CHECKSUMS.txt" ]; then
  EXPECTED=""
  ACTUAL=""

  # Prefer BLAKE3 if b3sum is available and CHECKSUMS.txt has a blake3 section
  if command -v b3sum >/dev/null 2>&1 && grep -q "^# blake3" "${WW_TMPDIR}/CHECKSUMS.txt"; then
    EXPECTED=$(sed -n '/^# blake3/,/^$/p' "${WW_TMPDIR}/CHECKSUMS.txt" | grep "${BIN_PATH}" | head -1 | awk '{print $1}')
    if [ -n "$EXPECTED" ]; then
      ACTUAL=$(b3sum --no-names "${WW_TMPDIR}/ww")
      CHECKSUM_ALGO="blake3"
    fi
  fi

  # Fall back to SHA-256 (always available on macOS and Linux)
  if [ -z "$CHECKSUM_ALGO" ] && grep -q "^# sha256" "${WW_TMPDIR}/CHECKSUMS.txt"; then
    EXPECTED=$(sed -n '/^# sha256/,/^$/p' "${WW_TMPDIR}/CHECKSUMS.txt" | grep "${BIN_PATH}" | head -1 | awk '{print $1}')
    if [ -n "$EXPECTED" ]; then
      ACTUAL=$(sha256sum "${WW_TMPDIR}/ww" 2>/dev/null || shasum -a 256 "${WW_TMPDIR}/ww")
      ACTUAL=$(echo "$ACTUAL" | awk '{print $1}')
      CHECKSUM_ALGO="sha256"
    fi
  fi

  if [ -n "$CHECKSUM_ALGO" ] && [ "$EXPECTED" != "$ACTUAL" ]; then
    die "Checksum mismatch (${CHECKSUM_ALGO})" \
      "expected: ${EXPECTED}" \
      "got:      ${ACTUAL}" \
      "Download may be corrupted.  Try again or download manually:" \
      "  https://github.com/wetware/ww/releases"
  elif [ -n "$CHECKSUM_ALGO" ]; then
    if $IS_TTY; then
      printf '  \342\234\223 Checksum OK (%s)\n' "$CHECKSUM_ALGO" >&2
    else
      printf 'Checksum OK (%s)\n' "$CHECKSUM_ALGO" >&2
    fi
  else
    warn "Checksum entry not found for ${BIN_PATH}, skipping verification"
  fi
else
  warn "Checksums not available, skipping verification"
fi

# --- Install binary ---
mkdir -p "${WW_HOME}/bin"
mv "${WW_TMPDIR}/ww" "${WW_HOME}/bin/ww"
chmod +x "${WW_HOME}/bin/ww"

# --- Fetch standard library -------------------------------------------------
# WASM cells and glia scripts from the release tree, needed to resolve
# std/ mount paths at runtime (e.g. `ww run std/kernel`).
# IPFS is already verified above — reuse $IPFS_BASE.

fetch_to() {
  _dst="${WW_HOME}/$2"
  mkdir -p "$(dirname "$_dst")"
  if ! ipfs cat "${IPFS_BASE}/$1" > "$_dst" 2>/dev/null; then
    rm -f "$_dst"
    return 1
  fi
}

spin "Fetching standard library..."

STD_OK=true
fetch_to "bin/main.wasm"     "std/kernel/bin/main.wasm"   || STD_OK=false
fetch_to "bin/shell.wasm"    "std/shell/bin/shell.wasm"   || STD_OK=false
fetch_to "bin/shell.capnpc"  "std/shell/bin/shell.capnpc" || STD_OK=false
fetch_to "bin/status.wasm"   "std/status/bin/status.wasm" || STD_OK=false

# Glia stdlib (enumerate directory, fetch each file)
mkdir -p "${WW_HOME}/std/lib/ww"
for _name in $(ipfs ls "${IPFS_BASE}/lib/ww" 2>/dev/null | awk '{print $NF}'); do
  fetch_to "lib/ww/${_name}" "std/lib/ww/${_name}" || true
done

if $STD_OK; then
  spin_ok "Fetched standard library"
else
  spin_fail "Some standard library files could not be fetched"
  warn "The binary is installed but std/ mounts may not resolve."
  warn "You can still use IPFS paths directly: ww run /ipfs/<CID>"
fi

# --- Full node setup (identity, namespace, daemon, MCP, PATH) ---
printf '\n'
if ! "${WW_HOME}/bin/ww" perform install; then
  warn "Some setup steps failed.  You can retry with:"
  printf '  %s/bin/ww perform install\n' "$WW_HOME"
fi

# --- Wait for the daemon to answer /status ---
# `ww perform install` registers and starts the daemon (launchd / systemd),
# but the daemon takes a few seconds to bind, evaluate init.d, and serve
# the status route. The install script owns this UX because it's the
# cold-install entry point; `ww perform install` itself can't see the
# daemon's stdout (launchd / systemd redirect it to ~/.ww/logs/ww.log).
status_url="http://127.0.0.1:2080/status"
printf '\n'
printf 'Waiting for daemon... '
for _ in 1 2 3 4 5 6 7 8 9 10; do
  if curl --silent --show-error --max-time 1 --output /dev/null "$status_url" 2>/dev/null; then
    printf 'ready.\n'
    printf 'Try: curl %s\n' "$status_url"
    exit 0
  fi
  sleep 1
done
printf 'timed out.\n'
warn "Daemon didn't answer $status_url within 10s."
warn "Check logs: tail -f ${WW_HOME}/logs/ww.log"
warn "Or hit it manually once it's up: curl $status_url"
