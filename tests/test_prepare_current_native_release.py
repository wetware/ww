#!/usr/bin/env python3
"""Current-build receipt and source refusal tests; synthetic executables never run."""

import copy
import json
import os
from pathlib import Path
import stat
import struct
import subprocess
import sys
import tempfile
from types import SimpleNamespace
import unittest
import zipfile
from unittest.mock import patch

ROOT = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(ROOT / "scripts"))
import prepare_current_native_release as current
import qualify_native_upgrade as qualifier
import native_release_integration as lane


class Candidate(unittest.TestCase):
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory()
        self.addCleanup(self.temporary.cleanup)
        self.root = Path(self.temporary.name).resolve()
        self.directory = self.root / "candidate"
        self.directory.mkdir()
        self.revision = "1" * 40
        self.builder = "github-actions-pr-native:123:1:" + current.TARGET + ":release"
        host = bytearray(64)
        host[:7] = b"\x7fELF\x02\x01\x01"
        struct.pack_into("<HHI", host, 16, 3, 183, 1)
        struct.pack_into("<H", host, 52, 64)
        (self.directory / "ww").write_bytes(host + self.revision.encode())
        (self.directory / "ww").chmod(0o755)
        self.components = {}
        for name in current.FHS_SOURCE:
            path = self.directory / name
            path.parent.mkdir(parents=True, exist_ok=True)
            path.write_bytes(b"\x00asm\x0d\x00\x01\x00" + name.encode())
            self.components[name] = current.file_binding(path)
        (self.directory / "build.log").write_text("Finished release profile\n")
        self.source = {"checkout_revision": self.revision, "artifact_revision": self.revision,
                       "runtime_source_equal": True, "component_tree_equal": True,
                       "component_revision": "2" * 40}
        self.receipt = {"schema": 1, "policy": current.BUILD_POLICY, "profile": "release",
            "target": current.TARGET, "builder": self.builder, "source": self.source,
            "ci": {"repository": "wetware/ww", "source_repository": "wetware/ww",
                   "event": "pull_request", "branch": "lthibault/pasted-text-v38",
                   "run_id": 123, "attempt": 1, "job": "test-native", "revision": self.revision,
                   "head_revision": self.revision, "head_repository": "wetware/ww",
                   "revision_kind": "pull-request-head", "workflow_sha": self.source["component_revision"],
                   "completed_job_receipt": False, "uploaded_producer_artifact": False},
            "command": current.build_command("release"), "profile_settings": current.PROFILE["release"],
            "environment": {"WW_BUILD_GIT_SHA": self.revision, "CARGO_INCREMENTAL": "0",
                            "CARGO_BUILD_JOBS": "2", "RUSTUP_TOOLCHAIN": "1.99.1", "CARGO_TARGET_DIR": str(self.root / "target")},
            "host": current.file_binding(self.directory / "ww"), "fhs": self.components,
            "build_log": current.file_binding(self.directory / "build.log"), "exit": 0,
            "native_acceptance": False, "publication_allowed": False}
        self.freeze()

    def freeze(self):
        current.write_json(self.directory / "BUILD-RECEIPT.json", self.receipt)
        current.write_json(self.directory / "PROVENANCE.json", {
            "repository": "https://github.com/" + self.receipt["ci"].get("source_repository", "wetware/ww"),
            "revision": self.revision, "builder": self.receipt["builder"]})
        (self.directory / "REVISION").write_text(self.revision + "\n")
        (self.directory / "SHA256SUMS").write_text(self.receipt["host"]["sha256"] + "  ww\n")
        with zipfile.ZipFile(self.directory / "candidate.zip", "w", compression=zipfile.ZIP_DEFLATED) as archive:
            for name in sorted(current.original.MEMBERS | {"BUILD-RECEIPT.json"}):
                archive.write(self.directory / name, name)
        self.bind_archive()

    def bind_archive(self):
        current.write_json(self.directory / "candidate.json", {"schema": 1, "policy": current.BUILD_POLICY,
            "build_receipt": current.file_binding(self.directory / "BUILD-RECEIPT.json"),
            "archive": current.file_binding(self.directory / "candidate.zip")})

    def test_exact_release_receipt_and_native_marker(self):
        self.assertEqual(current.verify_build_files(self.directory, "release"), self.receipt)
        marker = {"path": str(self.directory / "ww"), "archive_path": str(self.directory / "candidate.zip"),
            "sha256": self.receipt["host"]["sha256"], "archive_sha256": current.file_binding(self.directory / "candidate.zip")["sha256"],
            "revision": self.revision, "builder": self.builder, "run_id": 123, "attempt": 1}
        qualifier.verify_producer_files(marker, current.TARGET, os.geteuid())
        with self.assertRaises(RuntimeError):
            qualifier.validate_producer(marker, "x86_64-unknown-linux-gnu")

    def test_changed_host_archive_receipt_and_log_refuse(self):
        for name in ("ww", "candidate.zip", "BUILD-RECEIPT.json", "build.log", "fhs/boot/kernel.wasm"):
            with self.subTest(name=name):
                path = self.directory / name
                raw = path.read_bytes()
                path.write_bytes(raw + b" ")
                with self.assertRaises((RuntimeError, ValueError)):
                    current.verify_build_files(self.directory, "release")
                path.write_bytes(raw)

    def test_rebound_invalid_build_contracts_refuse(self):
        original = copy.deepcopy(self.receipt)
        mutations = [("profile", "development"), ("target", "x86_64-unknown-linux-gnu"),
                     ("command", ["cargo", "build"]), ("exit", 1), ("native_acceptance", True),
                     ("publication_allowed", True), ("profile_settings", current.PROFILE["development"])]
        for key, value in mutations:
            with self.subTest(key=key):
                self.receipt = copy.deepcopy(original)
                self.receipt[key] = value
                self.freeze()
                with self.assertRaises(RuntimeError):
                    current.verify_build_files(self.directory, "release")

    def test_ci_release_cannot_claim_dispatch_other_source_or_completed_job(self):
        original = copy.deepcopy(self.receipt)
        for key, value in (("event", "workflow_dispatch"), ("branch", "master"),
                           ("source_repository", "someone/ww"), ("completed_job_receipt", True),
                           ("uploaded_producer_artifact", True), ("run_id", 0)):
            with self.subTest(key=key):
                self.receipt = copy.deepcopy(original)
                self.receipt["ci"][key] = value
                self.freeze()
                with self.assertRaises(RuntimeError):
                    current.verify_build_files(self.directory, "release")

    def test_rebound_wrong_elf_and_revision_refuse(self):
        for offset in (18, 64):
            with self.subTest(offset=offset):
                raw = bytearray((self.directory / "ww").read_bytes())
                raw[offset] ^= 1
                (self.directory / "ww").write_bytes(raw)
                self.receipt["host"] = current.file_binding(self.directory / "ww")
                self.freeze()
                with self.assertRaisesRegex(RuntimeError, "target or embedded revision"):
                    current.verify_build_files(self.directory, "release")

    def test_rebound_unsafe_archive_members_refuse(self):
        for name, mode in (("../ww", stat.S_IFREG | 0o644), ("ww", stat.S_IFLNK | 0o777)):
            with self.subTest(name=name):
                self.freeze()
                path = self.directory / "candidate.zip"
                with zipfile.ZipFile(path) as archive:
                    entries = [(entry.filename, archive.read(entry)) for entry in archive.infolist()]
                with zipfile.ZipFile(path, "w") as archive:
                    for filename, raw in entries:
                        entry = zipfile.ZipInfo(name if filename == "ww" else filename)
                        entry.create_system = 3
                        entry.external_attr = (mode if filename == "ww" else stat.S_IFREG | 0o644) << 16
                        archive.writestr(entry, raw)
                self.bind_archive()
                with self.assertRaises(RuntimeError):
                    current.verify_build_files(self.directory, "release")

    def test_symlink_and_wrong_owner_refuse(self):
        with self.assertRaises(RuntimeError):
            current.verify_build_files(self.directory, "release", owner=os.geteuid() + 1)
        alias = self.root / "alias"
        alias.symlink_to(self.directory, target_is_directory=True)
        with self.assertRaises(RuntimeError):
            current.verify_build_files(alias, "release")

    def test_unbound_fhs_and_extra_input_files_refuse(self):
        for name in ("fhs/extra", "extra.json"):
            with self.subTest(name=name):
                path = self.directory / name
                path.write_text("unbound package input")
                with self.assertRaisesRegex(RuntimeError, "input file set differs"):
                    current.verify_build_files(self.directory, "release")
                path.unlink()

    def test_current_pair_under_permissive_umask_preserves_bytes_and_original_failures(self):
        from test_verify_native_release_inputs import Fixture
        original = Fixture()
        original.setUp()
        self.addCleanup(original.doCleanups)
        old_revision = original.pins["producers"]["b"]["revision"]
        source = {"artifact_revision": old_revision, "checkout_revision": old_revision, "runtime_source_equal": True}
        historical = original.root / "historical"
        with patch.object(current.original, "verify_source", return_value=source):
            current.original.verify(original.root, original.inputs, historical, original.pins_path)
        output = self.root / "pair"
        inherited_umask = os.umask(0)
        try:
            with patch.object(current, "verify_candidate", return_value=self.receipt):
                record = current.combine(self.root, self.directory, self.revision, historical, original.pins_path, output)
        finally:
            os.umask(inherited_umask)
        self.assertEqual(record["slot_identities"], {"a": "original-producer-A", "b": "current-source-candidate-C"})
        self.assertEqual((output / "a.zip").read_bytes(), (historical / "a.zip").read_bytes())
        self.assertEqual((output / "b.zip").read_bytes(), (self.directory / "candidate.zip").read_bytes())
        for label in ("a", "b"):
            self.assertEqual(stat.S_IMODE((output / (label + ".zip")).stat().st_mode), 0o644)
        self.assertEqual(lane.verify_input_policy(record), {"a": "failure", "b": "failure"})
        self.assertNotEqual(record["b"]["revision"], record["historical_baseline"]["b"]["revision"])
        with patch.object(current, "verify_candidate", return_value=self.receipt):
            with self.assertRaisesRegex(RuntimeError, "output must be absent"):
                current.combine(self.root, self.directory, self.revision, historical, original.pins_path, output)

    def test_current_source_must_equal_recorded_source(self):
        with patch.object(current, "source_snapshot", return_value=self.source), patch.object(current, "verify_profile"):
            current.verify_candidate(self.root, self.directory, self.revision, "release")
        with patch.object(current, "source_snapshot", return_value=dict(self.source, component_tree_equal=False)):
            with self.assertRaisesRegex(RuntimeError, "source changed"):
                current.verify_candidate(self.root, self.directory, self.revision, "release")
        with self.assertRaisesRegex(RuntimeError, "revision differs"):
            current.verify_candidate(self.root, self.directory, "3" * 40, "release")

    def test_local_development_is_explicit_and_has_no_fake_run(self):
        self.receipt.update(profile="development", profile_settings=current.PROFILE["development"],
            command=current.build_command("development"),
            builder=f"local-native-diagnostic:{self.revision}:{current.TARGET}:development")
        self.receipt["environment"]["CARGO_PROFILE_DEV_DEBUG"] = "0"
        self.receipt["ci"] = {"kind": "local-native-diagnostic", "revision": self.revision,
                              "completed_job_receipt": False, "uploaded_producer_artifact": False}
        self.freeze()
        current.verify_build_files(self.directory, "development")
        self.receipt["ci"]["run_id"] = 1
        self.freeze()
        with self.assertRaisesRegex(RuntimeError, "cannot assert CI"):
            current.verify_build_files(self.directory, "development")


class SourceAndContext(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmp.cleanup)
        self.root = Path(self.tmp.name).resolve()
        self.checkout = self.root / "checkout"
        self.checkout.mkdir()
        (self.checkout / "Cargo.toml").write_text('[profile.release]\nopt-level="z"\nlto=true\nstrip=true\n[profile.release.package.ww]\nopt-level=2\n')
        (self.checkout / ".github/workflows").mkdir(parents=True)
        (self.checkout / ".github/workflows/rust.yml").write_text('name: test\njobs:\n  build-binaries:\n    run: build\n')
        self.git("init", "-q")
        self.git("add", ".")
        self.git("-c", "user.name=fixture", "-c", "user.email=fixture@example.invalid", "commit", "-qm", "fixture")
        self.revision = self.git("rev-parse", "HEAD").strip()

    def git(self, *args):
        return subprocess.check_output(["git", "-C", str(self.checkout), *args], stderr=subprocess.PIPE).decode()

    def test_source_detects_dirty_changed_head_and_component_tree(self):
        result = current.source_snapshot(self.checkout, self.revision, self.revision)
        self.assertTrue(result["runtime_source_equal"])
        cargo = self.checkout / "Cargo.toml"
        raw = cargo.read_bytes()
        cargo.write_bytes(raw + b"# changed\n")
        with self.assertRaisesRegex(RuntimeError, "source drift"):
            current.source_snapshot(self.checkout, self.revision, self.revision)
        self.git("add", "Cargo.toml")
        self.git("-c", "user.name=fixture", "-c", "user.email=fixture@example.invalid", "commit", "-qm", "changed")
        with self.assertRaisesRegex(RuntimeError, "HEAD differs"):
            current.source_snapshot(self.checkout, self.revision, self.revision)
        head = self.git("rev-parse", "HEAD").strip()
        with self.assertRaisesRegex(RuntimeError, "component artifact source tree differs"):
            current.source_snapshot(self.checkout, head, self.revision)

    def test_custom_cargo_home_configuration_refuses(self):
        cargo_home = self.root / "custom-cargo-home"
        cargo_home.mkdir()
        (cargo_home / "config.toml").write_text('[profile.release]\nopt-level=0\n')
        with patch.dict(os.environ, {"CARGO_HOME": str(cargo_home)}):
            with self.assertRaisesRegex(RuntimeError, "unreviewed Cargo configuration"):
                current.verify_profile(self.checkout, "release")

    def test_privileged_source_read_trust_is_limited_to_one_checkout_and_command(self):
        with patch.object(current.original.subprocess, "run", return_value=SimpleNamespace(
                returncode=0, stdout=b"source", stderr=b"")) as run:
            self.assertEqual(current.original.git(self.checkout, "rev-parse", "HEAD"), b"source")
        self.assertEqual(run.call_args.args[0], ["git", "--no-replace-objects", "-c",
            "safe.directory=" + str(self.checkout), "-C", str(self.checkout), "rev-parse", "HEAD"])

    def test_builder_under_permissive_umask_preserves_command_environment_and_custody(self):
        source = current.source_snapshot(self.checkout, self.revision, self.revision)
        for name, source_name in current.FHS_SOURCE.items():
            path = self.checkout / "target" / name
            path.parent.mkdir(parents=True, exist_ok=True)
            path.write_bytes(b"\x00asm\x0d\x00\x01\x00" + name.encode())
            generated = self.checkout / source_name
            generated.parent.mkdir(parents=True, exist_ok=True)
            generated.write_bytes(path.read_bytes())
        output = self.root / "build"
        def fake_build(command, **kwargs):
            self.assertEqual(command, ["cargo", "build", "--release", "--locked", "--bin", "ww", "--target", current.TARGET])
            env = kwargs["env"]
            self.assertNotIn("RUSTFLAGS", env)
            self.assertNotIn("CARGO_PROFILE_RELEASE_OPT_LEVEL", env)
            self.assertEqual(env["RUSTUP_TOOLCHAIN"], "1.99.1")
            self.assertEqual(env["CARGO_BUILD_JOBS"], "2")
            self.assertEqual(kwargs["umask"], 0o022)
            binary = Path(env["CARGO_TARGET_DIR"]) / current.TARGET / "release/ww"
            binary.parent.mkdir(parents=True)
            host = bytearray(64)
            host[:7] = b"\x7fELF\x02\x01\x01"
            struct.pack_into("<HHI", host, 16, 3, 183, 1)
            struct.pack_into("<H", host, 52, 64)
            binary.write_bytes(host + self.revision.encode())
            kwargs["stdout"].write(b"fixture build complete\n")
            return SimpleNamespace(returncode=0)
        with patch.object(current.platform, "system", return_value="Linux"), \
                patch.object(current.platform, "machine", return_value="aarch64"), \
                patch.object(current.platform, "uname", return_value=("Linux", "fixture", "1", "1", "aarch64", "")), \
                patch.object(current, "source_snapshot", return_value=source), \
                patch.object(current, "verify_profile", return_value=current.PROFILE["release"]), \
                patch.object(current, "userspace_description", return_value={"path": "/usr/lib/os-release", "contents": "ID=fixture"}), \
                patch.object(current.subprocess, "run", side_effect=fake_build), \
                patch.object(current.subprocess, "check_output", return_value=b"fixture tool version\n"), \
                patch.dict(os.environ, {"RUSTFLAGS": "--cfg injected", "CARGO_PROFILE_RELEASE_OPT_LEVEL": "0"}):
            inherited_umask = os.umask(0)
            try:
                receipt = current.build(self.checkout, output, self.revision, self.revision, "release", local_diagnostic=True)
            finally:
                os.umask(inherited_umask)
        self.assertEqual(receipt["ci"]["kind"], "local-native-diagnostic")
        for path in output.rglob("*"):
            self.assertFalse(path.stat().st_mode & 0o022, str(path))
        current.verify_build_files(output, "release")

    def test_actual_ci_identity_and_release_branch_gate(self):
        event = self.root / "event.json"
        payload = {"pull_request": {"head": {"repo": {"full_name": "wetware/ww"},
                    "ref": "lthibault/pasted-text-v38", "sha": self.revision}}}
        current.write_json(event, payload)
        env = {"GITHUB_ACTIONS": "true", "GITHUB_REPOSITORY": "wetware/ww", "GITHUB_RUN_ID": "123",
               "GITHUB_RUN_ATTEMPT": "1", "GITHUB_JOB": "test", "GITHUB_EVENT_PATH": str(event),
               "GITHUB_EVENT_NAME": "pull_request", "GITHUB_SHA": self.revision}
        with patch.dict(os.environ, env, clear=True):
            current.ci_context(self.revision, "release", 123, 1, "test")
            payload["pull_request"]["head"]["repo"]["full_name"] = "fork/ww"
            current.write_json(event, payload)
            self.assertEqual(current.ci_context(self.revision, "development", 123, 1, "test")["source_repository"], "fork/ww")
            with self.assertRaises(RuntimeError):
                current.ci_context(self.revision, "release", 123, 1, "test")
            with self.assertRaises(RuntimeError):
                current.ci_context(self.revision, "development", 124, 1, "test")
        with patch.dict(os.environ, {}, clear=True):
            with self.assertRaisesRegex(RuntimeError, "actual GitHub Actions"):
                current.ci_context(self.revision, "release", 123, 1, "test")

    def test_development_merge_source_is_explicit_and_unrelated_revision_refuses(self):
        event = self.root / "event.json"
        current.write_json(event, {"pull_request": {"head": {"repo": {"full_name": "fork/ww"},
                    "ref": "feature", "sha": "1" * 40}}})
        env = {"GITHUB_ACTIONS": "true", "GITHUB_REPOSITORY": "wetware/ww", "GITHUB_RUN_ID": "123",
               "GITHUB_RUN_ATTEMPT": "1", "GITHUB_JOB": "test", "GITHUB_EVENT_PATH": str(event),
               "GITHUB_EVENT_NAME": "pull_request", "GITHUB_SHA": self.revision}
        with patch.dict(os.environ, env, clear=True):
            result = current.ci_context(self.revision, "development", 123, 1, "test")
            self.assertEqual(result["revision_kind"], "pull-request-merge")
            self.assertEqual(result["source_repository"], "wetware/ww")
            self.assertEqual(result["head_repository"], "fork/ww")
            self.assertEqual(result["head_revision"], "1" * 40)
            for revision, profile in (("2" * 40, "development"), (self.revision, "release")):
                with self.subTest(revision=revision, profile=profile), self.assertRaisesRegex(RuntimeError, "neither the exact head"):
                    current.ci_context(revision, profile, 123, 1, "test")


class IntegrationPolicy(unittest.TestCase):
    def test_current_slot_is_distinct_and_both_original_failures_survive(self):
        old = {"policy": "pr808-original-pair-diagnostic-only", "status": "VERIFIED_DIAGNOSTIC_INPUTS",
               "native_acceptance": False, "publication_allowed": False, "required_checks_success": False,
               "source": {"runtime_source_equal": True, "checkout_revision": "b", "artifact_revision": "b"},
               "producer_evidence": {label: {"required_checks": {"Test": {"conclusion": "failure"}}} for label in ("a", "b")}}
        self.assertEqual(lane.verify_input_policy(old), {"a": "failure", "b": "failure"})
        source = dict(old["source"], checkout_revision="c", artifact_revision="c")
        value = dict(old, policy=current.POLICY, historical_baseline=copy.deepcopy(old), source=source,
            slot_identities={"a": "original-producer-A", "b": "current-source-candidate-C"},
            b={"revision": "c", "sha256": "hash", "builder": "current"},
            current_build={"source": source, "ci": {"revision": "c", "completed_job_receipt": False,
                            "uploaded_producer_artifact": False}, "host": {"sha256": "hash"},
                            "builder": "current", "profile": "release"})
        self.assertEqual(lane.verify_input_policy(value), {"a": "failure", "b": "failure"})
        for change in ("historical", "slot", "source", "source-revision"):
            broken = copy.deepcopy(value)
            if change == "historical":
                broken["historical_baseline"]["producer_evidence"]["b"]["required_checks"]["Test"]["conclusion"] = "success"
            elif change == "slot":
                broken["slot_identities"]["b"] = "original-producer-B"
            elif change == "source":
                broken["current_build"]["ci"]["revision"] = "another"
            else:
                broken["source"]["checkout_revision"] = "historical"
            with self.assertRaises(RuntimeError):
                lane.verify_input_policy(broken)


if __name__ == "__main__":
    unittest.main()
