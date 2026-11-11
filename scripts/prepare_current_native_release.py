#!/usr/bin/env python3
"""Build and freeze a same-job GNU ARM64 diagnostic input from exact CI source.

This receipt describes a local CI build, not an uploaded producer artifact or a
completed required check. Original A/B evidence retains its separate policy.
"""

import argparse
from datetime import datetime, timezone
import json
import os
from pathlib import Path
import platform
import re
import shutil
import stat
import struct
import subprocess
import sys
import tempfile
import time
import tomllib
import zipfile

import verify_native_release_inputs as original
from qualify_native_upgrade import custody, verify_producer_files

TARGET = "aarch64-unknown-linux-gnu"
POLICY = "pr808-current-source-native-diagnostic-v1"
BUILD_POLICY = "same-job-native-build-v1"
PROFILE = {
    "release": {"dependency_opt_level": "z", "host_opt_level": 2, "lto": True,
                "strip": True, "debug_assertions": False, "overflow_checks": False,
                "debug": 0, "incremental": False},
    "development": {"dependency_opt_level": 0, "host_opt_level": 0, "lto": False,
                    "strip": False, "debug_assertions": True, "overflow_checks": True,
                    "debug": 0, "incremental": False},
}
require = original.require
FHS_SOURCE = {"fhs/boot/kernel.wasm": "std/kernel/bin/main.wasm",
              "fhs/bin/status.wasm": "std/status/bin/status.wasm"}


def write_json(path, value):
    path.write_text(json.dumps(value, indent=2) + "\n")
    path.chmod(0o644)


def file_binding(path, limit=original.MAX_HOST):
    raw = original.read_file(path, limit)
    return {"sha256": original.digest(raw), "size": len(raw)}


def userspace_description():
    # Debian exposes this fixed OS descriptor through /etc/os-release -> /usr/lib.
    # This is descriptive evidence, not an executable or qualification input.
    path = Path("/etc/os-release").resolve(strict=True)
    return {"path": str(path), "contents": original.read_file(path, original.MAX_SIDECAR).decode()}


def source_snapshot(checkout, revision, component_revision):
    require(re.fullmatch(r"[0-9a-f]{40}", revision) is not None
            and re.fullmatch(r"[0-9a-f]{40}", component_revision) is not None,
            "source revisions must be full Git SHA1 values")
    require(original.git(checkout, "rev-parse", "HEAD").decode().strip() == revision,
            "checkout HEAD differs from requested revision")
    tree = original.git(checkout, "rev-parse", revision + "^{tree}").decode().strip()
    require(original.git(checkout, "rev-parse", component_revision + "^{tree}").decode().strip() == tree,
            "component artifact source tree differs from current Host source")
    result = original.verify_source(checkout, {"producers": {"b": {"revision": revision}},
                                     "workflow_path": ".github/workflows/rust.yml"})
    result.update(component_revision=component_revision, source_tree=tree,
                  component_tree_equal=True)
    return result


def build_command(profile):
    require(profile in PROFILE, "unsupported current build profile")
    return ["cargo", "build", *(["--release"] if profile == "release" else []),
            "--locked", "--bin", "ww", "--target", TARGET]


def verify_profile(checkout, profile):
    value = tomllib.loads(original.read_file(checkout / "Cargo.toml", original.MAX_JSON).decode())
    require(value.get("profile") == {"release": {"opt-level": "z", "lto": True, "strip": True,
                                                "package": {"ww": {"opt-level": 2}}}},
            "Cargo profiles changed; review the effective build contract")
    require(profile in PROFILE, "unsupported current build profile")
    # No unrecorded global, parent, or checkout config may override rustflags,
    # target runners, toolchains or Cargo profile settings in this build.
    config_dirs = [folder / ".cargo" for folder in (checkout, *checkout.parents)]
    config_dirs.append(Path(os.environ.get("CARGO_HOME", str(Path.home() / ".cargo"))))
    for config_dir in config_dirs:
        for name in ("config", "config.toml"):
            require(not (config_dir / name).exists(), "unreviewed Cargo configuration: " + str(config_dir / name))
    return PROFILE[profile]


def ci_context(revision, profile, run_id, attempt, job):
    require(os.environ.get("GITHUB_ACTIONS") == "true", "current build requires an actual GitHub Actions job")
    require(os.environ.get("GITHUB_REPOSITORY") == "wetware/ww", "current build repository differs")
    require(str(run_id) == os.environ.get("GITHUB_RUN_ID")
            and str(attempt) == os.environ.get("GITHUB_RUN_ATTEMPT")
            and job == os.environ.get("GITHUB_JOB") and run_id > 0 and attempt > 0,
            "current build CI invocation differs")
    event = original.read_json(Path(os.environ["GITHUB_EVENT_PATH"]))
    event_name = os.environ.get("GITHUB_EVENT_NAME")
    pr_source = {}
    if event_name == "pull_request":
        head = event["pull_request"]["head"]
        head_repository = head["repo"]["full_name"]
        require(re.fullmatch(r"[A-Za-z0-9_.-]+/[A-Za-z0-9_.-]+", head_repository), "invalid PR source repository")
        is_head = head["sha"] == revision
        require(is_head or (profile == "development" and os.environ.get("GITHUB_SHA") == revision),
                "PR source revision is neither the exact head nor the current development merge")
        source_repository = head_repository if is_head else "wetware/ww"
        pr_source = {"head_revision": head["sha"], "head_repository": head_repository,
                     "revision_kind": "pull-request-head" if is_head else "pull-request-merge"}
        branch = head["ref"]
    else:
        require(profile == "development" and event_name in ("push", "workflow_dispatch")
                and os.environ.get("GITHUB_SHA") == revision, "unsupported current build event")
        branch = os.environ.get("GITHUB_REF_NAME")
        source_repository = "wetware/ww"
        require(event_name == "workflow_dispatch" or branch in ("master", "staging"), "unsupported development push branch")
    if profile == "release":
        # Branch names are descriptive evidence, not permanent CI policy.
        # Release diagnostics retain exact same-repository PR-head binding.
        require(source_repository == "wetware/ww",
                "release diagnostic requires a same-repository PR head")
    return {"repository": "wetware/ww", "event": event_name, "branch": branch,
            "run_id": run_id, "attempt": attempt, "job": job,
            "revision": revision, "source_repository": source_repository, "workflow_sha": os.environ.get("GITHUB_SHA"),
            "workflow_ref": os.environ.get("GITHUB_WORKFLOW_REF"),
            "completed_job_receipt": False, "uploaded_producer_artifact": False, **pr_source}


def build(checkout, output, revision, component_revision, profile, run_id=None, attempt=None, job=None, *, local_diagnostic=False):
    checkout, output = checkout.resolve(strict=True), output.absolute()
    require(platform.system() == "Linux" and platform.machine() in ("aarch64", "arm64"),
            "current build requires native GNU ARM64 Linux")
    require(not output.exists() and output.parent.resolve(strict=True) == output.parent,
            "current output must be absent below a canonical parent")
    if local_diagnostic:
        require(run_id is None and attempt is None and job is None,
                "local diagnostic must not assert CI identity")
        context = {"kind": "local-native-diagnostic", "revision": revision,
                   "completed_job_receipt": False, "uploaded_producer_artifact": False}
    else:
        context = ci_context(revision, profile, run_id, attempt, job)
        require(component_revision == context["workflow_sha"], "component revision must identify this workflow's source")
    source = source_snapshot(checkout, revision, component_revision)
    settings = verify_profile(checkout, profile)
    components = {}
    for name, source_name in FHS_SOURCE.items():
        components[name] = file_binding(checkout / "target" / name, original.MAX_FHS)
        require(components[name] == file_binding(checkout / source_name, original.MAX_FHS),
                "downloaded and embedded FHS differ: " + name)
    target = checkout / "target" / ("current-native-" + profile)
    target.mkdir(mode=0o755, parents=True, exist_ok=True)
    require(target.resolve(strict=True) == target, "build target path contains aliases")
    for name in ("default-config.cid", "std-namespace.cid"):
        require(not (target / name).exists(), "unexpected generated default CID in isolated build target")
    # Preserve tool locations and authenticated dependency caches, not ambient
    # rustflags, profile overrides, compiler wrappers or target configuration.
    env = {key: value for key, value in os.environ.items()
           if key in ("PATH", "HOME", "CARGO_HOME", "RUSTUP_HOME", "SSL_CERT_FILE", "SSL_CERT_DIR")}
    env.update(WW_BUILD_GIT_SHA=revision, CARGO_INCREMENTAL="0", CARGO_BUILD_JOBS="2", RUSTUP_TOOLCHAIN="1.99.1", CARGO_TARGET_DIR=str(target),
               CARGO_TERM_COLOR="never")
    if profile == "development":
        env["CARGO_PROFILE_DEV_DEBUG"] = "0"
    output.mkdir(mode=0o755)
    command = build_command(profile)
    started = datetime.now(timezone.utc).isoformat()
    monotonic = time.monotonic()
    with (output / "build.log").open("xb") as log:
        os.fchmod(log.fileno(), 0o644)
        process = subprocess.run(command, cwd=checkout, env=env, stdout=log, stderr=subprocess.STDOUT,
                                 timeout=7200, check=False, umask=0o022)
    require(process.returncode == 0, "current native build failed; retain build.log")
    require(source_snapshot(checkout, revision, component_revision) == source, "source changed during native build")
    for name, source_name in FHS_SOURCE.items():
        require(file_binding(checkout / "target" / name, original.MAX_FHS) == components[name]
                and file_binding(checkout / source_name, original.MAX_FHS) == components[name],
                "FHS changed during native build")
    binary = target / TARGET / ("release" if profile == "release" else "debug") / "ww"
    binding = file_binding(binary, 1024 ** 3)
    shutil.copyfile(binary, output / "ww")
    (output / "ww").chmod(0o755)
    (output / "fhs").mkdir(mode=0o755)
    for name in FHS_SOURCE:
        destination = output / name
        destination.parent.mkdir(mode=0o755, exist_ok=True)
        shutil.copyfile(checkout / "target" / name, destination)
        destination.chmod(0o644)
    builder = (f"local-native-diagnostic:{revision}:{TARGET}:{profile}" if local_diagnostic
               else f"github-actions-pr-native:{run_id}:{attempt}:{TARGET}:{profile}")
    provenance = {"repository": "https://github.com/" + context.get("source_repository", "wetware/ww"),
                  "revision": revision, "builder": builder}
    write_json(output / "PROVENANCE.json", provenance)
    (output / "REVISION").write_text(revision + "\n")
    (output / "REVISION").chmod(0o644)
    (output / "SHA256SUMS").write_text(binding["sha256"] + "  ww\n")
    (output / "SHA256SUMS").chmod(0o644)
    def version(argv):
        return subprocess.check_output(argv, cwd=checkout, env=env, timeout=30).decode().strip()
    receipt = {"schema": 1, "policy": BUILD_POLICY, "profile": profile, "target": TARGET,
               "builder": builder, "source": source, "ci": context, "command": command,
               "profile_settings": settings, "environment": {key: env[key] for key in env
                   if key in ("WW_BUILD_GIT_SHA", "CARGO_INCREMENTAL", "CARGO_BUILD_JOBS", "RUSTUP_TOOLCHAIN", "CARGO_TARGET_DIR", "CARGO_PROFILE_DEV_DEBUG")},
               "host": binding, "fhs": components,
               "tools": {"rustc": version(["rustc", "-vV"]), "cargo": version(["cargo", "-V"]),
                         "linker": version(["cc", "--version"]), "libc": version(["ldd", "--version"])},
               "userspace": userspace_description(),
               "uname": list(platform.uname()), "started_at": started,
               "elapsed_seconds": time.monotonic() - monotonic, "exit": 0,
               "build_log": file_binding(output / "build.log", 64 * 1024 ** 2),
               "producer_comparison": ("Same Cargo release profile and target; native cargo/cc userspace differs from original cross container"
                    if profile == "release" else "Development opt-level 0 with debug info disabled; no release-profile equivalence is claimed"),
               "native_acceptance": False, "publication_allowed": False}
    write_json(output / "BUILD-RECEIPT.json", receipt)
    with zipfile.ZipFile(output / "candidate.zip", "x", compression=zipfile.ZIP_DEFLATED) as archive:
        for name in sorted(original.MEMBERS | {"BUILD-RECEIPT.json"}):
            archive.write(output / name, name)
    (output / "candidate.zip").chmod(0o644)
    write_json(output / "candidate.json", {"schema": 1, "policy": BUILD_POLICY,
        "build_receipt": file_binding(output / "BUILD-RECEIPT.json", original.MAX_JSON),
        "archive": file_binding(output / "candidate.zip", 1024 ** 3)})
    verify_candidate(checkout, output, revision, profile)
    return receipt


def verify_build_files(directory, profile, owner=None):
    owner = os.geteuid() if owner is None else owner
    require(directory.absolute() == directory.resolve(strict=True), "current input directory contains aliases")
    directory = directory.resolve(strict=True)
    custody(directory, owner, directory=True)
    for path in directory.rglob("*"):
        custody(path, owner, directory=path.is_dir())
    require({str(path.relative_to(directory)) for path in directory.rglob("*") if path.is_file()}
            == original.MEMBERS | {"BUILD-RECEIPT.json", "candidate.json", "candidate.zip", "build.log"},
            "current input file set differs")
    record = original.read_json(directory / "candidate.json")
    receipt = original.read_json(directory / "BUILD-RECEIPT.json")
    require(record == {"schema": 1, "policy": BUILD_POLICY,
                      "build_receipt": file_binding(directory / "BUILD-RECEIPT.json", original.MAX_JSON),
                      "archive": file_binding(directory / "candidate.zip", 1024 ** 3)},
            "candidate receipt or archive digest differs")
    require(receipt["schema"] == 1 and receipt["policy"] == BUILD_POLICY
            and receipt["profile"] == profile and receipt["target"] == TARGET
            and receipt["profile_settings"] == PROFILE[profile]
            and receipt["command"] == build_command(profile) and receipt["exit"] == 0
            and receipt["native_acceptance"] is False and receipt["publication_allowed"] is False,
            "current build receipt policy or configuration differs")
    ci = receipt["ci"]
    revision = ci["revision"]
    local = ci.get("kind") == "local-native-diagnostic"
    if local:
        require(set(ci) == {"kind", "revision", "completed_job_receipt", "uploaded_producer_artifact"},
                "local diagnostic cannot assert CI provenance")
        builder = f"local-native-diagnostic:{revision}:{TARGET}:{profile}"
    else:
        builder = f"github-actions-pr-native:{ci['run_id']}:{ci['attempt']}:{TARGET}:{profile}"
        require(type(ci["run_id"]) is int and ci["run_id"] > 0 and type(ci["attempt"]) is int and ci["attempt"] > 0
                and ci["repository"] == "wetware/ww" and ci["event"] in ("pull_request", "push", "workflow_dispatch")
                and re.fullmatch(r"[A-Za-z0-9_.-]+/[A-Za-z0-9_.-]+", ci["source_repository"])
                and (profile != "release" or (ci["event"] == "pull_request"
                     and ci["source_repository"] == "wetware/ww")), "current CI context differs")
        require(ci["workflow_sha"] == receipt["source"]["component_revision"], "current component source differs from workflow")
        if ci["event"] == "pull_request":
            is_head = ci["revision_kind"] == "pull-request-head"
            require((is_head and revision == ci["head_revision"] and ci["source_repository"] == ci["head_repository"])
                    or (profile == "development" and ci["revision_kind"] == "pull-request-merge"
                        and revision == ci["workflow_sha"] and ci["source_repository"] == "wetware/ww"),
                    "current PR source identity differs")
        else:
            require(ci["source_repository"] == "wetware/ww"
                    and revision == ci["workflow_sha"]
                    and (ci["event"] == "workflow_dispatch" or ci["branch"] in ("master", "staging")),
                    "current development event source identity differs")
    require(receipt["builder"] == builder and re.fullmatch(r"[0-9a-f]{40}", revision)
            and ci["completed_job_receipt"] is False and ci["uploaded_producer_artifact"] is False,
            "current build context differs")
    require(receipt["source"]["checkout_revision"] == revision
            and receipt["source"]["artifact_revision"] == revision
            and receipt["source"]["runtime_source_equal"] is True
            and receipt["source"]["component_tree_equal"] is True, "current source binding differs")
    require(receipt["environment"] == {"WW_BUILD_GIT_SHA": revision, "CARGO_INCREMENTAL": "0", "CARGO_BUILD_JOBS": "2", "RUSTUP_TOOLCHAIN": "1.99.1",
             "CARGO_TARGET_DIR": receipt["environment"]["CARGO_TARGET_DIR"],
             **({"CARGO_PROFILE_DEV_DEBUG": "0"} if profile == "development" else {})},
            "current compiler environment differs")
    require(file_binding(directory / "ww", 1024 ** 3) == receipt["host"], "current Host binding differs")
    require(file_binding(directory / "build.log", 64 * 1024 ** 2) == receipt["build_log"], "current build log differs")
    require(set(receipt["fhs"]) == original.FHS, "current FHS set differs")
    members = original.MEMBERS | {"BUILD-RECEIPT.json"}
    with zipfile.ZipFile(directory / "candidate.zip") as archive:
        entries = archive.infolist()
        require(len(entries) == len(members) and {entry.filename for entry in entries} == members,
                "current archive member set differs")
        for entry in entries:
            require(entry.orig_filename == entry.filename and stat.S_ISREG(entry.external_attr >> 16)
                    and not entry.flag_bits & 1 and entry.compress_type in (zipfile.ZIP_STORED, zipfile.ZIP_DEFLATED),
                    "unsafe current archive member")
            raw = original.read_file(directory / entry.filename,
                                     1024 ** 3 if entry.filename == "ww" else original.MAX_FHS
                                     if entry.filename in original.FHS else original.MAX_JSON)
            require(entry.file_size == len(raw) and archive.read(entry) == raw, "current archive member differs")
    for name, binding in receipt["fhs"].items():
        raw = original.read_file(directory / name, original.MAX_FHS)
        require(raw[:8] == b"\x00asm\x0d\x00\x01\x00" and file_binding(directory / name, original.MAX_FHS) == binding,
                "current FHS binding differs")
    host = original.read_file(directory / "ww", 1024 ** 3)
    require(len(host) >= 64 and host[:7] == b"\x7fELF\x02\x01\x01"
            and struct.unpack_from("<HHI", host, 16) == (3, 183, 1)
            and struct.unpack_from("<H", host, 52)[0] == 64 and revision.encode() in host,
            "current executable target or embedded revision differs")
    require(original.read_json(directory / "PROVENANCE.json") == {
                "repository": "https://github.com/" + ci.get("source_repository", "wetware/ww"), "revision": revision, "builder": builder}
            and original.read_file(directory / "REVISION", original.MAX_SIDECAR) == (revision + "\n").encode()
            and original.read_file(directory / "SHA256SUMS", original.MAX_SIDECAR) == (receipt["host"]["sha256"] + "  ww\n").encode(),
            "current executable sidecars differ")
    return receipt


def verify_candidate(checkout, directory, revision, profile, owner=None):
    receipt = verify_build_files(directory, profile, owner=owner)
    require(receipt["ci"]["revision"] == revision, "candidate revision differs")
    require(source_snapshot(checkout.resolve(strict=True), revision, receipt["source"]["component_revision"])
            == receipt["source"], "current source changed after build")
    verify_profile(checkout, profile)
    return receipt


def combine(checkout, candidate, revision, historical, pins_path, output):
    receipt = verify_candidate(checkout, candidate, revision, "release")
    pins = original.read_json(pins_path)
    original.validate_pins(pins)
    require(historical.absolute() == historical.resolve(strict=True), "historical directory contains aliases")
    historical = historical.resolve(strict=True)
    custody(historical, os.geteuid(), directory=True)
    for path in historical.rglob("*"):
        custody(path, os.geteuid(), directory=path.is_dir())
    old = original.read_json(historical / "verified-inputs.json")
    require(old["policy"] == original.POLICY and old["status"] == "VERIFIED_DIAGNOSTIC_INPUTS"
            and old["pins_sha256"] == original.digest(original.read_file(pins_path, original.MAX_JSON))
            and old["source"]["artifact_revision"] == pins["producers"]["b"]["revision"]
            and old["source"]["checkout_revision"] == pins["producers"]["b"]["revision"]
            and old["source"]["runtime_source_equal"] is True
            and old["native_acceptance"] is False and old["publication_allowed"] is False,
            "historical baseline must retain the original policy and immutable B checkout")
    for label in ("a", "b"):
        require(old["producer_evidence"][label]["required_checks"] == pins["producers"][label]["required_checks"]
                and old[label]["sha256"] == pins["producers"][label]["sha256"]
                and old[label]["archive_sha256"] == pins["producers"][label]["archive_sha256"],
                "historical producer evidence differs")
        require(old[label]["path"] == str(historical / label / "ww")
                and old[label]["archive_path"] == str(historical / (label + ".zip")),
                "historical producer paths differ")
        expected_fhs = pins["producers"][label]["fhs"]
        require(old["producer_evidence"][label]["fhs"] == expected_fhs,
                "historical FHS evidence differs")
        directory = historical / label
        require({str(path.relative_to(directory)) for path in directory.rglob("*") if path.is_file()}
                == original.MEMBERS, "historical input file set differs")
        for name, binding in expected_fhs.items():
            require(file_binding(directory / name, original.MAX_FHS) == binding,
                    "historical FHS bytes differ")
        verify_producer_files(old[label], TARGET, os.geteuid())
    output = output.absolute()
    require(not output.exists() and output.parent.resolve(strict=True) == output.parent,
            "combined output must be absent below a canonical parent")
    with tempfile.TemporaryDirectory(prefix=".current-inputs-", dir=output.parent) as temporary:
        stage = Path(temporary) / "verified"
        stage.mkdir(mode=0o755)
        # Reconstruct A from the hash-pinned archive, not mutable extracted files.
        original.verify_archive(historical / "a.zip", pins["producers"]["a"], pins, stage / "a")
        shutil.copytree(candidate, stage / "b")
        verify_build_files(stage / "b", "release")
        shutil.copyfile(candidate / "candidate.zip", stage / "b.zip")
        (stage / "b.zip").chmod(0o644)
        markers = {"a": dict(old["a"]), "b": {"path": "", "archive_path": "",
            "sha256": receipt["host"]["sha256"], "archive_sha256": file_binding(stage / "b.zip", 1024 ** 3)["sha256"],
            "revision": revision, "run_id": receipt["ci"].get("run_id"), "attempt": receipt["ci"].get("attempt"),
            "builder": receipt["builder"]}}
        for label, marker in markers.items():
            marker.update(path=str(stage / label / "ww"), archive_path=str(stage / (label + ".zip")))
            verify_producer_files(marker, TARGET, os.geteuid())
            marker.update(path=str(output / label / "ww"), archive_path=str(output / (label + ".zip")))
        result = {"schema": 1, "policy": POLICY, "status": "VERIFIED_DIAGNOSTIC_INPUTS",
                  "native_acceptance": False, "publication_allowed": False, "required_checks_success": False,
                  "source": receipt["source"], "pins_sha256": old["pins_sha256"],
                  "slot_identities": {"a": "original-producer-A", "b": "current-source-candidate-C"},
                  "historical_baseline": old, "current_build": receipt,
                  "producer_evidence": {"a": old["producer_evidence"]["a"],
                                        "b": {"kind": "same-job-current-native-build", "fhs": receipt["fhs"],
                                              "completed_job_receipt": False, "required_checks_success": False}}, **markers}
        write_json(stage / "verified-inputs.json", result)
        require(not output.exists(), "combined output appeared during verification")
        stage.rename(output)
    return result


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    modes = parser.add_subparsers(dest="mode", required=True)
    producer = modes.add_parser("build")
    for name in ("checkout", "output"):
        producer.add_argument("--" + name, required=True, type=Path)
    for name in ("revision", "component-revision"):
        producer.add_argument("--" + name, required=True)
    producer.add_argument("--job")
    for name in ("run-id", "attempt"):
        producer.add_argument("--" + name, type=int)
    producer.add_argument("--local-diagnostic", action="store_true")
    producer.add_argument("--profile", choices=PROFILE, required=True)
    consumer = modes.add_parser("verify")
    for name in ("checkout", "candidate", "historical", "pins", "output"):
        consumer.add_argument("--" + name, required=True, type=Path)
    consumer.add_argument("--revision", required=True)
    args = parser.parse_args()
    try:
        if args.mode == "build":
            build(args.checkout, args.output, args.revision, args.component_revision,
                  args.profile, args.run_id, args.attempt, args.job, local_diagnostic=args.local_diagnostic)
        else:
            combine(args.checkout, args.candidate, args.revision, args.historical, args.pins, args.output)
    except (RuntimeError, OSError, ValueError, KeyError, TypeError, subprocess.SubprocessError, zipfile.BadZipFile) as error:
        print(json.dumps({"status": "REFUSED", "error": str(error)}), file=sys.stderr)
        return 1
    print(json.dumps({"status": "VERIFIED_DIAGNOSTIC_INPUTS", "native_acceptance": False,
                      "publication_allowed": False}))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
