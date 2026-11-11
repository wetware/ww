#!/usr/bin/env python3
"""Development fixture ownership controls; never create users or services."""
import os
from pathlib import Path
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
