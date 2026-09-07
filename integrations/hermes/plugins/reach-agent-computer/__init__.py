"""Hermes plugin: reach-agent-computer.

One reach screen per Hermes *session*: leased on session start under a
capability (lease token + handoff generation) held in per-session private
state, routed into every reach tool call, released on session finalize.
Adds ``reach_tool`` (authenticated passthrough to the reach dispatcher),
``reach_drive`` (CUA loop, Gemini 3.8 Flash via agy) plus lease/status tools.

Security model:
  - Lease state is keyed by the trusted ``session_id`` Hermes passes to hooks
    and tool handlers (never a model argument) via a ContextVar with scoped
    set/reset; direct standalone Python calls use a distinct default key.
  - The supervisor bearer (``REACH_AUTH_TOKEN``) is used only for lease
    allocation and pre-lease metadata. Every other call carries the session's
    own ``X-Lease-Token`` (+ ``X-Handoff-Gen``); tokens never appear in model
    arguments, tool outputs, logs, or get_state()/status results.
  - Authenticated requests never follow redirects (fail closed), and
    credentials are only ever sent to the exact configured API base.
  - The native Hermes MCP surface (``mcp__reach__*``) cannot carry lease
    headers, so it is blocked in favor of ``reach_tool``.

Config (``plugins.entries.reach-agent-computer.settings`` in config.yaml):
  api_url:   reach host daemon base URL (default http://127.0.0.1:4200)
  repo_root: agent-computer checkout holding scripts/reach_drive.py
             (default: derived from this file's real path)
"""

from __future__ import annotations

import contextlib
import json
import logging
import os
import sys
import threading
import weakref
from contextvars import ContextVar
from pathlib import Path
from typing import Any, Callable, Dict, Iterator, Optional, Tuple

logger = logging.getLogger("hermes.plugins.reach_agent_computer")

API_DEFAULT = "http://127.0.0.1:4200"
OWNER_DEFAULT = "default"
MCP_PREFIX = "mcp__reach__"
# Distinct default key for standalone direct Python calls (no Hermes session).
STANDALONE_SESSION = "__standalone__"

# Reach dispatcher toolset: crates/reach-cli/src/mcp.rs tool_definitions().
REACH_TOOLS = frozenset({
    "screenshot", "click", "type", "key", "browse", "scrape",
    "playwright_eval", "exec", "page_text", "auth_handoff", "live_view", "ack_handback",
})

# Model-supplied reach_tool argument keys that must never ride into a call.
_FORBIDDEN_TOOL_ARGS = frozenset(
    {"token", "authorization", "api_url", "base_url", "session_id", "owner"})

_ctx: Any = None  # PluginContext, set by register(); None under unit tests
_lock = threading.Lock()
_sessions: Dict[str, Dict[str, Any]] = {}
_session_locks: Any = weakref.WeakValueDictionary()

_session_var: ContextVar[Optional[str]] = ContextVar(
    "reach_plugin_session_id", default=None)

_EMPTY_STATE: Dict[str, Any] = {
    "screen": None, "owner": None, "token": None, "handoff_gen": None,
    "leased_at": None, "novnc_url": None, "error": None,
}
_REDACTED_KEYS = ("token", "handoff_gen")


# --------------------------------------------------------------------------
# Trusted session binding (ContextVar, scoped set/reset)
# --------------------------------------------------------------------------


def _trusted_session(session_id: Any) -> Optional[str]:
    """Accept only a nonempty string session id from trusted hook/handler kwargs."""
    if isinstance(session_id, str) and session_id.strip():
        return session_id
    return None


@contextlib.contextmanager
def _session_scope(session_id: Any) -> Iterator[str]:
    """Bind the trusted session key for this call; reset on exit."""
    session = _trusted_session(session_id)
    if _ctx is not None and session is None:
        raise ValueError("Reach requires a trusted session identity")
    key = session or STANDALONE_SESSION
    with _lock:
        session_lock = _session_locks.get(key)
        if session_lock is None:
            session_lock = threading.RLock()
            _session_locks[key] = session_lock
    with session_lock:
        token = _session_var.set(key)
        try:
            yield key
        finally:
            _session_var.reset(token)


def _current_key() -> str:
    return _session_var.get() or STANDALONE_SESSION


def _session_state(create: bool = False) -> Dict[str, Any]:
    """Private state for the bound session. create=False never allocates."""
    key = _current_key()
    with _lock:
        if create:
            return _sessions.setdefault(key, dict(_EMPTY_STATE))
        st = _sessions.get(key)
        return st if st is not None else dict(_EMPTY_STATE)


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
    """Lease owner label = active Hermes profile (diagnostic only, never a credential)."""
    if _ctx is not None:
        try:
            return _ctx.profile_name
        except Exception:
            pass
    return os.environ.get("HERMES_PROFILE", OWNER_DEFAULT)


def _supervisor_token() -> Optional[str]:
    tok = os.environ.get("REACH_AUTH_TOKEN")
    return tok if tok and tok.strip() else None


def _redacted(st: Dict[str, Any]) -> Dict[str, Any]:
    """Copy of session state safe for tool output: token/gen stay private."""
    pub = {k: v for k, v in st.items() if k not in _REDACTED_KEYS}
    pub["has_lease"] = bool(st.get("token"))
    return pub


def get_state(session_id: Optional[str] = None) -> Dict[str, Any]:
    """Redacted lease state; session_id overrides the binding for trusted callers."""
    if session_id is not None:
        with _lock:
            st = _sessions.get(session_id)
            return _redacted(st if st is not None else _EMPTY_STATE)
    return _redacted(_session_state())


def reset_state(session_id: Optional[str] = None) -> None:
    """Drop a session's private state (trusted/test helper)."""
    with _lock:
        _sessions.pop(session_id if session_id is not None else _current_key(), None)


# --------------------------------------------------------------------------
# Authenticated transport (no redirects, credentials to the configured API only)
# --------------------------------------------------------------------------



def _build_opener() -> Any:
    import urllib.request

    class NoRedirects(urllib.request.HTTPRedirectHandler):
        def redirect_request(self, req: Any, fp: Any, code: Any, msg: Any, headers: Any, newurl: Any) -> None:
            # Fail closed: token-bearing calls must never follow redirects.
            return None

    return urllib.request.build_opener(NoRedirects())


def _http_request(
    path: str,
    method: str = "GET",
    body: Optional[Dict[str, Any]] = None,
    api_url: Optional[str] = None,
    timeout: float = 10.0,
    lease_token: Optional[str] = None,
    handoff_gen: Optional[int] = None,
    supervisor: bool = False,
) -> Any:
    import urllib.parse
    import urllib.request

    configured = get_api_url()
    base = (api_url or configured).rstrip("/")
    if api_url and base != configured:
        # Exact configured API only: never forward credentials elsewhere.
        raise ValueError("refusing to send credentials to a non-configured endpoint")

    headers: Dict[str, str] = {}
    if body is not None:
        headers["content-type"] = "application/json"
    parsed = urllib.parse.urlparse(base)
    if parsed.port == 4200:
        headers["Host"] = f"127.0.0.1:{parsed.port}"
    if lease_token:
        headers["X-Lease-Token"] = lease_token
        if handoff_gen is not None:
            headers["X-Handoff-Gen"] = str(handoff_gen)
    elif supervisor:
        tok = _supervisor_token()
        if tok:
            headers["Authorization"] = f"Bearer {tok}"

    data = json.dumps(body).encode("utf-8") if body is not None else None
    req = urllib.request.Request(f"{base}{path}", data=data, headers=headers, method=method)
    with _build_opener().open(req, timeout=timeout) as resp:
        raw = resp.read().decode("utf-8")
        return json.loads(raw) if raw else {}
def _http_error_detail(error: Any) -> str:
    """Read an HTTP error body without allowing a broken peer to mask the error."""
    try:
        raw = error.read()
    except Exception as read_error:
        return f"<response body unavailable: {read_error}>"
    try:
        return raw.decode("utf-8", "replace")
    except Exception as decode_error:
        return f"<response body unavailable: {decode_error}>"



def reach_lease_screen(
    screen: Optional[int] = None,
    owner: Optional[str] = None,
    api_url: Optional[str] = None,
) -> Dict[str, Any]:
    import urllib.error

    st = _session_state(create=True)
    if st.get("token") and st.get("screen") is not None:
        # Reuse own retained capability; occupied screens are never re-POSTed.
        if screen is not None and screen != st["screen"]:
            return {"status": "error",
                    "message": f"session already leases screen {st['screen']}"}
        return {"status": "ok", "screen": st["screen"], "owner": st["owner"],
                "novnc_url": st.get("novnc_url"), "reused": True}

    lease_owner = owner or get_owner()
    api = api_url or get_api_url()
    try:
        screens = _http_request("/agent/screens", api_url=api, supervisor=True)
    except Exception as e:
        return {"status": "error", "message": f"Failed to list screens: {e}"}

    target = screen
    if target is None:
        # Free-screen selection: unowned screens only; owner labels never recover a lease.
        free = next((s for s in screens if s.get("owner") is None), None)
        if free is None:
            return {"status": "exhausted", "message": "No free Agent Computer screens available."}
        target = free.get("id", 0)

    try:
        lease = _http_request(f"/agent/screens/{target}/lease", "POST",
                              {"owner": lease_owner}, api_url=api, supervisor=True)
    except urllib.error.HTTPError as e:
        detail = _http_error_detail(e)
        return {"status": "conflict",
                "message": f"Failed to lease screen {target} (HTTP {e.code}): {detail}"}
    except Exception as e:
        return {"status": "error", "message": f"Lease request failed: {e}"}

    info = next((s for s in screens if s.get("id") == target), {})
    if not isinstance(lease.get("token"), str) or not lease["token"] or type(lease.get("handoff_gen")) is not int:
        return {"status": "error", "message": "server did not return a valid lease capability"}
    st.update(screen=target, owner=lease_owner, token=lease.get("token"),
              handoff_gen=lease.get("handoff_gen"), leased_at=info.get("leased_at"),
              novnc_url=info.get("novnc_url", ""), error=None)
    return {"status": "ok", "screen": target, "owner": lease_owner, "novnc_url": st["novnc_url"]}


def reach_release_screen(
    screen: Optional[int] = None,
    owner: Optional[str] = None,
    api_url: Optional[str] = None,
) -> Dict[str, Any]:
    import urllib.error

    del owner  # label only: the retained capability authorizes the release
    st = _session_state()
    if st.get("screen") is not None and screen is not None and screen != st["screen"]:
        return {"status": "error",
                "message": f"screen {screen} is not leased to this session"}
    target = screen if screen is not None else st.get("screen")
    if target is None or not st.get("token"):
        return {"status": "noop", "message": "No screen currently leased."}
    try:
        _http_request(f"/agent/screens/{target}/lease", "DELETE",
                      {"owner": st.get("owner") or get_owner()},
                      api_url=api_url, lease_token=st["token"])
    except urllib.error.HTTPError as e:
        # e.g. HumanActive/Busy (409): the capability is still ours — retain all
        # local lease state so a later release (after handback) still works.
        detail = _http_error_detail(e)
        logger.warning("Release of screen %s refused (HTTP %s)", target, e.code)
        return {"status": "error", "state_retained": True,
                "message": f"Failed to release screen {target} (HTTP {e.code}): {detail}"}
    except Exception as e:
        logger.warning("Release of screen %s failed", target)
        return {"status": "error", "state_retained": True,
                "message": f"Failed to release screen {target}: {e}"}
    reset_state()
    return {"status": "ok", "screen": target, "released": True}


def _public_screen(s: Any) -> Any:
    """Server screen entry safe for model output: generations and any
    credential-shaped fields are stripped; status reads are never a source
    for refreshing the session's generation."""
    if isinstance(s, dict):
        return {k: v for k, v in s.items() if k not in ("handoff_gen", "lease_token", "token")}
    return s


def reach_status(screen: Optional[int] = None, api_url: Optional[str] = None) -> Dict[str, Any]:
    st = _session_state()
    try:
        # Own lease token when we hold one, supervisor bearer for pre-lease metadata.
        screens = _http_request("/agent/screens", api_url=api_url,
                                lease_token=st.get("token"), supervisor=True)
    except Exception as e:
        return {"status": "error", "message": f"Failed to query reach status: {e}"}
    if screen is not None:
        found = next((s for s in screens if s.get("id") == screen), None)
        if found is None:
            return {"status": "not_found", "message": f"Screen {screen} not found."}
        return {"status": "ok", "screen": _public_screen(found)}
    return {"status": "ok", "current_session": get_state(),
            "screens": [_public_screen(s) for s in screens]}


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
    completion_text: Optional[str] = None,
) -> Dict[str, Any]:
    if completion_text is not None and not completion_text.strip():
        return {"status": "error", "message": "completion_text must be nonempty"}
    st = _session_state()
    if st.get("screen") is not None and screen is not None and screen != st["screen"]:
        return {"status": "error",
                "message": f"screen {screen} is not leased to this session"}
    target = screen if screen is not None else (st.get("screen") or 0)
    root = str(_repo_root())
    if root not in sys.path:
        sys.path.insert(0, root)
    try:
        from scripts.reach_drive import ReachDriver
    except ImportError as e:
        return {"status": "error", "message": f"Could not import scripts.reach_drive from {root}: {e}"}

    driver = ReachDriver(api_url=api_url or get_api_url(), screen=target, max_steps=max_steps,
                         lease_token=st.get("token"), handoff_gen=st.get("handoff_gen"),
                         completion_text=completion_text)
    try:
        return driver.drive(goal=goal, initial_url=initial_url).to_dict()
    except Exception as e:
        logger.exception("reach_drive execution failed")
        return {"status": "failed", "error": str(e)}


ANTIBOT_SIGNATURES = [
    "just a moment...",
    "attention required! | cloudflare",
    "cloudflare turnstile",
    "checking your browser before accessing",
    "verify you are human",
    "access denied",
    "403 forbidden",
    "security check",
    "failed to execute attachshadow",
    "minified react error",
]


def _try_obscura(url: str, timeout: int = 10) -> Tuple[bool, str, float]:
    """Attempts Tier 1 fast fetch with Obscura. Returns (passed_antibot, content, elapsed_ms)."""
    import shutil
    import subprocess
    import time

    obscura_bin = shutil.which("obscura") or os.path.expanduser("~/.local/bin/obscura")
    if not (os.path.isfile(obscura_bin) and os.access(obscura_bin, os.X_OK)):
        return False, "Obscura binary not installed or executable", 0.0

    start = time.perf_counter()
    try:
        proc = subprocess.run(
            [obscura_bin, "fetch", url, "--dump", "markdown", "--quiet"],
            capture_output=True,
            text=True,
            timeout=timeout,
        )
        elapsed_ms = (time.perf_counter() - start) * 1000.0
        output = proc.stdout or ""
        stderr = proc.stderr or ""

        if proc.returncode != 0:
            return False, f"Obscura exit {proc.returncode}: {stderr.strip()}", elapsed_ms

        combined = (output + "\n" + stderr).lower()
        for sig in ANTIBOT_SIGNATURES:
            if sig in combined:
                return False, f"Anti-bot signature detected: '{sig}'", elapsed_ms

        if not output.strip():
            return False, "Obscura returned empty content", elapsed_ms

        return True, output, elapsed_ms
    except Exception as e:
        elapsed_ms = (time.perf_counter() - start) * 1000.0
        return False, f"Obscura exception: {e}", elapsed_ms


def _ack_handback(st: Dict[str, Any]) -> Dict[str, Any]:
    """Explicitly acknowledge handback; never replay a previously rejected action."""
    try:
        response = _http_request(f"/agent/screens/{st['screen']}/ack", "POST", {},
                                 lease_token=st["token"])
        if response.get("phase") != "AgentActive" or type(response.get("handoff_gen")) is not int:
            raise ValueError("invalid handback acknowledgment")
        st["handoff_gen"] = response["handoff_gen"]
        return {"status": "ok", "phase": "AgentActive",
                "message": "Capture a fresh observation and replan before acting."}
    except Exception as error:
        return {"status": "error", "message": f"Handback acknowledgment failed: {error}"}


def _reach_mcp_call(
    method_name: str,
    arguments: Dict[str, Any],
    api_url: Optional[str] = None,
    timeout: float = 35.0,
) -> Any:
    st = _session_state()
    if not st.get("token") or st.get("screen") is None:
        raise ValueError("headed browser calls require this session's lease")
    arguments = dict(arguments)
    if arguments.get("screen", st["screen"]) != st["screen"]:
        raise ValueError("screen is outside this session's lease")
    arguments["screen"] = st["screen"]
    payload = {
        "jsonrpc": "2.0",
        "id": 1,
        "method": "tools/call",
        "params": {"name": method_name, "arguments": arguments},
    }
    return _http_request("/mcp", method="POST", body=payload, api_url=api_url, timeout=timeout,
                         lease_token=st["token"], handoff_gen=st.get("handoff_gen"))


def reach_tool(name: str, arguments: Optional[Dict[str, Any]] = None,
               timeout: int = 60) -> Dict[str, Any]:
    """Authenticated passthrough to the reach dispatcher on this session's lease.

    Injects the bound screen and sends the lease token + handoff generation in
    headers; the model can never pass credentials or header overrides.
    """
    import urllib.error

    if name not in REACH_TOOLS:
        return {"status": "error", "message": f"unknown reach tool '{name}'",
                "allowed": sorted(REACH_TOOLS)}
    if arguments is not None and not isinstance(arguments, dict):
        return {"status": "error", "message": "arguments must be an object"}
    args = dict(arguments or {})
    bad = sorted(set(args) & _FORBIDDEN_TOOL_ARGS)
    if bad:
        return {"status": "error",
                "message": f"rejected credential-bearing argument(s): {', '.join(bad)}"}

    st = _session_state()
    if not st.get("token") or st.get("screen") is None:
        return {"status": "error",
                "message": "no screen leased for this session; call reach_lease_screen first"}
    caller_screen = args.get("screen")
    if caller_screen is not None and caller_screen != st["screen"]:
        return {"status": "error",
                "message": f"screen {caller_screen} is not leased to this session"}
    if name == "ack_handback":
        if args:
            return {"status": "error", "message": "ack_handback accepts no arguments"}
        return _ack_handback(st)
    args["screen"] = st["screen"]
    timeout = max(1, min(int(timeout), 600))

    def _call() -> Any:
        return _http_request(f"/tools/{name}", "POST", args, timeout=float(timeout),
                             lease_token=st.get("token"), handoff_gen=st.get("handoff_gen"))

    try:
        result = _call()
    except urllib.error.HTTPError as e:
        detail = _http_error_detail(e)
        return {"status": "error", "tool": name,
                "message": f"reach tool {name} failed (HTTP {e.code}): {detail}",
                "help": "After human handback, explicitly call ack_handback, then observe and replan; do not replay this action."}
    except Exception as e:
        return {"status": "error", "tool": name, "message": f"reach tool {name} failed: {e}"}
    return {"status": "error" if isinstance(result, dict) and result.get("isError") else "ok",
            "tool": name, "result": result}


def reach_smart_browse(
    url: str,
    screen: Optional[int] = None,
    api_url: Optional[str] = None,
    query: Optional[str] = None,
    force_headed: bool = False,
    timeout: int = 30,
) -> Dict[str, Any]:
    """Adaptive two-tier browser:
    Tier 1: Obscura headless Rust engine (~50-350ms) for fast markdown extraction.
    Detection: Anti-bot checks (Cloudflare, Turnstile, 403, React hydration).
    Tier 2: Escalation to Reach MicroVM headed Chrome when anti-bot or JS hydration fails.
    """
    import time

    st = _session_state()
    if st.get("screen") is not None and screen is not None and screen != st["screen"]:
        return {"status": "error",
                "message": f"screen {screen} is not leased to this session"}
    target_screen = screen if screen is not None else (st.get("screen") or 0)
    t0 = time.perf_counter()

    if not force_headed:
        passed, obscura_res, ms_obscura = _try_obscura(url)
        if passed:
            total_ms = (time.perf_counter() - t0) * 1000.0
            return {
                "status": "ok",
                "tier": "Tier 1 (Obscura Fast Path)",
                "url": url,
                "latency_ms": round(total_ms, 1),
                "format": "markdown",
                "content": obscura_res,
            }
        escalation_reason = obscura_res
    else:
        escalation_reason = "force_headed requested"
    # Tier 2 runs on a leased headed screen. Acquire one lazily for direct
    # smart-browse calls and retain the capability in the bound session.
    if not st.get("token") or st.get("screen") is None:
        lease = reach_lease_screen(screen=screen, api_url=api_url)
        if lease.get("status") != "ok":
            return {
                "status": "error",
                "tier": "Tier 2 (Reach MicroVM)",
                "url": url,
                "escalation_reason": escalation_reason,
                "message": f"Unable to acquire a headed browser lease: {lease.get('message', lease)}",
            }
        st = _session_state()
    target_screen = st["screen"]

    try:
        args: Dict[str, Any] = {"url": url, "snapshot": True, "screen": target_screen}
        if query:
            args["query"] = query
        mcp_res = _reach_mcp_call("browse", args, api_url=api_url, timeout=float(timeout))
        total_ms = (time.perf_counter() - t0) * 1000.0

        content_list = mcp_res.get("result", {}).get("content", [])
        raw_text = content_list[0].get("text", "") if content_list else ""
        try:
            reach_data = json.loads(raw_text)
        except Exception:
            reach_data = {"raw": raw_text}

        return {
            "status": "ok",
            "tier": "Tier 2 (Reach MicroVM Headed Chrome)",
            "url": url,
            "escalation_reason": escalation_reason,
            "latency_ms": round(total_ms, 1),
            "screen": target_screen,
            "data": reach_data,
        }
    except Exception as e:
        total_ms = (time.perf_counter() - t0) * 1000.0
        return {
            "status": "error",
            "tier": "Tier 2 (Reach MicroVM)",
            "url": url,
            "escalation_reason": escalation_reason,
            "latency_ms": round(total_ms, 1),
            "message": f"Reach browse failed: {e}",
        }


# --------------------------------------------------------------------------
# Lifecycle hooks
# --------------------------------------------------------------------------


def on_session_start(**kw: Any) -> None:
    """Lease a screen for this session. Announced to the model via pre_llm_call (first turn only)."""
    with _session_scope(kw.get("session_id")):
        res = reach_lease_screen()
        if res.get("status") != "ok":
            _session_state(create=True)["error"] = res.get("message")
            logger.warning("Agent Computer screen auto-lease failed: %s", res.get("message"))


def pre_llm_call(is_first_turn: bool = False, **kw: Any) -> Optional[Dict[str, str]]:
    """First-turn context only: ephemeral user-message injection, never the system prompt."""
    with _session_scope(kw.get("session_id")):
        if not is_first_turn:
            return None
        st = _session_state()
        if st.get("screen") is not None:
            return {"context": f"Agent Computer screen {st['screen']} is leased to this session. "
                               f"Live view: {st.get('novnc_url') or 'unknown'}"}
        if st.get("error"):
            return {"context": f"Agent Computer unavailable: {st['error']}"}
        return None


def pre_tool_call(tool_name: str = "", args: Optional[Dict[str, Any]] = None, **kw: Any) -> Optional[Dict[str, Any]]:
    """Route reach tool calls to this session's leased screen; block the native MCP bypass."""
    with _session_scope(kw.get("session_id")):
        args = args or {}
        if tool_name.startswith(MCP_PREFIX):
            target = tool_name[len(MCP_PREFIX):] or "…"
            return {"action": "block", "message":
                    f"'{tool_name}' cannot carry this session's lease credentials; "
                    f"call reach_tool(name='{target}', arguments={{...}}) instead"}
        if tool_name in PLUGIN_TOOLS and tool_name != "reach_tool":
            st = _session_state()
            if st.get("screen") is not None:
                caller = args.get("screen")
                if caller is not None and caller != st["screen"]:
                    return {"action": "block", "message":
                            f"screen {caller} is not leased to this session "
                            f"(bound: screen {st['screen']})"}
                if caller is None:
                    return {"action": "modify", "args": {"screen": st["screen"]}}
    return None


def _result_dict(result: Any) -> Dict[str, Any]:
    if isinstance(result, dict):
        return result
    if isinstance(result, str):
        try:
            parsed = json.loads(result)
            return parsed if isinstance(parsed, dict) else {}
        except Exception:
            return {}
    return {}


def post_tool_call(tool_name: str = "", args: Optional[Dict[str, Any]] = None, result: Any = None, **kw: Any) -> None:
    """Observe the server-owned auth handoff transition and retain its generation."""
    via_tool = tool_name == "reach_tool" and isinstance(args, dict) and args.get("name") == "auth_handoff"
    native_tool = tool_name == f"{MCP_PREFIX}auth_handoff"
    if not via_tool and not native_tool:
        return

    parsed = _result_dict(result)
    if via_tool:
        if parsed.get("status") != "ok" or parsed.get("tool") != "auth_handoff":
            return
        parsed = parsed.get("result") if isinstance(parsed.get("result"), dict) else {}
    if not isinstance(parsed, dict) or parsed.get("isError"):
        return
    content = parsed.get("content")
    if not isinstance(content, list):
        rpc_result = parsed.get("result")
        if not isinstance(rpc_result, dict) or rpc_result.get("isError"):
            return
        content = rpc_result.get("content")
    if not isinstance(content, list) or not content:
        return
    text = content[0].get("text") if isinstance(content[0], dict) else None
    handoff = _result_dict(text)
    if (handoff.get("status") != "auth_required"
            or handoff.get("phase") != "HandoffPending"
            or type(handoff.get("handoff_gen")) is not int):
        return

    with _session_scope(kw.get("session_id")):
        st = _session_state()
        if st.get("screen") is None or not st.get("token"):
            return
        current_gen = st.get("handoff_gen")
        if type(current_gen) is int and handoff["handoff_gen"] < current_gen:
            return
        st["handoff_gen"] = handoff["handoff_gen"]


def on_session_finalize(**kw: Any) -> None:
    """Release this session's lease with its retained capability.

    A failed release (e.g. HumanActive) keeps the local lease state so a later
    finalize — after the human hands back — can still release with the token.
    """
    with _session_scope(kw.get("session_id")):
        st = _session_state()
        if st.get("screen") is not None and st.get("token"):
            res = reach_release_screen()
            if res.get("status") != "ok":
                logger.warning("Session lease retained after failed release: %s",
                               res.get("message"))
                return
        reset_state()


# --------------------------------------------------------------------------
# Registration
# --------------------------------------------------------------------------

_SCREEN = {"type": "integer", "description": "Screen ID. Defaults to the screen leased for this session."}
_OWNER = {"type": "string", "description": "Owner label for the lease (diagnostic only; never affects lease binding)."}
_COMPLETION_TEXT = {"type": "string",
                    "description": "Optional text that must appear on the page for the goal to count as completed; "
                                   "otherwise the result is postcondition_failed."}

PLUGIN_TOOLS: Dict[str, tuple] = {
    "reach_lease_screen": (reach_lease_screen, "Lease an Agent Computer screen for this session.",
                           {"type": "object", "properties": {"screen": _SCREEN, "owner": _OWNER}}),
    "reach_release_screen": (reach_release_screen, "Release the Agent Computer screen leased for this session.",
                             {"type": "object", "properties": {"screen": _SCREEN, "owner": _OWNER}}),
    "reach_status": (reach_status, "Inspect Agent Computer screens, lease ownership and live view URLs.",
                     {"type": "object", "properties": {"screen": _SCREEN}}),
    "reach_tool": (reach_tool,
                   "Call a Reach Agent Computer tool (screenshot, click, type, key, browse, scrape, "
                   "playwright_eval, exec, page_text, auth_handoff, live_view) on this session's leased "
                   "screen. Use ack_handback with no arguments after human handback, then observe and replan. "
                   "Credentials are injected automatically; do not pass tokens or headers.",
                   {"type": "object", "required": ["name"], "properties": {
                       "name": {"type": "string", "enum": sorted(REACH_TOOLS),
                                "description": "Reach tool to invoke."},
                       "arguments": {"type": "object",
                                     "description": "Arguments for the Reach tool (screen is bound to this "
                                                    "session's lease; never pass tokens or endpoints)."},
                       "timeout": {"type": "integer", "minimum": 1, "maximum": 600, "default": 60,
                                   "description": "Seconds to wait for the Reach tool."},
                   }}),
    "reach_drive": (reach_drive,
                    "Run the CUA vision loop (Gemini 3.8 Flash via agy) to achieve a browser or desktop goal on the Agent Computer.",
                    {"type": "object", "required": ["goal"], "properties": {
                        "goal": {"type": "string", "description": "Objective to accomplish on the desktop/browser."},
                        "screen": _SCREEN,
                        "max_steps": {"type": "integer", "default": 15, "description": "Maximum vision-action steps."},
                        "initial_url": {"type": "string", "description": "Optional URL to open before the loop starts."},
                        "completion_text": _COMPLETION_TEXT,
                    }}),
    "reach_smart_browse": (
        reach_smart_browse,
        "Adaptive tiered browser: fast local markdown scrape (~50-350ms) with automatic fallback to Reach headed Chrome on anti-bot/challenges.",
        {
            "type": "object",
            "required": ["url"],
            "properties": {
                "url": {"type": "string", "description": "URL to fetch or browse."},
                "screen": _SCREEN,
                "query": {"type": "string", "description": "Optional search/query filter."},
                "force_headed": {"type": "boolean", "description": "Bypass fast path and force headed Chrome."},
            },
        },
    ),
}


def _handler(fn: Callable[..., Dict[str, Any]], params: Dict[str, Any]) -> Callable[..., str]:
    allowed = set((params.get("properties") or {}).keys())

    def run(args: Dict[str, Any], **kw: Any) -> str:
        args = dict(args or {})
        unknown = sorted(set(args) - allowed)
        if unknown:
            return json.dumps({"status": "error",
                               "message": f"rejected undeclared parameter(s): {', '.join(unknown)}"})
        if _ctx is not None:
            # Real PluginContext invocation: the trusted session binding is mandatory.
            session_id = kw.get("session_id")
            if not _trusted_session(session_id):
                return json.dumps({"status": "error",
                                   "message": "reach tools require a nonempty session binding"})
        with _session_scope(kw.get("session_id")):
            return json.dumps(fn(**args))

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
                          handler=_handler(fn, params), description=description, emoji="🖥️")
