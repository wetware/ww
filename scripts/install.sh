#!/bin/sh
# Parse the complete installer before starting a piped installation.
{
# Independently distributed installer. Review these R1 values together when
# qualifying a new bootstrap release. Never load them from the release tree.
BOOTSTRAP_CID=bafybeihdekx4kiimojt7l3qcl52xwrwiuukst4556ongnacbo4ulj4aqpi
RELEASE_IPNS=k51qzi5uqu5dgbw6eyddak3ozm2p2zwet8skgn8qzbwzb90d5obsnnwimvp14i
BOOTSTRAP_KEY=MCowBQYDK2VwAyEAwZP5U1kng72CiZOgyDgkv8hiAshVDNxsf7PAunCO1EY=
set -eu
export LC_ALL=C

RELEASE_CID=""
RECOVER=false
RECOVERY_TIMEOUT_SECS=""
BOOTSTRAP_IPFS_URL=${IPFS_API:-http://localhost:5001}
KEYS_EXPLICIT=false
TRUSTED_RELEASE_KEYS=/etc/wetware/release-keys
VERIFY_MANIFEST=""
VERIFY_SIGNATURE=""
WW_HOME="${HOME}/.ww"
# The cleanup trap owns only directories created by this invocation.
WW_TMPDIR=""

# --- TTY detection & output helpers ---

# Kubo owns live transfer output. Each shell message ends on its own line.
show_progress=false
if [ -t 2 ] && [ "${TERM:-dumb}" != dumb ] && [ -z "${NO_COLOR+x}" ]; then
  # Kubo truncates the rate and percentage when its counters fill the row.
  size=$(stty size <&2 2>/dev/null) || size=""
  columns=${size##* }
  case "$columns" in
    ''|*[!0-9]*) ;;
    *) if [ "$columns" -ge 64 ]; then show_progress=true; fi ;;
  esac
fi
if ! "$show_progress"; then
  # Kubo's diagnostic logger selects color independently of its transfer bar.
  case "${GOLOG_LOG_FMT:-${IPFS_LOGGING_FMT:-}}" in
    json) ;;
    *) export GOLOG_LOG_FMT=nocolor ;;
  esac
fi

status() {
  if "$show_progress"; then
    # The tab aligns text after either a one- or two-column heading icon.
    printf '%s\t%s\n' "$1" "$2" >&2
  else
    printf '%s\n' "$2" >&2
  fi
}

die() {
  printf 'Error: %s\n' "$1" >&2
  shift
  for line in "$@"; do
    printf '  %s\n' "$line" >&2
  done
  exit 1
}

cleanup() {
  rm -rf "${WW_TMPDIR:-}"
}
trap cleanup EXIT

# Bound only the returned metadata. A process-wide file size limit also
# affects unrelated writes by the Kubo CLI, including diagnostics.
fetch_bounded() {
  python3 -I - "$@" <<'PYTHON'
import subprocess
import sys

limit = int(sys.argv[1])
with subprocess.Popen(["ipfs", *sys.argv[2:]], stdout=subprocess.PIPE) as command:
    data = command.stdout.read(limit + 1)
    if len(data) > limit:
        command.kill()
        sys.exit("Kubo response exceeds metadata size limit")
    if command.wait() != 0:
        sys.exit(1)
sys.stdout.buffer.write(data)
PYTHON
}

# Fetch the wire record through the configured Kubo CLI. Inspect it with the
# local Kubo executable in a private, empty repo path, without a daemon.
discover_release() {
  fetch_bounded 10240 name get "$RELEASE_IPNS" > "$WW_TMPDIR/ipns-record" \
    || die "Cannot fetch ww-release. Check Kubo connectivity and support for ipfs name get."
  [ -s "$WW_TMPDIR/ipns-record" ] && [ "$(wc -c < "$WW_TMPDIR/ipns-record")" -le 10240 ] \
    || die "Invalid ww-release IPNS record size"
  IPFS_PATH="$WW_TMPDIR/inspect-repo" ipfs --offline name inspect \
    --verify="$RELEASE_IPNS" --dump=false --enc=json < "$WW_TMPDIR/ipns-record" \
    > "$WW_TMPDIR/ipns-inspection" || die "Cannot inspect ww-release IPNS record"

  # A successful inspect command can report an invalid signature or expired
  # record. Read the verdict and target from the same local inspection.
  python3 -I - "$RELEASE_IPNS" "$WW_TMPDIR/ipns-inspection" <<'PY' || die "Invalid ww-release IPNS record or release CID"
import json
import re
import sys

with open(sys.argv[2], encoding="utf-8") as inspection:
    result = json.load(inspection)
verdict = result["Validation"]
if verdict["Valid"] is not True or verdict["Name"] != sys.argv[1]:
    sys.exit(1)
# CIDv1, DAG-PB, SHA-256 with a 32-byte digest, canonical lowercase base32.
target = re.fullmatch(r"/ipfs/(bafybei[a-h][a-z2-7]{50}[aeimquy4])", result["Entry"]["Value"])
if target is None:
    sys.exit(1)
print(target[1])
PY
}

# --- Parse arguments ---
while [ $# -gt 0 ]; do
  case "$1" in
    --release)
      if [ $# -lt 2 ] || [ -z "$2" ]; then
        die "--release requires a CID argument"
      fi
      RELEASE_CID="$2"; shift 2 ;;
    --recover) RECOVER=true; shift ;;
    --trusted-release-keys|--verify-manifest|--signature|--ipfs-url|--timeout-secs)
      [ $# -ge 2 ] && [ -n "$2" ] || die "$1 requires an argument"
      case "$2" in --*) die "$1 requires an argument" ;; esac
      case "$1" in
        --trusted-release-keys) TRUSTED_RELEASE_KEYS=$2; KEYS_EXPLICIT=true ;;
        --verify-manifest) VERIFY_MANIFEST=$2 ;;
        --signature) VERIFY_SIGNATURE=$2 ;;
        --ipfs-url) BOOTSTRAP_IPFS_URL=$2 ;;
        --timeout-secs) RECOVERY_TIMEOUT_SECS=$2 ;;
      esac
      shift 2 ;;
    --help)
      echo "Usage: install.sh [--release CID] [--ipfs-url URL] [--trusted-release-keys FILE]"
      echo "       install.sh --recover --timeout-secs N [--release CID] [--ipfs-url URL] [--trusted-release-keys FILE]"
      echo "       install.sh --verify-manifest FILE --signature FILE [--trusted-release-keys FILE]"
      echo "  Trust keys: --trusted-release-keys FILE or /etc/wetware/release-keys"
      echo "  --release  Install a specific release by immutable CID"
      echo "  Without --release, discover the release from the signed ww-release IPNS record."
      echo "  Only this mode uses the embedded R1 key when no keyring is configured."
      echo "  --recover  Recover only the pending native Host transaction"
      echo "  --ipfs-url Native installation/recovery Kubo HTTP endpoint (default: IPFS_API or http://localhost:5001)"
      echo "  --timeout-secs Positive native recovery operation budget; required with --recover"
      exit 0
      ;;
    *) die "Unknown option: $1" ;;
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

# The public package builder stores the <=1 MiB manifest as one raw block.
# Bind that block to the requested root independently of Kubo before executing
# any downloaded Host. This is a flat directory membership check, not a general
# IPLD decoder or a substitute for native typed-descriptor verification.
verify_release_binding() {
  if ! ipfs block get "$RELEASE_CID" > "$WW_TMPDIR/release-root"; then
    die "Could not fetch release root block"
  fi
  [ "$(wc -c < "$WW_TMPDIR/release-root")" -le 2097152 ] \
    || die "Release root exceeds 2 MiB"
  # Encode the fixed CIDv1 / DAG-PB / SHA-256 profile locally. Comparing the
  # entire canonical string also rejects other codecs and nonzero base32 padding.
  printf '\001\160\022\040' > "$WW_TMPDIR/root-cid"
  openssl dgst -sha256 -binary "$WW_TMPDIR/release-root" >> "$WW_TMPDIR/root-cid" \
    || die "Could not hash release root"
  od -An -v -tu1 "$WW_TMPDIR/root-cid" > "$WW_TMPDIR/root-cid-bytes" \
    || die "Could not read release root CID"
  actual_cid=$(awk '
    BEGIN { alphabet="abcdefghijklmnopqrstuvwxyz234567"; printf "b" }
    { for (i=1; i<=NF; i++) {
        bits+=8; value=value*256+$i
        while (bits>=5) {
          bits-=5; scale=2^bits
          printf "%s", substr(alphabet, int(value/scale)+1, 1)
          value%=scale
        }
      }
    }
    END { if (bits) printf "%s", substr(alphabet, value*2^(5-bits)+1, 1) }
  ' "$WW_TMPDIR/root-cid-bytes") || die "Could not encode release root CID"
  [ "$actual_cid" = "$RELEASE_CID" ] || die "Release root CID mismatch"
  openssl dgst -sha256 -binary "$WW_TMPDIR/CHECKSUMS.txt" > "$WW_TMPDIR/manifest-digest" \
    || die "Could not hash release manifest"
  manifest_digest=$(od -An -v -tx1 "$WW_TMPDIR/manifest-digest" | tr -d ' \n')
  od -An -v -tu1 "$WW_TMPDIR/release-root" > "$WW_TMPDIR/root-bytes" \
    || die "Could not read release root"
  # Accept only the producer profile: sorted unique ASCII names, canonical
  # length varints, exact Hash/Name/Tsize fields, and plain-directory Data.
  # Save the direct descriptor link for comparison with the signed binding.
  awk -v digest="$manifest_digest" -v size="$(wc -c < "$WW_TMPDIR/CHECKSUMS.txt")" '
    function fail() { exit 1 }
    function number(end,    value,scale,b,count) {
      value=0; scale=1
      for (count=1; count<=4; count++) {
        if (pos>end) fail()
        b=bytes[pos++]; value+=(b%128)*scale
        if (value>2097152) fail()
        if (b<128) { if (count>1 && b==0) fail(); return value }
        scale*=128
      }
      fail()
    }
    function field(tag,end,    field_length) {
      if (pos>end || bytes[pos++]!=tag) fail()
      field_length=number(end)
      if (field_length>end-pos+1) fail()
      return pos+field_length-1
    }
    # Validate u64 Tsize without rounding large values in awk. Only a small
    # raw manifest size is interpreted; unrelated artifact sizes stay opaque.
    function tsize(end,    count,b,value,scale) {
      value=0; scale=1
      for (count=1; count<=10; count++) {
        if (pos>end) fail()
        b=bytes[pos++]
        if (count==10 && b>1) fail()
        if (count<=4) { value+=(b%128)*scale; scale*=128 }
        if (b<128) {
          if (count>1 && b==0) fail()
          return count<=4 ? value : -1
        }
      }
      fail()
    }
    { for (i=1; i<=NF; i++) bytes[++total]=$i }
    END {
      pos=1
      while (pos<=total) {
        if (bytes[pos]==10) {
          if (data++) fail()
          data_end=field(10,total)
          if (data_end!=pos+1 || bytes[pos]!=8 || bytes[pos+1]!=1) fail()
          pos+=2
          if (links && pos<=total) fail()
        } else {
          if (++links>1024) fail()
          end=field(18,total)
          hash_end=field(10,end); hash=""
          if (hash_end-pos+1!=36 || bytes[pos]!=1 || bytes[pos+2]!=18 || bytes[pos+3]!=32 ||
              (bytes[pos+1]!=85 && bytes[pos+1]!=112 && bytes[pos+1]!=113)) fail()
          while (pos<=hash_end) hash=hash sprintf("%02x",bytes[pos++])
          name_end=field(18,end); name=""
          while (pos<=name_end) {
            if (bytes[pos]<32 || bytes[pos]>126) fail()
            name=name sprintf("%c",bytes[pos++])
          }
          if (name=="" || name=="." || name==".." || name~/[^A-Za-z0-9._-]/ ||
              ("x" name)<=("x" previous)) fail()
          previous=name
          if (pos>end || bytes[pos++]!=24) fail()
          link_size=tsize(end)
          if (pos!=end+1) fail()
          if (name=="CHECKSUMS.txt") {
            if (hash!="01551220" digest || link_size!=size) fail()
            matched=1
          }
          if (name=="release-descriptor") print hash, link_size
        }
      }
      if (data!=1 || !matched) fail()
    }
  ' "$WW_TMPDIR/root-bytes" > "$WW_TMPDIR/descriptor-link" || die "Invalid release root or manifest binding"
}

if ! "$RECOVER" && [ -n "$RECOVERY_TIMEOUT_SECS" ]; then
  die "--timeout-secs requires --recover"
fi

if "$RECOVER"; then
  [ -z "$VERIFY_MANIFEST" ] && [ -z "$VERIFY_SIGNATURE" ] \
    || die "cannot combine --recover with manifest verification"
  [ -n "$RECOVERY_TIMEOUT_SECS" ] \
    || die "--recover requires a positive --timeout-secs N before recovery transport or mutation"
  # Compare decimal text without shell overflow or floating-point rounding.
  printf '%s\n' "$RECOVERY_TIMEOUT_SECS" | awk '
    /^[0-9]+$/ {
      value=$0; sub(/^0+/, "", value)
      if (value != "" && (length(value)<10 ||
          (length(value)==10 && "x" value <= "x4294967295"))) valid=1
    }
    END { exit !(NR == 1 && valid) }
  ' || die "--timeout-secs requires a positive unsigned 32-bit integer"
  [ -d "$WW_HOME" ] && [ ! -L "$WW_HOME" ] \
    && [ -f "$WW_HOME/host-upgrade.json" ] && [ ! -L "$WW_HOME/host-upgrade.json" ] \
    || die "Recovery requires a pending Host upgrade in a regular host-upgrade.json file."
  # This locator grants no authority. The native recovery entry point checks
  # the authenticated release against the exact pending generation under lock.
  # Accept only the compact JSON emitted by InstalledRelease. Fail closed on
  # alternative encodings instead of adding a second general JSON parser.
  if [ -z "$RELEASE_CID" ]; then
    pointer="$WW_HOME/installed-release.json"
    [ -f "$pointer" ] && [ ! -L "$pointer" ] \
      || die "Recovery release locator is unavailable; supply --release CID for the recorded generation."
    pointer_size=$(wc -c < "$pointer")
    [ "$pointer_size" -le 1024 ] \
      || die "Recovery release locator is invalid; supply --release CID for the recorded generation."
    pointer_value=$(cat "$pointer")
    [ "${#pointer_value}" -eq "$pointer_size" ] \
      || die "Recovery release locator is noncanonical; supply --release CID for the recorded generation."
    RELEASE_CID=$(printf '%s' "$pointer_value" | sed -n 's/^{"version":1,"release":"\([A-Za-z0-9][A-Za-z0-9]*\)"}$/\1/p')
    [ -n "$RELEASE_CID" ] && [ "$pointer_value" = "{\"version\":1,\"release\":\"$RELEASE_CID\"}" ] \
      || die "Recovery release locator is invalid; supply --release CID for the recorded generation."
  fi
fi

if [ -n "$VERIFY_MANIFEST" ] || [ -n "$VERIFY_SIGNATURE" ]; then
  [ -n "$VERIFY_MANIFEST" ] && [ -n "$VERIFY_SIGNATURE" ] && [ -z "$RELEASE_CID" ] || die "Verification requires manifest and signature only"
  WW_TMPDIR=$(mktemp -d)
  verify_manifest "$VERIFY_MANIFEST" "$VERIFY_SIGNATURE"
  exit 0
fi

if [ -n "$RELEASE_CID" ]; then
  printf '%s\n' "$RELEASE_CID" | awk '
    length($0) == 59 && /^bafybei[a-z2-7]+$/ { valid=1 }
    END { exit !(NR == 1 && valid) }
  ' || die "--release requires one canonical CIDv1 DAG-PB SHA-256 release root, without a path or query."
elif [ -e "$WW_HOME/release-consumer" ] || [ -L "$WW_HOME/release-consumer" ]; then
  die "Existing release consumer state requires native channel verification. Use ww upgrade --check."
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
if "$RECOVER"; then
  status '⚗️' 'Recovering the recorded Host transaction...'
else
  status '⚗️' 'Installing wetware...'
fi

# --- Select immutable release ---
WW_TMPDIR=$(mktemp -d)
expected_channel=""
if [ -z "$RELEASE_CID" ]; then
  expected_channel=$RELEASE_IPNS
  # Resolve and validate the official signed record before selecting any bytes.
  RELEASE_CID=$(discover_release) || die "Could not authenticate ww-release IPNS record"
  if ! "$KEYS_EXPLICIT" && [ ! -e "$TRUSTED_RELEASE_KEYS" ] && [ ! -L "$TRUSTED_RELEASE_KEYS" ]; then
    key_directory=${TRUSTED_RELEASE_KEYS%/*}
    if [ -e "$key_directory" ] || [ -L "$key_directory" ]; then
      [ -d "$key_directory" ] && [ -x "$key_directory" ] || die "Cannot inspect $key_directory"
    fi
    TRUSTED_RELEASE_KEYS=$WW_TMPDIR/bootstrap-key
    (umask 077 && printf '%s\n' "$BOOTSTRAP_KEY" > "$TRUSTED_RELEASE_KEYS")
  fi
fi
IPFS_BASE="/ipfs/${RELEASE_CID}"

# Authenticate all staged bytes before executing the selected Host.
BIN_PATH="bin/ww/${OS_NAME}/${ARCH_NAME}/ww"
if ! ipfs cat "${IPFS_BASE}/CHECKSUMS.txt" --progress="$show_progress" > "${WW_TMPDIR}/CHECKSUMS.txt"; then
  die "Could not fetch required CHECKSUMS.txt"
fi

if ! ipfs cat "${IPFS_BASE}/CHECKSUMS.txt.sig" --progress="$show_progress" > "${WW_TMPDIR}/CHECKSUMS.txt.sig"; then
  die "Could not fetch required CHECKSUMS.txt.sig"
fi
verify_manifest "${WW_TMPDIR}/CHECKSUMS.txt" "${WW_TMPDIR}/CHECKSUMS.txt.sig"
verify_release_binding

fetch_verified() {
  package_path="$1"
  staged_path="$2"
  if [ "$#" -eq 3 ]; then
    fetch_bounded "$3" cat "${IPFS_BASE}/${package_path}" --progress=false
  else
    ipfs cat "${IPFS_BASE}/${package_path}" --progress="$show_progress"
  fi > "$staged_path" || die "Could not fetch mandatory payload: ${package_path}"
  [ -s "$staged_path" ] || die "Mandatory payload is empty: ${package_path}"
  expected=$(awk -v path="$package_path" '
    NR > 1 && $2 == path { count++; hash=$1 }
    END { if (count == 1) print hash; else exit 1 }
  ' "${WW_TMPDIR}/CHECKSUMS.txt") || die "Missing or duplicate SHA-256 entry: ${package_path}"
  case "$expected" in *[!0-9a-f]*|'') die "Invalid SHA-256 entry: ${package_path}" ;; esac
  [ "${#expected}" -eq 64 ] || die "Invalid SHA-256 length: ${package_path}"
  actual=$(file_sha256 "$staged_path")
  [ "$actual" = "$expected" ] || die "Checksum mismatch: ${package_path}"
}

file_sha256() {
  if command -v sha256sum >/dev/null 2>&1; then
    sha256sum "$1" | awk '{print $1}'
  else
    shasum -a 256 "$1" | awk '{print $1}'
  fi
}

verify_descriptor() {
  fetch_verified "RELEASE-DESCRIPTOR" "$WW_TMPDIR/binding" 128 \
    || die "Cannot authenticate release descriptor binding"
  descriptor_cid=$(python3 -I - "$WW_TMPDIR" <<'PY'
import base64
from pathlib import Path
import sys

stage = Path(sys.argv[1])
link, size = (stage / "descriptor-link").read_text().split()
raw = bytes.fromhex(link)
if len(raw) != 36 or raw[:4] != bytes.fromhex("01711220") or not 0 < int(size) <= 1048576:
    sys.exit("Invalid direct release descriptor link")
cid = "b" + base64.b32encode(raw).decode().rstrip("=").lower()
if (stage / "binding").read_bytes() != (cid + "\n").encode():
    sys.exit("Signed release descriptor binding disagrees with transport root")
print(cid)
PY
  ) || die "Invalid release descriptor binding"
  fetch_bounded 1048576 block get "$descriptor_cid" > "$WW_TMPDIR/descriptor.cbor" \
    || die "Cannot fetch release descriptor block"
  [ "$(wc -c < "$WW_TMPDIR/descriptor.cbor")" -eq "$(awk '{print $2}' "$WW_TMPDIR/descriptor-link")" ] \
    || die "Release descriptor size mismatch"

  descriptor_hash=$(awk '{print substr($1, 9)}' "$WW_TMPDIR/descriptor-link")
  [ "$(file_sha256 "$WW_TMPDIR/descriptor.cbor")" = "$descriptor_hash" ] || die "Release descriptor CID mismatch"
  if [ "$RELEASE_CID" = "$BOOTSTRAP_CID" ]; then
    # The verified R1 descriptor names ww-release and the four supported Hosts.
    [ "$descriptor_cid" = bafyreiavqux4s2baztgqwnvyqeh6ytxccpns5drvjunzkm5u76wx6mxuam ] \
      || die "Unexpected R1 descriptor"
    return
  fi
  # Decode only the block whose local CID matches the signed binding. Kubo's
  # temporary repository uses offline commands for every operation.
  (
    export IPFS_PATH="$WW_TMPDIR/inspect-repo"
    ipfs --offline init --profile=test > /dev/null &&
      ipfs --offline block put --cid-codec=dag-cbor < "$WW_TMPDIR/descriptor.cbor" > "$WW_TMPDIR/local-cid" &&
      cmp -s "$WW_TMPDIR/binding" "$WW_TMPDIR/local-cid" &&
      ipfs --offline dag get "$descriptor_cid" > "$WW_TMPDIR/descriptor.json"
  ) || die "Cannot authenticate or decode release descriptor"

  python3 -I - "$WW_TMPDIR/descriptor.json" "$RELEASE_IPNS" <<'PY' || die "Release descriptor channel mismatch"
import json
import sys

with open(sys.argv[1], encoding="utf-8") as descriptor:
    version, fields = json.load(descriptor)
if type(version) is not int or version != 1 or fields["channel"] != sys.argv[2]:
    sys.exit(1)
PY
}

status '>' "Fetching verified Host..."
fetch_verified "$BIN_PATH" "${WW_TMPDIR}/ww"
status '+' "Fetched verified Host"

if "$RECOVER"; then
  chmod +x "${WW_TMPDIR}/ww"
  "${WW_TMPDIR}/ww" upgrade --recover-only --release "$RELEASE_CID" \
    --ipfs-url "$BOOTSTRAP_IPFS_URL" --timeout-secs "$RECOVERY_TIMEOUT_SECS" \
    --trusted-release-keys "$TRUSTED_RELEASE_KEYS"
  exit 0
fi

status '>' "Fetching verified FHS payloads..."
mkdir -p "${WW_TMPDIR}/fhs/boot" "${WW_TMPDIR}/fhs/bin"
fetch_verified "fhs/boot/kernel.wasm" "${WW_TMPDIR}/fhs/boot/kernel.wasm"
fetch_verified "fhs/bin/status.wasm" "${WW_TMPDIR}/fhs/bin/status.wasm"
if [ -n "$expected_channel" ] || [ "$RELEASE_CID" = "$BOOTSTRAP_CID" ]; then
  verify_descriptor
fi
status '+' "Fetched verified package"

# These hashes come from the independently verified R1 manifest. The
# selected release cannot choose the executable that validates its descriptor.
case "$OS_NAME/$ARCH_NAME" in
  linux/aarch64) verifier_hash=b69bc0047fad1e69de8b3cf55f0c4b66cfbd6c54d7973a3e89b4db485227b927 ;;
  linux/x86_64) verifier_hash=edb12cd48e34cf13d60eeb9ac3b3789196a1f6d5e26a8b52a269a0952e42a9d8 ;;
  macos/aarch64) verifier_hash=1fd3a88757d8a1068c0ead50b4b2b535af580ad3c51375ea423d7d069e473e84 ;;
  macos/x86_64) verifier_hash=34d105775e63b9c6223414c8ee18b2f0ab1153ab51994ef37cbd6b2a13df4fb0 ;;
esac
verifier=$WW_TMPDIR/ww
if [ "$RELEASE_CID" != "$BOOTSTRAP_CID" ]; then
  verifier=$WW_TMPDIR/r1-verifier
  ipfs cat "/ipfs/$BOOTSTRAP_CID/$BIN_PATH" --progress="$show_progress" > "$verifier" || die "Could not fetch R1 verifier"
fi
[ "$(file_sha256 "$verifier")" = "$verifier_hash" ] || die "R1 verifier checksum mismatch"
chmod +x "$verifier"
"$verifier" install --release "$RELEASE_CID" --trusted-release-keys "$TRUSTED_RELEASE_KEYS" \
  --ipfs-url "$BOOTSTRAP_IPFS_URL"
"${WW_HOME}/bin/ww" --version
printf '\nInstalled Wetware in %s. Start the Host with "%s/bin/ww" run.\n' "$WW_HOME" "$WW_HOME"
}
