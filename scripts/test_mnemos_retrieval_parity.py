"""Test the source-slice parity verdict without compiling or accessing any runtime store.

Run: python -I scripts/test_mnemos_retrieval_parity.py
"""
from __future__ import annotations

from copy import deepcopy
import importlib.util
from pathlib import Path
import sys
import unittest

SPEC = importlib.util.spec_from_file_location(
    "mnemos_retrieval_parity", Path(__file__).with_name("mnemos_retrieval_parity.py")
)
parity = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(parity)


def snapshot():
    row = {"repeat": 0, "split": "edge", "query_id": "literal", "mode": "exact",
           "ordered_ids": ["a", "b"], "scores": [2.0, 1.0], "recall_admitted": True}
    repeated = dict(deepcopy(row), repeat=1)
    return {"query_count": 1, "repeats": 2, "rows": [row, repeated],
            "decay": [{"strength": 0.65, "archive": False}]}


class ParityVerdictTests(unittest.TestCase):
    def setUp(self):
        self.baseline = snapshot()
        self.candidate = deepcopy(self.baseline)

    def compare(self):
        return parity.comparisons(self.baseline, self.candidate)

    def test_equal_nonempty_observations_pass(self):
        self.assertTrue(self.compare()["passed"])

    def test_small_numeric_roundoff_is_tolerated(self):
        self.candidate["rows"][0]["scores"][0] += 1e-13
        self.assertTrue(self.compare()["passed"])

    def test_order_is_strict_even_when_score_delta_is_tiny(self):
        self.candidate["rows"][0]["ordered_ids"].reverse()
        result = self.compare()
        self.assertFalse(result["passed"])
        self.assertEqual(len(result["ordered_id_failures"]), 1)

    def test_large_score_difference_fails(self):
        self.candidate["rows"][0]["scores"][0] += 1e-5
        result = self.compare()
        self.assertFalse(result["passed"])
        self.assertEqual(len(result["value_failures"]), 1)

    def test_gate_difference_fails(self):
        self.candidate["rows"][0]["recall_admitted"] = False
        self.assertFalse(self.compare()["passed"])

    def test_common_repeat_instability_is_not_hidden_by_pairwise_equality(self):
        for value in (self.baseline, self.candidate):
            value["rows"][1]["ordered_ids"].reverse()
        result = self.compare()
        self.assertFalse(result["passed"])
        self.assertEqual(result["ordered_id_failures"], [])
        self.assertTrue(all(result["within_implementation_instability"].values()))

    def test_extra_row_fails_instead_of_truncating_zip_to_success(self):
        self.candidate["rows"].append(deepcopy(self.candidate["rows"][0]))
        self.assertFalse(self.compare()["passed"])

    def test_extra_metadata_fails(self):
        self.candidate["unexpected"] = True
        self.assertFalse(self.compare()["passed"])

    def test_empty_evidence_fails(self):
        for key in ("rows", "decay"):
            with self.subTest(key=key):
                self.baseline = snapshot()
                self.candidate = deepcopy(self.baseline)
                self.baseline[key] = []
                self.candidate[key] = []
                self.assertFalse(self.compare()["passed"])

    def test_decay_difference_fails(self):
        self.candidate["decay"][0]["strength"] = 0.7
        result = self.compare()
        self.assertFalse(result["passed"])
        self.assertFalse(result["decay_match"])

    def test_nonfinite_values_fail_even_when_both_sides_match(self):
        for value in (float("inf"), float("nan")):
            with self.subTest(value=value):
                self.assertFalse(parity.close(value, value))

    def test_booleans_are_not_numeric_scores(self):
        self.assertFalse(parity.close(True, 1))
        self.assertFalse(parity.close(False, 0))
        self.assertFalse(parity.close(1, True))

    def test_source_markers_must_be_unique_and_ordered(self):
        self.assertEqual(parity.span("prefix START body END suffix", "START", "END"), "START body ")
        for source in ("START START END", "END START", "START only"):
            with self.subTest(source=source):
                with self.assertRaises(ValueError):
                    parity.span(source, "START", "END")


if __name__ == "__main__":
    if sys.flags.isolated != 1:
        raise SystemExit("Run with Python -I")
    unittest.main()
