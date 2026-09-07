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
        self.assertIsNone(step1.before_frame)
        self.assertIsNone(step1.after_frame)

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

        # Screenshots are ephemeral and never archived.
        self.assertFalse((Path(self.temp_dir) / "test_record_flow" / "frames").exists())
        self.assertTrue(
            all(not call.args and not call.kwargs for call in self.mock_driver.capture_page_text.call_args_list)
        )


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
        self.assertIsNone(recorder.steps[1].text)
        self.assertIsNone(recorder.steps[1].input_name)
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
                    metadata={"after_frame_hash": compute_frame_hash_hex(self.frame1)},
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
                    input_name="query",
                    selector="input#searchbox_input",
                    aria_tag="Search query",
                    metadata={"dom_keywords": ["dashboard"]},
                ),
            ],
        )

        compiler = RoutineCompiler(screen_width=1280, screen_height=720)
        compiled = compiler.compile(trace, routines_dir=self.temp_dir)

        self.assertIn("query", compiled.parameters)
        self.assertIsNone(compiled.parameters["query"])
        # 2. Normalized coordinates for step 2
        click_step = compiled.steps[1]
        self.assertEqual(click_step.action.point, (640, 360))
        self.assertEqual(click_step.action.normalized_point, (0.5, 0.5))
        self.assertEqual(click_step.action.selector, "input#searchbox_input")
        type_step = compiled.steps[2]

        self.assertEqual(type_step.action.value, "{{query}}")

        # 4. Injected Checkpoints
        nav_step = compiled.steps[0]
        # URL checkpoint on navigate
        url_cps = [c for c in nav_step.checkpoints if c.type == "url_origin_equals"]
        self.assertTrue(len(url_cps) >= 1)
        self.assertEqual(url_cps[0].value, "https://duckduckgo.com")


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

    def test_navigation_path_query_uses_named_runtime_url(self) -> None:
        trace = RoutineTrace(
            name="url_routine",
            screen=0,
            created_at="2026-09-05T00:00:00Z",
            steps=[
                TraceStep(
                    step_index=1,
                    timestamp="2026-09-05T00:00:01Z",
                    action_type="navigate",
                    url="https://example.test/path?secret=canary",
                )
            ],
        )

        compiled = RoutineCompiler().compile(trace, routines_dir=self.temp_dir)
        action = compiled.steps[0].action

        self.assertEqual(action.url, "https://example.test")
        self.assertIsInstance(action.input_name, str)
        self.assertIn(action.input_name, compiled.parameters)
        self.assertIsNone(compiled.parameters[action.input_name])
        self.assertEqual(action.to_dict()["url"], "https://example.test")

        persisted = (Path(self.temp_dir) / "url_routine" / "routine.json").read_text(
            encoding="utf-8"
        )
        self.assertNotIn("canary", persisted)
        self.assertNotIn("/path", persisted)
        credential_trace = RoutineTrace(
            name="credential_root",
            screen=0,
            created_at="2026-09-05T00:00:00Z",
            steps=[
                TraceStep(
                    step_index=1,
                    timestamp="2026-09-05T00:00:01Z",
                    action_type="navigate",
                    url="https://alice:secret@example.test/",
                )
            ],
        )
        credential_compiled = RoutineCompiler().compile(
            credential_trace, routines_dir=self.temp_dir  # gitleaks:allow -- keyword arguments, not a secret
        )
        credential_action = credential_compiled.steps[0].action
        self.assertEqual(credential_action.url, "https://example.test")
        self.assertIsInstance(credential_action.input_name, str)
        credential_persisted = (
            Path(self.temp_dir) / "credential_root" / "routine.json"
        ).read_text(encoding="utf-8")
        self.assertNotIn("alice", credential_persisted)
        self.assertNotIn("secret", credential_persisted)


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
                            "type": "url_origin_equals",
                            "value": "https://example.com",
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
                        "input_name": "query",
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
        self.mock_driver.call_mcp_tool.return_value = {"isError": False, "content": [
            {"type": "text", "text": json.dumps({"status": "ok", "url": "https://example.com/search?q=Tesla"})}]}
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
        self.assertIn("query", res.parameters_used)
        self.assertNotIn("Tesla", json.dumps(res.to_dict()))
        self.assertFalse(res.healed)
    def test_lookalike_origin_checkpoint_stops_before_named_input(self) -> None:
        self.mock_driver.call_mcp_tool.return_value = {
            "isError": False,
            "content": [{"type": "text", "text": json.dumps({
                "status": "ok", "url": "https://example.com.evil.test/search",
            })}],
        }
        self.mock_driver.capture_page_text.return_value = "Operation Success"
        replayer = RoutineReplayer(
            routine_name="demo_routine",
            routines_dir=self.temp_dir,
            driver=self.mock_driver,
            heal_with_cua=False,
        )

        result = replayer.replay(params={"query": "Tesla"})

        self.assertFalse(result.success)
        self.assertEqual(result.status, "failed")
        self.assertEqual(self.mock_driver.execute_action.call_count, 1)

    def test_runtime_url_input_is_required_and_delivered_in_full(self) -> None:
        recorder_driver = MagicMock()
        recorder_driver.capture_screenshot.return_value = None
        recorder_driver.capture_page_text.return_value = ""
        recorder = RoutineRecorder(
            routine_name="demo_routine",
            screen=0,
            routines_dir=self.temp_dir,
            driver=recorder_driver,
        )
        recorder.record_step(
            action_type="navigate",
            url="https://example.test/path?secret=canary",
            execute=False,
        )
        trace_file = self.routine_dir / "trace.json"
        self.assertTrue(trace_file.is_file())
        trace_payload = trace_file.read_text(encoding="utf-8")
        self.assertNotIn("canary", trace_payload)
        self.assertNotIn("/path", trace_payload)

        RoutineCompiler().compile("demo_routine", routines_dir=self.temp_dir)
        replayer = RoutineReplayer(
            routine_name="demo_routine",
            routines_dir=self.temp_dir,
            driver=self.mock_driver,
            heal_with_cua=False,
        )
        input_name = replayer.routine.steps[0].action.input_name
        self.assertIsInstance(input_name, str)

        missing = replayer.replay(params={})
        self.assertFalse(missing.success)
        self.assertEqual(missing.status, "missing_parameters")
        self.mock_driver.execute_action.assert_not_called()

        wrong_origin = replayer.replay(
            params={input_name: "https://evil.test/path?secret=canary"}
        )
        self.assertFalse(wrong_origin.success)
        self.assertEqual(wrong_origin.status, "invalid_parameters")
        self.mock_driver.execute_action.assert_not_called()

        full_url = "https://example.test/path?secret=canary"
        self.mock_driver.call_mcp_tool.return_value = {
            "isError": False,
            "content": [{"type": "text", "text": json.dumps({"status": "ok", "url": full_url})}],
        }
        replayed = replayer.replay(params={input_name: full_url})
        self.assertTrue(replayed.success)
        action = self.mock_driver.execute_action.call_args[0][0]
        self.assertEqual(action.value, full_url)
        self.assertEqual(action.target, full_url)
        self.assertNotIn("canary", json.dumps(replayed.to_dict()))


    def test_checkpoint_failure_triggers_cua_self_healing(self) -> None:
        # Checkpoint failure: page text returns "404 Not Found", missing "success"
        self.mock_driver.call_mcp_tool.return_value = {"isError": False, "content": [
            {"type": "text", "text": json.dumps({"status": "ok", "url": "https://example.com/search"})}]}
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

        with open(self.routine_dir / "routine.json", "r", encoding="utf-8") as f:
            self.assertEqual(json.load(f), self.routine_data)

    def test_healing_model_success_cannot_bypass_failed_postcondition(self) -> None:
        replayer = RoutineReplayer(
            routine_name="demo_routine", routines_dir=self.temp_dir,
            driver=self.mock_driver, heal_with_cua=True,
        )
        self.mock_driver.capture_page_text.return_value = "Still broken"
        step = replayer.routine.steps[1]
        proposal = DriveResult(success=True, status="completed")
        with patch.object(ReachDriver, "drive", return_value=proposal):
            recovered, _, _, _ = replayer._heal_step(step, step.action, "broken", {}, 1)
        self.assertFalse(recovered)

    def test_uncertain_mutation_stops_recovery_without_healing(self) -> None:
        self.mock_driver.call_mcp_tool.return_value = {
            "isError": False,
            "content": [{"type": "text", "text": json.dumps({
                "status": "ok", "url": "https://example.com/search",
            })}],
        }
        self.mock_driver.execute_action.return_value = {
            "status": "uncertain",
            "error": "transport outcome unknown",
        }
        replayer = RoutineReplayer(
            routine_name="demo_routine",
            routines_dir=self.temp_dir,
            driver=self.mock_driver,
            heal_with_cua=True,
        )
        with patch.object(ReachDriver, "drive") as drive:
            result = replayer.replay(params={"query": "Tesla"})
        self.assertFalse(result.success)
        self.assertEqual(result.status, "uncertain")
        drive.assert_not_called()

    def test_invalid_visual_hash_fails_checkpoint(self) -> None:
        replayer = RoutineReplayer(
            routine_name="demo_routine",
            routines_dir=self.temp_dir,
            driver=self.mock_driver,
            heal_with_cua=False,
        )
        checkpoint = Checkpoint(type="visual_phash", expected_hash="not-a-hash", threshold=1.0)
        valid, failed, reason = replayer._validate_checkpoints([checkpoint])
        self.assertFalse(valid)
        self.assertIs(failed, checkpoint)
        self.assertIn("valid evidence", reason)

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

    def test_invalid_frame_cannot_become_a_visual_checkpoint(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            frame = Path(directory) / "invalid.png"
            frame.write_bytes(b"not an image")
            self.assertIsNone(compute_frame_hash_hex(frame))

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
