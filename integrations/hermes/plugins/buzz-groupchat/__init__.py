"""Hermes plugin: buzz-groupchat.

Multi-Bot Groupchat surface for Reach & Hermes agents.
Integrates with self-hosted Buzz (ariaserver:3000) for permanent history,
inter-agent @mentions, in-line visual diff audit reels, and human takeover cards.
"""

from __future__ import annotations

import json
import logging
import os
import shutil
import subprocess
from typing import Any, Dict, List, Optional

logger = logging.getLogger("hermes.plugins.buzz_groupchat")

DEFAULT_RELAY_URL = "http://100.124.38.17:3000"
DEFAULT_NOVNC_BASE = "http://100.124.38.17:6080/vnc.html?autoconnect=true"


def get_relay_url() -> str:
    """Return configured Buzz relay URL."""
    return os.environ.get("BUZZ_RELAY_URL", DEFAULT_RELAY_URL).rstrip("/")


def get_private_key() -> Optional[str]:
    """Return configured Buzz private key (hex or nsec)."""
    return os.environ.get("BUZZ_PRIVATE_KEY")


def find_buzz_cli() -> Optional[str]:
    """Find the path to the buzz-cli binary."""
    # Check PATH first
    cli = shutil.which("buzz-cli") or shutil.which("buzz")
    if cli:
        return cli
    # Check well-known local paths
    for path in [
        os.path.expanduser("~/.local/bin/buzz-cli"),
        os.path.expanduser("~/.local/bin/buzz"),
        "/usr/local/bin/buzz-cli",
        "/usr/local/bin/buzz",
    ]:
        if os.path.isfile(path) and os.access(path, os.X_OK):
            return path
    return None


def run_buzz_cli(args: List[str], timeout: float = 15.0) -> Dict[str, Any]:
    """Execute a buzz-cli command and return parsed JSON."""
    cli_path = find_buzz_cli()
    if not cli_path:
        return {
            "ok": False,
            "error": "buzz_cli_not_found",
            "message": "buzz-cli binary not found in PATH or ~/.local/bin",
        }

    cmd = [cli_path, "--relay", get_relay_url(), "--format", "json"]
    privkey = get_private_key()
    if privkey:
        cmd.extend(["--private-key", privkey])
    cmd.extend(args)

    try:
        proc = subprocess.run(
            cmd,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            text=True,
            timeout=timeout,
        )
        if proc.returncode == 0:
            try:
                data = json.loads(proc.stdout) if proc.stdout.strip() else {"status": "ok"}
                return {"ok": True, "data": data}
            except json.JSONDecodeError:
                return {"ok": True, "data": proc.stdout.strip()}
        else:
            err_msg = proc.stderr.strip() or proc.stdout.strip()
            return {
                "ok": False,
                "error": "cli_error",
                "exit_code": proc.returncode,
                "message": err_msg,
            }
    except subprocess.TimeoutExpired:
        return {"ok": False, "error": "timeout", "message": f"Command timed out after {timeout}s"}
    except Exception as exc:
        return {"ok": False, "error": "execution_failed", "message": str(exc)}


def buzz_send_message(
    channel: str,
    content: str,
    reply_to: Optional[str] = None,
    broadcast: bool = False,
) -> Dict[str, Any]:
    """Send a message or status update to a Buzz channel or thread."""
    args = ["messages", "send", "--channel", channel, "--content", content]
    if reply_to:
        args.extend(["--reply-to", reply_to])
    if broadcast:
        args.append("--broadcast")
    return run_buzz_cli(args)


def buzz_send_takeover_alert(
    channel: str,
    screen: int,
    reason: str,
    novnc_url: Optional[str] = None,
    reply_to: Optional[str] = None,
) -> Dict[str, Any]:
    """Post an interactive Human Takeover alert to a Buzz channel or thread."""
    url = novnc_url or os.environ.get("REACH_NOVNC_URL", DEFAULT_NOVNC_BASE)
    content = (
        f"🚨 **Reach Human Takeover Required**\n\n"
        f"- **Screen**: Display `{screen}`\n"
        f"- **Reason**: {reason}\n"
        f"- **Interactive noVNC Link**: [{url}]({url})\n\n"
        f"👉 *Instructions*: Click the link above to interact with the screen. "
        f"When finished with 2FA / CAPTCHA, click the floating **[ Hand Back to Agent ]** banner "
        f"at the top of the display to resume autonomous execution."
    )
    return buzz_send_message(channel=channel, content=content, reply_to=reply_to, broadcast=True)


def buzz_post_visual_diff(
    channel: str,
    summary: str,
    screenshot_path: Optional[str] = None,
    diff_percent: Optional[float] = None,
    tokens_saved: Optional[int] = None,
    reply_to: Optional[str] = None,
) -> Dict[str, Any]:
    """Post a visual diff audit reel / screenshot summary to a Buzz channel."""
    content_lines = [f"📊 **Reach Visual Diff Audit**", f"- **Summary**: {summary}"]
    if diff_percent is not None:
        content_lines.append(f"- **pHash Change**: `{diff_percent:.2f}%`")
    if tokens_saved is not None:
        content_lines.append(f"- **VLM Tokens Saved**: `{tokens_saved}` tokens (gated via pHash)")

    # If screenshot is provided, try uploading it via buzz media upload
    media_url = None
    if screenshot_path and os.path.exists(screenshot_path):
        upload_res = run_buzz_cli(["media", "upload", screenshot_path])
        if upload_res.get("ok") and isinstance(upload_res.get("data"), dict):
            media_url = upload_res["data"].get("url") or upload_res["data"].get("sha256")
            if media_url:
                content_lines.append(f"\n![Audit Screenshot]({media_url})")

    content = "\n".join(content_lines)
    return buzz_send_message(channel=channel, content=content, reply_to=reply_to)


def buzz_get_messages(channel: str, limit: int = 20) -> Dict[str, Any]:
    """Read recent messages from a Buzz channel."""
    return run_buzz_cli(["messages", "get", "--channel", channel, "--limit", str(limit)])


def buzz_list_channels(relay_url: Optional[str] = None) -> Dict[str, Any]:
    """List available channels on the Buzz relay."""
    args = ["channels", "list"]
    if relay_url:
        # Override relay if supplied
        prev_relay = os.environ.get("BUZZ_RELAY_URL")
        try:
            os.environ["BUZZ_RELAY_URL"] = relay_url
            return run_buzz_cli(args)
        finally:
            if prev_relay is not None:
                os.environ["BUZZ_RELAY_URL"] = prev_relay
            else:
                os.environ.pop("BUZZ_RELAY_URL", None)
    return run_buzz_cli(args)
