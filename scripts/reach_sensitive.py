"""Secret-safe persistence primitives for Reach routines.

Durable routine records intentionally contain only operational metadata.  This
module does not try to identify secrets heuristically: callers must provide
named input parameters, while arbitrary typed text is treated as untrusted and
is omitted from persistence.
"""
from __future__ import annotations

import contextlib
import fcntl
import json
import os
import stat
import tempfile
import urllib.parse
from pathlib import Path
from typing import Any, Iterator, Mapping

_DROP_KEYS = {
    "text",
    "value",
    "typed_value",
    "input_value",
    "description",
    "goal",
    "error",
    "exception",
    "dom_snapshot",
    "observed_dom",
    "before_frame",
    "after_frame",
    "screenshot",
    "screenshot_path",
    "frame_path",
    "frames",
    "content",
    "aria",
    "aria_tag",
    "role",
}
_SAFE_METADATA_KEYS = {"source", "dom_keywords", "before_frame_hash", "after_frame_hash", "credential_field"}
_SAFE_DOM_KEYWORDS = {"success", "dashboard", "results", "welcome", "account", "profile"}
_SAFE_ACTION_KEYS = {
    "kind",
    "action_type",
    "step_index",
    "timestamp",
    "x",
    "y",
    "point",
    "normalized_point",
    "ref",
    "reference",
    "selector",
    "key",
    "button",
    "input_name",
    "url",
    "target",
    "metadata",
    "source",
    "parameter",
    "parameters",
}


def safe_url(value: str) -> str:
    """Return an origin-only URL, dropping credentials, path, query, fragment.

    Invalid or non-network URLs return an empty string.  Userinfo is never
    retained, and only the standard HTTP(S)/WebSocket origins are accepted.
    """
    if not isinstance(value, str) or not value:
        return ""
    try:
        parsed = urllib.parse.urlsplit(value)
        if parsed.scheme.lower() not in {"http", "https", "ws", "wss"}:
            return ""
        if not parsed.hostname:
            return ""
        hostname = parsed.hostname
        if ":" in hostname and not hostname.startswith("["):
            hostname = f"[{hostname}]"
        else:
            hostname = hostname.encode("idna").decode("ascii")
        scheme = parsed.scheme.lower()
        origin = f"{scheme}://{hostname}"
        default_port = 80 if scheme in {"http", "ws"} else 443
        if parsed.port is not None and parsed.port != default_port:
            origin += f":{parsed.port}"
        return origin
    except (TypeError, ValueError, UnicodeError):
        return ""


@contextlib.contextmanager
def routine_lock(path: str | os.PathLike[str]) -> Iterator[None]:
    """Hold the persistent per-routine lock shared with the Rust writer."""
    target = Path(path)
    parent = target.parent
    parent.mkdir(parents=True, exist_ok=True, mode=0o700)
    lock_path = parent / ".routine.lock"
    fd = os.open(lock_path, os.O_RDWR | os.O_CREAT, 0o600)
    try:
        os.fchmod(fd, stat.S_IRUSR | stat.S_IWUSR)
        fcntl.flock(fd, fcntl.LOCK_EX)
        try:
            yield
        finally:
            fcntl.flock(fd, fcntl.LOCK_UN)
    finally:
        os.close(fd)

def _metadata(value: Any) -> Any:
    """Recursively retain only small, non-sensitive operational metadata."""
    if isinstance(value, Mapping):
        output: dict[str, Any] = {}
        for key, item in value.items():
            key_text = str(key)
            lowered = key_text.lower()
            if lowered in _DROP_KEYS or lowered not in _SAFE_METADATA_KEYS:
                continue
            if lowered == "dom_keywords":
                if isinstance(item, (list, tuple)):
                    keywords = [
                        str(keyword).lower()
                        for keyword in item[:4]
                        if str(keyword).lower() in _SAFE_DOM_KEYWORDS
                    ]
                    if keywords:
                        output[key_text] = keywords
                continue
            if lowered in {"before_frame_hash", "after_frame_hash"}:
                if isinstance(item, str) and len(item) <= 64:
                    output[key_text] = item
                continue
            if isinstance(item, (str, bool, int, float)):
                output[key_text] = item
        return output
    if isinstance(value, (list, tuple)):
        return [_metadata(item) for item in value[:32]]
    if isinstance(value, (str, int, float, bool)) or value is None:
        if isinstance(value, str) and len(value) > 256:
            return value[:256]
        return value
    return None


def redact_action(action: Mapping[str, Any]) -> dict[str, Any]:
    """Redact an action for durable storage without guessing secret patterns.

    Typed values are *always* omitted.  A caller may persist an explicit
    ``input_name``/``parameter`` so replay can request the value later.
    """
    result: dict[str, Any] = {}
    for key, value in action.items():
        key_text = str(key)
        lowered = key_text.lower()
        if lowered in _DROP_KEYS or any(token in lowered for token in ("secret", "password", "credential", "typed_value")):
            continue
        if key_text not in _SAFE_ACTION_KEYS and lowered not in _SAFE_ACTION_KEYS:
            continue
        if lowered in {"url", "target"}:
            if isinstance(value, str):
                result[key_text] = safe_url(value)
            continue
        if lowered == "metadata":
            cleaned = _metadata(value)
            if cleaned:
                result[key_text] = cleaned
            continue
        if lowered in {"input_name", "parameter"}:
            if isinstance(value, str) and value and len(value) <= 128:
                result[key_text] = value
            continue
        cleaned = _metadata(value)
        if cleaned is not None:
            result[key_text] = cleaned
    return result


def redact_step(step: Mapping[str, Any]) -> dict[str, Any]:
    """Convert a trace step to operational metadata only."""
    return redact_action(step)


def redact_result(result: Mapping[str, Any]) -> dict[str, Any]:
    """Redact replay output before it is serialized or returned as a record."""
    allowed = {
        "success",
        "status",
        "steps_executed",
        "healed",
        "duration_sec",
        "takeover_url",
        "parameters_used",
        "healed_steps",
    }
    output: dict[str, Any] = {}
    for key in allowed:
        if key not in result:
            continue
        value = result[key]
        if key == "parameters_used":
            if isinstance(value, Mapping):
                output[key] = sorted(str(name) for name in value)
            elif isinstance(value, (list, tuple, set)):
                output[key] = sorted(str(name) for name in value)
            else:
                output[key] = []
        elif key == "takeover_url":
            output[key] = safe_url(value) if isinstance(value, str) else None
        elif key == "healed_steps" and isinstance(value, list):
            output[key] = [redact_action(item) for item in value if isinstance(item, Mapping)]
        else:
            output[key] = _metadata(value)
    return output
def write_json_private(path: str | os.PathLike[str], data: Any) -> None:
    """Atomically replace ``path`` with mode 0600 JSON data."""
    target = Path(path)
    target.parent.mkdir(parents=True, exist_ok=True, mode=0o700)
    fd, temporary = tempfile.mkstemp(prefix=f".{target.name}.", dir=str(target.parent))
    temp_path = Path(temporary)
    try:
        os.fchmod(fd, stat.S_IRUSR | stat.S_IWUSR)
        payload = json.dumps(data, ensure_ascii=False, indent=2, sort_keys=True).encode("utf-8")
        with os.fdopen(fd, "wb") as stream:
            stream.write(payload)
            stream.flush()
            os.fsync(stream.fileno())
        os.replace(temp_path, target)
        os.chmod(target, stat.S_IRUSR | stat.S_IWUSR)
        try:
            dir_fd = os.open(target.parent, os.O_RDONLY)
            try:
                os.fsync(dir_fd)
            finally:
                os.close(dir_fd)
        except OSError:
            pass
    except Exception:
        try:
            os.close(fd)
        except OSError:
            pass
        try:
            temp_path.unlink()
        except FileNotFoundError:
            pass
        raise
