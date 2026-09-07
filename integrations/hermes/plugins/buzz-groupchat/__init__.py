"""Hermes plugin: buzz-groupchat.

Multi-Bot Groupchat surface for Reach & Hermes agents.
Integrates with self-hosted Buzz (ariaserver:3000) for permanent history,
inter-agent @mentions, numeric visual-change metadata, and authenticated human takeover links.
"""

from __future__ import annotations

import json
import logging
import os
import shutil
import subprocess
import urllib.parse
from typing import Any, Dict, List, Optional

logger = logging.getLogger("hermes.plugins.buzz_groupchat")

DEFAULT_RELAY_URL = "http://100.124.38.17:3000"


_ctx: Any = None


def _cfg(key: str, default: Any = None) -> Any:
    if _ctx is None:
        return default
    try:
        return _ctx.get_config(key, default)
    except Exception:
        return default


def get_relay_url() -> str:
    """Return configured Buzz relay URL."""
    cfg_val = _cfg("relay_url")
    return (str(cfg_val) if cfg_val else os.environ.get("BUZZ_RELAY_URL", DEFAULT_RELAY_URL)).rstrip("/")


def get_private_key() -> Optional[str]:
    """Return configured Buzz private key (hex or nsec)."""
    return _cfg("private_key") or os.environ.get("BUZZ_PRIVATE_KEY")


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
    reply_to: Optional[str] = None,
) -> Dict[str, Any]:
    """Post a non-secret authenticated viewer front door."""
    if type(screen) is not int or screen < 0:
        return {"ok": False, "error": "invalid screen"}
    endpoint = urllib.parse.urlsplit(os.environ.get("REACH_AGENT_URL", "http://127.0.0.1:4200"))
    host = endpoint.hostname
    if endpoint.scheme not in {"http", "https"} or not host:
        return {"ok": False, "error": "invalid Reach API origin"}
    authority = f"[{host}]" if ":" in host else host
    if endpoint.port is not None:
        authority += f":{endpoint.port}"
    url = urllib.parse.urlunsplit((endpoint.scheme, authority, f"/viewer/{screen}", "", ""))
    content = (
        f"Human action required on display {screen}.\n"
        f"Authenticated viewer: {url}\n"
        "Sign in as the supervisor, complete the handoff, then hand back to the agent."
    )
    return buzz_send_message(channel=channel, content=content, reply_to=reply_to, broadcast=True)


def buzz_post_visual_diff(
    channel: str,
    diff_percent: Optional[float] = None,
    tokens_saved: Optional[int] = None,
    reply_to: Optional[str] = None,
) -> Dict[str, Any]:
    """Post numeric visual-change metadata without raw captures or model text."""
    content_lines = ["Reach visual-change metadata"]
    if diff_percent is not None:
        if not isinstance(diff_percent, (int, float)) or isinstance(diff_percent, bool):
            return {"ok": False, "error": "invalid visual-change measurement"}
        content_lines.append(f"Visual change: {diff_percent:.2f}%")
    if tokens_saved is not None:
        if type(tokens_saved) is not int:
            return {"ok": False, "error": "invalid token estimate"}
        content_lines.append(f"Estimated tokens saved: {tokens_saved}")

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


# --------------------------------------------------------------------------
# Hermes Registration
# --------------------------------------------------------------------------

PLUGIN_TOOLS: Dict[str, tuple] = {
    "buzz_send_message": (
        buzz_send_message,
        "Send a message or status update to a Buzz channel or thread.",
        {
            "type": "object",
            "required": ["channel", "content"],
            "properties": {
                "channel": {"type": "string", "description": "Target channel ID or name."},
                "content": {"type": "string", "description": "Message content (supports markdown and @mentions)."},
                "reply_to": {"type": "string", "description": "Optional parent message ID to reply in a thread."},
                "broadcast": {"type": "boolean", "default": False, "description": "Whether to also broadcast thread reply to the channel."},
            },
        },
    ),
    "buzz_send_takeover_alert": (
        buzz_send_takeover_alert,
        "Post an interactive Human Takeover alert to a Buzz channel or thread when 2FA, Captcha, or credentials require human intervention.",
        {
            "type": "object",
            "required": ["channel", "screen"],
            "properties": {
                "channel": {"type": "string", "description": "Target Buzz channel ID or name."},
                "screen": {"type": "integer", "description": "Screen ID where human intervention is required."},
                "reply_to": {"type": "string", "description": "Optional thread ID to post the takeover request under."},
            },
        },
    ),
    "buzz_post_visual_diff": (
        buzz_post_visual_diff,
        "Post numeric visual-change metadata without screenshots or model text.",
        {
            "type": "object",
            "required": ["channel"],
            "properties": {
                "channel": {"type": "string", "description": "Target Buzz channel ID or name."},
                "diff_percent": {"type": "number", "description": "Optional perceptual hash or visual difference percentage (0.0 to 100.0)."},
                "tokens_saved": {"type": "integer", "description": "Optional estimated tokens saved; not a provider measurement."},
                "reply_to": {"type": "string", "description": "Optional thread ID to post under."},
            },
        },
    ),
    "buzz_get_messages": (
        buzz_get_messages,
        "Read recent messages from a Buzz channel or thread to check for @mentions or user instructions.",
        {
            "type": "object",
            "required": ["channel"],
            "properties": {
                "channel": {"type": "string", "description": "Channel ID or name."},
                "limit": {"type": "integer", "default": 20, "description": "Maximum number of messages to retrieve."},
            },
        },
    ),
    "buzz_list_channels": (
        buzz_list_channels,
        "List available channels on the Buzz relay.",
        {
            "type": "object",
            "properties": {
                "relay_url": {"type": "string", "description": "Optional Buzz relay URL override."},
            },
        },
    ),
}


def _handler(fn: Any) -> Any:
    def run(args: Optional[Dict[str, Any]] = None, **kw: Any) -> str:
        return json.dumps(fn(**(args or {})))
    return run


def register(ctx: Any) -> None:
    global _ctx
    _ctx = ctx
    for name, (fn, description, params) in PLUGIN_TOOLS.items():
        ctx.register_tool(
            name=name,
            toolset="buzz",
            schema={"name": name, "description": description, "parameters": params},
            handler=_handler(fn),
            description=description,
            emoji="🐝",
        )

