"""Unit tests for Browser Use integration with Agent Computer."""

from pathlib import Path
import sys
import unittest
from unittest.mock import MagicMock, patch

REPO_ROOT = Path(__file__).parent.parent.resolve()
if str(REPO_ROOT) not in sys.path:
    sys.path.insert(0, str(REPO_ROOT))

from integrations.browser_use import AgentComputerBrowserAdapter


class BrowserUseAdapterTests(unittest.TestCase):
    def setUp(self) -> None:
        self.adapter = AgentComputerBrowserAdapter(
            screen_id=1,
            api_url="http://127.0.0.1:4200",
            host="127.0.0.1",
        )

    def test_url_derivations(self) -> None:
        self.assertEqual(self.adapter.cdp_url, "http://127.0.0.1:9223")
        self.assertEqual(self.adapter.novnc_url, "http://127.0.0.1:6081/vnc.html")

    def test_custom_cdp_port(self) -> None:
        custom_adapter = AgentComputerBrowserAdapter(
            screen_id=0,
            cdp_port=9999,
        )
        self.assertEqual(custom_adapter.cdp_url, "http://127.0.0.1:9999")

    @patch("urllib.request.urlopen")
    def test_lease_screen_success(self, mock_urlopen: MagicMock) -> None:
        mock_resp = MagicMock()
        mock_resp.status = 200
        mock_resp.__enter__.return_value = mock_resp
        mock_urlopen.return_value = mock_resp

        res = self.adapter.lease_screen(duration_sec=300)
        self.assertEqual(res["status"], "leased")
        self.assertEqual(res["screen"], 1)
        self.assertTrue(self.adapter._leased)
        call_req = mock_urlopen.call_args[0][0]
        self.assertIn(b'"owner": "browser-use"', call_req.data)

    @patch("urllib.request.urlopen")
    def test_lease_screen_supervisor_offline_falls_back_gracefully(self, mock_urlopen: MagicMock) -> None:
        import urllib.error

        mock_urlopen.side_effect = urllib.error.URLError("Connection refused")
        res = self.adapter.lease_screen()
        self.assertEqual(res["status"], "unsupervised")
        self.assertFalse(self.adapter._leased)

    @patch("urllib.request.urlopen")
    def test_release_screen(self, mock_urlopen: MagicMock) -> None:
        self.adapter._leased = True
        self.adapter._owner = "test-worker"
        mock_resp = MagicMock()
        mock_resp.status = 200
        mock_resp.__enter__.return_value = mock_resp
        mock_urlopen.return_value = mock_resp

        ok = self.adapter.release_screen()
        self.assertTrue(ok)
        self.assertFalse(self.adapter._leased)
        call_req = mock_urlopen.call_args[0][0]
        self.assertIn(b'"owner": "test-worker"', call_req.data)

    @patch("urllib.request.urlopen")
    def test_context_manager(self, mock_urlopen: MagicMock) -> None:
        mock_resp = MagicMock()
        mock_resp.status = 200
        mock_resp.__enter__.return_value = mock_resp
        mock_urlopen.return_value = mock_resp

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

    @patch("urllib.request.urlopen")
    def test_lease_token_captured_and_sent_on_release(self, mock_urlopen: MagicMock) -> None:
        mock_lease_resp = MagicMock()
        mock_lease_resp.status = 200
        mock_lease_resp.read.return_value = b'{"status": "ok", "token": "test-crypto-token-123"}'
        mock_lease_resp.__enter__.return_value = mock_lease_resp

        mock_release_resp = MagicMock()
        mock_release_resp.status = 200
        mock_release_resp.read.return_value = b'{"status": "ok", "released": true}'
        mock_release_resp.__enter__.return_value = mock_release_resp

        mock_urlopen.side_effect = [mock_lease_resp, mock_release_resp]

        res = self.adapter.lease_screen()
        self.assertEqual(res["token"], "test-crypto-token-123")
        self.assertEqual(self.adapter.lease_token, "test-crypto-token-123")

        ok = self.adapter.release_screen()
        self.assertTrue(ok)
        self.assertIsNone(self.adapter.lease_token)

        release_req = mock_urlopen.call_args[0][0]
        self.assertEqual(release_req.get_header("X-lease-token"), "test-crypto-token-123")

    @patch("urllib.request.urlopen")
    def test_bearer_auth_sent_when_configured(self, mock_urlopen: MagicMock) -> None:
        adapter = AgentComputerBrowserAdapter(
            screen_id=0,
            auth_token="secret-bearer-token",
        )
        mock_resp = MagicMock()
        mock_resp.status = 200
        mock_resp.read.return_value = b'{"status": "ok"}'
        mock_resp.__enter__.return_value = mock_resp
        mock_urlopen.return_value = mock_resp

        adapter.lease_screen()
        lease_req = mock_urlopen.call_args[0][0]
        self.assertEqual(lease_req.get_header("Authorization"), "Bearer secret-bearer-token")

        adapter.release_screen()
        release_req = mock_urlopen.call_args[0][0]
        self.assertEqual(release_req.get_header("Authorization"), "Bearer secret-bearer-token")


if __name__ == "__main__":
    unittest.main()
