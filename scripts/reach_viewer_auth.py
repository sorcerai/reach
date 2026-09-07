"""websockify authentication for the Reach viewer WebSocket.

The token file is deliberately read for every connection.  The supervisor
rotates it out-of-band, so keeping a token in this process would make a
revoked viewer capability remain usable until websockify restarts.
"""

from __future__ import annotations

import hmac
import os
import stat
from typing import Mapping

from websockify.auth_plugins import AuthenticationError, BasePlugin


_MAX_TOKEN_BYTES = 4096
_DEFAULT_TOKEN_PATH = "/run/reach/viewer-token"


def _authentication_error() -> AuthenticationError:
    # Keep both the HTTP response and websockify's log message token-free.
    return AuthenticationError(
        log_msg="viewer authentication failed",
        response_code=403,
        response_msg="Forbidden",
    )


def _read_token(path: str) -> bytes | None:
    """Read one valid, owner-only token file without exposing its contents."""
    flags = os.O_RDONLY | getattr(os, "O_CLOEXEC", 0) | getattr(os, "O_NOFOLLOW", 0)
    try:
        fd = os.open(path, flags)
    except OSError:
        return None

    try:
        metadata = os.fstat(fd)
        if not stat.S_ISREG(metadata.st_mode) or stat.S_IMODE(metadata.st_mode) != 0o600:
            return None
        if metadata.st_size <= 0 or metadata.st_size > _MAX_TOKEN_BYTES:
            return None
        token = os.read(fd, _MAX_TOKEN_BYTES + 1)
    except OSError:
        return None
    finally:
        os.close(fd)

    # The startup contract writes the bearer token without a newline.  Reject
    # whitespace and control bytes so malformed files cannot broaden matching.
    if not token or len(token) > _MAX_TOKEN_BYTES:
        return None
    if any(byte < 0x21 or byte > 0x7E for byte in token):
        return None
    return token


class ViewerTokenAuth(BasePlugin):
    """Require ``Authorization: Bearer <token>`` for every WebSocket."""

    def __init__(self, src: str | None = None):
        # src is a non-secret path supplied by websockify's --auth-source.
        self.source = src or _DEFAULT_TOKEN_PATH

    def authenticate(
        self,
        headers: Mapping[str, str],
        target_host: str,
        target_port: int,
    ) -> None:
        del target_host, target_port

        expected = _read_token(self.source)
        authorization = headers.get("Authorization")
        if expected is None or not isinstance(authorization, str):
            raise _authentication_error()

        prefix = "Bearer "
        if not authorization.startswith(prefix):
            raise _authentication_error()
        try:
            presented = authorization[len(prefix) :].encode("ascii")
        except UnicodeEncodeError:
            raise _authentication_error()

        if not presented or not hmac.compare_digest(presented, expected):
            raise _authentication_error()
