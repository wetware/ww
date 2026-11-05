#!/bin/sh
# Wetware installer (IPFS-first)
# Usage: curl -sSf https://raw.githubusercontent.com/wetware/ww/master/scripts/install.sh | sh
#   or:  curl -sSf ... | sh -s -- --version <CID>
set -eu
export LC_ALL=C

IPNS_NAME="/ipns/releases.wetware.run"
IPNS_TIMEOUT=60
VERSION_CID=""
TRUSTED_RELEASE_KEYS=/etc/wetware/release-keys
VERIFY_MANIFEST=""
VERIFY_SIGNATURE=""
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
    printf ' FAILED: %s\n' "$1" >&2
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
    --trusted-release-keys|--verify-manifest|--signature)
      [ $# -ge 2 ] && [ -n "$2" ] || die "$1 requires a file argument"
      case "$1" in
        --trusted-release-keys) TRUSTED_RELEASE_KEYS=$2 ;;
        --verify-manifest) VERIFY_MANIFEST=$2 ;;
        --signature) VERIFY_SIGNATURE=$2 ;;
      esac
      shift 2 ;;
    --help)
      echo "Usage: install.sh [--version CID] [--trusted-release-keys FILE]"
      echo "       install.sh --verify-manifest FILE --signature FILE [--trusted-release-keys FILE]"
      echo "  Trust keys: independently provisioned /etc/wetware/release-keys by default"
      echo "  --version  Install a specific release by immutable CID"
      echo "             (default: resolve latest via IPNS)"
      exit 0
      ;;
    *) echo "Unknown option: $1"; exit 1 ;;
  esac
done

# Match verify_strict small-order rejection. These are the compressed Y
# coordinates in curve25519-dalek EIGHT_TORSION, plus field aliases p and p+1.
reject_weak_point() {
  point_hex=$1
  point_prefix=${point_hex%??}
  point_last=${point_hex#"$point_prefix"}
  point_y="$point_prefix$(printf '%02x' "$((0x$point_last & 127))")"
  case "$point_y" in
    0000000000000000000000000000000000000000000000000000000000000000|\
    0100000000000000000000000000000000000000000000000000000000000000|\
    26e8958fc2b227b045c3f489f2ef98f0d5dfac05d3c63339b13802886d53fc05|\
    c7176a703d4dd84fba3c0b760d10670f2a2053fa2c39ccc64ec7fd7792ac037a|\
    ecffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff7f|\
    edffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff7f|\
    eeffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff7f)
      die "Weak Ed25519 release point" ;;
  esac
}

# Authenticate exact bytes before parsing any checksum. This verification mode
# also serves the publisher; the downloaded installer is never a trust anchor.
verify_manifest() {
  manifest=$1
  signature=$2
  command -v openssl >/dev/null 2>&1 || die "OpenSSL 3 or later is required"
  case "$(openssl version)" in
    'OpenSSL 3.'*|'OpenSSL 4.'*) ;;
    *) die "OpenSSL 3 or later with Ed25519 support is required" ;;
  esac
  [ -f "$manifest" ] && [ -s "$manifest" ] || die "Missing release manifest"
  [ "$(wc -c < "$manifest")" -le 1048576 ] || die "Release manifest exceeds 1 MiB"
  [ -f "$signature" ] && [ "$(wc -c < "$signature")" -eq 64 ] || die "Missing or malformed Ed25519 signature"
  [ -f "$TRUSTED_RELEASE_KEYS" ] || die "Provision trusted release keys independently: $TRUSTED_RELEASE_KEYS"
  # Some awk implementations discard NUL bytes. Reject non-ASCII input before
  # awk so the shell and native parsers interpret identical manifest bytes.
  tr -cd '\12\40-\176' < "$manifest" > "$WW_TMPDIR/ascii-manifest"
  cmp -s "$manifest" "$WW_TMPDIR/ascii-manifest" || die "Manifest must contain printable ASCII and LF only"
  # Snapshot local trust into the private staging directory. A release cannot
  # supply or override this file. Limit rotation overlap to two keys.
  [ "$(wc -c < "$TRUSTED_RELEASE_KEYS")" -le 122 ] || die "Invalid release keyring size"
  cp "$TRUSTED_RELEASE_KEYS" "$WW_TMPDIR/trusted-keys"
  tr -cd '\12\40-\176' < "$WW_TMPDIR/trusted-keys" > "$WW_TMPDIR/ascii-keys"
  cmp -s "$WW_TMPDIR/trusted-keys" "$WW_TMPDIR/ascii-keys" || die "Malformed release keyring"
  awk '
    length($0) != 60 || $0 !~ /^MCowBQYDK2VwAyEA[A-Za-z0-9+\/]+=$/ || seen[$0]++ { exit 1 }
    END { if (NR < 1 || NR > 2) exit 1 }
  ' "$WW_TMPDIR/trusted-keys" || die "Malformed release keyring"
  [ "$(tail -c 1 "$WW_TMPDIR/trusted-keys" | od -An -tu1 | tr -d ' ')" = 10 ] || die "Keyring must end with LF"
  signature_r=$(od -An -N32 -v -tx1 "$signature" | tr -d ' \n')
  reject_weak_point "$signature_r"
  authenticated=false
  while IFS= read -r key; do
    printf '%s' "$key" | openssl base64 -d -A > "$WW_TMPDIR/key.der" || die "Malformed release key"
    canonical=$(openssl base64 -A < "$WW_TMPDIR/key.der")
    [ "$key" = "$canonical" ] || die "Noncanonical release key"
    der_hex=$(od -An -v -tx1 "$WW_TMPDIR/key.der" | tr -d ' \n')
    reject_weak_point "${der_hex#302a300506032b6570032100}"
    if openssl pkeyutl -verify -pubin -keyform DER -inkey "$WW_TMPDIR/key.der" \
        -rawin -in "$manifest" -sigfile "$signature" >/dev/null 2>&1; then
      authenticated=true
    fi
  done < "$WW_TMPDIR/trusted-keys"
  "$authenticated" || die "Release manifest authentication failed"
  [ "$(tail -c 1 "$manifest" | od -An -tu1 | tr -d ' ')" = 10 ] || die "Manifest must end with LF"
  # Reject unknown digest algorithms and ambiguous paths, including entries
  # outside the selected platform. No legacy digest inference is permitted.
  awk '
    NR == 1 { if ($0 != "# sha256") exit 1; next }
    {
      hash=substr($0, 1, 64); path=substr($0, 67)
      if (length(hash) != 64 || hash ~ /[^0-9a-f]/ || substr($0,65,2) != "  " ||
          path == "" || path ~ /[^A-Za-z0-9._\/-]/ || seen[path]++) exit 1
      n=split(path, parts, "/")
      for (i=1; i<=n; i++) if (parts[i] == "" || parts[i] == "." || parts[i] == "..") exit 1
    }
    END { if (NR < 2) exit 1 }
  ' "$manifest" || die "Malformed SHA-256 release manifest"
}

if [ -n "$VERIFY_MANIFEST" ] || [ -n "$VERIFY_SIGNATURE" ]; then
  [ -n "$VERIFY_MANIFEST" ] && [ -n "$VERIFY_SIGNATURE" ] && [ -z "$VERSION_CID" ] || die "Verification requires manifest and signature only"
  WW_TMPDIR=$(mktemp -d)
  verify_manifest "$VERIFY_MANIFEST" "$VERIFY_SIGNATURE"
  exit 0
fi

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

if ! ipfs cat "${IPFS_BASE}/CHECKSUMS.txt.sig" > "${WW_TMPDIR}/CHECKSUMS.txt.sig"; then
  die "Could not fetch required CHECKSUMS.txt.sig"
fi
verify_manifest "${WW_TMPDIR}/CHECKSUMS.txt" "${WW_TMPDIR}/CHECKSUMS.txt.sig"

fetch_verified() {
  package_path="$1"
  staged_path="$2"
  if ! ipfs cat "${IPFS_BASE}/${package_path}" > "$staged_path"; then
    die "Could not fetch mandatory payload: ${package_path}"
  fi
  [ -s "$staged_path" ] || die "Mandatory payload is empty: ${package_path}"
  expected=$(awk -v path="$package_path" '
    NR > 1 && $2 == path { count++; hash=$1 }
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
