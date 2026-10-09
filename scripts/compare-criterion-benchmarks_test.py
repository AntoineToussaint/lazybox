#!/usr/bin/env python3
"""Regression tests for compare-criterion-benchmarks.py."""

from __future__ import annotations

import importlib.util
import json
import tempfile
import unittest
from pathlib import Path


SCRIPT = Path(__file__).with_name("compare-criterion-benchmarks.py")
SPEC = importlib.util.spec_from_file_location("compare_criterion_benchmarks", SCRIPT)
assert SPEC is not None and SPEC.loader is not None
MODULE = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(MODULE)


class CompareCriterionBenchmarksTest(unittest.TestCase):
    def test_same_runner_regression_fails(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            base = root / "base.json"
            candidate = root / "candidate.json"
            base.write_text(
                json.dumps([{"name": "tick", "value": 1.0, "unit": "ms"}]),
                encoding="utf-8",
            )
            candidate.write_text(
                json.dumps([{"name": "tick", "value": 2.01, "unit": "ms"}]),
                encoding="utf-8",
            )

            self.assertEqual(MODULE.main([str(base), str(candidate)]), 1)

    def test_matching_pair_passes(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            base = root / "base.json"
            candidate = root / "candidate.json"
            payload = [{"name": "tick", "value": 1.0, "unit": "ms"}]
            base.write_text(json.dumps(payload), encoding="utf-8")
            candidate.write_text(json.dumps(payload), encoding="utf-8")

            self.assertEqual(MODULE.main([str(base), str(candidate)]), 0)

    def test_changed_benchmark_set_is_invalid(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            base = root / "base.json"
            candidate = root / "candidate.json"
            base.write_text(
                json.dumps([{"name": "before", "value": 1.0, "unit": "ms"}]),
                encoding="utf-8",
            )
            candidate.write_text(
                json.dumps([{"name": "after", "value": 1.0, "unit": "ms"}]),
                encoding="utf-8",
            )

            self.assertEqual(MODULE.main([str(base), str(candidate)]), 2)


if __name__ == "__main__":
    unittest.main()
