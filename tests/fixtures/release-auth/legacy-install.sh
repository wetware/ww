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

# Stage the complete package before touching installed state. SHA-256 is the
# release contract; missing or duplicate entries are errors.
BIN_PATH="bin/ww/${OS_NAME}/${ARCH_NAME}/ww"
mkdir -p "${WW_TMPDIR}/fhs/boot" "${WW_TMPDIR}/fhs/bin"
if ! ipfs cat "${IPFS_BASE}/CHECKSUMS.txt" > "${WW_TMPDIR}/CHECKSUMS.txt"; then
  die "Could not fetch required CHECKSUMS.txt"
fi

fetch_verified() {
  package_path="$1"
  staged_path="$2"
  if ! ipfs cat "${IPFS_BASE}/${package_path}" > "$staged_path"; then
    die "Could not fetch mandatory payload: ${package_path}"
  fi
  [ -s "$staged_path" ] || die "Mandatory payload is empty: ${package_path}"
  expected=$(awk -v path="$package_path" '
    /^# sha256$/ { sha=1; next }
    /^#/ { sha=0 }
    sha && ($2 == path || $2 == "/" path) { count++; hash=$1 }
    END { if (count == 1) print hash; else exit 1 }
  ' "${WW_TMPDIR}/CHECKSUMS.txt") || die "Missing or duplicate SHA-256 entry: ${package_path}"
  case "$expected" in *[!0-9a-f]*|'') die "Invalid SHA-256 entry: ${package_path}" ;; esac
  [ "${#expected}" -eq 64 ] || die "Invalid SHA-256 length: ${package_path}"
  if command -v sha256sum >/dev/null 2>&1; then
    actual=$(sha256sum "$staged_path" | awk '{print $1}')
  else
    actual=$(shasum -a 256 "$staged_path" | awk '{print $1}')
  fi
  [ "$actual" = "$expected" ] || die "Checksum mismatch: ${package_path}"
}

spin "Fetching verified Host and FHS payloads..."
fetch_verified "$BIN_PATH" "${WW_TMPDIR}/ww"
fetch_verified "fhs/boot/kernel.wasm" "${WW_TMPDIR}/fhs/boot/kernel.wasm"
fetch_verified "fhs/bin/status.wasm" "${WW_TMPDIR}/fhs/bin/status.wasm"
chmod +x "${WW_TMPDIR}/ww"
spin_ok "Fetched verified package"

# The candidate validates components, identity, ownership and configuration.
# Its installer repeats preflight and uses atomic replacement for each file.
# Multi-file installation is recoverable by rerunning this installer, not atomic.
"${WW_TMPDIR}/ww" perform install --check \
  --payload "${WW_TMPDIR}/fhs" --executable "${WW_HOME}/bin/ww"
"${WW_TMPDIR}/ww" perform install \
  --payload "${WW_TMPDIR}/fhs" --executable "${WW_HOME}/bin/ww"
"${WW_HOME}/bin/ww" --version
printf '\nInstalled Host and FHS. Use the service activation command printed above.\n'
