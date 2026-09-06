"""Unit tests for reach-agent-computer Hermes plugin (fake reach REST server, fake PluginContext)."""

import http.server
import json
import os
import sys
import threading
import unittest
from pathlib import Path
from typing import Any, Dict, List
from unittest.mock import MagicMock, patch

PLUGIN_DIR = Path(__file__).parent.resolve()
REPO_ROOT = PLUGIN_DIR.parents[3].resolve()
for p in (str(PLUGIN_DIR), str(REPO_ROOT)):
    if p not in sys.path:
        sys.path.insert(0, p)

import __init__ as plugin  # noqa: E402
from __init__ import (  # noqa: E402
    get_state, on_session_finalize, on_session_start, post_tool_call, pre_llm_call, pre_tool_call,
    reach_drive, reach_lease_screen, reach_release_screen, reach_status, register, reset_state,
)


class FakeReachServer(http.server.HTTPServer):
    def __init__(self) -> None:
        super().__init__(("127.0.0.1", 0), FakeReachHandler)
        self.screens: List[Dict[str, Any]] = [
            {"id": i, "owner": None, "takeover_pending": False, "takeover_url": None, "leased_at": None,
             "novnc_url": f"http://127.0.0.1:{6080 + i}/vnc.html"} for i in (0, 1)
        ]


class FakeReachHandler(http.server.BaseHTTPRequestHandler):
    server: FakeReachServer

    def _screen(self):
        return next((s for s in self.server.screens if s["id"] == int(self.path.split("/")[3])), None)

    def _body(self) -> Dict[str, Any]:
        n = int(self.headers.get("content-length", 0))
        return json.loads(self.rfile.read(n).decode("utf-8") or "{}") if n else {}

    def do_GET(self) -> None:
        if self.path == "/agent/screens":
            self._respond(200, self.server.screens)
        else:
            self._respond(404, {"error": "not found"})

    def do_POST(self) -> None:
        body, screen = self._body(), self._screen() if self.path.startswith("/agent/screens/") else None
        if screen is None:
            return self._respond(404, {"error": "not found"})
        if self.path.endswith("/lease"):
            owner = body.get("owner", "default")
            if screen["owner"] not in (None, owner):
                return self._respond(409, {"error": "occupied"})
            screen["owner"] = owner
            return self._respond(200, {"status": "ok"})
        if self.path.endswith("/takeover"):
            screen["takeover_pending"] = body.get("pending", False)
            screen["takeover_url"] = body.get("url")
            return self._respond(200, {"status": "ok"})
        self._respond(404, {"error": "not found"})

    def do_DELETE(self) -> None:
        body, screen = self._body(), self._screen() if self.path.startswith("/agent/screens/") else None
        if screen is None or not self.path.endswith("/lease"):
            return self._respond(404, {"error": "not found"})
        if body.get("owner") and screen["owner"] != body["owner"]:
            return self._respond(400, {"error": "not owner"})
        screen.update(owner=None, takeover_pending=False, takeover_url=None)
        self._respond(200, {"status": "ok", "released": True})

    def _respond(self, status: int, data: Any) -> None:
        self.send_response(status)
        self.send_header("content-type", "application/json")
        self.end_headers()
        self.wfile.write(json.dumps(data).encode("utf-8"))

    def log_message(self, *args: Any) -> None:
        pass


class FakeHermesContext:
    """Mirrors the PluginContext surface the plugin uses."""

    profile_name = "piper"

    def __init__(self, settings: Dict[str, Any]) -> None:
        self.settings, self.hooks, self.tools = settings, {}, {}

    def get_config(self, key: str, default: Any = None) -> Any:
        return self.settings.get(key, default)

    def register_hook(self, name: str, fn: Any) -> None:
        self.hooks[name] = fn

    def register_tool(self, name: str, toolset: str, schema: dict, handler: Any, **kw: Any) -> None:
        assert schema["name"] == name and "parameters" in schema
        self.tools[name] = handler


class ReachAgentComputerPluginTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls) -> None:
        cls.server = FakeReachServer()
        threading.Thread(target=cls.server.serve_forever, daemon=True).start()
        cls.api_url = f"http://127.0.0.1:{cls.server.server_address[1]}"

    @classmethod
    def tearDownClass(cls) -> None:
        cls.server.shutdown()
        cls.server.server_close()

    def setUp(self) -> None:
        reset_state()
        self.ctx = FakeHermesContext({"api_url": self.api_url})
        register(self.ctx)
        for s in self.server.screens:
            s.update(owner=None, takeover_pending=False, takeover_url=None)

    def tearDown(self) -> None:
        reset_state()
        plugin._ctx = None

    def test_register_attaches_hooks_and_tools(self) -> None:
        self.assertEqual(set(self.ctx.hooks), {"on_session_start", "pre_llm_call", "pre_tool_call",
                                               "post_tool_call", "on_session_finalize"})
        self.assertEqual(set(self.ctx.tools), {"reach_lease_screen", "reach_release_screen", "reach_status", "reach_drive"})
        out = json.loads(self.ctx.tools["reach_status"]({}))  # registry handler convention: handler(args_dict)
        self.assertEqual(out["status"], "ok")

    def test_session_start_leases_for_profile_and_first_turn_announces(self) -> None:
        on_session_start(session_id="s1", model="m", platform="cli")
        self.assertEqual(get_state()["screen"], 0)
        self.assertEqual(self.server.screens[0]["owner"], "piper")
        ctx_msg = pre_llm_call(is_first_turn=True)["context"]
        self.assertIn("screen 0", ctx_msg)
        self.assertIn("http://127.0.0.1:6080/vnc.html", ctx_msg)
        self.assertIsNone(pre_llm_call(is_first_turn=False))

    def test_session_start_when_screens_exhausted_or_api_down(self) -> None:
        for s in self.server.screens:
            s["owner"] = "other_profile"
        on_session_start(session_id="s2")
        self.assertIsNone(get_state()["screen"])
        self.assertIn("No free Agent Computer screen", pre_llm_call(is_first_turn=True)["context"])

        reset_state()
        self.ctx.settings["api_url"] = "http://127.0.0.1:1"  # unreachable
        on_session_start(session_id="s3")
        self.assertIsNone(get_state()["screen"])
        self.assertIn("Agent Computer unavailable", pre_llm_call(is_first_turn=True)["context"])

    def test_pre_tool_call_routes_reach_tools_to_leased_screen(self) -> None:
        on_session_start(session_id="s1")
        self.assertEqual(pre_tool_call(tool_name="mcp__reach__page_text", args={"url": "https://x"}),
                         {"action": "modify", "args": {"screen": 0}})
        self.assertEqual(pre_tool_call(tool_name="reach_drive", args={"goal": "g"}),
                         {"action": "modify", "args": {"screen": 0}})
        self.assertIsNone(pre_tool_call(tool_name="terminal", args={"command": "ls"}))
        self.assertIsNone(pre_tool_call(tool_name="mcp__reach__click", args={"screen": 1, "x": 1, "y": 1}))

    def test_post_tool_call_records_takeover(self) -> None:
        on_session_start(session_id="s1")
        result = json.dumps({"status": "auth_required", "vnc_url": "http://127.0.0.1:6080/vnc.html?autoconnect=1"})
        post_tool_call(tool_name="mcp__reach__auth_handoff", args={"url": "https://login"}, result=result)
        self.assertTrue(self.server.screens[0]["takeover_pending"])
        self.assertEqual(self.server.screens[0]["takeover_url"], "http://127.0.0.1:6080/vnc.html?autoconnect=1")

    def test_finalize_releases_lease(self) -> None:
        on_session_start(session_id="s1")
        on_session_finalize(session_id="s1", platform="cli")
        self.assertIsNone(self.server.screens[0]["owner"])
        self.assertIsNone(get_state()["screen"])

    def test_explicit_lease_release_and_status(self) -> None:
        self.assertEqual(len(reach_status()["screens"]), 2)
        self.assertEqual(reach_lease_screen(screen=1, owner="tester")["screen"], 1)
        self.assertEqual(reach_status(screen=1)["screen"]["owner"], "tester")
        self.assertEqual(reach_lease_screen(screen=1, owner="someone-else")["status"], "conflict")
        self.assertEqual(reach_release_screen(screen=1, owner="tester")["status"], "ok")
        self.assertIsNone(get_state()["screen"])

    @patch("scripts.reach_drive.ReachDriver")
    def test_reach_drive_tool(self, mock_driver_cls: MagicMock) -> None:
        inst = mock_driver_cls.return_value
        inst.drive.return_value.to_dict.return_value = {"success": True, "status": "completed"}
        out = reach_drive(goal="Log in and check dashboard", screen=0)
        self.assertEqual(out["status"], "completed")
        mock_driver_cls.assert_called_once_with(api_url=self.api_url, screen=0, max_steps=15)
        inst.drive.assert_called_once_with(goal="Log in and check dashboard", initial_url=None)


if __name__ == "__main__":
    unittest.main()
