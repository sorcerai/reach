"""Unit tests for Reach Buzz Agent Daemon (scripts/buzz_daemon.py)."""

import json
from pathlib import Path
import sys
import unittest
from unittest.mock import MagicMock, call, patch

REPO_ROOT = Path(__file__).parent.parent.resolve()
if str(REPO_ROOT) not in sys.path:
    sys.path.insert(0, str(REPO_ROOT))

from scripts.buzz_daemon import (
    BuzzDaemon,
    ParsedTask,
    ReachApiClient,
    buzz_get_messages,
    buzz_list_channels,
    buzz_post_visual_diff,
    buzz_send_message,
    buzz_send_takeover_alert,
    parse_task_message,
)
from scripts.reach_drive import DriveResult, ReachAction, StepRecord


class MessageParsingTests(unittest.TestCase):
    """Test parsing @ReachBot mentions, screen indexes, and goal instructions."""

    def test_simple_mention_default_screen(self) -> None:
        content = "@ReachBot check flights to SFO"
        task = parse_task_message(content)
        self.assertIsNotNone(task)
        assert task is not None
        self.assertEqual(task.screen, 0)
        self.assertEqual(task.goal, "check flights to SFO")
        self.assertIsNone(task.initial_url)

    def test_screen_indicators(self) -> None:
        cases = [
            ("@ReachBot screen 1 order coffee", 1, "order coffee"),
            ("@ReachBot screen:2 checkout cart", 2, "checkout cart"),
            ("@ReachBot screen=3 buy ticket", 3, "buy ticket"),
            ("@ReachBot [screen 4] check notifications", 4, "check notifications"),
            ("@ReachBot --screen 5 approve pull request", 5, "approve pull request"),
            ("@ReachBot display 6 monitor server", 6, "monitor server"),
            ("@ReachBot display:7 status", 7, "status"),
        ]
        for msg, expected_screen, expected_goal in cases:
            with self.subTest(msg=msg):
                task = parse_task_message(msg)
                self.assertIsNotNone(task)
                assert task is not None
                self.assertEqual(task.screen, expected_screen)
                self.assertEqual(task.goal, expected_goal)

    def test_url_extraction(self) -> None:
        # --url flag
        task1 = parse_task_message("@ReachBot --screen 1 --url https://example.com/login login to portal")
        self.assertIsNotNone(task1)
        assert task1 is not None
        self.assertEqual(task1.screen, 1)
        self.assertEqual(task1.initial_url, "https://example.com/login")
        self.assertEqual(task1.goal, "login to portal")

        # standalone url in text
        task2 = parse_task_message("@ReachBot open https://github.com/trending and inspect")
        self.assertIsNotNone(task2)
        assert task2 is not None
        self.assertEqual(task2.initial_url, "https://github.com/trending")
        self.assertIn("github.com/trending", task2.goal)

    def test_case_insensitive_and_custom_trigger(self) -> None:
        # Lowercase trigger
        task_lower = parse_task_message("@reachbot perform task")
        self.assertIsNotNone(task_lower)
        assert task_lower is not None
        self.assertEqual(task_lower.goal, "perform task")

        # Custom trigger
        task_custom = parse_task_message("@OpsBot screen:1 backup db", trigger="@OpsBot")
        self.assertIsNotNone(task_custom)
        assert task_custom is not None
        self.assertEqual(task_custom.screen, 1)
        self.assertEqual(task_custom.goal, "backup db")

    def test_non_matching_messages(self) -> None:
        self.assertIsNone(parse_task_message(""))
        self.assertIsNone(parse_task_message("Hello team, check out this link"))
        self.assertIsNone(parse_task_message("@OtherBot run routine"))


class ReachApiClientTests(unittest.TestCase):
    """Test ReachApiClient REST endpoints for lease, handoff, and wait."""

    def setUp(self) -> None:
        self.client = ReachApiClient(api_url="http://127.0.0.1:4200")

    @patch("urllib.request.urlopen")
    def test_lease_screen(self, mock_urlopen) -> None:
        mock_resp = MagicMock()
        mock_resp.read.return_value = json.dumps({
            "status": "ok",
            "id": 0,
            "owner": "ReachBot",
            "token": "lease-token-123",
        }).encode("utf-8")
        mock_urlopen.return_value.__enter__.return_value = mock_resp

        res = self.client.lease_screen(0, owner="ReachBot")
        self.assertEqual(res["status"], "ok")
        self.assertEqual(res["token"], "lease-token-123")
        self.assertEqual(self.client.lease_token, "lease-token-123")

        req = mock_urlopen.call_args[0][0]
        self.assertEqual(req.get_method(), "POST")
        self.assertIn("/agent/screens/0/lease", req.full_url)
        body = json.loads(req.data.decode("utf-8"))
        self.assertEqual(body["owner"], "ReachBot")

    @patch("urllib.request.urlopen")
    def test_release_screen(self, mock_urlopen) -> None:
        self.client.lease_token = "lease-token-123"
        mock_resp = MagicMock()
        mock_resp.read.return_value = json.dumps({"status": "ok", "released": True}).encode("utf-8")
        mock_urlopen.return_value.__enter__.return_value = mock_resp

        res = self.client.release_screen(0, owner="ReachBot")
        self.assertEqual(res["status"], "ok")
        self.assertTrue(res["released"])

        req = mock_urlopen.call_args[0][0]
        self.assertEqual(req.get_method(), "DELETE")
        self.assertEqual(req.get_header("X-lease-token"), "lease-token-123")

    @patch("urllib.request.urlopen")
    def test_request_takeover(self, mock_urlopen) -> None:
        self.client.lease_token = "lease-token-123"
        mock_resp = MagicMock()
        mock_resp.read.return_value = json.dumps({"status": "ok", "phase": "HandoffPending"}).encode("utf-8")
        mock_urlopen.return_value.__enter__.return_value = mock_resp

        res = self.client.request_takeover(0, reason="SMS 2FA Required", novnc_url="http://novnc:6080/vnc.html")
        self.assertEqual(res["status"], "ok")

        req = mock_urlopen.call_args[0][0]
        self.assertEqual(req.get_method(), "POST")
        self.assertEqual(req.get_header("X-lease-token"), "lease-token-123")
        body = json.loads(req.data.decode("utf-8"))
        self.assertTrue(body["pending"])
        self.assertEqual(body["reason"], "SMS 2FA Required")
        self.assertEqual(body["url"], "http://novnc:6080/vnc.html")

    @patch("urllib.request.urlopen")
    def test_wait_for_phase(self, mock_urlopen) -> None:
        mock_resp = MagicMock()
        mock_resp.read.return_value = json.dumps({"status": "ok", "phase": "HumanDone"}).encode("utf-8")
        mock_urlopen.return_value.__enter__.return_value = mock_resp

        res = self.client.wait_for_phase(0, phase="HumanDone", timeout=60)
        self.assertEqual(res["status"], "ok")
        self.assertEqual(res["phase"], "HumanDone")

        req = mock_urlopen.call_args[0][0]
        self.assertEqual(req.get_method(), "GET")
        self.assertIn("/agent/screens/0/wait?phase=HumanDone&timeout=60", req.full_url)

    @patch("urllib.request.urlopen")
    def test_ack_handback(self, mock_urlopen) -> None:
        self.client.lease_token = "lease-token-123"
        mock_resp = MagicMock()
        mock_resp.read.return_value = json.dumps({"status": "ok", "phase": "AgentActive"}).encode("utf-8")
        mock_urlopen.return_value.__enter__.return_value = mock_resp

        res = self.client.ack_handback(0)
        self.assertEqual(res["status"], "ok")
        self.assertEqual(res["phase"], "AgentActive")

        req = mock_urlopen.call_args[0][0]
        self.assertEqual(req.get_method(), "POST")
        self.assertEqual(req.get_header("X-lease-token"), "lease-token-123")


class BuzzDaemonWorkflowTests(unittest.TestCase):
    """Test BuzzDaemon message dispatch, thread updates, visual diffs, and completion."""

    def setUp(self) -> None:
        self.mock_reach_client = MagicMock(spec=ReachApiClient)
        self.mock_reach_client.lease_screen.return_value = {
            "status": "ok",
            "id": 0,
            "token": "tok-456",
        }
        self.mock_reach_client.release_screen.return_value = {"status": "ok", "released": True}
        self.mock_reach_client.get_novnc_url.return_value = "http://100.124.38.17:6080/vnc.html?autoconnect=true"

        self.mock_driver = MagicMock()
        self.mock_driver_factory = MagicMock(return_value=self.mock_driver)

        self.daemon = BuzzDaemon(
            relay_url="http://100.124.38.17:3000",
            reach_client=self.mock_reach_client,
            driver_factory=self.mock_driver_factory,
            enable_visual_diff=True,
        )

    @patch("scripts.buzz_daemon.buzz_send_message")
    @patch("scripts.buzz_daemon.buzz_post_visual_diff")
    def test_complete_workflow_success(self, mock_post_diff, mock_send_msg) -> None:
        """Verify thread ack reply, lease, driving loop with visual diff, and release/summary."""
        mock_send_msg.return_value = {"ok": True}
        mock_post_diff.return_value = {"ok": True}

        # Step record to simulate visual change
        step_rec = StepRecord(
            step_index=1,
            action=ReachAction(kind="click", target="Login Button", description="Click login"),
            observation_summary="Login button visible",
            screenshot_path="/tmp/screen_001.png",
            visual_change=0.25,
            vlm_cached=False,
        )

        def mock_drive(goal: str, initial_url: str = None) -> DriveResult:
            # Simulate step callback invocation during driving
            step_cb = self.mock_driver_factory.call_args[1].get("step_callback")
            if step_cb:
                step_cb(step_rec)
            return DriveResult(
                success=True,
                status="completed",
                steps=[step_rec],
                final_description="Successfully logged in and reached dashboard",
                audit_report_path="/srv/reach/audits/task-123/index.html",
            )

        self.mock_driver.drive.side_effect = mock_drive

        incoming_msg = {
            "id": "msg-001",
            "channel": "ops-channel",
            "content": "@ReachBot screen:0 sign in to admin portal",
            "created_at": 1725500000,
        }

        result = self.daemon.handle_message(incoming_msg)

        self.assertIsNotNone(result)
        assert result is not None
        self.assertTrue(result.success)

        # 1. Immediate acknowledgment posted to Buzz thread
        first_send = mock_send_msg.call_args_list[0]
        self.assertEqual(first_send.kwargs["channel"], "ops-channel")
        self.assertEqual(first_send.kwargs["reply_to"], "msg-001")
        self.assertIn("🐝 On it! Leased screen 0 and beginning execution...", first_send.kwargs["content"])

        # 2. Leased screen 0 from Reach API
        self.mock_reach_client.lease_screen.assert_called_once_with(0, owner="ReachBot")

        # 3. Driving loop invoked with token and callback
        self.mock_driver_factory.assert_called_once()
        self.assertEqual(self.mock_driver_factory.call_args[1]["screen"], 0)
        self.assertEqual(self.mock_driver_factory.call_args[1]["lease_token"], "tok-456")

        # 4. Visual diff posted to Buzz thread
        mock_post_diff.assert_called_once()
        self.assertEqual(mock_post_diff.call_args.kwargs["channel"], "ops-channel")
        self.assertEqual(mock_post_diff.call_args.kwargs["reply_to"], "msg-001")
        self.assertEqual(mock_post_diff.call_args.kwargs["screenshot_path"], "/tmp/screen_001.png")
        self.assertAlmostEqual(mock_post_diff.call_args.kwargs["diff_percent"], 25.0)

        # 5. Released screen lease
        self.mock_reach_client.release_screen.assert_called_once_with(
            screen=0, owner="ReachBot", token="tok-456"
        )

        # 6. Final summary message posted to Buzz thread
        last_send = mock_send_msg.call_args_list[-1]
        self.assertEqual(last_send.kwargs["channel"], "ops-channel")
        self.assertEqual(last_send.kwargs["reply_to"], "msg-001")
        self.assertIn("✅ **Reach Task Completed**", last_send.kwargs["content"])
        self.assertIn("/srv/reach/audits/task-123/index.html", last_send.kwargs["content"])


class TakeoverAndHandbackTests(unittest.TestCase):
    """Test 2FA/CAPTCHA takeover alert emission, waiting, ack handback, and execution resumption."""

    def setUp(self) -> None:
        self.mock_reach_client = MagicMock(spec=ReachApiClient)
        self.mock_reach_client.lease_screen.return_value = {"status": "ok", "token": "tok-789"}
        self.mock_reach_client.release_screen.return_value = {"status": "ok"}
        self.mock_reach_client.get_novnc_url.return_value = (
            "http://100.124.38.17:6080/vnc.html?autoconnect=true"
        )
        self.mock_reach_client.wait_for_phase.return_value = {
            "status": "ok",
            "phase": "HumanDone",
            "id": 0,
        }
        self.mock_reach_client.ack_handback.return_value = {
            "status": "ok",
            "phase": "AgentActive",
        }

        self.mock_driver = MagicMock()
        self.mock_driver_factory = MagicMock(return_value=self.mock_driver)

        self.daemon = BuzzDaemon(
            relay_url="http://100.124.38.17:3000",
            reach_client=self.mock_reach_client,
            driver_factory=self.mock_driver_factory,
        )

    @patch("scripts.buzz_daemon.buzz_send_message")
    @patch("scripts.buzz_daemon.buzz_send_takeover_alert")
    def test_interactive_takeover_and_handback_success(
        self, mock_takeover_alert, mock_send_msg
    ) -> None:
        """Verify takeover alert with direct noVNC link, wait for HumanDone, ack, and resume."""
        mock_takeover_alert.return_value = {"ok": True}
        mock_send_msg.return_value = {"ok": True}

        # First run encounters auth_required
        initial_result = DriveResult(
            success=False,
            status="auth_required",
            steps=[],
            final_description="SMS 2-Factor Challenge presented on screen",
            takeover_url="http://100.124.38.17:6080/vnc.html?autoconnect=true",
        )

        # Resumed run completes successfully
        resumed_result = DriveResult(
            success=True,
            status="completed",
            steps=[],
            final_description="Completed bank transfer post 2FA",
            audit_report_path="/srv/reach/audits/task-takeover/index.html",
        )

        self.mock_driver.drive.side_effect = [initial_result, resumed_result]

        incoming_msg = {
            "id": "thread-999",
            "channel": "finance",
            "content": "@ReachBot screen:0 transfer funds to vendor",
        }

        res = self.daemon.handle_message(incoming_msg)

        self.assertIsNotNone(res)
        assert res is not None
        self.assertTrue(res.success)
        self.assertEqual(res.status, "completed")

        # 1. Verify takeover alert called with correct arguments and direct noVNC URL
        mock_takeover_alert.assert_called_once_with(
            channel="finance",
            screen=0,
            reason="SMS 2-Factor Challenge presented on screen",
            novnc_url="http://100.124.38.17:6080/vnc.html?autoconnect=true",
            reply_to="thread-999",
            relay_url="http://100.124.38.17:3000",
            private_key=None,
        )

        # 2. Verify state machine set into takeover pending
        self.mock_reach_client.request_takeover.assert_called_once_with(
            screen=0,
            reason="SMS 2-Factor Challenge presented on screen",
            novnc_url="http://100.124.38.17:6080/vnc.html?autoconnect=true",
            token="tok-789",
        )

        # 3. Verify polling/wait on GET /agent/screens/0/wait?phase=HumanDone
        self.mock_reach_client.wait_for_phase.assert_called_once_with(
            screen=0,
            phase="HumanDone",
            timeout=600,
        )

        # 4. Verify ack sent to Reach API on handback
        self.mock_reach_client.ack_handback.assert_called_once_with(
            screen=0,
            token="tok-789",
        )

        # 5. Verify "Resuming automated execution..." posted to Buzz thread
        resume_calls = [
            c for c in mock_send_msg.call_args_list
            if "Resuming automated execution..." in c.kwargs.get("content", "")
        ]
        self.assertEqual(len(resume_calls), 1)
        self.assertEqual(resume_calls[0].kwargs["reply_to"], "thread-999")

        # 6. Verify driver resumed to finish goal
        self.assertEqual(self.mock_driver.drive.call_count, 2)

        # 7. Verify screen lease released after completion
        self.mock_reach_client.release_screen.assert_called_once_with(
            screen=0,
            owner="ReachBot",
            token="tok-789",
        )

    @patch("scripts.buzz_daemon.buzz_send_takeover_alert")
    def test_takeover_timeout_handling(self, mock_takeover_alert) -> None:
        """Verify behavior when human wait times out."""
        mock_takeover_alert.return_value = {"ok": True}
        self.mock_reach_client.wait_for_phase.return_value = {
            "status": "timeout",
            "phase": "HumanActive",
        }

        success = self.daemon.handle_takeover(
            channel="ops",
            screen=0,
            reason="Captcha challenge",
            reply_to="thread-1",
            token="tok-1",
        )

        self.assertFalse(success)
        # Should not ack if timed out
        self.mock_reach_client.ack_handback.assert_not_called()


class BuzzDaemonPollingTests(unittest.TestCase):
    """Test channel polling and unseen message handling."""

    @patch("scripts.buzz_daemon.buzz_get_messages")
    def test_poll_once_dispatches_mention(self, mock_get_messages) -> None:
        daemon = BuzzDaemon(channels=["dev-ops"])
        daemon.handle_message = MagicMock(return_value=DriveResult(success=True, status="completed", steps=[]))

        mock_get_messages.return_value = {
            "ok": True,
            "data": [
                {"id": "m1", "channel": "dev-ops", "content": "Just a status update"},
                {"id": "m2", "channel": "dev-ops", "content": "@ReachBot screen:0 run diagnostics"},
            ],
        }

        results = daemon.poll_once()

        self.assertEqual(len(results), 1)
        self.assertEqual(results[0]["message_id"], "m2")
        daemon.handle_message.assert_called_once()

        # Second poll with same message does not dispatch duplicate
        results_second = daemon.poll_once()
        self.assertEqual(len(results_second), 0)


if __name__ == "__main__":
    unittest.main()
