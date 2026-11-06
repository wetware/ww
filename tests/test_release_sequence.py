"""A workflow rerun gets a fresh release identity without overtaking a later run."""
import importlib.util
from pathlib import Path
import unittest

spec = importlib.util.spec_from_file_location("release_sequence", Path(__file__).resolve().parents[1] / "scripts/release_sequence.py")
module = importlib.util.module_from_spec(spec)
spec.loader.exec_module(module)


class ReleaseSequence(unittest.TestCase):
    def test_attempts_and_runs_remain_ordered(self):
        sequence = module.release_sequence
        self.assertLess(sequence(5, 1), sequence(5, 2))
        self.assertLess(sequence(5, 65535), sequence(6, 1))
        self.assertEqual(sequence(1, 1), 65537)

    def test_counter_exhaustion_fails_closed(self):
        for run, attempt in ((0, 1), (-1, 1), (1, 0), (1, 65536), (1 << 47, 1)):
            with self.subTest(run=run, attempt=attempt), self.assertRaises(ValueError):
                module.release_sequence(run, attempt)
        self.assertEqual(module.release_sequence((1 << 47) - 1, 65535), (1 << 63) - 1)


if __name__ == "__main__":
    unittest.main()
