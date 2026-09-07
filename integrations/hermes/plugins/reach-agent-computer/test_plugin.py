"""Unit tests for reach-agent-computer Hermes plugin.

The fake reach server enforces the Phase-1 authority protocol for real:
bearer-or-lease authentication, creation-only leases (same-owner recovery
rejected), lease-scoped capability paths, and X-Handoff-Gen required on every
leased /tools and /mcp primitive (screenshot and live_view included).
"""

import http.server
import json
import os
import sys
import threading
import unittest
import urllib.error
from pathlib import Path
from typing import Any, Dict, List, Optional, Tuple
from unittest.mock import MagicMock, patch

PLUGIN_DIR = Path(__file__).parent.resolve()
REPO_ROOT = PLUGIN_DIR.parents[3].resolve()
for p in (str(PLUGIN_DIR), str(REPO_ROOT)):
    if p not in sys.path:
        sys.path.insert(0, p)

import __init__ as plugin  # noqa: E402
from __init__ import (  # noqa: E402
    STANDALONE_SESSION, get_state, on_session_finalize, on_session_start,
    post_tool_call, pre_llm_call, pre_tool_call, reach_drive, reach_lease_screen,
    reach_release_screen, reach_smart_browse, reach_status, reach_tool, register, reset_state,
)


class FakeReachServer(http.server.HTTPServer):
    """Fake reach host daemon with real capability enforcement."""

    def __init__(self) -> None:
        super().__init__(("127.0.0.1", 0), FakeReachHandler)
        self.auth_token = "test-supervisor-token"
        self.screens: List[Dict[str, Any]] = [
            {"id": i, "owner": None, "lease_token": None, "handoff_gen": 1 + i,
             "phase": "Idle", "busy": False, "takeover_pending": False,
             "takeover_url": None, "leased_at": None,
             "novnc_url": f"http://127.0.0.1:{6080 + i}/vnc.html"}
            for i in (0, 1)
        ]
        self.tool_calls: List[Dict[str, Any]] = []  # observed primitives (no token values)
        self.request_count = 0
        self.redirects_followed = 0
        self._lease_seq = 0

    def reset(self) -> None:
        for s in self.screens:
            s.update(owner=None, lease_token=None, phase="Idle", busy=False,
                     takeover_pending=False, takeover_url=None, leased_at=None,
                     handoff_gen=1 + s["id"])
        self.tool_calls.clear()
        self.request_count = 0
        self.redirects_followed = 0


class FakeReachHandler(http.server.BaseHTTPRequestHandler):
    server: FakeReachServer

    # -- helpers ------------------------------------------------------------

    def _screen(self) -> Optional[Dict[str, Any]]:
        try:
            sid = int(self.path.split("/")[3])
        except (IndexError, ValueError):
            return None
        return next((s for s in self.server.screens if s["id"] == sid), None)

    def _body(self) -> Dict[str, Any]:
        n = int(self.headers.get("content-length", 0))
        return json.loads(self.rfile.read(n).decode("utf-8") or "{}") if n else {}

    def _auth(self) -> Tuple[str, Optional[Dict[str, Any]]]:
        """Resolve caller: ('lease', screen) | ('supervisor', None) | ('none', None)."""
        self.server.request_count += 1
        tok = self.headers.get("X-Lease-Token")
        if tok:
            for s in self.server.screens:
                if s["lease_token"] == tok:
                    return "lease", s
            return "none", None
        if self.headers.get("Authorization") == f"Bearer {self.server.auth_token}":
            return "supervisor", None
        return "none", None

    def _lease_scoped_ok(self, screen: Dict[str, Any], kind: str, lease_screen: Optional[Dict[str, Any]]) -> bool:
        """Lease tokens may only touch their own screen (plus list / tools / mcp)."""
        if kind != "lease" or lease_screen is None:
            return True
        return screen is not None and screen["id"] == lease_screen["id"]

    def _screen_for_args(self, args: Dict[str, Any]) -> Dict[str, Any]:
        sid = args.get("screen", 0)
        return next((s for s in self.server.screens if s["id"] == sid), self.server.screens[0])

    def _validate_tool_auth(self, args: Dict[str, Any]) -> Optional[Tuple[int, Dict[str, Any]]]:
        """Full Phase-1 enforcement for /tools and /mcp primitives."""
        kind, lease_screen = self._auth()
        if kind == "none":
            return 401, {"error": "unauthorized: valid Bearer or X-Lease-Token required"}
        screen = self._screen_for_args(args)
        if not self._lease_scoped_ok(screen, kind, lease_screen):
            return 403, {"error": "invalid or out-of-scope lease capability"}
        if screen["lease_token"] is not None:
            if kind != "lease" or lease_screen["id"] != screen["id"]:
                return 403, {"error": "forbidden: invalid or missing X-Lease-Token for leased screen"}
            gen = self.headers.get("X-Handoff-Gen")
            if gen is None:
                return 409, {"error": "missing_handoff_gen", "expected_gen": screen["handoff_gen"]}
            try:
                if int(gen) != screen["handoff_gen"]:
                    return 409, {"error": "stale_plan", "expected_gen": screen["handoff_gen"],
                                 "provided_gen": int(gen)}
            except ValueError:
                return 400, {"error": "invalid X-Handoff-Gen header"}
        if screen["phase"] not in ("AgentActive", "Idle"):
            return 409, {"error": "takeover_active", "phase": screen["phase"],
                         "handoff_gen": screen["handoff_gen"]}
        return None

    def _record(self, name: str, args: Dict[str, Any], status: int) -> None:
        self.server.tool_calls.append({
            "tool": name, "screen": args.get("screen", 0), "status": status,
            "gen": self.headers.get("X-Handoff-Gen"),
            "had_lease_token": self.headers.get("X-Lease-Token") is not None,
        })

    # -- routes -------------------------------------------------------------

    def do_GET(self) -> None:
        kind, lease_screen = self._auth()
        if kind == "none":
            return self._respond(401, {"error": "unauthorized"})
        if self.path == "/redirect":
            # 302 loop bait: token-bearing clients must fail closed, not follow.
            self.send_response(302)
            self.send_header("Location", "/redirect-destination")
            self.end_headers()
            return
        if self.path == "/redirect-destination":
            self.server.redirects_followed += 1
            return self._respond(200, {"status": "unexpected redirect follow"})
        if self.path == "/agent/screens":
            return self._respond(200, self.server.screens)

        self._respond(404, {"error": "not found"})

    def do_POST(self) -> None:
        body = self._body()
        if self.path == "/mcp":
            err = self._validate_tool_auth(body.get("params", {}).get("arguments", {}))
            if err:
                return self._respond(*err)
            name = body.get("params", {}).get("name", "")
            args = body.get("params", {}).get("arguments", {})
            self._record(name, args, 200)
            if name == "auth_handoff":
                screen = self._screen_for_args(args)
                screen.update(
                    takeover_pending=True,
                    takeover_url=screen["novnc_url"],
                    phase="HandoffPending",
                    handoff_gen=screen["handoff_gen"] + 1,
                )
                handoff = {
                    "status": "auth_required",
                    "vnc_url": screen["novnc_url"],
                    "phase": screen["phase"],
                    "handoff_gen": screen["handoff_gen"],
                }
                text = json.dumps(handoff)
            else:
                text = json.dumps(
                    {"title": "Mock Title", "status": "ok", "url": args.get("url", ""),
                     "elements_count": 42})
            return self._respond(200, {
                "jsonrpc": "2.0", "id": body.get("id", 1),
                "result": {"content": [{"type": "text", "text": text}], "isError": False},
            })
        if self.path.startswith("/tools/"):
            err = self._validate_tool_auth(body)
            if err:
                self._record(self.path.split("/")[2], body, err[0])
                return self._respond(*err)
            name = self.path.split("/")[2]
            self._record(name, body, 200)
            if name == "auth_handoff":
                screen = self._screen_for_args(body)
                screen.update(
                    takeover_pending=True,
                    takeover_url=screen["novnc_url"],
                    phase="HandoffPending",
                    handoff_gen=screen["handoff_gen"] + 1,
                )
                text = json.dumps({
                    "status": "auth_required",
                    "vnc_url": screen["novnc_url"],
                    "phase": screen["phase"],
                    "handoff_gen": screen["handoff_gen"],
                })
                return self._respond(200, {
                    "content": [{"type": "text", "text": text}], "isError": False,
                })
            return self._respond(200, {"status": "ok", "tool": name, "echo": body})
        kind, lease_screen = self._auth()
        if kind == "none":
            return self._respond(401, {"error": "unauthorized"})
        screen = self._screen() if self.path.startswith("/agent/screens/") else None
        if screen is None:
            return self._respond(404, {"error": "not found"})
        if self.path.endswith("/lease"):
            if kind != "supervisor":
                return self._respond(403, {"error": "lease allocation requires supervisor bearer"})
            if screen["owner"] is not None:
                # Creation-only: same-owner recovery never re-issues a token.
                return self._respond(409, {"error": "already_leased",
                                           "message": f"screen {screen['id']} is already owned"})
            self.server._lease_seq += 1
            screen.update(owner=body.get("owner", "default"),
                          lease_token=f"tok-{screen['id']}-{self.server._lease_seq}",
                          phase="AgentActive", leased_at="now")
            return self._respond(200, {"status": "ok", "id": screen["id"],
                                       "owner": screen["owner"],
                                       "token": screen["lease_token"],
                                       "handoff_gen": screen["handoff_gen"]})
        if self.path.endswith("/takeover"):
            if not self._lease_scoped_ok(screen, kind, lease_screen):
                return self._respond(403, {"error": "invalid or out-of-scope lease capability"})
            if screen["lease_token"] is not None:
                if kind != "lease" or lease_screen["id"] != screen["id"]:
                    return self._respond(403, {"error": "forbidden: invalid or missing X-Lease-Token"})
            screen.update(takeover_pending=body.get("pending", False),
                          takeover_url=body.get("url"), phase="HandoffPending")
            return self._respond(200, {"status": "ok", "id": screen["id"],
                                       "phase": screen["phase"],
                                       "handoff_gen": screen["handoff_gen"]})
        if self.path.endswith("/ack"):
            if not self._lease_scoped_ok(screen, kind, lease_screen):
                return self._respond(403, {"error": "invalid or out-of-scope lease capability"})
            if screen["lease_token"] is not None:
                if kind != "lease" or lease_screen["id"] != screen["id"]:
                    return self._respond(403, {"error": "forbidden: invalid or missing X-Lease-Token"})
            if screen["phase"] != "HumanDone":
                return self._respond(409, {"error": "invalid_phase", "phase": screen["phase"]})
            screen.update(phase="AgentActive", handoff_gen=screen["handoff_gen"] + 1)
            return self._respond(200, {"status": "ok", "id": screen["id"],
                                       "phase": screen["phase"],
                                       "handoff_gen": screen["handoff_gen"]})
        self._respond(404, {"error": "not found"})

    def do_DELETE(self) -> None:
        self._body()
        kind, lease_screen = self._auth()
        if kind == "none":
            return self._respond(401, {"error": "unauthorized"})
        screen = self._screen() if self.path.startswith("/agent/screens/") else None
        if screen is None or not self.path.endswith("/lease"):
            return self._respond(404, {"error": "not found"})
        if not self._lease_scoped_ok(screen, kind, lease_screen):
            return self._respond(403, {"error": "invalid or out-of-scope lease capability"})
        if screen["lease_token"] is None:
            return self._respond(400, {"error": "screen is not leased"})
        if kind != "lease" or lease_screen["id"] != screen["id"]:
            return self._respond(403, {"error": "forbidden: release requires the lease token"})
        if screen["phase"] == "HumanActive":
            return self._respond(409, {"error": "human_active",
                                       "phase": "HumanActive"})
        if screen["busy"]:
            return self._respond(409, {"error": "screen_busy"})
        screen.update(owner=None, lease_token=None, phase="Idle", leased_at=None,
                      takeover_pending=False, takeover_url=None,
                      handoff_gen=screen["handoff_gen"] + 1)
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


def _call(ctx: FakeHermesContext, tool: str, args: Dict[str, Any],
          session_id: Optional[str] = "s1") -> Dict[str, Any]:
    """Invoke a registered plugin tool through the (real-shape) handler contract."""
    kwargs: Dict[str, Any] = {} if session_id is None else {"session_id": session_id}
    return json.loads(ctx.tools[tool](args, **kwargs))


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
        self.server.reset()
        os.environ["REACH_AUTH_TOKEN"] = self.server.auth_token
        os.environ.pop("HERMES_PROFILE", None)
        self.ctx = FakeHermesContext({"api_url": self.api_url})
        register(self.ctx)

    def tearDown(self) -> None:
        plugin._sessions.clear()
        plugin._ctx = None
        os.environ.pop("REACH_AUTH_TOKEN", None)

    # -- registration & session binding ------------------------------------

    def test_register_attaches_hooks_and_tools(self) -> None:
        self.assertEqual(set(self.ctx.hooks), {"on_session_start", "pre_llm_call",
                                               "pre_tool_call", "post_tool_call",
                                               "on_session_finalize"})
        self.assertEqual(set(self.ctx.tools), {"reach_lease_screen", "reach_release_screen",
                                               "reach_status", "reach_tool", "reach_drive",
                                               "reach_smart_browse"})
        out = _call(self.ctx, "reach_status", {})
        self.assertEqual(out["status"], "ok")
        # Real PluginContext invocations require the trusted session binding.
        missing = json.loads(self.ctx.tools["reach_status"]({}))
        self.assertEqual(missing["status"], "error")
        self.assertIn("session binding", missing["message"])

    def test_same_profile_sessions_get_independent_leases(self) -> None:
        on_session_start(session_id="sA", platform="cli")
        on_session_start(session_id="sB", platform="cli")
        st_a, st_b = get_state("sA"), get_state("sB")
        self.assertEqual(st_a["screen"], 0)
        self.assertEqual(st_b["screen"], 1)
        self.assertNotEqual(st_a["screen"], st_b["screen"])
        self.assertTrue(st_a["has_lease"] and st_b["has_lease"])
        # Capabilities are private and distinct.
        tok_a = plugin._sessions["sA"]["token"]
        tok_b = plugin._sessions["sB"]["token"]
        self.assertTrue(tok_a and tok_b and tok_a != tok_b)
        # Concurrent interleaved tool calls stay on each session's own screen.
        out_b = _call(self.ctx, "reach_tool", {"name": "screenshot", "arguments": {}}, "sB")
        out_a = _call(self.ctx, "reach_tool", {"name": "screenshot", "arguments": {}}, "sA")
        self.assertEqual((out_a["status"], out_b["status"]), ("ok", "ok"))
        self.assertEqual([c["screen"] for c in self.server.tool_calls], [1, 0])
        # First-turn announcement is per-session.
        self.assertIn("screen 0", pre_llm_call(is_first_turn=True, session_id="sA")["context"])
        self.assertIn("screen 1", pre_llm_call(is_first_turn=True, session_id="sB")["context"])

    def test_standalone_calls_use_distinct_default_key(self) -> None:
        on_session_start(session_id="s1")
        res = reach_lease_screen()  # no binding -> standalone session
        self.assertEqual(res["status"], "ok")
        self.assertEqual(res["screen"], 1)
        self.assertIn(STANDALONE_SESSION, plugin._sessions)
        self.assertEqual(plugin._sessions[STANDALONE_SESSION]["screen"], 1)
        self.assertEqual(plugin._sessions["s1"]["screen"], 0)

    def test_session_start_when_screens_exhausted_or_api_down(self) -> None:
        for s in self.server.screens:
            s["owner"] = "other_profile"
        on_session_start(session_id="s2")
        self.assertIsNone(get_state("s2")["screen"])
        self.assertIn("No free Agent Computer screen",
                      pre_llm_call(is_first_turn=True, session_id="s2")["context"])

        self.ctx.settings["api_url"] = "http://127.0.0.1:1"  # unreachable
        on_session_start(session_id="s3")
        self.assertIsNone(get_state("s3")["screen"])
        self.assertIn("Agent Computer unavailable",
                      pre_llm_call(is_first_turn=True, session_id="s3")["context"])

    def test_occupied_screen_conflicts_and_never_recovers_token(self) -> None:
        on_session_start(session_id="s1")  # leases screen 0 as "piper"
        res = reach_lease_screen(screen=0, owner="piper")  # same owner label, standalone key
        self.assertEqual(res["status"], "conflict")

    # -- pre_tool_call routing / native MCP block ---------------------------

    def test_pre_tool_call_blocks_native_mcp_and_routes_plugin_tools(self) -> None:
        on_session_start(session_id="s1")
        block = pre_tool_call(tool_name="mcp__reach__click",
                              args={"x": 1, "y": 2}, session_id="s1")
        self.assertEqual(block["action"], "block")
        self.assertIn("reach_tool", block["message"])
        self.assertNotIn("tok-", block["message"])

        modify = pre_tool_call(tool_name="reach_drive", args={"goal": "g"}, session_id="s1")
        self.assertEqual(modify, {"action": "modify", "args": {"screen": 0}})

        foreign = pre_tool_call(tool_name="reach_drive",
                                args={"goal": "g", "screen": 1}, session_id="s1")
        self.assertEqual(foreign["action"], "block")

        self.assertIsNone(pre_tool_call(tool_name="terminal",
                                        args={"command": "ls"}, session_id="s1"))
        # reach_tool does its own binding checks at execution time.
        self.assertIsNone(pre_tool_call(tool_name="reach_tool",
                                        args={"name": "click"}, session_id="s1"))

    # -- reach_tool behavior -------------------------------------------------

    def test_reach_tool_injects_screen_and_capabilities(self) -> None:
        on_session_start(session_id="s1")
        lease_gen = self.server.screens[0]["handoff_gen"]
        out = _call(self.ctx, "reach_tool",
                    {"name": "page_text", "arguments": {}}, "s1")  # URL omitted: current page
        self.assertEqual(out["status"], "ok")
        call = self.server.tool_calls[-1]
        self.assertEqual(call["tool"], "page_text")
        self.assertEqual(call["screen"], 0)
        self.assertEqual(call["gen"], str(lease_gen))
        self.assertTrue(call["had_lease_token"])

    def test_reach_tool_refusals(self) -> None:
        on_session_start(session_id="s1")
        cases = [
            ({"name": "not_a_tool", "arguments": {}}, "unknown reach tool"),
            ({"name": "click", "arguments": {"screen": 1, "x": 1, "y": 1}},
              "not leased to this session"),
            ({"name": "click", "arguments": {"token": "tok-forged"}}, "credential-bearing"),
            ({"name": "click", "arguments": {"api_url": "http://evil:4200"}}, "credential-bearing"),
            ({"name": "click", "arguments": {"session_id": "other"}}, "credential-bearing"),
            ({"name": "click", "arguments": {"owner": "someone"}}, "credential-bearing"),
        ]
        for args, fragment in cases:
            out = _call(self.ctx, "reach_tool", args, "s1")
            self.assertEqual(out["status"], "error", msg=str(args))
            self.assertIn(fragment, out["message"], msg=str(args))
        self.assertEqual(self.server.tool_calls, [])
        # Unbound session cannot call primitives at all.
        self.assertIn("no screen leased",
                      _call(self.ctx, "reach_tool", {"name": "click", "arguments": {}}, "s9")["message"])

    def test_undeclared_model_parameters_rejected(self) -> None:
        on_session_start(session_id="s1")
        before = self.server.request_count
        for tool, args, fragment in [
            ("reach_status", {"api_url": "http://evil:4200"}, "undeclared"),
            ("reach_status", {"session_id": "hijack"}, "undeclared"),
            ("reach_release_screen", {"owner": "someone-else", "screen": 1},
             "not leased to this session"),
            ("reach_drive", {"goal": "g", "screen": 1, "completion_text": "x"},
             "not leased to this session"),
        ]:
            out = _call(self.ctx, tool, args, "s1")
            self.assertEqual(out["status"], "error", msg=tool)
            self.assertIn(fragment, out["message"], msg=tool)
        self.assertEqual(self.server.request_count, before)
        # Session binding unchanged: s1 still drives its own screen 0.
        out = _call(self.ctx, "reach_tool", {"name": "screenshot", "arguments": {}}, "s1")
        self.assertEqual(out["status"], "ok")
        self.assertEqual(self.server.tool_calls[-1]["screen"], 0)

    # -- release lifecycle ----------------------------------------------------

    def test_finalize_releases_lease_only_for_that_session(self) -> None:
        on_session_start(session_id="sA")
        on_session_start(session_id="sB")
        on_session_finalize(session_id="sA", platform="cli")
        self.assertIsNone(self.server.screens[0]["owner"])
        self.assertIsNone(self.server.screens[0]["lease_token"])
        self.assertEqual(self.server.screens[1]["owner"], "piper")  # sB untouched
        self.assertIsNone(get_state("sA")["screen"])
        self.assertEqual(get_state("sB")["screen"], 1)
        self.assertNotIn("sA", plugin._sessions)
        on_session_finalize(session_id="sB", platform="cli")
        self.assertIsNone(self.server.screens[1]["owner"])

    def test_failed_release_retains_state_until_handback(self) -> None:
        on_session_start(session_id="s1")
        self.server.screens[0]["phase"] = "HumanActive"  # human took over
        with self.assertLogs(plugin.logger.name, level="WARNING") as logs:
            on_session_finalize(session_id="s1", platform="cli")
        # State and capability retained: nothing lost after the failed release.
        st = get_state("s1")
        self.assertEqual(st["screen"], 0)
        self.assertTrue(st["has_lease"])
        self.assertEqual(self.server.screens[0]["owner"], "piper")
        self.assertIn("retained", " ".join(logs.output))
        self.assertNotIn("tok-", " ".join(logs.output))

        # Human hands back -> explicitly ack (gen bump) -> fresh observation -> release.
        self.server.screens[0]["phase"] = "HumanDone"
        ack = _call(self.ctx, "reach_tool", {"name": "ack_handback", "arguments": {}}, "s1")
        self.assertEqual(ack["status"], "ok")
        out = _call(self.ctx, "reach_tool", {"name": "screenshot", "arguments": {}}, "s1")
        self.assertEqual(out["status"], "ok")
        self.assertEqual(self.server.screens[0]["phase"], "AgentActive")
        self.assertEqual(plugin._sessions["s1"]["handoff_gen"],
                         self.server.screens[0]["handoff_gen"])
        self.assertEqual(self.server.tool_calls[-1]["gen"],
                         str(self.server.screens[0]["handoff_gen"]))
        on_session_finalize(session_id="s1", platform="cli")
        self.assertIsNone(self.server.screens[0]["owner"])
        self.assertNotIn("s1", plugin._sessions)

    def test_failed_release_survives_connection_reset_while_reading_error_body(self) -> None:
        on_session_start(session_id="s1")

        import io

        class ResetBody(io.BytesIO):
            def read(self) -> bytes:
                raise ConnectionResetError("peer reset")

        error = urllib.error.HTTPError(
            self.api_url + "/agent/screens/0/lease", 409, "busy", {}, ResetBody())
        with patch("__init__._http_request", side_effect=error):
            result = _call(self.ctx, "reach_release_screen", {}, "s1")
        self.assertEqual(result["status"], "error")
        self.assertTrue(result["state_retained"])
        self.assertIn("peer reset", result["message"])
        self.assertEqual(get_state("s1")["screen"], 0)
        self.assertTrue(get_state("s1")["has_lease"])

    def test_explicit_lease_release_and_status(self) -> None:
        out = _call(self.ctx, "reach_status", {})
        self.assertEqual(len(out["screens"]), 2)
        res = _call(self.ctx, "reach_lease_screen", {"screen": 1, "owner": "tester"})
        self.assertEqual(res["screen"], 1)
        self.assertEqual(self.server.screens[1]["owner"], "tester")
        # Reuse: a second lease call returns the retained capability, no re-POST.
        before_reuse = self.server.request_count
        again = _call(self.ctx, "reach_lease_screen", {"screen": 1}, "s1")
        self.assertEqual(again["status"], "ok")
        self.assertTrue(again["reused"])
        self.assertEqual(self.server.request_count, before_reuse)
        # Foreign screen refusal on the bound session.
        self.assertIn("not leased to this session",
                      _call(self.ctx, "reach_release_screen", {"screen": 0}, "s1")["message"])
        self.assertEqual(_call(self.ctx, "reach_release_screen", {}, "s1")["status"], "ok")
        self.assertIsNone(self.server.screens[1]["owner"])
        self.assertIsNone(get_state("s1")["screen"])

    def test_post_tool_call_observes_server_handoff_without_repeating_takeover(self) -> None:
        on_session_start(session_id="s1")
        result = _call(self.ctx, "reach_tool",
                       {"name": "auth_handoff",
                        "arguments": {"url": "https://login"}}, "s1")
        self.assertEqual(result["status"], "ok")
        self.assertEqual(self.server.screens[0]["phase"], "HandoffPending")
        server_gen = self.server.screens[0]["handoff_gen"]
        before_hook = self.server.request_count

        post_tool_call(tool_name="reach_tool",
                       args={"name": "auth_handoff", "arguments": {"url": "https://login"}},
                       result=json.dumps(result), session_id="s1")

        self.assertEqual(self.server.request_count, before_hook)
        self.assertEqual(plugin._sessions["s1"]["handoff_gen"], server_gen)
        self.assertTrue(self.server.screens[0]["takeover_pending"])

    def test_post_tool_call_ignores_failed_handoff_response(self) -> None:
        on_session_start(session_id="s1")
        before_gen = plugin._sessions["s1"]["handoff_gen"]
        before_requests = self.server.request_count
        failed = {
            "status": "error",
            "tool": "auth_handoff",
            "result": {"content": [{"type": "text", "text": json.dumps({
                "status": "auth_required",
                "phase": "HandoffPending",
                "handoff_gen": before_gen + 1,
            })}], "isError": True},
        }
        post_tool_call(tool_name="reach_tool",
                       args={"name": "auth_handoff"}, result=json.dumps(failed),
                       session_id="s1")
        self.assertEqual(self.server.request_count, before_requests)
        self.assertEqual(plugin._sessions["s1"]["handoff_gen"], before_gen)
        self.assertEqual(self.server.screens[0]["phase"], "AgentActive")

    # -- redaction ------------------------------------------------------------

    def test_no_token_or_generation_exposure(self) -> None:
        on_session_start(session_id="s1")
        blobs: List[str] = []
        blobs.append(json.dumps(get_state("s1")))
        blobs.append(json.dumps(_call(self.ctx, "reach_status", {}, "s1")))
        blobs.append(json.dumps(_call(self.ctx, "reach_lease_screen", {}, "s1")))
        blobs.append(json.dumps(pre_llm_call(is_first_turn=True, session_id="s1")))
        for blob in blobs:
            self.assertNotIn("tok-", blob)
            self.assertNotIn("handoff_gen", blob)
            self.assertNotIn("token", blob.replace("has_lease", ""))
        status = _call(self.ctx, "reach_status", {}, "s1")
        self.assertNotIn("lease_token", json.dumps(status["screens"]))
        self.assertEqual(status["current_session"]["has_lease"], True)

    # -- reach_drive / smart_browse -------------------------------------------

    @patch("scripts.reach_drive.ReachDriver")
    def test_reach_drive_passes_lease_capabilities(self, mock_driver_cls: MagicMock) -> None:
        on_session_start(session_id="s1")
        inst = mock_driver_cls.return_value
        inst.drive.return_value.to_dict.return_value = {"success": True, "status": "completed"}
        out = _call(self.ctx, "reach_drive",
                    {"goal": "Log in", "completion_text": "Welcome back"}, "s1")
        self.assertEqual(out["status"], "completed")
        mock_driver_cls.assert_called_once_with(
            api_url=self.api_url, screen=0, max_steps=15,
            lease_token=plugin._sessions["s1"]["token"],
            handoff_gen=plugin._sessions["s1"]["handoff_gen"],
            completion_text="Welcome back")
        inst.drive.assert_called_once_with(goal="Log in", initial_url=None)
        # Empty completion_text is rejected without invoking the driver.
        self.assertEqual(_call(self.ctx, "reach_drive",
                               {"goal": "g", "completion_text": "  "}, "s1")["status"], "error")
        self.assertEqual(mock_driver_cls.call_count, 1)

    @patch("__init__._try_obscura")
    def test_reach_smart_browse_obscura_hit(self, mock_obscura: MagicMock) -> None:
        mock_obscura.return_value = (True, "# Fast Markdown Output", 65.2)
        res = reach_smart_browse(url="https://news.ycombinator.com")
        self.assertEqual(res["status"], "ok")
        self.assertEqual(res["tier"], "Tier 1 (Obscura Fast Path)")
        self.assertEqual(res["content"], "# Fast Markdown Output")
        mock_obscura.assert_called_once_with("https://news.ycombinator.com")

    @patch("__init__._try_obscura")
    def test_reach_smart_browse_escalates_with_lease_auth(self, mock_obscura: MagicMock) -> None:
        on_session_start(session_id="s1")
        mock_obscura.return_value = (False, "Anti-bot signature detected: 'cloudflare turnstile'", 120.0)
        res = _call(self.ctx, "reach_smart_browse",
                    {"url": "https://protected.example.com"}, "s1")
        self.assertEqual(res["status"], "ok")
        self.assertEqual(res["tier"], "Tier 2 (Reach MicroVM Headed Chrome)")
        self.assertIn("cloudflare turnstile", res["escalation_reason"])
        self.assertEqual(res["data"]["title"], "Mock Title")
        call = self.server.tool_calls[-1]
        self.assertEqual(call["tool"], "browse")
        self.assertEqual(call["screen"], 0)
        self.assertEqual(call["gen"], str(self.server.screens[0]["handoff_gen"]))
        self.assertTrue(call["had_lease_token"])

    @patch("__init__._try_obscura")
    def test_reach_smart_browse_force_headed(self, mock_obscura: MagicMock) -> None:
        mock_obscura.return_value = (False, "Obscura binary not installed", 0.0)
        res = reach_smart_browse(url="https://example.com", force_headed=True)
        self.assertEqual(res["status"], "ok")
        self.assertEqual(res["tier"], "Tier 2 (Reach MicroVM Headed Chrome)")
        self.assertEqual(res["escalation_reason"], "force_headed requested")
        mock_obscura.assert_not_called()
        self.assertEqual(self.server.screens[0]["owner"], "piper")
        call = self.server.tool_calls[-1]
        self.assertEqual(call["screen"], 0)
        self.assertEqual(call["gen"], str(self.server.screens[0]["handoff_gen"]))
        self.assertTrue(call["had_lease_token"])

    # -- transport hardening ---------------------------------------------------

    def test_transport_fails_closed_on_redirect_and_foreign_endpoint(self) -> None:
        with self.assertRaises(urllib.error.HTTPError):
            plugin._http_request("/redirect", supervisor=True)
        self.assertEqual(self.server.redirects_followed, 0)
        with self.assertRaises(ValueError):
            plugin._http_request("/agent/screens", api_url="http://127.0.0.1:9",
                                 supervisor=True)


if __name__ == "__main__":
    unittest.main()
