#!/usr/bin/env python3
"""Reach Routine Engine: Demonstration Recorder, Compiler & Self-Healing Replayer.

Implements:
1. Routine Trace Recording: captures user actions during interactive takeover or
   demonstration (coordinates, keys/text, URLs, selectors, before/after screenshot frames,
   and structured trace.json).
2. Routine Compiler: normalizes coordinates, parameterizes input text into variables,
   and injects multi-modal verification checkpoints (URL, DOM text, visual pHash).
3. Self-Healing Replayer: executes steps deterministically, validates checkpoints,
   and automatically falls back to the CUA vision driving loop (ReachDriver / agy)
   upon failure or layout shift to heal the routine with newly working actions.
"""

from __future__ import annotations

import argparse
import base64
import copy
import json
import logging
import os
import re
import shutil
import socket
import sys
import threading
import time
import urllib.parse
import urllib.request
from dataclasses import asdict, dataclass, field
from datetime import datetime, timezone
from pathlib import Path
from typing import Any, Callable, Dict, List, Optional, Tuple, Union

logger = logging.getLogger("reach_routine")

# Ensure repository root is in sys.path
REPO_ROOT = Path(__file__).parent.parent.resolve()
if str(REPO_ROOT) not in sys.path:
    sys.path.insert(0, str(REPO_ROOT))

from scripts.reach_drive import (  # noqa: E402
    ReachAction,
    ReachDriver,
    StepRecord,
    calculate_visual_change,
    compute_dhash,
    compute_phash,
)

DEFAULT_ROUTINES_DIR = Path.home() / ".reach" / "routines"
DEFAULT_API_URL = os.environ.get("REACH_AGENT_URL", "http://127.0.0.1:4200")


# ==============================================================================
# Data Models
# ==============================================================================


@dataclass
class TraceStep:
    """A single step recorded during a demonstration."""

    step_index: int
    timestamp: str
    action_type: str  # click, type, navigate, key, scroll, wait
    x: Optional[int] = None
    y: Optional[int] = None
    text: Optional[str] = None
    key: Optional[str] = None
    url: Optional[str] = None
    selector: Optional[str] = None
    aria_tag: Optional[str] = None
    reference: Optional[str] = None  # Semantic ref, e.g. @e1, @e2
    before_frame: Optional[str] = None  # Relative path under routine dir
    after_frame: Optional[str] = None  # Relative path under routine dir
    dom_snapshot: Optional[str] = None
    metadata: Dict[str, Any] = field(default_factory=dict)

    def to_dict(self) -> Dict[str, Any]:
        d = asdict(self)
        if self.reference is not None:
            d["ref"] = self.reference
        return d

    @classmethod
    def from_dict(cls, d: Dict[str, Any]) -> TraceStep:
        return cls(
            step_index=d["step_index"],
            timestamp=d.get("timestamp", ""),
            action_type=d.get("action_type", "click"),
            x=d.get("x"),
            y=d.get("y"),
            text=d.get("text"),
            key=d.get("key"),
            url=d.get("url"),
            selector=d.get("selector"),
            aria_tag=d.get("aria_tag"),
            reference=d.get("ref") or d.get("reference"),
            before_frame=d.get("before_frame"),
            after_frame=d.get("after_frame"),
            dom_snapshot=d.get("dom_snapshot"),
            metadata=d.get("metadata", {}),
        )


@dataclass
class RoutineTrace:
    """Structured demonstration trace recording."""

    name: str
    screen: int
    created_at: str
    steps: List[TraceStep] = field(default_factory=list)
    version: int = 1

    def to_dict(self) -> Dict[str, Any]:
        return {
            "version": self.version,
            "name": self.name,
            "screen": self.screen,
            "created_at": self.created_at,
            "steps": [s.to_dict() for s in self.steps],
        }

    @classmethod
    def from_dict(cls, d: Dict[str, Any]) -> RoutineTrace:
        return cls(
            version=d.get("version", 1),
            name=d["name"],
            screen=d.get("screen", 0),
            created_at=d.get("created_at", ""),
            steps=[TraceStep.from_dict(s) for s in d.get("steps", [])],
        )


@dataclass
class Checkpoint:
    """Verification checkpoint evaluated after action execution."""

    type: str  # url_contains, url_matches, text_contains, visual_phash
    value: Optional[str] = None
    expected_hash: Optional[str] = None
    threshold: float = 0.20  # For visual_phash normalized hamming distance
    frame_path: Optional[str] = None
    description: str = ""

    def to_dict(self) -> Dict[str, Any]:
        return asdict(self)

    @classmethod
    def from_dict(cls, d: Dict[str, Any]) -> Checkpoint:
        return cls(
            type=d.get("type", "url_contains"),
            value=d.get("value"),
            expected_hash=d.get("expected_hash"),
            threshold=float(d.get("threshold", 0.20)),
            frame_path=d.get("frame_path"),
            description=d.get("description", ""),
        )


@dataclass
class CompiledAction:
    """Semantic normalized action specification."""

    kind: str  # click, type, navigate, key, scroll, wait
    point: Optional[Tuple[int, int]] = None
    normalized_point: Optional[Tuple[float, float]] = None
    reference: Optional[str] = None  # Semantic ref, e.g. @e1, @e2
    url: Optional[str] = None
    selector: Optional[str] = None
    aria: Optional[str] = None
    value: Optional[str] = None  # May contain template variables like {{query}}
    key: Optional[str] = None
    button: str = "left"
    description: str = ""

    def to_dict(self) -> Dict[str, Any]:
        d: Dict[str, Any] = {
            "kind": self.kind,
            "point": list(self.point) if self.point else None,
            "normalized_point": (
                list(self.normalized_point) if self.normalized_point else None
            ),
            "url": self.url,
            "selector": self.selector,
            "aria": self.aria,
            "value": self.value,
            "key": self.key,
            "button": self.button,
            "description": self.description,
        }
        if self.reference is not None:
            d["ref"] = self.reference
        return d

    @classmethod
    def from_dict(cls, d: Dict[str, Any]) -> CompiledAction:
        point = None
        if d.get("point"):
            point = (int(d["point"][0]), int(d["point"][1]))
        norm_point = None
        if d.get("normalized_point"):
            norm_point = (float(d["normalized_point"][0]), float(d["normalized_point"][1]))
        return cls(
            kind=d.get("kind", "click"),
            point=point,
            normalized_point=norm_point,
            reference=d.get("ref") or d.get("reference"),
            url=d.get("url"),
            selector=d.get("selector"),
            aria=d.get("aria"),
            value=d.get("value"),
            key=d.get("key"),
            button=d.get("button", "left"),
            description=d.get("description", ""),
        )


@dataclass
class CompiledStep:
    """A compiled step with semantic action and verification checkpoints."""

    step_index: int
    action: CompiledAction
    checkpoints: List[Checkpoint] = field(default_factory=list)

    def to_dict(self) -> Dict[str, Any]:
        return {
            "step_index": self.step_index,
            "action": self.action.to_dict(),
            "checkpoints": [c.to_dict() for c in self.checkpoints],
        }

    @classmethod
    def from_dict(cls, d: Dict[str, Any]) -> CompiledStep:
        return cls(
            step_index=d["step_index"],
            action=CompiledAction.from_dict(d["action"]),
            checkpoints=[Checkpoint.from_dict(c) for c in d.get("checkpoints", [])],
        )


@dataclass
class CompiledRoutine:
    """Compiled, parameterizable routine ready for deterministic replay."""

    name: str
    screen: int
    compiled_at: str
    parameters: Dict[str, Any] = field(default_factory=dict)
    steps: List[CompiledStep] = field(default_factory=list)
    version: int = 1
    healed_at: Optional[str] = None

    def to_dict(self) -> Dict[str, Any]:
        return {
            "version": self.version,
            "name": self.name,
            "screen": self.screen,
            "compiled_at": self.compiled_at,
            "healed_at": self.healed_at,
            "parameters": self.parameters,
            "steps": [s.to_dict() for s in self.steps],
        }

    @classmethod
    def from_dict(cls, d: Dict[str, Any]) -> CompiledRoutine:
        return cls(
            version=d.get("version", 1),
            name=d["name"],
            screen=d.get("screen", 0),
            compiled_at=d.get("compiled_at", ""),
            healed_at=d.get("healed_at"),
            parameters=d.get("parameters", {}),
            steps=[CompiledStep.from_dict(s) for s in d.get("steps", [])],
        )


@dataclass
class ReplayResult:
    """Execution output from a routine replay run."""

    success: bool
    status: str  # completed, failed, healed, auth_required
    steps_executed: int
    parameters_used: Dict[str, Any]
    healed: bool = False
    healed_steps: List[Dict[str, Any]] = field(default_factory=list)
    error: Optional[str] = None
    takeover_url: Optional[str] = None
    duration_sec: float = 0.0

    def to_dict(self) -> Dict[str, Any]:
        return asdict(self)


# ==============================================================================
# Helper Utilities
# ==============================================================================


def resolve_routine_dir(
    routine_name: str, base_dir: Optional[Union[str, Path]] = None
) -> Path:
    """Resolve directory path for a named routine."""
    base = Path(base_dir) if base_dir else DEFAULT_ROUTINES_DIR
    return base / routine_name


def render_template(template_str: Optional[str], params: Dict[str, Any]) -> Optional[str]:
    """Substitute {{var}} or {var} placeholders with provided parameter values."""
    if template_str is None:
        return None

    def _replace(match: re.Match[str]) -> str:
        var_name = match.group(1).strip()
        if var_name in params:
            return str(params[var_name])
        return match.group(0)

    # Replace {{var}} first, then {var}
    res = re.sub(r"\{\{([^{}]+)\}\}", _replace, template_str)
    res = re.sub(r"\{([^{}]+)\}", _replace, res)
    return res


def compute_frame_hash_hex(image_path: Union[str, Path]) -> str:
    """Compute 16-character hexadecimal difference hash for an image file."""
    if not os.path.exists(image_path):
        return "0" * 16
    try:
        val = compute_dhash(image_path, size=8)
        return f"{val:016x}"
    except Exception as e:
        logger.debug("Failed to compute dHash for %s: %s", image_path, e)
        return "0" * 16


def hash_distance(hex1: str, hex2: str) -> float:
    """Compute normalized Hamming distance between two 64-bit hex hashes (0.0 to 1.0)."""
    try:
        v1 = int(hex1, 16)
        v2 = int(hex2, 16)
        xor_diff = v1 ^ v2
        bit_diff = bin(xor_diff).count("1")
        return bit_diff / 64.0
    except Exception:
        return 1.0


class SimpleCDPClient:
    """Minimal, pure-Python RFC 6455 WebSocket client for Chrome DevTools Protocol."""

    def __init__(self, ws_url: str, timeout: float = 5.0) -> None:
        self.ws_url = ws_url
        self.timeout = timeout
        parsed = urllib.parse.urlparse(ws_url)
        self.host = parsed.hostname or "127.0.0.1"
        self.port = parsed.port or 9222
        self.path = parsed.path or "/"
        if parsed.query:
            self.path += "?" + parsed.query
        self.sock: Optional[socket.socket] = None
        self._msg_id = 0

    def connect(self) -> bool:
        try:
            self.sock = socket.create_connection((self.host, self.port), timeout=self.timeout)
            self.sock.settimeout(self.timeout)
            sec_key = base64.b64encode(os.urandom(16)).decode("ascii")
            req = (
                f"GET {self.path} HTTP/1.1\r\n"
                f"Host: {self.host}:{self.port}\r\n"
                f"Upgrade: websocket\r\n"
                f"Connection: Upgrade\r\n"
                f"Sec-WebSocket-Key: {sec_key}\r\n"
                f"Sec-WebSocket-Version: 13\r\n\r\n"
            )
            self.sock.sendall(req.encode("ascii"))
            resp = b""
            while b"\r\n\r\n" not in resp:
                chunk = self.sock.recv(4096)
                if not chunk:
                    break
                resp += chunk
            if b"101 Switching Protocols" not in resp:
                self.close()
                return False
            return True
        except Exception as e:
            logger.debug("CDP WebSocket connect error to %s: %s", self.ws_url, e)
            self.close()
            return False

    def send_cdp(self, method: str, params: Optional[Dict[str, Any]] = None) -> int:
        self._msg_id += 1
        payload = json.dumps({"id": self._msg_id, "method": method, "params": params or {}})
        self.send_text(payload)
        return self._msg_id

    def send_text(self, text: str) -> None:
        if not self.sock:
            return
        data = text.encode("utf-8")
        mask = os.urandom(4)
        header = bytearray([0x81])  # FIN + Text frame
        length = len(data)
        if length < 126:
            header.append(0x80 | length)
        elif length <= 0xFFFF:
            header.append(0x80 | 126)
            header.extend(length.to_bytes(2, "big"))
        else:
            header.append(0x80 | 127)
            header.extend(length.to_bytes(8, "big"))
        header.extend(mask)
        masked = bytearray(b ^ mask[i % 4] for i, b in enumerate(data))
        self.sock.sendall(header + masked)

    def recv_text(self, timeout: Optional[float] = None) -> Optional[str]:
        if not self.sock:
            return None
        if timeout is not None:
            self.sock.settimeout(timeout)
        try:
            head = self._recv_exact(2)
            if not head:
                return None
            b1, b2 = head[0], head[1]
            opcode = b1 & 0x0F
            masked = (b2 & 0x80) != 0
            payload_len = b2 & 0x7F
            if payload_len == 126:
                ext = self._recv_exact(2)
                if not ext:
                    return None
                payload_len = int.from_bytes(ext, "big")
            elif payload_len == 127:
                ext = self._recv_exact(8)
                if not ext:
                    return None
                payload_len = int.from_bytes(ext, "big")
            mask = self._recv_exact(4) if masked else None
            payload = self._recv_exact(payload_len)
            if masked and mask:
                payload = bytes(b ^ mask[i % 4] for i, b in enumerate(payload))
            if opcode == 0x8:  # Close frame
                self.close()
                return None
            if opcode == 0x1:  # Text frame
                return payload.decode("utf-8", errors="replace")
            return None
        except socket.timeout:
            return None
        except Exception as e:
            logger.debug("CDP recv_text error: %s", e)
            return None

    def _recv_exact(self, n: int) -> bytes:
        buf = bytearray()
        while len(buf) < n:
            try:
                chunk = self.sock.recv(n - len(buf)) if self.sock else b""
                if not chunk:
                    break
                buf.extend(chunk)
            except socket.timeout:
                break
        return bytes(buf)

    def close(self) -> None:
        if self.sock:
            try:
                self.sock.close()
            except Exception:
                pass
            self.sock = None


def discover_cdp_target(
    cdp_host: str = "127.0.0.1", cdp_port: int = 9222, timeout: float = 3.0
) -> Optional[Dict[str, Any]]:
    """Query Chrome /json/list to discover active page targets."""
    url = f"http://{cdp_host}:{cdp_port}/json/list"
    try:
        req = urllib.request.Request(url)
        with urllib.request.urlopen(req, timeout=timeout) as resp:
            data = json.loads(resp.read().decode("utf-8"))
            if isinstance(data, list):
                for target in data:
                    if target.get("type") == "page" and target.get("webSocketDebuggerUrl"):
                        return target
                if data:
                    return data[0]
    except Exception as e:
        logger.debug("Error discovering CDP targets at %s: %s", url, e)
    return None


CDP_TAP_SCRIPT = """
(() => {
    if (window.__reach_event_tap_installed) return;
    window.__reach_event_tap_installed = true;
    window.__reach_events = window.__reach_events || [];

    function getSelector(el) {
        if (!el || el === document || el === window) return '';
        if (el.id) return '#' + el.id;
        if (el.getAttribute && el.getAttribute('name')) return `${el.tagName.toLowerCase()}[name="${el.getAttribute('name')}"]`;
        if (el.getAttribute && el.getAttribute('data-reach-ref')) return `[data-reach-ref="${el.getAttribute('data-reach-ref')}"]`;
        let path = [];
        let cur = el;
        while (cur && cur.nodeType === Node.ELEMENT_NODE) {
            let selector = cur.nodeName.toLowerCase();
            if (cur.id) {
                selector += '#' + cur.id;
                path.unshift(selector);
                break;
            } else {
                let sib = cur, nth = 1;
                while (sib = sib.previousElementSibling) {
                    if (sib.nodeName.toLowerCase() === selector) nth++;
                }
                if (nth !== 1) selector += `:nth-of-type(${nth})`;
            }
            path.unshift(selector);
            cur = cur.parentNode;
        }
        return path.join(' > ');
    }

    function emitEvent(ev) {
        window.__reach_events.push(ev);
        if (typeof window.__reach_emit_event === 'function') {
            try {
                window.__reach_emit_event(JSON.stringify(ev));
            } catch (e) {}
        }
    }

    // 1. Click listener (capture phase)
    document.addEventListener('click', (e) => {
        try {
            const target = e.target;
            const refEl = target.closest ? target.closest('[data-reach-ref]') : null;
            const ref = refEl ? ('@' + refEl.getAttribute('data-reach-ref')) : null;
            const sel = getSelector(target);
            const role = target.getAttribute ? (target.getAttribute('role') || target.tagName.toLowerCase()) : '';
            const aria = target.getAttribute ? (target.getAttribute('aria-label') || target.getAttribute('placeholder') || (target.innerText || '')) : '';

            emitEvent({
                type: 'click',
                timestamp: new Date().toISOString(),
                x: e.clientX,
                y: e.clientY,
                ref: ref,
                selector: sel,
                role: role,
                aria: aria.slice(0, 100).trim(),
                url: window.location.href,
            });
        } catch (err) {}
    }, true);

    // 2. Change/Input listener (capture text inputs)
    document.addEventListener('change', (e) => {
        try {
            const target = e.target;
            if (target && (target.tagName === 'INPUT' || target.tagName === 'TEXTAREA' || target.tagName === 'SELECT')) {
                const refEl = target.closest ? target.closest('[data-reach-ref]') : null;
                const ref = refEl ? ('@' + refEl.getAttribute('data-reach-ref')) : null;
                const sel = getSelector(target);
                emitEvent({
                    type: 'type',
                    timestamp: new Date().toISOString(),
                    text: target.value,
                    ref: ref,
                    selector: sel,
                    url: window.location.href,
                });
            }
        } catch (err) {}
    }, true);

    // 3. Keydown listener (for Enter, Tab, Escape)
    document.addEventListener('keydown', (e) => {
        try {
            if (['Enter', 'Tab', 'Escape', 'Backspace'].includes(e.key)) {
                emitEvent({
                    type: 'key',
                    timestamp: new Date().toISOString(),
                    key: e.key,
                    url: window.location.href,
                });
            }
        } catch (err) {}
    }, true);
})();
"""


class CDPEventTap:
    """Hooks Chrome DevTools Protocol to capture user actions in real time."""

    def __init__(
        self,
        recorder: RoutineRecorder,
        cdp_host: str = "127.0.0.1",
        cdp_port: int = 9222,
    ) -> None:
        self.recorder = recorder
        self.cdp_host = cdp_host
        self.cdp_port = cdp_port
        self.client: Optional[SimpleCDPClient] = None
        self._running = False
        self._thread: Optional[threading.Thread] = None
        self._last_event_ts = 0.0

    def start(self) -> bool:
        """Discover Chrome page target and connect CDP tap."""
        target = discover_cdp_target(self.cdp_host, self.cdp_port)
        if not target or not target.get("webSocketDebuggerUrl"):
            return False

        ws_url = target["webSocketDebuggerUrl"]
        self.client = SimpleCDPClient(ws_url)
        if not self.client.connect():
            return False

        # Initialize CDP domains and tap hooks
        self.client.send_cdp("Page.enable")
        self.client.send_cdp("Runtime.enable")
        self.client.send_cdp("Runtime.addBinding", {"name": "__reach_emit_event"})
        self.client.send_cdp("Page.addScriptToEvaluateOnNewDocument", {"source": CDP_TAP_SCRIPT})
        self.client.send_cdp("Runtime.evaluate", {"expression": CDP_TAP_SCRIPT})

        self._running = True
        self._thread = threading.Thread(target=self._listen_loop, daemon=True)
        self._thread.start()
        return True

    def stop(self) -> None:
        """Stop listening and close CDP connection."""
        self._running = False
        if self.client:
            self.client.close()
            self.client = None
        if self._thread and self._thread.is_alive():
            self._thread.join(timeout=1.0)

    def _listen_loop(self) -> None:
        """Poll and receive CDP messages and in-page events."""
        eval_drain = (
            "(() => { const evs = window.__reach_events || []; "
            "window.__reach_events = []; return JSON.stringify(evs); })()"
        )
        last_poll = time.time()
        while self._running and self.client:
            try:
                # 1. Check for incoming WebSocket message
                raw = self.client.recv_text(timeout=0.2)
                if raw:
                    self._handle_cdp_message(raw)

                # 2. Periodically drain events in case page navigated or binding was bypassed
                now = time.time()
                if now - last_poll >= 1.0:
                    last_poll = now
                    self.client.send_cdp("Runtime.evaluate", {"expression": CDP_TAP_SCRIPT})
                    self.client.send_cdp(
                        "Runtime.evaluate",
                        {"expression": eval_drain, "returnByValue": True},
                    )
            except Exception as e:
                logger.debug("Error in CDP event tap loop: %s", e)
                break

    def _handle_cdp_message(self, raw_json: str) -> None:
        try:
            data = json.loads(raw_json)
        except Exception:
            return

        method = data.get("method")
        if method == "Runtime.bindingCalled":
            params = data.get("params", {})
            if params.get("name") == "__reach_emit_event":
                payload_str = params.get("payload", "{}")
                try:
                    ev = json.loads(payload_str)
                    self._process_event(ev)
                except Exception:
                    pass
        elif method == "Page.frameNavigated":
            params = data.get("params", {})
            frame = params.get("frame", {})
            url = frame.get("url")
            if url and not frame.get("parentId") and url != "about:blank":
                self._process_event({"type": "navigate", "url": url})
        elif "result" in data:
            res = data.get("result", {}).get("result", {})
            val = res.get("value")
            if val and isinstance(val, str) and val.startswith("["):
                try:
                    ev_list = json.loads(val)
                    for ev in ev_list:
                        self._process_event(ev)
                except Exception:
                    pass

    def _process_event(self, ev: Dict[str, Any]) -> None:
        ev_type = ev.get("type", "click")
        now = time.time()
        if now - self._last_event_ts < 0.2:
            pass
        self._last_event_ts = now

        x = ev.get("x")
        y = ev.get("y")
        text = ev.get("text")
        key = ev.get("key")
        url = ev.get("url")
        selector = ev.get("selector")
        aria = ev.get("aria") or ev.get("role")
        ref = ev.get("ref")

        self.recorder.record_step(
            action_type=ev_type,
            x=x,
            y=y,
            text=text,
            key=key,
            url=url,
            selector=selector,
            aria_tag=aria,
            reference=ref,
            execute=False,
            metadata={"source": "cdp_event_tap"},
        )


class RoutineRecorder:
    """Captures user actions during interactive takeover or demonstration."""

    def __init__(
        self,
        routine_name: str,
        screen: int = 0,
        routines_dir: Optional[Union[str, Path]] = None,
        driver: Optional[ReachDriver] = None,
        api_url: str = DEFAULT_API_URL,
        sandbox: Optional[str] = None,
    ) -> None:
        self.routine_name = routine_name
        self.screen = screen
        self.routine_dir = resolve_routine_dir(routine_name, routines_dir)
        self.frames_dir = self.routine_dir / "frames"
        self.frames_dir.mkdir(parents=True, exist_ok=True)
        self.trace_file = self.routine_dir / "trace.json"

        self.driver = driver or ReachDriver(
            api_url=api_url,
            screen=screen,
            sandbox=sandbox,
            enable_audit=False,
        )
        self.steps: List[TraceStep] = []
        self._current_url: Optional[str] = None

    def capture_frame(self, filename: str) -> str:
        """Capture screenshot to frames directory and return relative path."""
        abs_path = self.frames_dir / filename
        try:
            shot_tmp = self.driver.capture_screenshot(len(self.steps) + 1)
            if os.path.isfile(shot_tmp):
                shutil.copyfile(shot_tmp, abs_path)
            else:
                abs_path.touch()
        except Exception as e:
            logger.debug("Error capturing frame %s: %s", filename, e)
            abs_path.touch()

        return f"frames/{filename}"

    def capture_dom_snapshot(self) -> str:
        """Capture page text snapshot if available."""
        if not self._current_url:
            return ""
        try:
            return self.driver.capture_page_text(self._current_url)
        except Exception:
            return ""

    def record_step(
        self,
        action_type: str,
        x: Optional[int] = None,
        y: Optional[int] = None,
        text: Optional[str] = None,
        key: Optional[str] = None,
        url: Optional[str] = None,
        selector: Optional[str] = None,
        aria_tag: Optional[str] = None,
        reference: Optional[str] = None,
        execute: bool = True,
        metadata: Optional[Dict[str, Any]] = None,
    ) -> TraceStep:
        """Record an action step with before/after frames and structured metadata."""
        step_idx = len(self.steps) + 1
        ts = datetime.now(timezone.utc).isoformat()

        if url:
            self._current_url = url

        # 1. Capture before frame
        before_file = f"step_{step_idx:03d}_before.png"
        rel_before = self.capture_frame(before_file)

        # 2. Execute action if requested
        if execute:
            act = ReachAction(
                kind=action_type,
                point=(x, y) if x is not None and y is not None else None,
                ref=reference,
                value=text or url,
                key=key,
                target=selector or url,
            )
            try:
                self.driver.execute_action(act)
                time.sleep(0.5)
            except Exception as e:
                logger.warning("Step %s action execution error: %s", step_idx, e)

        # 3. Capture after frame and DOM
        after_file = f"step_{step_idx:03d}_after.png"
        rel_after = self.capture_frame(after_file)
        dom_snapshot = self.capture_dom_snapshot()

        step = TraceStep(
            step_index=step_idx,
            timestamp=ts,
            action_type=action_type,
            x=x,
            y=y,
            text=text,
            key=key,
            url=self._current_url,
            selector=selector,
            aria_tag=aria_tag,
            reference=reference,
            before_frame=rel_before,
            after_frame=rel_after,
            dom_snapshot=dom_snapshot,
            metadata=metadata or {},
        )
        self.steps.append(step)
        self.save_trace()
        return step

    def start_event_tap(
        self,
        initial_url: Optional[str] = None,
        timeout_sec: Optional[float] = None,
        stop_event: Optional[threading.Event] = None,
        cdp_host: str = "127.0.0.1",
        cdp_port: Optional[int] = None,
    ) -> RoutineTrace:
        """Start automated CDP event tap session to capture demonstration actions."""
        if initial_url:
            self.record_step("navigate", url=initial_url, execute=True)

        port = cdp_port if cdp_port is not None else (9222 + self.screen)
        tap = CDPEventTap(self, cdp_host=cdp_host, cdp_port=port)
        if not tap.start():
            logger.warning(
                "Could not attach CDP event tap on %s:%s. Chrome may not have remote debugging enabled.",
                cdp_host,
                port,
            )
            return self.save_trace()

        logger.info("CDP Event Tap active on %s:%s. Capturing demonstration...", cdp_host, port)
        start_time = time.time()
        try:
            while True:
                if stop_event and stop_event.is_set():
                    break
                if timeout_sec and (time.time() - start_time) > timeout_sec:
                    break
                time.sleep(0.5)
        finally:
            tap.stop()

        return self.save_trace()

    def save_trace(self) -> RoutineTrace:
        """Save the accumulated trace to trace.json."""
        trace = RoutineTrace(
            name=self.routine_name,
            screen=self.screen,
            created_at=datetime.now(timezone.utc).isoformat(),
            steps=self.steps,
        )
        self.routine_dir.mkdir(parents=True, exist_ok=True)
        with open(self.trace_file, "w", encoding="utf-8") as f:
            json.dump(trace.to_dict(), f, indent=2)
        return trace


# ==============================================================================
# 2. Routine Compiler
# ==============================================================================


class RoutineCompiler:
    """Compiles demonstration trace into a parameterized routine with checkpoints."""

    def __init__(
        self,
        screen_width: int = 1280,
        screen_height: int = 720,
    ) -> None:
        self.screen_width = screen_width
        self.screen_height = screen_height

    def compile(
        self,
        trace_input: Union[RoutineTrace, Dict[str, Any], str, Path],
        parameter_mappings: Optional[Dict[str, str]] = None,
        routines_dir: Optional[Union[str, Path]] = None,
    ) -> CompiledRoutine:
        """Compile a trace into a CompiledRoutine.

        Normalizes raw coordinates into semantic actions, parameterizes input text
        into variables, and injects verification checkpoints.
        """
        trace = self._load_trace(trace_input, routines_dir)
        routine_dir = resolve_routine_dir(trace.name, routines_dir)

        parameters: Dict[str, Any] = {}
        compiled_steps: List[CompiledStep] = []

        # Parameter inference map: text -> variable name
        param_map: Dict[str, str] = dict(parameter_mappings or {})

        # 1. Discover parameters from type actions
        for step in trace.steps:
            if step.action_type == "type" and step.text:
                if step.text not in param_map:
                    var_name = self._infer_param_name(step, len(parameters))
                    param_map[step.text] = var_name
                var_name = param_map[step.text]
                parameters[var_name] = step.text

        # 2. Compile steps
        for step in trace.steps:
            compiled_action = self._normalize_action(step, param_map)
            checkpoints = self._generate_checkpoints(step, routine_dir)
            compiled_steps.append(
                CompiledStep(
                    step_index=step.step_index,
                    action=compiled_action,
                    checkpoints=checkpoints,
                )
            )

        compiled_routine = CompiledRoutine(
            name=trace.name,
            screen=trace.screen,
            compiled_at=datetime.now(timezone.utc).isoformat(),
            parameters=parameters,
            steps=compiled_steps,
        )

        # Save routine.json to routine directory
        routine_json_path = routine_dir / "routine.json"
        routine_dir.mkdir(parents=True, exist_ok=True)
        with open(routine_json_path, "w", encoding="utf-8") as f:
            json.dump(compiled_routine.to_dict(), f, indent=2)

        return compiled_routine

    def _load_trace(
        self,
        trace_input: Union[RoutineTrace, Dict[str, Any], str, Path],
        routines_dir: Optional[Union[str, Path]],
    ) -> RoutineTrace:
        if isinstance(trace_input, RoutineTrace):
            return trace_input
        if isinstance(trace_input, dict):
            return RoutineTrace.from_dict(trace_input)

        p = Path(trace_input)
        if not p.is_file():
            # Try as routine name under routines_dir
            candidate = resolve_routine_dir(str(trace_input), routines_dir) / "trace.json"
            if candidate.is_file():
                p = candidate
            else:
                raise FileNotFoundError(f"Cannot find trace at {trace_input}")

        with open(p, "r", encoding="utf-8") as f:
            data = json.load(f)
        return RoutineTrace.from_dict(data)

    def _infer_param_name(self, step: TraceStep, current_count: int) -> str:
        """Infer parameter variable name from context / selector / ARIA label."""
        context_str = f"{step.selector or ''} {step.aria_tag or ''}".lower()
        if any(k in context_str for k in ("search", "query", "find", "keyword", "q")):
            return "query" if current_count == 0 else f"query_{current_count + 1}"
        if any(k in context_str for k in ("email", "mail")):
            return "email" if current_count == 0 else f"email_{current_count + 1}"
        if any(k in context_str for k in ("company", "organization", "org")):
            return "company" if current_count == 0 else f"company_{current_count + 1}"
        if any(k in context_str for k in ("user", "username", "login")):
            return "username" if current_count == 0 else f"username_{current_count + 1}"
        if any(k in context_str for k in ("password", "passwd")):
            return "password" if current_count == 0 else f"password_{current_count + 1}"

        return f"param_{current_count + 1}"

    def _normalize_action(
        self, step: TraceStep, param_map: Dict[str, str]
    ) -> CompiledAction:
        """Normalize raw coordinates and action into semantic action."""
        point = (step.x, step.y) if step.x is not None and step.y is not None else None
        normalized_point = None
        if point is not None:
            nx = round(point[0] / float(self.screen_width), 4)
            ny = round(point[1] / float(self.screen_height), 4)
            normalized_point = (nx, ny)

        value = step.text
        if step.action_type == "type" and value and value in param_map:
            param_name = param_map[value]
            value = f"{{{{{param_name}}}}}"

        desc = f"{step.action_type.capitalize()}"
        if step.reference:
            desc += f" on ref '{step.reference}'"
        elif step.selector:
            desc += f" on '{step.selector}'"
        elif step.aria_tag:
            desc += f" on '{step.aria_tag}'"
        elif point:
            desc += f" at ({point[0]}, {point[1]})"

        if value:
            desc += f" with '{value}'"
        if step.key:
            desc += f" key '{step.key}'"
        if step.url:
            desc += f" URL '{step.url}'"

        return CompiledAction(
            kind=step.action_type,
            point=point,
            normalized_point=normalized_point,
            reference=step.reference,
            url=step.url,
            selector=step.selector,
            aria=step.aria_tag,
            value=value,
            key=step.key,
            description=desc,
        )

    def _generate_checkpoints(
        self, step: TraceStep, routine_dir: Path
    ) -> List[Checkpoint]:
        """Inject verification checkpoints: URL, text keyword, visual pHash anchor."""
        checkpoints: List[Checkpoint] = []

        # 1. URL Checkpoint
        if step.action_type == "navigate" and step.url:
            url_val = step.url.split("?")[0].rstrip("/")
            domain_or_path = url_val.split("://")[-1]
            checkpoints.append(
                Checkpoint(
                    type="url_contains",
                    value=domain_or_path,
                    description=f"Verify URL contains '{domain_or_path}'",
                )
            )

        # 2. Text Checkpoint (if confirmation or significant keyword detected)
        if step.dom_snapshot:
            keywords = ["success", "dashboard", "results", "welcome", "account", "profile"]
            for kw in keywords:
                if kw in step.dom_snapshot.lower():
                    checkpoints.append(
                        Checkpoint(
                            type="text_contains",
                            value=kw,
                            description=f"Verify DOM contains keyword '{kw}'",
                        )
                    )
                    break

        # 3. Visual pHash Anchor Checkpoint
        if step.after_frame:
            abs_frame = routine_dir / step.after_frame
            if abs_frame.exists():
                hash_hex = compute_frame_hash_hex(abs_frame)
                checkpoints.append(
                    Checkpoint(
                        type="visual_phash",
                        expected_hash=hash_hex,
                        threshold=0.20,
                        frame_path=step.after_frame,
                        description=f"Visual anchor verification for step {step.step_index}",
                    )
                )

        return checkpoints


# ==============================================================================
# 3. Self-Healing Replayer
# ==============================================================================


class ReplayError(Exception):
    """Exception raised when routine execution fails and cannot be healed."""


class RoutineReplayer:
    """Executes compiled routine deterministically with CUA self-healing fallback."""

    def __init__(
        self,
        routine_name: str,
        routines_dir: Optional[Union[str, Path]] = None,
        driver: Optional[ReachDriver] = None,
        api_url: str = DEFAULT_API_URL,
        screen: Optional[int] = None,
        sandbox: Optional[str] = None,
        heal_with_cua: bool = True,
    ) -> None:
        self.routine_name = routine_name
        self.routine_dir = resolve_routine_dir(routine_name, routines_dir)
        self.routine_file = self.routine_dir / "routine.json"
        self.heal_with_cua = heal_with_cua

        self.routine = self._load_routine()
        target_screen = screen if screen is not None else self.routine.screen
        self.driver = driver or ReachDriver(
            api_url=api_url,
            screen=target_screen,
            sandbox=sandbox,
            enable_audit=True,
        )

    def _load_routine(self) -> CompiledRoutine:
        if not self.routine_file.exists():
            raise FileNotFoundError(f"Routine not found at {self.routine_file}")
        with open(self.routine_file, "r", encoding="utf-8") as f:
            data = json.load(f)
        return CompiledRoutine.from_dict(data)

    def replay(
        self,
        params: Optional[Dict[str, Any]] = None,
        max_healing_steps: int = 5,
    ) -> ReplayResult:
        """Run the routine, validating checkpoints and healing with CUA if blocked."""
        start_time = time.time()
        merged_params = copy.deepcopy(self.routine.parameters)
        if params:
            merged_params.update(params)

        logger.info(
            "Replaying routine '%s' with parameters: %s",
            self.routine_name,
            merged_params,
        )

        executed_count = 0
        healed_overall = False
        healed_step_records: List[Dict[str, Any]] = []

        i = 0
        while i < len(self.routine.steps):
            step = self.routine.steps[i]
            executed_count += 1

            # Render parameter variables
            action = copy.deepcopy(step.action)
            action.value = render_template(action.value, merged_params)
            action.url = render_template(action.url, merged_params)
            action.description = render_template(action.description, merged_params) or ""

            logger.info("Step %s/%s: %s", i + 1, len(self.routine.steps), action.description)

            # 1. Execute deterministic action
            exec_ok, exec_err = self._execute_deterministic_action(action)

            # 2. Checkpoints validation
            validation_ok = False
            failed_checkpoint: Optional[Checkpoint] = None
            checkpoint_reason = ""

            if exec_ok:
                validation_ok, failed_checkpoint, checkpoint_reason = (
                    self._validate_checkpoints(step.checkpoints)
                )

            # 3. Handle failure / roadblock -> CUA self-healing fallback
            if not exec_ok or not validation_ok:
                failure_desc = exec_err if not exec_ok else checkpoint_reason
                logger.warning(
                    "[!] Step %s roadblock detected: %s",
                    step.step_index,
                    failure_desc,
                )

                if not self.heal_with_cua:
                    return ReplayResult(
                        success=False,
                        status="failed",
                        steps_executed=executed_count,
                        parameters_used=merged_params,
                        error=f"Step {step.step_index} failed: {failure_desc}",
                        duration_sec=round(time.time() - start_time, 2),
                    )

                logger.info(
                    "Initiating CUA vision self-healing loop for step %s...",
                    step.step_index,
                )
                heal_ok, new_actions, heal_err, takeover_url = self._heal_step(
                    step, action, failure_desc, merged_params, max_healing_steps
                )

                if takeover_url:
                    return ReplayResult(
                        success=False,
                        status="auth_required",
                        steps_executed=executed_count,
                        parameters_used=merged_params,
                        takeover_url=takeover_url,
                        error="Authentication wall detected during healing",
                        duration_sec=round(time.time() - start_time, 2),
                    )

                if not heal_ok:
                    return ReplayResult(
                        success=False,
                        status="failed",
                        steps_executed=executed_count,
                        parameters_used=merged_params,
                        error=f"Self-healing failed at step {step.step_index}: {heal_err}",
                        duration_sec=round(time.time() - start_time, 2),
                    )

                logger.info(
                    "[✓] Step %s healed successfully! Recorded %s new action(s).",
                    step.step_index,
                    len(new_actions),
                )
                healed_overall = True
                healed_step_records.extend(new_actions)

                # Update the routine with newly working actions
                self._apply_healing_to_routine(i, new_actions)

                # Move index past the healed actions
                i += len(new_actions)
                continue

            i += 1

        duration = round(time.time() - start_time, 2)
        status = "healed" if healed_overall else "completed"
        return ReplayResult(
            success=True,
            status=status,
            steps_executed=executed_count,
            parameters_used=merged_params,
            healed=healed_overall,
            healed_steps=healed_step_records,
            duration_sec=duration,
        )

    def _execute_deterministic_action(
        self, action: CompiledAction
    ) -> Tuple[bool, Optional[str]]:
        """Dispatch deterministic action to the Reach environment."""
        try:
            reach_action = ReachAction(
                kind=action.kind,
                point=action.point,
                ref=action.reference,
                value=action.value or action.url,
                key=action.key,
                target=action.selector or action.url,
                button=action.button,
                description=action.description,
            )
            res = self.driver.execute_action(reach_action)
            if isinstance(res, dict) and res.get("error"):
                return False, str(res["error"])
            time.sleep(0.5)
            return True, None
        except Exception as e:
            return False, str(e)

    def _validate_checkpoints(
        self, checkpoints: List[Checkpoint]
    ) -> Tuple[bool, Optional[Checkpoint], str]:
        """Validate all checkpoints against current screen state."""
        if not checkpoints:
            return True, None, ""

        # Gather current observation state
        current_shot = None
        current_hash = None
        current_dom = None

        for cp in checkpoints:
            if cp.type == "url_contains":
                # Check current URL via driver or page_text
                url_to_check = self._get_current_url()
                if cp.value and (url_to_check is None or cp.value not in url_to_check):
                    return (
                        False,
                        cp,
                        f"URL '{url_to_check}' does not contain expected '{cp.value}'",
                    )

            elif cp.type == "text_contains":
                if current_dom is None:
                    current_dom = self._get_current_dom()
                if cp.value and cp.value.lower() not in current_dom.lower():
                    return (
                        False,
                        cp,
                        f"Page text does not contain expected '{cp.value}'",
                    )

            elif cp.type == "visual_phash":
                if current_shot is None:
                    current_shot = self.driver.capture_screenshot(999)
                if current_hash is None and current_shot:
                    current_hash = compute_frame_hash_hex(current_shot)

                if cp.expected_hash and current_hash:
                    dist = hash_distance(cp.expected_hash, current_hash)
                    if dist > cp.threshold:
                        return (
                            False,
                            cp,
                            f"Visual distance {dist:.2f} exceeds threshold {cp.threshold:.2f}",
                        )

        return True, None, ""

    def _get_current_url(self) -> Optional[str]:
        """Retrieve current URL from sandbox."""
        # Query via MCP or inspect
        try:
            res = self.driver.call_mcp_tool("page_text", {"timeout_ms": 2000})
            if isinstance(res, dict) and "url" in res:
                return str(res["url"])
        except Exception:
            pass
        return None

    def _get_current_dom(self) -> str:
        """Retrieve current page text."""
        try:
            return self.driver.capture_page_text(self._get_current_url() or "")
        except Exception:
            return ""

    def _heal_step(
        self,
        failed_step: CompiledStep,
        action: CompiledAction,
        roadblock_reason: str,
        params: Dict[str, Any],
        max_steps: int,
    ) -> Tuple[bool, List[Dict[str, Any]], Optional[str], Optional[str]]:
        """Run CUA vision loop (ReachDriver) to overcome the roadblock."""
        checkpoint_summary = ""
        if failed_step.checkpoints:
            checkpoint_summary = f" Target checkpoints: {', '.join(c.description for c in failed_step.checkpoints)}"

        healing_goal = (
            f"Routine step {failed_step.step_index} failed: {roadblock_reason}. "
            f"Original intention: {action.description}.{checkpoint_summary}. "
            f"Achieve this step's goal using browser actions, then terminate."
        )

        healing_driver = ReachDriver(
            api_url=self.driver.api_url,
            screen=self.driver.screen,
            sandbox=self.driver.sandbox,
            max_steps=max_steps,
            timeout_sec=self.driver.timeout_sec,
            model=self.driver.model,
            agy_bin=self.driver.agy_bin,
            enable_audit=True,
        )

        result = healing_driver.drive(goal=healing_goal)

        if result.status == "auth_required" or result.takeover_url:
            return False, [], "Auth required", result.takeover_url

        if not result.success:
            return False, [], result.error or "Healing vision loop did not succeed", None

        # Convert successful CUA steps into new routine action specs
        new_actions: List[Dict[str, Any]] = []
        for s in result.steps:
            act = s.action
            if act.kind in ("terminate", "auth_required"):
                continue

            point_norm = None
            if act.point:
                point_norm = [
                    round(act.point[0] / 1280.0, 4),
                    round(act.point[1] / 720.0, 4),
                ]

            new_actions.append(
                {
                    "kind": act.kind,
                    "point": list(act.point) if act.point else None,
                    "normalized_point": point_norm,
                    "url": act.target if act.kind == "navigate" else None,
                    "selector": act.target if act.kind != "navigate" else None,
                    "aria": None,
                    "value": act.value,
                    "key": act.key,
                    "button": act.button,
                    "description": act.description or f"Healed {act.kind}",
                    "after_screenshot": s.screenshot_path,
                }
            )

        return True, new_actions, None, None

    def _apply_healing_to_routine(
        self, failed_step_index: int, new_action_dicts: List[Dict[str, Any]]
    ) -> None:
        """Amends routine steps with the healed actions and saves healed routine.json."""
        if not new_action_dicts:
            return

        healed_steps: List[CompiledStep] = []
        for offset, a_dict in enumerate(new_action_dicts):
            action = CompiledAction.from_dict(a_dict)
            checkpoints: List[Checkpoint] = []

            # If screenshot was captured during healing, attach visual pHash anchor
            shot_path = a_dict.get("after_screenshot")
            if shot_path and os.path.isfile(shot_path):
                h_hex = compute_frame_hash_hex(shot_path)
                checkpoints.append(
                    Checkpoint(
                        type="visual_phash",
                        expected_hash=h_hex,
                        threshold=0.20,
                        description="Healed visual anchor",
                    )
                )

            healed_steps.append(
                CompiledStep(
                    step_index=failed_step_index + 1 + offset,
                    action=action,
                    checkpoints=checkpoints,
                )
            )

        # Replace the failing step with the healed steps
        self.routine.steps[failed_step_index : failed_step_index + 1] = healed_steps

        # Re-index all steps
        for idx, s in enumerate(self.routine.steps):
            s.step_index = idx + 1

        self.routine.version += 1
        self.routine.healed_at = datetime.now(timezone.utc).isoformat()

        # Save to routine.json
        with open(self.routine_file, "w", encoding="utf-8") as f:
            json.dump(self.routine.to_dict(), f, indent=2)

        logger.info(
            "Saved healed routine (v%s) to %s",
            self.routine.version,
            self.routine_file,
        )


# ==============================================================================
# CLI Entrypoint
# ==============================================================================


def main() -> None:
    parser = argparse.ArgumentParser(
        description="Reach Routine: Demonstration Recorder, Compiler & Replayer"
    )
    subparsers = parser.add_subparsers(dest="subcommand", required=True)

    # 1. record
    p_record = subparsers.add_parser("record", help="Record interactive routine demonstration")
    p_record.add_argument("--name", required=True, help="Routine name")
    p_record.add_argument("--screen", type=int, default=0, help="Screen ID to record (default: 0)")
    p_record.add_argument("--url", default=None, help="Initial URL to open for demonstration")
    p_record.add_argument("--manual", action="store_true", help="Fallback to manual terminal REPL")
    p_record.add_argument("--cdp-port", type=int, default=None, help="Chrome CDP port (default: 9222 + screen)")
    p_record.add_argument("--routines-dir", default=None, help="Base routines directory")
    p_record.add_argument("--api-url", default=DEFAULT_API_URL, help="Reach agent API URL")
    p_record.add_argument("--sandbox", default=None, help="Sandbox name or ID")

    # 2. compile
    p_compile = subparsers.add_parser("compile", help="Compile trace into routine.json")
    p_compile.add_argument("--name", required=True, help="Routine name")
    p_compile.add_argument("--routines-dir", default=None, help="Base routines directory")
    p_compile.add_argument("--params", default=None, help="JSON parameter mapping (e.g. '{\"Tesla\": \"query\"}')")

    # 3. replay
    p_replay = subparsers.add_parser("replay", help="Replay compiled routine with self-healing")
    p_replay.add_argument("--routine", required=True, help="Routine name")
    p_replay.add_argument("--params", default=None, help="JSON parameters override")
    p_replay.add_argument("--screen", type=int, default=None, help="Screen ID override")
    p_replay.add_argument("--routines-dir", default=None, help="Base routines directory")
    p_replay.add_argument("--api-url", default=DEFAULT_API_URL, help="Reach agent API URL")
    p_replay.add_argument("--sandbox", default=None, help="Sandbox name or ID")
    p_replay.add_argument("--no-heal", action="store_true", help="Disable CUA self-healing fallback")
    p_replay.add_argument("--json", action="store_true", help="Output result in JSON")

    args = parser.parse_args()
    logging.basicConfig(level=logging.INFO, format="%(asctime)s [%(levelname)s] %(message)s")

    if args.subcommand == "record":
        recorder = RoutineRecorder(
            routine_name=args.name,
            screen=args.screen,
            routines_dir=args.routines_dir,
            api_url=args.api_url,
            sandbox=args.sandbox,
        )
        print(f"[+] Demonstration recorder initialized for '{args.name}' on screen :{99 + args.screen}")
        print(f"    Routines directory: {recorder.routine_dir}")

        if args.url:
            print(f"    Navigating to initial URL: {args.url}")
            recorder.record_step("navigate", url=args.url, execute=True)

        if args.manual:
            print("    [Manual Mode] Enter actions in format: 'navigate <url>', 'click <x> <y>', 'click @ref', 'type <text>', 'key <combo>', 'done'")
            while True:
                try:
                    line = input("reach record> ").strip()
                except (EOFError, KeyboardInterrupt):
                    break
                if not line or line.lower() in ("done", "exit", "quit"):
                    break

                parts = line.split(maxsplit=2)
                cmd = parts[0].lower()
                if cmd == "navigate" and len(parts) > 1:
                    recorder.record_step("navigate", url=parts[1])
                elif cmd == "click" and len(parts) >= 2:
                    if parts[1].startswith("@") or parts[1].startswith("e"):
                        recorder.record_step("click", reference=parts[1])
                    elif len(parts) >= 3:
                        recorder.record_step("click", x=int(parts[1]), y=int(parts[2]))
                elif cmd == "type" and len(parts) > 1:
                    recorder.record_step("type", text=parts[1])
                elif cmd == "key" and len(parts) > 1:
                    recorder.record_step("key", key=parts[1])
                elif cmd == "scroll":
                    recorder.record_step("scroll")
                else:
                    print(f"Unknown command: {line}")
        else:
            # Automated CDP event tap mode
            cdp_port = args.cdp_port if args.cdp_port is not None else (9222 + args.screen)
            tap = CDPEventTap(recorder, cdp_port=cdp_port)
            attached = tap.start()
            if attached:
                print(f"[✓] Automated CDP event tap attached to Chrome on port {cdp_port}")
                print("    Perform demonstration actions in the browser window.")
                print("    Clicks, keystrokes, form inputs, and navigations are captured automatically.")
                print("    Press [Enter] or Ctrl+C when finished...")
                try:
                    input()
                except (EOFError, KeyboardInterrupt):
                    pass
                tap.stop()
            else:
                print(f"[!] Warning: Could not attach CDP event tap on port {cdp_port}.")
                print("    Falling back to interactive terminal REPL.")
                print("    Enter actions in format: 'navigate <url>', 'click <x> <y>', 'click @ref', 'type <text>', 'done'")
                while True:
                    try:
                        line = input("reach record> ").strip()
                    except (EOFError, KeyboardInterrupt):
                        break
                    if not line or line.lower() in ("done", "exit", "quit"):
                        break
                    parts = line.split(maxsplit=2)
                    cmd = parts[0].lower()
                    if cmd == "navigate" and len(parts) > 1:
                        recorder.record_step("navigate", url=parts[1])
                    elif cmd == "click" and len(parts) >= 2:
                        if parts[1].startswith("@") or parts[1].startswith("e"):
                            recorder.record_step("click", reference=parts[1])
                        elif len(parts) >= 3:
                            recorder.record_step("click", x=int(parts[1]), y=int(parts[2]))
                    elif cmd == "type" and len(parts) > 1:
                        recorder.record_step("type", text=parts[1])
                    elif cmd == "key" and len(parts) > 1:
                        recorder.record_step("key", key=parts[1])
                    elif cmd == "scroll":
                        recorder.record_step("scroll")
                    else:
                        print(f"Unknown command: {line}")

        trace = recorder.save_trace()
        print(f"[✓] Recorded {len(trace.steps)} step(s) to {recorder.trace_file}")

        # Auto-compile trace into routine.json
        compiler = RoutineCompiler()
        routine = compiler.compile(trace, routines_dir=args.routines_dir)
        print(f"[✓] Compiled routine with parameters {routine.parameters} to {recorder.routine_dir / 'routine.json'}")

    elif args.subcommand == "compile":
        compiler = RoutineCompiler()
        param_map = json.loads(args.params) if args.params else None
        routine = compiler.compile(args.name, parameter_mappings=param_map, routines_dir=args.routines_dir)
        print(f"[✓] Compiled routine '{routine.name}' with {len(routine.steps)} steps and parameters: {routine.parameters}")

    elif args.subcommand == "replay":
        params = json.loads(args.params) if args.params else None
        replayer = RoutineReplayer(
            routine_name=args.routine,
            routines_dir=args.routines_dir,
            api_url=args.api_url,
            screen=args.screen,
            sandbox=args.sandbox,
            heal_with_cua=not args.no_heal,
        )
        res = replayer.replay(params=params)
        if args.json:
            print(json.dumps(res.to_dict(), indent=2))
        else:
            print(f"\nResult: {res.status.upper()}")
            print(f"Success: {res.success}")
            print(f"Steps executed: {res.steps_executed}")
            print(f"Healed: {res.healed}")
            if res.error:
                print(f"Error: {res.error}")
        sys.exit(0 if res.success else 1)


if __name__ == "__main__":
    main()
