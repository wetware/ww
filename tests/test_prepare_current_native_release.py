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
import warnings
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
                   "event": "pull_request", "branch": "qualification-fixture",
                   "run_id": 123, "attempt": 1, "job": "test-native", "revision": self.revision,
                   "workflow_ref": "wetware/ww/.github/workflows/rust.yml@refs/pull/1/merge",
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
            for name in sorted(current.inputs.MEMBERS | {"BUILD-RECEIPT.json"}):
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
        for key, value in (("event", "workflow_dispatch"),
                           ("source_repository", "someone/ww"), ("completed_job_receipt", True),
                           ("uploaded_producer_artifact", True), ("run_id", 0)):
            with self.subTest(key=key):
                self.receipt = copy.deepcopy(original)
                self.receipt["ci"][key] = value
                self.freeze()
                with self.assertRaises(RuntimeError):
                    current.verify_build_files(self.directory, "release")

    def test_release_branch_name_is_descriptive_not_authority(self):
        self.receipt["ci"]["branch"] = "another-qualification-branch"
        self.freeze()
        self.assertEqual(current.verify_build_files(self.directory, "release"), self.receipt)

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
        for name, mode in (("../ww", stat.S_IFREG | 0o644), ("/ww", stat.S_IFREG | 0o644),
                           ("REVISION", stat.S_IFREG | 0o644),
                           ("fhs/bin/../bin/status.wasm", stat.S_IFREG | 0o644),
                           (r"fhs\bin\status.wasm", stat.S_IFREG | 0o644),
                           ("ww", stat.S_IFLNK | 0o777)):
            with self.subTest(name=name):
                self.freeze()
                path = self.directory / "candidate.zip"
                with zipfile.ZipFile(path) as archive:
                    entries = [(entry.filename, archive.read(entry)) for entry in archive.infolist()]
                with warnings.catch_warnings(), zipfile.ZipFile(path, "w") as archive:
                    warnings.simplefilter("ignore", UserWarning)
                    for filename, raw in entries:
                        entry = zipfile.ZipInfo(name if filename == "ww" else filename)
                        entry.create_system = 3
                        entry.external_attr = (mode if filename == "ww" else stat.S_IFREG | 0o644) << 16
                        archive.writestr(entry, raw)
                self.bind_archive()
                with self.assertRaises(RuntimeError):
                    current.verify_build_files(self.directory, "release")

    def test_oversized_current_sidecar_refuses_after_archive_rebinding(self):
        self.freeze()
        path = self.directory / "REVISION"
        path.write_bytes(b'x' * (current.inputs.MAX_SIDECAR + 1))
        with zipfile.ZipFile(self.directory / "candidate.zip", "w") as archive:
            for name in sorted(current.inputs.MEMBERS | {"BUILD-RECEIPT.json"}):
                archive.write(self.directory / name, name)
        self.bind_archive()
        with self.assertRaisesRegex(RuntimeError, "exceeds bound"):
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

    def second_candidate(self):
        other = Candidate()
        other.setUp()
        self.addCleanup(other.doCleanups)
        prior = other.revision
        other.revision = '3' * 40
        other.builder = "github-actions-pr-native:123:1:" + current.TARGET + ":release"
        binary = other.directory / 'ww'
        binary.write_bytes(binary.read_bytes().replace(prior.encode(), other.revision.encode()))
        other.source.update(checkout_revision=other.revision, artifact_revision=other.revision)
        other.receipt['ci'].update(revision=other.revision, head_revision=self.revision,
            revision_kind='pinned-reduced-baseline', baseline_pin={'schema': 1, 'repository': 'wetware/ww',
                'revision': other.revision, 'contract': 'managed-systemd-v3'})
        other.receipt['environment']['WW_BUILD_GIT_SHA'] = other.revision
        other.receipt['host'] = current.file_binding(binary)
        other.freeze()
        return other

    def roles(self, other):
        pin = {'schema': 1, 'repository': 'wetware/ww', 'revision': other.revision, 'contract': 'managed-systemd-v3'}
        return {'harness_revision': self.revision, 'baseline_pin': pin,
                'baseline_pin_sha256': current.inputs.digest((json.dumps(pin, indent=2) + '\n').encode())}

    def pair(self, other, output):
        def verify(checkout, directory, revision, profile):
            receipt = current.verify_build_files(directory, profile)
            self.assertEqual(receipt['source']['checkout_revision'], revision)
            return receipt
        with patch.object(current, 'verify_candidate', side_effect=verify), \
                patch.object(current, 'pair_source_roles', return_value=self.roles(other)):
            return current.combine_current(self.root, self.directory, self.revision,
                other.root, other.directory, other.revision, output)

    def test_current_pair_preserves_both_actual_build_receipts(self):
        other = self.second_candidate()
        output = self.root / 'pair'
        mask = os.umask(0)
        try:
            result = self.pair(other, output)
        finally:
            os.umask(mask)
        self.assertEqual(result['policy'], current.PAIR_POLICY)
        self.assertEqual(result['slot_identities'], {'a': 'reduced-current-R1', 'b': 'reduced-current-R0'})
        self.assertEqual(result['current_builds'], {'a': self.receipt, 'b': other.receipt})
        for slot, source in [('a', self.directory), ('b', other.directory)]:
            self.assertEqual((output / (slot + '.zip')).read_bytes(), (source / 'candidate.zip').read_bytes())
            self.assertEqual(stat.S_IMODE((output / (slot + '.zip')).stat().st_mode), 0o644)
        lane.verify_input_policy(result)
        with self.assertRaisesRegex(RuntimeError, 'output must be absent'):
            self.pair(other, output)

    def test_current_pair_refuses_identical_bytes_or_changed_second_build(self):
        with self.assertRaisesRegex(RuntimeError, 'source roles'):
            self.pair(self, self.root / 'identical')
        other = self.second_candidate()
        (other.directory / 'ww').write_bytes(b'corrupt')
        with self.assertRaisesRegex(RuntimeError, 'Host binding'):
            self.pair(other, self.root / 'corrupt')
        self.assertFalse((self.root / 'corrupt').exists())

    def test_pair_policy_refuses_swapped_source_profile_and_receipt(self):
        other = self.second_candidate()
        result = self.pair(other, self.root / 'pair')
        for field in ('source', 'profile', 'receipt', 'slots'):
            broken = copy.deepcopy(result)
            if field == 'source':
                broken['current_builds']['a']['source']['checkout_revision'] = other.revision
            elif field == 'profile':
                broken['current_builds']['b']['profile'] = 'development'
            elif field == 'receipt':
                broken['current_builds']['a'] = broken['current_builds']['b']
            else:
                broken['slot_identities']['b'] = 'original-producer-B'
            with self.subTest(field=field), self.assertRaises(RuntimeError):
                lane.verify_input_policy(broken)

    def test_pair_rejects_role_swaps_and_unpinned_local_builds_before_output(self):
        other = self.second_candidate()
        builds = {'a': self.receipt, 'b': other.receipt}
        roles = self.roles(other)
        current.validate_pair_roles(builds, roles)
        for mutation in ('swap', 'older-final', 'unpinned-local', 'mixed-authority'):
            changed = copy.deepcopy(builds)
            if mutation == 'swap':
                changed = {'a': changed['b'], 'b': changed['a']}
            elif mutation == 'older-final':
                changed['a']['ci']['revision'] = '4' * 40
            else:
                for slot in ('a', 'b') if mutation == 'unpinned-local' else ('b',):
                    changed[slot]['ci'] = {'kind': 'local-native-diagnostic', 'revision': changed[slot]['ci']['revision']}
                if mutation == 'unpinned-local':
                    changed['b']['ci']['revision'] = '4' * 40
            with self.subTest(mutation=mutation), patch.object(current, 'verify_candidate', side_effect=[changed['a'], changed['b']]), \
                    patch.object(current, 'pair_source_roles', return_value=roles):
                output = self.root / mutation
                with self.assertRaises(RuntimeError):
                    current.combine_current(self.root, self.directory, self.revision,
                        other.root, other.directory, other.revision, output)
                self.assertFalse(output.exists())
        local = copy.deepcopy(builds)
        for slot in ('a', 'b'):
            local[slot]['ci'] = {'kind': 'local-native-diagnostic', 'revision': local[slot]['ci']['revision']}
        current.validate_pair_roles(local, roles)

    def test_ci_pair_requires_matching_real_run_job_head_and_component_context(self):
        other = self.second_candidate()
        builds = {'a': self.receipt, 'b': other.receipt}
        roles = self.roles(other)
        for field in ('run_id', 'attempt', 'job', 'workflow_sha', 'workflow_ref', 'head_revision', 'head_repository'):
            changed = copy.deepcopy(builds)
            changed['b']['ci'][field] = 999 if field in ('run_id', 'attempt') else 'different'
            with self.subTest(field=field), self.assertRaisesRegex(RuntimeError, 'context'):
                current.validate_pair_roles(changed, roles)
        changed = copy.deepcopy(builds)
        del changed['a']['ci']['job']
        del changed['b']['ci']['job']
        with self.assertRaisesRegex(RuntimeError, 'context'):
            current.validate_pair_roles(changed, roles)
        changed = copy.deepcopy(builds)
        changed['b']['ci']['revision_kind'] = 'pull-request-head'
        with self.assertRaisesRegex(RuntimeError, 'source roles'):
            current.validate_pair_roles(changed, roles)

    def test_current_source_must_equal_recorded_source(self):
        with patch.object(current, "source_snapshot", return_value=self.source), patch.object(current, "verify_profile"):
            current.verify_candidate(self.root, self.directory, self.revision, "release")
        with patch.object(current, "source_snapshot", return_value=dict(self.source, component_tree_equal=False)):
            with self.assertRaisesRegex(RuntimeError, "source changed"):
                current.verify_candidate(self.root, self.directory, self.revision, "release")
        with self.assertRaisesRegex(RuntimeError, "revision differs"):
            current.verify_candidate(self.root, self.directory, "3" * 40, "release")

    def test_development_non_pr_receipt_matches_workflow_source_and_push_policy(self):
        self.receipt.update(profile="development", profile_settings=current.PROFILE["development"],
            command=current.build_command("development"),
            builder=f"github-actions-pr-native:123:1:{current.TARGET}:development")
        self.receipt["environment"]["CARGO_PROFILE_DEV_DEBUG"] = "0"
        self.receipt["source"]["component_revision"] = self.revision
        self.receipt["ci"].update(event="push", branch="master", workflow_sha=self.revision)
        valid = copy.deepcopy(self.receipt)
        for event, branch in (("push", "master"), ("push", "staging"),
                              ("workflow_dispatch", "diagnostic-branch")):
            with self.subTest(event=event, branch=branch):
                self.receipt = copy.deepcopy(valid)
                self.receipt["ci"].update(event=event, branch=branch)
                self.freeze()
                current.verify_build_files(self.directory, "development")
        for change in ("push-branch", "revision", "repository"):
            with self.subTest(refused=change):
                self.receipt = copy.deepcopy(valid)
                if change == "push-branch":
                    self.receipt["ci"]["branch"] = "unreviewed-feature"
                elif change == "revision":
                    # Keep the receipt internally coherent but bind it to the
                    # wrong workflow source; this must still refuse.
                    self.receipt["ci"]["workflow_sha"] = "2" * 40
                    self.receipt["source"]["component_revision"] = "2" * 40
                else:
                    self.receipt["ci"]["source_repository"] = "fork/ww"
                self.freeze()
                with self.assertRaisesRegex(RuntimeError, "development event source identity"):
                    current.verify_build_files(self.directory, "development")

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
        (self.checkout / ".github/workflows/rust.yml").write_text('name: test\njobs:\n  build-wasm:\n    run: make test-wasm\n  build-binaries:\n    run: build\n')
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
        with self.assertRaisesRegex(RuntimeError, "component artifact source differs"):
            current.source_snapshot(self.checkout, head, self.revision)

    def test_equal_tree_snapshot_preserves_original_r0_receipt_schema(self):
        # This is the exact source_snapshot output shape emitted by reviewed R0.
        expected = current.inputs.verify_source(self.checkout, self.revision, '.github/workflows/rust.yml')
        expected.update(component_revision=self.revision, source_tree=self.git('rev-parse', 'HEAD^{tree}').strip(),
                        component_tree_equal=True)
        self.assertEqual(current.source_snapshot(self.checkout, self.revision, self.revision), expected)

    def test_pair_authority_reads_actual_harness_head_and_committed_pin(self):
        baseline = self.revision
        current.write_json(self.checkout / current.BASELINE_PIN, {'schema': 1, 'repository': 'wetware/ww',
                           'revision': baseline, 'contract': 'managed-systemd-v3'})
        with self.assertRaises((RuntimeError, subprocess.SubprocessError)):
            current.pair_source_roles(self.checkout)
        self.git('add', current.BASELINE_PIN)
        self.git('-c', 'user.name=fixture', '-c', 'user.email=fixture@example.invalid', 'commit', '-qm', 'pin baseline')
        roles = current.pair_source_roles(self.checkout)
        self.assertEqual(roles['harness_revision'], self.git('rev-parse', 'HEAD').strip())
        self.assertEqual(roles['baseline_pin']['revision'], baseline)
        path = self.checkout / current.BASELINE_PIN
        path.write_text(path.read_text() + ' ')
        with self.assertRaisesRegex(RuntimeError, 'not committed'):
            current.pair_source_roles(self.checkout)

    def test_non_component_changes_reuse_components_with_unequal_trees(self):
        for name in ('README.md', 'CHANGELOG.md', 'doc/deployment.md', 'doc/release-authentication.md',
                     'scripts/native_release_inputs.py', 'scripts/verify_native_release_inputs.py',
                     'scripts/check_repository_artifacts.py', 'tests/test_repository_artifacts.py',
                     'tests/test_release_workflow.sh',
                     'tests/test_native_release_inputs.py', 'tests/test_verify_native_release_inputs.py',
                     'tests/test_fetch_native_release_inputs.py'):
            with self.subTest(non_component=name):
                path = self.checkout / name
                path.parent.mkdir(parents=True, exist_ok=True)
                path.write_text('qualification documentation')
                self.git('add', name)
                self.git('-c', 'user.name=fixture', '-c', 'user.email=fixture@example.invalid', 'commit', '-qm', 'documentation')
                head = self.git('rev-parse', 'HEAD').strip()
                source = current.source_snapshot(self.checkout, head, self.revision)
                self.assertFalse(source['component_tree_equal'])
                self.assertTrue(source['component_source_equal'])
                self.assertTrue(current.verify_component_binding(source))
                self.assertEqual(len(source['component_manifest_sha256']), 64)
                self.assertNotEqual(source['source_tree'], source['component_source_tree'])

    def test_retired_qualification_deletion_preserves_component_binding(self):
        names = (
            '.github/native-release-integration.json',
            'scripts/fetch_native_release_inputs.py',
            'scripts/verify_native_release_inputs.py',
            'tests/test_fetch_native_release_inputs.py',
            'tests/test_verify_native_release_inputs.py',
            'tests/fixtures/native-release-originals/README.md',
            'tests/fixtures/native-release-originals/.gitattributes',
            'tests/fixtures/native-release-originals/inputs/a-artifact.json',
            'tests/fixtures/native-release-originals/inputs/a-build-job.json',
            'tests/fixtures/native-release-originals/inputs/a-build.log',
            'tests/fixtures/native-release-originals/inputs/a-jobs.json',
            'tests/fixtures/native-release-originals/inputs/a-run.json',
            'tests/fixtures/native-release-originals/inputs/a.zip',
            'tests/fixtures/native-release-originals/inputs/b-artifact.json',
            'tests/fixtures/native-release-originals/inputs/b-build-job.json',
            'tests/fixtures/native-release-originals/inputs/b-build.log',
            'tests/fixtures/native-release-originals/inputs/b-jobs.json',
            'tests/fixtures/native-release-originals/inputs/b-run.json',
            'tests/fixtures/native-release-originals/inputs/b.zip',
            'tests/fixtures/native-release-originals/inputs/capture-manifest.json',
            'tests/fixtures/native-release-originals/inputs/retention.json',
        )
        for name in names:
            path = self.checkout / name
            path.parent.mkdir(parents=True, exist_ok=True)
            path.write_text('generated historical qualification fixture')
        self.git('add', '.')
        self.git('-c', 'user.name=fixture', '-c', 'user.email=fixture@example.invalid', 'commit', '-qm', 'historical paths')
        baseline = self.git('rev-parse', 'HEAD').strip()
        for name in names:
            (self.checkout / name).unlink()
        self.git('add', '-u')
        self.git('-c', 'user.name=fixture', '-c', 'user.email=fixture@example.invalid', 'commit', '-qm', 'fixture cleanup')
        head = self.git('rev-parse', 'HEAD').strip()
        source = current.source_snapshot(self.checkout, head, baseline)
        self.assertFalse(source['component_tree_equal'])
        self.assertTrue(source['component_source_equal'])
        self.assertTrue(current.verify_component_binding(source))

    def test_component_exclusions_are_exact_paths(self):
        for name in ('doc/deployment.md.extra', 'doc/release-authentication.md.extra',
                     'CHANGELOG.md.extra', 'scripts/native_release_inputs.py.extra',
                     'scripts/other_build_input.py', 'src/main.rs',
                     'scripts/check_repository_artifacts.py.extra', 'tests/test_repository_artifacts.py.extra',
                     'tests/test_release_workflow.sh.extra',
                     'tests/fixtures/native-release-originals/inputs/a.zip.extra',
                     'tests/fixtures/native-release-originals/inputs/new-build-input',
                     '.github/native-release-integration.json.extra'):
            with self.subTest(unapproved=name):
                path = self.checkout / name
                path.parent.mkdir(parents=True, exist_ok=True)
                path.write_text('not an approved documentation path')
                self.git('add', name)
                self.git('-c', 'user.name=fixture', '-c', 'user.email=fixture@example.invalid', 'commit', '-qm', 'unapproved path')
                head = self.git('rev-parse', 'HEAD').strip()
                with self.assertRaisesRegex(RuntimeError, 'component artifact source differs'):
                    current.source_snapshot(self.checkout, head, self.revision)
                path.unlink()
                self.git('add', name)
                self.git('-c', 'user.name=fixture', '-c', 'user.email=fixture@example.invalid', 'commit', '-qm', 'remove fixture path')

    def test_component_recipe_is_checked_even_when_other_workflow_jobs_change(self):
        workflow = self.checkout / '.github/workflows/rust.yml'
        workflow.write_text(workflow.read_text().replace('make test-wasm', 'make different-wasm'))
        self.git('add', '.github/workflows/rust.yml')
        self.git('-c', 'user.name=fixture', '-c', 'user.email=fixture@example.invalid', 'commit', '-qm', 'component recipe')
        head = self.git('rev-parse', 'HEAD').strip()
        with self.assertRaisesRegex(RuntimeError, 'component artifact source differs'):
            current.source_snapshot(self.checkout, head, self.revision)

    def test_baseline_ci_authority_requires_exact_pin_from_actual_pr_head(self):
        path = self.checkout / current.BASELINE_PIN
        current.write_json(path, {'schema': 1, 'repository': 'wetware/ww',
                                 'revision': self.revision, 'contract': 'managed-systemd-v3'})
        self.git('add', current.BASELINE_PIN)
        self.git('-c', 'user.name=fixture', '-c', 'user.email=fixture@example.invalid', 'commit', '-qm', 'reviewed pin')
        head = self.git('rev-parse', 'HEAD').strip()
        event = self.root / 'event.json'
        current.write_json(event, {'pull_request': {'head': {'repo': {'full_name': 'wetware/ww'},
                                                            'ref': 'any-branch', 'sha': head}}})
        env = {'GITHUB_ACTIONS': 'true', 'GITHUB_REPOSITORY': 'wetware/ww', 'GITHUB_RUN_ID': '123',
               'GITHUB_RUN_ATTEMPT': '1', 'GITHUB_JOB': 'test', 'GITHUB_EVENT_PATH': str(event),
               'GITHUB_EVENT_NAME': 'pull_request', 'GITHUB_SHA': head}
        with patch.dict(os.environ, env, clear=True), patch.object(current, '__file__', str(self.checkout / 'scripts/prepare.py')):
            context = current.ci_context(self.revision, 'release', 123, 1, 'test')
            self.assertEqual(context['revision_kind'], 'pinned-reduced-baseline')
            self.assertEqual(context['head_revision'], head)
            self.assertEqual(context['revision'], self.revision)
            path.write_text(path.read_text() + ' ')
            with self.assertRaisesRegex(RuntimeError, 'exact PR-head repository content'):
                current.ci_context(self.revision, 'release', 123, 1, 'test')

    def test_custom_cargo_home_configuration_refuses(self):
        cargo_home = self.root / "custom-cargo-home"
        cargo_home.mkdir()
        (cargo_home / "config.toml").write_text('[profile.release]\nopt-level=0\n')
        with patch.dict(os.environ, {"CARGO_HOME": str(cargo_home)}):
            with self.assertRaisesRegex(RuntimeError, "unreviewed Cargo configuration"):
                current.verify_profile(self.checkout, "release")

    def test_privileged_source_read_trust_is_limited_to_one_checkout_and_command(self):
        with patch.object(current.inputs.subprocess, "run", return_value=SimpleNamespace(
                returncode=0, stdout=b"source", stderr=b"")) as run:
            self.assertEqual(current.inputs.git(self.checkout, "rev-parse", "HEAD"), b"source")
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

    def test_failed_or_timed_out_build_never_creates_candidate_receipt(self):
        # Protect failed-build evidence: compiler failure must not create a valid candidate.
        for name, source_name in current.FHS_SOURCE.items():
            for path in (self.checkout / "target" / name, self.checkout / source_name):
                path.parent.mkdir(parents=True, exist_ok=True)
                path.write_bytes(b"\x00asm\x0d\x00\x01\x00" + name.encode())
        for timeout in (False, True):
            output = self.root / ("timeout" if timeout else "failed")
            def fail_build(command, **kwargs):
                kwargs["stdout"].write(b"compiler failed before output\n")
                if timeout:
                    raise subprocess.TimeoutExpired(command, kwargs["timeout"])
                return SimpleNamespace(returncode=1)
            with self.subTest(timeout=timeout), \
                    patch.object(current.platform, "system", return_value="Linux"), \
                    patch.object(current.platform, "machine", return_value="aarch64"), \
                    patch.object(current, "source_snapshot", return_value={}), \
                    patch.object(current, "verify_profile", return_value=current.PROFILE["release"]), \
                    patch.object(current.subprocess, "run", side_effect=fail_build):
                with self.assertRaises(subprocess.TimeoutExpired if timeout else RuntimeError):
                    current.build(self.checkout, output, self.revision, self.revision, "release", local_diagnostic=True)
            self.assertEqual((output / "build.log").read_bytes(), b"compiler failed before output\n")
            self.assertFalse((output / "candidate.json").exists())
            self.assertFalse((output / "BUILD-RECEIPT.json").exists())

    def test_actual_ci_identity_and_release_repository_gate(self):
        event = self.root / "event.json"
        payload = {"pull_request": {"head": {"repo": {"full_name": "wetware/ww"},
                    "ref": "qualification-fixture", "sha": self.revision}}}
        current.write_json(event, payload)
        env = {"GITHUB_ACTIONS": "true", "GITHUB_REPOSITORY": "wetware/ww", "GITHUB_RUN_ID": "123",
               "GITHUB_RUN_ATTEMPT": "1", "GITHUB_JOB": "test", "GITHUB_EVENT_PATH": str(event),
               "GITHUB_EVENT_NAME": "pull_request", "GITHUB_SHA": self.revision}
        with patch.dict(os.environ, env, clear=True):
            self.assertEqual(current.ci_context(self.revision, "release", 123, 1, "test")["branch"],
                             "qualification-fixture")
            payload["pull_request"]["head"]["ref"] = "another-qualification-branch"
            current.write_json(event, payload)
            self.assertEqual(current.ci_context(self.revision, "release", 123, 1, "test")["branch"],
                             "another-qualification-branch")
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
                with self.subTest(revision=revision, profile=profile), self.assertRaisesRegex(RuntimeError, "neither the exact head|baseline"):
                    current.ci_context(revision, profile, 123, 1, "test")


class IntegrationPolicy(unittest.TestCase):
    def test_historical_and_mixed_policies_cannot_authorize_reduced_execution(self):
        for policy in ('pr808-original-pair-diagnostic-only', 'pr808-current-source-native-diagnostic-v1',
                       'pr739-current-forward-native-diagnostic-v1'):
            with self.subTest(policy=policy), self.assertRaises(RuntimeError):
                lane.verify_input_policy({'schema': 1, 'policy': policy})


if __name__ == "__main__":
    unittest.main()
