#!/usr/bin/env python3
"""PR 2 exposes offline machinery but cannot run live native qualification."""
import json
from pathlib import Path
import subprocess
import sys
import unittest

ROOT = Path(__file__).resolve().parents[1]


class Boundary(unittest.TestCase):
    def test_live_entrypoints_are_unavailable(self):
        for name in ('scripts/qualify_native_upgrade.py',
                     'scripts/run_native_release_integration.py',
                     'scripts/run_native_development_boot.py',
                     'tests/native_release_integration.py'):
            with self.subTest(name=name):
                result = subprocess.run([sys.executable, str(ROOT / name)],
                                        capture_output=True, text=True, timeout=10)
                self.assertEqual(result.returncode, 3, result.stderr)
                report = json.loads(result.stdout)
                self.assertEqual(report['status'], 'UNAVAILABLE')
                for field in ('native_acceptance', 'production_activation',
                              'publication_allowed', 'integration_checks_passed'):
                    self.assertIs(report[field], False)

    def test_workflow_has_no_live_lane_or_branch_exception(self):
        workflow = (ROOT / '.github/workflows/rust.yml').read_text()
        self.assertNotIn('lthibault/pasted-text-v38', workflow)
        for name in ('run_native_release_integration.py', 'run_native_development_boot.py',
                     'prepare_current_native_release.py --', 'qualify_native_upgrade.py --'):
            self.assertNotIn(name, workflow)
        self.assertIn('name: Verify retained historical release inputs', workflow)


if __name__ == '__main__':
    unittest.main()
