"""Unit tests for Reach Buzz Agent Daemon (scripts/buzz_daemon.py)."""

import io
import json
import os
from pathlib import Path
import sys
import unittest
import urllib.error
import urllib.request
from unittest.mock import MagicMock, call, patch
from typing import Any, Dict

REPO_ROOT = Path(__file__).parent.parent.resolve()
if str(REPO_ROOT) not in sys.path:
    sys.path.insert(0, str(REPO_ROOT))

from scripts.buzz_daemon import (
    BuzzDaemon,
    ParsedTask,
    ReachApiClient,
    _NoRedirectHandler,
    buzz_get_messages,
    buzz_list_channels,
    buzz_post_visual_diff,
    buzz_send_message,
    buzz_send_takeover_alert,
    parse_task_message,
)
from scripts.reach_drive import DriveResult, ReachAction, ReachDriver, StepRecord


class ViewerProjectionTests(unittest.TestCase):
    def test_api_credentials_never_enter_viewer_messages(self) -> None:
        api_url = "https://operator:api-secret-canary@viewer.example:8443/api?token=api-secret-canary"
        driver = ReachDriver(api_url=api_url, screen=2, enable_audit=False)
        self.assertEqual(driver.get_novnc_url(), "https://viewer.example:8443/viewer/2")
        with patch("scripts.buzz_daemon.buzz_send_message") as send:
            buzz_send_takeover_alert("local-only", 2, "api-secret-canary", api_url=api_url)
        content = send.call_args.kwargs["content"]
        self.assertIn(driver.get_novnc_url(), content)
        self.assertNotIn("api-secret-canary", content)
        self.assertNotIn("operator:", content)


class MessageParsingTests(unittest.TestCase):
    """Test parsing @ReachBot mentions, screen indexes, and goal instructions."""

    def test_simple_mention_default_screen(self) -> None:
        content = '@ReachBot success: "Flights checked"; check flights to SFO'
        task = parse_task_message(content)
        self.assertIsNotNone(task)
        assert task is not None
        self.assertEqual(task.screen, 0)
        self.assertEqual(task.goal, "check flights to SFO")
        self.assertEqual(task.completion_text, "Flights checked")
        self.assertFalse(task.observation_only)

    def test_screen_indicators(self) -> None:
        cases = [
            ('@ReachBot screen 1 success: "done"; order coffee', 1, "order coffee"),
            ('@ReachBot screen:2 success: "done"; checkout cart', 2, "checkout cart"),
            ('@ReachBot screen=3 success: "done"; buy ticket', 3, "buy ticket"),
            ('@ReachBot [screen 4] success: "done"; check notifications', 4, "check notifications"),
            ('@ReachBot --screen 5 success: "done"; approve pull request', 5, "approve pull request"),
            ('@ReachBot display 6 success: "done"; monitor server', 6, "monitor server"),
            ('@ReachBot display:7 success: "done"; status', 7, "status"),
        ]
        for msg, expected_screen, expected_goal in cases:
            with self.subTest(msg=msg):
                task = parse_task_message(msg)
                self.assertIsNotNone(task)
                assert task is not None
                self.assertEqual(task.screen, expected_screen)
                self.assertEqual(task.goal, expected_goal)

    def test_observation_only_contract(self) -> None:
        task = parse_task_message("@ReachBot observation-only inspect screen")
        self.assertIsNotNone(task)
        assert task is not None
        self.assertTrue(task.observation_only)
        self.assertEqual(task.contract_status, "ready")

    def test_url_extraction(self) -> None:
        task1 = parse_task_message(
            '@ReachBot --screen 1 --url https://example.com/login success: "Dashboard"; login to portal'
        )
        self.assertIsNotNone(task1)
        assert task1 is not None
        self.assertEqual(task1.screen, 1)
        self.assertEqual(task1.initial_url, "https://example.com/login")
        self.assertEqual(task1.goal, "login to portal")

        task2 = parse_task_message(
            '@ReachBot success: "Trending"; open https://github.com/trending and inspect'
        )
        self.assertIsNotNone(task2)
        assert task2 is not None
        self.assertEqual(task2.initial_url, "https://github.com/trending")
        self.assertIn("github.com/trending", task2.goal)

    def test_case_insensitive_and_custom_trigger(self) -> None:
        task_lower = parse_task_message('@reachbot success: "done"; perform task')
        self.assertIsNotNone(task_lower)
        assert task_lower is not None
        self.assertEqual(task_lower.goal, "perform task")

        task_custom = parse_task_message(
            '@OpsBot screen:1 success: "Backed up"; backup db', trigger="@OpsBot"
        )
        self.assertIsNotNone(task_custom)
        assert task_custom is not None
        self.assertEqual(task_custom.screen, 1)
        self.assertEqual(task_custom.goal, "backup db")

    def test_non_matching_messages(self) -> None:
        self.assertIsNone(parse_task_message(""))
        self.assertIsNone(parse_task_message("Hello team, check out this link"))
        self.assertIsNone(parse_task_message("@OtherBot run routine"))


class ReachApiClientTests(unittest.TestCase):
    """Test ReachApiClient REST endpoints under the fixed lease protocol."""

    def setUp(self) -> None:
        # Isolate from any host-side supervisor credential.
        env_patcher = patch.dict(os.environ, {"REACH_AUTH_TOKEN": ""})
        env_patcher.start()
        self.addCleanup(env_patcher.stop)
        self.client = ReachApiClient(api_url="http://127.0.0.1:4200")
        self.opener = MagicMock()
        opener_patcher = patch("scripts.buzz_daemon._OPENER", self.opener)
        opener_patcher.start()
        self.addCleanup(opener_patcher.stop)

    def _respond(self, payload: dict) -> MagicMock:
        resp = MagicMock()
        resp.read.return_value = json.dumps(payload).encode("utf-8")
        resp.__enter__.return_value = resp
        self.opener.open.return_value = resp
        return resp

    def _http_error(self, code: int, body: bytes) -> urllib.error.HTTPError:
        return urllib.error.HTTPError(
            "http://127.0.0.1:4200/agent/screens/0/lease",
            code,
            "Error",
            {},
            io.BytesIO(body),
        )

    def test_lease_screen_captures_capability_and_generation(self) -> None:
        self._respond({
            "status": "ok",
            "id": 0,
            "owner": "ReachBot",
            "token": "lease-token-123",
            "handoff_gen": 2,
        })

        res = self.client.lease_screen(0, owner="ReachBot")
        self.assertEqual(res["status"], "ok")
        self.assertEqual(res["token"], "lease-token-123")
        self.assertEqual(self.client.lease_token, "lease-token-123")
        self.assertEqual(self.client.handoff_gen, 2)

        req = self.opener.open.call_args[0][0]
        self.assertEqual(req.get_method(), "POST")
        self.assertIn("/agent/screens/0/lease", req.full_url)
        body = json.loads(req.data.decode("utf-8"))
        self.assertEqual(body["owner"], "ReachBot")
        # Allocation carries no lease capability and, without a configured
        # supervisor credential, no bearer either.
        self.assertIsNone(req.get_header("X-lease-token"))
        self.assertIsNone(req.get_header("Authorization"))

    def test_lease_screen_sends_supervisor_bearer_on_allocation_only(self) -> None:
        client = ReachApiClient(
            api_url="http://127.0.0.1:4200", auth_token="supervisor-secret"
        )
        self._respond({"status": "ok", "id": 0, "token": "tok-1", "handoff_gen": 1})

        client.lease_screen(0, owner="ReachBot")
        req = self.opener.open.call_args[0][0]
        self.assertEqual(req.get_header("Authorization"), "Bearer supervisor-secret")

        client.release_screen(0, owner="ReachBot")
        release_req = self.opener.open.call_args[0][0]
        # Ordinary requests never carry the supervisor credential.
        self.assertIsNone(release_req.get_header("Authorization"))
        self.assertEqual(release_req.get_header("X-lease-token"), "tok-1")

    def test_lease_screen_occupied_fails_closed_without_reallocation(self) -> None:
        self.opener.open.side_effect = self._http_error(
            409, b'{"error": "screen already leased"}'
        )

        with self.assertRaises(RuntimeError):
            self.client.lease_screen(0, owner="ReachBot")
        # Creation-only: exactly one allocation attempt, no same-owner retry.
        self.assertEqual(self.opener.open.call_count, 1)
        self.assertIsNone(self.client.lease_token)
        self.assertIsNone(self.client.handoff_gen)

    def test_release_screen_uses_header_capability_and_clears_on_success(self) -> None:
        self.client.lease_token = "lease-token-123"
        self.client.handoff_gen = 2
        self._respond({"status": "ok", "id": 0, "released": True})

        res = self.client.release_screen(0, owner="ReachBot")
        self.assertEqual(res["status"], "ok")
        self.assertTrue(res["released"])

        req = self.opener.open.call_args[0][0]
        self.assertEqual(req.get_method(), "DELETE")
        self.assertEqual(req.get_header("X-lease-token"), "lease-token-123")
        body = json.loads(req.data.decode("utf-8"))
        self.assertEqual(body["owner"], "ReachBot")
        self.assertNotIn("token", body)

        # Capability discarded only after a confirmed release.
        self.assertIsNone(self.client.lease_token)
        self.assertIsNone(self.client.handoff_gen)

    def test_release_failure_retains_capability(self) -> None:
        self.client.lease_token = "lease-token-123"
        self.client.handoff_gen = 2
        self.opener.open.side_effect = self._http_error(
            409, b'{"error": "screen busy: human active"}'
        )

        res = self.client.release_screen(0, owner="ReachBot")
        self.assertIn("error", res)
        # HumanActive refusal must not clear local lease state.
        self.assertEqual(self.client.lease_token, "lease-token-123")
        self.assertEqual(self.client.handoff_gen, 2)

    def test_request_takeover_sends_capability_without_adopting_generation(self) -> None:
        self.client.lease_token = "lease-token-123"
        self.client.handoff_gen = 3
        self._respond({"status": "ok", "phase": "HandoffPending", "handoff_gen": 9})

        res = self.client.request_takeover(0, reason="SMS 2FA Required")
        self.assertEqual(res["status"], "ok")

        req = self.opener.open.call_args[0][0]
        self.assertEqual(req.get_method(), "POST")
        self.assertEqual(req.get_header("X-lease-token"), "lease-token-123")
        body = json.loads(req.data.decode("utf-8"))
        self.assertTrue(body["pending"])
        self.assertEqual(body["reason"], "SMS 2FA Required")
        self.assertNotIn("url", body)
        # Status generation is observed, never adopted.
        self.assertEqual(self.client.handoff_gen, 3)

    def test_lease_transport_failure_requires_reconciliation(self) -> None:
        self.opener.open.side_effect = TimeoutError("connection timed out")

        with self.assertRaisesRegex(RuntimeError, "uncertain"):
            self.client.lease_screen(0, owner="ReachBot")

        self.assertEqual(self.client.last_lease_cleanup, {"status": "uncertain"})
        self.assertIsNone(self.client.lease_token)
        self.assertIsNone(self.client.handoff_gen)
        self.assertEqual(self.opener.open.call_count, 1)

    def test_wait_for_phase_observes_without_adopting_generation(self) -> None:
        self.client.handoff_gen = 3
        self._respond({"status": "ok", "phase": "HumanDone", "handoff_gen": 42})

        res = self.client.wait_for_phase(0, phase="HumanDone", timeout=60)
        self.assertEqual(res["status"], "ok")
        self.assertEqual(res["phase"], "HumanDone")
        self.assertEqual(self.client.handoff_gen, 3)

        req = self.opener.open.call_args[0][0]
        self.assertEqual(req.get_method(), "GET")
        self.assertIn("/agent/screens/0/wait?phase=HumanDone&timeout=60", req.full_url)

    def test_ack_handback_adopts_new_generation(self) -> None:
        self.client.lease_token = "lease-token-123"
        self.client.handoff_gen = 3
        self._respond({"status": "ok", "phase": "AgentActive", "handoff_gen": 4})

        res = self.client.ack_handback(0)
        self.assertEqual(res["status"], "ok")
        self.assertEqual(res["phase"], "AgentActive")

        req = self.opener.open.call_args[0][0]
        self.assertEqual(req.get_method(), "POST")
        self.assertEqual(req.get_header("X-lease-token"), "lease-token-123")
        self.assertEqual(self.client.handoff_gen, 4)

    def test_capability_requests_refuse_redirects(self) -> None:
        handler = _NoRedirectHandler()
        req = urllib.request.Request("http://127.0.0.1:4200/agent/screens/0/lease")
        with self.assertRaises(urllib.error.HTTPError):
            handler.redirect_request(
                req, io.BytesIO(b""), 302, "Found", {}, "http://evil.example/steal"
            )

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
        self.mock_reach_client.handoff_gen = 5

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
            "content": '@ReachBot screen:0 success: "Dashboard"; sign in to admin portal',
            "created_at": 1725500000,
        }

        with self.assertLogs("reach_buzz_daemon", level="DEBUG") as logs:
            result = self.daemon.handle_message(incoming_msg)
        # The minted lease capability must never appear in logs or chat output.
        self.assertNotIn("tok-456", "\n".join(logs.output))

        self.assertIsNotNone(result)
        assert result is not None
        self.assertTrue(result.success)

        # 1. Immediate acknowledgment posted to Buzz thread
        first_send = mock_send_msg.call_args_list[0]
        self.assertEqual(first_send.kwargs["channel"], "ops-channel")
        self.assertEqual(first_send.kwargs["reply_to"], "msg-001")
        self.assertIn("Task contract received", first_send.kwargs["content"])

        # 2. Leased screen 0 from Reach API
        self.mock_reach_client.lease_screen.assert_called_once_with(0, owner="ReachBot")

        # 3. Driving loop invoked with token and callback
        self.mock_driver_factory.assert_called_once()
        self.assertEqual(self.mock_driver_factory.call_args[1]["screen"], 0)
        self.assertEqual(self.mock_driver_factory.call_args[1]["lease_token"], "tok-456")
        self.assertEqual(self.mock_driver_factory.call_args[1]["task_id"], "msg-001")
        self.assertEqual(self.mock_driver_factory.call_args[1]["attempt_id"], "1")
        self.assertEqual(self.mock_driver_factory.call_args[1]["completion_text"], "Dashboard")

        # Visual diffs contain only fixed structured metadata; no raw capture path
        # or model-supplied description crosses the Buzz boundary.
        mock_post_diff.assert_called_once()
        self.assertEqual(mock_post_diff.call_args.kwargs["channel"], "ops-channel")
        self.assertEqual(mock_post_diff.call_args.kwargs["reply_to"], "msg-001")
        self.assertNotIn("screenshot_path", mock_post_diff.call_args.kwargs)
        self.assertEqual(mock_post_diff.call_args.kwargs["action_kind"], "click")
        self.assertEqual(mock_post_diff.call_args.kwargs["outcome"], "completed")
        self.assertAlmostEqual(mock_post_diff.call_args.kwargs["diff_percent"], 25.0)

        # 5. Released screen lease
        self.mock_reach_client.release_screen.assert_called_once_with(
            screen=0, owner="ReachBot", token="tok-456"
        )

        # 6. Final summary contains fixed outcome/cleanup metadata only.
        last_send = mock_send_msg.call_args_list[-1]
        self.assertEqual(last_send.kwargs["channel"], "ops-channel")
        self.assertEqual(last_send.kwargs["reply_to"], "msg-001")
        self.assertIn("✅ **Reach Task Completed**", last_send.kwargs["content"])
        self.assertIn("Lease Cleanup", last_send.kwargs["content"])
        self.assertNotIn("Successfully logged in and reached dashboard", last_send.kwargs["content"])
        self.assertNotIn("/srv/reach/audits/task-123/index.html", last_send.kwargs["content"])
        self.assertNotIn("tok-456", last_send.kwargs["content"])

class TakeoverAndHandbackTests(unittest.TestCase):
    """Test 2FA/CAPTCHA takeover alert emission, waiting, ack handback, and execution resumption."""

    def setUp(self) -> None:
        self.mock_reach_client = MagicMock(spec=ReachApiClient)
        self.mock_reach_client.lease_screen.return_value = {"status": "ok", "token": "tok-789"}
        self.mock_reach_client.release_screen.return_value = {"status": "ok"}
        self.mock_reach_client.handoff_gen = 9
        self.mock_reach_client.wait_for_phase.return_value = {
            "status": "ok",
            "phase": "HumanDone",
            "id": 0,
        }
        self.mock_reach_client.ack_handback.return_value = {
            "status": "ok",
            "phase": "AgentActive",
            "handoff_gen": 10,
        }

        def install_ack_generation(*args: Any, **kwargs: Any) -> Dict[str, Any]:
            self.mock_reach_client.handoff_gen = 10
            return {
                "status": "ok",
                "phase": "AgentActive",
                "handoff_gen": 10,
            }

        self.mock_reach_client.ack_handback.side_effect = install_ack_generation

        self.mock_reach_client.request_takeover.return_value = {
            "status": "ok",
            "phase": "HandoffPending",
        }
        self.mock_driver = MagicMock()
        self.mock_driver_factory = MagicMock(return_value=self.mock_driver)

        self.daemon = BuzzDaemon(
            relay_url="http://100.124.38.17:3000",
            api_url="http://127.0.0.1:4200",
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
            "content": '@ReachBot screen:0 success: "Transfer complete"; transfer funds to vendor',
        }

        res = self.daemon.handle_message(incoming_msg)

        self.assertIsNotNone(res)
        assert res is not None
        self.assertTrue(res.success)
        self.assertEqual(res.status, "completed")

        # The driver-supplied final description is private metadata; the Buzz
        # takeover projection must use a fixed safe reason instead.
        takeover_call = mock_takeover_alert.call_args
        self.assertIsNotNone(takeover_call)
        assert takeover_call is not None
        self.assertEqual(takeover_call.kwargs["channel"], "finance")
        self.assertEqual(takeover_call.kwargs["screen"], 0)
        self.assertNotEqual(
            takeover_call.kwargs["reason"],
            initial_result.final_description,
        )
        self.assertNotIn(
            initial_result.final_description,
            takeover_call.kwargs["reason"],
        )
        self.assertEqual(takeover_call.kwargs["api_url"], "http://127.0.0.1:4200")
        self.assertNotIn("novnc_url", takeover_call.kwargs)
        self.assertNotIn("100.124.38.17:6080", takeover_call.kwargs)
        self.assertEqual(takeover_call.kwargs["reply_to"], "thread-999")
        self.assertEqual(
            takeover_call.kwargs["relay_url"],
            "http://100.124.38.17:3000",
        )
        self.assertIsNone(takeover_call.kwargs["private_key"])

        # Reach receives the same bounded, non-sensitive takeover reason; the
        # driver narrative is never promoted into an operator-facing message.
        request_call = self.mock_reach_client.request_takeover.call_args
        self.assertIsNotNone(request_call)
        assert request_call is not None
        self.assertEqual(request_call.kwargs["screen"], 0)
        self.assertNotEqual(
            request_call.kwargs["reason"],
            initial_result.final_description,
        )
        self.assertNotIn(
            initial_result.final_description,
            request_call.kwargs["reason"],
        )
        self.assertNotIn("novnc_url", request_call.kwargs)
        self.assertEqual(request_call.kwargs["token"], "tok-789")

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

        # Driver factory receives the generation retained after validated ack.
        gen_kwargs = [
            c.kwargs.get("handoff_gen") for c in self.mock_driver_factory.call_args_list
        ]
        self.assertEqual(gen_kwargs, [9, 10])

        # 7. Verify screen lease released after completion
        self.mock_reach_client.release_screen.assert_called_once_with(
            screen=0,
            owner="ReachBot",
            token="tok-789",
        )

        # The lease capability must never leak into Buzz chat output.
        self.assertNotIn(
            "tok-789", mock_send_msg.call_args_list[-1].kwargs["content"]
        )
        buzz_output = "\n".join(
            call.kwargs["content"] for call in mock_send_msg.call_args_list
        )
        for private_value in (
            initial_result.final_description,
            resumed_result.final_description,
            resumed_result.audit_report_path,
            "transfer funds to vendor",
        ):
            self.assertNotIn(private_value, buzz_output)


    @patch("scripts.buzz_daemon.buzz_send_message")
    @patch("scripts.buzz_daemon.buzz_send_takeover_alert")
    def test_failed_ack_does_not_announce_resume(
        self, mock_takeover_alert, mock_send_msg
    ) -> None:
        mock_takeover_alert.return_value = {"ok": True}
        mock_send_msg.return_value = {"ok": True}
        self.mock_reach_client.ack_handback.side_effect = None
        self.mock_reach_client.ack_handback.return_value = {
            "status": "error",
            "phase": "HumanDone",
            "handoff_gen": 10,
        }

        success = self.daemon.handle_takeover(
            channel="ops",
            screen=0,
            reason="Captcha challenge",
            reply_to="thread-2",
            token="tok-1",
        )

        self.assertFalse(success)
        self.mock_reach_client.ack_handback.assert_called_once_with(
            screen=0,
            token="tok-1",
        )
        self.assertFalse(
            any(
                "Resuming automated execution..." in call.kwargs.get("content", "")
                for call in mock_send_msg.call_args_list
            )
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
                {"id": "m2", "channel": "dev-ops", "content": '@ReachBot screen:0 observation-only run diagnostics'},
            ],
        }

        results = daemon.poll_once()

        self.assertEqual(len(results), 1)
        self.assertEqual(results[0]["message_id"], "m2")
        daemon.handle_message.assert_called_once()

        # Second poll with same message does not dispatch duplicate
        results_second = daemon.poll_once()
        self.assertEqual(len(results_second), 0)


class BuzzDaemonSecurityTests(unittest.TestCase):
    """Test sender allowlist and mutating tool restriction on chat-initiated goals."""

    def setUp(self) -> None:
        self.mock_reach_client = MagicMock()
        self.mock_reach_client.lease_screen.return_value = {"token": "test-tok"}
        self.daemon = BuzzDaemon(
            allowed_senders=["alice", "bob"],
            reach_client=self.mock_reach_client,
        )

    @patch("scripts.buzz_daemon.buzz_send_message")
    def test_unauthorized_sender_rejected(self, mock_send) -> None:
        msg = {
            "id": "msg-101",
            "channel": "dev",
            "sender": "mallory",
            "content": '@ReachBot success: "Rust docs visible"; search for rust docs',
        }
        res = self.daemon.handle_message(msg)
        self.assertIsNone(res)
        self.mock_reach_client.lease_screen.assert_not_called()
        mock_send.assert_called_once()
        self.assertIn("Unauthorized sender", mock_send.call_args[1]["content"])

    @patch("scripts.buzz_daemon.buzz_send_message")
    def test_authorized_sender_accepted(self, mock_send) -> None:
        driver_mock = MagicMock()
        driver_mock.drive.return_value = DriveResult(success=True, status="completed", steps=[])
        self.daemon.driver_factory = MagicMock(return_value=driver_mock)

        msg = {
            "id": "msg-102",
            "channel": "dev",
            "sender": "Alice",
            "content": '@ReachBot success: "Rust docs visible"; search for rust docs',
        }
        res = self.daemon.handle_message(msg)
        self.assertIsNotNone(res)
        self.mock_reach_client.lease_screen.assert_called_once()

    @patch("scripts.buzz_daemon.buzz_send_message")
    def test_mutating_chat_goals_rejected(self, mock_send) -> None:
        mutating_goals = [
            '@ReachBot success: "Command finished"; exec rm -rf /workspace',
            '@ReachBot success: "Card injected"; inject card for checkout',
            '@ReachBot success: "Logged in"; use vault credentials to login',
            '@ReachBot success: "Paid"; enter credit card on payment page',
        ]
        for goal_msg in mutating_goals:
            with self.subTest(goal=goal_msg):
                msg = {
                    "id": f"msg-{abs(hash(goal_msg))}",
                    "channel": "dev",
                    "sender": "alice",
                    "content": goal_msg,
                }
                res = self.daemon.handle_message(msg)
                self.assertIsNone(res)
                self.mock_reach_client.lease_screen.assert_not_called()
                self.assertIn(
                    "Mutating tools",
                    mock_send.call_args[1]["content"],
                )
class BuzzTaskContractTests(unittest.TestCase):
    def setUp(self) -> None:
        self.reach = MagicMock(spec=ReachApiClient)
        self.reach.lease_screen.return_value = {"status": "ok", "token": "lease"}
        self.reach.release_screen.return_value = {"status": "ok"}
        self.reach.handoff_gen = 3
        self.driver = MagicMock()
        self.factory = MagicMock(return_value=self.driver)
        self.daemon = BuzzDaemon(
            reach_client=self.reach,
            driver_factory=self.factory,
            allowed_senders=["alice"],
        )

    @patch("scripts.buzz_daemon.buzz_send_message")
    def test_ambiguous_free_text_requires_clarification_without_dispatch(self, send) -> None:
        result = self.daemon.handle_message(
            {
                "id": "ambiguous-1",
                "sender": "alice",
                "channel": "ops",
                "content": "@ReachBot sign in to the portal",
            }
        )
        self.assertIsNotNone(result)
        assert result is not None
        self.assertEqual(result.status, "clarification_required")
        self.reach.lease_screen.assert_not_called()
        self.factory.assert_not_called()
        self.assertIn("Clarification Required", send.call_args.kwargs["content"])

    @patch("scripts.buzz_daemon.buzz_send_message")
    def test_chat_approve_text_does_not_grant_execution_consent(self, send) -> None:
        self.driver.drive.return_value = DriveResult(
            success=False, status="approval_required", steps=[]
        )
        result = self.daemon.handle_message(
            {
                "id": "approve-1",
                "sender": "alice",
                "channel": "ops",
                "content": '@ReachBot success: "Change applied"; approve the pending change',
            }
        )
        self.assertIsNotNone(result)
        assert result is not None
        self.assertEqual(result.status, "approval_required")
        self.assertFalse(result.success)
        self.assertNotIn("Approved", send.call_args_list[-1].kwargs["content"])

    @patch("scripts.buzz_daemon.buzz_send_message")
    def test_uncertain_result_is_not_automatically_replayed(self, send) -> None:
        self.driver.drive.return_value = DriveResult(
            success=False, status="uncertain", steps=[]
        )
        result = self.daemon.handle_message(
            {
                "id": "uncertain-1",
                "attempt_id": "attempt-7",
                "sender": "alice",
                "channel": "ops",
                "content": '@ReachBot success: "Saved"; save the draft',
            }
        )
        self.assertIsNotNone(result)
        assert result is not None
        self.assertEqual(result.status, "uncertain")
        self.factory.assert_called_once()
        self.assertEqual(self.factory.call_args.kwargs["task_id"], "uncertain-1")
        self.assertEqual(self.factory.call_args.kwargs["attempt_id"], "attempt-7")
        self.assertNotIn("completed", send.call_args_list[-1].kwargs["content"].lower())

    @patch("scripts.buzz_daemon.buzz_send_message")
    def test_model_receipts_are_projected_or_explicitly_unknown(self, send) -> None:
        self.driver.drive.return_value = DriveResult(
            success=False,
            status="failed",
            steps=[],
            metrics={
                "model_requested": "requested-model",
                "model_reported": "reported-model",
                "model_version": "v-test",
            },
        )
        self.daemon.handle_message(
            {
                "id": "metrics-1",
                "sender": "alice",
                "channel": "ops",

                "content": '@ReachBot success: "Saved"; save the draft',
            }
        )
        content = send.call_args_list[-1].kwargs["content"]
        self.assertIn("requested-model", content)
        self.assertIn("reported-model", content)
        self.assertIn("v-test", content)
    def test_missing_model_receipts_are_explicitly_unknown(self) -> None:
        requested, reported, version = self.daemon._metric_receipts(
            DriveResult(success=False, status="failed", steps=[])
        )
        self.assertEqual((requested, reported, version), ("unknown", "unknown", "unknown"))



class TruthfulOutcomeTests(unittest.TestCase):
    """New terminal states are reported verbatim, never converted to completed."""

    def _make_daemon(self, status: str):
        mock_reach_client = MagicMock(spec=ReachApiClient)
        mock_reach_client.lease_screen.return_value = {
            "status": "ok",
            "id": 0,
            "token": "tok-x",
        }
        mock_reach_client.release_screen.return_value = {"status": "ok", "released": True}
        mock_reach_client.handoff_gen = 1
        mock_driver = MagicMock()
        mock_driver.drive.return_value = DriveResult(
            success=False,
            status=status,
            steps=[],
            final_description=f"Driver ended in {status}",
        )
        daemon = BuzzDaemon(
            relay_url="http://100.124.38.17:3000",
            reach_client=mock_reach_client,
            driver_factory=MagicMock(return_value=mock_driver),
        )
        return daemon, mock_reach_client

    @patch("scripts.buzz_daemon.buzz_send_message")
    def test_non_completed_states_reported_verbatim(self, mock_send) -> None:
        mock_send.return_value = {"ok": True}
        for status in ("unverified", "blocked", "postcondition_failed"):
            with self.subTest(status=status):
                daemon, reach_client = self._make_daemon(status)
                with self.assertLogs("reach_buzz_daemon", level="DEBUG"):
                    result = daemon.handle_message({
                        "id": f"msg-{status}",
                        "channel": "ops",
                        "content": '@ReachBot screen:0 success: "Done"; do a thing',
                    })
                self.assertIsNotNone(result)
                assert result is not None
                self.assertEqual(result.status, status)
                self.assertFalse(result.success)
                # Release still attempted with the retained capability.
                reach_client.release_screen.assert_called_once_with(
                    screen=0, owner="ReachBot", token="tok-x"
                )
                last_send = mock_send.call_args_list[-1]
                self.assertIn("⚠️", last_send.kwargs["content"])
                self.assertNotIn("✅", last_send.kwargs["content"])
                self.assertIn(status.replace("_", " ").title(), last_send.kwargs["content"])
                # The lease capability never leaks into chat output.
                self.assertNotIn("tok-x", last_send.kwargs["content"])


if __name__ == "__main__":
    unittest.main()
