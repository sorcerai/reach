"""Focused tests for the local/disposable AXI observation benchmark."""

import json
from pathlib import Path
import sys
import unittest

REPO_ROOT = Path(__file__).parent.parent.resolve()
if str(REPO_ROOT) not in sys.path:
    sys.path.insert(0, str(REPO_ROOT))

from scripts.benchmark_axi import (  # noqa: E402
    DEFAULT_TARGET,
    ObservationCase,
    load_cases,
    measure_observation,
    run_benchmark,
)


class AxiBenchmarkTests(unittest.TestCase):
    def test_measurements_cover_modes_bytes_latency_and_ref_fidelity(self) -> None:
        case = ObservationCase(
            name="local",
            observations={
                "full": {"refs": ["@e1", "@e2"], "text": "Full"},
                "compact": {"refs": ["@e1"], "text": "Compact"},
                "query": {"refs": ["@e1"], "text": "Query"},
            },
            expected_refs=("@e1", "@e2"),
        )
        result = run_benchmark([case])
        self.assertEqual(result["target"], DEFAULT_TARGET)
        self.assertEqual(len(result["measurements"]), 3)
        by_mode = {row["mode"]: row for row in result["measurements"]}
        self.assertEqual(by_mode["full"]["utf8_bytes"], len('{"refs":["@e1","@e2"],"text":"Full"}'.encode()))
        self.assertGreaterEqual(by_mode["full"]["latency_seconds"], 0)
        self.assertEqual(by_mode["full"]["ref_fidelity"], 1.0)
        self.assertEqual(by_mode["compact"]["retained_expected_refs"], ["@e1"])
        self.assertEqual(by_mode["compact"]["missing_expected_refs"], ["@e2"])
        self.assertIn("estimate", by_mode["full"]["token_estimate"]["label"])

    def test_absent_mode_is_reported_without_fabricating_data(self) -> None:
        case = ObservationCase(name="partial", observations={"full": "text"}, expected_refs=("@e1",))
        row = measure_observation(case, "query")
        self.assertFalse(row["available"])
        self.assertIsNone(row["utf8_bytes"])
        self.assertIsNone(row["ref_fidelity"])
        self.assertIsNone(row["latency_seconds"])
        self.assertIsNone(row["token_estimate"])
        self.assertEqual(row["missing_expected_refs"], ["@e1"])

    def test_live_site_target_is_rejected(self) -> None:
        case = ObservationCase(name="local", observations={"full": "text"})
        with self.assertRaises(ValueError):
            run_benchmark([case], target="https://example.com")

    def test_cases_load_from_local_json(self) -> None:
        import tempfile

        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "cases.json"
            path.write_text(
                json.dumps(
                    {
                        "cases": [
                            {
                                "name": "fixture",
                                "full": {"refs": ["@e1"]},
                                "expected_refs": ["@e1"],
                            }
                        ]
                    }
                ),
                encoding="utf-8",
            )
            cases = load_cases(str(path))
        self.assertEqual(cases[0].name, "fixture")
        self.assertEqual(cases[0].expected_refs, ("@e1",))


if __name__ == "__main__":
    unittest.main()
