#!/usr/bin/env python3
"""Verify the exact historical PR808 pair; never execute, build, sign or publish.

The caller downloads each ZIP, artifact/run/build-job/jobs API record and build
log separately using authenticated read-only GitHub requests. This offline
verifier binds those receipts to reviewed pins; it cannot authenticate a caller
that substitutes forged API responses. Fresh inputs reject transport expiry.
Explicit retained mode verifies the reviewed archive and pre-expiry capture;
transport deletion does not revoke these pinned diagnostic bytes. Source drift refuses.
The unchanged release acceptance gate still rejects both failed producer runs.
"""

import argparse
from datetime import datetime, timezone
import fnmatch
import hashlib
import json
import os
from pathlib import Path, PurePosixPath
import re
import stat
import struct
import subprocess
import sys
import tempfile
import zipfile

from qualify_native_upgrade import validate_producer, verify_producer_files


POLICY = "pr808-original-pair-diagnostic-only"
RETENTION_POLICY = "pr808-original-producer-retention-v1"
RETENTION_FILES = {label + suffix for label in ("a", "b")
                   for suffix in (".zip", "-artifact.json", "-run.json", "-build-job.json", "-jobs.json", "-build.log")}
AUTHORIZED_PAIR = {
    "a": (42623846342, 1, "93de83ad8a0f34b0c9998651e10f1267361265e4"),
    "b": (42648050888, 1, "d3d827c24d2f2d0525fd7056adaf6d18a7f33074"),
}
REQUIRED = ("Build ww-linux-aarch64", "Test", "Clippy", "Check formatting",
            "Check committed Cargo lockfiles", "Build native wasm32-wasip3 components")
FHS = {"fhs/boot/kernel.wasm", "fhs/bin/status.wasm"}
SIDECARS = {"REVISION", "PROVENANCE.json", "SHA256SUMS"}
MEMBERS = {"ww", *SIDECARS, *FHS}
MAX_JSON = 2 * 1024 ** 2
MAX_LOG = 8 * 1024 ** 2
MAX_ARCHIVE = 32 * 1024 ** 2
MAX_HOST = 64 * 1024 ** 2
MAX_FHS = 4 * 1024 ** 2
MAX_SIDECAR = 64 * 1024
GENERATED_COMPONENTS = {"std/kernel/bin/main.wasm", "std/status/bin/status.wasm"}
SOURCE_PATHS = ("src", "crates", "capnp", "std", "wit", ".cargo", "Cargo*",
                "build.rs", "Cross.toml", "rust-toolchain*", "VERSION", ".gitmodules",
                ".github/actions/setup-capnp", "examples/discovery/greeter.capnp")


def require(condition, message):
    if not condition:
        raise RuntimeError(message)


def digest(raw):
    return hashlib.sha256(raw).hexdigest()


def read_file(path, limit):
    info = path.lstat()
    require(stat.S_ISREG(info.st_mode) and not path.is_symlink(), f"not a regular file: {path}")
    require(0 < info.st_size <= limit, f"file exceeds bound or is empty: {path}")
    with path.open("rb") as stream:
        raw = stream.read(limit + 1)
    require(len(raw) == info.st_size, f"file changed or exceeds bound: {path}")
    return raw


def unique_object(pairs):
    result = {}
    for key, value in pairs:
        require(key not in result, f"duplicate JSON key: {key}")
        result[key] = value
    return result


def parse_json(raw):
    return json.loads(raw, object_pairs_hook=unique_object,
                      parse_constant=lambda value: require(False, f"invalid JSON constant: {value}"))


def read_json(path):
    value = parse_json(read_file(path, MAX_JSON))
    require(isinstance(value, dict), f"JSON record must be an object: {path}")
    return value


def validate_pins(pins):
    fixed = {"schema": 1, "policy": POLICY, "repository": "wetware/ww",
             "repository_id": 777809247, "branch": "lthibault/pasted-text-v38",
             "workflow_path": ".github/workflows/rust.yml", "workflow_id": 93232375,
             "event": "workflow_dispatch", "target": "aarch64-unknown-linux-gnu",
             "artifact_name": "ww-linux-aarch64", "native_acceptance": False,
             "publication_allowed": False}
    require(set(pins) == {*fixed, "producers", "retained_inputs"}, "unknown or missing pin fields")
    for key, value in fixed.items():
        require(pins[key] == value and type(pins[key]) is type(value), f"unsupported policy pin: {key}")
    retained = pins["retained_inputs"]
    require(set(retained) == {"policy", "manifest_sha256"}
            and retained["policy"] == RETENTION_POLICY
            and re.fullmatch(r"[0-9a-f]{64}", retained["manifest_sha256"]), "invalid retention pins")
    require(set(pins["producers"]) == {"a", "b"}, "only the original pair is authorized")
    for label, identity in AUTHORIZED_PAIR.items():
        pin = pins["producers"][label]
        require(set(pin) == {"run_id", "attempt", "revision", "artifact_id", "build_job_id",
                             "archive_sha256", "archive_size", "sha256", "size", "fhs",
                             "workflow_status", "workflow_conclusion", "required_checks"},
                "unknown or missing producer pins")
        require(tuple(pin[key] for key in ("run_id", "attempt", "revision")) == identity,
                "producer is outside the authorized original pair")
        for key in ("run_id", "attempt", "artifact_id", "build_job_id", "archive_size", "size"):
            require(type(pin[key]) is int and pin[key] > 0, f"invalid integer pin: {key}")
        require(pin["archive_size"] <= MAX_ARCHIVE and pin["size"] <= MAX_HOST, "oversized pin")
        for key in ("sha256", "archive_sha256"):
            require(re.fullmatch(r"[0-9a-f]{64}", pin[key]), f"invalid digest pin: {key}")
        require(pin["workflow_status"] == "completed" and pin["workflow_conclusion"] == "failure",
                "original workflow failure must remain explicit")
        require(set(pin["required_checks"]) == set(REQUIRED), "missing or extra required check pins")
        for name, check in pin["required_checks"].items():
            require(set(check) == {"id", "status", "conclusion"}
                    and type(check["id"]) is int and check["id"] > 0
                    and check["status"] == "completed"
                    and check["conclusion"] == ("failure" if name == "Test" else "success"),
                    f"original required check status must be preserved: {name}")
        require(pin["required_checks"][REQUIRED[0]]["id"] == pin["build_job_id"], "build job pin differs")
        require(set(pin["fhs"]) == FHS, "FHS pin set differs")
        for value in pin["fhs"].values():
            require(set(value) == {"sha256", "size"} and type(value["size"]) is int
                    and 8 <= value["size"] <= MAX_FHS
                    and re.fullmatch(r"[0-9a-f]{64}", value["sha256"]), "invalid FHS pin")
    require(pins["producers"]["a"]["sha256"] != pins["producers"]["b"]["sha256"],
            "the original Host bytes must differ")


def match_fields(record, expected, description):
    require(isinstance(record, dict), f"invalid {description}")
    for key, value in expected.items():
        require(record.get(key) == value and type(record.get(key)) is type(value),
                f"{description} differs: {key}")


def timestamp(value):
    require(isinstance(value, str), "receipt timestamp must be text")
    parsed = datetime.fromisoformat(value.replace("Z", "+00:00"))
    require(parsed.tzinfo is not None, "receipt timestamp requires timezone")
    return parsed


def verify_retained_bundle(inputs, pins):
    """Authenticate retained bytes against reviewed Git pins, not a live URL.

    The capture record describes the historical acquisition. It is not a new
    signature, cryptographic timestamp or authority to relabel failed checks.
    """
    raw = read_file(inputs / "retention.json", MAX_JSON)
    manifest_digest = digest(raw)
    require(manifest_digest == pins["retained_inputs"]["manifest_sha256"], "retention manifest digest differs")
    manifest = parse_json(raw)
    require(set(manifest) == {"schema", "policy", "capture_manifest_sha256", "files"}
            and type(manifest["schema"]) is int and manifest["schema"] == 1
            and manifest["policy"] == RETENTION_POLICY, "invalid retention manifest")
    require(set(manifest["files"]) == RETENTION_FILES
            and {path.name for path in inputs.iterdir()} == RETENTION_FILES | {"retention.json", "capture-manifest.json"},
            "retained file set differs")
    capture_raw = read_file(inputs / "capture-manifest.json", MAX_JSON)
    require(digest(capture_raw) == manifest["capture_manifest_sha256"], "capture manifest digest differs")
    capture = parse_json(capture_raw)
    captured = timestamp(capture["recorded_at_utc"])
    require(re.fullmatch(r"[0-9a-f]{40}", capture["head"]), "invalid capture revision")
    for name, record in manifest["files"].items():
        require(set(record) == {"sha256", "bytes"} and type(record["bytes"]) is int
                and record["bytes"] > 0 and re.fullmatch(r"[0-9a-f]{64}", record["sha256"]),
                "invalid retained file binding")
        require(capture["files"].get("fresh-inputs/" + name) == record, "capture file binding differs")
        raw = read_file(inputs / name, MAX_ARCHIVE if name.endswith(".zip") else MAX_LOG)
        require(len(raw) == record["bytes"] and digest(raw) == record["sha256"], "retained file digest differs: " + name)
    for label, pin in pins["producers"].items():
        require(manifest["files"][label + ".zip"] == {"sha256": pin["archive_sha256"], "bytes": pin["archive_size"]},
                "retained archive differs from original producer pin")
    return {"files": manifest["files"], "captured": captured,
            "capture_recorded_at": capture["recorded_at_utc"],
            "manifest_sha256": manifest_digest}


def verify_receipts(inputs, label, pins, now=None, *, retained=False):
    pin = pins["producers"][label]
    archive = verify_retained_bundle(inputs, pins) if retained else None
    raw_receipts = {}

    def receipt(suffix):
        raw = read_file(inputs / f"{label}-{suffix}", MAX_LOG if suffix.endswith(".log") else MAX_JSON)
        if archive:
            expected = archive["files"][f"{label}-{suffix}"]
            require(len(raw) == expected["bytes"] and digest(raw) == expected["sha256"],
                    "retained receipt changed during verification")
        raw_receipts[suffix] = raw
        return raw

    def receipt_json(suffix):
        value = parse_json(receipt(suffix + ".json"))
        require(isinstance(value, dict), "JSON receipt must be an object")
        return value

    artifact, run, build, jobs = [receipt_json(suffix)
                                  for suffix in ("artifact", "run", "build-job", "jobs")]
    repo = pins["repository"]
    api = f"https://api.github.com/repos/{repo}/actions"
    web = f"https://github.com/{repo}/actions"
    run_id, artifact_id = pin["run_id"], pin["artifact_id"]
    source = {"head_sha": pin["revision"], "head_branch": pins["branch"]}
    match_fields(run, {"id": run_id, "run_attempt": pin["attempt"], **source,
                       "path": pins["workflow_path"], "workflow_id": pins["workflow_id"],
                       "event": pins["event"], "status": pin["workflow_status"],
                       "conclusion": pin["workflow_conclusion"], "url": f"{api}/runs/{run_id}",
                       "html_url": f"{web}/runs/{run_id}"}, "producer run")
    for key in ("repository", "head_repository"):
        match_fields(run.get(key), {"full_name": repo, "id": pins["repository_id"]}, key)
    match_fields(artifact, {"id": artifact_id, "name": pins["artifact_name"], "expired": False,
                            "size_in_bytes": pin["archive_size"],
                            "digest": "sha256:" + pin["archive_sha256"],
                            "url": f"{api}/artifacts/{artifact_id}",
                            "archive_download_url": f"{api}/artifacts/{artifact_id}/zip"}, "artifact")
    match_fields(artifact.get("workflow_run"), {"id": run_id, **source,
                 "repository_id": pins["repository_id"], "head_repository_id": pins["repository_id"]},
                 "artifact source")
    expires = timestamp(artifact["expires_at"])
    if archive:
        require(timestamp(artifact["created_at"]) <= archive["captured"] < expires,
                "capture is outside original artifact retention interval")
    else:
        require(expires > (now or datetime.now(timezone.utc)), "original artifact has expired")

    entries = jobs.get("jobs")
    require(isinstance(entries, list) and 0 < len(entries) <= 100
            and type(jobs.get("total_count")) is int and jobs["total_count"] == len(entries),
            "jobs receipt is missing, excessive or paginated")
    ids, names = set(), set()
    for job in entries:
        job_id = job.get("id")
        require(type(job_id) is int and job_id > 0 and job_id not in ids, "duplicate or invalid job ID")
        require(isinstance(job.get("name"), str) and job["name"] not in names, "duplicate or invalid job name")
        ids.add(job_id)
        names.add(job["name"])
        match_fields(job, {"run_id": run_id, "run_attempt": pin["attempt"], **source,
                          "status": "completed", "url": f"{api}/jobs/{job_id}",
                          "html_url": f"{web}/runs/{run_id}/job/{job_id}"}, "job identity")
    checks = {}
    for name in REQUIRED:
        matches = [job for job in entries if job["name"] == name]
        require(len(matches) == 1, f"missing required check: {name}")
        match_fields(matches[0], pin["required_checks"][name], f"required check {name}")
        checks[name] = {key: matches[0][key] for key in ("id", "status", "conclusion")}
    listed_build = next(job for job in entries if job["id"] == pin["build_job_id"])
    # Separate job endpoint must corroborate the complete exact-attempt entry.
    require(build == listed_build, "independent build-job receipt differs from jobs receipt")
    for step_name in ("Build (cross)", "Package binary", "Run actions/upload-artifact@v7"):
        steps = [step for step in build.get("steps", []) if step.get("name") == step_name]
        require(len(steps) == 1 and steps[0].get("status") == "completed"
                and steps[0].get("conclusion") == "success", f"build receipt lacks successful {step_name}")
    log = receipt("build.log").decode("utf-8")
    expected_log = {
        r"SHA256 digest of uploaded artifact is ([0-9a-f]{64})": pin["archive_sha256"],
        r"Artifact ww-linux-aarch64 successfully finalized\. Artifact ID ([0-9]+)": str(artifact_id),
        r"Artifact ww-linux-aarch64 has been successfully uploaded! Final size is ([0-9]+ bytes\. Artifact ID is [0-9]+)":
            f"{pin['archive_size']} bytes. Artifact ID is {artifact_id}",
        r"Artifact download URL: (\S+)": f"{web}/runs/{run_id}/artifacts/{artifact_id}",
    }
    for pattern, expected in expected_log.items():
        require(re.findall(pattern, log) == [expected], "build upload log receipt differs or is missing")
    return {"artifact_id": artifact_id, "build_job_id": pin["build_job_id"],
            "workflow_status": run["status"], "workflow_conclusion": run["conclusion"],
            "required_checks": checks, "required_checks_success": False,
            "artifact_expires_at": artifact["expires_at"], "upload_receipt_verified": True,
            "receipt_mode": "pinned_historical_archive" if archive else "fresh_api",
            "capture_recorded_at": archive["capture_recorded_at"] if archive else None,
            "retention_manifest_sha256": archive["manifest_sha256"] if archive else None,
            "receipt_sha256": {suffix: digest(raw) for suffix, raw in raw_receipts.items()}}


def verify_archive(path, pin, pins, destination):
    raw = read_file(path, MAX_ARCHIVE)
    require(len(raw) == pin["archive_size"] and digest(raw) == pin["archive_sha256"],
            "original archive size or digest differs")
    # Verify the same immutable bytes we later retain; never extract by pathname.
    import io
    with zipfile.ZipFile(io.BytesIO(raw)) as archive:
        entries = archive.infolist()
        names = [entry.filename for entry in entries]
        require(len(names) == len(set(names)), "duplicate archive member")
        require(len(names) == len(MEMBERS) and set(names) == MEMBERS, "archive member set differs")
        require(sum(entry.file_size for entry in entries) <= MAX_HOST + 2 * MAX_FHS + 3 * MAX_SIDECAR,
                "archive expansion exceeds bound")
        for entry in entries:
            name = PurePosixPath(entry.filename)
            require(entry.orig_filename == entry.filename and str(name) == entry.filename
                    and not name.is_absolute() and ".." not in name.parts
                    and "\\" not in entry.filename and not any(ord(c) < 32 for c in entry.filename),
                    "unsafe archive member path")
            require(stat.S_ISREG(entry.external_attr >> 16) and not entry.flag_bits & 1
                    and entry.compress_type in (zipfile.ZIP_STORED, zipfile.ZIP_DEFLATED),
                    "unsafe archive member type or compression")
            limit = MAX_HOST if entry.filename == "ww" else MAX_FHS if entry.filename in FHS else MAX_SIDECAR
            require(0 < entry.file_size <= limit, "archive member exceeds bound")
        require(archive.getinfo("ww").file_size == pin["size"], "Host size differs")
        host = archive.read("ww")
        require(digest(host) == pin["sha256"], "Host digest differs")
        require(len(host) >= 64 and host[:7] == b"\x7fELF\x02\x01\x01"
                and struct.unpack_from("<HHI", host, 16) == (3, 183, 1)
                and struct.unpack_from("<H", host, 52)[0] == 64,
                "Host is not ELF64 little-endian AArch64 ET_DYN")
        require(pin["revision"].encode() in host, "Host embedded revision differs")
        values = {"ww": host, **{name: archive.read(name) for name in SIDECARS}}
        require(values["REVISION"] == (pin["revision"] + "\n").encode(), "REVISION differs")
        require(values["SHA256SUMS"] == (pin["sha256"] + "  ww\n").encode(), "SHA256SUMS differs")
        builder = f"github-actions:{pin['run_id']}:{pin['attempt']}:{pins['target']}"
        require(parse_json(values["PROVENANCE.json"]) == {
            "repository": "https://github.com/" + pins["repository"],
            "revision": pin["revision"], "builder": builder}, "PROVENANCE differs")
        for name in FHS:
            require(archive.getinfo(name).file_size == pin["fhs"][name]["size"], "FHS size differs")
            values[name] = archive.read(name)
            require(values[name][:8] == b"\x00asm\x0d\x00\x01\x00"
                    and digest(values[name]) == pin["fhs"][name]["sha256"], "FHS component or digest differs")
    destination.mkdir(mode=0o755)
    for name, value in values.items():
        target = destination / name
        target.parent.mkdir(parents=True, exist_ok=True, mode=0o755)
        target.write_bytes(value)
        target.chmod(0o755 if name == "ww" else 0o644)
    archive_path = destination.with_suffix(".zip")
    archive_path.write_bytes(raw)
    archive_path.chmod(0o644)
    marker = {"path": str(destination / "ww"), "archive_path": str(archive_path),
              "sha256": pin["sha256"], "archive_sha256": pin["archive_sha256"],
              "revision": pin["revision"], "run_id": pin["run_id"], "attempt": pin["attempt"],
              "builder": builder}
    validate_producer(marker, pins["target"])
    verify_producer_files(marker, pins["target"], owner=os.geteuid())
    return marker


def git(checkout, *args):
    result = subprocess.run(["git", "--no-replace-objects", "-C", str(checkout), *args],
                            stdout=subprocess.PIPE, stderr=subprocess.PIPE, timeout=60, check=False)
    require(result.returncode == 0, "source verification git command failed: " + result.stderr.decode(errors="replace")[:1024])
    require(len(result.stdout) <= 8 * 1024 ** 2, "source listing exceeds bound")
    return result.stdout


def protected_source(name):
    top = name.split("/", 1)[0]
    return (top in {"src", "crates", "capnp", "std", "wit", ".cargo"}
            or name in {"build.rs", "Cross.toml", "VERSION", ".gitmodules",
                        "examples/discovery/greeter.capnp"}
            or ("/" not in name and (fnmatch.fnmatch(name, "Cargo*") or fnmatch.fnmatch(name, "rust-toolchain*")))
            or name.startswith(".github/actions/setup-capnp/"))


def build_section(raw):
    lines = raw.splitlines(keepends=True)
    starts = [i for i, line in enumerate(lines) if line.rstrip(b"\r\n") == b"  build-binaries:"]
    require(len(starts) == 1, "workflow must have exactly one build-binaries section")
    start = starts[0]
    end = next((i for i in range(start + 1, len(lines))
                if re.match(rb"^  [A-Za-z0-9_-]+:", lines[i])), len(lines))
    return b"".join(lines[start:end]).rstrip() + b"\n"


def workflow_header(raw):
    """Bind inherited producer settings as well as the producer job itself.

    This workflow keeps its global settings before jobs. Refuse new top-level
    settings after jobs so an appended env/defaults block cannot evade binding.
    Consumer job additions within jobs remain outside the producer comparison.
    """
    lines = raw.splitlines(keepends=True)
    starts = [i for i, line in enumerate(lines) if line.rstrip(b"\r\n") == b"jobs:"]
    require(len(starts) == 1, "workflow must have exactly one top-level jobs section")
    start = starts[0]
    require(all(not line.strip() or line.lstrip().startswith(b"#") or line[:1] in (b" ", b"\t")
                for line in lines[start + 1:]), "unexpected top-level setting after jobs")
    return b"".join(lines[:start + 1])


def verify_source(checkout, pins):
    revision = pins["producers"]["b"]["revision"]
    require(git(checkout, "rev-parse", "--show-toplevel").decode().strip() == str(checkout),
            "checkout must be the repository root")
    head = git(checkout, "rev-parse", "--verify", "HEAD").decode().strip()
    entries = {}
    for entry in git(checkout, "ls-tree", "-rz", revision).split(b"\0"):
        if not entry:
            continue
        metadata, raw_name = entry.split(b"\t", 1)
        name = raw_name.decode("utf-8")
        if protected_source(name):
            mode, kind, object_id = metadata.decode().split()
            require(mode in ("100644", "100755") and kind == "blob", "unsupported runtime source type")
            entries[name] = {"mode": mode, "object": object_id}
    require(entries, "B revision has no protected source entries")
    current = {name.decode("utf-8") for name in git(checkout, "ls-files", "-z", "--cached", "--others", "--", *SOURCE_PATHS).split(b"\0")
               if name and protected_source(name.decode("utf-8"))}
    # These two ignored generated files are not checkout source and are never
    # consumed by this lane. Extracted, independently pinned FHS is used instead.
    current -= GENERATED_COMPONENTS - set(entries)
    require(current == set(entries), "runtime/build source paths differ from B")
    for name, expected in entries.items():
        path = checkout / name
        require(path.resolve(strict=True) == path, f"runtime source aliases another path: {name}")
        raw = read_file(path, 16 * 1024 ** 2) if path.stat().st_size else b""
        info = path.lstat()
        require(stat.S_ISREG(info.st_mode), f"runtime source is not a regular file: {name}")
        actual = hashlib.sha1(b"blob " + str(len(raw)).encode() + b"\0" + raw).hexdigest()
        mode = "100755" if info.st_mode & 0o111 else "100644"
        require(actual == expected["object"] and mode == expected["mode"], f"runtime/build source drift: {name}")
    workflow = pins["workflow_path"]
    original_workflow = git(checkout, "show", f"{revision}:{workflow}")
    current_workflow = read_file(checkout / workflow, MAX_JSON)
    header = workflow_header(original_workflow)
    require(header == workflow_header(current_workflow), "producer workflow inherited settings drift")
    original = build_section(original_workflow)
    current_build = build_section(current_workflow)
    require(original == current_build, "producer build-binaries recipe drift")
    return {"artifact_revision": revision, "checkout_revision": head,
            "runtime_and_build_entries_checked": len(entries),
            "runtime_and_build_manifest_sha256": digest(json.dumps(entries, sort_keys=True).encode()),
            "build_binaries_sha256": digest(original), "runtime_source_equal": True,
            "workflow_header_sha256": digest(header),
            "ignored_generated_components_not_consumed": sorted(GENERATED_COMPONENTS),
            "artifact_relabelled_as_checkout": False}


def verify(checkout, inputs, output, pins_path, *, retained=False):
    checkout, inputs = checkout.resolve(strict=True), inputs.resolve(strict=True)
    output = output.absolute()
    require(output.parent.resolve(strict=True) == output.parent, "output parent contains aliases")
    require(not output.exists() and not output.is_symlink(), "output must not already exist")
    pins = read_json(pins_path)
    validate_pins(pins)
    source = verify_source(checkout, pins)
    evidence = {label: verify_receipts(inputs, label, pins, retained=retained) for label in ("a", "b")}
    with tempfile.TemporaryDirectory(prefix=".native-inputs-", dir=output.parent) as temporary:
        stage = Path(temporary) / "verified"
        stage.mkdir(mode=0o755)
        markers = {}
        for label in ("a", "b"):
            markers[label] = verify_archive(inputs / f"{label}.zip", pins["producers"][label], pins, stage / label)
            markers[label]["path"] = str(output / label / "ww")
            markers[label]["archive_path"] = str(output / f"{label}.zip")
            evidence[label]["fhs"] = pins["producers"][label]["fhs"]
        result = {"schema": 1, "policy": POLICY, "status": "VERIFIED_DIAGNOSTIC_INPUTS",
                  "native_acceptance": False, "publication_allowed": False,
                  "required_checks_success": False, "source": source,
                  "pins_sha256": digest(read_file(pins_path, MAX_JSON)),
                  "producer_evidence": evidence, **markers}
        (stage / "verified-inputs.json").write_text(json.dumps(result, indent=2) + "\n")
        # No partial result is exposed if either input or any receipt refuses.
        require(not output.exists() and not output.is_symlink(), "output appeared during verification")
        stage.rename(output)
    return result


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    for name in ("checkout", "inputs", "output", "pins"):
        parser.add_argument("--" + name, required=True, type=Path)
    parser.add_argument("--retained", action="store_true",
                        help="verify the hash-pinned historical diagnostic archive; never relabel producer source or checks")
    args = parser.parse_args()
    try:
        result = verify(args.checkout, args.inputs, args.output, args.pins, retained=args.retained)
    except (RuntimeError, OSError, ValueError, KeyError, TypeError, zipfile.BadZipFile, subprocess.TimeoutExpired) as error:
        print(json.dumps({"status": "REFUSED", "error": str(error),
                          "native_acceptance": False, "publication_allowed": False}), file=sys.stderr)
        return 1
    print(json.dumps({"status": result["status"], "output": str(args.output.absolute()),
                      "required_checks_success": False, "native_acceptance": False,
                      "publication_allowed": False}))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
