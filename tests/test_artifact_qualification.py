#!/usr/bin/env python3
"""Check actual job predicates for artifact-only and existing release events."""
from pathlib import Path
import re
import unittest

ROOT = Path(__file__).resolve().parents[1]


def job(text, name):
    return re.search(r'^  ' + re.escape(name) + r':\n(.*?)(?=^  [\w-]+:|\Z)', text, re.M | re.S)[1]


def enabled(text, name, event, ref, artifact_only, success=True, changed=True):
    block = job(text, name)
    # Fold only the job-level expression, never a step or a shell body.
    lines = block.splitlines()
    index = next(i for i, line in enumerate(lines) if line.startswith('    if:'))
    expression = lines[index].removeprefix('    if: ').strip()
    if expression == '>-':
        folded = []
        for line in lines[index + 1:]:
            if not line.startswith('      '):
                break
            folded.append(line.strip())
        expression = ' '.join(folded)
    expression = expression.removeprefix('${{').removesuffix('}}').strip()
    values = {'github.event_name': event, 'github.ref': ref, 'inputs.artifact_only': artifact_only}
    for token in re.findall(r'needs\.[\w-]+\.(?:result|outputs\.[\w_]+)', expression):
        values[token] = ('success' if success else 'failure') if token.endswith('.result') else ('true' if changed else 'false')
    for key in sorted(values, key=len, reverse=True):
        expression = expression.replace(key, repr(values[key]))
    expression = expression.replace('!cancelled()', 'True').replace('&&', ' and ').replace('||', ' or ')
    expression = re.sub(r'!(?!=)', ' not ', expression)
    expression = re.sub(r'\bfalse\b', 'False', expression)
    return bool(eval(expression.strip(), {'__builtins__': {}, 'startsWith': lambda value, prefix: value.startswith(prefix)}, {}))


class QualificationWorkflow(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.text = (ROOT / '.github/workflows/rust.yml').read_text()

    def test_opt_in_is_boolean_and_default_false(self):
        self.assertRegex(self.text, r'artifact_only:\n(?:        .*\n)*?        type: boolean\n        default: false')

    def test_qualification_can_validate_without_publication(self):
        for ref in ('refs/heads/qualification-test', 'refs/heads/master', 'refs/tags/test'):
            with self.subTest(ref=ref):
                expected = ref == 'refs/heads/qualification-test'
                self.assertEqual(enabled(self.text, 'validate-release', 'workflow_dispatch', ref, True), expected)
                self.assertEqual(enabled(self.text, 'qualification', 'workflow_dispatch', ref, True), expected)
                for name in ('build-architecture-map', 'deploy-architecture-map', 'release_image', 'publish'):
                    self.assertFalse(enabled(self.text, name, 'workflow_dispatch', ref, True), name)

    def test_existing_master_and_branch_behavior(self):
        for event in ('push', 'workflow_dispatch', 'pull_request'):
            for ref in ('refs/heads/master', 'refs/heads/topic'):
                with self.subTest(event=event, ref=ref):
                    self.assertFalse(enabled(self.text, 'qualification', event, ref, False))
                    if event in ('push', 'workflow_dispatch'):
                        self.assertEqual(enabled(self.text, 'validate-release', event, ref, False), ref.endswith('/master'))
                        self.assertEqual(enabled(self.text, 'release_image', event, ref, False), ref.endswith('/master'))
                    self.assertEqual(enabled(self.text, 'build-architecture-map', event, ref, False),
                                     event == 'workflow_dispatch' or (event == 'push' and ref.endswith('/master')))
                    self.assertFalse(enabled(self.text, 'publish', event, ref, False))

    def test_failed_validation_cannot_get_marker_or_publish(self):
        self.assertFalse(enabled(self.text, 'qualification', 'workflow_dispatch', 'refs/heads/topic', True, success=False))
        self.assertFalse(enabled(self.text, 'release_image', 'workflow_dispatch', 'refs/heads/master', False, success=False))
        self.assertFalse(enabled(self.text, 'deploy-architecture-map', 'workflow_dispatch', 'refs/heads/master', False, success=False))

    def test_qualification_is_separate_raw_artifact_and_uses_offline_kubo(self):
        self.assertIn("RELEASE_INPUT_CAR: ${{ inputs.artifact_only && 'qualification-release-input.car' || 'release-input.car' }}", self.text)
        self.assertIn('--car "$RELEASE_INPUT_CAR"', job(self.text, 'validate-release'))
        self.assertIn('path: ${{ env.RELEASE_INPUT_CAR }}', job(self.text, 'validate-release'))
        self.assertIn("IPFS_PEER_ADDR: ${{ !inputs.artifact_only && secrets.IPFS_PEER_ADDR || '' }}", self.text)
        self.assertIn('ipfs daemon --offline --enable-gc &', job(self.text, 'test-host'))
        self.assertIn('if [ "$ARTIFACT_ONLY" = true ]; then', job(self.text, 'test-host'))
        self.assertIn('ARTIFACT_ONLY: ${{ inputs.artifact_only }}', job(self.text, 'test-host'))


if __name__ == '__main__':
    unittest.main()
