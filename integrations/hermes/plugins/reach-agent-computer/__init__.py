"""Hermes plugin: reach-agent-computer.

Leases one reach screen per hermes profile, routes reach tools to it,
provides CUA driver capability (Gemini 3.8 Flash via agy), and surfaces takeovers.
"""

from __future__ import annotations

import json
import logging
import os
import sys
import urllib.error
import urllib.request
from typing import Any, Dict, Optional

logger = logging.getLogger("hermes.plugins.reach_agent_computer")

API_DEFAULT = "http://127.0.0.1:4200"
OWNER_DEFAULT = "default"

_state: Dict[str, Any] = {
    "screen": None,
    "leased_at": None,
    "novnc_url": None,
    "owner": None,
}


def get_api_url() -> str:
    return os.environ.get("REACH_AGENT_URL", API_DEFAULT).rstrip("/")


def get_owner() -> str:
    return os.environ.get("HERMES_PROFILE", OWNER_DEFAULT)


def get_state() -> Dict[str, Any]:
    """Return a copy of the current plugin state."""
    return dict(_state)


def reset_state() -> None:
    """Reset plugin state (useful for tests and session cleanup)."""
    _state["screen"] = None
    _state["leased_at"] = None
    _state["novnc_url"] = None
    _state["owner"] = None


def _http_request(
    path: str,
    method: str = "GET",
    body: Optional[Dict[str, Any]] = None,
    api_url: Optional[str] = None,
    timeout: float = 10.0,
) -> Any:
    base = (api_url or get_api_url()).rstrip("/")
    url = f"{base}{path}"
    data = json.dumps(body).encode("utf-8") if body is not None else None
    headers = {"content-type": "application/json"} if data is not None else {}
    req = urllib.request.Request(url, data=data, headers=headers, method=method)
    with urllib.request.urlopen(req, timeout=timeout) as resp:
        raw = resp.read().decode("utf-8")
        return json.loads(raw) if raw else {}


# --------------------------------------------------------------------------
# Tool Implementations
# --------------------------------------------------------------------------


def reach_lease_screen(
    screen: Optional[int] = None,
    owner: Optional[str] = None,
    api_url: Optional[str] = None,
) -> Dict[str, Any]:
    """Lease an Agent Computer screen for this profile or specified owner."""
    lease_owner = owner or get_owner()
    api = api_url or get_api_url()

    try:
        screens = _http_request("/agent/screens", method="GET", api_url=api)
    except Exception as e:
        return {"status": "error", "message": f"Failed to list screens: {e}"}

    target_screen = screen
    if target_screen is None:
        # Find first free screen, or one already leased by this owner
        free = next(
            (s for s in screens if s.get("owner") in (None, lease_owner)),
            None,
        )
        if free is None:
            return {
                "status": "exhausted",
                "message": "No free Agent Computer screens available.",
            }
        target_screen = free.get("id", 0)

    try:
        _http_request(
            f"/agent/screens/{target_screen}/lease",
            method="POST",
            body={"owner": lease_owner},
            api_url=api,
        )
    except urllib.error.HTTPError as e:
        err_body = e.read().decode("utf-8", errors="replace")
        return {
            "status": "conflict",
            "message": f"Failed to lease screen {target_screen} (HTTP {e.code}): {err_body}",
        }
    except Exception as e:
        return {"status": "error", "message": f"Lease request failed: {e}"}

    novnc_url = ""
    for s in screens:
        if s.get("id") == target_screen:
            novnc_url = s.get("novnc_url", "")
            break

    _state["screen"] = target_screen
    _state["owner"] = lease_owner
    _state["novnc_url"] = novnc_url
    return {
        "status": "ok",
        "screen": target_screen,
        "owner": lease_owner,
        "novnc_url": novnc_url,
    }


def reach_release_screen(
    screen: Optional[int] = None,
    owner: Optional[str] = None,
    api_url: Optional[str] = None,
) -> Dict[str, Any]:
    """Release a leased screen."""
    lease_owner = owner or _state.get("owner") or get_owner()
    target_screen = screen if screen is not None else _state.get("screen")
    api = api_url or get_api_url()

    if target_screen is None:
        return {"status": "noop", "message": "No screen currently leased."}

    try:
        _http_request(
            f"/agent/screens/{target_screen}/lease",
            method="DELETE",
            body={"owner": lease_owner},
            api_url=api,
        )
        if _state.get("screen") == target_screen:
            reset_state()
        return {"status": "ok", "screen": target_screen, "released": True}
    except Exception as e:
        return {
            "status": "error",
            "message": f"Failed to release screen {target_screen}: {e}",
        }


def reach_status(
    screen: Optional[int] = None,
    api_url: Optional[str] = None,
) -> Dict[str, Any]:
    """Inspect screen lease status and live view URLs."""
    api = api_url or get_api_url()
    try:
        screens = _http_request("/agent/screens", method="GET", api_url=api)
        if screen is not None:
            found = next((s for s in screens if s.get("id") == screen), None)
            if found:
                return {"status": "ok", "screen": found}
            return {"status": "not_found", "message": f"Screen {screen} not found."}
        return {
            "status": "ok",
            "current_session": get_state(),
            "screens": screens,
        }
    except Exception as e:
        return {"status": "error", "message": f"Failed to query reach status: {e}"}


def reach_drive(
    goal: str,
    screen: Optional[int] = None,
    max_steps: int = 15,
    api_url: Optional[str] = None,
    initial_url: Optional[str] = None,
) -> Dict[str, Any]:
    """Drive the Reach sandbox using the CUA vision loop (Gemini 3.8 Flash via agy)."""
    target_screen = screen if screen is not None else (_state.get("screen") or 0)
    api = api_url or get_api_url()

    # Import reach_drive module
    try:
        repo_root = os.path.abspath(
            os.path.join(os.path.dirname(__file__), "../../../..")
        )
        if repo_root not in sys.path:
            sys.path.insert(0, repo_root)
        from scripts.reach_drive import ReachDriver
    except ImportError:
        try:
            from reach_drive import ReachDriver
        except ImportError as e:
            return {
                "status": "error",
                "message": f"Could not import reach_drive driver module: {e}",
            }

    driver = ReachDriver(
        api_url=api,
        screen=target_screen,
        max_steps=max_steps,
    )

    try:
        result = driver.drive(goal=goal, initial_url=initial_url)
        return result.to_dict()
    except Exception as e:
        logger.exception("reach_drive execution failed")
        return {"status": "failed", "error": str(e)}


# --------------------------------------------------------------------------
# Hermes Lifecycle Hooks
# --------------------------------------------------------------------------


def on_session_start(
    ctx: Any,
    session_id: Optional[str] = None,
    model: Optional[str] = None,
    platform: Optional[str] = None,
    **kw: Any,
) -> None:
    """Acquire a screen lease and inject live view info for the session."""
    api = get_api_url()
    owner = get_owner()
    try:
        screens = _http_request("/agent/screens", method="GET", api_url=api)
        free = next((s for s in screens if s.get("owner") in (None, owner)), None)
        if free is None:
            if hasattr(ctx, "inject_message"):
                ctx.inject_message(
                    "No free Agent Computer screen; desktop tools are unavailable this session.",
                    role="user",
                )
            return

        _http_request(
            f"/agent/screens/{free['id']}/lease",
            method="POST",
            body={"owner": owner},
            api_url=api,
        )
        _state["screen"] = free["id"]
        _state["owner"] = owner
        _state["novnc_url"] = free.get("novnc_url", "")
        _state["leased_at"] = free.get("leased_at")

        if hasattr(ctx, "inject_message"):
            ctx.inject_message(
                f"Agent Computer screen {free['id']} leased. Live view: {free.get('novnc_url')}",
                role="user",
            )
    except Exception as e:
        logger.warning("Agent Computer screen auto-lease failed: %s", e)
        if hasattr(ctx, "inject_message"):
            ctx.inject_message(f"Agent Computer unavailable: {e}", role="user")


def pre_tool_call(
    tool_name: str,
    args: Dict[str, Any],
    task_id: Optional[str] = None,
    **kw: Any,
) -> Optional[Dict[str, Any]]:
    """Inject leased screen into reach tools if not specified."""
    if (
        tool_name.startswith("reach_")
        and _state.get("screen") is not None
        and "screen" not in args
    ):
        return {"modify": {"screen": _state["screen"]}}
    return None


def post_tool_call(
    tool_name: str,
    args: Dict[str, Any],
    result: Any,
    **kw: Any,
) -> None:
    """Detect auth_handoff triggering human takeover."""
    if tool_name == "reach_auth_handoff" and "auth_required" in str(result):
        url = None
        if isinstance(result, dict):
            url = result.get("vnc_url")
        elif isinstance(result, str):
            try:
                parsed = json.loads(result)
                if isinstance(parsed, dict):
                    url = parsed.get("vnc_url")
            except Exception:
                pass

        screen = _state.get("screen")
        if screen is not None:
            try:
                _http_request(
                    f"/agent/screens/{screen}/takeover",
                    method="POST",
                    body={"pending": True, "url": url},
                )
            except Exception as e:
                logger.warning("Failed to record takeover notice: %s", e)


def on_session_finalize(
    session_id: Optional[str] = None,
    **kw: Any,
) -> None:
    """Release leased screen when session finishes."""
    screen = _state.get("screen")
    owner = _state.get("owner") or get_owner()
    if screen is not None:
        try:
            _http_request(
                f"/agent/screens/{screen}/lease",
                method="DELETE",
                body={"owner": owner},
            )
        except Exception as e:
            logger.debug("Failed to release screen on session finalize: %s", e)
        finally:
            reset_state()


# --------------------------------------------------------------------------
# Plugin Registration
# --------------------------------------------------------------------------


PLUGIN_TOOLS = {
    "reach_lease_screen": reach_lease_screen,
    "reach_release_screen": reach_release_screen,
    "reach_drive": reach_drive,
    "reach_status": reach_status,
}


def register(ctx: Any) -> None:
    """Register hooks and tools with the Hermes plugin context."""
    # Register lifecycle hooks
    if hasattr(ctx, "register_hook"):
        ctx.register_hook(
            "on_session_start",
            lambda **kw: on_session_start(ctx, **kw),
        )
        ctx.register_hook("pre_tool_call", pre_tool_call)
        ctx.register_hook("post_tool_call", post_tool_call)
        ctx.register_hook("on_session_finalize", on_session_finalize)

    # Register tools
    if hasattr(ctx, "register_tool"):
        ctx.register_tool(
            "reach_lease_screen",
            reach_lease_screen,
            description="Lease an Agent Computer screen for this hermes profile or specified owner.",
        )
        ctx.register_tool(
            "reach_release_screen",
            reach_release_screen,
            description="Release the leased Agent Computer screen.",
        )
        ctx.register_tool(
            "reach_drive",
            reach_drive,
            description="Run the CUA vision loop (Gemini 3.8 Flash via agy) to achieve a browser or desktop goal on Reach.",
        )
        ctx.register_tool(
            "reach_status",
            reach_status,
            description="Inspect the status of Reach Agent Computer screens, lease ownership, and live view URLs.",
        )
    elif hasattr(ctx, "tools") and isinstance(ctx.tools, dict):
        ctx.tools.update(PLUGIN_TOOLS)
