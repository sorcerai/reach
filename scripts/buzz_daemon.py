#!/usr/bin/env python3
"""Reach Buzz Agent Daemon (@ReachBot Continuous Listener).

Listens on Buzz relay channels for `@ReachBot` mentions, leases screens from
the Reach agent API, dispatches CUA vision driving loops, posts visual diff
audit updates, and handles interactive 2FA/CAPTCHA takeover and handback.
"""

from __future__ import annotations

import argparse
import hashlib
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
    _handoff_ack_generation,
    _lease_receipt_fields,
)
from scripts.reach_sensitive import safe_url

logger = logging.getLogger("reach_buzz_daemon")

DEFAULT_RELAY_URL = os.environ.get("BUZZ_RELAY_URL", "http://100.124.38.17:3000")
DEFAULT_WS_RELAY_URL = os.environ.get("BUZZ_WS_RELAY_URL", "ws://100.124.38.17:3000")
DEFAULT_BOT_TRIGGER = "@ReachBot"
DEFAULT_SCREEN = 0
DEFAULT_TAKEOVER_TIMEOUT_SEC = 600
DEFAULT_POLL_INTERVAL_SEC = 2.0


# ---------------------------------------------------------------------------
# Message & Mention Parsing
# ---------------------------------------------------------------------------


@dataclass
class ParsedTask:
    """Parsed Buzz task contract.

    A task is executable only when it is explicitly observation-only or carries
    a non-empty completion criterion.  ``completion_text`` is intentionally
    kept separate from the goal: it is a postcondition, not a conversational
    promise.
    """

    screen: int
    goal: str
    initial_url: Optional[str] = None
    raw_text: str = ""
    completion_text: Optional[str] = None
    observation_only: bool = False
    task_id: Optional[str] = None
    attempt_id: Optional[str] = None

    @property
    def success_criterion(self) -> Optional[str]:
        """Alias used by callers that describe the contract semantically."""
        return self.completion_text

    @property
    def contract_status(self) -> str:
        if self.observation_only or (self.completion_text and self.completion_text.strip()):
            return "ready"
        return "clarification_required"


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

OBSERVATION_ONLY_PATTERN = re.compile(
    r"(?:\[\s*)?(?:observation[-_ ]only|observe[-_ ]only|read[-_ ]only|"
    r"mode\s*[:=]\s*observe)(?:\s*\])?",
    re.IGNORECASE,
)
COMPLETION_PATTERN = re.compile(
    r"(?:\b(?:success(?:_criterion)?|completion(?:_text)?|complete|done|"
    r"criterion|criteria|verify)\s*(?:criterion|text|when|is)?\s*[:=]\s*)"
    r"(?P<quote>[\"']?)(?P<value>[^\"';|\n]+?)(?P=quote)(?=\s*(?:[;|]|$))",
    re.IGNORECASE,
)


def parse_task_message(content: str, trigger: str = DEFAULT_BOT_TRIGGER) -> Optional[ParsedTask]:
    """Parse a Buzz mention into an explicit task contract.

    Free-form mentions remain parseable for routing, but are marked
    ``clarification_required`` unless they opt into observation-only mode or
    provide a quoted/delimited completion criterion.  In particular, words
    such as ``approve`` are ordinary goal text and never grant consent.
    """
    if not content:
        return None

    if trigger.lower() not in content.lower():
        return None

    clean_text = re.sub(re.escape(trigger), "", content, flags=re.IGNORECASE)
    screen = DEFAULT_SCREEN
    screen_match = SCREEN_PATTERN.search(clean_text)
    if screen_match:
        for group in screen_match.groups():
            if group is not None:
                screen = int(group)
                break
        clean_text = SCREEN_PATTERN.sub("", clean_text)

    initial_url: Optional[str] = None
    url_param_match = URL_PARAM_PATTERN.search(clean_text)
    if url_param_match:
        initial_url = url_param_match.group(1) or url_param_match.group(2)
        clean_text = URL_PARAM_PATTERN.sub("", clean_text)
    else:
        url_match = STANDALONE_URL_PATTERN.search(clean_text)
        if url_match:
            initial_url = url_match.group(1)

    observation_only = bool(OBSERVATION_ONLY_PATTERN.search(clean_text))
    clean_text = OBSERVATION_ONLY_PATTERN.sub("", clean_text)

    completion_text: Optional[str] = None
    completion_match = COMPLETION_PATTERN.search(clean_text)
    if completion_match:
        completion_text = completion_match.group("value").strip()
        clean_text = (
            clean_text[: completion_match.start()]
            + " "
            + clean_text[completion_match.end() :]
        )
        if not completion_text:
            completion_text = None

    goal = clean_text.strip(" \t\r\n:,-;|")
    goal = re.sub(r"\s+", " ", goal).strip()
    if not goal and initial_url:
        goal = f"Open {initial_url} and inspect contents"

    return ParsedTask(
        screen=screen,
        goal=goal,
        initial_url=initial_url,
        raw_text=content,
        completion_text=completion_text,
        observation_only=observation_only,
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
    api_url: Optional[str] = None,
    reply_to: Optional[str] = None,
    relay_url: Optional[str] = None,
    private_key: Optional[str] = None,
) -> Dict[str, Any]:
    """Post a fixed Human Takeover alert with the authenticated viewer URL."""
    viewer_origin = safe_url(api_url or DEFAULT_API_URL)
    if not viewer_origin.startswith(("http://", "https://")):
        raise ValueError("invalid Reach API origin")
    url = f"{viewer_origin}/viewer/{screen}"
    content = (
        "🚨 **Reach Human Takeover Required**\n\n"
        f"- **Screen**: Display `{screen}`\n"
        "- **Reason**: Authentication or human verification required\n"
        f"- **Interactive noVNC Link**: [{url}]({url})\n\n"
        "👉 *Instructions*: Use the authenticated viewer to complete the required "
        "human verification, then click the floating **[ Hand Back to Agent ]** "
        "banner at the top of the display to resume."
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
    *,
    step_index: int,
    action_kind: str,
    outcome: str,
    diff_percent: Optional[float] = None,
    tokens_saved: Optional[int] = None,
    reply_to: Optional[str] = None,
    relay_url: Optional[str] = None,
    private_key: Optional[str] = None,
) -> Dict[str, Any]:
    """Post fixed structured step metadata; never upload or link raw media."""
    allowed_actions = {
        "click", "type", "key", "navigate", "inject", "wait", "scroll",
        "auth_required", "terminate",
    }
    allowed_outcomes = {
        "completed", "blocked", "failed", "uncertain", "vlm_cached",
        "approval_required", "auth_required", "postcondition_failed",
    }
    action = action_kind if action_kind in allowed_actions else "unknown"
    status = outcome if outcome in allowed_outcomes else "unknown"
    content_lines = [
        "📊 **Reach Step Audit**",
        f"- **Step**: `{int(step_index)}`",
        f"- **Action**: `{action}`",
        f"- **Outcome**: `{status}`",
    ]
    if diff_percent is not None:
        content_lines.append(f"- **pHash Change**: `{diff_percent:.2f}%`")
    if tokens_saved is not None:
        content_lines.append(f"- **VLM Tokens Saved**: `{int(tokens_saved)}`")
    return buzz_send_message(
        channel=channel,
        content="\n".join(content_lines),
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


class _NoRedirectHandler(urllib.request.HTTPRedirectHandler):
    """Refuse every redirect so lease credentials never leave the origin."""

    def redirect_request(self, req, fp, code, msg, headers, newurl):
        raise urllib.error.HTTPError(
            req.full_url,
            code,
            f"{msg} (refusing redirect of lease-capability-bearing request)",
            headers,
            fp,
        )


_OPENER = urllib.request.build_opener(_NoRedirectHandler())


def _supervisor_token(auth_token: Optional[str]) -> Optional[str]:
    """Resolve the supervisor allocation credential from constructor or env.

    Trusted operator metadata only; never logged, never forwarded to any
    endpoint other than lease allocation.
    """
    token = auth_token or os.environ.get("REACH_AUTH_TOKEN")
    return token if token and token.strip() else None


class ReachApiClient:
    """HTTP Client for Reach Screen Leasing and Handoff State Machine."""

    handoff_gen: Optional[int] = None

    def __init__(
        self,
        api_url: str = DEFAULT_API_URL,
        lease_token: Optional[str] = None,
        auth_token: Optional[str] = None,
    ) -> None:
        self.api_url = api_url.rstrip("/")
        self.lease_token = lease_token
        # Supervisor credential: allocation only; ordinary requests carry
        # the retained lease capability (X-Lease-Token) instead.
        self.auth_token = _supervisor_token(auth_token)
        self.handoff_gen = None
        self._reconciliation_token: Optional[str] = None
        self.last_lease_cleanup: Optional[Dict[str, str]] = None

    def lease_screen(self, screen: int, owner: str = "ReachBot") -> Dict[str, Any]:
        """POST /agent/screens/{screen}/lease (creation-only allocation).

        The optional supervisor bearer authorizes this allocation call only.
        An occupied screen — including one held under the same owner label —
        is refused by the server; there is no same-owner recovery path and a
        failed lease installs no capability or generation.
        """
        self.last_lease_cleanup = None
        url = f"{self.api_url}/agent/screens/{screen}/lease"
        payload = json.dumps({"owner": owner}).encode("utf-8")
        headers = {"Content-Type": "application/json"}
        if self.auth_token:
            headers["Authorization"] = f"Bearer {self.auth_token}"
        req = urllib.request.Request(
            url,
            data=payload,
            headers=headers,
            method="POST",
        )
        try:
            with _OPENER.open(req, timeout=10) as resp:
                try:
                    data = json.loads(resp.read().decode("utf-8") or "{}")
                except (TypeError, UnicodeError, ValueError):
                    self.last_lease_cleanup = {"status": "uncertain"}
                    raise RuntimeError(
                        "Lease outcome uncertain; reconciliation required"
                    ) from None
                candidate_token = data.get("token") if isinstance(data, dict) else None
                try:
                    token, generation = _lease_receipt_fields(data)
                except ValueError:
                    cleanup_status = "uncertain"
                    if isinstance(candidate_token, str) and candidate_token.strip():
                        self._reconciliation_token = candidate_token
                        cleanup = self.release_screen(
                            screen, owner=owner, token=candidate_token
                        )
                        cleanup_status = (
                            "confirmed"
                            if isinstance(cleanup, dict)
                            and cleanup.get("released") is True
                            else "uncertain"
                        )
                    self.last_lease_cleanup = {"status": cleanup_status}
                    raise RuntimeError(
                        f"Invalid lease receipt; cleanup={cleanup_status}"
                    ) from None
                self.lease_token = token
                self.handoff_gen = generation
                self._reconciliation_token = None
                self.last_lease_cleanup = None
                return data
        except urllib.error.HTTPError as err:
            if err.code == 408 or err.code >= 500:
                self.last_lease_cleanup = {"status": "uncertain"}
                logger.error("Lease screen outcome uncertain; reconciliation required")
                raise RuntimeError(
                    "Lease outcome uncertain; reconciliation required"
                ) from None
            self.last_lease_cleanup = {"status": "not_required"}
            logger.error("Lease screen %s failed (HTTP %s)", screen, err.code)
            raise RuntimeError(f"HTTP {err.code}") from err
        except RuntimeError:
            raise
        except Exception:
            self.last_lease_cleanup = {"status": "uncertain"}
            logger.error("Lease screen outcome uncertain; reconciliation required")
            raise RuntimeError(
                "Lease outcome uncertain; reconciliation required"
            ) from None

    def release_screen(
        self,
        screen: int,
        owner: str = "ReachBot",
        token: Optional[str] = None,
    ) -> Dict[str, Any]:
        """Release using the retained capability; clear only on confirmation."""
        active_token = token or self.lease_token or self._reconciliation_token
        url = f"{self.api_url}/agent/screens/{screen}/lease"
        headers = {"Content-Type": "application/json"}
        if active_token:
            headers["X-Lease-Token"] = active_token
        req = urllib.request.Request(
            url,
            data=json.dumps({"owner": owner}).encode("utf-8"),
            headers=headers,
            method="DELETE",
        )
        try:
            with _OPENER.open(req, timeout=10) as resp:
                result = json.loads(resp.read().decode("utf-8") or "{}")
                if isinstance(result, dict) and result.get("released") is True:
                    self.lease_token = None
                    self.handoff_gen = None
                    self._reconciliation_token = None
                return result
        except Exception as exc:
            logger.warning("Release screen %s failed: %s", screen, exc)
            return {"error": str(exc)}

    def request_takeover(
        self,
        screen: int,
        reason: str,
        token: Optional[str] = None,
    ) -> Dict[str, Any]:
        """POST /agent/screens/{screen}/takeover with the lease capability."""
        active_token = token or self.lease_token
        url = f"{self.api_url}/agent/screens/{screen}/takeover"
        headers = {"Content-Type": "application/json"}
        if active_token:
            headers["X-Lease-Token"] = active_token

        payload: Dict[str, Any] = {"pending": True, "reason": reason}
        req = urllib.request.Request(
            url,
            data=json.dumps(payload).encode("utf-8"),
            headers=headers,
            method="POST",
        )
        try:
            with _OPENER.open(req, timeout=10) as resp:
                return json.loads(resp.read().decode("utf-8") or "{}")
        except Exception as exc:
            logger.warning("Request takeover for screen %s failed: %s", screen, exc)
            return {"error": str(exc)}

    def wait_for_phase(
        self,
        screen: int,
        phase: str = "HumanDone",
        timeout: int = DEFAULT_TAKEOVER_TIMEOUT_SEC,
    ) -> Dict[str, Any]:
        """Observe the authorized phase without adopting its handoff generation."""
        url = f"{self.api_url}/agent/screens/{screen}/wait?phase={urllib.parse.quote(phase)}&timeout={timeout}"
        headers = {}
        if self.lease_token:
            headers["X-Lease-Token"] = self.lease_token
        elif self.auth_token:
            headers["Authorization"] = f"Bearer {self.auth_token}"
        req = urllib.request.Request(url, headers=headers, method="GET")
        try:
            with _OPENER.open(req, timeout=timeout + 5) as resp:
                return json.loads(resp.read().decode("utf-8") or "{}")
        except Exception as exc:
            logger.error("Wait for screen %s phase %s failed: %s", screen, phase, exc)
            return {"status": "error", "error": str(exc)}

    def ack_handback(self, screen: int, token: Optional[str] = None) -> Dict[str, Any]:
        """POST /agent/screens/{screen}/ack with the lease capability."""
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
        previous_generation = self.handoff_gen
        try:
            with _OPENER.open(req, timeout=10) as resp:
                data = json.loads(resp.read().decode("utf-8") or "{}")
                generation = _handoff_ack_generation(data, previous_generation)
        except Exception:
            logger.error("Ack handback for screen %s failed", screen)
            return {"status": "error", "error": "acknowledgement failed"}
        self.handoff_gen = generation
        return data



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
        enable_visual_diff: bool = True,
        max_steps: int = 20,
        model: str = DEFAULT_MODEL,
        reach_client: Optional[ReachApiClient] = None,
        driver_factory: Optional[Callable[..., Any]] = None,
        private_key: Optional[str] = None,
        allowed_senders: Optional[List[str]] = None,
        reach_auth_token: Optional[str] = None,
    ) -> None:
        self.relay_url = relay_url.rstrip("/")
        self.ws_relay_url = ws_relay_url.rstrip("/")
        self.api_url = api_url.rstrip("/")
        self.trigger = trigger
        self.default_screen = default_screen
        self.channels = channels or ["general"]
        self.poll_interval = poll_interval
        self.takeover_timeout = takeover_timeout
        self.enable_visual_diff = enable_visual_diff
        self.max_steps = max_steps
        self.model = model
        self.private_key = private_key or os.environ.get("BUZZ_PRIVATE_KEY")
        self.reach_client = reach_client or ReachApiClient(
            api_url=self.api_url, auth_token=reach_auth_token
        )
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
        lease_token: Optional[str],
        handoff_gen: Optional[int],
        step_callback: Optional[Callable[[StepRecord], None]],
        task_id: str,
        attempt_id: str,
        completion_text: Optional[str],
    ) -> ReachDriver:
        return ReachDriver(
            api_url=self.api_url,
            screen=screen,
            model=self.model,
            max_steps=self.max_steps,
            lease_token=lease_token,
            handoff_gen=handoff_gen,
            step_callback=step_callback,
            completion_text=completion_text,
            task_id=task_id,
            attempt_id=attempt_id,
            enable_audit=True,

        )


    def handle_takeover(
        self,
        channel: str,
        screen: int,
        reason: str,
        reply_to: Optional[str] = None,
        token: Optional[str] = None,
    ) -> bool:
        """Handle human takeover and resume only after a validated handback."""
        logger.warning(
            "Takeover required on screen %s (%s); viewer origin is configured API",
            screen,
            reason,
        )

        takeover_res = self.reach_client.request_takeover(
            screen=screen,
            reason=reason,
            token=token,
        )
        if not (
            isinstance(takeover_res, dict)
            and takeover_res.get("status") == "ok"
        ):
            logger.error("Takeover request failed on screen %s", screen)
            return False

        # 2. Post a fixed safe alert; the URL is derived from our API origin.
        buzz_send_takeover_alert(
            channel=channel,
            screen=screen,
            reason=reason,
            api_url=self.api_url,
            reply_to=reply_to,
            relay_url=self.relay_url,
            private_key=self.private_key,
        )

        # 3. Poll / wait for the exact HumanDone phase.
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
        if not (
            isinstance(wait_res, dict)
            and wait_res.get("status") == "ok"
            and wait_res.get("phase") == "HumanDone"
        ):
            logger.error(
                "Takeover wait did not confirm HumanDone on screen %s (status=%s, phase=%s)",
                screen,
                wait_res.get("status") if isinstance(wait_res, dict) else "invalid",
                wait_res.get("phase") if isinstance(wait_res, dict) else "invalid",
            )
            return False

        logger.info("Human handed back screen %s; sending ack", screen)
        previous_generation = getattr(self.reach_client, "handoff_gen", None)
        ack_res = self.reach_client.ack_handback(screen=screen, token=token)
        try:
            ack_generation = _handoff_ack_generation(
                ack_res, previous_generation
            )
        except ValueError:
            logger.error("Ack did not confirm AgentActive on screen %s", screen)
            return False
        if getattr(self.reach_client, "handoff_gen", None) != ack_generation:
            logger.error("Ack generation was not installed on screen %s", screen)
            return False

        # 5. Post resuming notification only after validated handback.
        buzz_send_message(
            channel=channel,
            content="Resuming automated execution...",
            reply_to=reply_to,
            relay_url=self.relay_url,
            private_key=self.private_key,
        )
        return True
    def _task_identity(
        self, message: Dict[str, Any], channel: str, content: str
    ) -> Tuple[str, str]:
        """Bind driver identity to the source message and its attempt."""
        raw_message_id = message.get("id") or message.get("event_id")
        if raw_message_id:
            task_id = str(message.get("task_id") or raw_message_id)
        else:
            digest = hashlib.sha256(
                f"{channel}\0{content}".encode("utf-8")
            ).hexdigest()[:20]
            task_id = str(message.get("task_id") or f"buzz-{digest}")
        attempt_id = str(
            message.get("attempt_id")
            or message.get("task_attempt_id")
            or message.get("attempt")
            or "1"
        )
        return task_id, attempt_id

    def _clarification_result(
        self, task: ParsedTask, task_id: str, attempt_id: str
    ) -> DriveResult:
        return DriveResult(
            success=False,
            status="clarification_required",
            steps=[],
            task_id=task_id,
            final_description=(
                "Provide a success criterion, or explicitly mark the request "
                "observation-only."
            ),
            metrics={
                "task_id": task_id,
                "attempt_id": attempt_id,
                "model_requested": "unknown",
                "model_reported": "unknown",
                "model_version": "unknown",
            },
        )

    @staticmethod
    def _metric_receipts(result: Optional[DriveResult]) -> Tuple[str, str, str]:
        metrics = result.metrics if result and isinstance(result.metrics, dict) else {}
        model_metrics = metrics.get("model")
        if not isinstance(model_metrics, dict):
            model_metrics = metrics.get("model_receipt")
        nested = model_metrics if isinstance(model_metrics, dict) else {}

        def value(*keys: str) -> str:
            for key in keys:
                candidate = metrics.get(key, nested.get(key))
                if candidate is not None and str(candidate).strip():
                    return str(candidate)
            return "unknown"

        return (
            value("model_requested", "requested_model", "requested"),
            value("model_reported", "reported_model", "reported"),
            value("model_version", "version", "provider_version"),
        )

    def _result_projection(
        self,
        task: ParsedTask,
        result: Optional[DriveResult],
        cleanup: Optional[Dict[str, str]] = None,
    ) -> List[str]:
        requested, reported, version = self._metric_receipts(result)
        if result is None:
            status_text = "unknown"
            success = False
            steps = 0
        else:
            status_text = result.status or "unknown"
            if not result.success and status_text == "completed":
                status_text = "unverified"
            success = bool(result.success and status_text == "completed")
            steps = len(result.steps)

        status_symbol = "✅" if success else "⚠️"
        lines = [
            f"{status_symbol} **Reach Task {status_text.replace('_', ' ').title()}**",
            f"- **Status**: `{status_text}`",
            f"- **Steps Executed**: {steps}",
            f"- **Model Requested**: `{requested}`",
            f"- **Model Reported**: `{reported}`",
            f"- **Model Version**: `{version}`",
        ]
        if cleanup:
            cleanup_status = cleanup.get("status", "unknown")
            if cleanup_status not in {"confirmed", "uncertain", "not_required"}:
                cleanup_status = "unknown"
            lines.append(f"- **Lease Cleanup**: `{cleanup_status}`")
            if cleanup_status == "uncertain":
                lines.append("- **Reconciliation**: `required`")
        return lines



    def handle_message(self, message: Dict[str, Any]) -> Optional[DriveResult]:
        """Project one Buzz message into a bounded, explicit task attempt."""
        content = message.get("content") or ""
        msg_id = str(message.get("id") or message.get("event_id") or "")
        channel = (
            message.get("channel")
            or message.get("channel_id")
            or (self.channels[0] if self.channels else "general")
        )

        if msg_id:
            self.seen_message_ids.add(msg_id)

        task = parse_task_message(content, trigger=self.trigger)
        if not task:
            return None
        task_id, attempt_id = self._task_identity(message, channel, content)
        task.task_id = task_id
        task.attempt_id = attempt_id

        sender = (
            message.get("sender")
            or message.get("pubkey")
            or message.get("author")
            or message.get("user")
            or message.get("from")
            or ""
        ).strip()

        # Sender authorization is independent of task-contract parsing.
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

        # Buzz is an attention/result projection, never a consent authority.
        # In particular, the word "approve" in chat is not an execution grant.
        if task.contract_status == "clarification_required":
            result = self._clarification_result(task, task_id, attempt_id)
            buzz_send_message(
                channel=channel,
                content=(
                    "**Reach Task Clarification Required**\n"
                    "- Provide `success: \"...\"` for a completion contract, "
                    "or mark the request `observation-only`.\n"
                    "- No screen was leased and no driver was launched."
                ),
                private_key=self.private_key,
            )
            return result

        # Chat text cannot request privileged mutation capabilities.
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
        if any(re.search(pat, task.goal.lower()) for pat in mutating_patterns):
            logger.warning("Rejected chat-initiated mutating goal: %s", task.goal)
            buzz_send_message(
                channel=channel,
                content=(
                    "⛔ Security restriction: Mutating tools (exec, card, vault) "
                    "cannot be invoked from chat-initiated goals."
                ),
                reply_to=msg_id,
                relay_url=self.relay_url,
                private_key=self.private_key,
            )
            return None

        logger.info(
            "Accepted task contract %s attempt %s from message %s (screen=%s)",
            task_id,
            attempt_id,
            msg_id,
            task.screen,
        )
        buzz_send_message(
            channel=channel,
            content=(
                f"🐝 Task contract received for screen {task.screen}; "
                "execution will report observed results only."
            ),
            reply_to=msg_id,
            relay_url=self.relay_url,
            private_key=self.private_key,
        )

        lease_token: Optional[str] = None
        driver_result: Optional[DriveResult] = None

        try:
            lease_data = self.reach_client.lease_screen(task.screen, owner="ReachBot")
            lease_token = lease_data.get("token")
            logger.info("Screen %s leased successfully", task.screen)

            def on_step_callback(step: StepRecord) -> None:
                if not self.enable_visual_diff:
                    return
                diff_pct = (
                    step.visual_change * 100.0 if step.visual_change is not None else None
                )
                tokens_saved = 1600 if getattr(step, "vlm_cached", False) else None
                step_result = step.result if isinstance(step.result, dict) else {}
                outcome = "failed" if step.error else str(step_result.get("status", "completed"))
                try:
                    buzz_post_visual_diff(
                        channel=channel,
                        step_index=step.step_index,
                        action_kind=step.action.kind,
                        outcome=outcome,
                        diff_percent=diff_pct,
                        tokens_saved=tokens_saved,
                        reply_to=msg_id,
                        relay_url=self.relay_url,
                        private_key=self.private_key,
                    )
                except Exception as post_err:
                    logger.warning("Failed to post visual diff update: %s", post_err)

            handoff_gen = getattr(self.reach_client, "handoff_gen", None)
            driver = self.driver_factory(
                screen=task.screen,
                lease_token=lease_token,
                handoff_gen=handoff_gen,
                step_callback=on_step_callback,
                task_id=task_id,
                attempt_id=attempt_id,
                completion_text=task.completion_text,
            )
            driver_result = driver.drive(goal=task.goal, initial_url=task.initial_url)

            # Only an explicit auth handoff may resume this same task attempt.
            # Uncertain or other nonterminal outcomes are never replayed.
            if driver_result and driver_result.status == "auth_required":
                handback_success = self.handle_takeover(
                    channel=channel,
                    screen=task.screen,
                    reason="Authentication or human verification required",
                    reply_to=msg_id,
                    token=lease_token,
                )
                if handback_success:
                    resumed_gen = getattr(self.reach_client, "handoff_gen", None)
                    resume_driver = self.driver_factory(
                        screen=task.screen,
                        lease_token=lease_token,
                        handoff_gen=resumed_gen,
                        step_callback=on_step_callback,
                        task_id=task_id,
                        attempt_id=attempt_id,
                        completion_text=task.completion_text,
                    )
                    driver_result = resume_driver.drive(
                        goal=f"Complete remaining tasks for: {task.goal}",
                        initial_url=None,
                    )

        except Exception as loop_err:
            logger.error(
                "Error executing task for message %s: %s", msg_id, loop_err, exc_info=True
            )
            if not driver_result:
                lease_uncertain = (
                    isinstance(getattr(self.reach_client, "last_lease_cleanup", None), dict)
                    and getattr(self.reach_client, "last_lease_cleanup", {}).get("status")
                    == "uncertain"
                )
                driver_result = DriveResult(
                    success=False,
                    status="uncertain" if lease_uncertain else "failed",
                    steps=[],
                    error=str(loop_err),
                    final_description="Execution error occurred",
                    task_id=task_id,
                    metrics={
                        "model_requested": "unknown",
                        "model_reported": "unknown",
                        "model_version": "unknown",
                    },
                )
        finally:
            cleanup = getattr(self.reach_client, "last_lease_cleanup", None)
            if not isinstance(cleanup, dict):
                cleanup = None
            if lease_token:
                try:
                    release_result = self.reach_client.release_screen(
                        screen=task.screen,
                        owner="ReachBot",
                        token=lease_token,
                    )
                    cleanup = {
                        "status": (
                            "confirmed"
                            if isinstance(release_result, dict)
                            and release_result.get("released") is True
                            else "uncertain"
                        )
                    }
                except Exception:
                    cleanup = {"status": "uncertain"}

            final_message = "\n".join(
                self._result_projection(task, driver_result, cleanup)
            )
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
