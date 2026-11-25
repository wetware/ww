"""Regression cases for missing test coverage in the Fast CI selectors."""

from pathlib import Path
import sys
import unittest
from unittest.mock import patch

sys.dont_write_bytecode = True
sys.path.insert(0, str(Path(__file__).resolve().parents[1] / "scripts"))
import check_fast_selection as selection


class FastSelectionTests(unittest.TestCase):
    def setUp(self):
        self.selector = "p3::tests::p3_"
        self.names = selection.MANUAL_CELL_CASES | {
            prefix + case for prefix, cases in selection.P3_CASES.items() for case in cases
        }

    def test_new_matching_case_runs(self):
        selection.check_p3(self.names | {self.selector + "new_regression"}, self.selector)

    def test_missing_case(self):
        self.names.remove(self.selector + "transport_ordering_final_write_and_half_close")
        with self.assertRaisesRegex(ValueError, "missing P3 tests"):
            selection.check_p3(self.names, self.selector)

    def test_empty_discovery(self):
        with self.assertRaisesRegex(ValueError, "empty libtest selector"):
            selection.check_p3(set(), self.selector)

    def test_empty_or_stale_filter(self):
        for selector in ("", "p3::typo"):
            with self.subTest(selector=selector), self.assertRaisesRegex(ValueError, "unexpected libtest selector"):
                selection.check_p3(self.names, selector)

    def test_new_unselected_case(self):
        with self.assertRaisesRegex(ValueError, "unclassified.*new_guest_test"):
            selection.check_p3(self.names | {"other::new_guest_test"}, self.selector)

    def test_incomplete_guest_composition(self):
        child = "tests/fixtures/guest-session-cancel/child/Cargo.toml"
        parent = "tests/fixtures/guest-session-cancel/parent/Cargo.toml"
        self.assertEqual(selection.fixture_selector([child, parent]), selection.SESSION)
        with self.assertRaisesRegex(ValueError, "no Fast P3 selector"):
            selection.fixture_selector([child])

    def test_fast_fixture_missing_from_build(self):
        with patch.dict(selection.FIXTURE_TIERS, {"new-fast-probe": selection.NATIVE}):
            with self.assertRaisesRegex(ValueError, "no Fast P3 selector"):
                selection.fixture_selector(["tests/fixtures/native-p3/Cargo.toml"])

    def test_new_fixture(self):
        paths = [f"tests/fixtures/{name}/Cargo.toml" for name in selection.FIXTURE_TIERS]
        with self.assertRaisesRegex(ValueError, "unclassified.*new_guest"):
            selection.check_fixtures(paths + ["tests/fixtures/new_guest/Cargo.toml"])

    def test_new_package_or_deferred_target(self):
        # Minimal metadata for the explicitly deferred packages.
        packages = [
            {"name": name, "id": name, "targets": [
                {"kind": kinds.split(","), "name": target, "test": True}
                for kinds, target in (item.split(":") for item in targets)
            ]}
            for name, targets in selection.DEFERRED_TARGETS.items()
        ]
        metadata = {"packages": packages, "workspace_members": [p["id"] for p in packages]}
        selection.check_targets(metadata, set())
        packages.append({"name": "new", "id": "new", "targets": []})
        metadata["workspace_members"].append("new")
        with self.assertRaisesRegex(ValueError, "unclassified.*new"):
            selection.check_targets(metadata, set())
        packages[0]["targets"].append({"kind": ["test"], "name": "new", "test": True})
        with self.assertRaisesRegex(ValueError, "unclassified.*test:new"):
            selection.check_targets(metadata, {"new"})


if __name__ == "__main__":
    unittest.main()
