"""Unit and integration tests for Reach Routine Engine (track reach-1sc).

Tests:
1. Demonstration Recorder: trace capturing, action types, coordinates, text, selectors,
   frames saving, and trace.json generation.
2. Routine Compiler: coordinate normalization, input parameterization, checkpoint injection
   (URL, DOM text, visual pHash anchor).
3. Self-Healing Replayer: deterministic execution, parameter overrides, checkpoint
   validation, failure detection, CUA vision driving loop fallback, and routine healing.
"""

from __future__ import annotations

import json
import os
import shutil
import sys
import tempfile
import unittest
from pathlib import Path
from unittest.mock import MagicMock, patch

REPO_ROOT = Path(__file__).parent.parent.resolve()
if str(REPO_ROOT) not in sys.path:
    sys.path.insert(0, str(REPO_ROOT))

from scripts.compiler import RoutineCompiler
from scripts.reach_drive import DriveResult, ReachAction, ReachDriver, StepRecord
from scripts.reach_routine import (
    Checkpoint,
    CompiledAction,
    CompiledRoutine,
    CompiledStep,
    ReplayResult,
    RoutineRecorder,
    RoutineReplayer,
    RoutineTrace,
    TraceStep,
    compute_frame_hash_hex,
    hash_distance,
    render_template,
)


class TestRoutineRecorder(unittest.TestCase):
    """Tests for RoutineRecorder engine."""

    def setUp(self) -> None:
        self.temp_dir = tempfile.mkdtemp(prefix="reach-test-recorder-")
        self.mock_driver = MagicMock()
        self.dummy_png = os.path.join(self.temp_dir, "dummy.png")
        # Write minimal valid 1x1 PNG bytes
        with open(self.dummy_png, "wb") as f:
            f.write(
                b"\x89PNG\r\n\x1a\n\x00\x00\x00\rIHDR\x00\x00\x00\x01\x00\x00\x00\x01\x08\x06\x00\x00\x00\x1f\x15c4\x00\x00\x00\rIDATx\x9cc\xf8\xff\xff?\x03\x00\x08\xfc\x02\xfe\xa7\x9a\xa0\xa0\x00\x00\x00\x00IEND\xaeB`\x82"
            )
        self.mock_driver.capture_screenshot.return_value = self.dummy_png
        self.mock_driver.capture_page_text.return_value = "Search Results Dashboard"

    def tearDown(self) -> None:
        shutil.rmtree(self.temp_dir, ignore_errors=True)

    def test_record_actions_and_generate_trace_json(self) -> None:
        recorder = RoutineRecorder(
            routine_name="test_record_flow",
            screen=0,
            routines_dir=self.temp_dir,
            driver=self.mock_driver,
        )

        # Step 1: navigate
        step1 = recorder.record_step(
            action_type="navigate",
            url="https://www.google.com",
            execute=True,
        )
        self.assertEqual(step1.step_index, 1)
        self.assertEqual(step1.action_type, "navigate")
        self.assertEqual(step1.url, "https://www.google.com")
        self.assertTrue(step1.before_frame.endswith("step_001_before.png"))
        self.assertTrue(step1.after_frame.endswith("step_001_after.png"))

        # Step 2: click input field with selector & ARIA tag
        step2 = recorder.record_step(
            action_type="click",
            x=640,
            y=360,
            selector="input[name='q']",
            aria_tag="searchbox: Search",
            execute=True,
        )
        self.assertEqual(step2.step_index, 2)
        self.assertEqual(step2.action_type, "click")
        self.assertEqual(step2.x, 640)
        self.assertEqual(step2.y, 360)
        self.assertEqual(step2.selector, "input[name='q']")
        self.assertEqual(step2.aria_tag, "searchbox: Search")

        # Step 3: type text
        step3 = recorder.record_step(
            action_type="type",
            text="Tesla",
            selector="input[name='q']",
            execute=True,
        )
        self.assertEqual(step3.step_index, 3)
        self.assertEqual(step3.action_type, "type")
        self.assertEqual(step3.text, "Tesla")

        # Step 4: press Return key
        step4 = recorder.record_step(
            action_type="key",
            key="Return",
            execute=True,
        )
        self.assertEqual(step4.step_index, 4)
        self.assertEqual(step4.key, "Return")

        # Verify trace.json exists and is structured
        trace_file = Path(self.temp_dir) / "test_record_flow" / "trace.json"
        self.assertTrue(trace_file.is_file())

        with open(trace_file, "r", encoding="utf-8") as f:
            data = json.load(f)

        self.assertEqual(data["name"], "test_record_flow")
        self.assertEqual(data["screen"], 0)
        self.assertEqual(len(data["steps"]), 4)

        # Verify frame files saved under frames/
        frames_dir = Path(self.temp_dir) / "test_record_flow" / "frames"
        self.assertTrue((frames_dir / "step_001_before.png").is_file())
        self.assertTrue((frames_dir / "step_001_after.png").is_file())
        self.assertTrue((frames_dir / "step_004_after.png").is_file())

    def test_record_actions_with_semantic_reference(self) -> None:
        recorder = RoutineRecorder(
            routine_name="test_ref_record",
            screen=0,
            routines_dir=self.temp_dir,
            driver=self.mock_driver,
        )
        step = recorder.record_step(
            action_type="click",
            x=500,
            y=250,
            selector="button#login",
            aria_tag="Login button",
            reference="@e14",
            execute=True,
        )
        self.assertEqual(step.reference, "@e14")
        self.mock_driver.execute_action.assert_called()
        call_action = self.mock_driver.execute_action.call_args[0][0]
        self.assertEqual(call_action.ref, "@e14")

        trace_file = Path(self.temp_dir) / "test_ref_record" / "trace.json"
        with open(trace_file, "r", encoding="utf-8") as f:
            data = json.load(f)
        self.assertEqual(data["steps"][0]["ref"], "@e14")

    def test_cdp_event_tap_event_processing(self) -> None:
        from scripts.reach_routine import CDPEventTap

        recorder = RoutineRecorder(
            routine_name="test_tap_flow",
            screen=0,
            routines_dir=self.temp_dir,
            driver=self.mock_driver,
        )
        tap = CDPEventTap(recorder=recorder, cdp_port=9222)

        # Process click event with semantic ref
        tap._process_event({
            "type": "click",
            "x": 320,
            "y": 240,
            "selector": "button#submit",
            "aria": "Submit form",
            "ref": "@e7",
            "url": "https://example.com/checkout",
        })
        self.assertEqual(len(recorder.steps), 1)
        self.assertEqual(recorder.steps[0].action_type, "click")
        self.assertEqual(recorder.steps[0].reference, "@e7")
        self.assertEqual(recorder.steps[0].selector, "button#submit")
        self.assertEqual(recorder.steps[0].x, 320)
        self.assertEqual(recorder.steps[0].y, 240)
        self.assertEqual(recorder.steps[0].metadata.get("source"), "cdp_event_tap")

        # Process type event
        tap._process_event({
            "type": "type",
            "text": "john_doe@example.com",
            "selector": "input#email",
            "ref": "@e2",
            "url": "https://example.com/checkout",
        })
        self.assertEqual(len(recorder.steps), 2)
        self.assertEqual(recorder.steps[1].action_type, "type")
        self.assertEqual(recorder.steps[1].text, "john_doe@example.com")
        self.assertEqual(recorder.steps[1].reference, "@e2")

        # Process key event
        tap._process_event({
            "type": "key",
            "key": "Enter",
            "url": "https://example.com/checkout",
        })
        self.assertEqual(len(recorder.steps), 3)
        self.assertEqual(recorder.steps[2].action_type, "key")
        self.assertEqual(recorder.steps[2].key, "Enter")

        # Process navigate event
        tap._process_event({
            "type": "navigate",
            "url": "https://example.com/dashboard",
        })
        self.assertEqual(len(recorder.steps), 4)
        self.assertEqual(recorder.steps[3].action_type, "navigate")
        self.assertEqual(recorder.steps[3].url, "https://example.com/dashboard")


class TestRoutineCompiler(unittest.TestCase):
    """Tests for RoutineCompiler (scripts/compiler.py)."""

    def setUp(self) -> None:
        self.temp_dir = tempfile.mkdtemp(prefix="reach-test-compiler-")
        self.routine_dir = Path(self.temp_dir) / "search_routine"
        self.frames_dir = self.routine_dir / "frames"
        self.frames_dir.mkdir(parents=True, exist_ok=True)

        # Create dummy frame for visual hash
        self.frame1 = self.frames_dir / "step_001_after.png"
        with open(self.frame1, "wb") as f:
            f.write(
                b"\x89PNG\r\n\x1a\n\x00\x00\x00\rIHDR\x00\x00\x00\x01\x00\x00\x00\x01\x08\x06\x00\x00\x00\x1f\x15c4\x00\x00\x00\rIDATx\x9cc\xf8\xff\xff?\x03\x00\x08\xfc\x02\xfe\xa7\x9a\xa0\xa0\x00\x00\x00\x00IEND\xaeB`\x82"
            )

    def tearDown(self) -> None:
        shutil.rmtree(self.temp_dir, ignore_errors=True)

    def test_coordinate_normalization_and_parameterization(self) -> None:
        trace = RoutineTrace(
            name="search_routine",
            screen=0,
            created_at="2026-09-05T00:00:00Z",
            steps=[
                TraceStep(
                    step_index=1,
                    timestamp="2026-09-05T00:00:01Z",
                    action_type="navigate",
                    url="https://duckduckgo.com",
                    after_frame="frames/step_001_after.png",
                ),
                TraceStep(
                    step_index=2,
                    timestamp="2026-09-05T00:00:03Z",
                    action_type="click",
                    x=640,
                    y=360,
                    selector="input#searchbox_input",
                    aria_tag="Search with DuckDuckGo",
                ),
                TraceStep(
                    step_index=3,
                    timestamp="2026-09-05T00:00:05Z",
                    action_type="type",
                    text="Tesla Motors",
                    selector="input#searchbox_input",
                    aria_tag="Search query",
                    dom_snapshot="Search Results Dashboard",
                ),
            ],
        )

        compiler = RoutineCompiler(screen_width=1280, screen_height=720)
        compiled = compiler.compile(trace, routines_dir=self.temp_dir)

        # 1. Parameterization: "Tesla Motors" should be extracted into "query"
        self.assertIn("query", compiled.parameters)
        self.assertEqual(compiled.parameters["query"], "Tesla Motors")

        # 2. Normalized coordinates for step 2
        click_step = compiled.steps[1]
        self.assertEqual(click_step.action.point, (640, 360))
        self.assertEqual(click_step.action.normalized_point, (0.5, 0.5))
        self.assertEqual(click_step.action.selector, "input#searchbox_input")

        # 3. Parameter placeholder injected into step 3 value
        type_step = compiled.steps[2]
        self.assertEqual(type_step.action.value, "{{query}}")

        # 4. Injected Checkpoints
        nav_step = compiled.steps[0]
        # URL checkpoint on navigate
        url_cps = [c for c in nav_step.checkpoints if c.type == "url_contains"]
        self.assertTrue(len(url_cps) >= 1)
        self.assertIn("duckduckgo.com", url_cps[0].value)

        # Visual pHash checkpoint on navigate after_frame
        phash_cps = [c for c in nav_step.checkpoints if c.type == "visual_phash"]
        self.assertTrue(len(phash_cps) >= 1)
        self.assertIsNotNone(phash_cps[0].expected_hash)

        # Text checkpoint on step 3 (from dom_snapshot keyword "dashboard" or "results")
        text_cps = [c for c in type_step.checkpoints if c.type == "text_contains"]
        self.assertTrue(len(text_cps) >= 1)
        self.assertIn(text_cps[0].value, ["dashboard", "results", "success"])

        # routine.json written to disk
        routine_json_path = self.routine_dir / "routine.json"
        self.assertTrue(routine_json_path.is_file())

    def test_compile_trace_with_semantic_reference(self) -> None:
        trace_data = {
            "version": 1,
            "name": "ref_routine",
            "screen": 0,
            "created_at": "2026-09-05T00:00:00Z",
            "steps": [
                {
                    "step_index": 1,
                    "action_type": "click",
                    "x": 640,
                    "y": 360,
                    "ref": "@e3",
                    "selector": "button#buy",
                    "aria_tag": "Buy Now",
                },
                {
                    "step_index": 2,
                    "action_type": "type",
                    "text": "Tesla",
                    "ref": "@e4",
                    "selector": "input#search",
                    "aria_tag": "Search",
                },
            ],
        }
        compiler = RoutineCompiler()
        compiled = compiler.compile(trace_data, routines_dir=self.temp_dir)
        self.assertEqual(compiled.steps[0].action.reference, "@e3")
        self.assertIn("ref '@e3'", compiled.steps[0].action.description)
        self.assertEqual(compiled.steps[1].action.reference, "@e4")
        self.assertEqual(compiled.steps[1].action.to_dict()["ref"], "@e4")


class TestRoutineReplayer(unittest.TestCase):
    """Tests for RoutineReplayer with deterministic replay and CUA self-healing."""

    def setUp(self) -> None:
        self.temp_dir = tempfile.mkdtemp(prefix="reach-test-replayer-")
        self.routine_dir = Path(self.temp_dir) / "demo_routine"
        self.frames_dir = self.routine_dir / "frames"
        self.frames_dir.mkdir(parents=True, exist_ok=True)

        self.frame_file = self.frames_dir / "step_001_after.png"
        with open(self.frame_file, "wb") as f:
            f.write(
                b"\x89PNG\r\n\x1a\n\x00\x00\x00\rIHDR\x00\x00\x00\x01\x00\x00\x00\x01\x08\x06\x00\x00\x00\x1f\x15c4\x00\x00\x00\rIDATx\x9cc\xf8\xff\xff?\x03\x00\x08\xfc\x02\xfe\xa7\x9a\xa0\xa0\x00\x00\x00\x00IEND\xaeB`\x82"
            )
        self.frame_hash = compute_frame_hash_hex(self.frame_file)

        # Setup compiled routine.json
        self.routine_data = {
            "version": 1,
            "name": "demo_routine",
            "screen": 0,
            "compiled_at": "2026-09-05T00:00:00Z",
            "healed_at": None,
            "parameters": {"query": "DefaultCorp"},
            "steps": [
                {
                    "step_index": 1,
                    "action": {
                        "kind": "navigate",
                        "url": "https://example.com/search?q={{query}}",
                        "description": "Navigate to search",
                    },
                    "checkpoints": [
                        {
                            "type": "url_contains",
                            "value": "example.com",
                            "description": "URL check",
                        },
                        {
                            "type": "visual_phash",
                            "expected_hash": self.frame_hash,
                            "threshold": 0.20,
                            "description": "Visual anchor",
                        },
                    ],
                },
                {
                    "step_index": 2,
                    "action": {
                        "kind": "type",
                        "value": "{{query}}",
                        "selector": "input#query",
                        "description": "Type search keyword",
                    },
                    "checkpoints": [
                        {
                            "type": "text_contains",
                            "value": "success",
                            "description": "Text check",
                        }
                    ],
                },
            ],
        }

        with open(self.routine_dir / "routine.json", "w", encoding="utf-8") as f:
            json.dump(self.routine_data, f, indent=2)

        self.mock_driver = MagicMock()
        self.mock_driver.api_url = "http://127.0.0.1:4200"
        self.mock_driver.screen = 0
        self.mock_driver.sandbox = None
        self.mock_driver.timeout_sec = 30
        self.mock_driver.model = "gemini-3.8-flash-high"
        self.mock_driver.agy_bin = "agy"
        self.mock_driver.capture_screenshot.return_value = str(self.frame_file)

    def tearDown(self) -> None:
        shutil.rmtree(self.temp_dir, ignore_errors=True)

    def test_deterministic_replay_success_with_parameter_override(self) -> None:
        # Mock URL and DOM page text to satisfy checkpoints
        self.mock_driver.call_mcp_tool.return_value = {"url": "https://example.com/search?q=Tesla"}
        self.mock_driver.capture_page_text.return_value = "Operation Success: Tesla results loaded"

        replayer = RoutineReplayer(
            routine_name="demo_routine",
            routines_dir=self.temp_dir,
            driver=self.mock_driver,
            heal_with_cua=False,
        )

        res = replayer.replay(params={"query": "Tesla"})
        self.assertTrue(res.success)
        self.assertEqual(res.status, "completed")
        self.assertEqual(res.steps_executed, 2)
        self.assertEqual(res.parameters_used["query"], "Tesla")
        self.assertFalse(res.healed)

    def test_checkpoint_failure_triggers_cua_self_healing(self) -> None:
        # Checkpoint failure: page text returns "404 Not Found", missing "success"
        self.mock_driver.call_mcp_tool.return_value = {"url": "https://example.com/search"}
        self.mock_driver.capture_page_text.return_value = "Layout shifted: 404 Not Found"

        # Mock the CUA vision driver for healing
        mock_healing_result = DriveResult(
            success=True,
            status="completed",
            steps=[
                StepRecord(
                    step_index=1,
                    action=ReachAction(
                        kind="click",
                        point=(400, 200),
                        description="Click healed alternative search button",
                    ),
                    observation_summary="Healed search",
                    screenshot_path=str(self.frame_file),
                ),
                StepRecord(
                    step_index=2,
                    action=ReachAction(
                        kind="type",
                        value="Tesla",
                        description="Type into healed input",
                    ),
                    observation_summary="Typed into healed input",
                    screenshot_path=str(self.frame_file),
                ),
            ],
            final_description="Successfully recovered search flow",
        )

        replayer = RoutineReplayer(
            routine_name="demo_routine",
            routines_dir=self.temp_dir,
            driver=self.mock_driver,
            heal_with_cua=True,
        )

        with patch.object(ReachDriver, "drive", return_value=mock_healing_result):
            # Once healed, page text contains "success"
            self.mock_driver.capture_page_text.side_effect = [
                "Layout shifted: 404 Not Found",
                "Success! Found search results",
                "Success! Found search results",
            ]

            res = replayer.replay(params={"query": "Tesla"})

        self.assertTrue(res.success)
        self.assertEqual(res.status, "healed")
        self.assertTrue(res.healed)
        self.assertTrue(len(res.healed_steps) >= 1)

        # Verify routine.json was healed and written to disk
        with open(self.routine_dir / "routine.json", "r", encoding="utf-8") as f:
            healed_json = json.load(f)

        self.assertIsNotNone(healed_json["healed_at"])
        self.assertGreaterEqual(healed_json["version"], 2)
        # Newly recorded action was spliced into the routine
        step_kinds = [s["action"]["kind"] for s in healed_json["steps"]]
        self.assertIn("click", step_kinds)

    def test_deterministic_replay_dispatches_ref(self) -> None:
        action = CompiledAction(
            kind="click",
            point=(100, 200),
            reference="@e9",
            description="Click on ref '@e9'",
        )
        replayer = RoutineReplayer(
            routine_name="demo_routine",
            routines_dir=self.temp_dir,
            driver=self.mock_driver,
            heal_with_cua=False,
        )
        ok, err = replayer._execute_deterministic_action(action)
        self.assertTrue(ok)
        self.assertIsNone(err)
        last_action = self.mock_driver.execute_action.call_args[0][0]
        self.assertEqual(last_action.ref, "@e9")


class TestTemplateAndHashingUtilities(unittest.TestCase):
    """Test helper functions."""

    def test_render_template_double_and_single_braces(self) -> None:
        params = {"company": "Acme Corp", "role": "Engineer"}
        t1 = "Hello {{company}}, looking for {role} role."
        rendered = render_template(t1, params)
        self.assertEqual(rendered, "Hello Acme Corp, looking for Engineer role.")

    def test_hash_distance(self) -> None:
        h1 = "0000000000000000"
        h2 = "0000000000000000"
        self.assertEqual(hash_distance(h1, h2), 0.0)

        h3 = "ffffffffffffffff"
        self.assertEqual(hash_distance(h1, h3), 1.0)


if __name__ == "__main__":
    unittest.main()
