#!/usr/bin/env python3
"""Input refusal controls; synthetic ELF fixtures never execute native code."""

import copy
import json
import os
from pathlib import Path
import stat
import struct
import subprocess
import sys
import tempfile
import unittest
import warnings
import zipfile
from unittest.mock import patch

ROOT = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(ROOT / "scripts"))
import verify_native_release_inputs as verifier


def write_json(path, value):
    path.write_text(json.dumps(value) + "\n")


class Fixture(unittest.TestCase):
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory()
        self.addCleanup(self.temporary.cleanup)
        self.root = Path(self.temporary.name).resolve()
        self.inputs = self.root / "inputs"
        self.inputs.mkdir()
        self.pins = json.loads((ROOT / ".github/native-release-integration.json").read_text())
        self.members = {}
        for label in ("a", "b"):
            pin = self.pins["producers"][label]
            header = bytearray(64)
            header[:7] = b"\x7fELF\x02\x01\x01"
            struct.pack_into("<HHI", header, 16, 3, 183, 1)
            struct.pack_into("<H", header, 52, 64)
            host = bytes(header) + pin["revision"].encode()
            pin["sha256"], pin["size"] = verifier.digest(host), len(host)
            builder = f"github-actions:{pin['run_id']}:{pin['attempt']}:{self.pins['target']}"
            self.members[label] = {"ww": host, "REVISION": (pin["revision"] + "\n").encode(),
                                   "SHA256SUMS": (pin["sha256"] + "  ww\n").encode(),
                                   "PROVENANCE.json": json.dumps({
                                       "repository": "https://github.com/wetware/ww",
                                       "revision": pin["revision"], "builder": builder}).encode()}
            for name in verifier.FHS:
                self.members[label][name] = b"\x00asm\x0d\x00\x01\x00" + name.encode()
                pin["fhs"][name] = {"sha256": verifier.digest(self.members[label][name]),
                                    "size": len(self.members[label][name])}
            self.archive(label)
            self.receipts(label)
        self.pins_path = self.root / "pins.json"
        write_json(self.pins_path, self.pins)

    def archive(self, label="a", values=None, modes=None):
        values = self.members[label] if values is None else values
        path = self.inputs / f"{label}.zip"
        with warnings.catch_warnings():
            warnings.simplefilter("ignore", UserWarning)
            with zipfile.ZipFile(path, "w", compression=zipfile.ZIP_DEFLATED) as output:
                for name, raw in (values.items() if isinstance(values, dict) else values):
                    entry = zipfile.ZipInfo(name)
                    entry.create_system = 3
                    entry.external_attr = (modes or {}).get(name, stat.S_IFREG | 0o644) << 16
                    output.writestr(entry, raw)
        pin = self.pins["producers"][label]
        pin["archive_sha256"], pin["archive_size"] = verifier.digest(path.read_bytes()), path.stat().st_size

    def receipts(self, label):
        pin = self.pins["producers"][label]
        api = "https://api.github.com/repos/wetware/ww/actions"
        web = "https://github.com/wetware/ww/actions"
        run_id, artifact_id = pin["run_id"], pin["artifact_id"]
        source = {"head_sha": pin["revision"], "head_branch": self.pins["branch"]}
        repo = {"full_name": "wetware/ww", "id": self.pins["repository_id"]}
        run = {"id": run_id, "run_attempt": pin["attempt"], **source,
               "path": self.pins["workflow_path"], "workflow_id": self.pins["workflow_id"],
               "event": "workflow_dispatch", "status": "completed", "conclusion": "failure",
               "url": f"{api}/runs/{run_id}", "html_url": f"{web}/runs/{run_id}",
               "repository": repo, "head_repository": repo}
        artifact = {"id": artifact_id, "name": self.pins["artifact_name"], "expired": False,
                    "expires_at": "2999-01-01T00:00:00Z", "size_in_bytes": pin["archive_size"],
                    "digest": "sha256:" + pin["archive_sha256"],
                    "url": f"{api}/artifacts/{artifact_id}", "archive_download_url": f"{api}/artifacts/{artifact_id}/zip",
                    "workflow_run": {"id": run_id, **source, "repository_id": repo["id"], "head_repository_id": repo["id"]}}
        jobs = []
        for name, check in pin["required_checks"].items():
            jobs.append({"name": name, **check, "run_id": run_id, "run_attempt": pin["attempt"], **source,
                         "url": f"{api}/jobs/{check['id']}", "html_url": f"{web}/runs/{run_id}/job/{check['id']}",
                         "steps": [{"name": step, "status": "completed", "conclusion": "success"}
                                   for step in ("Build (cross)", "Package binary", "Run actions/upload-artifact@v7")]})
        for suffix, value in (("artifact", artifact), ("run", run), ("jobs", {"total_count": len(jobs), "jobs": jobs}),
                              ("build-job", next(job for job in jobs if job["id"] == pin["build_job_id"]))):
            write_json(self.inputs / f"{label}-{suffix}.json", value)
        (self.inputs / f"{label}-build.log").write_text(
            f"SHA256 digest of uploaded artifact is {pin['archive_sha256']}\n"
            f"Artifact ww-linux-aarch64 successfully finalized. Artifact ID {artifact_id}\n"
            f"Artifact ww-linux-aarch64 has been successfully uploaded! Final size is {pin['archive_size']} bytes. Artifact ID is {artifact_id}\n"
            f"Artifact download URL: {web}/runs/{run_id}/artifacts/{artifact_id}\n")

    def mutate(self, suffix, callback, label="a"):
        path = self.inputs / f"{label}-{suffix}.json"
        value = json.loads(path.read_text())
        callback(value)
        write_json(path, value)

    def verify_archive(self, label="a"):
        return verifier.verify_archive(self.inputs / f"{label}.zip", self.pins["producers"][label],
                                       self.pins, self.root / f"extracted-{label}")


class Inputs(Fixture):
    def test_exact_pair_keeps_required_test_failure_and_marker_compatibility(self):
        verifier.validate_pins(self.pins)
        for label in ("a", "b"):
            receipts = verifier.verify_receipts(self.inputs, label, self.pins)
            self.assertEqual(receipts["required_checks"]["Test"]["conclusion"], "failure")
            self.assertFalse(receipts["required_checks_success"])
            marker = self.verify_archive(label)
            evidence = verifier.verify_producer_files(marker, self.pins["target"], owner=os.geteuid())
            self.assertEqual(evidence["embedded_revision_bytes"], self.pins["producers"][label]["revision"])
            self.assertEqual({str(p.relative_to(self.root / f'extracted-{label}'))
                              for p in (self.root / f'extracted-{label}').rglob('*') if p.is_file()}, verifier.MEMBERS)

    def test_unreviewed_producer_and_success_relabel_refuse(self):
        for field, value in (("run_id", 999), ("attempt", 2), ("revision", "f" * 40),
                             ("workflow_conclusion", "success")):
            with self.subTest(field=field):
                pins = copy.deepcopy(self.pins)
                pins["producers"]["b"][field] = value
                with self.assertRaises(RuntimeError):
                    verifier.validate_pins(pins)
        self.pins["producers"]["a"]["required_checks"]["Test"]["conclusion"] = "success"
        with self.assertRaisesRegex(RuntimeError, "status must be preserved"):
            verifier.validate_pins(self.pins)

    def test_archive_digest_mismatch_refuses_before_extraction(self):
        with (self.inputs / "a.zip").open("ab") as stream:
            stream.write(b"changed")
        with self.assertRaisesRegex(RuntimeError, "archive size or digest"):
            self.verify_archive()
        self.assertFalse((self.root / "extracted-a").exists())

    def test_wrong_elf_target_refuses_even_with_matching_digests(self):
        host = bytearray(self.members["a"]["ww"])
        struct.pack_into("<H", host, 18, 62)
        self.members["a"]["ww"] = bytes(host)
        self.pins["producers"]["a"]["sha256"] = verifier.digest(host)
        self.archive()
        with self.assertRaisesRegex(RuntimeError, "ELF64"):
            self.verify_archive()

    def test_missing_embedded_revision_refuses(self):
        self.members["a"]["ww"] = self.members["a"]["ww"][:64] + b"x" * 40
        self.pins["producers"]["a"]["sha256"] = verifier.digest(self.members["a"]["ww"])
        self.archive()
        with self.assertRaisesRegex(RuntimeError, "embedded revision"):
            self.verify_archive()

    def test_duplicate_and_aliased_zip_paths_refuse(self):
        for extra in ("ww", "../outside", "fhs/bin/../bin/status.wasm", "fhs\\bin\\status.wasm", "/ww"):
            with self.subTest(extra=extra):
                self.archive(values=[*self.members["a"].items(), (extra, b"wrong")])
                with self.assertRaisesRegex(RuntimeError, "duplicate|member set"):
                    self.verify_archive()

    def test_symlink_and_oversized_zip_member_refuse(self):
        self.archive(modes={"ww": stat.S_IFLNK | 0o777})
        with self.assertRaisesRegex(RuntimeError, "member type"):
            self.verify_archive()
        values = dict(self.members["a"], REVISION=b"x" * (verifier.MAX_SIDECAR + 1))
        self.archive(values=values)
        with self.assertRaisesRegex(RuntimeError, "member exceeds"):
            self.verify_archive()

    def test_sidecars_and_fhs_cannot_be_substituted(self):
        for name in (*sorted(verifier.SIDECARS), *sorted(verifier.FHS)):
            with self.subTest(name=name):
                values = dict(self.members["a"])
                values[name] = b"x" * len(values[name])
                self.archive(values=values)
                with self.assertRaises((RuntimeError, ValueError)):
                    self.verify_archive()

    def test_api_source_digest_attempt_event_path_and_repository_refuse(self):
        mutations = [
            ("artifact", lambda v: v.update(digest="sha256:" + "0" * 64)),
            ("artifact", lambda v: v["workflow_run"].update(head_sha="f" * 40)),
            ("run", lambda v: v.update(run_attempt=2)),
            ("run", lambda v: v.update(event="pull_request")),
            ("run", lambda v: v.update(path=".github/workflows/other.yml")),
            ("run", lambda v: v["head_repository"].update(full_name="fork/ww")),
            ("jobs", lambda v: v["jobs"][0].update(head_branch="other")),
        ]
        for suffix, change in mutations:
            with self.subTest(suffix=suffix):
                self.receipts("a")
                self.mutate(suffix, change)
                with self.assertRaises(RuntimeError):
                    verifier.verify_receipts(self.inputs, "a", self.pins)

    def test_expired_missing_and_paginated_inputs_refuse(self):
        self.mutate("artifact", lambda v: v.update(expires_at="2020-01-01T00:00:00Z"))
        with self.assertRaisesRegex(RuntimeError, "expired"):
            verifier.verify_receipts(self.inputs, "a", self.pins)
        self.receipts("a")
        self.mutate("artifact", lambda v: v.update(expired=True))
        with self.assertRaisesRegex(RuntimeError, "expired"):
            verifier.verify_receipts(self.inputs, "a", self.pins)
        self.receipts("a")
        self.mutate("jobs", lambda v: v.update(total_count=v["total_count"] + 1))
        with self.assertRaisesRegex(RuntimeError, "paginated"):
            verifier.verify_receipts(self.inputs, "a", self.pins)
        (self.inputs / "a-run.json").unlink()
        with self.assertRaises(FileNotFoundError):
            verifier.verify_receipts(self.inputs, "a", self.pins)

    def test_missing_and_relabelled_required_checks_refuse(self):
        self.mutate("jobs", lambda v: v.update(jobs=[j for j in v["jobs"] if j["name"] != "Test"], total_count=5))
        with self.assertRaisesRegex(RuntimeError, "missing required check: Test"):
            verifier.verify_receipts(self.inputs, "a", self.pins)
        self.receipts("a")
        self.mutate("jobs", lambda v: next(j for j in v["jobs"] if j["name"] == "Test").update(conclusion="success"))
        with self.assertRaisesRegex(RuntimeError, "required check Test"):
            verifier.verify_receipts(self.inputs, "a", self.pins)

    def test_independent_build_and_upload_receipts_are_required(self):
        self.mutate("build-job", lambda v: v.update(conclusion="failure"))
        with self.assertRaisesRegex(RuntimeError, "independent build-job"):
            verifier.verify_receipts(self.inputs, "a", self.pins)
        self.receipts("a")
        (self.inputs / "a-build.log").write_text("build succeeded without an upload receipt\n")
        with self.assertRaisesRegex(RuntimeError, "upload log receipt"):
            verifier.verify_receipts(self.inputs, "a", self.pins)

    def test_missing_build_step_success_refuses(self):
        for suffix in ("jobs", "build-job"):
            def change(value):
                job = value if suffix == "build-job" else next(j for j in value["jobs"] if j["name"] == verifier.REQUIRED[0])
                job["steps"][0]["conclusion"] = "skipped"
            self.mutate(suffix, change)
        with self.assertRaisesRegex(RuntimeError, "successful Build"):
            verifier.verify_receipts(self.inputs, "a", self.pins)

    def test_duplicate_json_keys_refuse(self):
        (self.inputs / "a-run.json").write_text('{"id": 1, "id": 2}')
        with self.assertRaisesRegex(RuntimeError, "duplicate JSON key"):
            verifier.verify_receipts(self.inputs, "a", self.pins)

    def test_atomic_result_keeps_original_markers_and_never_overwrites(self):
        output = self.root / "verified"
        with patch.object(verifier, "verify_source", return_value={"checkout_revision": "f" * 40}):
            result = verifier.verify(ROOT, self.inputs, output, self.pins_path)
            self.assertFalse(result["native_acceptance"])
            self.assertFalse(result["publication_allowed"])
            self.assertEqual(result["b"]["revision"], verifier.AUTHORIZED_PAIR["b"][2])
            self.assertNotEqual(result["b"]["revision"], result["source"]["checkout_revision"])
            self.assertEqual(json.loads((output / "verified-inputs.json").read_text()), result)
            for label in ("a", "b"):
                verifier.verify_producer_files(result[label], self.pins["target"], owner=os.geteuid())
            with self.assertRaisesRegex(RuntimeError, "already exist"):
                verifier.verify(ROOT, self.inputs, output, self.pins_path)

    def test_failed_second_archive_exposes_no_partial_output(self):
        output = self.root / "verified"
        (self.inputs / "b.zip").write_bytes(b"invalid")
        with patch.object(verifier, "verify_source", return_value={}):
            with self.assertRaisesRegex(RuntimeError, "archive size or digest"):
                verifier.verify(ROOT, self.inputs, output, self.pins_path)
        self.assertFalse(output.exists())
        self.assertEqual(list(self.root.glob(".native-inputs-*")), [])


class SourceEquality(unittest.TestCase):
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory()
        self.addCleanup(self.temporary.cleanup)
        self.root = Path(self.temporary.name).resolve()
        self.git("init", "-q")
        self.git("config", "user.name", "Fixture")
        self.git("config", "user.email", "fixture@example.invalid")
        for name, raw in {"src/main.rs": b"fn main() {}\n", "Cargo.toml": b"[workspace]\n",
                          ".cargo/config.toml": b"[build]\n", "tests/example.py": b"# test\n",
                          "examples/discovery/greeter.capnp": b"@0xb05a9dc09c970cda;\n",
                          ".github/workflows/rust.yml": b"env:\n  CARGO_INCREMENTAL: 0\ndefaults:\n  run:\n    shell: bash\njobs:\n  build-binaries:\n    run: original\n\n  test:\n    run: old\n"}.items():
            path = self.root / name
            path.parent.mkdir(exist_ok=True, parents=True)
            path.write_bytes(raw)
        self.git("add", ".")
        self.git("commit", "-qm", "fixture")
        self.revision = self.git("rev-parse", "HEAD").strip()
        self.pins = {"producers": {"b": {"revision": self.revision}}, "workflow_path": ".github/workflows/rust.yml"}

    def git(self, *args):
        return subprocess.check_output(["git", "-C", str(self.root), *args], stderr=subprocess.PIPE, text=True)

    def verify(self):
        return verifier.verify_source(self.root, self.pins)

    def test_test_and_consumer_job_edits_are_allowed(self):
        (self.root / "tests/example.py").write_text("# new consumer\n")
        workflow = self.root / self.pins["workflow_path"]
        workflow.write_text(workflow.read_text().replace("run: old", "run: new"))
        self.assertTrue(self.verify()["runtime_source_equal"])

    def test_runtime_content_mode_and_addition_drift_refuse(self):
        path = self.root / "src/main.rs"
        path.write_text("fn main() { panic!(); }\n")
        with self.assertRaisesRegex(RuntimeError, "source drift"):
            self.verify()
        path.write_text("fn main() {}\n")
        path.chmod(0o755)
        with self.assertRaisesRegex(RuntimeError, "source drift"):
            self.verify()
        path.chmod(0o644)
        (self.root / "src/untracked.rs").write_text("// untracked source\n")
        with self.assertRaisesRegex(RuntimeError, "source paths differ"):
            self.verify()

    def test_ignored_rust_source_is_not_silently_excluded(self):
        (self.root / ".gitignore").write_text("src/hidden.rs\n")
        (self.root / "src/hidden.rs").write_text("// ignored source\n")
        with self.assertRaisesRegex(RuntimeError, "source paths differ"):
            self.verify()

    def test_cargo_and_producer_recipe_changes_refuse(self):
        (self.root / "Cargo.toml").write_text("[workspace]\n# changed\n")
        with self.assertRaisesRegex(RuntimeError, "source drift: Cargo.toml"):
            self.verify()
        (self.root / "Cargo.toml").write_text("[workspace]\n")
        workflow = self.root / self.pins["workflow_path"]
        workflow.write_text(workflow.read_text().replace("run: original", "run: changed"))
        with self.assertRaisesRegex(RuntimeError, "build-binaries recipe drift"):
            self.verify()

    def test_symlinked_runtime_source_refuses(self):
        path = self.root / "src/main.rs"
        path.rename(self.root / "other.rs")
        path.symlink_to(self.root / "other.rs")
        with self.assertRaisesRegex(RuntimeError, "aliases another path"):
            self.verify()

    def test_build_script_schema_input_drift_refuses(self):
        (self.root / "examples/discovery/greeter.capnp").write_text("@0xb05a9dc09c970cda;\n# changed\n")
        with self.assertRaisesRegex(RuntimeError, "source drift: examples/discovery/greeter.capnp"):
            self.verify()

    def test_inherited_workflow_environment_and_defaults_drift_refuse(self):
        workflow = self.root / self.pins["workflow_path"]
        original = workflow.read_text()
        for before, after in (("CARGO_INCREMENTAL: 0", "CARGO_INCREMENTAL: 1"),
                              ("shell: bash", "shell: sh")):
            with self.subTest(setting=before):
                workflow.write_text(original.replace(before, after))
                with self.assertRaisesRegex(RuntimeError, "inherited settings drift"):
                    self.verify()

    def test_appended_top_level_defaults_cannot_evade_header_binding(self):
        workflow = self.root / self.pins["workflow_path"]
        with workflow.open("a") as stream:
            stream.write("defaults:\n  run:\n    working-directory: alternate\n")
        with self.assertRaisesRegex(RuntimeError, "top-level setting after jobs"):
            self.verify()


if __name__ == "__main__":
    unittest.main()
