#!/usr/bin/env python3
"""Exercise the actual compiled build.rs in a disposable manifest directory."""
import os
from pathlib import Path
import shutil
import subprocess
import tempfile
import unittest


ROOT = Path(__file__).resolve().parents[1]
PAYLOADS = ("std/kernel/bin/main.wasm", "std/status/bin/status.wasm")


class BuildPayloads(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        target = Path(os.environ.get("CARGO_TARGET_DIR", ROOT / "target"))
        scripts = list(target.glob("debug/build/ww-*/build-script-build"))
        if not scripts:
            raise RuntimeError("compile the Host or stage_fhs before this test")
        cls.script = max(scripts, key=lambda path: path.stat().st_mtime).resolve()

    def setUp(self):
        temporary = tempfile.TemporaryDirectory()
        self.addCleanup(temporary.cleanup)
        self.manifest = Path(temporary.name)
        for relative in PAYLOADS:
            destination = self.manifest / relative
            destination.parent.mkdir(parents=True, exist_ok=True)
            shutil.copyfile(ROOT / relative, destination)

    def run_build(self, profile):
        env = dict(os.environ, CARGO_MANIFEST_DIR=str(self.manifest),
                   CARGO_TARGET_DIR=str(self.manifest / "target"), PROFILE=profile)
        return subprocess.run([str(self.script)], cwd=self.manifest, env=env,
                              text=True, capture_output=True, timeout=30)

    def test_debug_and_release_embed_the_same_nonempty_payloads(self):
        outputs = []
        for profile in ("debug", "release"):
            result = self.run_build(profile)
            self.assertEqual(result.returncode, 0, result.stderr)
            flags = [line for line in result.stdout.splitlines()
                     if line.startswith("cargo:rustc-cfg=has_wasm_")]
            self.assertEqual(len(flags), 2)
            outputs.append(flags)
        self.assertEqual(*outputs)

    def test_release_rejects_missing_empty_and_symlink_payloads(self):
        for relative in PAYLOADS:
            path = self.manifest / relative
            original = path.read_bytes()
            for kind in ("missing", "empty", "symlink"):
                path.unlink(missing_ok=True)
                if kind == "empty":
                    path.touch()
                elif kind == "symlink":
                    target = self.manifest / "outside.wasm"
                    target.write_bytes(original)
                    path.symlink_to(target)
                result = self.run_build("release")
                self.assertNotEqual(result.returncode, 0, (relative, kind))
                self.assertIn("Missing, empty, or non-regular", result.stderr)
            path.unlink(missing_ok=True)
            path.write_bytes(original)

    def test_debug_missing_payloads_do_not_enable_embedding(self):
        for relative in PAYLOADS:
            (self.manifest / relative).unlink()
        result = self.run_build("debug")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertNotIn("cargo:rustc-cfg=has_wasm_", result.stdout)
        self.assertIn("cargo:warning=", result.stdout)


if __name__ == "__main__":
    unittest.main()
