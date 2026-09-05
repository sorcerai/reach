"""Unit tests for the buzz-groupchat Hermes plugin."""

import importlib.util
import json
import os
from pathlib import Path
import subprocess
from unittest.mock import MagicMock, patch

import pytest

PLUGIN_PATH = Path(__file__).parent / "__init__.py"
spec = importlib.util.spec_from_file_location("hermes_buzz_plugin", PLUGIN_PATH)
plugin = importlib.util.module_from_spec(spec)
spec.loader.exec_module(plugin)


def test_defaults(monkeypatch):
    monkeypatch.delenv("BUZZ_RELAY_URL", raising=False)
    monkeypatch.delenv("BUZZ_PRIVATE_KEY", raising=False)
    assert plugin.get_relay_url() == plugin.DEFAULT_RELAY_URL
    assert plugin.get_private_key() is None

    monkeypatch.setenv("BUZZ_RELAY_URL", "http://100.124.38.17:3000/")
    monkeypatch.setenv("BUZZ_PRIVATE_KEY", "nsec1abc")
    assert plugin.get_relay_url() == "http://100.124.38.17:3000"
    assert plugin.get_private_key() == "nsec1abc"


def test_find_buzz_cli(monkeypatch, tmp_path):
    # Fake binary
    fake_cli = tmp_path / "buzz-cli"
    fake_cli.write_text("#!/bin/sh\nexit 0")
    fake_cli.chmod(0o755)

    monkeypatch.setenv("PATH", f"{tmp_path}:{os.environ.get('PATH', '')}")
    assert plugin.find_buzz_cli() == str(fake_cli)


def test_run_buzz_cli_success(monkeypatch):
    mock_run = MagicMock()
    mock_run.return_value = subprocess.CompletedProcess(
        args=["buzz-cli"],
        returncode=0,
        stdout=json.dumps({"status": "sent", "id": "event-123"}),
        stderr="",
    )
    monkeypatch.setattr(subprocess, "run", mock_run)
    with patch.object(plugin, "find_buzz_cli", return_value="/usr/local/bin/buzz-cli"):
        result = plugin.run_buzz_cli(["messages", "list"])
        assert result["ok"] is True
        assert result["data"]["id"] == "event-123"


def test_run_buzz_cli_not_found():
    with patch.object(plugin, "find_buzz_cli", return_value=None):
        result = plugin.run_buzz_cli(["messages", "list"])
        assert result["ok"] is False
        assert result["error"] == "buzz_cli_not_found"


def test_buzz_send_message():
    with patch.object(plugin, "run_buzz_cli") as mock_cli:
        mock_cli.return_value = {"ok": True, "data": {"id": "ev1"}}
        res = plugin.buzz_send_message("chan-1", "hello world", reply_to="root-1", broadcast=True)
        assert res["ok"] is True
        mock_cli.assert_called_once_with(
            ["messages", "send", "--channel", "chan-1", "--content", "hello world", "--reply-to", "root-1", "--broadcast"]
        )


def test_buzz_send_takeover_alert():
    with patch.object(plugin, "run_buzz_cli") as mock_cli:
        mock_cli.return_value = {"ok": True, "data": {"id": "alert-1"}}
        res = plugin.buzz_send_takeover_alert(
            channel="ops",
            screen=0,
            reason="SMS 2FA Challenge",
            novnc_url="http://100.124.38.17:6080/vnc.html",
        )
        assert res["ok"] is True
        args = mock_cli.call_args[0][0]
        assert args[0:3] == ["messages", "send", "--channel"]
        assert args[3] == "ops"
        assert "SMS 2FA Challenge" in args[5]
        assert "Display `0`" in args[5]
        assert "Hand Back to Agent" in args[5]
        assert "--broadcast" in args


def test_buzz_post_visual_diff():
    with patch.object(plugin, "run_buzz_cli") as mock_cli:
        mock_cli.return_value = {"ok": True, "data": {"id": "diff-1"}}
        res = plugin.buzz_post_visual_diff(
            channel="dev",
            summary="Clicked login button and waited for dashboard",
            diff_percent=0.45,
            tokens_saved=1200,
        )
        assert res["ok"] is True
        args = mock_cli.call_args[0][0]
        content = args[5]
        assert "Reach Visual Diff Audit" in content
        assert "- **pHash Change**: `0.45%`" in content
        assert "- **VLM Tokens Saved**: `1200` tokens" in content


def test_buzz_get_messages():
    with patch.object(plugin, "run_buzz_cli") as mock_cli:
        mock_cli.return_value = {"ok": True, "data": []}
        res = plugin.buzz_get_messages("chan-general", limit=10)
        assert res["ok"] is True
        mock_cli.assert_called_once_with(["messages", "get", "--channel", "chan-general", "--limit", "10"])


def test_buzz_list_channels():
    with patch.object(plugin, "run_buzz_cli") as mock_cli:
        mock_cli.return_value = {"ok": True, "data": [{"id": "c1", "name": "general"}]}
        res = plugin.buzz_list_channels()
        assert res["ok"] is True
        mock_cli.assert_called_once_with(["channels", "list"])
