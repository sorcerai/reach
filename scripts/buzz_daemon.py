#!/usr/bin/env python3
"""Reach Buzz Agent Daemon (@ReachBot Continuous Listener).

Listens on Buzz relay channels for `@ReachBot` mentions, leases screens from
the Reach agent API, dispatches CUA vision driving loops, posts visual diff
audit updates, and handles interactive 2FA/CAPTCHA takeover and handback.
"""

from __future__ import annotations

import argparse
import json
import logging
import os
import re
import shutil
import signal
import subprocess
import sys
import time
import urllib.error
import urllib.parse
import urllib.request
from dataclasses import dataclass, field
from pathlib import Path
from typing import Any, Callable, Dict, List, Optional, Set, Tuple, Union

# Ensure repository root is on sys.path
REPO_ROOT = Path(__file__).parent.parent.resolve()
if str(REPO_ROOT) not in sys.path:
    sys.path.insert(0, str(REPO_ROOT))

from scripts.reach_drive import (  # noqa: E402
    DEFAULT_API_URL,
    DEFAULT_MODEL,
    DriveResult,
    ReachAction,
    ReachDriver,
    StepRecord,
)

logger = logging.getLogger("reach_buzz_daemon")

DEFAULT_RELAY_URL = os.environ.get("BUZZ_RELAY_URL", "http://100.124.38.17:3000")
DEFAULT_WS_RELAY_URL = os.environ.get("BUZZ_WS_RELAY_URL", "ws://100.124.38.17:3000")
DEFAULT_NOVNC_BASE = os.environ.get(
    "REACH_NOVNC_URL", "http://100.124.38.17:6080/vnc.html?autoconnect=true"
)
DEFAULT_BOT_TRIGGER = "@ReachBot"
DEFAULT_SCREEN = 0
DEFAULT_TAKEOVER_TIMEOUT_SEC = 600
DEFAULT_POLL_INTERVAL_SEC = 2.0


# ---------------------------------------------------------------------------
# Message & Mention Parsing
# ---------------------------------------------------------------------------


@dataclass
class ParsedTask:
    """Parsed task request from a Buzz mention."""

    screen: int
    goal: str
    initial_url: Optional[str] = None
    raw_text: str = ""


# Screen indicators:
# "screen 1", "screen:1", "screen=1", "[screen 1]", "--screen 1", "display 1", "display:1"
SCREEN_PATTERN = re.compile(
    r"(?:\[\s*screen\s*[:=]?\s*(\d+)\s*\]|--screen\s*[:=]?\s*(\d+)|\bscreen\s*[:=]\s*(\d+)|\bscreen\s+(\d+)|\bdisplay\s*[:=]?\s*(\d+))",
    re.IGNORECASE,
)

# URL indicators: "--url https://...", "url:https://...", or standalone http(s) URL
URL_PARAM_PATTERN = re.compile(
    r"(?:--url\s*[:=]?\s*(\S+)|url\s*[:=]\s*(\S+))", re.IGNORECASE
)
STANDALONE_URL_PATTERN = re.compile(r"(https?://[^\s>]+)", re.IGNORECASE)


def parse_task_message(content: str, trigger: str = DEFAULT_BOT_TRIGGER) -> Optional[ParsedTask]:
    """Parse task goal, screen index, and optional initial URL from a Buzz mention.

    Returns None if content does not contain the bot trigger mention.
    """
    if not content:
        return None

    # Case-insensitive check for trigger
    lower_content = content.lower()
    lower_trigger = trigger.lower()
    if lower_trigger not in lower_content:
        return None

    # Strip the trigger mention
    clean_text = re.sub(re.escape(trigger), "", content, flags=re.IGNORECASE)

    # Extract target screen (default 0)
    screen = DEFAULT_SCREEN
    screen_match = SCREEN_PATTERN.search(clean_text)
    if screen_match:
        for group in screen_match.groups():
            if group is not None:
                try:
                    screen = int(group)
                    break
                except ValueError:
                    pass
        # Remove screen specifier from prompt
        clean_text = SCREEN_PATTERN.sub("", clean_text)

    # Extract initial URL
    initial_url: Optional[str] = None
    url_param_match = URL_PARAM_PATTERN.search(clean_text)
    if url_param_match:
        initial_url = url_param_match.group(1) or url_param_match.group(2)
        clean_text = URL_PARAM_PATTERN.sub("", clean_text)
    else:
        url_match = STANDALONE_URL_PATTERN.search(clean_text)
        if url_match:
            initial_url = url_match.group(1)

    # Clean leftover whitespace and punctuation
    goal = clean_text.strip(" \t\r\n:,-")
    # Collapse multiple spaces
    goal = re.sub(r"\s+", " ", goal).strip()

    if not goal and initial_url:
        goal = f"Open {initial_url} and inspect contents"

    if not goal:
        goal = "Explore screen and await instructions"

    return ParsedTask(
        screen=screen,
        goal=goal,
        initial_url=initial_url,
        raw_text=content,
    )


# ---------------------------------------------------------------------------
# Buzz Relay Client Wrappers
# ---------------------------------------------------------------------------


def find_buzz_cli() -> Optional[str]:
    """Locate buzz-cli binary in PATH or well-known locations."""
    cli = shutil.which("buzz-cli") or shutil.which("buzz")
    if cli:
        return cli
    for path in [
        os.path.expanduser("~/.local/bin/buzz-cli"),
        os.path.expanduser("~/.local/bin/buzz"),
        "/usr/local/bin/buzz-cli",
        "/usr/local/bin/buzz",
    ]:
        if os.path.isfile(path) and os.access(path, os.X_OK):
            return path
    return None


def run_buzz_cli(
    args: List[str],
    relay_url: Optional[str] = None,
    private_key: Optional[str] = None,
    timeout: float = 15.0,
) -> Dict[str, Any]:
    """Execute a buzz-cli command and return parsed JSON."""
    cli_path = find_buzz_cli()
    if not cli_path:
        return {
            "ok": False,
            "error": "buzz_cli_not_found",
            "message": "buzz-cli binary not found in PATH or ~/.local/bin",
        }

    relay = relay_url or os.environ.get("BUZZ_RELAY_URL", DEFAULT_RELAY_URL).rstrip("/")
    cmd = [cli_path, "--relay", relay, "--format", "json"]
    key = private_key or os.environ.get("BUZZ_PRIVATE_KEY")
    if key:
        cmd.extend(["--private-key", key])
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
    relay_url: Optional[str] = None,
    private_key: Optional[str] = None,
) -> Dict[str, Any]:
    """Send a message to a Buzz channel or thread."""
    args = ["messages", "send", "--channel", channel, "--content", content]
    if reply_to:
        args.extend(["--reply-to", str(reply_to)])
    if broadcast:
        args.append("--broadcast")
    return run_buzz_cli(args, relay_url=relay_url, private_key=private_key)


def buzz_send_takeover_alert(
    channel: str,
    screen: int,
    reason: str,
    novnc_url: Optional[str] = None,
    reply_to: Optional[str] = None,
    relay_url: Optional[str] = None,
    private_key: Optional[str] = None,
) -> Dict[str, Any]:
    """Post an interactive Human Takeover alert to a Buzz channel or thread."""
    url = novnc_url or DEFAULT_NOVNC_BASE
    content = (
        f"🚨 **Reach Human Takeover Required**\n\n"
        f"- **Screen**: Display `{screen}`\n"
        f"- **Reason**: {reason}\n"
        f"- **Interactive noVNC Link**: [{url}]({url})\n\n"
        f"👉 *Instructions*: Click the link above to interact with the screen. "
        f"When finished with 2FA / CAPTCHA, click the floating **[ Hand Back to Agent ]** banner "
        f"at the top of the display to resume autonomous execution."
    )
    return buzz_send_message(
        channel=channel,
        content=content,
        reply_to=reply_to,
        broadcast=True,
        relay_url=relay_url,
        private_key=private_key,
    )


def buzz_post_visual_diff(
    channel: str,
    summary: str,
    screenshot_path: Optional[str] = None,
    diff_percent: Optional[float] = None,
    tokens_saved: Optional[int] = None,
    reply_to: Optional[str] = None,
    relay_url: Optional[str] = None,
    private_key: Optional[str] = None,
) -> Dict[str, Any]:
    """Post a visual diff audit update to a Buzz channel or thread."""
    content_lines = ["📊 **Reach Visual Diff Audit**", f"- **Summary**: {summary}"]
    if diff_percent is not None:
        content_lines.append(f"- **pHash Change**: `{diff_percent:.2f}%`")
    if tokens_saved is not None:
        content_lines.append(f"- **VLM Tokens Saved**: `{tokens_saved}` tokens (gated via pHash)")

    if screenshot_path and os.path.exists(screenshot_path):
        upload_res = run_buzz_cli(["media", "upload", screenshot_path], relay_url=relay_url, private_key=private_key)
        if upload_res.get("ok") and isinstance(upload_res.get("data"), dict):
            media_url = upload_res["data"].get("url") or upload_res["data"].get("sha256")
            if media_url:
                content_lines.append(f"\n![Audit Screenshot]({media_url})")

    content = "\n".join(content_lines)
    return buzz_send_message(
        channel=channel,
        content=content,
        reply_to=reply_to,
        relay_url=relay_url,
        private_key=private_key,
    )


def buzz_get_messages(
    channel: str,
    limit: int = 20,
    since: Optional[int] = None,
    relay_url: Optional[str] = None,
    private_key: Optional[str] = None,
) -> Dict[str, Any]:
    """Retrieve messages from a Buzz channel."""
    args = ["messages", "get", "--channel", channel, "--limit", str(limit)]
    if since is not None:
        args.extend(["--since", str(since)])
    return run_buzz_cli(args, relay_url=relay_url, private_key=private_key)


def buzz_list_channels(
    relay_url: Optional[str] = None,
    private_key: Optional[str] = None,
) -> Dict[str, Any]:
    """List channels from the Buzz relay."""
    return run_buzz_cli(["channels", "list"], relay_url=relay_url, private_key=private_key)


# ---------------------------------------------------------------------------
# Reach API Client (Screen Lease & Handoff State Machine)
# ---------------------------------------------------------------------------


class ReachApiClient:
    """HTTP Client for Reach Screen Leasing and Handoff State Machine."""

    handoff_gen: Optional[int] = None

    def __init__(self, api_url: str = DEFAULT_API_URL, lease_token: Optional[str] = None) -> None:
        self.api_url = api_url.rstrip("/")
        self.lease_token = lease_token
        self.handoff_gen = None

    def lease_screen(self, screen: int, owner: str = "ReachBot") -> Dict[str, Any]:
        """POST /agent/screens/{screen}/lease."""
        url = f"{self.api_url}/agent/screens/{screen}/lease"
        payload = json.dumps({"owner": owner}).encode("utf-8")
        req = urllib.request.Request(
            url,
            data=payload,
            headers={"Content-Type": "application/json"},
            method="POST",
        )
        try:
            with urllib.request.urlopen(req, timeout=10) as resp:
                data = json.loads(resp.read().decode("utf-8") or "{}")
                if isinstance(data, dict):
                    if data.get("token"):
                        self.lease_token = data["token"]
                    if "handoff_gen" in data:
                        self.handoff_gen = int(data["handoff_gen"])
                return data
        except urllib.error.HTTPError as err:
            body = err.read().decode("utf-8", errors="replace")
            logger.error("Lease screen %s failed (HTTP %s): %s", screen, err.code, body)
            raise RuntimeError(f"HTTP {err.code}: {body}") from err

    def release_screen(
        self,
        screen: int,
        owner: str = "ReachBot",
        token: Optional[str] = None,
    ) -> Dict[str, Any]:
        """DELETE /agent/screens/{screen}/lease."""
        active_token = token or self.lease_token
        url = f"{self.api_url}/agent/screens/{screen}/lease"
        headers = {"Content-Type": "application/json"}
        if active_token:
            headers["X-Lease-Token"] = active_token

        payload = {"owner": owner}
        if active_token:
            payload["token"] = active_token

        req = urllib.request.Request(
            url,
            data=json.dumps(payload).encode("utf-8"),
            headers=headers,
            method="DELETE",
        )
        try:
            with urllib.request.urlopen(req, timeout=10) as resp:
                return json.loads(resp.read().decode("utf-8") or "{}")
        except Exception as exc:
            logger.warning("Release screen %s failed: %s", screen, exc)
            return {"error": str(exc)}

    def request_takeover(
        self,
        screen: int,
        reason: str,
        novnc_url: Optional[str] = None,
        token: Optional[str] = None,
    ) -> Dict[str, Any]:
        """POST /agent/screens/{screen}/takeover."""
        active_token = token or self.lease_token
        url = f"{self.api_url}/agent/screens/{screen}/takeover"
        headers = {"Content-Type": "application/json"}
        if active_token:
            headers["X-Lease-Token"] = active_token

        payload: Dict[str, Any] = {"pending": True, "reason": reason}
        if novnc_url:
            payload["url"] = novnc_url

        req = urllib.request.Request(
            url,
            data=json.dumps(payload).encode("utf-8"),
            headers=headers,
            method="POST",
        )
        try:
            with urllib.request.urlopen(req, timeout=10) as resp:
                data = json.loads(resp.read().decode("utf-8") or "{}")
                if isinstance(data, dict) and "handoff_gen" in data:
                    self.handoff_gen = int(data["handoff_gen"])
                return data
        except Exception as exc:
            logger.warning("Request takeover for screen %s failed: %s", screen, exc)
            return {"error": str(exc)}

    def wait_for_phase(
        self,
        screen: int,
        phase: str = "HumanDone",
        timeout: int = DEFAULT_TAKEOVER_TIMEOUT_SEC,
    ) -> Dict[str, Any]:
        """GET /agent/screens/{screen}/wait?phase={phase}&timeout={timeout}."""
        url = f"{self.api_url}/agent/screens/{screen}/wait?phase={urllib.parse.quote(phase)}&timeout={timeout}"
        req = urllib.request.Request(url, method="GET")
        try:
            with urllib.request.urlopen(req, timeout=timeout + 5) as resp:
                return json.loads(resp.read().decode("utf-8") or "{}")
        except Exception as exc:
            logger.error("Wait for screen %s phase %s failed: %s", screen, phase, exc)
            return {"status": "error", "error": str(exc)}

    def ack_handback(self, screen: int, token: Optional[str] = None) -> Dict[str, Any]:
        """POST /agent/screens/{screen}/ack."""
        active_token = token or self.lease_token
        url = f"{self.api_url}/agent/screens/{screen}/ack"
        headers = {"Content-Type": "application/json"}
        if active_token:
            headers["X-Lease-Token"] = active_token

        req = urllib.request.Request(
            url,
            data=json.dumps({}).encode("utf-8"),
            headers=headers,
            method="POST",
        )
        try:
            with urllib.request.urlopen(req, timeout=10) as resp:
                data = json.loads(resp.read().decode("utf-8") or "{}")
                if isinstance(data, dict) and "handoff_gen" in data:
                    self.handoff_gen = int(data["handoff_gen"])
                return data
        except Exception as exc:
            logger.error("Ack handback for screen %s failed: %s", screen, exc)
            return {"status": "error", "error": str(exc)}

    def get_novnc_url(self, screen: int) -> str:
        """Construct or resolve noVNC URL for target screen."""
        if os.environ.get("REACH_NOVNC_URL"):
            return os.environ["REACH_NOVNC_URL"]
        parsed = urllib.parse.urlparse(self.api_url)
        host = parsed.hostname or "100.124.38.17"
        port = 6080 + screen
        return f"http://{host}:{port}/vnc.html?autoconnect=true"


# ---------------------------------------------------------------------------
# Buzz Daemon Coordinator
# ---------------------------------------------------------------------------


class BuzzDaemon:
    """Continuous daemon listening to Buzz relay for Reach agent automation."""

    def __init__(
        self,
        relay_url: str = DEFAULT_RELAY_URL,
        ws_relay_url: str = DEFAULT_WS_RELAY_URL,
        api_url: str = DEFAULT_API_URL,
        trigger: str = DEFAULT_BOT_TRIGGER,
        default_screen: int = DEFAULT_SCREEN,
        channels: Optional[List[str]] = None,
        poll_interval: float = DEFAULT_POLL_INTERVAL_SEC,
        takeover_timeout: int = DEFAULT_TAKEOVER_TIMEOUT_SEC,
        novnc_base: str = DEFAULT_NOVNC_BASE,
        enable_visual_diff: bool = True,
        max_steps: int = 20,
        model: str = DEFAULT_MODEL,
        reach_client: Optional[ReachApiClient] = None,
        driver_factory: Optional[Callable[..., Any]] = None,
        private_key: Optional[str] = None,
        allowed_senders: Optional[List[str]] = None,
    ) -> None:
        self.relay_url = relay_url.rstrip("/")
        self.ws_relay_url = ws_relay_url.rstrip("/")
        self.api_url = api_url.rstrip("/")
        self.trigger = trigger
        self.default_screen = default_screen
        self.channels = channels or ["general"]
        self.poll_interval = poll_interval
        self.takeover_timeout = takeover_timeout
        self.novnc_base = novnc_base
        self.enable_visual_diff = enable_visual_diff
        self.max_steps = max_steps
        self.model = model
        self.private_key = private_key or os.environ.get("BUZZ_PRIVATE_KEY")
        self.reach_client = reach_client or ReachApiClient(api_url=self.api_url)
        self.driver_factory = driver_factory or self._default_driver_factory

        if allowed_senders is not None:
            self.allowed_senders: Optional[Set[str]] = {
                s.strip().lower() for s in allowed_senders if s.strip()
            }
        else:
            env_senders = os.environ.get("BUZZ_ALLOWED_SENDERS", "")
            if env_senders.strip():
                self.allowed_senders = {
                    s.strip().lower() for s in env_senders.split(",") if s.strip()
                }
            else:
                self.allowed_senders = None

        self.seen_message_ids: Set[str] = set()
        self.running = False

    def _default_driver_factory(
        self,
        screen: int,
        lease_token: Optional[str] = None,
        handoff_gen: Optional[int] = None,
        step_callback: Optional[Callable[[StepRecord], None]] = None,
    ) -> ReachDriver:
        return ReachDriver(
            api_url=self.api_url,
            screen=screen,
            model=self.model,
            max_steps=self.max_steps,
            lease_token=lease_token,
            handoff_gen=handoff_gen,
            step_callback=step_callback,
            enable_audit=True,
            interactive=False,
        )

    def resolve_novnc_url(self, screen: int) -> str:
        """Return noVNC interactive link for screen."""
        if self.reach_client:
            return self.reach_client.get_novnc_url(screen)
        return f"http://100.124.38.17:{6080 + screen}/vnc.html?autoconnect=true"

    def handle_takeover(
        self,
        channel: str,
        screen: int,
        reason: str,
        reply_to: Optional[str] = None,
        token: Optional[str] = None,
    ) -> bool:
        """Handle 2FA/CAPTCHA human takeover alert, wait, and handback loop.

        Returns True if human handed back control and was acknowledged, False otherwise.
        """
        novnc_url = self.resolve_novnc_url(screen)
        logger.warning(
            "Takeover required on screen %s (%s). Sending alert with %s",
            screen,
            reason,
            novnc_url,
        )

        # 1. Inform Reach agent state machine and retrieve takeover URL (with human_token if minted)
        takeover_res = self.reach_client.request_takeover(
            screen=screen,
            reason=reason,
            novnc_url=novnc_url,
            token=token,
        )
        if isinstance(takeover_res, dict) and takeover_res.get("takeover_url"):
            novnc_url = takeover_res["takeover_url"]

        # 2. Post takeover alert to Buzz thread with direct noVNC link
        buzz_send_takeover_alert(
            channel=channel,
            screen=screen,
            reason=reason,
            novnc_url=novnc_url,
            reply_to=reply_to,
            relay_url=self.relay_url,
            private_key=self.private_key,
        )

        # 3. Poll / wait for HumanDone phase
        logger.info(
            "Waiting for human handback on screen %s (timeout: %ss)...",
            screen,
            self.takeover_timeout,
        )
        wait_res = self.reach_client.wait_for_phase(
            screen=screen,
            phase="HumanDone",
            timeout=self.takeover_timeout,
        )

        if wait_res.get("phase") == "HumanDone" or wait_res.get("status") == "ok":
            logger.info("Human handed back screen %s! Sending ack...", screen)
            # 4. Send ack to transition from HumanDone -> AgentActive
            self.reach_client.ack_handback(screen=screen, token=token)

            # 5. Post resuming notification to Buzz thread
            buzz_send_message(
                channel=channel,
                content="Resuming automated execution...",
                reply_to=reply_to,
                relay_url=self.relay_url,
                private_key=self.private_key,
            )
            return True

        logger.error(
            "Takeover wait timed out or failed for screen %s: %s", screen, wait_res
        )
        return False

    def handle_message(self, message: Dict[str, Any]) -> Optional[DriveResult]:
        """Process an incoming Buzz message.

        Parses mention, posts ack reply, leases screen, executes driving loop,
        posts periodic visual diff audit updates, handles takeover if needed,
        releases screen, and posts final audit summary.
        """
        content = message.get("content") or ""
        msg_id = message.get("id") or message.get("event_id") or ""
        channel = (
            message.get("channel")
            or message.get("channel_id")
            or (self.channels[0] if self.channels else "general")
        )

        if msg_id:
            self.seen_message_ids.add(str(msg_id))

        task = parse_task_message(content, trigger=self.trigger)
        if not task:
            return None

        sender = (
            message.get("sender")
            or message.get("pubkey")
            or message.get("author")
            or message.get("user")
            or message.get("from")
            or ""
        ).strip()

        # Sender allowlist verification
        if self.allowed_senders is not None:
            if not sender or sender.lower() not in self.allowed_senders:
                logger.warning(
                    "Rejected message %s from unauthorized sender '%s' (allowed: %s)",
                    msg_id,
                    sender,
                    self.allowed_senders,
                )
                buzz_send_message(
                    channel=channel,
                    content="⛔ Unauthorized sender. You are not in the BUZZ_ALLOWED_SENDERS allowlist.",
                    reply_to=msg_id,
                    relay_url=self.relay_url,
                    private_key=self.private_key,
                )
                return None

        # Forbid chat-initiated goals from requesting mutating capabilities (exec, card, vault)
        mutating_patterns = [
            r"\bexec\b",
            r"\bcard\b",
            r"\bvault\b",
            r"\bcard_mint\b",
            r"\bcard_inject\b",
            r"\bvault_inject\b",
            r"\bcredit\s*card\b",
            r"\bpayment\b",
            r"\bcheckout\b",
        ]
        lower_goal = task.goal.lower()
        if any(re.search(pat, lower_goal) for pat in mutating_patterns):
            logger.warning(
                "Rejected chat-initiated goal requesting mutating tool: %s", task.goal
            )
            buzz_send_message(
                channel=channel,
                content="⛔ Security restriction: Mutating tools (exec, card, vault) cannot be invoked from chat-initiated goals. Please run these operations directly from the Reach CLI or authorized supervisor.",
                reply_to=msg_id,
                relay_url=self.relay_url,
                private_key=self.private_key,
            )
            return None

        logger.info(
            "Mentions detected in message %s (channel %s): screen=%s, goal=%s",
            msg_id,
            channel,
            task.screen,
            task.goal,
        )

        # 1. Post immediate acknowledgment reply in the Buzz message thread
        ack_reply = f"🐝 On it! Leased screen {task.screen} and beginning execution..."
        buzz_send_message(
            channel=channel,
            content=ack_reply,
            reply_to=msg_id,
            relay_url=self.relay_url,
            private_key=self.private_key,
        )

        lease_token: Optional[str] = None
        driver_result: Optional[DriveResult] = None

        try:
            # 2. Lease target screen from Reach API
            lease_data = self.reach_client.lease_screen(task.screen, owner="ReachBot")
            lease_token = lease_data.get("token")
            logger.info("Screen %s leased successfully (token: %s)", task.screen, lease_token)

            # 3. Setup step callback for periodic visual diff audit updates
            def on_step_callback(step: StepRecord) -> None:
                if not self.enable_visual_diff:
                    return
                diff_pct = (
                    step.visual_change * 100.0 if step.visual_change is not None else None
                )
                tokens_saved = 1600 if getattr(step, "vlm_cached", False) else None
                desc = step.action.description or step.action.kind
                step_summary = f"Step {step.step_index}: {desc}"
                try:
                    buzz_post_visual_diff(
                        channel=channel,
                        summary=step_summary,
                        screenshot_path=step.screenshot_path,
                        diff_percent=diff_pct,
                        tokens_saved=tokens_saved,
                        reply_to=msg_id,
                        relay_url=self.relay_url,
                        private_key=self.private_key,
                    )
                except Exception as post_err:
                    logger.warning("Failed to post visual diff update: %s", post_err)

            # 4. Invoke driving loop
            handoff_gen = getattr(self.reach_client, "handoff_gen", None)
            try:
                driver = self.driver_factory(
                    screen=task.screen,
                    lease_token=lease_token,
                    handoff_gen=handoff_gen,
                    step_callback=on_step_callback,
                )
            except TypeError:
                driver = self.driver_factory(
                    screen=task.screen,
                    lease_token=lease_token,
                    step_callback=on_step_callback,
                )
            driver_result = driver.drive(goal=task.goal, initial_url=task.initial_url)

            # 5. Interactive takeover integration
            if driver_result and driver_result.status == "auth_required":
                handback_success = self.handle_takeover(
                    channel=channel,
                    screen=task.screen,
                    reason=driver_result.final_description or "Human login / 2FA required",
                    reply_to=msg_id,
                    token=lease_token,
                )
                if handback_success:
                    # Resume execution after handback
                    logger.info("Resuming CUA execution post-handback for goal: %s", task.goal)
                    resumed_gen = getattr(self.reach_client, "handoff_gen", None)
                    try:
                        resume_driver = self.driver_factory(
                            screen=task.screen,
                            lease_token=lease_token,
                            handoff_gen=resumed_gen,
                            step_callback=on_step_callback,
                        )
                    except TypeError:
                        resume_driver = self.driver_factory(
                            screen=task.screen,
                            lease_token=lease_token,
                            step_callback=on_step_callback,
                        )
                    driver_result = resume_driver.drive(
                        goal=f"Complete remaining tasks for: {task.goal}"
                    )

        except Exception as loop_err:
            logger.error("Error executing task for message %s: %s", msg_id, loop_err, exc_info=True)
            if not driver_result:
                driver_result = DriveResult(
                    success=False,
                    status="failed",
                    steps=[],
                    error=str(loop_err),
                    final_description="Execution error occurred",
                )
        finally:
            # 6. Release leased screen
            if lease_token:
                logger.info("Releasing lease for screen %s", task.screen)
                self.reach_client.release_screen(
                    screen=task.screen,
                    owner="ReachBot",
                    token=lease_token,
                )

            # 7. Post final summary and link to visual audit report
            status_symbol = "✅" if (driver_result and driver_result.success) else "⚠️"
            status_text = driver_result.status if driver_result else "unknown"
            step_count = len(driver_result.steps) if driver_result else 0
            report_url = (
                driver_result.audit_report_path
                if (driver_result and driver_result.audit_report_path)
                else None
            )

            summary_lines = [
                f"{status_symbol} **Reach Task {status_text.replace('_', ' ').title()}**",
                f"- **Goal**: {task.goal}",
                f"- **Status**: `{status_text}`",
                f"- **Steps Executed**: {step_count}",
            ]
            if report_url:
                summary_lines.append(f"- **Visual Audit Report**: [{report_url}]({report_url})")
            if driver_result and driver_result.final_description:
                summary_lines.append(f"- **Outcome**: {driver_result.final_description}")
            if driver_result and driver_result.error:
                summary_lines.append(f"- **Error**: {driver_result.error}")

            final_message = "\n".join(summary_lines)
            buzz_send_message(
                channel=channel,
                content=final_message,
                reply_to=msg_id,
                relay_url=self.relay_url,
                private_key=self.private_key,
            )

        return driver_result

    def poll_once(self) -> List[Dict[str, Any]]:
        """Poll channels once and process any new @ReachBot messages."""
        processed_results: List[Dict[str, Any]] = []

        # Auto-discover channels if 'all' or empty
        target_channels = self.channels
        if not target_channels or target_channels == ["all"]:
            chan_res = buzz_list_channels(relay_url=self.relay_url, private_key=self.private_key)
            if chan_res.get("ok") and isinstance(chan_res.get("data"), list):
                target_channels = [
                    c.get("id") or c.get("name")
                    for c in chan_res["data"]
                    if isinstance(c, dict) and (c.get("id") or c.get("name"))
                ]
            else:
                target_channels = ["general"]

        for chan in target_channels:
            res = buzz_get_messages(
                channel=chan,
                limit=25,
                relay_url=self.relay_url,
                private_key=self.private_key,
            )
            if not res.get("ok"):
                continue
            data = res.get("data", [])
            messages: List[Dict[str, Any]] = []
            if isinstance(data, list):
                messages = data
            elif isinstance(data, dict) and "messages" in data:
                messages = data["messages"]

            for msg in messages:
                if not isinstance(msg, dict):
                    continue
                mid = str(msg.get("id") or msg.get("event_id") or "")
                if mid and mid in self.seen_message_ids:
                    continue
                if mid:
                    self.seen_message_ids.add(mid)
                # If message contains our trigger, handle it
                content = msg.get("content") or ""
                if self.trigger.lower() in content.lower():
                    result = self.handle_message(msg)
                    processed_results.append({"message_id": mid, "result": result})

        return processed_results

    def run(self, run_once: bool = False) -> None:
        """Run the daemon continuously or once."""
        self.running = True
        logger.info(
            "Starting Reach Buzz Daemon (trigger: %s, relay: %s, channels: %s)",
            self.trigger,
            self.relay_url,
            self.channels,
        )

        def sig_handler(sig: int, frame: Any) -> None:
            logger.info("Termination signal %s received, stopping daemon...", sig)
            self.running = False

        signal.signal(signal.SIGINT, sig_handler)
        signal.signal(signal.SIGTERM, sig_handler)

        try:
            while self.running:
                try:
                    self.poll_once()
                except Exception as poll_err:
                    logger.warning("Error during poll cycle: %s", poll_err)

                if run_once or not self.running:
                    break
                time.sleep(self.poll_interval)
        finally:
            logger.info("Reach Buzz Daemon stopped.")


# ---------------------------------------------------------------------------
# CLI Entrypoint
# ---------------------------------------------------------------------------


def main() -> None:
    parser = argparse.ArgumentParser(
        description="Reach Live Buzz Agent Daemon (@ReachBot Continuous Listener)"
    )
    parser.add_argument(
        "--relay",
        default=DEFAULT_RELAY_URL,
        help=f"Buzz relay HTTP URL (default {DEFAULT_RELAY_URL})",
    )
    parser.add_argument(
        "--ws-relay",
        default=DEFAULT_WS_RELAY_URL,
        help=f"Buzz relay WebSocket URL (default {DEFAULT_WS_RELAY_URL})",
    )
    parser.add_argument(
        "--api-url",
        default=DEFAULT_API_URL,
        help=f"Reach Agent API URL (default {DEFAULT_API_URL})",
    )
    parser.add_argument(
        "--trigger",
        default=DEFAULT_BOT_TRIGGER,
        help=f"Bot mention trigger string (default {DEFAULT_BOT_TRIGGER})",
    )
    parser.add_argument(
        "--channel",
        action="append",
        dest="channels",
        help="Channel ID or name to monitor (can specify multiple)",
    )
    parser.add_argument(
        "--screen",
        type=int,
        default=DEFAULT_SCREEN,
        help=f"Default screen index (default {DEFAULT_SCREEN})",
    )
    parser.add_argument(
        "--model",
        default=DEFAULT_MODEL,
        help=f"Model ID for Reach CUA Driver (default {DEFAULT_MODEL})",
    )
    parser.add_argument(
        "--max-steps",
        type=int,
        default=20,
        help="Maximum CUA steps per task (default 20)",
    )
    parser.add_argument(
        "--poll-interval",
        type=float,
        default=DEFAULT_POLL_INTERVAL_SEC,
        help=f"Polling interval in seconds (default {DEFAULT_POLL_INTERVAL_SEC})",
    )
    parser.add_argument(
        "--takeover-timeout",
        type=int,
        default=DEFAULT_TAKEOVER_TIMEOUT_SEC,
        help=f"Seconds to wait for human handback during 2FA (default {DEFAULT_TAKEOVER_TIMEOUT_SEC})",
    )
    parser.add_argument(
        "--no-visual-diff",
        action="store_false",
        dest="enable_visual_diff",
        help="Disable posting visual diff audit updates to Buzz thread",
    )
    parser.add_argument(
        "--once",
        action="store_true",
        help="Poll once and exit",
    )
    parser.add_argument(
        "-v",
        "--verbose",
        action="store_true",
        help="Enable verbose debug logging",
    )

    args = parser.parse_args()
    log_level = logging.DEBUG if args.verbose else logging.INFO
    logging.basicConfig(
        level=log_level, format="%(asctime)s [%(levelname)s] %(message)s"
    )

    daemon = BuzzDaemon(
        relay_url=args.relay,
        ws_relay_url=args.ws_relay,
        api_url=args.api_url,
        trigger=args.trigger,
        default_screen=args.screen,
        channels=args.channels,
        poll_interval=args.poll_interval,
        takeover_timeout=args.takeover_timeout,
        enable_visual_diff=args.enable_visual_diff,
        max_steps=args.max_steps,
        model=args.model,
    )
    daemon.run(run_once=args.once)


if __name__ == "__main__":
    main()
