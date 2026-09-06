"""Hermes plugin: reach-agent-computer.

One reach screen per Hermes profile: leased on session start, routed into every
reach MCP tool call (``mcp__reach__*``), released on session finalize. Adds
``reach_drive`` (CUA loop, Gemini 3.8 Flash via agy) plus lease/status tools.

Config (``plugins.entries.reach-agent-computer.settings`` in config.yaml):
  api_url:   reach host daemon base URL (default http://127.0.0.1:4200)
  repo_root: agent-computer checkout holding scripts/reach_drive.py
             (default: derived from this file's real path)
"""

from __future__ import annotations

import json
import logging
import os
import sys
from pathlib import Path
from typing import Any, Callable, Dict, Optional

logger = logging.getLogger("hermes.plugins.reach_agent_computer")

API_DEFAULT = "http://127.0.0.1:4200"
OWNER_DEFAULT = "default"
MCP_PREFIX = "mcp__reach__"

_ctx: Any = None  # PluginContext, set by register(); None under unit tests

_state: Dict[str, Any] = {
    "screen": None,
    "leased_at": None,
    "novnc_url": None,
    "owner": None,
    "error": None,
}


def _cfg(key: str, default: Any = None) -> Any:
    if _ctx is None:
        return default
    try:
        return _ctx.get_config(key, default)
    except Exception:
        return default


def get_api_url() -> str:
    # ponytail: env fallback only for standalone tests; behavior config lives in config.yaml
    return str(_cfg("api_url") or os.environ.get("REACH_AGENT_URL") or API_DEFAULT).rstrip("/")


def get_owner() -> str:
    """Lease owner = active Hermes profile, so profiles never share a screen."""
    if _ctx is not None:
        try:
            return _ctx.profile_name
        except Exception:
            pass
    return os.environ.get("HERMES_PROFILE", OWNER_DEFAULT)


def get_state() -> Dict[str, Any]:
    return dict(_state)


def reset_state() -> None:
    for k in _state:
        _state[k] = None


def _http_request(
    path: str,
    method: str = "GET",
    body: Optional[Dict[str, Any]] = None,
    api_url: Optional[str] = None,
    timeout: float = 10.0,
) -> Any:
    import urllib.request

    base = (api_url or get_api_url()).rstrip("/")
    data = json.dumps(body).encode("utf-8") if body is not None else None
    headers = {"content-type": "application/json"} if data is not None else {}
    req = urllib.request.Request(f"{base}{path}", data=data, headers=headers, method=method)
    with urllib.request.urlopen(req, timeout=timeout) as resp:
        raw = resp.read().decode("utf-8")
        return json.loads(raw) if raw else {}


# --------------------------------------------------------------------------
# Tool implementations (kwargs API; wrapped into registry handlers below)
# --------------------------------------------------------------------------


def reach_lease_screen(
    screen: Optional[int] = None,
    owner: Optional[str] = None,
    api_url: Optional[str] = None,
) -> Dict[str, Any]:
    import urllib.error

    lease_owner = owner or get_owner()
    api = api_url or get_api_url()
    try:
        screens = _http_request("/agent/screens", api_url=api)
    except Exception as e:
        return {"status": "error", "message": f"Failed to list screens: {e}"}

    target = screen
    if target is None:
        free = next((s for s in screens if s.get("owner") in (None, lease_owner)), None)
        if free is None:
            return {"status": "exhausted", "message": "No free Agent Computer screens available."}
        target = free.get("id", 0)

    try:
        _http_request(f"/agent/screens/{target}/lease", "POST", {"owner": lease_owner}, api)
    except urllib.error.HTTPError as e:
        detail = e.read().decode("utf-8", "replace")
        return {"status": "conflict", "message": f"Failed to lease screen {target} (HTTP {e.code}): {detail}"}
    except Exception as e:
        return {"status": "error", "message": f"Lease request failed: {e}"}

    info = next((s for s in screens if s.get("id") == target), {})
    _state.update(screen=target, owner=lease_owner, novnc_url=info.get("novnc_url", ""),
                  leased_at=info.get("leased_at"), error=None)
    return {"status": "ok", "screen": target, "owner": lease_owner, "novnc_url": _state["novnc_url"]}


def reach_release_screen(
    screen: Optional[int] = None,
    owner: Optional[str] = None,
    api_url: Optional[str] = None,
) -> Dict[str, Any]:
    lease_owner = owner or _state.get("owner") or get_owner()
    target = screen if screen is not None else _state.get("screen")
    if target is None:
        return {"status": "noop", "message": "No screen currently leased."}
    try:
        _http_request(f"/agent/screens/{target}/lease", "DELETE", {"owner": lease_owner}, api_url)
    except Exception as e:
        return {"status": "error", "message": f"Failed to release screen {target}: {e}"}
    if _state.get("screen") == target:
        reset_state()
    return {"status": "ok", "screen": target, "released": True}


def reach_status(screen: Optional[int] = None, api_url: Optional[str] = None) -> Dict[str, Any]:
    try:
        screens = _http_request("/agent/screens", api_url=api_url)
    except Exception as e:
        return {"status": "error", "message": f"Failed to query reach status: {e}"}
    if screen is not None:
        found = next((s for s in screens if s.get("id") == screen), None)
        if found is None:
            return {"status": "not_found", "message": f"Screen {screen} not found."}
        return {"status": "ok", "screen": found}
    return {"status": "ok", "current_session": get_state(), "screens": screens}


def _repo_root() -> Path:
    configured = _cfg("repo_root")
    if configured:
        return Path(str(configured)).expanduser()
    # <repo>/integrations/hermes/plugins/reach-agent-computer/__init__.py; resolve() follows the
    # ~/.hermes/plugins symlink back to the checkout.
    return Path(__file__).resolve().parents[4]


def reach_drive(
    goal: str,
    screen: Optional[int] = None,
    max_steps: int = 15,
    api_url: Optional[str] = None,
    initial_url: Optional[str] = None,
) -> Dict[str, Any]:
    target = screen if screen is not None else (_state.get("screen") or 0)
    root = str(_repo_root())
    if root not in sys.path:
        sys.path.insert(0, root)
    try:
        from scripts.reach_drive import ReachDriver
    except ImportError as e:
        return {"status": "error", "message": f"Could not import scripts.reach_drive from {root}: {e}"}

    driver = ReachDriver(api_url=api_url or get_api_url(), screen=target, max_steps=max_steps)
    try:
        return driver.drive(goal=goal, initial_url=initial_url).to_dict()
    except Exception as e:
        logger.exception("reach_drive execution failed")
        return {"status": "failed", "error": str(e)}


# --------------------------------------------------------------------------
# Lifecycle hooks
# --------------------------------------------------------------------------


def on_session_start(**kw: Any) -> None:
    """Lease a screen for this profile. Announced to the model via pre_llm_call (first turn only)."""
    res = reach_lease_screen()
    if res.get("status") != "ok":
        _state["error"] = res.get("message")
        logger.warning("Agent Computer screen auto-lease failed: %s", res.get("message"))


def pre_llm_call(is_first_turn: bool = False, **kw: Any) -> Optional[Dict[str, str]]:
    """First-turn context only: ephemeral user-message injection, never the system prompt."""
    if not is_first_turn:
        return None
    if _state.get("screen") is not None:
        return {"context": f"Agent Computer screen {_state['screen']} is leased to this session. "
                           f"Live view: {_state.get('novnc_url') or 'unknown'}"}
    if _state.get("error"):
        return {"context": f"Agent Computer unavailable: {_state['error']}"}
    return None


def pre_tool_call(tool_name: str = "", args: Optional[Dict[str, Any]] = None, **kw: Any) -> Optional[Dict[str, Any]]:
    """Route reach MCP tools and our own reach_* tools to the leased screen."""
    args = args or {}
    is_reach = tool_name.startswith(MCP_PREFIX) or tool_name in PLUGIN_TOOLS
    if is_reach and _state.get("screen") is not None and args.get("screen") is None:
        return {"action": "modify", "args": {"screen": _state["screen"]}}
    return None


def post_tool_call(tool_name: str = "", args: Optional[Dict[str, Any]] = None, result: Any = None, **kw: Any) -> None:
    """auth_handoff → record a pending human takeover on the leased screen."""
    if tool_name != f"{MCP_PREFIX}auth_handoff" or "auth_required" not in str(result):
        return
    parsed = result
    if isinstance(result, str):
        try:
            parsed = json.loads(result)
        except Exception:
            parsed = {}
    url = parsed.get("vnc_url") if isinstance(parsed, dict) else None
    screen = _state.get("screen")
    if screen is None:
        return
    try:
        _http_request(f"/agent/screens/{screen}/takeover", "POST", {"pending": True, "url": url})
    except Exception as e:
        logger.warning("Failed to record takeover notice: %s", e)


def on_session_finalize(**kw: Any) -> None:
    if _state.get("screen") is not None:
        reach_release_screen()
    reset_state()


# --------------------------------------------------------------------------
# Registration
# --------------------------------------------------------------------------

_SCREEN = {"type": "integer", "description": "Screen ID. Defaults to the screen leased for this session."}
_OWNER = {"type": "string", "description": "Lease owner. Defaults to the active Hermes profile."}

PLUGIN_TOOLS: Dict[str, tuple] = {
    "reach_lease_screen": (reach_lease_screen, "Lease an Agent Computer screen for this Hermes profile.",
                           {"type": "object", "properties": {"screen": _SCREEN, "owner": _OWNER}}),
    "reach_release_screen": (reach_release_screen, "Release the leased Agent Computer screen.",
                             {"type": "object", "properties": {"screen": _SCREEN, "owner": _OWNER}}),
    "reach_status": (reach_status, "Inspect Agent Computer screens, lease ownership and live view URLs.",
                     {"type": "object", "properties": {"screen": _SCREEN}}),
    "reach_drive": (reach_drive,
                    "Run the CUA vision loop (Gemini 3.8 Flash via agy) to achieve a browser or desktop goal on the Agent Computer.",
                    {"type": "object", "required": ["goal"], "properties": {
                        "goal": {"type": "string", "description": "Objective to accomplish on the desktop/browser."},
                        "screen": _SCREEN,
                        "max_steps": {"type": "integer", "default": 15, "description": "Maximum vision-action steps."},
                        "initial_url": {"type": "string", "description": "Optional URL to open before the loop starts."},
                    }}),
}


def _handler(fn: Callable[..., Dict[str, Any]]) -> Callable[..., str]:
    def run(args: Dict[str, Any], **kw: Any) -> str:
        return json.dumps(fn(**(args or {})))
    return run


def register(ctx: Any) -> None:
    global _ctx
    _ctx = ctx
    ctx.register_hook("on_session_start", on_session_start)
    ctx.register_hook("pre_llm_call", pre_llm_call)
    ctx.register_hook("pre_tool_call", pre_tool_call)
    ctx.register_hook("post_tool_call", post_tool_call)
    ctx.register_hook("on_session_finalize", on_session_finalize)
    for name, (fn, description, params) in PLUGIN_TOOLS.items():
        ctx.register_tool(name=name, toolset="reach",
                          schema={"name": name, "description": description, "parameters": params},
                          handler=_handler(fn), description=description, emoji="🖥️")
