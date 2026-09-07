"""Unit tests for Reach CUA Driver (reach_drive.py) under the strict
action-proposal contract:

- The parser accepts exactly one JSON object holding an action object
  (an optional whole fenced JSON block is also accepted); surrounding
  prose is rejected.
- Only canonical kinds are valid: click, type, key, navigate, wait,
  scroll, auth_required, terminate. Unknown kinds and synonyms reject.
- Typed fields are validated (e.g. a nonnumeric click point rejects).
- A terminate proposal must carry outcome completed|blocked|failed.
- Drive-level success requires explicit outcome=completed, a
  caller-configured completion_text, and a freshly captured page_text
  containing it after termination; anything else is unsuccessful.
- agy nonzero exit is never accepted despite plausible stdout.
"""

import json
import subprocess
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
    DriveResult,
)


class ReachDriverTests(unittest.TestCase):
    def setUp(self) -> None:
        self.driver = ReachDriver(
            api_url="http://127.0.0.1:4200",
            screen=0,
            model="gemini-3.8-flash-high",
            max_steps=5,
            timeout_sec=60,
            enable_audit=False,
        )

    def tearDown(self) -> None:
        self.driver.cleanup()

    # ------------------------------------------------------------------
    # Prompt construction (Gauntlet control boundaries)
    # ------------------------------------------------------------------

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

    # ------------------------------------------------------------------
    # Strict action-proposal parsing
    # ------------------------------------------------------------------

    def test_parse_action_valid_json(self) -> None:
        # One pure JSON object with an action object: accepted.
        raw_text = json.dumps(
            {
                "action": {
                    "kind": "click",
                    "point": [240, 480],
                    "button": "left",
                    "description": "Click the submit button",
                }
            }
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
        # A whole fenced JSON block (nothing before or after): accepted.
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

    def test_parse_action_rejects_surrounding_prose(self) -> None:
        # Prose around a raw object or around a fenced block is rejected.
        pure = json.dumps({"action": {"kind": "click", "point": [240, 480]}})
        fenced = "```json\n" + pure + "\n```"
        for response in (
            "Sure, clicking now: " + pure,
            pure + "\nLet me know if that works.",
            "Here is my proposal:\n" + fenced,
            fenced + "\nHope that helps.",
        ):
            with self.subTest(response=response), self.assertRaises(ValueError):
                ReachDriver.extract_action_from_text(response)

    def test_parse_action_terminate_requires_valid_outcome(self) -> None:
        # Missing or invalid terminate outcome rejects.
        for outcome in (None, "", "maybe", "success", "COMPLETED"):
            action_data = {"kind": "terminate", "description": "Done"}
            if outcome is not None:
                action_data["outcome"] = outcome
            with self.subTest(outcome=outcome), self.assertRaises(ValueError):
                ReachDriver.extract_action_from_text(json.dumps({"action": action_data}))

        # Each canonical outcome parses and is preserved on the action.
        for outcome in ("completed", "blocked", "failed"):
            with self.subTest(outcome=outcome):
                action = ReachDriver.extract_action_from_text(
                    json.dumps({"action": {"kind": "terminate", "outcome": outcome}})
                )
                self.assertEqual(action.kind, "terminate")
                self.assertEqual(action.outcome, outcome)

    def test_parse_action_rejects_nonnumeric_click_point(self) -> None:
        # Malformed typed fields reject instead of silently degrading to a
        # click without coordinates.
        for point in (["left", 100], "center", [100]):
            with self.subTest(point=point), self.assertRaises(ValueError):
                ReachDriver.extract_action_from_text(
                    json.dumps({"action": {"kind": "click", "point": point}})
                )

    def test_parse_action_auth_required(self) -> None:
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

    def test_invoke_agy_rejects_nonzero_exit_despite_stdout(self) -> None:
        # A nonzero agy exit is never accepted, even when stdout carries a
        # well-formed SUCCESS envelope proposing a completed termination.
        decoy_stdout = json.dumps(
            {
                "status": "SUCCESS",
                "response": json.dumps(
                    {
                        "action": {
                            "kind": "terminate",
                            "outcome": "completed",
                            "description": "Done",
                        }
                    }
                ),
            }
        )
        fake_proc = subprocess.CompletedProcess(
            args=[], returncode=1, stdout=decoy_stdout, stderr="boom"
        )
        with patch(
            "scripts.reach_drive.subprocess.run", return_value=fake_proc
        ), self.assertRaises(RuntimeError):
            self.driver.invoke_agy("prompt", "/tmp/dummy.png")

    # ------------------------------------------------------------------
    # Ref parsing (accessibility-tree references)
    # ------------------------------------------------------------------

    def test_parse_ref_click_action_normalizes_ref(self) -> None:
        raw_text = '{"action": {"kind": "click", "ref": "@e3", "button": "left", "description": "Click login"}}'
        action = ReachDriver.extract_action_from_text(raw_text)
        self.assertEqual(action.kind, "click")
        self.assertEqual(action.ref, "@e3")
        self.assertIsNone(action.point)

    def test_parse_ref_type_action_normalizes_ref(self) -> None:
        raw_text = '{"action": {"kind": "type", "ref": "e1", "value": "alice@reach.io", "description": "Enter email"}}'
        action = ReachDriver.extract_action_from_text(raw_text)
        self.assertEqual(action.kind, "type")
        self.assertEqual(action.ref, "@e1")
        self.assertEqual(action.value, "alice@reach.io")

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

    # ------------------------------------------------------------------
    # Drive loop outcome contract
    # ------------------------------------------------------------------

    @staticmethod
    def _terminate_proposal(
        description: str = "Found answer: 42",
        outcome: str = "completed",
    ) -> str:
        return json.dumps(
            {
                "status": "SUCCESS",
                "response": json.dumps(
                    {
                        "action": {
                            "kind": "terminate",
                            "outcome": outcome,
                            "description": description,
                        }
                    }
                ),
            }
        )

    @patch.object(ReachDriver, "capture_screenshot")
    @patch.object(ReachDriver, "capture_page_text")
    @patch.object(ReachDriver, "invoke_agy")
    def test_drive_loop_completed_requires_fresh_matching_page_text(
        self, mock_agy: MagicMock, mock_text: MagicMock, mock_shot: MagicMock
    ) -> None:
        mock_shot.return_value = "/tmp/dummy.png"
        mock_agy.return_value = self._terminate_proposal()

        # Fresh post-termination capture contains the caller postcondition:
        # the only path to verified success.
        mock_text.side_effect = [
            "Loading data...",
            "final report shows answer: 42 confirmed",
        ]
        driver = ReachDriver(
            max_steps=1, enable_audit=False, completion_text="answer: 42"
        )
        try:
            result = driver.drive(goal="Find the answer")
        finally:
            driver.cleanup()
        self.assertTrue(result.success)
        self.assertEqual(result.status, "completed")
        self.assertEqual(result.final_description, "Found answer: 42")
        self.assertEqual(len(result.steps), 1)

        # A pre-action observation containing the marker does not count:
        # only the freshly captured post-termination page text is
        # authoritative, so a failed postcondition cannot yield success.
        mock_text.side_effect = [
            "answer: 42 was already visible",
            "page changed, marker gone",
        ]
        driver = ReachDriver(
            max_steps=1, enable_audit=False, completion_text="answer: 42"
        )
        try:
            result = driver.drive(goal="Find the answer")
        finally:
            driver.cleanup()
        self.assertFalse(result.success)
        self.assertEqual(result.status, "postcondition_failed")

        # An absent (unset) completion_text can never produce success,
        # even when the fresh page text happens to contain the marker.
        mock_text.side_effect = ["plain page", "answer: 42 present on screen"]
        driver = ReachDriver(max_steps=1, enable_audit=False)
        try:
            result = driver.drive(goal="Find the answer")
        finally:
            driver.cleanup()
        self.assertFalse(result.success)
        self.assertEqual(result.status, "unverified")

    @patch.object(ReachDriver, "capture_screenshot")
    @patch.object(ReachDriver, "capture_page_text")
    @patch.object(ReachDriver, "invoke_agy")
    def test_drive_loop_blocked_outcome_is_unsuccessful(
        self, mock_agy: MagicMock, mock_text: MagicMock, mock_shot: MagicMock
    ) -> None:
        mock_shot.return_value = "/tmp/dummy.png"
        mock_text.return_value = "Page content"
        mock_agy.return_value = self._terminate_proposal(
            description="Hit a paywall", outcome="blocked"
        )

        driver = ReachDriver(
            max_steps=1, enable_audit=False, completion_text="answer: 42"
        )
        try:
            result = driver.drive(goal="Find the answer")
        finally:
            driver.cleanup()
        self.assertFalse(result.success)
        self.assertEqual(result.status, "blocked")

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
        self.assertFalse(result.success)
        self.assertEqual(result.status, "max_steps_exceeded")
        self.assertEqual(len(result.steps), self.driver.max_steps)


    def test_injection_proposal_contains_only_server_record_fields(self) -> None:
        action = ReachDriver.extract_action_from_text(
            json.dumps({
                "action": {
                    "kind": "inject",
                    "record_kind": "card",
                    "id": "card_123",
                    "domain": "shop.example",
                    "submit": True,
                }
            })
        )
        self.assertEqual(action.record_kind, "card")
        self.assertEqual(action.record_id, "card_123")
        self.assertEqual(action.domain, "shop.example")
        self.assertTrue(action.submit)
        self.assertNotIn("card_number", action.to_dict())
        self.assertNotIn("cvv", action.to_dict())

    def test_nonterminal_status_cannot_be_successful(self) -> None:
        result = DriveResult(success=True, status="uncertain")
        self.assertFalse(result.success)
if __name__ == "__main__":
    unittest.main()
