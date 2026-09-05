"""Browser Use Adapter for Agent Computer.

Connects the `browser-use` library to Agent Computer's multi-screen virtual display
and remote Chrome CDP endpoints, with out-of-band credential vault integration
and screen lease lifecycle management.
"""

from __future__ import annotations

import json
import logging
from pathlib import Path
from typing import Any, Dict, Optional
import urllib.error
import urllib.parse
import urllib.request

logger = logging.getLogger("agent_computer.browser_use")


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
        self.auth_token = auth_token
        self._leased = False
        self._lease_token: Optional[str] = None

    @property
    def lease_token(self) -> Optional[str]:
        """Active screen lease token returned by the supervisor."""
        return self._lease_token

    def _get_headers(self, include_lease_token: bool = True) -> Dict[str, str]:
        """Build request headers including auth and active lease token if available."""
        headers = {"Content-Type": "application/json"}
        if include_lease_token and self._lease_token:
            headers["X-Lease-Token"] = self._lease_token
        if self.auth_token:
            headers["Authorization"] = f"Bearer {self.auth_token}"
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
        """Lease the target screen from Agent Computer supervisor to prevent collision."""
        url = f"{self.api_url}/agent/screens/{self.screen_id}/lease"
        payload = json.dumps({"owner": owner}).encode()
        headers = self._get_headers(include_lease_token=False)
        req = urllib.request.Request(
            url,
            data=payload,
            headers=headers,
            method="POST",
        )
        try:
            with urllib.request.urlopen(req, timeout=5) as resp:
                self._leased = True
                self._owner = owner
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
                if raw:
                    try:
                        body = json.loads(raw)
                        if isinstance(body, dict):
                            self._lease_token = body.get("token")
                    except Exception:
                        pass
                logger.info(f"Leased screen {self.screen_id} for {duration_sec}s (owner: {owner})")
                res = {"status": "leased", "screen": self.screen_id, "code": resp.status}
                if self._lease_token:
                    res["token"] = self._lease_token
                return res
        except urllib.error.URLError as e:
            logger.warning(
                f"Could not contact supervisor at {url} ({e}); continuing with standalone CDP connection."
            )
            self._leased = False
            self._lease_token = None
            return {"status": "unsupervised", "screen": self.screen_id, "error": str(e)}

    def release_screen(self) -> bool:
        """Release leased screen back to the pool."""
        if not self._leased:
            return True
        url = f"{self.api_url}/agent/screens/{self.screen_id}/lease"
        owner = getattr(self, "_owner", "browser-use")
        payload = json.dumps({"owner": owner}).encode()
        headers = self._get_headers(include_lease_token=True)
        req = urllib.request.Request(
            url,
            data=payload,
            headers=headers,
            method="DELETE",
        )
        try:
            with urllib.request.urlopen(req, timeout=5) as resp:
                self._leased = False
                self._lease_token = None
                logger.info(f"Released screen {self.screen_id} (owner: {owner})")
                return resp.status in (200, 204)
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
