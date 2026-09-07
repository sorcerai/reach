#!/usr/bin/env python3
"""Reach Routine Engine: Demonstration Recorder, Compiler & Self-Healing Replayer.

Implements:
1. Routine Trace Recording: captures operational action metadata while keeping
   typed values, DOM snapshots, and screenshot captures ephemeral.
2. Routine Compiler: normalizes coordinates, requires named input parameters,
   and injects origin/text/visual verification checkpoints.
3. Self-Healing Replayer: executes steps deterministically, bounds recovery,
   and returns candidates without implicitly rewriting stored routines.
"""

from __future__ import annotations

import argparse
import base64
import copy
import hashlib
import json
import logging
import math
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
from scripts.reach_sensitive import (
    redact_action,
    redact_result,
    redact_step,
    routine_lock,
    safe_url,
    write_json_private,
)

DEFAULT_ROUTINES_DIR = Path.home() / ".reach" / "routines"

DEFAULT_API_URL = os.environ.get("REACH_AGENT_URL", "http://127.0.0.1:4200")

_SHA256_RE = re.compile(r"^[0-9a-f]{64}$")
_PHASH_RE = re.compile(r"^[0-9a-f]{16}$")
_SUPPORTED_ACTION_KINDS = {"click", "type", "key", "navigate", "wait", "scroll"}
_SENSITIVE_CANDIDATE_KEYS = {
    "text",
    "value",
    "typed_value",
    "input_value",
    "dom_snapshot",
    "observed_dom",
    "before_frame",
    "after_frame",
    "screenshot",
    "screenshot_path",
    "frame_path",
    "frames",
}
def _navigation_needs_runtime_url(value: Optional[str]) -> bool:
    """Return whether a navigation URL has data beyond its durable origin."""
    if not isinstance(value, str) or not value:
        return False
    try:
        parsed = urllib.parse.urlsplit(value)
    except ValueError:
        return False
    return (
        parsed.username is not None
        or parsed.password is not None
        or parsed.path not in ("", "/")
        or bool(parsed.query or parsed.fragment)
    )
def _navigation_input_name(
    target: Optional[str], action: CompiledAction, params: Dict[str, Any]
) -> Optional[str]:
    """Find a supplied named input whose full value is the healed URL."""
    if not isinstance(target, str) or not _navigation_needs_runtime_url(target):
        return None
    if action.input_name and params.get(action.input_name) == target:
        return action.input_name
    for name in sorted(params):
        if params.get(name) == target:
            return name
    return None

# ==============================================================================
# Data Models
# ==============================================================================

@dataclass
class TraceStep:
    """A single action with raw values retained only in process memory."""

    step_index: int
    timestamp: str
    action_type: str
    x: Optional[int] = None
    y: Optional[int] = None
    text: Optional[str] = None
    key: Optional[str] = None
    url: Optional[str] = None
    selector: Optional[str] = None
    aria_tag: Optional[str] = None
    reference: Optional[str] = None
    before_frame: Optional[str] = None
    after_frame: Optional[str] = None
    dom_snapshot: Optional[str] = None
    input_name: Optional[str] = None
    metadata: Dict[str, Any] = field(default_factory=dict)

    def to_dict(self) -> Dict[str, Any]:
        data = asdict(self)
        if self.reference is not None:
            data["ref"] = self.reference
        return redact_step(data)

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
            input_name=d.get("input_name") or d.get("parameter"),
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

    type: str
    value: Optional[str] = None
    expected_hash: Optional[str] = None
    threshold: float = 0.20
    frame_path: Optional[str] = None
    description: str = ""

    def to_dict(self) -> Dict[str, Any]:
        return {
            "type": self.type,
            "value": self.value,
            "expected_hash": self.expected_hash,
            "threshold": self.threshold,
            "description": "Verify checkpoint",
        }

    @classmethod
    def from_dict(cls, d: Dict[str, Any]) -> Checkpoint:
        return cls(
            type=d.get("type", "url_origin_equals"),

            value=d.get("value"),
            expected_hash=d.get("expected_hash"),
            threshold=float(d.get("threshold", 0.20)),
            frame_path=d.get("frame_path"),
            description=d.get("description", ""),
        )

@dataclass
class CompiledAction:
    """Semantic action specification with named inputs, never input defaults."""

    kind: str
    point: Optional[Tuple[int, int]] = None
    normalized_point: Optional[Tuple[float, float]] = None
    reference: Optional[str] = None
    url: Optional[str] = None
    selector: Optional[str] = None
    aria: Optional[str] = None
    value: Optional[str] = None
    input_name: Optional[str] = None
    key: Optional[str] = None
    button: str = "left"
    description: str = ""

    def to_dict(self) -> Dict[str, Any]:
        data: Dict[str, Any] = {
            "kind": self.kind,
            "point": list(self.point) if self.point else None,
            "normalized_point": list(self.normalized_point) if self.normalized_point else None,
            "url": self.url,
            "selector": self.selector,
            "aria": self.aria,
            "key": self.key,
            "button": self.button,
            "description": self.description,
        }
        if self.reference is not None:
            data["ref"] = self.reference
        if self.input_name is not None:
            data["input_name"] = self.input_name
            if self.kind == "type":
                data["value"] = f"{{{{{self.input_name}}}}}"
        return redact_action(data)

    @classmethod
    def from_dict(cls, d: Dict[str, Any]) -> CompiledAction:
        point = tuple(int(item) for item in d["point"]) if d.get("point") else None
        normalized = tuple(float(item) for item in d["normalized_point"]) if d.get("normalized_point") else None
        input_name = d.get("input_name") or d.get("parameter")
        value = d.get("value")
        if input_name and d.get("kind", "click") == "type":
            value = f"{{{{{input_name}}}}}"
        return cls(
            kind=d.get("kind", "click"),
            point=point,  # type: ignore[arg-type]
            normalized_point=normalized,  # type: ignore[arg-type]
            reference=d.get("ref") or d.get("reference"),
            selector=d.get("selector"),
            aria=d.get("aria"),
            value=value,
            input_name=input_name,
            url=safe_url(d.get("url")) if isinstance(d.get("url"), str) else None,
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
        # Parameter values are runtime-only and must never reach disk.
        return {
            "version": self.version,
            "name": self.name,
            "screen": self.screen,
            "compiled_at": self.compiled_at,
            "healed_at": self.healed_at,
            "parameters": {str(name): None for name in self.parameters},
            "steps": [s.to_dict() for s in self.steps],
        }

    @classmethod
    def from_dict(cls, d: Dict[str, Any]) -> CompiledRoutine:
        parameters = d.get("parameters", {})
        if not isinstance(parameters, dict):
            raise ValueError("routine parameters must be an object")
        return cls(
            version=d.get("version", 1),
            name=d["name"],
            screen=d.get("screen", 0),
            compiled_at=d.get("compiled_at", ""),
            healed_at=d.get("healed_at"),
            # Loaded records retain names only; values are caller supplied.
            parameters={str(name): None for name in parameters},
            steps=[CompiledStep.from_dict(s) for s in d.get("steps", [])],
        )

@dataclass
class ReplayResult:
    """Execution output from a routine replay run."""

    success: bool
    status: str  # completed, failed, healed, auth_required, approval_required, stale_observation, uncertain
    steps_executed: int
    parameters_used: Dict[str, Any]
    healed: bool = False
    healed_steps: List[Dict[str, Any]] = field(default_factory=list)
    error: Optional[str] = None
    takeover_url: Optional[str] = None
    duration_sec: float = 0.0

    def to_dict(self) -> Dict[str, Any]:
        return redact_result(asdict(self))

# ==============================================================================
# Helper Utilities
# ==============================================================================

def resolve_routine_dir(
    routine_name: str, base_dir: Optional[Union[str, Path]] = None
) -> Path:
    """Resolve directory path for a named routine."""
    base = Path(base_dir) if base_dir else DEFAULT_ROUTINES_DIR
    return base / routine_name

def _canonical_json(value: Any) -> bytes:
    return json.dumps(value, ensure_ascii=False, sort_keys=True, separators=(",", ":")).encode("utf-8")

def routine_digest(value: Union[str, Path, Dict[str, Any], CompiledRoutine]) -> str:
    """Return a stable SHA-256 digest for compare-and-swap promotion."""
    if isinstance(value, CompiledRoutine):
        payload: Any = value.to_dict()
    elif isinstance(value, (str, Path)):
        payload = json.loads(Path(value).read_text(encoding="utf-8"))
    else:
        payload = value
    return hashlib.sha256(_canonical_json(payload)).hexdigest()

def write_healing_candidate(
    path: Union[str, Path],
    routine_name: str,
    original_digest: str,
    actions: List[Dict[str, Any]],
) -> None:
    """Persist an ordered redacted replacement sequence; never called implicitly."""
    if not isinstance(original_digest, str) or not _SHA256_RE.fullmatch(original_digest):
        raise ValueError("original_digest must be a lowercase SHA-256 digest")
    safe_actions = []
    for action in actions:
        if not isinstance(action, dict):
            raise TypeError("candidate actions must be objects")
        if isinstance(action.get("step_index"), bool) or "step_index" not in action:
            raise ValueError("candidate actions require an explicit step_index")
        try:
            step_index = int(action["step_index"])
        except (TypeError, ValueError):
            raise ValueError("candidate actions require a valid step_index") from None
        safe_input = dict(action)
        if action.get("kind") == "navigate":
            target = action.get("target") or action.get("url")
            input_name = action.get("input_name") or action.get("parameter")
            if _navigation_needs_runtime_url(target) and (
                not isinstance(input_name, str) or not input_name.strip()
            ):
                raise ValueError(
                    "healed navigation requires an available named runtime input"
                )
            if "url" not in safe_input and isinstance(target, str):
                safe_input["url"] = target
                safe_input.pop("target", None)
        safe = redact_action(safe_input)
        safe["step_index"] = step_index
        safe_actions.append(safe)
    with routine_lock(path):
        write_json_private(
            path,
            {
                "kind": "healing_candidate",
                "version": 1,
                "routine_name": routine_name,
                "original_digest": original_digest,
                "actions": safe_actions,
            },
        )

def promote_candidate(
    routine_path: Union[str, Path],
    candidate_path: Union[str, Path],
    expected_digest: str,
    revalidate: Callable[[Dict[str, Any]], ReplayResult],
) -> Dict[str, Any]:
    """Replay and CAS-promote a healing candidate.

    ``revalidate`` must execute the proposed routine against the current
    authorized driver and return a successful, fully completed ``ReplayResult``.
    A boolean or structural predicate is intentionally rejected.
    """
    if not callable(revalidate):
        raise TypeError("revalidate callback is required")
    if not isinstance(expected_digest, str) or not _SHA256_RE.fullmatch(expected_digest):
        raise ValueError("expected_digest must be a lowercase SHA-256 digest")

    routine_file = Path(routine_path)
    candidate_file = Path(candidate_path)
    current = json.loads(routine_file.read_text(encoding="utf-8"))
    if not isinstance(current, dict):
        raise ValueError("routine record must be an object")
    actual_digest = routine_digest(current)
    if actual_digest != expected_digest:
        raise ValueError("stale healing candidate: original routine digest changed")
    parameters = current.get("parameters", {})
    if not isinstance(parameters, dict) or any(value is not None for value in parameters.values()):
        raise ValueError("routine contains persisted input values; refusing promotion")

    candidate = json.loads(candidate_file.read_text(encoding="utf-8"))
    if not isinstance(candidate, dict):
        raise ValueError("candidate record must be an object")
    if (
        candidate.get("kind") != "healing_candidate"
        or type(candidate.get("version")) is not int
        or candidate["version"] != 1
    ):
        raise ValueError("unsupported healing candidate contract")
    if candidate.get("routine_name") != current.get("name"):
        raise ValueError("candidate routine binding does not match routine")
    if candidate.get("original_digest") != expected_digest:
        raise ValueError("candidate original digest does not match expected digest")
    actions = candidate.get("actions")
    if not isinstance(actions, list) or not actions:
        raise ValueError("candidate must contain at least one action")

    steps = current.get("steps")
    if not isinstance(steps, list) or not steps:
        raise ValueError("routine has no replayable steps")
    by_index: Dict[int, Dict[str, Any]] = {}
    original_checkpoints: Dict[int, Any] = {}
    for step in steps:
        if not isinstance(step, dict) or isinstance(step.get("step_index"), bool):
            raise ValueError("routine contains an invalid step")
        try:
            index = int(step["step_index"])
        except (KeyError, TypeError, ValueError):
            raise ValueError("routine contains an invalid step index") from None
        if index in by_index:
            raise ValueError(f"routine contains duplicate step {index}")
        action = step.get("action")
        if not isinstance(action, dict):
            raise ValueError(f"routine step {index} has no action")
        if action.get("kind") == "type":
            input_name = action.get("input_name") or action.get("parameter")
            if (
                not isinstance(input_name, str)
                or not input_name.strip()
                or (
                    action.get("value") is not None
                    and action.get("value") != f"{{{{{input_name}}}}}"
                )
            ):
                raise ValueError(f"type step {index} lacks a safe named input template")
        by_index[index] = step
        original_checkpoints[index] = copy.deepcopy(step.get("checkpoints", []))

    replacement_groups: Dict[int, List[Dict[str, Any]]] = {}
    for action in actions:
        if not isinstance(action, dict):
            raise ValueError("candidate actions must be objects")
        forbidden = {
            str(key).lower() for key in action
        }.intersection(_SENSITIVE_CANDIDATE_KEYS | {"checkpoints"})
        if forbidden:
            raise ValueError("candidate contains raw input, frame, DOM, or checkpoint data")
        if isinstance(action.get("step_index"), bool):
            raise ValueError("candidate contains an invalid step index")
        try:
            index = int(action["step_index"])
        except (KeyError, TypeError, ValueError):
            raise ValueError("candidate contains an invalid step index") from None
        if index not in by_index:
            raise ValueError(f"candidate references unknown step {index}")
        kind = action.get("kind")
        if not isinstance(kind, str) or kind not in _SUPPORTED_ACTION_KINDS:
            raise ValueError("candidate contains an unsupported action kind")
        if kind == "navigate":
            target = action.get("target") or action.get("url")
            input_name = action.get("input_name") or action.get("parameter")
            if _navigation_needs_runtime_url(target) and (
                not isinstance(input_name, str) or not input_name.strip()
            ):
                raise ValueError(
                    "healed navigation requires an available named runtime input"
                )
        if kind == "type":
            input_name = action.get("input_name") or action.get("parameter")
            if not isinstance(input_name, str) or not input_name.strip():
                raise ValueError("type recovery action requires an explicit input_name")
        safe_input = dict(action)
        if kind == "navigate":
            navigation_target = action.get("target") or action.get("url")
            if "url" not in safe_input and isinstance(navigation_target, str):
                safe_input["url"] = navigation_target
                safe_input.pop("target", None)
        durable_action = redact_action(safe_input)
        durable_action.pop("step_index", None)
        if not durable_action:
            raise ValueError("candidate action has no safe operational fields")
        replacement_groups.setdefault(index, []).append(durable_action)

    proposed = copy.deepcopy(current)
    proposed_steps: List[Dict[str, Any]] = []
    expected_checkpoints: List[Any] = []
    next_index = 1
    for original_step in steps:
        index = int(original_step["step_index"])
        replacements = replacement_groups.get(index)
        if replacements is None:
            replacement_step = copy.deepcopy(original_step)
            replacement_step["step_index"] = next_index
            proposed_steps.append(replacement_step)
            expected_checkpoints.append(copy.deepcopy(original_checkpoints[index]))
            next_index += 1
            continue

        checkpoints = original_checkpoints[index]
        for replacement_offset, replacement_action in enumerate(replacements):
            replacement_step = {
                "step_index": next_index,
                "action": copy.deepcopy(replacement_action),
                "checkpoints": (
                    copy.deepcopy(checkpoints)
                    if replacement_offset == len(replacements) - 1
                    else []
                ),
            }
            proposed_steps.append(replacement_step)
            expected_checkpoints.append(copy.deepcopy(replacement_step["checkpoints"]))
            next_index += 1
    proposed["steps"] = proposed_steps

    if [step.get("checkpoints", []) for step in proposed_steps] != expected_checkpoints:
        raise ValueError("candidate changed original checkpoints")
    validation = revalidate(proposed)
    if not isinstance(validation, ReplayResult):
        raise TypeError("revalidate must return a ReplayResult from a real replay")
    if (
        not validation.success
        or validation.status != "completed"
        or validation.healed
        or validation.healed_steps
        or validation.steps_executed != len(proposed["steps"])
    ):
        raise ValueError("candidate failed replay revalidation")
    final_steps = proposed.get("steps")
    if not isinstance(final_steps, list):
        raise ValueError("candidate replay changed routine steps")
    if len(final_steps) != len(expected_checkpoints) or [
        step.get("checkpoints", []) for step in final_steps
    ] != expected_checkpoints:
        raise ValueError("candidate changed original checkpoints")
    for step in final_steps:
        if not isinstance(step, dict) or not isinstance(step.get("action"), dict):
            raise ValueError("candidate replay changed routine actions")
        action = step["action"]
        action_kind = action.get("kind")
        for key, value in action.items():
            lowered = str(key).lower()
            if lowered not in _SENSITIVE_CANDIDATE_KEYS:
                continue
            if (
                action_kind == "type"
                and lowered == "value"
                and isinstance(action.get("input_name"), str)
                and value in (None, f"{{{{{action['input_name']}}}}}")
            ):
                continue
            raise ValueError("candidate replay introduced raw input, frame, or DOM data")

    # Close the CAS window opened while the disposable replay was running.
    with routine_lock(routine_file):
        latest = json.loads(routine_file.read_text(encoding="utf-8"))
        if routine_digest(latest) != expected_digest:
            raise ValueError("stale healing candidate: routine changed during revalidation")
        write_json_private(routine_file, proposed)
    return proposed

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

def compute_frame_hash_hex(image_path: Union[str, Path]) -> Optional[str]:
    """Compute 16-character hexadecimal difference hash for an image file."""
    try:
        val = compute_dhash(Path(image_path).read_bytes(), size=8)
        return f"{val:016x}"
    except Exception as e:
        logger.debug("Failed to compute dHash for %s: %s", image_path, e)
        return None

def hash_distance(hex1: str, hex2: str) -> float:
    """Compute normalized Hamming distance between two 64-bit hex hashes."""
    if (
        not isinstance(hex1, str)
        or not isinstance(hex2, str)
        or not _PHASH_RE.fullmatch(hex1)
        or not _PHASH_RE.fullmatch(hex2)
    ):
        return 1.0
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

            emitEvent({
                type: 'click',
                timestamp: new Date().toISOString(),
                x: e.clientX,
                y: e.clientY,
                ref: ref,
                selector: sel,
                url: window.location.href,
            });
        } catch (err) {}
    }, true);

    // 2. Change/Input listener. Never read or serialize target.value.
    document.addEventListener('change', (e) => {
        try {
            const target = e.target;
            if (target && (target.tagName === 'INPUT' || target.tagName === 'TEXTAREA' || target.tagName === 'SELECT')) {
                const refEl = target.closest ? target.closest('[data-reach-ref]') : null;
                const ref = refEl ? ('@' + refEl.getAttribute('data-reach-ref')) : null;
                const sel = getSelector(target);
                const fieldName = target.getAttribute('data-reach-input-name') ||
                    target.getAttribute('name') || target.id || null;
                const fieldType = (target.getAttribute('type') || '').toLowerCase();
                const autocomplete = (target.getAttribute('autocomplete') || '').toLowerCase();
                const credentialField = fieldType === 'password' ||
                    autocomplete.includes('password') ||
                    autocomplete.includes('one-time-code') ||
                    autocomplete.includes('cc-') ||
                    autocomplete.includes('payment');
                emitEvent({
                    type: 'type',
                    timestamp: new Date().toISOString(),
                    ref: ref,
                    selector: sel,
                    input_name: fieldName,
                    credential_field: credentialField,
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
                except json.JSONDecodeError:
                    logger.debug("Discarding malformed recorder event batch")

    def _process_event(self, ev: Dict[str, Any]) -> None:
        ev_type = ev.get("type", "click")
        now = time.time()
        if now - self._last_event_ts < 0.2:
            pass
        self._last_event_ts = now

        x = ev.get("x")
        y = ev.get("y")
        # Event taps never carry typed values; only explicit parameter names.
        input_name = ev.get("input_name") or ev.get("parameter")
        key = ev.get("key")
        url = ev.get("url")
        selector = ev.get("selector")
        ref = ev.get("ref")

        self.recorder.record_step(
            action_type=ev_type,
            x=x,
            y=y,
            key=key,
            url=url,
            selector=selector,
            reference=ref,
            input_name=input_name,
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
        self.frames_dir = self.routine_dir / "frames"  # captures stay ephemeral
        self.trace_file = self.routine_dir / "trace.json"

        self.driver = driver or ReachDriver(
            api_url=api_url,
            screen=screen,
            sandbox=sandbox,
            enable_audit=False,
        )
        self.steps: List[TraceStep] = []
        self._current_url: Optional[str] = None

    def capture_frame(self, filename: str) -> Optional[str]:
        """Capture, hash for live validation, and discard the screenshot."""
        del filename
        shot_tmp: Optional[str] = None
        try:
            shot_tmp = self.driver.capture_screenshot(len(self.steps) + 1)
            if shot_tmp and os.path.isfile(shot_tmp):
                return compute_frame_hash_hex(shot_tmp)
        except Exception as exc:
            logger.debug("Error capturing ephemeral frame: %s", exc)
        finally:
            if shot_tmp and os.path.isfile(shot_tmp):
                try:
                    os.unlink(shot_tmp)
                except OSError:
                    pass
        return None

    def capture_dom_snapshot(self) -> str:
        """Capture page text from the live tab for checkpoint extraction."""
        if not self._current_url:
            return ""
        try:
            return self.driver.capture_page_text()
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
        input_name: Optional[str] = None,
        execute: bool = True,
        metadata: Optional[Dict[str, Any]] = None,
    ) -> TraceStep:
        """Record an action; screenshots/DOM remain ephemeral and are not archived."""
        step_idx = len(self.steps) + 1
        ts = datetime.now(timezone.utc).isoformat()

        if action_type == "navigate" and _navigation_needs_runtime_url(url) and not input_name:
            used_input_names = {
                step.input_name for step in self.steps if step.input_name
            }
            url_index = 1
            input_name = f"url_{url_index}"
            while input_name in used_input_names:
                url_index += 1
                input_name = f"url_{url_index}"

        if url:
            self._current_url = url
        before_hash = self.capture_frame(f"step_{step_idx:03d}_before.png")
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
            except Exception as exc:
                logger.warning("Step %s action execution error: %s", step_idx, type(exc).__name__)

        after_hash = self.capture_frame(f"step_{step_idx:03d}_after.png")
        dom_snapshot = self.capture_dom_snapshot()
        safe_metadata = dict(metadata or {})
        if before_hash:
            safe_metadata["before_frame_hash"] = before_hash
        if after_hash:
            safe_metadata["after_frame_hash"] = after_hash
        if dom_snapshot:
            keywords = [
                keyword
                for keyword in ("success", "dashboard", "results", "welcome", "account", "profile")
                if keyword in dom_snapshot.lower()
            ]
            if keywords:
                safe_metadata["dom_keywords"] = keywords[:2]

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
            input_name=input_name,
            metadata=safe_metadata,
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
        """Atomically persist only redacted operational metadata."""
        trace = RoutineTrace(
            name=self.routine_name,
            screen=self.screen,
            created_at=datetime.now(timezone.utc).isoformat(),
            steps=self.steps,
        )
        self.routine_dir.mkdir(parents=True, exist_ok=True)
        with routine_lock(self.trace_file):
            write_json_private(self.trace_file, trace.to_dict())
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

        # Mappings are explicit caller-provided names. Never infer from content.
        param_map: Dict[str, str] = dict(parameter_mappings or {})
        for step in trace.steps:
            if step.action_type == "type":
                name = step.input_name
                if not name and step.text and step.text in param_map:
                    name = param_map[step.text]
                if not name:
                    name = f"param_{len(parameters) + 1}"
                if step.text:
                    param_map[step.text] = name
                parameters[name] = None
            elif step.action_type == "navigate" and (
                step.input_name or _navigation_needs_runtime_url(step.url)
            ):
                name = step.input_name
                if not name and step.url:
                    name = param_map.get(step.url)
                if not name:
                    url_index = len(parameters) + 1
                    name = f"url_{url_index}"
                    while name in parameters:
                        url_index += 1
                        name = f"url_{url_index}"
                if step.url:
                    param_map[step.url] = name
                parameters[name] = None

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
        # Save only redacted routine metadata.
        routine_json_path = routine_dir / "routine.json"
        routine_dir.mkdir(parents=True, exist_ok=True)
        with routine_lock(routine_json_path):
            write_json_private(routine_json_path, compiled_routine.to_dict())

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
        data = json.loads(p.read_text(encoding="utf-8"))
        return RoutineTrace.from_dict(data)

    def _normalize_action(self, step: TraceStep, param_map: Dict[str, str]) -> CompiledAction:
        """Normalize coordinates and preserve only named input placeholders."""
        point = (step.x, step.y) if step.x is not None and step.y is not None else None
        normalized_point = None
        if point is not None:
            normalized_point = (
                round(point[0] / float(self.screen_width), 4),
                round(point[1] / float(self.screen_height), 4),
            )
        input_name = step.input_name
        if not input_name and step.action_type == "type" and step.text:
            input_name = param_map.get(step.text)
        if (
            not input_name
            and step.action_type == "navigate"
            and _navigation_needs_runtime_url(step.url)
            and step.url
        ):
            input_name = param_map.get(step.url)
        value = f"{{{{{input_name}}}}}" if step.action_type == "type" and input_name else None
        desc = f"{step.action_type.capitalize()}"
        if step.reference:
            desc += f" on ref '{step.reference}'"
        elif step.selector:
            desc += " on selector"
        elif step.aria_tag:
            desc += " on labeled element"
        elif point:
            desc += f" at ({point[0]}, {point[1]})"
        if input_name:
            desc += f" using input '{input_name}'"
        if step.key:
            desc += f" key '{step.key}'"
        return CompiledAction(
            kind=step.action_type,
            point=point,
            normalized_point=normalized_point,
            reference=step.reference,
            url=safe_url(step.url) if step.url else None,
            selector=step.selector,
            aria=step.aria_tag,
            value=value,
            input_name=input_name,
            key=step.key,
            description=desc,
        )

    def _generate_checkpoints(
        self, step: TraceStep, routine_dir: Path
    ) -> List[Checkpoint]:
        """Inject checkpoints from origin URLs, keyword metadata, and hashes."""
        del routine_dir
        checkpoints: List[Checkpoint] = []
        if step.action_type == "navigate" and step.url:
            origin = safe_url(step.url)
            if origin:
                checkpoints.append(
                    Checkpoint(
                        type="url_origin_equals",
                        value=origin,
                        description="Verify origin",
                    )

                )
        keywords = step.metadata.get("dom_keywords", [])
        if isinstance(keywords, list) and keywords:
            keyword = str(keywords[0])
            checkpoints.append(
                Checkpoint(
                    type="text_contains",
                    value=keyword,
                    description="Verify operational page keyword",
                )
            )
        hash_hex = step.metadata.get("after_frame_hash")
        if isinstance(hash_hex, str) and hash_hex:
            checkpoints.append(
                Checkpoint(
                    type="visual_phash",
                    expected_hash=hash_hex,
                    threshold=0.20,
                    description="Verify live visual anchor",
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
        lease_token: Optional[str] = None,
        handoff_gen: Optional[int] = None,
        heal_with_cua: bool = True,
    ) -> None:
        self.routine_name = routine_name
        self.routine_dir = resolve_routine_dir(routine_name, routines_dir)
        self.routine_file = self.routine_dir / "routine.json"
        self.heal_with_cua = heal_with_cua
        self.original_digest = ""
        self._last_mutation_uncertain = False
        self._last_execution_status: Optional[str] = None
        self.routine = self._load_routine()
        target_screen = screen if screen is not None else self.routine.screen
        self.driver = driver or ReachDriver(
            api_url=api_url,
            screen=target_screen,
            sandbox=sandbox,
            lease_token=lease_token,
            handoff_gen=handoff_gen,
            enable_audit=False,
        )

    def _load_routine(self) -> CompiledRoutine:
        if not self.routine_file.exists():
            raise FileNotFoundError(f"Routine not found at {self.routine_file}")
        data = json.loads(self.routine_file.read_text(encoding="utf-8"))
        self.original_digest = routine_digest(data)
        return CompiledRoutine.from_dict(data)

    def replay(
        self,
        params: Optional[Dict[str, Any]] = None,
        max_healing_steps: int = 5,
        max_healing_attempts: int = 1,
        require_explicit_parameters: bool = False,
    ) -> ReplayResult:
        """Replay with caller-supplied inputs and bounded healing."""
        start_time = time.time()
        if max_healing_steps < 0 or max_healing_attempts < 0:
            raise ValueError("healing bounds must be nonnegative")
        merged_params: Dict[str, Any] = {}
        if params:
            merged_params.update(params)
        try:
            typed_inputs = self._required_input_names()
        except ValueError as input_error:
            return ReplayResult(
                success=False,
                status="invalid_routine",
                steps_executed=0,
                parameters_used={},
                error=str(input_error),
                duration_sec=round(time.time() - start_time, 2),
            )
        url_inputs = {
            step.action.input_name
            for step in self.routine.steps
            if step.action.kind == "navigate" and step.action.input_name
        }
        required_inputs = (
            typed_inputs
            if require_explicit_parameters
            else set(self.routine.parameters) | url_inputs
        )
        missing = sorted(
            name for name in required_inputs
            if name not in merged_params or merged_params[name] is None
        )
        if missing:
            return ReplayResult(
                success=False,
                status="missing_parameters",
                steps_executed=0,
                parameters_used={name: "<missing>" for name in missing},
                error="Required named inputs were not supplied",
                duration_sec=round(time.time() - start_time, 2),
            )
        for step in self.routine.steps:
            action = step.action
            if action.kind != "navigate" or not action.input_name:
                continue
            runtime_url = merged_params[action.input_name]
            if not isinstance(runtime_url, str) or safe_url(runtime_url) != action.url:
                return ReplayResult(
                    success=False,
                    status="invalid_parameters",
                    steps_executed=0,
                    parameters_used={name: "<provided>" for name in merged_params},
                    error="Named navigation URL input does not match the retained origin",
                    duration_sec=round(time.time() - start_time, 2),
                )
        logger.info("Replaying routine '%s' with input names: %s", self.routine_name, sorted(merged_params))
        executed_count = 0
        healed_overall = False
        healed_step_records: List[Dict[str, Any]] = []
        healing_attempts = 0

        i = 0
        while i < len(self.routine.steps):
            step = self.routine.steps[i]
            executed_count += 1

            # Render parameter variables
            action = copy.deepcopy(step.action)
            action.value = render_template(action.value, merged_params)
            if action.kind == "navigate" and action.input_name:
                action.url = merged_params[action.input_name]
            else:
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

            if not exec_ok and self._last_execution_status in {
                "approval_required",
                "stale_observation",
            }:
                return ReplayResult(
                    success=False,
                    status=self._last_execution_status,
                    steps_executed=executed_count,
                    parameters_used={name: "<provided>" for name in merged_params},
                    error=exec_err or self._last_execution_status,
                    duration_sec=round(time.time() - start_time, 2),
                )
            # An uncertain mutation has unknown side effects; never replay or heal.
            if not exec_ok and self._last_mutation_uncertain:
                return ReplayResult(
                    success=False,
                    status="uncertain",
                    steps_executed=executed_count,
                    parameters_used={name: "<provided>" for name in merged_params},
                    error=exec_err or "Mutation outcome is uncertain",
                    duration_sec=round(time.time() - start_time, 2),
                )
            if not exec_ok or not validation_ok:
                failure_desc = exec_err if not exec_ok else checkpoint_reason
                logger.warning(
                    "[!] Step %s roadblock detected: %s",
                    step.step_index,
                    failure_desc,
                )

                healing_attempts += 1
                if healing_attempts > max_healing_attempts:
                    return ReplayResult(
                        success=False,
                        status="failed",
                        steps_executed=executed_count,
                        parameters_used={name: "<provided>" for name in merged_params},
                        error="Healing attempt bound exhausted",
                        duration_sec=round(time.time() - start_time, 2),
                    )
                if not self.heal_with_cua:
                    return ReplayResult(
                        success=False,
                        status="failed",
                        steps_executed=executed_count,
                        parameters_used={name: "<provided>" for name in merged_params},
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
                        parameters_used={name: "<provided>" for name in merged_params},
                        takeover_url=takeover_url,
                        error="Authentication wall detected during healing",
                        duration_sec=round(time.time() - start_time, 2),
                    )

                if not heal_ok:
                    return ReplayResult(
                        success=False,
                        status="failed",
                        steps_executed=executed_count,
                        parameters_used={name: "<provided>" for name in merged_params},
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

                # Recovery candidates are evidence, not permission to rewrite the routine.
                i += 1
                continue

            i += 1

        duration = round(time.time() - start_time, 2)
        status = "healed" if healed_overall else "completed"
        return ReplayResult(
            success=True,
            status=status,
            steps_executed=executed_count,
            parameters_used={name: "<provided>" for name in merged_params},
            healed=healed_overall,
            healed_steps=healed_step_records,
            duration_sec=duration,
        )

    def _required_input_names(self) -> set[str]:
        names: set[str] = set()
        for step in self.routine.steps:
            action = step.action
            if action.kind == "type":
                if not action.input_name or action.value != f"{{{{{action.input_name}}}}}":
                    raise ValueError(
                        f"type step {step.step_index} lacks an explicit named input"
                    )
                names.add(action.input_name)
            elif action.kind == "navigate" and action.input_name:
                if not action.url or safe_url(action.url) != action.url:
                    raise ValueError(
                        f"navigate step {step.step_index} lacks a retained allowed origin"
                    )
                names.add(action.input_name)
        return names

    def revalidate_candidate(
        self, proposed: Dict[str, Any], params: Optional[Dict[str, Any]] = None
    ) -> ReplayResult:
        """Replay a candidate in memory without healing or writing records."""
        previous = self.routine
        try:
            self.routine = CompiledRoutine.from_dict(copy.deepcopy(proposed))
            return self.replay(
                params=params,
                max_healing_steps=0,
                max_healing_attempts=0,
                require_explicit_parameters=True,
            )
        finally:
            self.routine = previous

    def _execute_deterministic_action(
        self, action: CompiledAction
    ) -> Tuple[bool, Optional[str]]:
        """Dispatch deterministic action through the driver's authorized API."""
        self._last_mutation_uncertain = False
        self._last_execution_status = None
        stop_statuses = {"approval_required", "stale_observation"}
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
            if isinstance(res, dict):
                status = res.get("status")
                if status in stop_statuses:
                    self._last_execution_status = status
                meta = res.get("_meta")
                uncertain = (
                    status in {"uncertain", "transport_uncertain"}
                    or res.get("uncertain") is True
                    or (isinstance(meta, dict) and meta.get("status") == "uncertain")
                )
                self._last_mutation_uncertain = bool(uncertain)
                if res.get("error"):
                    return False, str(res["error"])
                if uncertain:
                    return False, "Mutation outcome is uncertain"
                if status in stop_statuses:
                    return False, status
            time.sleep(0.5)
            return True, None
        except Exception as e:
            message = str(e)
            lowered = message.lower()
            exception_status = getattr(e, "status", None)
            if exception_status in stop_statuses:
                self._last_execution_status = exception_status
                return False, message
            if "approval_required" in lowered:
                self._last_execution_status = "approval_required"
                return False, message
            if "stale" in lowered or "fresh observation" in lowered:
                self._last_execution_status = "stale_observation"
                return False, message
            # Driver transport failures cannot establish whether a mutation ran.
            self._last_mutation_uncertain = True
            return False, message

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
            if cp.type in ("url_origin_equals", "text_contains") and (
                not isinstance(cp.value, str) or not cp.value.strip()
            ):
                return False, cp, "Checkpoint requires a nonempty expected value"
            if cp.type == "url_origin_equals":
                url_to_check = self._get_current_url()
                expected_origin = safe_url(cp.value or "")
                current_origin = safe_url(url_to_check or "")
                if (
                    not expected_origin
                    or not current_origin
                    or current_origin != expected_origin
                ):
                    return False, cp, "URL origin checkpoint failed"
            elif cp.type == "text_contains":
                if current_dom is None:
                    current_dom = self._get_current_dom()
                if cp.value and cp.value.lower() not in current_dom.lower():
                    return False, cp, "Operational page keyword checkpoint failed"
            elif cp.type == "visual_phash":
                if current_shot is None:
                    try:
                        current_shot = self.driver.capture_screenshot(999)
                    except Exception:
                        return False, cp, "Visual checkpoint lacks live screenshot evidence"
                if current_hash is None and current_shot:
                    current_hash = compute_frame_hash_hex(current_shot)
                    try:
                        if current_shot and os.path.isfile(current_shot):
                            os.unlink(current_shot)
                    except OSError:
                        pass
                if (
                    not isinstance(cp.expected_hash, str)
                    or not _PHASH_RE.fullmatch(cp.expected_hash)
                    or not isinstance(current_hash, str)
                    or not _PHASH_RE.fullmatch(current_hash)
                    or not math.isfinite(cp.threshold)
                    or not 0 <= cp.threshold <= 1
                ):
                    return False, cp, "Visual checkpoint lacks valid evidence or threshold"
                dist = hash_distance(cp.expected_hash, current_hash)
                if dist > cp.threshold:
                    return False, cp, f"Visual distance {dist:.2f} exceeds threshold"
            else:
                return False, cp, f"Unsupported checkpoint type: {cp.type}"

        return True, None, ""

    def _get_current_url(self) -> Optional[str]:
        """Retrieve current URL from sandbox."""
        # Query via MCP or inspect
        try:
            res = self.driver.call_mcp_tool("page_text", {"timeout_ms": 2000})
            if isinstance(res, dict) and not res.get("isError"):
                for block in res.get("content", []):
                    if block.get("type") == "text":
                        page = json.loads(block["text"])
                        if page.get("status") == "ok" and isinstance(page.get("url"), str):
                            return page["url"]
        except Exception:
            pass
        return None

    def _get_current_dom(self) -> str:
        """Retrieve current page text."""
        try:
            return self.driver.capture_page_text()
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
            lease_token=self.driver.lease_token,
            handoff_gen=self.driver.handoff_gen,
            enable_audit=False,
        )
        try:
            result = healing_driver.drive(goal=healing_goal)
        except Exception as exc:
            healing_driver.cleanup()
            return False, [], f"Healing driver failed: {exc}", None
        healing_driver.cleanup()
        if result.status not in ("completed", "unverified") or not failed_step.checkpoints:
            return False, [], result.error or "Healing requires a verifiable postcondition", None
        if not result.steps:
            return False, [], "Healing made no progress", None
        verified, _, reason = self._validate_checkpoints(failed_step.checkpoints)
        if not verified:
            return False, [], reason, None

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

            target = (act.target or act.value) if act.kind == "navigate" else None
            navigation_input_name: Optional[str] = None
            if act.kind == "navigate":
                if not isinstance(target, str) or not safe_url(target):
                    return False, [], "Healing produced an invalid navigation URL", None
                navigation_input_name = _navigation_input_name(target, action, params)
                if _navigation_needs_runtime_url(target) and not navigation_input_name:
                    return (
                        False,
                        [],
                        "Healing navigation requires a supplied named runtime input",
                        None,
                    )

            new_actions.append(
                {
                    "ref": act.ref,
                    "kind": act.kind,
                    "point": list(act.point) if act.point else None,
                    "normalized_point": point_norm,
                    "url": safe_url(target) if act.kind == "navigate" else None,
                    "selector": act.target if act.kind != "navigate" else None,
                    "input_name": (
                        action.input_name
                        if act.kind == "type"
                        else navigation_input_name
                    ),
                    "key": act.key,
                    "button": act.button,
                    "description": f"Healed {act.kind}",
                }
            )

        if not new_actions:
            return False, [], "Healing produced no actionable progress", None
        return True, new_actions, None, None

    def export_candidate(
        self, actions: List[Dict[str, Any]], path: Union[str, Path], step_index: int
    ) -> None:
        """Persist an ordered replacement sequence for one failed step."""
        candidate_actions: List[Dict[str, Any]] = []
        for action in actions:
            if not isinstance(action, dict):
                raise TypeError("candidate actions must be objects")
            candidate_action = dict(action)
            candidate_action["step_index"] = step_index
            candidate_actions.append(candidate_action)
        write_healing_candidate(
            path, self.routine_name, self.original_digest, candidate_actions
        )
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
    # 4. promote
    p_promote = subparsers.add_parser("promote", help="Replay and explicitly CAS-promote a healed candidate")
    p_promote.add_argument("--routine", required=True, help="Routine name")
    p_promote.add_argument("--candidate", required=True, help="Candidate JSON path")
    p_promote.add_argument("--expected-digest", required=True, help="Original routine SHA-256 digest")
    p_promote.add_argument("--routines-dir", default=None, help="Base routines directory")
    p_promote.add_argument("--api-url", default=DEFAULT_API_URL, help="Reach agent API URL")
    p_promote.add_argument("--screen", type=int, default=None, help="Screen ID override")
    p_promote.add_argument("--sandbox", default=None, help="Sandbox name or ID")
    p_promote.add_argument("--params", default=None, help="JSON values for every named input")
    p_promote.add_argument("--revalidate", action="store_true", help="Replay candidate against current state before writing")

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
    elif args.subcommand == "promote":
        if not args.revalidate:
            parser.error("promote requires --revalidate; candidate writes are never implicit")
        params = json.loads(args.params) if args.params else {}
        if not isinstance(params, dict):
            parser.error("--params must be a JSON object")
        handoff_raw = os.environ.get("REACH_HANDOFF_GEN")
        try:
            handoff_gen = int(handoff_raw) if handoff_raw is not None else None
        except ValueError:
            parser.error("REACH_HANDOFF_GEN must be an integer")
        routine_path = resolve_routine_dir(args.routine, args.routines_dir) / "routine.json"
        replayer = RoutineReplayer(
            routine_name=args.routine,
            routines_dir=args.routines_dir,
            api_url=args.api_url,
            screen=args.screen,
            sandbox=args.sandbox,
            lease_token=os.environ.get("REACH_LEASE_TOKEN"),
            handoff_gen=handoff_gen,
            heal_with_cua=False,
        )
        try:
            proposed = promote_candidate(
                routine_path,
                args.candidate,
                args.expected_digest,
                revalidate=lambda value: replayer.revalidate_candidate(value, params=params),
            )
        finally:
            replayer.driver.cleanup()
        print(f"[✓] Promoted candidate for '{args.routine}' at digest {routine_digest(proposed)}")

if __name__ == "__main__":
    main()
