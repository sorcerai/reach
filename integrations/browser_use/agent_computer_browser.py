"""Browser Use Adapter for Agent Computer.

Connects the `browser-use` library to Agent Computer's multi-screen virtual display
and remote Chrome CDP endpoints, with out-of-band credential vault integration
and screen lease lifecycle management.
"""

from __future__ import annotations

import json
import logging
import os
from pathlib import Path
from typing import Any, Dict, Optional
import urllib.error
import urllib.parse
import urllib.request

logger = logging.getLogger("agent_computer.browser_use")


class _NoRedirectHandler(urllib.request.HTTPRedirectHandler):
    """Refuse every redirect so credentials never leave the request origin."""

    def redirect_request(self, req, fp, code, msg, headers, newurl):
        raise urllib.error.HTTPError(
            req.full_url,
            code,
            f"{msg} (refusing redirect of credential-bearing request)",
            headers,
            fp,
        )


_OPENER = urllib.request.build_opener(_NoRedirectHandler())


class AgentComputerBrowserAdapter:
    """Adapter bridging browser-use to an Agent Computer screen and CDP session."""

    def __init__(
        self,
        screen_id: int = 0,
        api_url: str = "http://127.0.0.1:4200",
        host: str = "127.0.0.1",
        cdp_port: Optional[int] = None,
        vault_path: Optional[Path] = None,
        auth_token: Optional[str] = None,
    ) -> None:
        self.screen_id = screen_id
        self.api_url = api_url.rstrip("/")
        self.host = host
        # Default CDP port scheme: 9222 + screen_id (screen 0 = 9222, screen 1 = 9223, ...)
        self.cdp_port = cdp_port if cdp_port is not None else (9222 + screen_id)
        self.novnc_port = 6080 + screen_id
        self.vault_path = vault_path
        token = auth_token or os.environ.get("REACH_AUTH_TOKEN")
        self.auth_token = token if token and token.strip() else None
        self._owner: Optional[str] = None
        self._leased = False
        self._lease_token: Optional[str] = None
        self._handoff_gen: Optional[int] = None

    @property
    def lease_token(self) -> Optional[str]:
        """Active screen lease capability returned by the supervisor."""
        return self._lease_token

    @property
    def handoff_gen(self) -> Optional[int]:
        """Handoff generation retained from the lease response."""
        return self._handoff_gen

    def _allocation_headers(self) -> Dict[str, str]:
        """Headers for lease allocation: optional supervisor bearer only.

        The supervisor credential (constructor-held or REACH_AUTH_TOKEN)
        authorizes allocation exclusively; it never accompanies ordinary
        worker requests.
        """
        headers = {"Content-Type": "application/json"}
        if self.auth_token:
            headers["Authorization"] = f"Bearer {self.auth_token}"
        return headers

    def _worker_headers(self) -> Dict[str, str]:
        """Headers for ordinary requests: retained lease capability only."""
        headers = {"Content-Type": "application/json"}
        if self._lease_token:
            headers["X-Lease-Token"] = self._lease_token
        if self._handoff_gen is not None:
            headers["X-Handoff-Gen"] = str(self._handoff_gen)
        return headers

    @property
    def cdp_url(self) -> str:
        """Remote Chrome DevTools Protocol endpoint URL."""
        return f"http://{self.host}:{self.cdp_port}"

    @property
    def novnc_url(self) -> str:
        """Live noVNC browser viewport URL for human observation or takeover."""
        return f"http://{self.host}:{self.novnc_port}/vnc.html"

    def lease_screen(self, duration_sec: int = 600, owner: str = "browser-use") -> Dict[str, Any]:
        """Lease the target screen from the Agent Computer supervisor.

        Allocation is creation-only: an occupied screen (including one held
        under the same owner label) is refused by the server and is never
        recovered by re-allocating. The adapter fails closed on a refusal
        instead of downgrading to unsupervised CDP; only an unreachable
        supervisor keeps the standalone direct-CDP fallback.
        """
        url = f"{self.api_url}/agent/screens/{self.screen_id}/lease"
        payload = json.dumps({"owner": owner}).encode()
        req = urllib.request.Request(
            url,
            data=payload,
            headers=self._allocation_headers(),
            method="POST",
        )
        try:
            with _OPENER.open(req, timeout=5) as resp:
                raw = None
                if hasattr(resp, "read"):
                    try:
                        data = resp.read()
                        if isinstance(data, (bytes, bytearray)):
                            raw = data.decode("utf-8")
                        elif isinstance(data, str):
                            raw = data
                    except Exception:
                        pass
                body: Dict[str, Any] = {}
                if raw:
                    try:
                        parsed = json.loads(raw)
                        if isinstance(parsed, dict):
                            body = parsed
                    except Exception:
                        body = {}
                if not body.get("token"):
                    raise RuntimeError(
                        f"lease response for screen {self.screen_id} missing capability token"
                    )
                self._leased = True
                self._owner = owner
                self._lease_token = body["token"]
                if "handoff_gen" in body:
                    self._handoff_gen = int(body["handoff_gen"])
                logger.info(
                    f"Leased screen {self.screen_id} for {duration_sec}s (owner: {owner})"
                )
                return {
                    "status": "leased",
                    "screen": self.screen_id,
                    "code": resp.status,
                    "token": self._lease_token,
                    "handoff_gen": self._handoff_gen,
                }
        except urllib.error.HTTPError as e:
            logger.warning(
                "Lease refused for screen %s (HTTP %s): %s", self.screen_id, e.code, e
            )
            raise RuntimeError(
                f"lease refused for screen {self.screen_id}: HTTP {e.code}"
            ) from e
        except urllib.error.URLError as e:
            logger.warning(
                f"Could not contact supervisor at {url} ({e}); continuing with standalone CDP connection."
            )
            self._leased = False
            self._lease_token = None
            self._handoff_gen = None
            return {"status": "unsupervised", "screen": self.screen_id, "error": str(e)}

    def release_screen(self) -> bool:
        """Release leased screen using the retained lease capability.

        The capability rides in the X-Lease-Token header (with the retained
        generation); the supervisor bearer and the token itself never appear
        in the request body. On refusal (e.g. HTTP 409 while a human is
        active) local lease state is retained — nothing is cleared on a
        failed release.
        """
        if not self._leased:
            return True
        if not self._lease_token:
            logger.warning(
                "Cannot release screen %s without a lease capability", self.screen_id
            )
            return False
        url = f"{self.api_url}/agent/screens/{self.screen_id}/lease"
        owner = self._owner or "browser-use"
        payload = json.dumps({"owner": owner}).encode()
        req = urllib.request.Request(
            url,
            data=payload,
            headers=self._worker_headers(),
            method="DELETE",
        )
        try:
            with _OPENER.open(req, timeout=5) as resp:
                if resp.status not in (200, 204):
                    logger.warning(
                        "Release of screen %s refused (HTTP %s); retaining lease state",
                        self.screen_id,
                        resp.status,
                    )
                    return False
                self._leased = False
                self._lease_token = None
                self._handoff_gen = None
                logger.info(f"Released screen {self.screen_id} (owner: {owner})")
                return True
        except urllib.error.HTTPError as e:
            logger.warning(
                "Release of screen %s refused (HTTP %s); retaining lease state",
                self.screen_id,
                e.code,
            )
            return False
        except urllib.error.URLError as e:
            logger.warning(f"Failed to release screen {self.screen_id} on supervisor: {e}")
            return False

    def get_vault_credentials(self, domain: str) -> Dict[str, str]:
        """Retrieve credentials from out-of-band vault for secure form filling."""
        from scripts.reach_vault import ReachVault

        vault = ReachVault(vault_path=self.vault_path)
        cred = vault.get(domain)
        if not cred:
            return {}

        result = {
            "username": cred.username,
            "password": cred.password,
        }
        if cred.totp_secret:
            totp = vault.generate_totp(domain)
            if totp:
                result["totp"] = totp
        return result

    def get_browser_config(self) -> Dict[str, Any]:
        """Generate browser-use BrowserConfig dictionary targeting this screen's CDP."""
        return {
            "cdp_url": self.cdp_url,
            "disable_security": True,
        }

    def create_browser(self, **kwargs: Any) -> Any:
        """Instantiate and return a configured browser_use.Browser instance connected via CDP.

        Requires `browser-use` package to be installed.
        """
        try:
            from browser_use import Browser, BrowserConfig  # type: ignore
        except ImportError as e:
            raise ImportError(
                "browser-use is not installed in the current Python environment.\n"
                "Install it via:\n"
                "  pip install browser-use\n"
                "or\n"
                "  uv pip install browser-use"
            ) from e

        config_args: Dict[str, Any] = {
            "cdp_url": self.cdp_url,
        }
        config_args.update(kwargs)
        browser_config = BrowserConfig(**config_args)
        return Browser(config=browser_config)

    def __enter__(self) -> AgentComputerBrowserAdapter:
        self.lease_screen()
        return self

    def __exit__(self, exc_type: Any, exc_val: Any, exc_tb: Any) -> None:
        self.release_screen()
