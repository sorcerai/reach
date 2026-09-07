import io
import json
import unittest
from unittest.mock import patch

from scripts.reach_drive import ReachDriver


class CompletionContractTests(unittest.TestCase):
    def test_prose_and_unknown_actions_never_become_executable_actions(self):
        for response in (
            "The task is not done. I could not complete it.",
            "I cannot terminate safely because an approval is pending.",
            json.dumps({"action": {"kind": "unknown", "point": [1, 2]}}),
            json.dumps({"action": {"kind": "click", "point": [1, 2], "buton": "right"}}),
        ):
            with self.subTest(response=response), self.assertRaises(ValueError):
                ReachDriver.extract_action_from_text(response)

    def test_model_completion_without_postcondition_is_not_verified_success(self):
        driver = ReachDriver(max_steps=1, enable_audit=False)
        proposal = json.dumps({"status": "SUCCESS", "response": json.dumps({
            "action": {"kind": "terminate", "outcome": "completed", "description": "Done"}
        })})
        try:
            with patch.object(driver, "capture_screenshot", return_value="/tmp/not-a-screenshot"), \
                 patch.object(driver, "capture_page_text", return_value="Ordinary page"), \
                 patch.object(driver, "invoke_agy", return_value=proposal):
                result = driver.drive("Complete the task")
            self.assertFalse(result.success)
            self.assertEqual(result.status, "unverified")
        finally:
            driver.cleanup()

    def test_empty_mcp_result_is_not_evidence(self):
        driver = ReachDriver(enable_audit=False)
        response = io.BytesIO(json.dumps({"jsonrpc": "2.0", "id": 1, "result": {}}).encode())
        with patch.object(driver._api_opener, "open", return_value=response):
            with self.assertRaises(RuntimeError):
                driver.call_mcp_tool("page_text", {})
