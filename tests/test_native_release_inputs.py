#!/usr/bin/env python3
"""Shared bounded input and source checks; fixtures never execute native code."""

import io
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest
from unittest.mock import patch

ROOT = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(ROOT / "scripts"))
import native_release_inputs as inputs


class BoundedInputs(unittest.TestCase):
    def setUp(self):
        temporary = tempfile.TemporaryDirectory()
        self.addCleanup(temporary.cleanup)
        self.root = Path(temporary.name)
        self.path = self.root / "record.json"

    def test_duplicate_keys_and_nonfinite_json_refuse(self):
        for raw in ('{"id": 1, "id": 2}', '{"nested": {"id": 1, "id": 2}}',
                    '{"value": NaN}', '{"value": Infinity}', '{"value": -Infinity}'):
            with self.subTest(raw=raw), self.assertRaises(RuntimeError):
                inputs.parse_json(raw)

    def test_json_records_must_be_objects(self):
        for raw in ('[]', 'null', 'true', '1', '"record"'):
            self.path.write_text(raw)
            with self.subTest(raw=raw), self.assertRaisesRegex(RuntimeError, "must be an object"):
                inputs.read_json(self.path)
        self.path.write_text('{"value": 1}')
        self.assertEqual(inputs.read_json(self.path), {"value": 1})

    def test_file_bounds_and_changed_reads_refuse(self):
        self.path.write_bytes(b'1234')
        self.assertEqual(inputs.read_file(self.path, 4), b'1234')
        with self.assertRaisesRegex(RuntimeError, "exceeds bound"):
            inputs.read_file(self.path, 3)
        with patch.object(Path, "open", return_value=io.BytesIO(b'12')):
            with self.assertRaisesRegex(RuntimeError, "file changed"):
                inputs.read_file(self.path, 4)
        self.path.write_bytes(b'')
        with self.assertRaisesRegex(RuntimeError, "empty"):
            inputs.read_file(self.path, 4)

    def test_directory_and_symlink_inputs_refuse(self):
        with self.assertRaisesRegex(RuntimeError, "not a regular file"):
            inputs.read_file(self.root, 4)
        self.path.write_bytes(b'1234')
        alias = self.root / "alias"
        alias.symlink_to(self.path)
        with self.assertRaisesRegex(RuntimeError, "not a regular file"):
            inputs.read_file(alias, 4)


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
        self.workflow_path = ".github/workflows/rust.yml"

    def git(self, *args):
        return subprocess.check_output(["git", "-C", str(self.root), *args], stderr=subprocess.PIPE, text=True)

    def verify(self):
        return inputs.verify_source(self.root, self.revision, self.workflow_path)

    def test_test_and_consumer_job_edits_are_allowed(self):
        (self.root / "tests/example.py").write_text("# new consumer\n")
        workflow = self.root / self.workflow_path
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
        workflow = self.root / self.workflow_path
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
        workflow = self.root / self.workflow_path
        original = workflow.read_text()
        for before, after in (("CARGO_INCREMENTAL: 0", "CARGO_INCREMENTAL: 1"),
                              ("shell: bash", "shell: sh")):
            with self.subTest(setting=before):
                workflow.write_text(original.replace(before, after))
                with self.assertRaisesRegex(RuntimeError, "inherited settings drift"):
                    self.verify()

    def test_appended_top_level_defaults_cannot_evade_header_binding(self):
        workflow = self.root / self.workflow_path
        with workflow.open("a") as stream:
            stream.write("defaults:\n  run:\n    working-directory: alternate\n")
        with self.assertRaisesRegex(RuntimeError, "top-level setting after jobs"):
            self.verify()



if __name__ == "__main__":
    unittest.main()
