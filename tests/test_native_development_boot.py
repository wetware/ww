#!/usr/bin/env python3
"""Development fixture ownership controls; never create users or services."""
import os
from pathlib import Path
import hashlib
import subprocess
import sys
import tempfile
import unittest
from unittest.mock import patch

sys.dont_write_bytecode = True
sys.path.insert(0, str(Path(__file__).resolve().parents[1] / 'scripts'))
import run_native_development_boot as boot


class RunnerInputs(unittest.TestCase):
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory()
        self.addCleanup(self.temporary.cleanup)
        self.root = Path(self.temporary.name).resolve()
        self.inputs = self.root / 'inputs'
        self.inputs.mkdir()
        self.tool = self.root / 'tool'
        self.tool.write_bytes(b'tool input')
        self.tool.chmod(0o755)

    def stage_workflow_tool(self):
        workflow = (Path(__file__).resolve().parents[1] / '.github/workflows/rust.yml').read_text()
        marker = '    - name: Stage private package utility\n'
        self.assertIn(marker, workflow, 'workflow must stage the Cargo output before custody checks')
        step = workflow.split(marker, 1)[1].split('\n    - ', 1)[0]
        body = step.split('      run: |\n', 1)[1]
        script = '\n'.join(line[8:] for line in body.splitlines())
        return subprocess.run(['bash', '-eu', '-c', script], cwd=self.root,
                              env={**os.environ, 'WW739_LANE': str(self.inputs)},
                              capture_output=True, text=True, timeout=10)

    def cargo_tool(self):
        source = self.root / 'target/release/ww-release-package'
        source.parent.mkdir(parents=True)
        os.link(self.tool, source)
        return source

    def test_hardlinked_cargo_output_requires_identical_single_link_staging(self):
        source = self.cargo_tool()
        with patch.object(boot, 'verify_candidate') as verify:
            with self.assertRaisesRegex(RuntimeError, 'hardlinks'):
                boot.verify_runner_inputs(self.root, self.inputs, 'd' * 40, (source,))
            verify.assert_not_called()
        result = self.stage_workflow_tool()
        self.assertEqual(result.returncode, 0, result.stderr)
        staged = self.inputs / 'package-tool/ww-release-package'
        self.assertEqual(staged.read_bytes(), source.read_bytes())
        self.assertEqual(staged.stat().st_nlink, 1)
        self.assertEqual(source.stat().st_nlink, 2)
        self.assertEqual(staged.stat().st_uid, source.stat().st_uid)
        self.assertEqual(staged.stat().st_mode & 0o777, 0o755)
        self.assertEqual(staged.parent.stat().st_mode & 0o777, 0o700)
        digest = hashlib.sha256(source.read_bytes()).hexdigest()
        self.assertEqual((self.inputs / 'package-tool.sha256').read_text(), f'{digest}  {staged}\n')
        with patch.object(boot, 'verify_candidate', return_value={'verified': True}) as verify:
            self.assertEqual(boot.verify_runner_inputs(self.root, self.inputs, 'd' * 40, (staged,)),
                             {'verified': True})
            verify.assert_called_once_with(self.root, self.inputs, 'd' * 40,
                                           'development', owner=os.geteuid())
        repeated = self.stage_workflow_tool()
        self.assertNotEqual(repeated.returncode, 0, 'existing stage must never be reused')
        self.assertEqual(staged.read_bytes(), source.read_bytes())

    def test_staging_refuses_writable_or_symlink_cargo_output(self):
        source = self.cargo_tool()
        for kind in ('writable', 'symlink'):
            with self.subTest(kind=kind):
                if kind == 'writable':
                    source.chmod(0o775)
                else:
                    source.unlink()
                    self.tool.chmod(0o755)
                    source.symlink_to(self.tool)
                result = self.stage_workflow_tool()
                self.assertNotEqual(result.returncode, 0)
                self.assertIn('unsafe Cargo package utility', result.stderr)
                self.assertFalse((self.inputs / 'package-tool').exists())

    def test_root_handoff_verifies_source_using_the_original_checkout_owner(self):
        with patch.object(boot, 'verify_candidate', return_value={'verified': True}) as verify:
            result = boot.verify_runner_inputs(self.root, self.inputs, 'd' * 40, (self.tool,))
        self.assertEqual(result, {'verified': True})
        verify.assert_called_once_with(self.root, self.inputs, 'd' * 40, 'development', owner=os.geteuid())

    def test_group_writable_tool_is_refused_before_source_handoff(self):
        self.tool.chmod(0o775)
        with patch.object(boot, 'verify_candidate') as verify:
            with self.assertRaisesRegex(RuntimeError, 'permits other-user writes'):
                boot.verify_runner_inputs(self.root, self.inputs, 'd' * 40, (self.tool,))
        verify.assert_not_called()

    def test_symlink_tool_is_refused_before_source_handoff(self):
        alias = self.root / 'alias'
        alias.symlink_to(self.tool)
        with patch.object(boot, 'verify_candidate') as verify:
            with self.assertRaises(RuntimeError):
                boot.verify_runner_inputs(self.root, self.inputs, 'd' * 40, (alias,))
        verify.assert_not_called()

    def test_group_writable_input_directory_is_refused(self):
        self.inputs.chmod(0o775)
        with patch.object(boot, 'verify_candidate') as verify:
            with self.assertRaisesRegex(RuntimeError, 'permits other-user writes'):
                boot.verify_runner_inputs(self.root, self.inputs, 'd' * 40, (self.tool,))
        verify.assert_not_called()


if __name__ == '__main__':
    unittest.main()
