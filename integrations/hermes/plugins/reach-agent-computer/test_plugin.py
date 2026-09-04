"""Unit tests for reach-agent-computer Hermes plugin."""

import http.server
import json
import os
import threading
from typing import Any, Dict, List
import unittest
from unittest.mock import MagicMock, patch

from pathlib import Path
import sys

PLUGIN_DIR = Path(__file__).parent.resolve()
REPO_ROOT = PLUGIN_DIR.parents[3].resolve()

if str(PLUGIN_DIR) not in sys.path:
    sys.path.insert(0, str(PLUGIN_DIR))
if str(REPO_ROOT) not in sys.path:
    sys.path.insert(0, str(REPO_ROOT))

from __init__ import (  # noqa: E402
    get_state,
    on_session_finalize,
    on_session_start,
    post_tool_call,
    pre_tool_call,
    reach_drive,
    reach_lease_screen,
    reach_release_screen,
    reach_status,
    register,
    reset_state,
)


class FakeReachServer(http.server.HTTPServer):
    def __init__(self, host: str = "127.0.0.1", port: int = 0) -> None:
        super().__init__((host, port), FakeReachHandler)
        self.screens: List[Dict[str, Any]] = [
            {
                "id": 0,
                "owner": None,
                "takeover_pending": False,
                "takeover_url": None,
                "leased_at": None,
                "novnc_url": "http://127.0.0.1:6080/vnc.html",
            },
            {
                "id": 1,
                "owner": None,
                "takeover_pending": False,
                "takeover_url": None,
                "leased_at": None,
                "novnc_url": "http://127.0.0.1:6081/vnc.html",
            },
        ]
        self.requests_log: List[Dict[str, Any]] = []


class FakeReachHandler(http.server.BaseHTTPRequestHandler):
    server: FakeReachServer

    def do_GET(self) -> None:
        self.server.requests_log.append({"method": "GET", "path": self.path})
        if self.path == "/agent/screens":
            self._respond_json(200, self.server.screens)
        else:
            self._respond_json(404, {"error": "not found"})

    def do_POST(self) -> None:
        length = int(self.headers.get("content-length", 0))
        body = json.loads(self.rfile.read(length).decode("utf-8") or "{}")
        self.server.requests_log.append(
            {"method": "POST", "path": self.path, "body": body}
        )

        if self.path.startswith("/agent/screens/") and self.path.endswith("/lease"):
            screen_id = int(self.path.split("/")[3])
            owner = body.get("owner", "default")
            screen = next(
                (s for s in self.server.screens if s["id"] == screen_id), None
            )
            if not screen:
                self._respond_json(404, {"error": "screen not found"})
                return
            if screen["owner"] is not None and screen["owner"] != owner:
                self._respond_json(409, {"error": "occupied"})
                return
            screen["owner"] = owner
            self._respond_json(200, {"status": "ok", "id": screen_id, "owner": owner})
        elif self.path.startswith("/agent/screens/") and self.path.endswith(
            "/takeover"
        ):
            screen_id = int(self.path.split("/")[3])
            pending = body.get("pending", False)
            url = body.get("url")
            screen = next(
                (s for s in self.server.screens if s["id"] == screen_id), None
            )
            if screen:
                screen["takeover_pending"] = pending
                screen["takeover_url"] = url
            self._respond_json(200, {"status": "ok", "id": screen_id})
        else:
            self._respond_json(404, {"error": "not found"})

    def do_DELETE(self) -> None:
        length = int(self.headers.get("content-length", 0))
        body = (
            json.loads(self.rfile.read(length).decode("utf-8") or "{}")
            if length > 0
            else {}
        )
        self.server.requests_log.append(
            {"method": "DELETE", "path": self.path, "body": body}
        )

        if self.path.startswith("/agent/screens/") and self.path.endswith("/lease"):
            screen_id = int(self.path.split("/")[3])
            owner = body.get("owner")
            screen = next(
                (s for s in self.server.screens if s["id"] == screen_id), None
            )
            if not screen:
                self._respond_json(404, {"error": "screen not found"})
                return
            if owner and screen["owner"] != owner:
                self._respond_json(400, {"error": "not owner"})
                return
            screen["owner"] = None
            screen["takeover_pending"] = False
            screen["takeover_url"] = None
            self._respond_json(200, {"status": "ok", "id": screen_id, "released": True})
        else:
            self._respond_json(404, {"error": "not found"})

    def _respond_json(self, status: int, data: Any) -> None:
        self.send_response(status)
        self.send_header("content-type", "application/json")
        self.end_headers()
        self.wfile.write(json.dumps(data).encode("utf-8"))

    def log_message(self, format: str, *args: Any) -> None:
        # Silence server log output during test runs
        pass


class FakeHermesContext:
    def __init__(self) -> None:
        self.hooks: Dict[str, Any] = {}
        self.tools: Dict[str, Any] = {}
        self.injected_messages: List[Dict[str, Any]] = []

    def register_hook(self, name: str, fn: Any) -> None:
        self.hooks[name] = fn

    def register_tool(self, name: str, fn: Any, description: str = "") -> None:
        self.tools[name] = fn

    def inject_message(self, text: str, role: str = "user") -> None:
        self.injected_messages.append({"text": text, "role": role})


class ReachAgentComputerPluginTests(unittest.TestCase):
    server: FakeReachServer
    server_thread: threading.Thread
    api_url: str

    @classmethod
    def setUpClass(cls) -> None:
        cls.server = FakeReachServer()
        cls.server_thread = threading.Thread(
            target=cls.server.serve_forever, daemon=True
        )
        cls.server_thread.start()
        port = cls.server.server_address[1]
        cls.api_url = f"http://127.0.0.1:{port}"

    @classmethod
    def tearDownClass(cls) -> None:
        cls.server.shutdown()
        cls.server.server_close()

    def setUp(self) -> None:
        reset_state()
        os.environ["REACH_AGENT_URL"] = self.api_url
        os.environ["HERMES_PROFILE"] = "piper"
        # Reset server screens
        for s in self.server.screens:
            s["owner"] = None
            s["takeover_pending"] = False
            s["takeover_url"] = None
        self.server.requests_log.clear()

    def tearDown(self) -> None:
        reset_state()

    def test_on_session_start_leases_screen_and_notifies(self) -> None:
        ctx = FakeHermesContext()
        on_session_start(ctx, session_id="s1", model="gemini-3.8-flash", platform="cli")

        state = get_state()
        self.assertEqual(state["screen"], 0)
        self.assertEqual(state["owner"], "piper")
        self.assertTrue(len(ctx.injected_messages) > 0)
        msg = ctx.injected_messages[0]["text"]
        self.assertIn("screen 0 leased", msg)
        self.assertIn("http://127.0.0.1:6080/vnc.html", msg)

    def test_on_session_start_when_screens_exhausted(self) -> None:
        # Mark all screens occupied
        for s in self.server.screens:
            s["owner"] = "other_profile"

        ctx = FakeHermesContext()
        on_session_start(ctx, session_id="s2")

        state = get_state()
        self.assertIsNone(state["screen"])
        self.assertTrue(
            any(
                "No free Agent Computer screen" in m["text"]
                for m in ctx.injected_messages
            )
        )

    def test_on_session_start_when_api_down(self) -> None:
        os.environ["REACH_AGENT_URL"] = "http://127.0.0.1:65530"  # unreachable
        ctx = FakeHermesContext()
        on_session_start(ctx, session_id="s3")

        state = get_state()
        self.assertIsNone(state["screen"])
        self.assertTrue(
            any(
                "Agent Computer unavailable" in m["text"] for m in ctx.injected_messages
            )
        )

    def test_pre_tool_call_injects_leased_screen(self) -> None:
        # Pre-set leased screen in state
        ctx = FakeHermesContext()
        on_session_start(ctx, session_id="s1")

        # Reach tool lacking screen argument
        mod = pre_tool_call(
            tool_name="reach_page_text", args={"url": "https://example.com"}
        )
        self.assertEqual(mod, {"modify": {"screen": 0}})

        # Non-reach tool is ignored
        mod_terminal = pre_tool_call(tool_name="terminal", args={"command": "ls"})
        self.assertIsNone(mod_terminal)

        # Reach tool with explicit screen argument is not overridden
        mod_explicit = pre_tool_call(
            tool_name="reach_click", args={"screen": 1, "x": 50, "y": 50}
        )
        self.assertIsNone(mod_explicit)

    def test_post_tool_call_triggers_takeover(self) -> None:
        ctx = FakeHermesContext()
        on_session_start(ctx, session_id="s1")

        auth_result = json.dumps(
            {
                "status": "auth_required",
                "vnc_url": "http://127.0.0.1:6080/vnc.html?autoconnect=1",
            }
        )
        post_tool_call(
            "reach_auth_handoff", {"url": "https://login.example.com"}, auth_result
        )

        screen = self.server.screens[0]
        self.assertTrue(screen["takeover_pending"])
        self.assertEqual(
            screen["takeover_url"], "http://127.0.0.1:6080/vnc.html?autoconnect=1"
        )

    def test_on_session_finalize_releases_lease(self) -> None:
        ctx = FakeHermesContext()
        on_session_start(ctx, session_id="s1")
        self.assertEqual(self.server.screens[0]["owner"], "piper")

        on_session_finalize(session_id="s1")
        self.assertIsNone(self.server.screens[0]["owner"])
        self.assertIsNone(get_state()["screen"])

    def test_tools_lease_release_and_status(self) -> None:
        # Test reach_status initially free
        st = reach_status()
        self.assertEqual(st["status"], "ok")
        self.assertEqual(len(st["screens"]), 2)

        # Lease screen 1 explicitly
        lease_res = reach_lease_screen(screen=1, owner="tester")
        self.assertEqual(lease_res["status"], "ok")
        self.assertEqual(lease_res["screen"], 1)
        self.assertEqual(get_state()["screen"], 1)

        # Check status of screen 1
        st1 = reach_status(screen=1)
        self.assertEqual(st1["status"], "ok")
        self.assertEqual(st1["screen"]["owner"], "tester")

        # Release screen 1
        rel_res = reach_release_screen(screen=1, owner="tester")
        self.assertEqual(rel_res["status"], "ok")
        self.assertIsNone(get_state()["screen"])

    @patch("scripts.reach_drive.ReachDriver")
    def test_reach_drive_tool(self, mock_driver_cls: MagicMock) -> None:
        mock_driver_instance = MagicMock()
        mock_driver_cls.return_value = mock_driver_instance

        mock_result = MagicMock()
        mock_result.to_dict.return_value = {
            "success": True,
            "status": "completed",
            "final_description": "Goal achieved",
            "steps": [],
        }
        mock_driver_instance.drive.return_value = mock_result

        out = reach_drive(goal="Log in and check dashboard", screen=0)
        self.assertEqual(out["status"], "completed")
        self.assertTrue(out["success"])
        mock_driver_instance.drive.assert_called_once_with(
            goal="Log in and check dashboard", initial_url=None
        )

    def test_register_attaches_hooks_and_tools(self) -> None:
        ctx = FakeHermesContext()
        register(ctx)

        self.assertIn("on_session_start", ctx.hooks)
        self.assertIn("pre_tool_call", ctx.hooks)
        self.assertIn("post_tool_call", ctx.hooks)
        self.assertIn("on_session_finalize", ctx.hooks)

        self.assertIn("reach_lease_screen", ctx.tools)
        self.assertIn("reach_release_screen", ctx.tools)
        self.assertIn("reach_drive", ctx.tools)
        self.assertIn("reach_status", ctx.tools)


if __name__ == "__main__":
    unittest.main()
