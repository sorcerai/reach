"""Unit tests for Obscura Browser Hermes Plugin."""

import importlib.util
from pathlib import Path
import unittest
from unittest.mock import MagicMock, patch

PLUGIN_PATH = Path(__file__).parent / "__init__.py"
spec = importlib.util.spec_from_file_location("hermes_obscura_plugin", PLUGIN_PATH)
plugin = importlib.util.module_from_spec(spec)
spec.loader.exec_module(plugin)


class TestObscuraPlugin(unittest.TestCase):
    def test_cdp_info(self):
        info = plugin.obscura_cdp_info(port=9222)
        self.assertEqual(info["port"], 9222)
        self.assertIn("ws://127.0.0.1:9222", info["ws_endpoint"])
        self.assertEqual(info["protocol"], "Chrome DevTools Protocol (CDP)")
        self.assertTrue(len(info["features"]) >= 4)

    @patch("shutil.which", return_value=None)
    @patch("os.path.isfile", return_value=False)
    def test_fetch_missing_binary(self, _mock_file, _mock_which):
        res = plugin.obscura_fetch("https://example.com")
        self.assertEqual(res["error"], "obscura_not_found")

    @patch("shutil.which", return_value="/usr/local/bin/obscura")
    @patch("subprocess.run")
    def test_fetch_success(self, mock_run, _mock_which):
        mock_proc = MagicMock()
        mock_proc.returncode = 0
        mock_proc.stdout = "# Heading\nSome text"
        mock_run.return_value = mock_proc

        res = plugin.obscura_fetch("https://example.com", dump="markdown", stealth=True)
        self.assertEqual(res["status"], "success")
        self.assertEqual(res["format"], "markdown")
        self.assertIn("# Heading", res["content"])
        mock_run.assert_called_once()
        cmd = mock_run.call_args[0][0]
        self.assertIn("--stealth", cmd)
        self.assertIn("--dump", cmd)
        self.assertIn("markdown", cmd)

    @patch("shutil.which", return_value="/usr/local/bin/obscura")
    @patch("subprocess.run")
    def test_eval_success(self, mock_run, _mock_which):
        mock_proc = MagicMock()
        mock_proc.returncode = 0
        mock_proc.stdout = "Example Domain"
        mock_run.return_value = mock_proc

        res = plugin.obscura_eval("https://example.com", "document.title")
        self.assertEqual(res["status"], "success")
        self.assertEqual(res["result"], "Example Domain")
        cmd = mock_run.call_args[0][0]
        self.assertIn("--eval", cmd)
        self.assertIn("document.title", cmd)

    @patch("shutil.which", return_value="/usr/local/bin/obscura")
    @patch("subprocess.run")
    def test_scrape_json_success(self, mock_run, _mock_which):
        mock_proc = MagicMock()
        mock_proc.returncode = 0
        mock_proc.stdout = '[{"url": "https://example.com", "title": "Example"}]'
        mock_run.return_value = mock_proc

        res = plugin.obscura_scrape(["https://example.com", "https://news.ycombinator.com"])
        self.assertEqual(res["status"], "success")
        self.assertEqual(len(res["results"]), 1)
        self.assertEqual(res["results"][0]["title"], "Example")


if __name__ == "__main__":
    unittest.main()
