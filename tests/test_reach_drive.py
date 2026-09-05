"""Unit tests for Reach CUA Driver (reach_drive.py)."""

import json
from pathlib import Path
import sys
import unittest
from unittest.mock import MagicMock, patch

REPO_ROOT = Path(__file__).parent.parent.resolve()
if str(REPO_ROOT) not in sys.path:
    sys.path.insert(0, str(REPO_ROOT))

from scripts.reach_drive import (  # noqa: E402
    AUTH_SIGNALS_RE,
    AGY_CONTROL_PREFIX,
    AGY_CONTROL_SUFFIX,
    AGY_UNTRUSTED_SCREENSHOT_LABEL,
    ReachAction,
    ReachDriver,
    StepRecord,
)


class ReachDriverTests(unittest.TestCase):
    def setUp(self) -> None:
        self.driver = ReachDriver(
            api_url="http://127.0.0.1:4200",
            screen=0,
            model="gemini-3.8-flash-high",
            max_steps=5,
            timeout_sec=60,
        )

    def tearDown(self) -> None:
        self.driver.cleanup()

    def test_prompt_construction_matches_gauntlet_protocol(self) -> None:
        screenshot_path = "/tmp/fake_shot.png"
        goal = "Click the login button and sign in"
        page_text = "Login page with Username and Password inputs"
        history = [
            StepRecord(
                step_index=1,
                action=ReachAction(
                    kind="navigate",
                    target="https://example.com",
                    description="Open site",
                ),
                observation_summary="Opened site",
            )
        ]

        prompt = self.driver.build_prompt(
            goal=goal,
            screenshot_path=screenshot_path,
            page_text=page_text,
            history=history,
            remaining_steps=4,
        )

        # Verify Gauntlet control boundaries
        self.assertIn(AGY_CONTROL_PREFIX[0], prompt)
        self.assertIn(AGY_CONTROL_SUFFIX[0], prompt)
        self.assertIn(AGY_UNTRUSTED_SCREENSHOT_LABEL, prompt)
        self.assertIn(f"@{screenshot_path}", prompt)
        self.assertIn(f"Goal: {goal}", prompt)
        self.assertIn("Page Text Snapshot:", prompt)
        self.assertIn(page_text, prompt)
        self.assertIn("#1 navigate", prompt)
        self.assertIn("END GAUNTLET UNTRUSTED PAGE/GOAL DATA.", prompt)

    def test_parse_action_valid_json(self) -> None:
        raw_text = (
            'Some thinking here... {"action": {"actionClass": "read_only", '
            '"kind": "click", "point": [240, 480], "button": "left", '
            '"description": "Click the submit button"}}'
        )
        action = ReachDriver.extract_action_from_text(raw_text)
        self.assertEqual(action.kind, "click")
        self.assertEqual(action.point, (240, 480))
        self.assertEqual(action.button, "left")
        self.assertEqual(action.description, "Click the submit button")

    def test_parse_action_from_agy_envelope(self) -> None:
        envelope = json.dumps(
            {
                "status": "SUCCESS",
                "response": json.dumps(
                    {
                        "action": {
                            "kind": "type",
                            "target": "Search input",
                            "value": "Gemini 3.8 Flash",
                            "description": "Type search query",
                        }
                    }
                ),
            }
        )
        action = self.driver.parse_action(envelope)
        self.assertEqual(action.kind, "type")
        self.assertEqual(action.value, "Gemini 3.8 Flash")
        self.assertEqual(action.target, "Search input")

    def test_parse_action_markdown_fenced(self) -> None:
        raw_text = """```json
{
  "action": {
    "kind": "key",
    "key": "Return",
    "description": "Press enter to submit search"
  }
}
```"""
        action = ReachDriver.extract_action_from_text(raw_text)
        self.assertEqual(action.kind, "key")
        self.assertEqual(action.key, "Return")

    def test_parse_action_auth_required_and_terminate(self) -> None:
        auth_text = json.dumps(
            {
                "action": {
                    "kind": "auth_required",
                    "description": "Two-factor authentication prompt detected",
                }
            }
        )
        action_auth = ReachDriver.extract_action_from_text(auth_text)
        self.assertEqual(action_auth.kind, "auth_required")

        term_text = json.dumps(
            {
                "action": {
                    "kind": "terminate",
                    "description": "Goal accomplished successfully",
                }
            }
        )
        action_term = ReachDriver.extract_action_from_text(term_text)
        self.assertEqual(action_term.kind, "terminate")

    def test_auth_signals_regex(self) -> None:
        self.assertTrue(
            AUTH_SIGNALS_RE.search("Please complete two-factor authentication")
        )
        self.assertTrue(AUTH_SIGNALS_RE.search("Enter your 2fa verification code"))
        self.assertTrue(AUTH_SIGNALS_RE.search("Security Check: verify it's you"))
        self.assertTrue(AUTH_SIGNALS_RE.search("Enter OTP sent to your phone"))
        self.assertTrue(AUTH_SIGNALS_RE.search("reCAPTCHA checkbox required"))
        self.assertFalse(
            AUTH_SIGNALS_RE.search("Welcome to our blog article about computers")
        )

    @patch.object(ReachDriver, "call_mcp_tool")
    def test_execute_action_dispatches_correctly(self, mock_mcp: MagicMock) -> None:
        mock_mcp.return_value = {"status": "ok"}

        # Click action
        self.driver.execute_action(
            ReachAction(kind="click", point=(300, 450), button="right")
        )
        mock_mcp.assert_called_with(
            "click", {"x": 300, "y": 450, "button": "right", "screen": 0}
        )

        # Type action
        self.driver.execute_action(ReachAction(kind="type", value="hello reach"))
        mock_mcp.assert_called_with("type", {"text": "hello reach", "screen": 0})

        # Key action
        self.driver.execute_action(ReachAction(kind="key", key="Escape"))
        mock_mcp.assert_called_with("key", {"combo": "Escape", "screen": 0})

        # Navigate action
        self.driver.execute_action(
            ReachAction(kind="navigate", target="https://example.com")
        )
        mock_mcp.assert_called_with(
            "browse",
            {"url": "https://example.com", "screen": 0, "use_profile": "default"},
        )

    @patch.object(ReachDriver, "capture_screenshot")
    @patch.object(ReachDriver, "capture_page_text")
    @patch.object(ReachDriver, "invoke_agy")
    def test_drive_loop_completes_on_terminate(
        self, mock_agy: MagicMock, mock_text: MagicMock, mock_shot: MagicMock
    ) -> None:
        mock_shot.return_value = "/tmp/dummy.png"
        mock_text.return_value = "Page content"
        mock_agy.return_value = json.dumps(
            {
                "status": "SUCCESS",
                "response": json.dumps(
                    {"action": {"kind": "terminate", "description": "Found answer: 42"}}
                ),
            }
        )

        result = self.driver.drive(goal="Find the answer")
        self.assertTrue(result.success)
        self.assertEqual(result.status, "completed")
        self.assertEqual(result.final_description, "Found answer: 42")
        self.assertEqual(len(result.steps), 1)

    @patch.object(ReachDriver, "capture_screenshot")
    @patch.object(ReachDriver, "capture_page_text")
    @patch.object(ReachDriver, "invoke_agy")
    @patch.object(ReachDriver, "set_takeover")
    @patch.object(ReachDriver, "get_novnc_url")
    def test_drive_loop_triggers_takeover_on_auth_proposal(
        self,
        mock_vnc: MagicMock,
        mock_takeover: MagicMock,
        mock_agy: MagicMock,
        mock_text: MagicMock,
        mock_shot: MagicMock,
    ) -> None:
        mock_shot.return_value = "/tmp/dummy.png"
        mock_text.return_value = "Normal page"
        mock_vnc.return_value = "http://127.0.0.1:6080/vnc.html"
        mock_agy.return_value = json.dumps(
            {
                "status": "SUCCESS",
                "response": json.dumps(
                    {
                        "action": {
                            "kind": "auth_required",
                            "description": "Please enter SMS verification code",
                        }
                    }
                ),
            }
        )

        result = self.driver.drive(goal="Check bank account balance")
        self.assertFalse(result.success)
        self.assertEqual(result.status, "auth_required")
        self.assertEqual(result.takeover_url, "http://127.0.0.1:6080/vnc.html")
        mock_takeover.assert_called_with(True, "http://127.0.0.1:6080/vnc.html")

    @patch.object(ReachDriver, "capture_screenshot")
    @patch.object(ReachDriver, "capture_page_text")
    @patch.object(ReachDriver, "invoke_agy")
    @patch.object(ReachDriver, "set_takeover")
    @patch.object(ReachDriver, "get_novnc_url")
    def test_drive_loop_triggers_takeover_on_dom_2fa_detection(
        self,
        mock_vnc: MagicMock,
        mock_takeover: MagicMock,
        mock_agy: MagicMock,
        mock_text: MagicMock,
        mock_shot: MagicMock,
    ) -> None:
        mock_shot.return_value = "/tmp/dummy.png"
        mock_text.return_value = (
            "Enter your two-factor authentication code to continue."
        )
        mock_vnc.return_value = "http://127.0.0.1:6080/vnc.html"

        result = self.driver.drive(goal="Access secure portal")
        self.assertFalse(result.success)
        self.assertEqual(result.status, "auth_required")
        self.assertEqual(result.takeover_url, "http://127.0.0.1:6080/vnc.html")
        mock_takeover.assert_called_with(True, "http://127.0.0.1:6080/vnc.html")
        # agy should not even be called when DOM 2FA is intercepted immediately
        mock_agy.assert_not_called()

    @patch.object(ReachDriver, "capture_screenshot")
    @patch.object(ReachDriver, "capture_page_text")
    @patch.object(ReachDriver, "invoke_agy")
    @patch.object(ReachDriver, "execute_action")
    def test_drive_loop_hits_max_steps(
        self,
        mock_exec: MagicMock,
        mock_agy: MagicMock,
        mock_text: MagicMock,
        mock_shot: MagicMock,
    ) -> None:
        mock_shot.return_value = "/tmp/dummy.png"
        mock_text.return_value = "Page content"
        mock_agy.return_value = json.dumps(
            {
                "status": "SUCCESS",
                "response": json.dumps(
                    {
                        "action": {
                            "kind": "click",
                            "point": [100, 100],
                            "description": "Keep clicking",
                        }
                    }
                ),
            }
        )

        with patch("time.sleep"):  # skip sleep delays in test
            result = self.driver.drive(goal="Endless loop")

    def test_parse_and_execute_ref_click_action(self) -> None:
        raw_text = '{"action": {"kind": "click", "ref": "@e3", "button": "left", "description": "Click login"}}'
        action = ReachDriver.extract_action_from_text(raw_text)
        self.assertEqual(action.kind, "click")
        self.assertEqual(action.ref, "@e3")
        self.assertIsNone(action.point)

        with patch.object(self.driver, "call_mcp_tool") as mock_mcp:
            mock_mcp.return_value = {"status": "ok"}
            self.driver.execute_action(action)
            mock_mcp.assert_called_once_with(
                "click",
                {"ref": "@e3", "button": "left", "screen": 0},
            )

    def test_parse_and_execute_ref_type_action(self) -> None:
        raw_text = '{"action": {"kind": "type", "ref": "e1", "value": "alice@reach.io", "description": "Enter email"}}'
        action = ReachDriver.extract_action_from_text(raw_text)
        self.assertEqual(action.kind, "type")
        self.assertEqual(action.ref, "@e1")
        self.assertEqual(action.value, "alice@reach.io")

        with patch.object(self.driver, "call_mcp_tool") as mock_mcp:
            mock_mcp.return_value = {"status": "ok"}
            self.driver.execute_action(action)
            mock_mcp.assert_called_once_with(
                "type",
                {"text": "alice@reach.io", "ref": "@e1", "screen": 0},
            )

    def test_capture_page_text_extracts_axtree_and_refs(self) -> None:
        page_text_payload = json.dumps({
            "status": "ok",
            "url": "https://example.com/login",
            "title": "Login",
            "text": "Login page body text",
            "axtree": "[heading \"Sign In\"]\n[@e1: textbox \"Email\" focused x=200 y=100 w=200 h=30]\n[@e2: button \"Submit\" x=200 y=150 w=80 h=30]",
            "refs": {
                "e1": {"ref": "e1", "role": "textbox", "name": "Email", "point": [300, 115]},
                "e2": {"ref": "e2", "role": "button", "name": "Submit", "point": [240, 165]}
            }
        })

        with patch.object(self.driver, "call_mcp_tool") as mock_mcp:
            mock_mcp.return_value = {
                "content": [{"type": "text", "text": page_text_payload}]
            }
            res = self.driver.capture_page_text("https://example.com/login")
            self.assertIn("Accessibility Tree (Interact via @eN refs):", res)
            self.assertIn("@e1: textbox \"Email\"", res)
            self.assertIn("@e2: button \"Submit\"", res)


if __name__ == "__main__":
    unittest.main()
