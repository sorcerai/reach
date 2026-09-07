"""Unit tests for Browser Use integration with Agent Computer."""

import io
import json
import os
from pathlib import Path
import sys
import unittest
import urllib.error
import urllib.request
from typing import Any, Dict, Optional
from unittest.mock import MagicMock, patch

REPO_ROOT = Path(__file__).parent.parent.resolve()
if str(REPO_ROOT) not in sys.path:
    sys.path.insert(0, str(REPO_ROOT))

import integrations.browser_use.agent_computer_browser as adapter_module
from integrations.browser_use import AgentComputerBrowserAdapter


def _response(payload: Optional[Dict[str, Any]] = None, status: int = 200) -> MagicMock:
    resp = MagicMock()
    resp.status = status
    if payload is not None:
        resp.read.return_value = json.dumps(payload).encode("utf-8")
    resp.__enter__.return_value = resp
    return resp


def _http_error(code: int, body: bytes) -> urllib.error.HTTPError:
    return urllib.error.HTTPError(
        "http://127.0.0.1:4200/agent/screens/0/lease",
        code,
        "Error",
        {},
        io.BytesIO(body),
    )


class BrowserUseAdapterTests(unittest.TestCase):
    def setUp(self) -> None:
        # Isolate from any host-side supervisor credential.
        env_patcher = patch.dict(os.environ, {"REACH_AUTH_TOKEN": ""})
        env_patcher.start()
        self.addCleanup(env_patcher.stop)
        self.adapter = AgentComputerBrowserAdapter(
            screen_id=1,
            api_url="http://127.0.0.1:4200",
            host="127.0.0.1",
        )
        self.opener = MagicMock()
        opener_patcher = patch.object(adapter_module, "_OPENER", self.opener)
        opener_patcher.start()
        self.addCleanup(opener_patcher.stop)

    def test_url_derivations(self) -> None:
        self.assertEqual(self.adapter.cdp_url, "http://127.0.0.1:9223")
        self.assertEqual(self.adapter.novnc_url, "http://127.0.0.1:6081/vnc.html")

    def test_custom_cdp_port(self) -> None:
        custom_adapter = AgentComputerBrowserAdapter(
            screen_id=0,
            cdp_port=9999,
        )
        self.assertEqual(custom_adapter.cdp_url, "http://127.0.0.1:9999")

    def test_lease_screen_captures_capability_and_generation(self) -> None:
        self.opener.open.return_value = _response({
            "status": "ok",
            "token": "tok-1",
            "handoff_gen": 3,
        })

        res = self.adapter.lease_screen(duration_sec=300)
        self.assertEqual(res["status"], "leased")
        self.assertEqual(res["screen"], 1)
        self.assertEqual(res["token"], "tok-1")
        self.assertTrue(self.adapter._leased)
        self.assertEqual(self.adapter.lease_token, "tok-1")
        self.assertEqual(self.adapter.handoff_gen, 3)

        call_req = self.opener.open.call_args[0][0]
        self.assertEqual(call_req.get_method(), "POST")
        self.assertIn(b'"owner": "browser-use"', call_req.data)
        # Allocation carries no lease capability and, without a configured
        # supervisor credential, no bearer either.
        self.assertIsNone(call_req.get_header("X-lease-token"))
        self.assertIsNone(call_req.get_header("Authorization"))

    def test_lease_screen_sends_supervisor_bearer_on_allocation_only(self) -> None:
        adapter = AgentComputerBrowserAdapter(
            screen_id=0,
            auth_token="supervisor-secret",
        )
        self.opener.open.side_effect = [
            _response({"status": "ok", "token": "tok-2", "handoff_gen": 6}),
            _response({"status": "ok", "released": True}),
        ]

        adapter.lease_screen()
        lease_req = self.opener.open.call_args_list[0][0][0]
        self.assertEqual(lease_req.get_header("Authorization"), "Bearer supervisor-secret")

        ok = adapter.release_screen()
        self.assertTrue(ok)
        release_req = self.opener.open.call_args_list[1][0][0]
        # Ordinary requests carry the retained lease capability, never the bearer.
        self.assertIsNone(release_req.get_header("Authorization"))
        self.assertEqual(release_req.get_header("X-lease-token"), "tok-2")
        self.assertEqual(release_req.get_header("X-handoff-gen"), "6")

    def test_lease_screen_supervisor_offline_falls_back_gracefully(self) -> None:
        self.opener.open.side_effect = urllib.error.URLError("Connection refused")
        res = self.adapter.lease_screen()
        self.assertEqual(res["status"], "unsupervised")
        self.assertFalse(self.adapter._leased)

    def test_lease_screen_http_refusal_fails_closed(self) -> None:
        self.opener.open.side_effect = _http_error(
            409, b'{"error": "screen 1 already leased"}'
        )

        with self.assertRaises(RuntimeError):
            self.adapter.lease_screen()
        # Creation-only: no same-owner re-allocation, no silent unsupervised
        # downgrade after a server refusal.
        self.assertEqual(self.opener.open.call_count, 1)
        self.assertFalse(self.adapter._leased)
        self.assertIsNone(self.adapter.lease_token)

    def test_release_screen(self) -> None:
        self.adapter._leased = True
        self.adapter._owner = "test-worker"
        self.adapter._lease_token = "tok-1"
        self.adapter._handoff_gen = 4
        self.opener.open.return_value = _response({"status": "ok", "released": True})

        ok = self.adapter.release_screen()
        self.assertTrue(ok)
        self.assertFalse(self.adapter._leased)
        self.assertIsNone(self.adapter.lease_token)
        self.assertIsNone(self.adapter.handoff_gen)

        call_req = self.opener.open.call_args[0][0]
        self.assertEqual(call_req.get_method(), "DELETE")
        self.assertIn(b'"owner": "test-worker"', call_req.data)
        # Capability rides in the header, never the body.
        body = json.loads(call_req.data.decode("utf-8"))
        self.assertNotIn("token", body)

    def test_release_without_capability_refused(self) -> None:
        self.adapter._leased = True
        self.adapter._lease_token = None

        ok = self.adapter.release_screen()
        self.assertFalse(ok)
        self.opener.open.assert_not_called()
        self.assertTrue(self.adapter._leased)

    def test_release_failure_retains_lease_state(self) -> None:
        self.adapter._leased = True
        self.adapter._owner = "test-worker"
        self.adapter._lease_token = "tok-1"
        self.adapter._handoff_gen = 4
        self.opener.open.side_effect = _http_error(
            409, b'{"error": "screen busy: human active"}'
        )

        ok = self.adapter.release_screen()
        self.assertFalse(ok)
        # HumanActive refusal keeps the capability for a later retry.
        self.assertTrue(self.adapter._leased)
        self.assertEqual(self.adapter.lease_token, "tok-1")
        self.assertEqual(self.adapter.handoff_gen, 4)

    def test_context_manager(self) -> None:
        self.opener.open.side_effect = [
            _response({"status": "ok", "token": "tok-2", "handoff_gen": 5}),
            _response({"status": "ok", "released": True}),
        ]

        with AgentComputerBrowserAdapter(screen_id=2) as adapter:
            self.assertEqual(adapter.screen_id, 2)
            self.assertTrue(adapter._leased)

        self.assertFalse(adapter._leased)

    def test_browser_config_structure(self) -> None:
        cfg = self.adapter.get_browser_config()
        self.assertEqual(cfg["cdp_url"], "http://127.0.0.1:9223")
        self.assertTrue(cfg["disable_security"])

    def test_missing_dependency_raises_informative_error(self) -> None:
        with patch.dict("sys.modules", {"browser_use": None}):
            with self.assertRaises(ImportError) as ctx:
                self.adapter.create_browser()
            self.assertIn("pip install browser-use", str(ctx.exception))

    def test_lease_token_captured_and_sent_on_release(self) -> None:
        self.opener.open.side_effect = [
            _response({"status": "ok", "token": "test-crypto-token-123", "handoff_gen": 7}),
            _response({"status": "ok", "released": True}),
        ]

        res = self.adapter.lease_screen()
        self.assertEqual(res["token"], "test-crypto-token-123")
        self.assertEqual(self.adapter.lease_token, "test-crypto-token-123")

        ok = self.adapter.release_screen()
        self.assertTrue(ok)
        self.assertIsNone(self.adapter.lease_token)

        release_req = self.opener.open.call_args[0][0]
        self.assertEqual(release_req.get_header("X-lease-token"), "test-crypto-token-123")
        self.assertEqual(release_req.get_header("X-handoff-gen"), "7")

    def test_capability_requests_refuse_redirects(self) -> None:
        handler = adapter_module._NoRedirectHandler()
        req = urllib.request.Request("http://127.0.0.1:4200/agent/screens/1/lease")
        with self.assertRaises(urllib.error.HTTPError):
            handler.redirect_request(
                req, io.BytesIO(b""), 302, "Found", {}, "http://evil.example/steal"
            )


if __name__ == "__main__":
    unittest.main()
