#!/usr/bin/env python3
"""Build and freeze a same-job GNU ARM64 diagnostic input from exact CI source.

This receipt describes a local CI build, not an uploaded producer artifact or a
completed required check. It grants no release publication authority.
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

import native_release_inputs as inputs
from qualify_native_upgrade import custody, verify_producer_files

TARGET = "aarch64-unknown-linux-gnu"
PAIR_POLICY = "pr739-reduced-current-pair-native-diagnostic-v1"
BUILD_POLICY = "same-job-native-build-v1"
PROFILE = {
    "release": {"dependency_opt_level": "z", "host_opt_level": 2, "lto": True,
                "strip": True, "debug_assertions": False, "overflow_checks": False,
                "debug": 0, "incremental": False},
    "development": {"dependency_opt_level": 0, "host_opt_level": 0, "lto": False,
                    "strip": False, "debug_assertions": True, "overflow_checks": True,
                    "debug": 0, "incremental": False},
}
require = inputs.require
FHS_SOURCE = {"fhs/boot/kernel.wasm": "std/kernel/bin/main.wasm",
              "fhs/bin/status.wasm": "std/status/bin/status.wasm"}


def write_json(path, value):
    path.write_text(json.dumps(value, indent=2) + "\n")
    path.chmod(0o644)


def file_binding(path, limit=inputs.MAX_HOST):
    raw = inputs.read_file(path, limit)
    return {"sha256": inputs.digest(raw), "size": len(raw)}


def userspace_description():
    # Debian exposes this fixed OS descriptor through /etc/os-release -> /usr/lib.
    # This is descriptive evidence, not an executable or qualification input.
    path = Path("/etc/os-release").resolve(strict=True)
    return {"path": str(path), "contents": inputs.read_file(path, inputs.MAX_SIDECAR).decode()}


BASELINE_PIN = ".github/native-upgrade-baseline.json"
# These files neither compile nor stage the canonical FHS. Everything else,
# including Cargo locks, all crates, std, build scripts and component fixtures,
# is compared by Git mode/object. The component workflow job is compared below.
COMPONENT_EXCLUSIONS = {BASELINE_PIN, ".github/workflows/rust.yml",
    "tests/test_release_workflow.sh",
    "README.md", "CHANGELOG.md", "doc/deployment.md", "doc/release-authentication.md",
    "doc/cli.md", "doc/installation.md", "doc/native-upgrades.md", "doc/ai-context.md",
    ".agents/prompt.md", ".agents/skills/ww-onboard.md",
    # Retain exact retired paths while R0 still contains these qualification inputs.
    # Removing an exclusion would compare the retired R0 path with its deletion.
    "tests/fixtures/native-release-originals/README.md",
    "tests/fixtures/native-release-originals/.gitattributes",
    ".github/native-release-integration.json", "scripts/fetch_native_release_inputs.py",
    *["tests/fixtures/native-release-originals/inputs/" + name for name in (
        "a.zip", "b.zip", "a-artifact.json", "b-artifact.json", "a-run.json", "b-run.json",
        "a-build-job.json", "b-build-job.json", "a-jobs.json", "b-jobs.json",
        "a-build.log", "b-build.log", "capture-manifest.json", "retention.json")],
    *["scripts/" + name + ".py" for name in (
        "prepare_current_native_release", "native_release_inputs", "verify_native_release_inputs",
        "check_repository_artifacts",
        "qualify_native_upgrade", "run_native_release_integration", "run_native_development_boot")],
    *["tests/" + name + ".py" for name in (
        "native_release_integration", "test_installed_boot", "test_guarded_boot_fixture", "test_prepare_current_native_release",
        "test_qualify_native_upgrade", "test_native_release_integration", "test_native_release_provisioner",
        "test_native_development_boot", "test_native_qualification_boundary", "test_install",
        "test_native_release_inputs", "test_verify_native_release_inputs", "test_fetch_native_release_inputs",
        "test_repository_artifacts")]}


def baseline_pin(checkout):
    path = checkout / BASELINE_PIN
    value = inputs.read_json(path)
    require(set(value) == {"schema", "repository", "revision", "contract"}
            and value["schema"] == 1 and value["repository"] == "wetware/ww"
            and value["contract"] == "managed-systemd-v3"
            and isinstance(value["revision"], str) and re.fullmatch(r"[0-9a-f]{40}", value["revision"]),
            "reviewed reduced baseline pin is unresolved or invalid")
    return value


def component_manifest(checkout, revision):
    entries = {}
    for entry in inputs.git(checkout, "ls-tree", "-rz", revision).split(b"\0"):
        if not entry:
            continue
        metadata, name = entry.split(b"\t", 1)
        name = name.decode()
        if name in COMPONENT_EXCLUSIONS:
            continue
        entries[name] = metadata.decode()
    workflow = inputs.git(checkout, "show", revision + ":.github/workflows/rust.yml").decode()
    match = re.search(r"(?m)^  build-wasm:\n(?:^ {4}.*\n|^\n)*", workflow)
    require(match is not None, "component build workflow is missing")
    entries[".github/workflows/rust.yml:build-wasm"] = inputs.digest((match.group().rstrip() + "\n").encode())
    entries[".github/workflows/rust.yml:header"] = inputs.digest(inputs.workflow_header(workflow.encode()))
    return inputs.digest(json.dumps(entries, sort_keys=True).encode())


def source_snapshot(checkout, revision, component_revision):
    require(re.fullmatch(r"[0-9a-f]{40}", revision) is not None
            and re.fullmatch(r"[0-9a-f]{40}", component_revision) is not None,
            "source revisions must be full Git SHA1 values")
    require(inputs.git(checkout, "rev-parse", "HEAD").decode().strip() == revision,
            "checkout HEAD differs from requested revision")
    tree = inputs.git(checkout, "rev-parse", revision + "^{tree}").decode().strip()
    component_tree = inputs.git(checkout, "rev-parse", component_revision + "^{tree}").decode().strip()
    component = {"component_tree_equal": component_tree == tree}
    if component_tree != tree:
        manifest = component_manifest(checkout, revision)
        require(manifest == component_manifest(checkout, component_revision),
                "component artifact source differs from current Host source")
        component.update(component_source_equal=True, component_manifest_sha256=manifest,
                         component_source_tree=component_tree)
    result = inputs.verify_source(checkout, revision, ".github/workflows/rust.yml")
    result.update(component_revision=component_revision, source_tree=tree, **component)
    return result


def verify_component_binding(source):
    if source.get("component_tree_equal") is True:
        return True
    return (source.get("component_tree_equal") is False and source.get("component_source_equal") is True
            and re.fullmatch(r"[0-9a-f]{64}", source.get("component_manifest_sha256", "")) is not None
            and re.fullmatch(r"[0-9a-f]{40}", source.get("component_source_tree", "")) is not None)


def build_command(profile):
    require(profile in PROFILE, "unsupported current build profile")
    return ["cargo", "build", *(["--release"] if profile == "release" else []),
            "--locked", "--bin", "ww", "--target", TARGET]


def verify_profile(checkout, profile):
    value = tomllib.loads(inputs.read_file(checkout / "Cargo.toml", inputs.MAX_JSON).decode())
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
    event = inputs.read_json(Path(os.environ["GITHUB_EVENT_PATH"]))
    event_name = os.environ.get("GITHUB_EVENT_NAME")
    pr_source = {}
    if event_name == "pull_request":
        head = event["pull_request"]["head"]
        head_repository = head["repo"]["full_name"]
        require(re.fullmatch(r"[A-Za-z0-9_.-]+/[A-Za-z0-9_.-]+", head_repository), "invalid PR source repository")
        is_head = head["sha"] == revision
        pin = None
        if not is_head and profile == "release":
            harness = Path(__file__).resolve().parents[1]
            pin = baseline_pin(harness)
            require(pin["revision"] == revision and head_repository == "wetware/ww",
                    "baseline source does not match the reviewed repository pin")
            require(inputs.git(harness, "rev-parse", "HEAD").decode().strip() == head["sha"]
                    and inputs.git(harness, "show", head["sha"] + ":" + BASELINE_PIN)
                    == inputs.read_file(harness / BASELINE_PIN, inputs.MAX_JSON),
                    "baseline pin is not the exact PR-head repository content")
        require(is_head or pin is not None or (profile == "development" and os.environ.get("GITHUB_SHA") == revision),
                "PR source revision is neither the exact head nor the current development merge")
        source_repository = head_repository if is_head else "wetware/ww"
        pr_source = {"head_revision": head["sha"], "head_repository": head_repository,
                     "revision_kind": "pull-request-head" if is_head else "pinned-reduced-baseline" if pin else "pull-request-merge",
                     **({"baseline_pin": pin} if pin else {})}
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
        require(os.environ.get("GITHUB_ACTIONS") != "true", "local diagnostic cannot replace actual CI source authority")
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
        components[name] = file_binding(checkout / "target" / name, inputs.MAX_FHS)
        require(components[name] == file_binding(checkout / source_name, inputs.MAX_FHS),
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
        require(file_binding(checkout / "target" / name, inputs.MAX_FHS) == components[name]
                and file_binding(checkout / source_name, inputs.MAX_FHS) == components[name],
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
        for name in sorted(inputs.MEMBERS | {"BUILD-RECEIPT.json"}):
            archive.write(output / name, name)
    (output / "candidate.zip").chmod(0o644)
    write_json(output / "candidate.json", {"schema": 1, "policy": BUILD_POLICY,
        "build_receipt": file_binding(output / "BUILD-RECEIPT.json", inputs.MAX_JSON),
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
            == inputs.MEMBERS | {"BUILD-RECEIPT.json", "candidate.json", "candidate.zip", "build.log"},
            "current input file set differs")
    record = inputs.read_json(directory / "candidate.json")
    receipt = inputs.read_json(directory / "BUILD-RECEIPT.json")
    require(record == {"schema": 1, "policy": BUILD_POLICY,
                      "build_receipt": file_binding(directory / "BUILD-RECEIPT.json", inputs.MAX_JSON),
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
            pinned = ci["revision_kind"] == "pinned-reduced-baseline"
            if pinned:
                pin = ci.get("baseline_pin", {})
                require(set(pin) == {"schema", "repository", "revision", "contract"}
                        and pin == {"schema": 1, "repository": "wetware/ww", "revision": revision,
                                    "contract": "managed-systemd-v3"}
                        and profile == "release" and ci["head_repository"] == "wetware/ww"
                        and revision != ci["head_revision"], "invalid pinned baseline CI source")
            require((is_head and revision == ci["head_revision"] and ci["source_repository"] == ci["head_repository"])
                    or pinned
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
            and verify_component_binding(receipt["source"]), "current source binding differs")
    require(receipt["environment"] == {"WW_BUILD_GIT_SHA": revision, "CARGO_INCREMENTAL": "0", "CARGO_BUILD_JOBS": "2", "RUSTUP_TOOLCHAIN": "1.99.1",
             "CARGO_TARGET_DIR": receipt["environment"]["CARGO_TARGET_DIR"],
             **({"CARGO_PROFILE_DEV_DEBUG": "0"} if profile == "development" else {})},
            "current compiler environment differs")
    require(file_binding(directory / "ww", 1024 ** 3) == receipt["host"], "current Host binding differs")
    require(file_binding(directory / "build.log", 64 * 1024 ** 2) == receipt["build_log"], "current build log differs")
    require(set(receipt["fhs"]) == inputs.FHS, "current FHS set differs")
    members = inputs.MEMBERS | {"BUILD-RECEIPT.json"}
    with zipfile.ZipFile(directory / "candidate.zip") as archive:
        entries = archive.infolist()
        require(len(entries) == len(members) and {entry.filename for entry in entries} == members,
                "current archive member set differs")
        for entry in entries:
            require(entry.orig_filename == entry.filename and stat.S_ISREG(entry.external_attr >> 16)
                    and not entry.flag_bits & 1 and entry.compress_type in (zipfile.ZIP_STORED, zipfile.ZIP_DEFLATED),
                    "unsafe current archive member")
            raw = inputs.read_file(directory / entry.filename,
                                     1024 ** 3 if entry.filename == "ww" else inputs.MAX_FHS
                                     if entry.filename in inputs.FHS else inputs.MAX_JSON)
            require(entry.file_size == len(raw) and archive.read(entry) == raw, "current archive member differs")
    for name, binding in receipt["fhs"].items():
        raw = inputs.read_file(directory / name, inputs.MAX_FHS)
        require(raw[:8] == b"\x00asm\x0d\x00\x01\x00" and file_binding(directory / name, inputs.MAX_FHS) == binding,
                "current FHS binding differs")
    host = inputs.read_file(directory / "ww", 1024 ** 3)
    require(len(host) >= 64 and host[:7] == b"\x7fELF\x02\x01\x01"
            and struct.unpack_from("<HHI", host, 16) == (3, 183, 1)
            and struct.unpack_from("<H", host, 52)[0] == 64 and revision.encode() in host,
            "current executable target or embedded revision differs")
    require(inputs.read_json(directory / "PROVENANCE.json") == {
                "repository": "https://github.com/" + ci.get("source_repository", "wetware/ww"), "revision": revision, "builder": builder}
            and inputs.read_file(directory / "REVISION", inputs.MAX_SIDECAR) == (revision + "\n").encode()
            and inputs.read_file(directory / "SHA256SUMS", inputs.MAX_SIDECAR) == (receipt["host"]["sha256"] + "  ww\n").encode(),
            "current executable sidecars differ")
    return receipt


def verify_candidate(checkout, directory, revision, profile, owner=None):
    receipt = verify_build_files(directory, profile, owner=owner)
    require(receipt["ci"]["revision"] == revision, "candidate revision differs")
    if receipt["ci"].get("revision_kind") == "pinned-reduced-baseline":
        require(receipt["ci"]["baseline_pin"] == baseline_pin(Path(__file__).resolve().parents[1]),
                "candidate baseline differs from reviewed repository pin")
    require(source_snapshot(checkout.resolve(strict=True), revision, receipt["source"]["component_revision"])
            == receipt["source"], "current source changed after build")
    verify_profile(checkout, profile)
    return receipt


def pair_source_roles(checkout=None):
    """Bind both local and CI fixture slots to this actual reviewed harness."""
    checkout = (Path(__file__).resolve().parents[1] if checkout is None else checkout).resolve(strict=True)
    head = inputs.git(checkout, "rev-parse", "HEAD").decode().strip()
    pin = baseline_pin(checkout)
    raw = inputs.read_file(checkout / BASELINE_PIN, inputs.MAX_JSON)
    require(inputs.git(checkout, "show", head + ":" + BASELINE_PIN) == raw,
            "pair baseline pin is not committed in the actual final harness HEAD")
    require(head != pin["revision"], "final harness and baseline must be distinct reviewed revisions")
    return {"harness_revision": head, "baseline_pin": pin, "baseline_pin_sha256": inputs.digest(raw)}


def validate_pair_roles(builds, roles):
    require(set(builds) == {"a", "b"}
            and set(roles) == {"harness_revision", "baseline_pin", "baseline_pin_sha256"},
            "current pair source roles are incomplete")
    pin = roles["baseline_pin"]
    require(set(pin) == {"schema", "repository", "revision", "contract"}
            and pin["schema"] == 1 and pin["repository"] == "wetware/ww"
            and pin["contract"] == "managed-systemd-v3"
            and re.fullmatch(r"[0-9a-f]{40}", pin["revision"])
            and re.fullmatch(r"[0-9a-f]{40}", roles["harness_revision"])
            and re.fullmatch(r"[0-9a-f]{64}", roles["baseline_pin_sha256"])
            and roles["harness_revision"] != pin["revision"], "invalid current pair source roles")
    a, b = builds["a"]["ci"], builds["b"]["ci"]
    require(a["revision"] == roles["harness_revision"] and b["revision"] == pin["revision"],
            "pair slots must be actual final harness HEAD and exact reviewed baseline")
    local_a, local_b = a.get("kind") == "local-native-diagnostic", b.get("kind") == "local-native-diagnostic"
    require(local_a == local_b, "pair cannot mix local and CI source authority")
    if not local_a:
        require(a["revision_kind"] == "pull-request-head" and b["revision_kind"] == "pinned-reduced-baseline"
                and b["baseline_pin"] == pin and a["head_revision"] == roles["harness_revision"],
                "CI pair must use exact head and pinned baseline source roles")
        shared = ("repository", "source_repository", "event", "run_id", "attempt", "job",
                  "workflow_sha", "workflow_ref", "head_revision", "head_repository")
        require(all(key in a and key in b and a[key] == b[key] for key in shared)
                and a["repository"] == a["source_repository"] == a["head_repository"] == "wetware/ww"
                and a["event"] == "pull_request", "CI pair comes from different run/job/head/component context")


def combine_current(checkout_a, candidate_a, revision_a, checkout_b, candidate_b, revision_b, output):
    """Freeze two current-contract release builds; a is the final-head initiator.

    Fixture sequence order is independent of Git ancestry. Original historical
    A/B evidence is never used as either reduced-contract executable.
    """
    builds = {label: verify_candidate(checkout, candidate, revision, "release")
              for label, checkout, candidate, revision in
              (("a", checkout_a, candidate_a, revision_a), ("b", checkout_b, candidate_b, revision_b))}
    roles = pair_source_roles()
    validate_pair_roles(builds, roles)
    require(builds["a"]["host"]["sha256"] != builds["b"]["host"]["sha256"],
            "changed native qualification requires distinct executable bytes")
    output = output.absolute()
    require(not output.exists() and output.parent.resolve(strict=True) == output.parent,
            "combined output must be absent below a canonical parent")
    with tempfile.TemporaryDirectory(prefix=".current-inputs-", dir=output.parent) as temporary:
        stage = Path(temporary) / "verified"
        stage.mkdir(mode=0o755)
        markers = {}
        for label, candidate in (("a", candidate_a), ("b", candidate_b)):
            receipt = builds[label]
            shutil.copytree(candidate, stage / label)
            verify_build_files(stage / label, "release")
            shutil.copyfile(candidate / "candidate.zip", stage / (label + ".zip"))
            (stage / (label + ".zip")).chmod(0o644)
            marker = {"path": str(stage / label / "ww"), "archive_path": str(stage / (label + ".zip")),
                "sha256": receipt["host"]["sha256"],
                "archive_sha256": file_binding(stage / (label + ".zip"), 1024 ** 3)["sha256"],
                "revision": receipt["ci"]["revision"], "run_id": receipt["ci"].get("run_id"),
                "attempt": receipt["ci"].get("attempt"), "builder": receipt["builder"]}
            verify_producer_files(marker, TARGET, os.geteuid())
            marker.update(path=str(output / label / "ww"), archive_path=str(output / (label + ".zip")))
            markers[label] = marker
        result = {"schema": 2, "policy": PAIR_POLICY, "status": "VERIFIED_DIAGNOSTIC_INPUTS",
                  "native_acceptance": False, "publication_allowed": False, "required_checks_success": False,
                  "slot_identities": {"a": "reduced-current-R1", "b": "reduced-current-R0"},
                  "current_builds": builds, "source_roles": roles,
                  "producer_evidence": {label: {"kind": "same-job-current-native-build", "fhs": build["fhs"],
                      "completed_job_receipt": False, "required_checks_success": False}
                      for label, build in builds.items()}, **markers}
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
    consumer = modes.add_parser("verify-current-pair")
    for label in ("a", "b"):
        for name in ("checkout", "candidate"):
            consumer.add_argument("--" + name + "-" + label, required=True, type=Path)
        consumer.add_argument("--revision-" + label, required=True)
    consumer.add_argument("--output", required=True, type=Path)
    args = parser.parse_args()
    try:
        if args.mode == "build":
            build(args.checkout, args.output, args.revision, args.component_revision,
                  args.profile, args.run_id, args.attempt, args.job, local_diagnostic=args.local_diagnostic)
        else:
            combine_current(args.checkout_a, args.candidate_a, args.revision_a,
                            args.checkout_b, args.candidate_b, args.revision_b, args.output)
    except (RuntimeError, OSError, ValueError, KeyError, TypeError, subprocess.SubprocessError, zipfile.BadZipFile) as error:
        print(json.dumps({"status": "REFUSED", "error": str(error)}), file=sys.stderr)
        return 1
    print(json.dumps({"status": "VERIFIED_DIAGNOSTIC_INPUTS", "native_acceptance": False,
                      "publication_allowed": False}))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
