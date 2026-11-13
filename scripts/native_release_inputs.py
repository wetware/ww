#!/usr/bin/env python3
"""Bounded native build-input checks for current qualification.

This module contains no producer identities, receipt retention policy, release
signing authority or Host execution path. Callers supply the source revision to bind.
"""

import fnmatch
import hashlib
import json
import re
import stat
import subprocess

FHS = {"fhs/boot/kernel.wasm", "fhs/bin/status.wasm"}
SIDECARS = {"REVISION", "PROVENANCE.json", "SHA256SUMS"}
MEMBERS = {"ww", *SIDECARS, *FHS}
MAX_JSON = 2 * 1024 ** 2
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


def git(checkout, *args):
    result = subprocess.run(["git", "--no-replace-objects", "-c", "safe.directory=" + str(checkout), "-C", str(checkout), *args],
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


def verify_source(checkout, revision, workflow_path):
    """Bind runtime bytes and the build recipe to one explicit source revision."""
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
    require(entries, "revision has no protected source entries")
    current = {name.decode("utf-8") for name in git(checkout, "ls-files", "-z", "--cached", "--others", "--", *SOURCE_PATHS).split(b"\0")
               if name and protected_source(name.decode("utf-8"))}
    # Generated component outputs are not checkout source. Each caller verifies
    # its FHS bytes separately against the artifact or current-build bindings.
    current -= GENERATED_COMPONENTS - set(entries)
    require(current == set(entries), "runtime/build source paths differ from revision")
    for name, expected in entries.items():
        path = checkout / name
        require(path.resolve(strict=True) == path, f"runtime source aliases another path: {name}")
        raw = read_file(path, 16 * 1024 ** 2) if path.stat().st_size else b""
        info = path.lstat()
        require(stat.S_ISREG(info.st_mode), f"runtime source is not a regular file: {name}")
        actual = hashlib.sha1(b"blob " + str(len(raw)).encode() + b"\0" + raw).hexdigest()
        mode = "100755" if info.st_mode & 0o111 else "100644"
        require(actual == expected["object"] and mode == expected["mode"], f"runtime/build source drift: {name}")
    workflow = workflow_path
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
