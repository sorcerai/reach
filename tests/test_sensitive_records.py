from __future__ import annotations

import json
import os
import stat
import tempfile
import unittest
from pathlib import Path
from unittest.mock import MagicMock

from scripts.reach_sensitive import (
    redact_action,
    redact_result,
    redact_step,
    safe_url,
    write_json_private,
)
from scripts.reach_routine import (
    ReplayResult,
    RoutineCompiler,
    RoutineReplayer,
    RoutineTrace,
    TraceStep,
    promote_candidate,
    routine_digest,
    write_healing_candidate,
)


class TestSensitivePersistence(unittest.TestCase):
    def test_safe_url_keeps_origin_only(self) -> None:
        self.assertEqual(
            safe_url("https://alice:secret@example.test:8443/path?q=raw#fragment"),
            "https://example.test:8443",
        )

    def test_redaction_drops_typed_dom_errors_and_frames(self) -> None:
        raw = {
            "kind": "type",
            "text": "password=super-secret",
            "value": "super-secret",
            "description": "goal: submit super-secret",
            "error": "DOM said account token",
            "dom_snapshot": "typed super-secret",
            "aria_tag": "DOM canary arbitrary text",
            "aria": "DOM canary arbitrary text",
            "before_frame": "frames/raw.png",
            "after_frame": "frames/raw.png",
            "url": "https://user:pw@example.test/form?q=secret#x",
            "input_name": "password",
        }
        encoded = json.dumps(redact_action(raw), sort_keys=True)
        self.assertNotIn("super-secret", encoded)
        self.assertNotIn("DOM canary arbitrary text", encoded)
        self.assertNotIn("frames/raw.png", encoded)
        self.assertEqual(redact_action(raw)["input_name"], "password")

        step = redact_step({**raw, "step_index": 1, "action_type": "type"})
        self.assertNotIn("super-secret", json.dumps(step))
        self.assertNotIn("dom_snapshot", step)
        self.assertNotIn("before_frame", step)
    def test_trace_and_compiled_artifacts_never_contain_raw_inputs(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            trace = RoutineTrace(
                name="secure",
                screen=0,
                created_at="2026-09-07T00:00:00Z",
                steps=[
                    TraceStep(
                        1,
                        "now",
                        "navigate",
                        url="https://user:pw@example.test/search?q=raw-query#fragment",
                    ),
                    TraceStep(
                        2,
                        "now",
                        "type",
                        text="raw-password-value",
                        input_name="password",
                        dom_snapshot="raw-password-value in observed DOM",
                        before_frame="frames/raw.png",
                        after_frame="frames/raw.png",
                    ),
                ],
            )
            trace_file = root / "trace.json"
            trace_file.write_text(json.dumps(trace.to_dict()))
            compiler = RoutineCompiler()
            compiler.compile(trace, routines_dir=root)
            artifacts = (trace_file.read_text() + (root / "secure" / "routine.json").read_text())
            self.assertNotIn("raw-password-value", artifacts)
            self.assertNotIn("raw-query", artifacts)
            self.assertNotIn("frames/raw.png", artifacts)

    def test_private_write_is_atomic_and_private(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            target = Path(directory) / "records" / "trace.json"
            write_json_private(target, {"status": "ok"})
            self.assertEqual(json.loads(target.read_text()), {"status": "ok"})
            self.assertEqual(stat.S_IMODE(target.stat().st_mode), 0o600)

    def test_result_redaction_drops_raw_parameter_values(self) -> None:
        result = redact_result(
            {
                "success": False,
                "status": "failed",
                "parameters_used": {"password": "super-secret"},
                "error": "failed with super-secret",
                "healed_steps": [{"value": "super-secret", "screenshot_path": "raw.png"}],
            }
        )
        encoded = json.dumps(result)
        self.assertNotIn("super-secret", encoded)
        self.assertNotIn("raw.png", encoded)
        self.assertEqual(result["parameters_used"], ["password"])


class TestCandidatePromotion(unittest.TestCase):
    def _build_click_candidate(self, root: Path, routine_name: str = "demo") -> tuple[Path, Path, str]:
        trace = RoutineTrace(
            name=routine_name,
            screen=0,
            created_at="2026-09-07T00:00:00Z",
            steps=[TraceStep(1, "now", "navigate", url="https://example.test/path")],
        )
        RoutineCompiler().compile(trace, routines_dir=root)
        path = root / routine_name / "routine.json"
        expected = routine_digest(path)
        candidate = root / "candidate.json"
        write_healing_candidate(
            candidate,
            routine_name=routine_name,
            original_digest=expected,
            actions=[{"step_index": 1, "kind": "click", "point": [2, 3]}],
        )
        return path, candidate, expected

    def test_stale_candidate_fails_compare_and_swap(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            path, candidate, expected = self._build_click_candidate(root)
            current = json.loads(path.read_text())
            current["steps"][0]["action"]["point"] = [99, 99]
            path.write_text(json.dumps(current))
            with self.assertRaises(ValueError):
                promote_candidate(
                    path,
                    candidate,
                    expected,
                    revalidate=lambda _: ReplayResult(True, "completed", 1, {}),
                )
    def test_candidate_binding_must_match_routine_and_digest(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            path, candidate, expected = self._build_click_candidate(root)
            payload = json.loads(candidate.read_text())
            payload["routine_name"] = "other"
            candidate.write_text(json.dumps(payload))
            with self.assertRaises(ValueError):
                promote_candidate(
                    path,
                    candidate,
                    expected,
                    revalidate=lambda _: ReplayResult(True, "completed", 1, {}),
                )

            payload["routine_name"] = "demo"
            payload["original_digest"] = "0" * 64
            candidate.write_text(json.dumps(payload))
            with self.assertRaises(ValueError):
                promote_candidate(
                    path,
                    candidate,
                    expected,
                    revalidate=lambda _: ReplayResult(True, "completed", 1, {}),
                )


    def test_structural_only_revalidation_is_rejected(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            path, candidate, expected = self._build_click_candidate(root)
            with self.assertRaises(TypeError):
                promote_candidate(path, candidate, expected, revalidate=lambda _: True)

    def test_candidate_promotion_replays_and_preserves_original_checkpoints(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            path, candidate, expected = self._build_click_candidate(root)
            driver = MagicMock()
            driver.api_url = "http://127.0.0.1:4200"
            driver.screen = 0
            driver.sandbox = None
            driver.timeout_sec = 30
            driver.model = "test"
            driver.agy_bin = "agy"
            driver.call_mcp_tool.return_value = {
                "isError": False,
                "content": [{"type": "text", "text": json.dumps({
                    "status": "ok", "url": "https://example.test/path",
                })}],
            }
            replayer = RoutineReplayer(
                routine_name="demo", routines_dir=root, driver=driver, heal_with_cua=False
            )
            promoted = promote_candidate(
                path,
                candidate,
                expected,
                revalidate=lambda proposed: replayer.revalidate_candidate(proposed, params={}),
            )
            self.assertEqual(promoted["steps"][0]["checkpoints"][0]["type"], "url_origin_equals")
            self.assertEqual(promoted["steps"][0]["action"]["point"], [2, 3])
            driver.execute_action.assert_called_once()


    def test_multi_action_candidate_replaces_step_and_reindexes(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            trace = RoutineTrace(
                name="multi",
                screen=0,
                created_at="2026-09-07T00:00:00Z",
                steps=[
                    TraceStep(1, "now", "navigate", url="https://example.test/"),
                    TraceStep(
                        2,
                        "now",
                        "click",
                        x=10,
                        y=10,
                        metadata={"dom_keywords": ["success"]},
                    ),
                    TraceStep(3, "now", "type", input_name="query"),
                ],
            )
            RoutineCompiler().compile(trace, routines_dir=root)
            path = root / "multi" / "routine.json"
            original_checkpoints = json.loads(path.read_text())["steps"][1]["checkpoints"]
            expected = routine_digest(path)
            candidate = root / "candidate.json"
            write_healing_candidate(
                candidate,
                routine_name="multi",
                original_digest=expected,
                actions=[
                    {"step_index": 2, "kind": "click", "point": [20, 30]},
                    {"step_index": 2, "kind": "type", "input_name": "query"},
                ],
            )
            driver = MagicMock()
            driver.api_url = "http://127.0.0.1:4200"
            driver.screen = 0
            driver.sandbox = None
            driver.timeout_sec = 30
            driver.model = "test"
            driver.agy_bin = "agy"
            driver.call_mcp_tool.return_value = {
                "isError": False,
                "content": [{"type": "text", "text": json.dumps({
                    "status": "ok", "url": "https://example.test/",
                })}],
            }
            driver.capture_page_text.return_value = "success"
            replayer = RoutineReplayer(
                routine_name="multi", routines_dir=root, driver=driver, heal_with_cua=False
            )

            promoted = promote_candidate(
                path,
                candidate,
                expected,
                revalidate=lambda proposed: replayer.revalidate_candidate(
                    proposed, params={"query": "Tesla"}
                ),
            )

            self.assertEqual(
                [step["step_index"] for step in promoted["steps"]], [1, 2, 3, 4]
            )
            self.assertEqual(
                [step["action"]["kind"] for step in promoted["steps"]],
                ["navigate", "click", "type", "type"],
            )
            self.assertEqual(promoted["steps"][1]["checkpoints"], [])
            self.assertEqual(promoted["steps"][2]["checkpoints"], original_checkpoints)
            self.assertEqual(driver.execute_action.call_count, 4)

    def test_candidate_with_type_action_missing_input_name_is_rejected(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            trace = RoutineTrace(
                name="typed",
                screen=0,
                created_at="2026-09-07T00:00:00Z",
                steps=[TraceStep(1, "now", "type", input_name="password")],
            )
            RoutineCompiler().compile(trace, routines_dir=root)
            path = root / "typed" / "routine.json"
            expected = routine_digest(path)
            candidate = root / "candidate.json"
            write_healing_candidate(
                candidate,
                routine_name="typed",
                original_digest=expected,
                actions=[{"step_index": 1, "kind": "type"}],
            )
            with self.assertRaises(ValueError):
                promote_candidate(
                    path,
                    candidate,
                    expected,
                    revalidate=lambda proposed: ReplayResult(True, "completed", 1, {}),
                )

    def test_failed_replay_does_not_promote(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            path, candidate, expected = self._build_click_candidate(root)
            before = path.read_bytes()
            driver = MagicMock()
            driver.api_url = "http://127.0.0.1:4200"
            driver.screen = 0
            driver.sandbox = None
            driver.timeout_sec = 30
            driver.model = "test"
            driver.agy_bin = "agy"
            driver.call_mcp_tool.return_value = {
                "isError": False,
                "content": [{"type": "text", "text": json.dumps({
                    "status": "ok", "url": "https://wrong.example/path",
                })}],
            }
            replayer = RoutineReplayer(
                routine_name="demo", routines_dir=root, driver=driver, heal_with_cua=False
            )
            with self.assertRaises(ValueError):
                promote_candidate(
                    path,
                    candidate,
                    expected,
                    revalidate=lambda proposed: replayer.revalidate_candidate(proposed, params={}),
                )
            self.assertEqual(path.read_bytes(), before)

if __name__ == "__main__":
    unittest.main()
